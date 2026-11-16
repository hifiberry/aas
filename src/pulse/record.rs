//! One PulseAudio record stream, fed from a capture or a monitor stream of
//! the daemon.
//!
//! The daemon delivers stereo S32 at its own rate. The stream resamples
//! that to the client's rate where they differ, converts it to the
//! client's format and sends it in fragments of `fragsize` bytes. It all
//! runs on the connection's thread.

use crate::pulse::Settings;
use crate::pulse::conn::{MAX_TICK, MIN_TICK};
use crate::pulse::objects::{self, Source};
use crate::pulse::packet::Encoder;
use crate::pulse::registry::{Registry, SourceOutput};
use crate::pulse::stream::{CreateError, client_spec, pcm_props};
use crate::pulse::tagstruct::{Builder, Reader, SampleSpec};
use crate::pulse::wire;
use crate::resample::Resampler;
use aas_client::client::Client;
use aas_client::convert::{ClientFormat, from_stereo_s32};
use aas_client::ring::Ring;
use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The fragment a client gets when it names none.
const DEFAULT_FRAGMENT_MS: u64 = 25;
/// The largest fragment.
const MAX_FRAGMENT: u32 = 65_536;
/// What waits to be sent when a client names no limit.
const DEFAULT_MAXLENGTH_MS: u64 = 2_000;
/// The most that ever waits to be sent, PulseAudio's own cap on a queue.
const MAX_LENGTH: u32 = 4 << 20;
/// The most a stream lets wait for the client on its way, in the
/// connection's queue and the socket, whatever its maxlength and however
/// large the socket's buffer. Well below the 1 MiB at which a connection
/// is closed, so a stalled client overflows its stream instead of losing
/// its connection.
pub const SEND_BUDGET: usize = 256 << 10;
/// Frames read from the daemon's ring at a time.
const CHUNK: usize = 1024;
/// Output frames of one resampler call.
const PERIOD: u32 = 256;

pub struct CreatedRecord {
    pub record: Record,
    pub reply: Vec<u8>,
}

pub struct RecordPolled {
    /// Encoded frames, ready for the socket: memblocks and packets.
    pub packets: Vec<Vec<u8>>,
    pub gone: bool,
}

pub struct RecordLatency {
    /// What the daemon's ring and the resampler hold.
    pub source_usec: u64,
    /// What waits to be sent.
    pub buffer_usec: u64,
    pub playing: bool,
    pub write_index: i64,
    pub read_index: i64,
}

/// The resampler, and the ring it reads from, which the daemon's frames
/// are copied into.
struct Resampling {
    resampler: Resampler,
    bounce: Ring,
}

/// The peak of each interval of daemon frames, for a client's rate that
/// may be far below the daemon's. No filter: each output frame is the
/// largest magnitude per channel of the frames its interval covers, the
/// intervals counted by the rate ratio so any ratio keeps real time.
struct Peaks {
    daemon_rate: u32,
    client_rate: u32,
    acc: u64,
    peak: [i32; 2],
}

impl Peaks {
    fn new(daemon_rate: u32, client_rate: u32) -> Peaks {
        Peaks {
            daemon_rate: daemon_rate.max(1),
            client_rate,
            acc: 0,
            peak: [0; 2],
        }
    }

    /// Feeds `frames` and appends the frames that completes to `out`:
    /// `[left, right]`, or the louder of the two twice for mono.
    fn push(&mut self, frames: &[[i32; 2]], channels: u8, out: &mut Vec<[i32; 2]>) {
        let magnitude = |v: i32| v.unsigned_abs().min(i32::MAX as u32) as i32;
        let daemon = u64::from(self.daemon_rate);
        for f in frames {
            self.peak[0] = self.peak[0].max(magnitude(f[0]));
            self.peak[1] = self.peak[1].max(magnitude(f[1]));
            self.acc += u64::from(self.client_rate);
            if self.acc >= daemon {
                let frame = if channels == 1 {
                    let m = self.peak[0].max(self.peak[1]);
                    [m, m]
                } else {
                    self.peak
                };
                while self.acc >= daemon {
                    out.push(frame);
                    self.acc -= daemon;
                }
                self.peak = [0; 2];
            }
        }
    }

