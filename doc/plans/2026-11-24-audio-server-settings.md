# Audio Server Settings Implementation Plan

**Goal:** Settings → Audio server in the HiFiBerry OS UI sets aas's card latency, AES67 on/off, stream and AES67 latency, warns about a latency that is too low, and offers the board's recommended values.

**Architecture:** aas reports the board's latency figures and runs AES67 from a table `aas-config` now writes; the config server writes `latency_ms` into `/etc/aas/aas.local.toml` and restarts aas; the UI page calls the config server for the latency and aas's API for AES67.

**Tech Stack:** Rust (aas), Python 3 with Flask and pytest (configurator, `aas-config`), Vue 3 with TypeScript, Pinia and Vitest (hbos-ui).

**Spec:** `doc/plans/2026-11-24-audio-server-settings-design.md` (aas repository).

## Global Constraints

- Three repositories, one branch each, nothing pushed without the owner:
  - aas: worktree `~/localdev/aas-aes67`, branch `feat/aes67` (the work builds on AES67);
  - configurator: `~/localdev/configurator`, new branch `feat/aas-settings` in a worktree `~/localdev/configurator-aas-settings`;
  - hbos-ui: `~/localdev/hbos-ui`, new branch `feat/aas-settings` in a worktree `~/localdev/hbos-ui-aas-settings`.
- Rust formatting: `rustfmt --edition 2024 <file>` per file, never `cargo fmt`.
- Commit messages in the project's voice; no tool attribution, no co-author trailers, no internal addresses or host names.
- Test data uses placeholders: addresses 192.0.2.x, MAC 02:00:00:00:00:01, host names like `studio`.
- Board figures, exact: Pi 5/CM5 recommended 5 ms, lowest safe 3 ms; any other board recommended 12 ms, lowest safe 6 ms.
- Latency range for both settings: whole ms, 1 to 100.
- Warnings are strict: a value equal to the recommended or the lowest safe value is not warned.
- Exact UI copy: amber card-latency note "Lower than recommended for this board: it may drop out under heavy load."; red note "Below what this board handles: dropouts are likely."; AES67 floor note "This board holds at least F ms; the stream plays at F ms."; no-AES67 note "AES67 is not available on this device: it needs aas with AES67 and a wired network connection."

## Review Focus

1. `aas.local.toml` with comments and other tables: the config server's edit keeps every other line as it was — Task 4 `test_post_keeps_other_lines`.
2. `aas.local.toml` that is not valid TOML: the config server refuses with 409 and leaves it untouched rather than rewriting it — Task 4 `test_post_refuses_invalid_file`.
3. The selected AES67 stream is no longer announced: the select still shows it, marked not announced, instead of an empty select that would send `null` — Task 8 `keeps an unannounced selection`.
4. The device runs PipeWire, so aas's API does not answer: the settings sections are hidden and no error toast shows — Task 7 `hides aas settings while PipeWire runs`.
5. A latency typed as `4.5`, `0` or `101`: the page refuses it before sending — Task 7 `refuses a latency outside 1 to 100 whole ms`.

---

### Task 1: aas reports the board's latency figures; Pi 5 default 5 ms

**Files:**
- Modify: `src/board.rs` (default 4 → 5; new `min_latency_ms`)
- Modify: `src/api.rs` (`SettingsView`, `SettingsPatch`)
- Modify: `src/control/api.rs` (`settings_view`, the patch check near line 1304)
- Modify: `src/control.rs` or wherever `Control` keeps `latency_ms`: keep the detected `Board`
- Modify: `README.md` (latency row near line 125; `GET /settings` text near line 711)

**Interfaces:**
- Produces: `Board::default_latency_ms(self) -> f32` gives 5.0 / 12.0; `Board::min_latency_ms(self) -> f32` gives 3.0 / 6.0; `GET /settings` JSON gains `latency_recommended_ms: f32` and `latency_min_ms: f32`.

- [ ] **Step 1: Write the failing tests**

