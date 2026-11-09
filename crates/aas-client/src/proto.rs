//! Control protocol between client and daemon. Every request is
//! `REQUEST_LEN` bytes and every reply `REPLY_LEN` bytes, little endian.
//! A message is a u32 tag followed by u32 arguments, unused ones zero.
//!
//! Each request gets one reply, in order. The one exception is a
//! connection that has sent `SubscribeVolume`: it is also sent
//! `Reply::Volume` whenever the output volume changes, from any
//! connection, including its own. Such a notification looks the same as
//! the reply to `GetVolume`. Both carry the volume at the time they were
//! sent, so a subscriber keeps the last `Volume` it read, whatever it
//! answers. Its other requests get replies other than `Volume`, which it
//! matches to them in order. A notification that does not fit in the
//! socket buffer is dropped.

use crate::convert::ClientFormat;
use std::path::PathBuf;

/// The protocol version a client sends in `Hello`.
pub const VERSION: u32 = 1;
pub const REQUEST_LEN: usize = 20;
pub const REPLY_LEN: usize = 8;
/// Largest buffer a client may ask for. Bounds the ring memory.
pub const MAX_BUFFER_FRAMES: u32 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Hello {
        version: u32,
    },
    Configure {
        rate: u32,
        format: ClientFormat,
        channels: u8,
        buffer_frames: u32,
    },
    Start,
    Stop,
    Drain,
    /// Answered by `Volume`.
    GetVolume,
    /// From 0 to 100. Answered by `Ok`, or an error over 100.
    SetVolume {
        percent: u8,
    },
    /// Answered by `Volume`, then sent `Volume` on every change.
    SubscribeVolume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Ok,
    Configured,
    Volume { percent: u8 },
    Error { code: u32 },
}

fn format_code(f: ClientFormat) -> u32 {
    match f {
        ClientFormat::S16 => 0,
        ClientFormat::S24 => 1,
        ClientFormat::S24Packed => 2,
        ClientFormat::S32 => 3,
        ClientFormat::Float => 4,
    }
}

fn format_from_code(c: u32) -> Option<ClientFormat> {
    Some(match c {
        0 => ClientFormat::S16,
        1 => ClientFormat::S24,
        2 => ClientFormat::S24Packed,
        3 => ClientFormat::S32,
        4 => ClientFormat::Float,
        _ => return None,
    })
}

fn pack(tag: u32, args: [u32; 4]) -> [u8; REQUEST_LEN] {
    let mut out = [0u8; REQUEST_LEN];
    out[..4].copy_from_slice(&tag.to_le_bytes());
    for (i, a) in args.iter().enumerate() {
        out[4 + 4 * i..8 + 4 * i].copy_from_slice(&a.to_le_bytes());
    }
    out
}

fn word(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
}

pub fn encode_request(r: &Request) -> [u8; REQUEST_LEN] {
    match *r {
        Request::Hello { version } => pack(1, [version, 0, 0, 0]),
        Request::Configure {
            rate,
            format,
            channels,
            buffer_frames,
        } => pack(
            2,
            [rate, format_code(format), channels as u32, buffer_frames],
        ),
        Request::Start => pack(3, [0; 4]),
        Request::Stop => pack(4, [0; 4]),
        Request::Drain => pack(5, [0; 4]),
        Request::GetVolume => pack(6, [0; 4]),
        Request::SetVolume { percent } => pack(7, [percent as u32, 0, 0, 0]),
        Request::SubscribeVolume => pack(8, [0; 4]),
    }
}

/// Decodes one request. Returns `None` unless `b` is exactly `REQUEST_LEN`
/// bytes holding a known message with in-range arguments.
pub fn decode_request(b: &[u8]) -> Option<Request> {
    if b.len() != REQUEST_LEN {
        return None;
    }
    Some(match word(b, 0) {
        1 => Request::Hello {
            version: word(b, 1),
        },
        2 => Request::Configure {
            rate: word(b, 1),
            format: format_from_code(word(b, 2))?,
            channels: u8::try_from(word(b, 3)).ok()?,
            buffer_frames: Some(word(b, 4)).filter(|&n| n <= MAX_BUFFER_FRAMES)?,
        },
        3 => Request::Start,
        4 => Request::Stop,
        5 => Request::Drain,
        6 => Request::GetVolume,
        7 => Request::SetVolume {
            percent: u8::try_from(word(b, 1)).ok()?,
        },
        8 => Request::SubscribeVolume,
        _ => return None,
    })
}

