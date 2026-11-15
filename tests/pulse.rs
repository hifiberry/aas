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
        RawPulse { stream, tag: 2 }
    }

    fn send(&mut self, payload: &[u8]) {
        let mut head = [0u8; 20];
        head[..4].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        head[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        self.stream.write_all(&head).unwrap();
        self.stream.write_all(payload).unwrap();
    }

    fn recv(&mut self) -> (u32, u32, Vec<u8>) {
        let mut head = [0u8; 20];
        self.stream.read_exact(&mut head).unwrap();
        let length = u32::from_be_bytes(head[..4].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; length];
        self.stream.read_exact(&mut payload).unwrap();
        let mut reader = Reader::new(&payload);
        let command = reader.u32().unwrap();
        let tag = reader.u32().unwrap();
        let body = payload[reader.position()..].to_vec();
        (command, tag, body)
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

    fn set_name(&mut self, name: &str) {
        let mut body = Builder::new();
        body.u32(wire::SET_CLIENT_NAME);
        body.u32(2);
        body.proplist(&[("application.name".to_string(), name.to_string())]);
        self.send(&body.into_vec());
        let (command, tag, body) = self.recv();
        assert_eq!((command, tag), (wire::REPLY, 2));
        let mut reader = Reader::new(&body);
        assert!(reader.u32().is_some(), "a client index");
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
    assert_eq!(reader.string(), None, "no default source");
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
        wire::GET_SOURCE_INFO_LIST,
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
    fn try_recv(&mut self, wait: Duration) -> Option<(u32, u32, Vec<u8>)> {
        self.stream.set_read_timeout(Some(wait)).unwrap();
        let mut head = [0u8; 20];
        let got = self.stream.read_exact(&mut head);
        self.stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        got.ok()?;
        let length = u32::from_be_bytes(head[..4].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; length];
        self.stream.read_exact(&mut payload).unwrap();
        let mut reader = Reader::new(&payload);
        let command = reader.u32().unwrap();
        let tag = reader.u32().unwrap();
        Some((command, tag, payload[reader.position()..].to_vec()))
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
