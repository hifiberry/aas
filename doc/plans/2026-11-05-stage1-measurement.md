# Stage 1: measurement implementation plan

**Goal:** Measure what the hardware can do, so the sample format and default
latency are decided from evidence before the engine and plugin interface are
built.

**Architecture:** A separate workspace crate, `tools/measure`, holds small
command-line probes. Each probe answers one question and prints a result that can
be copied into `doc/measurements.md`. Nothing here is the daemon. Later stages may
reuse the sample conversion code but do not depend on this crate.

**Tech Stack:** Rust (edition 2024, `rust-version` 1.85), `alsa` crate, `libc`,
`rubato` for resampling, `num-traits` for the `Float` bound, `clap` for arguments.

**Spec:** `doc/design.md`

## Stages

1. Measurement (this plan)
2. Core daemon and ALSA plugin
3. Analog input, plugins, matrix, multiple outputs
4. API, monitoring, graph export
5. Packaging, Bluetooth and the switch from PipeWire, with all players tested

Each stage gets its own plan, written once the one before has finished.

## Questions this stage answers

| # | Question | Probe |
|---|---|---|
| 1 | Is 64-bit float affordable for mixing and volume? | `dsp-bench` |
| 2 | Is 64-bit float affordable for resampling? | `dsp-bench` |
| 3 | What period size runs one hour with no xruns? | `period-probe` |
| 4 | What does a client-to-daemon handover cost in wake-up time? | `handoff-probe` |
| 5 | What is the real latency at the analog output? | `loopback-latency` |

## Global Constraints

- Latency target: below 4 ms on Pi 4 and Pi 5, from the plugin receiving audio to
  the audio leaving the card. More is fine on a Pi 3, with the figure set by
  measurement.
- Pass criterion: one hour of playback at 44.1 kHz and at 96 kHz with no xruns, on
  an otherwise idle system.
- The kernel is standard `PREEMPT`, not `PREEMPT_RT`.
- Target: Debian trixie, 64-bit, aarch64.
- Output format S32 on cards that support it.
- The audio loop in any probe does not allocate, lock or block other than on ALSA
  or the wake-up it measures.
- This repository is public: no machine names or internal addresses in any file,
  including `doc/measurements.md`. Use "the test board".

## Review Focus

- A probe that cannot get real-time priority keeps running and reports misleading
  figures: it must exit with an error unless `--no-rt` is given. Pinned in Task 3.
- `snd_pcm_recover` hides an xrun so the run looks clean: every recovery must be
  counted. Pinned in Task 3.
- Interrupted runs report as complete: the printed result must state the planned
  and the actual duration. Pinned in Task 1.
- A cold first cycle or page faults dominate the worst case: probes discard a
  warm-up period and call `mlockall`. Pinned in Task 1 and Task 3.
- The loopback measurement locks onto the wrong peak with a quiet or disconnected
  cable: it must report no signal, not a number. Pinned in Task 5.

---

### Task 1: measure crate and result statistics

**Files:**
- Modify: `Cargo.toml` (add `[workspace]` with members `.` and `tools/measure`)
- Create: `tools/measure/Cargo.toml`
- Create: `tools/measure/src/lib.rs`
- Create: `tools/measure/src/stats.rs`

**Interfaces:**
- Produces: `measure::stats::Stats` with `Stats::with_warmup(discard: usize) ->
  Stats`, `record(&mut self, nanos: u64)`, `summary(&self) -> Summary`.
- Produces: `Summary { count: u64, min: u64, mean: u64, p99: u64, p999: u64, max:
  u64 }`.
- Produces: `measure::stats::report_header(planned: Duration, actual: Duration) ->
  String`.
- Produces: `measure::rt::lock_memory() -> Result<(), String>` and
  `measure::rt::set_fifo(priority: i32) -> Result<(), String>`.

- [ ] **Step 1: Write the failing tests** in `stats.rs`

