//! AudioControl (ACR) as a source of song metadata: its now-playing answer,
//! its events, and the thread that follows them.

use super::{NowPlaying, Track};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Follows ACR at `base` into `store` until `stop` is set: the events, a
/// fresh now-playing on each that matters, and the cover. A lost or refused
/// connection empties the store and is tried again after `RETRY`.
pub fn run(base: String, store: Arc<NowPlaying>, stop: Arc<AtomicBool>) {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .build();
    // Logged once per outage, not on every try.
    let mut down = false;
    while !stop.load(Ordering::SeqCst) {
        let mut follower = Follower {
            base: &base,
            store: &store,
            agent: &agent,
            active: None,
            cover_url: None,
        };
        match follower.follow(&stop, &mut down) {
            Ok(()) => return,
            Err(e) => {
                if !down {
                    eprintln!("aas: metadata: AudioControl at {base}: {e}");
                    down = true;
                }
            }
        }
        store.clear();
        let until = Instant::now() + RETRY;
        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// One connection to ACR.
struct Follower<'a> {
    base: &'a str,
    store: &'a NowPlaying,
    agent: &'a ureq::Agent,
    /// The active player's name, from the last now-playing.
    active: Option<String>,
    /// The cover URL last fetched.
    cover_url: Option<String>,
}

/// How long a read of the WebSocket waits before `stop` is looked at.
const POLL: Duration = Duration::from_millis(500);

impl Follower<'_> {
    /// Until the connection ends (an error) or `stop` is set (`Ok`).
    fn follow(&mut self, stop: &AtomicBool, down: &mut bool) -> Result<(), String> {
        use tungstenite::Message;
        let url = events_url(self.base);
        let host = url
            .strip_prefix("ws://")
            .and_then(|r| r.split('/').next())
            .ok_or("no host")?;
        let addr = std::net::ToSocketAddrs::to_socket_addrs(host)
            .map_err(|e| e.to_string())?
            .next()
            .ok_or("no address")?;
        let stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(POLL))
            .map_err(|e| e.to_string())?;
        let (mut ws, _) = tungstenite::client(url.as_str(), stream).map_err(|e| e.to_string())?;
        ws.send(Message::text(r#"{"players":null,"event_types":null}"#))
            .map_err(|e| e.to_string())?;
        if *down {
            eprintln!("aas: metadata: AudioControl at {} is back", self.base);
            *down = false;
        }
        self.refresh()?;
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            let message = match ws.read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(e.to_string()),
            };
            let text = match &message {
                Message::Text(t) => t.as_str(),
                Message::Close(_) => return Err("the connection closed".into()),
                _ => continue,
            };
            match event(text, self.active.as_deref()) {
                Event::Refresh => self.refresh()?,
                Event::Position(p) => self.store.set_position(p, Instant::now()),
                Event::Ignore => {}
            }
        }
    }

    /// Takes ACR's now-playing into the store, and the cover if it is new.
    fn refresh(&mut self) -> Result<(), String> {
        let body = self
            .agent
            .get(&format!("{}/now-playing", self.base.trim_end_matches('/')))
            .timeout(Duration::from_secs(5))
            .call()
            .map_err(|e| e.to_string())?
            .into_string()
            .map_err(|e| e.to_string())?;
        let Some(answer) = now_playing(&body) else {
            return Err("now-playing is not what was expected".into());
        };
        self.active = answer.player;
        self.store.set_track(answer.track);
        if let Some(p) = answer.position_s {
            self.store.set_position(p, Instant::now());
        }
        if answer.cover_url != self.cover_url {
            self.cover_url = answer.cover_url;
            let cover = self.cover_url.as_deref().and_then(|u| self.cover(u));
            self.store.set_cover(cover);
        }
        Ok(())
    }

    /// Fetches a cover; `None` when it fails, is too large or is neither
    /// JPEG nor PNG.
    fn cover(&self, url: &str) -> Option<super::Cover> {
        use std::io::Read;
        let response = self
            .agent
            .get(&cover_url(self.base, url))
            .timeout(COVER_TIMEOUT)
            .call()
            .ok()?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(COVER_MAX + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > COVER_MAX {
            return None;
        }
        let mime = super::cover_mime(&bytes)?;
        Some(super::Cover { mime, bytes })
    }
}

