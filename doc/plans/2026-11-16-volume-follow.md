# Following the card's volume control Implementation Plan

**Goal:** `aas` takes every change another program makes to the card's volume control as its own output volume, and reads the control instead of writing over it when the control is opened, so players and `audiocontrol` can go on writing the card's control under `aas`.

**Architecture:** `HardwareVolume` learns to read its control and to report a level someone else set; a small `Watch` in `volume.rs` tells such a level from `aas`'s own writes. `OutputVolume` gets a followed level (`outside`) that it takes without writing the card, and an `adopt` that reads the control at open. The control thread checks the control every `volume_poll_ms` (100 ms by default) and publishes a followed change exactly as one made through `aas`. A new `volume_mode = "follow"` names a user-space control, such as `config-soundcard`'s `Softvol`, that `aas` follows and writes but applies in software.

**Tech Stack:** Rust edition 2024, `rust-version` 1.85, the `alsa` crate already in use. No new dependencies.

**Spec:** `doc/switch.md`, "## Volume (6)" and "## Work in this repository".

## Global Constraints

- No new dependencies. The mixer is read and written on the control thread only, never on the audio thread.
- `Master` spans -60 to 0 dB: percent `p` in 1..=100 is `(p - 100) * 0.6` dB, 0 mutes. A followed level maps back with `round(100 + dB / 0.6)`, clamped to 1..=100; a muted level (switch off, or the control at a step that mutes) is 0.
- An outside level below -60 dB that is not a mute reads as `Master` 1, not 0. The card keeps the value the outside writer set: a followed change never writes the card.
- A followed level carries no software part: on a control that carries the volume the engine gain is 1 (as in forced bit-perfect), whatever the percent says.
- A change `aas` makes itself is never taken back in as an outside one.
- At start (`restore`) and when the card's control is opened again (`card_changed`), the control's level is the truth. The stored or current percent is kept only when the control is within 1 dB of what that percent would set, or both are muted; otherwise the control's level is followed.
- A followed change is published to every `Master` subscriber, `publish_output(0)` and the state file exactly as `set_volume` publishes one, and further outputs follow it with their offsets.
- New config key `volume_poll_ms`: how often the card's control is checked for a change made outside `aas`, 50 to 5000, default 100; outside that range the config is an error, validated at startup like every other key. The control thread ticks every 50 ms (`TICK_MS`), so the check runs on the first tick at or after it is due, which is why 50 is the lowest value.
- `volume_mode = "follow"` requires `volume_control`; without it the config is an error. Under it the control is written like a hardware control, but the engine applies the volume (software mode), including the followed level.
- Unsupported or failing mixer reads are logged at most as `set_db` logs today and never panic; a control that has gone reads as no change.
- Public text (commits, docs, comments) names no tooling and no test machines; commits use the repository's lowercase `area: summary` style.
- Test with `cargo test -p aas -p aas-client` (libasound is not on the build host for the whole workspace); `cargo clippy -p aas -p aas-client --all-targets` stays free of new warnings. Do not run `cargo fmt` on the whole tree; format only the files you change (`rustfmt --edition 2024 <file>`).

## Review Focus

1. A player sets the card's control between two of `aas`'s own writes, at a level `aas`'s rounding would also produce: the change is still seen, since `Watch` compares against what was read back, not against the percent (Task 1, `watch_sees_a_level_equal_to_a_percent`).
2. `aas` mutes (percent 0) and a player then moves the control without touching the switch: the switch stays off, so the level still reads muted and `Master` stays 0 (Task 1, `control_db` with the switch off).
3. The volume is in the floor below the control's lowest step (percent 10 on a -51.5 dB control) and the daemon restarts: `adopt` keeps 10 and its software part, since the control reads exactly its floor (Task 1, `adopt_keeps_a_percent_in_the_floor`).
4. An outside change while the PulseAudio sink is muted: the `VolumeChanged` notice with a non-zero percent clears the sink's mute, as `src/pulse/conn.rs` already does; a server test pins that the notice is sent (Task 2).
5. A card that comes back at its driver's default level: `aas` takes that level and announces it, rather than restoring its own (Task 2, the server test).

---

### Task 1: Reading and following in `volume.rs`

**Files:**
- Modify: `src/volume.rs`
- Modify (compile only): `src/control.rs` (`FakeMixer` in the tests), `tests/server.rs` (`FakeVolume`), `src/sink_alsa.rs` (`AlsaVolume`, stub bodies until Task 3)

