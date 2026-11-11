//! Processing formats: the arithmetic used to mix sources and apply volume.
//!
//! The format is chosen once at startup. `mix` never allocates.

use crate::plugin::Block;

/// A per-block gain that moves linearly from `from` to `to`.
///
/// Frame `i` of an `n` frame block uses `from + (to - from) * i / n`, so the
/// next block, starting at `to`, continues without a step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ramp {
    pub from: f32,
    pub to: f32,
}

impl Ramp {
    /// A constant gain.
    pub fn flat(gain: f32) -> Self {
        Ramp {
            from: gain,
            to: gain,
        }
    }

    /// Gain for frame `i` of `n`.
    fn at(self, i: usize, n: usize) -> f32 {
        if self.from == self.to {
            self.from
        } else {
            self.from + (self.to - self.from) * (i as f32 / n as f32)
        }
    }
}

/// Convert decibels to a linear gain.
pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// One arithmetic format for mixing.
pub trait Processing {
    type Acc: Copy;
    fn zero() -> Self::Acc;
    /// Add `sample * gain` to `acc`.
    fn accumulate(acc: &mut Self::Acc, sample: i32, gain: f32);
    /// Convert to S32, saturating at the limits.
    fn finish(acc: Self::Acc) -> i32;
    /// Copies `acc` into a block in `scratch` for the plugins. Float blocks
    /// have full scale at 1.0 and are not clipped. An `i32` block is clipped
    /// to S32. `scratch` must hold `acc.len()` frames.
    fn to_block<'s>(acc: &mut [[Self::Acc; 2]], scratch: &'s mut BlockStore) -> Block<'s>;
    /// Copies a block that `to_block` gave back into `acc`.
    fn from_block(block: Block<'_>, acc: &mut [[Self::Acc; 2]]);
}

/// The buffers a block lives in between the mix and the plugins, one per
/// sample type. Sized once, outside the audio thread.
pub struct BlockStore {
    pub(crate) i32: Vec<[i32; 2]>,
    pub(crate) f32: Vec<[f32; 2]>,
    pub(crate) f64: Vec<[f64; 2]>,
}

impl BlockStore {
    /// Room for blocks of up to `max_frames` frames. Allocates nothing for 0.
    pub fn new(max_frames: usize) -> Self {
        BlockStore {
            i32: vec![[0; 2]; max_frames],
            f32: vec![[0.0; 2]; max_frames],
            f64: vec![[0.0; 2]; max_frames],
        }
    }
}

/// Integer processing. The accumulator is wide, so a sum that passes full
/// scale midway is exact at the end.
pub struct I32;
/// Single precision float processing.
pub struct F32;
/// Double precision float processing.
pub struct F64;

/// 2^31. Scaling by a power of two is exact in floating point.
const SCALE: f64 = 2_147_483_648.0;
/// Fixed-point gain of 1.0 (Q32).
const Q32_ONE: f64 = 4_294_967_296.0;

impl Processing for I32 {
    type Acc = i64;

    fn zero() -> i64 {
        0
    }

    fn accumulate(acc: &mut i64, sample: i32, gain: f32) {
        let q = (f64::from(gain) * Q32_ONE).round() as i64;
        // The product can pass 64 bits at large gains. Add half for rounding.
        let p = i128::from(sample) * i128::from(q) + (1i128 << 31);
        let v = (p >> 32).clamp(i128::from(i64::MIN / 2), i128::from(i64::MAX / 2));
        *acc = acc.saturating_add(v as i64);
    }

    fn finish(acc: i64) -> i32 {
        acc.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    }

    fn to_block<'s>(acc: &mut [[i64; 2]], scratch: &'s mut BlockStore) -> Block<'s> {
        let block = &mut scratch.i32[..acc.len()];
        for (b, a) in block.iter_mut().zip(acc.iter()) {
            *b = [Self::finish(a[0]), Self::finish(a[1])];
        }
        Block::I32(block)
    }

    fn from_block(block: Block<'_>, acc: &mut [[i64; 2]]) {
        let Block::I32(block) = block else {
            unreachable!("an i32 accumulator takes an i32 block")
        };
        for (a, b) in acc.iter_mut().zip(block.iter()) {
            *a = [i64::from(b[0]), i64::from(b[1])];
        }
    }
}

