# Stage 3a: analog input and native plugins implementation plan

> The design, `doc/design.md`, is the authority: where this plan disagrees with it, the
> design wins.

**Goal:** The card's ADC plays through `aas` as a built-in source that opens only when
there is signal, plugins process audio at three fixed places, and other programs can
record the analog input from an ALSA capture device.

**Architecture:** A plugin is a Rust trait inside the daemon. Built-in plugins implement
it directly, and a shared library is wrapped by an adapter over a small C ABI. Plugins
sit in ordered chains that the control thread builds and hands to the audio thread, the
same way resamplers travel today. The analog input is not a client: the control thread
opens the capture PCM and hands it to the engine, which reads one period per cycle, runs
the input chain and mixes the result. The detection plugin decides whether the input
counts as a client.

**Tech Stack:** Rust (edition 2024, `rust-version` 1.85), `alsa` 0.12 behind the
`alsa-output` feature, `libc` (`dlopen`), `rtrb`, `serde` with `toml`. No new
dependencies.

**Spec:** `doc/design.md`, sections Analog input, Plugins, Song recognition, Sample rate
rules, Bit-perfect, Latency, Failure behaviour. Measurements: `doc/measurements.md`.

## Stages

1. Measurement (done)
2. Core daemon and ALSA plugin (done)
3. a. Analog input and native plugins (this plan)
   b. Matrix, multiple outputs, adaptive resampling
   c. `input-processor` as a native plugin
4. API, monitoring, graph export
5. Packaging, Bluetooth and the switch from PipeWire, with all players tested

Stage 3 is split because its parts work on their own. 3b gets its plan when 3a has
finished. 3c needs the source of today's LADSPA plugin and only the ABI from this plan.

## Scope

In this stage: the plugin trait, chains at the three places, the C ABI and loader, the
`detect` and `balance` plugins, an example external plugin, the card capability table,
capture from the ADC, the analog input as a source under the rate and bit-perfect rules,
its volume, the ALSA capture device, and the measurements on both boards.

Not in this stage, and nothing here may block them:

| Later work | Stage |
|---|---|
| Matrix, several outputs, a chain per output, plugins with other channel counts | 3b |
| `input-processor` (RIAA, subsonic, declick, notch) | 3c |
| The limiter plugin | 3b or later |
| Chains on a single client's input (needs clients known by name) | 4 |
| Changing parameters at runtime, saving them, `/plugins`, `/levels`, per-plugin timing | 4 |

## Decisions this plan makes

These are not in the design. Task 12 adds them to it, marked *(call)*.

- **The analog input is off unless the config turns it on** (`[analog] enabled = true`).
  It keeps the card open all the time, which changes the idle behaviour, so it is not a
  default. The owner should confirm this one.
- **With the analog input on, the output never closes when idle.** The engine's cycle is
  paced by the playback card, and detection has to keep measuring. `idle = "off"` is
  then ignored, with one log line at startup.
- **The control thread opens the capture PCM and hands it over.** Opening a PCM takes
  milliseconds and must not happen on the audio thread during playback. The engine
  closes the capture before it reopens the output, because these cards refuse two rates
  at once, and the control thread opens a new one after `Event::Reconfigured`.
- **A capture that fails never fails a cycle.** The input goes quiet, the control thread
  is told, and it tries again every second.
- **The engine keeps at most two periods of captured audio.** It reads one period per
  cycle. A shortfall is padded with zeros at the front and anything over two periods is
  dropped, and both are counted. The ADC then adds at most one period to the two the
  output has.
- **Capture exists only on cards in the table.** `[analog] enabled = true` on any other
  card logs one line and opens nothing. The table starts with the two cards the test
  boards carry and grows as cards are verified.
- **The detector is always in the analog chain.** If the config lists no `detect` entry
  it goes last, so it sees the signal after every other input plugin. There is no way to
  run the analog input without it.
- **Detection defaults:** threshold -50 dBFS, hold 3 s, level measured as RMS over
  50 ms. It opens as soon as one 50 ms window is above the threshold.
- **Chains exist at `analog`, `mix` and `output`.** With one output the mix chain runs
  first and the output chain after it. Both run after the output volume, which stays
  where it is, in the mix.
- **Plugins see stereo blocks in the processing format.** Float blocks are scaled so
  full scale is 1.0 and are not clipped between plugins. An `i32` block is clipped
  before the first plugin. Where a plugin does not take the configured format the
  chain converts to `f64`, else `f32`, else `i32`, and back.
- **Neutral is a property of a plugin's parameters, not of the audio.** The engine asks
  before the cycle, so the bit-perfect path can skip the chains.
- **A plugin that fails to load stops the daemon at startup,** like any other config
  error. Nothing is skipped without a word.
- **External plugins use a C ABI, version 1,** found through one symbol, `aas_plugin`.
  A rate change builds a new instance on the control thread.
