# Stage 4, part 2: metrics, the graph, plugins, settings, levels and `aasctl`

> The design, `doc/design.md`, is the authority: where this plan disagrees with it, the
> design wins.

**Goal:** The API built in part 1 also serves `GET /metrics`, `GET /graph` (JSON or
DOT), `/plugins` (list, set parameters, enable and disable at run time, kept in the
state file), `/settings` (bit-perfect mode, latency, default rate, until restart) and
WebSocket `/levels`. A command line tool `aasctl` drives all of it, and `dist/` gets
the `hifiberry-auth` manifest, the nginx location and a script that installs both.

**Architecture:** As in part 1, the API thread decides nothing: every call but
`/levels` is a `Call` to the control thread, which reads counters the audio and
writer threads publish without locks, and sends engine commands. `/levels` is read
by the API thread directly from a `Levels` block of atomics, written by the audio
thread with a sequence counter per meter. Plugin timing, neutrality and latency are
published per plugin through `PluginStats` atomics that the control thread creates
once per plugin and hands to every chain built for it.

**Tech Stack:** Rust (edition 2024, `rust-version` 1.85). One new dependency:
`tungstenite` 0.29 (already in `Cargo.lock` through `tokio-tungstenite`), default
features off except `handshake`, for `aasctl`. No HTTP client crate, no `clap`.

**Spec:** `doc/design.md`, "The API in part 2", and the sections it builds on: "The
API in part 1", API, Plugins, Plugin libraries, Graph export, Monitoring and
reporting, VU meter, Bit-perfect, Latency.

## Stages

1. Measurement (done)
2. Core daemon and ALSA plugin (done)
3. a. Analog input and native plugins (done)
   b. Matrix, channels and several outputs (done)
   c. `input-processor` as a native plugin
4. API, monitoring, graph export
   - Part 1: outputs, inputs, the matrix, the state file, `/events` (done)
   - **Part 2: `/metrics`, `/graph`, `/plugins`, `/settings`, `/levels`, `aasctl`,
     the auth manifest and nginx (this plan)**
   - Part 3: a matrix row and a chain per client
   - Part 4: the clock master at run time, after its own design
5. Packaging, Bluetooth and the switch from PipeWire, with all players tested

## Decisions this plan makes

Beyond the design's *(call)*s:

- `Command::SetParam`'s `index` becomes `u16`: a plugin may have up to 4096
  parameters, and `u8` reaches only 256.
- Plugin ids in code: `PluginId { output: usize, place: Place, index: usize }`,
  `output` as in the API (master 1), shown as `"1/mix/0"`.
- The control thread keeps one `PluginEntry` per plugin id (below), filled when the
  chains are first built at startup, and is the only owner of a plugin's current
  values; chains built later take them from there.
- `ApiEvent` gains `Plugin { id, enabled, params }` and `Settings { bit_perfect,
  latency_ms, default_rate }`.
- The meter for output N is `Levels::meters[N - 1]`, the analog input's
  `meters[4]`; on the wire 1 to 4 and 16. A meter with 0 channels is not configured
  and is left out of every frame.
- `aasctl` lives in `src/bin/aasctl/` (`main.rs`, `args.rs`, `client.rs`), a
  `[[bin]]` of the root crate. The integration tests' daemon harness moves from
  `tests/api.rs` to `tests/common/mod.rs` so `tests/aasctl.rs` shares it.

## Global Constraints

- Nothing on the audio thread or a writer thread allocates, locks or blocks. Every
  new engine path has an `assert_no_alloc` test as the existing ones do.
- The API thread never calls into `Control` and never sends engine commands; it only
  reads `Levels`.
- Out of range is 422, never clamped. Error bodies `{"error": "..."}`, worded as the
  config's errors are.
- Fades 10 ms. Levels windows 100 ms of the meter's own frames. A meter whose newest
  window ended more than 200 ms ago reads as silence.
- Level bytes: `0` at -60 dBFS or less (and for silence), `255` at 0 dBFS or more,
  `round((dB + 60) / 60 * 255)` between.
