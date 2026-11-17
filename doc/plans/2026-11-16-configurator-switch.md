# The configurator side of the switch Implementation Plan

**Goal:** The HiFiBerry configurator lets the web UI read and set which audio server a device runs, resets `aas` on a factory reset, keeps `config-volume` off `aas`'s `Master`, restores the card's volume before `aas` starts, and reports `aas` as the audio engine when it is the chosen one.

**Architecture:** A small module `configurator/audio_server.py` wraps the `hifiberry-audio-server` command that the `aas` package ships (absent on a device without it) and reads the choice file. A handler serves `GET`/`POST /api/v1/audio-server`. The factory reset, `volume.py` and the features loader ask the module which server is chosen.

**Tech Stack:** Python 3, Flask, pytest. No new dependencies.

**Repository:** `https://github.com/hifiberry/configurator`, checked out beside this one at `../configurator`; branch `feat/audio-server` from `main`.

**Spec:** `aas` repository, `doc/switch.md`: "The choice", "The web UI and the configurator (7)", "Volume" (the `config-volume` paragraph).

## Global Constraints

- The command is `/usr/sbin/hifiberry-audio-server`; the choice file `/etc/hifiberry/audio-server` (one line, `pipewire` or `aas`, missing or anything else means `pipewire`). Both paths are module constants tests can monkeypatch.
- `get` prints two lines, `running <server>` and `chosen <server>`. `set pipewire|aas` exits 0 on success, 1 on failure, 2 on a bad argument. Run with `subprocess.run([...], capture_output=True, text=True, timeout=30)`, never `shell=True` (`.codereview.toml` forbids it).
- `GET /api/v1/audio-server` answers `{"status": "success", "data": {"running": R, "chosen": C, "available": A}}`. Without the command: `running` `pipewire`, `chosen` from the choice file (so `pipewire`), `available` false, still 200.
- `POST /api/v1/audio-server` takes `{"server": "pipewire"|"aas"}`. Any other body: 400 `{"status": "error", "message": ...}`. Without the command: 409 with a message that `aas` is not installed. Command failure: 500 with its stderr in the message. Success: 200 with `data` as for `GET` plus `"reboot_required": true` when `chosen` differs from `running`, else false. It does not reboot.
- `auth/config.json`: `GET /api/v1/audio-server` in the `ok` GET list; `POST` stays at the default `risky`.
- Factory reset (`POST /api/v1/config/reset`): after the existing steps, `reset_audio_server()`; its result goes into `data` as `audio_server`. It is best effort and never fails the reset. Without the command: `{"status": "unavailable"}`. With it: if `aas.service` is active, stop it, remove `/var/lib/aas/state.toml`, start it; then `set pipewire`. A factory reset therefore always chooses PipeWire for the next boot.
- `config-volume --store/--restore`: when the chosen server is `aas`, the `default` ctl's `Master` and `Capture` (the `system.volume.pipewire.*` keys) are neither stored nor restored; the card's own control still is. When `pipewire` is chosen, behaviour is exactly as today.
- `systemd/volume-restore.service` gains `Before=aas.service` (harmless where `aas` is absent).
- Feature descriptors: an optional field `"audio_server": "pipewire"|"aas"`. `load_descriptors` drops a descriptor whose `audio_server` is not the chosen server. `features.d/pipewire.json` gets `"audio_server": "pipewire"`; new `features.d/aas.json`: name `"Audio engine"`, `provided_by` `"aas"`, `systemd_services` `["aas.service"]`, icon and category and disposition as `pipewire.json`, `"audio_server": "aas"`; added to `setup.py` data_files.
- Version 2.20.0 in `configurator/_version.py` and a `debian/changelog` stanza in the existing style (`.codereview.toml` asks for both on behaviour changes).
- Commits: the repository's Conventional Commits, `type(scope): lowercase summary`. No trailers, nothing naming tooling, no test machines.
- Tests: `python -m pytest tests/ -q`, all passing. Never touch the real `/etc`, `/var/lib` or systemd in tests: monkeypatch paths and `subprocess.run`.

## Review Focus

