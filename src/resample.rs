//! Sample rate conversion for one client, read straight from its ring.
//!
//! `process` does not allocate. All buffers are made in `new`, which runs on
//! the control thread.

use crate::config::{Format, Quality};
use crate::format::{F32, F64, Processing};
use aas_client::ring::Ring;
use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Indexing, Resampler as _, Sample, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};
use std::sync::atomic::Ordering;

const CHANNELS: usize = 2;
/// The largest ratio change rubato prepares for. The ratio stays fixed here.
const MAX_RATIO_CHANGE: f64 = 1.1;
/// Why the calls into rubato cannot fail.
const SIZED: &str = "the buffers are sized in Stage::new";

/// The parameters `dsp-bench` measured as `high` and `medium`.
fn parameters(quality: Quality) -> SincInterpolationParameters {
    match quality {
        Quality::High => SincInterpolationParameters::new(128, WindowFunction::Blackman2)
            .oversampling_factor(256)
            .interpolation(SincInterpolationType::Cubic),
        Quality::Medium => SincInterpolationParameters::new(64, WindowFunction::Blackman2)
            .oversampling_factor(128)
            .interpolation(SincInterpolationType::Linear),
    }
}

/// The resampler and its buffers, in the precision of `P`.
struct Stage<P: Processing>
where
    P::Acc: Sample,
{
    inner: Async<P::Acc>,
    frames: Vec<[i32; 2]>,
    input: Vec<P::Acc>,
    output: Vec<P::Acc>,
}

impl<P: Processing> Stage<P>
where
    P::Acc: Sample,
{
    fn new(ratio: f64, period: usize, quality: Quality) -> Self {
        let inner = Async::new_sinc(
            ratio,
            MAX_RATIO_CHANGE,
            &parameters(quality),
            period,
            CHANNELS,
            FixedAsync::Output,
        )
        .expect("the rates and the period are positive");
        let frames = inner.input_frames_max();
        Stage {
            inner,
            frames: vec![[0; 2]; frames],
            input: vec![P::zero(); frames * CHANNELS],
            output: vec![P::zero(); period * CHANNELS],
        }
    }

    /// Takes what the resampler wants from `ring`, up to `limit` frames and
    /// what is there. The rest counts as silence. Returns the frames taken
    /// and the frames missing.
    fn run(&mut self, ring: &Ring, out: &mut [[i32; 2]], limit: usize) -> (usize, usize) {
        let need = self.inner.input_frames_next();
        let got = ring.read(&mut self.frames[..need.min(limit)]);
        for (frame, floats) in self.frames[..got]
            .iter()
            .zip(self.input.chunks_exact_mut(CHANNELS))
        {
            floats[0] = to_float::<P>(frame[0]);
            floats[1] = to_float::<P>(frame[1]);
        }
        let period = self.output.len() / CHANNELS;
        let input = InterleavedSlice::new(&self.input, CHANNELS, need).expect(SIZED);
        let mut output =
            InterleavedSlice::new_mut(&mut self.output, CHANNELS, period).expect(SIZED);
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(got),
            active_channels_mask: None,
        };
        self.inner
            .process_into_buffer(&input, &mut output, Some(&indexing))
            .expect(SIZED);
        for (frame, floats) in out.iter_mut().zip(self.output.chunks_exact(CHANNELS)) {
            *frame = [P::finish(floats[0]), P::finish(floats[1])];
        }
        (got, need - got)
    }
}

/// S32 to float, by the exact scaling of the processing format.
fn to_float<P: Processing>(sample: i32) -> P::Acc {
    let mut acc = P::zero();
    P::accumulate(&mut acc, sample, 1.0);
    acc
}

enum Precision {
    Single(Stage<F32>),
    Double(Stage<F64>),
}

/// Converts one client's audio to the output rate, one period per call. It
/// runs in `f32` for the `i32` and `f32` formats and in `f64` for `f64`.
pub struct Resampler {
    precision: Precision,
    period: usize,
    /// Input frames of silence that push the last audio out of the filter.
    tail: usize,
    /// Input frames of silence fed in since the last audio.
    padded: usize,
    /// Audio has gone in.
    primed: bool,
}

