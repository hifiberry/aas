//! Song metadata: the current track, filled by one source and read by the
//! remote outputs that can show it. See
//! `doc/plans/2026-11-22-song-metadata-design.md`.

pub mod acr;
pub mod config;
#[cfg(test)]
mod fake_acr;

use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A cover image as fetched.
#[derive(Debug, PartialEq)]
pub struct Cover {
    /// `"image/jpeg"` or `"image/png"`.
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

/// What is playing, as text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Track {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_s: Option<f64>,
    pub playing: bool,
}

/// The current track at one instant, with the generations an output
/// compares to know what changed since it last sent.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub track: Track,
    pub cover: Option<Arc<Cover>>,
    /// The position now, in seconds.
    pub position_s: Option<f64>,
    pub text_gen: u64,
    pub cover_gen: u64,
    pub progress_gen: u64,
}

/// A position this far from where the running position should be is a
/// jump, a seek or a new start, and is sent again.
const JUMP_S: f64 = 2.0;

#[derive(Default)]
struct State {
    track: Track,
    cover: Option<Arc<Cover>>,
    /// The position, and the instant it was taken.
    position: Option<(f64, Instant)>,
    text_gen: u64,
    cover_gen: u64,
    progress_gen: u64,
}

/// The current track, shared between the source's thread and the outputs.
/// The lock is held only to read or replace fields.
#[derive(Default)]
pub struct NowPlaying {
    state: Mutex<State>,
}

impl NowPlaying {
    pub fn new() -> Arc<NowPlaying> {
        Arc::new(NowPlaying::default())
    }

    /// Sets the track. New text moves the text and progress generations
    /// and drops the position; a change of `playing` alone moves nothing,
    /// so a pause changes nothing on a receiver.
    pub fn set_track(&self, track: Track) {
        let now = Instant::now();
        let mut s = self.state.lock().unwrap();
        let mut text = track.clone();
        text.playing = s.track.playing;
        if text != s.track {
            s.text_gen += 1;
            s.progress_gen += 1;
            s.position = None;
        } else if track.playing != s.track.playing {
            // Hold the position where it is when playback stops or starts;
            // on a resume the receiver's progress, which ran on, is set
            // again.
            s.position = s.position_at(now).map(|p| (p, now));
            if track.playing {
                s.progress_gen += 1;
            }
        }
        s.track = track;
    }

    /// Sets the cover; the same cover again moves nothing.
    pub fn set_cover(&self, cover: Option<Cover>) {
        let mut s = self.state.lock().unwrap();
        if s.cover.as_deref() != cover.as_ref() {
            s.cover = cover.map(Arc::new);
            s.cover_gen += 1;
        }
    }

    /// Sets the position at `at`. One that is not where the running
    /// position is, by more than `JUMP_S`, moves the progress generation.
    pub fn set_position(&self, position_s: f64, at: Instant) {
        let mut s = self.state.lock().unwrap();
        let jumped = s
            .position_at(at)
            .is_none_or(|p| (p - position_s).abs() > JUMP_S);
        if jumped {
            s.progress_gen += 1;
        }
        s.position = Some((position_s, at));
    }

    /// Empties the track, as when the source is gone.
    pub fn clear(&self) {
        let mut s = self.state.lock().unwrap();
        // Already empty: nothing to tell, however often a source retries.
        if s.track == Track::default() && s.cover.is_none() && s.position.is_none() {
            return;
        }
        *s = State {
            text_gen: s.text_gen + 1,
            cover_gen: s.cover_gen + 1,
            progress_gen: s.progress_gen + 1,
            ..State::default()
        };
    }

    /// The text, cover and progress generations, for a cheap look whether
    /// anything changed.
    pub fn generations(&self) -> (u64, u64, u64) {
        let s = self.state.lock().unwrap();
        (s.text_gen, s.cover_gen, s.progress_gen)
    }

    pub fn snapshot(&self, now: Instant) -> Snapshot {
        let s = self.state.lock().unwrap();
        Snapshot {
            track: s.track.clone(),
            cover: s.cover.clone(),
            position_s: s.position_at(now),
            text_gen: s.text_gen,
            cover_gen: s.cover_gen,
            progress_gen: s.progress_gen,
        }
    }
}

impl State {
    /// The position at `now`: running on while playing.
    fn position_at(&self, now: Instant) -> Option<f64> {
        let (p, at) = self.position?;
        if !self.track.playing {
            return Some(p);
        }
        Some(p + now.saturating_duration_since(at).as_secs_f64())
    }
}

