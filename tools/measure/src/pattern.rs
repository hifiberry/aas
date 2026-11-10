//! A test stream whose every frame says where it belongs, and the comparison
//! of a capture against it.

/// Rounds of the Feistel network. Four make each output bit depend on every
/// input bit.
const ROUNDS: u64 = 4;
/// Frames checked after a jump before it counts as a break, not a difference.
const RESYNC_FRAMES: usize = 4;

/// A seeded pseudo-random stereo stream.
///
/// Frame `k` is a keyed permutation of `k`, so a captured frame can be turned
/// back into its position. The left and right samples are the two halves of
/// the permuted index, which covers the full range of each sample.
pub struct Pattern {
    bits: u32,
    seed: u64,
}

/// One step of SplitMix64. A good mix of 64 bits, used as a round function.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

impl Pattern {
    /// `bits` is 16, 24 or 32: the client's sample size.
    pub fn new(bits: u32, seed: u64) -> Pattern {
        assert!(matches!(bits, 16 | 24 | 32), "bits must be 16, 24 or 32");
        Pattern { bits, seed }
    }

    fn mask(&self) -> u64 {
        (1u64 << self.bits) - 1
    }

    fn round(&self, round: u64, half: u64) -> u64 {
        mix(half ^ mix(self.seed ^ round)) & self.mask()
    }

    /// Frame `index` as the client sends it: each sample right-aligned and
    /// sign-extended in an `i32`.
    pub fn client_frame(&self, index: u64) -> [i32; 2] {
        let mask = self.mask();
        let (mut left, mut right) = ((index >> self.bits) & mask, index & mask);
        for round in 0..ROUNDS {
            (left, right) = (right, left ^ self.round(round, right));
        }
        [left, right].map(|half| self.signed(half))
    }

    /// Frame `index` as S32 on the card.
    pub fn frame(&self, index: u64) -> [i32; 2] {
        self.client_frame(index).map(|s| s << (32 - self.bits))
    }

    /// The position of a card frame in the stream, if the client could have
    /// sent it. Any such frame has one; whether it fits its neighbours is
    /// for the caller to check.
    pub fn position(&self, frame: [i32; 2]) -> Option<u64> {
        let shift = 32 - self.bits;
        let low = (1u32 << shift).wrapping_sub(1);
        if frame.iter().any(|s| (*s as u32) & low != 0) {
            return None;
        }
        let [mut left, mut right] = frame.map(|s| u64::from((s as u32) >> shift));
        for round in (0..ROUNDS).rev() {
            (left, right) = (right ^ self.round(round, left), left);
        }
        Some((left << self.bits) | right)
    }

    /// Sign-extends a half of `bits` bits.
    fn signed(&self, half: u64) -> i32 {
        let shift = 32 - self.bits;
        ((half as u32) << shift) as i32 >> shift
    }
}

/// What a capture showed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Comparison {
    /// Captured frames that were held against the stream.
    pub compared: u64,
    /// Compared frames that were not the frame due.
    pub differing: u64,
    /// Places where the stream jumped to another position.
    pub breaks: u64,
    /// The stream position due at the first difference or break.
    pub first_fault: Option<u64>,
}

/// One place where a capture left the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The frame at capture index `at` was `got`, where frame `due` was
    /// expected.
    Difference { at: usize, due: u64, got: [i32; 2] },
    /// At capture index `at`, where frame `due` was expected, the stream
    /// went on from frame `to`.
    Break { at: usize, due: u64, to: u64 },
}

/// Compare `captured`, S32 from the card, with the first `sent` frames of
/// `pattern`.
///
/// Silence before the stream is skipped, and the first frame after it is
/// held against frame 0, so a stream that starts late or altered shows up.
/// From there each captured frame should be the next one. A frame that is
/// not, but that starts a run of the stream somewhere else, is a break and
/// the comparison follows the stream from there. Any other frame is a
/// difference. The comparison ends with the last frame sent.
pub fn compare(pattern: &Pattern, sent: u64, captured: &[[i32; 2]]) -> Comparison {
    walk(pattern, sent, captured, |_| {})
}

/// The first `limit` faults that `compare` counts, in order.
pub fn faults(pattern: &Pattern, sent: u64, captured: &[[i32; 2]], limit: usize) -> Vec<Fault> {
    let mut found = Vec::new();
    walk(pattern, sent, captured, |fault| {
        if found.len() < limit {
            found.push(fault);
        }
    });
    found
}

fn walk(
    pattern: &Pattern,
    sent: u64,
    captured: &[[i32; 2]],
    mut on_fault: impl FnMut(Fault),
) -> Comparison {
    let mut result = Comparison::default();
    let Some(mut at) = captured.iter().position(|f| *f != [0, 0]) else {
        return result;
    };
    let mut due = 0;
    while at < captured.len() && due < sent {
        if captured[at] == pattern.frame(due) {
            result.compared += 1;
            due += 1;
            at += 1;
        } else if let Some(to) = resync(pattern, sent, &captured[at..]) {
            on_fault(Fault::Break { at, due, to });
            result.first_fault.get_or_insert(due);
            result.breaks += 1;
            due = to;
        } else {
            on_fault(Fault::Difference {
                at,
                due,
                got: captured[at],
            });
            result.first_fault.get_or_insert(due);
            result.compared += 1;
            result.differing += 1;
            due += 1;
            at += 1;
        }
    }
    result
}

