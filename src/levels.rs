//! Levels for the `/levels` WebSocket: RMS, peak and clipping per channel of
//! every output and of the analog input. The audio thread sums each meter's
//! samples over windows of a tenth of a second and publishes each finished
//! window into atomics behind a counter; the API thread reads the newest
//! window of each meter, without locks and without waking anyone.

use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, fence};

/// The meters: outputs 1 to 4, then the analog input.
pub const METERS: usize = 5;
/// The analog input's meter.
pub const ANALOG: usize = 4;
/// The most channels a meter has, one clip bit each.
const MAX_CHANNELS: usize = 8;
/// The byte a frame names the analog input's meter by.
const ANALOG_ID: u8 = 16;
/// A meter whose newest window ended longer ago than this reads as silence:
/// two windows.
const STALE_NS: u64 = 200_000_000;
/// How often `frame` tries a meter that is being written before it gives
/// silence. The writer holds it for a few stores, every 100 ms.
const TRIES: usize = 1000;

/// What the audio thread publishes and the API thread reads.
pub struct Levels {
    pub meters: [Meter; METERS],
}

/// One meter's newest window. Linear levels at full scale 1.0, as `f32`
/// bits. `seq` is odd while a window is written.
pub struct Meter {
    channels: AtomicU8,
    seq: AtomicU64,
    /// When the newest window ended, on CLOCK_MONOTONIC; 0 for none yet.
    end_ns: AtomicU64,
    rms: [AtomicU32; MAX_CHANNELS],
    peak: [AtomicU32; MAX_CHANNELS],
    /// A bit per channel, channel 1 the lowest.
    clipped: AtomicU8,
}

impl Meter {
    fn new(channels: u8) -> Meter {
        Meter {
            channels: AtomicU8::new(channels.min(MAX_CHANNELS as u8)),
            seq: AtomicU64::new(0),
            end_ns: AtomicU64::new(0),
            rms: std::array::from_fn(|_| AtomicU32::new(0)),
            peak: std::array::from_fn(|_| AtomicU32::new(0)),
            clipped: AtomicU8::new(0),
        }
    }

    /// The channels it measures; 0 for a meter that is not configured.
    pub fn channels(&self) -> usize {
        usize::from(self.channels.load(Relaxed))
    }

    /// Writes a window, as a seqlock: `seq` odd while the fields change.
    fn publish(&self, rms: &[f32], peak: &[f32], clipped: u8, end_ns: u64) {
        let seq = self.seq.load(Relaxed);
        self.seq.store(seq.wrapping_add(1), Relaxed);
        fence(Release);
        for (to, v) in self.rms.iter().zip(rms) {
            to.store(v.to_bits(), Relaxed);
        }
        for (to, v) in self.peak.iter().zip(peak) {
            to.store(v.to_bits(), Relaxed);
        }
        self.clipped.store(clipped, Relaxed);
        self.end_ns.store(end_ns, Relaxed);
        self.seq.store(seq.wrapping_add(2), Release);
    }

    /// The newest window, whole: its levels, clip bits and end. None if
    /// it was being written every time it was tried.
    pub(crate) fn read(&self) -> Option<Window> {
        for _ in 0..TRIES {
            let before = self.seq.load(Acquire);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let mut window = Window {
                rms: [0.0; MAX_CHANNELS],
                peak: [0.0; MAX_CHANNELS],
                clipped: self.clipped.load(Relaxed),
                end_ns: self.end_ns.load(Relaxed),
            };
            for (i, (r, p)) in self.rms.iter().zip(&self.peak).enumerate() {
                window.rms[i] = f32::from_bits(r.load(Relaxed));
                window.peak[i] = f32::from_bits(p.load(Relaxed));
            }
            fence(Acquire);
            if self.seq.load(Relaxed) == before {
                return Some(window);
            }
        }
        None
    }
}

pub(crate) struct Window {
    pub(crate) rms: [f32; MAX_CHANNELS],
    pub(crate) peak: [f32; MAX_CHANNELS],
    pub(crate) clipped: u8,
    pub(crate) end_ns: u64,
}

impl Levels {
    /// Meters of these channel counts, outputs 1 to 4 and then the analog
    /// input. 0 is a meter that is not configured, and is left out of
    /// every frame.
    pub fn new(channels: [u8; METERS]) -> Levels {
        Levels {
            meters: channels.map(Meter::new),
        }
    }
}

/// A sample a meter takes, in one of the processing formats.
pub trait Sample: Copy {
    /// The value at full scale 1.0.
    fn unit(self) -> f32;
    /// At or past full scale.
    fn clips(self) -> bool;
}

impl Sample for i32 {
    fn unit(self) -> f32 {
        self as f32 / 2_147_483_648.0
    }

    fn clips(self) -> bool {
        self == i32::MAX || self == i32::MIN
    }
}

impl Sample for f32 {
    fn unit(self) -> f32 {
        self
    }

    fn clips(self) -> bool {
        self.abs() >= 1.0
    }
}

impl Sample for f64 {
    fn unit(self) -> f32 {
        self as f32
    }