- `/settings` changes are not stored and never rewrite the config. A `latency_ms`
  above the startup latency is 422 `"latency_ms above the <N> ms the daemon started
  with needs a restart"`.
- A text parameter in a `PATCH` is 422 `"<name> is a text parameter; it changes only
  in the config"`. Disabling `detect` or a plugin whose counts differ is 422.
- Auth manifest tiers: every `GET`, `PATCH /outputs/*`, `PATCH /inputs/**`, `PUT
  /matrix`, `PATCH /plugins/**` are `ok`; `PATCH /settings` and anything else is
  `risky`.
- `cargo build` without `alsa-output` still works; `cargo test -p aas` and
  `cargo clippy -p aas --all-targets` stay clean.
- Public text (commits, docs) names no machine, address or tool. The test boards are
  "the CM5 test board" and "the Pi 3 test board".

## Review Focus

1. A plugin parameter set through the API survives a rate change: the chain built
   for the new rate has the API's value, not the config's.
2. A `/levels` subscriber while the master is closed for idle sees zeros, not the
   last level, and frames keep coming at 10 Hz.
3. A `PATCH /settings` lowering `latency_ms` while a client plays: the card reopens
   at the shorter period and the client keeps playing without an underrun count
   beyond the reopen's.
4. A plugin disabled and enabled again does not play its old tail: `reset` runs
   before the fade in.
5. A config edit of a plugin parameter after an API change wins at the next start;
   a plugin moved to another position loses its stored values rather than giving
   them to the plugin now in its place.

Each has a test in the task that owns the code: 1, 4 and 5 in Task 3, 2 in Task 6,
3 in Task 5.

---

### Task 1: the chain times its plugins and can disable one

**Files:**
- Modify: `src/counters.rs` (`PluginStats`), `src/plugin/chain.rs`, `src/engine.rs`
  (`Command::SetParam` index, `Command::SetEnabled`)

**Interfaces:**
- Produces, in `src/counters.rs`:

```rust
#[derive(Debug, Default)]
pub struct PluginStats {
    pub runs: AtomicU64,          // blocks the plugin processed
    pub total_ns: AtomicU64,
    pub worst_ns: AtomicU64,
    pub neutral: AtomicBool,      // written whenever it may have changed
    pub latency_frames: AtomicU32,
}
```

- In `src/plugin/chain.rs`: `pub fn with_stats(self, stats: Vec<Arc<PluginStats>>) ->
  Chain` (one per plugin, in order; panics on another length; done on the control
  thread), `pub fn set_enabled(&mut self, plugin: usize, enabled: bool)` (ignored for
  an unknown index or a plugin whose counts differ), `pub fn enabled(&self, plugin:
  usize) -> bool`. `neutral()` is true when every enabled plugin is neutral and no
  fade runs; `latency_frames()` counts enabled plugins only.
- In `src/engine.rs`: `Command::SetParam { target, plugin: u8, index: u16, value }`,
  `Command::SetEnabled { target: Target, plugin: u8, enabled: bool }`.

- [ ] **Step 1: Write the failing tests** in `src/plugin/chain.rs`:
  - `stats_count_each_block`: a chain of two `gain` test plugins with stats; 10 blocks
    → each `runs == 10`, `worst_ns >= total_ns / 10`, `total_ns > 0`.
  - `a_disabled_plugin_fades_out_over_10_ms`: at 48 kHz, a plugin that multiplies by
    0.5 on a constant 1.0 input; `set_enabled(0, false)`; frame 0 after it is 0.5,
    frame 480 is 1.0 within 1e-6, no step between frames above `0.5 / 480 + 1e-6`,
    and later blocks are passed through exactly.
  - `enabling_resets_first`: a delay-line test plugin holding old samples, disabled,
    then fed other audio, then enabled: no old sample appears in the output.
  - `a_disabled_plugin_is_neutral`: a non-neutral plugin disabled → `neutral()` true
    once the fade has ended, false during it.
  - `stats_publish_neutral_and_latency`: after `set_param` that makes a plugin
    non-neutral, its `PluginStats::neutral` is false.
  - `set_enabled_does_not_allocate`, `timed_processing_does_not_allocate` under
    `assert_no_alloc`.
  - In `src/engine.rs`: `set_enabled_reaches_the_chain` with the rig the `SetParam`
    tests use, and the existing `SetParam` tests updated to `index: u16`.
