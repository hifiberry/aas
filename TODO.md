# TODO

## Measure the Pi 4

The CM5 and Pi 3 are done, see `doc/measurements.md`. The Pi 4 still needs a test
board. Then run the same probes, confirm or reject 4 ms, and set its resampling
default. Without a Pi 4, the design's 4 ms claim for it stays unconfirmed.

## Prove that a late wake-up does not drop audio

On the Pi 3 under load, a 4.05 ms wake-up gap with a 2.67 ms buffer gave no xrun.
ALSA can miss an underrun when the interrupt is late, so it is unknown whether audio
broke. Write a probe that plays a steady tone through the cable and reports breaks
in the capture. Both test boards have the cable connected.

## Check heat in a real case

A CM5 without a heatsink throttles on one busy core. Measure a Pi 5 class board in
the actual product enclosure, then decide on a minimum cooling requirement.

## Measure the sound of medium resampling

The Pi 3 default uses `medium` on cost alone. Measure its passband and stopband
against `high`, and confirm the owner is happy with it.

## Remaining minor issues in tools/measure

- `handoff-probe` can print `run INCOMPLETE` on a full run: the reporting process
  starts its timer after the sender. Counts are unaffected.
- `handoff-probe` hangs if only the parent fails to get its priority.
- The `rt` field prints the flag, not the state achieved.
- A run shorter than the warm-up prints zeros under "complete".
- `loopback-latency` accepts negative delays and `--repeats 0`.
- `period-probe` prints no buffer time or verdict.
