//! DoP (DSD over PCM) detection.

const MARKERS: [u8; 2] = [0x05, 0xFA];
const MIN_FRAMES: usize = 16;

/// True when the frames carry alternating DoP markers in the top byte of
/// both channels. The block may start on either marker.
pub fn is_dop(frames: &[[i32; 2]]) -> bool {
    if frames.len() < MIN_FRAMES {
        return false;
    }
    let marker = |s: i32| (s >> 24) as u8;
    let Some(start) = MARKERS.iter().position(|&m| m == marker(frames[0][0])) else {
        return false;
    };
    frames.iter().enumerate().all(|(i, f)| {
        let want = MARKERS[(start + i) % 2];
        marker(f[0]) == want && marker(f[1]) == want
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dop(n: usize, first_is_05: bool) -> Vec<[i32; 2]> {
        (0..n)
            .map(|i| {
                let m: u32 = if (i % 2 == 0) == first_is_05 {
                    0x05
                } else {
                    0xFA
                };
                let s = ((m << 24) | 0x0012_3400) as i32;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn alternating_05_and_fa_markers_on_both_channels_are_dop() {
        assert!(is_dop(&dop(32, true)));
        assert!(is_dop(&dop(32, false)));
        assert!(is_dop(&dop(17, true)));
    }

    #[test]
    fn fewer_than_16_frames_are_not_dop() {
        assert!(!is_dop(&dop(15, true)));
        assert!(is_dop(&dop(16, true)));
    }

    #[test]
    fn one_wrong_marker_is_not_dop() {
        let mut f = dop(32, true);
        f[20][1] = 0x0600_0000;
        assert!(!is_dop(&f));
        let mut g = dop(32, true);
        g[7] = [g[6][0], g[6][1]];
        assert!(!is_dop(&g));
    }

    #[test]
    fn a_full_scale_square_wave_is_not_dop() {
        let f: Vec<[i32; 2]> = (0..32)
            .map(|i| {
                if i % 2 == 0 {
                    [i32::MAX; 2]
                } else {
                    [i32::MIN; 2]
                }
            })
            .collect();
        assert!(!is_dop(&f));
    }
}
