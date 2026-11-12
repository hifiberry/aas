# Stage 3b, part 1: mixing matrix and channel counts implementation plan

> The design, `doc/design.md`, is the authority: where this plan disagrees with it, the
> design wins.

**Goal:** A mixing matrix routes the channels of the sources to the channels of the
output, the output can have up to eight channels, and a plugin can take and give any
number of channels, all on one card.

**Architecture:** Sources stay stereo up to the mix: clients, rings, resamplers, the
analog input and its chain, and the capture streams do not change. The engine mixes
them into two stereo buses, `players` and `analog`. The matrix adds the four bus
channels into a block that is as wide as the chains need, the `mix` and `output` chains
run on it and may change its width, and the result is written to the card as
interleaved S32. A block carries its channel count, so one `Chain` type serves the
stereo analog chain and the wide ones.

**Tech Stack:** Rust (edition 2024, `rust-version` 1.85), `alsa` 0.12 behind the
`alsa-output` feature, `libc`, `rtrb`, `serde` with `toml`. No new dependencies.

**Spec:** `doc/design.md`, sections Output, Mixing matrix, Bit-perfect, DoP, Plugins,
Plugin libraries, Configuration. Measurements: `doc/measurements.md`.

## Stages

1. Measurement (done)
2. Core daemon and ALSA plugin (done)
3. a. Analog input and native plugins (done)
   b. Part 1: matrix and channel counts on one card (this plan).
      Part 2: several cards, adaptive resampling, a matrix and a chain per card.
   c. `input-processor` as a native plugin
4. API, monitoring, graph export
5. Packaging, Bluetooth and the switch from PipeWire, with all players tested

The owner chose this order on 2026-11-12, and that more than two channels belong here.
A second card with its own clock is not on the test boards yet, so part 2 will be tested
with `snd-aloop` and simulated drift.

## Scope

In this part: blocks with a channel count, plugins that change the count, ABI
version 2, a card opened with a configured number of channels, the matrix from the
config, its effect on bit-perfect and DoP, a matrix change at run time inside the
engine, and measurements on both boards.

Not in this part, and nothing here may block it:

| Later work | Stage |
|---|---|
| Several cards, the clock master, adaptive resampling, idle and volume per output | 3b part 2 |
| A matrix row per client (needs clients known by name) | 4 |
| `GET`, `PUT /matrix`, the matrix in the state file | 4 |
| Clients with more than two channels | not planned |
| The limiter plugin, a crossover plugin | later |

## Decisions this plan makes

These are not in the design. Task 10 adds them to it, marked *(call)*.

- **The matrix has four inputs:** `players.left`, `players.right`, `analog.left` and
  `analog.right`. `players` is the sum of all clients after their volumes. A row per
  client waits for stage 4, where clients are known by name.
- **The matrix is a list of routes in the config.** Each `[[matrix]]` table has `from`,
  `to` (an output channel, counted from 1) and `gain_db`, from -100 to 0, 0 by default.
  What no route names is off. Without any `[[matrix]]` table, left goes to channel 1 and
  right to channel 2, for both sources, at 0 dB, and further channels are silent. This
  is a third table that is not flat, for the reason `[[plugin]]` is not.
- **The card's channel count is `channels` in the config,** 2 by default, at most 8.
  A card that does not take that count counts as missing: the null sink plays, the
  reason is logged, and the open is tried again every second.
- **The output volume and the client volumes stay where they are,** before the matrix.
  The matrix gains come on top.
- **`mix` and `output` both belong to the output and run after the matrix,** `mix`
  first. They differ only in order, as today. In part 2 a plugin names its card.
- **The width of the matrix follows from the chains.** Walking the `mix` and then the
  `output` chain, the first plugin with a fixed channel count sets the width to its
  input count; if there is none the width is `channels`. The chains must end at
  `channels`, and a route to a channel above the width is an error. Both stop the
  daemon at startup with a message.
- **A plugin's channels are fixed, or any.** A fixed plugin names its input and output
  counts. A plugin for any count keeps the count and is told it when created.
  `balance` and `detect` take two channels. A plugin whose counts differ is never
  neutral.