    fn reset(&mut self) {
        self.acc = 0;
        self.peak = [0; 2];
    }
}

pub struct Record {
    pub index: u32,
    pub client: u32,
    pub name: String,
    /// The `application.*` entries of the client's proplist.
    pub application: Vec<(String, String)>,
    source: Source,
    spec: SampleSpec,
    map: Vec<u8>,
    format: ClientFormat,
    frame_bytes: u32,
    corked: bool,
    settings: Settings,
    daemon: Client,
    daemon_rate: u32,
    resampling: Option<Resampling>,
    /// Set for a peak-detect stream, which has no resampler.
    peaks: Option<Peaks>,
    peak_detect: bool,
    /// The sink input a stream of one input records, and where to look it up.
    direct: Option<(u32, Arc<Registry>)>,
    peak_frames: Vec<[i32; 2]>,
    frames: Vec<[i32; 2]>,
    period: Vec<[i32; 2]>,
    converted: Vec<u8>,
    pending: Pending,
    fragsize: u32,
    /// The daemon ring's count of frames it dropped, as last seen.
    underruns: u64,
}

impl Record {
    /// Parses `CREATE_RECORD_STREAM` and sets up the daemon stream.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        settings: Settings,
        daemon_socket: &Path,
        peer_pid: Option<u32>,
        client_index: u32,
        index: u32,
        default: Source,
        registry: &Arc<Registry>,
        reader: &mut Reader,
    ) -> Result<CreatedRecord, CreateError> {
        let protocol = |what| CreateError::new(wire::ERR_PROTOCOL, what);
        let spec = reader
            .sample_spec()
            .ok_or_else(|| CreateError::new(wire::ERR_INVALID, "no sample spec"))?;
        let map = reader
            .channel_map()
            .ok_or_else(|| CreateError::new(wire::ERR_INVALID, "no channel map"))?;
        let source_index = reader.u32().ok_or_else(|| protocol("no source index"))?;
        let source_name = reader.string();
        let maxlength = reader.u32().ok_or_else(|| protocol("no maxlength"))?;
        let corked = reader.boolean().ok_or_else(|| protocol("no corked flag"))?;
        let fragsize = reader.u32().ok_or_else(|| protocol("no fragsize"))?;
        // From no_remap to variable_rate, then peak_detect and
        // adjust_latency.
        let mut flags = [false; 9];
        for flag in &mut flags {
            *flag = reader.boolean().ok_or_else(|| protocol("no stream flag"))?;
        }
        let peak_detect = flags[7];
        let props = reader.proplist().ok_or_else(|| protocol("no proplist"))?;
        let direct_on_input = reader.u32().ok_or_else(|| protocol("no direct_on_input"))?;
        // early_requests, dont_inhibit_auto_suspend, fail_on_suspend.
        for _ in 0..3 {
            reader.boolean().ok_or_else(|| protocol("no stream flag"))?;
        }
        let n_formats = reader.u8().ok_or_else(|| protocol("no format count"))?;
        let mut formats = Vec::new();
        for _ in 0..n_formats {
            formats.push(
                reader
                    .format_info()
                    .ok_or_else(|| protocol("a broken format"))?,
            );
        }
        reader.cvolume().ok_or_else(|| protocol("no volume"))?;
        // muted, volume_set, muted_set, relative_volume, passthrough.
        for _ in 0..5 {
            reader.boolean().ok_or_else(|| protocol("no stream flag"))?;
        }
        if !reader.eof() {
            return Err(protocol("trailing stream data"));
        }
        let (spec, map, format) = client_spec(spec, map, &formats, peak_detect)?;
        let source =
            objects::find_source(Some(source_index), source_name, default, settings.analog)
                .ok_or_else(|| CreateError::new(wire::ERR_NOENTITY, "no such source"))?;
        let direct = (direct_on_input != wire::INVALID_INDEX).then_some(direct_on_input);
        if direct.is_some() {
            if source == Source::Analog {
                return Err(CreateError::new(
                    wire::ERR_INVALID,
                    "the analog input has no sink inputs",
                ));
            }
            if registry.sink_input(direct_on_input).is_none() {
                return Err(CreateError::new(wire::ERR_NOENTITY, "no such sink input"));
            }
        }
        let frame_bytes = format.sample_bytes() as u32 * u32::from(spec.channels);
        let fragsize = fragment_bytes(fragsize, spec.rate, frame_bytes);
        let maxlength = max_length(maxlength, spec.rate, frame_bytes, fragsize);
        let daemon = Client::connect(daemon_socket)
            .map_err(|_| CreateError::new(wire::ERR_INTERNAL, "the daemon is not there"))?;
        let name = props
            .iter()
            .find(|(key, _)| key == "media.name")
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| format!("record stream {index}"));
        let mut record = Record {
            index,
            client: client_index,
            name,
            application: Vec::new(),
            source,
            spec,
            map,
            format,
            frame_bytes,
            corked,
            settings,
            daemon,
            daemon_rate: 0,
            resampling: None,
            peaks: None,
            peak_detect,
            direct: direct.map(|index| (index, registry.clone())),
            peak_frames: Vec::new(),
            frames: vec![[0; 2]; CHUNK],
            period: vec![[0; 2]; PERIOD as usize],
            converted: Vec::new(),
            pending: Pending::new(maxlength),
            fragsize,
            underruns: 0,
        };
        if let Some(pid) = peer_pid {
            let _ = record.daemon.identify(pid);
        }
        record.open().map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                CreateError::new(wire::ERR_NOENTITY, "no such sink input")
            } else {
                CreateError::new(wire::ERR_INVALID, "the daemon refused the stream")
            }
        })?;
        let mut reply = Builder::new();
        reply.u32(index);
        reply.u32(index);
        reply.u32(maxlength);
        reply.u32(fragsize);
        reply.sample_spec(spec);
        reply.channel_map(&record.map);
        reply.u32(source.index());
        reply.string(source.name());
        reply.boolean(false);
        reply.usec(record.configured_usec());
        reply.format_info(wire::ENCODING_PCM, &pcm_props(spec, &record.map));
        Ok(CreatedRecord {
            record,
            reply: reply.into_vec(),
        })
    }

    /// Sets up the daemon stream for the source, with a ring of
    /// `buffer_ms` at the daemon's rate and a resampler if that is not
    /// the client's, and starts it unless corked.
    fn open(&mut self) -> io::Result<()> {
        let guess = self
            .daemon
            .output_rate()
            .ok()
            .filter(|&rate| rate != 0)
            .unwrap_or(48_000);
        let frames = (u64::from(guess) * u64::from(self.settings.buffer_ms) / 1000).max(1) as u32;
        let rate = match self.source {
            Source::Monitor => match &self.direct {
                Some((index, registry)) => {
                    let id = registry.sink_input(*index).map(|input| input.stream);
                    let id = monitored_stream(id)?;
                    self.daemon.configure_stream_monitor(frames, id)?
                }
                None => self.daemon.configure_monitor(frames)?,
            },
            Source::Analog => self.daemon.configure_capture(frames)?,
        };
        self.daemon_rate = rate;
        self.underruns = 0;
        self.peaks = self
            .peak_detect
            .then(|| Peaks::new(rate, self.spec.rate));
        self.resampling = if self.peak_detect || rate == self.spec.rate {
            None
        } else {
            let resampler = Resampler::new(
                rate,
                self.spec.rate,
                PERIOD,
                self.settings.quality,
                self.settings.format,
            );
            let capacity = (CHUNK + resampler.input_frames_max()) as u32;
            let (bounce, _fd) = Ring::create(capacity)?;
            Some(Resampling { resampler, bounce })
        };
        if !self.corked {
            self.daemon.start()?;
        }
        Ok(())
    }

    /// Moves what the daemon's ring holds into `pending`, converted.
    /// Returns whether `pending` had to drop some.
    fn pull(&mut self) -> io::Result<bool> {
        self.converted.clear();
        loop {
            let room = match &self.resampling {
                Some(r) => r.bounce.free().min(CHUNK),
                None => CHUNK,
            };
            let n = self.daemon.read(&mut self.frames[..room])?;
            if n == 0 {
                break;
            }
            if let Some(p) = &mut self.peaks {
                self.peak_frames.clear();
                p.push(&self.frames[..n], self.spec.channels, &mut self.peak_frames);
                from_stereo_s32(
                    &self.peak_frames,
                    self.format,
                    self.spec.channels,
                    &mut self.converted,
                );
                continue;
            }
            match &mut self.resampling {
                None => from_stereo_s32(
                    &self.frames[..n],
                    self.format,
                    self.spec.channels,
                    &mut self.converted,
                ),
                Some(r) => {
                    r.bounce.write(&self.frames[..n]);
                    loop {
                        let made = r.resampler.process(&r.bounce, &mut self.period);
                        if made == 0 {
                            break;
                        }
                        from_stereo_s32(
                            &self.period[..made],
                            self.format,
                            self.spec.channels,
                            &mut self.converted,
                        );
                    }
                }
            }
        }
        Ok(self.pending.push(&self.converted))
    }

    /// Polls the stream: reads what the daemon recorded, reopens it after
    /// a rate change, and returns the fragments ready to go, as long as
    /// what `backlog` says still waits for the client is below
    /// `send_limit` of its maxlength and the connection's `budget`. An
    /// overrun sends one `OVERFLOW`, and the next only after a fragment
    /// has gone out again, so a client that stays stalled gets one.
    pub fn poll(&mut self, backlog: usize, budget: usize) -> RecordPolled {
        let gone = RecordPolled {
            packets: Vec::new(),
            gone: true,
        };
        if self.daemon.check_alive().is_err() {
            return gone;
        }
        let mut packets = Vec::new();
        if self.corked {
            return RecordPolled {
                packets,
                gone: false,
            };
        }
        let ended = self.daemon.ended();
        let Ok(mut overrun) = self.pull() else {
            return gone;
        };
        let underruns = self.daemon.underruns();
        if underruns > self.underruns {
            overrun = true;
        }
        self.underruns = underruns;
        if ended && self.open().is_err() {
            return gone;
        }
        if overrun && self.pending.overflowed() {
            packets.push(Encoder.packet(wire::INVALID_INDEX, &wire::overflow(self.index)));
        }
        let mut sent = backlog;
        while sent < send_limit(self.pending.maxlength, budget) {
            let Some(chunk) = self.pending.take(self.fragsize as usize) else {
                break;
            };
            sent += chunk.len();
            packets.push(Encoder.memblock(self.index, &chunk));
        }
        RecordPolled {
            packets,
            gone: false,
        }
    }

    pub fn corked(&self) -> bool {
        self.corked
    }

    /// Corked, the daemon stream is stopped and nothing is sent.
    pub fn cork(&mut self, corked: bool) -> io::Result<()> {
        if corked == self.corked {
            return Ok(());
        }
        self.corked = corked;
        if self.daemon.ended() {
            // Reopened at the new rate, and started unless corked.
            return self.open();
        }
        if corked {
            self.daemon.stop()
        } else {
            self.daemon.start()
        }
    }

    /// Drops what was recorded and not sent, and records on.
    pub fn flush(&mut self) -> io::Result<()> {
        self.pending.clear();
        if self.daemon.ended() {
            return self.open();
        }
        self.daemon.reset()?;
        if let Some(p) = &mut self.peaks {
            p.reset();
        }
        if let Some(r) = &mut self.resampling {
            r.bounce.consume(usize::MAX);
            r.resampler.reset();
        }
        if !self.corked {
            self.daemon.start()?;
        }
        Ok(())
    }

    /// Applies `SET_RECORD_STREAM_BUFFER_ATTR` as `CREATE_RECORD_STREAM`
    /// does and returns maxlength and fragsize.
    pub fn set_buffer_attr(&mut self, maxlength: u32, fragsize: u32) -> [u32; 2] {
        self.fragsize = fragment_bytes(fragsize, self.spec.rate, self.frame_bytes);
        self.pending.maxlength =
            max_length(maxlength, self.spec.rate, self.frame_bytes, self.fragsize);
        [self.pending.maxlength, self.fragsize]
    }

    /// The latency the buffer attributes give: one fragment.
    pub fn configured_usec(&self) -> u64 {
        self.bytes_usec(u64::from(self.fragsize))
    }

    fn bytes_usec(&self, bytes: u64) -> u64 {
        bytes * 1_000_000 / (u64::from(self.spec.rate) * u64::from(self.frame_bytes)).max(1)
    }

    pub fn latency(&self) -> RecordLatency {
        let ring = self.daemon.delay_frames() * 1_000_000 / u64::from(self.daemon_rate.max(1));
        let filter = self.resampling.as_ref().map_or(0, |r| {
            u64::from(r.resampler.delay_frames()) * 1_000_000 / u64::from(self.spec.rate)
        });
        RecordLatency {
            source_usec: ring + filter,
            buffer_usec: self.bytes_usec(self.pending.bytes.len() as u64),
            playing: !self.corked,
            write_index: self.pending.write_index as i64,
            read_index: self.pending.read_index as i64,
        }
    }

    pub fn entry(&self) -> SourceOutput {
        SourceOutput {
            index: self.index,
            client: self.client,
            name: self.name.clone(),
            source: self.source,
            rate: self.spec.rate,
            channels: self.spec.channels,
            format: self.spec.format,
            corked: self.corked,
            application: self.application.clone(),
            resampled: self.resampling.is_some(),
            buffer_usec: 0,
            source_usec: 0,
        }
    }

    /// How often the stream should be polled: twice a fragment, within
    /// the connection's bounds.
    pub fn interval(&self) -> Duration {
        (Duration::from_micros(self.configured_usec()) / 2).clamp(MIN_TICK, MAX_TICK)
    }

    pub fn release(&mut self) {
        self.daemon.release();
    }
}