**Interfaces:**
- Produces, all in `src/volume.rs`:
  - `pub const MUTE_MB: i64 = -9_999_999;` (moved from `sink_alsa.rs`, which uses it from here)
  - `pub fn db_to_percent(db: f32) -> u8`
  - `pub fn control_db(millibel: i64, switch_on: bool) -> f32` — `f32::NEG_INFINITY` when `!switch_on` or `millibel <= MUTE_MB`, else `millibel as f32 / 100.0`
  - `#[derive(Debug, Default)] pub struct Watch { last: Option<f32> }` with `pub fn known(&mut self, db: f32)` (a level `aas` wrote and read back, or read at open) and `pub fn seen(&mut self, db: f32) -> Option<f32>` (the control reads `db` now: `Some(db)` and remembered if it differs from the last known or seen level, also when nothing was known yet; `None` otherwise)
  - on `trait HardwareVolume`: `fn read_db(&mut self) -> f32;` (the level now, `NEG_INFINITY` when muted; it counts as known), `fn changed(&mut self) -> Option<f32>;` (a level someone else set since the last write, read or change), `fn carries_volume(&self) -> bool { true }` (false for a control `aas` follows but applies itself)
  - on `OutputVolume`: `pub fn follow(&mut self, db: f32)`, `pub fn adopt(&mut self, percent: u8)`, `pub fn level_db(&self) -> Option<f32>`
  - `pub const SAME_DB: f32 = 1.0;`

- [ ] **Step 1: Write the failing tests** in `src/volume.rs`'s `mod tests`. Extend `FakeHardware` with `level: Arc<Mutex<f32>>` (what the control reads, set by `set_db` and by the test as "another program"), a `watch: Watch` and `carries: bool`: `set_db` stores the level and calls `watch.known`, `read_db` reads it and calls `known`, `changed` returns `watch.seen(level)`. Tests, with `hw` a fake of range (-103.5, 0.0) unless stated:

