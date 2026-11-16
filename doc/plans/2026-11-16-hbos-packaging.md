# Packaging aas for HiFiBerry OS, with the switch Implementation Plan

**Goal:** `aas` builds into the Debian packages `aas` and `libasound2-plugin-aas`, which carry the switch command `hifiberry-audio-server` and the boot-time `aas-config`, and HiFiBerry OS builds them and pulls them into its meta packages.

**Architecture:** Two scripts under `dist/hifiberryos/`: `hifiberry-audio-server`, a POSIX shell script that stores the choice and links, masks and enables, and `aas-config`, a Python 3 script that writes `/etc/aas/aas.toml` from the config server's system info. `debian/` builds both packages from the workspace, as `acr` does. `hifiberry-os` gets `packages/aas/build.sh`, which clones this repository and runs its `build.sh`, and the Depends in `hbos-minimal` and `hbos-full`.

**Tech Stack:** POSIX sh, Python 3 standard library only (`tomllib` to read, a small writer of its own), debhelper 13, cargo.

**Spec:** `doc/switch.md`, "The choice", "What `set` changes", "Packages", "Work in this repository".

## Global Constraints

- The choice file is `/etc/hifiberry/audio-server`, one line, `pipewire` or `aas`; missing means `pipewire`.
- `hifiberry-audio-server set pipewire|aas` writes the file and applies; `get` prints exactly two lines, `running <server>` and `chosen <server>`, where running is `aas` if `systemctl is-active --quiet aas.service`, else `pipewire`. `set` needs root (exit 1 with a message otherwise); a bad argument is exit 2 with usage. `set` applies even when the choice is unchanged; every step is idempotent. Nothing reboots.
- `set aas` links `/etc/alsa/conf.d/99-zz-aas.conf` → `/usr/share/aas/asound.conf`, `/etc/pulse/client.conf.d/90-aas.conf` → `/usr/share/aas/pulse-client.conf`, `/etc/sysctl.d/90-aas.conf` → `/usr/share/aas/90-aas.conf`; masks globally (`systemctl --global mask`) the user units `pipewire.service pipewire.socket pipewire-pulse.service pipewire-pulse.socket wireplumber.service filter-chain.service pipewire-api.service vu-meter.service aes67.service aes67-agent.service usbaudio.service usbaudio-state.service analoginput.service`; and enables the system units `aas.service aas-config.service`. `set pipewire` removes the three links (only if they are links to those targets), unmasks the same user units globally, and disables `aas.service aas-config.service`. `pipewire-alsa`'s files and `/etc/asound.conf` are never touched. System units of other packages are not touched.
- For tests every path is under `$AAS_SWITCH_ROOT` (default empty) and `systemctl` is `${SYSTEMCTL:-systemctl}`; the root check is skipped when `AAS_SWITCH_ROOT` is set.
- `aas-config` reads `http://127.0.0.1:1081/api/v1/systeminfo`, retrying once a second for up to 30 s, and writes `/etc/aas/aas.toml` atomically (temp file + rename), starting with a comment that says the file is generated at every boot and that `/etc/aas/aas.local.toml` is merged over it. It always exits 0. If the config server cannot be reached: an existing `aas.toml` is kept; without one, the file gets only the comment (and the local file merged), so `aas` detects the card.
- What `aas-config` writes from `soundcard`: `card` = `hardware_index` as a string when it is an integer, else no `card`; `volume_control` = `volume_control` when non-empty, else `volume_control = "Softvol"` and `volume_mode = "follow"`; `[analog] enabled = true` when `input_channels > 0` and `card_type` does not contain `"Digi"` (it may be a list or a string), else no `[analog]` table; a balance plugin, one `[[plugin]]` table with `place = "output"`, `type = "balance"`, `balance = 0`.
- `/etc/aas/aas.local.toml`, when present and valid TOML, is merged over the generated values: a top-level key or a whole array of tables is replaced, a table is merged key by key. An invalid local file is logged and ignored.
- `aas-config.service`: `Type=oneshot`, `RemainAfterExit=yes`, `After=config-server.service`, `Before=aas.service`, `[Install] WantedBy=aas.service`. `dist/aas.service` gains `After=volume-restore.service aas-config.service`, harmless where they do not exist.
- Packages: `aas` (Architecture any) ships `/usr/bin/aas`, `/usr/bin/aasctl`, `/usr/sbin/hifiberry-audio-server`, `/usr/lib/aas/aas-config`, `aas.service` and `aas-config.service` installed disabled and not started, `/usr/share/aas/{asound.conf,pulse-client.conf,90-aas.conf}`, the nginx location from `dist/nginx/` into `/etc/nginx/hifiberry-api.d/` and the auth manifest from `dist/auth.d/` into `/etc/hifiberry/auth.d/`; Depends add `python3`, Recommends `libasound2-plugin-aas`. `libasound2-plugin-aas` ships `/usr/lib/${DEB_HOST_MULTIARCH}/alsa-lib/libasound_module_pcm_aas.so` and the symlink `libasound_module_ctl_aas.so` to it. Installing either changes nothing on a device that stays on PipeWire.
- Maintainer scripts: `aas` postinst `configure` reloads nginx and restarts `hifiberry-auth` when each is active, failures ignored; it never enables or starts `aas`. `aas` prerm `remove` runs `hifiberry-audio-server set pipewire` when the choice is `aas`, so removing the package leaves a working PipeWire device. postrm `purge` removes `/etc/hifiberry/audio-server` and `/etc/aas/aas.toml`.
- Version: `debian/changelog` `aas (0.1.0) trixie; urgency=medium`, matching `Cargo.toml` (`hifiberry-os` fails the build on a mismatch). `debian/source/format` `3.0 (native)`. Rust floor 1.85 (`rust-version`), with acr's rustup fallback.
- Public text names no tooling and no test machines. Commits in this repository: lowercase `area: summary`. In `hifiberry-os`: its Conventional Commits style, `feat(packages): ...`.
- Never run `cargo fmt` on the tree. Test the Rust side with `cargo test -p aas -p aas-client`.

