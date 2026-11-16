//! The PulseAudio endpoint end to end: a raw protocol client against a
//! daemon on a temporary socket.

mod common;

use aas::pulse::tagstruct::{Builder, Reader};
use aas::pulse::wire;
use common::*;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

fn create_body(rate: u32, format: u8, channels: u8, tlength_ms: u32) -> Vec<u8> {
    let frame = match format {
        wire::FORMAT_S16LE => 2,
        wire::FORMAT_S24LE => 3,
        _ => 4,
    } * u32::from(channels);
    let mut body = Builder::new();
    body.sample_spec(aas::pulse::tagstruct::SampleSpec {
        format,
        channels,
        rate,
    });
    let map: &[u8] = if channels == 1 { &[0] } else { &[1, 2] };
    body.channel_map(map);
    body.u32(u32::MAX);
    body.null_string();
    body.u32(u32::MAX);
    body.boolean(false);
    body.u32(rate / 1000 * tlength_ms * frame);
    body.u32(0);
    body.u32(0);
    body.u32(0);
    body.cvolume(&[wire::VOLUME_NORM; 2]);
    for _ in 0..7 {
        body.boolean(false);
    }
    body.boolean(false);
    body.boolean(false);
    body.proplist(&[("media.name".to_string(), "a test stream".to_string())]);
    for value in [true, false, false, false, false, false, false] {
        body.boolean(value);
    }
    body.u8(0);
    body.into_vec()
}

/// A PulseAudio client that speaks the wire format itself.
struct RawPulse {
    stream: UnixStream,
    tag: u32,
    /// Memblocks read while a packet was awaited: the stream and the data.
    blocks: Vec<(u32, Vec<u8>)>,
}

impl RawPulse {
    fn connect(path: &Path) -> RawPulse {
        let stream = UnixStream::connect(path).expect("connect to the pulse socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        RawPulse {
            stream,
            tag: 2,
            blocks: Vec::new(),
        }
    }

    fn send(&mut self, payload: &[u8]) {
        let mut head = [0u8; 20];
        head[..4].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        head[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        self.stream.write_all(&head).unwrap();
        self.stream.write_all(payload).unwrap();
    }

    /// The next packet. Memblocks before it are kept in `blocks`.
    fn recv(&mut self) -> (u32, u32, Vec<u8>) {
        loop {
            if let Some(packet) = self.read_frame().unwrap() {
                return packet;
            }
        }
    }

    /// Reads one frame: a packet, or None for a memblock, which goes to
    /// `blocks`.
    fn read_frame(&mut self) -> std::io::Result<Option<(u32, u32, Vec<u8>)>> {
        let mut head = [0u8; 20];
        self.stream.read_exact(&mut head)?;
        let length = u32::from_be_bytes(head[..4].try_into().unwrap()) as usize;
        let channel = u32::from_be_bytes(head[4..8].try_into().unwrap());
        let mut payload = vec![0u8; length];
        self.stream.read_exact(&mut payload).unwrap();
        if channel != u32::MAX {
            self.blocks.push((channel, payload));
            return Ok(None);
        }
        let mut reader = Reader::new(&payload);
        let command = reader.u32().unwrap();
        let tag = reader.u32().unwrap();
        let body = payload[reader.position()..].to_vec();
        Ok(Some((command, tag, body)))
    }

    fn next_tag(&mut self) -> u32 {
        self.tag += 1;
        self.tag
    }

    fn auth(&mut self) {
        let mut body = Builder::new();
        body.u32(wire::AUTH);
        body.u32(1);
        body.u32(wire::PROTOCOL_VERSION);
        body.arbitrary(&[0u8; 256]);
        self.send(&body.into_vec());
        let (command, tag, body) = self.recv();
        assert_eq!((command, tag), (wire::REPLY, 1));
        let mut reader = Reader::new(&body);
        assert_eq!(reader.u32(), Some(wire::PROTOCOL_VERSION));
    }

    fn set_name(&mut self, name: &str) -> u32 {
        let mut body = Builder::new();
        body.u32(wire::SET_CLIENT_NAME);
        body.u32(2);
        body.proplist(&[("application.name".to_string(), name.to_string())]);
        self.send(&body.into_vec());
        let (command, tag, body) = self.recv();
        assert_eq!((command, tag), (wire::REPLY, 2));
        let mut reader = Reader::new(&body);
        reader.u32().expect("a client index")
    }

    fn command(&mut self, command: u32, body: &[u8]) -> (u32, u32, Vec<u8>) {
        let tag = self.next_tag();
        let mut payload = Builder::new();
        payload.u32(command);
        payload.u32(tag);
        let mut encoded = payload.into_vec();
        encoded.extend_from_slice(body);
        self.send(&encoded);
        loop {
            let reply = self.recv();
            if reply.0 == wire::REPLY || reply.0 == wire::ERROR {
                return reply;
            }
        }
    }
}


impl RawPulse {
    fn create_stream(&mut self, rate: u32, format: u8, channels: u8, tlength_ms: u32) -> (u32, usize, u32) {
        let frame = match format {
            wire::FORMAT_S16LE => 2,
            wire::FORMAT_S24LE => 3,
            _ => 4,
        } * u32::from(channels);
        let (command, _, reply) = self.command(
            wire::CREATE_PLAYBACK_STREAM,
            &create_body(rate, format, channels, tlength_ms),
        );
        assert_eq!(command, wire::REPLY);
        let mut reader = Reader::new(&reply);
        let stream = reader.u32().expect("a stream index");
        assert_eq!(reader.u32(), Some(stream), "a sink-input index");
        let missing = reader.u32().unwrap() as usize;
        assert!(missing > 0, "the first request is not empty");
        (stream, missing, frame)
    }

    fn send_memblock(&mut self, stream: u32, data: &[u8]) {
        let mut head = [0u8; 20];
        head[..4].copy_from_slice(&(data.len() as u32).to_be_bytes());
        head[4..8].copy_from_slice(&stream.to_be_bytes());
        self.stream.write_all(&head).unwrap();
        self.stream.write_all(data).unwrap();
    }

    fn latency(&mut self, stream: u32) -> (u64, i64, i64) {
        let mut body = Builder::new();
        body.u32(stream);
        body.timeval(0, 0);
        let (command, _, reply) = self.command(wire::GET_PLAYBACK_LATENCY, &body.into_vec());
        assert_eq!(command, wire::REPLY);
        let mut reader = Reader::new(&reply);
        let sink_usec = reader.usec().expect("a sink latency");
        let _source = reader.usec();
        let _playing = reader.boolean();
        let _request = reader.timeval();
        let _now = reader.timeval();
        let write = reader.i64().expect("a write index");
        let read = reader.i64().expect("a read index");
        (sink_usec, write, read)
    }
}


impl RawPulse {
    fn set_sink_volume(&mut self, percent: u8) {
        let level = u32::from(percent) * wire::VOLUME_NORM / 100;
        let mut body = Builder::new();
        body.u32(0);
        body.null_string();
        body.cvolume(&[level; 2]);
        let (command, _, _) = self.command(wire::SET_SINK_VOLUME, &body.into_vec());
        assert_eq!(command, wire::REPLY);
    }

    fn set_sink_mute(&mut self, mute: bool) {
        let mut body = Builder::new();
        body.u32(0);
        body.null_string();
        body.boolean(mute);
        let (command, _, _) = self.command(wire::SET_SINK_MUTE, &body.into_vec());
        assert_eq!(command, wire::REPLY);
    }

    fn set_stream_volume(&mut self, stream: u32, percent: u8) {
        let level = u32::from(percent) * wire::VOLUME_NORM / 100;
        let mut body = Builder::new();
        body.u32(stream);
        body.cvolume(&[level; 2]);
        let (command, _, _) = self.command(wire::SET_SINK_INPUT_VOLUME, &body.into_vec());
        assert_eq!(command, wire::REPLY);
    }

    fn sink_state(&mut self) -> (u8, bool) {
        let mut body = Builder::new();
        body.u32(0);
        body.null_string();
        let (command, _, reply) = self.command(wire::GET_SINK_INFO, &body.into_vec());
        assert_eq!(command, wire::REPLY);
        let mut reader = Reader::new(&reply);
        assert_eq!(reader.u32(), Some(0));
        assert_eq!(reader.string(), Some("aas"));
        let _ = reader.string();
        let _ = reader.sample_spec();
        let _ = reader.channel_map();
        let _ = reader.u32();
        let cvolume = reader.cvolume().expect("a volume");
        let mute = reader.boolean().expect("a mute flag");
        (aas::pulse::volume::percent(&cvolume), mute)
    }
}

#[test]
fn a_pulse_client_authenticates_and_is_refused_before_it_does() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    let (command, tag, body) = client.command(wire::GET_SERVER_INFO, &[]);
    assert_eq!((command, tag), (wire::ERROR, 3));
    let mut reader = Reader::new(&body);
    assert_eq!(reader.u32(), Some(wire::ERR_ACCESS));
    assert!(reader.eof(), "an error carries only its code");
    client.auth();
    client.set_name("a test player");
}

#[test]
fn the_pulse_socket_is_group_audio() {
    let daemon = Daemon::start();
    let mode = std::fs::metadata(daemon.pulse_socket())
        .expect("the pulse socket")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o660);
}