- **ABI version 2** adds the channel count to `create` and a `process_into` function
  for a plugin whose counts differ. `channels_in` and `channels_out` both 0 mean any
  count. The daemon loads version 1 libraries as before, as plugins with two channels
  in and out.
- **Bit-perfect needs pure routing.** The routes from the one active source are pure
  when each is 0 dB and no output channel takes more than one of them. A swap is
  bit-perfect; a sum to mono is not. The copy path then copies each source channel to
  its output channels, and writes zeros to the others.
- **In forced bit-perfect every route counts as 0 dB,** as volumes do, and chains are
  bypassed. So forced bit-perfect with chains that change the channel count cannot
  work, and stops the daemon at startup with a message.
- **DoP needs pure routing of `players` and chains that keep the count.** Otherwise a
  DoP stream is silent, like one at a rate the card does not take, and never takes the
  output.
- **A matrix change fades.** The engine takes a new matrix as a command and moves each
  gain to its new value over 10 ms, at once when nothing is audible. Only the config
  sends one in this stage.
- **Buffers are sized for the configured channels,** not for eight. A wide chain costs
  about 0.25 MiB per channel.

## Global Constraints

- The audio thread does not allocate, take locks or make blocking calls during playback.
  Chains, plugin instances and matrices are built on the control thread.
- A client can always connect. A stalled or crashed client loses only its own audio.
- Routing changes only through the config. Nothing rewires a chain or the matrix on
  its own.
- With no `[[matrix]]` table and `channels = 2`, what reaches the card is bit for bit
  what `main` at 2ce4e21 sends, in every processing format. The existing tests are the
  check, and stay.
- A DoP stream reaches the card unaltered or not at all.
- The analog input never passes ungated.
- A chain never runs at a rate, or on a channel count, it was not built for.
- Audio is never dithered. Float conversion scales by an exact power of two.
- A wake-up gap longer than the buffer counts as a failure even with 0 xruns.
- `MAX_CHANNELS` is 8, in `src/matrix.rs`.
- The workstation has no ALSA headers: `cargo test --workspace --exclude aas-alsa`
  there, and anything linking ALSA builds on a test board.
- Public text names no internal hosts or addresses, and no tools.

## Review Focus

1. The card does not take the configured channels, such as `channels = 8` on a stereo
   DAC. The daemon keeps running on the null sink and logs why, once per reason.
   Task 4, `a_card_that_refuses_the_channel_count_counts_as_missing`.
2. Two full-scale channels are summed into one. The result clips at the S32 limits in
   every format and does not wrap. Task 6,
   `a_sum_of_two_full_scale_channels_clips_and_does_not_wrap`.
3. A DoP stream plays while the matrix sums to mono. The card gets silence, never
   altered DoP. Task 7, `a_dop_stream_is_silent_when_the_routing_is_not_pure`.
4. A version 1 library, or `balance`, sits where the block has eight channels. The
   daemon refuses at startup and names the plugin and both counts. Task 2,
   `a_plugin_at_the_wrong_channel_count_is_refused_with_both_counts`.
5. The matrix changes while music plays. No sample steps by more than the 10 ms ramp
   allows. Task 8, `a_matrix_change_ramps_over_10_ms`.

---

### Task 1: blocks carry a channel count

A refactoring. No behaviour changes and every existing test passes unchanged in what
it asserts.

**Files:**
- Modify: `src/plugin.rs`, `src/format.rs`, `src/plugin/chain.rs`,
  `src/plugin/detect.rs`, `src/plugin/balance.rs`, `src/plugin/native.rs`,
  `src/engine.rs`, `tests/plugin.rs`

**Interfaces:**
- Produces, in `src/plugin.rs`:

```rust
/// Interleaved frames in one of the processing formats.
pub struct Block<'a> { pub channels: usize, pub samples: Samples<'a> }
pub enum Samples<'a> { I32(&'a mut [i32]), F32(&'a mut [f32]), F64(&'a mut [f64]) }
impl Block<'_> {
    pub fn reborrow(&mut self) -> Block<'_>;
    pub fn format(&self) -> Format;
    pub fn frames(&self) -> usize;          // samples / channels
}
```

