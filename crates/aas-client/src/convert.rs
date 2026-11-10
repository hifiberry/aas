//! Conversion of client sample formats to stereo `i32` frames.

/// Sample formats a client may play. All are little endian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientFormat {
    S16,
    /// 24 significant bits in a 4 byte container.
    S24,
    /// 24 bits in 3 bytes.
    S24Packed,
    S32,
    Float,
}

impl ClientFormat {
    /// Bytes per sample.
    pub fn sample_bytes(self) -> usize {
        match self {
            ClientFormat::S16 => 2,
            ClientFormat::S24Packed => 3,
            ClientFormat::S24 | ClientFormat::S32 | ClientFormat::Float => 4,
        }
    }
}

fn sample(format: ClientFormat, b: &[u8]) -> i32 {
    match format {
        ClientFormat::S16 => (i16::from_le_bytes([b[0], b[1]]) as i32) << 16,
        // The shift drops the container's top byte and sign extends bit 23.
        ClientFormat::S24 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) << 8,
        ClientFormat::S24Packed => i32::from_le_bytes([0, b[0], b[1], b[2]]),
        ClientFormat::S32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        // Float to int casts saturate and map NaN to 0.
        ClientFormat::Float => (f32::from_le_bytes([b[0], b[1], b[2], b[3]]) * 2147483648.0) as i32,
    }
}

/// Converts interleaved client samples to stereo frames and returns the number
/// of frames written to `out`. Mono is copied to both channels. Of more than
/// two channels only the first two are used. Incomplete trailing frames are
/// ignored.
pub fn to_stereo_s32(
    format: ClientFormat,
    channels: u8,
    input: &[u8],
    out: &mut [[i32; 2]],
) -> usize {
    let channels = channels as usize;
    if channels == 0 {
        return 0;
    }
    let width = format.sample_bytes();
    let frame_bytes = width * channels;
    let n = (input.len() / frame_bytes).min(out.len());
    for (frame, dst) in input.chunks_exact(frame_bytes).zip(out.iter_mut()).take(n) {
        let left = sample(format, &frame[..width]);
        let right = if channels == 1 {
            left
        } else {
            sample(format, &frame[width..2 * width])
        };
        *dst = [left, right];
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(format: ClientFormat, channels: u8, input: &[u8]) -> Vec<[i32; 2]> {
        let mut out = vec![[0; 2]; 8];
        let n = to_stereo_s32(format, channels, input, &mut out);
        out.truncate(n);
        out
    }

    #[test]
    fn s16_shifts_left_16_exactly() {
        let input = [0x34, 0x12, 0xFF, 0xFF];
        assert_eq!(
            one(ClientFormat::S16, 2, &input),
            [[0x1234_0000, 0xFFFF_0000u32 as i32]]
        );
    }

    #[test]
    fn s24_shifts_left_8_exactly() {
        let packed = [0x56, 0x34, 0x12, 0xFF, 0xFF, 0xFF];
        assert_eq!(
            one(ClientFormat::S24Packed, 2, &packed),
            [[0x1234_5600, 0xFFFF_FF00u32 as i32]]
        );
        // Container byte 3 is garbage in this one and must be ignored.
        let wide = [0x56, 0x34, 0x12, 0x7F, 0xFF, 0xFF, 0xFF, 0x00];
        assert_eq!(
            one(ClientFormat::S24, 2, &wide),
            [[0x1234_5600, 0xFFFF_FF00u32 as i32]]
        );
    }

    #[test]
    fn s32_passes_unchanged() {
        let mut input = Vec::new();
        input.extend_from_slice(&0x1234_5678i32.to_le_bytes());
        input.extend_from_slice(&i32::MIN.to_le_bytes());
        assert_eq!(one(ClientFormat::S32, 2, &input), [[0x1234_5678, i32::MIN]]);
    }

    #[test]
    fn float_full_scale_saturates() {
        let mut input = Vec::new();
        input.extend_from_slice(&1.5f32.to_le_bytes());
        input.extend_from_slice(&(-1.5f32).to_le_bytes());
        assert_eq!(one(ClientFormat::Float, 2, &input), [[i32::MAX, i32::MIN]]);
        let mut half = Vec::new();
        half.extend_from_slice(&0.5f32.to_le_bytes());
        half.extend_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(one(ClientFormat::Float, 2, &half), [[1 << 30, 0]]);
    }

    #[test]
    fn mono_is_copied_to_both_channels() {
        let input = [0x34, 0x12, 0x00, 0x80];
        assert_eq!(
            one(ClientFormat::S16, 1, &input),
            [[0x1234_0000, 0x1234_0000], [i32::MIN, i32::MIN]]
        );
    }

    #[test]
    fn extra_channels_and_short_input() {
        let input = [1, 0, 2, 0, 3, 0, 9];
        assert_eq!(one(ClientFormat::S16, 3, &input), [[1 << 16, 2 << 16]]);
        assert!(one(ClientFormat::S16, 0, &input).is_empty());
        let mut out = [[0; 2]; 1];
        assert_eq!(to_stereo_s32(ClientFormat::S16, 1, &input, &mut out), 1);
    }
}
