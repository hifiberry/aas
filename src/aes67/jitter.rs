//! Received RTP audio, held by media timestamp until the input thread
//! takes it. Everything is allocated in `new`; `put` and `take` never
//! allocate.

use super::rtp::ts_diff;

/// Ring size in frames. A power of two, a little over `HORIZON`.
const RING: usize = 16384;
/// How far ahead of `read_ts` a packet may start: 200 ms at 48 kHz.
const HORIZON: i32 = 9600;

/// What `put` did with a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    Stored,
    /// Entirely older than the read position; dropped.
    Late,
    /// Starts more than 200 ms ahead of the read position; dropped.
    TooEarly,
}

pub struct Jitter {
    frames: Vec<[i32; 2]>,
    /// The timestamp each slot was written for.
    stamps: Vec<u32>,
    /// Whether the slot holds a frame not yet taken.
    present: Vec<bool>,
    anchored: bool,
    read_ts: u32,
    /// One past the newest stored frame's timestamp.
    end_ts: u32,
    lost: u64,
    late: u64,
}

impl Default for Jitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Jitter {
    pub fn new() -> Jitter {
        Jitter {
            frames: vec![[0; 2]; RING],
            stamps: vec![0; RING],
            present: vec![false; RING],
            anchored: false,
            read_ts: 0,
            end_ts: 0,
            lost: 0,
            late: 0,
        }
    }

    /// Stores a packet at its timestamp. Until the first `anchor`, the
    /// first packet anchors the read position at its own timestamp.
    pub fn put(&mut self, ts: u32, frames: &[[i32; 2]]) -> Put {
        if !self.anchored {
            self.anchor(ts);
        }
        let d = ts_diff(ts, self.read_ts);
        if d >= HORIZON {
            return Put::TooEarly;
        }
        // Frames before read_ts are skipped.
        let skip = if d < 0 { (-(d as i64)) as usize } else { 0 };
        if skip >= frames.len() {
            self.late += 1;
            return Put::Late;
        }
        for (i, f) in frames.iter().enumerate().skip(skip) {
            let t = ts.wrapping_add(i as u32);
            let slot = t as usize % RING;
            self.frames[slot] = *f;
            self.stamps[slot] = t;
            self.present[slot] = true;
        }
        let end = ts.wrapping_add(frames.len() as u32);
        if ts_diff(end, self.end_ts) > 0 {
            self.end_ts = end;
        }
        Put::Stored
    }

    /// Fills `out` from `read_ts` on. A frame that never arrived reads as
    /// zeros and counts once in `lost` (the unit is frames).
    pub fn take(&mut self, out: &mut [[i32; 2]]) {
        for o in out.iter_mut() {
            let t = self.read_ts;
            let slot = t as usize % RING;
            if self.present[slot] && self.stamps[slot] == t {
                *o = self.frames[slot];
            } else {
                *o = [0; 2];
                self.lost += 1;
            }
            self.present[slot] = false;
            self.read_ts = t.wrapping_add(1);
        }
    }

    /// The next `take` starts at `ts`; whatever is stored is dropped.
    pub fn anchor(&mut self, ts: u32) {
        self.present.fill(false);
        self.anchored = true;
        self.read_ts = ts;
        self.end_ts = ts;
    }

    pub fn read_ts(&self) -> u32 {
        self.read_ts
    }

    /// Frames from `read_ts` to the end of the newest stored frame.
    pub fn depth(&self) -> u32 {
        ts_diff(self.end_ts, self.read_ts).max(0) as u32
    }

