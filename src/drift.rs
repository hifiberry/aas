//! The drift controller of a further output: keeps the total delay of its
//! path at a fixed target by moving the ratio of its adaptive resampler.
//!
//! The path's delay is the frames in the feed ring, in the resampler and in
//! the card, counted at the master's rate. A card that runs fast drains the
//! path, the delay falls below the target, and the resampler must make more
//! output frames from each input frame: the ratio [`Drift::update`] returns
//! rises above 1. A slow card lowers it. That ratio goes to
//! `Adaptive::set_ratio` unchanged.
//!
//! It is a PI controller. The proportional part corrects an error over
//! [`PROPORTIONAL`] seconds, the integral over [`INTEGRAL`]. The two make an
//! overdamped loop: an error decays with a fast mode of about 2.3 s and a
//! slow one of about 18 s. A step of the delay comes back with a small
//! undershoot, as the integral has to give back what it took meanwhile:
//! 0.08 of a period for a step of one period, up to 0.32 of a period for a
//! step of four at short periods, where the clamp does not slow the return
//! (simulated without jitter from 32 to 1024 frames at 44.1 to 384 kHz).
//! Both time constants are set in seconds and turned into gains from the
//! rate and the period, so they hold at every rate.
//!
//! The delay is read with up to a period of jitter, as the master writes a
//! period at a time and the reading falls anywhere in its cycle. The
//! controller smooths it with a one-pole low-pass of [`SMOOTHING`] seconds
//! before using it, which keeps the jitter out of the ratio and out of the
//! lock decision while adding little lag to a loop ten times slower.
//!
//! The integral still wanders by a few ppm with that jitter: a slope read
//! over 20 s from readings a period apart cannot be told more closely. So
//! [`Drift::ppm`] is a separate, slower estimate. Over one period the delay
//! changes by `period × (ratio − 1 − offset)`, so each update yields the
//! offset as the ratio applied less the change of the delay over a period.
//! Those are averaged over everything since the start for the first
//! [`ESTIMATE`] seconds and exponentially over that time after; the changes
//! of the delay add up to the change over the window, so only the jitter
//! of its two ends is left, divided by the window. The delay seen there is
//! smoothed for [`ESTIMATE_SMOOTHING`] seconds, longer than for the ratio,
//! as lag does not matter to an average but jitter does. Its first reading
//! goes in unsmoothed, so the estimate starts only after five of those time
//! constants, when that reading's jitter has died away. The estimate is
//! right while the ratio sits at the clamp, too. A jump of the delay that
//! no clock made, a lost period, moves it by the jump over the window, about
//! 40 ppm for a period of 256 frames at 48 kHz, decaying over the window. An
//! error above a period is taken for such a jump, and the estimate starts
//! over once the error is back under it, unless the ratio has held the
//! clamp for longer than such a jump keeps it there: then the error grows
//! from a card the clamp cannot follow.
//!
//! A path primed onto its target again after a starve or an overflow jumps
//! too, by what was dropped or played as zeros. [`Drift::reprime`] knows
//! when: the delay the estimate sees is moved by the difference between the
//! readings on either side, so the jump stays out of it and the estimate
//! goes on, however often the path is primed.
//!
//! A reprime also ends the run of readings on target that a lock needs, so
//! the path counts as locked only after [`LOCK`] seconds without one. A
//! locked path keeps its lock over a single reprime, a starve now and then;
//! a second within [`REPRIMES`] seconds ends it. The run at the clamp is
//! kept over a reprime while the ratio comes back to the clamp, which a
//! card beyond it does within a second or two.
//!
//! A reading outside what a path can hold, below nothing or far above the
//! target, or one that is not finite, is skipped before it reaches any of
//! the state, as a single wrapped counter would otherwise pin the ratio for
//! half a minute and spoil the estimate for hours.

use crate::adaptive::MAX_DRIFT;

