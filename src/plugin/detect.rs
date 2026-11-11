//! Detect: tells whether the analog input carries signal.
//!
//! The level is the RMS of both channels together over windows of 50 ms. A
//! window above the threshold opens the detector. It closes when no window
//! has been above the threshold for the hold time. Opening and closing
//! fade over 10 ms, so they do not click: the level rises from the first
//! frame after the window that opens it, and falls from the frame at which
//! it closes. Once closed and faded out the samples are overwritten with
//! exact zeros. A block reports `no_signal` when nothing of it is heard. While open and faded in
//! the samples are left exactly as they are.
//!
//! A hold of zero closes at the end of the first window that is not above
//! the threshold. A NaN sample makes its own window not above the
//! threshold and leaves no trace in the next. An infinite sample is above
//! any threshold.

use super::{Block, Formats, Param, ParamKind, Plugin, Status};

/// Full scale of an `i32` sample.
const I32_FULL: f64 = 2_147_483_648.0;

/// How long opening or closing fades, as the engine's volume fades.
const FADE_MS: u32 = 10;

pub struct Detect {
    params: [Param; 2],
    /// dBFS.
    threshold: f64,
    /// Frames, at the plugin's rate.
    hold_frames: u64,
    rate: u32,
    /// Frames in one window.
    window: u32,
    /// Frames of the window so far, and their sum of squares at full
    /// scale 1.0. Kept across calls.
    seen: u32,
    sum: f64,
    open: bool,
    /// Frames since the end of the last window above the threshold.
    quiet: u64,
    /// Frames one fade lasts.
    fade: u32,
    /// Where the fade stands: 0 sends zeros, `fade` leaves the samples as
    /// they are. It moves one step per frame towards `open`.
    level: u32,
}

impl Detect {
    pub fn new(rate: u32) -> Detect {
        let params = [
            Param {
                name: "threshold".into(),
                unit: "dBFS".into(),
                kind: ParamKind::Float,
                min: -90.0,
                max: 0.0,
                default: -50.0,
            },
            Param {
                name: "hold".into(),
                unit: "s".into(),
                kind: ParamKind::Float,
                min: 0.0,
                max: 60.0,
                default: 3.0,
            },
        ];
        let mut d = Detect {
            params,
            threshold: -50.0,
            hold_frames: 0,
            rate,
            window: (rate / 20).max(1),
            seen: 0,
            sum: 0.0,
            open: false,
            quiet: 0,
            fade: (rate * FADE_MS / 1000).max(1),
            level: 0,
        };
        d.set_param(1, 3.0);
        d
    }

    /// True while the input carries signal.
    pub fn open(&self) -> bool {
        self.open
    }

    /// Runs the detector over `data`, fading it in and out and zeroing it
    /// while closed. `level` gives a sample at full scale 1.0, and `scale`
    /// multiplies a sample by a gain below 1. Returns the status.
    fn run<T: Copy>(
        &mut self,
        data: &mut [[T; 2]],
        level: impl Fn(T) -> f64,
        scale: impl Fn(T, f64) -> T,
        zero: T,
    ) -> Status {
        let mut changed = false;
        let mut heard = false;
        for frame in data {
            let (l, r) = (level(frame[0]), level(frame[1]));
            self.sum += l * l + r * r;
            if self.open {
                self.level = (self.level + 1).min(self.fade);
            } else {
                self.level = self.level.saturating_sub(1);
            }
            heard |= self.level > 0;
            if self.level == 0 {
                *frame = [zero, zero];
                changed = true;
            } else if self.level < self.fade {
                let gain = f64::from(self.level) / f64::from(self.fade);
                *frame = [scale(frame[0], gain), scale(frame[1], gain)];
                changed = true;
            }
            self.seen += 1;
            let mut loud = false;
            let ended = self.seen == self.window;
            if ended {
                let mean = self.sum / (2.0 * f64::from(self.window));
                loud = 10.0 * mean.log10() > self.threshold;
                self.seen = 0;
                self.sum = 0.0;
            }
            if loud {
                self.open = true;
                self.quiet = 0;
            } else if self.open {
                if self.hold_frames == 0 {
                    // No hold: the first window not above the threshold closes.
                    self.open = !ended;
                } else {
                    self.quiet += 1;
                    if self.quiet >= self.hold_frames {
                        self.open = false;
                    }
                }
            }
        }
        // Signal until the fade out is over, so that the engine plays it.
        Status {
            no_signal: !self.open && !heard,
            changed,
        }
    }
}