- [ ] **Step 2: Run** `cargo test -p aas --lib chain` — fails to compile.
- [ ] **Step 3: Implement.** Time each plugin with `counters::monotonic_ns()` around
  its `process`/`process_into`. The crossfade keeps a copy of the plugin's input in a
  `dry` `BlockStore` allocated in `Chain::new` when the chain holds a plugin (sized
  as `store`), and mixes `dry * (1 - g) + wet * g` per frame, `g` ramping over
  `rate / 100` frames (480 when the chain has no rate). A plugin whose counts differ
  is never faded.
- [ ] **Step 4: Run** `cargo test -p aas` and `cargo clippy -p aas --all-targets` — clean.
- [ ] **Step 5: Commit** `feat: a chain times its plugins and fades one out or in`

### Task 2: `GET /metrics`

**Files:**
- Modify: `src/counters.rs` (`Counters::cycle_ns_total`, `OutputCounters::ratio_bits`),
  `src/engine.rs` (`record_timing`), `src/further.rs` (`report`), `src/server.rs`
  (hand the output counters to `Control`), `src/control.rs`, `src/control/api.rs`,
  `src/api.rs` (`Call::Metrics`), `src/http.rs` (route), `tests/api.rs`

**Interfaces:**
- Consumes: `PluginStats` (Task 1); the control thread holds one per plugin id from
  Task 3, so this task reports plugins as an empty list until Task 3 fills it.
- Produces: `Counters::cycle_ns_total: AtomicU64` (sum of cycle times);
  `OutputCounters::ratio_bits: AtomicU64` (`f64::to_bits` of the writer's ratio,
  card frames per master frame; `0` before the first); `Control::set_output_counters(
  Vec<Arc<OutputCounters>>)`, index = engine output index - 1; `Call::Metrics`.
- JSON of `GET /metrics`:

```json
{"master": {"cycles": 0, "cycle_ns_avg": 0, "cycle_ns_worst": 0, "buffer_ns": 0,
            "xruns": 0, "wake_gaps": 0, "worst_gap_ns": 0,
            "last_gap": {"ns": 0, "at_ns": 0} | null,
            "period_frames": 0, "card_frames": 0, "delay_frames": 0},
 "outputs": [{"id": 2, "card": "...", "xruns": 0, "wake_gaps": 0, "worst_gap_ns": 0,
              "starves": 0, "overflows": 0, "skipped": 0, "ppm": 0.0,
              "locked": false, "ratio": 1.0 | null, "period_frames": 0}],
 "streams": [{"name": "mpd", "rate": 44100, "resampled": true, "underruns": 0,
              "fill_frames": 0, "capacity_frames": 0, "latency_frames": 0}],
 "analog": {"open": false, "padded_frames": 0, "dropped_frames": 0,
            "latency_frames": 0} | null,
 "plugins": [{"id": "1/mix/0", "runs": 0, "ns_avg": 0, "ns_worst": 0}]}
```

- [ ] **Step 1: Write the failing tests:**
  - `src/counters.rs`: `the_average_comes_from_the_sum` (`on_cycle` twice with 100 and
    300 ns → `cycle_ns_total == 400`, `cycles == 2`).
  - `src/further.rs`: `the_writer_publishes_its_ratio` with the writer test rig: after
    a report, `f64::from_bits(ratio_bits)` equals the writer's ratio.
  - `src/control/api.rs`: `metrics_list_every_output_and_stream` with the part 1 test
    helpers: two outputs, one playing client named `mpd` → `outputs[0].id == 2`,
    `streams[0].name == "mpd"`, `analog == null` without an analog input.
  - `tests/api.rs`: `get_metrics_counts_cycles`: while a client plays, two reads
    100 ms apart show `master.cycles` growing and `cycle_ns_avg <= cycle_ns_worst`.
