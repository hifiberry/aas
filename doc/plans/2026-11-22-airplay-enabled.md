# AirPlay receivers on and off: design and plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

## Design (approved by the owner, 2026-11-22)

Several `[[airplay]]` receivers stay configured; only some are active, and
the API switches them. On the turntable board both the sound bar and the
Apple TV are configured, only the sound bar active.

- `[[airplay]]` takes `enabled = true | false`, default `true`.
- `PATCH /airplay/{name}` with `{"enabled": bool}`; `GET /airplay` gives
  `"enabled"` per receiver. Turning one off ends a session with TEARDOWN and
  its thread; turning one on starts it, connecting as its `connect` says.
- *(call)* A change through the API is kept in the state file, by receiver
  name, and applies while the config's `enabled` stays what it was (the
  rule of the volume limit).
- *(call)* A receiver that is off can still be paired.
- `aasctl airplay NAME enable|disable`.
- *(call)* `PATCH /airplay/*` is the `ok` tier in `dist/auth.d/aas.json`, as
  `PATCH /outputs/*` is; pairing stays `risky`.

Approaches: the switch in the control thread, which already owns the
receivers (chosen); a flag the receiver's thread obeys while it keeps
running was rejected, since an idle thread and tap per unused receiver cost
for nothing.

**Global constraints:** the control thread never waits on a receiver's
TEARDOWN (it is joined in a thread of its own); `rustfmt --edition 2024` per
file, `src/control.rs` and `src/control/api.rs` left unformatted;
conventional commits.

---

### Task 1: `enabled` in the config and the state file

**Files:** `src/airplay/config.rs`, `src/state.rs`

- `AirplayConfig::enabled: bool`, key `enabled`, default `true`, a boolean
  or "airplay N: enabled must be true or false".
- `State::airplay: BTreeMap<String, StoredReceiver>` with
  `StoredReceiver { enabled: Option<Stored<bool>> }`, skipped when empty.

- [ ] Tests: `enabled_is_read_and_true_by_default`,
  `enabled_that_is_not_a_boolean_is_an_error` (config.rs);
  `a_receivers_enabled_is_kept` (state.rs: round trip through the file).
- [ ] Run (fail), implement, run (pass), commit
  `feat(airplay): enabled in the config and the state file`.

### Task 2: Turning receivers on and off

**Files:** `src/airplay/mod.rs`, `src/control.rs`, `src/control/api.rs`,
`src/api.rs`, `src/http.rs`, `dist/auth.d/aas.json`

- `Receiver::enabled() -> bool`, `Receiver::set_enabled(&mut self, on)`:
  off takes the handle, asks it to stop and joins it in a detached thread;
  gives back the tap id for the engine (`take_tap`).
- `Receiver::spawn` does nothing while off. `AirplayView::enabled: bool`.
- `restore()` resolves each receiver's `enabled` from the state file.
- `Call::PatchAirplay(String, AirplayPatch)` with
  `AirplayPatch { enabled: Option<bool> }` (`deny_unknown_fields`); 404 for
  an unknown name; turning on while the output is open gives the tap at
  once (`airplay_taps`); the change is stored and saved; a receiver event is
  published.

- [ ] Tests (control/api.rs, receiver on port 9):
  `a_receiver_that_is_off_has_no_thread_and_says_so`,
  `patch_airplay_turns_a_receiver_off_and_on`,
  `patch_airplay_is_kept_in_the_state`,
  `the_state_does_not_outlast_a_changed_config`,
  `patch_airplay_for_an_unknown_receiver_is_404`.
- [ ] Run (fail), implement, run `cargo test -p aas`, clippy, commit
  `feat(airplay): turn receivers on and off through the API`.

### Task 3: aasctl, docs, the turntable board

- [ ] `aasctl airplay NAME enable|disable` → `PATCH /airplay/NAME`
  `{"enabled": true|false}`; tests `airplay_enable`, `airplay_disable`.
- [ ] README: `enabled` in the `[[airplay]]` table, the API row, the
  aasctl line.
- [ ] Commit `feat(aasctl): airplay NAME enable|disable`.
- [ ] The turntable board: build, install, Apple TV `enabled = false` in
  `aas.local.toml`; `GET /airplay` shows the sound bar on and the Apple TV
  off; a record plays to the bar only; `aasctl airplay "Apple TV" enable`
  starts it, `disable` stops it again.
