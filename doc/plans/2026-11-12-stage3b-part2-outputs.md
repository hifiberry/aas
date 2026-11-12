# Stage 3b, part 2: several outputs and adaptive resampling implementation plan

> The design, `doc/design.md`, is the authority: where this plan disagrees with it, the
> design wins.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps
> use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The daemon drives up to four cards at once. The card in the flat config is the
clock master. Every other card plays in time with it, each with its own matrix, plugin
chains, channel count, volume offset and idle behaviour, and is adaptively resampled to
follow the master's clock.

**Architecture:** The audio thread keeps its one wait, on the master card. Each cycle it
reads the sources once, mixes the master's output as today, and then mixes each further
output from the same buses through that output's own matrix, chains and gain. The result
goes into a wait-free ring per output. A writer thread per further output takes from its
ring, resamples adaptively to its card's clock and writes to its card. A drift controller
in the writer keeps the total delay of that path at a fixed target. The master is delayed
by a delay line to the same target, so every output plays a sample at the same moment.

**Tech Stack:** Rust (edition 2024, `rust-version` 1.85), `alsa` 0.12 behind the
`alsa-output` feature, `libc`, `rtrb`, `rubato` (already a dependency), `serde` with
`toml`. No new dependencies.

**Spec:** `doc/design.md`, sections Output, Mixing matrix, Sample rate rules,
Resampling, Bit-perfect, DoP, Volume, Latency, Monitoring and reporting, Failure
behaviour, Configuration. Measurements: `doc/measurements.md`.

## Stages

1. Measurement (done)
2. Core daemon and ALSA plugin (done)
3. a. Analog input and native plugins (done)
   b. Part 1: matrix and channel counts on one card (done).
      Part 2: several cards, adaptive resampling, a matrix and chains per card (this plan).
   c. `input-processor` as a native plugin
4. API, monitoring, graph export
5. Packaging, Bluetooth and the switch from PipeWire, with all players tested

No board has a second card with its own clock. The owner chose on 2026-11-12 to test
with `snd-aloop` only for now. Its `PCM Rate Shift 100000` control runs the loopback's
clock off by a set amount, so drift is real and measurable on the boards.

## Scope

In this part: `[[output]]` tables in the config, one writer thread per further output,
the adaptive resampler, the drift controller, alignment of all outputs to the master,
volume, idle, DoP and failure behaviour with several outputs, per-output counters in
the log, and measurements with `snd-aloop` on both boards.

Not in this part, and nothing here may block it:

| Later work | Stage |
|---|---|
| `GET`, `PATCH /outputs/{id}`, drift and ratio in `GET /metrics` | 4 |
| Choosing the clock master at run time | 4 |
| The analog input from a card other than the master | later |
| A real second card with its own clock on a board | when one is fitted |

## Decisions this plan makes

The owner decided the first four on 2026-11-12. The rest are calls. Task 9 adds all of
them to the design, the owner's as decided and the rest marked *(call)*.

- **Every output plays in time with the master** (owner). Each further output's path
  is held at a fixed total delay, and the master is delayed by the same amount through
  a delay line. With further outputs configured, the master's latency grows by that
  delay. Without them nothing changes.
- **The `Master` volume control moves all outputs together** (owner). Each further
  output keeps its own offset, `volume_db` in its `[[output]]` table, -60 to 0, 0 by
  default. A further output's volume is always calculated in software; its card's
  hardware mixer is left alone.
- **While the master plays a DoP stream the other outputs are silent** (owner).
- **If the master card is lost, the other outputs keep playing** (owner). The master's
  timed null sink keeps the cycle going, as it does today, and the writers follow it.
  A lost further output plays to its own null sink and is looked for every second, like
  the master. Neither ever stalls the other.
- **Config:** the flat keys stay the master. Each further output is an `[[output]]`
  table with `card` (required), `channels`, `idle`, `idle_delay_s`, `volume_db`, and
  its own `[[output.matrix]]` and `[[output.plugin]]` tables, with the same rules as
  the flat ones. With any `[[output]]` table the flat `card` is required. At most three
  `[[output]]` tables, so four outputs. No two outputs name the same card.
- **Only the master has the analog input.** `place = "analog"` and `detect` are an error
  in an `[[output]]` table. The analog bus reaches further outputs through their matrix
  like any source.