impl Resampler {
    pub fn new(from: u32, to: u32, period: u32, quality: Quality, format: Format) -> Resampler {
        let ratio = f64::from(to) / f64::from(from);
        let frames = period as usize;
        let precision = match format {
            Format::I32 | Format::F32 => Precision::Single(Stage::new(ratio, frames, quality)),
            Format::F64 => Precision::Double(Stage::new(ratio, frames, quality)),
        };
        let mut resampler = Resampler {
            precision,
            period: frames,
            tail: 0,
            padded: 0,
            primed: false,
        };
        resampler.tail = (f64::from(resampler.delay_frames()) / ratio).ceil() as usize + 1;
        // An empty filter has nothing left to push out.
        resampler.padded = resampler.tail;
        resampler
    }

    /// Input frames the next call takes. It varies by a frame from call to
    /// call.
    pub fn input_frames_next(&self) -> usize {
        match &self.precision {
            Precision::Single(s) => s.inner.input_frames_next(),
            Precision::Double(s) => s.inner.input_frames_next(),
        }
    }

    /// The delay of the filter, in output frames.
    pub fn delay_frames(&self) -> u32 {
        let delay = match &self.precision {
            Precision::Single(s) => s.inner.output_delay(),
            Precision::Double(s) => s.inner.output_delay(),
        };
        delay as u32
    }

    /// Whether `process` can yield a period now.
    pub fn ready(&self, ring: &Ring) -> bool {
        // The flag comes first: a client sets it after its last frames.
        let draining = ring.header().draining.load(Ordering::Acquire) != 0;
        let available = ring.available();
        let enough = available >= self.input_frames_next();
        enough || (draining && (available > 0 || !self.flushed()))
    }

    /// Whether audio has gone in yet. Before that, a client short of input is
    /// starting up rather than late.
    pub fn primed(&self) -> bool {
        self.primed
    }

    /// Whether all the audio fed in so far has come out again.
    pub fn flushed(&self) -> bool {
        self.padded >= self.tail
    }

    /// Forgets what is in the filter, as its audio is being discarded.
    pub fn skip(&mut self) {
        self.padded = self.tail;
    }

    /// Writes one period to `out` and returns its length, or returns 0 and
    /// consumes nothing when the ring holds too little. A draining ring is
    /// padded with silence instead, so its last frames still come out.
    pub fn process(&mut self, ring: &Ring, out: &mut [[i32; 2]]) -> usize {
        self.process_up_to(ring, out, usize::MAX)
    }

