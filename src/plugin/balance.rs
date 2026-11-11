//! Balance: turns one channel down to favour the other.

use super::{Block, Formats, Param, Plugin, Status};

/// Towards one side the other channel's gain falls linearly to 0. The near
/// side stays at 1, so the centre leaves the audio untouched.
pub struct Balance {
    params: [Param; 1],
    balance: f64,
}

impl Balance {
    pub fn new() -> Balance {
        Balance {
            params: [Param {
                name: "balance".into(),
                unit: String::new(),
                min: -1.0,
                max: 1.0,
                default: 0.0,
            }],
            balance: 0.0,
        }
    }

    /// The gains of the left and right channel.
    fn gains(&self) -> (f64, f64) {
        let b = self.balance;
        if b > 0.0 {
            (1.0 - b, 1.0)
        } else {
            (1.0, 1.0 + b)
        }
    }
}

impl Default for Balance {
    fn default() -> Self {
        Balance::new()
    }
}

impl Plugin for Balance {
    fn name(&self) -> &str {
        "balance"
    }

    fn formats(&self) -> Formats {
        Formats {
            i32: true,
            f32: true,
            f64: true,
        }
    }

    fn params(&self) -> &[Param] {
        &self.params
    }

    fn set_param(&mut self, index: usize, value: f64) {
        if index == 0 && !value.is_nan() {
            self.balance = value.clamp(-1.0, 1.0);
        }
    }

    fn neutral(&self) -> bool {
        self.balance == 0.0
    }

    fn latency_frames(&self) -> u32 {
        0
    }

    fn process(&mut self, block: Block<'_>) -> Status {
        if self.neutral() {
            return Status::default();
        }
        let (gl, gr) = self.gains();
        let (ql, qr) = (q32(gl), q32(gr));
        // The channel at gain 1 is left alone, so it stays bit-identical.
        match block {
            Block::I32(d) => {
                for f in d {
                    if gl != 1.0 {
                        f[0] = scale(f[0], ql);
                    }
                    if gr != 1.0 {
                        f[1] = scale(f[1], qr);
                    }
                }
            }
            Block::F32(d) => {
                for f in d {
                    if gl != 1.0 {
                        f[0] *= gl as f32;
                    }
                    if gr != 1.0 {
                        f[1] *= gr as f32;
                    }
                }
            }
            Block::F64(d) => {
                for f in d {
                    if gl != 1.0 {
                        f[0] *= gl;
                    }
                    if gr != 1.0 {
                        f[1] *= gr;
                    }
                }
            }
        }
        Status::default()
    }
}

/// A gain in 0 to 1 as a Q32 fixed point number.
fn q32(gain: f64) -> i64 {
    (gain * 4_294_967_296.0).round() as i64
}

