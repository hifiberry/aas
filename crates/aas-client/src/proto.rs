//! Control protocol between client and daemon. Every request is
//! `REQUEST_LEN` bytes and every reply `REPLY_LEN` bytes, little endian.
//! A message is a u32 tag followed by u32 arguments. A sender sets the
//! arguments a message does not use to zero. A receiver ignores them.
//! `Configure` carries the direction of the stream and whether a capture
//! stream records the output monitor in its channels word, as `CAPTURE`
//! and `MONITOR` describe. A monitor's rate word, otherwise unused, is the
//! id of the stream it records, 0 for the master.
//!
//! Each request gets one reply, in order. A connection that has sent
//! `SubscribeVolume` is also sent `Reply::VolumeChanged` when the output
//! volume changes, from any connection, including its own.
//! `VolumeChanged` is never a reply, so a client waiting for a reply
//! takes it aside and reads on.
//!
//! Notifications are coalesced: a subscriber that does not read gets no
//! more than one waiting notification, carrying the latest volume, once
//! it has read what was sent before. So the last notification a
//! subscriber reads after a change always holds the current volume, and
//! notifications never fill its socket.

use crate::convert::ClientFormat;
use std::ffi::OsString;
use std::path::PathBuf;

/// The protocol version a client sends in `Hello`, and the daemon in its
/// reply.
pub const VERSION: u32 = 3;
// The ring's layout is part of the protocol, and the two versions move
// together. A ring that changes without the protocol would get past
// `Hello` and fail at `Configure`, where a player takes it for a bad
// parameter.
const _: () = assert!(VERSION == crate::ring::VERSION);
pub const REQUEST_LEN: usize = 20;
pub const REPLY_LEN: usize = 8;
/// Largest buffer a client may ask for. Bounds the ring memory.
pub const MAX_BUFFER_FRAMES: u32 = 1 << 20;
/// The bit of `Configure`'s channels word that asks for a capture stream.
/// The word holds the channels in bits 0 to 7, the capture flag in bit 31
/// and the monitor flag in bit 30; every other bit is refused. The word
/// had room to spare, so the request keeps its size.
const CAPTURE: u32 = 1 << 31;
/// The bit of `Configure`'s channels word that makes a capture stream
/// record what the master card is sent, instead of the analog input. It
/// is only valid beside `CAPTURE`.
const MONITOR: u32 = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// Answered by `Hello`, or an error for a version the daemon does not
    /// speak.
    Hello {
        version: u32,
    },
    /// Answered by `Configured`, with the ring and the wake-up.
    ///
    /// With `capture` the stream records: the daemon writes the analog
    /// input into the ring and the client reads it. It is always S32
    /// stereo at the daemon's rate, so `rate`, `format` and `channels` are
    /// ignored, and the reply names the rate. `buffer_frames` is rounded up
    /// to what the daemon needs, never refused for being small.
    ///
    /// With `monitor`, which needs `capture`, the stream records what the
    /// master card is sent instead of the analog input. Then `rate` names
    /// the stream to record: the id `GetStreamId` gave, or 0 for the
    /// master. An id that names no stream fails with `ENOENT`.
    Configure {
        rate: u32,
        format: ClientFormat,
        channels: u8,
        buffer_frames: u32,
        capture: bool,
        monitor: bool,
    },
    /// For a capture stream, fails with `ENODEV` once the stream has ended.
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
    /// Stops the stream and empties its ring. Answered by `Ok` once the
    /// ring is empty, so what is written after the reply is kept.
    Reset,
    /// Names the connection for a daemon-side adapter that did not accept
    /// it from the player itself. Sent before `Configure`.
    Identify {
        pid: u32,
    },
    /// Answered by `ClientVolume`.
    GetClientVolume,
    /// The gain of every stream of this connection's client, from -60 to 0
    /// dB. Answered by `Ok`, or an error outside that range.
    SetClientVolume {
        db_millis: i32,
        muted: bool,
    },
    /// Answered by `OutputRate`.
    GetOutputRate,
    /// Answered by `StreamId`.
    GetStreamId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Ok,
    /// For a capture stream, `rate` is the rate it records at. Zero for a
    /// playback stream.
    Configured {
        rate: u32,
    },
    /// The reply to `GetVolume` and `SubscribeVolume`.
    Volume {
        percent: u8,
    },
    Error {
        code: u32,
    },
    /// Not a reply: the output volume changed. Sent only to subscribers.
    VolumeChanged {
        percent: u8,
    },
    /// The reply to `Hello`: the protocol version the daemon speaks.
    Hello {
        version: u32,
    },
    /// The reply to `GetClientVolume`.
    ClientVolume {
        db_millis: i32,
        muted: bool,
    },
    /// Not a reply: this client's gain changed.
    ClientVolumeChanged {
        db_millis: i32,
        muted: bool,
    },
    /// The reply to `GetOutputRate`, zero while the output is off.
    OutputRate {
        rate: u32,
    },
    /// The reply to `GetStreamId`: the id of this connection's playback
    /// stream, which a monitor names in `Configure`'s rate word. Zero for a
    /// connection without a playback stream.
    StreamId {
        id: u32,
    },
}