- **An `[[output]]` without a matrix plays what the master's default plays:** left to 1,
  right to 2, for both sources, at 0 dB.
- **One writer thread per further output,** named `aas-out-N` (N from 2), at
  `SCHED_FIFO` 65, one below the audio thread. The audio thread never waits for a
  writer. Its feed ring holds 4 master periods. A full ring drops the new period and
  counts an overflow; an empty one plays zeros and counts a starve. Old audio is never
  played twice.
- **A further output runs at the master's rate where its card takes it,** else at the
  nearest rate it takes, the lower in a tie. Its period is the master's period in time,
  the nearest power of two in frames, and it buffers 3 periods.
- **The adaptive resampler is rubato's asynchronous sinc with cubic interpolation
  between 256 phases.** This departs on purpose from the rule that no sample is
  interpolated between phases: an adaptive ratio is not a fixed fraction, so no fixed
  set of phases exists. It works in `f64` when the processing format is `f64`, else in
  `f32`, with 128 taps at `high` and 64 at `medium`, as the clients' resampler. It
  always runs on a further output, also at equal rates.
- **The drift controller** is a PI controller on the path's total delay: frames in the
  feed ring, in the resampler and in the card, against the target. It corrects an
  error over about 2 s, its integral over about 20 s. The ratio is clamped to
  ±2000 ppm; a card that needs more is logged once and plays on with starves or
  overflows counted. The output counts as locked once its error stays under a quarter
  period for 5 s.
- **The alignment target** is one master period for the feed ring, plus 3 periods of
  the further card converted to the master's rate, plus the resampler's delay, rounded
  up to a frame and taken over all further outputs. The master's delay line is the
  target less the master's 2 periods, at most 8192 frames.
- **Bit-perfect applies to the master only,** as the design says. The master's copy and
  DoP paths pass through the delay line unaltered. Further outputs always take the
  processing path.
- **Idle is per output.** A further output with `idle = "off"` closes its card after its
  delay when no source is active, discards its feed while closed, and relocks when it
  opens again.
- **Counters per output:** xruns, wake-up gaps, starves, overflows, ratio in ppm and
  lock. A line is logged when an output locks or loses lock, and all counters at stop.

## Global Constraints

- The audio thread does not allocate, take locks or make blocking calls during playback.
  Its one wait is the master card's. Writers, chains, resamplers and matrices are built
  on the control thread.
- With no `[[output]]` table, what reaches the master card is bit for bit what `main`
  sends at the stage 3b part 1 merge, in every processing format, and the master's
  latency is unchanged. The existing tests are the check, and stay.
- A further output never stalls the master, and the master never waits for one.
- A DoP stream reaches the master card unaltered or not at all, and never reaches a
  further output.
- A chain never runs at a rate, or on a channel count, it was not built for.
- Audio is never dithered. Float conversion scales by an exact power of two.
- A wake-up gap longer than the buffer counts as a failure even with 0 xruns.
- The workstation has no ALSA headers: `cargo test --workspace --exclude aas-alsa`
  there, and anything linking ALSA builds on a test board.
- Public text names no internal hosts or addresses, and no tools.

## Review Focus

1. A further output's card is unplugged while music plays. The master plays on
   without a gap, the further output goes to its null sink, and one line is logged.
   Task 4, `a_lost_further_output_never_stalls_the_master`.
2. The master's rate changes from 44.1 to 96 kHz while a further output plays. The
   further output reopens at its nearest rate, relocks, and plays no old audio.
   Task 7, `a_master_rate_change_reopens_every_further_output`.
3. A further card is 5000 ppm off. The ratio stops at 2000 ppm, one line is logged,
   and starves are counted without the master noticing. Task 3,
   `a_card_beyond_the_clamp_holds_the_clamp_and_says_so`.
4. A writer starts before the feed has audio, or the feed runs dry. It plays zeros,
   never a period it played before. Task 4, `an_empty_feed_plays_zeros_not_old_audio`.
5. The config names the same card twice, or has `[[output]]` without a flat `card`.
   The daemon stops at startup with a message naming both. Task 1,
   `two_outputs_on_one_card_are_refused`.

---

### Task 1: `[[output]]` tables in the config

**Files:**
- Modify: `src/config.rs`, `README.md` (the config table)
- Test: `src/config.rs` tests