/// How long to wait before connecting again.
const RETRY: Duration = Duration::from_secs(5);
/// The largest cover taken.
const COVER_MAX: u64 = 4 * 1024 * 1024;
/// How long a cover may take.
const COVER_TIMEOUT: Duration = Duration::from_secs(10);

/// What `GET /api/now-playing` says.
#[derive(Debug, Clone, PartialEq)]
pub struct NowPlayingAnswer {
    /// The active player's name.
    pub player: Option<String>,
    pub track: Track,
    pub cover_url: Option<String>,
    pub position_s: Option<f64>,
}

/// What an event asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Fetch now-playing again.
    Refresh,
    /// The active player's position, in seconds.
    Position(f64),
    Ignore,
}

/// Reads the now-playing answer; `None` when it is not one.
pub fn now_playing(json: &str) -> Option<NowPlayingAnswer> {
    let v: Value = serde_json::from_str(json).ok()?;
    let v = v.as_object()?;
    let text = |song: &Value, key: &str| {
        song.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let song = v.get("song").filter(|s| s.is_object());
    let state = v
        .get("state")
        .or_else(|| v.get("player").and_then(|p| p.get("state")))
        .and_then(Value::as_str);
    let track = match song {
        None => Track::default(),
        Some(song) => Track {
            title: text(song, "title"),
            artist: text(song, "artist"),
            album: text(song, "album"),
            duration_s: song
                .get("duration")
                .and_then(Value::as_f64)
                .filter(|d| *d > 0.0),
            playing: state == Some("playing"),
        },
    };
    Some(NowPlayingAnswer {
        player: v
            .get("player")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .map(String::from),
        cover_url: song.and_then(|s| text(s, "cover_art_url")),
        position_s: v.get("position").and_then(Value::as_f64),
        track,
    })
}

/// What an event from ACR's WebSocket asks for, `active` being the active
/// player's name.
pub fn event(json: &str, active: Option<&str>) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Event::Ignore;
    };
    let kind = v.get("type").and_then(Value::as_str);
    if kind == Some("active_player_changed") {
        return Event::Refresh;
    }
    let player = v.get("player_name").and_then(Value::as_str);
    if player.is_none() || player != active {
        return Event::Ignore;
    }
    match kind {
        Some("song_changed" | "song_information_update" | "state_changed") => Event::Refresh,
        Some("position_changed") => v
            .get("position")
            .and_then(Value::as_f64)
            .map_or(Event::Ignore, Event::Position),
        _ => Event::Ignore,
    }
}

/// The URL to fetch a cover from: an absolute URL as it is, a path on the
/// host of `base`.
pub fn cover_url(base: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.to_string();
    }
    let rest = base.strip_prefix("http://").unwrap_or(base);
    let host = rest.split('/').next().unwrap_or(rest);
    let slash = if url.starts_with('/') { "" } else { "/" };
    format!("http://{host}{slash}{url}")
}