/// Starts the configured source filling `store`; `None` when there is
/// none. The source runs until the daemon ends.
pub fn start(
    config: &config::MetadataConfig,
    store: Arc<NowPlaying>,
) -> Option<std::thread::JoinHandle<()>> {
    match config.source {
        config::Source::None => None,
        config::Source::Acr => {
            let base = config.url.clone();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            std::thread::Builder::new()
                .name("aas-metadata".into())
                .spawn(move || acr::run(base, store, stop))
                .ok()
        }
    }
}

/// The type of a cover, from its first bytes; `None` for anything but
/// JPEG and PNG.
pub fn cover_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn track(title: &str, playing: bool) -> Track {
        Track {
            title: Some(title.into()),
            artist: Some("Artist".into()),
            album: None,
            duration_s: Some(200.0),
            playing,
        }
    }

    fn jpeg() -> Cover {
        Cover {
            mime: "image/jpeg",
            bytes: vec![0xFF, 0xD8, 0xFF, 0xE0],
        }
    }

    #[test]
    fn a_new_track_moves_the_text_and_progress_generations() {
        let now = Instant::now();
        let np = NowPlaying::new();
        np.set_track(track("A", true));
        let s = np.snapshot(now);
        assert_eq!((s.text_gen, s.progress_gen, s.cover_gen), (1, 1, 0));
        assert_eq!(s.track.title.as_deref(), Some("A"));
        np.set_track(track("A", true));
        let s = np.snapshot(now);
        assert_eq!((s.text_gen, s.progress_gen), (1, 1));
    }

    #[test]
    fn a_cover_moves_only_its_generation() {
        let np = NowPlaying::new();
        np.set_track(track("A", true));
        np.set_cover(Some(jpeg()));
        let s = np.snapshot(Instant::now());
        assert_eq!((s.text_gen, s.cover_gen), (1, 1));
        assert_eq!(s.cover.as_deref(), Some(&jpeg()));
    }

    #[test]
    fn the_position_runs_on_while_playing() {
        let t0 = Instant::now();
        let np = NowPlaying::new();
        np.set_track(track("A", true));
        np.set_position(10.0, t0);
        let s = np.snapshot(t0 + Duration::from_secs(2));
        assert_eq!(s.position_s, Some(12.0));
    }

    #[test]
    fn the_position_stands_while_paused() {
        let t0 = Instant::now();
        let np = NowPlaying::new();
        np.set_track(track("A", false));
        np.set_position(10.0, t0);
        let s = np.snapshot(t0 + Duration::from_secs(2));
        assert_eq!(s.position_s, Some(10.0));
    }

    #[test]
    fn a_jump_in_position_moves_the_progress_generation() {
        let t0 = Instant::now();
        let np = NowPlaying::new();
        np.set_track(track("A", true));
        np.set_position(10.0, t0);
        let first = np.snapshot(t0).progress_gen;
        np.set_position(11.0, t0 + Duration::from_secs(1));
        assert_eq!(np.snapshot(t0).progress_gen, first);
        np.set_position(60.0, t0 + Duration::from_secs(2));
        assert_eq!(np.snapshot(t0).progress_gen, first + 1);
    }

    #[test]
    fn clearing_empties_everything_and_moves_every_generation() {
        let t0 = Instant::now();
        let np = NowPlaying::new();
        np.set_track(track("A", true));
        np.set_cover(Some(jpeg()));
        np.set_position(10.0, t0);
        let before = np.snapshot(t0);
        np.clear();
        let s = np.snapshot(t0);
        assert_eq!(s.track, Track::default());
        assert!(s.cover.is_none());
        assert_eq!(s.position_s, None);
        assert_eq!(s.text_gen, before.text_gen + 1);
        assert_eq!(s.cover_gen, before.cover_gen + 1);
        assert_eq!(s.progress_gen, before.progress_gen + 1);
    }

    #[test]
    fn clearing_an_empty_store_moves_nothing() {
        let np = NowPlaying::new();
        np.set_track(track("A", true));
        np.clear();
        let once = np.generations();
        np.clear();
        assert_eq!(np.generations(), once);
    }

    #[test]
    fn a_resume_moves_the_progress_generation() {
        let t0 = Instant::now();
        let np = NowPlaying::new();
        np.set_track(track("A", true));
        np.set_position(10.0, t0);
        np.set_track(track("A", false));
        let paused = np.generations();
        np.set_track(track("A", true));
        let resumed = np.generations();
        assert_eq!(resumed.0, paused.0, "a resume is no new text");
        assert_eq!(resumed.2, paused.2 + 1);
    }

    #[test]
    fn covers_are_known_by_their_first_bytes() {
        assert_eq!(cover_mime(&[0xFF, 0xD8, 0xFF, 0xE0, 0]), Some("image/jpeg"));
        assert_eq!(
            cover_mime(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("image/png")
        );
        assert_eq!(cover_mime(b"GIF89a"), None);
        assert_eq!(cover_mime(&[]), None);
    }
}