- Produces, in `src/format.rs`: `BlockStore::new(max_frames: usize, channels: usize)`
  with flat `Vec<i32>`, `Vec<f32>`, `Vec<f64>`; and on `Processing`

```rust
fn to_block<'s>(acc: &mut [Self::Acc], channels: usize, scratch: &'s mut BlockStore) -> Block<'s>;
fn from_block(block: Block<'_>, acc: &mut [Self::Acc]);
```

- `Chain::new(plugins, format, max_frames)` keeps its signature in this task and
  builds a two-channel store.

- [ ] **Step 1: Write the failing tests** in `src/plugin.rs` and `src/format.rs`:

```rust
#[test] fn a_block_of_six_samples_in_three_channels_has_two_frames()
#[test] fn a_block_round_trip_keeps_three_channels_apart()   // acc [1,2,3,4,5,6] in f64 and i32 comes back equal
```

- [ ] **Step 2: Run them, expect a compile failure.**
- [ ] **Step 3: Change `Block`, `BlockStore` and `Processing`,** then every user. Stereo
  frames `[[T; 2]]` become flat with `as_flattened_mut()`; the analog path and the
  engine's stereo accumulator stay `[[T; 2]]` and flatten at the call.
- [ ] **Step 4: `cargo test --workspace --exclude aas-alsa`,** all pass; `cargo clippy
  --workspace --exclude aas-alsa --all-targets -- -D warnings`; `cargo fmt --check`.
- [ ] **Step 5: Commit** as `refactor(plugin): blocks carry their channel count`.

### Task 2: plugins and chains that change the channel count

**Files:**
- Modify: `src/plugin.rs`, `src/plugin/chain.rs`, `src/plugin/builtin.rs`,
  `src/plugin/balance.rs`, `src/plugin/detect.rs`, `src/plugin/build.rs`
- Create: `src/matrix.rs` with only `pub const MAX_CHANNELS: usize = 8;` (Task 5 fills
  it), listed in `src/lib.rs`

**Interfaces:**
- Produces, in `src/plugin.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels { Any, Fixed { input: usize, output: usize } }

pub trait Plugin: Send {
    /// Default: Fixed { input: 2, output: 2 }.
    fn channels(&self) -> Channels;
    /// For a plugin whose counts differ: reads `input`, writes `output`, the same
    /// number of frames. Never called otherwise. Default: unreachable!().
    fn process_into(&mut self, input: Block<'_>, output: Block<'_>) -> Status;
    // the rest as before
}
```

- Produces, in `src/plugin/chain.rs`:

```rust
/// `channels` is the count of the blocks the chain is given. Fails with a message that
/// names the plugin by its name and gives both counts when a plugin does not fit the
/// count that reaches it, or a count is 0 or above MAX_CHANNELS.
pub fn new(plugins: Vec<Box<dyn Plugin>>, format: Format, max_frames: usize, channels: usize)
    -> Result<Chain, String>;
pub fn channels_in(&self) -> usize;
pub fn channels_out(&self) -> usize;
/// Runs the plugins. The result is `block` itself if the chain kept the count, else a
/// block in the chain's own memory with `channels_out` channels.
pub fn run<'a>(&'a mut self, block: Block<'a>) -> (Block<'a>, Status);
/// True if a plugin's counts differ.
pub fn changes_channels(&self) -> bool;
```

  `Chain::process` stays for a chain that keeps the count and panics otherwise.
  `Chain::neutral` is false when `changes_channels`. `Chain::empty()` takes any count.
- Produces: `builtin::create(kind: &str, rate: u32, channels: usize)`;
  `build_chain(config, place, rate, format, channels, libraries)`, whose error for a
  misfit is `Failure::Config` and names the plugin by its position, as its other
  errors do.
- Consumes: Task 1.

- [ ] **Step 1: Write the failing tests** in `src/plugin/chain.rs`, with test plugins
  defined in the test module (`Wide`: fixed 2 to 4, copies left to 1 and 3, right to 2
  and 4; `Scale`: any count, halves every sample):