1. A device without `aas` installed (every device today): the route answers `pipewire`/`available: false`, the factory reset is unchanged apart from `audio_server: unavailable`, `config-volume` and the features are unchanged (Tasks 1-4).
2. The command times out or is killed: the route answers 500, the reset goes on (Tasks 1, 2).
3. A choice file with trailing whitespace or a newline only: read as written, stripped; empty means `pipewire` (Task 1).
4. `config-volume --restore` under `aas` never opens the `default` ctl, so it does not wait on a daemon that is not running yet (Task 3).

---

### Task 1: `audio_server` module and route

**Files:** Create `configurator/audio_server.py`, `configurator/handlers/audio_server_handler.py`, `tests/test_audio_server.py`. Modify `configurator/server.py` (routes, handler instance), `auth/config.json`.

**Interfaces (produced):** in `configurator/audio_server.py`: `COMMAND`, `CHOICE_PATH`, `def chosen() -> str`, `def available() -> bool`, `def get() -> dict` (`running`, `chosen`, `available`), `def set_server(server: str) -> dict` (raises `ValueError` for a bad name, `AudioServerUnavailable` without the command, `AudioServerError(message)` on failure; returns `get()` plus `reboot_required`).

- [ ] Step 1: tests for `chosen()` (missing, `aas\n`, `aas  `, empty, `foo`), `get()` with and without the command (fake `subprocess.run`), `set_server` (bad name, unavailable, exit 1 with stderr, success with reboot_required true and false), and the two routes through Flask's test client (200/400/409/500 bodies as in the Global Constraints). Follow the fixtures of `tests/test_reset_managed_services.py`.
- [ ] Step 2: run, see them fail.
- [ ] Step 3: implement; register the routes next to `/api/v1/system/reboot`; add the GET path to `auth/config.json`.
- [ ] Step 4: `python -m pytest tests/ -q` passes.
- [ ] Step 5: commit `feat(audio-server): read and set the audio server`.

### Task 2: factory reset

**Files:** Modify `configurator/server.py` (`reset_config`), `configurator/audio_server.py` (`def reset() -> dict`). Test: `tests/test_audio_server_reset.py`.

- [ ] Step 1: tests for `reset()`: unavailable; aas active → calls in order `systemctl stop aas.service`, removal of the state file (a tmp path via monkeypatched `STATE_PATH`), `systemctl start aas.service`, `set pipewire`; aas inactive → no stop/start, state file removed, `set pipewire`; a failing step is reported in the result and the rest still runs. And the reset route's JSON carries `data.audio_server`.
- [ ] Step 2-4: fail, implement, pass.
- [ ] Step 5: commit `feat(audio-server): a factory reset resets aas and chooses PipeWire`.

### Task 3: `config-volume` and `volume-restore`

**Files:** Modify `configurator/volume.py` (the store block near line 211-236, the restore block near 289-317), `systemd/volume-restore.service`. Test: `tests/test_volume_audio_server.py`, extend `tests/test_volume_units_ordering.py`.

- [ ] Step 1: tests: with `audio_server.chosen` monkeypatched to `aas`, store and restore never call `is_pipewire_available`, `get_pipewire_volume` or `set_pipewire_volume` (fakes that fail the test if called) and still handle the card control (fake); with `pipewire` they are called as before. Ordering test: `aas.service` in `volume-restore.service` `Before`.
- [ ] Step 2-4: fail, implement, pass.
- [ ] Step 5: commit `fix(volume): leave aas's Master alone and restore before aas starts`.

### Task 4: features, version, changelog

**Files:** Create `features.d/aas.json`. Modify `features.d/pipewire.json`, `configurator/memoryinfo.py` (`load_descriptors`, `_descriptor_from_dict` if the field must be carried), `setup.py`, `configurator/_version.py`, `debian/changelog`. Test: extend `tests/test_memoryinfo_shipped_descriptors.py` or a new `tests/test_memoryinfo_audio_server.py`.

- [ ] Step 1: tests: with `pipewire` chosen, the loaded descriptors contain `pipewire` and not `aas`; with `aas`, the reverse; a descriptor without the field is always loaded; every shipped file still loads.
- [ ] Step 2-4: fail, implement, pass. Version 2.20.0; changelog stanza listing the four changes in the existing style.
- [ ] Step 5: commit `feat(audio-server): report aas as the audio engine when it is chosen (2.20.0)`.
