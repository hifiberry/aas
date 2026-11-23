//! RTP packets, L16/L24 payloads and the media clock.

use super::sdp::Encoding;

pub struct Packet<'a> {
    pub seq: u16,
    pub ts: u32,
    pub ssrc: u32,
    pub payload_type: u8,
    pub payload: &'a [u8],
}

/// Parse an RTP datagram (version 2). CSRCs, an extension header and padding
/// are skipped; anything inconsistent gives `None`.
pub fn parse(datagram: &[u8]) -> Option<Packet<'_>> {
    if datagram.len() < 12 || datagram[0] >> 6 != 2 {
        return None;
    }
    let b0 = datagram[0];
    let mut start = 12 + 4 * (b0 & 0x0f) as usize;
    if b0 & 0x10 != 0 {
        let ext = datagram.get(start..start + 4)?;
        let words = u16::from_be_bytes([ext[2], ext[3]]) as usize;
        start += 4 + 4 * words;
    }
    let mut end = datagram.len();
    if b0 & 0x20 != 0 {
        let pad = *datagram.last()? as usize;
        if pad == 0 || pad > end {
            return None;
        }
        end -= pad;
    }
    if start > end {
        return None;
    }
    Some(Packet {
        seq: u16::from_be_bytes([datagram[2], datagram[3]]),
        ts: u32::from_be_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]),
        ssrc: u32::from_be_bytes([datagram[8], datagram[9], datagram[10], datagram[11]]),
        payload_type: datagram[1] & 0x7f,
        payload: &datagram[start..end],
    })
}

/// Decode the picked channels (1-based) of an L16/L24 payload into S32 frames.
/// Returns the frames written: whole frames only, at most `out.len()`.
/// Pick channels beyond `channels` read as 0.
pub fn decode(
    enc: Encoding,
    channels: u8,
    pick: [u8; 2],
    payload: &[u8],
    out: &mut [[i32; 2]],
) -> usize {
    let width = match enc {
        Encoding::L16 => 2,
        Encoding::L24 => 3,
        Encoding::Other => return 0,
    };
    let channels = channels as usize;
    if channels == 0 {
        return 0;
    }
    let stride = channels * width;
    let sample = |frame: &[u8], ch: u8| -> i32 {
        let ch = ch as usize;
        if ch == 0 || ch > channels {
            return 0;
        }
        let b = &frame[(ch - 1) * width..ch * width];
        match enc {
            Encoding::L16 => (i16::from_be_bytes([b[0], b[1]]) as i32) << 16,
            _ => i32::from_be_bytes([b[0], b[1], b[2], 0]),
        }
    };
    let mut n = 0;
    for (frame, slot) in payload.chunks_exact(stride).zip(out.iter_mut()) {
        *slot = [sample(frame, pick[0]), sample(frame, pick[1])];
        n += 1;
    }
    n
}

/// Append stereo frames as big-endian L24 (the top 24 bits of each sample).
pub fn encode_l24(frames: &[[i32; 2]], out: &mut Vec<u8>) {
    out.reserve(frames.len() * 6);
    for frame in frames {
        for s in frame {
            out.extend_from_slice(&s.to_be_bytes()[..3]);
        }
    }
}

/// A 12-byte RTP header: version 2, no padding, extension, CSRC or marker.
pub fn header(seq: u16, ts: u32, ssrc: u32, payload_type: u8) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0] = 0x80;
    h[1] = payload_type & 0x7f;
    h[2..4].copy_from_slice(&seq.to_be_bytes());
    h[4..8].copy_from_slice(&ts.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
    h
}

/// RTP timestamp for a PTP time: `(ptp_ns * 48000 / 1e9 + offset) mod 2^32`.
pub fn media_ts(ptp_ns: i128, offset: u32) -> u32 {
    let ticks = (ptp_ns * 48_000).div_euclid(1_000_000_000);
    (ticks + offset as i128).rem_euclid(1i128 << 32) as u32
}

