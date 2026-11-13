//! Processing formats: the arithmetic used to mix sources and apply volume.
//!
//! The format is chosen once at startup. `mix` never allocates.

use crate::matrix::MAX_CHANNELS;
use crate::plugin::{Block, Samples};

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

/// One arithmetic format for mixing. `'static`, so that an output's mix in
/// it can travel to the audio thread as `Any`.
pub trait Processing: 'static {
    type Acc: Copy + Send;
    /// A gain in the form `accumulate` multiplies by.
    type Gain: Copy;
    fn zero() -> Self::Acc;
    /// Converts a gain. `mix_add` does this once per frame, or once per call
    /// when the gain does not move, not once per sample.
    fn gain(gain: f32) -> Self::Gain;
    /// Add `sample * gain` to `acc`.
    fn accumulate(acc: &mut Self::Acc, sample: i32, gain: Self::Gain);
    /// Add `value * gain` to `acc`, where `value` is a sample already in
    /// the accumulator's form, such as the result of a plugin chain.
    fn accumulate_acc(acc: &mut Self::Acc, value: Self::Acc, gain: Self::Gain);
    /// Convert to S32, saturating at the limits. A float that is not a
    /// number becomes 0, and an infinite one the limit of its sign.
    fn finish(acc: Self::Acc) -> i32;
    /// Copies `acc` into a block in `scratch` for the plugins. Float blocks
    /// have full scale at 1.0 and are not clipped. An `i32` block is clipped
    /// to S32. `acc` holds interleaved frames of `channels` samples, and
    /// `scratch` must hold as many.
    fn to_block<'s>(
        acc: &mut [Self::Acc],
        channels: usize,
        scratch: &'s mut BlockStore,
    ) -> Block<'s>;
    /// Copies a block that `to_block` gave back into `acc`.
    fn from_block(block: Block<'_>, acc: &mut [Self::Acc]);
}

/// The buffers a block lives in between the mix and the plugins, one per
/// sample type. Sized once, outside the audio thread.
pub struct BlockStore {
    pub(crate) i32: Vec<i32>,
    pub(crate) f32: Vec<f32>,
    pub(crate) f64: Vec<f64>,
}

impl BlockStore {
    /// Room for blocks of up to `max_frames` frames of `channels` samples.
    /// Allocates nothing for 0 frames.
    pub fn new(max_frames: usize, channels: usize) -> Self {
        let samples = max_frames * channels;
        BlockStore {
            i32: vec![0; samples],
            f32: vec![0.0; samples],
            f64: vec![0.0; samples],
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
    /// Q32 fixed point.
    type Gain = i64;

    fn zero() -> i64 {
        0
    }

    fn gain(gain: f32) -> i64 {
        (f64::from(gain) * Q32_ONE).round() as i64
    }

    fn accumulate(acc: &mut i64, sample: i32, q: i64) {
        // The product can pass 64 bits at large gains. Add half for rounding,
        // and one less below zero, so that an exact half rounds away from
        // zero as the float formats do.
        let p = i128::from(sample) * i128::from(q);
        let p = p + (1i128 << 31) + (p >> 127);
        let v = (p >> 32).clamp(i128::from(i64::MIN / 2), i128::from(i64::MAX / 2));
        *acc = acc.saturating_add(v as i64);
    }

    /// As `accumulate`, on a wider value.
    fn accumulate_acc(acc: &mut i64, value: i64, q: i64) {
        let p = i128::from(value) * i128::from(q);
        let p = p + (1i128 << 31) + (p >> 127);
        let v = (p >> 32).clamp(i128::from(i64::MIN / 2), i128::from(i64::MAX / 2));
        *acc = acc.saturating_add(v as i64);
    }

    fn finish(acc: i64) -> i32 {
        acc.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    }

    fn to_block<'s>(acc: &mut [i64], channels: usize, scratch: &'s mut BlockStore) -> Block<'s> {
        debug_assert_eq!(acc.len() % channels, 0, "a whole number of frames");
        let samples = &mut scratch.i32[..acc.len()];
        for (b, a) in samples.iter_mut().zip(acc.iter()) {
            *b = Self::finish(*a);
        }
        Block {
            channels,
            samples: Samples::I32(samples),
        }
    }

