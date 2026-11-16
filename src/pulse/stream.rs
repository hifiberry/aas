//! One PulseAudio playback stream, bridged onto a client stream of the
//! daemon.

use crate::pulse::registry::SinkInput;
use crate::pulse::tagstruct::{Reader, SampleSpec};
use crate::pulse::wire;
use aas_client::client::Client;
use aas_client::convert::ClientFormat;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The largest request this endpoint makes, so a large buffer is filled
/// over several rounds.
const REQUEST_CAP: u32 = 65_536;

/// The smallest request, as a share of the ring: a quarter, so a client
/// is not asked for a few frames at every tick.
const MINREQ_DIVISOR: u32 = 4;

pub struct CreateError {
    pub code: u32,
    pub message: &'static str,
}

impl CreateError {
    pub(crate) fn new(code: u32, message: &'static str) -> Self {
        CreateError { code, message }
    }
}

pub struct Created {
    pub stream: Stream,
    pub reply: Vec<u8>,
}

pub struct Polled {
    pub packets: Vec<Vec<u8>>,
    pub gone: bool,
    pub volume_changed: bool,
}

pub struct Latency {
    /// What plays after the ring: the output's delay.
    pub sink_usec: u64,
    /// What the ring holds.
    pub buffer_usec: u64,
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
    /// The daemon's id of the stream, 0 if it did not say.
    pub daemon_stream: u32,
    pub db_millis: i32,
    /// Set to PulseAudio's silence, volume 0. `aas` has no gain below
    /// -60 dB, so silence is the client's mute bit, while the stream reports
    /// volume 0 and its own mute flag apart.
    pub silent: bool,
    pub mute: bool,
    daemon: Client,
    started: bool,
    write_index: u64,
    underruns: u64,
    /// Bytes requested from the client and not yet received. A request
    /// never asks for more than the ring's free room minus these, so what
    /// the client may write always fits.
    outstanding: u64,
    /// The smallest request, in bytes.
    minreq: u32,
    /// The fill the client asked to keep, in bytes: its `tlength`, at most
    /// the ring. Requests fill the ring up to this, not beyond.
    target: u32,
    /// The daemon stream was started and `STARTED` has not gone out yet.
    announce_started: bool,
    /// Whether a write past the request was logged, so a client that
    /// keeps doing it logs once.
    overrun_logged: bool,
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
        let mut formats = Vec::new();
        for _ in 0..n_formats {
            let format = reader
                .format_info()
                .ok_or_else(|| CreateError::new(wire::ERR_PROTOCOL, "a broken format"))?;
            formats.push(format);
        }
        if !reader.eof() {
            return Err(CreateError::new(wire::ERR_PROTOCOL, "trailing stream data"));
        }
        let (spec, map, format) = client_spec(spec, map, &formats, false)?;
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
        let daemon_stream = daemon.stream_id().unwrap_or(0);
        // An uncorked stream starts with its first write, not now: started
        // empty, it would underrun at once and hold the output's rate
        // before it has anything to play.
        let (db_millis, muted) = daemon.client_volume().unwrap_or((0, muted));
        let capacity = daemon.capacity_frames().unwrap_or(frames as usize) as u32;
        let capacity_bytes = capacity * frame_bytes as u32;
        let name = props
            .iter()
            .find(|(key, _)| key == "media.name")
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| format!("stream {index}"));
        let missing = request_length(capacity_bytes, frame_bytes as u32);
        let minreq = minreq_bytes(capacity, frame_bytes as u32);
        let stream = Stream {
            index,
            client: client_index,
            name,
            rate: spec.rate,
            format: spec.format,
            channels: spec.channels,
            frame_bytes,
            corked,
            daemon_stream,
            db_millis,
            silent: false,
            mute: muted,
            daemon,
            started: false,
            write_index: 0,
            underruns: 0,
            outstanding: u64::from(missing),
            minreq,
            target: capacity_bytes,
            announce_started: false,
            overrun_logged: false,
            drain_tag: None,
        };
        let mut reply = crate::pulse::tagstruct::Builder::new();
        reply.u32(index);
        reply.u32(index);
        reply.u32(missing);
        reply.u32(capacity_bytes);
        reply.u32(capacity_bytes);
        reply.u32(0);
        reply.u32(minreq);
        reply.sample_spec(spec);
        reply.channel_map(&map);
        reply.u32(0);
        reply.string("aas");
        reply.boolean(false);
        reply.usec(u64::from(buffer_ms) * 1000);
        reply.format_info(wire::ENCODING_PCM, &pcm_props(spec, &map));
        Ok(Created {
            stream,
            reply: reply.into_vec(),
        })
    }

    /// The request that should go out now: the room up to the target fill
    /// minus what was asked for already, once that is at least `minreq`.
    pub fn request(&mut self) -> Option<Vec<u8>> {
        if self.corked {
            return None;
        }
        let free = (self.daemon.free_frames().ok()? * self.frame_bytes) as u64;
        let fill = u64::from(self.capacity_bytes()).saturating_sub(free);
        let room = u64::from(self.target).saturating_sub(fill);
        let unasked = room.saturating_sub(self.outstanding);
        if unasked < u64::from(self.minreq) {
            return None;
        }
        let unasked = unasked.min(u64::from(REQUEST_CAP)) as u32;
        let length = request_length(unasked, self.frame_bytes as u32);
        if length == 0 {
            return None;
        }
        self.outstanding += u64::from(length);
        Some(wire::request(self.index, length))
    }

    /// Writes a memblock. A misaligned one is dropped.
    pub fn write(&mut self, data: &[u8]) {
        self.outstanding = self.outstanding.saturating_sub(data.len() as u64);
        if data.len() % self.frame_bytes != 0 {
            return;
        }
        let start = !self.started && !self.corked;
        if let Ok(frames) = self.daemon.write(data) {
            self.write_index += (frames * self.frame_bytes) as u64;
            if frames * self.frame_bytes < data.len() && !self.overrun_logged {
                self.overrun_logged = true;
                eprintln!("aas: pulse: stream {} wrote past its request", self.index);
            }
        }
        // Started once the data is in the ring, so the first period has
        // something to play.
        if start && !data.is_empty() {
            self.begin();
        }
    }

    /// Starts the daemon stream and has `STARTED` sent on the next poll.
    fn begin(&mut self) {
        if self.daemon.start().is_ok() {
            self.started = true;
            self.announce_started = true;
        }
    }

    /// Frames the ring holds.
    fn fill_frames(&self) -> usize {
        let capacity = self.daemon.capacity_frames().unwrap_or(0);
        capacity.saturating_sub(self.daemon.free_frames().unwrap_or(capacity))
    }

    /// Applies `SET_PLAYBACK_STREAM_BUFFER_ATTR` as far as the ring allows
    /// and returns what holds now: maxlength, tlength, prebuf and minreq,
    /// in bytes. The ring is not resized, so a `tlength` above it is cut to
    /// it, as PulseAudio cuts what it cannot give; `u32::MAX` or 0 keep the
    /// current value.
    pub fn set_buffer_attr(&mut self, tlength: u32, minreq: u32) -> [u32; 4] {
        let capacity = self.capacity_bytes();
        let frame = self.frame_bytes as u32;
        let floor = (self.rate / 1000 * 20).max(1) * frame;
        if tlength != 0 && tlength != u32::MAX {
            self.target = (tlength.clamp(floor.min(capacity), capacity) / frame) * frame;
        }
        let quarter = ((self.target / 4) / frame).max(1) * frame;
        self.minreq = if minreq == 0 || minreq == u32::MAX {
            quarter
        } else {
            ((minreq.clamp(frame, self.target / 2) / frame) * frame).max(frame)
        };
        [capacity, self.target, 0, self.minreq]
    }

    /// The fill the client asked to keep, in bytes.
    pub fn target_bytes(&self) -> u32 {
        self.target
    }

    /// The smallest request, in bytes, as the buffer attributes report it.
    pub fn minreq_bytes(&self) -> u32 {
        self.minreq
    }

    /// How long the ring plays when full, so the connection checks for room
    /// often enough.
    pub fn capacity_duration(&self) -> std::time::Duration {
        let frames = self.daemon.capacity_frames().unwrap_or(0) as u64;
        std::time::Duration::from_micros(frames * 1_000_000 / u64::from(self.rate.max(1)))
    }

    pub fn cork(&mut self, corked: bool) -> io::Result<()> {
        if corked == self.corked {
            return Ok(());
        }
        if corked {
            self.daemon.stop()?;
            self.started = false;
            self.corked = true;
        } else {
            self.corked = false;
            // With data in the ring it plays on now; empty, it starts with
            // its next write, like a new stream.
            if self.fill_frames() > 0 {
                self.begin();
                if !self.started {
                    return Err(io::Error::other("the daemon could not start"));
                }
            }
        }
        Ok(())
    }

    /// Empties the ring. The stream starts again with its next write.
    pub fn flush(&mut self) -> io::Result<()> {
        self.daemon.reset()?;
        self.started = false;
        Ok(())
    }

    /// Starts the stream now, even with nothing written.
    pub fn trigger(&mut self) -> io::Result<()> {
        if !self.started && !self.corked {
            self.begin();
            if !self.started {
                return Err(io::Error::other("the daemon could not start"));
            }
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
            .map(|frames| frames * self.frame_bytes as u64)
            .unwrap_or(0);
        // The client adds what the ring holds (write minus read index), so
        // the sink's part is only what plays after the ring.
        let after = self
            .daemon
            .delay_frames()
            .saturating_sub(self.fill_frames() as u64);
        Latency {
            sink_usec: after * 1_000_000 / u64::from(self.rate.max(1)),
            buffer_usec: self.fill_frames() as u64 * 1_000_000 / u64::from(self.rate.max(1)),
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

    pub fn set_volume(&mut self, db_millis: i32, silent: bool, muted: bool) -> io::Result<()> {
        self.daemon.set_client_volume(db_millis, muted || silent)?;
        self.db_millis = db_millis;
        self.silent = silent;
        self.mute = muted;
        Ok(())
    }

    pub fn set_mute(&mut self, muted: bool) -> io::Result<()> {
        self.set_volume(self.db_millis, self.silent, muted)
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
            stream: self.daemon_stream,
            db_millis: self.db_millis,
            silent: self.silent,
            mute: self.mute,
            buffer_usec: 0,
            sink_usec: 0,
        }
    }

    /// Polls the stream: a finished drain, an underrun, a lost daemon or
    /// room for more.
    pub fn poll(&mut self) -> Polled {
        let mut packets = Vec::new();
        let mut volume_changed = false;
        loop {
            match self.daemon.poll_notice() {
                Ok(Some(aas_client::proto::Reply::ClientVolumeChanged { db_millis, muted })) => {
                    // The echo of what this endpoint set changes nothing;
                    // a change from elsewhere (the API, another stream of
                    // the same process) replaces silence too.
                    if (db_millis, muted) != (self.db_millis, self.mute || self.silent) {
                        self.db_millis = db_millis;
                        self.silent = false;
                        self.mute = muted;
                    }
                    volume_changed = true;
                }
                Ok(Some(_)) | Ok(None) => break,
                Err(_) => return Polled {
                    packets,
                    gone: true,
                    volume_changed,
                },
            }
        }
        if let Some(tag) = self.drain_tag {
            match self.daemon.poll_drain() {
                Ok(true) => {
                    packets.push(wire::reply(tag, Vec::new()));
                    self.drain_tag = None;
                    self.started = false;
                }
                Ok(false) => {}
                Err(_) => {
                    packets.push(wire::error(tag, wire::ERR_NOENTITY));
                    self.drain_tag = None;
                    return Polled {
                        packets,
                        gone: true,
                        volume_changed,
                    };
                }
            }
        }
        if std::mem::take(&mut self.announce_started) {
            packets.push(wire::started(self.index));
        }
        let underruns = self.daemon.underruns();
        if underruns > self.underruns {
            self.underruns = underruns;
            packets.push(wire::underflow(self.index, self.latency().read_index));
        }
        if self.daemon.check_alive().is_err() {
            packets.push(wire::killed(self.index));
            if let Some(tag) = self.drain_tag.take() {
                packets.push(wire::error(tag, wire::ERR_NOENTITY));
            }
            return Polled {
                packets,
                gone: true,
                volume_changed,
            };
        }
        if let Some(request) = self.request() {
            packets.push(request);
        }
        Polled {
            packets,
            gone: false,
            volume_changed,
        }
    }

    pub fn release(&mut self) {
        self.daemon.release();
    }
}

/// Checks the sample spec and channel map a stream is created with, or the
/// first PCM entry of its format list, and returns what is used and the
/// format to convert to or from. With `peak`, a rate from 1 Hz is accepted.
pub(crate) fn client_spec(
    mut spec: SampleSpec,
    mut map: Vec<u8>,
    formats: &[(u8, Vec<(String, String)>)],
    peak: bool,
) -> Result<(SampleSpec, Vec<u8>, ClientFormat), CreateError> {
    // A stream made with pa_stream_new_extended, as mpv and GStreamer
    // make theirs, sends a placeholder sample spec and its real format
    // as a list. The first PCM entry is used; encoded ones are not.
    if !formats.is_empty() {
        let pcm = formats
            .iter()
            .find(|(encoding, _)| *encoding == wire::ENCODING_PCM)
            .ok_or_else(|| {
                CreateError::new(wire::ERR_NOTSUPPORTED, "encoded formats are not supported")
            })?;
        (spec, map) = pcm_spec(&pcm.1).ok_or_else(|| {
            CreateError::new(wire::ERR_NOTSUPPORTED, "that PCM format is not supported")
        })?;
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
    let lowest = if peak { 1 } else { 8_000 };
    if !(lowest..=384_000).contains(&spec.rate) {
        return Err(CreateError::new(wire::ERR_INVALID, "that rate is out of range"));
    }
    if map.len() != usize::from(spec.channels) {
        return Err(CreateError::new(wire::ERR_INVALID, "the channel map is wrong"));
    }
    Ok((spec, map, format))
}

/// The sample spec and channel map of a PCM `format_info`. PulseAudio
/// stores each property as JSON: a quoted string or a number.
fn pcm_spec(props: &[(String, String)]) -> Option<(SampleSpec, Vec<u8>)> {
    let get = |key: &str| {
        props
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim().trim_matches('"').to_string())
    };
    let format = match get("format.sample_format")?.as_str() {
        "s16le" => wire::FORMAT_S16LE,
        "float32le" => wire::FORMAT_FLOAT32LE,
        "s32le" => wire::FORMAT_S32LE,
        "s24le" => wire::FORMAT_S24LE,
        "s24-32le" => wire::FORMAT_S24_32LE,
        _ => return None,
    };
    let rate: u32 = get("format.rate")?.parse().ok()?;
    let channels: u8 = get("format.channels")?.parse().ok()?;
    let map = match get("format.channel_map") {
        Some(names) => names
            .split(',')
            .map(|name| match name.trim() {
                "mono" => Some(0),
                "front-left" | "left" => Some(1),
                "front-right" | "right" => Some(2),
                _ => None,
            })
            .collect::<Option<Vec<u8>>>()?,
        None if channels == 1 => vec![0],
        None => vec![1, 2],
    };
    Some((
        SampleSpec {
            format,
            channels,
            rate,
        },
        map,
    ))
}

/// The properties of a PCM `format_info` for `spec`, as PulseAudio writes
/// them.
pub fn pcm_props(spec: SampleSpec, map: &[u8]) -> Vec<(String, String)> {
    let format = match spec.format {
        wire::FORMAT_S16LE => "s16le",
        wire::FORMAT_FLOAT32LE => "float32le",
        wire::FORMAT_S32LE => "s32le",
        wire::FORMAT_S24LE => "s24le",
        wire::FORMAT_S24_32LE => "s24-32le",
        _ => "invalid",
    };
    let names: Vec<&str> = map
        .iter()
        .map(|&position| match position {
            0 => "mono",
            1 => "front-left",
            _ => "front-right",
        })
        .collect();
    vec![
        ("format.sample_format".into(), format!("\"{format}\"")),
        ("format.rate".into(), spec.rate.to_string()),
        ("format.channels".into(), spec.channels.to_string()),
        ("format.channel_map".into(), format!("\"{}\"", names.join(","))),
    ]
}

fn minreq_bytes(capacity_frames: u32, frame_bytes: u32) -> u32 {
    (capacity_frames / MINREQ_DIVISOR).max(1) * frame_bytes
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