impl Plugin for Detect {
    fn name(&self) -> &str {
        "detect"
    }

    fn formats(&self) -> Formats {
        Formats {
            i32: true,
            f32: true,
            f64: true,
        }
    }

    fn params(&self) -> &[Param] {
        &self.params
    }

    fn set_param(&mut self, index: usize, value: f64) {
        if value.is_nan() {
            return;
        }
        match index {
            0 => self.threshold = value.clamp(-90.0, 0.0),
            1 => {
                let hold = value.clamp(0.0, 60.0);
                self.hold_frames = (hold * f64::from(self.rate)).round() as u64;
            }
            _ => {}
        }
    }

    fn neutral(&self) -> bool {
        true
    }

    fn latency_frames(&self) -> u32 {
        0
    }

    fn process(&mut self, block: Block<'_>) -> Status {
        match block {
            Block::I32(d) => self.run(
                d,
                |x| f64::from(x) / I32_FULL,
                |x, g| (f64::from(x) * g).round() as i32,
                0,
            ),
            Block::F32(d) => self.run(d, f64::from, |x, g| (f64::from(x) * g) as f32, 0.0),
            Block::F64(d) => self.run(d, |x| x, |x, g| x * g, 0.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_no_alloc::assert_no_alloc;

    const RATE: u32 = 48_000;

    /// A 1 kHz sine at `db` dBFS RMS, `frames` long, on both channels.
    fn tone(db: f64, frames: usize, rate: u32) -> Vec<[f64; 2]> {
        let amp = 10f64.powf((db + 3.0103) / 20.0);
        (0..frames)
            .map(|n| {
                let s =
                    amp * (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / f64::from(rate)).sin();
                [s, s]
            })
            .collect()
    }

    fn silence(frames: usize) -> Vec<[f64; 2]> {
        vec![[0.0; 2]; frames]
    }

    fn feed(d: &mut Detect, mut data: Vec<[f64; 2]>) -> Status {
        d.process(Block::F64(&mut data))
    }

    #[test]
    fn silence_stays_closed_and_reports_no_signal() {
        let mut d = Detect::new(RATE);
        for _ in 0..10 {
            assert!(feed(&mut d, silence(4800)).no_signal);
            assert!(!d.open());
        }
    }

    #[test]
    fn a_tone_above_the_threshold_opens_within_one_window() {
        let mut d = Detect::new(RATE);
        assert!(feed(&mut d, tone(-40.0, 2399, RATE)).no_signal);
        assert!(!d.open());
        let s = feed(&mut d, tone(-40.0, 1, RATE));
        assert!(d.open());
        assert!(!s.no_signal);
    }

    #[test]
    fn a_tone_below_the_threshold_stays_closed() {
        let mut d = Detect::new(RATE);
        assert!(feed(&mut d, tone(-51.0, 48_000, RATE)).no_signal);
        assert!(!d.open());
        // The same tone just above it opens.
        assert!(!feed(&mut d, tone(-49.0, 4800, RATE)).no_signal);
    }

    #[test]
    fn a_single_loud_sample_does_not_open_it() {
        let mut d = Detect::new(RATE);
        let mut data = silence(48_000);
        data[1000] = [0.1, 0.1];
        assert!(d.process(Block::F64(&mut data)).no_signal);
        assert!(!d.open());
    }

    #[test]
    fn a_level_that_dips_within_the_hold_time_stays_open() {
        let mut d = Detect::new(RATE);
        feed(&mut d, tone(-30.0, 4800, RATE));
        assert!(d.open());
        assert!(!feed(&mut d, silence(139_200)).no_signal); // 2.9 s
        // A tone again restarts the hold.
        feed(&mut d, tone(-30.0, 2400, RATE));
        assert!(!feed(&mut d, silence(139_200)).no_signal);
        assert!(d.open());
    }

    #[test]
    fn it_closes_when_the_hold_time_has_passed() {
        let mut d = Detect::new(RATE);
        feed(&mut d, tone(-30.0, 4800, RATE));
        // 3.1 s: closed and faded out, so the next block carries nothing.
        assert!(!feed(&mut d, silence(148_799)).no_signal);
        assert!(!d.open());
        assert!(feed(&mut d, silence(1)).no_signal);
        // Exactly at the hold time: open one frame before, closed at it.
        feed(&mut d, tone(-30.0, 2400, RATE));
        feed(&mut d, silence(143_999));
        assert!(d.open());
        feed(&mut d, silence(1));
        assert!(!d.open());
    }

    #[test]
    fn while_closed_the_output_is_exactly_zero() {
        let mut d = Detect::new(RATE);
        // Quiet noise-like data below the threshold, in each format.
        let mut i = vec![[1000i32, -1000]; 4800];
        d.process(Block::I32(&mut i));
        assert!(i.iter().all(|f| *f == [0, 0]));
        let mut f = vec![[1e-4f32, -1e-4]; 4800];
        d.process(Block::F32(&mut f));
        assert!(f.iter().all(|f| *f == [0.0, 0.0]));
        let mut x = vec![[1e-4f64, -1e-4]; 4800];
        d.process(Block::F64(&mut x));
        assert!(x.iter().all(|f| *f == [0.0, 0.0]));
        // The window that opens it is gone, the next 10 ms fade in, and
        // the samples pass from then on.
        let mut x = tone(-30.0, 4800, RATE);
        let loud = x.clone();
        d.process(Block::F64(&mut x));
        assert!(x[..2400].iter().all(|f| *f == [0.0, 0.0]));
        assert_eq!(x[2400 + 479..], loud[2400 + 479..]);
    }

    #[test]
    fn while_open_the_samples_are_unchanged() {
        let mut d = Detect::new(RATE);
        // Open, and faded in.
        let mut x = tone(-20.0, 2400 + 480, RATE);
        d.process(Block::F64(&mut x));
        assert!(d.open());
        let loud = tone(-20.0, 480, RATE);
        let mut x = loud.clone();
        d.process(Block::F64(&mut x));
        assert_eq!(x, loud);
        let mut f: Vec<[f32; 2]> = loud.iter().map(|f| f.map(|v| v as f32)).collect();
        let f0 = f.clone();
        d.process(Block::F32(&mut f));
        assert_eq!(f, f0);
        let mut i: Vec<[i32; 2]> = loud
            .iter()
            .map(|f| f.map(|v| (v * I32_FULL * 0.5) as i32))
            .collect();
        let i0 = i.clone();
        d.process(Block::I32(&mut i));
        assert_eq!(i, i0);
        assert!(d.neutral());
        assert_eq!(d.latency_frames(), 0);
    }

    #[test]
    fn an_i32_block_is_measured_against_its_own_full_scale() {
        let mut d = Detect::new(RATE);
        // -40 dBFS RMS as i32 opens it, -60 dBFS does not.
        let as_i32 = |db: f64| -> Vec<[i32; 2]> {
            tone(db, 2400, RATE)
                .iter()
                .map(|f| f.map(|v| (v * I32_FULL) as i32))
                .collect()
        };
        let mut q = as_i32(-60.0);
        assert!(d.process(Block::I32(&mut q)).no_signal);
        let mut l = as_i32(-40.0);
        d.process(Block::I32(&mut l));
        assert!(d.open());
    }

    #[test]
    fn the_window_follows_the_rate() {
        for rate in [44_100u32, 192_000] {
            let window = (rate / 20) as usize;
            let mut d = Detect::new(rate);
            feed(&mut d, tone(-30.0, window - 1, rate));
            assert!(!d.open(), "{rate}");
            feed(&mut d, tone(-30.0, 1, rate));
            assert!(d.open(), "{rate}");
            // The hold is 3 s of frames at this rate.
            feed(&mut d, silence(3 * rate as usize - 1));
            assert!(d.open(), "{rate}");
            feed(&mut d, silence(1));
            assert!(!d.open(), "{rate}");
        }
    }

    #[test]
    fn the_window_spans_calls_of_any_size() {
        let mut d = Detect::new(RATE);
        for chunk in tone(-30.0, 2400, RATE).chunks(7) {
            feed(&mut d, chunk.to_vec());
        }
        assert!(d.open());
    }

    #[test]
    fn a_new_threshold_applies_from_the_next_window() {
        let mut d = Detect::new(RATE);
        // -40 dBFS is below a threshold of -30.
        d.set_param(0, -30.0);
        feed(&mut d, tone(-40.0, 2399, RATE));
        // Lowering it mid-window counts for the window that ends next.
        d.set_param(0, -50.0);
        feed(&mut d, tone(-40.0, 1, RATE));
        assert!(d.open());
        // And back up: the next windows are measured against -30.
        d.set_param(0, -30.0);
        d.set_param(1, 0.1);
        feed(&mut d, tone(-40.0, 4800 * 2, RATE));
        assert!(!d.open());
    }

    #[test]
    fn a_hold_set_applies_at_once_and_values_are_clamped() {
        let mut d = Detect::new(RATE);
        feed(&mut d, tone(-30.0, 2400, RATE));
        d.set_param(1, 0.5);
        feed(&mut d, silence(23_999));
        assert!(d.open());
        feed(&mut d, silence(1));
        assert!(!d.open());
        d.set_param(0, 5.0);
        d.set_param(1, 500.0);
        d.set_param(9, 1.0);
        assert_eq!(d.threshold, 0.0);
        assert_eq!(d.hold_frames, 60 * 48_000);
        assert_eq!(d.params().len(), 2);
    }

    #[test]
    fn a_hold_of_zero_closes_at_the_end_of_the_first_window_not_above() {
        let mut d = Detect::new(RATE);
        d.set_param(1, 0.0);
        feed(&mut d, tone(-30.0, 2400, RATE));
        assert!(d.open());
        // Open through a window above the threshold, and into the next.
        feed(&mut d, tone(-30.0, 2400, RATE));
        assert!(d.open());
        feed(&mut d, silence(2399));
        assert!(d.open());
        feed(&mut d, silence(1));
        assert!(!d.open());
    }

    #[test]
    fn samples_after_a_close_in_the_middle_of_a_call_are_zero_in_that_call() {
        let mut d = Detect::new(RATE);
        let mut x = tone(-30.0, 2400, RATE);
        // Quiet, constant and below the threshold, for more than the hold.
        x.extend(vec![[1e-4, 1e-4]; 150_000]);
        d.process(Block::F64(&mut x));
        // The hold ends 144000 frames after the loud window, and the fade
        // out 10 ms later.
        let closed = 2400 + 144_000;
        assert!(x[2400 + 480..closed].iter().all(|f| *f == [1e-4, 1e-4]));
        assert!(
            x[closed..closed + 479]
                .iter()
                .all(|f| f[0] < 1e-4 && f[0] > 0.0)
        );
        assert!(x[closed + 479..].iter().all(|f| *f == [0.0, 0.0]));
        assert!(!d.open());
    }

    #[test]
    fn calls_of_one_frame_give_the_same_result_as_one_large_call() {
        let mut data = tone(-30.0, 4800, RATE);
        data.extend(vec![[1e-4, -1e-4]; 160_000]);
        data.extend(tone(-30.0, 4800, RATE));
        let mut whole = Detect::new(RATE);
        let mut a = data.clone();
        let status = whole.process(Block::F64(&mut a));
        let mut ones = Detect::new(RATE);
        let mut b = data.clone();
        for f in b.chunks_mut(1) {
            ones.process(Block::F64(f));
        }
        assert_eq!(a, b);
        assert_eq!(whole.open(), ones.open());
        assert!(!status.no_signal);
    }

    #[test]
    fn nan_poisons_only_its_own_window_and_infinity_counts_as_loud() {
        let mut d = Detect::new(RATE);
        let mut x = tone(-30.0, 2400, RATE);
        x[10] = [f64::NAN, 0.0];
        assert!(d.process(Block::F64(&mut x)).no_signal);
        assert!(!d.open());
        // The next window is clean and opens it.
        feed(&mut d, tone(-30.0, 2400, RATE));
        assert!(d.open());
        // A NaN while open does not close it by itself, and does not stick.
        let mut x = tone(-30.0, 2400, RATE);
        x[0] = [f64::NAN, f64::NAN];
        feed(&mut d, x);
        assert!(d.open());
        feed(&mut d, silence(144_000));
        assert!(!d.open());
        // Infinity is above any threshold, and the state recovers from it.
        let mut x = silence(2400);
        x[5] = [f64::INFINITY, 0.0];
        feed(&mut d, x);
        assert!(d.open());
        feed(&mut d, silence(144_000));
        assert!(!d.open());
    }

    /// The largest change from one frame to the next, on the left channel.
    fn largest_step(data: &[[f64; 2]]) -> f64 {
        data.windows(2)
            .map(|w| (w[1][0] - w[0][0]).abs())
            .fold(0.0, f64::max)
    }

    #[test]
    fn opening_fades_in_over_10_ms_at_any_rate() {
        for rate in [44_100u32, 48_000, 192_000] {
            let window = (rate / 20) as usize;
            let fade = (rate / 100) as usize;
            let mut d = Detect::new(rate);
            let mut x = vec![[0.1, 0.1]; window + 2 * fade];
            let status = d.process(Block::F64(&mut x));
            assert!(!status.no_signal && status.changed, "{rate}");
            // The window that opens it is gone. The first frame after it
            // starts near zero, and the level rises by equal steps.
            assert!(x[..window].iter().all(|f| *f == [0.0, 0.0]), "{rate}");
            let step = 0.1 / fade as f64;
            assert!((x[window][0] - step).abs() < 1e-12, "{rate}");
            assert!(largest_step(&x) <= step * (1.0 + 1e-9), "{rate}");
            // From 10 ms on, the samples are as they came.
            assert!(x[window + fade - 1..].iter().all(|f| *f == [0.1, 0.1]));
            // A block that is open all through is left as it is.
            let mut y = vec![[0.1, 0.1]; 100];
            let status = d.process(Block::F64(&mut y));
            assert!(!status.changed && !status.no_signal);
        }
    }

    #[test]
    fn closing_fades_out_over_10_ms_then_sends_zeros() {
        let mut d = Detect::new(RATE);
        d.set_param(0, -10.0);
        d.set_param(1, 0.0);
        // -6 dBFS opens it, -14 dBFS is below the threshold.
        feed(&mut d, vec![[0.5, 0.5]; 4800]);
        assert!(d.open());
        let mut x = vec![[0.2, 0.2]; 2400 + 480 + 100];
        let status = d.process(Block::F64(&mut x));
        assert!(!d.open());
        // The block still carried the fade.
        assert!(!status.no_signal && status.changed);
        // Open to the end of the window, then down by equal steps.
        assert!(x[..2400].iter().all(|f| *f == [0.2, 0.2]));
        assert!(largest_step(&x) <= 0.2 / 480.0 * (1.0 + 1e-9));
        assert!(x[2400 + 479..].iter().all(|f| *f == [0.0, 0.0]));
        // A block that ends while it fades still has signal.
        let mut d = Detect::new(RATE);
        d.set_param(0, -10.0);
        d.set_param(1, 0.0);
        feed(&mut d, vec![[0.5, 0.5]; 4800]);
        let status = feed(&mut d, vec![[0.2, 0.2]; 2400 + 100]);
        assert!(!d.open() && !status.no_signal && status.changed);
        let status = feed(&mut d, vec![[0.2, 0.2]; 380]);
        assert!(!status.no_signal);
        // Nothing of the next block is heard.
        let status = feed(&mut d, vec![[0.2, 0.2]; 1]);
        assert!(status.no_signal && status.changed);
    }

    #[test]
    fn a_fade_in_i32_has_no_step_larger_than_the_ramp() {
        let mut d = Detect::new(RATE);
        let mut x = vec![[1 << 28, -(1 << 28)]; 2400 + 960];
        d.process(Block::I32(&mut x));
        let step = (1 << 28) / 480 + 1;
        let largest = x.windows(2).map(|w| (w[1][0] - w[0][0]).abs()).max();
        assert!(largest.unwrap() <= step);
        assert_eq!(x[2400 + 479..], vec![[1 << 28, -(1 << 28)]; 481][..]);
    }

    #[test]
    fn processing_does_not_allocate() {
        let mut d = Detect::new(RATE);
        let mut x = tone(-30.0, 4800, RATE);
        let mut f = vec![[0.0f32; 2]; 4800];
        let mut i = vec![[0i32; 2]; 4800];
        assert_no_alloc(|| {
            d.process(Block::F64(&mut x));
            d.process(Block::F32(&mut f));
            d.process(Block::I32(&mut i));
            d.set_param(0, -40.0);
            d.set_param(1, 1.0);
        });
    }
}
