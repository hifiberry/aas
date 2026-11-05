//! Finding where a known signal appears in a capture.

use std::f64::consts::PI;

/// Lags this close to the strongest peak belong to the same peak.
const PEAK_WIDTH: usize = 16;

/// A sweep across the band. Its correlation has one narrow peak, so a delay can
/// be found to the frame.
pub fn test_chirp(len: usize) -> Vec<f64> {
    (0..len)
        .map(|i| {
            let t = i as f64 / len as f64;
            (PI * 0.45 * len as f64 * t * t).sin() * 0.5
        })
        .collect()
}

/// The offset of `reference` in `captured`. Returns `None` when the strongest
/// correlation peak is below `min_peak_ratio` times the next strongest, because
/// then the result would be a guess.
pub fn find_delay(reference: &[f64], captured: &[f64], min_peak_ratio: f64) -> Option<usize> {
    if reference.is_empty() || captured.len() < reference.len() {
        return None;
    }
    let scores: Vec<f64> = captured
        .windows(reference.len())
        .map(|w| {
            w.iter()
                .zip(reference)
                .map(|(a, b)| a * b)
                .sum::<f64>()
                .abs()
        })
        .collect();
    let (best, peak) = scores
        .iter()
        .copied()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    if peak == 0.0 {
        return None;
    }
    let next = scores
        .iter()
        .enumerate()
        .filter(|(i, _)| i.abs_diff(best) > PEAK_WIDTH)
        .map(|(_, s)| *s)
        .fold(0.0, f64::max);
    (peak >= next * min_peak_ratio).then_some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded_noise(len: usize) -> Vec<f64> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as f64 / u64::MAX as f64 * 2.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn finds_a_known_delay() {
        let reference = test_chirp(256);
        let mut captured = vec![0.0; 1024];
        captured[300..556].copy_from_slice(&reference);
        assert_eq!(find_delay(&reference, &captured, 2.0), Some(300));
    }

    #[test]
    fn silence_reports_no_signal() {
        assert_eq!(find_delay(&test_chirp(256), &vec![0.0; 1024], 2.0), None);
    }

    #[test]
    fn noise_reports_no_signal() {
        assert_eq!(find_delay(&test_chirp(256), &seeded_noise(1024), 2.0), None);
    }

    #[test]
    fn a_capture_shorter_than_the_reference_reports_no_signal() {
        assert_eq!(find_delay(&test_chirp(256), &[0.5; 100], 2.0), None);
    }
}
