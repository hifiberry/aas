//! Which wake-ups a client still has to account for.
//!
//! A late client must be timed from the wake-up it missed, not from the newest
//! one, or the worst case can never read much above one period.

use std::ops::Range;

/// How many send times are kept. A client later than this loses the oldest.
pub const RING: u64 = 4096;

#[derive(Default)]
pub struct Tracker {
    last: u64,
    missed: u64,
}

impl Tracker {
    /// `newest` is the latest sequence sent. Returns the sequences to time.
    pub fn on_wake(&mut self, newest: u64) -> Range<u64> {
        if newest <= self.last {
            return self.last + 1..self.last + 1;
        }
        self.missed += newest - self.last - 1;
        let first = (self.last + 1).max((newest + 1).saturating_sub(RING));
        self.last = newest;
        first..newest + 1
    }

    /// Wake-ups that arrived while the client was still late for an earlier one.
    pub fn missed(&self) -> u64 {
        self.missed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_on_time_wake_up_times_only_itself() {
        let mut t = Tracker::default();
        assert_eq!(t.on_wake(1), 1..2);
        assert_eq!(t.on_wake(2), 2..3);
        assert_eq!(t.missed(), 0);
    }

    #[test]
    fn a_late_client_is_timed_from_the_wake_up_it_missed() {
        let mut t = Tracker::default();
        t.on_wake(1);
        assert_eq!(t.on_wake(4), 2..5);
    }

    #[test]
    fn every_lost_wake_up_is_counted_not_just_the_gap() {
        let mut t = Tracker::default();
        t.on_wake(1);
        t.on_wake(151);
        assert_eq!(t.missed(), 149);
    }

    #[test]
    fn a_wake_up_with_nothing_new_times_nothing() {
        let mut t = Tracker::default();
        t.on_wake(4);
        let missed = t.missed();
        assert!(t.on_wake(4).is_empty());
        assert_eq!(t.missed(), missed);
    }

    #[test]
    fn a_client_later_than_the_ring_times_only_what_is_left() {
        let mut t = Tracker::default();
        t.on_wake(1);
        let newest = 1 + RING + 10;
        assert_eq!(t.on_wake(newest), newest + 1 - RING..newest + 1);
        assert_eq!(t.missed(), RING + 9);
    }
}
