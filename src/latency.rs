//! The latency rule: how a target in milliseconds becomes a period size.

/// Periods in the ALSA buffer.
pub const PERIODS: u32 = 2;

/// Smallest period the card paths are expected to handle.
const MIN_PERIOD: u32 = 32;

/// Frames per period for a target buffer latency, as a power of two.
///
/// The target is split over `PERIODS` periods and rounded down, so the buffer
/// never exceeds the target. The result is at least 32 frames, which is also
/// the answer for a target that is not a positive finite number.
pub fn period_frames(rate: u32, target_ms: f32) -> u32 {
    if !(target_ms.is_finite() && target_ms > 0.0) {
        return MIN_PERIOD;
    }
    let total = rate as f64 * target_ms as f64 / 1000.0;
    let per_period = (total / PERIODS as f64).floor();
    if per_period < MIN_PERIOD as f64 {
        return MIN_PERIOD;
    }
    let frames = per_period as u32;
    1 << (31 - frames.leading_zeros())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_ms_gives_the_pi5_periods() {
        for (rate, frames) in [(44100, 64), (48000, 64), (96000, 128), (192000, 256)] {
            assert_eq!(period_frames(rate, 4.0), frames);
        }
    }

    #[test]
    fn twelve_ms_gives_the_pi3_periods() {
        for (rate, frames) in [(44100, 256), (48000, 256), (96000, 512), (192000, 1024)] {
            assert_eq!(period_frames(rate, 12.0), frames);
        }
    }

    #[test]
    fn a_target_that_is_not_positive_and_finite_gives_the_minimum() {
        for t in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -4.0] {
            assert_eq!(period_frames(48000, t), 32, "{t}");
        }
    }

    #[test]
    fn period_never_falls_below_32() {
        assert_eq!(period_frames(8000, 1.0), 32);
    }
}