#[test]
fn an_unknown_command_is_refused_and_the_connection_survives() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("a test player");
    let (command, tag, body) = client.command(104, &[]);
    assert_eq!((command, tag), (wire::ERROR, 3));
    let mut reader = Reader::new(&body);
    assert_eq!(reader.u32(), Some(wire::ERR_NOTSUPPORTED));
    // The next command still gets an answer.
    let (command, _, _) = client.command(104, &[]);
    assert_eq!(command, wire::ERROR);
}

#[test]
fn pactl_style_introspection_sees_one_sink() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("introspect");

    let (command, _, body) = client.command(wire::GET_SERVER_INFO, &[]);
    assert_eq!(command, wire::REPLY);
    let mut reader = Reader::new(&body);
    assert_eq!(reader.string(), Some("aas"));
    assert_eq!(reader.string(), Some("0.1.0"));
    assert!(reader.string().is_some(), "a user");
    assert!(reader.string().is_some(), "a host");
    let spec = reader.sample_spec().expect("a sample spec");
    assert_eq!(spec.channels, 2);
    assert!(spec.rate >= 8000);
    assert_eq!(reader.string(), Some("aas"));
    assert_eq!(reader.string(), Some("aas.monitor"), "the default source");
    assert!(reader.u32().is_some(), "a cookie");
    assert_eq!(reader.channel_map(), Some(vec![1, 2]));

    let (command, _, body) = client.command(wire::GET_SINK_INFO_LIST, &[]);
    assert_eq!(command, wire::REPLY);
    let mut reader = Reader::new(&body);
    assert_eq!(reader.u32(), Some(0));
    assert_eq!(reader.string(), Some("aas"));
    assert_eq!(reader.string(), Some("aas output"));
    assert!(reader.sample_spec().is_some());
    assert_eq!(reader.channel_map(), Some(vec![1, 2]));

    for command in [
        wire::GET_CARD_INFO_LIST,
        wire::GET_MODULE_INFO_LIST,
        wire::GET_SAMPLE_INFO_LIST,
        wire::GET_SINK_INPUT_INFO_LIST,
    ] {
        let (reply, _, body) = client.command(command, &[]);
        assert_eq!(reply, wire::REPLY);
        assert!(body.is_empty(), "a list with nothing in it");
    }
}

#[test]
fn subscribe_answers_then_sends_events() {
    let daemon = Daemon::start();
    let mut listener = RawPulse::connect(&daemon.pulse_socket());
    listener.auth();
    listener.set_name("listener");
    let mut mask = Builder::new();
    mask.u32(wire::MASK_SINK | wire::MASK_CLIENT | wire::MASK_SINK_INPUT);
    let (command, tag, body) = listener.command(wire::SUBSCRIBE, &mask.into_vec());
    assert_eq!((command, tag), (wire::REPLY, 3));
    assert!(body.is_empty());

    let mut newcomer = RawPulse::connect(&daemon.pulse_socket());
    newcomer.auth();
    newcomer.set_name("newcomer");

    let (command, _, body) = listener.recv();
    assert_eq!(command, wire::SUBSCRIBE_EVENT);
    let mut reader = Reader::new(&body);
    let event = reader.u32().expect("an event type");
    assert_eq!(event & 0xf, wire::EVENT_CLIENT);
    assert_eq!(event & 0x30, wire::EVENT_NEW);
}

#[test]
fn create_playback_stream_returns_a_ring_and_asks_for_data() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("a player");
    let (stream, missing, frame) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    assert_eq!(missing % (frame as usize), 0);
    client.send_memblock(stream, &vec![0u8; missing]);
    std::thread::sleep(Duration::from_millis(100));
    let (sink_usec, write, read) = client.latency(stream);
    assert!(write > 0, "the client has written");
    assert!(read >= 0, "the daemon has taken frames");
    assert!(sink_usec > 0 || read > 0, "the stream is moving");
}

#[test]
fn a_misaligned_memblock_is_ignored() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("a player");
    let (stream, missing, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    client.send_memblock(stream, &vec![0u8; missing - 3]);
    std::thread::sleep(Duration::from_millis(30));
    let (_, write, _) = client.latency(stream);
    assert_eq!(write, 0, "nothing misaligned was written");
}

#[test]
fn delete_stops_the_stream_and_another_keeps_playing() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("a player");
    let (first, _, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    let (second, _, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    let mut body = Builder::new();
    body.u32(first);
    let (command, _, _) = client.command(wire::DELETE_PLAYBACK_STREAM, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    client.latency(second);
}

#[test]
fn sink_volume_and_the_api_are_one_number() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("volume");
    client.set_sink_volume(40);
    assert_eq!(client.sink_state().0, 40);
    let (_, body) = daemon.get("/outputs/1");
    assert_eq!(body["volume"], serde_json::json!(40));
}

#[test]
fn stream_volume_is_the_client_gain() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("volume");
    let (stream, _, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    client.set_stream_volume(stream, 50);
    let (_, inputs) = daemon.get("/inputs");
    let found = inputs
        .as_array()
        .unwrap()
        .iter()
        .find(|input| {
            input["volume_db"]
                .as_f64()
                .is_some_and(|db| (db + 18.06).abs() < 0.1)
        })
        .expect("the stream's client at 50 percent, -18 dB on PulseAudio's curve");
    assert_eq!(found["muted"], serde_json::json!(false));
}

#[test]
fn mute_sets_zero_and_restores_the_remembered_percent() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("volume");
    client.set_sink_volume(40);
    client.set_sink_mute(true);
    assert_eq!(client.sink_state(), (0, true));
    client.set_sink_mute(false);
    assert_eq!(client.sink_state(), (40, false));
}

#[test]
fn a_volume_change_made_elsewhere_reaches_a_subscriber() {
    let daemon = Daemon::start();
    let mut listener = RawPulse::connect(&daemon.pulse_socket());
    listener.auth();
    listener.set_name("listener");
    let mut mask = Builder::new();
    mask.u32(wire::MASK_SINK);
    let (command, _, _) = listener.command(wire::SUBSCRIBE, &mask.into_vec());
    assert_eq!(command, wire::REPLY);
    daemon.patch("/outputs/1", r#"{"volume":25}"#);
    loop {
        let (command, _, body) = listener.recv();
        if command != wire::SUBSCRIBE_EVENT {
            continue;
        }
        let mut reader = Reader::new(&body);
        let event = reader.u32().expect("an event");
        assert_eq!(event & 0xf, wire::EVENT_SINK);
        assert_eq!(event & 0x30, wire::EVENT_CHANGE);
        break;
    }
}

#[test]
fn a_malformed_packet_closes_only_its_connection() {
    let daemon = Daemon::start();
    let mut broken = RawPulse::connect(&daemon.pulse_socket());
    let mut head = [0u8; 20];
    head[..4].copy_from_slice(&u32::MAX.to_be_bytes());
    head[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
    broken.stream.write_all(&head).unwrap();

    let mut other = RawPulse::connect(&daemon.pulse_socket());
    other.auth();
    other.set_name("still here");
    let (command, _, _) = other.command(wire::STAT, &[]);
    assert_eq!(command, wire::REPLY);
}

#[test]
fn a_pulse_and_an_alsa_client_play_together() {
    let daemon = Daemon::start();
    let mut alsa = daemon.playing(16384);
    let mut pulse = RawPulse::connect(&daemon.pulse_socket());
    pulse.auth();
    pulse.set_name("pulse player");
    let (stream, missing, _) = pulse.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    pulse.send_memblock(stream, &vec![0u8; missing]);
    let (_, state) = daemon.get("/state");
    assert_eq!(state["rate"], serde_json::json!(48_000));
    assert!(
        state["clients"]
            .as_array()
            .unwrap()
            .iter()
            .any(|client| client["running"] == serde_json::json!(true)),
        "a client runs: {state}"
    );
    let mut body = Builder::new();
    body.u32(stream);
    let (command, _, _) = pulse.command(wire::DELETE_PLAYBACK_STREAM, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    // The ALSA client keeps playing.
    top_up(&mut alsa);
    let (_, state) = daemon.get("/state");
    assert_eq!(state["rate"], serde_json::json!(48_000));
}

#[test]
fn a_client_killed_mid_stream_does_not_block_the_daemon() {
    let daemon = Daemon::start();
    let mut first = RawPulse::connect(&daemon.pulse_socket());
    first.auth();
    first.set_name("one");
    let (gone, _, _) = first.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    drop(first);

    let mut second = RawPulse::connect(&daemon.pulse_socket());
    second.auth();
    second.set_name("two");
    let (_stream, missing, _) = second.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    assert!(missing > 0);
    let mut body = Builder::new();
    body.u32(gone);
    let query = body.into_vec();
    let mut waited = 0;
    let reply = loop {
        let (command, _, _) = second.command(wire::GET_SINK_INPUT_INFO, &query);
        if command == wire::ERROR || waited > 200 {
            break command;
        }
        waited += 10;
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(reply, wire::ERROR, "the dead stream is gone from the registry");
}

#[test]
fn a_partial_packet_does_not_block_another_connection() {
    let daemon = Daemon::start();
    let mut stalled = RawPulse::connect(&daemon.pulse_socket());
    stalled.stream.write_all(&[0u8; 7]).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let mut other = RawPulse::connect(&daemon.pulse_socket());
    other.auth();
    let (command, _, _) = other.command(wire::STAT, &[]);
    assert_eq!(command, wire::REPLY);
}

#[test]
fn mute_survives_the_connection_that_set_it() {
    let daemon = Daemon::start();
    let mut first = RawPulse::connect(&daemon.pulse_socket());
    first.auth();
    first.set_sink_volume(40);
    first.set_sink_mute(true);
    drop(first);

    let mut second = RawPulse::connect(&daemon.pulse_socket());
    second.auth();
    assert_eq!(second.sink_state(), (0, true));
    second.set_sink_mute(false);
    assert_eq!(second.sink_state(), (40, false));
}

#[test]
fn a_client_gain_change_reaches_a_subscriber() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("gain");
    let mut mask = Builder::new();
    mask.u32(wire::MASK_SINK_INPUT);
    let (command, _, _) = client.command(wire::SUBSCRIBE, &mask.into_vec());
    assert_eq!(command, wire::REPLY);
    let (_stream, _missing, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    let name = std::fs::read_to_string("/proc/self/comm")
        .unwrap()
        .trim_end()
        .to_string();
    daemon.patch(
        &format!("/inputs/clients/{}", name),
        r#"{"volume_db":-6}"#,
    );
    loop {
        let (command, _, body) = client.recv();
        if command != wire::SUBSCRIBE_EVENT {
            continue;
        }
        let mut reader = Reader::new(&body);
        let event = reader.u32().expect("an event");
        if event & 0xf == wire::EVENT_SINK_INPUT && event & 0x30 == wire::EVENT_CHANGE {
            break;
        }
    }
}

#[test]
fn a_refused_format_is_an_error_and_the_connection_survives() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("bad format");
    let (command, tag, body) =
        client.command(wire::CREATE_PLAYBACK_STREAM, &create_body(48_000, 99, 2, 40));
    assert_eq!((command, tag), (wire::ERROR, 3));
    let mut reader = Reader::new(&body);
    assert_eq!(reader.u32(), Some(wire::ERR_NOTSUPPORTED));
    let (command, _, _) = client.command(wire::STAT, &[]);
    assert_eq!(command, wire::REPLY);
}

#[test]
fn a_volume_race_leaves_the_last_value_and_no_deadlock() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("race");
    for step in 0..20u8 {
        let percent = 20 + step;
        if step % 2 == 0 {
            client.set_sink_volume(percent);
        } else {
            daemon.patch(
                "/outputs/1",
                &format!(r#"{{"volume":{percent}}}"#),
            );
        }
    }
    assert_eq!(client.sink_state().0, 39);
}

impl RawPulse {
    /// The next packet, or None if none comes within `wait`.
    /// Memblocks go to `blocks` and do not end the wait.
    fn try_recv(&mut self, wait: Duration) -> Option<(u32, u32, Vec<u8>)> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            self.stream
                .set_read_timeout(Some(left.max(Duration::from_millis(1))))
                .unwrap();
            let got = self.read_frame();
            self.stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            match got {
                Ok(Some(packet)) => return Some(packet),
                Ok(None) if std::time::Instant::now() < deadline => {}
                _ => return None,
            }
        }
    }
}

fn request_length(body: &[u8]) -> usize {
    let mut reader = Reader::new(body);
    let _stream = reader.u32();
    reader.u32().expect("a request length") as usize
}

#[test]
fn a_client_that_answers_every_request_never_runs_dry() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("tone");
    let (stream, missing, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 200);
    client.send_memblock(stream, &vec![0u8; missing]);
    // The stream starts before its first write, so an underflow from then
    // may still arrive; count only from a moment after the ring was full.
    let start = std::time::Instant::now();
    let settled = Duration::from_millis(50);
    let mut underflows = 0;
    while start.elapsed() < Duration::from_millis(1200) {
        match client.try_recv(Duration::from_millis(5)) {
            Some((command, _, body)) if command == wire::REQUEST => {
                client.send_memblock(stream, &vec![0u8; request_length(&body)]);
            }
            Some((command, _, _)) if command == wire::UNDERFLOW && start.elapsed() > settled => {
                underflows += 1;
            }
            _ => {}
        }
    }
    assert_eq!(underflows, 0, "the 200 ms ring ran dry");
}