**Interfaces:**
- Produces: `pub struct OutputConfig { pub card: String, pub channels: usize, pub idle:
  Idle, pub volume_db: f32, pub plugins: Vec<PluginConfig>, pub matrix:
  Option<Vec<Route>> }`; `Config.outputs: Vec<OutputConfig>` (the further outputs, in
  file order); `pub const MAX_OUTPUTS: usize = 4` (master included).

- [ ] **Step 1: Write the failing tests:** `an_output_table_takes_its_own_matrix_and_plugins`
  (two `[[output]]` tables with `[[output.matrix]]` and `[[output.plugin]]` land on the
  right output, in order); `an_output_without_a_card_is_refused`;
  `two_outputs_on_one_card_are_refused` (also the flat `card` against an `[[output]]`
  `card`; the message names the card and both outputs);
  `outputs_need_the_master_card_named`; `a_fifth_output_is_refused`;
  `analog_plugins_and_detect_are_refused_in_an_output`;
  `output_volume_db_is_minus_60_to_0_and_0_by_default`;
  `an_output_takes_idle_and_channels_by_the_flat_rules`; `no_output_table_leaves_outputs_empty`.
- [ ] **Step 2: Run them:** `cargo test --lib config` fails on the missing field.
- [ ] **Step 3: Parse `[[output]]`** through a `RawOutput` with `deny_unknown_fields`,
  reusing `route_from_table`, `plugin_from_table` and the idle and channel rules, and
  validate across outputs in `Config::load`.
- [ ] **Step 4: Run** `cargo test --workspace --exclude aas-alsa`: all pass.
- [ ] **Step 5: Document `[[output]]` in `README.md`** next to `[[matrix]]`.
- [ ] **Step 6: Commit** as `feat(config): further outputs in [[output]] tables`.

### Task 2: the adaptive resampler

**Files:**
- Create: `src/adaptive.rs`
- Modify: `src/lib.rs` (module)
- Test: `src/adaptive.rs` tests

**Interfaces:**
- Consumes: `Quality`, `Format` from the existing resampler and config.
- Produces: `pub struct Adaptive`; `Adaptive::new(from: u32, to: u32, channels: usize,
  max_out: u32, quality: Quality, format: Format) -> Adaptive` (allocates; control
  thread); `fn set_ratio(&mut self, relative: f64)` (relative to `to / from`, clamped
  to `1 ± MAX_DRIFT`); `fn input_needed(&self, out_frames: u32) -> u32`;
  `fn process(&mut self, input: &[i32], output: &mut [i32]) -> (usize, usize)`
  (interleaved S32 in and out, frames consumed and produced; no allocation);
  `fn delay_frames(&self) -> f64` (at the output rate); `fn reset(&mut self)`;
  `pub const MAX_DRIFT: f64 = 0.002`.

- [ ] **Step 1: Write the failing tests:**
  `a_tone_passes_at_ratio_one` (997 Hz, -6 dBFS, 48 kHz, 2 channels, 10 s: after the
  first second the residual against a fitted sine is below -100 dBFS in `f32` and
  below -110 dBFS in `f64`); `the_ratio_moves_the_output_rate` (at `set_ratio(1.001)`
  10 s of input give 0.1% more frames, within 2 frames);
  `the_ratio_is_clamped_at_max_drift`; `eight_channels_stay_apart` (a tone on channel 5
  only; every other channel stays below -140 dBFS); `process_does_not_allocate` (the
  counting allocator the engine tests use); `integer_samples_round_trip_by_a_power_of_two`
  (full-scale S32 in, no wrap, saturated).
- [ ] **Step 2: Run them:** `cargo test --lib adaptive` fails on the missing module.
- [ ] **Step 3: Implement** on rubato's asynchronous sinc with
  `SincInterpolationType::Cubic`, oversampling 256, the taps of `parameters()` for the
  quality, and room for `MAX_DRIFT` in its ratio headroom. Keep the float stage behind
  an enum as `Resampler` does.
- [ ] **Step 4: Run** the workspace tests: all pass.
- [ ] **Step 5: Commit** as `feat(resample): an adaptive resampler for further outputs`.

### Task 3: the drift controller

**Files:**
- Create: `src/drift.rs`
- Test: `src/drift.rs` tests

