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
        self.recv()
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
