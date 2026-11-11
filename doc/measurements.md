# Measurements

Stage 1 measured what the hardware can do, before the daemon was written.
[Stage 2](#stage-2) measured the daemon itself. The probes are in `tools/measure`.
The Pi 4 is still to be measured.

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
  format can be set in the config. `i32` mixing was measured in stage 2 on both
  boards: it costs more than either float, 3.5 times `f32` on the Pi 3 and 7 times
  on the CM5. Mixing costs little on any board, about 1% of a period on the Pi 3
  at 48 kHz. Resampling costs 1.35 to
  1.7 times as much in 64-bit.
- **Resampling quality:** `high` by default. With a filter table of the ratio's
  exact phases, `high` to 192 kHz on the Pi 3 uses about 10% of a period in 32-bit
  and 17% in 64-bit, and the worst single cycle at any period was 27% and 33%. So
  `high` fits on the Pi 3 in both formats. With the interpolating resampler it
  used about 44% and 80%, and could overrun in 64-bit. See
  [Exact phases](#exact-phases).
- **Default period on CM5:** 64 frames at 44.1 and 48 kHz, 128 at 96 kHz, 256 at
  192 kHz, with 2 periods.
- **Default period on Pi 3:** 256 frames at 44.1 and 48 kHz, 512 at 96 kHz, 1024
  at 192 kHz, with 2 periods. That is about 11 to 12 ms.
- **4 ms on the CM5 holds,** with the handover cost included and under full load.
  With the daemon in stage 2, `aas` adds two periods there, 2.6 to 3.0 ms through
  the cable. Of four hour runs under full load, one at 96 kHz had one wake-up gap
  longer than the buffer, and its repeat passed. It stands in for the Pi 5, which
  uses the same processor. It is not confirmed on the Pi 4.
- **`PREEMPT_RT` is not needed.** The standard kernel holds every default on both
  boards, under full load. The one exception so far is the single wake-up gap in a
  CM5 hour run in stage 2, which fell within 5 ms of the end of a synchronous
  write.
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

The table below was taken in stage 1 with a resampler that interpolates between
filter phases. The daemon no longer resamples that way. See
[Exact phases](#exact-phases) for what it costs now.

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

### Exact phases

Between two fixed rates the sinc filter only needs a fixed set of phases, the
output rate divided by the greatest common divisor of the two rates. The resampler
now holds exactly those in its table and takes the nearest, which is always exact,
instead of interpolating between phases. A unit test checks that a tone resampled
this way is as pure as one resampled by interpolating, with the same delay.

One full `dsp-bench` run on each board timed both ways in the same build, idle,
at real-time priority: 168 of 168 cases on each. Share of one period at 256 frames,
p99.9 and then max, on exact phases:

| Case | CM5 f32 | CM5 f64 | Pi 3 f32 | Pi 3 f64 |
|---|---|---|---|---|
| High, 44.1 to 48 kHz | 0.4%, 0.9% | 0.6%, 1.1% | 3.4%, 5.8% | 5.6%, 8.6% |
| Medium, 44.1 to 48 kHz | 0.2%, 0.3% | 0.3%, 0.8% | 2.1%, 4.3% | 3.3%, 5.5% |
| High, 48 to 96 kHz | 0.6%, 0.8% | 0.9%, 1.1% | 5.2%, 7.4% | 8.7%, 11.7% |
| Medium, 48 to 96 kHz | 0.4%, 0.6% | 0.6%, 0.7% | 3.3%, 5.4% | 5.4%, 11.0% |
| High, 48 to 192 kHz | 1.2%, 1.4% | 1.7%, 2.3% | 10.4%, 14.4% | 17.2%, 21.0% |
| Medium, 48 to 192 kHz | 0.8%, 0.9% | 1.1%, 1.2% | 6.8%, 9.6% | 10.8%, 13.9% |
| High, 96 to 192 kHz | 1.2%, 1.5% | 1.7%, 2.2% | 10.6%, 15.5% | 17.0%, 22.9% |
| Medium, 96 to 192 kHz | 0.8%, 2.0% | 1.1%, 2.1% | 6.7%, 12.0% | 10.9%, 16.0% |

The interpolating figures from the same run agree with the stage 1 table above:
on the Pi 3, `high` from 48 to 192 kHz took 42.4%, 49.3% in 32-bit and 76.8%, 85.0%
in 64-bit.

Mean time per period at 64 frames, in µs, interpolating and then exact:

| Case | CM5 | Pi 3 |
|---|---|---|
| High f32, 44.1 to 48 kHz | 13.5, 4.8 | 152.5, 41.4 |
| High f64, 44.1 to 48 kHz | 23.9, 7.5 | 275.9, 68.2 |
| High f32, 48 to 192 kHz | 46.1, 14.4 | 534.8, 125.2 |
| High f64, 48 to 192 kHz | 79.6, 21.6 | 987.1, 211.6 |
| Medium f32, 44.1 to 48 kHz | 5.0, 3.0 | 44.9, 26.4 |

Over all cases on both boards, exact phases are 1.6 to 4.8 times faster on the
mean. `high` on exact phases costs less than `medium` did by interpolating. The
delay is unchanged.

Across all period sizes the worst single cycle on the Pi 3 is now 26.7% of a period
in 32-bit and 33.7% in 64-bit, both 96 to 192 kHz at 64 frames. Interpolating, the
same run reached 60.8% and 109.8%. On the CM5 the worst is 4.6%. 64-bit costs 1.35
to 1.6 times 32-bit on the CM5 and 1.5 to 1.7 times on the Pi 3. The Pi 3 is about
8.9 times slower than the CM5 at this.

The Pi 3 ran for 68 minutes, from 55 to 68 °C. Its throttle flags show a frequency
cap and throttling at some time since boot, and none while they were read. The CM5
ran for 6.5 minutes with the heatsink, from 51 to 62 °C. Its flags also show only
past events.

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

## Stage 2

The daemon, the ALSA plugin and real players, measured on the Pi 3 test board and
on the CM5 test board. The CM5 could not be reached during most of stage 2, because
its SSH server reset every connection before the banner, so its runs were made
afterwards, with the audio loop that waits for room. Each figure below names its
board.

The daemon was built from this repository with the `alsa-output` feature and ran
as root with real-time rights. Players reached it through the plugin as ALSA's
`default` device. The config for each run is given with it.

### Summary

| Criterion | Pi 3 result | CM5 result | Verdict |
|---|---|---|---|
| Bit-exact through `snd-aloop`, 36 cases | 36 of 36 at `latency_ms = 48`. 5 of 36 at 24, from `snd-aloop` xruns | 36 of 36 at `latency_ms = 48` | Pass at 48 ms on both |
| Added latency, against the 12 ms target on the Pi 3 and 4 ms on the CM5 | Two periods: 11.6 ms at 44.1 kHz, 10.6 ms at 48 to 192 kHz | Two periods: 3.0 ms at 44.1 kHz, 2.6 to 2.7 ms at 48 to 192 kHz | Pass on both |
| One hour at 44.1 kHz, full load | 0 xruns, 0 wake-up gaps, worst gap 7.4 ms against an 11.6 ms buffer | 0 xruns, 0 wake-up gaps, worst gap 1.5 ms against a 2.9 ms buffer | Pass on both |
| One hour at 96 kHz, full load | 0 xruns, 0 wake-up gaps, worst gap 6.6 ms against a 10.7 ms buffer | First run 0 xruns but 1 wake-up gap, 4.0 ms against a 2.7 ms buffer. Repeated: 0, 0, worst gap 1.4 ms | Pass on the Pi 3. **Fail** on the CM5 by one gap, then a pass |
| One hour at 44.1 kHz with a second client at 96 kHz, full load | 0 xruns, 0 wake-up gaps, worst gap 9.1 ms against an 11.6 ms buffer | 0 xruns, 0 wake-up gaps, worst gap 1.5 ms against a 2.9 ms buffer | Pass on both |
| `mpd` and `squeezelite`, alone and together | Both play, mixed, 0 xruns | Both play, mixed, 0 xruns | Pass on both |
| Mixing cost in `i32` | About 1.2% of a period at 48 kHz, 8 sources | About 0.3% of a period at 48 kHz, 8 sources | Measured |

The first attempt at the CM5 hour runs stopped when the board stopped responding,
90 seconds in. The cause is not known, and it did not happen again. See
[Hour runs](#hour-runs).

### Bit-exactness

`bit-check` plays a seeded pseudo-random stream at full scale through `default`,
captures the other end of `snd-aloop` and compares the two frame by frame. Each
frame of the stream encodes its own position, so a jump in the sequence counts as
a break, and a dropout filled with silence shows up too. The daemon ran with
`card = "Loopback"`, each processing format in turn, and its default volume of
0 dB. Each case lasts 10 seconds. With one client at 0 dB the output is
bit-perfect, so the processing format should make no difference, and it made none.

With `latency_ms = 48`, all 36 cases gave `differing 0, breaks 0`: S16, S24 and S32,
at 44.1, 48, 96 and 192 kHz, in `i32`, `f32` and `f64`. The daemon counted 0 xruns
and 0 wake-up gaps. The same 36 cases passed again with the same counts after the
audio loop was changed to wait for room before reading the clients.

With `latency_ms = 24`, the setting first chosen for `snd-aloop`, only 5 of the 36
passed. The daemon counted 32, 31 and 35 xruns in the `i32`, `f32` and `f64` runs,
about one per case, and each failure looks like one. At the fault the capture repeats about a buffer
of earlier frames, then holds a buffer of silence or of frames that fit nowhere,
then skips ahead a few hundred frames. The faults fall at random places in the
stream. One case run three times at each latency gave 4 xruns and one failure at
24 ms, and 0 xruns and no failure at 48 ms. The cause is `snd-aloop`'s timer: the
board's kernel runs at HZ = 250, so the timer ticks every 4 ms, and with a capture
running on the other end it now and then misses a 21 ms buffer. Earlier the same buffer held with no capture running. Bit-exactness does not depend on the period
size, so 48 ms proves the same thing. Timing is measured on the real card.

A negative check: with the volume at 99%, a 3 second case reported every one of
its 144000 frames as differing.

On the CM5 the same 36 cases at `latency_ms = 48` all gave `differing 0, breaks 0`,
and the daemon counted 0 xruns and 0 wake-up gaps in each format. Its kernel also
runs at HZ = 250, so 48 ms was kept there and 24 ms was not tried. The daemon
chose 1024 frames per period on `snd-aloop` at every rate.

### Latency through the cable

On the Pi 3 the daemon ran with its defaults on the DAC+ ADC: a 12 ms target, so
256 frames at 44.1 and 48 kHz, 512 at 96 kHz and 1024 at 192 kHz, with 2 periods
buffered.

A player's stream cannot be linked to the card's capture, so `loopback-latency`
times both ends instead. It writes the chirp into a full buffer, notes when the
write returns, and takes the capture's start from the earliest of its reads
against the card's position. The player's own buffer is taken off. That leaves the
time from a frame leaving the player's buffer to reaching the ADC. Timed this way,
direct `hw:` reads about 40 frames more than the linked method finds for the
converters, at every rate. So the latency `aas` adds is the figure through `aas`
minus the figure for direct `hw:`, both timed this way. 20 repeats each. The player
used the daemon's period with 4 periods buffered, because the daemon refuses a 2
period buffer at 96 kHz.

| Rate / period | Converters, linked | Direct, timed | Through `aas`, timed, median | Added by `aas` | In daemon periods |
|---|---|---|---|---|---|
| 44.1 kHz / 256 | 31 frames, 0.70 ms | 1.63 ms | 13.18 ms | 11.55 ms | 1.99 |
| 48 kHz / 256 | 31 frames, 0.65 ms | 1.50 ms | 12.11 ms | 10.62 ms | 1.99 |
| 96 kHz / 512 | 32 to 33 frames, 0.34 ms | 0.75 ms | 11.34 ms | 10.59 ms | 1.99 |
| 192 kHz / 1024 | 34 frames, 0.18 ms | 0.37 ms | 10.92 ms | 10.56 ms | 1.98 |

Every repeat added two periods, within 0.1 ms of the median. The converter delays
match stage 1 exactly.

**This meets the Pi 3's 12 ms target.** The daemon waits for room on the card,
then reads the clients' rings and writes the period at once, so a frame waits
behind the card's two buffered periods and nothing else.

Before that change the engine read the rings first and then blocked in the card
write, and the same runs measured three periods: 17.4 ms at 44.1 kHz and 15.9 ms
at 48 and 96 kHz.

The daemon counted 0 xruns. At 192 kHz the probe stops at the player's first
`-EPIPE`, and some attempts did, through `aas` and with direct `hw:` alike: one
in two through `aas` in the final runs, and one in two direct. The probe reads its
capture in the same loop as it writes, so its own loop is the likely cause. A
plain `aplay` at 192 kHz through `aas`, with buffers of 2048, 4096 and 8192
frames, had no underrun in 5 seconds each, before the change. That a player is
never starved at 192 kHz is not proven.

On the CM5 the daemon ran with its defaults on the DAC+ ADC Pro: a 4 ms target, so
64 frames at 44.1 and 48 kHz, 128 at 96 kHz and 256 at 192 kHz, with 2 periods
buffered. The method is the same, 20 repeats each, and the direct and through `aas`
figures are both medians. The player used the daemon's period with 4 periods
buffered, except through `aas` at 192 kHz: there the daemon refuses a 1024 frame
buffer from a client at the 4 ms target, so the player used 8 periods, 2048
frames. 16 periods gave the same figure within 0.02 ms, and so did direct `hw:`
with 8 periods, so the player's buffer does not change the result.

| Rate / period | Converters, linked | Direct, timed | Through `aas`, timed, median | Added by `aas` | In daemon periods |
|---|---|---|---|---|---|
| 44.1 kHz / 64 | 50 frames, 1.134 ms | 1.48 ms | 4.52 ms | 3.04 ms | 2.09 |
| 48 kHz / 64 | 50 frames, 1.042 ms | 1.39 ms | 4.01 ms | 2.62 ms | 1.96 |
| 96 kHz / 128 | 52 frames, 0.542 ms | 0.68 ms | 3.37 ms | 2.69 ms | 2.02 |
| 192 kHz / 256 | 53 frames, 0.276 ms | 0.35 ms | 3.00 ms | 2.65 ms | 1.99 |

**This meets the 4 ms target on the CM5.** `aas` adds two periods there too. Every
repeat through `aas` was within 0.17 ms of its median, and the direct repeats
within 0.12 ms. The converter delays match stage 1 exactly. The daemon counted 0
xruns and 0 wake-up gaps, and no player hit `-EPIPE` at any rate. Counted from the
player's write to the ADC, with the ADC and the method's 40 frames included, the
path through `aas` reads 4.52 ms at 44.1 kHz and 3.0 to 4.0 ms at the other rates.

### Hour runs

On the Pi 3 the daemon ran with its defaults on the DAC+ ADC, 256 frames at
44.1 kHz and 512 at 96 kHz, 2 periods buffered, with the audio loop that waits for
room before it reads the clients. Full load is the stage 1 recipe: one `sha256sum` per
core and a repeated 128 MB synchronous write, with 5 seconds between writes. The
music was public domain orchestral recordings from the Musopen collection,
resampled to 44.1 kHz 16-bit and 96 kHz 24-bit, a 5 minute stretch of each kept in
RAM and looped by `cat` into `aplay -D default -t raw` for an hour. The Pi 3 has
no heatsink. From 5 minutes into each run it sat at 86 to 87 °C and
440 to 660 MHz, throttled, the same worst case as stage 1.

A fresh daemon was started for each run and stopped when its players ended, so
its counters cover that hour, plus a few start-up cycles at 48 kHz. For the third
run the output went idle when the players ended and closed 60 seconds later, and
its counters were printed then.

| Run | Daemon xruns | Wake-up gaps over the buffer | Worst gap / buffer | Worst cycle | `aplay` underruns |
|---|---|---|---|---|---|
| 44.1 kHz | 0 | 0 | 7.44 / 11.61 ms | 0.83 ms | 0 |
| 96 kHz | 0 | 0 | 6.60 / 10.67 ms | 4.65 ms | 0 |
| 44.1 kHz, then a second client at 96 kHz | 0 | 0 | 9.15 / 11.61 ms | 7.91 ms | 0 and 0 |

All three pass. In the third run the output stayed at 44.1 kHz and the 96 kHz client
was resampled at `high` quality in `f32`: 630902 cycles of 256 frames are an hour
at 44.1 kHz and the minute of idle after it.

The worst cycles of 4.65 and 7.91 ms are long next to the work in them, which
stage 1 timed at a few percent of a period. With the loop waiting for room first,
the work must fit in the one period still queued, 5.3 to 5.8 ms here, and the
7.91 ms cycle did not cause an xrun. Where that time went is not known. The cycle
time runs from the end of the wait for room to the end of the cycle, without the
write to the card. It includes reading the commands and waking the clients, and
any time interrupts take from the audio thread.

No hour run was made on an otherwise idle Pi 3. The design's criterion is an hour
at 44.1 kHz and at 96 kHz on an idle system, so stage 2 neither meets nor fails it
as written. The runs here are under full load, which is the harder test. The same
holds for the CM5.

On the CM5 the runs were made the same way: the daemon's defaults on the DAC+ ADC
Pro, 64 frames at 44.1 kHz and 128 at 96 kHz with 2 periods buffered, the same
full load and the same music from RAM, and a fresh daemon for each run. The CM5
writes the load's 128 MB to an NVMe drive, not an SD card. With the heatsink it sat
at 82 to 86 °C through the runs, at its soft temperature limit, and ran between
1.5 and 2.4 GHz, at 2.1 GHz or more in four readings of five.

| Run | Daemon xruns | Wake-up gaps over the buffer | Worst gap / buffer | Worst cycle | `aplay` underruns |
|---|---|---|---|---|---|
| 44.1 kHz | 0 | 0 | 1.54 / 2.90 ms | 0.05 ms | 0 |
| 96 kHz | 0 | **1** | **4.01 / 2.67 ms** | 0.06 ms | 0 |
| 96 kHz, repeated | 0 | 0 | 1.42 / 2.67 ms | 0.06 ms | 0 |
| 44.1 kHz, then a second client at 96 kHz | 0 | 0 | 1.53 / 2.90 ms | 0.11 ms | 0 and 0 |

**The first 96 kHz run fails the criterion by one event.** It had no xrun and
`aplay` reported no underrun, but one wake-up came 4.01 ms after the one before,
longer than the 2.67 ms buffer, so a dropout is suspected and not proven. The
daemon logged it at 6017.795 s on `CLOCK_MONOTONIC`. The load's write loop logged
the end of a 128 MB synchronous write at 6017.79 s, from `/proc/uptime`, within
5 ms of the gap. That points at the drive's completion work, not at the daemon,
but one event does not prove it. The 96 kHz run, repeated after the other runs
with the same load, passed with a worst gap of 1.42 ms. The other runs passed with a worst
gap of about half the buffer, as in stage 1. The worst cycle stayed below 0.11 ms,
under 8% of a period, so the CM5 shows nothing like the long cycles of the throttled
Pi 3.

The cycle counts cover the hours: 2482672 cycles of 64 frames at 44.1 kHz and
2701536 of 128 frames at 96 kHz are 3603 s each. In the third run the output stayed
at 44.1 kHz, with the 96 kHz client resampled, until the first player ended.

The first attempt at these runs did not finish. About 90 seconds into the 44.1 kHz
run, with the board at 75 °C and 2.4 GHz, the board stopped answering: its SSH
server reset every connection before the banner, the same symptom that kept the
CM5 out of reach earlier in stage 2, and the logs on its drive stopped. It did not
restart by itself. It came back from a restart about 7 hours later, with its file
system not cleanly unmounted. Nothing from that boot was kept, so the cause is not
known. 15 minutes of the full load alone did not repeat it, and neither did the 4
hours of runs above, made with the logs in RAM and the kernel log streamed off the
board.

#### Before the change, and the SD card

The first hour runs, made before the loop waited for room, read the music from the
SD card. Two of them failed the criterion by one event each. At 44.1 kHz the
daemon counted 1 xrun and 1 wake-up gap of 13.41 ms against an 11.61 ms buffer, at
96 kHz 1 gap of 12.06 ms against 10.67 ms. A third run, with a second client and
the music in RAM, passed with a worst gap of 6.92 ms, close to stage 1's 6.80 ms.

A gap longer than the buffer cannot come from a long cycle in that loop. Each
cycle started with the card full, so even 6.7 ms of work left about 4 ms queued.
The gaps need the wake-up itself to be late by more than a period, for example
interrupt work holding the CPU. The reading `aplay` was also the difference
between the failing runs and the passing one. It ran into the SD card's write
load: `aplay` underran 696 times in the 44.1 kHz run and 1494 times in the 96 kHz
run, and a check of 5 minutes each under the same load settles where that came
from:

| Player | Underruns |
|---|---|
| `aplay` through `aas`, file on the SD card | 62 |
| `aplay` through `aas`, the same music from RAM | 0 |
| `aplay` straight to `hw:0,0`, daemon stopped, file on the SD card | 61 |

A player that reads from the SD card while something else writes to it with
`fsync` drops out on a Pi 3, with or without `aas`, so the runs above keep the
music in RAM. That the SD card's interrupt load also delayed the daemon's wake-ups
in those two runs fits, but is not proven: they gave one event each. The daemon
now logs each gap over the buffer as it happens, with its time on
`CLOCK_MONOTONIC`, so that a future gap can be matched to load events.

### Players

`mpd` and `squeezelite` from Debian, each alone and then together at different
rates, through `default` with the daemon's defaults. This was run before the audio
loop was changed to wait for room first, and was not repeated after. `mpd` played a 44.1 kHz file
with `mixer_type "hardware"` and `mixer_control "Master"`. `squeezelite` ran with
its default ALSA settings, which use mmap, and played a 96 kHz FLAC. A minimal
SlimProto server stood in for Lyrion: it sends one start command, unity gain and
a status request every 5 seconds. Each phase ran for about 30 seconds.

| Phase | Output rate | Level through the cable, RMS / peak |
|---|---|---|
| `mpd` alone | 44.1 kHz | 0.051 / 0.40 |
| `squeezelite` alone | 96 kHz | 0.024 / 0.11 |
| `mpd`, then `squeezelite` joins | 44.1 kHz, `squeezelite` resampled | 0.081 / 0.49 |
| `squeezelite`, then `mpd` joins | 96 kHz, `mpd` resampled | 0.033 / 0.16 |

Both players played in every phase, and the output stayed at the first one's
rate. `mpd`'s position and the elapsed time `squeezelite` reported both advanced
in real time, about 30 seconds in 30 seconds. `squeezelite` reported no underrun,
and the daemon ended with 0 xruns and 0 wake-up gaps. `mpc volume 50` set `Master`
to 70%, -18 dB, through `mpd`'s own curve for a wide dB range.

Without a gain command from the server, `squeezelite` plays silence. The first run
did that, until the stand-in server sent unity gain as Lyrion does.

On the CM5 the same four phases ran with the audio loop that waits for room, the
daemon's defaults, 64 frames at 44.1 kHz and 128 at 96 kHz, and a fresh daemon for
each phase. The setup was the same, with `mpd` 0.24.4 and `squeezelite`
2.0.0-1517 from Debian. The level was captured at the rate the card ran at.

| Phase | Output rate | Level through the cable, RMS / peak |
|---|---|---|
| `mpd` alone | 44.1 kHz | 0.117 / 0.89 |
| `squeezelite` alone | 96 kHz | 0.067 / 0.53 |
| `mpd`, then `squeezelite` joins | 44.1 kHz, `squeezelite` resampled | 0.114 / 0.89 |
| `squeezelite`, then `mpd` joins | 96 kHz, `mpd` resampled | 0.131 / 1.00 |

Both players played in every phase and the output stayed at the first one's rate.
`mpd`'s position advanced 20 seconds in 20 seconds in each phase. The elapsed time
`squeezelite` reported advanced 29.0 seconds in 29.1 seconds alone, and 24.0 in
24.1 after joining `mpd`. Each daemon ended with 0 xruns and 0 wake-up gaps.
`mpc volume 50` again set `Master` to 70%, -18 dB. In the last phase the sum of
the two players reached full scale at the ADC.

A first CM5 run went wrong in the test setup, and showed something about the card.
The stand-in server sent a malformed start command, so `squeezelite` held the
output at 44.1 kHz and played nothing, and the level was captured at a fixed
96 kHz. In that run `mpd`'s position advanced 44 seconds in 20, and the daemon
logged one wake-up gap of 502 ms. Neither came back in two runs with the same
order of phases. The fast clock is the card's, not `aas`'s: with the `,slave`
overlay the Pi clocks playback and capture together, and a 96 kHz capture opened
while the card plays 44.1 kHz makes the playback run at the faster clock. Straight
to `hw:0,0` with the daemon stopped, a 10 second 44.1 kHz file played in 5.15
seconds when a 96 kHz capture was opened one second in. 96 / 44.1 is 2.18, which
matches `mpd`'s 44 seconds in 20. The 502 ms gap came half a second after such a
capture closed, which fits the same cause but is not proven. On this card, capture
only at the rate the playback runs at.

### Processing cost of `i32`

`dsp-bench --mix-only` on the Pi 3, idle, at real-time priority. `i32` mixes the
way the daemon does: each gain is turned into fixed point, each product is taken in
128 bits and rounded once, the sum is kept in 64 bits, and only the result
saturates. Share of a period, p99.9 and then max, 8 sources:

| Case | `i32` | `f32` | `f64` |
|---|---|---|---|
| 48 kHz, 64 frames | 1.6%, 6.1% | 0.7%, 10.0% | 1.0%, 5.0% |
| 48 kHz, 256 frames | 1.2%, 3.4% | 0.5%, 3.0% | 0.7%, 2.6% |
| 192 kHz, 256 frames | 4.7%, 13.3% | 1.8%, 6.5% | 4.5%, 11.8% |

On the mean, `i32` costs about 3.5 times `f32` and 1.8 times `f64`. It is still
small, but it saves no CPU. The board had throttled earlier since boot, but ran at
1.2 GHz and 60 to 67 °C during the run.

On the CM5, idle, with the heatsink, at 2.4 GHz and 54 to 55 °C, never throttled:

| Case | `i32` | `f32` | `f64` |
|---|---|---|---|
| 48 kHz, 64 frames | 0.2%, 0.7% | 0.0%, 0.3% | 0.1%, 0.3% |
| 48 kHz, 256 frames | 0.3%, 0.5% | 0.0%, 0.1% | 0.1%, 0.2% |
| 192 kHz, 256 frames | 1.0%, 1.4% | 0.1%, 0.9% | 0.3%, 1.0% |

On the mean at 48 kHz and 256 frames, `i32` costs about 7.0 times `f32` and 3.3
times `f64` on the CM5: 12.5 µs against 1.8 and 3.8 µs. That is a larger ratio than
on the Pi 3, but the share of a period stays at about 1% or less.

### Player wake-ups

The daemon used to wake every client after every period. It now wakes a client
once when the read position the client waits for is reached. On the Pi 3 test
board, 15 seconds of `aplay` through the plugin, with the daemon at 256 frames and
48 kHz, before and after:

| Case | Player wake-ups | Player CPU | Daemon CPU |
|---|---|---|---|
| 48 kHz, `aplay` defaults | 2817, then 120 | 0.18 s, then 0.04 s | 0.13 s, then 0.20 s |
| 48 kHz, buffer 100 ms, period 25 ms | 2816, then 601 | 0.14 s, then 0.08 s | 0.12 s, then 0.19 s |
| 44.1 kHz resampled beside a 48 kHz player | 2817, then 120 | 0.14 s, then 0.04 s | 2.57 s, then 0.93 s |

CPU time is counted in ticks of 10 ms, so the small figures are coarse. The
daemon's fall in the last row is the resampler on exact phases. Its rise in the
first two rows, of 7 ticks in 15 seconds, is not explained.

The plugin's `pause_and_resume` test failed 40 of 40 runs on the Pi 3, because the
stream played one period more after a pause. With the audio loop reading its
commands after the wait for room, it passed 40 of 40.
