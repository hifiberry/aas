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
    fn create_stream(&mut self, rate: u32, format: u8, channels: u8, tlength_ms: u32) -> (u32, u32) {
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
        let (command, _, reply) = self.command(wire::CREATE_PLAYBACK_STREAM, &body.into_vec());
        assert_eq!(command, wire::REPLY);
        let mut reader = Reader::new(&reply);
        let stream = reader.u32().expect("a stream index");
        assert_eq!(reader.u32(), Some(stream), "a sink-input index");
        assert!(reader.u32().unwrap() > 0, "the first request is not empty");
        (stream, frame)
    }

    fn send_memblock(&mut self, stream: u32, data: &[u8]) {
        let mut head = [0u8; 20];
        head[..4].copy_from_slice(&(data.len() as u32).to_be_bytes());
        head[4..8].copy_from_slice(&stream.to_be_bytes());
        self.stream.write_all(&head).unwrap();
        self.stream.write_all(data).unwrap();
    }

    fn request(&mut self) -> (u32, usize) {
        let (command, _, body) = self.recv();
        assert_eq!(command, wire::REQUEST);
        let mut reader = Reader::new(&body);
        let stream = reader.u32().expect("a stream");
        let length = reader.u32().expect("a request length") as usize;
        (stream, length)
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
    let (stream, frame) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    let (asked, length) = client.request();
    assert_eq!(asked, stream);
    assert_eq!(length % (frame as usize), 0);
    client.send_memblock(stream, &vec![0u8; length]);
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
    let (stream, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    let (_, length) = client.request();
    client.send_memblock(stream, &vec![0u8; length - 3]);
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
    let (first, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    let _ = client.request();
    let (second, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
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
    let (stream, _) = client.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    client.set_stream_volume(stream, 50);
    let (_, inputs) = daemon.get("/inputs");
    let found = inputs
        .as_array()
        .unwrap()
        .iter()
        .find(|input| {
            input["volume_db"]
                .as_f64()
                .is_some_and(|db| (db + 6.02).abs() < 0.1)
        })
        .expect("the stream's client at 50 percent");
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
    let (stream, _) = pulse.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    let (_, length) = pulse.request();
    pulse.send_memblock(stream, &vec![0u8; length]);
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
    let _ = first.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    drop(first);

    let mut second = RawPulse::connect(&daemon.pulse_socket());
    second.auth();
    second.set_name("two");
    let (stream, _) = second.create_stream(48_000, wire::FORMAT_S16LE, 2, 40);
    let (asked, _) = second.request();
    assert_eq!(asked, stream);
}
