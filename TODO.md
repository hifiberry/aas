# TODO

## Measure the Pi 4

The stage 1 measurements on the CM5 and Pi 3 are done, see `doc/measurements.md`.
The Pi 4 still needs a test board. Then run the same probes, confirm or reject 4 ms,
and confirm that `high` resampling fits. Without a Pi 4, the design's 4 ms claim
for it stays unconfirmed.

## Shorten the outage after a late DMA interrupt on the CM5

Found with `tone-probe` and `stall`, see "Late wake-ups and the tone" in
`doc/measurements.md`. On the CM5 test board, so on the RP1 and the Pi 5 too, a
late DMA interrupt can stop the card with no ALSA error. The kernel logs
`dma dma2chan2: Bad descriptor submitted`, the daemon's 500 ms wait for the card
runs out, a restart of the stream does not bring the card back, and the card is
reopened a second later. The tone is gone for about 1.5 s. It happened 76 times
in the test, each time after a stall made on purpose by firing thousands of
timers on the CPU that serves the card's interrupt. It did not happen in 15
minutes of full load, or on the Pi 3, which has another DMA controller.

Two things to do. In the daemon: reopen the card at once when the wait times out,
and judge whether 500 ms can be shorter, so the outage is a fraction of a second.
In the kernel: find why the descriptor is refused after a late interrupt, in the
RP1 DMA driver, and report it. First step: `stall --mode irq` on the CM5 while
`tone-probe` runs, with the kernel log streamed off the board.

## Measure the sound of medium resampling

`medium` is no longer a default, and since the resampler uses exact phases a Pi 3
no longer needs it to fit. It stays a config option. Its cost is measured, its
sound is not. Measure its passband and stopband against `high`.

## Find why the CM5 test board stopped responding under load

The first attempt at the stage 2 hour runs on the CM5 ended about 90 seconds into
the 44.1 kHz run under full load: the SSH server reset every connection before the
banner, the logs on the NVMe drive stopped, and the board stayed that way
until it was restarted 7 hours later. It had shown the same symptom earlier in
stage 2. 15 minutes of the load alone and 4 hours of the runs did not repeat it,
and no log of the hung boot survived, so it is not known whether the drive, the
load, the daemon or the board is at fault. First step: make the journal persistent
or stream the kernel log off the board (`dmesg -w` over SSH, as the later runs
did), keep the run's own logs in RAM, and repeat the hour runs until it happens
again. See "Hour runs" in `doc/measurements.md`.

In the same runs, one wake-up gap of 4.01 ms against a 2.67 ms buffer at 96 kHz
fell within 5 ms of the end of a 128 MB synchronous write, and the repeated run
had none. The trace planned for the Pi 3 below would show whether the drive's
completion work delays the audio thread on the CM5 too.

## Find where the long daemon cycles on a throttled Pi 3 come from

In the stage 2 hour runs on the Pi 3 under full load, throttled to about 550 MHz,
the daemon's worst cycle was 4.65 ms at 96 kHz and 7.91 ms at 44.1 kHz with a
resampled second client, for work that stage 1 timed at well under a millisecond.
The runs passed, and the 7.91 ms cycle gave no xrun. But the audio loop now waits
for room before it works, so its work has only the one queued period, 5.3 to 5.8 ms
there, and a cycle that long leaves no margin. The cycle time counts everything after the wait, including time interrupts
take from the audio thread, so the first step is to find out whether it is the
engine or the system: trace an hour at 44.1 kHz with a 96 kHz client under the full
load in `doc/measurements.md` with ftrace's `irqsoff` and `wakeup_rt` tracers, music
in RAM, and match the daemon's logged gap times (CLOCK_MONOTONIC) to the trace.
Timing the parts of a cycle comes after, if the time turns out to be the engine's.

Since then the stage 3a investigation found that compaction moving locked pages
stalls the audio thread on this board (entry below). Those stalls are far longer
than these cycles, so this may be another cause; repeat the measurement with
`dist/90-aas.conf` installed before tracing.

## Pi 3: what is left after the compaction finding

The 44.1 kHz hour under full load failed on the Pi 3 because memory compaction
moved the daemon's locked pages and the audio thread waited for each move, 15 to
34 ms at a time; the analog input is not the cause, and the stage 2 build fails
the same way. `doc/measurements.md`, "Why the 44.1 kHz hour failed on the Pi 3",
has the evidence. `dist/90-aas.conf` sets `vm.compact_unevictable_allowed = 0`
and the daemon warns at its start while it is 1.

