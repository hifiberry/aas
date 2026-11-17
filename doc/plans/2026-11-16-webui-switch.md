# The web UI side of the switch Implementation Plan

**Goal:** The HiFiBerry OS web UI works under both audio servers: it knows which one runs, switches between them from a settings page (warning first about what stops working, then offering the reboot), and under `aas` drives balance, channel mode, the VU meter and the background-service probes through `aas`'s API instead of `pipewire-api` and `vu-meter`.

**Architecture:** A Pinia store `audio-server` loads `GET /api/config/v1/audio-server` once at start. A new API module `src/api/aas.ts` (base `/api/aas/v1`, through `appconfig` and `apiFetch`) wraps the few `aas` calls the UI needs. Views branch on the store: `general-sound.vue`, `stores/vu-meter.ts`, `stores/settings.ts`, `system-info.vue`, the filter-chain page. A new page `views/services/audio-server.vue` does the switch.

**Tech Stack:** Vue 3, TypeScript, Pinia setup stores, Vitest with happy-dom. No new dependencies.

**Repository:** `https://github.com/hifiberry/hbos-ui`, checked out beside this one at `../hbos-ui`; branch `feat/audio-server` from `dev`.

**Spec:** `aas` repository, `doc/switch.md`: "The choice", "Speaker EQ, input processor, volume limit, balance, mono", "What the user is told before switching", "The web UI and the configurator (7)". The `aas` API: `aas` `README.md`, "The API".

## Global Constraints