    fn from_block(block: Block<'_>, acc: &mut [i64]) {
        let Samples::I32(samples) = block.samples else {
            unreachable!("an i32 accumulator takes an i32 block")
        };
        for (a, b) in acc.iter_mut().zip(samples.iter()) {
            *a = i64::from(*b);
        }
    }
}

impl Processing for F32 {
    type Acc = f32;
    type Gain = f32;

    fn zero() -> f32 {
        0.0
    }

    fn gain(gain: f32) -> f32 {
        gain
    }

    fn accumulate(acc: &mut f32, sample: i32, gain: f32) {
        *acc += sample as f32 / SCALE as f32 * gain;
    }

    fn accumulate_acc(acc: &mut f32, value: f32, gain: f32) {
        *acc += value * gain;
    }

    fn finish(acc: f32) -> i32 {
        (acc * SCALE as f32).round() as i32
    }

    /// The accumulator already has full scale at 1.0, so this is a copy.
    fn to_block<'s>(acc: &mut [f32], channels: usize, scratch: &'s mut BlockStore) -> Block<'s> {
        debug_assert_eq!(acc.len() % channels, 0, "a whole number of frames");
        let samples = &mut scratch.f32[..acc.len()];
        samples.copy_from_slice(acc);
        Block {
            channels,
            samples: Samples::F32(samples),
        }
    }

    fn from_block(block: Block<'_>, acc: &mut [f32]) {
        let Samples::F32(samples) = block.samples else {
            unreachable!("an f32 accumulator takes an f32 block")
        };
        acc.copy_from_slice(&samples[..acc.len()]);
    }
}

impl Processing for F64 {
    type Acc = f64;
    type Gain = f64;

    fn zero() -> f64 {
        0.0
    }

    fn gain(gain: f32) -> f64 {
        f64::from(gain)
    }

    fn accumulate(acc: &mut f64, sample: i32, gain: f64) {
        *acc += f64::from(sample) / SCALE * gain;
    }

    fn accumulate_acc(acc: &mut f64, value: f64, gain: f64) {
        *acc += value * gain;
    }

    fn finish(acc: f64) -> i32 {
        (acc * SCALE).round() as i32
    }

    /// The accumulator already has full scale at 1.0, so this is a copy.
    fn to_block<'s>(acc: &mut [f64], channels: usize, scratch: &'s mut BlockStore) -> Block<'s> {
        debug_assert_eq!(acc.len() % channels, 0, "a whole number of frames");
        let samples = &mut scratch.f64[..acc.len()];
        samples.copy_from_slice(acc);
        Block {
            channels,
            samples: Samples::F64(samples),
        }
    }

    fn from_block(block: Block<'_>, acc: &mut [f64]) {
        let Samples::F64(samples) = block.samples else {
            unreachable!("an f64 accumulator takes an f64 block")
        };
        acc.copy_from_slice(&samples[..acc.len()]);
    }
}

/// Sum the sources, each with its gain ramp, into `out`.
///
/// Every source and `acc` must be as long as `out`. A shorter `acc` would
/// leave the tail of `out` as it was, so debug builds refuse it.
pub fn mix<P: Processing>(
    sources: &[(&[[i32; 2]], Ramp)],
    acc: &mut [[P::Acc; 2]],
    out: &mut [[i32; 2]],
) {
    debug_assert_eq!(acc.len(), out.len());
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
    if ramp.from == ramp.to {
        // The usual case: the volume is not moving.
        let g = P::gain(ramp.from);
        for (a, s) in acc.iter_mut().zip(samples.iter()) {
            P::accumulate(&mut a[0], s[0], g);
            P::accumulate(&mut a[1], s[1], g);
        }
        return;
    }
    let n = acc.len();
    for (i, (a, s)) in acc.iter_mut().zip(samples.iter()).enumerate() {
        let g = P::gain(ramp.at(i, n));
        P::accumulate(&mut a[0], s[0], g);
        P::accumulate(&mut a[1], s[1], g);
    }
}

