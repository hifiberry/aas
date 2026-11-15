//! Balance: turns one channel down to favour the other.

use super::{Block, Formats, Param, ParamKind, Plugin, Samples, Status};

/// Towards one side the other channel's gain falls linearly to 0. The near
/// side stays at 1, so the centre leaves the audio untouched. A new value is
/// reached by a linear ramp over 10 ms, a step per frame, so a change does
/// not click.
pub struct Balance {
    params: [Param; 1],
    /// The value set, where the ramp goes.
    target: f64,
    /// The value the last frame played with.
    balance: f64,
    /// What the ramp adds per frame.
    step: f64,
    /// The frames left of the ramp; 0 when none runs.
    left: u32,
    /// The frames a ramp lasts: 10 ms at the rate.
    ramp_frames: u32,
}

impl Balance {
    /// A balance for a stream at `rate`, at the centre.
    pub fn new(rate: u32) -> Balance {
        Balance {
            params: [Param {
                name: "balance".into(),
                unit: String::new(),
                kind: ParamKind::Float,
                min: -1.0,
                max: 1.0,
                default: 0.0,
            }],
            target: 0.0,
            balance: 0.0,
            step: 0.0,
            left: 0,
            ramp_frames: (rate / 100).max(1),
        }
    }

    /// The gains of the left and right channel at balance `b`.
    fn gains(b: f64) -> (f64, f64) {
        if b > 0.0 {
            (1.0 - b, 1.0)
        } else {
            (1.0, 1.0 + b)
        }
    }

    /// The balance of the next frame: one step on, the target once reached.
    /// Counted in frames, so rounding cannot make it end early or late.
    fn advance(&mut self) -> f64 {
        if self.left <= 1 {
            self.balance = self.target;
            self.left = 0;
        } else {
            self.balance += self.step;
            self.left -= 1;
        }
        self.balance
    }

    /// Ramps a block: every frame at its own balance.
    fn ramp(&mut self, samples: Samples<'_>) {
        match samples {
            Samples::I32(d) => {
                for f in d.chunks_exact_mut(2) {
                    let (gl, gr) = Self::gains(self.advance());
                    // The near side at 1 is left alone, as without a ramp.
                    if gl != 1.0 {
                        f[0] = scale(f[0], q32(gl));
                    }
                    if gr != 1.0 {
                        f[1] = scale(f[1], q32(gr));
                    }
                }
            }
            Samples::F32(d) => {
                for f in d.chunks_exact_mut(2) {
                    let (gl, gr) = Self::gains(self.advance());
                    f[0] *= gl as f32;
                    f[1] *= gr as f32;
                }
            }
            Samples::F64(d) => {
                for f in d.chunks_exact_mut(2) {
                    let (gl, gr) = Self::gains(self.advance());
                    f[0] *= gl;
                    f[1] *= gr;
                }
            }
        }
    }
}

