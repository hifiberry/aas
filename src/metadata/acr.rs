//! AudioControl (ACR) as a source of song metadata: its now-playing answer,
//! its events, and the thread that follows them.

use super::Track;
use serde_json::Value;

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
    use super::*;

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