- **The capture device runs at the output rate only.** `plug` converts in the
  recorder's process. When the output rate changes, an open capture stream ends with
  `ENODEV` and the recorder reopens. The rate cannot change while the analog input is
  active, because the input is always at the output rate, so only silence is cut.
- **The capture device is `pcm.aas` opened for capture,** and `default` covers both
  directions.

## Global Constraints

- The audio thread does not allocate, take locks or make blocking calls during playback.
  Chains, plugin instances and capture PCMs are built on the control thread.
- A client can always connect. A stalled or crashed client, player or recorder, loses
  only its own audio.
- Routing changes only through the config. Nothing rewires a chain on its own.
- The ADC is never resampled and never sets the rate. It starts at the rate the card is
  at.
- Below the threshold the analog input sends no audio at all, does not count as a
  client, does not affect the rate rules and does not block bit-perfect.
- The capture device never delivers the output mix.
- Recursive filters keep state and coefficients in 64-bit float in every format.
- Audio is never dithered. Float conversion scales by an exact power of two.
- A wake-up gap longer than the buffer counts as a failure even with 0 xruns.
- The workstation has no ALSA headers. Anything linking ALSA builds on a test board.
- Public text names no internal hosts or addresses.

## Review Focus

1. The level sits at the threshold, or a record has a quiet passage. The input must not
   cut in and out. Task 3, `a_level_that_dips_within_the_hold_time_stays_open`.
2. The capture stops delivering, overruns or the card goes away. Players must keep
   playing. Task 6, `a_capture_that_fails_costs_only_the_analog_input`.
3. A player at another rate makes the output change rate while the capture is open. The
   reopen must not fail. Task 6, `the_capture_is_closed_before_the_output_reopens`.
4. A recorder opens the capture device and stops reading. The audio thread must not
   wait for it. Task 9, `a_recorder_that_stops_reading_loses_only_its_own_audio`.
5. A shared library with the wrong ABI version, no symbol, or a plugin that returns
   NaN. The daemon refuses the first two at startup with the path in the message, and
   a NaN reaches the card as 0. Task 4, `a_library_of_another_abi_version_is_refused`,
   and Task 2, `a_nan_from_a_plugin_reaches_the_card_as_zero`.

---

### Task 1: plugin trait, blocks and chains

**Files:**
- Create: `src/plugin.rs`, `src/plugin/chain.rs`
- Modify: `src/lib.rs`, `src/format.rs`

**Interfaces:**
- Produces, `plugin.rs`:
  - `pub enum Block<'a> { I32(&'a mut [[i32; 2]]), F32(&'a mut [[f32; 2]]),
    F64(&'a mut [[f64; 2]]) }`, processed in place.
  - `pub struct Formats { pub i32: bool, pub f32: bool, pub f64: bool }`
  - `pub struct Param { pub name: String, pub unit: String, pub min: f64, pub max: f64,
    pub default: f64 }`
  - `pub struct Status { pub no_signal: bool }`
  - `pub trait Plugin: Send { fn name(&self) -> &str; fn formats(&self) -> Formats;
    fn params(&self) -> &[Param]; fn set_param(&mut self, index: usize, value: f64);
    fn neutral(&self) -> bool; fn latency_frames(&self) -> u32;
    fn process(&mut self, block: Block<'_>) -> Status; }`
- Produces, `plugin/chain.rs`:
  - `pub struct Chain`, `Chain::new(plugins: Vec<Box<dyn Plugin>>, format: Format,
    max_frames: usize) -> Chain`, `Chain::empty() -> Chain`
  - `pub fn process(&mut self, block: Block<'_>) -> Status`: runs the plugins in order.
    `no_signal` is true if any plugin reported it.
  - `pub fn neutral(&self) -> bool` (true for an empty chain),
    `pub fn latency_frames(&self) -> u32` (the sum),
    `pub fn set_param(&mut self, plugin: usize, index: usize, value: f64)`
- Produces, `format.rs`: on `Processing`, `fn to_block(acc: &mut [[Self::Acc; 2]],
  scratch: &mut BlockStore) -> Block<'_>` and `fn from_block(block: Block<'_>,
  acc: &mut [[Self::Acc; 2]])`, and `pub struct BlockStore` holding one buffer per
  sample type, sized once. `I32` clips into the block. The floats scale by 2^-31 and
  back, with no clipping.

- [ ] **Step 1: Write the failing tests** in `plugin/chain.rs` and `format.rs`