    fn clips(self) -> bool {
        self.abs() >= 1.0
    }
}

/// The audio thread's side of one meter: sums over a window, then
/// publishes. Holds no heap memory, so it is made anywhere and never
/// allocates.
pub struct Metering {
    channels: usize,
    /// Frames in a window: a tenth of a second.
    window: usize,
    /// Frames summed in this window so far.
    frames: usize,
    sum: [f64; MAX_CHANNELS],
    peak: [f32; MAX_CHANNELS],
    clipped: u8,
}

impl Metering {
    /// A meter of `channels` channels, at most 8, at `rate`.
    pub fn new(channels: usize, rate: u32) -> Metering {
        Metering {
            channels: channels.min(MAX_CHANNELS),
            window: window_of(rate),
            frames: 0,
            sum: [0.0; MAX_CHANNELS],
            peak: [0.0; MAX_CHANNELS],
            clipped: 0,
        }
    }

    /// Windows of a tenth of a second at `rate`, from the next frame on.
    pub fn set_rate(&mut self, rate: u32) {
        self.window = window_of(rate);
        self.restart();
    }

    fn restart(&mut self) {
        self.frames = 0;
        self.sum = [0.0; MAX_CHANNELS];
        self.peak = [0.0; MAX_CHANNELS];
        self.clipped = 0;
    }

    /// Takes frames of interleaved samples, of the meter's channels, and
    /// publishes each window they finish into `meter` as ending at
    /// `now_ns`.
    pub fn add<S: Sample>(&mut self, interleaved: &[S], meter: &Meter, now_ns: u64) {
        let channels = self.channels;
        if channels == 0 {
            return;
        }
        for frame in interleaved.chunks_exact(channels) {
            for (c, &s) in frame.iter().enumerate() {
                let v = s.unit();
                self.sum[c] += f64::from(v) * f64::from(v);
                // Not a number counts as nothing.
                if v.abs() > self.peak[c] {
                    self.peak[c] = v.abs();
                }
                if s.clips() {
                    self.clipped |= 1 << c;
                }
            }
            self.frames += 1;
            if self.frames >= self.window {
                self.publish(meter, now_ns);
            }
        }
    }

    /// Takes `frames` frames of silence, such as while DoP plays.
    pub fn silence(&mut self, frames: usize, meter: &Meter, now_ns: u64) {
        if self.channels == 0 {
            return;
        }
        let mut left = frames;
        while left > 0 {
            let take = left.min(self.window - self.frames);
            self.frames += take;
            left -= take;
            if self.frames >= self.window {
                self.publish(meter, now_ns);
            }
        }
    }

    fn publish(&mut self, meter: &Meter, now_ns: u64) {
        let mut rms = [0.0f32; MAX_CHANNELS];
        for (r, sum) in rms.iter_mut().zip(&self.sum) {
            *r = (sum / self.frames as f64).sqrt() as f32;
        }
        let n = self.channels;
        meter.publish(&rms[..n], &self.peak[..n], self.clipped, now_ns);
        self.restart();
    }
}

fn window_of(rate: u32) -> usize {
    (rate as usize / 10).max(1)
}

/// One `/levels` frame, read at `now_ns`: each configured meter in turn, a
/// byte for its id (1 to 4 the output, 16 the analog input), a byte for its
/// channels n, a byte each for RMS and peak per channel, and a byte with a
/// bit per channel that clipped. A meter whose newest window ended more
/// than 200 ms before `now_ns` reads as silence.
pub fn frame(levels: &Levels, now_ns: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, meter) in levels.meters.iter().enumerate() {
        let n = meter.channels();
        if n == 0 {
            continue;
        }
        let id = if i == ANALOG { ANALOG_ID } else { i as u8 + 1 };
        out.extend([id, n as u8]);
        let window = meter
            .read()
            .filter(|w| w.end_ns != 0 && now_ns.saturating_sub(w.end_ns) <= STALE_NS);
        match window {
            Some(w) => {
                for c in 0..n {
                    out.extend([level_byte(w.rms[c]), level_byte(w.peak[c])]);
                }
                out.push(w.clipped);
            }
            None => out.extend(std::iter::repeat_n(0, 2 * n + 1)),
        }
    }
    out
}

