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

/// The loudest sample, in dB relative to full scale.
/// The level of the chirp alone, so that a start-up click cannot pass for a cable.
pub fn chirp_level_dbfs(captured: &[f64], found: usize, chirp_frames: usize) -> f64 {
    let end = captured.len().min(found.saturating_add(chirp_frames));
    peak_dbfs(captured.get(found..end).unwrap_or(&[]))
}

pub fn peak_dbfs(samples: &[f64]) -> f64 {
    let peak = samples.iter().fold(0.0, |m: f64, s| m.max(s.abs()));
    20.0 * peak.log10()
}

/// Linked streams share a sample clock, so `converter_frames` is only the delay
/// through the DAC and ADC. Audio also waits in the output queue, which is full
/// when a block is written.
pub fn write_to_capture_frames(converter_frames: i64, period: usize, periods: u32) -> i64 {
    converter_frames + (period as i64) * i64::from(periods)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_outside_the_chirp_does_not_count_as_level() {
        let mut captured = vec![0.0; 4096];
        captured[100] = 0.0316;
        captured[2000..2004].fill(0.0001);
        assert!(chirp_level_dbfs(&captured, 1000, 2048) > -90.0);
        assert!(chirp_level_dbfs(&captured, 1000, 2048) < -40.0);
    }

    #[test]
    fn the_level_is_the_peak_inside_the_chirp() {
        let mut captured = vec![0.0; 4096];
        captured[1500] = 0.5;
        let level = chirp_level_dbfs(&captured, 1000, 2048);
        assert!((level - -6.02).abs() < 0.01);
    }

    #[test]
    fn a_chirp_past_the_end_of_the_capture_has_no_level() {
        let captured = vec![0.5; 100];
        assert_eq!(chirp_level_dbfs(&captured, 200, 2048), f64::NEG_INFINITY);
    }

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
    fn peak_level_of_half_scale_is_minus_six_db() {
        let level = peak_dbfs(&[0.0, -0.5, 0.25]);
        assert!((level + 6.02).abs() < 0.01, "got {level}");
    }

    #[test]
    fn peak_level_of_silence_is_minus_infinity() {
        assert_eq!(peak_dbfs(&[0.0; 8]), f64::NEG_INFINITY);
    }

    #[test]
    fn write_to_output_adds_the_queue_to_the_converter_delay() {
        assert_eq!(write_to_capture_frames(50, 64, 2), 178);
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