In `src/board.rs` tests:
```rust
#[test]
fn the_latency_figures_by_board() {
    assert_eq!(Board::Pi5Class.default_latency_ms(), 5.0);
    assert_eq!(Board::Pi5Class.min_latency_ms(), 3.0);
    assert_eq!(Board::Other.default_latency_ms(), 12.0);
    assert_eq!(Board::Other.min_latency_ms(), 6.0);
}
```
In `src/control/api.rs` tests, beside the test at line ~3375 that names `format` and `resampling`:
```rust
// GET /settings has the board's figures; a PATCH naming either is 422.
assert_eq!(settings["latency_recommended_ms"], json!(12.0)); // the test control's board is Other
assert_eq!(settings["latency_min_ms"], json!(6.0));
for key in ["latency_recommended_ms", "latency_min_ms"] { /* PATCH {key: 1} → 422 */ }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test board:: control::api::tests::` — Expected: FAIL, `min_latency_ms` not found, fields missing.

- [ ] **Step 3: Implement**

`Control` keeps the `Board` it started with (from `server.rs:183`, where `board::detect()` already runs; pass it in, a test control uses `Board::Other`). `settings_view` fills the two fields from it. `SettingsPatch` gains the two names as `Option<serde_json::Value>`-like fields that make the patch 422 with "latency_recommended_ms is read-only", the same way `format` is refused. Any existing test asserting the 4 ms default changes to 5.

- [ ] **Step 4: README**

Latency row: "5 on a Pi 5 or CM5, 12 elsewhere". `GET /settings` text: the two read-only fields, what they are, and that the Pi 4 is "other" until measured.

- [ ] **Step 5: Run all tests, format, commit**

Run: `cargo test` — Expected: all pass. `rustfmt --edition 2024` on each changed `.rs` file.
```bash
git commit -am "settings: the board's recommended and lowest safe latency; 5 ms on a Pi 5"
```

### Task 2: AES67 `"auto"` holds the floor without a warning

**Files:**
- Modify: `src/aes67/input.rs` (`InputCommand::Latency`, `Receiver::set_latency`, `hold_floor`)
- Modify: `src/aes67/mod.rs` (the two `InputCommand::Latency(self.latency_ms())` sends, lines ~354 and ~535; test at ~869)
- Modify: `README.md` (the `latency_floor_ms` paragraph near line 489)

**Interfaces:**
- Produces: `InputCommand::Latency { ms: u32, auto: bool }`; `Receiver::set_latency(&mut self, latency_ms: u32, auto: bool)`.

- [ ] **Step 1: Write the failing test** in `src/aes67/input.rs` beside `a_latency_below_the_floor_is_held_at_the_floor`:
```rust
#[test]
fn an_automatic_latency_below_the_floor_is_held_without_a_warning() {
    let mut sim = Sim::new(session(2, Some((MASTER, 0))), square, true);
    sim.rx.set_latency(3, true);
    sim.run(10.0);
    let floor = ((1.5 * OUTPUT_DELAY + sim.rx.adaptive.delay_frames()) / 48.0).ceil() as u32;
    assert_eq!(sim.status().latency_floor_ms, Some(floor));
    assert_eq!(sim.rx.take_floor_warning(), None);
    assert_eq!(sim.status().restarts, 0);
}
```
The existing floor test calls `set_latency(3, false)` and `set_latency(20, false)`.

- [ ] **Step 2: Run it to see it fail** — `cargo test aes67::input` — Expected: compile error on the new signature.

- [ ] **Step 3: Implement** — the receiver keeps `auto`; `hold_floor` sets `floor_told` only when `!auto`. `mod.rs` sends `auto: self.latency == Latency::Auto`.

- [ ] **Step 4: README** — one sentence: with `"auto"` the floor is held without a warning.

- [ ] **Step 5: Run, format, commit**

Run: `cargo test` — all pass.
```bash
git commit -am "aes67: an automatic latency below the floor is held without a warning"
```

### Task 3: `aas-config` writes a disabled `[aes67]` table on a wired device

