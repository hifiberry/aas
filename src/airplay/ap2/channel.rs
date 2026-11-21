//! The control channel after pairing: every byte of RTSP in either
//! direction goes in frames of at most 1024 bytes of plaintext, each its
//! 2-byte little-endian length, the sealed bytes and a 16-byte tag. The
//! length is the frame's associated data; the nonce is 4 zero bytes and a
//! 64-bit little-endian count of the frames in that direction.

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha512;
use std::fmt;

/// The most plaintext in one frame.
pub const FRAME: usize = 1024;
pub const TAG: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelError(pub String);

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// HKDF-SHA512 of `secret` with `salt` and `info`, 32 bytes.
pub fn derive(secret: &[u8], salt: &str, info: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha512>::new(Some(salt.as_bytes()), secret)
        .expand(info.as_bytes(), &mut out)
        .expect("32 bytes is a length HKDF-SHA512 gives");
    out
}

/// The two keys of the control channel, as the sender uses them.
#[derive(Clone)]
pub struct Keys {
    pub write: [u8; 32],
    pub read: [u8; 32],
}

impl Keys {
    pub fn control(secret: &[u8]) -> Keys {
        Keys {
            write: derive(secret, "Control-Salt", "Control-Write-Encryption-Key"),
            read: derive(secret, "Control-Salt", "Control-Read-Encryption-Key"),
        }
    }
}

fn nonce(count: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&count.to_le_bytes());
    Nonce::from(n)
}

/// Seals what one side writes.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    count: u64,
}

impl Sealer {
    pub fn new(key: &[u8; 32]) -> Sealer {
        Sealer {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            count: 0,
        }
    }

    /// Appends `plain` to `out` as frames.
    pub fn seal(&mut self, plain: &[u8], out: &mut Vec<u8>) {
        for part in plain.chunks(FRAME) {
            let length = (part.len() as u16).to_le_bytes();
            out.extend_from_slice(&length);
            let start = out.len();
            out.extend_from_slice(part);
            let tag = self
                .cipher
                .encrypt_in_place_detached(&nonce(self.count), &length, &mut out[start..])
                .expect("a frame of at most 1024 bytes seals");
            out.extend_from_slice(&tag);
            self.count += 1;
        }
    }
}

/// Opens what the other side wrote.
pub struct Opener {
    cipher: ChaCha20Poly1305,
    count: u64,
}

impl Opener {
    pub fn new(key: &[u8; 32]) -> Opener {
        Opener {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            count: 0,
        }
    }