```rust
#[test] fn plugins_run_in_the_order_given()            // +1 then x2 is not x2 then +1
#[test] fn an_empty_chain_changes_nothing_and_is_neutral()
#[test] fn a_plugin_without_the_format_is_converted_around()
        // an f64-only plugin in an i32 chain sees 0.5 for 1 << 30 and gets 1 << 30 back
#[test] fn conversion_prefers_f64_then_f32_then_i32()
#[test] fn a_chain_is_neutral_only_if_every_plugin_is()
#[test] fn latency_is_the_sum_of_the_plugins()
#[test] fn no_signal_from_any_plugin_is_reported()
#[test] fn a_float_block_is_not_clipped_between_plugins()   // x4 then x0.25 returns 0.9
#[test] fn an_i32_block_is_clipped_before_the_first_plugin()
#[test] fn every_s24_value_survives_the_block_round_trip_in_f32()
#[test] fn every_s32_edge_survives_the_block_round_trip_in_f64_and_i32()
#[test] fn processing_does_not_allocate()               // assert_no_alloc, all formats
```

- [ ] **Step 2: Run `cargo test plugin` and see them fail**
- [ ] **Step 3: Implement the trait, `Block`, `Chain` and the two `Processing` functions**

  The chain owns its conversion scratch, sized from `max_frames` in `new`.
- [ ] **Step 4: Run `cargo test`, `cargo fmt --check` and clippy**
- [ ] **Step 5: Commit** as `feat(plugin): plugin trait and ordered chains`

### Task 2: chains in the engine, and the bit-perfect rule

**Files:**
- Modify: `src/engine.rs`, `src/bitperfect.rs`

**Interfaces:**
- Consumes: `Chain`, `Processing::to_block`, `from_block` from Task 1.
- Produces, `bitperfect.rs`: `NotBitPerfect::Plugin`, and `Situation` gains
  `pub plugins_neutral: bool`. Checked after `Resampled` and before `Volume`, in every
  mode but `Off`. Forced mode does not skip plugins.
- Produces, `engine.rs`:
  - `pub enum Place { Analog, Mix, Output }`
  - `Command::SetChain(Place, Box<Chain>)`, answered by `Event::OldChain(Box<Chain>)`
  - `Command::SetParam { place: Place, plugin: u8, index: u8, value: f64 }`
  - The engine starts with empty chains. In `play_pcm` the mix chain and then the
    output chain run on the accumulator, between the last `mix_add` and `mix_finish`.
    The copy path and `play_dop` run no chain.
  - The delay published to clients adds the latency of the mix and output chains.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn a_plugin_on_the_mix_changes_what_reaches_the_sink()