**Interfaces:**
- Produces: `pub struct Drift`; `Drift::new(target_frames: f64, rate: u32, period: u32)
  -> Drift`; `fn update(&mut self, measured_frames: f64) -> f64` (called once per writer
  period with the path's total delay at the master rate; returns the relative ratio for
  `Adaptive::set_ratio`); `fn ppm(&self) -> f64`; `fn locked(&self) -> bool`;
  `fn clamped(&self) -> bool`; `fn reset(&mut self)`.

- [ ] **Step 1: Write the failing tests** against a simulated card that drains at
  `rate × (1 + ppm)` while a feed fills at `rate`, period 256 at 48 kHz:
  `it_locks_on_a_card_100_ppm_fast` and `it_locks_on_a_card_300_ppm_slow` (locked within
  30 s, then the delay stays within a quarter period of target and `ppm()` within 5 ppm
  of the truth for 10 minutes); `a_one_period_step_recovers_without_overshooting_a_period`;
  `a_card_beyond_the_clamp_holds_the_clamp_and_says_so` (5000 ppm: ratio at the clamp,
  `clamped()` true, never locked); `reset_starts_unlocked`.
- [ ] **Step 2: Run them:** `cargo test --lib drift` fails.
- [ ] **Step 3: Implement** the PI controller with the time constants in the decisions,
  computed from `rate` and `period` so they hold at every rate.
- [ ] **Step 4: Run** the workspace tests: all pass.
- [ ] **Step 5: Commit** as `feat(drift): follow the master's clock with a PI controller`.

### Task 4: the writer of a further output

**Files:**
- Create: `src/further.rs`
- Modify: `src/sink.rs` (`delay_now`), `src/sink_alsa.rs` (`delay_now` from
  `snd_pcm_delay`), `src/counters.rs` (`OutputCounters`)
- Test: `src/further.rs` tests

**Interfaces:**
- Consumes: `Adaptive` (Task 2), `Drift` (Task 3), `Sink`, `FallbackSink`.
- Produces: `trait Sink { fn delay_now(&mut self) -> Result<u32, SinkError> }`, default
  `Ok(self.delay_frames())`, the frames queued in the card now; `pub struct Feed` (audio
  side) with `fn push(&mut self, samples: &[i32]) -> bool` (wait-free; false and an
  overflow counted when full) and `fn silence(&mut self, frames: u32) -> bool`;
  `pub struct Writer` with `fn cycle(&mut self) -> Result<(), SinkError>` (one card
  period: `wait_room`, take from the feed or zeros, resample, write, measure, update the
  drift); `pub struct FurtherHandle` (control side) with `fn reconfigure(&self,
  master_rate: u32, card_rate: u32, period: u32, resampler: Box<Adaptive>, target:
  f64)`, `fn set_open(&self, open: bool)`, `fn search(&self) -> CardSearch`;
  `pub fn spawn(index: usize, sink: FallbackSink, feed_frames: usize, channels: usize,
  counters: Arc<OutputCounters>) -> (Feed, FurtherHandle)` (starts `aas-out-N` at
  `SCHED_FIFO` 65); `pub struct OutputCounters` with `xruns`, `wake_gaps`,
  `worst_gap_ns`, `starves`, `overflows`, `ppm_milli` (`AtomicI64`), `locked`.

- [ ] **Step 1: Write the failing tests,** driving `Writer::cycle` on the test thread
  with a simulated card whose `wait_room` advances a virtual clock and whose
  `delay_now` drains at a set ppm, and a feed filled on the same virtual clock:
  `a_further_output_locks_and_holds_its_target`;
  `an_empty_feed_plays_zeros_not_old_audio`; `a_full_feed_drops_the_new_period_and_counts_it`;
  `a_lost_further_output_never_stalls_the_master` (the sink fails `Gone`; `push` keeps
  returning at once, the writer plays to its null sink, one event for the control
  thread); `a_closed_writer_discards_its_feed`; `writer_cycle_does_not_allocate`.
- [ ] **Step 2: Run them:** `cargo test --lib further` fails.
- [ ] **Step 3: Implement.** The commands from `FurtherHandle` reach the writer through
  its own `rtrb` ring and are read after its wait, as the engine reads its own. Old
  resamplers go back to the control thread to be dropped.
