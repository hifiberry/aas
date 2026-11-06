# Stage 1 measurements

What the hardware can do, measured before the daemon is written. The probes are
in `tools/measure`. The Pi 4 is still to be measured.

## Boards

| | CM5 test board | Pi 3 test board |
|---|---|---|
| Board | Compute Module 5 Lite, 8 GB | Pi 3 Model B Rev 1.2, 905 MB |
| Card | DAC+ ADC Pro | DAC+ ADC |
| Overlay | `hifiberry-dacplusadcpro,slave` | `hifiberry-dacplusadc` |
| Kernel | 6.18.50, standard `PREEMPT` | 6.18.50, standard `PREEMPT` |
| OS | Debian trixie, 64-bit | Debian trixie, 64-bit |
| Governor | `ondemand` | `ondemand` |
| Top clock | 2.4 GHz | 1.2 GHz |

On the CM5 the plain ADC Pro overlay gives no sound card
(`snd_soc_dai_set_fmt() -22`). The `,slave` form makes the Pi the clock master
and works.

The probes ran at `SCHED_FIFO` with locked memory. "Idle" means nothing else was
running. "Full load" means one `sha256sum` per core plus repeated 128 MB
synchronous disk writes.

## Conclusions

- **Processing format: 32-bit float as the default, 64-bit still supported.** The
  format can be set in the config, and `i32` is not yet measured. Mixing costs little
  on any board, about 1% of a period on the Pi 3 at 48 kHz. Resampling costs 1.4 to
  1.85 times as much in 64-bit.
- **Resampling quality:** `high` by default. On the Pi 3, `high` to 192 kHz uses
  about 44% of a period in 32-bit, 62% in the worst cycle. In 64-bit it uses about
  80% and can overrun, so a Pi 3 set to 64-bit should also use `medium`.
- **Default period on CM5:** 64 frames at 44.1 and 48 kHz, 128 at 96 kHz, 256 at
  192 kHz, with 2 periods.
- **Default period on Pi 3:** 256 frames at 44.1 and 48 kHz, 512 at 96 kHz, 1024
  at 192 kHz, with 2 periods. That is about 11 to 12 ms.
- **4 ms on the CM5 holds,** with the handover cost included and under full load.
  It stands in for the Pi 5, which uses the same processor. It is not confirmed on
  the Pi 4.
- **`PREEMPT_RT` is not needed.** The standard kernel holds every default on both
  boards, under full load.