/// Seconds over which the proportional part corrects an error.
pub const PROPORTIONAL: f64 = 2.0;
/// Seconds over which the integral part corrects an error.
pub const INTEGRAL: f64 = 20.0;
/// Seconds of the low-pass on the measured delay.
pub const SMOOTHING: f64 = 0.5;
/// Seconds the error has to stay under a quarter period to count as locked.
pub const LOCK: f64 = 5.0;
/// Seconds the ratio has to stay at the clamp to count as clamped. Bringing
/// back a lost period, or four at 256 frames, holds it there for 10 s at
/// most.
pub const CLAMPED: f64 = 30.0;
/// Seconds the ratio stays at the clamp at most to bring back a jump of the
/// delay. Longer, a large error comes from the card's clock.
pub const RECOVERY: f64 = 10.0;
/// Seconds within which a second reprime ends a lock.
pub const REPRIMES: f64 = 10.0;
/// Seconds after a reprime in which the run at the clamp is kept while the
/// ratio is not back at it.
pub const HOLD: f64 = 5.0;
/// Seconds of readings in [`Drift::ppm`]'s window after which an estimate
/// beyond the clamp counts as clamped.
pub const BEYOND: f64 = 10.0;
/// Periods above the target beyond which a reading is not believed.
pub const SANE: f64 = 8.0;
/// Seconds of readings in a row not believed after which the controller
/// starts over.
pub const LOST: f64 = 1.0;
/// Seconds over which [`Drift::ppm`] averages.
pub const ESTIMATE: f64 = 120.0;
/// Seconds of the low-pass on the delay that [`Drift::ppm`] sees.
pub const ESTIMATE_SMOOTHING: f64 = 4.0;

/// Follows the master's clock for one further output.
pub struct Drift {
    target: f64,
    /// Ratio change per frame of error.
    proportional: f64,
    /// Integral change per frame of error and update.
    integral_gain: f64,
    /// The share of a new reading in the smoothed error.
    smoothing: f64,
    /// The share of a new reading in the error the estimate sees.
    estimate_smoothing: f64,
    /// The error under which the path counts as on target.
    window: f64,
    /// Updates the error has to stay inside `window` to lock.
    lock_updates: u32,
    /// Updates the ratio has to stay at the clamp to count as clamped.
    clamped_updates: u32,
    /// Updates at the clamp after which a large error is no jump.
    recovery_updates: u32,
    /// Updates within which a second reprime ends a lock.
    reprime_updates: u32,
    /// Updates a reprime keeps the run at the clamp for.
    hold_updates: u32,
    /// Updates in the estimate before it may say the card is beyond the
    /// clamp.
    beyond_updates: u32,
    /// Updates of readings in a row not believed before starting over.
    lost_updates: u32,
    /// The largest reading believed, frames.
    sane: f64,
    /// Updates averaged for the estimate, once that many have passed.
    estimate_updates: u32,
    /// Updates before the estimate starts.
    settle_updates: u32,
    /// The smoothed error, frames; `None` before the first reading.
    error: Option<f64>,
    /// The error the estimate sees, frames.
    slow: f64,
    /// The integral part of the ratio, less 1.
    integral: f64,
    /// Consecutive updates inside `window` since the last reprime, up to
    /// `lock_updates`.
    inside: u32,
    locked: bool,
    /// Updates since the last reprime, up to `reprime_updates`.
    since_reprime: u32,
    /// Consecutive updates at the clamp, up to `clamped_updates`.
    pinned: u32,
    /// Updates left in which the run at the clamp is kept after a reprime.
    held: u32,
    /// The estimate says the card runs beyond the clamp.
    beyond: bool,
    /// Periods since the last reading believed, less one.
    missed: u32,
    /// The last reading believed, less the target; `None` before the
    /// first or after a reset.
    last_raw: Option<f64>,
    /// Consecutive readings not believed.
    lost: u32,
    /// Readings not believed since the controller was made.
    skipped: u64,
    /// Updates since the start, up to `settle_updates`.
    settled: u32,
    /// Updates in the estimate, up to `estimate_updates`.
    updates: u32,
    /// The estimate of the card's offset, relative.
    mean: f64,
    /// The ratio last returned.
    ratio: f64,
    /// The period in frames at the master's rate.
    period: f64,
}

