//! DoP (DSD over PCM) detection.

const MARKERS: [u8; 2] = [0x05, 0xFA];
/// Fewer frames than this cannot be told from PCM.
pub const MIN_FRAMES: usize = 16;

/// The two payload bytes of DoP silence, in the place the samples carry them.
const SILENCE: u32 = 0x0069_6900;

/// What a block of frames shows about DoP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// No DoP.
    Nothing,
    /// A run of at least `MIN_FRAMES` starts at this frame.
    At(usize),
    /// A shorter run starts at this frame and reaches the end of the block,
    /// so it takes more frames to tell.
    Unclear(usize),
}

/// The position in `MARKERS` of the marker on both channels of a frame.
fn marker_of(frame: &[i32; 2]) -> Option<usize> {
    let top = |s: i32| (s >> 24) as u8;
    if top(frame[0]) != top(frame[1]) {
        return None;
    }
    MARKERS.iter().position(|&m| m == top(frame[0]))
}

/// Looks for the first run of alternating markers.
pub fn find(frames: &[[i32; 2]]) -> Found {
    let mut run = 0;
    let mut last = None;
    for (i, frame) in frames.iter().enumerate() {
        let marker = marker_of(frame);
        run = match (marker, last) {
            (None, _) => 0,
            (Some(now), Some(before)) if now != before => run + 1,
            (Some(_), _) => 1,
        };
        last = marker;
        if run == MIN_FRAMES {
            return Found::At(i + 1 - MIN_FRAMES);
        }
    }
    if run > 0 {
        Found::Unclear(frames.len() - run)
    } else {
        Found::Nothing
    }
}

/// Frames at the start of the block that carry a marker, in any order. A
/// stream known to be DoP keeps its markers across a join of two tracks,
/// which can repeat one.
pub fn marked_run(frames: &[[i32; 2]]) -> usize {
    frames
        .iter()
        .position(|f| marker_of(f).is_none())
        .unwrap_or(frames.len())
}

/// The marker that follows a frame. A frame without one is followed by 0x05.
pub fn marker_after(frame: &[i32; 2]) -> u8 {
    MARKERS[marker_of(frame).map_or(0, |i| (i + 1) % 2)]
}

/// Fills `frames` with DoP silence starting on `marker`, which keeps the DAC
/// in DoP mode. Returns the marker that comes next.
pub fn fill_silence(frames: &mut [[i32; 2]], mut marker: u8) -> u8 {
    for frame in frames.iter_mut() {
        let sample = ((u32::from(marker) << 24) | SILENCE) as i32;
        *frame = [sample, sample];
        marker = marker_after(frame);
    }
    marker
}

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
    fn find_sees_a_run_at_the_start_and_further_in() {
        assert_eq!(find(&dop(32, true)), Found::At(0));
        let mut block = vec![[0; 2]; 20];
        block.extend(dop(16, false));
        assert_eq!(find(&block), Found::At(20));
    }

    #[test]
    fn find_leaves_a_short_run_at_the_end_open() {
        let mut block = vec![[0; 2]; 20];
        block.extend(dop(15, true));
        assert_eq!(find(&block), Found::Unclear(20));
        assert_eq!(find(&dop(1, true)), Found::Unclear(0));
    }

    #[test]
    fn find_passes_over_a_short_run_that_ends() {
        let mut block = dop(15, true);
        block.extend([[0; 2]; 4]);
        assert_eq!(find(&block), Found::Nothing);
        assert_eq!(find(&[]), Found::Nothing);
    }

    #[test]
    fn find_restarts_the_count_at_a_repeated_marker() {
        let mut block = dop(9, true);
        block.extend(dop(9, true));
        assert_eq!(find(&block), Found::Unclear(9));
    }

    #[test]
    fn different_markers_on_the_two_channels_are_not_a_marker() {
        assert_eq!(marked_run(&[[0x0500_0000, 0xFA00_0000u32 as i32]]), 0);
    }

    #[test]
    fn a_marked_run_survives_a_repeated_marker_and_ends_at_the_first_gap() {
        let mut block = dop(9, true);
        block.extend(dop(9, true));
        assert_eq!(marked_run(&block), 18);
        block.push([0; 2]);
        block.extend(dop(4, true));
        assert_eq!(marked_run(&block), 18);
    }

    #[test]
    fn dop_silence_alternates_and_continues_the_phase() {
        let mut block = [[0; 2]; 3];
        assert_eq!(fill_silence(&mut block, 0xFA), 0x05);
        let high = 0xFA69_6900u32 as i32;
        assert_eq!(block, [[high; 2], [0x0569_6900; 2], [high; 2]]);
        assert_eq!(marker_after(&block[2]), 0x05);
        assert_eq!(marker_after(&block[1]), 0xFA);
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
