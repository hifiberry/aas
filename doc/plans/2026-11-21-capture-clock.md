# A master clocked by a capture card Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `aas` runs on a card that only captures, such as vinyl's HiFiBerry
ADC. The capture's clock drives the audio thread. The master's mix plays
nowhere, but reaches the monitor, the further outputs and, later, AirPlay.

**Architecture:** A new `Sink`, `ClockSink`, has no playback device. Its
`wait_room` waits until the capture card has a period, and its `write`
discards. It owns the capture device and hands the engine's analog input a
`ClockCapture` that reads the same device, so the input needs no second open.
The engine is unchanged: its only wait is `wait_room`, and the analog input
already reads "the latest period" each cycle.
- A cycle with no reader still takes a period off the device, so the clock
  never runs ahead.
- The `FallbackSink` around the master keeps time with its timer while the
  card is gone, as for a playback card.

**Spec:** this conversation, 2026-11-21: "clock on a capture card must also be
possible. implement this first" (before the AirPlay output,
`doc/plans/2026-11-21-airplay-output.md`). The evidence is vinyl's
`/proc/asound/pcm`: `02-00: HiFiBerry ADC HiFi pcm1863-aif-0 : … : capture 1`,
with no playback device on the card.

## Global Constraints

- Config: `clock = "auto" | "playback" | "capture"`, top level, default
  `"auto"`.
  - `auto` picks the capture clock when the master card has a capture device
    and no playback device, by `/proc/asound/pcm`. Otherwise it picks
    playback, which includes a card that is not there at the start.
  - `capture` forces it on a card that has both, for testing on a DAC+ ADC,
    whose DAC then stays closed.
  - The error is `clock must be "auto", "playback" or "capture"`.
- With the capture clock the volume is software, whatever `volume_mode` says.
  `volume_mode = "hardware"` or `"follow"` gives one warning.
- The capture is opened as the analog input opens it now: S32, else S24 in
  32 bits, else S16, a buffer of four periods, and the first `SETTLE_MS` read
  as zeros.
- `snd_rpi_hifiberry_adc` joins `cards::CARDS` with `adc: true`, taken from
  vinyl's `/proc/asound/cards`.
- `GET /outputs/1` gains `"clock": "capture"`, which is left out under the
  playback clock.
- The log at start reads `aas: the output is clocked by the capture of hw:<card>; it plays nowhere`.

## Review Focus

1. **The analog input disabled under the capture clock.** Nothing reads, and
   the cycles must still come once a period. The pin is the ClockSink test
   `a_cycle_nobody_reads_takes_a_period`.
2. **A capture overrun from a late cycle.** It recovers, is counted as an
   xrun, and the next cycle waits a whole period. The pin is
   `an_overrun_is_an_xrun_and_the_next_wait_is_a_period`.
3. **A rate change.** The device reopens at the new rate, and a rate the ADC
   does not take is never chosen, because `supported_rates` gives the
   capture's rates. The pin is `rates_are_the_captures`.
4. **The card gone and back.** `wait_room` says `Gone`, and `FallbackSink`
   keeps time. The pin is `a_gone_device_is_gone_and_a_reopen_works`.

---

### Task 1: `ClockSink` and `ClockCapture` (`src/clock_sink.rs`)

**Interfaces:**
- Produces:
  - `pub trait ClockDevice: Capture`, with:
    - `fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError>`
    - `fn wait(&mut self, frames: usize) -> Result<(), SinkError>`: until `available() >= frames`; `Err(Xrun)` after recovering from an overrun
    - `fn rates(&self) -> Vec<u32>`
    - `fn is_open(&self) -> bool`
  - `pub struct ClockSink`:
    - `ClockSink::new(device: Box<dyn ClockDevice>, channels: usize) -> ClockSink`
    - `ClockSink::capture(&self) -> ClockCapture`
  - `pub struct ClockCapture`, which implements `Capture`:
    - reads, skips and counts what it took this cycle;
    - `available` is 0 while the device is closed;
    - `close` leaves the device alone, since the sink owns it.

In `write`, the frames taken this cycle (reads and skips) are compared with the
period; when short, the rest is skipped, at most what is available. The count
then starts over.

- [ ] Tests, written first with a `FakeDevice`:
  - `wait_room_waits_for_a_period`
  - `a_cycle_nobody_reads_takes_a_period`
  - `a_cycle_the_input_reads_takes_nothing_more`
  - `an_overrun_is_an_xrun_and_the_next_wait_is_a_period`
  - `rates_are_the_captures`
  - `a_gone_device_is_gone_and_a_reopen_works`
  - `the_capture_reads_nothing_while_closed`
  - `closing_the_capture_leaves_the_device_open`
  - `delay_is_zero_and_channels_are_the_mix`
- [ ] Commit: `feat: a sink clocked by a capture card`.

### Task 2: The ALSA device and the choice of clock

**Files:** `src/sink_alsa.rs`, `src/cards.rs`, `src/config.rs`, `src/main.rs`,
`src/api.rs`, `src/control/api.rs`, `README.md`.

**Interfaces:**
- Produces:
  - `pub fn card_directions(pcm_text: &str, card: u32) -> (bool, bool)`: playback, capture
  - `pub fn capture_clocked(clock: Clock, directions: Option<(bool, bool)>) -> bool`
  - `AlsaClock` (an ALSA `ClockDevice`, built on `AlsaCapture` plus `wait`)
  - `Config.clock: Clock`

- [ ] Tests, written first:
  - `card_directions_reads_vinyls_pcm_list` and `…_a_dacplus`, from the real texts of vinyl and the Pi 4.
  - `auto_is_capture_only_on_a_capture_only_card`, `forced_capture_on_a_card_with_both`, `an_absent_card_is_played_to`.
  - `clock_is_parsed_and_checked`.
  - `the_hifiberry_adc_has_an_adc`.
  - The API: `the_master_says_its_clock_when_it_is_the_capture`.
- [ ] Wire `main.rs`:
  - under the capture clock, the master sink is `ClockSink(AlsaClock)`;
  - the analog input's open gives `ClockCapture`;
  - `hardware_volume` is `None`.
- [ ] Commit: `feat: clock = "capture", chosen on its own for a card that only captures`.

### Task 3: On the devices

- [ ] On the CM5 test board (DAC+ ADC Pro), `clock = "capture"`:
  - the analog input reaches `aas.monitor` (`parecord`);
  - the meters move;
  - no xruns in 10 minutes;
  - a further output, if one is there, plays in time.
- [ ] On vinyl, with the owner's go only, since it is the household's turntable:
  - `aas` with `clock = "auto"` picks the capture;
  - the monitor carries the record.
- [ ] Record the results in `doc/measurements.md`.