Still missing:

- After an xrun the restart of the DAC+ ADC takes 12 to 15 ms, longer than the
  11.6 ms buffer, so one stall shows as two gaps. Whether the restart can be
  shorter, or moved off the audio thread, is not looked at.
- Memory taken by the kernel's contiguous allocator can still move locked
  pages. Not tested.
- Why the CM5 passes is not measured.

The hour with the file installed through `/etc/sysctl.d` and the board
rebooted has passed; `doc/measurements.md` has the figures.

First step: time `snd_pcm_prepare` on the playback after a forced xrun on the
Pi 3 test board and see which part of the codec's restart takes the time.

## Owner decisions left by stage 3a

Stage 3a built each of these one way and marked it *(call)* in
`doc/design.md`. Each needs the owner's yes or no. A no is a change to the
code and the design.

### Should a running recorder keep the output open?

A recorder is not a client: it cannot hold the rate or keep the output open.
With the analog input off and `idle = "off"`, the output closes
`idle_delay_s` after the last player stops, and a recording then gets no frames
and waits for ever, until a player opens the output again. No real card behaves
like this. Missing: the decision. If yes, first step: let a running capture
stream count as activity in the idle rule in `src/control.rs` (search for
`idle`), without letting it hold the rate.

### Is the analog input off unless the config turns it on?

Today it needs `[analog] enabled = true`, because it keeps the card's capture
open and the output never closes while it is on. Missing: the decision. If the
default should be on for cards with an ADC, first step: change the default in
`Analog::default` in `src/config.rs` and its test
`the_analog_input_is_off_by_default`, and the README's key description.

### The 1 s of silence after a capture starts

A capture delivers silence for 1 s after each start, `SETTLE_MS` in
`src/sink_alsa.rs`, because the DAC+ ADC Pro's ADC starts with a transient of
about -28 dBFS that would open the input for about 650 ms. It mutes the analog
input for 1 s at every start and every rate change. The figure is measured on
that card only. Missing: whether 1 s stays, and whether it becomes a value per
card in `src/cards.rs`. The DAC+ ADC on the Pi 3 test board is measured now: it
never went above -50 dBFS at a start, its loudest 50 ms -83.8 dBFS, so it does
not need the 1 s; the Pro does, for up to 700 ms (`doc/measurements.md`, Stage
3a). The design keeps 1 s for every card. First step: the owner confirms that,
or `SETTLE_MS` moves into the card table with 0 for the DAC+ ADC.

## Owner decisions left by stage 3b part 2

Stage 3b part 2 built each of these one way, marked *(call)* in
`doc/design.md`, "Several outputs". Each needs the owner's yes or no.

### Should a reopen of the master reopen every further card?

Every reopen of the master, a rate change for example, reopens every further
card, even one whose rate does not change. Each then primes and locks again,
about 5 s. Opening the master's card under a cycle parked by the idle rule,
at the rate the cycle runs at, touches no further card. Missing: the
decision. If no, first step: in `plan_further` in `src/control.rs`, give a
further output a new configuration only when its own rate, period or the
target changes, as a change of one card's rates already does.

### Should the same card be refused after startup too?

Two outputs naming the same card are refused at startup, by resolving each
name to the kernel's card index (`distinct_cards` in `src/sink_alsa.rs`). A
card that is not there at startup cannot be told, and passes, so two
outputs may later try to play to one card. Missing: the
decision whether a card found later is checked too. First step: find where
a further card's search reports a card found, call `distinct_cards` there as
`src/main.rs` does at startup, and decide what the later of the two outputs
does then.

## Let the capture PCM ask for the daemon's rate without a stream

The capture PCM learns the daemon's rate at open by setting up a one-frame
capture stream and replacing it at `hw_params` (`rate` in
`crates/aas-alsa/src/pcm/capture.rs`). So every open, also one that only asks
for capabilities, costs a ring, a memfd, an eventfd and two commands to the
audio thread. Missing: a read-only request for the rate in the protocol. First
step: add a request to `crates/aas-client/src/proto.rs` that answers the output
rate, bump the protocol version, and use it in `rate`.

## Plugins not built yet

