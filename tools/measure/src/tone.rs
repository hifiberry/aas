//! Finding breaks in a captured steady sine.
//!
//! A sine obeys `x[n] = 2cos(w) x[n-1] - x[n-2]`. What is left when that is
//! taken off is the noise, until the tone is cut, repeated or shifted: then
//! the residual jumps far above it. The samples are differenced first, which
//! removes a DC offset from the converter and leaves a sine of the same
//! frequency. A tone that fades to silence without a jump is found by its
//! level instead.

use std::collections::VecDeque;
use std::f64::consts::TAU;

/// The threshold is this many times the largest residual of the calibration.
pub const THRESHOLD_FACTOR: f64 = 8.0;
/// A level below this part of the calibrated one counts as a drop.
const LEVEL_DROP: f64 = 0.5;
/// The level is the mean over this many samples. Short, so that the jump
/// at the start of a silence has left it soon.
const LEVEL_WINDOW: usize = 16;
/// The threshold is never below this part of the differenced tone, so that a
/// capture with no noise at all still has one.
const MIN_THRESHOLD: f64 = 1e-3;
/// A calibration whose residual peaks above this part of the differenced
/// tone is not a clean tone.
const MAX_CALIBRATION_RESIDUAL: f64 = 0.1;
/// The phase before a break is taken this many samples before its first
/// flagged sample, ahead of any ringing of the converter's filter.
const PHASE_LOOKBACK: usize = 48;
/// The phase is averaged over this many samples, against the noise.
const PHASE_SAMPLES: usize = 32;

/// What a clean stretch of the tone looks like.
#[derive(Debug, Clone, Copy)]
pub struct Calibration {
    /// `2cos(w)`, fitted to the capture.
    pub k: f64,
    /// Peak amplitude of the tone, 1.0 is full scale.
    pub amplitude: f64,
    /// RMS of the residual.
    pub noise_rms: f64,
    /// Largest residual.
    pub noise_peak: f64,
    /// A residual above this marks a break.
    pub threshold: f64,
}

impl Calibration {
    /// Radians per sample.
    pub fn omega(&self) -> f64 {
        (self.k / 2.0).acos()
    }

    pub fn frequency(&self, rate: f64) -> f64 {
        self.omega() / TAU * rate
    }

    /// Peak amplitude of the differenced tone.
    fn diff_amplitude(&self) -> f64 {
        self.amplitude * 2.0 * (self.omega() / 2.0).sin()
    }
}

fn differences(samples: &[f64]) -> impl Iterator<Item = f64> + '_ {
    samples.windows(2).map(|w| w[1] - w[0])
}

/// Fits the tone to `samples`, which must be a clean stretch of it.
pub fn calibrate(samples: &[f64], threshold_factor: f64) -> Result<Calibration, String> {
    if samples.len() < 64 {
        return Err("too few samples to calibrate".to_string());
    }
    // The frequency is fitted to the samples themselves, less their mean.
    // Differencing first would raise the noise, which pulls the fit down.
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let (mut num, mut den) = (0.0, 0.0);
    for w in samples.windows(3) {
        num += (w[1] - mean) * (w[2] + w[0] - 2.0 * mean);
        den += (w[1] - mean) * (w[1] - mean);
    }
    if den <= 0.0 {
        return Err("no signal: the capture is silent".to_string());
    }
    let k = num / den;
    if !(-1.99..1.9999).contains(&k) {
        return Err(format!("no tone found: 2cos(w) fitted as {k:.5}"));
    }
    let amplitude = (2.0 * den / (samples.len() - 2) as f64).sqrt();
    let y: Vec<f64> = differences(samples).collect();
    let (mut sum, mut peak) = (0.0f64, 0.0f64);
    for w in y.windows(3) {
        let r = w[2] - k * w[1] + w[0];
        sum += r * r;
        peak = peak.max(r.abs());
    }
    let noise_rms = (sum / (y.len() - 2) as f64).sqrt();
    let mut cal = Calibration {
        k,
        amplitude,
        noise_rms,
        noise_peak: peak,
        threshold: 0.0,
    };
    let diff_amplitude = cal.diff_amplitude();
    if peak > MAX_CALIBRATION_RESIDUAL * diff_amplitude {
        return Err(format!(
            "the calibration stretch is not a clean tone: residual peak {:.1} dB under it",
            -db(peak, amplitude)
        ));
    }
    cal.threshold = (threshold_factor * peak).max(MIN_THRESHOLD * diff_amplitude);
    Ok(cal)
}