fn format_code(f: ClientFormat) -> u32 {
    match f {
        ClientFormat::S16 => 0,
        ClientFormat::S24 => 1,
        ClientFormat::S24Packed => 2,
        ClientFormat::S32 => 3,
        ClientFormat::Float => 4,
        ClientFormat::S24In32 => 5,
    }
}

fn format_from_code(c: u32) -> Option<ClientFormat> {
    Some(match c {
        0 => ClientFormat::S16,
        1 => ClientFormat::S24,
        2 => ClientFormat::S24Packed,
        3 => ClientFormat::S32,
        4 => ClientFormat::Float,
        5 => ClientFormat::S24In32,
        _ => return None,
    })
}

/// The lowest client gain, in millidB.
const CLIENT_DB_MIN: i32 = -60_000;

/// A client volume in one word: the millidB offset from `CLIENT_DB_MIN`
/// below bit 17, and mute above it. An out-of-range value keeps its high
/// bits, so decoding refuses it rather than silently clamping it.
fn client_volume_word(db_millis: i32, muted: bool) -> u32 {
    let db = db_millis.wrapping_sub(CLIENT_DB_MIN) as u32;
    db | (u32::from(muted) << 17)
}

fn client_volume_from_word(word: u32) -> Option<(i32, bool)> {
    if word & !((1 << 18) - 1) != 0 || (word & 0x1_ffff) > 60_000 {
        return None;
    }
    Some((
        (word & 0x1_ffff) as i32 + CLIENT_DB_MIN,
        word & (1 << 17) != 0,
    ))
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
            capture,
            monitor,
        } => {
            let flags = if capture { CAPTURE } else { 0 } | if monitor { MONITOR } else { 0 };
            let channels = u32::from(channels) | flags;
            pack(2, [rate, format_code(format), channels, buffer_frames])
        }
        Request::Start => pack(3, [0; 4]),
        Request::Stop => pack(4, [0; 4]),
        Request::Drain => pack(5, [0; 4]),
        Request::GetVolume => pack(6, [0; 4]),
        Request::SetVolume { percent } => pack(7, [percent as u32, 0, 0, 0]),
        Request::SubscribeVolume => pack(8, [0; 4]),
        Request::Reset => pack(9, [0; 4]),
        Request::Identify { pid } => pack(10, [pid, 0, 0, 0]),
        Request::GetClientVolume => pack(11, [0; 4]),
        Request::SetClientVolume { db_millis, muted } => {
            pack(12, [db_millis as u32, u32::from(muted), 0, 0])
        }
        Request::GetOutputRate => pack(13, [0; 4]),
        Request::GetStreamId => pack(14, [0; 4]),
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
        2 => {
            let flags = word(b, 3);
            let (capture, monitor) = (flags & CAPTURE != 0, flags & MONITOR != 0);
            // A monitor is a kind of capture.
            if monitor && !capture {
                return None;
            }
            Request::Configure {
                rate: word(b, 1),
                format: format_from_code(word(b, 2))?,
                channels: u8::try_from(flags & !(CAPTURE | MONITOR)).ok()?,
                buffer_frames: Some(word(b, 4)).filter(|&n| n <= MAX_BUFFER_FRAMES)?,
                capture,
                monitor,
            }
        }
        3 => Request::Start,
        4 => Request::Stop,
        5 => Request::Drain,
        6 => Request::GetVolume,
        7 => Request::SetVolume {
            percent: u8::try_from(word(b, 1)).ok()?,
        },
        8 => Request::SubscribeVolume,
        9 => Request::Reset,
        10 => Request::Identify {
            pid: Some(word(b, 1)).filter(|&pid| pid != 0)?,
        },
        11 => Request::GetClientVolume,
        12 => Request::SetClientVolume {
            db_millis: word(b, 1) as i32,
            muted: match word(b, 2) {
                0 => false,
                1 => true,
                _ => return None,
            },
        },
        13 => Request::GetOutputRate,
        14 => Request::GetStreamId,
        _ => return None,
    })
}