Stage 3a built chains at `analog`, `mix` and `output` and the built-ins `detect`
and `balance`. Stage 3b part 1 added the mixing matrix, the card's channel count
and ABI version 2, with plugins for any channel count. Still missing:

- The limiter plugin. Without it, mixed clients that sum above full scale clip.
- Chains on a single client's input. `PluginPlace` in `src/config.rs` has no
  place for one.
- `input-processor` as a native plugin (stage 3c): RIAA, subsonic, gain,
  declick and notch, at 44.1 to 192 kHz. See "Input processor plugin" in
  `doc/design.md`.

## A real second card on a board

Stage 3b part 2 built several outputs: the master and up to three further
cards that follow its clock through an adaptive resampler, all held in time
(`doc/design.md`, "Several outputs"). The only further card tested is
`snd-aloop` (`doc/measurements.md`, Stage 3b, part 2). Its clock is software
and wanders when shifted, and it cannot run a period under about 48 ms, so
every output played about 70 ms late in the tests. That proves the routing,
the lock, the clamp and the counters, not how closely a card with a crystal
of its own locks, nor the latency with a card that runs the master's period.
Missing: a second real card on a test board, such as a USB DAC. First step:
fit it, add it as an `[[output]]` with its default `latency_ms`, and repeat
the lock, alignment and hour runs of that section of `doc/measurements.md`.

## Plan a further output's alignment from the buffer its card grants

The alignment target counts three of the periods a further output asks its
card for (`card_part` and `align_frames` in `src/control.rs`; the path is
built from the asked period in `FurtherHandle::reconfigure_holding`,
`src/further.rs`). `AlsaSink::try_open` in `src/sink_alsa.rs` asks with
`set_period_size_near` and `set_periods(3, Nearest)`, so a card may grant
another period or round the count. The writer fills what it was granted; a
larger buffer holds the path past its target, and the output starves over
and over. Done: after each open of a further card, and when its card comes
back, the writer compares `Sink::granted` with what was asked and sends
`FurtherEvent::Granted`; the control thread logs one warning naming the
output, asked against granted in frames and ms, and saying to set that
output's `latency_ms` (`Writer::check_grant`, `event_line`). The remedy is
still manual. Missing: plan the output's `card_part`, the target and the
master's delay from the granted buffer, as `set_output_rates` already
replans when a card's rates change; and a decision whether a grant smaller
than asked replans too. Until then the warning's advice does not fit every
case: a card that runs the asked period but four periods instead of three
keeps starving whatever `latency_ms` is set, since the excess stays one
period. And `Writer::watch_card` (`src/further.rs`) marks the card opened on
any watch change that ends not gone, so a spurious change could repeat the
warning; rare, and it goes away with the replan. First step: have the control thread handle
`FurtherEvent::Granted` by storing the granted period and buffer on the
`FurtherOutput` and calling `plan_further` with `only` set to that output;
in the writer tests, a `Sim` with `granted_periods = 4` that runs with no
starve then shows it works.

## Choose the clock master at run time

The master is the card of the flat `card` key, fixed until a restart. Stage 4
is to let the API choose it. Missing: what a switch does to the outputs, each
of which has its own resampler, drift and place in the delay, and whether
every card reopens for it. First step: read "Several outputs" in
`doc/design.md` and `plan_further` and `hand_over` in `src/control.rs`, and
write down what a switch must change before planning the API.

## The analog input from a card other than the master

Only the master's card has the analog input: `place = "analog"` and `detect`
are refused in an `[[output]]` table, and a further output gets the master's
input through its matrix. A further card with an ADC runs on its own clock,
so its capture would need an adaptive resampler into the master's clock, the
reverse of a further output's writer. Missing: whether it is wanted, then
its design. First step: the owner's decision; if yes, read the writer in
`src/further.rs` as the model for a capture that follows the master.

## The master plays 14 to 15 frames late

With `snd-aloop` as the further output the master plays 14 to 15 frames
after it, beyond the converters' own delay, at 44.1, 48 and 96 kHz on the CM5
test board: +0.32 ms at 44.1 and 48 kHz, +0.15 ms at 96 kHz, steady within a
frame. The same count of frames at each rate points at a count somewhere in
the path: the daemon's reading of either card's delay, the DAC's FIFO, or
`snd-aloop`'s pointer. `align-check` gives only the sum; how it was measured
is in `doc/measurements.md`, Stage 3b, part 2, "Alignment". Missing: the
cause, and then whether to correct for it. First step: repeat the run with
the master at another period (`latency_ms = 12` on the CM5) to see whether the
offset follows the period, and log the daemon's reading of each card's delay
beside `align-check`'s offset.

