# Song Metadata Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** aas pulls the current track (text, cover, progress) from ACR and sends it to its AirPlay receivers.

**Architecture:** A shared `NowPlaying` store in a new `src/metadata/` module, filled by one source thread (`acr`), read by every AirPlay output thread, which compares generations and sends what changed as `SET_PARAMETER` requests without waiting. `[metadata]` in the config turns it on; `GET /metadata` shows the store.

**Tech Stack:** Rust 2024, `tungstenite` (already a dependency) for ACR's WebSocket, `ureq` 2 with rustls for REST and covers, `serde_json`.

**Spec:** `doc/plans/2026-11-22-song-metadata-design.md`

## Global Constraints

- Nothing runs on the audio thread; the source has its own thread; an AirPlay session never fails, stalls or resends because of metadata.
- Only ACR's active player's track goes out.
- ACR base URL default `http://127.0.0.1:1080/api`; events at `ws://` + host + `/api/events`; now-playing at `<url>/now-playing`.
- Covers: `image/jpeg` or `image/png` only, recognised by their first bytes; anything else is dropped. Size cap 4 MiB, fetch timeout 10 s.
- Metadata requests: `Content-Type` `application/x-dmap-tagged`, `image/jpeg`, `image/png`, `image/none` (empty body, clears the cover) and `text/parameters` (`progress: start/curr/end`), each with `RTP-Info: rtptime=<the next packet's rtptime>`.
- A refusal or a missing answer to a metadata request is ignored; it is logged once per session.
- Format single files with `rustfmt --edition 2024 <file>`, never `cargo fmt`. Conventional commits, no tool names in them.

## Review Focus

1. ACR not running when aas starts, or going away: aas runs on, the track is empty, the source retries every 5 s and logs once per outage. Test in Task 4.
2. A cover URL that hangs or serves 100 MB: the text still goes out at once; the cover is dropped. Test in Task 4.
3. Events for a player that is not active (a paused Spotify while the record plays): ignored. Test in Task 3.
4. A receiver that answers 400 or nothing to metadata: the session goes on playing. Test in Task 5.
5. Tracks skipped quickly: only the newest is sent, no queue builds up. Test in Task 5.

---

### Task 1: The NowPlaying store

**Files:**
- Create: `src/metadata/mod.rs`
- Modify: `src/lib.rs` (add `pub mod metadata;`)

**Interfaces:**
- Produces:
  - `pub struct Cover { pub mime: &'static str, pub bytes: Vec<u8> }` (`mime` is `"image/jpeg"` or `"image/png"`)
  - `#[derive(Clone, Default, PartialEq)] pub struct Track { pub title: Option<String>, pub artist: Option<String>, pub album: Option<String>, pub duration_s: Option<f64>, pub playing: bool }`
  - `pub struct Snapshot { pub track: Track, pub cover: Option<Arc<Cover>>, pub position_s: Option<f64>, pub text_gen: u64, pub cover_gen: u64, pub progress_gen: u64 }`
  - `pub struct NowPlaying` with `new() -> Arc<NowPlaying>`, `set_track(&self, Track)`, `set_cover(&self, Option<Cover>)`, `set_position(&self, f64, Instant)`, `clear(&self)`, `snapshot(&self, now: Instant) -> Snapshot`
  - `pub fn cover_mime(bytes: &[u8]) -> Option<&'static str>`

- [ ] **Step 1: Write the failing tests** in `src/metadata/mod.rs`:

```rust
#[test] fn a_new_track_moves_the_text_and_progress_generations() // set_track(A); snapshot().text_gen == 1, progress_gen == 1; set_track(A) again: both unchanged
#[test] fn a_cover_moves_only_its_generation()                  // set_cover(Some(jpeg)); cover_gen 1, text_gen unchanged
#[test] fn the_position_runs_on_while_playing()                  // playing, set_position(10.0, t0); snapshot(t0 + 2 s).position_s == Some(12.0)
#[test] fn the_position_stands_while_paused()                    // playing false: snapshot(t0 + 2 s).position_s == Some(10.0)
#[test] fn a_jump_in_position_moves_the_progress_generation()   // set_position(10, t0) then set_position(11, t0+1s): progress_gen unchanged; set_position(60, t0+2s): +1
#[test] fn clearing_empties_everything_and_moves_every_generation() // clear(): Track::default(), cover None, position None, all three gens +1
#[test] fn covers_are_known_by_their_first_bytes()              // cover_mime(&[0xFF,0xD8,0xFF,..]) == Some("image/jpeg"); &[0x89,b'P',b'N',b'G',..] == Some("image/png"); b"GIF89a" == None
```

A position counts as a jump when it differs from the running position by more than 2 s.

- [ ] **Step 2:** `cargo test --lib metadata` — fails to compile (no module).
- [ ] **Step 3:** Implement with one `Mutex<State>` held only to read or replace fields (no I/O under it).
- [ ] **Step 4:** `cargo test --lib metadata` — 7 pass.
- [ ] **Step 5:** `git commit -m "feat(metadata): the current track, shared by a source and the outputs"`

### Task 2: `[metadata]` in the config

**Files:**
- Modify: `src/config.rs` (raw table, `Config::metadata`, defaults at :431, load at :653)
- Create: `src/metadata/config.rs`

**Interfaces:**
- Produces: `#[derive(Debug, Clone, PartialEq)] pub enum Source { None, Acr }`; `#[derive(Debug, Clone, PartialEq)] pub struct MetadataConfig { pub source: Source, pub url: String }`; `pub fn from_table(Option<toml::Table>) -> Result<MetadataConfig, String>`; `Config::metadata: MetadataConfig`.

- [ ] **Step 1: Failing tests** in `src/metadata/config.rs` and one in `src/config.rs`:

```rust
#[test] fn no_table_is_no_source()            // from_table(None) == MetadataConfig { source: Source::None, url: "http://127.0.0.1:1080/api" }
#[test] fn acr_is_read_with_its_url()         // source = "acr", url = "http://192.0.2.1:1080/api"
#[test] fn an_unknown_source_is_an_error()    // source = "mpd" -> Err("metadata: source must be \"acr\" or \"none\"")
#[test] fn an_unknown_key_is_an_error()       // colour = 1 -> Err containing "metadata: unknown key colour"
#[test] fn a_url_that_is_not_http_is_an_error() // url = "ftp://x" -> Err("metadata: url must start with http://")
// src/config.rs:
#[test] fn the_metadata_table_reaches_the_config() // load_str("[metadata]\nsource = \"acr\"\n").unwrap().metadata.source == Source::Acr
```

- [ ] **Step 2:** run, fails. **Step 3:** implement, following `src/airplay/config.rs`'s key checks. **Step 4:** pass. **Step 5:** `git commit -m "feat(config): [metadata] names the source of song metadata"`

### Task 3: Reading ACR

**Files:**
- Create: `src/metadata/acr.rs` (parsing only in this task)

**Interfaces:**
- Consumes: `Track` (Task 1).
- Produces:
  - `pub struct NowPlayingAnswer { pub player: Option<String>, pub track: Track, pub cover_url: Option<String>, pub position_s: Option<f64> }`
  - `pub fn now_playing(json: &str) -> Option<NowPlayingAnswer>` — `player.name`, `song.{title,artist,album,duration,cover_art_url}`, `state == "playing"`, `position`; `song: null` gives an empty `Track`.
  - `pub enum Event { Refresh, Position(f64), Ignore }`
  - `pub fn event(json: &str, active: Option<&str>) -> Event` — `active_player_changed` is `Refresh` whatever the player; `song_changed`, `song_information_update`, `state_changed` from `active` are `Refresh`; `position_changed` from `active` is `Position`; everything else, and anything from another player, `Ignore`.
  - `pub fn cover_url(base: &str, url: &str) -> String` — an absolute `http(s)://` URL as it is; a path joined to the scheme and host of `base` (`http://127.0.0.1:1080/api` + `/api/coverart/x` gives `http://127.0.0.1:1080/api/coverart/x`).
  - `pub fn events_url(base: &str) -> String` — `http://h:p/api` gives `ws://h:p/api/events`.

