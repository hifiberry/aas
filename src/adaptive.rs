//! Adaptive sample rate conversion for a further output, whose ratio moves to
//! follow the master's clock.
//!
//! The ratio is not a fixed fraction, so the filter is interpolated between
//! the points of a fine table, as `resample::interpolating` sets out.
//! `new` allocates and runs on the control thread. Nothing else allocates.

use crate::config::{Format, Quality};
use crate::format::{F32, F64, Processing};
use crate::resample::{interpolating, to_float};
use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Adjustable as _, Async, FixedAsync, Resampler as _, Resizable as _, Sample};

/// How far the ratio may move from the nominal one, either way.
pub const MAX_DRIFT: f64 = 0.002;
/// The ratio change rubato prepares for. It allows `1 / x` to `x`, and
/// `1 / (1 - MAX_DRIFT)` is a little more than `1 + MAX_DRIFT`.
const MAX_RATIO_CHANGE: f64 = 1.0 + 2.0 * MAX_DRIFT;
/// Most channels of an output.
const MAX_CHANNELS: usize = 8;
/// Why the calls into rubato cannot fail.
const SIZED: &str = "the buffers are sized in Stage::new and the ratio is clamped";

/// The resampler and its buffers, in the precision of `P`.
struct Stage<P: Processing>
where
    P::Acc: Sample,
{
    inner: Async<P::Acc>,
    input: Vec<P::Acc>,
    output: Vec<P::Acc>,
}

impl<P: Processing> Stage<P>
where
    P::Acc: Sample,
{
    fn new(ratio: f64, max_out: usize, channels: usize, quality: Quality) -> Self {
        let inner = Async::new_sinc(
            ratio,
            MAX_RATIO_CHANGE,
            &interpolating(quality),
            max_out,
            channels,
            FixedAsync::Output,
        )
        .expect("the rates, the period and the channels are positive");
        let frames = inner.input_frames_max();
        Stage {
            inner,
            input: vec![P::zero(); frames * channels],
            output: vec![P::zero(); max_out * channels],
        }
    }

    /// Resamples the first `need` frames of `input` into `output`, which
    /// holds the chunk size of frames.
    fn run(&mut self, input: &[i32], output: &mut [i32], channels: usize, need: usize) {
        let samples = need * channels;
        for (float, &sample) in self.input[..samples].iter_mut().zip(&input[..samples]) {
            *float = to_float::<P>(sample);
        }
        let frames = output.len() / channels;
        let input = InterleavedSlice::new(&self.input[..samples], channels, need).expect(SIZED);
        let mut floats =
            InterleavedSlice::new_mut(&mut self.output[..output.len()], channels, frames)
                .expect(SIZED);
        self.inner
            .process_into_buffer(&input, &mut floats, None)
            .expect(SIZED);
        for (sample, &float) in output.iter_mut().zip(&self.output) {
            *sample = P::finish(float);
        }
    }
}

enum Precision {
    Single(Stage<F32>),
    Double(Stage<F64>),
}

/// Calls `$body` with the stage, whichever its precision.
macro_rules! stage {
    ($self:expr, $s:ident => $body:expr) => {
        match $self {
            Precision::Single($s) => $body,
            Precision::Double($s) => $body,
        }
    };
}

/// Converts the audio of `channels` interleaved channels to a ratio that the
/// caller moves within `1 ± MAX_DRIFT` of the nominal one. It runs in `f32`
/// for the `i32` and `f32` formats and in `f64` for `f64`.
pub struct Adaptive {
    precision: Precision,
    channels: usize,
    max_out: usize,
    /// The output frames rubato is set up for now.
    chunk: usize,
    /// The nominal ratio, output rate over input rate.
    nominal: f64,
    /// The ratio relative to `nominal` last asked for, after clamping.
    relative: f64,
    taps: usize,
}