## Review Focus

1. `set aas` then `set pipewire` leaves no link, mask or enablement behind that was not there before (Task 1 round-trip test).
2. A `pipewire-alsa` default link or an admin's own `/etc/asound.conf` is never removed or changed (Task 1).
3. The config server is down at boot on a device that has run `aas` before: the old config is kept, not replaced by an empty one (Task 2).
4. A card without a hardware control, a Digi, and no card at all each give a config `aas` loads (Task 2 fixtures, Task 5 on a board).
5. `apt remove aas` on a device switched to `aas` leaves PipeWire running after reboot (Task 3 prerm, Task 5).

---

### Task 1: `hifiberry-audio-server`

**Files:**
- Create: `dist/hifiberryos/hifiberry-audio-server`
- Create: `dist/hifiberryos/tests/test-hifiberry-audio-server.sh`

- [ ] **Step 1: Write the test script.** POSIX sh, runs from anywhere, exits non-zero on the first failure and prints `ok` lines. It makes a temp root, puts a fake `systemctl` first on `PATH` (a script that appends its arguments to `$ROOT/systemctl.log` and answers `is-active` from `$ROOT/active`), and checks:
  - `get` with no choice file: `running pipewire` / `chosen pipewire`.
  - `set aas`: the file holds `aas`; the three links exist and point at the targets above; the log holds `--global mask` with every user unit listed, and `enable aas.service aas-config.service`; `get` prints `chosen aas`.
  - `set aas` again: exit 0, same links.
  - `set pipewire`: links gone; log has `--global unmask` with the same units and `disable aas.service aas-config.service`; file holds `pipewire`.
  - A regular file at `/etc/alsa/conf.d/99-zz-aas.conf` (not a link) survives `set pipewire`; `/etc/alsa/conf.d/99-pipewire-default.conf` and `/etc/asound.conf` placed before are untouched by both.
  - `set foo` and no argument: exit 2. With `AAS_SWITCH_ROOT` unset and not root: `set aas` exits 1.