/// Adds a source that is already in the accumulator's form, such as the
/// output of a plugin chain, without going through S32 and so without
/// clipping it. The ramp spans all of `acc`.
pub fn mix_add_acc<P: Processing>(values: &[[P::Acc; 2]], ramp: Ramp, acc: &mut [[P::Acc; 2]]) {
    if ramp.from == ramp.to {
        // The usual case, as in `mix_add`: one gain for the whole call.
        let g = P::gain(ramp.from);
        for (a, v) in acc.iter_mut().zip(values.iter()) {
            P::accumulate_acc(&mut a[0], v[0], g);
            P::accumulate_acc(&mut a[1], v[1], g);
        }
        return;
    }
    let n = acc.len();
    for (i, (a, v)) in acc.iter_mut().zip(values.iter()).enumerate() {
        let g = P::gain(ramp.at(i, n));
        P::accumulate_acc(&mut a[0], v[0], g);
        P::accumulate_acc(&mut a[1], v[1], g);
    }
}

/// Ends a mix by converting to S32.
pub fn mix_finish<P: Processing>(acc: &[[P::Acc; 2]], out: &mut [[i32; 2]]) {
    for (o, a) in out.iter_mut().zip(acc.iter()) {
        *o = [P::finish(a[0]), P::finish(a[1])];
    }
}

/// Adds `bus * gain` for both bus channels into the interleaved `acc`, `width` wide.
/// `gains[c]` is the ramp from bus channel c to each output channel; off is skipped.
///
/// A ramp spans all the frames of `bus`, and `acc` holds as many frames. A
/// gain of exactly 1.0 adds a value unchanged in every format, so a bus
/// that reaches one channel at 0 dB arrives there as it was.
pub fn matrix_add<P: Processing>(
    bus: &[[P::Acc; 2]],
    gains: &[[Ramp; MAX_CHANNELS]; 2],
    width: usize,
    acc: &mut [P::Acc],
) {
    debug_assert_eq!(
        acc.len(),
        bus.len() * width,
        "a frame of `acc` per bus frame"
    );
    let n = bus.len();
    for (side, row) in gains.iter().enumerate() {
        for (channel, &ramp) in row.iter().enumerate().take(width) {
            if ramp.from == 0.0 && ramp.to == 0.0 {
                continue;
            }
            let frames = acc.chunks_exact_mut(width).zip(bus.iter());
            if ramp.from == ramp.to {
                let g = P::gain(ramp.from);
                for (a, b) in frames {
                    P::accumulate_acc(&mut a[channel], b[side], g);
                }
            } else {
                for (i, (a, b)) in frames.enumerate() {
                    let g = P::gain(ramp.at(i, n));
                    P::accumulate_acc(&mut a[channel], b[side], g);
                }
            }
        }
    }
}

/// Multiplies `acc`, interleaved frames `width` wide, by `ramp`, which
/// spans all of its frames. A gain of exactly 1.0 leaves it as it is.
pub fn scale_wide<P: Processing>(acc: &mut [P::Acc], width: usize, ramp: Ramp) {
    if ramp == Ramp::flat(1.0) {
        return;
    }
    let n = acc.len() / width;
    for (i, frame) in acc.chunks_exact_mut(width).enumerate() {
        let g = P::gain(ramp.at(i, n));
        for a in frame.iter_mut() {
            let v = std::mem::replace(a, P::zero());
            P::accumulate_acc(a, v, g);
        }
    }
}