#[test] fn the_mix_chain_runs_before_the_output_chain()
#[test] fn a_plugin_that_is_not_neutral_stops_bit_perfect()   // decide() gives Plugin
#[test] fn a_neutral_plugin_leaves_one_source_bit_exact()
#[test] fn forced_bit_perfect_still_runs_plugins()
#[test] fn dop_passes_by_every_chain()
#[test] fn a_replaced_chain_comes_back()                       // Event::OldChain
#[test] fn a_parameter_set_takes_effect_on_the_next_cycle()
#[test] fn a_parameter_for_a_plugin_that_is_not_there_is_ignored()
#[test] fn plugin_latency_is_added_to_the_published_delay()
#[test] fn a_nan_from_a_plugin_reaches_the_card_as_zero()
#[test] fn cycles_with_chains_do_not_allocate()
```

- [ ] **Step 2: Run them and see them fail**
- [ ] **Step 3: Implement.** Add `Plugin` to the `decide` tests' `sit` helper.
- [ ] **Step 4: Run `cargo test`, fmt and clippy**
- [ ] **Step 5: Commit** as `feat(engine): run plugin chains on the mix and the output`

### Task 3: the `detect` and `balance` plugins

**Files:**
- Create: `src/plugin/detect.rs`, `src/plugin/balance.rs`, `src/plugin/builtin.rs`

**Interfaces:**
- Produces, `detect.rs`: `pub struct Detect`, `Detect::new(rate: u32) -> Detect`.
  Parameters in this order: `threshold` (dBFS, -90 to 0, default -50), `hold` (s, 0 to
  60, default 3). All formats. Always neutral. `pub fn open(&self) -> bool`.
  The level is the RMS of both channels over windows of 50 ms. It opens when a window
  is above the threshold. It closes when no window has been above it for the hold
  time. While closed it writes zeros and reports `no_signal`.
- Produces, `balance.rs`: `pub struct Balance`. One parameter, `balance` (-1 to 1,
  default 0). At 0 it is neutral and changes nothing. Towards one side the other
  channel's gain falls linearly to 0, and the near side stays at 1. All formats.
- Produces, `builtin.rs`: `pub fn create(kind: &str, rate: u32) ->
  Option<Box<dyn Plugin>>` for `"detect"` and `"balance"`.

- [ ] **Step 1: Write the failing tests**

```rust
// detect
#[test] fn silence_stays_closed_and_reports_no_signal()
#[test] fn a_tone_above_the_threshold_opens_within_one_window()   // 50 ms at 48 kHz
#[test] fn a_tone_below_the_threshold_stays_closed()             // -51 dBFS at -50
#[test] fn a_single_loud_sample_does_not_open_it()
#[test] fn a_level_that_dips_within_the_hold_time_stays_open()   // 2.9 s of silence
#[test] fn it_closes_when_the_hold_time_has_passed()             // 3.1 s of silence
#[test] fn while_closed_the_output_is_exactly_zero()
#[test] fn while_open_the_samples_are_unchanged()                // every format
#[test] fn the_window_follows_the_rate()                         // 44.1 and 192 kHz
#[test] fn a_new_threshold_applies_from_the_next_window()
// balance
#[test] fn centre_is_neutral_and_changes_nothing()
#[test] fn full_left_silences_the_right_channel_and_keeps_the_left()
#[test] fn half_right_halves_the_left_channel()
#[test] fn a_value_outside_the_range_is_clamped()
// builtin
#[test] fn an_unknown_kind_gives_none()
```

- [ ] **Step 2: Run them and see them fail**
- [ ] **Step 3: Implement.** The detector keeps its sums in `f64`.
- [ ] **Step 4: Run `cargo test`, fmt and clippy**
- [ ] **Step 5: Commit** as `feat(plugin): input detection and balance`

### Task 4: the C ABI, the loader and an example plugin

**Files:**
- Create: `crates/aas-plugin/{Cargo.toml,src/lib.rs}`, `plugins/gain/{Cargo.toml,src/lib.rs}`,
  `plugins/fixture-bad/{Cargo.toml,src/lib.rs}`, `src/plugin/native.rs`, `tests/plugin.rs`
- Modify: `Cargo.toml` (members and default-members add the three crates)

**Interfaces:**
- Produces, `aas-plugin` (no dependencies, `#[repr(C)]` throughout):
  - `pub const ABI: u32 = 1;`
  - `pub const FORMAT_I32: u32 = 1; FORMAT_F32: u32 = 2; FORMAT_F64: u32 = 4;`
  - `pub const STATUS_NO_SIGNAL: u32 = 1;`
  - `pub struct ParamInfo { name: *const c_char, unit: *const c_char, min: f64,
    max: f64, default: f64 }`
  - `pub struct Descriptor { abi: u32, name: *const c_char, formats: u32,
    channels_in: u32, channels_out: u32, param_count: u32, params: *const ParamInfo,
    create: extern "C" fn(rate: u32, max_frames: u32) -> *mut c_void,
    destroy: extern "C" fn(*mut c_void),
    set_param: extern "C" fn(*mut c_void, index: u32, value: f64),
    neutral: extern "C" fn(*const c_void) -> u32,
    latency: extern "C" fn(*const c_void) -> u32,
    process: extern "C" fn(*mut c_void, format: u32, samples: *mut c_void,
    frames: u32) -> u32 }`. `samples` is interleaved stereo in `format`, in place.
    `process`, `set_param`, `neutral` and `latency` run on the audio thread.
  - The library exports `extern "C" fn aas_plugin() -> *const Descriptor`.
- Produces, `native.rs`: `pub struct Library`, `Library::open(path: &Path) ->
  Result<Library, String>`, `Library::create(&self, rate: u32, max_frames: usize) ->
  Result<Box<dyn Plugin>, String>`. The library stays loaded for the life of the daemon.
- Produces, `plugins/gain`: a `cdylib` named `aas_gain` with one parameter, `gain`
  (dB, -60 to 20, default 0), neutral at 0, `f32` and `f64` only.

- [ ] **Step 1: Write the failing tests** in `tests/plugin.rs`. A helper builds the
  example with `Command::new(env!("CARGO")).args(["build", "-p", "aas-plugin-gain"])`
  in the test's profile and returns the path of the library.

```rust
#[test] fn a_loaded_plugin_processes_audio()            // gain -6.02 dB halves it
#[test] fn its_parameters_are_described()               // name gain, unit dB, range
#[test] fn it_is_neutral_at_its_default()
#[test] fn a_missing_file_is_refused_with_its_path()
#[test] fn a_library_without_the_symbol_is_refused()    // libc itself
#[test] fn a_library_of_another_abi_version_is_refused()   // a second cdylib fixture
#[test] fn a_plugin_that_is_not_stereo_is_refused()     // message says stage 3b
#[test] fn a_plugin_whose_create_returns_null_is_refused()
#[test] fn two_instances_do_not_share_state()
```

  The wrong version, mono and null cases come from one more `cdylib`,
  `plugins/fixture-bad` (package `aas-plugin-fixture-bad`). The variable
  `AAS_FIXTURE` picks the case when `aas_plugin()` is called, and each test that
  uses it runs the load in a child process, so tests do not share the variable.
