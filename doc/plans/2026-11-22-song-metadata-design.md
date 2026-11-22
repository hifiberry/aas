# Song metadata for remote outputs: design

## Intent

aas sends audio to remote outputs: AirPlay receivers today, a Bluetooth A2DP
sender later. Those receivers can show what is playing, but aas knows nothing
about the music it mixes. This adds a way for aas to pull the current track
from a metadata source and hand it to every remote output that can show it.

The owner decided (2026-11-22):

- **What reaches the receiver:** title, artist and album, the cover image, and
  progress where the source knows the track's length.
- **Consumers:** the AirPlay output now; the Bluetooth sender, which does not
  exist yet, later. Bluetooth (AVRCP 1.6) carries cover art too, so the
  interface hands the image to every output, not only AirPlay.
- **aas pulls the metadata itself**, through a generic source interface inside
  the daemon. The first source is AudioControl (ACR).

## Goals

- One current track in the daemon, filled by one configured source, read by
  any output.
- ACR as the first source: it already knows the active player's track on a
  HiFiBerry OS device, the analog input included (via analog-recognition).
- AirPlay 1 and AirPlay 2 sessions send the title, artist and album, the
  cover, and progress when the length is known, and send them again when the
  track changes.

## Non-goals

- The Bluetooth sender itself, and turning covers into the 200x200 JPEG
  thumbnail AVRCP's cover art transfer expects. That is the Bluetooth
  sender's work when it is built; the interface already gives it the image.
- A second source (MPD, a DLNA renderer, PulseAudio stream properties). The
  interface allows one; none is built now.
- Metadata from aas to its own players or clients, beyond a read-only view in
  the API for checking.
- Pushing metadata into aas through its API: the owner chose the pull model.

## Anti-goals

- **Nothing here may disturb the audio.** Fetching, parsing and sending run off
  the audio thread, and an AirPlay session must not stall its packet pacing
  while it sends a cover. A slow or dead ACR, a cover server that hangs, or a
  receiver that never answers a metadata request must cost at most missing
  metadata, never a gap, a resend storm or a dropped session.
- No metadata from the wrong player: only the track of ACR's *active* player
  goes out, so a paused Spotify does not label the record that is playing.

## Constraints

- aas is one Rust binary; the source runs in its own thread, like the AirPlay
  output threads.
- ACR: WebSocket `ws://127.0.0.1:1080/api/events` (subscribe message with
  `players: null`), REST `http://127.0.0.1:1080/api/now-playing`. Events come
  for every player. `song_information_update` is partial and may arrive late
  or for a song that has already moved on; `cover_art_url` is either a path on
  ACR (join it to ACR's base URL) or an absolute external URL, for example
  HTTPS from Last.fm. `position_changed` gives seconds as a float; the length
  is the song's `duration`, often absent (the analog player has none).
- Covers can come from the internet over HTTPS, so aas gains an HTTPS client.
  It runs only in the source's thread.
- The AirPlay sessions already send `SET_PARAMETER` once after RECORD (volume,
  then `progress: 0/0/0`). Metadata uses the same channel: DMAP-tagged text
  (`application/x-dmap-tagged`: `mlit` with `minm`, `asar`, `asal`), the cover
  (`image/jpeg` or `image/png`), and `text/parameters` progress, each with an
  `RTP-Info: rtptime=` of the stream. For AirPlay 2 they go over the sealed
  control connection.
- Metadata must not fail a session: a refusal or a missing answer is logged
  once and ignored.

## How it is confirmed

The owner decided (2026-11-22): first on the sound bar, over AirPlay 2 from
the turntable board. If the bar does not show metadata (it may not have the feature), on an
Apple TV. The fake receiver in the unit tests shows what is sent; only a
receiver shows that it is accepted and displayed. "Sent and not refused" on
the bar alone is not done if the bar shows nothing; the Apple TV is then the
test.

## Approaches

- **A. aas pulls through an internal source interface** *(chosen)*. A source
  thread fills a shared current track; outputs read it. The ACR source follows
  the WebSocket and fetches the cover. Costs an HTTPS client in aas; needs no
  extra service.
- **B. A separate bridge pushes into a new aas API.** Keeps HTTPS out of aas
  and lets any program feed it, but adds a service to package and run, and is
  a push, not the pull asked for.
- Rejected without a sketch: covers only from ACR's own paths (plain HTTP to
  localhost, no TLS) would lose the Last.fm covers the analog player gets,
  which is the main case on a turntable device.

