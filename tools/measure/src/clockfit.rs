//! When a captured frame was taken, from the card's own timestamps.
//!
//! A capture with timestamps on reports, at each status call, how many
//! frames the card has taken and the time at which its pointer was read.
//! The pointer is read before the time is taken, so each reading is at or
//! after the moment its frame was taken, never before. A line through the
//! readings gives the card's rate against the system clock. A second line
//! through the earliest reading of each stretch, the lower edge, gives the
//! time of a frame with the least delay of the pointer.

/// Stretches of the readings, each giving its earliest to the lower edge.
const STRETCHES: usize = 20;

/// Frame position against time, fitted to a card's readings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockFit {
    /// Seconds per frame on the system clock.
    pub slope: f64,
    /// The time of frame 0 by the line through all readings.
    pub mean_intercept: f64,
    /// Seconds per frame, and the time of frame 0, by the lower edge.
    pub lower_slope: f64,
    pub lower_intercept: f64,
    /// RMS of the readings' distance from the line, in seconds.
    pub rms: f64,
    /// Readings fitted.
    pub readings: usize,
}

impl ClockFit {
    /// Fits `(frame, seconds)` readings, in the order taken. Needs at
    /// least two distinct frames.
    pub fn new(readings: &[(f64, f64)]) -> Option<ClockFit> {
        let (slope, mean_intercept) = line(readings)?;
        let err: f64 = readings
            .iter()
            .map(|&(x, y)| (y - slope * x - mean_intercept).powi(2))
            .sum();
        let size = readings.len().div_ceil(STRETCHES).max(1);
        let earliest: Vec<(f64, f64)> = readings
            .chunks(size)
            .filter_map(|c| {
                c.iter()
                    .copied()
                    .min_by(|a, b| (a.1 - slope * a.0).total_cmp(&(b.1 - slope * b.0)))
            })
            .collect();
        let (lower_slope, lower_intercept) = line(&earliest).unwrap_or((slope, mean_intercept));
        Some(ClockFit {
            slope,
            mean_intercept,
            lower_slope,
            lower_intercept,
            rms: (err / readings.len() as f64).sqrt(),
            readings: readings.len(),
        })
    }

    /// The card's rate on the system clock, in frames per second.
    pub fn rate(&self) -> f64 {
        1.0 / self.slope
    }

    /// The time frame `frame` was taken, by the line.
    pub fn mean_time(&self, frame: f64) -> f64 {
        self.mean_intercept + self.slope * frame
    }

    /// The time frame `frame` was taken, by the lower edge.
    pub fn lower_time(&self, frame: f64) -> f64 {
        self.lower_intercept + self.lower_slope * frame
    }
}

/// The least-squares line through `(x, y)`, as slope and intercept.
fn line(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let (mx, my) = points
        .iter()
        .fold((0.0, 0.0), |(x, y), &(a, b)| (x + a / n, y + b / n));
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for &(x, y) in points {
        sxx += (x - mx) * (x - mx);
        sxy += (x - mx) * (y - my);
    }
    if sxx <= 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    Some((slope, my - slope * mx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_clock_is_fitted_exactly() {
        let readings: Vec<(f64, f64)> = (0..100)
            .map(|i| {
                let frame = f64::from(i) * 480.0;
                (frame, 10.0 + frame / 48_010.0)
            })
            .collect();
        let fit = ClockFit::new(&readings).unwrap();
        assert!((fit.rate() - 48_010.0).abs() < 1e-6);
        assert!((fit.mean_time(0.0) - 10.0).abs() < 1e-9);
        assert!((fit.lower_time(0.0) - 10.0).abs() < 1e-9);
        assert!(fit.rms < 1e-9);
    }

    #[test]
    fn the_lower_edge_takes_the_earliest_reading() {
        // Each reading is late by 0 to 4 ms, never early, at random.
        let mut seed = 1u32;
        let readings: Vec<(f64, f64)> = (0..4000)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let frame = f64::from(i) * 480.0;
                let late = f64::from(seed >> 8) / f64::from(1u32 << 24) * 0.004;
                (frame, 5.0 + frame / 48_000.0 + late)
            })
            .collect();
        let fit = ClockFit::new(&readings).unwrap();
        for frame in [0.0, 1e6, 1.9e6] {
            let truth = 5.0 + frame / 48_000.0;
            assert!((fit.lower_time(frame) - truth).abs() < 2e-5, "{fit:?}");
            assert!(
                (fit.mean_time(frame) - truth - 0.002).abs() < 1e-4,
                "{fit:?}"
            );
        }
    }

    #[test]
    fn one_reading_is_not_a_fit() {
        assert_eq!(ClockFit::new(&[(0.0, 1.0)]), None);
        assert_eq!(ClockFit::new(&[(5.0, 1.0), (5.0, 2.0)]), None);
    }
}
