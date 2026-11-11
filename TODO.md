# TODO

## Measure the Pi 4

The stage 1 measurements on the CM5 and Pi 3 are done, see `doc/measurements.md`.
The Pi 4 still needs a test board. Then run the same probes, confirm or reject 4 ms,
and confirm that `high` resampling fits. Without a Pi 4, the design's 4 ms claim
for it stays unconfirmed.

## Prove that a late wake-up does not drop audio

On the Pi 3 under load, a 4.05 ms wake-up gap with a 2.67 ms buffer gave no xrun.
ALSA can miss an underrun when the interrupt is late, so it is unknown whether audio
broke. Write a probe that plays a steady tone through the cable and reports breaks
in the capture. Both test boards have the cable connected.

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

Found in the stage 2 reviews and left for later. None of them breaks playback as
measured. Each line names the place, the problem and a first step. Line numbers
are from the end of stage 2 and will drift, so search for the name given.

### Daemon

- `src/sink.rs`, `FallbackSink::try_inner`: while the card is gone, the audio
  thread retries once a second, and each retry allocates (the rates `Vec`, the
  `supported_rates` copy) and reads `/proc/asound/cards`. Only the null sink is
  playing then, so nothing audible is timed, but it breaks the audio thread's
  rules. Fix: let the control thread probe for the card and tell the audio thread
  to retry, or hand the rates over in a preallocated buffer.
- `src/sink.rs`, `FallbackSink`: the switch from the null sink back to the card
  costs one wake-up gap of about 14 ms, logged as a gap. Fix: open the card on the
  control thread and swap it in, or leave the gap out of the count for that cycle.
- `src/server.rs`, `accept_all`: when `accept4` fails with `EMFILE` or `ENFILE`,
  the listener stays readable and the control thread spins in `poll`. The fd
  limit is raised to the hard limit, so it takes many clients. Fix: on those
  errors stop polling the listener for a short time, or keep a spare descriptor
  to accept and close the extra connection.
- `src/server.rs`, `run`: the socket is renamed into place before the eventfd and
  the audio thread are made, so a failure there leaves a socket file nobody
  listens on. Fix: remove the socket on those error paths, or listen last.
- `src/control.rs`, `send`: a batch of more than 4096 commands is split, and while
  the queue is full the control thread sleeps 1 ms at a time with no limit. Fix:
  bound the wait and log it, or size the queue from the table capacity.
- `src/control.rs`, `sync_resamplers`: when the output rate changes, a running
  client whose ring is too small to resample from at the new rate is only logged
  and plays short. Fix: answer its next request with an error so the plugin
  reconfigures, or size the minimum ring for the worst rate ratio.
- `src/control.rs`, `tick`: a drain whose ring stops moving is answered `Ok` at
  its deadline plus 500 ms even if the audio was never played, such as during a
  slow reopen of the card. Fix: hold the deadline while the output is reopening.
- `src/control.rs`, `Stream::view`: the control thread maps and mlocks the whole
  ring to read one header field, which doubles the locked memory per client. Fix:
  map only the header page for the control thread's view.
- `src/server.rs` and `src/sink_alsa.rs`, `AlsaVolume`: the daemon holds the
  card's mixer open while the output is closed, which is the resting state after
  60 s idle. A card lost then is never reported: `FallbackSink::close` clears
  `live` without counting, and `try_inner` counts only an open that succeeds. So
  a driver unbound through sysfs, or a module reload, hangs until the daemon
  stops, even if playback starts again, and `rmmod` reports the module in use.
  Loss during playback is handled. Fix: close the mixer with the output and open
  it again with it, or count a failed open when the sink was live before the
  last close (a flag that `close` does not clear).
- `src/server.rs`, card return: udev runs `alsactl restore` when a card appears.
  On the Pi 3, after an unbind and bind, it set the card's level about 0.5 s
  after the card was back, and the daemon set its own within the next second, so
  the daemon won. With a faster retry the restore could land last and overwrite
  the daemon's level. Fix: set the volume again a second or two after the card
  returns, or keep udev from restoring a card the daemon drives.
- `src/control.rs`, `set_volume`: the same percent again returns early, so it
  does not reassert the card's mixer after someone changed it with `alsamixer`.
  Changes made outside `aas` are never read back either. Fix: always write the
  mixer on `SetVolume`, and read it back when the plugin asks, or watch the
  mixer's events on the control thread.