impl Adaptive {
    /// A resampler from `from` to `to` Hz yielding up to `max_out` frames per
    /// call. Allocates.
    pub fn new(
        from: u32,
        to: u32,
        channels: usize,
        max_out: u32,
        quality: Quality,
        format: Format,
    ) -> Adaptive {
        assert!(
            (1..=MAX_CHANNELS).contains(&channels),
            "{channels} channels, 1 to {MAX_CHANNELS} supported"
        );
        assert!(max_out > 0, "a period of no frames");
        let nominal = f64::from(to) / f64::from(from);
        let max_out = max_out as usize;
        let precision = match format {
            Format::I32 | Format::F32 => {
                Precision::Single(Stage::new(nominal, max_out, channels, quality))
            }
            Format::F64 => Precision::Double(Stage::new(nominal, max_out, channels, quality)),
        };
        Adaptive {
            precision,
            channels,
            max_out,
            chunk: max_out,
            nominal,
            relative: 1.0,
            taps: interpolating(quality).sinc_len,
        }
    }

    /// Sets the ratio relative to the nominal one, clamped to
    /// `1 ± MAX_DRIFT`. Not a number counts as 1. The change ramps in over
    /// the next call.
    pub fn set_ratio(&mut self, relative: f64) {
        let relative = if relative.is_nan() {
            1.0
        } else {
            relative.clamp(1.0 - MAX_DRIFT, 1.0 + MAX_DRIFT)
        };
        self.relative = relative;
        stage!(&mut self.precision, s => s.inner.set_resample_ratio_relative(relative, true))
            .expect(SIZED);
    }

    /// Input frames the next `process` takes to yield `out_frames` frames,
    /// at most `max_out`. Exact for the size of the last call (`max_out`
    /// before the first); for another size it may be a frame more.
    pub fn input_needed(&self, out_frames: u32) -> u32 {
        let out = (out_frames as usize).min(self.max_out);
        let (next, ratio) =
            stage!(&self.precision, s => (s.inner.input_frames_next(), s.inner.resample_ratio()));
        if out == self.chunk {
            return next as u32;
        }
        // rubato asks for the frames the chunk spans at the mean of the
        // current and the target ratio, rounded up.
        let mean = 0.5 * (ratio + self.nominal * self.relative);
        let shift = (out as f64 - self.chunk as f64) / mean;
        (next as f64 + shift).ceil().max(0.0) as u32
    }

    /// Resamples interleaved S32 frames from `input` into `output`. Yields
    /// as many frames as `output` holds, at most `max_out`, and takes
    /// `input_needed` of that from the front of `input`. Returns the frames
    /// consumed and produced, or `(0, 0)` with nothing done when `input` is
    /// short. Does not allocate.
    pub fn process(&mut self, input: &[i32], output: &mut [i32]) -> (usize, usize) {
        let channels = self.channels;
        let out = (output.len() / channels).min(self.max_out);
        if out == 0 {
            return (0, 0);
        }
        if out != self.chunk {
            stage!(&mut self.precision, s => s.inner.set_chunk_size(out)).expect(SIZED);
            self.chunk = out;
        }
        let need = stage!(&self.precision, s => s.inner.input_frames_next());
        if input.len() / channels < need {
            return (0, 0);
        }
        let output = &mut output[..out * channels];
        stage!(&mut self.precision, s => s.run(input, output, channels, need));
        (need, out)
    }

    /// The delay of the filter, in output frames at the current ratio. It
    /// is half the filter less one output frame, where a step comes out;
    /// rubato's own `output_delay` leaves out the frame.
    pub fn delay_frames(&self) -> f64 {
        let ratio = stage!(&self.precision, s => s.inner.resample_ratio());
        self.taps as f64 * ratio / 2.0 - 1.0
    }

    /// Empties the filter and returns to the nominal ratio. Does not
    /// allocate.
    pub fn reset(&mut self) {
        stage!(&mut self.precision, s => s.inner.reset());
        self.chunk = self.max_out;
        self.relative = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Format, Quality};
    use crate::format::{F32, F64, Processing};
    use crate::resample::to_float;
    use assert_no_alloc::assert_no_alloc;
    use std::f64::consts::TAU;

    const RATE: u32 = 48000;
    const PERIOD: usize = 256;
    /// 2^31, full scale.
    const FULL: f64 = 2_147_483_648.0;

