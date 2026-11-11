//! Counters the audio thread keeps for monitoring. All updates are lock free.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

/// Totals since the start of the daemon.
#[derive(Debug, Default)]
pub struct Counters {
    pub xruns: AtomicU64,
    /// Wake-ups later than the buffer lasts. A failure even with no xrun.
    pub wake_gaps: AtomicU64,
    pub worst_gap_ns: AtomicU64,
    pub worst_cycle_ns: AtomicU64,
    pub cycles: AtomicU64,
    /// Frames held back a cycle because they might be DoP, counted in every
    /// cycle that holds them in a stream under way. They play later. A
    /// client that keeps less than a period and 16 frames in its ring can
    /// cause this with PCM whose last frame looks like a marker. Usually
    /// the hold leaves a gap in the stream. A resampled source that holds
    /// such frames while it waits for the rest of a block is counted too,
    /// though its gap comes from the wait.
    pub held_frames: AtomicU64,
    /// The latest wake-up gap longer than the buffer, and when it ended on
    /// CLOCK_MONOTONIC, so that it can be matched to other events.
    pub last_gap_ns: AtomicU64,
    pub last_gap_at_ns: AtomicU64,
    /// A capture has been handed to the engine at some point. Only then
    /// does the summary name the analog input.
    pub analog_enabled: AtomicBool,
    /// Zeros put in front of a captured period that came short. A capture
    /// overrun is counted nowhere else: the capture restarts in place and
    /// the frames it lost show up here.
    pub analog_padded_frames: AtomicU64,
    /// Captured frames dropped because more than two periods were queued.
    pub analog_dropped_frames: AtomicU64,
    /// 1 while the analog input has signal, else 0. During DoP it can have
    /// signal and not be played.
    pub analog_open: AtomicU64,
}

/// Now on CLOCK_MONOTONIC, in ns. The same clock as `/proc/uptime` without
/// suspend, and as `CLOCK_MONOTONIC` in other programs.
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid timespec for the call. clock_gettime is
    // async-signal-safe and does not allocate.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

impl Counters {
    /// Records one cycle. `gap_ns` is the time since the previous wake-up,
    /// `cycle_ns` the work done, and `buffer_ns` how long the buffer lasts.
    pub fn on_cycle(&self, gap_ns: u64, cycle_ns: u64, buffer_ns: u64) {
        self.cycles.fetch_add(1, Relaxed);
        self.worst_gap_ns.fetch_max(gap_ns, Relaxed);
        self.worst_cycle_ns.fetch_max(cycle_ns, Relaxed);
        if gap_ns > buffer_ns {
            self.last_gap_ns.store(gap_ns, Relaxed);
            self.last_gap_at_ns.store(monotonic_ns(), Relaxed);
            self.wake_gaps.fetch_add(1, Relaxed);
        }
    }
}

impl std::fmt::Display for Counters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = |ns: &AtomicU64| ns.load(Relaxed) as f64 / 1e6;
        write!(
            f,
            "{} xruns, {} wake gaps, worst gap {:.3} ms, worst cycle {:.3} ms, {} cycles",
            self.xruns.load(Relaxed),
            self.wake_gaps.load(Relaxed),
            ms(&self.worst_gap_ns),
            ms(&self.worst_cycle_ns),
            self.cycles.load(Relaxed),
        )?;
        if self.analog_enabled.load(Relaxed) {
            let state = if self.analog_open.load(Relaxed) == 1 {
                "open"
            } else {
                "quiet"
            };
            write!(
                f,
                "; analog input {state}, {} frames padded, {} frames dropped",
                self.analog_padded_frames.load(Relaxed),
                self.analog_dropped_frames.load(Relaxed),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    fn only_a_gap_longer_than_the_buffer_counts() {
        let c = Counters::default();
        c.on_cycle(1_000, 10, 1_000);
        assert_eq!(c.wake_gaps.load(Relaxed), 0);
        c.on_cycle(1_001, 10, 1_000);
        assert_eq!(c.wake_gaps.load(Relaxed), 1);
    }

    #[test]
    fn a_gap_longer_than_the_buffer_is_kept_with_its_time() {
        let c = Counters::default();
        let before = monotonic_ns();
        c.on_cycle(3_000, 10, 1_000);
        c.on_cycle(500, 10, 1_000);
        assert_eq!(c.last_gap_ns.load(Relaxed), 3_000);
        let at = c.last_gap_at_ns.load(Relaxed);
        assert!(at >= before && at <= monotonic_ns());
    }

    #[test]
    fn the_summary_names_xruns_and_gaps() {
        let c = Counters::default();
        c.on_cycle(2_000_000, 500_000, 1_000_000);
        c.xruns.fetch_add(3, Relaxed);
        assert_eq!(
            c.to_string(),
            "3 xruns, 1 wake gaps, worst gap 2.000 ms, worst cycle 0.500 ms, 1 cycles"
        );
    }

    #[test]
    fn the_summary_names_the_analog_input_once_it_is_enabled() {
        let c = Counters::default();
        c.analog_padded_frames.fetch_add(5, Relaxed);
        assert!(c.to_string().ends_with(" 0 cycles"));
        c.analog_enabled.store(true, Relaxed);
        let quiet = "; analog input quiet, 5 frames padded, 0 frames dropped";
        assert!(c.to_string().ends_with(quiet), "{c}");
        c.analog_open.store(1, Relaxed);
        c.analog_dropped_frames.fetch_add(7, Relaxed);
        let open = "; analog input open, 5 frames padded, 7 frames dropped";
        assert!(c.to_string().ends_with(open), "{c}");
    }

    #[test]
    fn the_worst_values_are_kept_and_cycles_are_counted() {
        let c = Counters::default();
        c.on_cycle(500, 70, 1_000);
        c.on_cycle(300, 90, 1_000);
        c.on_cycle(100, 20, 1_000);
        assert_eq!(c.worst_gap_ns.load(Relaxed), 500);
        assert_eq!(c.worst_cycle_ns.load(Relaxed), 90);
        assert_eq!(c.cycles.load(Relaxed), 3);
    }
}