- `src/control.rs`, `save`, and `src/state.rs`: the state file is fsynced on the
  control thread, which can hold up requests on a slow SD card, and the directory
  is not fsynced after the rename. Fix: write it from a helper thread, and fsync
  the directory after the rename.
- `src/server.rs`, `notify`: if `SIOCOUTQ` fails on a subscriber's socket, its
  notice stays pending for good and it gets no more notices. It fails safe. Fix:
  send the notice when the ioctl fails.
- `src/main.rs`, `hardware_volume`: a build without `alsa-output` ignores
  `volume_mode = "hardware"` without a word. Fix: log a warning.
- `src/main.rs`, `parse_args`: untested. Fix: take the arguments as a parameter
  and add tests for each flag, a missing value and an unknown flag.
- `src/sink_alsa.rs`, `AlsaSink::try_open`: a card that grants another period
  than asked is accepted with a warning, and the engine keeps its own period.
  Since stage 2 the card's `avail_min` follows the engine period, so this no
  longer spins, but a USB card's buffer can then be under two engine periods.
  Fix: adopt the granted period as the engine period.
- `src/sink_alsa.rs`, `Open::write_all`: the path where `snd_pcm_wait` times out
  is not exercised by any test. Fix: a board test that stops the card's clock,
  for example by unbinding the driver while a write waits.
- `src/engine.rs`, `cycle`: `report()` is skipped when `apply_reopen` fails,
  through the early `?`. Harmless while the sink is a `FallbackSink`, whose `open`
  never fails. Fix: run `report` on every path.
- `src/engine.rs`, DoP: when the grace time ends for a stalled DoP stream, no
  `DopStopped` is sent. Fix: send it when the grace time releases the output.
- `src/engine.rs`, DoP: the `dropped` credit can build up across repeated drops
  with no discard; a DoP writer giving under 16 frames a cycle after a cut loses
  frames until a window holds 16; and a stale `cut_run` survives a stop that
  leaves frames. All lose only that client's frames and never alter any. Fix:
  clear `dropped` and `cut_run` on a stop, and add a fuzz case for each.
- `src/engine.rs`, DoP: the engine and the control thread can disagree on which
  DoP claim is oldest when about 1024 events are unread, because claims are
  numbered when `DopStarted` cannot be sent. Fix: carry the claim number in the
  event.
- `src/counters.rs`, `held_frames`: also counts a resampled source waiting for
  input, which is wider than its doc comment says. Fix: count only frames held as
  possible DoP, or reword the comment.
- `src/dop.rs`, `find`: only the top byte is checked, with a 16-frame minimum, so
  a deliberate full-scale test tone at the marker values could be taken for DoP.
  A stricter check is an owner decision. Fix: also require the DoP silence or
  payload pattern over a longer run, after deciding.
- `src/format.rs`, `finish`: float finish maps NaN to 0 through the `as` cast.
  It is not documented or tested. Fix: a doc line and a test.
- `src/format.rs`, `mix_add` and `I32::accumulate`: the ramp divides per frame
  and `I32` converts the gain to fixed point per sample. Fix: compute the gain
  step once per call, and convert it once per frame. Matters on the Pi 3.
- `src/format.rs`, `mix`: if `acc` is shorter than `out`, the tail of `out` is
  left stale without a word. Fix: `debug_assert_eq!` the lengths.
- `src/format.rs`: `I32` rounds half up and the floats round half away from zero,
  one LSB apart at exact halves. Fix: pick one and test it.
- `src/format.rs` tests: no test of many sources with a large gain in `I32`, or
  of a `Ramp` across two `mix` calls. Fix: add both.
- `src/rate.rs` tests: the lower-rate tie in `nearest` and `set_supported` have
  no unit test. Fix: add one each.
- `src/bitperfect.rs` tests: `hardware_volume_does_not_block_bit_perfect` tests
  nothing hardware specific; forced mode with a resampled client is not
  documented; `Off` with no source or several is untested. Fix: rework that
  test, add a doc line and the two cases.
- `src/config.rs`: `default_rate` is not range checked, and the fields have no
  doc comments. `README.md` says "The keys are in `src/config.rs`" instead of
  listing them. Fix: validate the rate, and add a key table to the README:
  `latency_ms`, `card`, `format`, `resampling`, `bit_perfect`, `idle`,
  `idle_delay_s`, `volume_mode`, `volume_control`, `default_rate`.
- `src/board.rs`: the order of `trim_end_matches('\0')` and `trim` is right by
  accident. Fix: a comment, or one trim that covers both.