- [ ] **Step 2: Run** `cargo test -p aas metrics` — fails.
- [ ] **Step 3: Implement.** `cycle_ns_avg` is `cycle_ns_total / cycles` (0 for none).
  `ratio` is null while `ratio_bits` is 0. A stream's fill is `write_pos - read_pos`
  from its `HeaderView`; recorders are left out.
- [ ] **Step 4: Run** `cargo test -p aas` and clippy — clean.
- [ ] **Step 5: Commit** `feat: GET /metrics`

### Task 3: plugins at run time, kept in the state file

**Files:**
- Create: `src/control/plugins.rs` (child module of `control`, as `api.rs` is)
- Modify: `src/control.rs` (chain building takes values and stats from the entries),
  `src/state.rs`, `src/api.rs`, `src/http.rs`, `tests/api.rs`, `README.md`

**Interfaces:**
- Consumes: `Chain::with_stats`, `Chain::set_enabled`, `Command::SetEnabled`,
  `Command::SetParam` (Task 1).
- Produces:

```rust
// src/api.rs
pub struct PluginId { pub output: usize, pub place: Place, pub index: usize }
impl PluginId { pub fn parse(output: &str, place: &str, index: &str) -> Result<PluginId, ApiError>; }
impl std::fmt::Display for PluginId { /* "1/mix/0" */ }
#[derive(Deserialize)] #[serde(deny_unknown_fields)]
pub struct PluginPatch { pub params: Option<BTreeMap<String, f64>>, pub enabled: Option<bool> }
// Call::Plugins, Call::Plugin(PluginId), Call::PatchPlugin(PluginId, PluginPatch)
// ApiEvent::Plugin { id: String, enabled: bool, params: BTreeMap<String, f64> }

// src/control/plugins.rs
pub(super) struct PluginEntry {
    pub id: PluginId,
    pub config: PluginConfig,          // its params hold the current values
    pub name: String,                  // the descriptor's, or the built-in's
    pub params: Vec<Param>,
    pub enabled: bool,
    pub stats: Arc<PluginStats>,
}

// src/state.rs, in StoredOutput:
#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
pub plugins: BTreeMap<String, StoredPlugin>,      // key "mix/0"
pub struct StoredPlugin {
    pub kind: String, pub path: Option<String>,   // must match the config's plugin
    pub params: BTreeMap<String, Stored<f64>>,    // config = the config's value or the default
    pub enabled: Option<bool>,
}
```

