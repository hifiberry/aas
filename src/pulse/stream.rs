//! One PulseAudio playback stream, bridged onto a client stream of the
//! daemon.

use crate::pulse::registry::SinkInput;
use crate::pulse::tagstruct::Reader;
use crate::pulse::wire;
use aas_client::client::Client;
use aas_client::convert::ClientFormat;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The largest request this endpoint makes, so a large buffer is filled
/// over several rounds and a small one is not overrun.
const REQUEST_CAP: u32 = 65_536;

pub struct CreateError {
    pub code: u32,
    pub message: &'static str,
}

impl CreateError {
    fn new(code: u32, message: &'static str) -> Self {
        CreateError { code, message }
    }
}

pub struct Created {
    pub stream: Stream,
    pub reply: Vec<u8>,
}

pub enum Polled {
    Packets(Vec<Vec<u8>>),
    Gone,
}

pub struct Latency {
    pub sink_usec: u64,
    pub playing: bool,
    pub write_index: i64,
    pub read_index: i64,
    pub underrun_for: u64,
    pub playing_for: u64,
}

pub struct Stream {
    pub index: u32,
    pub client: u32,
    pub name: String,
    pub rate: u32,
    pub format: u8,
    pub channels: u8,
    pub frame_bytes: usize,
    pub corked: bool,
    pub volume: u8,
    pub mute: bool,
    daemon: Client,
    started: bool,
    write_index: u64,
    read_base: u64,
    underruns: u64,
    pending: Option<u32>,
    drain_tag: Option<u32>,
}

