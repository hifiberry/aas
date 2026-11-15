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
