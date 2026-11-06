//! Sample conversion and mixing, in the formats being compared.

use num_traits::Float;

/// 2^31. Scaling by a power of two is exact in floating point.
const SCALE: f64 = 2_147_483_648.0;

pub fn s32_to_f64(v: i32) -> f64 {
    f64::from(v) / SCALE
}

/// Saturates at the S32 limits. No dither.
pub fn f64_to_s32(v: f64) -> i32 {
    (v * SCALE).round() as i32
}

pub fn s32_to_f32(v: i32) -> f32 {
    v as f32 / SCALE as f32
}

/// Saturates at the S32 limits. No dither.
pub fn f32_to_s32(v: f32) -> i32 {
    (v * SCALE as f32).round() as i32
}

/// Sum `inputs`, each multiplied by its gain, into `out`.
pub fn mix_gain<T: Float>(inputs: &[&[T]], gains: &[T], out: &mut [T]) {
    out.fill(T::zero());
    for (input, gain) in inputs.iter().zip(gains) {
        for (o, i) in out.iter_mut().zip(input.iter()) {
            *o = *o + *i * *gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f64_round_trip_is_exact() {
        for v in [i32::MIN, -1, 0, 1, 0x7FFF_FF01, i32::MAX] {
            assert_eq!(f64_to_s32(s32_to_f64(v)), v);
        }
    }

    #[test]
    fn f32_round_trip_loses_bits() {
        assert_ne!(f32_to_s32(s32_to_f32(0x7FFF_FF01)), 0x7FFF_FF01);
    }

    #[test]
    fn conversion_saturates_above_full_scale() {
        assert_eq!(f64_to_s32(1.5), i32::MAX);
        assert_eq!(f64_to_s32(-1.5), i32::MIN);
    }

    #[test]
    fn unity_mix_of_one_input_is_exact() {
        let input: Vec<f64> = [i32::MIN, 1, i32::MAX].map(s32_to_f64).to_vec();
        let mut out = vec![0.0; 3];
        mix_gain(&[&input], &[1.0], &mut out);
        assert_eq!(out, input);
    }
}