pub fn encode_reply(r: &Reply) -> [u8; REPLY_LEN] {
    let (tag, arg) = match *r {
        Reply::Ok => (1u32, 0u32),
        Reply::Configured { rate } => (2, rate),
        Reply::Volume { percent } => (3, percent as u32),
        Reply::Error { code } => (4, code),
        Reply::VolumeChanged { percent } => (5, percent as u32),
        Reply::Hello { version } => (6, version),
        Reply::ClientVolume { db_millis, muted } => (7, client_volume_word(db_millis, muted)),
        Reply::ClientVolumeChanged { db_millis, muted } => {
            (8, client_volume_word(db_millis, muted))
        }
        Reply::OutputRate { rate } => (9, rate),
        Reply::StreamId { id } => (10, id),
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
        2 => Reply::Configured { rate: word(b, 1) },
        3 => Reply::Volume {
            percent: u8::try_from(word(b, 1)).ok()?,
        },
        4 => Reply::Error { code: word(b, 1) },
        5 => Reply::VolumeChanged {
            percent: u8::try_from(word(b, 1)).ok()?,
        },
        6 => Reply::Hello {
            version: word(b, 1),
        },
        7 => {
            let (db_millis, muted) = client_volume_from_word(word(b, 1))?;
            Reply::ClientVolume { db_millis, muted }
        }
        8 => {
            let (db_millis, muted) = client_volume_from_word(word(b, 1))?;
            Reply::ClientVolumeChanged { db_millis, muted }
        }
        9 => Reply::OutputRate { rate: word(b, 1) },
        10 => Reply::StreamId { id: word(b, 1) },
        _ => return None,
    })
}

/// `AAS_SOCKET` if set, else `/run/aas/aas.sock`.
pub fn default_socket_path() -> PathBuf {
    socket_path(std::env::var_os("AAS_SOCKET"))
}