    /// As `process`, but takes at most `limit` frames from the ring, even if
    /// it wanted more. A limit that cuts the input short gives a full period
    /// with silence in place of the frames left out.
    pub fn process_up_to(&mut self, ring: &Ring, out: &mut [[i32; 2]], limit: usize) -> usize {
        let cut_short = limit < self.input_frames_next().min(ring.available());
        if !cut_short && !self.ready(ring) {
            return 0;
        }
        let n = self.period.min(out.len());
        let (got, missing) = match &mut self.precision {
            Precision::Single(s) => s.run(ring, &mut out[..n], limit),
            Precision::Double(s) => s.run(ring, &mut out[..n], limit),
        };
        if got > 0 {
            self.primed = true;
            self.padded = missing;
        } else {
            self.padded += missing;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_no_alloc::assert_no_alloc;

    const PERIOD: usize = 64;

    fn ring() -> Ring {
        Ring::create(1 << 16).unwrap().0
    }

    fn resampler(format: Format) -> Resampler {
        Resampler::new(44100, 48000, PERIOD as u32, Quality::High, format)
    }

    fn tone(n: usize) -> Vec<[i32; 2]> {
        (0..n)
            .map(|i| {
                let phase = i as f64 * 1000.0 / 44100.0 * std::f64::consts::TAU;
                let v = (phase.sin() * 1e9) as i32;
                [v, -v]
            })
            .collect()
    }

    #[test]
    fn each_call_yields_one_period_and_takes_what_it_asked_for() {
        for format in [Format::I32, Format::F32, Format::F64] {
            let ring = ring();
            ring.write(&tone(20_000));
            let mut r = resampler(format);
            let mut out = [[0; 2]; PERIOD];
            for _ in 0..200 {
                let need = r.input_frames_next();
                let before = ring.available();
                assert_eq!(r.process(&ring, &mut out), PERIOD);
                assert_eq!(before - ring.available(), need);
            }
        }
    }

    #[test]
    fn too_little_input_yields_nothing_and_consumes_nothing() {
        let ring = ring();
        let mut r = resampler(Format::F32);
        ring.write(&tone(r.input_frames_next() - 1));
        let before = ring.available();
        assert!(!r.ready(&ring));
        assert_eq!(r.process(&ring, &mut [[0; 2]; PERIOD]), 0);
        assert_eq!(ring.available(), before);
    }

    #[test]
    fn both_precisions_produce_the_same_audio() {
        let (a, b) = (ring(), ring());
        a.write(&tone(20_000));
        b.write(&tone(20_000));
        let mut single = resampler(Format::F32);
        let mut double = resampler(Format::F64);
        let mut x = [[0; 2]; PERIOD];
        let mut y = [[0; 2]; PERIOD];
        let mut loudest = 0;
        for _ in 0..100 {
            single.process(&a, &mut x);
            double.process(&b, &mut y);
            for (p, q) in x.iter().zip(y.iter()) {
                assert!((p[0] - q[0]).abs() < 2_000, "{} {}", p[0], q[0]);
                loudest = loudest.max(q[0].abs());
            }
        }
        assert!(loudest > 900_000_000, "{loudest}");
    }

    #[test]
    fn process_does_not_allocate_even_while_draining() {
        for format in [Format::F32, Format::F64] {
            for quality in [Quality::High, Quality::Medium] {
                let ring = ring();
                ring.write(&tone(10_000));
                let mut r = Resampler::new(44100, 48000, PERIOD as u32, quality, format);
                let mut out = [[0; 2]; PERIOD];
                assert_no_alloc(|| {
                    for _ in 0..100 {
                        assert_eq!(r.process(&ring, &mut out), PERIOD);
                    }
                    // The input size varies, and now it falls short as well.
                    ring.consume(ring.available() - 10);
                    ring.header().draining.store(1, Ordering::Release);
                    while r.ready(&ring) {
                        r.process(&ring, &mut out);
                    }
                });
                assert_eq!(ring.available(), 0);
            }
        }
    }

    #[test]
    fn medium_quality_runs_too() {
        let ring = ring();
        ring.write(&tone(20_000));
        let mut r = Resampler::new(96000, 48000, 128, Quality::Medium, Format::F32);
        assert_eq!(r.process(&ring, &mut [[0; 2]; 128]), 128);
    }

    #[test]
    fn a_draining_stream_is_flushed_through_the_filter() {
        // A step at the very end of the input must come out of the resampler.
        let ring = ring();
        let mut input = vec![[0; 2]; 1000];
        input.extend([[1_000_000_000; 2]; 200]);
        ring.write(&input);
        ring.header().draining.store(1, Ordering::Release);
        let mut r = resampler(Format::F32);
        let mut out = [[0; 2]; PERIOD];
        let mut all = Vec::new();
        for _ in 0..100 {
            if ring.available() == 0 && r.flushed() {
                break;
            }
            assert_eq!(r.process(&ring, &mut out), PERIOD);
            all.extend_from_slice(&out);
        }
        assert!(r.flushed());
        assert!(!r.ready(&ring));
        // 200 input frames are 217 at the output rate. All of them must appear.
        let loud = all.iter().filter(|f| f[0] > 500_000_000).count();
        assert!((215..=220).contains(&loud), "{loud}");
    }

    #[test]
    fn a_limit_keeps_the_rest_of_the_ring_out_of_the_filter() {
        let ring = ring();
        ring.write(&[[1_000_000_000; 2]; 2000]);
        let mut r = resampler(Format::F32);
        let before = ring.available();
        assert_eq!(r.process_up_to(&ring, &mut [[0; 2]; PERIOD], 30), PERIOD);
        assert_eq!(before - ring.available(), 30);
    }

    #[test]
    fn a_new_resampler_is_flushed_and_not_primed() {
        let ring = ring();
        let mut r = resampler(Format::F32);
        assert!(r.flushed());
        assert!(!r.primed());
        ring.write(&tone(1000));
        r.process(&ring, &mut [[0; 2]; PERIOD]);
        assert!(!r.flushed());
        assert!(r.primed());
    }

    #[test]
    fn a_short_remainder_can_always_be_read_while_draining() {
        let ring = ring();
        let r = resampler(Format::F32);
        ring.write(&tone(3));
        assert!(!r.ready(&ring));
        ring.header().draining.store(1, Ordering::Release);
        assert!(r.ready(&ring));
    }

    #[test]
    fn the_delay_is_reported_in_output_frames() {
        let r = resampler(Format::F32);
        assert_eq!(r.delay_frames(), 69);
    }
}