- Routes: `GET /plugins`, `GET` and `PATCH /plugins/{output}/{place}/{index}`. A
  plugin's JSON: `{"id", "output", "place", "index", "type", "path", "name",
  "enabled", "neutral", "latency_frames", "params": [{"name", "unit", "type":
  "float"|"integer"|"switch"|"enumeration"|"text", "min", "max", "default", "values"?,
  "value"}]}`; a text parameter's `value` is its config text.

- [ ] **Step 1: Write the failing tests:**
  - `src/api.rs`: `plugin_ids_parse_and_print` (`"1","mix","0"` ↔ `"1/mix/0"`;
    `"0","mix","0"`, `"1","nowhere","0"`, `"1","mix","x"` are 404).
  - `src/control/plugins.rs` with the part 1 test helpers and a config holding
    `balance` at `output` and `gain` (the test library) at `mix`:
    - `list_gives_every_plugin_in_order` (ids, `type`, params with ranges and value).
    - `a_value_goes_to_the_engine_and_the_entry`: `PatchPlugin("1/output/0",
      {"params": {"balance": 0.5}})` → a `SetParam` with that value, and `GET` gives 0.5.
    - `a_value_out_of_range_is_422_and_not_sent`; `an_unknown_parameter_is_422`;
      `a_fraction_for_an_integer_is_422`; `a_text_parameter_is_422`.
    - `disabling_detect_is_422`; `disabling_sends_set_enabled`.
    - `a_value_survives_a_rate_change` (Review Focus 1): set a value, force a rate
      change in the test engine, the `SetChain` for the new rate carries a chain whose
      plugin has that value (read through the test plugin's recorded `set_param`).
    - `the_value_is_set_in_ready_chains`: a chain held ready for another rate gets the
      value too.
    - `a_patch_publishes_a_plugin_event`.
  - `src/state.rs`:
    - `stored_plugin_values_round_trip`.
    - `a_config_edit_of_a_parameter_wins` (Review Focus 5): stored `{value: 0.5,
      config: 0.0}`, config now 0.2 → 0.2.
    - `a_plugin_in_another_place_does_not_take_the_values` (Review Focus 5): stored
      under `"mix/0"` with kind `library` and path A; the config now has path B at
      `mix/0` → the stored values are not applied, and stay in the file.
  - `src/plugin/chain.rs` (Review Focus 4 is covered by `enabling_resets_first`, Task 1).
  - `tests/api.rs`: `patch_a_plugin_and_read_it_back` with a `balance` on the master.
- [ ] **Step 2: Run** `cargo test -p aas plugin` — fails.
- [ ] **Step 3: Implement.** Entries are built from each output's `[[plugin]]` /
  `[[output.plugin]]` tables at startup, with values resolved from the state file
  before the first chain is built, so a stored value is in place from the first
  cycle. Every chain the control thread builds gets the entries' values and stats.
  A `PATCH` checks every value first and applies none if one fails. It sends one
  `SetParam` per value and a `SetEnabled` if `enabled` changed, for the output's
  live chain, sets the same values in every chain held ready or spare, updates the
  entry, stores it with `save_later`, and publishes `ApiEvent::Plugin`.
- [ ] **Step 4: Run** `cargo test -p aas` and clippy — clean. README: a `/plugins`
  paragraph in the API section.
- [ ] **Step 5: Commit** `feat: plugins listed and changed through the API, and kept`

### Task 4: `GET /graph`

**Files:**
- Create: `src/control/graph.rs`
- Modify: `src/control.rs` (`mod graph;`), `src/api.rs` (`Call::Graph`), `src/http.rs`,
  `tests/api.rs`

**Interfaces:**
- Consumes: `PluginEntry` (Task 3), the part 1 views.
- Produces: `Call::Graph`; `pub(super) fn graph(&self) -> serde_json::Value` and
  `pub fn to_dot(graph: &serde_json::Value) -> String` (in `src/control/graph.rs`,
  public through `crate::control::to_dot` for the HTTP thread, which converts after
  the answer so DOT costs the control thread nothing).
- JSON: `{"nodes": [{"id", "kind": "client"|"analog"|"bus"|"matrix"|"volume"|"plugin"
  |"delay"|"card", "label", "rate"?, "channels"?, "format"?, "latency_frames"?,
  "plugin"?: {"id", "ns_avg", "converts_to"?}}], "edges": [{"from", "to"}]}`. Node ids
  are stable strings: `client/mpd`, `analog`, `bus/players`, `bus/analog`,
  `out/1/matrix`, `out/1/volume`, `out/1/mix/0`, `out/1/delay`, `out/1/card`.

- [ ] **Step 1: Write the failing tests** in `src/control/graph.rs`:
  - `the_master_alone`: one client `mpd`, no plugins → edges `client/mpd → bus/players
    → out/1/matrix → out/1/volume → out/1/delay → out/1/card`.
  - `plugins_in_chain_order`: `mix` then `output` plugins appear between volume and
    delay in config order.
  - `the_analog_chain_comes_before_its_bus`.
  - `a_further_output_has_no_delay_node`.
  - `dot_is_a_digraph`: `to_dot` starts with `digraph aas {`, has one `->` per edge and
    quotes every id.
  - `tests/api.rs`: `graph_as_dot` (`GET /graph?format=dot` is 200 with content type
    `text/vnd.graphviz`), and `?format=svg` is 400.
- [ ] **Step 2: Run** `cargo test -p aas graph` — fails.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p aas` and clippy — clean.
- [ ] **Step 5: Commit** `feat: GET /graph, as JSON or DOT`

### Task 5: `/settings`

**Files:**
- Modify: `src/engine.rs` (`Command::SetMode`), `src/rate.rs` (`RatePolicy::set_base`),
  `src/control.rs`, `src/control/api.rs`, `src/api.rs`, `src/http.rs`, `tests/api.rs`,
  `README.md`

**Interfaces:**
- Produces: `Command::SetMode(BitPerfect)`; `RatePolicy::set_base(&mut self, rate:
  u32)`; `Call::Settings`, `Call::PatchSettings(SettingsPatch)` with
  `SettingsPatch { bit_perfect: Option<String>, latency_ms: Option<f32>,
  default_rate: Option<u32> }` (deny unknown fields; `format` or `resampling` in a
  body is 422 `"<key> changes only in the config"`); `ApiEvent::Settings {
  bit_perfect: String, latency_ms: f32, default_rate: u32 }`. `GET /settings`:
  `{"bit_perfect", "latency_ms", "default_rate", "format", "resampling"}`.

- [ ] **Step 1: Write the failing tests:**
  - `src/engine.rs`: `set_mode_off_stops_the_copy_path`: one 0 dB source, auto →
    bit-perfect; `SetMode(Off)` → next cycle not bit-perfect with reason
    `"disabled"`; under `assert_no_alloc`.
  - `src/rate.rs`: `a_new_base_applies_with_no_client`.
  - `src/control/api.rs`:
    - `forced_with_a_chain_that_changes_channels_is_422`.
    - `a_latency_above_the_startup_one_is_422` (started at 10 ms; 12 → 422 with the
      Global Constraints message; 5 → ok).
    - `a_lower_latency_reopens_with_the_shorter_period`: a `Reconfigure` with
      `period_frames(rate, 5.0)` follows.
    - `a_latency_change_waits_during_dop`.
    - `a_default_rate_the_config_refuses_is_422` (`12345`).
    - `settings_are_not_stored`: after a change, the saved state has no settings.
    - `a_change_publishes_a_settings_event`.
  - `tests/api.rs`: `lowering_the_latency_while_playing` (Review Focus 3): a client
    plays; `PATCH /settings {"latency_ms": <half the default>}` is 200; `GET
    /metrics` shows the shorter `master.period_frames`; the client's underruns grow
    by at most 2 over the next 500 ms.
- [ ] **Step 2: Run** `cargo test -p aas settings` — fails.
- [ ] **Step 3: Implement.** The latency change goes through the path a rate change
  takes (reopen of every output, alignment planned again); outputs with their own
  `latency_ms` keep it. The control thread keeps the startup latency to check against.
- [ ] **Step 4: Run** `cargo test -p aas` and clippy — clean. README: `/settings`.
- [ ] **Step 5: Commit** `feat: /settings for bit-perfect mode, latency and default rate`

### Task 6: levels and WebSocket `/levels`

**Files:**
- Create: `src/levels.rs`
- Modify: `src/lib.rs`, `src/engine.rs` (metering the master, further outputs and the
  analog input), `src/server.rs` (create `Levels`, pass it to the engine and to
  `http::spawn`), `src/http.rs` (`/levels`), `tests/api.rs`, `README.md`

**Interfaces:**
- Produces, in `src/levels.rs`:

```rust
pub const METERS: usize = 5;            // outputs 1..4, then the analog input
pub const ANALOG: usize = 4;
pub struct Levels { pub meters: [Meter; METERS] }
pub struct Meter { /* channels: AtomicU8, seq: AtomicU64, end_ns: AtomicU64,
                      rms: [AtomicU32; 8], peak: [AtomicU32; 8], clipped: AtomicU8 */ }
impl Levels { pub fn new(channels: [u8; METERS]) -> Levels; }   // 0 = no such meter
/// The audio thread's side of one meter: sums over a window, then publishes.
pub struct Metering { /* sum of squares and peak per channel, frames, window */ }
impl Metering {
    pub fn new(channels: usize, rate: u32) -> Metering;       // window = rate / 10
    pub fn set_rate(&mut self, rate: u32);                    // restarts the window
    pub fn add<S: Sample>(&mut self, interleaved: &[S], meter: &Meter, now_ns: u64);
    pub fn silence(&mut self, frames: usize, meter: &Meter, now_ns: u64);  // DoP
}
/// One `/levels` frame, read at `now_ns`.
pub fn frame(levels: &Levels, now_ns: u64) -> Vec<u8>;
pub fn level_byte(linear: f32) -> u8;
```

  `Sample` is whatever trait the engine's formats share (`i32`, `f32`, `f64`); a
  clip is `|x| >= 1.0` for floats and `i32::MAX`/`i32::MIN` for `i32`. Publishing is a
  seqlock: `seq` odd while the fields are written, even after, `Release`; `frame`
  retries a meter whose `seq` is odd or changed while read.

- [ ] **Step 1: Write the failing tests** in `src/levels.rs`:
  - `level_bytes`: 1.0 → 255, 0.5 (-6.02 dB) → 229, 0.001 (-60 dB) → 0, 0.0 → 0.
  - `a_full_scale_sine_window`: 4800 frames of a 0 dBFS 1 kHz sine at 48 kHz in
    blocks of 128 → RMS byte 242 ± 1 (-3.01 dB), peak 255, no clip.
  - `a_clip_is_seen_in_every_format` (`i32::MAX`, `1.0f32`, `-1.0f64`).
  - `a_stale_meter_reads_as_silence` (Review Focus 2): a window published at t;
    `frame` at t + 150 ms has the level; at t + 250 ms its bytes are 0.
  - `the_frame_layout`: meters 1 (2 channels) and analog (2 channels) →
    `[1, 2, r, p, r, p, clip, 16, 2, r, p, r, p, clip]`; unconfigured meters absent.
  - `metering_does_not_allocate` under `assert_no_alloc`.
  - `src/engine.rs`: `the_master_is_metered_after_its_chains` (a `gain` plugin at
    -6 dB on the `output` chain lowers the master's RMS by 6 dB ± 0.25);
    `dop_meters_silence`.
  - `tests/api.rs`: `levels_come_every_100_ms`: 10 binary frames in 1.0 ± 0.2 s, each
    starting with meter id 1; and `an_idle_master_reads_zeros` (Review Focus 2).
- [ ] **Step 2: Run** `cargo test -p aas levels` — fails.
- [ ] **Step 3: Implement.** The master is metered on what goes to the card after its
  chains, before the conversion to the card's format; each further output on its
  `further_samples`; the analog input in `Analog::read`, before its chain. The
  `/levels` handler sends `frame(...)` on a `tokio::time::interval` of 100 ms with
  `MissedTickBehavior::Skip`, ignores what the peer sends (4 KiB limit as on
  `/events`) and pings as `/events` does.
- [ ] **Step 4: Run** `cargo test -p aas` and clippy — clean. README: `/levels` with
  the frame layout.
- [ ] **Step 5: Commit** `feat: levels measured on the audio thread, sent on /levels`

### Task 7: `aasctl`

**Files:**
- Create: `src/bin/aasctl/main.rs`, `src/bin/aasctl/args.rs`, `src/bin/aasctl/client.rs`,
  `tests/common/mod.rs` (the daemon harness moved from `tests/api.rs`), `tests/aasctl.rs`
- Modify: `Cargo.toml` (`[[bin]] name = "aasctl"`, `tungstenite`), `tests/api.rs`,
  `README.md`

**Interfaces:**
- Produces: the commands in the design's "The command line tool", parsed into

```rust
pub enum Command {
    Get(&'static str),                       // state, metrics, outputs, inputs, matrix, plugins, settings
    Graph { dot: bool },
    Patch { path: String, body: serde_json::Value },
    Put { path: String, body: serde_json::Value },   // matrix put FILE
    Events, Levels,
}
pub struct Args { pub port: u16, pub command: Command }
pub fn parse(args: impl IntoIterator<Item = OsString>, config_port: impl Fn(&Path) -> Result<u16, String>) -> Result<Args, String>;
```

  `client.rs`: `pub fn request(port: u16, method: &str, path: &str, body: Option<&str>)
  -> io::Result<(u16, String)>` over `std::net::TcpStream`, `Connection: close`.
  Exit codes: 0, 1 for an error answer or no daemon (message on stderr), 2 for usage.

- [ ] **Step 1: Write the failing tests:**
  - `args.rs`: `output_volume` (`output 1 volume 60` → `Patch { "/outputs/1",
    {"volume": 60} }`), `client_mute` (`client mpd mute` → path
    `/inputs/clients/mpd`, `{"muted": true}`; a name with a space is
    percent-encoded), `plugin_set` (`plugin 1/mix/0 set gain_db=-3 bypass=1` →
    `/plugins/1/mix/0`, `{"params": {"gain_db": -3.0, "bypass": 1.0}}`),
    `settings_values` (`settings latency_ms=5 bit_perfect=off`), `port_from_config`,
    `port_flag_wins`, `unknown_command_is_a_usage_error`, `a_value_that_is_not_a_number`.
  - `tests/aasctl.rs` against the harness daemon: `state_prints_json` (exit 0, stdout
    parses, has `rate`), `an_error_answer_exits_1` (`output 7 volume 10` → exit 1,
    stderr has the API's message), `no_daemon_exits_1`, `events_prints_a_line` (start
    `aasctl events`, change a volume, one JSON line with `"type":"output"` appears).
- [ ] **Step 2: Run** `cargo test -p aas --bin aasctl` and `--test aasctl` — fail.
- [ ] **Step 3: Implement.** `levels` prints one line per frame: per meter `id:` and per
  channel RMS/peak in dBFS with one decimal, `-inf` below -60, `CLIP` when clipped.
- [ ] **Step 4: Run** `cargo test -p aas` and clippy — clean. README: an `aasctl`
  section.
- [ ] **Step 5: Commit** `feat: aasctl, the command line tool`

### Task 8: the auth manifest, nginx, and the boards

**Files:**
- Create: `dist/auth.d/aas.json`, `dist/nginx/hifiberry-aas.nginx`, `dist/install-api.sh`
- Modify: `README.md`, `doc/measurements.md`, `TODO.md`, `doc/design.md` (only if the
  build departed from it)

- [ ] **Step 1: Write** the manifest (`"service": "aas"`, `"match_prefix":
  "/api/aas/v1"`, `"default_tier": "risky"`, an `ok` rule for `GET` on `/**`, one for
  `PATCH` on `/outputs/*`, `/inputs/**`, `/plugins/**`, one for `PUT` on `/matrix`,
  then `risky` for `*` on `/**`) and the nginx file (the `vu-meter` service's
  location block with `location /api/aas/v1/` and `proxy_pass http://127.0.0.1:2718/`).
  A test in `tests/api.rs`, `the_manifest_marks_only_settings_risky`, parses the JSON
  and checks each route of the API against its rules with the matching rules of the
  security model (`*` one segment, `**` the rest, first match wins).
- [ ] **Step 2: Write** `dist/install-api.sh`: installs both files (root, 0644),
  runs `nginx -t` and reloads nginx if it is active, restarts `hifiberry-auth` if it
  is active, and warns when `/etc/aas.conf` sets an `api_port` other than 2718. It
  runs with `sh -n` clean.
- [ ] **Step 3: On the CM5 test board and the Pi 3 test board**, build with
  `alsa-output`, and run 10 minutes as part 1's measurement did, with a `/levels`
  subscriber, a `GET /metrics` every second and a `balance` parameter change every
  second. Record xruns, wake gaps and worst gap against part 1's runs, and the
  per-plugin `ns_avg` and `ns_worst` from `/metrics`. On the CM5 loopback capture,
  check a `PATCH /plugins` enable/disable for a click as part 1 checked volume.
  Lower `latency_ms` through `/settings` while playing and record what the client
  sees.
- [ ] **Step 4: Docs.** `doc/measurements.md`: "Stage 4 part 2: metering, timing and
  plugins under load". README: install of the API files. TODO.md: delete "Stage 4
  part 2: the rest of the API", add what is left (the web UI, the Debian package and
  removing `pw-api` and `vu-meter` go under stage 5's entry if one exists, else a new
  entry).
- [ ] **Step 5: Commit** `docs: part 2 of the API on the test boards` (and the dist
  files as `feat: the auth manifest, the nginx location and their install`).