/// ACR's WebSocket beside its API at `base`.
pub fn events_url(base: &str) -> String {
    let rest = base.strip_prefix("http://").unwrap_or(base);
    format!("ws://{}/events", rest.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::super::fake_acr::FakeAcr;
    use super::*;

    fn playing(title: &str, cover: &str) -> String {
        format!(
            r#"{{"player": {{"name": "analog"}}, "state": "playing",
                "song": {{"title": "{title}", "artist": "A", "cover_art_url": "{cover}"}}}}"#
        )
    }

    /// Runs the source against `url` until the returned flag is set.
    fn follow(url: String) -> (Arc<NowPlaying>, Arc<AtomicBool>) {
        let store = NowPlaying::new();
        let stop = Arc::new(AtomicBool::new(false));
        let (s, f) = (store.clone(), stop.clone());
        std::thread::spawn(move || run(url, s, f));
        (store, stop)
    }

    /// Waits up to `secs` for `test` on the store.
    fn until(
        store: &NowPlaying,
        secs: u64,
        test: impl Fn(&super::super::Snapshot) -> bool,
    ) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if test(&store.snapshot(Instant::now())) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn title(s: &super::super::Snapshot) -> Option<&str> {
        s.track.title.as_deref()
    }

    #[test]
    fn the_track_and_its_cover_arrive() {
        let acr = FakeAcr::start(&playing("One", "/cover.jpg"));
        let (store, stop) = follow(acr.url());
        assert!(until(&store, 3, |s| title(s) == Some("One")
            && s.cover.as_ref().map(|c| c.mime) == Some("image/jpeg")));
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn a_song_change_is_fetched_again() {
        let acr = FakeAcr::start(&playing("One", "/cover.jpg"));
        let (store, stop) = follow(acr.url());
        assert!(until(&store, 3, |s| title(s) == Some("One")));
        acr.set_now_playing(&playing("Two", "/cover.jpg"));
        acr.push(r#"{"type": "song_changed", "player_name": "analog", "player_id": "analog"}"#);
        assert!(until(&store, 3, |s| title(s) == Some("Two")));
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn an_event_of_another_player_fetches_nothing() {
        let acr = FakeAcr::start(&playing("One", "/cover.jpg"));
        let (store, stop) = follow(acr.url());
        assert!(until(&store, 3, |s| title(s) == Some("One")));
        acr.set_now_playing(&playing("Other", "/cover.jpg"));
        acr.push(r#"{"type": "song_changed", "player_name": "spotify", "player_id": "spotify"}"#);
        assert!(!until(&store, 1, |s| title(s) == Some("Other")));
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn a_cover_that_hangs_does_not_hold_the_text() {
        let acr = FakeAcr::start(&playing("One", "/slow.jpg"));
        let (store, stop) = follow(acr.url());
        assert!(until(&store, 3, |s| title(s) == Some("One")));
        assert!(store.snapshot(Instant::now()).cover.is_none());
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn a_cover_too_large_is_dropped() {
        let acr = FakeAcr::start(&playing("One", "/big.jpg"));
        let (store, stop) = follow(acr.url());
        assert!(until(&store, 3, |s| title(s) == Some("One")));
        let end = Instant::now() + Duration::from_secs(3);
        while !acr.saw("/big.jpg") && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(1500));
        let s = store.snapshot(Instant::now());
        assert!(s.cover.is_none());
        assert_eq!(s.cover_gen, 0, "a cover was set and taken back");
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn acr_gone_empties_the_track_and_it_comes_back() {
        let acr = FakeAcr::start(&playing("One", "/cover.jpg"));
        let (store, stop) = follow(acr.url());
        assert!(until(&store, 3, |s| title(s) == Some("One")));
        acr.close_sockets();
        assert!(until(&store, 3, |s| title(s).is_none()));
        assert!(until(&store, 10, |s| title(s) == Some("One")));
        assert_eq!(acr.sockets(), 2);
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn acr_not_there_at_start_is_tried_again() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let (store, stop) = follow(format!("http://{addr}/api"));
        std::thread::sleep(Duration::from_secs(1));
        assert!(title(&store.snapshot(Instant::now())).is_none());
        let _acr = FakeAcr::start_on(
            std::net::TcpListener::bind(addr).unwrap(),
            &playing("One", "/cover.jpg"),
        );
        assert!(until(&store, 10, |s| title(s) == Some("One")));
        stop.store(true, Ordering::SeqCst);
    }

    /// As the turntable board's ACR answered while a record played.
    const ANALOG: &str = r#"{
        "player": {"name": "analog", "id": "analog", "state": "playing", "is_active": true,
                   "position": null, "capabilities": []},
        "song": {"title": "My Brother's a Basehead", "artist": "De La Soul",
                 "album": "De La Soul Is Dead",
                 "cover_art_url": "https://lastfm-img.freetls.fastly.net/i/u/300x300/f402.png",
                 "liked": false, "metadata": {"cover_art_source": "lastfm"}},
        "state": "playing", "shuffle": false, "loop_mode": "no", "position": null
    }"#;

    #[test]
    fn the_now_playing_answer_gives_the_active_players_track() {
        let a = now_playing(ANALOG).unwrap();
        assert_eq!(a.player.as_deref(), Some("analog"));
        assert_eq!(
            a.track,
            Track {
                title: Some("My Brother's a Basehead".into()),
                artist: Some("De La Soul".into()),
                album: Some("De La Soul Is Dead".into()),
                duration_s: None,
                playing: true,
            }
        );
        assert_eq!(
            a.cover_url.as_deref(),
            Some("https://lastfm-img.freetls.fastly.net/i/u/300x300/f402.png")
        );
        assert_eq!(a.position_s, None);
    }

    #[test]
    fn duration_and_position_are_read() {
        let a = now_playing(
            r#"{"player": {"name": "mpd"}, "song": {"title": "T", "duration": 180},
                "state": "paused", "position": 45.5}"#,
        )
        .unwrap();
        assert_eq!(a.track.duration_s, Some(180.0));
        assert_eq!(a.position_s, Some(45.5));
        assert!(!a.track.playing);
    }

    #[test]
    fn no_song_gives_an_empty_track() {
        let a = now_playing(r#"{"player": {"name": "mpd"}, "song": null, "state": "stopped"}"#)
            .unwrap();
        assert_eq!(a.track, Track::default());
        assert_eq!(a.cover_url, None);
    }

    fn ev(kind: &str, player: &str, extra: &str) -> String {
        format!(
            r#"{{"type": "{kind}", "player_name": "{player}", "player_id": "{player}",
                "source": {{"player_name": "{player}", "player_id": "{player}:6600"}}{extra}}}"#
        )
    }

    #[test]
    fn a_song_change_of_the_active_player_refreshes() {
        for kind in ["song_changed", "song_information_update", "state_changed"] {
            assert_eq!(
                event(&ev(kind, "analog", ""), Some("analog")),
                Event::Refresh
            );
        }
    }

    #[test]
    fn events_of_another_player_are_ignored() {
        let e = ev("song_changed", "spotify", "");
        assert_eq!(event(&e, Some("analog")), Event::Ignore);
        let p = ev("position_changed", "spotify", r#", "position": 3.0"#);
        assert_eq!(event(&p, Some("analog")), Event::Ignore);
    }

    #[test]
    fn a_change_of_active_player_always_refreshes() {
        let e = ev(
            "active_player_changed",
            "spotify",
            r#", "new_player_id": "spotify:librespot""#,
        );
        assert_eq!(event(&e, Some("analog")), Event::Refresh);
        assert_eq!(event(&e, None), Event::Refresh);
    }

    #[test]
    fn the_position_of_the_active_player_is_taken() {
        let e = ev("position_changed", "analog", r#", "position": 45.5"#);
        assert_eq!(event(&e, Some("analog")), Event::Position(45.5));
    }

    #[test]
    fn garbage_is_ignored() {
        assert_eq!(event("not json", Some("analog")), Event::Ignore);
        assert_eq!(
            event(r#"{"type": "welcome"}"#, Some("analog")),
            Event::Ignore
        );
        assert_eq!(
            event(&ev("volume_changed", "analog", ""), Some("analog")),
            Event::Ignore
        );
        assert_eq!(now_playing("[]"), None);
        assert_eq!(now_playing("not json"), None);
    }

    #[test]
    fn cover_paths_are_joined_to_the_host() {
        let base = "http://127.0.0.1:1080/api";
        assert_eq!(
            cover_url(base, "/api/coverart/x"),
            "http://127.0.0.1:1080/api/coverart/x"
        );
        assert_eq!(
            cover_url(base, "https://lastfm.example/c.png"),
            "https://lastfm.example/c.png"
        );
        assert_eq!(
            cover_url(base, "http://192.0.2.1/c.jpg"),
            "http://192.0.2.1/c.jpg"
        );
    }

    #[test]
    fn the_events_url_is_the_websocket_beside_the_api() {
        assert_eq!(
            events_url("http://127.0.0.1:1080/api"),
            "ws://127.0.0.1:1080/api/events"
        );
        assert_eq!(
            events_url("http://192.0.2.1:1080/api/"),
            "ws://192.0.2.1:1080/api/events"
        );
    }
}