/// One break in the tone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Break {
    /// Index of the first sample out of line, counted from the first
    /// sample pushed.
    pub start: u64,
    /// From the first sample out of line to the last.
    pub frames: u64,
    /// Largest residual in the break, 1.0 is full scale.
    pub peak_residual: f64,
    /// Lowest level in the break, as a part of the calibrated level.
    pub min_level: f64,
    /// How much later the tone runs after the break than before it, in
    /// frames, known only modulo the tone's period. Positive when frames
    /// were inserted, negative when frames were lost.
    pub shift: f64,
}

struct Open {
    start: u64,
    last: u64,
    peak_residual: f64,
    min_level: f64,
    phase_before: Option<(u64, f64)>,
}

/// Looks at the capture sample by sample and reports each break once the
/// tone has been clean again for `merge` samples.
pub struct Detector {
    cal: Calibration,
    merge: u64,
    n: u64,
    /// Samples seen since the start or the last reset.
    seen: u64,
    x1: f64,
    y1: f64,
    y2: f64,
    /// The square of the differenced tone's amplitude at the last samples.
    levels: [f64; LEVEL_WINDOW],
    phases: VecDeque<(u64, f64)>,
    open: Option<Open>,
}

impl Detector {
    pub fn new(cal: Calibration, merge: usize) -> Detector {
        let full = cal.diff_amplitude().powi(2);
        Detector {
            cal,
            merge: merge.max(1) as u64,
            n: 0,
            seen: 0,
            x1: 0.0,
            y1: 0.0,
            y2: 0.0,
            levels: [full; LEVEL_WINDOW],
            phases: VecDeque::with_capacity(PHASE_LOOKBACK + PHASE_SAMPLES + 1),
            open: None,
        }
    }

    /// Index of the next sample pushed.
    pub fn position(&self) -> u64 {
        self.n
    }

    /// Forgets the samples before, after a hole in the capture itself. The
    /// hole is not a break. A break open at that point is returned.
    pub fn reset(&mut self) -> Option<Break> {
        let open = self.open.take().map(|o| self.close(o, None));
        self.seen = 0;
        self.levels = [self.cal.diff_amplitude().powi(2); LEVEL_WINDOW];
        self.phases.clear();
        open
    }

    /// The end of the capture. A break still open is returned.
    pub fn finish(&mut self) -> Option<Break> {
        self.open.take().map(|o| self.close(o, None))
    }

    /// The tone's phase at the first of `points`, averaged over all of them.
    fn mean_phase(&self, points: impl Iterator<Item = (u64, f64)>) -> Option<(u64, f64)> {
        let omega = self.cal.omega();
        let mut first = None;
        let (mut sum, mut count) = (0.0, 0.0);
        for (n, phase) in points {
            let (n0, p0) = *first.get_or_insert((n, phase));
            let off = phase - p0 - omega * (n - n0) as f64;
            sum += off - TAU * (off / TAU).round();
            count += 1.0;
        }
        first.map(|(n0, p0)| (n0, p0 + sum / count))
    }

    fn close(&self, open: Open, phase_after: Option<(u64, f64)>) -> Break {
        let omega = self.cal.omega();
        let shift = match (open.phase_before, phase_after) {
            (Some((n0, p0)), Some((n1, p1))) => {
                let turned = p1 - p0 - omega * (n1 - n0) as f64;
                let wrapped = turned - TAU * (turned / TAU).round();
                -wrapped / omega
            }
            _ => f64::NAN,
        };
        Break {
            start: open.start,
            frames: open.last - open.start + 1,
            peak_residual: open.peak_residual,
            min_level: open.min_level,
            shift,
        }
    }

