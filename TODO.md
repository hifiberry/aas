# TODO

## Measure the Pi 4

The CM5 and Pi 3 are done, see `doc/measurements.md`. The Pi 4 still needs a test
board. Then run the same probes, confirm or reject 4 ms, and confirm that `high`
resampling fits. Without a Pi 4, the design's 4 ms claim for it stays unconfirmed.

## Prove that a late wake-up does not drop audio

On the Pi 3 under load, a 4.05 ms wake-up gap with a 2.67 ms buffer gave no xrun.
ALSA can miss an underrun when the interrupt is late, so it is unknown whether audio
broke. Write a probe that plays a steady tone through the cable and reports breaks
in the capture. Both test boards have the cable connected.

## Measure the sound of medium resampling

`medium` is no longer a default, but it is the fallback for a Pi 3 in `f64` or with
several resampled streams. Its cost is measured, its sound is not. Measure its
passband and stopband against `high`.

## Run the stage 2 measurements on the CM5

Stage 2 was measured on the Pi 3 only, see "Stage 2" in `doc/measurements.md`. The
CM5 test board's SSH server closed every connection before the banner, so nothing
ran there. Its 4 ms criterion is unconfirmed until this is done. First step: get a
shell on the CM5 again (console or reinstall), then sync the tree and build on it:

```sh
cargo build --release --features alsa-output
cargo build --release -p measure --features alsa-probes --bins
sh dist/install-plugin.sh
```

Then, with the daemon started as root with real-time rights (`target/release/aas
--config FILE`):

- Bit-exactness, 36 cases: `sudo modprobe snd-aloop`, config `card = "Loopback"`,
  `latency_ms = 48` and `format` set to `i32`, `f32` and `f64` in turn, then
  `bit-check --format S16|S24|S32 --rate 44100|48000|96000|192000 --seconds 10`.
  Pass: `differing 0, breaks 0` in all 36.
- Latency, with the default config and the cable connected: at 44.1, 48, 96 and
  192 kHz with the daemon's periods (64, 64, 128, 256), run
  `loopback-latency --device hw:0,0 --capture hw:CARD=<card>,DEV=0 --periods 4`
  with the daemon stopped, then `loopback-latency --device default --capture
  hw:0,0 --periods 4` with it running. Added latency is the second minus the
  first. Pass: below 4 ms. On the Pi 3 `aas` added three periods, so expect this
  to fail until the engine waits for room before rendering (see below).
- Hour runs under the full load in `doc/measurements.md`, with the default config:
  `aplay -D default` of real music at 44.1 kHz, then at 96 kHz, then at 44.1 kHz
  with a second `aplay` at 96 kHz. Play from RAM, for example a few minutes of
  music looped by `cat` into `aplay -t raw`: a file read from the SD card while
  the load writes to it starves the player, with or without `aas`. Pass: the
  daemon's closing line shows 0 xruns and 0 wake gaps, and `aplay` reports no
  underrun.
- `dsp-bench --mix-only`, for the `i32` mix figures.
- `mpd` and `squeezelite`, each alone and together at 44.1 and 96 kHz. Both must
  play. `squeezelite` needs a server: a minimal SlimProto server that sends one
  `strm` start for a FLAC file over HTTP is enough.

Add the figures to the Stage 2 section and update its summary.

## Make the engine add two periods, not three

On the Pi 3, `aas` added three of its periods of latency, about 16 ms against a
12 ms target (`doc/measurements.md`, Stage 2). The engine renders a period, which
reads the clients' rings, and then blocks in the card write until there is room.
So a rendered period waits outside the card's two buffered periods. Wait for room
first, with `snd_pcm_wait`, and render after it. Then measure again with
`loopback-latency --device default --capture hw:0,0`.

## Find what makes a daemon cycle take 6.7 ms on a throttled Pi 3

In the stage 2 hour runs on the Pi 3 under full load, throttled to about 550 MHz,
the daemon's worst cycle was 3.6 ms at 44.1 kHz and 6.7 ms at 96 kHz, with no
resampling. That is more than a 5.33 ms period, and each of those runs missed
its buffer once. `period-probe` held the same load in stage 1. The daemon reports
only the worst cycle, so it is not known which part took the time. Time the parts
of a cycle (commands, rings, mixing, the card write) and keep the worst of each,
then run an hour at 96 kHz under the full load in `doc/measurements.md`, with the
music in RAM.

## Remaining minor issues in tools/measure

- `handoff-probe` can print `run INCOMPLETE` on a full run: the reporting process
  starts its timer after the sender. Counts are unaffected.
- `handoff-probe` hangs if only the parent fails to get its priority.
- The `rt` field prints the flag, not the state achieved.
- A run shorter than the warm-up prints zeros under "complete".
- `loopback-latency` accepts negative delays and `--repeats 0`.
- `period-probe` prints no buffer time or verdict.