pub fn encode_reply(r: &Reply) -> [u8; REPLY_LEN] {
    let (tag, arg) = match *r {
        Reply::Ok => (1u32, 0u32),
        Reply::Configured => (2, 0),
        Reply::Volume { percent } => (3, percent as u32),
        Reply::Error { code } => (4, code),
    };
    let mut out = [0u8; REPLY_LEN];
    out[..4].copy_from_slice(&tag.to_le_bytes());
    out[4..].copy_from_slice(&arg.to_le_bytes());
    out
}

/// Decodes one reply. Returns `None` unless `b` is exactly `REPLY_LEN` bytes
/// holding a known message with in-range arguments.
pub fn decode_reply(b: &[u8]) -> Option<Reply> {
    if b.len() != REPLY_LEN {
        return None;
    }
    Some(match word(b, 0) {
        1 => Reply::Ok,
        2 => Reply::Configured,
        3 => Reply::Volume {
            percent: u8::try_from(word(b, 1)).ok()?,
        },
        4 => Reply::Error { code: word(b, 1) },
        _ => return None,
    })
}

/// `AAS_SOCKET` if set, else `/run/aas/aas.sock`.
pub fn default_socket_path() -> PathBuf {
    match std::env::var_os("AAS_SOCKET") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from("/run/aas/aas.sock"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips() {
        let requests = [
            Request::Hello { version: 1 },
            Request::Configure {
                rate: 48000,
                format: ClientFormat::S24Packed,
                channels: 2,
                buffer_frames: 4096,
            },
            Request::Start,
            Request::Stop,
            Request::Drain,
            Request::GetVolume,
            Request::SetVolume { percent: 55 },
            Request::SubscribeVolume,
        ];
        for r in requests {
            assert_eq!(decode_request(&encode_request(&r)), Some(r));
        }
        let replies = [
            Reply::Ok,
            Reply::Configured,
            Reply::Volume { percent: 100 },
            Reply::Error { code: 22 },
        ];
        for r in replies {
            assert_eq!(decode_reply(&encode_reply(&r)), Some(r));
        }
        for f in [
            ClientFormat::S16,
            ClientFormat::S24,
            ClientFormat::S32,
            ClientFormat::Float,
        ] {
            let r = Request::Configure {
                rate: 44100,
                format: f,
                channels: 1,
                buffer_frames: 1,
            };
            assert_eq!(decode_request(&encode_request(&r)), Some(r));
        }
    }

    #[test]
    fn decode_rejects_a_short_or_unknown_message() {
        let good = encode_request(&Request::Start);
        assert_eq!(decode_request(&good[..REQUEST_LEN - 1]), None);
        assert_eq!(decode_request(&[]), None);
        let mut long = good.to_vec();
        long.push(0);
        assert_eq!(decode_request(&long), None);
        assert_eq!(decode_request(&pack(99, [0; 4])), None);
        assert_eq!(decode_request(&pack(2, [48000, 9, 2, 64])), None);
        assert_eq!(decode_request(&pack(2, [48000, 0, 256, 64])), None);
        assert_eq!(decode_request(&pack(7, [300, 0, 0, 0])), None);
        let at = pack(2, [48000, 0, 2, MAX_BUFFER_FRAMES]);
        assert!(decode_request(&at).is_some());
        assert_eq!(
            decode_request(&pack(2, [48000, 0, 2, MAX_BUFFER_FRAMES + 1])),
            None
        );
        let reply = encode_reply(&Reply::Ok);
        assert_eq!(decode_reply(&reply[..REPLY_LEN - 1]), None);
        assert_eq!(decode_reply(&[9, 0, 0, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn socket_path_default_and_override() {
        // Both cases in one test so the environment is not raced.
        unsafe { std::env::remove_var("AAS_SOCKET") };
        assert_eq!(default_socket_path(), PathBuf::from("/run/aas/aas.sock"));
        unsafe { std::env::set_var("AAS_SOCKET", "/tmp/x.sock") };
        assert_eq!(default_socket_path(), PathBuf::from("/tmp/x.sock"));
        unsafe { std::env::remove_var("AAS_SOCKET") };
    }
}