#[test]
fn small_writes_never_overfill_the_ring() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("tone");
    let (stream, missing, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 200);
    // A client that writes what it was asked for, in 4 KiB pieces, and
    // adds every request to what it owes, as libpulse does.
    let piece = 4096;
    let mut owed = missing;
    let mut sent = 0usize;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        if owed >= piece {
            client.send_memblock(stream, &vec![0u8; piece]);
            owed -= piece;
            sent += piece;
        }
        while let Some((command, _, body)) = client.try_recv(Duration::from_millis(1)) {
            if command == wire::REQUEST {
                owed += request_length(&body);
            }
        }
    }
    let (_, write_index, _) = client.latency(stream);
    assert_eq!(write_index as usize, sent, "every byte asked for is kept");
}

#[test]
fn the_default_sink_alias_names_the_sink() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("pactl");
    let mut body = Builder::new();
    body.u32(u32::MAX);
    body.string("@DEFAULT_SINK@");
    body.cvolume(&[wire::VOLUME_NORM * 2 / 5; 2]);
    let (command, _, _) = client.command(wire::SET_SINK_VOLUME, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    assert_eq!(client.sink_state(), (40, false));
    let mut body = Builder::new();
    body.u32(u32::MAX);
    body.string("@DEFAULT_SINK@");
    let (command, _, _) = client.command(wire::GET_SINK_INFO, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    let mut body = Builder::new();
    body.string("@DEFAULT_SINK@");
    let (command, _, _) = client.command(wire::LOOKUP_SINK, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    let mut body = Builder::new();
    body.u32(u32::MAX);
    body.string("elsewhere");
    let (command, _, _) = client.command(wire::GET_SINK_INFO, &body.into_vec());
    assert_eq!(command, wire::ERROR);
}

#[test]
fn another_connection_sets_a_streams_volume_and_mute() {
    let daemon = Daemon::start();
    let mut player = RawPulse::connect(&daemon.pulse_socket());
    player.auth();
    player.set_name("player");
    let (stream, _, _) = player.create_stream(48_000, wire::FORMAT_S16LE, 2, 200);
    let mut pactl = RawPulse::connect(&daemon.pulse_socket());
    pactl.auth();
    pactl.set_name("pactl");
    let mut body = Builder::new();
    body.u32(stream);
    body.cvolume(&[wire::VOLUME_NORM / 2; 2]);
    let (command, _, _) = pactl.command(wire::SET_SINK_INPUT_VOLUME, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    let mut body = Builder::new();
    body.u32(stream);
    body.boolean(true);
    let (command, _, _) = pactl.command(wire::SET_SINK_INPUT_MUTE, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    // The owning connection applies both on its next tick.
    let start = std::time::Instant::now();
    loop {
        let mut body = Builder::new();
        body.u32(stream);
        let (command, _, reply) = pactl.command(wire::GET_SINK_INPUT_INFO, &body.into_vec());
        assert_eq!(command, wire::REPLY);
        let mut reader = Reader::new(&reply);
        for _ in 0..2 {
            let _ = reader.u32();
        }
        let _ = reader.string();
        for _ in 0..3 {
            let _ = reader.u32();
        }
        let _ = reader.sample_spec();
        let _ = reader.channel_map();
        let cvolume = reader.cvolume().expect("a volume");
        let _ = reader.usec();
        let _ = reader.usec();
        let _ = reader.string();
        let _ = reader.string();
        let muted = reader.boolean().expect("a mute flag");
        let db = aas::pulse::volume::db_millis(&cvolume);
        if (-18_100..=-18_000).contains(&db) && muted {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the stream is at {db} millidB, muted {muted}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut body = Builder::new();
    body.u32(9999);
    body.cvolume(&[wire::VOLUME_NORM; 2]);
    let (command, _, _) = pactl.command(wire::SET_SINK_INPUT_VOLUME, &body.into_vec());
    assert_eq!(command, wire::ERROR);
}

/// Reads a sink info reply field by field, as libpulse 17 does for
/// protocol 35, and returns the number of formats. Panics where libpulse
/// would fail the connection with a protocol error.
fn read_sink_info_as_libpulse(body: &[u8]) -> u8 {
    let mut r = Reader::new(body);
    r.u32().expect("index");
    r.string().expect("name");
    r.string().expect("description");
    r.sample_spec().expect("sample spec");
    r.channel_map().expect("channel map");
    r.u32().expect("owner module");
    r.cvolume().expect("volume");
    r.boolean().expect("mute");
    r.u32().expect("monitor source");
    let _ = r.string(); // monitor source name, may be null
    r.usec().expect("latency");
    r.string().expect("driver");
    r.u32().expect("flags");
    r.proplist().expect("proplist");
    r.usec().expect("configured latency");
    r.volume().expect("base volume");
    r.u32().expect("state");
    r.u32().expect("volume steps");
    r.u32().expect("card");
    assert_eq!(r.u32(), Some(0), "no ports");
    let _ = r.string(); // active port, may be null
    let formats = r.u8().expect("format count");
    for _ in 0..formats {
        r.format_info().expect("a format");
    }
    assert!(r.eof(), "nothing after the formats");
    formats
}

/// Reads a sink input info reply as libpulse 17 does and returns its name
/// and format.
fn read_sink_input_as_libpulse(body: &[u8]) -> (String, (u8, Vec<(String, String)>)) {
    let mut r = Reader::new(body);
    r.u32().expect("index");
    let name = r.string().expect("name").to_string();
    r.u32().expect("owner module");
    r.u32().expect("client");
    r.u32().expect("sink");
    r.sample_spec().expect("sample spec");
    r.channel_map().expect("channel map");
    r.cvolume().expect("volume");
    r.usec().expect("buffer latency");
    r.usec().expect("sink latency");
    r.string().expect("resample method");
    r.string().expect("driver");
    r.boolean().expect("mute");
    r.proplist().expect("proplist");
    r.boolean().expect("corked");
    r.boolean().expect("has volume");
    r.boolean().expect("volume writable");
    let format = r.format_info().expect("format");
    assert!(r.eof(), "nothing after the format");
    (name, format)
}

#[test]
fn the_sink_reads_as_libpulse_reads_it_with_one_format() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("pactl");
    let mut body = Builder::new();
    body.u32(u32::MAX);
    body.string("@DEFAULT_SINK@");
    let (command, _, reply) = client.command(wire::GET_SINK_INFO, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    // libpulse fails the whole connection on a sink without formats.
    assert_eq!(read_sink_info_as_libpulse(&reply), 1);
}

#[test]
fn a_stream_name_comes_back_without_the_clients_nul() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("player");
    // create_body's proplist carries "a test stream" with its NUL, as
    // libpulse sends text properties.
    let (stream, _, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 200);
    let mut body = Builder::new();
    body.u32(stream);
    let (command, _, reply) = client.command(wire::GET_SINK_INPUT_INFO, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    let (name, (encoding, _)) = read_sink_input_as_libpulse(&reply);
    assert_eq!(name, "a test stream");
    assert_eq!(encoding, 1, "PCM");
    let (command, _, reply) = client.command(wire::GET_CLIENT_INFO_LIST, &[]);
    assert_eq!(command, wire::REPLY);
    let mut r = Reader::new(&reply);
    r.u32().expect("index");
    assert_eq!(r.string(), Some("player"));
}

#[test]
fn a_stream_given_as_a_pcm_format_list_plays() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("mpv");
    // As pa_stream_new_extended sends it: an invalid placeholder sample
    // spec, an empty channel map, and the real format in the list.
    let mut body = Builder::new();
    body.sample_spec(aas::pulse::tagstruct::SampleSpec {
        format: 0xff,
        channels: 0,
        rate: 0,
    });
    body.channel_map(&[]);
    body.u32(u32::MAX);
    body.null_string();
    body.u32(u32::MAX);
    body.boolean(false);
    body.u32(u32::MAX);
    body.u32(u32::MAX);
    body.u32(u32::MAX);
    body.u32(0);
    body.cvolume(&[wire::VOLUME_NORM; 2]);
    for _ in 0..9 {
        body.boolean(false);
    }
    body.proplist(&[("media.name".to_string(), "audio stream".to_string())]);
    for _ in 0..7 {
        body.boolean(false);
    }
    body.u8(1);
    body.format_info(
        1,
        &[
            ("format.sample_format".to_string(), "\"float32le\"".to_string()),
            ("format.rate".to_string(), "44100".to_string()),
            ("format.channels".to_string(), "2".to_string()),
            (
                "format.channel_map".to_string(),
                "\"front-left,front-right\"".to_string(),
            ),
        ],
    );
    let (command, _, reply) = client.command(wire::CREATE_PLAYBACK_STREAM, &body.into_vec());
    assert_eq!(command, wire::REPLY, "a PCM format list is played");
    let mut r = Reader::new(&reply);
    let stream = r.u32().unwrap();
    for _ in 0..6 {
        r.u32().unwrap();
    }
    let spec = r.sample_spec().unwrap();
    assert_eq!(
        (spec.format, spec.channels, spec.rate),
        (wire::FORMAT_FLOAT32LE, 2, 44_100)
    );
    // 512 stereo float frames.
    client.send_memblock(stream, &vec![0u8; 4096]);
    let (_, write_index, _) = client.latency(stream);
    assert_eq!(write_index, 4096);
}

#[test]
fn a_stream_at_volume_zero_is_silent_and_says_so() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("quiet");
    let (stream, _, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 200);
    let read_back = |client: &mut RawPulse| {
        let mut body = Builder::new();
        body.u32(stream);
        let (command, _, reply) = client.command(wire::GET_SINK_INPUT_INFO, &body.into_vec());
        assert_eq!(command, wire::REPLY);
        let mut r = Reader::new(&reply);
        r.u32();
        r.string();
        r.u32();
        r.u32();
        r.u32();
        r.sample_spec();
        r.channel_map();
        let cvolume = r.cvolume().unwrap();
        r.usec();
        r.usec();
        r.string();
        r.string();
        (cvolume[0], r.boolean().unwrap())
    };
    let client_muted = || {
        let (_, inputs) = daemon.get("/inputs");
        inputs
            .as_array()
            .unwrap()
            .iter()
            .filter(|input| input["id"] != "analog")
            .any(|input| input["muted"] == serde_json::json!(true))
    };
    client.set_stream_volume(stream, 0);
    // Silence is the client's mute in aas, and volume 0 without a mute in
    // PulseAudio, as pactl shows it.
    assert!(client_muted());
    assert_eq!(read_back(&mut client), (0, false));
    client.set_stream_volume(stream, 100);
    assert!(!client_muted());
    assert_eq!(read_back(&mut client), (wire::VOLUME_NORM, false));
}

#[test]
fn a_stream_starts_with_its_first_write_and_says_so() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("late");
    let (stream, missing, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 200);
    // Nothing written yet: no underflow, and no STARTED.
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        if let Some((command, _, _)) = client.try_recv(Duration::from_millis(20)) {
            assert_ne!(command, wire::UNDERFLOW, "an empty stream underran");
            assert_ne!(command, wire::STARTED, "started before any data");
        }
    }
    client.send_memblock(stream, &vec![0u8; missing]);
    let start = std::time::Instant::now();
    loop {
        assert!(start.elapsed() < Duration::from_secs(1), "no STARTED");
        if let Some((command, _, body)) = client.try_recv(Duration::from_millis(20))
            && command == wire::STARTED
        {
            assert_eq!(Reader::new(&body).u32(), Some(stream));
            break;
        }
    }
}

#[test]
fn a_smaller_tlength_keeps_the_ring_at_that_fill() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("short");
    let (stream, missing, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 200);
    // 50 ms of the 200 ms ring.
    let target = 48 * 50 * 4;
    let mut body = Builder::new();
    body.u32(stream);
    body.u32(u32::MAX);
    body.u32(target);
    body.u32(u32::MAX);
    body.u32(u32::MAX);
    body.boolean(false);
    body.boolean(false);
    let (command, _, reply) =
        client.command(wire::SET_PLAYBACK_STREAM_BUFFER_ATTR, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    let mut r = Reader::new(&reply);
    let maxlength = r.u32().unwrap();
    assert_eq!(r.u32(), Some(target), "tlength is honoured");
    let _prebuf = r.u32();
    let minreq = r.u32().unwrap();
    assert!(minreq > 0 && minreq <= target / 2);
    assert!(maxlength >= target);
    // The first request was for the whole ring; send only the target, then
    // answer each request and watch the fill.
    let _ = missing;
    client.send_memblock(stream, &vec![0u8; target as usize]);
    let mut fullest = 0i64;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        if let Some((command, _, body)) = client.try_recv(Duration::from_millis(5))
            && command == wire::REQUEST
        {
            client.send_memblock(stream, &vec![0u8; request_length(&body)]);
            let (_, write, read) = client.latency(stream);
            fullest = fullest.max(write - read);
        }
    }
    assert!(fullest <= i64::from(target), "the ring held {fullest} of {target} bytes");
}

#[test]
fn latencies_are_measured_not_zero() {
    let daemon = Daemon::start();
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("measured");
    let (stream, missing, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 200);
    client.send_memblock(stream, &vec![0u8; missing]);
    // The ring holds up to 200 ms; GET_PLAYBACK_LATENCY's sink part is what
    // plays after it, which the client adds to write minus read.
    let (sink_usec, write, read) = client.latency(stream);
    assert!(write - read > 0);
    assert!(sink_usec < 200_000, "the sink part counts the ring: {sink_usec}");
    std::thread::sleep(Duration::from_millis(100));
    let mut body = Builder::new();
    body.u32(stream);
    let (command, _, reply) = client.command(wire::GET_SINK_INPUT_INFO, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    let mut r = Reader::new(&reply);
    r.u32();
    r.string();
    r.u32();
    r.u32();
    r.u32();
    r.sample_spec();
    r.channel_map();
    r.cvolume();
    let buffer_usec = r.usec().unwrap();
    assert!(buffer_usec > 0, "the stream's buffer latency is measured");
}

#[test]
fn connections_beyond_the_cap_are_closed() {
    let daemon = Daemon::start();
    let mut held: Vec<RawPulse> = (0..32)
        .map(|_| {
            let mut c = RawPulse::connect(&daemon.pulse_socket());
            c.auth();
            c
        })
        .collect();
    let mut extra = UnixStream::connect(daemon.pulse_socket()).unwrap();
    extra
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(extra.read(&mut byte).unwrap(), 0, "the 33rd is closed");
    // One leaves, and a newcomer is served again.
    held.pop();
    std::thread::sleep(Duration::from_millis(400));
    let mut again = RawPulse::connect(&daemon.pulse_socket());
    again.auth();
}

/// What a source info reply says, read field by field as libpulse 17 reads
/// it for protocol 35.
struct SourceInfo {
    index: u32,
    name: String,
    monitor_of: u32,
    monitor_of_name: Option<String>,
    flags: u32,
    volume: Vec<u32>,
    mute: bool,
}

/// Reads one source info from `r`, panicking where libpulse would fail the
/// connection with a protocol error.
fn read_source_info_as_libpulse(r: &mut Reader) -> SourceInfo {
    let index = r.u32().expect("index");
    let name = r.string().expect("name").to_string();
    r.string().expect("description");
    let spec = r.sample_spec().expect("sample spec");
    assert_eq!((spec.format, spec.channels), (wire::FORMAT_S32LE, 2));
    assert_eq!(r.channel_map(), Some(vec![1, 2]));
    assert_eq!(r.u32(), Some(wire::INVALID_INDEX), "no owner module");
    let volume = r.cvolume().expect("volume");
    let mute = r.boolean().expect("mute");
    let monitor_of = r.u32().expect("monitor of sink");
    let monitor_of_name = r.string().map(str::to_string);
    r.usec().expect("latency");
    r.string().expect("driver");
    let flags = r.u32().expect("flags");
    r.proplist().expect("proplist");
    r.usec().expect("configured latency");
    assert_eq!(r.volume(), Some(wire::VOLUME_NORM), "base volume");
    r.u32().expect("state");
    r.u32().expect("volume steps");
    assert_eq!(r.u32(), Some(wire::INVALID_INDEX), "no card");
    assert_eq!(r.u32(), Some(0), "no ports");
    let _ = r.string(); // active port, may be null
    let formats = r.u8().expect("format count");
    assert_eq!(formats, 1, "libpulse refuses a source without formats");
    r.format_info().expect("a format");
    SourceInfo {
        index,
        name,
        monitor_of,
        monitor_of_name,
        flags,
        volume,
        mute,
    }
}

fn pactl(daemon: &Daemon) -> RawPulse {
    let mut client = RawPulse::connect(&daemon.pulse_socket());
    client.auth();
    client.set_name("pactl");
    client
}

#[test]
fn the_sources_are_listed() {
    let daemon = Daemon::start();
    let mut client = pactl(&daemon);
    let (command, _, body) = client.command(wire::GET_SOURCE_INFO_LIST, &[]);
    assert_eq!(command, wire::REPLY);
    let mut r = Reader::new(&body);
    let source = read_source_info_as_libpulse(&mut r);
    assert!(r.eof(), "one source without an analog input");
    assert_eq!((source.index, source.name.as_str()), (0, "aas.monitor"));
    assert_eq!(source.monitor_of, 0);
    assert_eq!(source.monitor_of_name.as_deref(), Some("aas"));
    assert_eq!(source.volume, vec![wire::VOLUME_NORM; 2]);
    assert!(!source.mute);
    assert_eq!(source.flags, 0x2, "PA_SOURCE_LATENCY");

    let mut body = Builder::new();
    body.u32(wire::INVALID_INDEX);
    body.string("aas.monitor");
    let (command, _, reply) = client.command(wire::GET_SOURCE_INFO, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    let mut r = Reader::new(&reply);
    assert_eq!(read_source_info_as_libpulse(&mut r).index, 0);
    assert!(r.eof());

    let mut body = Builder::new();
    body.u32(1);
    body.null_string();
    let (command, _, reply) = client.command(wire::GET_SOURCE_INFO, &body.into_vec());
    assert_eq!(command, wire::ERROR);
    assert_eq!(Reader::new(&reply).u32(), Some(wire::ERR_NOENTITY));

    let (command, _, body) = client.command(wire::GET_SOURCE_OUTPUT_INFO_LIST, &[]);
    assert_eq!(command, wire::REPLY);
    assert!(body.is_empty(), "no record streams yet");
}

#[test]
fn the_sink_names_its_monitor() {
    let daemon = Daemon::start();
    let mut client = pactl(&daemon);
    let (command, _, body) = client.command(wire::GET_SINK_INFO_LIST, &[]);
    assert_eq!(command, wire::REPLY);
    let mut r = Reader::new(&body);
    r.u32().unwrap();
    r.string().unwrap();
    r.string().unwrap();
    r.sample_spec().unwrap();
    r.channel_map().unwrap();
    r.u32().unwrap();
    r.cvolume().unwrap();
    r.boolean().unwrap();
    assert_eq!(r.u32(), Some(0), "monitor source");
    assert_eq!(r.string(), Some("aas.monitor"), "monitor source name");
    assert_eq!(read_sink_info_as_libpulse(&body), 1);
}

fn lookup_source(client: &mut RawPulse, name: &str) -> Result<u32, u32> {
    let mut body = Builder::new();
    body.string(name);
    let (command, _, reply) = client.command(wire::LOOKUP_SOURCE, &body.into_vec());
    let value = Reader::new(&reply).u32().expect("an index or a code");
    if command == wire::REPLY {
        Ok(value)
    } else {
        Err(value)
    }
}

#[test]
fn the_default_source_is_the_monitor_without_an_analog_input() {
    let daemon = Daemon::start();
    let mut client = pactl(&daemon);
    let (command, _, body) = client.command(wire::GET_SERVER_INFO, &[]);
    assert_eq!(command, wire::REPLY);
    let mut r = Reader::new(&body);
    for _ in 0..4 {
        r.string();
    }
    r.sample_spec().unwrap();
    assert_eq!(r.string(), Some("aas"), "default sink");
    assert_eq!(r.string(), Some("aas.monitor"), "default source");
    assert_eq!(lookup_source(&mut client, "@DEFAULT_SOURCE@"), Ok(0));
    assert_eq!(lookup_source(&mut client, "@DEFAULT_MONITOR@"), Ok(0));
    assert_eq!(lookup_source(&mut client, "aas.monitor"), Ok(0));
    assert_eq!(
        lookup_source(&mut client, "aas.analog"),
        Err(wire::ERR_NOENTITY)
    );
}

#[test]
fn source_volume_is_fixed() {
    let daemon = Daemon::start();
    let mut client = pactl(&daemon);
    let mut body = Builder::new();
    body.u32(0);
    body.null_string();
    body.cvolume(&[wire::VOLUME_NORM / 2; 2]);
    let (command, _, reply) = client.command(wire::SET_SOURCE_VOLUME, &body.into_vec());
    assert_eq!(command, wire::ERROR);
    assert_eq!(Reader::new(&reply).u32(), Some(wire::ERR_NOTSUPPORTED));
    let mut body = Builder::new();
    body.u32(0);
    body.null_string();
    body.boolean(true);
    let (command, _, reply) = client.command(wire::SET_SOURCE_MUTE, &body.into_vec());
    assert_eq!(command, wire::ERROR);
    assert_eq!(Reader::new(&reply).u32(), Some(wire::ERR_NOTSUPPORTED));
    let (command, _, _) = client.command(wire::GET_SERVER_INFO, &[]);
    assert_eq!(command, wire::REPLY);
}

#[test]
fn set_default_source_names_one_of_ours() {
    let daemon = Daemon::start();
    let mut client = pactl(&daemon);
    let mut body = Builder::new();
    body.string("aas.monitor");
    let (command, _, _) = client.command(wire::SET_DEFAULT_SOURCE, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    let mut body = Builder::new();
    body.string("nonsense");
    let (command, _, reply) = client.command(wire::SET_DEFAULT_SOURCE, &body.into_vec());
    assert_eq!(command, wire::ERROR);
    assert_eq!(Reader::new(&reply).u32(), Some(wire::ERR_NOENTITY));
}

use aas::pulse::tagstruct::SampleSpec;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

const S16_STEREO_48K: SampleSpec = SampleSpec {
    format: wire::FORMAT_S16LE,
    channels: 2,
    rate: 48_000,
};

/// The amplitude of the test tone, in S16.
const TONE: i32 = 16_000;

/// `CREATE_RECORD_STREAM` for protocol 35, as libpulse sends it.
#[allow(clippy::too_many_arguments)]
fn record_body(
    spec: SampleSpec,
    source_index: u32,
    source_name: Option<&str>,
    maxlength: u32,
    corked: bool,
    fragsize: u32,
    peak_detect: bool,
    direct_on_input: u32,
) -> Vec<u8> {
    let mut body = Builder::new();
    body.sample_spec(spec);
    let map: &[u8] = if spec.channels == 1 { &[0] } else { &[1, 2] };
    body.channel_map(map);
    body.u32(source_index);
    match source_name {
        Some(name) => body.string(name),
        None => body.null_string(),
    }
    body.u32(maxlength);
    body.boolean(corked);
    body.u32(fragsize);
    for _ in 0..7 {
        body.boolean(false);
    }
    body.boolean(peak_detect);
    body.boolean(false); // adjust_latency
    body.proplist(&[("media.name".to_string(), "a test recording".to_string())]);
    body.u32(direct_on_input);
    body.boolean(false); // early_requests
    body.boolean(false); // dont_inhibit_auto_suspend
    body.boolean(false); // fail_on_suspend
    body.u8(0);
    body.cvolume(&[wire::VOLUME_NORM; 2]);
    for _ in 0..5 {
        body.boolean(false); // muted … passthrough
    }
    body.into_vec()
}

/// What a `CREATE_RECORD_STREAM` reply says.
struct RecordReply {
    stream: u32,
    maxlength: u32,
    fragsize: u32,
    spec: SampleSpec,
    source: u32,
}

fn read_record_reply(body: &[u8]) -> RecordReply {
    let mut r = Reader::new(body);
    let stream = r.u32().expect("a stream index");
    assert_eq!(r.u32(), Some(stream), "the source output index");
    let maxlength = r.u32().expect("maxlength");
    let fragsize = r.u32().expect("fragsize");
    let spec = r.sample_spec().expect("a sample spec");
    r.channel_map().expect("a channel map");
    let source = r.u32().expect("a source index");
    r.string().expect("a source name");
    assert_eq!(r.boolean(), Some(false), "not suspended");
    r.usec().expect("the configured latency");
    r.format_info().expect("a format");
    assert!(r.eof(), "nothing after the format");
    RecordReply {
        stream,
        maxlength,
        fragsize,
        spec,
        source,
    }
}

/// Creates a record stream and returns its index.
fn create_record_stream(
    conn: &mut RawPulse,
    spec: SampleSpec,
    source_name: &str,
    fragsize: u32,
) -> u32 {
    let body = record_body(spec, wire::INVALID_INDEX, Some(source_name), u32::MAX, false, fragsize, false, wire::INVALID_INDEX);
    let (command, _, reply) = conn.command(wire::CREATE_RECORD_STREAM, &body);
    assert_eq!(command, wire::REPLY, "the record stream is created");
    read_record_reply(&reply).stream
}

/// Memblocks of one stream, and packets as command and body.
type Collected = (Vec<Vec<u8>>, Vec<(u32, Vec<u8>)>);

impl RawPulse {
    /// Reads for `wait` and returns the memblocks of `stream` and every
    /// packet that came, the memblocks read before included.
    fn collect(&mut self, stream: u32, wait: Duration) -> Collected {
        let deadline = Instant::now() + wait;
        let mut packets = Vec::new();
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if let Some((command, _, body)) = self.try_recv(left) {
                packets.push((command, body));
            }
        }
        let blocks = std::mem::take(&mut self.blocks)
            .into_iter()
            .filter(|(channel, _)| *channel == stream)
            .map(|(_, data)| data)
            .collect();
        (blocks, packets)
    }

    /// Waits up to `wait` for the first memblock of `stream`.
    fn first_block(&mut self, stream: u32, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if self.blocks.iter().any(|(channel, _)| *channel == stream) {
                return true;
            }
            self.try_recv(Duration::from_millis(10));
        }
        self.blocks.iter().any(|(channel, _)| *channel == stream)
    }
}

/// The loudest S16 sample of `blocks`.
fn s16_peak(blocks: &[Vec<u8>]) -> i32 {
    blocks
        .iter()
        .flat_map(|block| block.chunks_exact(2))
        .map(|s| i32::from(i16::from_le_bytes([s[0], s[1]])).abs())
        .max()
        .unwrap_or(0)
}

/// A PulseAudio player of a 1 kHz S16 stereo tone at `rate`, on its own
/// connection, until the flag is set.
fn tone_player(daemon: &Daemon, rate: u32) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let mut player = RawPulse::connect(&daemon.pulse_socket());
    player.auth();
    player.set_name("tone");
    let (stream, missing, _) = player.create_stream(rate, wire::FORMAT_S16LE, 2, 200);
    let flag = stop.clone();
    let handle = std::thread::spawn(move || {
        let mut n = 0u64;
        let mut tone = |bytes: usize| {
            let mut out = Vec::with_capacity(bytes);
            for _ in 0..bytes / 4 {
                let phase = n as f64 * 1000.0 / f64::from(rate) * std::f64::consts::TAU;
                let v = (phase.sin() * f64::from(TONE)) as i16;
                out.extend_from_slice(&v.to_le_bytes());
                out.extend_from_slice(&v.to_le_bytes());
                n += 1;
            }
            out
        };
        player.send_memblock(stream, &tone(missing));
        while !flag.load(Ordering::Relaxed) {
            if let Some((command, _, body)) = player.try_recv(Duration::from_millis(5))
                && command == wire::REQUEST
            {
                player.send_memblock(stream, &tone(request_length(&body)));
            }
        }
    });
    (stop, handle)
}

fn stop_player((stop, handle): (Arc<AtomicBool>, std::thread::JoinHandle<()>)) {
    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap();
}

/// The daemon's rate, as GET_SERVER_INFO gives it.
fn server_rate(conn: &mut RawPulse) -> u32 {
    let (command, _, body) = conn.command(wire::GET_SERVER_INFO, &[]);
    assert_eq!(command, wire::REPLY);
    let mut r = Reader::new(&body);
    for _ in 0..4 {
        r.string();
    }
    r.sample_spec().expect("a sample spec").rate
}

#[test]
fn record_a_monitor_recording_hears_a_player() {
    let daemon = Daemon::start();
    let player = tone_player(&daemon, 48_000);
    let mut recorder = pactl(&daemon);
    let stream = create_record_stream(&mut recorder, S16_STEREO_48K, "@DEFAULT_MONITOR@", 0);
    let (blocks, _) = recorder.collect(stream, Duration::from_secs(1));
    stop_player(player);
    assert!(!blocks.is_empty(), "no memblock within 1 s");
    assert!(blocks.iter().all(|block| block.len() == 4800), "each is fragsize");
    let peak = s16_peak(&blocks);
    assert!(peak > TONE / 2, "the monitor heard a peak of {peak}");
}

#[test]
fn record_peak_a_meter_at_144_hz_reads_the_monitor() {
    let daemon = Daemon::start();
    let player = tone_player(&daemon, 48_000);
    let mut recorder = pactl(&daemon);
    let spec = SampleSpec {
        format: wire::FORMAT_FLOAT32LE,
        channels: 1,
        rate: 144,
    };
    let body = record_body(spec, wire::INVALID_INDEX, Some("aas.monitor"), u32::MAX, false, 4, true, wire::INVALID_INDEX);
    let (command, _, reply) = recorder.command(wire::CREATE_RECORD_STREAM, &body);
    assert_eq!(command, wire::REPLY, "the meter is created");
    let stream = read_record_reply(&reply).stream;
    assert!(recorder.first_block(stream, Duration::from_secs(1)));
    recorder.blocks.clear();
    let start = Instant::now();
    let (blocks, _) = recorder.collect(stream, Duration::from_secs(1));
    let elapsed = start.elapsed().as_secs_f64();
    stop_player(player);
    assert!(blocks.iter().all(|block| block.len() == 4), "each is one value");
    let values: Vec<f32> = blocks
        .iter()
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let rate = values.len() as f64 / elapsed;
    assert!((rate - 144.0).abs() < 144.0 * 0.15, "{rate} values a second");
    let loud = values.iter().filter(|v| (0.4..0.55).contains(*v)).count();
    assert!(loud * 10 >= values.len() * 9, "values near 0.5: {values:?}");
}

#[test]
fn record_peak_low_rates_need_peak_detect() {
    let daemon = Daemon::start();
    let mut client = pactl(&daemon);
    let spec = SampleSpec {
        format: wire::FORMAT_FLOAT32LE,
        channels: 1,
        rate: 25,
    };
    let plain = record_body(spec, wire::INVALID_INDEX, Some("aas.monitor"), u32::MAX, false, 4, false, wire::INVALID_INDEX);
    let (command, _, _) = client.command(wire::CREATE_RECORD_STREAM, &plain);
    assert_eq!(command, wire::ERROR);
    let peak = record_body(spec, wire::INVALID_INDEX, Some("aas.monitor"), u32::MAX, false, 4, true, wire::INVALID_INDEX);
    let (command, _, _) = client.command(wire::CREATE_RECORD_STREAM, &peak);
    assert_eq!(command, wire::REPLY);
}

#[test]
fn record_a_recording_is_converted_to_the_clients_spec() {
    let daemon = Daemon::start();
    let player = tone_player(&daemon, 48_000);
    let mut recorder = pactl(&daemon);
    assert_eq!(server_rate(&mut recorder), 48_000);
    let spec = SampleSpec {
        format: wire::FORMAT_S16LE,
        channels: 1,
        rate: 44_100,
    };
    let stream = create_record_stream(&mut recorder, spec, "aas.monitor", 0);
    assert!(recorder.first_block(stream, Duration::from_secs(1)));
    recorder.blocks.clear();
    let start = Instant::now();
    let (blocks, _) = recorder.collect(stream, Duration::from_secs(1));
    let elapsed = start.elapsed().as_secs_f64();
    stop_player(player);
    let frames = blocks.iter().map(Vec::len).sum::<usize>() as f64 / 2.0;
    let rate = frames / elapsed;
    assert!((rate - 44_100.0).abs() < 44_100.0 * 0.05, "{rate} frames a second");
    let peak = s16_peak(&blocks);
    assert!(peak > TONE / 2, "the recording peaks at {peak}");
}

#[test]
fn record_fragsize_is_rounded_and_clamped() {
    let daemon = Daemon::start();
    let mut client = pactl(&daemon);
    for (asked, got) in [(0, 4800), (u32::MAX, 4800), (1001, 1000), (1 << 20, 65_536)] {
        let body = record_body(S16_STEREO_48K, 0, None, u32::MAX, true, asked, false, wire::INVALID_INDEX);
        let (command, _, reply) = client.command(wire::CREATE_RECORD_STREAM, &body);
        assert_eq!(command, wire::REPLY);
        let reply = read_record_reply(&reply);
        assert_eq!(reply.fragsize, got, "fragsize {asked}");
        assert!(reply.maxlength >= reply.fragsize);
        assert_eq!(reply.spec, S16_STEREO_48K);
        assert_eq!(reply.source, 0);
    }
}

#[test]
fn record_an_unknown_source_is_noentity() {
    let daemon = Daemon::start();
    let mut client = pactl(&daemon);
    for body in [
        record_body(S16_STEREO_48K, wire::INVALID_INDEX, Some("aas.analog"), u32::MAX, false, 0, false, wire::INVALID_INDEX),
        record_body(S16_STEREO_48K, 7, None, u32::MAX, false, 0, false, wire::INVALID_INDEX),
    ] {
        let (command, _, reply) = client.command(wire::CREATE_RECORD_STREAM, &body);
        assert_eq!(command, wire::ERROR);
        assert_eq!(Reader::new(&reply).u32(), Some(wire::ERR_NOENTITY));
    }
    let (command, _, _) = client.command(wire::GET_SERVER_INFO, &[]);
    assert_eq!(command, wire::REPLY);
}

#[test]
fn record_a_corked_recording_sends_nothing() {
    let daemon = Daemon::start();
    let mut client = pactl(&daemon);
    let body = record_body(S16_STEREO_48K, 0, None, u32::MAX, true, 0, false, wire::INVALID_INDEX);
    let (command, _, reply) = client.command(wire::CREATE_RECORD_STREAM, &body);
    assert_eq!(command, wire::REPLY);
    let stream = read_record_reply(&reply).stream;
    let (blocks, _) = client.collect(stream, Duration::from_millis(300));
    assert!(blocks.is_empty(), "a corked stream sent {} memblocks", blocks.len());
    let mut body = Builder::new();
    body.u32(stream);
    body.boolean(false);
    let (command, _, _) = client.command(wire::CORK_RECORD_STREAM, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    assert!(client.first_block(stream, Duration::from_secs(1)), "nothing once uncorked");
}

#[test]
fn record_a_recording_survives_a_rate_change() {
    let daemon = Daemon::start();
    let mut recorder = pactl(&daemon);
    let stream = create_record_stream(&mut recorder, S16_STEREO_48K, "aas.monitor", 0);
    let player = tone_player(&daemon, 48_000);
    assert!(recorder.first_block(stream, Duration::from_secs(1)));
    assert_eq!(server_rate(&mut recorder), 48_000);
    stop_player(player);
    let player = tone_player(&daemon, 44_100);
    let start = Instant::now();
    while server_rate(&mut recorder) != 44_100 {
        assert!(start.elapsed() < Duration::from_secs(2), "the rate never changed");
        std::thread::sleep(Duration::from_millis(20));
    }
    recorder.blocks.clear();
    let (blocks, packets) = recorder.collect(stream, Duration::from_millis(500));
    stop_player(player);
    assert!(
        packets.iter().all(|(command, _)| *command != wire::RECORD_STREAM_KILLED),
        "the stream was killed"
    );
    assert!(blocks.len() >= 5, "{} memblocks after the change", blocks.len());
    let peak = s16_peak(&blocks);
    assert!(peak > TONE / 2, "the recording peaks at {peak} after the change");
}

#[test]
fn record_a_client_that_stops_reading_gets_an_overflow() {
    let daemon = Daemon::start();
    let mut recorder = pactl(&daemon);
    let body = record_body(S16_STEREO_48K, 0, None, 9600, false, 0, false, wire::INVALID_INDEX);
    let (command, _, reply) = recorder.command(wire::CREATE_RECORD_STREAM, &body);
    assert_eq!(command, wire::REPLY);
    let reply = read_record_reply(&reply);
    assert_eq!(reply.maxlength, 9600);
    // 300 ms is about 58 KB, which the socket alone would hold: only the
    // stream's own limit makes this an overflow.
    let mut other = pactl(&daemon);
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        let asked = Instant::now();
        let (command, _, _) = other.command(wire::GET_SERVER_INFO, &[]);
        assert_eq!(command, wire::REPLY);
        assert!(asked.elapsed() < Duration::from_millis(200));
        std::thread::sleep(Duration::from_millis(50));
    }
    let (_, packets) = recorder.collect(reply.stream, Duration::from_millis(500));
    assert!(
        packets.iter().any(|(command, body)| *command == wire::OVERFLOW
            && Reader::new(body).u32() == Some(reply.stream)),
        "no OVERFLOW among {} packets",
        packets.len()
    );
}

#[test]
fn record_a_stalled_high_rate_recording_overflows_and_keeps_its_connection() {
    let daemon = Daemon::start();
    let mut recorder = pactl(&daemon);
    // 192 kHz float stereo is 1.5 MB a second. A maxlength of 1.5 MiB,
    // about a second, is beyond the 1 MiB a connection may queue.
    let spec = SampleSpec {
        format: wire::FORMAT_FLOAT32LE,
        channels: 2,
        rate: 192_000,
    };
    let body = record_body(spec, 0, None, 3 << 19, false, 0, false, wire::INVALID_INDEX);
    let (command, _, reply) = recorder.command(wire::CREATE_RECORD_STREAM, &body);
    assert_eq!(command, wire::REPLY);
    let reply = read_record_reply(&reply);
    assert_eq!(reply.maxlength, 3 << 19);
    // Long enough to record more than the maxlength.
    std::thread::sleep(Duration::from_millis(1500));
    let (_, packets) = recorder.collect(reply.stream, Duration::from_millis(300));
    assert!(
        packets.iter().any(|(command, body)| *command == wire::OVERFLOW
            && Reader::new(body).u32() == Some(reply.stream)),
        "no OVERFLOW among {} packets",
        packets.len()
    );
    let (command, _, _) = recorder.command(wire::GET_SERVER_INFO, &[]);
    assert_eq!(command, wire::REPLY, "the connection is still served");
}

/// Reads one source output info as libpulse 17 reads it for protocol 35
/// and returns its index, name, client and source.
fn read_source_output_as_libpulse(r: &mut Reader) -> (u32, String, u32, u32) {
    let index = r.u32().expect("index");
    let name = r.string().expect("name").to_string();
    assert_eq!(r.u32(), Some(wire::INVALID_INDEX), "no owner module");
    let client = r.u32().expect("client");
    let source = r.u32().expect("source");
    let spec = r.sample_spec().expect("sample spec");
    let map = r.channel_map().expect("channel map");
    assert_eq!(map.len(), usize::from(spec.channels));
    r.usec().expect("buffer latency");
    r.usec().expect("source latency");
    r.string().expect("resample method");
    assert_eq!(r.string(), Some("aas"), "driver");
    r.proplist().expect("proplist");
    r.boolean().expect("corked");
    assert_eq!(r.cvolume(), Some(vec![wire::VOLUME_NORM; usize::from(spec.channels)]));
    assert_eq!(r.boolean(), Some(false), "not muted");
    assert_eq!(r.boolean(), Some(true), "has volume");
    assert_eq!(r.boolean(), Some(false), "volume not writable");
    r.format_info().expect("format");
    (index, name, client, source)
}

/// The next subscription event of `facility`, as (change, index).
fn next_event(conn: &mut RawPulse, facility: u32) -> (u32, u32) {
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < Duration::from_secs(2), "no event");
        let Some((command, _, body)) = conn.try_recv(Duration::from_millis(100)) else {
            continue;
        };
        if command != wire::SUBSCRIBE_EVENT {
            continue;
        }
        let mut r = Reader::new(&body);
        let event = r.u32().unwrap();
        let index = r.u32().unwrap();
        if event & 0xf == facility {
            return (event & 0x30, index);
        }
    }
}