- [ ] **Step 4: Run** the workspace tests: all pass. Build with `alsa-output` on the
  CM5: no warnings.
- [ ] **Step 5: Commit** as `feat(output): a writer thread per further output`.

### Task 5: one mix per output in the engine

A refactor with no change in behaviour, so Task 6 adds outputs without touching the
master's path.

**Files:**
- Create: `src/output_mix.rs`
- Modify: `src/engine.rs`
- Test: existing engine tests; one new test

**Interfaces:**
- Produces: `pub struct OutputMix` holding what is now per output in `Engine`: the
  matrix, gains, ramp, routes, DoP routing, `mix` and `output` chains, gain and fade,
  channel count and the wide buffers; `fn mix(&mut self, players: &[..], analog:
  Option<&[..]>, out: &mut [i32], period: usize)` for the processing path. `Engine`
  keeps the sink, sources, analog input, claims and copy path, and holds
  `master: OutputMix`.

- [ ] **Step 1: Write** `the_master_mix_is_unchanged_by_the_split`: the stage 3b part 1
  byte-identity probe (60 cycles × `i32`, `f32`, `f64`, bit-perfect off and auto) in
  tree, with its expected output recorded from the merge commit before the change.
- [ ] **Step 2: Run it:** it passes before the change (it pins today's output).
- [ ] **Step 3: Move the per-output state and the matrix pass into `OutputMix`.**
- [ ] **Step 4: Run** the workspace tests and the new test: all pass, the probe
  byte-identical, `tests/cycle_cost.rs` within 2% of before on the workstation.
- [ ] **Step 5: Commit** as `refactor(engine): the output's mix in a struct of its own`.

### Task 6: the engine feeds further outputs

**Files:**
- Modify: `src/engine.rs`, `src/output_mix.rs`
- Test: engine tests

**Interfaces:**
- Consumes: `OutputMix` (Task 5), `Feed` (Task 4).
- Produces: `pub struct Target { pub output: usize, pub place: Place }` with `output` 0
  for the master; `Command::SetChain(Target, Box<Chain>)`, `Command::SetMatrix(usize,
  Box<Matrix>)`, `Command::SetGain(usize, f32)`, `Command::AddOutput(usize,
  Box<OutputMix>, Feed)`, `Command::SetDelay(u32)` (the master's delay line, in frames);
  `Event::OldChain` and `Event::OldMatrix` carry the output index; `Engine::new` takes
  the delay line's capacity.

- [ ] **Step 1: Write the failing tests:** `every_output_gets_the_same_mix_by_default`;
  `a_further_matrix_routes_on_its_own` (a swap on output 2 only);
  `further_outputs_are_silent_while_dop_plays` (the master gets the DoP stream
  unaltered, every feed gets zeros); `the_delay_line_delays_by_exact_frames` (an
  impulse, also on the copy path and with DoP, arrives exactly `n` frames late and
  bit for bit); `a_full_feed_never_waits` (feeds that refuse every push: the master's
  output is unchanged); `feeding_outputs_does_not_allocate`;
  `a_chain_for_output_2_never_runs_on_output_1`.
- [ ] **Step 2: Run them:** they fail on the missing commands.
- [ ] **Step 3: Implement.** Sources are read once per cycle. Each further output mixes
  from the same `players` and `analog` buses through its own `OutputMix`, applies its
  gain and pushes one master period to its feed. The copy and DoP paths serve the
  master only; while DoP plays every feed gets `silence`. The delay line is
  preallocated and sits just before `sink.write`.
- [ ] **Step 4: Run** the workspace tests: all pass, the Task 5 probe still
  byte-identical with no further output.
- [ ] **Step 5: Commit** as `feat(engine): mix every output and feed the further ones`.

### Task 7: the control thread drives several outputs

**Files:**
- Modify: `src/control.rs`, `src/server.rs`, `src/main.rs`, `src/volume.rs`
- Test: `src/control.rs` tests, `tests/server.rs`

**Interfaces:**
- Consumes: `OutputConfig` (Task 1), `Adaptive` (2), `spawn` and `FurtherHandle` (4),
  the commands of Task 6.
- Produces: `fn align_frames(master_rate: u32, master_period: u32, further: &[(u32,
  u32, f64)]) -> u32` (card rate, card period, resampler delay → the target in master
  frames); `Control` builds chains, matrices and resamplers per output and reconfigures
  every further output after each master `Reconfigured`.

- [ ] **Step 1: Write the failing tests:**
  `a_master_rate_change_reopens_every_further_output` (each gets `reconfigure` at its
  nearest rate with a fresh resampler, after the master's `Reconfigured`);
  `master_volume_moves_every_output_with_its_offset` (Master at 50% with an output at
  -6 dB: that output's gain is the software gain of 50% less 6 dB, the master's as
  before); `the_master_is_delayed_to_the_further_outputs_target`;
  `no_further_output_means_no_delay`; `a_further_output_goes_off_by_its_own_idle_rule`;
  `a_further_chain_that_cannot_build_at_the_rate_silences_that_output_only`;
  `a_lost_further_card_is_looked_for_every_second` (`tests/server.rs`).
- [ ] **Step 2: Run them:** they fail.
- [ ] **Step 3: Implement.** `main.rs` opens an `AlsaSink` per `[[output]]` card,
  wraps each in a `FallbackSink` and spawns its writer before the audio thread starts.
  The card search covers every output. The rate rules stay the master's; a rate the
  master takes but a further card does not is played at that card's nearest rate,
  through its resampler.
- [ ] **Step 4: Run** the workspace tests: all pass. Build on the CM5 with
  `alsa-output`; with no `[[output]]` the daemon logs exactly what it logged before.
- [ ] **Step 5: Log per output:** a line when an output locks or loses lock (with its
  ppm), and every output's counters at stop.
- [ ] **Step 6: Commit** as `feat(control): several outputs follow the master`.

### Task 8: measurements on the test boards

`snd-aloop` is the further output. Its `PCM Rate Shift 100000` control sets the drift:
100000 is none, 100010 is +100 ppm.

**Files:**
- Modify: `tools/measure` (an `align-check` binary), `dist/test-plugin.sh`,
  `doc/measurements.md`

- [ ] **Step 1: Lock on the CM5** with the DAC as master and the loopback as further
  output, at 0, +100, -300 and +1000 ppm and at 44.1 and 96 kHz: time to lock, the
  reported ppm against the set shift, and the delay error after lock. At 5000 ppm the
  clamp holds and is logged.
- [ ] **Step 2: Bit-check the further output** at a set shift of 0 with `bit-check`
  in its resampled mode: the tone reaches the loopback with the residual of Task 2's
  test, and the swap matrix of that output only swaps there.
- [ ] **Step 3: Alignment on the CM5.** `align-check` records the ADC through the cable
  and the loopback capture at once with hardware timestamps, finds a click in both,
  and reports the offset. The expected offset is the capture path's own delay from
  stage 3a. Report how far the measured offset is from it.
- [ ] **Step 4: One hour at 44.1 kHz and one at 96 kHz under full load on each board,**
  master on the DAC with the analog input on, one further output on the loopback at
  +100 ppm: master 0 xruns and no wake-up gap over the buffer; the further output
  locked throughout, 0 xruns, no starve or overflow after lock.
- [ ] **Step 5: Cost on the Pi 3** at 48 kHz, `f32` and `f64`: the audio thread per
  cycle with and without the further output, and the writer thread's CPU time.
- [ ] **Step 6: Write the figures into a "Stage 3b, part 2" section** of
  `doc/measurements.md`, failures included, and leave the boards as found.
- [ ] **Step 7: Commit** as `docs: record the stage 3b part 2 measurements`.

### Task 9: the design and the open list

**Files:**
- Modify: `doc/design.md`, `README.md`, `TODO.md`

- [ ] **Step 1: Add every entry of "Decisions this plan makes" to `doc/design.md`,** in
  Output, Mixing matrix, Resampling, Volume, Latency, DoP, Failure behaviour, Monitoring
  and Configuration: the owner's four as decided, the rest marked *(call)*. Replace
  "Built so far is one output" with what is built. Add anything a task decided that is
  not listed here, and the measured cost and latency.
- [ ] **Step 2: Update `TODO.md`:** remove what this part built, and add entries for a
  real second card on a board, choosing the master at run time, and the analog input
  from another card.
- [ ] **Step 3: Check that `README.md` documents `[[output]]`,** and that nothing public
  names a host, an address or a tool.
- [ ] **Step 4: Commit** as `docs: several outputs in the design`.
