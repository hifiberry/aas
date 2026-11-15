//! One PulseAudio connection: its handshake, state and command dispatch.

use crate::pulse::objects;
use crate::pulse::packet::{Encoder, Frame, Reader as Framer};
use crate::pulse::registry::{Event, Registry};
use crate::pulse::tagstruct::{Builder, Reader};
use crate::pulse::wire;
use aas_client::client::Client as DaemonClient;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_millis(200);

static NEXT_CLIENT: AtomicU32 = AtomicU32::new(1);

pub fn serve(
    stream: UnixStream,
    daemon_socket: &std::path::Path,
    buffer_ms: u32,
    stop: &AtomicBool,
    shutdown: &AtomicBool,
    registry: Arc<Registry>,
) {
    let peer_pid = peer_pid(&stream);
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    let mut conn = Conn::new(stream, daemon_socket, buffer_ms, peer_pid, registry);
    let mut buf = [0u8; 8192];
    while !stop.load(Ordering::Relaxed) && !shutdown.load(Ordering::Relaxed) && !conn.done {
        match conn.stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => match conn.framer.read(&buf[..n]) {
                Ok(frames) => {
                    for frame in frames {
                        if conn.handle(frame).is_err() {
                            return;
                        }
                    }
                }
                Err(_) => return,
            },
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => break,
        }
        if conn.tick().is_err() {
            break;
        }
        if conn.flush().is_err() {
            break;
        }
    }
    let _ = conn.flush();
    conn.registry.remove_client(conn.client_index);
}

pub struct Conn {
    stream: UnixStream,
    framer: Framer,
    out: Vec<u8>,
    daemon_socket: std::path::PathBuf,
    buffer_ms: u32,
    peer_pid: Option<u32>,
    authorized: bool,
    version: u32,
    client_index: u32,
    name: String,
    proplist: Vec<(String, String)>,
    control: Option<DaemonClient>,
    registry: Arc<Registry>,
    events: Receiver<Event>,
    subscribed: u32,
    done: bool,
}

impl Conn {
    fn new(
        stream: UnixStream,
        daemon_socket: &std::path::Path,
        buffer_ms: u32,
        peer_pid: Option<u32>,
        registry: Arc<Registry>,
    ) -> Conn {
        let client_index = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
        let name = format!("pulse-{client_index}");
        registry.add_client(client_index, name.clone());
        let events = registry.subscribe();
        Conn {
            control: DaemonClient::connect(daemon_socket).ok(),
            stream,
            framer: Framer::new(),
            out: Vec::new(),
            daemon_socket: daemon_socket.to_path_buf(),
            buffer_ms,
            peer_pid,
            authorized: false,
            version: 0,
            client_index,
            name,
            proplist: Vec::new(),
            registry,
            events,
            subscribed: 0,
            done: false,
        }
    }

    fn handle(&mut self, frame: Frame) -> io::Result<()> {
        if let Some(memblock) = frame.memblock {
            if !self.authorized {
                return Err(invalid("a memblock before AUTH"));
            }
            let _ = memblock;
            return Ok(());
        }
        self.command(&frame.payload)
    }

    fn command(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut reader = Reader::new(payload);
        let command = reader.u32().ok_or_else(|| invalid("a packet without a command"))?;
        let tag = reader.u32().ok_or_else(|| invalid("a packet without a tag"))?;
        if command == wire::AUTH {
            return self.auth(&mut reader, tag);
        }
        if !self.authorized {
            self.send_error(tag, wire::ERR_ACCESS, "not authorized");
            return Ok(());
        }
        match command {
            wire::EXIT => {
                self.done = true;
            }
            wire::SET_CLIENT_NAME => return self.set_name(&mut reader, tag),
            wire::GET_SERVER_INFO => self.server_info(tag),
            wire::GET_SINK_INFO => self.sink_info(&mut reader, tag),
            wire::GET_SINK_INFO_LIST => self.sink_info_list(tag),
            wire::GET_CLIENT_INFO => self.client_info(&mut reader, tag),
            wire::GET_CLIENT_INFO_LIST => self.client_info_list(tag),
            wire::GET_SINK_INPUT_INFO => {
                self.send_error(tag, wire::ERR_NOENTITY, "no such stream");
            }
            wire::GET_SINK_INPUT_INFO_LIST => self.send_reply(tag, Vec::new()),
            wire::GET_CARD_INFO_LIST
            | wire::GET_MODULE_INFO_LIST
            | wire::GET_SAMPLE_INFO_LIST
            | wire::GET_SOURCE_INFO_LIST => self.send_reply(tag, Vec::new()),
            wire::STAT => self.stat(tag),
            wire::LOOKUP_SINK => self.lookup_sink(&mut reader, tag),
            wire::SUBSCRIBE => self.subscribe(&mut reader, tag),
            _ => self.send_error(tag, wire::ERR_NOTSUPPORTED, "not supported"),
        }
        Ok(())
    }

    fn auth(&mut self, reader: &mut Reader, tag: u32) -> io::Result<()> {
        let version = reader.u32().ok_or_else(|| invalid("AUTH without a version"))?;
        let cookie = reader
            .arbitrary()
            .ok_or_else(|| invalid("AUTH without a cookie"))?;
        if cookie.len() != 256 {
            self.send_error(tag, wire::ERR_INVALID, "a cookie of 256 bytes is needed");
            return Ok(());
        }
        if !reader.eof() {
            self.send_error(tag, wire::ERR_PROTOCOL, "trailing AUTH data");
            return Ok(());
        }
        let version = version & 0xffff;
        if version < 8 {
            self.send_error(tag, wire::ERR_VERSION, "a protocol older than 8");
            return Ok(());
        }
        self.version = version;
        self.authorized = true;
        let mut body = Builder::new();
        body.u32(wire::PROTOCOL_VERSION);
        self.send_reply(tag, body.into_vec());
        Ok(())
    }

