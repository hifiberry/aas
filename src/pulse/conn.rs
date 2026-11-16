//! One PulseAudio connection: its handshake, state and command dispatch.

use crate::pulse::Settings;
use crate::pulse::objects::{self, Source};
use crate::pulse::packet::{Encoder, Frame, Reader as Framer};
use crate::pulse::record::Record;
use crate::pulse::registry::{Event, Registry, Remote};
use crate::pulse::stream::{Stream, now_timeval};
use crate::pulse::volume;
use crate::pulse::tagstruct::{Builder, Reader};
use crate::pulse::wire;
use aas_client::client::Client as DaemonClient;
use aas_client::proto::Reply;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// How long a read waits while no stream plays. It bounds how late the
/// connection notices a stop or a shutdown.
const IDLE_TICK: Duration = Duration::from_millis(200);
/// The bounds of the wait while a stream plays. Between them the wait is an
/// eighth of the smallest ring, so room is offered well before a ring that
/// was full runs out.
pub(crate) const MIN_TICK: Duration = Duration::from_millis(2);
pub(crate) const MAX_TICK: Duration = Duration::from_millis(20);
/// How long a write may block before the connection gives up on it.
const WRITE_TIMEOUT: Duration = Duration::from_millis(200);

static NEXT_CLIENT: AtomicU32 = AtomicU32::new(1);