**Files:**
- Modify: `dist/hifiberryos/aas-config`
- Test: `dist/hifiberryos/tests/test_aas_config.py`
- Modify: `README.md` (the HiFiBerry OS / `aas-config` text)

**Interfaces:**
- Produces: `wired_interface(net_dir="/sys/class/net") -> str | None`; `config_from(systeminfo, audiocontrol=False, wired=None)` adds the table when `wired` is a name.

- [ ] **Step 1: Write the failing tests** (unittest style of the file; `tmp` dirs built per test):
```python
def test_eth0_is_chosen(self):          # net/eth0/device and net/eth1/device exist
    self.assertEqual(aas_config.wired_interface(net), "eth0")
def test_first_wired_without_eth0(self): # end0/device, wlan0/device + wlan0/wireless, lo
    self.assertEqual(aas_config.wired_interface(net), "end0")
def test_no_wired(self):                 # wlan0 with wireless, lo without device
    self.assertIsNone(aas_config.wired_interface(net))
def test_aes67_table(self):
    cfg = aas_config.config_from(load("dacplus-adc-pro"), wired="eth0")
    self.assertEqual(cfg["aes67"], {"interface": "eth0", "ptp": "listen",
                                    "input": {"enabled": False}, "output": {"enabled": False}})
def test_no_aes67_table_without_wired(self):
    self.assertNotIn("aes67", aas_config.config_from(load("dacplus-adc-pro")))
def test_local_aes67_merged_key_by_key(self):
    merged = aas_config.merge(aas_config.config_from(load("dacplus-adc-pro"), wired="eth0"),
                              {"aes67": {"input": {"enabled": True}}})
    self.assertEqual(merged["aes67"]["input"], {"enabled": True})
    self.assertEqual(merged["aes67"]["ptp"], "listen")
```
Interfaces sorted by name when there is no `eth0`; "wired" means a `device` entry and no `wireless` entry.

- [ ] **Step 2: Run to see them fail** — `python3 -m pytest dist/hifiberryos/tests -q` — FAIL.

- [ ] **Step 3: Implement**; `main` passes `wired_interface()` into `config_from`.

- [ ] **Step 4: Run, document, commit** — tests pass; README says the table is written on a wired device, disabled, and how to override it in `aas.local.toml`.
```bash
git commit -am "aas-config: a disabled AES67 table on a device with wired Ethernet"
```

### Task 4: Config server route `/api/v1/audio-server/aas-settings`

Repository: configurator, worktree `~/localdev/configurator-aas-settings`, branch `feat/aas-settings` from `main`.

**Files:**
- Create: `configurator/aas_settings.py`
- Modify: `configurator/handlers/audio_server_handler.py` (two handlers)
- Modify: `configurator/server.py` (routes beside `/api/v1/audio-server`, line ~524; the endpoint list at ~179)
- Modify: `auth/config.json` (add `/audio-server/aas-settings` to the `GET` "ok" paths; `POST` stays risky through the catch-all)
- Test: `tests/test_aas_settings.py`

**Interfaces:**
- Produces:
  - `aas_settings.LOCAL_PATH = "/etc/aas/aas.local.toml"`
  - `aas_settings.get() -> {"latency_ms": int | None}`
  - `aas_settings.set_latency(latency_ms: int | None) -> {"latency_ms": int | None, "restarted": bool}`; raises `ValueError` for a value outside 1–100 or not an int (bool is not an int), `AasSettingsFileError` for a file that is not valid TOML.
  - `GET /api/v1/audio-server/aas-settings` → `{"status": "success", "data": {"latency_ms": n | null}}`
  - `POST` with `{"latency_ms": n | null}` → same envelope with `data` from `set_latency`; 400 for a bad value, 409 for an invalid file, 500 otherwise.