## The drift estimate on `snd-aloop`

The daemon's ppm, the separate 120 s estimate of a further card's drift, was
within 3 ppm of the measured drift in 14 of 17 lock runs on the CM5 test
board and 3.4 to 11.6 ppm off in three. A later 180 s in-range run read 12.8
ppm off; on the final build a 300 s run read 3.1 ppm off. The aim is 5 ppm.
Beyond the clamp the stop line's ppm is a snapshot and wanders: at +5052 ppm
measured it read +4434, at -4950 it read -4916; the clamp itself was
reported both times. `snd-aloop` with a shift set is not a clean clock, its
timestamps 31 to 313 µs off a straight line, so it is not settled whether the
estimate or the card is off. Under the hours' load the estimate also read 8
to 9 ppm further from the master than on an idle board, and the loopback's
rate was not measured there. Missing: runs long enough to tell. First step:
read the latest runs in `doc/measurements.md`, Stage 3b, part 2, "Lock and
drift"; if the estimate is still off, compare it with `align-check`'s
measured rate over the same window in longer runs, and on a real second card
once there is one.

## Pi 3: 98 frames dropped on the analog input at 96 kHz

The Pi 3's 96 kHz hour with a further output, repeated after the feed fix,
ended with the analog input's counters at 256 frames padded and 98 dropped;
the first hour dropped none. The input went quiet 8 s before the stop, when
the player ended, and the counters do not say when the 98 were dropped. Not
investigated. Missing: when it happens, and whether the further output's
writer, which took 44% of a core on the throttled board, has a part in it.
First step: repeat that hour of `doc/measurements.md`, Stage 3b, part 2,
"Hour runs with a further output", and log the time of each dropped frame.

## A real card with more than two channels

No board has one. More than two channels were run only through `snd-aloop`
(`doc/measurements.md`, Stage 3b, part 1), which proves the samples, not the
timing, the period the card grants, or how the card's driver refuses a count.
The DoP pair rule for channels 3 and up is checked in tests only. Missing: a
board with such a card, for example a multichannel USB DAC or a HiFiBerry card
with more than two outputs. First step: run `bit-check --channels 8 --expect`
against it, then the hour runs of Stage 3b with `channels` set to its count.

## Pi 3: the 96 kHz input padding

At 96 kHz with the analog input on, the Pi 3 test board padded 256 frames in
the stage 3b part 1 hour run against 768 in stage 3a. Not looked into; it may
be chance. Missing: a look at when the padding happens. First step: repeat the
hour run of `doc/measurements.md`, Stage 3b part 1, at 96 kHz and log the time
of each padding.

## Matrix changes through the API

Part 1 sends a matrix only from the config. When the API sends it (stage 4),
two things need a decision. A matrix changed more often than every 10 ms keeps
a DoP stream that starts meanwhile waiting, since it waits for the fade to end.
And a change made while a DoP claim at the wrong rate is held applies at once
under PCM. Neither can happen from the config. First step: read the matrix
fade in `src/engine.rs` (search for `ramp`) and decide a bound for the wait.

## Add cards to the ADC table

`src/cards.rs` lists the cards whose capture `aas` opens. Only
`snd_rpi_hifiberry_dacplusadc` and `snd_rpi_hifiberry_dacplusadcpro` are
verified on hardware, and the Digi is listed as having no ADC. Any other card
gets no analog input. Missing: the other HiFiBerry cards with an ADC, each
verified on a board. First step: list the HiFiBerry drivers with an ADC, then
for each run `ANALOG=1 sh dist/test-plugin.sh` on a board with that card and a
cable from output to input, and add a row.

## Verify the capture's failure paths on a board

Tested only off the board: a capture that is suspended or disconnected (it
should be reported gone and retried every second), a card that appears after
the daemon starts (whether it has an ADC is then decided at each open), and a
card without an ADC (one log line, nothing opened). All need a driver unbind or
a card the test boards do not have. First step: on the Pi 3 test board, unbind
the card's driver through sysfs while the analog input plays, and check the log
and that the input comes back after the bind.

## Open minor findings from stage 2

Found in the stage 2 reviews. Most are fixed.
These are left, each with the reason. None of them breaks playback as measured.
Search for the name given; line numbers drift.