/// The position `captured` continues the stream from, if its first frames
/// are a run of the stream.
fn resync(pattern: &Pattern, sent: u64, captured: &[[i32; 2]]) -> Option<u64> {
    let start = pattern.position(captured[0])?;
    let run = (sent.checked_sub(start)? as usize)
        .min(captured.len())
        .min(RESYNC_FRAMES);
    let fits = (0..run).all(|i| captured[i] == pattern.frame(start + i as u64));
    (run > 0 && fits).then_some(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENT: u64 = 10_000;

    /// Silence, then the stream, then silence, as a capture shows it.
    fn capture(pattern: &Pattern) -> Vec<[i32; 2]> {
        let mut c = vec![[0, 0]; 333];
        c.extend((0..SENT).map(|k| pattern.frame(k)));
        c.extend([[0, 0]; 500]);
        c
    }

    #[test]
    fn identical_streams_have_no_differences_or_breaks() {
        for bits in [16, 24, 32] {
            let p = Pattern::new(bits, 7);
            let found = compare(&p, SENT, &capture(&p));
            let want = Comparison {
                compared: SENT,
                differing: 0,
                breaks: 0,
                first_fault: None,
            };
            assert_eq!(found, want, "{bits} bits");
        }
    }

    #[test]
    fn one_changed_sample_is_one_difference() {
        for bits in [16, 24, 32] {
            let p = Pattern::new(bits, 7);
            let mut c = capture(&p);
            // The lowest bit the client can set.
            c[333 + 4000][1] ^= 1 << (32 - bits);
            let found = compare(&p, SENT, &c);
            let want = Comparison {
                compared: SENT,
                differing: 1,
                breaks: 0,
                first_fault: Some(4000),
            };
            assert_eq!(found, want, "{bits} bits");
        }
    }

    #[test]
    fn a_missing_block_is_one_break() {
        for bits in [16, 24, 32] {
            let p = Pattern::new(bits, 7);
            let mut c = capture(&p);
            c.drain(333 + 5000..333 + 5256);
            let found = compare(&p, SENT, &c);
            let want = Comparison {
                compared: SENT - 256,
                differing: 0,
                breaks: 1,
                first_fault: Some(5000),
            };
            assert_eq!(found, want, "{bits} bits");
        }
    }

    #[test]
    fn a_silent_gap_is_a_break_after_its_frames() {
        let p = Pattern::new(16, 7);
        let mut c = capture(&p);
        let at = 333 + 6000;
        c.splice(at..at, [[0, 0]; 64]);
        let found = compare(&p, SENT, &c);
        assert_eq!(found.differing, 64);
        assert_eq!(found.breaks, 1);
    }

    #[test]
    fn low_bits_the_client_cannot_set_are_a_difference() {
        let p = Pattern::new(24, 7);
        let mut c = capture(&p);
        c[333 + 10][0] ^= 1;
        assert_eq!(compare(&p, SENT, &c).differing, 1);
    }

    #[test]
    fn card_frame_is_the_client_frame_shifted_left() {
        for bits in [16, 24, 32] {
            let p = Pattern::new(bits, 3);
            for k in 0..1000 {
                let client = p.client_frame(k);
                assert_eq!(p.frame(k), client.map(|s| s << (32 - bits)), "{bits}");
                let limit = 1i64 << (bits - 1);
                for s in client {
                    assert!((-limit..limit).contains(&i64::from(s)), "{bits}: {s}");
                }
            }
        }
    }

    #[test]
    fn the_stream_reaches_full_scale() {
        for bits in [16, 24, 32] {
            let p = Pattern::new(bits, 1);
            let peak = (0..10_000)
                .flat_map(|k| p.frame(k))
                .map(|s| i64::from(s).abs())
                .max()
                .unwrap();
            assert!(peak > i64::from(i32::MAX) / 100 * 99, "{bits}: {peak}");
        }
    }

    #[test]
    fn every_frame_gives_back_its_position() {
        for bits in [16, 24, 32] {
            let p = Pattern::new(bits, 5);
            for k in (0..1000).chain([u64::MAX >> (64 - 2 * bits)]) {
                assert_eq!(p.position(p.frame(k)), Some(k), "{bits}");
            }
        }
    }

    #[test]
    fn a_stream_that_starts_late_is_a_break() {
        let p = Pattern::new(32, 7);
        let mut c = capture(&p);
        c.drain(333..333 + 100);
        let found = compare(&p, SENT, &c);
        assert_eq!((found.differing, found.breaks), (0, 1));
    }

    #[test]
    fn faults_are_listed_in_order() {
        let p = Pattern::new(16, 7);
        let mut c = capture(&p);
        c[333 + 10] = [1 << 16, 0];
        c.drain(333 + 50..333 + 60);
        let found = faults(&p, SENT, &c, 10);
        let want = vec![
            Fault::Difference {
                at: 333 + 10,
                due: 10,
                got: [1 << 16, 0],
            },
            Fault::Break {
                at: 333 + 50,
                due: 50,
                to: 60,
            },
        ];
        assert_eq!(found, want);
        assert_eq!(faults(&p, SENT, &c, 1).len(), 1);
    }

    #[test]
    fn seeds_give_different_streams() {
        let a = Pattern::new(32, 1);
        let b = Pattern::new(32, 2);
        assert_ne!(a.frame(0), b.frame(0));
    }
}
