//! Timing statistics that can be recorded from an audio loop.

use std::time::Duration;

/// Values below this have a bucket each, so they are exact.
const EXACT: u64 = 1024;
const EXACT_BITS: u32 = 10;
/// Above `EXACT`, each power of two is split into this many buckets (about 1.5 %).
const SUB_BITS: u32 = 6;
const SUBS: usize = 1 << SUB_BITS;
const BUCKETS: usize = EXACT as usize + (64 - EXACT_BITS as usize) * SUBS;

/// Records durations in nanoseconds. All memory is allocated up front, so
/// `record` is safe to call from an audio loop.
pub struct Stats {
    warmup_left: usize,
    buckets: Vec<u64>,
    count: u64,
    sum: u128,
    min: u64,
    max: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub count: u64,
    pub min: u64,
    pub mean: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
}

fn bucket_of(v: u64) -> usize {
    if v < EXACT {
        return v as usize;
    }
    let exp = 63 - v.leading_zeros();
    let sub = (v >> (exp - SUB_BITS)) as usize & (SUBS - 1);
    EXACT as usize + (exp - EXACT_BITS) as usize * SUBS + sub
}

/// The largest value that falls in bucket `index`.
fn bucket_top(index: usize) -> u64 {
    if index < EXACT as usize {
        return index as u64;
    }
    let exp = (index - EXACT as usize) / SUBS + EXACT_BITS as usize;
    let sub = ((index - EXACT as usize) % SUBS) as u64;
    let width = 1u64 << (exp as u32 - SUB_BITS);
    (1u64 << exp) + sub * width + (width - 1)
}

impl Stats {
    /// Ignore the first `discard` values, so a cold start does not count.
    pub fn with_warmup(discard: usize) -> Stats {
        Stats {
            warmup_left: discard,
            buckets: vec![0; BUCKETS],
            count: 0,
            sum: 0,
            min: u64::MAX,
            max: 0,
        }
    }

    pub fn record(&mut self, nanos: u64) {
        if self.warmup_left > 0 {
            self.warmup_left -= 1;
            return;
        }
        self.buckets[bucket_of(nanos)] += 1;
        self.count += 1;
        self.sum += u128::from(nanos);
        self.min = self.min.min(nanos);
        self.max = self.max.max(nanos);
    }

    fn percentile(&self, per_mille: u64) -> u64 {
        let rank = (self.count * per_mille).div_ceil(1000);
        let mut seen = 0;
        for (index, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                return bucket_top(index).min(self.max);
            }
        }
        self.max
    }

    pub fn summary(&self) -> Summary {
        if self.count == 0 {
            return Summary {
                count: 0,
                min: 0,
                mean: 0,
                p99: 0,
                p999: 0,
                max: 0,
            };
        }
        Summary {
            count: self.count,
            min: self.min,
            mean: (self.sum / u128::from(self.count)) as u64,
            p99: self.percentile(990),
            p999: self.percentile(999),
            max: self.max,
        }
    }
}

/// The first line of a result. A run that ended early says so, because its
/// figures do not meet the time the measurement was planned for.
pub fn report_cases(done: usize, planned: usize) -> String {
    let state = if done < planned {
        "INCOMPLETE"
    } else {
        "complete"
    };
    format!("run {state}: {done} of {planned} cases")
}

///
/// `recorded` is the number of values behind the figures. A run that ended
/// inside its warm-up has none, and is not complete either.
pub fn report_header(planned: Duration, actual: Duration, recorded: u64) -> String {
    let state = if actual < planned || recorded == 0 {
        "INCOMPLETE"
    } else {
        "complete"
    };
    format!(
        "run {state}: planned {:.1} s, actual {:.1} s",
        planned.as_secs_f64(),
        actual.as_secs_f64()
    )
}

/// Whether a period run held: complete, no xrun, and no wake-up interval
/// longer than the buffer, which ALSA may not report as an xrun.
pub fn verdict(complete: bool, xruns: u64, worst_ns: u64, buffer_ns: u64) -> &'static str {
    if !complete {
        "INCOMPLETE"
    } else if xruns == 0 && worst_ns <= buffer_ns {
        "PASS"
    } else {
        "FAIL"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_stopped_early_says_how_many_cases_it_reached() {
        assert_eq!(report_cases(12, 96), "run INCOMPLETE: 12 of 96 cases");
    }

    #[test]
    fn a_full_run_reports_every_case() {
        assert_eq!(report_cases(96, 96), "run complete: 96 of 96 cases");
    }

    #[test]
    fn warmup_samples_are_discarded() {
        let mut s = Stats::with_warmup(2);
        for v in [1_000_000, 1_000_000, 10, 20, 30] {
            s.record(v);
        }
        let r = s.summary();
        assert_eq!((r.count, r.min, r.max), (3, 10, 30));
    }

    #[test]
    fn percentiles_use_recorded_values() {
        let mut s = Stats::with_warmup(0);
        for v in 1..=1000 {
            s.record(v);
        }
        let r = s.summary();
        assert_eq!((r.p99, r.p999), (990, 999));
    }

    #[test]
    fn header_shows_an_interrupted_run() {
        let h = report_header(Duration::from_secs(3600), Duration::from_secs(10), 1);
        assert!(h.contains("INCOMPLETE"));
        assert!(
            !report_header(Duration::from_secs(5), Duration::from_secs(5), 1)
                .contains("INCOMPLETE")
        );
    }

    #[test]
    fn a_run_with_nothing_recorded_is_not_complete() {
        let h = report_header(Duration::from_secs(1), Duration::from_secs(1), 0);
        assert!(h.contains("INCOMPLETE"));
    }

    #[test]
    fn a_wake_up_longer_than_the_buffer_fails_the_verdict() {
        assert_eq!(verdict(true, 0, 2_000_000, 2_666_666), "PASS");
        assert_eq!(verdict(true, 0, 2_700_000, 2_666_666), "FAIL");
        assert_eq!(verdict(true, 1, 2_000_000, 2_666_666), "FAIL");
        assert_eq!(verdict(false, 0, 2_000_000, 2_666_666), "INCOMPLETE");
    }
}