- [ ] **Step 2: Run `cargo test --test plugin` and see them fail**
- [ ] **Step 3: Implement** with `libc::dlopen(RTLD_NOW | RTLD_LOCAL)` and `dlsym`.
  The adapter's `Drop` calls `destroy`.
- [ ] **Step 4: Run `cargo test`, fmt and clippy**
- [ ] **Step 5: Commit** as `feat(plugin): load plugins from shared libraries`

### Task 5: config and the card table

**Files:**
- Create: `src/cards.rs`
- Modify: `src/config.rs`, `src/lib.rs`, `README.md`

**Interfaces:**
- Produces, `cards.rs`: `pub struct Card { pub driver: &'static str, pub adc: bool }`,
  `pub fn lookup(driver: &str) -> Option<&'static Card>`, `pub fn has_adc(driver: &str)
  -> bool` (false for an unknown card), `pub fn driver_of(card_line: &str) -> Option<&str>`
  for a line of `/proc/asound/cards`. The table holds `snd_rpi_hifiberry_dacplusadc`
  and `snd_rpi_hifiberry_dacplusadcpro` with `adc: true`, and
  `snd_rpi_hifiberry_digi` with `adc: false`.
- Produces, `config.rs`:
  - `pub struct Analog { pub enabled: bool, pub volume_db: f32 }`, default off and 0 dB.
    `volume_db` from -60 to 0.
  - `pub enum PluginPlace { Analog, Mix, Output }`
  - `pub struct PluginConfig { pub place: PluginPlace, pub kind: String,
    pub path: Option<PathBuf>, pub params: BTreeMap<String, f64> }`
  - `Config` gains `pub analog: Analog` and `pub plugins: Vec<PluginConfig>`, in file
    order.

```toml
[analog]
enabled = true
volume_db = 0

[[plugin]]
place = "analog"          # analog, mix or output
type = "detect"           # a built-in name, or "library" with a path
threshold = -50
hold = 3

[[plugin]]
place = "output"
type = "library"
path = "/usr/lib/aas/libaas_gain.so"
gain = -3
```

  Every key of a `[[plugin]]` table other than `place`, `type` and `path` is a
  parameter.

- [ ] **Step 1: Write the failing tests**

```rust
// cards
#[test] fn the_test_boards_cards_have_an_adc()
#[test] fn a_digi_and_an_unknown_card_have_none()
#[test] fn the_driver_is_read_from_a_cards_line()
// config
#[test] fn the_analog_input_is_off_by_default()
#[test] fn plugins_keep_the_order_of_the_file()
#[test] fn a_plugin_table_carries_its_parameters()
#[test] fn an_unknown_place_is_an_error()
#[test] fn a_library_without_a_path_is_an_error()
#[test] fn a_path_on_a_built_in_is_an_error()
#[test] fn detect_outside_the_analog_chain_is_an_error()
#[test] fn two_detect_entries_are_an_error()
#[test] fn a_volume_outside_the_range_is_an_error()
#[test] fn a_parameter_that_is_not_a_number_is_an_error()
```

- [ ] **Step 2: Run them and see them fail**
- [ ] **Step 3: Implement.** Add the two tables to the key list in `README.md`.
- [ ] **Step 4: Run `cargo test`, fmt and clippy**
- [ ] **Step 5: Commit** as `feat(config): analog input and plugin tables`

### Task 6: the analog input in the engine

**Files:**
- Create: `src/capture.rs`
- Modify: `src/engine.rs`, `src/counters.rs`, `src/lib.rs`

**Interfaces:**
- Consumes: `Chain`, `Place`, `Command::SetChain` from Tasks 1 and 2.
- Produces, `capture.rs`:
  - `pub trait Capture: Send { fn available(&mut self) -> Result<usize, SinkError>;
    fn read(&mut self, out: &mut [[i32; 2]]) -> Result<usize, SinkError>;
    fn skip(&mut self, frames: usize) -> Result<(), SinkError>; fn close(&mut self); }`
    None of them waits.
  - `pub struct MemoryCapture` for tests: `MemoryCapture::new() -> (MemoryCapture,
    CaptureFeed)`, with `CaptureFeed::push(&self, frames: &[[i32; 2]])`,
    `fail(&self)` and `closed(&self) -> bool`.
- Produces, `engine.rs`:
  - `Command::SetCapture(Option<Box<dyn Capture>>)`, answered by
    `Event::OldCapture(Option<Box<dyn Capture>>)`
  - `Command::SetAnalogGain(f32)`
  - `Event::InputSignal(bool)`, sent when the analog chain's `no_signal` changes
  - `Event::CaptureFailed`, sent once when a capture call fails. The engine then
    closes that capture and keeps it until the control thread takes it back.
  - `pub const ANALOG: ClientId = ClientId::MAX;`
