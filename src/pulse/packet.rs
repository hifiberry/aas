//! PulseAudio's pstream framing: a 5-word big-endian descriptor and its
//! payload.

use std::io;

/// The descriptor: length, channel, offset high and low, flags.
pub const HEADER_LEN: usize = 20;
/// The largest frame this endpoint accepts, so a bad length cannot make it
/// allocate without bound.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// The channel a packet uses, as opposed to a memblock.
const PACKET_CHANNEL: u32 = u32::MAX;
/// Flags that say an audio block is a shared-memory reference, which this
/// endpoint declines.
const SHM_FLAGS: u32 = 0xff00_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Seek {
    Relative = 0,
    Absolute = 1,
    RelativeOnRead = 2,
    RelativeEnd = 3,
}

impl Seek {
    fn from_u32(value: u32) -> Option<Seek> {
        Some(match value {
            0 => Seek::Relative,
            1 => Seek::Absolute,
            2 => Seek::RelativeOnRead,
            3 => Seek::RelativeEnd,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memblock {
    pub stream: u32,
    pub offset: i64,
    pub seek: Seek,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub channel: u32,
    pub payload: Vec<u8>,
    pub memblock: Option<Memblock>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[derive(Default)]
pub struct Encoder;

impl Encoder {
    fn descriptor(length: u32, channel: u32, offset: i64, flags: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN);
        for word in [
            length,
            channel,
            (offset as u64 >> 32) as u32,
            offset as u32,
            flags,
        ] {
            out.extend_from_slice(&word.to_be_bytes());
        }
        out
    }

    pub fn packet(&self, channel: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Self::descriptor(payload.len() as u32, channel, 0, 0);
        out.extend_from_slice(payload);
        out
    }

    pub fn memblock(&self, stream: u32, data: &[u8]) -> Vec<u8> {
        self.memblock_with(stream, 0, Seek::Relative, data)
    }

    pub fn memblock_with(&self, stream: u32, offset: i64, seek: Seek, data: &[u8]) -> Vec<u8> {
        let mut out = Self::descriptor(data.len() as u32, stream, offset, seek as u32);
        out.extend_from_slice(data);
        out
    }
}

#[derive(Default)]
pub struct Reader {
    buf: Vec<u8>,
}

impl Reader {
    pub fn new() -> Reader {
        Reader::default()
    }

    /// Adds what was read and returns every frame that is now complete.
    pub fn read(&mut self, bytes: &[u8]) -> io::Result<Vec<Frame>> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            if self.buf.len() < HEADER_LEN {
                return Ok(frames);
            }
            let word = |at: usize| {
                u32::from_be_bytes([
                    self.buf[at],
                    self.buf[at + 1],
                    self.buf[at + 2],
                    self.buf[at + 3],
                ])
            };
            let length = word(0) as usize;
            let channel = word(4);
            let offset = ((u64::from(word(8)) << 32) | u64::from(word(12))) as i64;
            let flags = word(16);
            if length > MAX_FRAME {
                return Err(invalid("a frame longer than 16 MiB"));
            }
            if flags & SHM_FLAGS != 0 {
                return Err(invalid("a shared-memory block, which is declined"));
            }
            if self.buf.len() < HEADER_LEN + length {
                return Ok(frames);
            }
            let payload: Vec<u8> = self.buf[HEADER_LEN..HEADER_LEN + length].to_vec();
            self.buf.drain(..HEADER_LEN + length);
            if channel == PACKET_CHANNEL {
                frames.push(Frame {
                    channel,
                    payload,
                    memblock: None,
                });
            } else {
                let seek = Seek::from_u32(flags & 0xff)
                    .ok_or_else(|| invalid("an unknown seek mode"))?;
                frames.push(Frame {
                    channel,
                    payload: Vec::new(),
                    memblock: Some(Memblock {
                        stream: channel,
                        offset,
                        seek,
                        data: payload,
                    }),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packet_split_across_reads_is_one_frame() {
        let bytes = Encoder.packet(0xffff_ffff, &[1, 2, 3]);
        let mut reader = Reader::new();
        assert!(reader.read(&bytes[..7]).unwrap().is_empty());
        let frames = reader.read(&bytes[7..]).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].channel, 0xffff_ffff);
        assert_eq!(frames[0].payload, [1, 2, 3]);
    }

    #[test]
    fn a_memblock_carries_its_stream_and_seek() {
        let bytes = Encoder.memblock(7, &[9, 8]);
        let frames = Reader::new().read(&bytes).unwrap();
        let m = frames[0].memblock.as_ref().unwrap();
        assert_eq!(
            (m.stream, m.seek, m.data.as_slice()),
            (7, Seek::Relative, &[9, 8][..])
        );
    }

    #[test]
    fn an_oversized_length_is_refused() {
        let mut bytes = Encoder.packet(0xffff_ffff, &[]);
        bytes[..4].copy_from_slice(&(16 * 1024 * 1024u32 + 1).to_be_bytes());
        assert!(Reader::new().read(&bytes).is_err());
    }

    #[test]
    fn a_shared_memory_block_is_refused() {
        let mut bytes = Encoder.memblock(7, &[1]);
        bytes[16..20].copy_from_slice(&0x8000_0000u32.to_be_bytes());
        assert!(Reader::new().read(&bytes).is_err());
    }
}