## Design

### The current track

One value in the daemon, shared between the source thread and the outputs,
read without blocking the reader (a lock held only to swap a pointer, or a
generation counter plus a swap):

- `title`, `artist`, `album`: each optional text.
- `duration`: optional, seconds.
- `position`: optional, as seconds at a monotonic instant, so a reader can
  work out the position now without a fresh update every second.
- `playing`: whether the source reports playback.
- `cover`: optional bytes with their media type (`image/jpeg` or
  `image/png`), as fetched.
- A generation number that changes whenever any of the above does, and a
  separate one for the cover, so an output resends only what changed (a cover
  is large; progress changes often).

No source configured, or a source that has nothing, gives an empty track.

**An emptied or changed track is sent too** *(call)*: when the track becomes
empty (ACR lost, no song, another player became active with nothing known
yet), a playing session sends blank text and clears the cover, so the
receiver does not keep showing the last player's track over another. A new
track without a cover, or whose cover could not be fetched, clears the old
cover. A pause changes nothing on the receiver; progress simply stops being
updated.

### The source interface

A source is started with the shared track and fills it until the daemon stops.
It owns its thread, its reconnects and its errors. The daemon starts at most
one, chosen in the config. The interface is what a second source would
implement; nothing else in the daemon knows which source runs.

### The ACR source

- Connects to the WebSocket, subscribes to all players, and on connect, on
  `active_player_changed`, `song_changed`, `song_information_update` and
  `state_changed` from the active player, fetches `/api/now-playing` and sets
  the track from it. The REST answer is the whole truth, so partial updates
  need no merge logic of their own. Events from other players are ignored.
- `position_changed` from the active player updates only the position,
  without a fetch.
- A new `cover_art_url` is fetched (ACR path joined to the base URL, or the
  absolute URL as it is), with a timeout and a size cap; on failure the track
  goes out without a cover. A URL that did not change is not fetched again.
- A lost connection clears the track and is retried after a pause; ACR not
  running is not an error worth more than one log line.

### AirPlay

The output thread already wakes for every packet. While a session plays, it
compares the track's generations with what it last sent and sends what
changed: text and progress on a track change, the cover when the cover's
generation moved, progress again when the position jumps (a seek). A new
session sends the current track once after RECORD. Requests are sent without
waiting for the answer, as `/feedback` is; answers are matched and dropped.
Sending a cover must not hold the packet pacing up (see the anti-goals).

### Config

A new `[metadata]` table:

```toml
[metadata]
source = "acr"                      # "acr", or "none" (default)
url = "http://127.0.0.1:1080/api"   # the ACR base URL; events at <url>/events
```

On HiFiBerry OS, `aas-config` writes `source = "acr"` when the AudioControl
package is installed (its binary is present) *(call)*; `aas.local.toml` can
set `source = "none"` to turn it off.

### API

`GET /metadata` gives the current track without the cover bytes (title,
artist, album, duration, position now, playing, the cover's type and size),
for checking on a device *(call)*.

## Left to the builder

- The exact lock-free or short-lock shape of the shared track.
- The HTTPS client crate (blocking is fine in the source thread) and its
  timeouts, and the cover size cap (a few MB).
- How the AirPlay thread sends a large cover without stalling packets: a
  nonblocking write queue on the control connection, or a short write timeout
  with the cover dropped on overrun.
- Whether to check an AirPlay 2 receiver's `/info` features for metadata
  support before sending, or send and ignore refusals.
- DMAP encoding details, and whether progress goes out on a timer as well as
  on changes.

### Timing *(call)*

Every metadata request carries the rtptime of the audio being sent at that
moment. The receiver plays that audio after the latency, so a receiver that
applies metadata by rtptime switches the label as the new track is heard;
one that applies it on arrival switches early by the latency, which is
accepted. Progress is stamped the same way: start is the send rtptime minus
the position, end is start plus the length.