/// `a - b` as a signed distance, correct across the 2^32 wrap.
pub fn ts_diff(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l24_round_trips_through_s32() {
        let frames = [[0x1234_5600, -0x0000_0100], [i32::MIN, 0x7fff_ff00]];
        let mut wire = Vec::new();
        encode_l24(&frames, &mut wire);
        assert_eq!(wire.len(), 12);
        assert_eq!(&wire[..6], &[0x12, 0x34, 0x56, 0xff, 0xff, 0xff]);
        let mut out = [[0; 2]; 2];
        assert_eq!(decode(Encoding::L24, 2, [1, 2], &wire, &mut out), 2);
        assert_eq!(out, frames);
        // low byte is dropped
        let mut wire = Vec::new();
        encode_l24(&[[0x1234_56ab, 0]], &mut wire);
        let mut out = [[0; 2]; 1];
        decode(Encoding::L24, 2, [1, 2], &wire, &mut out);
        assert_eq!(out[0][0], 0x1234_5600);
    }

    #[test]
    fn l16_decodes_to_s32() {
        let payload = [0x12, 0x34, 0xff, 0xfe];
        let mut out = [[0; 2]; 1];
        assert_eq!(decode(Encoding::L16, 2, [1, 2], &payload, &mut out), 1);
        assert_eq!(out[0], [0x1234_0000, -0x0002_0000]);
    }

    #[test]
    fn pick_channels_from_eight() {
        let mut payload = Vec::new();
        for f in 0..2u8 {
            for c in 1..=8u8 {
                payload.extend_from_slice(&[f, c, 0]);
            }
        }
        let mut out = [[0; 2]; 2];
        assert_eq!(decode(Encoding::L24, 8, [3, 7], &payload, &mut out), 2);
        assert_eq!(out[0], [(3 << 8) << 8, (7 << 8) << 8]);
        assert_eq!(out[1], [(0x0103 << 8) << 8, (0x0107 << 8) << 8]);
        // mono feeds both
        let mono = [0x00, 0x01, 0x00, 0x00, 0x02, 0x00];
        let mut out = [[0; 2]; 2];
        assert_eq!(decode(Encoding::L24, 1, [1, 1], &mono, &mut out), 2);
        assert_eq!(out, [[0x100 << 8; 2], [0x200 << 8; 2]]);
    }

    #[test]
    fn decode_never_panics_on_odd_input() {
        let mut out = [[0; 2]; 4];
        // 7 bytes of 2ch L24 is one whole frame
        assert_eq!(decode(Encoding::L24, 2, [1, 2], &[0; 7], &mut out), 1);
        // pick beyond channels reads 0
        let p = [1, 2, 3, 4, 5, 6];
        assert_eq!(decode(Encoding::L24, 2, [1, 9], &p, &mut out), 1);
        assert_eq!(out[0][1], 0);
        // out shorter than payload
        let mut short = [[0; 2]; 1];
        assert_eq!(decode(Encoding::L24, 2, [1, 2], &[0; 24], &mut short), 1);
        assert_eq!(decode(Encoding::Other, 2, [1, 2], &p, &mut out), 0);
        assert_eq!(decode(Encoding::L24, 0, [1, 2], &p, &mut out), 0);
        assert_eq!(decode(Encoding::L24, 2, [0, 2], &p, &mut out), 1);
    }

    #[test]
    fn media_ts_wraps() {
        // ptp_ns * 48000 / 1e9 = 2^32 + 5  =>  ptp_ns = (2^32 + 5) * 1e9 / 48000
        // pick a multiple: (2^32 + 5) must be divisible by 48 for exactness; use ceil
        let ns: i128 = (((1i128 << 32) + 5) * 1_000_000_000 + 47_999) / 48_000;
        assert_eq!(media_ts(ns, 0), 5);
        assert_eq!(media_ts(ns, 10), 15);
        assert_eq!(media_ts(1_700_000_000_000_000_000, 0), {
            ((1_700_000_000_000_000_000i128 * 48000 / 1_000_000_000) % (1i128 << 32)) as u32
        });
        assert_eq!(media_ts(-1_000_000_000 / 48_000 * 1, 0), u32::MAX);
        assert_eq!(media_ts(0, u32::MAX), u32::MAX);
    }

    #[test]
    fn ts_diff_across_wrap() {
        assert_eq!(ts_diff(3, u32::MAX - 2), 6);
        assert_eq!(ts_diff(u32::MAX - 2, 3), -6);
    }

    #[test]
    fn header_round_trips() {
        let mut d = header(0xfffe, 0x01020304, 0xaabbccdd, 97).to_vec();
        d.extend_from_slice(&[9, 8, 7]);
        let p = parse(&d).unwrap();
        assert_eq!(
            (p.seq, p.ts, p.ssrc, p.payload_type),
            (0xfffe, 0x01020304, 0xaabbccdd, 97)
        );
        assert_eq!(p.payload, &[9, 8, 7]);
    }

    #[test]
    fn rtp_with_csrc_and_extension() {
        let mut d = vec![0x80 | 0x10 | 0x02, 0x80 | 97, 0, 5, 0, 0, 0, 1, 0, 0, 0, 2];
        d.extend_from_slice(&[0; 8]); // two CSRCs
        d.extend_from_slice(&[0xbe, 0xde, 0, 1, 1, 2, 3, 4]); // extension, 1 word
        d.extend_from_slice(&[0xaa, 0xbb]);
        let p = parse(&d).unwrap();
        assert_eq!(p.payload_type, 97); // marker bit stripped
        assert_eq!(p.seq, 5);
        assert_eq!(p.payload, &[0xaa, 0xbb]);
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert!(parse(&[]).is_none());
        assert!(parse(&[0x80; 11]).is_none());
        let mut d = header(1, 2, 3, 97).to_vec();
        d[0] = 0x40; // version 1
        assert!(parse(&d).is_none());
        let mut d = header(1, 2, 3, 97).to_vec();
        d[0] = 0x83; // 3 CSRCs, none present
        assert!(parse(&d).is_none());
        let mut d = header(1, 2, 3, 97).to_vec();
        d[0] = 0x90; // extension, none present
        assert!(parse(&d).is_none());
        d.extend_from_slice(&[0, 0, 0, 5]); // length beyond end
        assert!(parse(&d).is_none());
    }

    #[test]
    fn parse_drops_padding() {
        let mut d = header(1, 2, 3, 97).to_vec();
        d[0] |= 0x20;
        d.extend_from_slice(&[1, 2, 3, 0, 0, 3]);
        assert_eq!(parse(&d).unwrap().payload, &[1, 2, 3]);
        let n = d.len();
        d[n - 1] = 0;
        assert!(parse(&d).is_none());
        d[n - 1] = 7;
        assert!(parse(&d).is_none());
    }
}