```rust
#[test]
fn warmup_samples_are_discarded() {
    let mut s = Stats::with_warmup(2);
    for v in [1_000_000, 1_000_000, 10, 20, 30] { s.record(v); }
    let r = s.summary();
    assert_eq!((r.count, r.min, r.max), (3, 10, 30));
}

#[test]
fn percentiles_use_recorded_values() {
    let mut s = Stats::with_warmup(0);
    for v in 1..=1000 { s.record(v); }
    let r = s.summary();
    assert_eq!((r.p99, r.p999), (990, 999));
}

#[test]
fn header_shows_an_interrupted_run() {
    let h = report_header(Duration::from_secs(3600), Duration::from_secs(10));
    assert!(h.contains("INCOMPLETE"));
    assert!(!report_header(Duration::from_secs(5), Duration::from_secs(5))
        .contains("INCOMPLETE"));
}
```

- [ ] **Step 2: Run `cargo test -p measure`.** Expected: FAIL, the types do not exist.

- [ ] **Step 3: Implement `Stats`, `Summary`, `report_header` and `rt`.** `Stats`
  preallocates a log-spaced histogram so `record` never allocates, with exact
  buckets at the sizes the tests use. `rt` wraps `mlockall(MCL_CURRENT |
  MCL_FUTURE)` and `sched_setscheduler(SCHED_FIFO)`, returning the OS error text.

- [ ] **Step 4: Run `cargo test -p measure`.** Expected: PASS.

- [ ] **Step 5: Commit** as `feat(measure): add result statistics and real-time setup`.

### Task 2: dsp-bench

Answers questions 1 and 2.

**Files:**
- Create: `tools/measure/src/sample.rs`
- Create: `tools/measure/src/bin/dsp-bench.rs`

**Interfaces:**
- Consumes: `Stats`, `report_header`.
- Produces: `measure::sample::s32_to_f64(i32) -> f64`, `f64_to_s32(f64) -> i32`,
  and `f32` equivalents. Scale by exactly 2^31, no dither, saturate at the S32
  limits.
- Produces: `measure::sample::mix_gain<T: Float>(inputs: &[&[T]], gains: &[T], out:
  &mut [T])`.

- [ ] **Step 1: Write the failing tests** in `sample.rs`

```rust
#[test]
fn f64_round_trip_is_exact() {
    for v in [i32::MIN, -1, 0, 1, 0x7FFF_FF01, i32::MAX] {
        assert_eq!(f64_to_s32(s32_to_f64(v)), v);
    }
}

#[test]
fn f32_round_trip_loses_bits() {
    assert_ne!(f32_to_s32(s32_to_f32(0x7FFF_FF01)), 0x7FFF_FF01);
}

#[test]
fn conversion_saturates_above_full_scale() {
    assert_eq!(f64_to_s32(1.5), i32::MAX);
    assert_eq!(f64_to_s32(-1.5), i32::MIN);
}

#[test]
fn unity_mix_of_one_input_is_exact() {
    let input: Vec<f64> = [i32::MIN, 1, i32::MAX].map(s32_to_f64).to_vec();
    let mut out = vec![0.0; 3];
    mix_gain(&[&input], &[1.0], &mut out);
    assert_eq!(out, input);
}
```

- [ ] **Step 2: Run `cargo test -p measure sample`.** Expected: FAIL.

- [ ] **Step 3: Implement `sample.rs`.**

- [ ] **Step 4: Run `cargo test -p measure sample`.** Expected: PASS.

- [ ] **Step 5: Implement `dsp-bench`.** For `f32` and `f64`, for each of 2, 4 and 8
  stereo sources, and for periods of 32, 64, 128 and 256 frames: time convert, mix,
  convert back over 100,000 cycles. Then time `rubato` synchronous sinc resampling,
  44.1 to 48 kHz and 48 to 96 kHz, stereo, at a high and a medium quality setting.
  Print each result as a percentage of the period's time budget, using the worst
  case and p99.9.

- [ ] **Step 6: Run `cargo run --release -p measure --bin dsp-bench`.** Expected: a
  table with one row per combination and no zero timings.