impl Drift {
    /// A controller holding the path at `target_frames`. `rate` is the
    /// master's rate, the one the delay is measured in; `period` is the
    /// writer's period converted to that rate, so that a period lasts
    /// `period / rate` seconds and a quarter period is in the delay's units.
    pub fn new(target_frames: f64, rate: u32, period: u32) -> Drift {
        let rate = f64::from(rate.max(1));
        let period = f64::from(period.max(1));
        let seconds = period / rate;
        let proportional = 1.0 / (rate * PROPORTIONAL);
        Drift {
            target: target_frames,
            proportional,
            integral_gain: proportional * seconds / INTEGRAL,
            smoothing: seconds / (SMOOTHING + seconds),
            estimate_smoothing: seconds / (ESTIMATE_SMOOTHING + seconds),
            window: period / 4.0,
            lock_updates: (LOCK / seconds).ceil() as u32,
            clamped_updates: (CLAMPED / seconds).ceil() as u32,
            recovery_updates: (RECOVERY / seconds).ceil() as u32,
            reprime_updates: (REPRIMES / seconds).ceil() as u32,
            hold_updates: (HOLD / seconds).ceil() as u32,
            beyond_updates: (BEYOND / seconds).ceil() as u32,
            lost_updates: (LOST / seconds).ceil() as u32,
            sane: target_frames + SANE * period,
            estimate_updates: (ESTIMATE / seconds).ceil() as u32,
            settle_updates: (5.0 * ESTIMATE_SMOOTHING / seconds).ceil() as u32,
            error: None,
            slow: 0.0,
            integral: 0.0,
            inside: 0,
            locked: false,
            since_reprime: (REPRIMES / seconds).ceil() as u32,
            pinned: 0,
            held: 0,
            beyond: false,
            missed: 0,
            last_raw: None,
            lost: 0,
            skipped: 0,
            settled: 0,
            updates: 0,
            mean: 0.0,
            ratio: 1.0,
            period,
        }
    }

    /// Takes the path's total delay at the master's rate, read once per
    /// writer period, and returns the ratio relative to the nominal one
    /// for `Adaptive::set_ratio`, within `1 ± MAX_DRIFT`.
    ///
    /// A reading that is not finite, below nothing or more than [`SANE`]
    /// periods above the target is skipped: it reaches none of the state,
    /// the last ratio is returned and [`Drift::skipped`] counts it. After
    /// [`LOST`] seconds of nothing but such readings the controller starts
    /// over as after [`Drift::reset`]. Does not allocate.
    pub fn update(&mut self, measured_frames: f64) -> f64 {
        if !(0.0..=self.sane).contains(&measured_frames) {
            self.skipped += 1;
            self.lost += 1;
            // The ratio stays where it was for this period, too.
            self.since_reprime = (self.since_reprime + 1).min(self.reprime_updates);
            if self.pinned > 0 {
                self.pinned = (self.pinned + 1).min(self.clamped_updates);
            }
            if self.lost >= self.lost_updates {
                self.reset();
            }
            return self.ratio;
        }
        self.lost = 0;
        self.since_reprime = (self.since_reprime + 1).min(self.reprime_updates);
        let periods = f64::from(self.missed + 1);
        self.missed = 0;
        let raw = measured_frames - self.target;
        let error = match self.error {
            Some(previous) => {
                let error = previous + self.smoothing * (raw - previous);
                let before = self.slow;
                self.slow += self.estimate_smoothing * (raw - self.slow);
                if self.settled < self.settle_updates {
                    self.settled += 1;
                } else {
                    // The ratio last returned has played for the periods
                    // since the last reading and moved the delay by this
                    // much.
                    let moved = (self.slow - before) / (periods * self.period);
                    let offset = (self.ratio - 1.0) - moved;
                    self.updates = (self.updates + 1).min(self.estimate_updates);
                    self.mean += (offset - self.mean) / f64::from(self.updates);
                    if self.updates >= self.beyond_updates {
                        // A little within the clamp before it ends, so
                        // that a card near it does not come and go.
                        let at = if self.beyond { 0.95 } else { 1.0 };
                        self.beyond = self.mean.abs() > at * MAX_DRIFT;
                    }
                }
                error
            }
            None => {
                match self.last_raw {
                    // Primed again: the delay the estimate sees moves by
                    // the jump, which stays out of the estimate.
                    Some(last) => self.slow += raw - last,
                    None => self.slow = raw,
                }
                raw
            }
        };
        self.error = Some(error);
        self.last_raw = Some(raw);
        // An error this large is a lost period or a refill, no clock: the
        // estimate starts over once the delay is back. Unless the ratio has
        // held the clamp for longer than a jump keeps it there.
        if error.abs() > self.period && self.pinned < self.recovery_updates {
            self.settled = 0;
            self.updates = 0;
        }

        // The integral stops while the ratio is at the clamp and the error
        // would push it further, so it does not wind up during a large
        // correction and swing past the target afterwards.
        let output = self.integral - self.proportional * error;
        let step = -self.integral_gain * error;
        let pushing = (output >= MAX_DRIFT && step > 0.0) || (output <= -MAX_DRIFT && step < 0.0);
        if !pushing {
            self.integral = (self.integral + step).clamp(-MAX_DRIFT, MAX_DRIFT);
        }
        let wanted = self.integral - self.proportional * error;
        let output = wanted.clamp(-MAX_DRIFT, MAX_DRIFT);
        self.ratio = 1.0 + output;

        if wanted.abs() >= MAX_DRIFT {
            self.pinned = (self.pinned + 1).min(self.clamped_updates);
            self.held = 0;
        } else if self.held > 0 {
            // Primed again from the clamp: the ratio is on its way back.
            self.held -= 1;
            self.pinned = (self.pinned + 1).min(self.clamped_updates);
        } else {
            self.pinned = 0;
        }

        if error.abs() < self.window {
            self.inside = (self.inside + 1).min(self.lock_updates);
            if self.inside >= self.lock_updates {
                self.locked = true;
            }
        } else {
            self.inside = 0;
            self.locked = false;
        }
        self.ratio
    }

