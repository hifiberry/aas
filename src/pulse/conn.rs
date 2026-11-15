//! One PulseAudio connection: its handshake, state and command dispatch.

use crate::pulse::objects;
use crate::pulse::packet::{Encoder, Frame, Reader as Framer};
use crate::pulse::registry::{Event, Registry};
use crate::pulse::stream::{Polled, Stream, now_timeval};
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
    for stream in &mut conn.streams {
        stream.release();
    }
    for stream in &conn.streams {
        conn.registry.remove_sink_input(stream.index);
    }
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
    streams: Vec<Stream>,
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
            streams: Vec::new(),
            done: false,
        }
    }

    fn handle(&mut self, frame: Frame) -> io::Result<()> {
        if let Some(memblock) = frame.memblock {
            if !self.authorized {
                return Err(invalid("a memblock before AUTH"));
            }
            self.handle_memblock(memblock);
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
            wire::GET_SINK_INPUT_INFO => self.sink_input_info(&mut reader, tag),
            wire::GET_SINK_INPUT_INFO_LIST => self.sink_input_info_list(tag),
            wire::CREATE_PLAYBACK_STREAM => self.create_stream(&mut reader, tag),
            wire::DELETE_PLAYBACK_STREAM => self.delete_stream(&mut reader, tag),
            wire::DRAIN_PLAYBACK_STREAM => self.drain_stream(&mut reader, tag),
            wire::CORK_PLAYBACK_STREAM => self.cork_stream(&mut reader, tag),
            wire::FLUSH_PLAYBACK_STREAM => self.flush_stream(&mut reader, tag),
            wire::TRIGGER_PLAYBACK_STREAM => self.trigger_stream(&mut reader, tag),
            wire::PREBUF_PLAYBACK_STREAM => self.prebuf_stream(&mut reader, tag),
            wire::SET_PLAYBACK_STREAM_BUFFER_ATTR => self.set_buffer_attr(&mut reader, tag),
            wire::SET_PLAYBACK_STREAM_NAME => self.set_stream_name(&mut reader, tag),
            wire::UPDATE_PLAYBACK_STREAM_PROPLIST | wire::REMOVE_PLAYBACK_STREAM_PROPLIST => {
                self.stream_proplist(&mut reader, tag)
            }
            wire::GET_PLAYBACK_LATENCY => self.playback_latency(&mut reader, tag),
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

    fn stream_mut(&mut self, index: u32) -> Option<&mut Stream> {
        self.streams.iter_mut().find(|stream| stream.index == index)
    }

    fn create_stream(&mut self, reader: &mut Reader, tag: u32) {
        let index = self.registry.next_stream();
        let created = Stream::create(
            &self.daemon_socket,
            self.peer_pid,
            self.buffer_ms,
            self.client_index,
            index,
            reader,
        );
        match created {
            Ok(created) => {
                self.registry.add_sink_input(created.stream.entry());
                self.streams.push(created.stream);
                let request = self.streams.last_mut().and_then(Stream::request);
                self.send_reply(tag, created.reply);
                if let Some(request) = request {
                    self.send_packet(request);
                }
            }
            Err(e) => {
                eprintln!("aas: pulse: create: {}", e.message);
                self.send_error(tag, e.code, e.message)
            }
        }
    }

    fn delete_stream(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        if let Some(position) = self.streams.iter().position(|stream| stream.index == index) {
            let mut stream = self.streams.remove(position);
            stream.release();
            self.registry.remove_sink_input(index);
            self.send_reply(tag, Vec::new());
        } else {
            self.send_error(tag, wire::ERR_NOENTITY, "no such stream");
        }
    }

    fn drain_stream(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        match self.stream_mut(index) {
            Some(stream) => match stream.drain(tag) {
                Ok(()) => {}
                Err(_) => self.send_error(tag, wire::ERR_NOENTITY, "the stream is gone"),
            },
            None => self.send_error(tag, wire::ERR_NOENTITY, "no such stream"),
        }
    }

    fn cork_stream(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        let Some(corked) = reader.boolean() else {
            self.send_error(tag, wire::ERR_INVALID, "no cork flag");
            return;
        };
        match self.stream_mut(index) {
            Some(stream) => match stream.cork(corked) {
                Ok(()) => {
                    let entry = stream.entry();
                    self.registry.update_sink_input(entry);
                    self.send_reply(tag, Vec::new());
                }
                Err(_) => self.send_error(tag, wire::ERR_NOENTITY, "the stream is gone"),
            },
            None => self.send_error(tag, wire::ERR_NOENTITY, "no such stream"),
        }
    }

    fn flush_stream(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        match self.stream_mut(index) {
            Some(stream) => match stream.flush() {
                Ok(()) => self.send_reply(tag, Vec::new()),
                Err(_) => self.send_error(tag, wire::ERR_NOENTITY, "the stream is gone"),
            },
            None => self.send_error(tag, wire::ERR_NOENTITY, "no such stream"),
        }
    }

    fn trigger_stream(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        if reader.timeval().is_none() {
            self.send_error(tag, wire::ERR_PROTOCOL, "no trigger time");
            return;
        }
        match self.stream_mut(index) {
            Some(stream) => match stream.trigger() {
                Ok(()) => self.send_reply(tag, Vec::new()),
                Err(_) => self.send_error(tag, wire::ERR_NOENTITY, "the stream is gone"),
            },
            None => self.send_error(tag, wire::ERR_NOENTITY, "no such stream"),
        }
    }

    fn prebuf_stream(&mut self, reader: &mut Reader, tag: u32) {
        if reader.u32().is_none() {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        }
        self.send_reply(tag, Vec::new());
    }

    fn set_buffer_attr(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        for _ in 0..4 {
            if reader.u32().is_none() {
                self.send_error(tag, wire::ERR_PROTOCOL, "a short buffer attr");
                return;
            }
        }
        let _ = reader.boolean();
        let _ = reader.boolean();
        let Some(stream) = self.stream_mut(index) else {
            self.send_error(tag, wire::ERR_NOENTITY, "no such stream");
            return;
        };
        let bytes = stream.capacity_bytes();
        let mut body = Builder::new();
        body.u32(bytes);
        body.u32(bytes);
        body.u32(0);
        body.u32(0);
        body.usec(0);
        self.send_reply(tag, body.into_vec());
    }

    fn set_stream_name(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        let Some(name) = reader.string() else {
            self.send_error(tag, wire::ERR_INVALID, "no name");
            return;
        };
        match self.stream_mut(index) {
            Some(stream) => {
                stream.name = name.to_string();
                let entry = stream.entry();
                self.registry.update_sink_input(entry);
                self.send_reply(tag, Vec::new());
            }
            None => self.send_error(tag, wire::ERR_NOENTITY, "no such stream"),
        }
    }

    fn stream_proplist(&mut self, reader: &mut Reader, tag: u32) {
        if reader.u32().is_none() {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        }
        self.send_reply(tag, Vec::new());
    }

    fn playback_latency(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        let Some((sec, usec)) = reader.timeval() else {
            self.send_error(tag, wire::ERR_PROTOCOL, "no request time");
            return;
        };
        let Some(stream) = self.stream_mut(index) else {
            self.send_error(tag, wire::ERR_NOENTITY, "no such stream");
            return;
        };
        let latency = stream.latency();
        let (now_sec, now_usec) = now_timeval();
        let mut body = Builder::new();
        body.usec(latency.sink_usec);
        body.usec(0);
        body.boolean(latency.playing);
        body.timeval(sec, usec);
        body.timeval(now_sec, now_usec);
        body.i64(latency.write_index);
        body.i64(latency.read_index);
        body.u64(latency.underrun_for);
        body.u64(latency.playing_for);
        self.send_reply(tag, body.into_vec());
    }

    fn sink_input_info(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        match self.registry.sink_input(index) {
            Some(input) => self.send_reply(tag, objects::sink_input_info(&input)),
            None => self.send_error(tag, wire::ERR_NOENTITY, "no such stream"),
        }
    }

    fn sink_input_info_list(&mut self, tag: u32) {
        let mut body = Vec::new();
        for input in self.registry.sink_inputs() {
            body.extend(objects::sink_input_info(&input));
        }
        self.send_reply(tag, body);
    }

    fn handle_memblock(&mut self, memblock: crate::pulse::packet::Memblock) {
        if let Some(stream) = self.stream_mut(memblock.stream) {
            stream.write(&memblock.data);
            if let Some(request) = stream.request() {
                self.send_packet(request);
            }
        }
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
        let mut packets = Vec::new();
        let mut dead = Vec::new();
        for stream in &mut self.streams {
            match stream.poll() {
                Polled::Packets(polled) => packets.extend(polled),
                Polled::Gone => dead.push(stream.index),
            }
        }
        for packet in packets {
            self.send_packet(packet);
        }
        for index in dead {
            self.streams.retain(|stream| stream.index != index);
            self.registry.remove_sink_input(index);
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