- [ ] **Step 7: Commit** as `feat(measure): compare 32 and 64-bit float processing cost`.

### Task 3: period-probe

Answers question 3.

**Files:**
- Create: `tools/measure/src/bin/period-probe.rs`
- Create: `tools/measure/src/xrun.rs`

**Interfaces:**
- Consumes: `Stats`, `report_header`, `rt`.
- Produces: `measure::xrun::Counter` with `on_write_result(&mut self, result: i32)
  -> Action`, `count(&self) -> u64`, and `enum Action { Continue, Recover, Fatal }`.

Command line: `period-probe --device hw:0,0 --rate 48000 --period 64 --periods 2
--seconds 3600 [--no-rt]`.

- [ ] **Step 1: Write the failing tests** in `xrun.rs`

```rust
#[test]
fn every_epipe_is_counted_and_recovered() {
    let mut c = Counter::default();
    assert_eq!(c.on_write_result(-libc::EPIPE), Action::Recover);
    assert_eq!(c.on_write_result(-libc::EPIPE), Action::Recover);
    assert_eq!(c.count(), 2);
}

#[test]
fn a_successful_write_is_not_an_xrun() {
    let mut c = Counter::default();
    assert_eq!(c.on_write_result(64), Action::Continue);
    assert_eq!(c.count(), 0);
}

#[test]
fn a_suspended_or_unknown_error_is_fatal() {
    let mut c = Counter::default();
    assert_eq!(c.on_write_result(-libc::ENODEV), Action::Fatal);
}
```

- [ ] **Step 2: Run `cargo test -p measure xrun`.** Expected: FAIL.

- [ ] **Step 3: Implement `xrun.rs` and `period-probe`.** Open the device S32,
  interleaved, stereo, with exactly the requested period and count, and fail if the
  card changes them. Call `lock_memory`, then `set_fifo(70)`. Without `--no-rt`, a
  failure of either exits non-zero with the OS error. Write silence in a loop and
  record the time between wake-ups. Discard the first second. Print the header,
  xrun count and wake-up statistics.

- [ ] **Step 4: Run `cargo test -p measure xrun`.** Expected: PASS.

- [ ] **Step 5: Check the failure path.** As a user with no real-time rights, run
  `period-probe --device hw:0,0 --seconds 5`. Expected: non-zero exit naming the
  scheduling error.

- [ ] **Step 6: Commit** as `feat(measure): add period and xrun probe`.

### Task 4: handoff-probe

Answers question 4, which is the cost the client-to-daemon design adds.

**Files:**
- Create: `tools/measure/src/bin/handoff-probe.rs`

**Interfaces:**
- Consumes: `Stats`, `report_header`, `rt`.

Command line: `handoff-probe --period-us 1333 --seconds 60 [--no-rt]`.

- [ ] **Step 1: Implement `handoff-probe`.** Fork a child. Parent and child share a
  `memfd` mapping holding a monotonic timestamp and a sequence number. The parent is
  `SCHED_FIFO` 70 and the child 60, matching daemon and client. The parent writes
  the timestamp, then signals an `eventfd`, every `--period-us`. The child wakes,
  reads the clock, and records the difference. Report wake-up latency from the child.

- [ ] **Step 2: Run for 60 seconds on the workstation.** Expected: statistics with a
  non-zero mean, and a sequence gap count of 0.

- [ ] **Step 3: Commit** as `feat(measure): measure client to daemon wake-up cost`.

### Task 5: loopback-latency

Answers question 5.

**Files:**
- Create: `tools/measure/src/correlate.rs`
- Create: `tools/measure/src/bin/loopback-latency.rs`

**Interfaces:**
- Consumes: `report_header`, `rt`.
- Produces: `measure::correlate::find_delay(reference: &[f64], captured: &[f64],
  min_peak_ratio: f64) -> Option<usize>`. Returns `None` when the strongest
  correlation peak is below `min_peak_ratio` times the next strongest.

Command line: `loopback-latency --device hw:0,0 --rate 48000 --period 64 --periods 2
--repeats 20`.

