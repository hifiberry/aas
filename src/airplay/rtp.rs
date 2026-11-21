//! The packets of an AirPlay 1 stream, byte for byte as PipeWire's RAOP
//! sender (`module-raop-sink.c`, MIT) sends them, which receivers such as
//! sound bars take: the audio as ALAC escape frames in RTP, the sync
//! packets on the control port, the answers to timing requests, and the
//! history that retransmit requests are served from.

use std::time::Duration;

/// Frames in one audio packet.
pub const FRAMES: usize = 352;
/// The stream's rate.
pub const RATE: u32 = 44100;
/// Audio packets between two sync packets, about a second.
pub const SYNC_EVERY: u32 = 121;
/// Audio packets kept for retransmission, about 4 s.
pub const HISTORY: usize = 512;

/// Seconds from 1900, NTP's epoch, to 1970.
const NTP_UNIX: u64 = 2_208_988_800;

/// Writes bits most significant first.
struct Bits<'a> {
    out: &'a mut Vec<u8>,
    /// Bits used in the last byte, 0 to 7; 0 starts a new byte.
    used: u32,
}

impl Bits<'_> {
    fn put(&mut self, value: u32, count: u32) {
        for i in (0..count).rev() {
            if self.used == 0 {
                self.out.push(0);
            }
            let bit = ((value >> i) & 1) as u8;
            *self.out.last_mut().unwrap() |= bit << (7 - self.used);
            self.used = (self.used + 1) % 8;
        }
    }
}

/// Appends `frames` as an ALAC frame that is not compressed: a 55-bit
/// header, the samples as 16-bit big-endian words, an end tag of 3 bits.
/// 4n+8 bytes for n frames.
pub fn alac_escape(frames: &[[i16; 2]], out: &mut Vec<u8>) {
    let mut bits = Bits { out, used: 0 };
    // Stereo, three fields PipeWire leaves at zero, "has size", two
    // unused bits, "not compressed", then the frame count.
    for (value, count) in [(1, 3), (0, 4), (0, 8), (0, 4), (1, 1), (0, 2), (1, 1)] {
        bits.put(value, count);
    }
    bits.put(frames.len() as u32, 32);
    for &[left, right] in frames {
        bits.put(u32::from(left as u16), 16);
        bits.put(u32::from(right as u16), 16);
    }
    bits.put(7, 3);
}

/// Appends an RTP audio packet: version 2, payload type 96, the marker on
/// the first packet of a session.
pub fn audio_packet(
    first: bool,
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    payload: &[u8],
    out: &mut Vec<u8>,
) {
    out.extend_from_slice(&[0x80, if first { 0xe0 } else { 0x60 }]);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
    out.extend_from_slice(payload);
}

/// The sync packet: the receiver plays the frame with RTP time
/// `rtp_now - latency` at NTP time `ntp`.
pub fn sync_packet(first: bool, rtp_now: u32, latency: u32, ntp: u64) -> [u8; 20] {
    let mut p = [0u8; 20];
    // The extension bit marks the first; sequence 7, as every sender
    // sends it.
    p[..4].copy_from_slice(&[if first { 0x90 } else { 0x80 }, 0xd4, 0x00, 0x07]);
    p[4..8].copy_from_slice(&rtp_now.wrapping_sub(latency).to_be_bytes());
    p[8..16].copy_from_slice(&ntp.to_be_bytes());
    p[16..].copy_from_slice(&rtp_now.to_be_bytes());
    p
}

/// The answer to a timing request: its own time echoed, with when it came
/// and when the answer goes. None for anything that is not a timing
/// request.
pub fn timing_reply(request: &[u8], received: u64, transmitted: u64) -> Option<[u8; 32]> {
    if request.len() != 32 || request[..4] != [0x80, 0xd2, 0x00, 0x07] {
        return None;
    }
    let mut p = [0u8; 32];
    p[..2].copy_from_slice(&[0x80, 0xd3]);
    p[8..16].copy_from_slice(&request[24..32]);
    p[16..24].copy_from_slice(&received.to_be_bytes());
    p[24..].copy_from_slice(&transmitted.to_be_bytes());
    Some(p)
}