- [ ] **Step 1: Write the failing tests** (pytest, `monkeypatch` `LOCAL_PATH` into `tmp_path`, and the restart function):
```python
def test_get_without_file(local): assert aas_settings.get() == {"latency_ms": None}
def test_get_reads_top_level_key(local):   # file: 'latency_ms = 8\n[aes67]\nlatency_ms = 3\n'
    assert aas_settings.get() == {"latency_ms": 8}
def test_post_writes_and_restarts(local, restarts):  # aas active
    assert aas_settings.set_latency(6) == {"latency_ms": 6, "restarted": True}
    assert tomllib.loads(local.read_text())["latency_ms"] == 6
def test_post_null_removes_key(local, restarts): ...  # key gone, other keys kept
def test_post_keeps_other_lines(local, restarts):
    # '# mine\n\nbit_perfect = "off"\nlatency_ms = 12\n\n[aes67]\ninterface = "eth0"  # wired\n'
    # after set_latency(6): only the latency_ms line differs, comments and [aes67] unchanged
def test_post_into_file_with_only_tables(local, restarts):  # '[aes67]\nptp = "listen"\n'
    # latency_ms goes above the first table header, parses as top level
def test_post_refuses_invalid_file(local, restarts):        # 'latency_ms = = 4'
    with pytest.raises(aas_settings.AasSettingsFileError): aas_settings.set_latency(6)
    assert local.read_text() == 'latency_ms = = 4'
@pytest.mark.parametrize("bad", [0, 101, 4.5, "6", True])
def test_post_refuses_bad_values(local, bad): ...           # ValueError, file untouched
def test_no_restart_when_aas_is_not_active(local, restarts): # restarted False
def test_routes(client): ...  # GET 200 envelope; POST 400 for {"latency_ms": 0}; 409 invalid file
```

- [ ] **Step 2: Run to see them fail** — `python3 -m pytest tests/test_aas_settings.py -q`.

- [ ] **Step 3: Implement `aas_settings.py`**

Line-based edit of the one key, so comments survive: the top-level part is the lines before the first line that starts a table (`[`, after leading white space). Drop every top-level line whose key is `latency_ms`; for a number, insert `latency_ms = N` as the last top-level line (or first line when there is none). Write to a temp file in the same directory and `os.replace` it; create `/etc/aas` if missing. Validate the whole result with `tomllib` before replacing. Restart with `systemctl restart aas.service` only when `systemctl is-active --quiet aas.service` is true; reuse the helpers in `audio_server.py` if they fit. Answer without waiting for aas to come up.

- [ ] **Step 4: Run all tests, commit**

Run: `python3 -m pytest -q` — all pass.
```bash
git commit -am "audio-server: read and set aas's latency in aas.local.toml"
```

### Task 5: UI API clients

Repository: hbos-ui, worktree `~/localdev/hbos-ui-aas-settings`, branch `feat/aas-settings` from `main`.

**Files:**
- Modify: `src/api/aas.ts`
- Modify: `src/api/audio-server.ts`
- Test: `src/api/__tests__/aas.test.ts`, `src/api/__tests__/audio-server.test.ts`

**Interfaces:**
- Produces in `src/api/aas.ts`:
  - `interface AasSettings { latency_ms: number; latency_recommended_ms?: number; latency_min_ms?: number }`
  - `interface Aes67Input { enabled: boolean; stream: string | null; latency_ms: number; latency_auto: boolean; latency_floor_ms?: number; receiving: boolean; mode: string | null }`
  - `interface Aes67Stream { name: string; selectable: boolean }`
  - `getSettings(): Promise<AasResponse<AasSettings>>` — `GET /settings`
  - `getAes67(): Promise<AasResponse<{ input: Aes67Input }>>` — `GET /aes67`
  - `getAes67Streams(): Promise<AasResponse<Aes67Stream[]>>` — `GET /aes67/streams`
  - `setAes67Input(patch: { enabled?: boolean; stream?: string | null; latency_ms?: number | 'auto' }): Promise<AasResponse<unknown>>` — `PATCH /aes67/input`