/// A linear level as a byte: 0 at -60 dBFS or less, 255 at 0 dBFS or more,
/// `round((dB + 60) / 60 * 255)` between.
pub fn level_byte(linear: f32) -> u8 {
    if linear.is_nan() || linear <= 0.0 {
        return 0;
    }
    let db = 20.0 * linear.log10();
    if db <= -60.0 {
        0
    } else if db >= 0.0 {
        255
    } else {
        ((db + 60.0) / 60.0 * 255.0).round() as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_no_alloc::assert_no_alloc;

    const T: u64 = 1_000_000_000;
    const MS: u64 = 1_000_000;

    #[test]
    fn level_bytes() {
        assert_eq!(level_byte(1.0), 255);
        assert_eq!(level_byte(2.0), 255);
        assert_eq!(level_byte(0.5), 229);
        assert_eq!(level_byte(0.001), 0);
        assert_eq!(level_byte(0.0001), 0);
        assert_eq!(level_byte(0.0), 0);
        assert_eq!(level_byte(f32::NAN), 0);
    }

    /// The bytes of a stereo meter, after its id and channel count.
    fn bytes_of(levels: &Levels, now_ns: u64) -> Vec<u8> {
        frame(levels, now_ns)[2..].to_vec()
    }

    fn stereo() -> Levels {
        Levels::new([2, 0, 0, 0, 0])
    }

    #[test]
    fn a_full_scale_sine_window() {
        let levels = stereo();
        let mut metering = Metering::new(2, 48000);
        // Half a sample out of phase, so that no sample lands on the crest:
        // one at full scale would be a clip.
        let samples: Vec<f32> = (0..4800)
            .flat_map(|i| {
                let t = (i as f64 + 0.5) / 48000.0;
                let v = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() as f32;
                [v, v]
            })
            .collect();
        for block in samples.chunks(128 * 2) {
            metering.add(block, &levels.meters[0], T);
        }
        let bytes = bytes_of(&levels, T);
        for c in 0..2 {
            let rms = bytes[2 * c];
            assert!((241..=243).contains(&rms), "RMS byte {rms}");
            assert_eq!(bytes[2 * c + 1], 255, "peak");
        }
        assert_eq!(bytes[4], 0, "no clip");
    }

    #[test]
    fn a_clip_is_seen_in_every_format() {
        fn clips<S: Sample>(full: S, quiet: S) -> u8 {
            let levels = stereo();
            let mut metering = Metering::new(2, 10);
            // A window of one frame: the clip on channel 2 alone.
            metering.add(&[quiet, full], &levels.meters[0], T);
            bytes_of(&levels, T)[4]
        }
        assert_eq!(clips(i32::MAX, 1 << 20), 0b10);
        assert_eq!(clips(i32::MIN, 1 << 20), 0b10);
        assert_eq!(clips(1.0f32, 0.5), 0b10);
        assert_eq!(clips(-1.0f64, 0.5), 0b10);
        assert_eq!(clips(i32::MAX - 1, 0), 0);
        assert_eq!(clips(0.999f32, 0.0), 0);
    }

    #[test]
    fn a_stale_meter_reads_as_silence() {
        let levels = stereo();
        let mut metering = Metering::new(2, 10);
        metering.add(&[0.5f32, 0.5], &levels.meters[0], T);
        assert_eq!(bytes_of(&levels, T + 150 * MS), [229, 229, 229, 229, 0]);
        assert_eq!(bytes_of(&levels, T + 250 * MS), [0, 0, 0, 0, 0]);
        // Still in the frame, with its id and channels.
        assert_eq!(frame(&levels, T + 250 * MS)[..2], [1, 2]);
        // Never published reads as silence too.
        assert_eq!(bytes_of(&stereo(), T), [0; 5]);
    }

    #[test]
    fn a_window_is_published_only_once_full() {
        let levels = stereo();
        let mut metering = Metering::new(2, 48000);
        metering.add(&[0.5f32; 2 * 4799], &levels.meters[0], T);
        assert_eq!(bytes_of(&levels, T), [0; 5]);
        metering.add(&[0.5f32; 2], &levels.meters[0], T);
        assert_eq!(bytes_of(&levels, T), [229, 229, 229, 229, 0]);
    }

    #[test]
    fn silence_finishes_windows() {
        let levels = stereo();
        let mut metering = Metering::new(2, 48000);
        metering.add(&[0.5f32; 2 * 4800], &levels.meters[0], T);
        metering.silence(4800, &levels.meters[0], T + 100 * MS);
        assert_eq!(bytes_of(&levels, T + 100 * MS), [0; 5]);
        assert_eq!(levels.meters[0].read().unwrap().end_ns, T + 100 * MS);
    }

    #[test]
    fn the_frame_layout() {
        let levels = Levels::new([2, 0, 0, 0, 2]);
        let mut output = Metering::new(2, 10);
        let mut analog = Metering::new(2, 10);
        output.add(&[0.5f32, 1.0], &levels.meters[0], T);
        analog.add(&[1 << 30, i32::MIN], &levels.meters[ANALOG], T);
        let f = frame(&levels, T);
        assert_eq!(
            f,
            [
                1, 2, 229, 229, 255, 255, 0b10, 16, 2, 229, 229, 255, 255, 0b10
            ]
        );
    }

    #[test]
    fn metering_does_not_allocate() {
        let levels = Levels::new([8, 2, 0, 0, 2]);
        let mut metering = Metering::new(8, 48000);
        let block = vec![0.25f64; 8 * 128];
        assert_no_alloc(|| {
            for i in 0..100 {
                metering.add(&block, &levels.meters[0], T + i * MS);
                metering.silence(128, &levels.meters[0], T + i * MS);
            }
            metering.set_rate(44100);
        });
        assert_ne!(bytes_of(&levels, T + 100 * MS)[0], 0);
    }
}