impl Default for Balance {
    fn default() -> Self {
        Balance::new(48_000)
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
            self.target = value.clamp(-1.0, 1.0);
            self.step = (self.target - self.balance) / f64::from(self.ramp_frames);
            self.left = if self.target == self.balance {
                0
            } else {
                self.ramp_frames
            };
        }
    }

    /// At the centre with no ramp running: a ramp back to the centre still
    /// changes the audio until it ends.
    fn neutral(&self) -> bool {
        self.target == 0.0 && self.balance == 0.0
    }

    fn latency_frames(&self) -> u32 {
        0
    }

    fn process(&mut self, block: Block<'_>) -> Status {
        debug_assert_eq!(block.channels, 2);
        if self.neutral() {
            return Status::default();
        }
        if self.left > 0 {
            self.ramp(block.samples);
            // The ramp may have ended at the centre, so the plugin is
            // neutral after this block, but the block itself was changed.
            return Status {
                changed: true,
                ..Status::default()
            };
        }
        let (gl, gr) = Self::gains(self.balance);
        let (ql, qr) = (q32(gl), q32(gr));
        // The channel at gain 1 is left alone, so it stays bit-identical.
        match block.samples {
            Samples::I32(d) => {
                for f in d.chunks_exact_mut(2) {
                    if gl != 1.0 {
                        f[0] = scale(f[0], ql);
                    }
                    if gr != 1.0 {
                        f[1] = scale(f[1], qr);
                    }
                }
            }
            Samples::F32(d) => {
                for f in d.chunks_exact_mut(2) {
                    if gl != 1.0 {
                        f[0] *= gl as f32;
                    }
                    if gr != 1.0 {
                        f[1] *= gr as f32;
                    }
                }
            }
            Samples::F64(d) => {
                for f in d.chunks_exact_mut(2) {
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

    /// Keeps the value and the ramp as they are: a balance keeps nothing
    /// from earlier blocks. The blocks that did not pass it played what it
    /// would have at the value it is at, as the bit-perfect copy path plays
    /// a centred balance, so a ramp to a new value set meanwhile still runs
    /// from there, and a jump would click.
    fn reset(&mut self) {}

    /// Jumps to the value set: the config's and the stored values apply from
    /// the first frame.
    fn settle(&mut self) {
        self.balance = self.target;
        self.left = 0;
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

    /// A balance at `b` with its ramp already run, as a plugin of a chain
    /// that has just been built, so `b` applies at once.
    fn with(b: f64) -> Balance {
        let mut p = Balance::new(48_000);
        p.set_param(0, b);
        p.settle();
        p
    }

    #[test]
    fn centre_is_neutral_and_changes_nothing() {
        let mut p = Balance::new(48_000);
        assert!(p.neutral());
        assert_eq!(p.latency_frames(), 0);
        let mut i = [[1, -7], [i32::MAX, i32::MIN]];
        let mut f = [[0.3f32, -0.7], [f32::MIN_POSITIVE, 1e30]];
        let mut d = [[0.1f64, -0.9], [1.0 / 3.0, f64::MAX]];
        let (i0, f0, d0) = (i, f, d);
        assert_eq!(
            p.process(Block {
                channels: 2,
                samples: Samples::I32(i.as_flattened_mut())
            }),
            Status::default()
        );
        p.process(Block {
            channels: 2,
            samples: Samples::F32(f.as_flattened_mut()),
        });
        p.process(Block {
            channels: 2,
            samples: Samples::F64(d.as_flattened_mut()),
        });
        assert_eq!(i, i0);
        assert_eq!(
            f.map(|x| x.map(f32::to_bits)),
            f0.map(|x| x.map(f32::to_bits))
        );
        assert_eq!(
            d.map(|x| x.map(f64::to_bits)),
            d0.map(|x| x.map(f64::to_bits))
        );
        // Back at the centre after a move, once the ramp there has run.
        let mut p = with(0.5);
        assert!(!p.neutral());
        ramped(&mut p, 1);
        p.set_param(0, 0.0);
        assert!(!p.neutral(), "neutral while the ramp to the centre runs");
        let mut d = [[1.0f64, 1.0]; 480];
        let status = p.process(Block {
            channels: 2,
            samples: Samples::F64(d.as_flattened_mut()),
        });
        assert!(status.changed, "the block the ramp ended in was changed");
        assert!(p.neutral());
    }

    #[test]
    fn full_left_silences_the_right_channel_and_keeps_the_left() {
        let mut p = with(-1.0);
        let mut i = [[1000, -7], [i32::MAX, i32::MIN]];
        p.process(Block {
            channels: 2,
            samples: Samples::I32(i.as_flattened_mut()),
        });
        assert_eq!(i, [[1000, 0], [i32::MAX, 0]]);
        let mut f = [[0.5f32, -0.5]];
        p.process(Block {
            channels: 2,
            samples: Samples::F32(f.as_flattened_mut()),
        });
        assert_eq!(f, [[0.5, 0.0]]);
        let mut d = [[0.25f64, 0.75]];
        p.process(Block {
            channels: 2,
            samples: Samples::F64(d.as_flattened_mut()),
        });
        assert_eq!(d, [[0.25, 0.0]]);
    }

    #[test]
    fn half_right_halves_the_left_channel() {
        let mut p = with(0.5);
        let mut i = [[1000, -7], [-1001, 5]];
        p.process(Block {
            channels: 2,
            samples: Samples::I32(i.as_flattened_mut()),
        });
        // Halves round to the nearest, away from zero.
        assert_eq!(i, [[500, -7], [-501, 5]]);
        let mut f = [[0.5f32, -0.5]];
        p.process(Block {
            channels: 2,
            samples: Samples::F32(f.as_flattened_mut()),
        });
        assert_eq!(f, [[0.25, -0.5]]);
        let mut d = [[0.5f64, 0.75]];
        p.process(Block {
            channels: 2,
            samples: Samples::F64(d.as_flattened_mut()),
        });
        assert_eq!(d, [[0.25, 0.75]]);
    }

    #[test]
    fn a_value_outside_the_range_is_clamped() {
        let mut p = with(7.0);
        let mut d = [[1.0f64, 1.0]];
        p.process(Block {
            channels: 2,
            samples: Samples::F64(d.as_flattened_mut()),
        });
        assert_eq!(d, [[0.0, 1.0]]);
        p.set_param(0, -7.0);
        ramped(&mut p, 480);
        let mut d = [[1.0f64, 1.0]];
        p.process(Block {
            channels: 2,
            samples: Samples::F64(d.as_flattened_mut()),
        });
        assert_eq!(d, [[1.0, 0.0]]);
        p.set_param(3, 0.5);
        p.set_param(0, f64::NAN);
        p.process(Block {
            channels: 2,
            samples: Samples::F64(d.as_flattened_mut()),
        });
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
        p.process(Block {
            channels: 2,
            samples: Samples::I32(i.as_flattened_mut()),
        });
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
        p.process(Block {
            channels: 2,
            samples: Samples::I32(i.as_flattened_mut()),
        });
        assert_eq!(i, [[0, i32::MIN], [0, -7]]);
        let mut f = [[0.5f32, 0.1]];
        p.process(Block {
            channels: 2,
            samples: Samples::F32(f.as_flattened_mut()),
        });
        assert_eq!(f[0][0], 0.0);
        assert_eq!(f[0][1].to_bits(), 0.1f32.to_bits());
        let mut d = [[0.5f64, 1.0 / 3.0]];
        p.process(Block {
            channels: 2,
            samples: Samples::F64(d.as_flattened_mut()),
        });
        assert_eq!(d[0][0], 0.0);
        assert_eq!(d[0][1].to_bits(), (1.0f64 / 3.0).to_bits());
    }

    /// The left and right samples of `frames` frames of 1.0 after `p`.
    fn ramped(p: &mut Balance, frames: usize) -> Vec<[f64; 2]> {
        let mut d = vec![[1.0f64, 1.0]; frames];
        p.process(Block {
            channels: 2,
            samples: Samples::F64(d.as_flattened_mut()),
        });
        d
    }

    #[test]
    fn a_step_from_the_centre_to_full_right_ramps_over_10_ms() {
        let mut p = Balance::new(48_000);
        ramped(&mut p, 1);
        p.set_param(0, 1.0);
        assert!(!p.neutral());
        // Two blocks, so the ramp crosses a block boundary.
        let mut d = ramped(&mut p, 300);
        d.extend(ramped(&mut p, 300));
        let slope = 1.0 / 480.0;
        let mut prev = 1.0;
        for (i, f) in d.iter().enumerate() {
            assert!(
                (prev - f[0]).abs() <= slope + 1e-12,
                "frame {i}: a step of {} is above the ramp's {slope}",
                prev - f[0]
            );
            assert_eq!(f[1], 1.0, "frame {i}: the near side moved");
            prev = f[0];
        }
        assert!(d[0][0] < 1.0, "the ramp starts at once");
        assert!(d[478][0] > 0.0, "the ramp ends before 10 ms");
        assert!(d[479..].iter().all(|f| f[0] == 0.0), "not at 0 after 10 ms");
    }

    #[test]
    fn a_new_value_during_a_ramp_ramps_on_from_where_it_is() {
        let mut p = Balance::new(48_000);
        ramped(&mut p, 1);
        p.set_param(0, 1.0);
        let d = ramped(&mut p, 240);
        let half = d[239][0];
        assert!((half - 0.5).abs() < 1e-9, "{half}");
        p.set_param(0, 0.0);
        let d = ramped(&mut p, 480);
        assert!((d[0][0] - half).abs() <= 0.5 / 480.0 + 1e-12);
        assert!(d.windows(2).all(|w| w[1][0] >= w[0][0]), "not monotonic");
        assert_eq!(d[479], [1.0, 1.0]);
        assert!(p.neutral());
    }

    #[test]
    fn an_i32_ramp_keeps_the_near_side_bit_identical() {
        let mut p = with(0.0);
        ramped(&mut p, 1);
        p.set_param(0, 1.0);
        let mut i = [[1 << 30, i32::MIN]; 480];
        p.process(Block {
            channels: 2,
            samples: Samples::I32(i.as_flattened_mut()),
        });
        assert!(i.iter().all(|f| f[1] == i32::MIN));
        assert_eq!(i[479][0], 0);
    }

    #[test]
    fn a_settled_value_applies_at_once() {
        let mut p = Balance::new(48_000);
        p.set_param(0, 0.5);
        p.settle();
        assert_eq!(ramped(&mut p, 1), [[0.5, 1.0]]);
    }

    #[test]
    fn a_reset_keeps_the_ramp_from_where_it_is() {
        let mut p = Balance::new(48_000);
        ramped(&mut p, 1);
        p.set_param(0, -1.0);
        p.reset();
        let d = ramped(&mut p, 480);
        assert!((d[0][1] - (1.0 - 1.0 / 480.0)).abs() < 1e-12, "{:?}", d[0]);
        assert_eq!(d[479], [1.0, 0.0]);
    }

    #[test]
    fn ramping_does_not_allocate() {
        let mut p = with(0.0);
        ramped(&mut p, 1);
        let mut i = [[1000, -7]; 64];
        let mut f = [[0.5f32, -0.5]; 64];
        let mut d = [[0.5f64, -0.5]; 64];
        assert_no_alloc(|| {
            p.set_param(0, 1.0);
            p.process(Block {
                channels: 2,
                samples: Samples::I32(i.as_flattened_mut()),
            });
            p.process(Block {
                channels: 2,
                samples: Samples::F32(f.as_flattened_mut()),
            });
            p.set_param(0, -0.5);
            p.process(Block {
                channels: 2,
                samples: Samples::F64(d.as_flattened_mut()),
            });
            p.reset();
        });
    }

    #[test]
    fn it_describes_its_parameter() {
        let p = Balance::new(48_000);
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
            p.process(Block {
                channels: 2,
                samples: Samples::I32(i.as_flattened_mut()),
            });
            p.process(Block {
                channels: 2,
                samples: Samples::F32(f.as_flattened_mut()),
            });
            p.process(Block {
                channels: 2,
                samples: Samples::F64(d.as_flattened_mut()),
            });
            p.set_param(0, -0.5);
            p.process(Block {
                channels: 2,
                samples: Samples::F64(d.as_flattened_mut()),
            });
        });
    }
}