- Produces, `counters.rs`: `analog_padded_frames`, `analog_dropped_frames`,
  `analog_open` (0 or 1), all atomic, and a line for them in `Display` when capture is on.

Each cycle with a capture, after the wait for room: skip what is queued beyond two
periods, read one period, pad a shortfall with zeros at the front, run the analog
chain, and if it did not report `no_signal`, mix the block at the analog gain like a
running source. The input counts in `Situation` as one active source only then, and
its chain counts for `plugins_neutral` only then. On the copy path with the analog
input as the only source, the captured block is copied to the card. `apply_reopen`
closes the capture before it opens the sink.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn a_captured_period_reaches_the_sink()
#[test] fn the_analog_gain_applies()
#[test] fn the_analog_input_is_mixed_with_a_client()
#[test] fn below_the_threshold_nothing_of_it_reaches_the_sink()
#[test] fn below_the_threshold_one_client_stays_bit_exact()
#[test] fn above_the_threshold_a_client_is_no_longer_bit_perfect()   // SeveralSources
#[test] fn the_analog_input_alone_is_copied_bit_exact()
#[test] fn opening_and_closing_send_one_event_each()
#[test] fn a_short_capture_is_padded_at_the_front_and_counted()
#[test] fn a_capture_more_than_two_periods_ahead_is_cut_back_and_counted()
#[test] fn a_capture_that_fails_costs_only_the_analog_input()
        // a client plays on unaltered, one CaptureFailed, the capture is closed
#[test] fn the_capture_is_closed_before_the_output_reopens()
#[test] fn a_dop_stream_silences_the_analog_input()
#[test] fn the_fade_out_before_a_reopen_fades_the_analog_input_too()
#[test] fn a_replaced_capture_comes_back()
#[test] fn cycles_with_a_capture_do_not_allocate()
```

- [ ] **Step 2: Run them and see them fail**
- [ ] **Step 3: Implement**
- [ ] **Step 4: Run `cargo test`, fmt and clippy**
- [ ] **Step 5: Commit** as `feat(engine): mix the analog input as a built-in source`

### Task 7: control thread, rate rules and chains

**Files:**
- Modify: `src/control.rs`, `src/server.rs`, `tests/server.rs`

**Interfaces:**
- Consumes: `Config::analog`, `Config::plugins`, `cards::has_adc`, `builtin::create`,
  `Library`, `Command::SetChain`, `SetCapture`, `SetAnalogGain`, `Event::InputSignal`,
  `CaptureFailed`, `OldChain`, `OldCapture`, `ANALOG`.
- Produces, `control.rs`:
  - `pub type OpenCapture = Box<dyn FnMut(u32, u32) -> Result<Box<dyn Capture>, String>>`
    taking the rate and period.
  - `Control::set_analog(&mut self, open: OpenCapture)`, called before `start` when the
    config enables the input and the card has an ADC.
  - `pub fn build_chain(config: &[PluginConfig], place: PluginPlace, rate: u32,
    format: Format, period: usize, libraries: &mut Libraries) -> Result<Chain, String>`.
    For `Analog` it appends a `detect` if none is listed. A parameter name the plugin
    does not have is an error.
- `PluginPlace` in the config maps one to one to `Place` in the engine.
- `server::run` gains `capture: Option<OpenCapture>`. It loads every library and builds
  every chain once at the default rate before it starts the audio thread, and returns
  the error if one fails.

Behaviour:
- With the analog input on, `start` opens the output at the current rate at once, and
  the idle rule never closes it.
- After every `Event::Reconfigured` the control thread builds the three chains for the
  new rate and period and sends them, then opens the capture and sends it.
- `InputSignal(true)` is `policy.client_started(ANALOG, current_rate, now)`.
  `InputSignal(false)` is `policy.client_stopped(ANALOG, now)`. The analog input never
  gets a resampler.
- After `CaptureFailed`, and after an open that fails, it tries again once a second
  and logs the first failure and the recovery.

- [ ] **Step 1: Write the failing tests**

```rust
// control.rs
#[test] fn with_the_analog_input_on_the_output_opens_at_startup()
#[test] fn with_the_analog_input_on_the_output_stays_open_when_idle()
#[test] fn the_capture_is_opened_after_each_reconfigure_at_its_rate()
#[test] fn an_active_analog_input_is_the_first_client()
        // a player at 96 kHz arriving while it plays at 48 kHz is resampled
#[test] fn a_silent_analog_input_does_not_hold_the_rate()
        // a player at 96 kHz arriving then sets 96 kHz, and the capture reopens at it