impl Processing for F32 {
    type Acc = f32;

    fn zero() -> f32 {
        0.0
    }

    fn accumulate(acc: &mut f32, sample: i32, gain: f32) {
        *acc += sample as f32 / SCALE as f32 * gain;
    }

    fn finish(acc: f32) -> i32 {
        (acc * SCALE as f32).round() as i32
    }

    /// The accumulator already has full scale at 1.0, so this is a copy.
    fn to_block<'s>(acc: &mut [[f32; 2]], scratch: &'s mut BlockStore) -> Block<'s> {
        let block = &mut scratch.f32[..acc.len()];
        block.copy_from_slice(acc);
        Block::F32(block)
    }

    fn from_block(block: Block<'_>, acc: &mut [[f32; 2]]) {
        let Block::F32(block) = block else {
            unreachable!("an f32 accumulator takes an f32 block")
        };
        acc.copy_from_slice(&block[..acc.len()]);
    }
}

impl Processing for F64 {
    type Acc = f64;

    fn zero() -> f64 {
        0.0
    }

    fn accumulate(acc: &mut f64, sample: i32, gain: f32) {
        *acc += f64::from(sample) / SCALE * f64::from(gain);
    }

    fn finish(acc: f64) -> i32 {
        (acc * SCALE).round() as i32
    }

    /// The accumulator already has full scale at 1.0, so this is a copy.
    fn to_block<'s>(acc: &mut [[f64; 2]], scratch: &'s mut BlockStore) -> Block<'s> {
        let block = &mut scratch.f64[..acc.len()];
        block.copy_from_slice(acc);
        Block::F64(block)
    }

    fn from_block(block: Block<'_>, acc: &mut [[f64; 2]]) {
        let Block::F64(block) = block else {
            unreachable!("an f64 accumulator takes an f64 block")
        };
        acc.copy_from_slice(&block[..acc.len()]);
    }
}

/// Sum the sources, each with its gain ramp, into `out`.
///
/// Every source and `acc` must be as long as `out`; extra frames are ignored.
pub fn mix<P: Processing>(
    sources: &[(&[[i32; 2]], Ramp)],
    acc: &mut [[P::Acc; 2]],
    out: &mut [[i32; 2]],
) {
    let n = out.len().min(acc.len());
    mix_clear::<P>(&mut acc[..n]);
    for (samples, ramp) in sources {
        mix_add::<P>(samples, *ramp, &mut acc[..n]);
    }
    mix_finish::<P>(&acc[..n], &mut out[..n]);
}

/// Starts a mix. The engine uses these three steps directly, because it has
/// no list of sources to hand to `mix` without allocating one.
pub fn mix_clear<P: Processing>(acc: &mut [[P::Acc; 2]]) {
    acc.fill([P::zero(), P::zero()]);
}

/// Adds one source. The ramp spans all of `acc`.
pub fn mix_add<P: Processing>(samples: &[[i32; 2]], ramp: Ramp, acc: &mut [[P::Acc; 2]]) {
    let n = acc.len();
    for (i, (a, s)) in acc.iter_mut().zip(samples.iter()).enumerate() {
        let g = ramp.at(i, n);
        P::accumulate(&mut a[0], s[0], g);
        P::accumulate(&mut a[1], s[1], g);
    }
}