```rust
#[test] fn a_chain_that_widens_gives_its_own_block_with_four_channels()
    // run on frames [[1,2],[3,4]] gives channels 4, samples [1,2,1,2,3,4,3,4]
#[test] fn a_plugin_for_any_count_runs_after_one_that_widens()   // Wide, Scale: channels_out 4, halves
#[test] fn a_plugin_at_the_wrong_channel_count_is_refused_with_both_counts()
    // Wide then balance: Err containing "balance", "2" and "4"
#[test] fn a_chain_that_changes_the_count_is_never_neutral()
#[test] fn a_chain_keeps_the_count_it_is_given_without_fixed_plugins()  // Scale at 6: in 6, out 6
#[test] fn a_widening_plugin_that_wants_a_conversion_gets_it()    // Wide takes f64 only, chain is f32
#[test] fn more_than_eight_channels_are_refused()
```

- [ ] **Step 2: Run them, expect failures.**
- [ ] **Step 3: Implement.** The chain allocates, on the control thread, the buffers
  each change of count needs, for `max_frames` frames at that width.
- [ ] **Step 4: Run the tests, clippy and fmt as in Task 1.**
- [ ] **Step 5: Commit** as `feat(plugin): chains that change the channel count`.

### Task 3: plugin ABI version 2

**Files:**
- Modify: `crates/aas-plugin/src/lib.rs`, `src/plugin/native.rs`, `plugins/gain`,
  `tests/plugin.rs`, `plugins/fixture-bad`
- Create: `plugins/fixture-v1` (the `gain` plugin as it is at 2ce4e21, with its own
  copy of the version 1 structs, so that it does not follow the crate),
  `plugins/fixture-wide` (version 2, fixed 2 to 4, as `Wide` in Task 2)

**Interfaces:**
- Produces, in `crates/aas-plugin`: `ABI = 2`. `Descriptor` keeps every field and its
  offset, with two changes:

```rust
/// 0 with `channels_out` 0: any count, kept. Else the fixed input count, 1 to 8.
pub channels_in: u32,
pub channels_out: u32,
/// `channels` is the count of the blocks the instance is given.
pub create: Option<extern "C" fn(rate: u32, max_frames: u32, channels: u32,
                                 instance: *mut *mut c_void) -> u32>,
/// Appended. Must be given if the counts differ, may be None otherwise. Reads
/// `frames * channels_in` samples at `input`, writes `frames * channels_out` at
/// `output`; the two do not overlap.
pub process_into: Option<extern "C" fn(*mut c_void, format: u32, input: *const c_void,
                                       output: *mut c_void, frames: u32) -> u32>,
```

  `process` is in place and takes `frames * channels` samples.
  `DESCRIPTOR_SIZE_V2` is frozen at 112 bytes on 64-bit and 68 on 32-bit, with const
  assertions on the offset of `process_into`. The version 1 constants stay.
- Produces, in the loader: a library with `abi == 1` is read with the version 1
  `create` signature and is `Channels::Fixed { input: 2, output: 2 }`. With
  `abi == 2` the loader refuses: one count 0 and the other not, a count above 8,
  differing counts without `process_into`, a size below `DESCRIPTOR_SIZE_V2`. Any
  other `abi` is refused as today.
- `plugins/gain` becomes version 2 for any count.

- [ ] **Step 1: Write the failing tests** in `tests/plugin.rs`:

```rust
#[test] fn a_version_1_library_still_loads_and_is_stereo()
#[test] fn a_version_2_library_for_any_count_runs_on_six_channels()   // gain -6 dB halves all six
#[test] fn a_library_that_widens_runs_through_process_into()          // fixture-wide in a chain
#[test] fn a_version_2_library_with_differing_counts_and_no_process_into_is_refused()
#[test] fn a_version_2_library_with_one_count_zero_is_refused()
#[test] fn a_version_1_library_is_refused_where_the_block_has_four_channels()
```

  The two refusals of a descriptor come from new cases in `plugins/fixture-bad`.
