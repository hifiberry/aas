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