- Produces in `src/api/audio-server.ts`:
  - `getAasLatency(): Promise<{ latency_ms: number | null } | ApiError>` — `GET {config}/audio-server/aas-settings`
  - `setAasLatency(latency_ms: number | null): Promise<{ latency_ms: number | null; restarted: boolean } | ApiError>` — `POST`

- [ ] **Step 1: Failing tests** in the existing files' style (mocked `apiFetch`): each function hits the path and method above and returns the body; an HTTP 404 from `getAes67` comes back as an `ApiError` (`isAasError` true).
- [ ] **Step 2: Run** `npx vitest run src/api` — FAIL.
- [ ] **Step 3: Implement** with the files' existing `request`/`jsonInit` and `call` helpers.
- [ ] **Step 4: Run** — PASS. **Commit** `git commit -am "api: aas settings, AES67 input and the config server's aas latency"`.

### Task 6: The Settings tile

**Files:**
- Modify: `src/views/services/index.vue` (a `ContentBoxLink` to `{ name: 'audio-server' }`, after "DSP Backends", icon `tabler/server`, heading "Audio Server", text "Choose the audio server and set its latency and AES67")
- Test: `src/views/services/__tests__/index.audio-server.test.ts`

- [ ] **Step 1: Failing test** — mounting the view renders a link whose `to` is `{ name: 'audio-server' }` and whose text contains "Audio Server".
- [ ] **Step 2: Run** `npx vitest run src/views/services` — FAIL.
- [ ] **Step 3: Implement**; the System Tools link stays.
- [ ] **Step 4: Run** — PASS. **Commit** `git commit -am "settings: an Audio Server tile"`.

### Task 7: The card latency on the page

**Files:**
- Create: `src/composables/useLatencyNotes.ts`
- Modify: `src/views/services/audio-server.vue`
- Test: `src/composables/__tests__/useLatencyNotes.test.ts`, `src/views/services/__tests__/audio-server.latency.test.ts`

**Interfaces:**
- Produces in `useLatencyNotes.ts`:
  - `type Note = { level: 'amber' | 'red'; text: string } | null`
  - `cardLatencyNote(ms: number, recommended?: number, min?: number): Note` — red below `min`, amber below `recommended`, else `null`; `null` when the figures are missing (older aas).
  - `aes67LatencyNote(ms: number, floor?: number): Note` — amber with the floor text when `ms < floor`, else `null`.
  - `validLatency(value: unknown): number | null` — a whole number 1 to 100, else `null`.

- [ ] **Step 1: Failing unit tests** for the composable, with the Global Constraints' exact texts:
```ts
expect(cardLatencyNote(5, 5, 3)).toBeNull()
expect(cardLatencyNote(4, 5, 3)?.level).toBe('amber')
expect(cardLatencyNote(3, 5, 3)?.level).toBe('amber')
expect(cardLatencyNote(2, 5, 3)).toEqual({ level: 'red', text: 'Below what this board handles: dropouts are likely.' })
expect(cardLatencyNote(2)).toBeNull()
expect(aes67LatencyNote(5, 6)?.text).toBe('This board holds at least 6 ms; the stream plays at 6 ms.')
expect(aes67LatencyNote(6, 6)).toBeNull()
expect(aes67LatencyNote(5)).toBeNull()
for (const bad of [0, 101, 4.5, '6', null]) expect(validLatency(bad)).toBeNull()
expect(validLatency(6)).toBe(6)
```
- [ ] **Step 2: Failing view tests** (mock the API modules and the audio-server store):
  - `hides aas settings while PipeWire runs` — `running: 'pipewire'`: no latency input, no AES67 section, no error toast, the switch shows.
  - `shows the saved latency or the recommended one` — config value `null`, settings recommended 5 → the field shows "Recommended (5 ms)"; config value 8 → 8.
  - `refuses a latency outside 1 to 100 whole ms` — typing 4.5 disables Save, no `setAasLatency` call.
  - `confirms below the lowest safe value` — 2 with min 3 → a confirmation opens; cancel sends nothing; confirm calls `setAasLatency(2)`.
  - `waits for aas after saving` — after `setAasLatency` resolves `restarted: true`, "Restarting…" shows; `getSettings` failing then succeeding clears it and shows the new value; 30 failed polls at 1 s (fake timers) show an error naming the saved value.