    /// `frames` of a sine at `freq` and `-6 dBFS` on the channels in `on`,
    /// silence on the others. Odd channels carry it in the opposite phase.
    fn tone(frames: usize, channels: usize, on: &[usize], freq: f64) -> Vec<i32> {
        let mut out = vec![0; frames * channels];
        for (i, frame) in out.chunks_exact_mut(channels).enumerate() {
            let v = ((i as f64 * freq / f64::from(RATE) * TAU).sin() * FULL / 2.0).round() as i32;
            for &c in on {
                frame[c] = if c % 2 == 0 { v } else { -v };
            }
        }
        out
    }

    /// Feeds all of `input` through `a` one period at a time, while it holds
    /// enough for a period. Returns the output and the frames consumed and
    /// produced after the first call, which fills the filter.
    fn run(a: &mut Adaptive, input: &[i32], channels: usize) -> (Vec<i32>, usize, usize) {
        let mut out = Vec::new();
        let mut period = vec![0; PERIOD * channels];
        let (mut at, mut consumed, mut produced) = (0, 0, 0);
        let mut first = true;
        loop {
            let (c, p) = a.process(&input[at..], &mut period);
            if p == 0 {
                break;
            }
            at += c * channels;
            out.extend_from_slice(&period[..p * channels]);
            if !first {
                consumed += c;
                produced += p;
            }
            first = false;
        }
        (out, consumed, produced)
    }