```rust
#[test]
fn db_maps_back_to_percent() {
    assert_eq!(db_to_percent(0.0), 100);
    assert_eq!(db_to_percent(-30.0), 50);
    assert_eq!(db_to_percent(-20.0), 67);
    assert_eq!(db_to_percent(3.0), 100);
    assert_eq!(db_to_percent(-59.4), 1);
    assert_eq!(db_to_percent(-80.0), 1); // quiet, not muted
    assert_eq!(db_to_percent(f32::NEG_INFINITY), 0);
}

#[test]
fn control_db_reads_switch_and_mute_step() {
    assert_eq!(control_db(-3000, true), -30.0);
    assert_eq!(control_db(-3000, false), f32::NEG_INFINITY);
    assert_eq!(control_db(MUTE_MB, true), f32::NEG_INFINITY);
}

#[test]
fn watch_reports_only_levels_aas_did_not_set() {
    let mut watch = Watch::default();
    assert_eq!(watch.seen(-30.0), Some(-30.0)); // nothing known yet
    assert_eq!(watch.seen(-30.0), None);
    watch.known(-20.0);
    assert_eq!(watch.seen(-20.0), None); // aas's own write
    assert_eq!(watch.seen(f32::NEG_INFINITY), Some(f32::NEG_INFINITY));
    assert_eq!(watch.seen(f32::NEG_INFINITY), None);
}

#[test]
fn watch_sees_a_level_equal_to_a_percent() {
    let mut watch = Watch::default();
    watch.known(-30.0);
    assert_eq!(watch.seen(-29.4), Some(-29.4)); // what percent 51 would set
}
```

  And `OutputVolume` tests:
  - `following_takes_the_level_without_writing_the_card`: `set(50)` (one write, -30), then `follow(-20.0)`: `percent() == 67`, no new write, `gain() == 1.0`, `level_db() == Some(-20.0)`.
  - `following_below_minus_60_reads_one`: `follow(-80.0)`: `percent() == 1`, `gain() == 1.0`, `further_gain(-6.0) == db_to_gain(-86.0)`.
  - `following_a_mute_reads_zero`: `follow(f32::NEG_INFINITY)`: `percent() == 0`, `gain() == 1.0` (a control that can mute), `further_gain(0.0) == 0.0`.
  - `following_ends_the_floor`: range (-51.5, 0.0); `set(10)` gives gain `db_to_gain(-2.5)`; `follow(-51.5)`: `percent() == 14`, `gain() == 1.0`. A later `set(10)` brings the software part back.
  - `adopt_takes_the_cards_level`: the fake reads -20.0; `adopt(50)`: `percent() == 67`, no write.
  - `adopt_keeps_a_percent_the_card_matches`: the fake reads -30.5; `adopt(50)`: `percent() == 50`, no write; reading -31.5 instead gives 48 (`round(100 - 31.5/0.6)` = 48).
  - `adopt_keeps_a_percent_in_the_floor`: range (-51.5, 0.0), the fake reads -51.5; `adopt(10)`: `percent() == 10`, `gain()` ≈ `db_to_gain(-2.5)`.
  - `adopt_of_a_muted_card`: the fake reads `NEG_INFINITY`; `adopt(0)` keeps 0; `adopt(50)` gives 0.
  - `adopt_without_a_card_sets_software`: `OutputVolume::new(None, Auto)`, `adopt(50)`: `gain() == db_to_gain(-30.0)`.
  - `a_control_aas_applies_itself`: `carries = false`, range (-51.0, 0.0): `set(50)` writes -30.0 to the control and `gain() == db_to_gain(-30.0)`, `is_hardware() == false`; `follow(-20.0)` gives `gain() == db_to_gain(-20.0)`; `follow(f32::NEG_INFINITY)` gives `gain() == 0.0`; with `BitPerfect::Forced` the gain is 1.0.
  - The `hardware(range)` helpers start the fake's level at 0.0. In `a_card_without_a_control_moves_the_volume_to_software_and_back`, the control that comes back reads -30.0, so it keeps 50 and is not written; the test asserts that instead of the write.
  - Update `a_new_control_gets_the_current_volume`: a new control reading -30.0 after `set(50)` keeps 50 and is not written; rename it `a_new_control_matching_the_volume_keeps_it`. Add `a_new_control_at_another_level_is_followed`: it reads 0.0, `replace_hardware` gives `percent() == 100`, no write.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p aas --lib volume`
Expected: compile errors for `db_to_percent`, `Watch`, `follow`, `adopt`.

- [ ] **Step 3: Implement** in `src/volume.rs`.

  `OutputVolume` gets `outside: Option<f32>`, the level last followed, `None` after any `set`. `follow(db)` sets `outside = Some(db)` and `percent = db_to_percent(db)`, and writes nothing. `level_db()` is `outside` (`NEG_INFINITY` → `None`, clamped to at most 0.0) when set, else `percent_to_db(percent)`. `is_hardware()` is `hardware.is_some_and(|h| h.carries_volume())`.

  `gain()`, in this order: on a control that carries the volume, percent 0 on a control that cannot mute is 0; forced bit-perfect is 1; a followed level is 1; below the control's range it is what the range lacks (as now); else 1. On no control or one that does not carry: forced is 1, else `level_db().map_or(0.0, db_to_gain)`. `further_gain(offset)` is `level_db().map_or(0.0, |db| db_to_gain(db + offset))`.

  `adopt(percent)`: without a control, `set(percent)`. With one, `read_db()`; `expected` is what `set(percent)` would write (`percent_to_db` clamped to the range, `NEG_INFINITY` for 0). Both muted, or both finite and within `SAME_DB` of each other: keep `percent` with `outside = None`, and write nothing. Otherwise `follow(read)`. `replace_hardware` calls `adopt(self.percent)` instead of `set`.

  Implement the three new trait methods in the test fakes in `src/control.rs` (`FakeMixer`) and `tests/server.rs` (`FakeVolume`): each keeps a `Watch` and a level, set by `set_db`, starting at the first value written or 0.0. In `src/sink_alsa.rs`, `AlsaVolume` gets `read_db` returning the range's top and `changed` returning `None`, with a comment that Task 3 replaces them; import `MUTE_MB` from `volume`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p aas -p aas-client`
Expected: the new tests pass. Existing tests in `src/control.rs` and `tests/server.rs` that relied on a new control being written with the current volume may now fail: leave them to Task 2 only if they are about `card_changed` or `restore`, and list them in the report; everything else must pass.

- [ ] **Step 5: Commit**

```bash
git add src/volume.rs src/control.rs src/sink_alsa.rs tests/server.rs
git commit -m "volume: follow a level set on the card's control"
```

### Task 2: The control thread follows the card

**Files:**
- Modify: `src/control.rs` (`restore`, `card_changed`, `set_volume`, `tick`, tests)
- Modify: `src/server.rs` (the `card_changed` call)
- Modify: `src/config.rs` (`volume_poll_ms`, tests), `README.md` (its row in the config table, after `volume_control`)
- Test: `src/control.rs` tests, `tests/server.rs`