    pub fn push(&mut self, x: f64) -> Option<Break> {
        let n = self.n;
        self.n += 1;
        self.seen += 1;
        let y = x - self.x1;
        self.x1 = x;
        let (y1, y2) = (self.y1, self.y2);
        self.y2 = y1;
        self.y1 = y;
        // The first sample has no difference, the next two no residual.
        if self.seen < 4 {
            return None;
        }
        let k = self.cal.k;
        let residual = (y - k * y1 + y2).abs();
        let sin2 = 1.0 - k * k / 4.0;
        let amplitude2 = (y * y + y1 * y1 - k * y * y1) / sin2;
        self.levels[(n % LEVEL_WINDOW as u64) as usize] = amplitude2;
        let mean2 = self.levels.iter().sum::<f64>() / LEVEL_WINDOW as f64;
        let level = mean2.max(0.0).sqrt() / self.cal.diff_amplitude();
        let phase = y.atan2((y * k / 2.0 - y1) / sin2.sqrt());
        let flagged = residual > self.cal.threshold || level < LEVEL_DROP;
        let mut done = None;
        match &mut self.open {
            Some(open) => {
                open.peak_residual = open.peak_residual.max(residual);
                open.min_level = open.min_level.min(level);
                if flagged {
                    open.last = n;
                } else if n - open.last >= self.merge {
                    let open = self.open.take().expect("checked above");
                    let clean = (self.merge as usize - 1).min(PHASE_SAMPLES - 1);
                    let from = self.phases.len().saturating_sub(clean);
                    let after = self.phases.iter().copied().skip(from);
                    let after = self.mean_phase(after.chain([(n, phase)]));
                    done = Some(self.close(open, after));
                }
            }
            None if flagged => {
                self.open = Some(Open {
                    start: n,
                    last: n,
                    peak_residual: residual,
                    min_level: level,
                    phase_before: self.mean_phase(self.phases.iter().copied().take(PHASE_SAMPLES)),
                });
            }
            None => {}
        }
        if self.phases.len() >= PHASE_LOOKBACK + PHASE_SAMPLES {
            self.phases.pop_front();
        }
        self.phases.push_back((n, phase));
        done
    }
}

/// Maps a frame of the capture to the clock. Each read of the capture gives
/// the clock and how many frames the card had captured by then. The card's
/// count can only lag, so the earliest start those give is the nearest.
pub struct FrameClock {
    rate: f64,
    starts: VecDeque<f64>,
}

/// Reads kept. Long enough to hold a prompt one, short enough that the
/// card's clock does not drift against the system's in that time.
const CLOCK_WINDOW: usize = 64;

impl FrameClock {
    pub fn new(rate: f64) -> FrameClock {
        FrameClock {
            rate,
            starts: VecDeque::with_capacity(CLOCK_WINDOW + 1),
        }
    }

    /// At `now` seconds the card had captured `frames` frames.
    pub fn note(&mut self, frames: u64, now: f64) {
        if self.starts.len() >= CLOCK_WINDOW {
            self.starts.pop_front();
        }
        self.starts.push_back(now - frames as f64 / self.rate);
    }

    /// After a hole in the capture the frames count from a new start.
    pub fn reset(&mut self) {
        self.starts.clear();
    }

    /// When `frame` was captured, in seconds on the clock given to `note`.
    pub fn time_of(&self, frame: u64) -> Option<f64> {
        let start = self.starts.iter().copied().reduce(f64::min)?;
        Some(start + frame as f64 / self.rate)
    }
}