/// What was recorded and waits to be sent, at most `maxlength` bytes, and
/// the stream's write and read indexes in bytes.
struct Pending {
    bytes: VecDeque<u8>,
    maxlength: u32,
    write_index: u64,
    read_index: u64,
    /// An overrun was reported and no fragment has gone out since.
    overflowing: bool,
}

impl Pending {
    fn new(maxlength: u32) -> Pending {
        Pending {
            bytes: VecDeque::new(),
            maxlength,
            write_index: 0,
            read_index: 0,
            overflowing: false,
        }
    }

    /// Appends whole frames, dropping the oldest beyond `maxlength`.
    /// Returns whether any were dropped.
    fn push(&mut self, data: &[u8]) -> bool {
        self.bytes.extend(data);
        self.write_index += data.len() as u64;
        let over = self.bytes.len().saturating_sub(self.maxlength as usize);
        self.bytes.drain(..over);
        self.read_index += over as u64;
        over > 0
    }

    /// The oldest `size` bytes, once there are as many.
    fn take(&mut self, size: usize) -> Option<Vec<u8>> {
        if size == 0 || self.bytes.len() < size {
            return None;
        }
        self.read_index += size as u64;
        self.overflowing = false;
        Some(self.bytes.drain(..size).collect())
    }

    /// Notes an overrun. True if it starts one the client has not heard
    /// of: the first since a fragment last went out.
    fn overflowed(&mut self) -> bool {
        !std::mem::replace(&mut self.overflowing, true)
    }