/// Ends a mix by converting to S32.
pub fn mix_finish<P: Processing>(acc: &[[P::Acc; 2]], out: &mut [[i32; 2]]) {
    for (o, a) in out.iter_mut().zip(acc.iter()) {
        *o = [P::finish(a[0]), P::finish(a[1])];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDGES: [i32; 7] = [i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX - 1, i32::MAX];

    fn mix_one<P: Processing>(input: &[i32], gain: f32) -> Vec<i32> {
        let frames: Vec<[i32; 2]> = input.iter().map(|&v| [v, v]).collect();
        let mut acc = vec![[P::zero(); 2]; frames.len()];
        let mut out = vec![[0; 2]; frames.len()];
        mix::<P>(&[(&frames, Ramp::flat(gain))], &mut acc, &mut out);
        out.iter().map(|f| f[0]).collect()
    }

    #[test]
    fn i32_sum_over_full_scale_is_not_clipped_midway() {
        // MAX + MAX + MIN + MIN == -2: over full scale midway, exact at the
        // end, because i32::MIN is one further from zero than i32::MAX.
        let a = [[i32::MAX; 2]];
        let b = [[i32::MIN; 2]];
        let r = Ramp::flat(1.0);
        let mut acc = [[0i64; 2]];
        let mut out = [[0; 2]];
        mix::<I32>(&[(&a, r), (&a, r), (&b, r), (&b, r)], &mut acc, &mut out);
        assert_eq!(out, [[-2, -2]]);
    }

    #[test]
    fn all_formats_saturate_at_the_s32_limits() {
        let hi = [i32::MAX, i32::MAX];
        let lo = [i32::MIN, i32::MIN];
        assert_eq!(mix_one::<I32>(&hi, 1.0), [i32::MAX; 2]);
        assert_eq!(mix_one::<F32>(&hi, 1.0), [i32::MAX; 2]);
        for (input, expect) in [(&hi, i32::MAX), (&lo, i32::MIN)] {
            assert_eq!(mix_one::<I32>(input, 2.0), [expect; 2]);
            assert_eq!(mix_one::<F32>(input, 2.0), [expect; 2]);
            assert_eq!(mix_one::<F64>(input, 2.0), [expect; 2]);
        }
        assert_eq!(mix_one::<F64>(&hi, 1.0), [i32::MAX; 2]);
    }

    #[test]
    fn f64_at_unity_returns_every_s32_value_exactly() {
        let mut v = EDGES.to_vec();
        v.extend([0x1234_5679, -0x1234_5679, 0x7FFF_FF01]);
        assert_eq!(mix_one::<F64>(&v, 1.0), v);
    }

    #[test]
    fn f32_at_unity_returns_every_s24_value_exactly() {
        let v: Vec<i32> = [
            i32::MIN,
            -0x0123_4500,
            -256,
            0,
            256,
            0x0123_4500,
            0x7FFF_FF00,
        ]
        .to_vec();
        assert_eq!(mix_one::<F32>(&v, 1.0), v);
    }

    #[test]
    fn i32_at_unity_returns_every_s32_value_exactly() {
        let mut v = EDGES.to_vec();
        v.extend([0x1234_5679, -0x1234_5679, 0x7FFF_FF01]);
        assert_eq!(mix_one::<I32>(&v, 1.0), v);
    }

    #[test]
    fn i32_volume_rounds_rather_than_truncates() {
        // 3 * 0.5 = 1.5 rounds to 2; -3 * 0.5 = -1.5 rounds to -1 (half up).
        assert_eq!(mix_one::<I32>(&[3, 1, 5], 0.5), [2, 1, 3]);
        // 7 * 0.25 = 1.75 truncates to 1, rounds to 2.
        assert_eq!(mix_one::<I32>(&[7], 0.25), [2]);
        assert_eq!(mix_one::<I32>(&[-7], 0.25), [-2]);
    }

    #[test]
    fn ramp_moves_linearly_from_start_to_end() {
        let n = 4;
        let input = vec![[1 << 20; 2]; n];
        let ramp = Ramp { from: 0.0, to: 1.0 };
        let mut acc = vec![[0i64; 2]; n];
        let mut out = vec![[0; 2]; n];
        mix::<I32>(&[(&input, ramp)], &mut acc, &mut out);
        let got: Vec<i32> = out.iter().map(|f| f[0]).collect();
        assert_eq!(got, [0, 1 << 18, 1 << 19, 3 << 18]);
        assert_eq!(ramp.at(n, n), 1.0);
    }

    #[test]
    fn formats_agree_within_one_s24_step() {
        let a: Vec<[i32; 2]> = (0..64)
            .map(|i| [i * 9_000_011 - 300_000_000, i * 7_000_003])
            .collect();
        let b: Vec<[i32; 2]> = (0..64)
            .map(|i| [-i * 3_000_017, 200_000_000 - i * 5_000_011])
            .collect();
        let srcs = [
            (
                &a[..],
                Ramp {
                    from: 0.5,
                    to: 0.25,
                },
            ),
            (&b[..], Ramp::flat(db_to_gain(-6.0))),
        ];
        let mut oi = vec![[0; 2]; 64];
        let mut of = vec![[0; 2]; 64];
        let mut od = vec![[0; 2]; 64];
        mix::<I32>(&srcs, &mut vec![[0i64; 2]; 64], &mut oi);
        mix::<F32>(&srcs, &mut vec![[0f32; 2]; 64], &mut of);
        mix::<F64>(&srcs, &mut vec![[0f64; 2]; 64], &mut od);
        for i in 0..64 {
            for c in 0..2 {
                assert!((oi[i][c] - od[i][c]).abs() <= 1, "i32 vs f64 at {i}");
                assert!((of[i][c] - od[i][c]).abs() <= 256, "f32 vs f64 at {i}");
            }
        }
    }

    /// Mixes `input` at unity, passes it through a block and back, and
    /// finishes it. `look` sees the block on the way.
    fn block_round_trip<P: Processing>(input: &[i32], look: impl Fn(&Block<'_>)) -> Vec<i32> {
        let frames: Vec<[i32; 2]> = input.iter().map(|&v| [v, v]).collect();
        let mut acc = vec![[P::zero(); 2]; frames.len()];
        let mut store = BlockStore::new(frames.len());
        let mut out = vec![[0; 2]; frames.len()];
        mix_clear::<P>(&mut acc);
        mix_add::<P>(&frames, Ramp::flat(1.0), &mut acc);
        let block = P::to_block(&mut acc, &mut store);
        look(&block);
        P::from_block(block, &mut acc);
        mix_finish::<P>(&acc, &mut out);
        out.iter().map(|f| f[0]).collect()
    }

    #[test]
    fn every_s24_value_survives_the_block_round_trip_in_f32() {
        // Every S24 value, left aligned in an S32 sample.
        let v: Vec<i32> = (-(1 << 23)..(1 << 23)).map(|k| k << 8).collect();
        let got = block_round_trip::<F32>(&v, |block| {
            let Block::F32(b) = block else {
                panic!("not an f32 block")
            };
            for (f, &s) in b.iter().zip(v.iter()) {
                assert_eq!(f[0], s as f32 / 2_147_483_648.0);
            }
        });
        assert_eq!(got, v);
    }

    #[test]
    fn every_s32_edge_survives_the_block_round_trip_in_f64_and_i32() {
        let mut v = EDGES.to_vec();
        v.extend([0x1234_5679, -0x1234_5679, 0x7FFF_FF01]);
        let got = block_round_trip::<F64>(&v, |block| {
            let Block::F64(b) = block else {
                panic!("not an f64 block")
            };
            for (f, &s) in b.iter().zip(v.iter()) {
                assert_eq!(f[0], f64::from(s) / 2_147_483_648.0);
            }
        });
        assert_eq!(got, v);
        let got = block_round_trip::<I32>(&v, |block| {
            let Block::I32(b) = block else {
                panic!("not an i32 block")
            };
            assert_eq!(b.iter().map(|f| f[0]).collect::<Vec<_>>(), v);
        });
        assert_eq!(got, v);
    }

    #[test]
    fn db_to_gain_known_values() {
        assert_eq!(db_to_gain(0.0), 1.0);
        assert!((db_to_gain(-20.0) - 0.1).abs() < 1e-6);
    }
}
