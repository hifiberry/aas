//! AirPlay 2's audio packets: the RTP payload sealed with ChaCha20-Poly1305
//! under the stream key `shk`, the RTP timestamp and SSRC (bytes 4 to 12)
//! as associated data, and the tag and the 8-byte nonce count appended, as
//! PipeWire's `rtp_ap2_encrypt` sends them. The count goes up by one a
//! packet, so no nonce is used twice under a key.

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

/// The RTP header's length.
const HEADER: usize = 12;

pub struct AudioSealer {
    cipher: ChaCha20Poly1305,
    count: u64,
}

impl AudioSealer {
    pub fn new(shk: &[u8; 32]) -> AudioSealer {
        AudioSealer {
            cipher: ChaCha20Poly1305::new(Key::from_slice(shk)),
            count: 0,
        }
    }

    /// Seals the payload of the RTP packet in `packet` in place and
    /// appends the tag and the count.
    pub fn seal(&mut self, packet: &mut Vec<u8>) {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.count.to_le_bytes());
        let (header, payload) = packet.split_at_mut(HEADER);
        let tag = self
            .cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), &header[4..12], payload)
            .expect("a packet's payload seals");
        packet.extend_from_slice(&tag);
        packet.extend_from_slice(&self.count.to_le_bytes());
        self.count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chacha20poly1305::Tag;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    const KEY: [u8; 32] = {
        let mut k = [0u8; 32];
        let mut i = 0;
        while i < 32 {
            k[i] = i as u8;
            i += 1;
        }
        k
    };

    #[test]
    fn a_packet_is_sealed_as_python_cryptography_seals_it() {
        // The eighth packet (count 7) of a stream under KEY.
        let mut sealer = AudioSealer::new(&KEY);
        let mut packet = Vec::new();
        for _ in 0..8 {
            packet = unhex("806001020000aabb112233442000000001020304");
            sealer.seal(&mut packet);
        }
        assert_eq!(
            packet,
            unhex(
                "806001020000aabb11223344d10f844823ea798d93cc55370259323cc3757f46398e5e7e0700000000000000"
            )
        );
    }

    #[test]
    fn a_sealed_payload_opens_with_shk() {
        let shk = [9u8; 32];
        let mut sealer = AudioSealer::new(&shk);
        let mut header = vec![0x80, 0x60, 0, 1, 0, 0, 1, 0, 0xde, 0xad, 0xbe, 0xef];
        let payload: Vec<u8> = (0..1416u32).map(|i| i as u8).collect();
        for count in 0u64..3 {
            header[3] = count as u8;
            let mut packet = header.clone();
            packet.extend_from_slice(&payload);
            sealer.seal(&mut packet);
            assert_eq!(packet.len(), 12 + 1416 + 16 + 8);
            assert_eq!(&packet[..12], &header[..]);
            let n = packet.len();
            assert_eq!(packet[n - 8..], count.to_le_bytes());
            let mut nonce = [0u8; 12];
            nonce[4..].copy_from_slice(&packet[n - 8..]);
            let tag = Tag::clone_from_slice(&packet[n - 24..n - 8]);
            let mut body = packet[12..n - 24].to_vec();
            ChaCha20Poly1305::new(Key::from_slice(&shk))
                .decrypt_in_place_detached(
                    Nonce::from_slice(&nonce),
                    &header[4..12],
                    &mut body,
                    &tag,
                )
                .unwrap();
            assert_eq!(body, payload);
        }
    }
}