    /// (lost frames, late packets).
    pub fn counts(&self) -> (u64, u64) {
        (self.lost, self.late)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_no_alloc::assert_no_alloc;

    fn pkt(start: i32, n: usize) -> Vec<[i32; 2]> {
        (0..n as i32).map(|i| [start + i, -(start + i)]).collect()
    }

    fn take(j: &mut Jitter, n: usize) -> Vec<[i32; 2]> {
        let mut out = vec![[7; 2]; n];
        j.take(&mut out);
        out
    }

    #[test]
    fn in_order_packets_come_out_in_order() {
        let mut j = Jitter::new();
        assert_eq!(j.put(1000, &pkt(1, 48)), Put::Stored);
        assert_eq!(j.put(1048, &pkt(49, 48)), Put::Stored);
        assert_eq!(j.depth(), 96);
        assert_eq!(take(&mut j, 96), pkt(1, 96));
        assert_eq!(j.depth(), 0);
        assert_eq!(j.counts(), (0, 0));
    }

    #[test]
    fn reordered_packets_are_placed_by_timestamp() {
        let mut j = Jitter::new();
        j.put(1000, &pkt(1, 48));
        j.put(1096, &pkt(97, 48));
        j.put(1048, &pkt(49, 48));
        assert_eq!(take(&mut j, 144), pkt(1, 144));
        assert_eq!(j.counts(), (0, 0));
    }

    #[test]
    fn a_lost_packet_is_silence_in_its_place() {
        let mut j = Jitter::new();
        j.put(1000, &pkt(1, 48));
        j.put(1096, &pkt(97, 48));
        let out = take(&mut j, 144);
        assert_eq!(&out[..48], &pkt(1, 48)[..]);
        assert!(out[48..96].iter().all(|f| *f == [0; 2]));
        assert_eq!(&out[96..], &pkt(97, 48)[..]);
        assert_eq!(j.counts(), (48, 0));
    }

    #[test]
    fn a_packet_older_than_read_ts_is_late() {
        let mut j = Jitter::new();
        j.put(1000, &pkt(1, 48));
        take(&mut j, 48);
        assert_eq!(j.put(1000, &pkt(1, 48)), Put::Late);
        assert_eq!(j.counts(), (0, 1));
        // A packet straddling read_ts keeps its newer part.
        assert_eq!(j.put(1024, &pkt(25, 48)), Put::Stored);
        assert_eq!(j.counts(), (0, 1));
        assert_eq!(take(&mut j, 24), pkt(49, 24));
        assert_eq!(j.counts(), (0, 1));
    }

    #[test]
    fn wrap_in_the_middle() {
        let mut j = Jitter::new();
        let start = u32::MAX - 23;
        j.put(start, &pkt(1, 48));
        j.put(start.wrapping_add(48), &pkt(49, 48));
        assert_eq!(take(&mut j, 96), pkt(1, 96));
        assert_eq!(j.read_ts(), start.wrapping_add(96));
        assert_eq!(j.counts(), (0, 0));
    }

    #[test]
    fn a_jump_beyond_capacity_is_too_early_and_does_not_corrupt() {
        let mut j = Jitter::new();
        j.put(1000, &pkt(1, 48));
        assert_eq!(j.put(1000 + 9600, &pkt(5000, 48)), Put::TooEarly);
        assert_eq!(j.put(1000 + 20000, &pkt(5000, 48)), Put::TooEarly);
        assert_eq!(j.depth(), 48);
        assert_eq!(take(&mut j, 48), pkt(1, 48));
        assert_eq!(j.counts(), (0, 0));
        // Just inside the horizon is accepted.
        assert_eq!(j.put(1048 + 9599, &pkt(1, 1)), Put::Stored);
    }

    #[test]
    fn anchor_drops_what_is_stored() {
        let mut j = Jitter::new();
        j.put(1000, &pkt(1, 48));
        j.anchor(5000);
        assert_eq!(j.read_ts(), 5000);
        assert_eq!(j.depth(), 0);
        assert_eq!(j.put(5000, &pkt(9, 4)), Put::Stored);
        assert_eq!(take(&mut j, 4), pkt(9, 4));
    }

    #[test]
    fn take_and_put_do_not_allocate() {
        let mut j = Jitter::new();
        let p = pkt(1, 48);
        let mut out = vec![[0; 2]; 48];
        assert_no_alloc(|| {
            for i in 0..400u32 {
                j.put(i * 48, &p);
                j.put(i * 48 + 100_000, &p);
                j.take(&mut out);
            }
            j.anchor(0);
        });
    }
}