- [ ] **Step 2: Run them, expect failures.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests, clippy and fmt.**
- [ ] **Step 5: Update "Writing a plugin" in `README.md`** for version 2.
- [ ] **Step 6: Commit** as `feat(plugin): ABI version 2 with channel counts`.

### Task 4: a card with a configured number of channels

**Files:**
- Modify: `src/sink.rs`, `src/sink_alsa.rs`, `src/config.rs`, `src/main.rs`,
  `src/server.rs`, `src/engine.rs` (only the call of `write`), `README.md`

**Interfaces:**
- Produces, on `Sink`:

```rust
/// Interleaved S32, `channels()` samples per frame.
fn write(&mut self, samples: &[i32]) -> Result<(), SinkError>;
/// Fixed for the life of the sink.
fn channels(&self) -> usize;
```

  `NullSink::new(channels)`, `MemorySink::with_channels(channels)` (`new` and `paced`
  stay stereo), `SinkLog::samples() -> Vec<i32>` beside the existing stereo accessor,
  which panics for another count. `FallbackSink` takes its count from the sink it
  wraps. `AlsaSink::new(card: Option<String>, channels: usize)`.
- Produces: `Config::channels: usize`, key `channels`, default 2, 1 to 8, else an
  error naming the key.
- In this task the engine still renders stereo and writes it flattened, so the
  daemon refuses `channels != 2` at startup with "needs the matrix"; Task 6 removes
  that.

- [ ] **Step 1: Write the failing tests:**

```rust
// src/config.rs
#[test] fn channels_defaults_to_two_and_takes_one_to_eight()
#[test] fn nine_channels_are_an_error()
// src/sink.rs
#[test] fn a_memory_sink_with_four_channels_records_interleaved_samples()
#[test] fn a_write_that_is_not_whole_frames_panics_in_the_memory_sink()
#[test] fn a_card_that_refuses_the_channel_count_counts_as_missing()
    // a fallback over a sink whose open fails: the null sink plays with the same
    // count and the open is retried after RETRY
```

- [ ] **Step 2: Run them, expect failures.**
- [ ] **Step 3: Implement.** In `sink_alsa.rs` the playback `CHANNELS` constant becomes
  the sink's field, a card that does not take the count fails the open with
  "{device}: does not take {n} channels", and the S32 fast path writes the samples
  as they are. Capture stays at two channels.
- [ ] **Step 4: Run the tests, clippy and fmt.** Build on the CM5 test board with
  `--features alsa-output` and play one minute with `channels = 2`: 0 xruns.
- [ ] **Step 5: Commit** as `feat(sink): a card opened with a configured channel count`.

### Task 5: the matrix and its config

**Files:**
- Modify: `src/matrix.rs`, `src/config.rs`, `README.md`

**Interfaces:**
- Produces, in `src/matrix.rs`:

```rust
pub const INPUTS: usize = 4;                 // players.left, players.right, analog.left, analog.right
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bus { Players, Analog }
#[derive(Debug, Clone, PartialEq)]
pub struct Matrix { /* gains[input][channel], linear; 0.0 is off */ }
impl Matrix {
    /// Left to channel 1 and right to channel 2 for both buses, as far as `width` goes.
    pub fn default_for(width: usize) -> Matrix;
    /// Fails for a channel 0 or above `width`, or the same from and to twice.
    pub fn from_routes(routes: &[Route], width: usize) -> Result<Matrix, String>;
    pub fn width(&self) -> usize;
    pub fn gain(&self, input: usize, channel: usize) -> f32;
    /// For each output channel, which channel of `bus` it copies (0 left, 1 right), if
    /// the routes from that bus are pure: each 0 dB, at most one per output channel.
    /// `forced` takes every route as 0 dB.
    pub fn routing(&self, bus: Bus, forced: bool) -> Option<[Option<u8>; MAX_CHANNELS]>;
}
```