/// A retransmit request's first sequence number and count. None for
/// anything else, and for a count of 0.
pub fn retransmit_request(packet: &[u8]) -> Option<(u16, u16)> {
    if packet.len() < 8 || packet[0] != 0x80 || packet[1] != 0xd5 {
        return None;
    }
    let seq = u16::from_be_bytes([packet[4], packet[5]]);
    let count = u16::from_be_bytes([packet[6], packet[7]]);
    (count > 0).then_some((seq, count))
}

/// An NTP timestamp of a time since 1970: seconds since 1900 in the high
/// word, the fraction in the low.
pub fn ntp_from(since_1970: Duration) -> u64 {
    let fraction = (u64::from(since_1970.subsec_nanos()) << 32) / 1_000_000_000;
    ((since_1970.as_secs() + NTP_UNIX) << 32) | fraction
}

/// The last `HISTORY` audio packets, by sequence number.
pub struct History {
    slots: Vec<Option<(u16, Vec<u8>)>>,
}

impl Default for History {
    fn default() -> History {
        History {
            slots: vec![None; HISTORY],
        }
    }
}

impl History {
    /// Keeps `packet`, reusing the slot's buffer.
    pub fn push(&mut self, seq: u16, packet: &[u8]) {
        let slot = &mut self.slots[usize::from(seq) % HISTORY];
        match slot {
            Some((s, buf)) => {
                *s = seq;
                buf.clear();
                buf.extend_from_slice(packet);
            }
            None => *slot = Some((seq, packet.to_vec())),
        }
    }