- [ ] **Step 2: Run it to see it fail.** `sh dist/hifiberryos/tests/test-hifiberry-audio-server.sh` → fails, script missing.

- [ ] **Step 3: Write `dist/hifiberryos/hifiberry-audio-server`**, POSIX sh, `set -eu`, per the Global Constraints. `mkdir -p` the parent of each link and of the choice file. `systemctl --global mask` and `unmask` take all units in one call each; a failure of either is reported and makes `set` exit 1 after the other steps ran.

- [ ] **Step 4: Run the test.** Expected: all `ok`, exit 0. Also `shellcheck` it if installed.

- [ ] **Step 5: Commit.** `git add dist/hifiberryos && git commit -m "dist: hifiberry-audio-server, the switch between PipeWire and aas"`

### Task 2: `aas-config` and its unit

**Files:**
- Create: `dist/hifiberryos/aas-config`, `dist/hifiberryos/aas-config.service`
- Create: `dist/hifiberryos/tests/test_aas_config.py`, fixtures in `dist/hifiberryos/tests/systeminfo/*.json`
- Modify: `dist/aas.service` (`After=`)

**Interfaces:** `aas-config [--systeminfo FILE] [--output PATH] [--local PATH]`; the options are for tests (default: fetch from the config server, `/etc/aas/aas.toml`, `/etc/aas/aas.local.toml`). Functions importable by the test: `config_from(systeminfo: dict) -> dict`, `merge(generated: dict, local: dict) -> dict`, `dump(config: dict) -> str`.

- [ ] **Step 1: Write the tests** (`python3 -m unittest`), loading the script as a module with `importlib.machinery.SourceFileLoader`. Fixtures: `dacplus-adc-pro.json` (`hardware_index` 0, `volume_control` `Digital`, `input_channels` 2, `card_type` `["DAC","ADC"]`), `digiplus.json` (`card_type` `["Digi"]`, `input_channels` 2, `volume_control` null), `amp-no-control.json` (`volume_control` null, `input_channels` 0), `no-card.json` (`hardware_index` null). Tests:
  - DAC+ ADC Pro → `card == "0"`, `volume_control == "Digital"`, no `volume_mode`, `analog.enabled is True`, one `plugin` entry `{place: "output", type: "balance", balance: 0}`.
  - Digi → no `analog`; `volume_control == "Softvol"`, `volume_mode == "follow"`.
  - Amp → `Softvol`/`follow`, no `analog`.
  - No card → no `card` key.
  - `merge`: local `latency_ms = 8` added; local `card = "1"` wins; local `[analog] volume_db = -6` keeps generated `enabled`; a local `[[...]]` array replaces the generated one.
  - `dump` round-trips through `tomllib.loads` for every fixture and for the merged case, and starts with the generated-file comment.
  - Main with `--systeminfo` missing and an existing output file: the output is unchanged. With no output file: a file with only the comment.
  - An invalid local file is ignored (output equals the generated config).

- [ ] **Step 2: Run them to see them fail.** `python3 -m unittest discover -s dist/hifiberryos/tests -p 'test_*.py'` → errors, script missing.

- [ ] **Step 3: Implement** `aas-config` (`#!/usr/bin/python3`, standard library only: `urllib.request`, `json`, `tomllib`, `os`, `tempfile`), per the Global Constraints. `dump` writes strings with TOML escaping, ints, floats, bools, lists of scalars, tables and arrays of tables, top-level scalars first. Log lines go to stderr with the prefix `aas-config:`. Write `aas-config.service` and add the `After=` to `dist/aas.service`.

- [ ] **Step 4: Run the tests.** Expected: all pass. `aas` has no config check option; loading a generated file in the daemon is checked in Task 5.

- [ ] **Step 5: Commit.** `git commit -m "dist: aas-config writes the daemon's config from the system info"`

### Task 3: `debian/` and `build.sh`

