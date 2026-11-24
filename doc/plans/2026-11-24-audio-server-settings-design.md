# Audio server settings in the HiFiBerry OS UI — design

Status: draft for the owner's approval, 2026-11-24.

## Intent

A user of HiFiBerry OS gets one place, **Settings → Audio server**, to choose
and tune the audio server. While the device still runs PipeWire, the page
offers to switch to aas (the switch that exists today). Once aas runs, the
page changes its settings:

- the latency of the card's buffer (`latency_ms`);
- AES67 on or off, which announced stream plays, and the AES67 input's
  latency;
- a "Recommended for this board" button that puts these back to what aas
  picks for the Pi model;
- a warning when the latency is set lower than the board handles well.

Why: these are the settings a user of aas actually tunes, and today they need
an SSH session and a TOML file. A latency that is too low gives dropouts that
look like a bug, so the page says so before the user finds out by ear.

## Goals, non-goals, anti-goals

- Goal: change the three settings from the web UI; they survive a reboot.
- Goal: the user sees the recommended latency for the board and is warned
  below it.
- Non-goal: every aas setting. Output volumes, AirPlay receivers, plugins
  and the AES67 output stay where they are (API, config file, state file).
- Non-goal: a new switch. The PipeWire/aas switch is the existing page's, as
  it is.
- Anti-goal: a stream selection that cuts the sound for a restart. Choosing a
  stream, turning AES67 on or off or changing its latency must not restart
  the daemon.

## Approaches considered

- **A (chosen): each setting where it lives.** The latency goes through the
  config server into `/etc/aas/aas.local.toml`, with a restart of aas; AES67
  on/off and the stream go through aas's own API, which applies them at once
  and keeps them in its state file.
- B: everything through aas's API, with aas keeping the latency in its state
  file and restarting itself to raise it. Rejected: a daemon that restarts
  itself is new, fragile machinery.
- C: everything through the config server. Rejected: every change, a stream
  pick too, would restart aas.

## The parts

### aas (repository `aas`, on the AES67 branch)

1. **The board's latency figures in `GET /settings`:** two new read-only
   fields, `latency_recommended_ms` and `latency_min_ms`, from the board class
   aas already detects (`src/board.rs`):

   | Board | Recommended | Lowest safe | Evidence |
   |---|---|---|---|
   | Pi 5, CM5 | 5 | 3 | CM5 clean under full load down to 32-frame periods |
   | Any other | 12 | 6 | Pi 3: 128-frame periods clean under full load, with AES67; 64-frame periods had xruns |

   The Pi 4 falls under "any other" until it is measured (TODO.md already
   carries that). `latency_recommended_ms` is the default aas uses without a
   `latency_ms` in the config. On a Pi 5 or CM5 that default rises from 4 to
   5 ms (the owner's call); both give the same period at every rate (64
   frames at 44.1 and 48 kHz, 128 at 96 kHz, 256 at 192 kHz), so playback
   does not change. A `PATCH` naming either field is 422, as for `format`.
2. **`aas-config` writes an `[aes67]` table** on a device with wired Ethernet:
   the interface, `ptp = "listen"`, `[aes67.input] enabled = false` and
   `[aes67.output] enabled = false`, all explicit. Without the table aas has
   no AES67 to turn on through the API. The wired interface is `eth0` if it
   exists, else the first interface in `/sys/class/net` with a `device` link
   and no `wireless` directory; none, no table. A user's `aas.local.toml` is
   merged over it key by key, as today, so it can set any of these.

   What the API sets survives a reboot through aas's state file: the input's
   `enabled`, `stream` and `latency_ms` are kept there and apply while the
   config says what it said when they were stored, which holds because
   `aas-config` writes the same table at every boot. The switch on the page is
   `input.enabled` only; the PTP listener and the stream list run whenever the
   table exists, so streams show before the input is on. A restart of aas for
   a new latency keeps the stream and the switch for the same reason.
3. **`"auto"` never sits below the floor.** The input holds at least its
   floor already (`latency_floor_ms`). With `latency_ms = "auto"` it does so
   without the log warning, and the page shows the higher of `latency_ms` and
   `latency_floor_ms` as the automatic value: on a CM5
   the automatic 5 ms is held at the 6 ms floor without a complaint, since the
   user chose nothing. A number the user set below the floor still warns.

