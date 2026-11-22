//! Song metadata as AirPlay carries it: DMAP-tagged text, the cover, and
//! progress, each a `SET_PARAMETER` stamped with the stream's rtptime.

use crate::metadata::{Snapshot, Track};

/// The frames of a second of the stream.
const RATE: f64 = 44100.0;

/// One metadata request to send.
#[derive(Debug, Clone, PartialEq)]
pub enum Meta {
    /// `application/x-dmap-tagged`.
    Text(Vec<u8>),
    /// `image/jpeg` or `image/png`, and the bytes.
    Cover(&'static str, Vec<u8>),
    /// `image/none`: there is no cover.
    NoCover,
    /// `text/parameters`.
    Progress(String),
}

impl Meta {
    /// Which requests replace one another while waiting to go out.
    pub fn key(&self) -> &'static str {
        match self {
            Meta::Text(_) => "text",
            Meta::Cover(..) | Meta::NoCover => "cover",
            Meta::Progress(_) => "progress",
        }
    }

    /// The request's content type and body.
    pub fn content(&self) -> (&'static str, &[u8]) {
        match self {
            Meta::Text(b) => ("application/x-dmap-tagged", b),
            Meta::Cover(mime, b) => (mime, b),
            Meta::NoCover => ("image/none", &[]),
            Meta::Progress(p) => ("text/parameters", p.as_bytes()),
        }
    }
}

/// The generations of what a session has sent.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sent {
    pub text_gen: u64,
    pub cover_gen: u64,
    pub progress_gen: u64,
}

/// The track as a DMAP listing item: `mlit` holding `minm`, `asar` and
/// `asal`. A missing field is empty, so an empty track clears the text.
pub fn dmap(track: &Track) -> Vec<u8> {
    let mut items = Vec::new();
    for (tag, text) in [
        (b"minm", &track.title),
        (b"asar", &track.artist),
        (b"asal", &track.album),
    ] {
        let text = text.as_deref().unwrap_or("").as_bytes();
        items.extend_from_slice(tag);
        items.extend_from_slice(&(text.len() as u32).to_be_bytes());
        items.extend_from_slice(text);
    }
    let mut out = Vec::with_capacity(8 + items.len());
    out.extend_from_slice(b"mlit");
    out.extend_from_slice(&(items.len() as u32).to_be_bytes());
    out.extend_from_slice(&items);
    out
}

/// `progress: start/now/end`, in rtptime: the track started `position_s`
/// before `rtp_now` and lasts `duration_s`.
pub fn progress(rtp_now: u32, position_s: f64, duration_s: f64) -> String {
    let frames = |s: f64| (s.max(0.0) * RATE).round() as u64 as u32;
    let start = rtp_now.wrapping_sub(frames(position_s));
    let end = start.wrapping_add(frames(duration_s));
    format!("progress: {start}/{rtp_now}/{end}\r\n")
}

