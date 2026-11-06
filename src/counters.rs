//! Counters the audio thread keeps for monitoring. All updates are lock free.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Totals since the start of the daemon.
#[derive(Debug, Default)]
pub struct Counters {
    pub xruns: AtomicU64,
    /// Wake-ups later than the buffer lasts. A failure even with no xrun.
    pub wake_gaps: AtomicU64,
    pub worst_gap_ns: AtomicU64,
    pub worst_cycle_ns: AtomicU64,
    pub cycles: AtomicU64,
    /// Frames held back a cycle because they might be DoP, each time it left
    /// a gap in a running stream. They play later. A client that keeps less
    /// than a period and 16 frames in its ring can cause this with PCM whose
    /// last frame looks like a marker.
    pub held_frames: AtomicU64,
}

impl Counters {
    /// Records one cycle. `gap_ns` is the time since the previous wake-up,
    /// `cycle_ns` the work done, and `buffer_ns` how long the buffer lasts.
    pub fn on_cycle(&self, gap_ns: u64, cycle_ns: u64, buffer_ns: u64) {
        self.cycles.fetch_add(1, Relaxed);
        self.worst_gap_ns.fetch_max(gap_ns, Relaxed);
        self.worst_cycle_ns.fetch_max(cycle_ns, Relaxed);
        if gap_ns > buffer_ns {
            self.wake_gaps.fetch_add(1, Relaxed);
        }
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