- Produces, in `src/config.rs`: `pub struct Route { pub from: usize, pub to: usize,
  pub gain_db: f32 }` (`from` is the input index, `to` counts from 1) and
  `Config::matrix: Option<Vec<Route>>`, `None` without a `[[matrix]]` table. `from`
  is one of the four names, `gain_db` from -100 to 0. Errors read
  `matrix 3: from "players" must be ...`, with the table's position from 1.

- [ ] **Step 1: Write the failing tests:**

```rust
// src/matrix.rs
#[test] fn the_default_matrix_is_left_to_1_and_right_to_2_for_both_buses()
#[test] fn the_default_matrix_for_one_channel_takes_left_only()
#[test] fn a_route_above_the_width_is_refused_with_the_channel_and_the_width()
#[test] fn the_same_route_twice_is_refused()
#[test] fn a_swap_is_pure_routing()            // routing(Players) == [Some(1), Some(0), None, ...]
#[test] fn a_sum_to_mono_is_not_pure()
#[test] fn a_route_at_minus_6_db_is_not_pure_unless_forced()
#[test] fn one_source_channel_to_two_outputs_is_pure()
#[test] fn minus_6_db_is_a_gain_of_about_a_half()
// src/config.rs
#[test] fn matrix_routes_are_read_in_file_order()
#[test] fn a_matrix_route_with_an_unknown_source_is_an_error()
#[test] fn a_matrix_gain_above_0_db_is_an_error()
#[test] fn a_matrix_route_without_to_is_an_error()
#[test] fn no_matrix_table_gives_none()
```

- [ ] **Step 2: Run them, expect failures.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests, clippy and fmt.**
- [ ] **Step 5: Add `channels` and `[[matrix]]` to the key list in `README.md`,** with
  a swap and a mono sum as examples.
- [ ] **Step 6: Commit** as `feat(matrix): the mixing matrix and its config`.

### Task 6: the engine mixes through the matrix

**Files:**
- Modify: `src/engine.rs`, `src/format.rs`, `src/server.rs`, `src/main.rs`,
  `src/control.rs`, `src/plugin/build.rs`

**Interfaces:**
- Consumes: Tasks 1, 2, 4, 5.
- Produces: `Engine::new(sink, commands, events, counters, mode, matrix: Matrix)`.
  The engine takes its output count from `sink.channels()` and sizes its wide
  buffers for `max(matrix.width(), sink.channels())`.
- Produces, in `src/format.rs`:

```rust
/// Adds `bus * gain` for both bus channels into the interleaved `acc`, `width` wide.
/// `gains[c]` is the ramp from bus channel c to each output channel; off is skipped.
pub fn matrix_add<P: Processing>(bus: &[[P::Acc; 2]], gains: &[[Ramp; MAX_CHANNELS]; 2],
                                 width: usize, acc: &mut [P::Acc]);
pub fn mix_finish_wide<P: Processing>(acc: &[P::Acc], out: &mut [i32]);
```

- Produces, in `src/plugin/build.rs`: `pub fn matrix_width(config: &[PluginConfig],
  channels: usize, libraries: &mut Libraries) -> Result<usize, BuildError>`, the rule
  in "Decisions": the input count of the first fixed plugin of the `mix` then the
  `output` chain, else `channels`. `Chains::build_for_start` builds `mix` at that
  width and `output` at what `mix` gives, and fails unless `output` ends at `channels`.
- `Command::SetChain` refuses, like a chain too short, a `mix` chain whose
  `channels_in` is not the matrix width, an `output` chain that does not continue
  the `mix` chain, and a pair that does not end at the sink's count. An empty chain
  fits any count.

The cycle, where it differs from today: clients are mixed into the `players` bus and
the analog input into the `analog` bus, each with the fades and gains they have now.
`matrix_add` puts both into the wide accumulator. The `mix` chain and then the
`output` chain run on it with `Chain::run`. `mix_finish_wide` gives the samples for
`sink.write`. The mute for a chain at another rate, its fade in, the reset after idle
periods, and the fade out before a reopen act on the wide block. The copy path keeps
working for the default matrix on two channels; Task 7 makes it general.

- [ ] **Step 1: Write the failing tests** in `src/engine.rs`, in every processing
  format unless noted:

```rust
#[test] fn the_default_matrix_plays_what_the_engine_played_before()   // two clients and the analog input, against known samples
#[test] fn a_swap_matrix_swaps_left_and_right()
#[test] fn a_mono_sum_at_minus_6_db_gives_half_of_each_channel_on_both()
#[test] fn a_sum_of_two_full_scale_channels_clips_and_does_not_wrap()
#[test] fn the_analog_input_can_go_to_other_channels_than_the_players()   // 4 channels: players 1,2, analog 3,4
#[test] fn channels_no_route_names_are_silent()
#[test] fn a_chain_that_widens_feeds_a_four_channel_card()      // matrix width 2, Wide, sink 4
#[test] fn the_output_volume_applies_on_every_channel()
#[test] fn a_reopen_fades_every_channel_out()
#[test] fn a_chain_for_another_rate_mutes_every_channel_and_fades_back_in()
#[test] fn a_mix_chain_of_the_wrong_width_is_sent_back()
// src/plugin/build.rs
#[test] fn the_matrix_is_as_wide_as_the_first_fixed_plugin_takes()
#[test] fn without_a_fixed_plugin_the_matrix_is_as_wide_as_the_card()
#[test] fn chains_that_do_not_end_at_the_cards_channels_are_refused_with_both_counts()
```

- [ ] **Step 2: Run them, expect failures.**
- [ ] **Step 3: Implement,** and build the matrix at startup in `src/server.rs` from
  `Config::matrix` and `matrix_width`; an error stops the daemon like any config
  error. Remove Task 4's refusal of `channels != 2`. Log one line at startup, such as
  `aas: matrix: players.left to 1, players.right to 2, analog.left to 1, analog.right to 2`,
  with the gain where it is not 0 dB.
- [ ] **Step 4: Run all tests, clippy and fmt.** Every existing engine and server test
  passes without a change to what it asserts.
- [ ] **Step 5: Commit** as `feat(engine): mix through the matrix into the card's channels`.

### Task 7: bit-perfect and DoP with the matrix

**Files:**
- Modify: `src/engine.rs`, `src/bitperfect.rs`, `src/server.rs`

**Interfaces:**
- Consumes: Task 6, `Matrix::routing`.
- Produces: `Situation::routing_pure: bool` and a reason
  `NotBitPerfect::Routing` that `decide` returns when it is false, after the reasons
  it has today. The engine sets it from the routing of the one active source's bus;
  with no source active it is true.

Rules, each from "Decisions":
- The copy path copies each channel of the one active source to the output channels
  its routing names and writes zeros to the others. It needs a matrix as wide as the
  card and chains that do not change the count.
- In forced bit-perfect the routing is taken with every route at 0 dB, and a second
  source is mixed through the matrix with every route at 0 dB.
- A DoP stream is played through the `players` routing, pure and not forced, under
  the same two conditions. Otherwise it is muted as `Source::muted` does for a rate
  mismatch, holds no claim, and is not reported with `Event::DopStarted`.
- Forced bit-perfect with `mix` or `output` chains that change the count stops the
  daemon at startup: "bit_perfect = \"forced\" needs chains that keep the channel
  count".

- [ ] **Step 1: Write the failing tests** in `src/engine.rs` and `src/bitperfect.rs`:

```rust
#[test] fn a_swap_matrix_is_bit_perfect_and_swaps()                // f32, every S32 edge value arrives swapped
#[test] fn a_route_at_minus_6_db_is_not_bit_perfect_in_auto()
#[test] fn a_mono_sum_is_not_bit_perfect()
#[test] fn the_copy_path_writes_zeros_to_channels_without_a_route()   // 4 channels
#[test] fn an_impure_analog_route_does_not_stop_bit_perfect_for_a_player()
#[test] fn forced_takes_a_route_at_minus_6_db_as_0_db()
#[test] fn a_dop_stream_reaches_swapped_channels_unaltered()
#[test] fn a_dop_stream_is_silent_when_the_routing_is_not_pure()   // and no DopStarted is sent
#[test] fn a_dop_stream_is_silent_behind_a_chain_that_widens()
#[test] fn impure_routing_is_reported_as_the_reason()              // bitperfect.rs
```