#[test]
fn record_source_outputs_are_listed_and_announced() {
    let daemon = Daemon::start();
    let mut listener = pactl(&daemon);
    let mut mask = Builder::new();
    mask.u32(wire::MASK_SOURCE_OUTPUT);
    let (command, _, _) = listener.command(wire::SUBSCRIBE, &mask.into_vec());
    assert_eq!(command, wire::REPLY);
    let mut recorder = RawPulse::connect(&daemon.pulse_socket());
    recorder.auth();
    let client = recorder.set_name("recorder");
    let stream = create_record_stream(&mut recorder, S16_STEREO_48K, "aas.monitor", 0);
    assert_eq!(
        next_event(&mut listener, wire::EVENT_SOURCE_OUTPUT),
        (wire::EVENT_NEW, stream)
    );
    let (command, _, body) = listener.command(wire::GET_SOURCE_OUTPUT_INFO_LIST, &[]);
    assert_eq!(command, wire::REPLY);
    let mut r = Reader::new(&body);
    let entry = read_source_output_as_libpulse(&mut r);
    assert!(r.eof(), "one source output");
    assert_eq!(entry, (stream, "a test recording".to_string(), client, 0));
    let mut body = Builder::new();
    body.u32(stream);
    let (command, _, reply) = listener.command(wire::GET_SOURCE_OUTPUT_INFO, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    assert_eq!(read_source_output_as_libpulse(&mut Reader::new(&reply)).0, stream);
    let mut body = Builder::new();
    body.u32(stream);
    let (command, _, _) = recorder.command(wire::DELETE_RECORD_STREAM, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    assert_eq!(
        next_event(&mut listener, wire::EVENT_SOURCE_OUTPUT),
        (wire::EVENT_REMOVE, stream)
    );
    let mut body = Builder::new();
    body.u32(stream);
    let (command, _, _) = listener.command(wire::GET_SOURCE_OUTPUT_INFO, &body.into_vec());
    assert_eq!(command, wire::ERROR);
}

#[test]
fn record_source_output_volume_is_fixed() {
    let daemon = Daemon::start();
    let mut client = pactl(&daemon);
    let stream = create_record_stream(&mut client, S16_STEREO_48K, "aas.monitor", 0);
    let mut body = Builder::new();
    body.u32(stream);
    body.cvolume(&[wire::VOLUME_NORM / 2; 2]);
    let (command, _, reply) = client.command(wire::SET_SOURCE_OUTPUT_VOLUME, &body.into_vec());
    assert_eq!(command, wire::ERROR);
    assert_eq!(Reader::new(&reply).u32(), Some(wire::ERR_NOTSUPPORTED));
    let mut body = Builder::new();
    body.u32(stream);
    body.boolean(true);
    let (command, _, reply) = client.command(wire::SET_SOURCE_OUTPUT_MUTE, &body.into_vec());
    assert_eq!(command, wire::ERROR);
    assert_eq!(Reader::new(&reply).u32(), Some(wire::ERR_NOTSUPPORTED));
}

#[test]
fn record_latency_is_answered() {
    let daemon = Daemon::start();
    let mut client = pactl(&daemon);
    let stream = create_record_stream(&mut client, S16_STEREO_48K, "aas.monitor", 0);
    assert!(client.first_block(stream, Duration::from_secs(1)));
    let mut body = Builder::new();
    body.u32(stream);
    body.timeval(12, 34);
    let (command, _, reply) = client.command(wire::GET_RECORD_LATENCY, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    let mut r = Reader::new(&reply);
    assert_eq!(r.usec(), Some(0), "monitor latency");
    r.usec().expect("source latency");
    assert_eq!(r.boolean(), Some(true), "playing");
    assert_eq!(r.timeval(), Some((12, 34)), "the request's time");
    r.timeval().expect("now");
    let write = r.i64().expect("write index");
    let read = r.i64().expect("read index");
    assert!(r.eof());
    assert!(read > 0 && write >= read, "write {write}, read {read}");
}

/// A player of a constant `level` (S16, both channels). Returns its sink
/// input index and what stops it: stopping deletes its playback stream.
fn const_player(
    daemon: &Daemon,
    level: i16,
) -> (u32, (Arc<AtomicBool>, std::thread::JoinHandle<()>)) {
    let stop = Arc::new(AtomicBool::new(false));
    let mut player = RawPulse::connect(&daemon.pulse_socket());
    player.auth();
    player.set_name("constant");
    let (stream, missing, _) = player.create_stream(48_000, wire::FORMAT_S16LE, 2, 200);
    let flag = stop.clone();
    let handle = std::thread::spawn(move || {
        let fill = |bytes: usize| level.to_le_bytes().repeat(bytes / 2);
        player.send_memblock(stream, &fill(missing));
        while !flag.load(Ordering::Relaxed) {
            if let Some((command, _, body)) = player.try_recv(Duration::from_millis(5))
                && command == wire::REQUEST
            {
                player.send_memblock(stream, &fill(request_length(&body)));
            }
        }
        let mut body = Builder::new();
        body.u32(stream);
        player.command(wire::DELETE_PLAYBACK_STREAM, &body.into_vec());
    });
    (stream, (stop, handle))
}

fn peak_meter_body(direct_on_input: u32) -> Vec<u8> {
    let spec = SampleSpec {
        format: wire::FORMAT_FLOAT32LE,
        channels: 1,
        rate: 144,
    };
    record_body(spec, wire::INVALID_INDEX, Some("aas.monitor"), u32::MAX, false, 4, true, direct_on_input)
}

#[test]
fn record_direct_a_meter_hears_one_sink_input() {
    let daemon = Daemon::start();
    let (_, loud) = const_player(&daemon, 16_384);
    let (quiet_index, quiet) = const_player(&daemon, 3_277);
    let mut recorder = pactl(&daemon);
    let body = peak_meter_body(quiet_index);
    let (command, _, reply) = recorder.command(wire::CREATE_RECORD_STREAM, &body);
    assert_eq!(command, wire::REPLY, "the meter is created");
    let stream = read_record_reply(&reply).stream;
    assert!(recorder.first_block(stream, Duration::from_secs(1)));
    recorder.blocks.clear();
    let (blocks, _) = recorder.collect(stream, Duration::from_secs(1));
    stop_player(loud);
    stop_player(quiet);
    let values: Vec<f32> = blocks
        .iter()
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    assert!(!values.is_empty());
    let near = values.iter().filter(|v| (0.08..0.12).contains(*v)).count();
    assert!(near * 10 >= values.len() * 9, "values near 0.1: {values:?}");
}

#[test]
fn record_direct_a_deleted_sink_input_kills_its_meter() {
    let daemon = Daemon::start();
    let (_, loud) = const_player(&daemon, 16_384);
    let (index, quiet) = const_player(&daemon, 3_277);
    let mut recorder = pactl(&daemon);
    let body = peak_meter_body(index);
    let (command, _, reply) = recorder.command(wire::CREATE_RECORD_STREAM, &body);
    assert_eq!(command, wire::REPLY, "the meter is created");
    let stream = read_record_reply(&reply).stream;
    assert!(recorder.first_block(stream, Duration::from_secs(1)));
    stop_player(quiet);
    let (_, packets) = recorder.collect(stream, Duration::from_secs(1));
    let killed = packets.iter().any(|(command, body)| {
        *command == wire::RECORD_STREAM_KILLED && Reader::new(body).u32() == Some(stream)
    });
    assert!(killed, "the meter is told it is gone");
    let (command, _, _) = recorder.command(wire::GET_SERVER_INFO, &[]);
    assert_eq!(command, wire::REPLY);
    stop_player(loud);
}

#[test]
fn record_direct_errors() {
    let config = aas::config::Config {
        latency_ms: Some(LATENCY_MS),
        analog: aas::config::Analog {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let daemon = Daemon::start_config(None, config);
    let (index, player) = const_player(&daemon, 3_277);
    let mut client = pactl(&daemon);
    let unknown = record_body(S16_STEREO_48K, wire::INVALID_INDEX, Some("aas.monitor"), u32::MAX, false, 0, false, 9_999);
    let analog = record_body(S16_STEREO_48K, wire::INVALID_INDEX, Some("aas.analog"), u32::MAX, false, 0, false, index);
    for (body, code) in [(unknown, wire::ERR_NOENTITY), (analog, wire::ERR_INVALID)] {
        let (command, _, reply) = client.command(wire::CREATE_RECORD_STREAM, &body);
        assert_eq!(command, wire::ERROR);
        assert_eq!(Reader::new(&reply).u32(), Some(code));
    }
    let (command, _, _) = client.command(wire::GET_SERVER_INFO, &[]);
    assert_eq!(command, wire::REPLY);
    stop_player(player);
}

#[test]
fn record_direct_a_meter_names_the_monitor_by_its_index() {
    let daemon = Daemon::start();
    let (index, player) = const_player(&daemon, 3_277);
    let mut recorder = pactl(&daemon);
    let spec = SampleSpec {
        format: wire::FORMAT_FLOAT32LE,
        channels: 1,
        rate: 144,
    };
    // As libpulse sends pavucontrol's request: no index, the index as name.
    let invalid = wire::INVALID_INDEX;
    let body = record_body(spec, invalid, Some("0"), u32::MAX, false, 4, true, index);
    let (command, _, reply) = recorder.command(wire::CREATE_RECORD_STREAM, &body);
    assert_eq!(command, wire::REPLY, "the meter is created");
    let reply = read_record_reply(&reply);
    assert_eq!(reply.source, 0);
    assert!(recorder.first_block(reply.stream, Duration::from_secs(1)));
    stop_player(player);
}

#[test]
fn a_decimal_name_names_the_sink_by_its_index() {
    let daemon = Daemon::start();
    let mut client = pactl(&daemon);
    let mut body = Builder::new();
    body.string("0");
    let (command, _, reply) = client.command(wire::LOOKUP_SINK, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    assert_eq!(Reader::new(&reply).u32(), Some(0));
    let mut body = Builder::new();
    body.u32(wire::INVALID_INDEX);
    body.string("0");
    let (command, _, _) = client.command(wire::GET_SINK_INFO, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    let mut body = Builder::new();
    body.string("1");
    let (command, _, _) = client.command(wire::LOOKUP_SINK, &body.into_vec());
    assert_eq!(command, wire::ERROR);
}

#[test]
fn record_a_source_output_carries_its_clients_application() {
    let daemon = Daemon::start();
    let mut recorder = RawPulse::connect(&daemon.pulse_socket());
    recorder.auth();
    let app = [
        ("application.name", "Volume Control"),
        ("application.id", "org.PulseAudio.pavucontrol"),
        ("application.process.binary", "pavucontrol"),
        ("window.x11.display", ":0"),
    ];
    let mut body = Builder::new();
    body.proplist(&app.map(|(k, v)| (k.to_string(), v.to_string())));
    let (command, _, _) = recorder.command(wire::SET_CLIENT_NAME, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    let stream = create_record_stream(&mut recorder, S16_STEREO_48K, "aas.monitor", 0);
    let mut body = Builder::new();
    body.u32(stream);
    let (command, _, reply) = recorder.command(wire::GET_SOURCE_OUTPUT_INFO, &body.into_vec());
    assert_eq!(command, wire::REPLY);
    let mut r = Reader::new(&reply);
    r.u32();
    r.string();
    r.u32();
    r.u32();
    r.u32();
    r.sample_spec();
    r.channel_map();
    r.usec();
    r.usec();
    r.string();
    r.string();
    let props = r.proplist().expect("a proplist");
    let get = |key: &str| props.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    assert_eq!(get("media.name"), Some("a test recording"));
    assert_eq!(get("application.id"), Some("org.PulseAudio.pavucontrol"));
    assert_eq!(get("application.name"), Some("Volume Control"));
    assert_eq!(get("application.process.binary"), Some("pavucontrol"));
    assert_eq!(get("window.x11.display"), None, "only the application's");
}