impl Stream {
    /// Parses `CREATE_PLAYBACK_STREAM` and sets up the daemon stream.
    pub fn create(
        daemon_socket: &Path,
        peer_pid: Option<u32>,
        buffer_ms: u32,
        client_index: u32,
        index: u32,
        reader: &mut Reader,
    ) -> Result<Created, CreateError> {
        let spec = reader
            .sample_spec()
            .ok_or_else(|| CreateError::new(wire::ERR_INVALID, "no sample spec"))?;
        let map = reader
            .channel_map()
            .ok_or_else(|| CreateError::new(wire::ERR_INVALID, "no channel map"))?;
        let _sink_index = reader
            .u32()
            .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "no sink index"))?;
        let _sink_name = reader.string();
        let _maxlength = reader
            .u32()
            .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "no maxlength"))?;
        let corked = reader
            .boolean()
            .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "no corked flag"))?;
        let tlength = reader
            .u32()
            .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "no tlength"))?;
        for _ in 0..2 {
            reader
                .u32()
                .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "no buffer attr"))?;
        }
        let _syncid = reader
            .u32()
            .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "no syncid"))?;
        reader
            .cvolume()
            .ok_or_else(|| CreateError::new(wire::ERR_INVALID, "no volume"))?;
        for _ in 0..7 {
            reader
                .boolean()
                .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "no stream flag"))?;
        }
        let muted = reader
            .boolean()
            .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "no muted flag"))?;
        let _adjust_latency = reader
            .boolean()
            .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "no adjust flag"))?;
        let props = reader
            .proplist()
            .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "no proplist"))?;
        for _ in 0..7 {
            reader
                .boolean()
                .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "no stream flag"))?;
        }
        let n_formats = reader
            .u8()
            .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "no format count"))?;
        for _ in 0..n_formats {
            reader
                .format_info()
                .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "a broken format"))?;
        }
        if !reader.eof() {
            return Err(CreateError::new(wire::ERR_PROTOCOL, "trailing stream data"));
        }
        if n_formats != 0 {
            return Err(CreateError::new(
                wire::ERR_NOTSUPPORTED,
                "encoded formats are not supported",
            ));
        }
        let format = match spec.format {
            wire::FORMAT_S16LE => ClientFormat::S16,
            wire::FORMAT_FLOAT32LE => ClientFormat::Float,
            wire::FORMAT_S32LE => ClientFormat::S32,
            wire::FORMAT_S24LE => ClientFormat::S24Packed,
            wire::FORMAT_S24_32LE => ClientFormat::S24In32,
            _ => {
                return Err(CreateError::new(
                    wire::ERR_NOTSUPPORTED,
                    "that sample format is not supported",
                ));
            }
        };
        if !(1..=2).contains(&spec.channels) {
            return Err(CreateError::new(
                wire::ERR_NOTSUPPORTED,
                "more than two channels",
            ));
        }
        if !(8_000..=384_000).contains(&spec.rate) {
            return Err(CreateError::new(wire::ERR_INVALID, "that rate is out of range"));
        }
        if map.len() != usize::from(spec.channels) {
            return Err(CreateError::new(wire::ERR_INVALID, "the channel map is wrong"));
        }
        let frame_bytes = format.sample_bytes() * usize::from(spec.channels);
        let min_frames = (spec.rate / 1000 * 20).max(1);
        let cap_frames = (spec.rate / 1000 * buffer_ms).max(min_frames);
        let wanted = if tlength == 0 {
            cap_frames
        } else {
            tlength / frame_bytes as u32
        };
        let frames = wanted.clamp(min_frames, cap_frames).min(1 << 20);

        let mut daemon = Client::connect(daemon_socket)
            .map_err(|_| CreateError::new(wire::ERR_INTERNAL, "the daemon is not there"))?;
        if let Some(pid) = peer_pid {
            let _ = daemon.identify(pid);
        }
        daemon
            .configure(spec.rate, format, spec.channels, frames)
            .map_err(|_| CreateError::new(wire::ERR_INVALID, "the daemon refused the stream"))?;
        if !corked {
            daemon
                .start()
                .map_err(|_| CreateError::new(wire::ERR_INTERNAL, "the daemon could not start"))?;
        }
        let capacity = daemon.capacity_frames().unwrap_or(frames as usize) as u32;
        let capacity_bytes = capacity * frame_bytes as u32;
        let name = props
            .iter()
            .find(|(key, _)| key == "media.name")
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| format!("stream {index}"));
        let missing = request_length(capacity_bytes, frame_bytes as u32);
        let stream = Stream {
            index,
            client: client_index,
            name,
            rate: spec.rate,
            format: spec.format,
            channels: spec.channels,
            frame_bytes,
            corked,
            volume: 100,
            mute: muted,
            daemon,
            started: !corked,
            write_index: 0,
            read_base: 0,
            underruns: 0,
            pending: None,
            drain_tag: None,
        };
        let mut reply = crate::pulse::tagstruct::Builder::new();
        reply.u32(index);
        reply.u32(index);
        reply.u32(missing);
        reply.u32(capacity_bytes);
        reply.u32(capacity_bytes);
        reply.u32(0);
        reply.u32(0);
        reply.sample_spec(spec);
        reply.channel_map(&map);
        reply.u32(0);
        reply.string("aas");
        reply.boolean(false);
        reply.usec(u64::from(buffer_ms) * 1000);
        reply.format_info(0, &[]);
        Ok(Created {
            stream,
            reply: reply.into_vec(),
        })
    }

    /// The request that should go out now, if the ring has room.
    pub fn request(&mut self) -> Option<Vec<u8>> {
        if self.pending.is_some() || self.corked {
            return None;
        }
        let room = self.daemon.free_frames().ok()? * self.frame_bytes;
        let length = request_length(room as u32, self.frame_bytes as u32);
        if length == 0 {
            return None;
        }
        self.pending = Some(length);
        Some(wire::request(self.index, length))
    }

    /// Writes a memblock. A misaligned one is dropped.
    pub fn write(&mut self, data: &[u8]) {
        if data.len() % self.frame_bytes != 0 {
            return;
        }
        if let Ok(frames) = self.daemon.write(data) {
            self.write_index += (frames * self.frame_bytes) as u64;
        }
        self.pending = None;
    }

    pub fn cork(&mut self, corked: bool) -> io::Result<()> {
        if corked == self.corked {
            return Ok(());
        }
        if corked {
            self.daemon.stop()?;
            self.started = false;
        } else {
            self.daemon.start()?;
            self.started = true;
        }
        self.corked = corked;
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.daemon.reset()?;
        self.read_base = self.write_index;
        if !self.corked {
            self.daemon.start()?;
            self.started = true;
        }
        Ok(())
    }

    pub fn trigger(&mut self) -> io::Result<()> {
        if !self.started && !self.corked {
            self.daemon.start()?;
            self.started = true;
        }
        Ok(())
    }

    pub fn drain(&mut self, tag: u32) -> io::Result<()> {
        self.daemon.start_drain()?;
        self.drain_tag = Some(tag);
        Ok(())
    }

    pub fn latency(&self) -> Latency {
        let read = self
            .daemon
            .hw_position()
            .map(|frames| self.read_base + frames * self.frame_bytes as u64)
            .unwrap_or(self.read_base);
        let frames = self.daemon.delay_frames();
        Latency {
            sink_usec: frames * 1_000_000 / u64::from(self.rate.max(1)),
            playing: self.started && !self.corked,
            write_index: self.write_index as i64,
            read_index: read as i64,
            underrun_for: self.daemon.underruns(),
            playing_for: read * 1_000_000
                / (u64::from(self.rate.max(1)) * self.frame_bytes as u64).max(1),
        }
    }

    pub fn capacity_bytes(&self) -> u32 {
        self.daemon
            .capacity_frames()
            .unwrap_or(0)
            .saturating_mul(self.frame_bytes) as u32
    }

    pub fn entry(&self) -> SinkInput {
        SinkInput {
            index: self.index,
            client: self.client,
            name: self.name.clone(),
            rate: self.rate,
            channels: self.channels,
            format: self.format,
            corked: self.corked,
            volume: self.volume,
            mute: self.mute,
        }
    }

    /// Polls the stream: a finished drain, an underrun, a lost daemon or
    /// room for more.
    pub fn poll(&mut self) -> Polled {
        let mut packets = Vec::new();
        if let Some(tag) = self.drain_tag {
            match self.daemon.poll_drain() {
                Ok(true) => {
                    packets.push(wire::reply(tag, Vec::new()));
                    self.drain_tag = None;
                }
                Ok(false) => {}
                Err(_) => return Polled::Gone,
            }
        }
        let underruns = self.daemon.underruns();
        if underruns > self.underruns {
            self.underruns = underruns;
            packets.push(wire::underflow(self.index));
        }
        if self.daemon.check_alive().is_err() {
            return Polled::Gone;
        }
        if let Some(request) = self.request() {
            packets.push(request);
        }
        Polled::Packets(packets)
    }

    pub fn release(&mut self) {
        self.daemon.release();
    }
}

fn request_length(room: u32, frame_bytes: u32) -> u32 {
    (room.min(REQUEST_CAP) / frame_bytes.max(1)) * frame_bytes.max(1)
}

/// The timeval of the wall clock, as PulseAudio carries it.
pub fn now_timeval() -> (u32, u32) {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (since.as_secs() as u32, since.subsec_micros())
}