    /// Opens every whole frame at the start of `sealed`, takes it out and
    /// appends its plaintext to `out`. A part of a frame stays for later.
    pub fn open(&mut self, sealed: &mut Vec<u8>, out: &mut Vec<u8>) -> Result<(), ChannelError> {
        let mut used = 0;
        while let Some(&[lo, hi]) = sealed.get(used..used + 2) {
            let length = [lo, hi];
            let n = usize::from(u16::from_le_bytes(length));
            let end = used + 2 + n + TAG;
            if sealed.len() < end {
                break;
            }
            let start = out.len();
            out.extend_from_slice(&sealed[used + 2..used + 2 + n]);
            let tag = Tag::from_slice(&sealed[used + 2 + n..end]);
            if self
                .cipher
                .decrypt_in_place_detached(&nonce(self.count), &length, &mut out[start..], tag)
                .is_err()
            {
                out.truncate(start);
                return Err(ChannelError(format!(
                    "frame {} of the control channel does not open",
                    self.count
                )));
            }
            self.count += 1;
            used = end;
        }
        sealed.drain(..used);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    const SECRET: [u8; 64] = {
        let mut s = [0u8; 64];
        let mut i = 0;
        while i < 64 {
            s[i] = i as u8;
            i += 1;
        }
        s
    };

    // Worked out with Python's hmac and the `cryptography` package.
    const WRITE: &str = "5a6cb19bcbe7d4df2dd8279f39562f7fae2dbf73eb5a4f98849c245c82b2fe96";
    const READ: &str = "c2f56f1912e2fc1f6604fd5bccab619272f687345aa4bce1c3c857421bd3b821";
    const FRAME_0: &str =
        "1600e44f37096f116a81a6d494b8322fd17c266961cf91ef7518e38bf204256c3024345a9c987a59";
    const FRAME_1: &str =
        "1600607c33ab158b9ef55a1e939a6e71a1af7477818a15b921d8c0d4ecb8a0245476257352284b7a";
    const MSG: &[u8] = b"GET /info HTTP/1.1\r\n\r\n";

    #[test]
    fn the_keys_are_hkdf_sha512_of_the_secret() {
        let keys = Keys::control(&SECRET);
        assert_eq!(keys.write.to_vec(), unhex(WRITE));
        assert_eq!(keys.read.to_vec(), unhex(READ));
    }

    #[test]
    fn frames_are_length_sealed_bytes_and_tag_with_counted_nonces() {
        let mut sealer = Sealer::new(&Keys::control(&SECRET).write);
        let mut out = Vec::new();
        sealer.seal(MSG, &mut out);
        assert_eq!(out, unhex(FRAME_0));
        out.clear();
        sealer.seal(MSG, &mut out);
        assert_eq!(out, unhex(FRAME_1));
    }

    #[test]
    fn a_message_over_1024_bytes_is_two_frames() {
        let key = [3u8; 32];
        let plain: Vec<u8> = (0..1500u32).map(|i| i as u8).collect();
        let mut out = Vec::new();
        Sealer::new(&key).seal(&plain, &mut out);
        assert_eq!(out.len(), 1500 + 2 * (2 + TAG));
        assert_eq!(&out[..2], &[0x00, 0x04]);
        assert_eq!(&out[2 + 1024 + TAG..2 + 1024 + TAG + 2], &[476u16 as u8, 1]);
        let mut opened = Vec::new();
        Opener::new(&key).open(&mut out, &mut opened).unwrap();
        assert_eq!(opened, plain);
        assert!(out.is_empty());
    }

    #[test]
    fn a_part_of_a_frame_waits_for_the_rest() {
        let key = [4u8; 32];
        let mut sealed = Vec::new();
        let mut sealer = Sealer::new(&key);
        sealer.seal(b"one", &mut sealed);
        sealer.seal(b"two", &mut sealed);
        let mut opener = Opener::new(&key);
        let mut opened = Vec::new();
        let mut buf = Vec::new();
        for &b in &sealed {
            buf.push(b);
            opener.open(&mut buf, &mut opened).unwrap();
        }
        assert_eq!(opened, b"onetwo");
        assert!(buf.is_empty());
    }

    #[test]
    fn frames_out_of_order_do_not_open() {
        let key = [5u8; 32];
        let mut sealer = Sealer::new(&key);
        let (mut first, mut second) = (Vec::new(), Vec::new());
        sealer.seal(b"one", &mut first);
        sealer.seal(b"two", &mut second);
        let mut opened = Vec::new();
        assert!(Opener::new(&key).open(&mut second, &mut opened).is_err());
    }

    #[test]
    fn a_tampered_frame_is_an_error() {
        let key = [6u8; 32];
        for at in [0usize, 2, 5] {
            let mut sealed = Vec::new();
            Sealer::new(&key).seal(b"abc", &mut sealed);
            let last = sealed.len() - 1;
            sealed[if at == 5 { last } else { at }] ^= 1;
            let mut opened = Vec::new();
            // A changed length leaves a frame that is either cut short or
            // fails its tag.
            let r = Opener::new(&key).open(&mut sealed, &mut opened);
            assert!(r.is_err() || opened.is_empty(), "{at}");
            assert!(opened.is_empty(), "{at}");
        }
    }
}