- **A heatsink is not required, but it keeps the speed.** Without one, a CM5 throttled
  and still had no xruns. See [Heat](#heat).

## What these figures do not show

- The period runs play silence. Real audio, a real daemon and real clients come in
  Stage 2, which must repeat these runs.
- Full load is CPU and disk. Network and USB load are not included.
- An xrun count alone is not enough. See the 96 kHz row under
  [Periods](#periods). ALSA can miss an underrun when the interrupt itself is late,
  so a run only passes if no wake-up gap is longer than the buffer. A gap longer
  than the buffer is a suspected dropout, not a proven one. A test that plays a
  steady tone through the cable and looks for breaks would settle it.
- `medium` resampling is measured for cost only. Its audio quality has not been
  measured.
- `dsp-bench` times one resampler at a time. Several resampled clients add up.
- Each card was tested on one board only, so a converter difference cannot be
  separated from a board difference with certainty.

## Periods

Each entry is the longest gap between wake-ups in ms, then xruns. Two periods are
buffered. Runs last 10 minutes unless marked. A gap in bold is longer than the
buffer, or the run had xruns.

| Rate / period | Buffer ms | CM5 idle | CM5 full load | Pi 3 idle | Pi 3 full load |
|---|---|---|---|---|---|
| 44.1 kHz / 64 | 2.90 | 1.55, 0 (1 h) | 1.54, 0 (1 h) | 1.95, 0 (1 h) | |
| 44.1 kHz / 256 | 11.61 | | | | 6.80, 0 |
| 48 kHz / 32 | 1.33 | 0.72, 0 | 0.74, 0 | 1.10, 0 | |
| 48 kHz / 64 | 2.67 | 1.36, 0 | 1.39, 0 | 1.75, 0 | **12.74, 3** |
| 48 kHz / 128 | 5.33 | 2.70, 0 | 2.74, 0 | 2.82, 0 | 4.23, 0 |
| 48 kHz / 256 | 10.67 | | | 5.45, 0 | 6.65, 0; 6.62, 0 (1 h) |
| 48 kHz / 512 | 21.33 | | | 10.79, 0 | |
| 96 kHz / 128 | 2.67 | 1.49, 0 (1 h) | 1.43, 0 (1 h) | 1.72, 0 (1 h) | **4.05, 0** |
| 96 kHz / 512 | 10.67 | | | | 6.37, 0 |
| 192 kHz / 64 | 0.67 | 0.39, 0 | | | |
| 192 kHz / 128 | 1.33 | 0.78, 0 | | 1.00, 0 | |
| 192 kHz / 256 | 2.67 | 1.46, 0 (1 h) | 1.43, 0 | 2.13, 0 (1 h) | 2.55, 0 |
| 192 kHz / 512 | 5.33 | | | 2.84, 0 | 3.98, 0 |
| 192 kHz / 1024 | 10.67 | | | 5.48, 0 | 6.40, 0 |

The CM5 idle runs were made before the heatsink was fitted, and its full load runs
after. The Pi 3 had no heatsink. It ran the full load runs at about 550 MHz and
87 °C, so they are a worst case, not normal playback.

The Pi 3 holds the 4 ms periods when idle, but not reliably under load. At 128
frames at 48 kHz it passed, with 79% of the buffer used in 10 minutes. That is a
reasonable setting for a lightly used Pi 3, but it is not the default.

## Handover cost

`handoff-probe` wakes a second process through shared memory and an eventfd every
1.333 ms for 10 minutes. Latency is in µs.

| | Mean | p99.9 | Max | Missed |
|---|---|---|---|---|
| CM5 idle | 3.6 | 6.7 | 145 | 0 |
| CM5 full load | 3.1 | 19 | 73 | 1 |
| Pi 3 idle | 17.6 | 37 | 165 | 0 |
| Pi 3 full load | 23.7 | 92 | 1168 | 1 |

The single missed wake-up under load is probably the start of the run, before the
client was ready, but that is not proven.

## Delay through the cable

20 repeats per rate, with the DAC output connected to the ADC input. Converters is
the measured delay. The total adds the 2 period queue, so it is calculated, not
measured. It also includes the ADC, which playback does not have, so it is more
than real playback latency.

| Rate / period | CM5, Pro: converters | Total | Pi 3, DAC+ ADC: converters | Total |
|---|---|---|---|---|
| 44.1 kHz / 64 | 50 frames, 1.134 ms | 4.036 ms | 31 frames, 0.703 ms | 3.605 ms |
| 48 kHz / 64 | 50 frames, 1.042 ms | 3.708 ms | 31 frames, 0.646 ms | 3.312 ms |
| 96 kHz / 128 | 52 frames, 0.542 ms | 3.208 ms | 32 to 33 frames, 0.338 ms | 3.000 ms |
| 192 kHz / 256 | 53 frames, 0.276 ms | 2.943 ms | 34 frames, 0.177 ms | 2.844 ms |

The peak level was about -4.6 dBFS on the Pro and about -6 dBFS on the DAC+ ADC. All
repeats were identical, except 96 kHz on the DAC+ ADC, which moved by one frame.

With the 73 µs handover worst case added, the CM5 stays below 4 ms at 48, 96 and
192 kHz even with the ADC included. At 44.1 kHz the total reads 4.11 ms. The ADC's
share is not known, but playback alone should be below 4 ms.

## Processing cost

Share of one period, at 256 frames, as p99.9 and then max. CM5 figures were taken
with the heatsink fitted and no throttling. The figures barely change with the
period, except for the max column at small periods.

| Case | CM5 f32 | CM5 f64 | Pi 3 f32 | Pi 3 f64 |
|---|---|---|---|---|
| Mix 8 sources, 48 kHz | 0.0%, 0.2% (64 frames) | 0.1%, 0.2% (64 frames) | 0.7%, 1.5% (64 frames) | 1.0%, 7.2% (64 frames) |
| High, 44.1 to 48 kHz | 1.0%, 1.5% | 1.7%, 2.0% | 12.0%, 17.8% | 22.0%, 28.7% |
| Medium, 44.1 to 48 kHz | 0.4%, 0.5% | 0.6%, 0.8% | 3.6%, 5.8% | 5.9%, 7.9% |
| High, 48 to 96 kHz | 1.8%, 2.2% | 3.3%, 3.6% | 22.4%, 24.8% | 39.9%, 44.4% |
| Medium, 48 to 96 kHz | 0.7%, 1.2% | 1.0%, 1.9% | 6.7%, 8.9% | 10.1%, 12.2% |
| High, 48 to 192 kHz | 3.6%, 4.2% | 6.6%, 6.8% | 43.0%, 46.2% | 76.2%, 80.7% |
| Medium, 48 to 192 kHz | 1.4%, 1.7% | 2.0%, 2.5% | 12.9%, 14.8% | 19.7%, 22.1% |

Across all period sizes the worst single cycle in 64-bit on the Pi 3 was 102.9% of
a period with `high` and 39.2% with `medium`, both 96 to 192 kHz at 64 frames. In
32-bit the same cases reached 61.9% and 29.9%. The
worst in 64-bit on the CM5 was 8.9%. `medium` in 64-bit costs less than `high` in
32-bit, so lowering the quality saves more than lowering the float format. The
Pi 3 is about 10.6 times slower than the CM5 at resampling.

Resampling delay is 69 frames with `high` and 34 with `medium` at 44.1 to 48 kHz.
At 48 to 96 kHz it is 128 and 64.

## Heat

Without a heatsink the CM5 sat at about 74 °C near idle. From 78 °C, one busy core
took it below its top clock within 5 seconds. It then spent most of a benchmark run
between 1.5 and 2.2 GHz at 85 °C. Under full load it reached 96 °C and fell to 1 GHz,
with one reading at 250 MHz. It still ran every period with no xruns.

With a heatsink it held 2.4 GHz through the benchmark at no more than 57 °C. Under
full load it reached 84 °C and held 2.2 to 2.3 GHz, touching the soft temperature
limit.

The first CM5 `dsp-bench` runs were discarded for this reason: resampling read up
to 50% slower late in a run. A Pi 5 class board in a closed case can throttle during
normal use, so processing figures from such a board can read higher.

The Pi 3 had no throttling at idle or under one busy core. It throttled only under
full load.