**Interfaces:**
- Consumes: Task 1's `HardwareVolume::changed`, `OutputVolume::follow`, `adopt`, `level_db`.
- Produces: `Config::volume_poll_ms: u32` (default 100); `pub fn card_changed(&mut self, hardware: Option<Box<dyn HardwareVolume>>, now: Instant)`; private `fn follow_card(&mut self, now: Instant)` called first in `tick`; private `fn volume_moved(&mut self, before: (u8, f32, Option<f32>), now: Instant)`, `before` being `(percent(), gain(), level_db())` taken before the change.

- [ ] **Step 1: Write the failing tests** in `src/control.rs`, with a fake whose level the test can set as another program (as Task 1's `FakeHardware`; reuse `FakeMixer` extended with a shared `level`):
  - In `src/config.rs`: `volume_poll_ms` defaults to 100, `volume_poll_ms = 250` loads as 250, and 49 and 5001 are errors whose text contains `volume_poll_ms`.
  - `an_outside_change_waits_for_the_poll`: with `volume_poll_ms = 100`, a first `tick(now)` checks at once; the level then set to -20.0 is not seen at `tick(now + 50 ms)`, and is at `tick(now + 100 ms)` (`VolumeChanged { percent: 67 }`).
  - `an_outside_change_is_the_volume`: subscribed connection; set the fake's level to -20.0; `control.tick(now)`: one notice `VolumeChanged { percent: 67 }`, the engine gets `SetGain(0, 1.0)`, a further output (as in the existing further-gain test) gets `db_to_gain(-26.0)` for offset -6, and the state is saved after `SAVE_DELAY` with `output_percent == 67`.
  - `aas_own_write_is_not_followed_back`: `SetVolume { percent: 50 }`, then `tick`: no second notice, no command.
  - `an_outside_mute_reads_zero_and_unmute_restores`: level `NEG_INFINITY`, `tick`: `VolumeChanged { percent: 0 }`; level -30.0, `tick`: `VolumeChanged { percent: 50 }`.
  - `an_outside_level_below_minus_60_reads_one`: level -80.0 → `VolumeChanged { percent: 1 }`, the card's fake has not been written.
  - `the_card_is_the_truth_at_start`: `restore` with a state file holding `output_percent = 30` and a control reading -20.0: `volume_reply` is 67 and the control was never written. With a control reading -42.0 (what 30 sets), it stays 30.
  - `a_card_back_at_another_level_is_announced`: subscribed; `card_changed(Some(fake reading 0.0), now)` while the volume is 50: one notice `VolumeChanged { percent: 100 }`.
  - Update `the_same_volume_again_is_written_to_the_card_but_not_announced` and the further-gain test for `card_changed`'s new `now` parameter; the first write `0.0` there no longer happens (the fake's level is adopted), so its expected writes become `[-30.0]` and `[-30.0, -30.0]`.
  - In `tests/server.rs`, change `a_card_missing_at_the_start_gets_the_volume_when_it_appears` so that the fake control appears reading -20.0, with a `SubscribeVolume` connection: `VolumeChanged { percent: 67 }` arrives, and the fake has no write. Rename it `a_card_missing_at_the_start_brings_its_own_volume`. Add `an_outside_change_reaches_subscribers`: the fake's level set to -30.0 from the test thread gives `VolumeChanged { percent: 50 }` within `PATIENCE`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p aas -p aas-client volume`
Expected: the new tests fail (no notices; `card_changed` takes one argument).

- [ ] **Step 3: Implement** in `src/control.rs`.
  - `restore` calls `volume.adopt(stored.output_percent)` instead of `set`.
  - `card_changed(hardware, now)` takes `before`, calls `replace_hardware`, logs as now, then `volume_moved(before, now)`. `src/server.rs` passes `now`.
  - `set_volume` takes `before`, calls `set` (still writing the card when the percent is the same), then `volume_moved(before, now)`. Its doc's last sentence becomes: "A change made outside aas is followed on the next tick, see `follow_card`."
  - `volume_moved(before, now)`: if `(percent, gain, level_db)` is unchanged, nothing. Otherwise the engine batch `SetGain(0, gain)` plus each further output's gain, as `set_volume` sends now. If the percent changed: the `VolumeChanged` notices to subscribers, `publish_output(0)`, `save_later(now)`.
  - `follow_card(now)`: if `self.volume`'s control reports `changed()` (add `pub fn changed(&mut self) -> Option<f32>` on `OutputVolume`, `None` without a control), take `before`, `follow(db)`, `volume_moved(before, now)`. Called first in `tick`. It reads the control only when due: `follow_at: Option<Instant>`, `None` at first (due at once), set to `now + volume_poll_ms` after each check. The README row: "`volume_poll_ms` | 50 to 5000 | 100 | How often, in ms, the card's volume control is checked for a change made by another program."