- [ ] **Step 1: Failing tests**, with fixtures copied from the turntable board's `/api/now-playing` (in the design's recon) and ACR's `doc/websocket.md`:

```rust
#[test] fn the_now_playing_answer_gives_the_active_players_track()  // analog, De La Soul fixture: player Some("analog"), title, album, playing true, duration None, cover_url the Last.fm URL
#[test] fn no_song_gives_an_empty_track()                           // "song": null -> Track::default()
#[test] fn a_song_change_of_the_active_player_refreshes()           // event(song_changed from "analog", Some("analog")) == Refresh
#[test] fn events_of_another_player_are_ignored()                   // song_changed from "spotify", active "analog" -> Ignore
#[test] fn a_change_of_active_player_always_refreshes()             // active_player_changed, active Some("analog") -> Refresh
#[test] fn the_position_of_the_active_player_is_taken()             // position_changed 45.5 -> Position(45.5)
#[test] fn garbage_is_ignored()                                     // "not json" -> Ignore; now_playing("[]") == None
#[test] fn cover_paths_are_joined_to_the_host()                     // as above; absolute https URL unchanged
#[test] fn the_events_url_is_the_websocket_beside_the_api()
```

- [ ] **Steps 2–4:** run, implement with `serde_json::Value`, pass.
- [ ] **Step 5:** `git commit -m "feat(metadata): read ACR's now-playing answer and events"`

### Task 4: The ACR source thread