    /// RMS of what is left of one channel after the best fitting sine at
    /// `freq` is taken away, in dB relative to a full scale sine.
    fn residual_dbfs(samples: &[f64], freq: f64) -> f64 {
        let w = TAU * freq / f64::from(RATE);
        let (mut ss, mut cc, mut sc, mut ys, mut yc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (n, y) in samples.iter().enumerate() {
            let (s, c) = (w * n as f64).sin_cos();
            ss += s * s;
            cc += c * c;
            sc += s * c;
            ys += y * s;
            yc += y * c;
        }
        let det = ss * cc - sc * sc;
        let (a, b) = ((ys * cc - yc * sc) / det, (yc * ss - ys * sc) / det);
        let mut err = 0.0;
        for (n, y) in samples.iter().enumerate() {
            let (s, c) = (w * n as f64).sin_cos();
            err += (y - a * s - b * c).powi(2);
        }
        let rms = (err / samples.len() as f64).sqrt();
        20.0 * (rms / (FULL / 2f64.sqrt())).log10()
    }

    fn channel(out: &[i32], channels: usize, c: usize, skip: usize) -> Vec<f64> {
        out.chunks_exact(channels)
            .skip(skip)
            .map(|f| f64::from(f[c]))
            .collect()
    }

    /// The residual of a 997 Hz tone through `a` at `relative`, per channel.
    fn purity(format: Format, quality: Quality, seconds: usize, relative: f64) -> [f64; 2] {
        let mut a = Adaptive::new(RATE, RATE, 2, PERIOD as u32, quality, format);
        a.set_ratio(relative);
        let input = tone(RATE as usize * seconds, 2, &[0, 1], 997.0);
        let (out, _, _) = run(&mut a, &input, 2);
        let skip = RATE as usize;
        // The tone comes out lower by the ratio.
        let freq = 997.0 / relative;
        [
            residual_dbfs(&channel(&out, 2, 0, skip), freq),
            residual_dbfs(&channel(&out, 2, 1, skip), freq),
        ]
    }

    #[test]
    fn a_tone_passes_at_ratio_one() {
        for (format, floor) in [
            (Format::F32, -100.0),
            (Format::I32, -100.0),
            (Format::F64, -110.0),
        ] {
            for quality in [Quality::High, Quality::Medium] {
                let residuals = purity(format, quality, SECONDS, 1.0);
                for r in residuals {
                    assert!(r < floor, "{format:?} {quality:?}: {r} dBFS");
                }
            }
        }
    }

    #[test]
    fn a_tone_stays_clean_between_the_phases_of_the_table() {
        // Off ratio one, output frames fall between the points of the table.
        for (format, floor) in [(Format::F32, -100.0), (Format::F64, -110.0)] {
            for quality in [Quality::High, Quality::Medium] {
                let residuals = purity(format, quality, 2, 1.0 + MAX_DRIFT * 0.37);
                for r in residuals {
                    assert!(r < floor, "{format:?} {quality:?}: {r} dBFS");
                }
            }
        }
    }

    /// Seconds of tone. Ten in an optimised build, as the plan says.
    const SECONDS: usize = if cfg!(debug_assertions) { 3 } else { 10 };

    /// Output frames over input frames once the filter is full, at `relative`.
    fn measured(relative: f64, seconds: usize) -> (usize, usize) {
        let mut a = Adaptive::new(RATE, RATE, 2, PERIOD as u32, Quality::High, Format::F32);
        a.set_ratio(relative);
        let input = tone(RATE as usize * seconds, 2, &[0, 1], 997.0);
        let (_, consumed, produced) = run(&mut a, &input, 2);
        (consumed, produced)
    }

    #[test]
    fn the_ratio_moves_the_output_rate() {
        let (consumed, produced) = measured(1.001, 10);
        let expected = consumed as f64 * 1.001;
        assert!(
            (produced as f64 - expected).abs() <= 2.0,
            "{consumed} in, {produced} out, {expected} expected"
        );
        assert!(consumed > 479_000, "{consumed}");
    }

    #[test]
    fn the_ratio_is_clamped_at_max_drift() {
        for (asked, held) in [(1.01, 1.0 + MAX_DRIFT), (0.5, 1.0 - MAX_DRIFT)] {
            let (consumed, produced) = measured(asked, 2);
            let expected = consumed as f64 * held;
            assert!(
                (produced as f64 - expected).abs() <= 2.0,
                "{asked}: {consumed} in, {produced} out, {expected} expected"
            );
        }
        // Not a number holds the nominal ratio rather than failing.
        let (consumed, produced) = measured(f64::NAN, 1);
        assert!(consumed.abs_diff(produced) <= 2, "{consumed} {produced}");
    }

    #[test]
    fn eight_channels_stay_apart() {
        for format in [Format::F32, Format::F64] {
            let mut a = Adaptive::new(RATE, RATE, 8, PERIOD as u32, Quality::High, format);
            a.set_ratio(1.0 + MAX_DRIFT / 2.0);
            let input = tone(RATE as usize / 2, 8, &[4], 997.0);
            let (out, _, _) = run(&mut a, &input, 8);
            let loud = channel(&out, 8, 4, 0);
            assert!(loud.iter().any(|v| v.abs() > FULL / 2.0 * 0.99));
            for c in (0..8).filter(|&c| c != 4) {
                let samples = channel(&out, 8, c, 0);
                let rms = (samples.iter().map(|v| v * v).sum::<f64>() / samples.len() as f64)
                    .sqrt()
                    .max(f64::MIN_POSITIVE);
                let db = 20.0 * (rms / (FULL / 2f64.sqrt())).log10();
                assert!(db < -140.0, "{format:?} channel {}: {db} dBFS", c + 1);
            }
        }
    }

    #[test]
    fn process_does_not_allocate() {
        for format in [Format::I32, Format::F32, Format::F64] {
            for quality in [Quality::High, Quality::Medium] {
                let mut a = Adaptive::new(44100, RATE, 6, PERIOD as u32, quality, format);
                let input = tone(20_000, 6, &[0, 1, 5], 997.0);
                let mut out = vec![0; PERIOD * 6];
                assert_no_alloc(|| {
                    let mut at = 0;
                    for i in 0..40 {
                        a.set_ratio(1.0 + (i as f64 - 20.0) * 1e-4);
                        // Some calls ask for less than a full period.
                        let frames = if i % 7 == 3 { PERIOD / 2 } else { PERIOD };
                        let need = a.input_needed(frames as u32) as usize;
                        assert!(a.delay_frames() > 0.0);
                        let (c, p) = a.process(&input[at..], &mut out[..frames * 6]);
                        assert_eq!(p, frames);
                        // Exact for the size of the last call, at most a
                        // frame over for another.
                        assert!(c == need || c + 1 == need);
                        at += c * 6;
                    }
                    // Too little input: nothing happens.
                    assert_eq!(a.process(&input[..6], &mut out), (0, 0));
                    a.reset();
                    assert_eq!(a.process(&input, &mut out).1, PERIOD);
                });
            }
        }
    }

    #[test]
    fn input_needed_is_what_process_takes() {
        let mut a = Adaptive::new(44100, RATE, 2, PERIOD as u32, Quality::High, Format::F32);
        let input = tone(40_000, 2, &[0, 1], 997.0);
        let mut out = vec![0; PERIOD * 2];
        let mut at = 0;
        for i in 0..100 {
            a.set_ratio(1.0 + ((i % 9) as f64 - 4.0) * 4e-4);
            let need = a.input_needed(PERIOD as u32) as usize;
            // A shorter call never takes more than the period's estimate
            // scaled down, plus a frame.
            let half = a.input_needed(PERIOD as u32 / 2) as usize;
            assert!(half <= need, "{half} {need}");
            let (c, p) = a.process(&input[at..], &mut out);
            assert_eq!((c, p), (need, PERIOD), "call {i}");
            at += c * 2;
        }
        // Asked for more than `max_out`, it plans for `max_out`.
        assert_eq!(
            a.input_needed(PERIOD as u32 * 4),
            a.input_needed(PERIOD as u32)
        );
    }

    #[test]
    fn the_delay_is_where_a_step_lands() {
        for (from, to) in [(RATE, RATE), (44100, RATE), (RATE, 44100), (96000, RATE)] {
            for format in [Format::F32, Format::F64] {
                for quality in [Quality::High, Quality::Medium] {
                    let mut a = Adaptive::new(from, to, 1, PERIOD as u32, quality, format);
                    let at = 1000;
                    let mut input = vec![0; 4000];
                    input[at..].fill(1 << 30);
                    let (out, _, _) = run(&mut a, &input, 1);
                    // Where the output passes half the step, which lies
                    // halfway between the last input frame of 0 and the first
                    // of full height.
                    let half = out.iter().position(|&v| v >= 1 << 29).unwrap();
                    let before = f64::from(out[half - 1]);
                    let after = f64::from(out[half]);
                    let crossing =
                        (half - 1) as f64 + (f64::from(1 << 29) - before) / (after - before);
                    let step = (at as f64 - 0.5) * f64::from(to) / f64::from(from);
                    let delay = crossing - step;
                    assert!(
                        (delay - a.delay_frames()).abs() < 0.1,
                        "{from}>{to} {format:?} {quality:?}: step at {delay}, said {}",
                        a.delay_frames()
                    );
                }
            }
        }
    }

    #[test]
    fn integer_samples_round_trip_by_a_power_of_two() {
        let extremes = [i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX - 1, i32::MAX];
        for v in extremes {
            // f64 holds every S32 exactly, and scaling by 2^31 is exact.
            let x = to_float::<F64>(v);
            assert_eq!(x, f64::from(v) / FULL);
            assert_eq!(F64::finish(x), v);
        }
        assert_eq!(to_float::<F32>(i32::MIN), -1.0);
        assert_eq!(F32::finish(to_float::<F32>(i32::MIN)), i32::MIN);
        // f32 rounds the largest one up to 1.0, which saturates back.
        assert_eq!(F32::finish(to_float::<F32>(i32::MAX)), i32::MAX);
        assert_eq!(F32::finish(to_float::<F32>(1 << 8)), 1 << 8);

        // Full scale through the filter: the overshoot of the step
        // saturates and never wraps to the other sign.
        for format in [Format::I32, Format::F32, Format::F64] {
            let mut a = Adaptive::new(RATE, RATE, 2, PERIOD as u32, Quality::High, format);
            let mut input = vec![0; 2 * 4000];
            for frame in input.chunks_exact_mut(2).skip(1000) {
                frame.copy_from_slice(&[i32::MAX, i32::MIN]);
            }
            let (out, _, _) = run(&mut a, &input, 2);
            let delay = a.delay_frames() as usize;
            for (i, frame) in out.chunks_exact(2).enumerate().skip(1000 + delay + 1) {
                assert!(frame[0] > 0 && frame[1] < 0, "{format:?} {i}: {frame:?}");
            }
            assert!(out.chunks_exact(2).any(|f| f[0] == i32::MAX), "{format:?}");
            assert!(out.chunks_exact(2).any(|f| f[1] == i32::MIN), "{format:?}");
        }
    }
}