#[test] fn the_analog_input_going_quiet_lets_the_remaining_player_take_the_rate()
#[test] fn a_failed_capture_is_retried_every_second()
#[test] fn the_analog_chain_gets_a_detector_if_none_is_listed()
#[test] fn a_listed_detector_keeps_its_place()
#[test] fn an_unknown_parameter_is_an_error()
#[test] fn chains_are_rebuilt_at_the_new_rate()
// tests/server.rs
#[test] fn the_analog_input_plays_through_the_daemon()        // MemoryCapture, MemorySink
#[test] fn a_plugin_that_cannot_be_loaded_stops_the_start()
#[test] fn an_enabled_input_on_a_card_without_an_adc_opens_nothing()
```

- [ ] **Step 2: Run them and see them fail**
- [ ] **Step 3: Implement**
- [ ] **Step 4: Run `cargo test`, fmt and clippy**
- [ ] **Step 5: Commit** as `feat(control): the analog input under the rate rules`

### Task 8: capture from the card

**Files:**
- Modify: `src/sink_alsa.rs`, `src/main.rs`, `dist/test-plugin.sh`

**Interfaces:**
- Consumes: `Capture`, `OpenCapture`, `cards`.
- Produces: `pub struct AlsaCapture`, `AlsaCapture::open(card: &str, rate: u32,
  period: u32) -> Result<AlsaCapture, String>`: S32 stereo, non-blocking, buffer of 4
  periods, started at once. `main` passes it to `server::run` when the config enables
  the input and `cards::has_adc` says so, and logs one line either way.

This task builds on a board. The loopback cable joins the DAC output to the ADC input,
so an open analog input feeds back. Every board test here sets `volume_db = -20`.

- [ ] **Step 1: Add the cases to `dist/test-plugin.sh`**, run only with `ANALOG=1`:
  - `analog_input_opens_on_signal`: `aplay` a -20 dBFS tone, the journal shows the
    input open within 200 ms and closed 3 to 4 s after the tone ends.
  - `analog_input_stays_closed_on_silence`: 10 s idle, no open in the journal.
  - `rate_change_with_capture_open`: play 44.1 kHz then 96 kHz, both exit 0, and the
    counters show no xrun.
- [ ] **Step 2: Run them on the Pi 3 and see them fail**
- [ ] **Step 3: Implement `AlsaCapture` and the wiring**
- [ ] **Step 4: Run the script with `ANALOG=1` on both boards.** Expected: the new cases
  pass and the existing ones still do.
- [ ] **Step 5: Run `cards` against both boards** (`cat /proc/asound/cards`) and confirm
  the driver names in the table.
- [ ] **Step 6: Commit** as `feat(sink): capture from the card's ADC`

### Task 9: capture streams in the protocol and the daemon

**Files:**
- Modify: `crates/aas-client/src/{proto.rs,ring.rs,client.rs}`, `src/control.rs`,
  `src/engine.rs`, `tests/server.rs`

**Interfaces:**
- Produces, `proto.rs`: `VERSION = 3`. `Request::Configure` gains `capture: bool`.
  `Reply::Configured` for a capture stream carries the daemon's rate.
  `Request::Start`, `Stop` and `Reset` work on a capture stream as on a playback one.
- Produces, `ring.rs`: `VERSION = 3`, header field `pub ended: AtomicU32`, set by the
  daemon when the stream can no longer be served.
- Produces, `client.rs`: `Client::configure_capture(buffer_frames: u32) ->
  io::Result<u32>` returning the rate, `Client::read(&mut self, out: &mut [[i32; 2]])
  -> io::Result<usize>`, `Client::ended(&self) -> bool`.
- Produces, `engine.rs`: `pub struct Tap { pub id: ClientId, pub ring: Ring,
  pub wake: OwnedFd, pub running: bool }`, `Command::AddTap(Box<Tap>)`,
  `Command::RemoveTap(ClientId)`, `Event::DroppedTap(Box<Tap>)`. `SetRunning` and
  `Flush` find a tap by its id too. Each cycle the engine writes the analog block
  after its chain to every running tap, zeros while there is no signal or no capture,
  and wakes it by the same `wake_at` rule as a player, with the roles swapped. A full
  ring takes what fits, and the tap's `underruns` counts the rest.

Before `Reconfigure` at another rate the control thread sets `ended` on every tap and
removes it. A capture client configured while the output is closed gets the rate the
output will open at.

- [ ] **Step 1: Write the failing tests**

```rust
// proto
#[test] fn a_capture_configure_round_trips()
// engine
#[test] fn a_tap_gets_the_analog_input_after_its_chain()
#[test] fn a_tap_gets_silence_while_there_is_no_signal()
#[test] fn a_tap_never_gets_the_mix()             // a client plays, the tap stays zero
#[test] fn a_recorder_that_stops_reading_loses_only_its_own_audio()
        // its ring fills, its counter rises, the sink and a second tap are unaffected
#[test] fn a_stopped_tap_is_not_written()
#[test] fn taps_do_not_allocate()
// tests/server.rs
#[test] fn a_recorder_reads_what_the_capture_delivers()
#[test] fn a_rate_change_ends_an_open_capture_stream()
#[test] fn a_recorder_does_not_count_as_a_client()   // the rate rules ignore it
```