/// `x * q / 2^32`, rounded to the nearest integer, halves away from zero.
/// The product needs up to 64 bits, so it is taken in 128.
fn scale(x: i32, q: i64) -> i32 {
    let p = i128::from(x).abs() * i128::from(q) + (1 << 31);
    let v = (p >> 32) as i32;
    if x < 0 { -v } else { v }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_no_alloc::assert_no_alloc;

    fn with(b: f64) -> Balance {
        let mut p = Balance::new();
        p.set_param(0, b);
        p
    }

    #[test]
    fn centre_is_neutral_and_changes_nothing() {
        let mut p = Balance::new();
        assert!(p.neutral());
        assert_eq!(p.latency_frames(), 0);
        let mut i = [[1, -7], [i32::MAX, i32::MIN]];
        let mut f = [[0.3f32, -0.7], [f32::MIN_POSITIVE, 1e30]];
        let mut d = [[0.1f64, -0.9], [1.0 / 3.0, f64::MAX]];
        let (i0, f0, d0) = (i, f, d);
        assert_eq!(p.process(Block::I32(&mut i)), Status::default());
        p.process(Block::F32(&mut f));
        p.process(Block::F64(&mut d));
        assert_eq!(i, i0);
        assert_eq!(
            f.map(|x| x.map(f32::to_bits)),
            f0.map(|x| x.map(f32::to_bits))
        );
        assert_eq!(
            d.map(|x| x.map(f64::to_bits)),
            d0.map(|x| x.map(f64::to_bits))
        );
        // Back at the centre after a move.
        let mut p = with(0.5);
        assert!(!p.neutral());
        p.set_param(0, 0.0);
        assert!(p.neutral());
    }

    #[test]
    fn full_left_silences_the_right_channel_and_keeps_the_left() {
        let mut p = with(-1.0);
        let mut i = [[1000, -7], [i32::MAX, i32::MIN]];
        p.process(Block::I32(&mut i));
        assert_eq!(i, [[1000, 0], [i32::MAX, 0]]);
        let mut f = [[0.5f32, -0.5]];
        p.process(Block::F32(&mut f));
        assert_eq!(f, [[0.5, 0.0]]);
        let mut d = [[0.25f64, 0.75]];
        p.process(Block::F64(&mut d));
        assert_eq!(d, [[0.25, 0.0]]);
    }

    #[test]
    fn half_right_halves_the_left_channel() {
        let mut p = with(0.5);
        let mut i = [[1000, -7], [-1001, 5]];
        p.process(Block::I32(&mut i));
        // Halves round to the nearest, away from zero.
        assert_eq!(i, [[500, -7], [-501, 5]]);
        let mut f = [[0.5f32, -0.5]];
        p.process(Block::F32(&mut f));
        assert_eq!(f, [[0.25, -0.5]]);
        let mut d = [[0.5f64, 0.75]];
        p.process(Block::F64(&mut d));
        assert_eq!(d, [[0.25, 0.75]]);
    }

    #[test]
    fn a_value_outside_the_range_is_clamped() {
        let mut p = with(7.0);
        let mut d = [[1.0f64, 1.0]];
        p.process(Block::F64(&mut d));
        assert_eq!(d, [[0.0, 1.0]]);
        p.set_param(0, -7.0);
        let mut d = [[1.0f64, 1.0]];
        p.process(Block::F64(&mut d));
        assert_eq!(d, [[1.0, 0.0]]);
        p.set_param(3, 0.5);
        p.set_param(0, f64::NAN);
        p.process(Block::F64(&mut d));
        assert_eq!(d, [[1.0, 0.0]]);
    }

    #[test]
    fn an_i32_block_is_scaled_exactly_and_rounded_to_nearest() {
        // 0.3 to the right leaves the left at 0.7, which is 3006477107 / 2^32.
        // 651086875 * 0.7 is 455760812.5 up to a 1e-9 error: one f64
        // multiply would land one LSB high.
        let mut p = with(0.3);
        let mut i = [
            [651_086_875, 5],
            [-651_086_875, 5],
            [i32::MAX, 5],
            [i32::MIN, 5],
            [1 << 30, 5],
            [-(1 << 30) + 1, 5],
            [1, 5],
            [-1, 5],
        ];
        p.process(Block::I32(&mut i));
        let left: Vec<i32> = i.iter().map(|f| f[0]).collect();
        assert_eq!(
            left,
            [
                455_760_812,
                -455_760_812,
                1_503_238_553,
                -1_503_238_554,
                751_619_277,
                -751_619_276,
                1,
                -1
            ]
        );
        assert!(i.iter().all(|f| f[1] == 5));
    }

    #[test]
    fn full_right_silences_the_left_channel_and_keeps_the_right_bit_identical() {
        let mut p = with(1.0);
        let mut i = [[1000, i32::MIN], [i32::MAX, -7]];
        p.process(Block::I32(&mut i));
        assert_eq!(i, [[0, i32::MIN], [0, -7]]);
        let mut f = [[0.5f32, 0.1]];
        p.process(Block::F32(&mut f));
        assert_eq!(f[0][0], 0.0);
        assert_eq!(f[0][1].to_bits(), 0.1f32.to_bits());
        let mut d = [[0.5f64, 1.0 / 3.0]];
        p.process(Block::F64(&mut d));
        assert_eq!(d[0][0], 0.0);
        assert_eq!(d[0][1].to_bits(), (1.0f64 / 3.0).to_bits());
    }

    #[test]
    fn it_describes_its_parameter() {
        let p = Balance::new();
        let q = &p.params()[0];
        assert_eq!(
            (q.name.as_str(), q.min, q.max, q.default),
            ("balance", -1.0, 1.0, 0.0)
        );
    }

    #[test]
    fn processing_does_not_allocate() {
        let mut p = with(0.25);
        let mut i = [[1000, -7]; 64];
        let mut f = [[0.5f32, -0.5]; 64];
        let mut d = [[0.5f64, -0.5]; 64];
        assert_no_alloc(|| {
            p.process(Block::I32(&mut i));
            p.process(Block::F32(&mut f));
            p.process(Block::F64(&mut d));
            p.set_param(0, -0.5);
            p.process(Block::F64(&mut d));
        });
    }
}
