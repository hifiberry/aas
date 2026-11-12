//! Where each channel of a stereo stream should come out on a card with
//! more channels, and the check of a captured frame against it.

/// One of the two channels of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    fn index(self) -> usize {
        match self {
            Side::Left => 0,
            Side::Right => 1,
        }
    }
}

/// For each captured channel, the side of the stream it should carry, or
/// none for a channel that should be silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routing {
    sides: Vec<Option<Side>>,
    /// The first channel that carries each side.
    first: [usize; 2],
}

impl Routing {
    /// Left on channel 1 and right on channel 2, as a stereo card plays.
    pub fn stereo() -> Routing {
        Routing::parse(2, "1=L,2=R").expect("the stereo routing parses")
    }

    /// `text` lists the channels that carry the stream, counted from 1, as
    /// `channel=side` separated by commas, such as `1=L,2=R,7=L,8=R`. A
    /// side is `L` or `R`. Each side must appear at least once, so that a
    /// captured frame can be placed in the stream. A channel not listed
    /// must be silent.
    pub fn parse(channels: usize, text: &str) -> Result<Routing, String> {
        if !(1..=8).contains(&channels) {
            return Err(format!("{channels} channels: 1 to 8 can be captured"));
        }
        let mut sides = vec![None; channels];
        for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (channel, side) = item
                .split_once('=')
                .ok_or_else(|| format!("{item}: not channel=side"))?;
            let channel: usize = channel
                .trim()
                .parse()
                .map_err(|_| format!("{item}: the channel is not a number"))?;
            if !(1..=channels).contains(&channel) {
                return Err(format!("{item}: channel {channel} is not 1 to {channels}"));
            }
            let side = match side.trim() {
                "L" | "l" => Side::Left,
                "R" | "r" => Side::Right,
                other => return Err(format!("{item}: the side {other} is not L or R")),
            };
            if sides[channel - 1].replace(side).is_some() {
                return Err(format!("{item}: channel {channel} is named twice"));
            }
        }
        let find = |want: Side| {
            sides
                .iter()
                .position(|s| *s == Some(want))
                .ok_or_else(|| format!("{text}: no channel carries {want:?}"))
        };
        let first = [find(Side::Left)?, find(Side::Right)?];
        Ok(Routing { sides, first })
    }

    pub fn channels(&self) -> usize {
        self.sides.len()
    }

    /// The stereo frame a captured frame of `channels()` samples carries,
    /// taken from the first channel of each side, and whether every other
    /// channel agrees: a copy of its side, or zero where none is routed.
    pub fn split(&self, frame: &[i32]) -> ([i32; 2], bool) {
        let stereo = [frame[self.first[0]], frame[self.first[1]]];
        let fits = self
            .sides
            .iter()
            .zip(frame)
            .all(|(side, sample)| match side {
                Some(side) => *sample == stereo[side.index()],
                None => *sample == 0,
            });
        (stereo, fits)
    }

    /// The routing as `parse` takes it.
    pub fn describe(&self) -> String {
        let named: Vec<String> = self
            .sides
            .iter()
            .enumerate()
            .filter_map(|(i, side)| {
                side.map(|s| {
                    let s = if s == Side::Left { 'L' } else { 'R' };
                    format!("{}={s}", i + 1)
                })
            })
            .collect();
        named.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_is_identity_and_always_fits() {
        let r = Routing::stereo();
        assert_eq!(r.channels(), 2);
        assert_eq!(r.split(&[5, -7]), ([5, -7], true));
        assert_eq!(r.split(&[0, 0]), ([0, 0], true));
        assert_eq!(r.describe(), "1=L,2=R");
    }

    #[test]
    fn a_swap_takes_left_from_channel_2() {
        let r = Routing::parse(2, "1=R,2=L").unwrap();
        assert_eq!(r.split(&[5, -7]), ([-7, 5], true));
    }

    #[test]
    fn eight_channels_with_copies_and_silence() {
        let r = Routing::parse(8, "1=L,2=R,7=L,8=R").unwrap();
        assert_eq!(r.split(&[1, 2, 0, 0, 0, 0, 1, 2]), ([1, 2], true));
        // A copy that differs, and a silent channel that is not.
        assert!(!r.split(&[1, 2, 0, 0, 0, 0, 1, 3]).1);
        assert!(!r.split(&[1, 2, 0, 0, 9, 0, 1, 2]).1);
        assert_eq!(r.describe(), "1=L,2=R,7=L,8=R");
    }

    #[test]
    fn bad_routings_are_refused() {
        for (channels, text) in [
            (2, "1=L"),
            (2, "1=L,2=L"),
            (2, "1=L,3=R"),
            (2, "1=L,1=R"),
            (2, "1=X,2=R"),
            (2, "one=L,2=R"),
            (2, "1L,2=R"),
            (9, "1=L,2=R"),
            (0, ""),
        ] {
            assert!(Routing::parse(channels, text).is_err(), "{channels} {text}");
        }
    }
}