    fn clear(&mut self) {
        self.read_index += self.bytes.len() as u64;
        self.bytes.clear();
    }
}

/// How much may wait for the client on its way before a stream holds its
/// fragments back: its maxlength, at most the connection's `budget` and
/// `SEND_BUDGET`.
fn send_limit(maxlength: u32, budget: usize) -> usize {
    (maxlength as usize).min(budget).min(SEND_BUDGET)
}

/// The budget of a connection whose socket has a send buffer of `sndbuf`,
/// as `SO_SNDBUF` reports it: half of it, at most `SEND_BUDGET`. The
/// socket's queue is counted as the kernel counts it for `SIOCOUTQ`, by
/// the memory its buffers take rather than their payload, and the
/// connection's replies need room too: within half, record data alone
/// never fills the socket, so a stalled client does not make every write
/// of the connection wait for its timeout.
pub fn socket_budget(sndbuf: usize) -> usize {
    (sndbuf / 2).min(SEND_BUDGET)
}

/// A fragment in bytes: `u32::MAX` or 0 is 25 ms, otherwise whole frames,
/// at least one and at most 64 KiB.
fn fragment_bytes(asked: u32, rate: u32, frame: u32) -> u32 {
    let frame = frame.max(1);
    let bytes = if asked == 0 || asked == u32::MAX {
        (u64::from(rate) * DEFAULT_FRAGMENT_MS / 1000 * u64::from(frame)).min(u64::from(u32::MAX))
            as u32
    } else {
        asked
    };
    (bytes / frame).clamp(1, MAX_FRAGMENT / frame) * frame
}