/// `value` as dB relative to `reference`.
pub fn db(value: f64, reference: f64) -> f64 {
    20.0 * (value / reference).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;
    const FREQ: f64 = 1100.0;
    const AMPLITUDE: f64 = 0.5;
    const DC: f64 = 0.01;
    /// About -80 dBFS RMS, a poor converter.
    const NOISE: f64 = 1e-4;
    const MERGE: usize = 240;
    const CALIBRATION: usize = 48_000;

    /// Uniform noise in -1..1 from a fixed seed.
    struct Noise(u64);

    impl Noise {
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        }
    }

    /// Sample `i` of the tone, with no noise.
    fn tone_at(i: f64) -> f64 {
        DC + AMPLITUDE * (TAU * FREQ / RATE * i).sin()
    }

    fn tone(range: std::ops::Range<usize>) -> Vec<f64> {
        range.map(|i| tone_at(i as f64)).collect()
    }

    fn noisy(mut samples: Vec<f64>) -> Vec<f64> {
        let mut noise = Noise(7);
        // The sum of three uniform values is close to normal.
        for s in &mut samples {
            *s += NOISE * (noise.next() + noise.next() + noise.next());
        }
        samples
    }

    /// Calibrates on the first second and returns the breaks after it,
    /// with positions counted from the start of `samples`.
    fn breaks(samples: &[f64]) -> Vec<Break> {
        let cal = calibrate(&samples[..CALIBRATION], THRESHOLD_FACTOR).unwrap();
        let mut detector = Detector::new(cal, MERGE);
        let mut found = Vec::new();
        for s in &samples[CALIBRATION..] {
            found.extend(detector.push(*s));
        }
        found.extend(detector.finish());
        for b in &mut found {
            b.start += CALIBRATION as u64;
        }
        found
    }

    fn assert_near(got: f64, want: f64, slack: f64, what: &str) {
        assert!(
            (got - want).abs() <= slack,
            "{what}: got {got}, want {want}"
        );
    }

    /// The shift a detector can see for `frames` inserted: modulo the period.
    fn seen_shift(frames: f64) -> f64 {
        let period = RATE / FREQ;
        frames - period * (frames / period).round()
    }

    #[test]
    fn calibration_finds_the_tone_and_its_level() {
        let cal = calibrate(&noisy(tone(0..CALIBRATION)), THRESHOLD_FACTOR).unwrap();
        assert_near(cal.frequency(RATE), FREQ, 0.5, "frequency");
        assert_near(cal.amplitude, AMPLITUDE, 0.005, "amplitude");
        assert!(cal.noise_rms > 0.0 && cal.noise_rms < 20.0 * NOISE);
        assert!(cal.threshold < 0.05 * AMPLITUDE, "{}", cal.threshold);
    }

    #[test]
    fn silence_and_noise_do_not_calibrate() {
        assert!(calibrate(&vec![0.0; CALIBRATION], THRESHOLD_FACTOR).is_err());
        let mut noise = Noise(3);
        let hiss: Vec<f64> = (0..CALIBRATION).map(|_| noise.next() * 0.1).collect();
        assert!(calibrate(&hiss, THRESHOLD_FACTOR).is_err());
    }

    #[test]
    fn a_calibration_stretch_with_a_break_is_refused() {
        let mut samples = tone(0..CALIBRATION);
        samples[20_000..20_050].fill(0.0);
        assert!(calibrate(&samples, THRESHOLD_FACTOR).is_err());
    }

    #[test]
    fn a_clean_tone_has_no_break() {
        assert_eq!(breaks(&tone(0..10 * 48_000)), vec![]);
    }

    #[test]
    fn noise_at_a_realistic_level_gives_no_break() {
        assert_eq!(breaks(&noisy(tone(0..60 * 48_000))), vec![]);
    }

    #[test]
    fn inserted_silence_is_one_break_where_it_was_put() {
        for zeros in [1usize, 5, 64, 128, 1000, 10_000] {
            let at = 100_003;
            let mut samples = tone(0..at);
            samples.extend(std::iter::repeat_n(0.0, zeros));
            samples.extend(tone(at..200_000));
            let found = breaks(&noisy(samples));
            assert_eq!(found.len(), 1, "{zeros} zeros: {found:?}");
            let b = found[0];
            assert_near(b.start as f64, at as f64, 3.0, "start");
            assert_near(b.frames as f64, zeros as f64, 12.0, "length");
            assert_near(b.shift, seen_shift(zeros as f64), 0.1, "shift");
            if zeros >= 64 {
                assert!(b.min_level < 0.1, "level {}", b.min_level);
            }
        }
    }

    #[test]
    fn removed_samples_are_one_break_where_they_were() {
        for removed in [1usize, 7, 64, 128, 500] {
            let at = 123_457;
            let mut samples = tone(0..at);
            samples.extend(tone(at + removed..250_000));
            let found = breaks(&noisy(samples));
            assert_eq!(found.len(), 1, "{removed} removed: {found:?}");
            let b = found[0];
            assert_near(b.start as f64, at as f64, 3.0, "start");
            assert!(b.frames <= 6, "length {}", b.frames);
            assert_near(b.shift, seen_shift(-(removed as f64)), 0.1, "shift");
        }
    }

    #[test]
    fn a_phase_jump_is_one_break() {
        let at = 99_999;
        let mut samples = tone(0..at);
        samples.extend((at..200_000).map(|i| tone_at(i as f64 + 10.5)));
        let found = breaks(&noisy(samples));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_near(found[0].start as f64, at as f64, 3.0, "start");
        assert_near(found[0].shift, -10.5, 0.1, "shift");
    }

    #[test]
    fn a_repeated_stretch_is_one_break() {
        // The last 128 frames again, then the stream goes on. The tone
        // jumps back once and is whole after that.
        let at = 150_000;
        let mut samples = tone(0..at);
        samples.extend(tone(at - 128..at));
        samples.extend(tone(at..250_000));
        let found = breaks(&noisy(samples));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_near(found[0].start as f64, at as f64, 3.0, "start");
        assert!(found[0].frames <= 6, "length {}", found[0].frames);
        assert_near(found[0].shift, seen_shift(128.0), 0.1, "shift");
    }

    #[test]
    fn two_breaks_far_apart_are_two_and_close_together_one() {
        let mut samples = tone(0..300_000);
        samples[100_000..100_020].fill(0.0);
        samples[100_100..100_120].fill(0.0);
        samples[200_000..200_020].fill(0.0);
        let found = breaks(&noisy(samples));
        assert_eq!(found.len(), 2, "{found:?}");
        assert_near(found[0].frames as f64, 120.0, 6.0, "merged length");
        assert_near(found[1].start as f64, 200_000.0, 3.0, "second start");
    }

    #[test]
    fn a_fade_to_silence_without_a_jump_is_a_break() {
        let mut samples = tone(0..300_000);
        for (i, s) in samples.iter_mut().enumerate().skip(100_000) {
            let gain = match i {
                100_000..110_000 => 1.0 - (i - 100_000) as f64 / 10_000.0,
                110_000..120_000 => 0.0,
                120_000..130_000 => (i - 120_000) as f64 / 10_000.0,
                _ => 1.0,
            };
            *s = DC + (*s - DC) * gain;
        }
        let found = breaks(&noisy(samples));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_near(found[0].start as f64, 105_000.0, 200.0, "start");
        assert_near(found[0].frames as f64, 20_000.0, 400.0, "length");
    }

    #[test]
    fn a_hole_in_the_capture_is_not_a_break() {
        let samples = noisy(tone(0..200_000));
        let cal = calibrate(&samples[..CALIBRATION], THRESHOLD_FACTOR).unwrap();
        let mut detector = Detector::new(cal, MERGE);
        let mut found = Vec::new();
        for s in &samples[CALIBRATION..100_000] {
            found.extend(detector.push(*s));
        }
        // The capture lost 777 frames here.
        found.extend(detector.reset());
        for s in &samples[100_777..] {
            found.extend(detector.push(*s));
        }
        found.extend(detector.finish());
        assert_eq!(found, vec![]);
    }

    #[test]
    fn a_break_open_at_the_end_is_reported() {
        let mut samples = tone(0..100_000);
        samples[99_950..].fill(0.0);
        let found = breaks(&samples);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].shift.is_nan());
    }

    #[test]
    fn the_frame_clock_takes_the_most_prompt_read() {
        let mut clock = FrameClock::new(RATE);
        assert_eq!(clock.time_of(0), None);
        // The capture started at 100 s. The reads come 3, 0.2 and 5 ms late.
        clock.note(480, 100.013);
        clock.note(960, 100.0202);
        clock.note(1440, 100.035);
        assert_near(clock.time_of(48_000).unwrap(), 101.0002, 1e-9, "time");
        clock.reset();
        assert_eq!(clock.time_of(0), None);
    }
}