- Config server route (base `appConfig.getConfigApiBaseUrl()`, i.e. `/api/config/v1`): `GET /audio-server` → `{status, data: {running, chosen, available}}`; `POST /audio-server` with `{server}` → `{status, data: {running, chosen, available, reboot_required}}`, 400/409/500 with `{status: "error", message}`. Reboot: the existing `rebootSystem()`.
- The store `useAudioServerStore` (`src/stores/audio-server.ts`): `running`, `chosen`, `available` (refs, default `pipewire`/`pipewire`/`false`), `loaded`, `isAas` (computed `running === 'aas'`), `load()`, `choose(server)`. A failed load leaves the defaults, so every view behaves as under PipeWire, as today. Loaded once in the app's start-up where the settings store loads (next to the Pi-version fetch).
- `appconfig`: an `aas_api` entry with prefix `/api/aas/v1` and `getAasApiBaseUrl()`, built like `getConfigApiBaseUrl()`. `src/api/aas.ts` uses only `apiFetch` (`noRawFetch.test.ts`) and returns `T | ApiError` like `api/pipewire.ts`: `getState()` (`GET /state`), `getPlugins()` (`GET /plugins`), `setPluginParams(id: string, params: Record<string, number>)` (`PATCH /plugins/{id}` with `{params}`, `id` like `1/output/0`), `getMatrix()` / `putMatrix(matrix)` (`GET`/`PUT /matrix`, `{outputs: [{output, routes: [{from, to, gain_db?}]}]}`).
- Balance under `aas`: the first plugin of type `balance` on output 1 (from `getPlugins()`, by `name === 'balance'` and an id starting `1/`); the slider's -1..1 is its `balance` param directly. No balance plugin: the balance slider is disabled with a note.
- Channel mode under `aas`: the master's routes in the matrix. Sources `players` and, when present in the current matrix, `analog`, each mapped the same way: stereo L→1, R→2; swapped L→2, R→1; mono L→1, R→1, L→2, R→2 each `gain_db: -6`; left L→1, L→2; right R→1, R→2. Routes of further outputs (output > 1) are kept unchanged. Reading the mode back: from the `players` routes of output 1; anything else shows as stereo.
- Volume limit under `aas`: hidden (`v-if`), since `aas` has no output ceiling yet.
- VU meter under `aas`: WebSocket `/api/aas/v1/levels` (same host and scheme rules as today's URL). Each frame is meters back to back, each `[id, n, rms1, peak1, …, rmsn, peakn, clipbits]`; use meter id 1 (the master), take its first two channels into the store's existing `leftRms`, `leftPeak`, `rightRms`, `rightPeak`, clip flags (bit 0 left, bit 1 right). Under `aas` `updateVuMeterEnabled` does not call `enableService`/`disableService('vu-meter')`.
- `system-info.vue` background services under `aas`: one entry "aas" probing `GET /api/aas/v1/state` instead of the PipeWire API and VU meter entries. The filter-chain page and its route stay reachable under PipeWire; under `aas` the page shows that it applies to PipeWire only and loads nothing.
- Audio server page `views/services/audio-server.vue`, route `audio-server` under `/services`, a card in `views/services/index.vue`:
  - Shows which server runs and which is chosen for the next boot (they differ until the reboot, with a "reboot to apply" action).
  - Not `available`: says `aas` is not installed on this device; no switch.
  - Switching to `aas`: first a `ConfirmationDialog` listing what stops working on this device, only what is in use: a paired Bluetooth device (system info `bluetooth.devices` not empty); AES67 (`aes67` service enabled); the USB audio gadget (`usbaudio` service enabled); the input processor (`getRIAAEnabled()` true); a volume limit below 100 % (`getSpeakerEQMasterGain()` below 0 dB); Soloist (`soloist` service enabled). A check that fails is left out of the list, not shown as an error. Nothing in use: the dialog says nothing stops working. Confirm → `choose('aas')`; then the reboot dialog as in `system-tools.vue`.
  - Switching to PipeWire: no warning; `choose('pipewire')`, then the reboot dialog.
  - Errors from the route: an error toast with its message.
- Hard-coded English strings, as the rest of the UI. Commits: Conventional Commits `type(scope): lowercase summary`. No trailers, nothing naming tooling, no test machines.
- Checks: `npm run type-check`, `npm run test`, `npm run lint` (do not commit unrelated lint fixes; `eslint --fix` touches only changed files: run `npx eslint --fix <files>`).

## Review Focus

1. A device without `aas` (every device today): the store load gets `available: false` or fails; every view is exactly as today (Tasks 1-4).
2. `aas` runs but its API does not answer: the sound page disables its controls with a message instead of throwing; the VU meter reconnects as today (Tasks 2, 3).
3. The matrix holds analog routes and further outputs: changing the mode keeps them correct (Task 2).
4. A levels frame with the analog meter first, or a mono master: meter 1 is found by id, a mono master shows its channel on both bars (Task 3).

---

### Task 1: store, API modules, start-up

**Files:** Create `src/stores/audio-server.ts`, `src/api/aas.ts`, `src/api/audio-server.ts` (`getAudioServer()`, `setAudioServer(server)` on the config server), tests `src/api/__tests__/aas.test.ts`, `src/api/__tests__/audio-server.test.ts`, `src/stores/__tests__/audio-server.test.ts`. Modify `src/stores/appconfig.ts`, the start-up load (where `settings.load()` runs).

- [ ] Tests (pattern of `src/api/__tests__/capabilities.test.ts`): each `aas.ts` call hits the right URL and method with the right body; errors come back as `ApiError`; `getAudioServer`/`setAudioServer` likewise; the store's defaults, `load()` success, `load()` failure keeping defaults, `isAas`, `choose()` updating `chosen` and returning `reboot_required`.
- [ ] Fail, implement, pass; type-check and lint.
- [ ] Commit `feat(audio-server): know which audio server runs`.

### Task 2: sound page under `aas`

**Files:** Modify `src/views/sound/general-sound.vue`. Create `src/helpers/aas_matrix.ts` (`modeToMatrix(current, mode)`, `matrixToMode(matrix)`), test `src/helpers/__tests__/aas_matrix.test.ts`.

- [ ] Tests for the helper: each of the five modes from a matrix with only players routes, from one with analog routes (analog follows the mode), with a further output (kept as is); `matrixToMode` for each mode and an unknown shape (stereo).
- [ ] Fail, implement the helper, pass. Then branch `general-sound.vue` on `isAas`: hide the volume limit; balance through the balance plugin; mode through the matrix; an unreachable API disables those controls with a message. PipeWire path unchanged.
- [ ] Commit `feat(sound): balance and channel mode under aas`.

### Task 3: VU meter, settings, system info, filter-chain page

**Files:** Modify `src/stores/vu-meter.ts`, `src/stores/settings.ts`, `src/views/services/system-info.vue`, `src/views/services/pipewire-filter-chain.vue`. Create `src/stores/__tests__/vu-meter-aas.test.ts` (or a parser helper with its test).

- [ ] Tests: the frame parser finds meter 1 after an analog meter, handles a mono master, reads clip bits; the URL under `aas` and under PipeWire; `updateVuMeterEnabled` under `aas` makes no service call.
- [ ] Fail, implement, pass.
- [ ] Commit `feat(vu-meter): read the levels from aas when it runs`.

### Task 4: the audio server page

**Files:** Create `src/views/services/audio-server.vue`, a composable `src/composables/useAasWarnings.ts` (collects the in-use list) with test `src/composables/__tests__/useAasWarnings.test.ts`. Modify `src/router/index.ts`, `src/views/services/index.vue`.

- [ ] Tests for the composable: each item appears only when in use; a failing check is left out; nothing in use gives an empty list.
- [ ] Fail, implement, pass. Then the page and the route and card, copying the dialog and reboot pattern of `system-tools.vue`.
- [ ] `npm run build` passes (type-check and bundle).
- [ ] Commit `feat(audio-server): switch between PipeWire and aas`.