### Needs a decision from the owner

- `src/dop.rs`, `find`: only the top byte is checked, with a 16-frame minimum, so
  a deliberate full-scale test tone at the marker values could be taken for DoP.
  Decide whether detection should also require the DoP silence or payload pattern
  over a longer run. A stricter check delays the start of a real DoP stream.
- `crates/aas-alsa/src/pcm.rs`: the 8192-byte buffer floor applies at every rate,
  so tiny buffers above 48 kHz are refused. The plugin's limits are fixed in bytes
  when the PCM is opened, with no refinement per rate. A floor made for the highest
  rate, about 64 KiB, would force 16384 frames, 341 ms, on a 48 kHz S16 player.
  Decide between the refusal as it is and a larger floor.
- `src/control.rs`, `send`: while the command queue is full the control thread
  waits 1 ms at a time. The wait is logged after 1 s but has no bound. Decide what
  happens at a bound: drop the commands, or stop the daemon.

### Needs a test board

- `src/server.rs`, card return: udev runs `alsactl restore` when a card appears.
  On the Pi 3, after an unbind and bind, it set the card's level about 0.5 s
  after the card was back, and the daemon set its own within the next second, so
  the daemon won. With a faster retry the restore could land last. Fix: set the
  volume again a second or two after the card returns, or keep udev from
  restoring a card the daemon drives. First step: unbind and bind the card's
  driver through sysfs with the daemon running and read `amixer -c <card>`.
- `src/sink_alsa.rs`, `AlsaSink::try_open`: a card that grants another period
  than asked is accepted with a warning, and the engine keeps its own period, so
  a USB card's buffer can be under two engine periods. Fix: adopt the granted
  period as the engine period. Needs a card that does this, such as a USB DAC.
- `src/sink_alsa.rs`, `Open::write_all`: the path where `snd_pcm_wait` times out
  is not exercised by any test. Fix: a board test that stops the card's clock,
  for example by unbinding the driver while a write waits.
- `src/sink.rs`, the switch from the null sink back to the card: the gap is no
  longer counted, but the stall of about 14 ms is still there. Fix: open the card
  on the control thread and swap it in. That changes `AlsaSink`, which builds
  only on a board.
- `src/control.rs`, `set_volume`: the card's mixer is now written on every
  `SetVolume`, but a level changed outside `aas`, with `alsamixer`, is never read
  back. Fix: watch the mixer's events on the control thread. That changes
  `AlsaVolume`, which builds only on a board.
- `dist/test-plugin.sh`: no case unbinds and rebinds the card and checks
  `amixer -c <card>` after a `SetVolume`. It was left out because a failure could
  leave the only Pi 3 with a loopback cable without its card. Run it on a board
  that can be power-cycled.

### Open in the code

- `src/sink.rs`, `FallbackSink`: the control thread now looks for a gone card,
  and the audio thread only reads a flag. A card that is listed but cannot be
  opened, because another program holds it, or one named by index, is still
  tried once a second on the audio thread, and each try allocates. Only the null
  sink is playing then. Fix: open on the control thread, as above.
- `src/engine.rs`, DoP: a DoP writer giving under 16 frames a cycle after a cut
  loses frames until a window holds 16. It loses only that client's frames and
  alters none. `cut_run` is kept across a stop on purpose: clearing it let the
  fuzz tests find DoP frames in the mix. Fix: a fuzz case that measures lost
  frames, then decide whether the loss matters.
- `crates/aas-alsa/src/pcm.rs`: rewind is not implemented. The plugin interface
  has no rewind callback, libasound moves the application pointer itself, and
  frames already in the ring cannot be taken back. Fix: a request that lets the
  daemon give back unread frames, if a player turns out to need it.
- `crates/aas-alsa/src/pcm.rs`, `poll_revents`: a state another thread has just
  changed can still be overwritten. The window is narrowed by reading the state
  again before the write, but it is not closed, because libasound's lock is not
  available to a plugin and taking the plugin's lock there deadlocks against
  `snd_pcm_drop`.
- `crates/aas-alsa/src/pcm.rs`, `hw_free`: the protocol has no request to give a
  stream up, so the daemon keeps its stopped side until the next `hw_params` or
  the close. Fix: a `Release` request.
- `crates/aas-alsa/src/ctl.rs`: its request timeout is 2 s, and the PCM client's
  is 5 s. Fix: use one value, after deciding which.