### Config server (repository `configurator`)

A route beside the existing `/audio-server`, `/audio-server/aas-settings`:

- `GET` gives `latency_ms` from `aas.local.toml`, or `null` when it is not set
  (aas uses the recommended value).
- `POST {"latency_ms": n | null}` writes the key into `aas.local.toml`, or
  removes it for `null`, keeping every other key and comment it does not
  touch, then restarts the aas service if it runs. It answers once the file is
  written; the restart is not waited for. Range 1 to 100; outside is 400.
- `hifiberry-auth`: the `POST` is `risky`, as `PATCH /settings` is for aas;
  where the config server declares its routes' classes is for the builder to
  find in `configurator`.

### Web UI (repository `hbos-ui`)

- **A tile in Settings** (`src/views/services/index.vue`) for the existing
  `audio-server` route. The link from System Tools stays.
- **Which latency the page shows:** the config server's value, the one that
  persists. `null` shows as "Recommended (N ms)" with N from aas. The running
  value from aas's `GET /settings` shows in the status line; if it differs
  (a value set at run time with `aasctl`), the line says so.
- **Against an older aas** that lacks `latency_recommended_ms` and
  `latency_min_ms`: no warnings and no recommended value are shown; the
  latency can still be set.
- **The page** (`audio-server.vue`):
  - while PipeWire runs, it is the page of today, with the switch;
  - while aas runs, below the switch:
    - **Latency**: the value and the board's recommended one, a number input
      or slider in whole ms. An amber note below the recommended value
      ("may drop out under heavy load"), a red one below the lowest safe
      value ("dropouts are likely on this board"); saving below the lowest
      safe value asks for confirmation. "Below" is strict: the recommended
      and the lowest safe value themselves are not warned. Saving says the
      sound stops for a moment while aas restarts; the page then shows
      "restarting" and polls `GET /settings` once a second. When aas answers,
      it shows the new value; after 30 s without an answer it shows an error
      that names the value saved, so the user can set it back.
    - **AES67**: on/off (`PATCH /aes67/input` `enabled`), and a select of
      the streams from `GET /aes67/streams` that can be selected (`stream`).
      Hidden with a note when aas has no AES67 (`GET /aes67` 404).
    - **AES67 latency**: "Automatic (N ms)" or a number of ms, 1 to 100,
      through `PATCH /aes67/input` `latency_ms`; it applies at once, the
      stream starts over at it. While a stream plays, `latency_floor_ms` is
      the least it holds: a number below it shows an amber note, "this board
      holds at least F ms; the stream plays at F ms", and the value stays as
      set. Before a stream plays the floor is not known and there is no note.
      The note explains that the AES67 latency includes the card's buffer, so
      a lower card latency lowers the floor.
    - **Recommended for this board**: after a confirmation that names the
      values, first sets the AES67 input latency to `"auto"` through aas's
      API (kept in the state file), then the latency to `null` through the
      config server, which restarts aas. When the latency is `null` already,
      there is no restart.
  
## Left to the builder

- Layout and wording of the page beyond the notes above, in the style of the
  existing page.
- How the config server edits TOML while keeping comments: a TOML library
  that round-trips, or a line-based edit of the one key.
- Tests in each repository in its own style.

## Rollout

The aas part sits on the AES67 branch, which is not merged yet. The config
server and the UI parts can merge on their own: against an aas without the
new fields or without AES67 the page shows what it can (see above). A
`GET /aes67` 404 shows "AES67 is not available on this device: it needs aas
with AES67 and a wired network connection".

## Calls made in this design (the owner can correct them)

- The AES67 latency warning uses the floor aas measures while a stream
  plays; there is no warning before a stream plays.
- `"auto"` holds the floor quietly where the board's automatic value is
  below it (CM5: 5 ms held at 6 ms), instead of raising the automatic value.
- The lowest safe values 3 ms (Pi 5/CM5) and 6 ms (others) come from the
  measurements; the Pi 4 gets the "others" values unmeasured.
- The tile goes into Settings; the System Tools link stays.
- After a latency change the page waits 30 s for aas before showing an error.
- Against an older aas the page shows no warnings rather than a built-in table.
