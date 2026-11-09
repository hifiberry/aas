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

/// Fixed-point gain of 1.0, Q32.
const Q32_ONE: f64 = 4_294_967_296.0;

/// Sum `inputs`, each multiplied by its gain, into `out` in integer
/// arithmetic, as the daemon's `i32` format does. Each product is rounded
/// once, the sum is kept in a 64-bit accumulator, and only the result
/// saturates at the S32 limits. `acc` is as long as `out`.
pub fn mix_i32(inputs: &[&[i32]], gains: &[f32], acc: &mut [i64], out: &mut [i32]) {
    acc.fill(0);
    for (input, gain) in inputs.iter().zip(gains) {
        for (a, i) in acc.iter_mut().zip(input.iter()) {
            let q = (f64::from(*gain) * Q32_ONE).round() as i64;
            let p = i128::from(*i) * i128::from(q) + (1i128 << 31);
            let v = (p >> 32).clamp(i128::from(i64::MIN / 2), i128::from(i64::MAX / 2));
            *a = a.saturating_add(v as i64);
        }
    }
    for (o, a) in out.iter_mut().zip(acc.iter()) {
        *o = (*a).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_i32_mix_of_one_input_is_exact() {
        let input = [i32::MIN, -1, 0, 1, i32::MAX];
        let mut acc = [0; 5];
        let mut out = [0; 5];
        mix_i32(&[&input], &[1.0], &mut acc, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn i32_mix_saturates_only_at_the_end() {
        let high = [i32::MAX];
        let low = [-i32::MAX];
        let mut acc = [0; 1];
        let mut out = [0; 1];
        mix_i32(&[&high, &high, &low], &[1.0; 3], &mut acc, &mut out);
        assert_eq!(out, high);
        mix_i32(&[&high, &high], &[1.0; 2], &mut acc, &mut out);
        assert_eq!(out, high);
    }

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