- [ ] **Step 3: Run** `npx vitest run src/composables src/views/services` — FAIL.
- [ ] **Step 4: Implement** the composable and, in `audio-server.vue`, a "Latency" section shown while `running === 'aas'` and `getSettings` answers: the value from `getAasLatency` (null → "Recommended (N ms)"), a number input, the note, Save, the running value from `getSettings().latency_ms` in the status line with "differs from the saved value" when it does; restart polling `getSettings` once a second up to 30 s.
- [ ] **Step 5: Run** — PASS. **Commit** `git commit -am "audio-server: the card latency, with warnings below the board's figures"`.

### Task 8: AES67 and "Recommended for this board" on the page

**Files:**
- Modify: `src/views/services/audio-server.vue`
- Test: `src/views/services/__tests__/audio-server.aes67.test.ts`

**Interfaces:**
- Consumes: Task 5's `getAes67`, `getAes67Streams`, `setAes67Input`, `setAasLatency`; Task 7's `aes67LatencyNote`, `validLatency`.

- [ ] **Step 1: Failing view tests** (mocked API):
  - `shows the no-AES67 note on 404` — `getAes67` gives an `ApiError` with HTTP 404 → the exact no-AES67 text, no switch.
  - `turns AES67 on` — the switch calls `setAes67Input({ enabled: true })` once.
  - `lists only selectable streams` — streams `a` (selectable) and `b` (not) → the select offers `a`.
  - `keeps an unannounced selection` — input `stream: 'gone'`, not in the list → the select shows "gone (not announced)" selected; nothing is sent.
  - `selects a stream` — choosing `a` calls `setAes67Input({ stream: 'a' })`.
  - `sets the AES67 latency` — "Automatic (6 ms)" when `latency_auto` with `latency_ms` 5 and `latency_floor_ms` 6 (shows the higher); entering 4 with floor 6 shows the floor note and calls `setAes67Input({ latency_ms: 4 })`; choosing Automatic sends `'auto'`.
  - `recommended sets AES67 first, then the latency` — after the confirmation, `setAes67Input({ latency_ms: 'auto' })` is called before `setAasLatency(null)`; when the saved latency is already `null`, `setAasLatency` is not called.
- [ ] **Step 2: Run** `npx vitest run src/views/services` — FAIL.
- [ ] **Step 3: Implement** the "AES67" section (switch, select, latency with Automatic or a number, and under it the line "The AES67 latency includes the card's buffer: a lower card latency lowers the least it can hold.") and the "Recommended for this board" button with a `ConfirmationDialog` whose message names the card latency (`Recommended (N ms)`) and "AES67 latency: Automatic". Every AES67 change applies at once, with no restart.
- [ ] **Step 4: Run** `npm test` — all pass; `npm run build` (or the repository's type check) passes.
- [ ] **Step 5: Commit** `git commit -am "audio-server: AES67 on/off, stream and latency, and the recommended values"`.

### Task 9: Check on a test board

No code. On the CM5 test board (never the Pi 4 device for AES67): build aas from `feat/aes67`, install the configurator branch's `aas_settings.py` route and the UI build per each repository's README, then:

- [ ] `GET /api/aas/v1/settings` shows `latency_recommended_ms: 5`, `latency_min_ms: 3`.
- [ ] Setting the card latency to 8 in the page restarts aas, the page shows "Restarting…" and then 8; `aas.local.toml` has `latency_ms = 8` with its other lines intact.
- [ ] AES67 on, a stream selected, AES67 latency 3 → the floor note with 6; Automatic → no note and no log warning.
- [ ] "Recommended for this board" → card latency back to recommended, AES67 Automatic.
- [ ] Record the results in the ledger; put the board back as it was.