/// The socket for a value of `AAS_SOCKET`: the value, or the default if
/// it is unset or empty.
fn socket_path(value: Option<OsString>) -> PathBuf {
    match value {
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
                capture: false,
                monitor: false,
            },
            Request::Start,
            Request::Stop,
            Request::Drain,
            Request::GetVolume,
            Request::SetVolume { percent: 55 },
            Request::SubscribeVolume,
            Request::Reset,
        ];
        for r in requests {
            assert_eq!(decode_request(&encode_request(&r)), Some(r));
        }
        let replies = [
            Reply::Ok,
            Reply::Configured { rate: 0 },
            Reply::Volume { percent: 100 },
            Reply::Error { code: 22 },
            Reply::VolumeChanged { percent: 40 },
            Reply::Hello { version: 7 },
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
                capture: false,
                monitor: false,
            };
            assert_eq!(decode_request(&encode_request(&r)), Some(r));
        }
    }

    #[test]
    fn a_capture_configure_round_trips() {
        let capture = Request::Configure {
            rate: 0,
            format: ClientFormat::S32,
            channels: 2,
            buffer_frames: 4096,
            capture: true,
            monitor: false,
        };
        assert_eq!(decode_request(&encode_request(&capture)), Some(capture));
        // The flag is all that tells it from a playback request.
        let playback = Request::Configure {
            rate: 0,
            format: ClientFormat::S32,
            channels: 2,
            buffer_frames: 4096,
            capture: false,
            monitor: false,
        };
        assert_ne!(encode_request(&capture), encode_request(&playback));
        assert_eq!(decode_request(&encode_request(&playback)), Some(playback));
        // A playback request reads as before: the flag is clear.
        assert_eq!(
            encode_request(&playback),
            pack(2, [0, format_code(ClientFormat::S32), 2, 4096])
        );
        let rate = Reply::Configured { rate: 44100 };
        assert_eq!(decode_reply(&encode_reply(&rate)), Some(rate));
        // Any other bit in the channels word is still refused.
        assert_eq!(decode_request(&pack(2, [0, 3, 2 | 1 << 29, 64])), None);
    }

    #[test]
    fn a_monitor_configure_round_trips_in_twenty_bytes() {
        let r = Request::Configure {
            rate: 0,
            format: ClientFormat::S32,
            channels: 2,
            buffer_frames: 4800,
            capture: true,
            monitor: true,
        };
        let bytes = encode_request(&r);
        assert_eq!(bytes.len(), REQUEST_LEN);
        assert_eq!(word(&bytes, 3), 2 | (1 << 31) | (1 << 30));
        assert_eq!(decode_request(&bytes), Some(r));
    }

    #[test]
    fn a_monitor_without_capture_is_refused() {
        let mut bytes = encode_request(&Request::Configure {
            rate: 48000,
            format: ClientFormat::S16,
            channels: 2,
            buffer_frames: 4800,
            capture: false,
            monitor: false,
        });
        bytes[12..16].copy_from_slice(&(2u32 | (1 << 30)).to_le_bytes());
        assert_eq!(decode_request(&bytes), None);
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
        assert_eq!(decode_reply(&[20, 0, 0, 0, 0, 0, 0, 0]), None);
        // A notification is told apart from a reply by its tag.
        assert_ne!(
            encode_reply(&Reply::VolumeChanged { percent: 40 }),
            encode_reply(&Reply::Volume { percent: 40 })
        );
        assert_eq!(decode_reply(&[5, 0, 0, 0, 0, 1, 0, 0]), None);
    }

    #[test]
    fn identify_and_client_volume_round_trip() {
        let requests = [
            Request::Identify { pid: 4321 },
            Request::GetClientVolume,
            Request::SetClientVolume {
                db_millis: -6000,
                muted: true,
            },
            Request::GetOutputRate,
        ];
        for r in requests {
            assert_eq!(decode_request(&encode_request(&r)), Some(r));
        }
    }

    #[test]
    fn client_volume_replies_round_trip() {
        for (db, muted) in [(0, false), (-6000, true), (-60000, false), (-1, true)] {
            let r = Reply::ClientVolume {
                db_millis: db,
                muted,
            };
            assert_eq!(decode_reply(&encode_reply(&r)), Some(r));
            let r = Reply::ClientVolumeChanged {
                db_millis: db,
                muted,
            };
            assert_eq!(decode_reply(&encode_reply(&r)), Some(r));
        }
    }

    #[test]
    fn a_client_volume_outside_the_range_does_not_decode() {
        let bad = Reply::ClientVolume {
            db_millis: -60001,
            muted: false,
        };
        assert_eq!(decode_reply(&encode_reply(&bad)), None);
        let bad = Reply::ClientVolume {
            db_millis: 1,
            muted: false,
        };
        assert_eq!(decode_reply(&encode_reply(&bad)), None);
    }

    #[test]
    fn socket_path_default_and_override() {
        // The value is passed in, so the test leaves the environment alone.
        let default = PathBuf::from("/run/aas/aas.sock");
        assert_eq!(socket_path(None), default);
        assert_eq!(socket_path(Some(OsString::new())), default);
        assert_eq!(
            socket_path(Some(OsString::from("/tmp/x.sock"))),
            PathBuf::from("/tmp/x.sock")
        );
    }

    #[test]
    fn decode_ignores_the_arguments_a_message_does_not_use() {
        assert_eq!(decode_request(&pack(3, [1, 2, 3, 4])), Some(Request::Start));
        assert_eq!(
            decode_request(&pack(7, [55, 9, 9, 9])),
            Some(Request::SetVolume { percent: 55 })
        );
        assert_eq!(decode_reply(&[1, 0, 0, 0, 9, 0, 0, 0]), Some(Reply::Ok));
    }

    #[test]
    fn get_stream_id_round_trips() {
        let r = Request::GetStreamId;
        assert_eq!(decode_request(&encode_request(&r)), Some(r));
        let r = Reply::StreamId { id: 7 };
        assert_eq!(decode_reply(&encode_reply(&r)), Some(r));
    }

    #[test]
    fn a_monitors_rate_word_names_its_stream() {
        let r = Request::Configure {
            rate: 42,
            format: ClientFormat::S32,
            channels: 2,
            buffer_frames: 4800,
            capture: true,
            monitor: true,
        };
        match decode_request(&encode_request(&r)) {
            Some(Request::Configure { rate, .. }) => assert_eq!(rate, 42),
            other => panic!("{other:?}"),
        }
    }
}