/// What changed since `sent`, which is brought up to date. Progress goes
/// out only with both a position and a length.
pub fn due(sent: &mut Sent, snap: &Snapshot, rtp_now: u32) -> Vec<Meta> {
    let mut out = Vec::new();
    if snap.text_gen != sent.text_gen {
        out.push(Meta::Text(dmap(&snap.track)));
        sent.text_gen = snap.text_gen;
    }
    if snap.cover_gen != sent.cover_gen {
        out.push(match &snap.cover {
            Some(c) => Meta::Cover(c.mime, c.bytes.clone()),
            None => Meta::NoCover,
        });
        sent.cover_gen = snap.cover_gen;
    }
    if snap.progress_gen != sent.progress_gen {
        if let (Some(p), Some(d)) = (snap.position_s, snap.track.duration_s) {
            out.push(Meta::Progress(progress(rtp_now, p, d)));
        }
        sent.progress_gen = snap.progress_gen;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::Cover;
    use std::sync::Arc;

    fn snap(
        track: Track,
        cover: Option<Cover>,
        position_s: Option<f64>,
        gens: (u64, u64, u64),
    ) -> Snapshot {
        Snapshot {
            track,
            cover: cover.map(Arc::new),
            position_s,
            text_gen: gens.0,
            cover_gen: gens.1,
            progress_gen: gens.2,
        }
    }

    fn tab(title: &str) -> Track {
        Track {
            title: Some(title.into()),
            artist: Some("A".into()),
            album: Some("B".into()),
            duration_s: Some(10.0),
            playing: true,
        }
    }

    #[test]
    fn the_text_is_a_dmap_listing_item() {
        assert_eq!(
            dmap(&tab("T")),
            b"mlit\0\0\0\x1bminm\0\0\0\x01Tasar\0\0\0\x01Aasal\0\0\0\x01B".to_vec()
        );
    }

    #[test]
    fn an_empty_track_clears_the_text() {
        assert_eq!(
            dmap(&Track::default()),
            b"mlit\0\0\0\x18minm\0\0\0\0asar\0\0\0\0asal\0\0\0\0".to_vec()
        );
    }

    #[test]
    fn text_is_utf8() {
        let t = Track {
            title: Some("Café".into()),
            ..Track::default()
        };
        let d = dmap(&t);
        assert_eq!(&d[8..17], b"minm\0\0\0\x05C");
    }

    #[test]
    fn progress_counts_back_from_now() {
        assert_eq!(
            progress(441000, 2.0, 10.0),
            "progress: 352800/441000/793800\r\n"
        );
    }

    #[test]
    fn progress_wraps_around_zero() {
        assert_eq!(
            progress(1000, 1.0, 2.0),
            "progress: 4294924196/1000/45100\r\n"
        );
    }

    #[test]
    fn a_new_track_sends_text_cover_and_progress() {
        let mut sent = Sent::default();
        let jpeg = Cover {
            mime: "image/jpeg",
            bytes: vec![0xFF, 0xD8, 0xFF],
        };
        let due = due(
            &mut sent,
            &snap(tab("T"), Some(jpeg), Some(2.0), (1, 1, 1)),
            441000,
        );
        assert_eq!(
            due,
            [
                Meta::Text(dmap(&tab("T"))),
                Meta::Cover("image/jpeg", vec![0xFF, 0xD8, 0xFF]),
                Meta::Progress("progress: 352800/441000/793800\r\n".into()),
            ]
        );
        assert_eq!(
            sent,
            Sent {
                text_gen: 1,
                cover_gen: 1,
                progress_gen: 1
            }
        );
    }

    #[test]
    fn only_what_changed_is_due() {
        let mut sent = Sent {
            text_gen: 1,
            cover_gen: 1,
            progress_gen: 1,
        };
        let png = Cover {
            mime: "image/png",
            bytes: vec![1],
        };
        let due = due(
            &mut sent,
            &snap(tab("T"), Some(png), Some(2.0), (1, 2, 1)),
            0,
        );
        assert_eq!(due, [Meta::Cover("image/png", vec![1])]);
        assert!(super::due(&mut sent, &snap(tab("T"), None, Some(2.0), (1, 2, 1)), 0).is_empty());
    }

    #[test]
    fn a_cleared_cover_is_sent_as_none() {
        let mut sent = Sent {
            text_gen: 1,
            cover_gen: 1,
            progress_gen: 1,
        };
        let due = due(&mut sent, &snap(tab("T"), None, None, (1, 2, 1)), 0);
        assert_eq!(due, [Meta::NoCover]);
        assert_eq!(Meta::NoCover.content(), ("image/none", &[][..]));
    }

    #[test]
    fn no_duration_no_progress() {
        let mut sent = Sent::default();
        let t = Track {
            duration_s: None,
            ..tab("T")
        };
        let due = due(&mut sent, &snap(t.clone(), None, Some(5.0), (1, 0, 1)), 0);
        assert_eq!(due, [Meta::Text(dmap(&t))]);
        assert_eq!(sent.progress_gen, 1);
    }

    #[test]
    fn nothing_known_sends_nothing() {
        let mut sent = Sent::default();
        assert!(due(&mut sent, &snap(Track::default(), None, None, (0, 0, 0)), 0).is_empty());
    }
}