    pub fn get(&self, seq: u16) -> Option<&[u8]> {
        match &self.slots[usize::from(seq) % HISTORY] {
            Some((s, buf)) if *s == seq => Some(buf),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escape(frames: &[[i16; 2]]) -> Vec<u8> {
        let mut out = Vec::new();
        alac_escape(frames, &mut out);
        out
    }

    #[test]
    fn an_escape_frame_of_352_frames_is_1416_bytes_and_starts_20_00_12() {
        let out = escape(&[[0, 0]; FRAMES]);
        assert_eq!(out.len(), 1416);
        // The header's bits, then the count 352 = 0x160 from bit 23.
        assert_eq!(out[..8], [0x20, 0x00, 0x12, 0x00, 0x00, 0x02, 0xc0, 0x00]);
    }

    #[test]
    fn the_samples_are_big_endian_after_the_55_bit_header() {
        // Worked out bit by bit: header 001 0000 00000000 0000 1 00 1,
        // count 1 in 32 bits, 0x1234, 0xfffe, end tag 111, zero padding.
        assert_eq!(
            escape(&[[0x1234, -2]]),
            [
                0x20, 0x00, 0x12, 0x00, 0x00, 0x00, 0x02, 0x24, 0x69, 0xff, 0xfd, 0xc0
            ]
        );
    }

    #[test]
    fn the_end_tag_is_7_in_3_bits() {
        // 0 frames: 55 + 3 bits; the tag sits in bits 55 to 57.
        let out = escape(&[]);
        assert_eq!(out.len(), 8);
        assert_eq!(out[6] & 0x01, 0x01);
        assert_eq!(out[7], 0xc0);
    }

    #[test]
    fn the_first_audio_packet_has_the_marker() {
        let mut out = Vec::new();
        audio_packet(true, 0x0102, 0x0304_0506, 0x0708_090a, &[9, 9], &mut out);
        assert_eq!(out, [0x80, 0xe0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 9, 9]);
        out.clear();
        audio_packet(false, 0xfffe, 1, 2, &[], &mut out);
        assert_eq!(out, [0x80, 0x60, 0xff, 0xfe, 0, 0, 0, 1, 0, 0, 0, 2]);
    }

    #[test]
    fn a_sync_packet_carries_the_time_less_the_latency() {
        let p = sync_packet(true, 100_000, 66150, 0x1122_3344_5566_7788);
        assert_eq!(p[..4], [0x90, 0xd4, 0x00, 0x07]);
        assert_eq!(p[4..8], (100_000u32 - 66150).to_be_bytes());
        assert_eq!(p[8..16], 0x1122_3344_5566_7788u64.to_be_bytes());
        assert_eq!(p[16..20], 100_000u32.to_be_bytes());
        let later = sync_packet(false, 10, 66150, 0);
        assert_eq!(later[..4], [0x80, 0xd4, 0x00, 0x07]);
        // The RTP time wraps.
        assert_eq!(later[4..8], 10u32.wrapping_sub(66150).to_be_bytes());
    }

    #[test]
    fn a_timing_reply_echoes_the_request_time() {
        let mut request = [0u8; 32];
        request[..4].copy_from_slice(&[0x80, 0xd2, 0x00, 0x07]);
        request[24..].copy_from_slice(&0xaabb_ccdd_eeff_0011u64.to_be_bytes());
        let reply = timing_reply(&request, 5, 6).unwrap();
        assert_eq!(reply[..8], [0x80, 0xd3, 0, 0, 0, 0, 0, 0]);
        assert_eq!(reply[8..16], 0xaabb_ccdd_eeff_0011u64.to_be_bytes());
        assert_eq!(reply[16..24], 5u64.to_be_bytes());
        assert_eq!(reply[24..], 6u64.to_be_bytes());
    }

    #[test]
    fn timing_reply_rejects_31_bytes_and_a_wrong_header() {
        let mut request = [0u8; 32];
        request[..4].copy_from_slice(&[0x80, 0xd2, 0x00, 0x07]);
        assert!(timing_reply(&request[..31], 0, 0).is_none());
        request[1] = 0xd4;
        assert!(timing_reply(&request, 0, 0).is_none());
    }

    #[test]
    fn retransmit_request_reads_seq_and_count() {
        let p = [0x80, 0xd5, 0x00, 0x01, 0x12, 0x34, 0x00, 0x03];
        assert_eq!(retransmit_request(&p), Some((0x1234, 3)));
        assert_eq!(retransmit_request(&p[..7]), None);
        let mut other = p;
        other[1] = 0xd4;
        assert_eq!(retransmit_request(&other), None);
        other = p;
        other[0] = 0x90;
        assert_eq!(retransmit_request(&other), None);
    }

    #[test]
    fn retransmit_request_ignores_count_0() {
        let p = [0x80, 0xd5, 0x00, 0x01, 0x12, 0x34, 0x00, 0x00];
        assert_eq!(retransmit_request(&p), None);
    }

    #[test]
    fn history_keeps_the_last_512_across_the_wrap() {
        let mut h = History::default();
        let seqs = (65500u32..65536).chain(0..601).map(|s| s as u16);
        for seq in seqs {
            h.push(seq, &seq.to_be_bytes());
        }
        // 637 pushed: the oldest 125 are gone.
        assert_eq!(h.get(65535), None);
        assert_eq!(h.get(88), None);
        assert_eq!(h.get(89), Some(&89u16.to_be_bytes()[..]));
        assert_eq!(h.get(600), Some(&600u16.to_be_bytes()[..]));
        assert_eq!(h.get(601), None);
    }

    #[test]
    fn ntp_of_the_unix_epoch_is_2208988800_seconds() {
        assert_eq!(ntp_from(Duration::ZERO), 2_208_988_800u64 << 32);
    }

    #[test]
    fn half_a_second_is_0x80000000() {
        let ntp = ntp_from(Duration::from_millis(1500));
        assert_eq!(ntp >> 32, 2_208_988_801);
        assert_eq!(ntp & 0xffff_ffff, 0x8000_0000);
    }
}