    /// A period passed without a reading the controller may take, such as
    /// one taken while the master's push was overdue: the ratio stays, the
    /// time counts at the clamp and since the last reprime, as it does for
    /// a reading skipped, and the next reading's change of the delay is
    /// spread over the periods since the last. Does not allocate.
    pub fn pass(&mut self) {
        self.since_reprime = (self.since_reprime + 1).min(self.reprime_updates);
        self.missed = self.missed.saturating_add(1);
        if self.pinned > 0 {
            self.pinned = (self.pinned + 1).min(self.clamped_updates);
        }
    }

    /// The estimate of how far the card runs from the master, in parts per
    /// million: positive for a card that runs fast. It averages over the
    /// last [`ESTIMATE`] seconds, so it is right only once the card has run
    /// for about that long. While it starts over after a jump of the delay
    /// it keeps the last value.
    pub fn ppm(&self) -> f64 {
        self.mean * 1e6
    }

    /// Whether [`Drift::ppm`] has averaged over its whole window of
    /// [`ESTIMATE`] seconds since it last started.
    pub fn estimate_ready(&self) -> bool {
        self.updates >= self.estimate_updates
    }

    /// Whether the error has stayed under a quarter period for the last
    /// [`LOCK`] seconds without a reprime, and has since, with no two
    /// reprimes within [`REPRIMES`] seconds.
    pub fn locked(&self) -> bool {
        self.locked
    }

    /// Whether the card needs more than the clamp allows: the ratio has
    /// been held at `1 ± MAX_DRIFT` for the last [`CLAMPED`] seconds, or
    /// [`Drift::ppm`] has averaged [`BEYOND`] seconds and lies beyond it. A
    /// step of a period or a refill puts the ratio there for a while, too,
    /// but not that long, except four periods of 1024 frames, which take
    /// 43 s. A card beyond the clamp that starves every second or two keeps
    /// the error too small for the ratio to reach the clamp before each
    /// priming; its estimate says so.
    pub fn clamped(&self) -> bool {
        self.pinned >= self.clamped_updates || self.beyond
    }

    /// Readings skipped as not believable since the controller was made;
    /// [`Drift::reset`] keeps the count.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// The path was primed onto its target again after a starve, an
    /// overflow or an xrun, with the clocks as they were: forgets the
    /// smoothed error, whose next reading starts it afresh, and the run of
    /// readings not believed. Keeps the integral, so the ratio stays where
    /// the card needs it, and the estimate, which goes on averaging without
    /// the jump. The run of readings on target starts over; a lock is kept
    /// unless the last reprime came within [`REPRIMES`] seconds. A run at
    /// the clamp is kept for [`HOLD`] seconds while the ratio comes back.
    pub fn reprime(&mut self) {
        self.error = None;
        self.lost = 0;
        self.missed = 0;
        self.inside = 0;
        // Primed again before a reading: the same gap, such as a stall
        // that overflowed the feed.
        if self.since_reprime > 0 && self.since_reprime < self.reprime_updates {
            self.locked = false;
        }
        self.since_reprime = 0;
        if self.pinned > 0 {
            self.held = self.hold_updates;
        }
    }