- [ ] **Step 2: Run them, expect failures.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run all tests, clippy and fmt.**
- [ ] **Step 5: Commit** as `feat(engine): bit-perfect and DoP follow the matrix`.

### Task 8: a matrix change at run time

**Files:**
- Modify: `src/engine.rs`

**Interfaces:**
- Produces: `Command::SetMatrix(Box<Matrix>)`, answered by
  `Event::OldMatrix(Box<Matrix>)` with the matrix it replaced, or with the new one if
  its width differs from the one in place, which is then kept.
- Each gain moves linearly to its new value over 10 ms of frames, from where it is,
  also when a second change arrives during the ramp. With nothing audible it applies
  at once. While a ramp runs the copy path is not used.

- [ ] **Step 1: Write the failing tests** in `src/engine.rs`:

```rust
#[test] fn a_matrix_change_ramps_over_10_ms()
    // constant input, swap to default at 48 kHz: 480 frames, each step at most 1/480 of the level
#[test] fn a_matrix_change_with_nothing_audible_applies_at_once()
#[test] fn a_second_change_during_the_ramp_starts_from_the_gain_reached()
#[test] fn a_matrix_of_another_width_is_sent_back()
#[test] fn bit_perfect_resumes_after_the_ramp()
```

- [ ] **Step 2: Run them, expect failures.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run all tests, clippy and fmt.**
- [ ] **Step 5: Commit** as `feat(engine): a matrix change fades over 10 ms`.

### Task 9: measurements on the test boards

Both boards carry stereo cards, so more than two channels run against `snd-aloop`.

**Files:**
- Modify: `dist/test-plugin.sh`, `tools/measure` (a `--channels` option for
  `bit-check`), `doc/measurements.md`

- [ ] **Step 1: Bit-exactness through `snd-aloop` on the CM5,** with `bit-check`:
  the default matrix; a swap, expecting swapped channels bit for bit; and
  `channels = 8` with players on 1, 2 and 7, 8, expecting those four exact and the
  rest zero.
- [ ] **Step 2: Cost on the Pi 3,** worst and average cycle over 10 minutes at 48 kHz,
  `f32` and `f64`: stereo with the default matrix against `main` at 2ce4e21, and eight
  channels to `snd-aloop` with `gain` on all eight. The stereo figures must be within
  the run-to-run spread of `main`.
- [ ] **Step 3: One hour at 44.1 kHz and one at 96 kHz under full load on each board,**
  on the real card with a swap matrix and the analog input on: 0 xruns and no wake-up
  gap over the buffer.
- [ ] **Step 4: By ear and by capture on the CM5,** a swap matrix moves a left-only
  tone to the right output, through the cable into the ADC.
- [ ] **Step 5: Write the figures into a "Stage 3b, part 1" section of
  `doc/measurements.md`,** failures included, and leave the boards as found.
- [ ] **Step 6: Commit** as `docs: record the stage 3b matrix measurements`.

### Task 10: the design and the open list

**Files:**
- Modify: `doc/design.md`, `README.md`, `TODO.md`

- [ ] **Step 1: Add every entry of "Decisions this plan makes" to `doc/design.md`,**
  marked *(call)*, in Mixing matrix, Bit-perfect, DoP, Plugins, Plugin libraries and
  Configuration. Replace "Plugins take two channels and give two" and the version 1
  wording with what is built. Add anything a task decided that is not listed here.
- [ ] **Step 2: Update `TODO.md`:** remove what this part built from "Plugins not built
  yet", and add an entry for part 2 (several cards, adaptive resampling, a matrix and
  chains per card, tested with `snd-aloop` until a second card is on a board) and one
  for a real card with more than two channels, which no board has.
- [ ] **Step 3: Check that `README.md` lists `channels`, `[[matrix]]` and ABI
  version 2,** and that nothing public names a host, an address or a tool.
- [ ] **Step 4: Commit** as `docs: the matrix and channel counts in the design`.
