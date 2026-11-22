//! When an AirPlay output has something to send: the level of the mix it
//! gets, as the `detect` plugin measures the analog input, the RMS of both
//! channels over windows of 50 ms. A window above the threshold is a
//! signal; no window above it for `idle_s` is silence.

/// Full scale of an `i32` sample.
const FULL: f64 = 2_147_483_648.0;

/// A change of the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateEvent {
    /// The first window above the threshold after silence.
    Signal,
    /// `idle_s` since the last window above it.
    Silence,
}

pub struct Gate {
    /// The threshold as a mean square at full scale 1.0.
    threshold: f64,
    idle_s: u32,
    rate: u32,
    /// Frames in one window.
    window: u32,
    /// Frames of the window so far and their sum of squares.
    seen: u32,
    sum: f64,
    open: bool,
    /// Frames since the end of the last window above the threshold.
    quiet: u64,
}

impl Gate {
    pub fn new(rate: u32, threshold_dbfs: f32, idle_s: u32) -> Gate {
        Gate {
            threshold: 10f64.powf(f64::from(threshold_dbfs) / 10.0),
            idle_s,
            rate,
            window: (rate / 20).max(1),
            seen: 0,
            sum: 0.0,
            open: false,
            quiet: 0,
        }
    }

    /// A new rate for what comes next; the state stays, and the window
    /// under way starts over.
    pub fn set_rate(&mut self, rate: u32) {
        self.rate = rate;
        self.window = (rate / 20).max(1);
        self.seen = 0;
        self.sum = 0.0;
    }

    pub fn signal(&self) -> bool {
        self.open
    }

    /// Time in which no frames came, as when the master has closed: it
    /// counts as silence.
    pub fn nothing(&mut self, elapsed: std::time::Duration) -> Option<GateEvent> {
        let frames = (elapsed.as_secs_f64() * f64::from(self.rate)) as u64;
        self.quiet = self.quiet.saturating_add(frames);
        if self.open && self.quiet >= u64::from(self.idle_s) * u64::from(self.rate) {
            self.open = false;
            return Some(GateEvent::Silence);
        }
        None
    }

    /// Measures interleaved stereo frames. Says when the gate opened or
    /// closed in them.
    pub fn feed(&mut self, interleaved: &[i32]) -> Option<GateEvent> {
        let was = self.open;
        let idle = u64::from(self.idle_s) * u64::from(self.rate);
        for frame in interleaved.chunks_exact(2) {
            let (l, r) = (f64::from(frame[0]) / FULL, f64::from(frame[1]) / FULL);
            self.sum += l * l + r * r;
            self.seen += 1;
            self.quiet += 1;
            if self.seen == self.window {
                let mean = self.sum / (2.0 * f64::from(self.window));
                if mean > self.threshold {
                    self.open = true;
                    self.quiet = 0;
                }
                self.seen = 0;
                self.sum = 0.0;
            }
            if self.open && self.quiet >= idle {
                self.open = false;
            }
        }
        match (was, self.open) {
            (false, true) => Some(GateEvent::Signal),
            (true, false) => Some(GateEvent::Silence),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const RATE: u32 = 48000;

    /// Stereo frames of a 1 kHz sine whose RMS is `dbfs`.
    fn sine(dbfs: f64, frames: usize) -> Vec<i32> {
        let amplitude = 10f64.powf(dbfs / 20.0) * 2f64.sqrt() * FULL;
        (0..frames)
            .flat_map(|n| {
                let v = (amplitude
                    * (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / f64::from(RATE)).sin())
                .clamp(-FULL, FULL - 1.0) as i32;
                [v, v]
            })
            .collect()
    }

    /// Feeds `samples` in chunks of 240 frames, returning every event.
    fn feed(gate: &mut Gate, samples: &[i32]) -> Vec<GateEvent> {
        samples.chunks(480).filter_map(|c| gate.feed(c)).collect()
    }

    fn zeros(seconds: f64) -> Vec<i32> {
        vec![0; (seconds * f64::from(RATE)) as usize * 2]
    }

    #[test]
    fn a_window_above_the_threshold_signals_once() {
        let mut gate = Gate::new(RATE, -50.0, 2);
        assert!(!gate.signal());
        assert_eq!(feed(&mut gate, &sine(-20.0, 4800)), [GateEvent::Signal]);
        assert!(gate.signal());
        assert_eq!(feed(&mut gate, &sine(-20.0, 4800)), []);
    }

    #[test]
    fn a_signal_on_one_channel_counts() {
        let mut right_only = sine(-20.0, 4800);
        right_only.iter_mut().step_by(2).for_each(|l| *l = 0);
        let mut gate = Gate::new(RATE, -50.0, 2);
        assert_eq!(feed(&mut gate, &right_only), [GateEvent::Signal]);
    }

    #[test]
    fn silence_after_idle_s_not_before() {
        let mut gate = Gate::new(RATE, -50.0, 2);
        // Exactly one window of signal.
        assert_eq!(feed(&mut gate, &sine(-20.0, 2400)), [GateEvent::Signal]);
        assert_eq!(feed(&mut gate, &zeros(1.95)), []);
        assert!(gate.signal());
        assert_eq!(feed(&mut gate, &zeros(0.05)), [GateEvent::Silence]);
        assert!(!gate.signal());
        assert_eq!(feed(&mut gate, &zeros(3.0)), []);
    }

    #[test]
    fn a_signal_within_idle_s_keeps_it_open() {
        let mut gate = Gate::new(RATE, -50.0, 2);
        feed(&mut gate, &sine(-20.0, 2400));
        assert_eq!(feed(&mut gate, &zeros(1.5)), []);
        assert_eq!(feed(&mut gate, &sine(-20.0, 2400)), []);
        assert_eq!(feed(&mut gate, &zeros(1.95)), []);
        assert_eq!(feed(&mut gate, &zeros(0.05)), [GateEvent::Silence]);
    }

    #[test]
    fn minus_60_dbfs_is_silence_at_minus_50_and_signal_at_minus_70() {
        let quiet = sine(-60.0, 4800);
        assert_eq!(feed(&mut Gate::new(RATE, -50.0, 2), &quiet), []);
        assert_eq!(
            feed(&mut Gate::new(RATE, -70.0, 2), &quiet),
            [GateEvent::Signal]
        );
    }

    #[test]
    fn time_without_frames_counts_as_silence() {
        let mut gate = Gate::new(RATE, -50.0, 2);
        feed(&mut gate, &sine(-20.0, 2400));
        assert_eq!(gate.nothing(Duration::from_millis(1950)), None);
        assert!(gate.signal());
        assert_eq!(
            gate.nothing(Duration::from_millis(50)),
            Some(GateEvent::Silence)
        );
        assert_eq!(gate.nothing(Duration::from_secs(5)), None);
    }

    #[test]
    fn a_rate_change_keeps_the_state_and_counts_at_the_new_rate() {
        let mut gate = Gate::new(RATE, -50.0, 2);
        feed(&mut gate, &sine(-20.0, 2400));
        gate.set_rate(96000);
        assert!(gate.signal());
        // 2 s at 96 kHz are twice the frames.
        assert_eq!(feed(&mut gate, &zeros(2.0)), []);
        assert_eq!(feed(&mut gate, &zeros(2.0)), [GateEvent::Silence]);
    }
}