    /// Forgets the estimate, the error and the lock: back to ratio 1.
    pub fn reset(&mut self) {
        self.error = None;
        self.integral = 0.0;
        self.inside = 0;
        self.locked = false;
        self.since_reprime = self.reprime_updates;
        self.pinned = 0;
        self.held = 0;
        self.beyond = false;
        self.missed = 0;
        self.last_raw = None;
        self.lost = 0;
        self.settled = 0;
        self.updates = 0;
        self.mean = 0.0;
        self.ratio = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive::MAX_DRIFT;
    use assert_no_alloc::assert_no_alloc;

    const RATE: u32 = 48_000;
    const PERIOD: u32 = 256;
    const TARGET: f64 = 1_000.0;

    /// A small deterministic generator for the measurement's phase.
    struct Lcg(u64);

    impl Lcg {
        /// Uniform in [0, 1).
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// A feed filled at `rate` by the master, read through the resampler
    /// by a writer that puts one period at a time into a card draining at
    /// `rate × (1 + ppm)`. `delay` is the path's true delay in master
    /// frames, averaged over a period: the card's own fill does not change
    /// from one wake of the writer to the next, so every change comes from
    /// the feed gaining the master's frames for one card period and losing
    /// what the resampler takes for one period of output.
    struct Path {
        rate: f64,
        period: f64,
        ppm: f64,
        delay: f64,
        ratio: f64,
        noise: Lcg,
        /// The reading's jitter, in periods peak to peak.
        jitter: f64,
    }

    impl Path {
        fn new(rate: u32, period: u32, ppm: f64, delay: f64) -> Self {
            Path {
                rate: rate as f64,
                period: period as f64,
                ppm,
                delay,
                ratio: 1.0,
                noise: Lcg(0x5eed),
                jitter: 1.0,
            }
        }

        /// The delay the writer reads: the master writes a period at a
        /// time, so the reading lands anywhere within a period of the
        /// true delay depending on where in the master's cycle it falls.
        fn measure(&mut self) -> f64 {
            self.delay + (self.noise.next() - 0.5) * self.jitter * self.period
        }

        /// One period of the card: the feed gains what the master writes
        /// meanwhile and gives the resampler what it needs for a period
        /// of output at the ratio last set. The delay cannot fall below
        /// nothing, where the card starves and the master's frames pass
        /// straight through, nor rise beyond the feed ring, which drops
        /// what does not fit.
        fn advance(&mut self) {
            let card = 1.0 + self.ppm * 1e-6;
            self.delay += self.period / card - self.period / self.ratio;
            self.delay = self.delay.clamp(0.0, TARGET + 6.0 * self.period);
        }

        fn seconds(&self, periods: usize) -> f64 {
            periods as f64 * self.period / self.rate
        }

        fn periods(&self, seconds: f64) -> usize {
            (seconds * self.rate / self.period).round() as usize
        }

        /// One wake of the writer: measure, update, write a period.
        fn step(&mut self, drift: &mut Drift) {
            let measured = self.measure();
            self.ratio = drift.update(measured);
            self.advance();
        }
    }

    /// Runs `ppm` for ten minutes and checks it locks within 30 s, then
    /// holds the delay within a quarter period and, once the integral has
    /// settled, the ppm within `tolerance` of the truth. Returns the lock
    /// time.
    fn holds(rate: u32, period: u32, ppm: f64, tolerance: f64) -> f64 {
        let mut path = Path::new(rate, period, ppm, TARGET);
        let mut drift = Drift::new(TARGET, rate, period);
        let quarter = period as f64 / 4.0;
        let mut lock = None;
        let total = path.periods(600.0);
        let settled = path.periods(120.0);
        for n in 0..total {
            path.step(&mut drift);
            if lock.is_none() && drift.locked() {
                lock = Some(n);
            }
            if let Some(at) = lock {
                assert!(drift.locked(), "lost lock at {} s", path.seconds(n));
                let error = path.delay - TARGET;
                assert!(
                    error.abs() < quarter,
                    "{ppm} ppm: error {error:.1} at {} s, locked at {} s",
                    path.seconds(n),
                    path.seconds(at)
                );
            }
            if n >= settled {
                assert!(
                    (drift.ppm() - ppm).abs() < tolerance,
                    "{ppm} ppm: estimate {:.2} at {} s",
                    drift.ppm(),
                    path.seconds(n)
                );
            }
            assert!(!drift.clamped());
        }
        let at = lock.expect("never locked");
        let seconds = path.seconds(at);
        assert!(seconds <= 30.0, "{ppm} ppm: locked only at {seconds} s");
        seconds
    }

    #[test]
    fn the_estimate_is_ready_only_after_its_whole_window() {
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        let seconds = f64::from(PERIOD) / f64::from(RATE);
        let updates = |s: f64| (s / seconds).ceil() as u32;
        // It settles for 20 s and then averages over 120.
        for _ in 0..updates(ESTIMATE) {
            drift.update(TARGET);
        }
        assert!(drift.locked());
        assert!(!drift.estimate_ready());
        for _ in 0..updates(5.0 * ESTIMATE_SMOOTHING + 1.0) {
            drift.update(TARGET);
        }
        assert!(drift.estimate_ready());
    }

    #[test]
    fn it_locks_on_a_card_100_ppm_fast() {
        holds(RATE, PERIOD, 100.0, 5.0);
    }

    #[test]
    fn it_locks_on_a_card_300_ppm_slow() {
        holds(RATE, PERIOD, -300.0, 5.0);
    }

    #[test]
    fn the_time_constants_hold_at_another_rate_and_period() {
        holds(44_100, 256, 300.0, 5.0);
        holds(96_000, 512, -100.0, 5.0);
        holds(192_000, 64, -100.0, 5.0);
        holds(384_000, 128, 100.0, 5.0);
    }

    #[test]
    fn it_locks_at_the_largest_period() {
        // A period of jitter on readings 23 ms apart tells the slope of
        // the delay over the estimate's window only to about 2.7 ppm (one
        // sigma) at best. The estimate gets 6.3 ppm rms and 27 ppm at worst
        // over 16 runs of 10 minutes, so it is held to 30 ppm here.
        holds(44_100, 1_024, 300.0, 30.0);
        holds(44_100, 1_024, -100.0, 30.0);
    }

    #[test]
    fn the_integral_takes_the_steady_error_away() {
        // Without the integral the delay would settle 2 s of the offset
        // away from the target: 96 frames at 1000 ppm.
        let ppm = 1_000.0;
        let mut path = Path::new(RATE, PERIOD, ppm, TARGET);
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        for _ in 0..path.periods(120.0) {
            path.step(&mut drift);
        }
        assert!(drift.locked());
        let n = path.periods(480.0);
        let mut sum = 0.0;
        for _ in 0..n {
            path.step(&mut drift);
            sum += path.delay - TARGET;
        }
        let mean = sum / n as f64;
        assert!(mean.abs() < 0.01 * PERIOD as f64, "steady error {mean:.2}");
    }

    #[test]
    fn a_fast_card_raises_the_ratio() {
        // A fast card drains the path, the delay falls below the target,
        // and the resampler must make more output frames from each input
        // frame: the ratio rises above 1.
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        let ratio = drift.update(TARGET - 50.0);
        assert!(ratio > 1.0, "{ratio}");
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        let ratio = drift.update(TARGET + 50.0);
        assert!(ratio < 1.0, "{ratio}");
    }

    #[test]
    fn a_one_period_step_recovers_without_overshooting_a_period() {
        let ppm = 100.0;
        let mut path = Path::new(RATE, PERIOD, ppm, TARGET);
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        for _ in 0..path.periods(180.0) {
            path.step(&mut drift);
        }
        assert!(drift.locked());
        // A period lost: the card skipped one, or the feed gained one.
        path.delay += PERIOD as f64;
        let quarter = PERIOD as f64 / 4.0;
        let mut back = None;
        let mut under = 0.0f64;
        for n in 0..path.periods(120.0) {
            path.step(&mut drift);
            let error = path.delay - TARGET;
            assert!(error < 1.1 * PERIOD as f64, "grew to {error:.1}");
            under = under.min(error);
            if back.is_none() && error.abs() < quarter {
                back = Some(n);
            }
        }
        let back = path.seconds(back.expect("never came back"));
        assert!(back < 20.0, "back within a quarter period only at {back} s");
        assert!(under > -quarter, "overshot to {under:.1}");
        assert!(drift.locked());
        // The lost period is no drift, but the estimate cannot tell it from
        // one: it moves by the period over the window and decays with it.
        for _ in 0..path.periods(180.0) {
            path.step(&mut drift);
            assert!(drift.locked());
        }
        assert!((drift.ppm() - ppm).abs() < 5.0, "{}", drift.ppm());
    }

    /// Locks `ppm` at `rate` and `period`, steps the delay by `periods`
    /// periods and checks `clamped()` stays false while it comes back.
    fn a_step_is_not_clamped(rate: u32, period: u32, periods: f64) -> (Path, Drift) {
        let mut path = Path::new(rate, period, 100.0, TARGET);
        let mut drift = Drift::new(TARGET, rate, period);
        for _ in 0..path.periods(120.0) {
            path.step(&mut drift);
        }
        assert!(drift.locked());
        path.delay += periods * period as f64;
        let mut pinned = 0.0;
        for _ in 0..path.periods(120.0) {
            path.step(&mut drift);
            if (path.ratio - 1.0).abs() >= MAX_DRIFT {
                pinned += path.seconds(1);
            }
            assert!(!drift.clamped(), "clamped after {pinned:.1} s at the clamp");
        }
        assert!(pinned > 5.0, "only {pinned:.1} s at the clamp");
        assert!(drift.locked());
        (path, drift)
    }

    #[test]
    fn a_refill_is_not_a_card_beyond_the_clamp() {
        a_step_is_not_clamped(RATE, PERIOD, 4.0);
        a_step_is_not_clamped(44_100, 1_024, 1.0);
    }

    #[test]
    fn a_large_step_restarts_the_estimate() {
        let (mut path, mut drift) = a_step_is_not_clamped(RATE, PERIOD, 4.0);
        // 120 s have passed since the step.
        for _ in 0..path.periods(30.0) {
            path.step(&mut drift);
        }
        assert!((drift.ppm() - 100.0).abs() < 5.0, "{}", drift.ppm());
    }

    #[test]
    fn a_card_beyond_the_clamp_holds_the_clamp_and_says_so() {
        let mut path = Path::new(RATE, PERIOD, 5_000.0, TARGET);
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        for _ in 0..path.periods(30.0) {
            path.step(&mut drift);
            assert!(!drift.locked());
            assert!(!drift.clamped());
        }
        // The card starves, and half the readings fall below nothing.
        assert_eq!(path.delay, 0.0);
        assert!(drift.skipped() > 0);
        for _ in 0..path.periods(5.0) {
            path.step(&mut drift);
        }
        assert!(drift.clamped());
        for _ in 0..path.periods(25.0) {
            path.step(&mut drift);
            assert!(!drift.locked());
            assert!(drift.clamped());
        }
        assert_eq!(path.ratio, 1.0 + MAX_DRIFT);
        // The other way too.
        let mut path = Path::new(RATE, PERIOD, -5_000.0, TARGET);
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        for _ in 0..path.periods(60.0) {
            path.step(&mut drift);
            assert!(!drift.locked());
        }
        assert_eq!(path.ratio, 1.0 - MAX_DRIFT);
        assert!(drift.clamped());
    }

    #[test]
    fn reset_starts_unlocked() {
        let mut path = Path::new(RATE, PERIOD, 200.0, TARGET);
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        for _ in 0..path.periods(120.0) {
            path.step(&mut drift);
        }
        assert!(drift.locked());
        assert!(drift.ppm() > 150.0);
        drift.reset();
        assert!(!drift.locked());
        assert!(!drift.clamped());
        assert_eq!(drift.ppm(), 0.0);
        assert_eq!(drift.update(TARGET), 1.0);
    }

    #[test]
    fn a_reprime_keeps_the_integral_the_lock_and_the_estimate() {
        let mut path = Path::new(RATE, PERIOD, 100.0, TARGET);
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        for _ in 0..path.periods(180.0) {
            path.step(&mut drift);
        }
        assert!(drift.locked());
        let (ratio, ppm, integral) = (path.ratio, drift.ppm(), drift.integral);
        // The path was primed again onto its target after a starve.
        drift.reprime();
        assert!(drift.locked());
        assert_eq!(drift.ppm(), ppm);
        assert_eq!(drift.integral, integral);
        // The first reading seeds the smoothed error afresh: a reading on
        // target gives the ratio the card needs, not 1.
        let next = drift.update(TARGET);
        assert!((next - ratio).abs() < 20e-6, "{next} after {ratio}");
        assert!((next - 1.0 - 100e-6).abs() < 20e-6, "{next}");
        // And it holds on from there.
        for _ in 0..path.periods(60.0) {
            path.step(&mut drift);
            assert!(drift.locked());
        }
    }

    #[test]
    fn ppm_survives_a_reprime() {
        // The writer reads the path from timestamps, to a few frames.
        let mut path = Path::new(RATE, PERIOD, 100.0, TARGET);
        path.jitter = 0.02;
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        for _ in 0..path.periods(180.0) {
            path.step(&mut drift);
        }
        assert!(drift.locked() && drift.estimate_ready());
        assert!((drift.ppm() - 100.0).abs() < 5.0, "{}", drift.ppm());
        // The card warms up, and the path starves every 3 s: each time it
        // is primed onto its target again.
        path.ppm = 150.0;
        let every = path.periods(3.0);
        for n in 0..path.periods(360.0) {
            if n % every == 0 {
                path.delay = TARGET;
                drift.reprime();
            }
            path.step(&mut drift);
            assert!(drift.ppm() != 0.0, "0 ppm at {} s", path.seconds(n));
            assert!(drift.estimate_ready());
        }
        // The estimate went on: it follows the card over its window.
        assert!((drift.ppm() - 150.0).abs() < 10.0, "{}", drift.ppm());
    }

    #[test]
    fn a_reprime_restarts_the_lock_time() {
        let mut path = Path::new(RATE, PERIOD, 100.0, TARGET);
        path.jitter = 0.02;
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        // Not locked yet: a reprime 4 s in starts the 5 s over.
        for _ in 0..path.periods(4.0) {
            path.step(&mut drift);
        }
        assert!(!drift.locked());
        drift.reprime();
        for _ in 0..path.periods(4.0) {
            path.step(&mut drift);
            assert!(!drift.locked(), "locked within 5 s of a reprime");
        }
        for _ in 0..path.periods(30.0) {
            path.step(&mut drift);
        }
        assert!(drift.locked());
        // Locked, one reprime is a starve and keeps the lock; a second
        // within 10 s ends it, until 5 s pass without one.
        drift.reprime();
        path.step(&mut drift);
        assert!(drift.locked());
        for _ in 0..path.periods(6.0) {
            path.step(&mut drift);
        }
        drift.reprime();
        assert!(!drift.locked());
        for _ in 0..path.periods(4.5) {
            path.step(&mut drift);
            assert!(!drift.locked());
        }
        for _ in 0..path.periods(1.0) {
            path.step(&mut drift);
        }
        assert!(drift.locked());
    }

    /// Runs two controllers on the same path, one of them also given
    /// `bad` readings at 60 s, and checks they stay the same.
    fn ignores(bad: &[f64]) {
        let mut a = Path::new(RATE, PERIOD, 100.0, TARGET);
        let mut b = Path::new(RATE, PERIOD, 100.0, TARGET);
        let mut da = Drift::new(TARGET, RATE, PERIOD);
        let mut db = Drift::new(TARGET, RATE, PERIOD);
        for n in 0..a.periods(180.0) {
            if n == a.periods(60.0) {
                for &reading in bad {
                    assert_eq!(db.update(reading), a.ratio, "{reading}");
                }
            }
            a.step(&mut da);
            b.step(&mut db);
            assert_eq!(a.ratio, b.ratio);
            assert_eq!(da.integral, db.integral);
            assert_eq!(da.ppm(), db.ppm());
        }
        assert_eq!(db.skipped(), bad.len() as u64);
        assert!(db.locked());
    }

    #[test]
    fn a_wrapped_reading_is_skipped() {
        ignores(&[1.8e19]);
    }

    #[test]
    fn an_absurd_reading_is_skipped() {
        ignores(&[1e300]);
    }

    #[test]
    fn a_negative_reading_is_skipped() {
        ignores(&[-1.0]);
    }

    #[test]
    fn an_infinite_reading_is_skipped() {
        ignores(&[f64::INFINITY]);
        ignores(&[f64::NEG_INFINITY]);
    }

    #[test]
    fn a_reading_that_is_not_a_number_is_skipped() {
        ignores(&[f64::NAN]);
    }

    #[test]
    fn a_second_of_bad_readings_resets() {
        let mut path = Path::new(RATE, PERIOD, 100.0, TARGET);
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        for _ in 0..path.periods(120.0) {
            path.step(&mut drift);
        }
        assert!(drift.locked());
        // Half a second of them changes nothing.
        let half = path.periods(0.5);
        for _ in 0..half {
            path.ratio = drift.update(f64::NAN);
            path.advance();
        }
        assert!(drift.locked());
        assert!(drift.ppm() > 90.0);
        path.step(&mut drift);
        // A second of them starts over.
        let second = path.periods(1.0);
        for _ in 0..second {
            path.ratio = drift.update(f64::NAN);
            path.advance();
        }
        assert!(!drift.locked());
        assert_eq!(drift.ppm(), 0.0);
        assert_eq!(path.ratio, 1.0);
        assert_eq!(drift.skipped(), (half + second) as u64);
        let mut lock = None;
        for n in 0..path.periods(60.0) {
            path.step(&mut drift);
            if lock.is_none() && drift.locked() {
                lock = Some(path.seconds(n));
            }
        }
        let lock = lock.expect("never locked again");
        assert!(lock < 30.0, "locked again only at {lock} s");
    }

    #[test]
    fn update_does_not_allocate() {
        let mut drift = Drift::new(TARGET, RATE, PERIOD);
        assert_no_alloc(|| {
            for n in 0..1_000 {
                drift.update(TARGET + (n % 7) as f64);
            }
        });
    }
}