    fn set_name(&mut self, reader: &mut Reader, tag: u32) -> io::Result<()> {
        let props = reader
            .proplist()
            .ok_or_else(|| invalid("SET_CLIENT_NAME without a proplist"))?;
        if !reader.eof() {
            self.send_error(tag, wire::ERR_PROTOCOL, "trailing SET_CLIENT_NAME data");
            return Ok(());
        }
        if let Some((_, name)) = props.iter().find(|(key, _)| key == "application.name") {
            self.name = name.clone();
        }
        self.registry.rename_client(self.client_index, self.name.clone());
        self.proplist = props;
        let mut body = Builder::new();
        if self.version >= 13 {
            body.u32(self.client_index);
        }
        self.send_reply(tag, body.into_vec());
        Ok(())
    }

    fn daemon_rate(&mut self) -> u32 {
        self.control
            .as_mut()
            .and_then(|client| client.output_rate().ok())
            .filter(|&rate| rate != 0)
            .unwrap_or(48_000)
    }

    fn daemon_volume(&mut self) -> u8 {
        self.control
            .as_mut()
            .and_then(|client| client.volume().ok())
            .unwrap_or(100)
    }

    fn server_info(&mut self, tag: u32) {
        let rate = self.daemon_rate();
        let user = std::env::var("USER").unwrap_or_else(|_| "aas".into());
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|name| name.trim_end().to_string())
            .unwrap_or_else(|_| "aas".into());
        let body = objects::server_info(rate, &user, &host);
        self.send_reply(tag, body);
    }

    fn sink_info(&mut self, reader: &mut Reader, tag: u32) {
        let index = reader.u32();
        let name = reader.string();
        let ours = index.is_none_or(|index| index == 0 || index == wire::INVALID_INDEX)
            && (name.is_none() || name == Some("aas"));
        if !ours {
            self.send_error(tag, wire::ERR_NOENTITY, "no such sink");
            return;
        }
        let rate = self.daemon_rate();
        let volume = self.daemon_volume();
        let body = objects::sink_info(rate, volume, false, 0, 1);
        self.send_reply(tag, body);
    }

    fn sink_info_list(&mut self, tag: u32) {
        let rate = self.daemon_rate();
        let volume = self.daemon_volume();
        let body = objects::sink_info(rate, volume, false, 0, 1);
        self.send_reply(tag, body);
    }

    fn client_info(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no client index");
            return;
        };
        let Some((index, name)) = self
            .registry
            .clients()
            .into_iter()
            .find(|(found, _)| *found == index)
        else {
            self.send_error(tag, wire::ERR_NOENTITY, "no such client");
            return;
        };
        let body = objects::client_info(index, &name);
        self.send_reply(tag, body);
    }

    fn client_info_list(&mut self, tag: u32) {
        let mut body = Vec::new();
        for (index, name) in self.registry.clients() {
            body.extend(objects::client_info(index, &name));
        }
        self.send_reply(tag, body);
    }

    fn stat(&mut self, tag: u32) {
        let mut body = Builder::new();
        for _ in 0..5 {
            body.u32(0);
        }
        self.send_reply(tag, body.into_vec());
    }

    fn lookup_sink(&mut self, reader: &mut Reader, tag: u32) {
        let index = reader.u32();
        let name = reader.string();
        if index == Some(0) || name == Some("aas") {
            let mut body = Builder::new();
            body.u32(0);
            self.send_reply(tag, body.into_vec());
        } else {
            self.send_error(tag, wire::ERR_NOENTITY, "no such sink");
        }
    }

    fn subscribe(&mut self, reader: &mut Reader, tag: u32) {
        let Some(mask) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no subscription mask");
            return;
        };
        if mask & !wire::MASK_ALL != 0 {
            self.send_error(tag, wire::ERR_INVALID, "an unknown subscription mask");
            return;
        }
        self.subscribed = mask;
        self.send_reply(tag, Vec::new());
    }

    fn tick(&mut self) -> io::Result<()> {
        let events: Vec<Event> = self.events.try_iter().collect();
        for event in events {
            if self.subscribed & (1 << event.facility) != 0 {
                self.send_packet(wire::subscribe_event(
                    event.facility | event.change,
                    event.index,
                ));
            }
        }
        Ok(())
    }

    fn send_reply(&mut self, tag: u32, body: Vec<u8>) {
        self.send_packet(wire::reply(tag, body));
    }

    fn send_error(&mut self, tag: u32, code: u32, message: &str) {
        self.send_packet(wire::error(tag, code, message));
    }

    fn send_packet(&mut self, payload: Vec<u8>) {
        self.out
            .extend(Encoder.packet(wire::INVALID_INDEX, &payload));
    }

    fn flush(&mut self) -> io::Result<()> {
        while !self.out.is_empty() {
            match self.stream.write(&self.out) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "socket closed")),
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

fn peer_pid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    // SAFETY: an all-zero ucred is valid, and getsockopt fills at most len.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}