- `src/state.rs` tests: the branch for an out-of-range percent is untested,
  because 300 is not a `u8` in the test. Fix: write the file as text. The
  debounced save from `tick` is untested too.
- `doc/design.md`, failure behaviour: each client can lock up to 2 x 8 MiB of
  ring, with no bound over all clients and `LimitMEMLOCK=infinity` in the unit.
  It follows the rule that no client is refused. Fix: state it in the design.

### Client

- `crates/aas-client/src/proto.rs` and `ring.rs`: two version numbers and no
  negotiation. A ring layout change without a protocol bump fails at Configure as
  `EIO`, not at Hello. Fix: a const assert that ties the two, and let the Hello
  reply carry the daemon's version. Nothing is released, so v1 can still absorb
  the `VolumeChanged` tag.
- `crates/aas-client/src/proto.rs`: the module doc says unused arguments are zero,
  but decoding ignores them. Fix: reject non-zero ones, or reword the doc.
- `crates/aas-client/src/proto.rs` tests: one test sets and removes `AAS_SOCKET`
  in a multithreaded test binary. Nothing else there reads it today. Fix: test
  through a function that takes the value as a parameter.
- `crates/aas-client/src/client.rs`: requests have no timeout, while the control
  plugin has 2 s. A daemon that is alive but slow blocks a player in
  `snd_pcm_start` or `prepare`. Fix: set `SO_RCVTIMEO` as the control plugin does.
- `crates/aas-client/src/ring.rs`, `header`: the header is plain `u32`s in memory
  the peer can write. Fix: read the non-atomic fields with `read_volatile` after
  mapping.
- `crates/aas-client/src/ring.rs`: `Ring` is `Sync`, so safe code can break the
  single reader and single writer rule. `available()` is exact only on the reader
  and can show a transient 0 to a third party. Fix: doc lines on both.
- `crates/aas-client/src/ring.rs`: the fallback for kernels before 5.14, which
  populate without `MADV_POPULATE_WRITE`, has no test, and keeps the A53 first
  write faults. Fix: a test that forces the fallback.

### PCM plugin

- `crates/aas-alsa/src/pcm.rs`, drain: a blocking drain holds the plugin's mutex,
  so `snd_pcm_drop` from another thread waits up to a buffer plus 500 ms. Fix:
  wait without the lock and recheck the state after.
- `crates/aas-alsa/src/pcm.rs`, `poll_revents`: returns `POLLERR` in the
  DRAINING, SETUP and OPEN states. Fix: check what libasound expects in each and
  return 0 where nothing is wrong.
- `crates/aas-alsa/src/pcm.rs`, `state`: reads `io.state` without the PCM lock.
  Fix: read it under the lock or document why it is safe.
- `crates/aas-alsa/src/pcm.rs`: `hw_free` and rewind are not implemented. Fix:
  implement `hw_free` to drop the stream, and rewind within the unread part of
  the ring.
- `crates/aas-alsa/src/pcm.rs`: the 8192-byte buffer floor applies at every rate,
  so tiny buffers above 48 kHz are refused. Fix: derive the floor from the
  daemon's period at the highest rate.
- `crates/aas-alsa`: no recorded mutation catch for a PCM callback signature,
  and the crate's unit tests do not run with a plain `cargo test` on the
  workstation. Fix: break one signature on purpose once and record that the
  static assertions fail; document how to run the crate's tests.
- `dist/test-plugin.sh`, `aplay_plays_each_format`: checks only the exit status.
  Fix: compare the played samples through the loopback, as `bit-check` does.
  Also add an unbind and rebind case that checks `amixer -c <card>` after a
  `SetVolume`, which covers the volume following the card.

### Control plugin

- `crates/aas-alsa/src/ctl.rs`, `write_integer`: reads the volume and then sets
  it, so a change from another client in between makes it skip the write. Fix:
  always send `SetVolume`.
- `crates/aas-alsa/src/ctl.rs`, `poll_revents`: takes the daemon mutex, so it can
  wait up to the 5 s socket timeout behind a call. Fix: keep the hung-up flag in
  an atomic.
- `crates/aas-alsa/src/ctl.rs`: the result of the eventfd write is ignored. Fix:
  log or count a failure.
- `crates/aas-alsa/src/ctl.rs`: the control does not reconnect after the daemon
  restarts. The README and design say so. Fix: reconnect on the next call after a
  hang-up.