- [ ] **Step 2: Run them and see them fail**
- [ ] **Step 3: Implement**
- [ ] **Step 4: Run `cargo test`, fmt and clippy**
- [ ] **Step 5: Commit** as `feat: capture streams that deliver the analog input`

### Task 10: the ALSA capture device

**Files:**
- Modify: `crates/aas-alsa/src/pcm.rs`, `dist/asound.conf`, `dist/test-plugin.sh`,
  `README.md`

**Interfaces:**
- Consumes: `Client::configure_capture`, `read`, `ended`, `wake_at`.
- Produces: `pcm.aas` opens for `SND_PCM_STREAM_CAPTURE`. S32 stereo only, at the one
  rate the daemon gives, so `plug` does every conversion. `transfer` copies out of the
  ring. `pointer`, `poll_revents` and `prepare` mirror playback, with `POLLIN`. A
  stream whose ring says `ended` returns `-ENODEV`. An overrun returns `-EPIPE`.

- [ ] **Step 1: Add the cases to `dist/test-plugin.sh`**
  - `arecord_records_each_format`: S16_LE, S24_LE, S32_LE at 44.1 and 48 kHz, 1 s
    each, exit 0 and the file has the length asked for.
  - `capture_is_silent_without_signal`: every sample of the recording is 0.
  - `capture_hears_the_analog_input` (`ANALOG=1`): a 1 kHz tone played through the
    cable is in the recording at 1 kHz, within 2 Hz.
  - `capture_never_hears_a_player` : with the analog input disabled in the config, a
    tone played through `aas` gives an all-zero recording.
  - `capture_ends_on_a_rate_change`: `arecord` exits non-zero within 1 s of a player
    at another rate starting, and a new `arecord` works.
  - `recorder_killed_keeps_daemon_up`
- [ ] **Step 2: Run them on the Pi 3 and see them fail**
- [ ] **Step 3: Implement**
- [ ] **Step 4: Run the whole script on both boards.** Expected: all cases pass but
  `mpd_mixer_works` where mpd is not installed.
- [ ] **Step 5: Commit** as `feat(alsa): capture device for the analog input`

### Task 11: measurements on the boards

**Files:**
- Modify: `doc/measurements.md`, `tools/measure/src/bin/dsp-bench.rs`

Run on the Pi 3 and the CM5 test boards, with the cable connected and
`volume_db = -20`. Give every long run a hard deadline and keep its logs in RAM.

- [ ] **Step 1: Add `detect` and `balance` to `dsp-bench`** as cases beside the mixes,
  at 48 and 192 kHz, in the three formats. Run it on both boards.
  Expected: each under 1% of a period on the Pi 3.
- [ ] **Step 2: Confirm the shared clock.** One hour with the analog input on and a
  tone playing. Expected: `analog_padded_frames` and `analog_dropped_frames` stay at
  what the first second left them, 0 xruns, 0 wake-up gaps over the buffer.
- [ ] **Step 3: Measure the latency of the analog path.** No second signal source is
  on the boards, so measure the echo. Run `loopback-latency` through `aas`. The chirp
  arrives once from the player, and again after one trip through the analog path.
  The time between the two arrivals is that path. Expected: at most three periods
  plus the converters' delay from stage 1.
- [ ] **Step 4: Repeat the three hour runs of stage 2 under full load with the analog
  input on,** on both boards. Pass criterion as in stage 2.
- [ ] **Step 5: Time detection.** 20 starts of a -20 dBFS tone: time from the first
  sample at the ADC to the input opening, and from the end to it closing.
  Expected: opens within 100 ms, closes 3.0 to 3.1 s after.
- [ ] **Step 6: Write the figures into a "Stage 3a" section of `doc/measurements.md`,**
  naming the board for each. A failure is stated as a failure.
- [ ] **Step 7: Commit** as `docs: record the stage 3a measurements`

### Task 12: design and README

**Files:**
- Modify: `doc/design.md`, `README.md`, `TODO.md`

- [ ] **Step 1: Add every decision from "Decisions this plan makes" to the design,**
  in the section it belongs to, marked *(call)*. Remove "confirm on the hardware" from
  the shared clock sentence if Task 11 confirmed it, and say what was measured.
- [ ] **Step 2: Describe in `README.md`** how to turn the analog input on, the plugin
  tables with an example, how to build a plugin against `aas-plugin`, and how to
  record from the capture device.
- [ ] **Step 3: Park in `TODO.md`** what this stage found and left, and the cards not
  yet in the table.
- [ ] **Step 4: Commit** as `docs: analog input and plugins in the design`