**Files:**
- Modify: `src/metadata/acr.rs`, `src/metadata/mod.rs`, `Cargo.toml` (`ureq = { version = "2", default-features = false, features = ["tls"] }`)
- Create: `src/metadata/fake_acr.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 1–3.
- Produces: `pub fn start(config: &MetadataConfig, store: Arc<NowPlaying>) -> Option<std::thread::JoinHandle<()>>` in `src/metadata/mod.rs` (`None` for `Source::None`); `acr::run(base: String, store: Arc<NowPlaying>, stop: Arc<AtomicBool>)`.

Loop: connect the WebSocket (`tungstenite::client` over a `TcpStream` with a 5 s read timeout), send `{"players":null,"event_types":null}`, fetch now-playing and apply it, then per message act on `event()`. Applying: `set_track`, `set_position`; a `cover_url` that differs from the last one is fetched (`ureq`, 10 s timeout, read at most 4 MiB + 1 byte, more is dropped, `cover_mime` decides) and set, `None` when absent or failed. On any connection error: `clear()`, log once per outage, sleep 5 s, retry. `stop` is checked at least every 5 s.

The fake serves one port: `GET /api/now-playing` (a JSON string it is given, changeable), `GET /cover.jpg` (bytes), `GET /slow.jpg` (never answers), `GET /api/events` (WebSocket upgrade; the test pushes messages through a channel; the test can close it).

- [ ] **Step 1: Failing tests:**

```rust
#[test] fn the_track_and_its_cover_arrive()            // now-playing with cover_art_url "/cover.jpg": within 3 s store has title and cover mime "image/jpeg"
#[test] fn a_song_change_is_fetched_again()           // change the JSON, push song_changed from the active player: new title within 3 s
#[test] fn a_cover_that_hangs_does_not_hold_the_text() // cover_art_url "/slow.jpg": title within 3 s, cover None
#[test] fn a_cover_too_large_is_dropped()             // 5 MiB body: title set, cover None
#[test] fn acr_gone_empties_the_track_and_it_comes_back() // close the socket: track empty within 3 s; fake accepts again: title back within 10 s
#[test] fn acr_not_there_at_start_is_tried_again()   // start against a closed port, open it 1 s later: title within 10 s
```

- [ ] **Steps 2–4:** run, implement, pass. Check `cargo clippy --all-targets` is clean.
- [ ] **Step 5:** `git commit -m "feat(metadata): follow ACR's active player and fetch its cover"`

### Task 5: AirPlay sends the metadata

**Files:**
- Create: `src/airplay/meta.rs`
- Modify: `src/airplay/stream.rs` (rtptime of the next packet), `src/airplay/raop.rs` and `src/airplay/ap2/session.rs` (a no-wait `SET_PARAMETER` with a content type and `RTP-Info`), `src/airplay/output.rs` (Session keeps what was sent; `pump` sends what changed; `spawn` takes the store), `src/airplay/mod.rs` (`Receiver::new` takes the store), `src/airplay/fake.rs` (the fake records bodies; it already does)

**Interfaces:**
- Consumes: `NowPlaying`, `Snapshot` (Task 1).
- Produces:
  - `pub fn dmap(track: &Track) -> Vec<u8>` — `mlit` containing `minm`, `asar`, `asal` (each tag, u32 big-endian length, UTF-8 bytes; a missing field is an empty string), so an empty track clears the text.
  - `pub fn progress(rtp_now: u32, position_s: f64, duration_s: f64) -> String` — `"progress: {start}/{now}/{end}\r\n"`, start = now − position·44100, end = start + duration·44100, wrapping u32.
  - `pub struct Sent { text_gen: u64, cover_gen: u64, progress_gen: u64 }` and `pub fn due(sent: &Sent, snap: &Snapshot) -> Vec<Meta>` with `pub enum Meta { Text(Vec<u8>), Cover(&'static str, Vec<u8>), NoCover, Progress(String) }` — only what changed; progress only with both a position and a duration.
  - `Stream::rtptime(&self) -> u32`; `Raop::metadata(&mut self, mime: &str, body: &[u8], rtptime: u32) -> Result<(), RtspError>` and the same on `Ap2`; `Control::metadata(..)`.
  - `output::spawn(config: AirplayConfig, now_playing: Arc<NowPlaying>) -> AirplayHandle`; `Receiver::new(config, now_playing)`.

A new session starts with `Sent` all zero, so the current track goes out right after RECORD. Before each `due()`, the output reads the snapshot once per pump; a skip storm between two pumps sends only the newest.

- [ ] **Step 1: Failing tests:**

```rust
// meta.rs
#[test] fn the_text_is_a_dmap_listing_item()   // dmap(title "T", artist "A", album "B") == b"mlit\0\0\0\x1bminm\0\0\0\x01Tasar\0\0\0\x01Aasal\0\0\0\x01B"
#[test] fn an_empty_track_clears_the_text()    // dmap(&Track::default()) == b"mlit\0\0\0\x18minm\0\0\0\0asar\0\0\0\0asal\0\0\0\0"
#[test] fn progress_counts_back_from_now()     // progress(441000, 2.0, 10.0) == "progress: 352800/441000/793800\r\n"
#[test] fn progress_wraps_around_zero()        // progress(1000, 1.0, 2.0) == "progress: 4294924196/1000/45100\r\n"
#[test] fn only_what_changed_is_due()          // after a first due(), moving only cover_gen gives [Cover(..)]
#[test] fn a_cleared_cover_is_sent_as_none()   // cover_gen moved, cover None -> [NoCover]
#[test] fn no_duration_no_progress()           // position Some, duration None -> no Progress
// output.rs (fake receiver, as the existing session tests)
#[test] fn a_session_sends_the_current_track_after_record()  // store has a track and a jpeg: receiver sees SET_PARAMETER application/x-dmap-tagged and image/jpeg after RECORD, each with an RTP-Info header
#[test] fn a_new_track_during_the_session_is_sent()          // change the title while playing: a second dmap body with the new title
#[test] fn a_receiver_that_refuses_metadata_keeps_playing()   // fake answers 400 to SET_PARAMETER with a dmap body: state stays Playing for 2 s, audio packets keep arriving
#[test] fn a_skip_storm_sends_only_the_newest()              // 50 set_track calls with no pump between (hold the store before the session starts): exactly one dmap body, with the last title
// raop.rs / ap2/session.rs
#[test] fn metadata_does_not_wait_for_an_answer()            // the fake never answers it; metadata() returns within 100 ms
```

- [ ] **Steps 2–4:** run, implement, pass; the existing AirPlay tests still pass with `spawn` taking a store (`NowPlaying::new()` in their helpers).
- [ ] **Step 5:** `git commit -m "feat(airplay): send the current track, its cover and its progress"`

### Task 6: Wiring, API and docs

**Files:**
- Modify: `src/control.rs` (create the store, start the source from `config.metadata`, give the store to every `Receiver::new` at :1015, keep the join handle), `src/api.rs` (`Call::Metadata`), `src/control/api.rs` (answer it), `src/http.rs` (`.route("/metadata", get(|s| ask(s, Call::Metadata)))`), `README.md` (`[metadata]` table, `GET /metadata`, the AirPlay paragraph)

**Interfaces:**
- Consumes: `metadata::start`, `NowPlaying::snapshot`.
- Produces: `GET /metadata` → `{"title","artist","album","duration_s","position_s","playing","cover": {"mime","bytes"} | null}`.

- [ ] **Step 1: Failing test** in `src/control/api.rs`, beside the `Call::Airplay` tests at :1582:

```rust
#[test] fn metadata_gives_the_current_track_without_the_cover_bytes()
// store.set_track(title "T"); store.set_cover(Some(jpeg of 3 bytes));
// control.call(Call::Metadata, now) == json!({"title":"T","artist":null,"album":null,"duration_s":null,"position_s":null,"playing":false,"cover":{"mime":"image/jpeg","bytes":3}})
```

- [ ] **Steps 2–4:** run, implement, pass. `cargo test` and `cargo clippy --all-targets` clean.
- [ ] **Step 5:** `git commit -m "feat: [metadata] starts the source; GET /metadata shows the track"`

### Task 7: aas-config turns ACR on

**Files:**
- Modify: `dist/hifiberryos/aas-config` (`config_from(systeminfo, audiocontrol=False)`; `main` passes `os.path.exists("/usr/bin/audiocontrol")`), `dist/hifiberryos/tests/test_aas_config.py`

- [ ] **Step 1: Failing tests:**

```python
def test_audiocontrol_turns_metadata_on(self):  # config_from(DACPLUS_ADC_PRO, audiocontrol=True)["metadata"] == {"source": "acr"}
def test_no_audiocontrol_no_metadata(self):      # "metadata" not in config_from(DACPLUS_ADC_PRO)
def test_local_turns_metadata_off(self):         # merge({"metadata": {"source": "acr"}}, {"metadata": {"source": "none"}})["metadata"]["source"] == "none"
```

- [ ] **Step 2:** `python3 -m unittest dist/hifiberryos/tests/test_aas_config.py` fails. **Steps 3–4:** implement, pass.
- [ ] **Step 5:** `git commit -m "feat(hifiberryos): aas-config turns on ACR metadata where AudioControl is installed"`

### Task 8: On a receiver

- [ ] **Step 1:** Build on the build host with `--features alsa-output`, install the binary on the turntable board in place of `/usr/bin/aas`, add `[metadata]\nsource = "acr"` to `/etc/aas/aas.local.toml`, restart aas.
- [ ] **Step 2:** With a record playing: `curl -s 127.0.0.1:2718/metadata` shows the recognised track and a cover; the log shows no metadata refusal; the bar shows the track or not (look, or ask the owner).
- [ ] **Step 3:** If the bar shows nothing, repeat against the Apple TV with a second `[[airplay]]` table, AirPlay 2, volume_db -25.
- [ ] **Step 4:** Record what each receiver showed in `doc/measurements.md` under `### Song metadata`; `git commit -m "docs: song metadata on the sound bar and the Apple TV"`.