pub fn serve(
    stream: UnixStream,
    daemon_socket: &std::path::Path,
    settings: Settings,
    stop: &AtomicBool,
    shutdown: &AtomicBool,
    registry: Arc<Registry>,
) {
    let peer_pid = peer_pid(&stream);
    let _ = stream.set_read_timeout(Some(IDLE_TICK));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let mut conn = Conn::new(stream, daemon_socket, settings, peer_pid, registry);
    let mut buf = [0u8; 8192];
    let mut tick = IDLE_TICK;
    while !stop.load(Ordering::Relaxed) && !shutdown.load(Ordering::Relaxed) && !conn.done {
        let wanted = conn.tick_interval();
        if wanted != tick {
            tick = wanted;
            let _ = conn.stream.set_read_timeout(Some(tick));
        }
        match conn.stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => match conn.framer.read(&buf[..n]) {
                Ok(frames) => {
                    let mut ok = true;
                    for frame in frames {
                        if conn.handle(frame).is_err() {
                            ok = false;
                            break;
                        }
                    }
                    if !ok {
                        break;
                    }
                }
                Err(_) => break,
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
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.registry.remove_client(self.client_index);
        for stream in &self.streams {
            self.registry.remove_sink_input(stream.index);
        }
        for record in &mut self.records {
            record.release();
            self.registry.remove_source_output(record.index);
        }
    }
}

pub struct Conn {
    stream: UnixStream,
    framer: Framer,
    out: Vec<u8>,
    daemon_socket: std::path::PathBuf,
    settings: Settings,
    peer_pid: Option<u32>,
    authorized: bool,
    version: u32,
    client_index: u32,
    name: String,
    proplist: Vec<(String, String)>,
    control: Option<DaemonClient>,
    registry: Arc<Registry>,
    events: Receiver<Event>,
    inbox: Receiver<Remote>,
    subscribed: u32,
    streams: Vec<Stream>,
    records: Vec<Record>,
    done: bool,
}

impl Conn {
    fn new(
        stream: UnixStream,
        daemon_socket: &std::path::Path,
        settings: Settings,
        peer_pid: Option<u32>,
        registry: Arc<Registry>,
    ) -> Conn {
        let client_index = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
        let name = format!("pulse-{client_index}");
        registry.add_client(client_index, name.clone());
        let events = registry.subscribe();
        let inbox = registry.inbox(client_index);
        Conn {
            control: DaemonClient::connect(daemon_socket).ok(),
            stream,
            framer: Framer::new(),
            out: Vec::new(),
            daemon_socket: daemon_socket.to_path_buf(),
            settings,
            peer_pid,
            authorized: false,
            version: 0,
            client_index,
            name,
            proplist: Vec::new(),
            registry,
            events,
            inbox,
            subscribed: 0,
            streams: Vec::new(),
            records: Vec::new(),
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
            wire::SET_SINK_VOLUME => self.set_sink_volume(&mut reader, tag),
            wire::SET_SINK_MUTE => self.set_sink_mute(&mut reader, tag),
            wire::SET_SINK_INPUT_VOLUME => self.set_sink_input_volume(&mut reader, tag),
            wire::SET_SINK_INPUT_MUTE => self.set_sink_input_mute(&mut reader, tag),
            wire::SET_DEFAULT_SINK => self.set_default_sink(&mut reader, tag),
            wire::GET_SOURCE_INFO => self.source_info(&mut reader, tag),
            wire::GET_SOURCE_INFO_LIST => self.source_info_list(tag),
            wire::LOOKUP_SOURCE => self.lookup_source(&mut reader, tag),
            wire::SET_DEFAULT_SOURCE => self.set_default_source(&mut reader, tag),
            wire::SET_SOURCE_VOLUME
            | wire::SET_SOURCE_MUTE
            | wire::SET_SOURCE_OUTPUT_VOLUME
            | wire::SET_SOURCE_OUTPUT_MUTE => {
                self.send_error(tag, wire::ERR_NOTSUPPORTED, "a source's volume is fixed")
            }
            wire::GET_SOURCE_OUTPUT_INFO => self.source_output_info(&mut reader, tag),
            wire::GET_SOURCE_OUTPUT_INFO_LIST => self.source_output_info_list(tag),
            wire::CREATE_RECORD_STREAM => self.create_record(&mut reader, tag),
            wire::DELETE_RECORD_STREAM => self.delete_record(&mut reader, tag),
            wire::CORK_RECORD_STREAM => self.cork_record(&mut reader, tag),
            wire::FLUSH_RECORD_STREAM => self.flush_record(&mut reader, tag),
            wire::GET_RECORD_LATENCY => self.record_latency(&mut reader, tag),
            wire::SET_RECORD_STREAM_BUFFER_ATTR => self.record_buffer_attr(&mut reader, tag),
            wire::SET_RECORD_STREAM_NAME => self.set_record_name(&mut reader, tag),
            wire::UPDATE_RECORD_STREAM_PROPLIST | wire::REMOVE_RECORD_STREAM_PROPLIST => {
                self.record_proplist(command, &mut reader, tag)
            }
            wire::GET_CARD_INFO_LIST | wire::GET_MODULE_INFO_LIST | wire::GET_SAMPLE_INFO_LIST => {
                self.send_reply(tag, Vec::new())
            }
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
        if version < 21 {
            self.send_error(tag, wire::ERR_VERSION, "a protocol older than 21");
            return Ok(());
        }
        self.version = version;
        self.authorized = true;
        if let Some(control) = &mut self.control {
            let _ = control.subscribe_volume();
        }
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
        let body = objects::server_info(rate, &user, &host, self.registry.default_source());
        self.send_reply(tag, body);
    }

    fn sink_info(&mut self, reader: &mut Reader, tag: u32) {
        let index = reader.u32();
        let name = reader.string();
        if !is_our_sink(index, name) {
            self.send_error(tag, wire::ERR_NOENTITY, "no such sink");
            return;
        }
        let body = self.sink_info_body();
        self.send_reply(tag, body);
    }

    fn sink_info_list(&mut self, tag: u32) {
        let body = self.sink_info_body();
        self.send_reply(tag, body);
    }

    fn sink_info_body(&mut self) -> Vec<u8> {
        let rate = self.daemon_rate();
        let level = self.daemon_volume();
        let mute = self.registry.sink_muted().is_some();
        let state = if self
            .registry
            .sink_inputs()
            .iter()
            .any(|input| !input.corked)
        {
            0
        } else {
            1
        };
        let latency = self.registry.sink_latency();
        objects::sink_info(rate, level, mute, latency, state)
    }

    fn find_source(&self, index: Option<u32>, name: Option<&str>) -> Option<Source> {
        let default = self.registry.default_source();
        objects::find_source(index, name, default, self.settings.analog)
    }

    fn source_info_body(&mut self, source: Source) -> Vec<u8> {
        let rate = self.daemon_rate();
        // The monitor hears what the sink plays, so it lags as the sink
        // does; nothing measures the analog input's latency yet.
        let latency = match source {
            Source::Monitor => self.registry.sink_latency(),
            Source::Analog => 0,
        };
        objects::source_info(source, rate, latency)
    }

    fn source_info(&mut self, reader: &mut Reader, tag: u32) {
        let index = reader.u32();
        let name = reader.string();
        let Some(source) = self.find_source(index, name) else {
            self.send_error(tag, wire::ERR_NOENTITY, "no such source");
            return;
        };
        let body = self.source_info_body(source);
        self.send_reply(tag, body);
    }

    fn source_info_list(&mut self, tag: u32) {
        let mut body = Vec::new();
        for source in objects::sources(self.settings.analog) {
            body.extend(self.source_info_body(source));
        }
        self.send_reply(tag, body);
    }

    fn lookup_source(&mut self, reader: &mut Reader, tag: u32) {
        let Some(name) = reader.string() else {
            self.send_error(tag, wire::ERR_INVALID, "no source name");
            return;
        };
        match self.find_source(None, Some(name)) {
            Some(source) => {
                let mut body = Builder::new();
                body.u32(source.index());
                self.send_reply(tag, body.into_vec());
            }
            None => self.send_error(tag, wire::ERR_NOENTITY, "no such source"),
        }
    }

    fn set_default_source(&mut self, reader: &mut Reader, tag: u32) {
        let Some(name) = reader.string() else {
            self.send_error(tag, wire::ERR_INVALID, "no source name");
            return;
        };
        if name == "@NONE@" {
            self.send_reply(tag, Vec::new());
            return;
        }
        match self.find_source(None, Some(name)) {
            Some(source) => {
                self.registry.set_default_source(source);
                self.send_reply(tag, Vec::new());
            }
            None => self.send_error(tag, wire::ERR_NOENTITY, "no such source"),
        }
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
        let name = reader.string();
        if name.is_some_and(is_our_sink_name) {
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
            self.settings.buffer_ms,
            self.client_index,
            index,
            reader,
        );
        match created {
            Ok(created) => {
                self.registry.add_sink_input(created.stream.entry());
                self.streams.push(created.stream);
                self.send_reply(tag, created.reply);
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
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        if self.stream_mut(index).is_none() {
            self.send_error(tag, wire::ERR_NOENTITY, "no such stream");
            return;
        }
        self.send_reply(tag, Vec::new());
    }

    fn set_buffer_attr(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        let mut attr = [0u32; 4];
        for value in &mut attr {
            let Some(read) = reader.u32() else {
                self.send_error(tag, wire::ERR_PROTOCOL, "a short buffer attr");
                return;
            };
            *value = read;
        }
        let _ = reader.boolean();
        let _ = reader.boolean();
        let Some(stream) = self.stream_mut(index) else {
            self.send_error(tag, wire::ERR_NOENTITY, "no such stream");
            return;
        };
        // maxlength, tlength, prebuf, minreq: the ring cannot change size,
        // so tlength and minreq are honoured within it and the reply says
        // what holds.
        let [maxlength, tlength, prebuf, minreq] = stream.set_buffer_attr(attr[1], attr[3]);
        let configured = stream.latency().sink_usec
            + u64::from(tlength) * 1_000_000 / (u64::from(stream.rate) * stream.frame_bytes as u64).max(1);
        let mut body = Builder::new();
        body.u32(maxlength);
        body.u32(tlength);
        body.u32(prebuf);
        body.u32(minreq);
        body.usec(configured);
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
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        if self.stream_mut(index).is_none() {
            self.send_error(tag, wire::ERR_NOENTITY, "no such stream");
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

    fn set_sink_volume(&mut self, reader: &mut Reader, tag: u32) {
        let index = reader.u32();
        let name = reader.string();
        let Some(cvolume) = reader.cvolume() else {
            self.send_error(tag, wire::ERR_INVALID, "no volume");
            return;
        };
        if !is_our_sink(index, name) {
            self.send_error(tag, wire::ERR_NOENTITY, "no such sink");
            return;
        }
        let percent = volume::percent(&cvolume);
        if let Some(control) = &mut self.control {
            let _ = control.set_volume(percent);
        }
        self.registry.set_sink_muted(None);
        self.send_reply(tag, Vec::new());
    }

    fn set_sink_mute(&mut self, reader: &mut Reader, tag: u32) {
        let index = reader.u32();
        let name = reader.string();
        let Some(mute) = reader.boolean() else {
            self.send_error(tag, wire::ERR_INVALID, "no mute flag");
            return;
        };
        if !is_our_sink(index, name) {
            self.send_error(tag, wire::ERR_NOENTITY, "no such sink");
            return;
        }
        if mute {
            if self.registry.sink_muted().is_none() {
                let current = self.daemon_volume();
                self.registry.set_sink_muted(Some(current));
            }
            if let Some(control) = &mut self.control {
                let _ = control.set_volume(0);
            }
        } else if let Some(previous) = self.registry.sink_muted() {
            self.registry.set_sink_muted(None);
            if let Some(control) = &mut self.control {
                let _ = control.set_volume(previous);
            }
        }
        self.send_reply(tag, Vec::new());
    }

    fn set_sink_input_volume(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        let Some(cvolume) = reader.cvolume() else {
            self.send_error(tag, wire::ERR_INVALID, "no volume");
            return;
        };
        let change = Remote::Volume {
            stream: index,
            db_millis: volume::db_millis(&cvolume),
            silent: volume::is_silent(&cvolume),
        };
        self.change_stream(tag, change);
    }

    fn set_sink_input_mute(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        let Some(muted) = reader.boolean() else {
            self.send_error(tag, wire::ERR_INVALID, "no mute flag");
            return;
        };
        self.change_stream(tag, Remote::Mute { stream: index, muted });
    }

    /// Applies a stream change here if this connection owns the stream,
    /// and otherwise hands it to the connection that does, as `pactl` or a
    /// mixer changes another program's stream.
    fn change_stream(&mut self, tag: u32, change: Remote) {
        let index = match change {
            Remote::Volume { stream, .. } | Remote::Mute { stream, .. } => stream,
        };
        if self.apply(change) {
            self.send_reply(tag, Vec::new());
            return;
        }
        let delivered = self
            .registry
            .sink_input(index)
            .is_some_and(|input| self.registry.send_remote(input.client, change));
        if delivered {
            self.send_reply(tag, Vec::new());
        } else {
            self.send_error(tag, wire::ERR_NOENTITY, "no such stream");
        }
    }

    /// Applies a stream change to every stream of the same client, since
    /// they share one gain. False if this connection has no such stream.
    fn apply(&mut self, change: Remote) -> bool {
        let index = match change {
            Remote::Volume { stream, .. } | Remote::Mute { stream, .. } => stream,
        };
        let Some(stream) = self.streams.iter().find(|stream| stream.index == index) else {
            return false;
        };
        let (client, db_millis, silent, muted) = match change {
            Remote::Volume {
                db_millis, silent, ..
            } => (stream.client, db_millis, silent, stream.mute),
            Remote::Mute { muted, .. } => (stream.client, stream.db_millis, stream.silent, muted),
        };
        for stream in self.streams.iter_mut().filter(|stream| stream.client == client) {
            let _ = stream.set_volume(db_millis, silent, muted);
            let entry = stream.entry();
            self.registry.update_sink_input(entry);
        }
        true
    }

    /// How long the next read may wait: short while a stream plays, so
    /// its ring is refilled in time, and long otherwise.
    fn tick_interval(&self) -> Duration {
        let playing = self
            .streams
            .iter()
            .filter(|stream| !stream.corked)
            .map(|stream| (stream.capacity_duration() / 8).clamp(MIN_TICK, MAX_TICK));
        let recording = self
            .records
            .iter()
            .filter(|record| !record.corked())
            .map(Record::interval);
        playing.chain(recording).min().unwrap_or(IDLE_TICK)
    }

    fn set_default_sink(&mut self, reader: &mut Reader, tag: u32) {
        match reader.string() {
            Some(name) if name == "@NONE@" || is_our_sink_name(name) => {
                self.send_reply(tag, Vec::new())
            }
            Some(_) => self.send_error(tag, wire::ERR_NOENTITY, "no such sink"),
            None => self.send_error(tag, wire::ERR_INVALID, "no sink name"),
        }
    }

    fn tick(&mut self) -> io::Result<()> {
        let remote: Vec<Remote> = self.inbox.try_iter().collect();
        for change in remote {
            self.apply(change);
        }
        let events: Vec<Event> = self.events.try_iter().collect();
        for event in events {
            if self.subscribed & (1 << event.facility) != 0 {
                self.send_packet(wire::subscribe_event(
                    event.facility | event.change,
                    event.index,
                ));
            }
        }
        let mut sink_changed = false;
        if let Some(control) = &mut self.control
            && let Ok(Some(Reply::VolumeChanged { percent })) = control.poll_notice()
        {
            sink_changed = true;
            if percent != 0 && self.registry.sink_muted().is_some_and(|muted| muted != percent) {
                self.registry.set_sink_muted(None);
            }
        }
        if sink_changed && self.subscribed & wire::MASK_SINK != 0 {
            self.send_packet(wire::subscribe_event(wire::EVENT_SINK | wire::EVENT_CHANGE, 0));
        }
        let mut packets = Vec::new();
        let mut dead = Vec::new();
        let mut changed = Vec::new();
        for stream in &mut self.streams {
            let polled = stream.poll();
            packets.extend(polled.packets);
            let latency = stream.latency();
            self.registry
                .set_latency(stream.index, latency.buffer_usec, latency.sink_usec);
            if polled.volume_changed {
                let entry = stream.entry();
                self.registry.update_sink_input(entry);
                changed.push(stream.index);
            }
            if polled.gone {
                dead.push(stream.index);
            }
        }
        for packet in packets {
            self.send_packet(packet);
        }
        for index in changed {
            self.send_packet(wire::subscribe_event(
                wire::EVENT_SINK_INPUT | wire::EVENT_CHANGE,
                index,
            ));
        }
        for index in dead {
            self.streams.retain(|stream| stream.index != index);
            self.registry.remove_sink_input(index);
        }
        self.poll_records();
        Ok(())
    }

    /// Polls the record streams after the playback ones. Each sends only
    /// while what the client has not read yet, here and in the socket, is
    /// below its `maxlength` and a budget well under this connection's
    /// limit, so a client that stops reading overflows its stream rather
    /// than losing its connection.
    fn poll_records(&mut self) {
        let queued = socket_queue(&self.stream);
        let mut dead = Vec::new();
        for record in &mut self.records {
            let polled = record.poll(queued + self.out.len());
            for frame in polled.packets {
                self.out.extend(frame);
            }
            if polled.gone {
                dead.push(record.index);
                continue;
            }
            let latency = record.latency();
            self.registry.set_source_output_latency(
                record.index,
                latency.buffer_usec,
                latency.source_usec,
            );
            // A reopen at another rate may start or stop resampling.
            let entry = record.entry();
            let listed = self.registry.source_output(record.index).map(|mut listed| {
                listed.buffer_usec = 0;
                listed.source_usec = 0;
                listed
            });
            if listed.as_ref() != Some(&entry) {
                self.registry.update_source_output(entry);
            }
        }
        for index in dead {
            if let Some(position) = self.records.iter().position(|r| r.index == index) {
                self.records.remove(position).release();
            }
            self.registry.remove_source_output(index);
            self.send_packet(wire::record_killed(index));
        }
        if self.out.len() > 1 << 20 {
            self.done = true;
        }
    }

    fn record_mut(&mut self, index: u32) -> Option<&mut Record> {
        self.records.iter_mut().find(|record| record.index == index)
    }

    fn create_record(&mut self, reader: &mut Reader, tag: u32) {
        let index = self.registry.next_stream();
        let created = Record::create(
            self.settings,
            &self.daemon_socket,
            self.peer_pid,
            self.client_index,
            index,
            self.registry.default_source(),
            reader,
        );
        match created {
            Ok(created) => {
                self.registry.add_source_output(created.record.entry());
                self.records.push(created.record);
                self.send_reply(tag, created.reply);
            }
            Err(e) => {
                eprintln!("aas: pulse: create record: {}", e.message);
                self.send_error(tag, e.code, e.message)
            }
        }
    }

    /// The record stream a command names, or the error sent for it.
    fn record_index(&mut self, reader: &mut Reader, tag: u32) -> Option<u32> {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return None;
        };
        if self.record_mut(index).is_none() {
            self.send_error(tag, wire::ERR_NOENTITY, "no such record stream");
            return None;
        }
        Some(index)
    }

    fn delete_record(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = self.record_index(reader, tag) else {
            return;
        };
        if let Some(position) = self.records.iter().position(|r| r.index == index) {
            self.records.remove(position).release();
        }
        self.registry.remove_source_output(index);
        self.send_reply(tag, Vec::new());
    }

    fn cork_record(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = self.record_index(reader, tag) else {
            return;
        };
        let Some(corked) = reader.boolean() else {
            self.send_error(tag, wire::ERR_INVALID, "no cork flag");
            return;
        };
        let Some(record) = self.record_mut(index) else {
            return;
        };
        match record.cork(corked) {
            Ok(()) => {
                let entry = record.entry();
                self.registry.update_source_output(entry);
                self.send_reply(tag, Vec::new());
            }
            Err(_) => self.send_error(tag, wire::ERR_NOENTITY, "the stream is gone"),
        }
    }

    fn flush_record(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = self.record_index(reader, tag) else {
            return;
        };
        let flushed = self.record_mut(index).map(Record::flush);
        match flushed {
            Some(Ok(())) => self.send_reply(tag, Vec::new()),
            _ => self.send_error(tag, wire::ERR_NOENTITY, "the stream is gone"),
        }
    }

    fn record_latency(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = self.record_index(reader, tag) else {
            return;
        };
        let Some((sec, usec)) = reader.timeval() else {
            self.send_error(tag, wire::ERR_PROTOCOL, "no request time");
            return;
        };
        let Some(record) = self.record_mut(index) else {
            return;
        };
        let latency = record.latency();
        let (now_sec, now_usec) = now_timeval();
        let mut body = Builder::new();
        body.usec(0);
        body.usec(latency.source_usec);
        body.boolean(latency.playing);
        body.timeval(sec, usec);
        body.timeval(now_sec, now_usec);
        body.i64(latency.write_index);
        body.i64(latency.read_index);
        self.send_reply(tag, body.into_vec());
    }

    fn record_buffer_attr(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = self.record_index(reader, tag) else {
            return;
        };
        let (Some(maxlength), Some(fragsize)) = (reader.u32(), reader.u32()) else {
            self.send_error(tag, wire::ERR_PROTOCOL, "a short buffer attr");
            return;
        };
        let Some(record) = self.record_mut(index) else {
            return;
        };
        let [maxlength, fragsize] = record.set_buffer_attr(maxlength, fragsize);
        let mut body = Builder::new();
        body.u32(maxlength);
        body.u32(fragsize);
        body.usec(record.configured_usec());
        self.send_reply(tag, body.into_vec());
    }

    fn rename_record(&mut self, index: u32, name: String) {
        if let Some(record) = self.record_mut(index) {
            record.name = name;
            let entry = record.entry();
            self.registry.update_source_output(entry);
        }
    }

    fn set_record_name(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = self.record_index(reader, tag) else {
            return;
        };
        let Some(name) = reader.string() else {
            self.send_error(tag, wire::ERR_INVALID, "no name");
            return;
        };
        self.rename_record(index, name.to_string());
        self.send_reply(tag, Vec::new());
    }

    /// Accepts a proplist change. Only `media.name` means anything here:
    /// it is the stream's name.
    fn record_proplist(&mut self, command: u32, reader: &mut Reader, tag: u32) {
        let Some(index) = self.record_index(reader, tag) else {
            return;
        };
        if command == wire::UPDATE_RECORD_STREAM_PROPLIST {
            let _mode = reader.u32();
            if let Some(name) = reader
                .proplist()
                .and_then(|props| props.into_iter().find(|(key, _)| key == "media.name"))
                .map(|(_, value)| value)
            {
                self.rename_record(index, name);
            }
        }
        self.send_reply(tag, Vec::new());
    }

    fn source_output_info(&mut self, reader: &mut Reader, tag: u32) {
        let Some(index) = reader.u32() else {
            self.send_error(tag, wire::ERR_INVALID, "no stream index");
            return;
        };
        match self.registry.source_output(index) {
            Some(output) => self.send_reply(tag, objects::source_output_info(&output)),
            None => self.send_error(tag, wire::ERR_NOENTITY, "no such record stream"),
        }
    }

    fn source_output_info_list(&mut self, tag: u32) {
        let mut body = Vec::new();
        for output in self.registry.source_outputs() {
            body.extend(objects::source_output_info(&output));
        }
        self.send_reply(tag, body);
    }

    fn send_reply(&mut self, tag: u32, body: Vec<u8>) {
        self.send_packet(wire::reply(tag, body));
    }

    fn send_error(&mut self, tag: u32, code: u32, message: &str) {
        eprintln!("aas: pulse: {message}");
        self.send_packet(wire::error(tag, code));
    }

    fn send_packet(&mut self, payload: Vec<u8>) {
        if self.out.len() > 1 << 20 {
            self.done = true;
            return;
        }
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

/// Whether a sink name is the one sink, under its own name or the alias
/// PulseAudio clients use for the default sink.
fn is_our_sink_name(name: &str) -> bool {
    name == "aas" || name == "@DEFAULT_SINK@"
}

/// Whether an index and name pick the one sink. A valid index decides
/// alone; otherwise the name does, and no name means the default sink.
fn is_our_sink(index: Option<u32>, name: Option<&str>) -> bool {
    match index {
        Some(index) if index != wire::INVALID_INDEX => index == 0,
        _ => name.is_none_or(is_our_sink_name),
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

/// Bytes written to the socket that the client has not read yet.
fn socket_queue(stream: &UnixStream) -> usize {
    use std::os::fd::AsRawFd;
    let mut queued: libc::c_int = 0;
    // SAFETY: TIOCOUTQ (SIOCOUTQ) writes one int.
    let rc = unsafe { libc::ioctl(stream.as_raw_fd(), libc::TIOCOUTQ, &mut queued) };
    if rc == 0 { queued.max(0) as usize } else { 0 }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}