/// The most that waits to be sent, in bytes: `u32::MAX` or 0 is 2 s,
/// otherwise whole frames, at least `fragsize` and at most 4 MiB.
/// The daemon id a meter of one sink input records. A sink input that is
/// gone, or whose id is unknown (0 is the master), is not found: it must
/// never fall back to recording the mix.
fn monitored_stream(id: Option<u32>) -> io::Result<u32> {
    id.filter(|&id| id != 0)
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
}

fn max_length(asked: u32, rate: u32, frame: u32, fragsize: u32) -> u32 {
    let frame = frame.max(1);
    let bytes = if asked == 0 || asked == u32::MAX {
        (u64::from(rate) * DEFAULT_MAXLENGTH_MS / 1000 * u64::from(frame)).min(u64::from(u32::MAX))
            as u32
    } else {
        asked
    };
    ((bytes / frame) * frame).clamp(fragsize, MAX_LENGTH / frame * frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sink_input_without_a_daemon_id_is_not_found() {
        assert_eq!(monitored_stream(Some(7)).unwrap(), 7);
        for id in [None, Some(0)] {
            let e = monitored_stream(id).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::NotFound);
        }
    }

    #[test]
    fn peaks_take_the_largest_magnitude_per_interval() {
        let mut peaks = Peaks::new(48_000, 25);
        let mut out = Vec::new();
        let mut frames = vec![[1, 2]; 1920];
        frames[700] = [-9, 3];
        frames[1000] = [4, -20];
        peaks.push(&frames, 2, &mut out);
        assert_eq!(out, vec![[9, 20]]);
        out.clear();
        peaks.push(&vec![[5, 6]; 3840], 2, &mut out);
        assert_eq!(out, vec![[5, 6], [5, 6]]);
    }

    #[test]
    fn peaks_of_i32_min_are_i32_max() {
        let mut peaks = Peaks::new(48_000, 48_000);
        let mut out = Vec::new();
        peaks.push(&[[i32::MIN, i32::MIN + 1]], 2, &mut out);
        assert_eq!(out, vec![[i32::MAX, i32::MAX]]);
    }

    #[test]
    fn mono_peaks_take_the_louder_channel() {
        let mut peaks = Peaks::new(48_000, 48_000);
        let mut out = Vec::new();
        peaks.push(&[[100, -300]], 1, &mut out);
        assert_eq!(out, vec![[300, 300]]);
    }

    #[test]
    fn peaks_above_the_daemon_rate_repeat() {
        let mut peaks = Peaks::new(8_000, 16_000);
        let mut out = Vec::new();
        peaks.push(&[[1, 1], [2, 2], [3, 3]], 2, &mut out);
        assert_eq!(out, vec![[1, 1], [1, 1], [2, 2], [2, 2], [3, 3], [3, 3]]);
    }

    #[test]
    fn peaks_keep_real_time_at_an_uneven_ratio() {
        let mut peaks = Peaks::new(44_100, 144);
        let mut out = Vec::new();
        for _ in 0..441 {
            peaks.push(&[[7, 7]; 100], 2, &mut out);
        }
        assert_eq!(out.len(), 144);
    }

    #[test]
    fn a_fragment_is_whole_frames_within_bounds() {
        assert_eq!(fragment_bytes(0, 48_000, 4), 4800);
        assert_eq!(fragment_bytes(u32::MAX, 48_000, 4), 4800);
        assert_eq!(fragment_bytes(1001, 48_000, 4), 1000);
        assert_eq!(fragment_bytes(1, 48_000, 4), 4);
        assert_eq!(fragment_bytes(1 << 20, 48_000, 4), 65_536);
        // 3-byte mono frames: 64 KiB rounded down to whole frames.
        assert_eq!(fragment_bytes(1 << 20, 48_000, 3), 65_535);
        // 25 ms at 384 kHz of 8-byte frames is beyond 64 KiB.
        assert_eq!(fragment_bytes(0, 384_000, 8), 65_536);
        assert_eq!(fragment_bytes(0, 44_100, 2), 2204);
    }

    #[test]
    fn maxlength_is_two_seconds_by_default_and_at_least_a_fragment() {
        assert_eq!(max_length(0, 48_000, 4, 4800), 384_000);
        assert_eq!(max_length(u32::MAX, 48_000, 4, 4800), 384_000);
        assert_eq!(max_length(9601, 48_000, 4, 4800), 9600);
        assert_eq!(max_length(100, 48_000, 4, 4800), 4800);
        assert_eq!(max_length(u32::MAX - 1, 48_000, 4, 4800), MAX_LENGTH);
    }

    #[test]
    fn what_waits_for_the_client_stays_within_the_budget() {
        assert_eq!(send_limit(9600, SEND_BUDGET), 9600);
        assert_eq!(send_limit(MAX_LENGTH, SEND_BUDGET), SEND_BUDGET);
        // Each poll sends while below the limit, so at most a fragment
        // more; with the largest, still under the connection's 1 MiB.
        assert!(send_limit(MAX_LENGTH, usize::MAX) + MAX_FRAGMENT as usize <= 1 << 19);
        // A socket's budget smaller than the stream's limit wins.
        assert_eq!(send_limit(MAX_LENGTH, 100_000), 100_000);
        assert_eq!(send_limit(9600, 100_000), 9600);
    }

    #[test]
    fn a_socket_budget_is_half_its_send_buffer() {
        // Linux reports twice what was set, 212992 by default.
        assert_eq!(socket_budget(212_992), 106_496);
        assert_eq!(socket_budget(0), 0);
        assert_eq!(socket_budget(8 << 20), SEND_BUDGET);
    }

    #[test]
    fn a_long_stall_sends_one_overflow_until_a_fragment_goes_out() {
        let mut pending = Pending::new(8);
        let mut overflows = 0;
        // A client that reads nothing: every poll drops, ten thousand times.
        for _ in 0..10_000 {
            if pending.push(&[0; 4]) && pending.overflowed() {
                overflows += 1;
            }
        }
        assert_eq!(overflows, 1);
        // Once it reads again, the next overrun is news.
        assert!(pending.take(4).is_some());
        assert!(pending.push(&[0; 8]));
        assert!(pending.overflowed());
        assert!(!pending.overflowed());
    }

    #[test]
    fn pending_drops_the_oldest_and_counts_both_indexes() {
        let mut pending = Pending::new(8);
        assert!(!pending.push(&[1, 2, 3, 4, 5, 6]));
        assert!(pending.push(&[7, 8, 9, 10]));
        assert_eq!(pending.take(4), Some(vec![3, 4, 5, 6]));
        assert_eq!(pending.take(8), None);
        assert_eq!((pending.write_index, pending.read_index), (10, 6));
        pending.clear();
        assert_eq!((pending.write_index, pending.read_index), (10, 10));
        assert_eq!(pending.take(0), None);
    }
}