/// Ends a mix of any width by converting to S32, saturating once, here.
pub fn mix_finish_wide<P: Processing>(acc: &[P::Acc], out: &mut [i32]) {
    debug_assert_eq!(acc.len(), out.len());
    for (o, a) in out.iter_mut().zip(acc.iter()) {
        *o = P::finish(*a);
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
        // 3 * 0.5 = 1.5 rounds to 2.
        assert_eq!(mix_one::<I32>(&[3, 1, 5], 0.5), [2, 1, 3]);
        // 7 * 0.25 = 1.75 truncates to 1, rounds to 2.
        assert_eq!(mix_one::<I32>(&[7], 0.25), [2]);
        assert_eq!(mix_one::<I32>(&[-7], 0.25), [-2]);
    }

    #[test]
    fn every_format_rounds_an_exact_half_away_from_zero() {
        // 3 * 0.5 = 1.5 and -3 * 0.5 = -1.5: the negative of a signal mixes
        // to the negative of the result.
        let input = [3, -3, 1, -1, 5, -5, 2, -2];
        let expect = [2, -2, 1, -1, 3, -3, 1, -1];
        assert_eq!(mix_one::<I32>(&input, 0.5), expect);
        assert_eq!(mix_one::<F32>(&input, 0.5), expect);
        assert_eq!(mix_one::<F64>(&input, 0.5), expect);
        // Just under a half still rounds towards zero.
        assert_eq!(mix_one::<I32>(&[-5, 5, -7, 7], 0.25), [-1, 1, -2, 2]);
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
    fn a_ramp_continues_over_two_mix_calls_without_a_step() {
        // Two blocks of four frames, the second starting at the gain the
        // first ended on, give the same as one ramp over eight frames.
        let input = [[1 << 24; 2]; 4];
        let mut acc = [[0i64; 2]; 4];
        let mut out = [[0; 2]; 4];
        let mut got = Vec::new();
        for (from, to) in [(0.0, 0.5), (0.5, 1.0)] {
            mix::<I32>(&[(&input, Ramp { from, to })], &mut acc, &mut out);
            got.extend(out.iter().map(|f| f[0]));
        }
        let expect: Vec<i32> = (0..8).map(|i| i << 21).collect();
        assert_eq!(got, expect);
    }

    #[test]
    fn i32_sums_many_sources_at_a_large_gain_and_saturates_once() {
        // 64 sources at a gain of 1000: each product is far over full scale,
        // and the sum is exact in the wide accumulator until the end.
        let up = [[1 << 20, i32::MAX]];
        let down = [[-(1 << 20), i32::MIN]];
        let r = Ramp::flat(1000.0);
        let mut acc = [[0i64; 2]];
        let mut out = [[0; 2]];
        let mut sources = vec![(&up[..], r); 32];
        sources.extend(vec![(&down[..], r); 32]);
        mix::<I32>(&sources, &mut acc, &mut out);
        // MAX and MIN differ by one, so 32 pairs leave -32 at a gain of 1000.
        assert_eq!(out, [[0, -32_000]]);
        mix::<I32>(&sources[..33], &mut acc, &mut out);
        assert_eq!(out, [[i32::MAX, i32::MAX]]);
        mix::<I32>(&sources[31..], &mut acc, &mut out);
        assert_eq!(out, [[i32::MIN, i32::MIN]]);
    }

    fn scaled<P: Processing>(values: &[i32], ramp: Ramp) -> Vec<i32> {
        let mut acc = vec![P::zero(); values.len()];
        for (a, &v) in acc.iter_mut().zip(values) {
            P::accumulate(a, v, P::gain(1.0));
        }
        scale_wide::<P>(&mut acc, 2, ramp);
        let mut out = vec![0; values.len()];
        mix_finish_wide::<P>(&acc, &mut out);
        out
    }

    #[test]
    fn scaling_a_wide_block_follows_the_ramp_and_leaves_it_at_unity() {
        fn check<P: Processing>() {
            // Exact in every format.
            const ODD_EXACT: i32 = 0x1234_5000;
            let values = [ODD_EXACT, -ODD_EXACT, 1 << 20, -(1 << 20), 4096, 0];
            assert_eq!(scaled::<P>(&values, Ramp::flat(1.0)), values);
            let half: Vec<i32> = values.iter().map(|v| v / 2).collect();
            assert_eq!(scaled::<P>(&values, Ramp::flat(0.5)), half);
            // Three frames from 1 to 0: 1, 2/3 and 1/3 of each.
            let ramp = scaled::<P>(&values, Ramp { from: 1.0, to: 0.0 });
            assert_eq!(ramp[..2], values[..2]);
            assert!((f64::from(ramp[3]) + f64::from(1 << 20) * 2.0 / 3.0).abs() < 2.0);
            assert_eq!(ramp[4], 1365);
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_float_that_is_not_a_number_finishes_as_silence() {
        assert_eq!(F32::finish(f32::NAN), 0);
        assert_eq!(F64::finish(f64::NAN), 0);
        assert_eq!(F32::finish(f32::INFINITY), i32::MAX);
        assert_eq!(F32::finish(f32::NEG_INFINITY), i32::MIN);
        assert_eq!(F64::finish(f64::INFINITY), i32::MAX);
        assert_eq!(F64::finish(f64::NEG_INFINITY), i32::MIN);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic]
    fn mix_refuses_an_accumulator_shorter_than_the_output() {
        let mut acc = [[0i64; 2]; 1];
        let mut out = [[7; 2]; 2];
        mix::<I32>(&[], &mut acc, &mut out);
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
        let mut store = BlockStore::new(frames.len(), 2);
        let mut out = vec![[0; 2]; frames.len()];
        mix_clear::<P>(&mut acc);
        mix_add::<P>(&frames, Ramp::flat(1.0), &mut acc);
        let block = P::to_block(acc.as_flattened_mut(), 2, &mut store);
        look(&block);
        P::from_block(block, acc.as_flattened_mut());
        mix_finish::<P>(&acc, &mut out);
        out.iter().map(|f| f[0]).collect()
    }

    #[test]
    fn every_s24_value_survives_the_block_round_trip_in_f32() {
        // Every S24 value, left aligned in an S32 sample.
        let v: Vec<i32> = (-(1 << 23)..(1 << 23)).map(|k| k << 8).collect();
        let got = block_round_trip::<F32>(&v, |block| {
            let Samples::F32(b) = &block.samples else {
                panic!("not an f32 block")
            };
            for (f, &s) in b.chunks(2).zip(v.iter()) {
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
            let Samples::F64(b) = &block.samples else {
                panic!("not an f64 block")
            };
            for (f, &s) in b.chunks(2).zip(v.iter()) {
                assert_eq!(f[0], f64::from(s) / 2_147_483_648.0);
            }
        });
        assert_eq!(got, v);
        let got = block_round_trip::<I32>(&v, |block| {
            let Samples::I32(b) = &block.samples else {
                panic!("not an i32 block")
            };
            assert_eq!(b.chunks(2).map(|f| f[0]).collect::<Vec<_>>(), v);
        });
        assert_eq!(got, v);
    }

    #[test]
    fn db_to_gain_known_values() {
        assert_eq!(db_to_gain(0.0), 1.0);
        assert!((db_to_gain(-20.0) - 0.1).abs() < 1e-6);
    }

    /// `mix_add_acc` on what `mix_add` made of S32 frames at unity gives
    /// what `mix_add` gives with the same ramp.
    fn adding_an_accumulator_matches_adding_its_frames<P: Processing>() {
        let frames: Vec<[i32; 2]> = EDGES.iter().map(|&v| [v, v ^ 0x55]).collect();
        let mut block = vec![[P::zero(); 2]; frames.len()];
        mix_add::<P>(&frames, Ramp::flat(1.0), &mut block);
        for ramp in [Ramp::flat(0.5), Ramp { from: 1.0, to: 0.0 }] {
            let mut want = vec![[P::zero(); 2]; frames.len()];
            let mut got = vec![[P::zero(); 2]; frames.len()];
            mix_add::<P>(&frames, ramp, &mut want);
            mix_add_acc::<P>(&block, ramp, &mut got);
            let (mut w, mut g) = (vec![[0; 2]; frames.len()], vec![[0; 2]; frames.len()]);
            mix_finish::<P>(&want, &mut w);
            mix_finish::<P>(&got, &mut g);
            assert_eq!(g, w, "{ramp:?}");
        }
    }

    #[test]
    fn adding_an_accumulator_is_adding_its_frames() {
        adding_an_accumulator_matches_adding_its_frames::<I32>();
        adding_an_accumulator_matches_adding_its_frames::<F32>();
        adding_an_accumulator_matches_adding_its_frames::<F64>();
    }

    #[test]
    fn a_float_accumulator_over_full_scale_is_added_unclipped() {
        let mut out = [[0; 2]];
        let mut acc = [[0.0f32; 2]];
        mix_add_acc::<F32>(&[[2.0, -3.0]], Ramp::flat(0.25), &mut acc);
        mix_finish::<F32>(&acc, &mut out);
        assert_eq!(out, [[1 << 30, -(3 << 29)]]);
        let mut acc = [[0.0f64; 2]];
        mix_add_acc::<F64>(&[[2.0, -3.0]], Ramp::flat(0.25), &mut acc);
        mix_finish::<F64>(&acc, &mut out);
        assert_eq!(out, [[1 << 30, -(3 << 29)]]);
    }

    /// Gains that send left to channel `l` and right to channel `r` at 0 dB.
    fn route(l: usize, r: usize) -> [[Ramp; MAX_CHANNELS]; 2] {
        let mut gains = [[Ramp::flat(0.0); MAX_CHANNELS]; 2];
        gains[0][l] = Ramp::flat(1.0);
        gains[1][r] = Ramp::flat(1.0);
        gains
    }

    /// Mixes `frames` into a bus at unity, through `gains` into `width`
    /// channels, and finishes.
    fn through_matrix<P: Processing>(
        frames: &[[i32; 2]],
        gains: &[[Ramp; MAX_CHANNELS]; 2],
        width: usize,
    ) -> Vec<i32> {
        let mut bus = vec![[P::zero(); 2]; frames.len()];
        mix_add::<P>(frames, Ramp::flat(1.0), &mut bus);
        let mut acc = vec![P::zero(); frames.len() * width];
        matrix_add::<P>(&bus, gains, width, &mut acc);
        let mut out = vec![0; acc.len()];
        mix_finish_wide::<P>(&acc, &mut out);
        out
    }

    #[test]
    fn a_route_at_0_db_passes_every_s32_value_unchanged() {
        let mut v = EDGES.to_vec();
        v.extend([0x1234_5679, -0x1234_5679, 0x7FFF_FF01]);
        let frames: Vec<[i32; 2]> = v.iter().map(|&s| [s, !s]).collect();
        let want: Vec<i32> = frames.iter().flat_map(|f| [f[1], 0, f[0]]).collect();
        assert_eq!(through_matrix::<I32>(&frames, &route(2, 0), 3), want);
        assert_eq!(through_matrix::<F64>(&frames, &route(2, 0), 3), want);
        let s24: Vec<[i32; 2]> = (-4..4).map(|k| [k << 28, (k << 8) - 256]).collect();
        let want: Vec<i32> = s24.iter().flat_map(|f| [f[1], 0, f[0]]).collect();
        assert_eq!(through_matrix::<F32>(&s24, &route(2, 0), 3), want);
    }

    #[test]
    fn a_wide_i32_mix_saturates_once_at_the_end() {
        // Both channels of the bus onto channel 1: MAX + MAX, and then the
        // other bus adds MIN + MIN, which leaves -2, not a clipped sum.
        let mut both = [[Ramp::flat(0.0); MAX_CHANNELS]; 2];
        both[0][0] = Ramp::flat(1.0);
        both[1][0] = Ramp::flat(1.0);
        let hi = [[i64::from(i32::MAX); 2]];
        let lo = [[i64::from(i32::MIN); 2]];
        let mut acc = [0i64; 1];
        matrix_add::<I32>(&hi, &both, 1, &mut acc);
        matrix_add::<I32>(&lo, &both, 1, &mut acc);
        let mut out = [0; 1];
        mix_finish_wide::<I32>(&acc, &mut out);
        assert_eq!(out, [-2]);
        let mut acc = [0i64; 1];
        matrix_add::<I32>(&hi, &both, 1, &mut acc);
        mix_finish_wide::<I32>(&acc, &mut out);
        assert_eq!(out, [i32::MAX]);
    }

    #[test]
    fn a_matrix_ramp_moves_over_the_block() {
        let mut gains = [[Ramp::flat(0.0); MAX_CHANNELS]; 2];
        gains[0][1] = Ramp { from: 0.0, to: 1.0 };
        let bus = [[1i64 << 20, 7]; 4];
        let mut acc = [0i64; 8];
        matrix_add::<I32>(&bus, &gains, 2, &mut acc);
        assert_eq!(acc, [0, 0, 0, 1 << 18, 0, 1 << 19, 0, 3 << 18]);
    }

    #[test]
    fn a_block_round_trip_keeps_three_channels_apart() {
        fn trip<P: Processing>(values: [P::Acc; 6])
        where
            P::Acc: PartialEq + std::fmt::Debug,
        {
            let mut acc = values;
            let mut store = BlockStore::new(2, 3);
            let block = P::to_block(&mut acc, 3, &mut store);
            assert_eq!(block.channels, 3);
            assert_eq!(block.frames(), 2);
            let mut back = [P::zero(); 6];
            P::from_block(block, &mut back);
            assert_eq!(back, values);
        }
        trip::<F64>([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        trip::<F32>([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        trip::<I32>([1, 2, 3, 4, 5, 6]);
    }
}
