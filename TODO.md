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

## Check heat in a real case

A CM5 without a heatsink throttles on one busy core. Measure a Pi 5 class board in
the actual product enclosure, then decide on a minimum cooling requirement.

## Measure the sound of medium resampling

`medium` is no longer a default, but it is the fallback for a Pi 3 in `f64` or with
several resampled streams. Its cost is measured, its sound is not. Measure its
passband and stopband against `high`.

## Measure the i32 processing format

The design allows `i32`, `f32` and `f64`. `dsp-bench` covers only the two float
formats. Add an `i32` mix case with a 64-bit accumulator and saturation, then run it
on both test boards and add the figures to `doc/measurements.md`.

## Remaining minor issues in tools/measure

- `handoff-probe` can print `run INCOMPLETE` on a full run: the reporting process
  starts its timer after the sender. Counts are unaffected.
- `handoff-probe` hangs if only the parent fails to get its priority.
- The `rt` field prints the flag, not the state achieved.
- A run shorter than the warm-up prints zeros under "complete".
- `loopback-latency` accepts negative delays and `--repeats 0`.
- `period-probe` prints no buffer time or verdict.