**Files:**
- Create: `debian/{control,rules,changelog,copyright,aas.install,aas.links,libasound2-plugin-aas.install,libasound2-plugin-aas.links,aas.postinst,aas.prerm,aas.postrm,source/format}` (use `.install` only where `rules` does not install), `build.sh`
- Modify: `.gitignore` (debian build output), `README.md` (a short "Debian packages" note under Installing)

- [ ] **Step 1: Write `debian/`** from `acr`'s packaging as a model (its `rules` rustup fallback, `CARGO_HOME`/`RUSTUP_HOME` under `debian/`, `--jobs 1`), with `RUST_MIN_VERSION = 1.85`. `override_dh_auto_build`: `cargo build --release --features alsa-output --bin aas --bin aasctl` and `cargo build --release -p aas-alsa`. `override_dh_auto_test`: nothing (the tests need no card but take long on a Pi; they run in CI). `override_dh_installsystemd`: `dh_installsystemd --no-enable --no-start` for both units. Install per the Global Constraints. Maintainer: as in `acr`'s `control`.
- [ ] **Step 2: Write the maintainer scripts** per the Global Constraints, each `set -e`, `#DEBHELPER#` kept.
- [ ] **Step 3: `build.sh`** as `acr`'s: `sbuild --chroot-mode=unshare --enable-network --no-clean-source $DIST_ARG`, and, when `sbuild` is not installed, `dpkg-buildpackage -us -uc -b`.
- [ ] **Step 4: Check what can be checked here:** `sh -n` on the maintainer scripts, `dpkg-parsechangelog` works, `cargo test -p aas -p aas-client` passes. A package build needs libasound; the controller builds on a board (Task 5).
- [ ] **Step 5: Commit.** `git commit -m "debian: packages aas and libasound2-plugin-aas"`

### Task 4: `hifiberry-os`

**Repository:** `/home/matuschd/localdev/hifiberry-os`, new branch `feat/aas` from `main`.

**Files:**
- Create: `packages/aas/build.sh` (from `packages/sendspin/build.sh`: `PACKAGE="aas"`, `REPO_URL="https://github.com/hifiberry/aas"`, moves both `aas_*.deb` and `libasound2-plugin-aas_*.deb`, `--clean` removes both), `packages/aas/clean.sh` if siblings have one
- Modify: `.gitignore` (`packages/aas/aas`), `packages/repos.md` (a row), `packages/hifiberryos/src/debian/control` (`aas, libasound2-plugin-aas` in `hbos-minimal` and `hbos-full` Depends, and their Descriptions if they list components), `packages/hifiberryos/src/debian/changelog` (new stanza `0.15`, one line: the audio server aas, beside PipeWire, chosen in the web UI)

- [ ] **Step 1:** Make the changes. Run `python3 scripts/check-packages.py` if it runs without network; `bash -n packages/aas/build.sh`.
- [ ] **Step 2: Commit** on `feat/aas`: `feat(packages): aas audio server and its ALSA plugin`. Do not push.

### Task 5: Board check (controller)

Not dispatched. On the CM5 test board (plain Debian trixie, no HiFiBerry OS, no config server): install `debhelper` if missing, build with `dpkg-buildpackage -us -uc -b` from a copy of the tree, then:
- [ ] Install both packages: nothing linked, nothing masked, `aas` not enabled, `/etc/asound.conf` untouched.
- [ ] `sudo hifiberry-audio-server set aas`: links, global masks (`ls /etc/systemd/user/`), `aas.service` enabled. `sudo /usr/lib/aas/aas-config` without a config server writes the comment-only file; `sudo systemctl start aas` plays a tone through `aplay -D default` and `paplay`.
- [ ] `aas-config --systeminfo` with the DAC+ ADC Pro fixture adapted to the board's card index: `aas` starts with it, the analog input and the balance plugin show in `aasctl plugins`.
- [ ] `sudo hifiberry-audio-server set pipewire`: back to as installed. `set aas` again, then `sudo apt remove aas`: the choice is `pipewire` and the links and masks are gone.
- [ ] Purge both, restore the board as found.