- [ ] **Step 4: Run the tests**

Run: `cargo test -p aas -p aas-client`
Expected: all pass, including the tests Task 1 listed.

- [ ] **Step 5: Commit**

```bash
git add src/control.rs src/server.rs src/config.rs README.md tests/server.rs
git commit -m "control: follow the card's volume control"
```

### Task 3: The ALSA control, `volume_mode = "follow"`, docs

**Files:**
- Modify: `src/sink_alsa.rs` (`mod mixer`)
- Modify: `src/config.rs` (`VolumeMode`, validation, tests)
- Modify: `src/main.rs` (`hardware_volume`)
- Modify: `README.md` (the `volume_mode` row, the `Master` bullet near line 96, the API's `volume` bullet)
- Modify: `doc/design.md` (`### Volume` under the PulseAudio section: one sentence that the sink volume follows the card's control)

**Interfaces:**
- Consumes: Task 1's `Watch`, `control_db`, `MUTE_MB`, trait methods.
- Produces: `VolumeMode::Follow` (`"follow"`); `AlsaVolume::open(card: Option<&str>, configured: Option<&str>, carries: bool) -> Option<AlsaVolume>`.

- [ ] **Step 1: Write the failing config tests** in `src/config.rs`:
  - `volume_mode = "follow"` with `volume_control = "Softvol"` loads as `Some(VolumeMode::Follow)`.
  - `volume_mode = "follow"` without `volume_control` is an error whose text contains `volume_control`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p aas --lib config`
Expected: FAIL, unknown variant `follow`.

- [ ] **Step 3: Implement.**
  - `VolumeMode::Follow`, doc: "aas follows and writes the named control, and applies the volume itself"; the validation error: `volume_mode = "follow" needs volume_control`.
  - `main.rs`: `Follow` opens `AlsaVolume::open(card, control, false)`; `Hardware` and `None` pass `true`. Without ALSA, `Follow` warns as `Hardware` does.
  - `AlsaVolume` gets `carries: bool` and `watch: Watch`. A private `fn level(&mut self) -> Option<f32>`: `handle_events`, find the element (gone → log once as `set_db` does, `None`), read channel 0 (`SelemChannelId::FrontLeft`, or the element's first channel) with `get_playback_vol_db` and, if it has a switch, `get_playback_switch`, then `control_db`. `set_db` after its write reads `level()` and calls `watch.known`. `read_db` is `level()` with `known`; when the read fails it returns the range's top, logged. `changed` is `level().and_then(|db| self.watch.seen(db))`. `carries_volume` returns `carries`. The open log line adds `, followed and applied in software` when `!carries`.
  - Remove the stubs Task 1 left.
  - README: the `volume_mode` row lists `"follow"`: "`follow` follows and writes `volume_control`, such as a user-space `Softvol`, and applies the volume in software." The `Master` bullet and the API's `volume` bullet gain: "A change made to the card's control by another program, a player's mixer or `amixer -c`, becomes the volume; at start the card's control is read, not written."

- [ ] **Step 4: Build and test**

Run: `cargo test -p aas -p aas-client && cargo clippy -p aas -p aas-client --all-targets`
Expected: tests pass, no new warnings. `cargo build --features alsa-output` cannot link here; the controller builds on a board (Task 4).

- [ ] **Step 5: Commit**

```bash
git add src/sink_alsa.rs src/config.rs src/main.rs README.md doc/design.md
git commit -m "alsa: read the card's volume control, and follow a named one"
```

### Task 4: Board check (controller, on a board with a DAC+)

Not dispatched: run by the controller on a test board with the release build, in the scratch directory, like the earlier `peak.sh`.

- [ ] Start `aas` with `card = "sndrpihifiberry"`, subscribe with `aasctl` (or poll `GET /outputs/1`), and check each, restoring the mixer afterwards:
  - `amixer -c sndrpihifiberry sset Digital -20dB` → volume 67 within 100 ms; the card still reads -20 dB.
  - `amixer ... sset Digital mute` → 0; `unmute` → 67 again.
  - `amixer ... sset Digital -80dB` → 1; the card still reads -80 dB.
  - `aasctl volume 50` → 50, the card -30 dB, and no second change reported.
  - Restart `aas` with the card at -24 dB and a state file of 30 → 60 at start, the card untouched.
- [ ] Optional, if `config-soundcard`'s `Softvol` exists on the board: `volume_mode = "follow"`, `volume_control = "Softvol"`; the open log names it, and a change to it is followed and heard in software.
- [ ] Record the outcome in the ledger; fix in a new task if anything fails.