- [ ] **Step 1: Write the failing tests** in `correlate.rs`

```rust
#[test]
fn finds_a_known_delay() {
    let reference = test_chirp(256);
    let mut captured = vec![0.0; 1024];
    captured[300..556].copy_from_slice(&reference);
    assert_eq!(find_delay(&reference, &captured, 2.0), Some(300));
}

#[test]
fn silence_reports_no_signal() {
    assert_eq!(find_delay(&test_chirp(256), &vec![0.0; 1024], 2.0), None);
}

#[test]
fn noise_reports_no_signal() {
    assert_eq!(find_delay(&test_chirp(256), &seeded_noise(1024), 2.0), None);
}
```

`test_chirp` and `seeded_noise` are private test helpers, the second using a fixed
seed so the test is repeatable.

- [ ] **Step 2: Run `cargo test -p measure correlate`.** Expected: FAIL.

- [ ] **Step 3: Implement `correlate.rs` and `loopback-latency`.** Open playback and
  capture linked with `snd_pcm_link`. Play the chirp while capturing, find the
  delay, and print each repeat in frames and milliseconds, plus minimum, mean and
  maximum. If any repeat returns `None`, exit non-zero with "no signal: check the
  cable and input level".

- [ ] **Step 4: Run `cargo test -p measure correlate`.** Expected: PASS.

- [ ] **Step 5: Commit** as `feat(measure): measure real latency through a cable loop`.

### Task 6: run on the test board and record the results

**Files:**
- Create: `doc/measurements.md`

Steps 1 to 3 change the test board, which the owner has said may be reinstalled.
Step 4 needs the owner.

- [ ] **Step 1: Prepare the board.** In `/boot/firmware/config.txt`, replace
  `dtoverlay=hifiberry-dacplus-std` with `dtoverlay=hifiberry-dacplusadcpro`, then
  reboot. Expected: `aplay -l` and `arecord -l` both list the card. Install
  `build-essential`, `pkg-config`, `libasound2-dev` and Rust through rustup.

- [ ] **Step 2: Grant real-time rights.** Add a file in `/etc/security/limits.d/`
  with `rtprio 95` and `memlock unlimited` for the `audio` group. Log in again.
  Expected: `ulimit -r` prints 95.

- [ ] **Step 3: Run the probes.** Run each one in a file-logged background job:
  `dsp-bench`, then `handoff-probe --seconds 600`, then `period-probe` for periods of
  32, 64 and 128 frames for 10 minutes each at 48 kHz. For the smallest period with
  no xruns, run the full pass criterion: one hour at 44.1 kHz with a 64 frame period,
  and one hour at 96 kHz with a 128 frame period.

- [ ] **Step 4: Run the cable test.** Ask the owner to connect the DAC output to the
  ADC input. Run `loopback-latency` at the chosen period. Expected: 20 repeats within
  a few frames of each other.

- [ ] **Step 5: Write `doc/measurements.md`.** Give the board, kernel, each probe's
  figures and these conclusions: float format (32 or 64-bit, with the numbers that
  decide it), default period per rate, whether the 4 ms budget holds with the
  handover cost added, and whether `PREEMPT_RT` is needed.

- [ ] **Step 6: Commit** as `docs: record stage 1 measurements`.

### Task 7: repeat on Pi 3 and Pi 4

Needs test systems the project does not have yet, so it is blocked until the owner
provides them.

**Files:**
- Modify: `doc/measurements.md`
- Modify: `doc/design.md` (float format and per-board default latency)

- [ ] **Step 1: Repeat Task 6 steps 1 to 3 on each board.**

- [ ] **Step 2: Add the figures to `doc/measurements.md`.** Set the Pi 3 default
  latency, and confirm or reject 4 ms on the Pi 4.

- [ ] **Step 3: Update `doc/design.md`** with the final float format and the
  per-board defaults, for the owner to approve.

- [ ] **Step 4: Commit** as `docs: record Pi 3 and Pi 4 measurements`.