- `tools/measure/src/hw.rs`, `open_near`: repeats the daemon's card setup. The
  copy is noted there. Fix: add the same note at `AlsaSink::try_open`, or share
  the code.

## Open minor findings from stage 3b part 2

Found in the stage 3b part 2 reviews and left, each with the reason. None of
them was seen to break playback. Search for the name given.

- `src/engine.rs`, `holds_sound`: a change of the master's delay reads the
  delay line once, in silence to its end, to see whether it holds sound. The
  line is made as long as the config can need: about 27,600 frames, times
  the channels, for a further output at `latency_ms = 48`. Bounded, and only
  at a change of the delay. On the Pi 3, twelve rate changes in silence, with
  delays up to 13887 frames at 192 kHz, showed no longer worst cycle with it
  (`doc/measurements.md`, Stage 3b, part 2, "Processing cost on the Pi 3").
  First step, if it ever shows: keep a count of the sound in the line as it
  is written, and drop the scan.
- `src/sink_alsa.rs`, `AlsaSink::io_failed`: after an xrun, a restart that
  succeeds returns at once, with no sleep (the `Ok(()) => SinkError::Xrun`
  arm). A card that took the restart and failed again at the next wait would
  be restarted at every cycle. The restart writes the whole buffer of zeros,
  which the card has to play, so it is paced, and no real case was found.
  After a restart that fails, the thread sleeps a period (`rest`), which for
  the master is the audio thread; that is bounded. First step: count
  restarts in a row in `io_failed` and rest after the second.
- `src/engine.rs`, the xrun count: when the master's wait finds an underrun,
  `Engine::wait_room` counts an xrun and `AlsaSink::io_failed` restarts the
  card; the cycle then goes on to its write, and if that write fails too
  before the restarted card runs, `render` counts a second. So one underrun
  may count twice. Cosmetic: the count is too high, never too low. First
  step: a sink test that fails the wait and then the write once each, and a
  decision whether the write after a failed wait counts.
- `src/resample.rs`, `Resampler::delay_frames`: the clients' resampler
  reports rubato's `output_delay`, about one output frame more than where a
  step lands, as the adaptive resampler's own measurement found. It matters
  only for the delay reported to clients. First step: measure it as
  `Adaptive::delay_frames` was in `src/adaptive.rs` and subtract the frame.
- `src/drift.rs`, `Drift::update`: a reading skipped as not believable does
  not add to `missed`, as `Drift::pass` does for a withheld one, so the next
  reading's change of the delay spans two periods and is taken as one, and
  the estimate gets that step's slope twice. Negligible while locked, where
  the slope is near 0, and the estimate averages it out. First step: count
  the skip in `missed` too, with a test of the estimate across a skip.
- `src/further.rs`, the drift's readings: with the master and the writer at
  the same period, and the writer's read just before the master's push, up
  to about half the readings can be withheld as overdue for minutes. The
  controller keeps running on the rest. None was seen on the boards. Such
  readings go to `Drift::pass` and are not in the stop line's skipped count.
  First step: a simulated run at equal periods with that phase held
  constant, counting the withheld readings.
- `src/config.rs`, `an_output_takes_idle_and_channels_by_the_flat_rules`: the
  unknown-key case of an `[[output]]` table is the test's last assertion and
  checks only `is_err()`, not the message. First step: a test of its own
  that checks the message names the output and the key.
- `src/engine.rs`: no test checks the fade of a shutdown with PCM in the
  delay line. `Command::Shutdown` takes the same path as `CloseOutput`,
  which `closing_fades_what_the_delay_line_holds` covers;
  `dop_in_the_delay_line_is_never_scaled` shuts down with DoP and PCM in the
  line, but checks only that no DoP word is scaled. First step: the close
  test with `Shutdown`.
- `tests/server.rs`, `a_lost_further_card_is_looked_for_every_second`: it
  sleeps 3.5 s and accepts 2 to 5 looks, which is slow and depends on the
  host's load. First step: wait for the second look, then check the count
  within a bound.
- `src/further.rs`, `a_lost_further_output_never_stalls_the_master`: it runs
  a real writer thread and allows at most 5 refused pushes over about
  0.8 s, so it can fail on a loaded host. First step: run it on a test clock,
  as the writer's other tests do, or widen the bound with a reason.
