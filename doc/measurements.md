# Measurements

Stage 1 measured what the hardware can do, before the daemon was written.
[Stage 2](#stage-2) measured the daemon itself, [Stage 3a](#stage-3a) its
analog input and plugins, [Stage 3b, part 1](#stage-3b-part-1) the mixing
matrix and the output's channel count, and [Stage 3b, part 2](#stage-3b-part-2)
several outputs. The probes are in `tools/measure`.
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
  longer than the buffer, and its repeat passed. Such a gap is a break in the
  sound, see [Late wake-ups and the tone](#late-wake-ups-and-the-tone). It stands in for the Pi 5, which
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
  so a run only passes if it has no xrun and no wake-up gap longer than the
  buffer. A tone played through the cable settled it on both boards: a gap
  longer than the buffer is a break in the sound, with or without an xrun. See
  [Late wake-ups and the tone](#late-wake-ups-and-the-tone).
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
longer than the 2.67 ms buffer. No tone was captured in that run, so what it did
to the sound was not heard, but the later test under
[Late wake-ups and the tone](#late-wake-ups-and-the-tone) reproduced the same
figures, a 4.00 ms gap with no xrun, and each time the card played one buffer
of old audio again. The daemon logged it at 6017.795 s on `CLOCK_MONOTONIC`. The load's write loop logged
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

### Late wake-ups and the tone

The hour runs left one question open: when the audio loop wakes later than the
card's buffer lasts and ALSA counts no xrun, did the sound break? It did, every
time, on the CM5 test board and on the Pi 3 test board.

`tone-probe` plays a 1100 Hz sine at -6 dBFS through `default` and captures it from
the ADC through the cable, as S32 at 48 kHz. A sine obeys
`x[n] = 2cos(w) x[n-1] - x[n-2]`, so what is left after that is the noise, until
the tone is cut, repeated or shifted. The probe calibrates on one second, marks a
break where the residual passes 8 times the largest one of that second or the
level falls below half, and prints each break with its time on `CLOCK_MONOTONIC`,
its length and how far the tone is shifted after it. The capture came back at
-6.1 dBFS with a residual of -91 dBFS RMS on the CM5, which put the threshold 53
to 54 dB under the tone, and at -6.0 dBFS with -85 to -88 dBFS on the Pi 3, 48 to
50 dB under the tone. The probe's own capture never overran and its player never
underran in any run below.

#### CM5

The daemon ran from its unit with its defaults: 64 frames at 48 kHz, 2 periods,
a 2.67 ms buffer. Straight to the card for 20 s and through the daemon for 300 s,
idle, the probe heard no break.

`stall` then made wake-ups late on purpose, one every 1 or 2 seconds at a random
point in the period, in batches of 30 or 150 stalls of one length, with a fresh
daemon for each batch so that its counters cover that batch. Two kinds:

- **The thread is late.** The audio thread was pinned to one CPU and `stall` held
  that CPU at `SCHED_FIFO` 99 for a set time. The card's interrupt is served on
  time and only the daemon is late.
- **The interrupt is late.** `stall` set thousands of timers to expire at one
  instant on CPU 0, which serves the card's DMA interrupt. The kernel works
  through them with interrupts off, about 65 µs per 1000 timers. The length is
  measured from the hole in a busy loop on that CPU. When the DMA fails, the
  kernel's handling of it adds to the hole, so the lower figure of each range is
  the stall itself.

CM5, thread late, 30 stalls per row:

| Stall | Gaps logged over the buffer | Xruns counted | Breaks heard |
|---|---|---|---|
| 0.5 ms | 0, worst gap 1.85 ms | 0 | 0 |
| 1 ms | 0, worst gap 2.36 ms | 0 | 0 |
| 2 ms | 25, 2.98 to 5.69 ms | 25 | 25, 4.8 to 5.2 ms of silence |
| 2.5 ms | 30, 2.99 to 5.77 ms | 30 | 30, 5.0 to 5.2 ms |
| 3 ms | 30, 5.64 to 6.29 ms | 30 | 30, 4.9 to 5.7 ms |
| 4 ms | 30, 6.05 to 7.55 ms | 30 | 30, 5.5 to 7.0 ms |
| 6 ms | 30, 8.02 to 9.33 ms | 30 | 30, 7.4 to 8.7 ms |
| 10 ms | 30, 12.02 to 13.38 ms | 30 | 30, 11.3 to 12.7 ms |

Each logged gap had its break within a few milliseconds and each break its gap,
205 of each, and ALSA counted all 205. A stall that ends while audio is still
queued does nothing: between one and two periods are queued at any time, so a
2 ms stall broke the tone 25 times in 30. The gap includes the restart of the
card, which is why it is longer than the stall.

CM5, interrupt late. The stalls counted are those the probe was listening to,
with no earlier outage still running:

| Stall | Stalls | No gap, no break | Short silence, xrun counted | Outage of about 1.5 s, no xrun | One buffer played again, no xrun |
|---|---|---|---|---|---|
| 0.5 to 0.8 ms | 30 | 30 | 0 | 0 | 0 |
| 0.7 to 2.2 ms | 148 | 147 | 1 | 0 | 0 |
| 1.0 to 2.5 ms | 178 | 160 | 12 | 4 | 2 |
| 1.2 to 3.0 ms | 147 | 102 | 28 | 14 | 3 |
| 1.5 to 3.3 ms | 26 | 14 | 8 | 4 | 0 |
| 2.0 to 3.1 ms | 22 | 5 | 9 | 8 | 0 |
| 2.5 to 3.9 ms | 19 | 0 | 8 | 11 | 0 |
| 3.0 to 4.5 ms | 18 | 0 | 6 | 12 | 0 |
| 4.1 to 6.1 ms | 22 | 0 | 14 | 8 | 0 |
| 6.0 to 8.9 ms | 20 | 0 | 13 | 7 | 0 |
| 9.9 to 14.6 ms | 18 | 0 | 10 | 8 | 0 |

Again every stall with a logged gap had a break and every break a gap, 190 of
each. What ALSA made of them differs:

- **Short silence, 109 times.** The card stopped, ALSA reported the xrun, the
  daemon restarted the card. The gap was 3.0 to 17.4 ms and the silence 3.9 to
  16.8 ms, both growing with the stall. The daemon's xrun count for a batch
  equals the short silences heard in it, in every batch whose stalls were all
  heard.
- **One buffer played again, 5 times.** The gap was 4.00 ms each time, three
  periods, and the daemon counted no xrun. The tone did not go silent. It broke
  for 0.6 to 0.7 ms and ran 128 frames late from then on: the card had gone once
  more round its buffer of old audio before ALSA looked, so ALSA saw nothing
  wrong. These are the figures of the hour run at 96 kHz, a 4.01 ms gap with no
  xrun.
- **Outage, 76 times.** The first gap was 2.7 to 15.2 ms with no xrun. Then the
  card took no more audio and ALSA reported no error. The daemon's wait for room
  ended after its 500 ms limit, the restart did not bring the card back, and the
  daemon closed the card and opened it again a second later. The tone was gone
  for 1.44 to 1.53 s. Two more were cut short by the end of the listening. The kernel
  logged `dma dma2chan2: Bad descriptor submitted` for each of these, from the
  DMA controller of the RP1: its interrupt came too late to renew the transfer,
  and the driver gave the transfer up. So on a CM5, and by the same chip on a
  Pi 5, a late interrupt can stop the card for good until it is reopened.

Last on the CM5, 15 minutes of tone under the full load, one `sha256sum` per core
and 171 writes of 128 MB with `fsync`, at 83 to 86 °C: no break, no gap over the buffer,
0 xruns, worst gap 1.44 ms, no DMA error. So the load did not make a late
interrupt in that time, as in three of the four hour runs.

#### Pi 3

The same on the Pi 3 test board with the DAC+ ADC, first with the daemon's
defaults there, 256 frames at 48 kHz, 2 periods, a 10.67 ms buffer. Straight to
the card for 20 s and through the daemon for 300 s, idle, the probe heard no
break.

The card's DMA interrupt goes to all four CPUs in turn on this board, so for the
interrupt stalls `stall` set its timers on all four at once, about 0.69 ms per
1000 timers on each. That holds the audio thread as well, whichever CPU it is on.
The length is the time from the instant the timers were due until a sleep due at
the same instant returned.

Pi 3, 10.67 ms buffer, thread late, 30 stalls per row:

| Stall | Gaps logged over the buffer | Xruns counted | Breaks heard |
|---|---|---|---|
| 2 ms | 0, worst gap 7.50 ms | 0 | 0 |
| 4 ms | 0, worst gap 10.05 ms | 0 | 0 |
| 6 ms | 4, 17.07 to 17.62 ms | 4 | 4, 13.3 to 14.2 ms of silence |
| 8 ms | 18, 16.92 to 21.36 ms | 18 | 18, 12.8 to 17.5 ms |
| 10 ms | 28, 17.12 to 21.65 ms | 28 | 28, 12.8 to 19.5 ms |
| 12 ms | 30, 18.02 to 24.62 ms | 30 | 30, 14.2 to 21.0 ms |
| 16 ms | 30, 22.31 to 27.75 ms | 30 | 30, 18.8 to 24.4 ms |
| 24 ms | 30, 30.62 to 35.80 ms | 30 | 30, 27.0 to 32.3 ms |
| 40 ms | 30, 56.94 to 62.50 ms | 30 | 30, 53.0 to 58.8 ms |

200 gaps, 200 xruns, 200 breaks, one for one, as on the CM5.

Pi 3, 10.67 ms buffer, interrupt late:

| Stall | Stalls | No gap, no break | Silence, xrun counted | Buffer played again, no xrun |
|---|---|---|---|---|
| 2.0 to 3.2 ms | 30 | 30 | 0 | 0 |
| 5.6 to 6.3 ms | 30 | 24 | 3 | 3 |
| 6.0 to 9.4 ms | 30 | 17 | 11, 2 of them with no gap logged | 2 |
| 8.0 to 12.5 ms | 30 | 8 | 19 | 3 |
| 10.0 to 15.6 ms | 30 | 1 | 15 | 14 |
| 12.0 to 18.7 ms | 30 | 0 | 12 | 18 |
| 16.1 to 24.9 ms | 29 | 0 | 7 | 22 |
| 23.9 to 37.2 ms | 28 | 0 | 13 | 15 |
| 39.8 to 61.6 ms | 26 | 0 | 13 | 13 |

Then with `latency_ms = 4`, which gives 64 frames and a 2.67 ms buffer, because
the 4.05 ms gap with no xrun that started this question was seen on this board
with that buffer. Idle for 300 s there was no break, with a worst gap of 1.88 ms.

Pi 3, 2.67 ms buffer, thread late, 30 stalls per row:

| Stall | Gaps logged over the buffer | Xruns counted | Breaks heard |
|---|---|---|---|
| 0.5 ms | 1, 5.43 ms | 1 | 1 |
| 1 ms | 1, 5.31 ms | 1 | 1 |
| 2 ms | 17, 3.89 to 6.63 ms | 19 | 19 |
| 2.5 ms | 30, 4.52 to 7.14 ms | 30 | 30 |
| 3 ms | 30, 5.56 to 7.90 ms | 30 | 30 |
| 4 ms | 30, 6.27 to 9.48 ms | 30 | 30 |
| 6 ms | 30, 8.06 to 11.44 ms | 30 | 30 |
| 10 ms | 30, 12.46 to 14.77 ms | 30 | 30 |

Pi 3, 2.67 ms buffer, interrupt late, 50 stalls per row:

| Stall | No gap, no break | Silence, xrun counted | Buffer played again, no xrun |
|---|---|---|---|
| 0.5 to 0.8 ms | 50 | 0 | 0 |
| 1.0 to 1.7 ms | 29 | 12, 8 of them with no gap logged | 9 |
| 1.5 to 2.4 ms | 6 | 10, 5 of them with no gap logged | 34 |
| 2.8 to 3.2 ms | 0 | 0 | 50 |
| 3.5 to 4.0 ms | 0 | 3 | 47 |
| 3.0 to 4.8 ms | 0 | 23 | 27 |
| 4.1 to 6.3 ms | 0 | 14 | 36 |
| 6.0 to 9.4 ms | 0 | 17 | 33 |
| 10.0 to 15.6 ms | 0 | 33 | 17 |

What the Pi 3 shows:

- **Every logged gap was a break,** 902 of 902 over the four tables.
- **The buffer played again is the common result of a late interrupt here,** 343
  times, never with an xrun. The card goes round its buffer of old audio once
  for each buffer length the interrupt is late, a break every 10.67 ms or every
  2.67 ms, and the tone runs one buffer late for each. After a stall of 1 to
  3.2 ms with the small buffer the gap was 4.03 to 4.10 ms in all but a few: the
  4.05 ms gap with no xrun of the earlier run is this.
- **Silence** is ALSA seeing the underrun and the daemon restarting the card, 13
  to 18 ms with the default buffer. With a long stall it comes after one to five
  rounds of old audio. The daemon's xrun count equals the silences in every
  batch whose stalls were all heard.
- **17 silences had no gap logged.** Each was counted as an xrun. The wake-up
  came within the buffer time, but less than a full buffer was queued, because
  the write follows the wake-up by the cycle's own time, and the card had run
  dry a hair before the gap reached the buffer. 15 of them were interrupt
  stalls and 2 thread stalls. So the gap log alone is not enough either, and a run
  needs both: no gap over the buffer and no xrun.
- **No outage.** The card never stopped taking audio, the daemon never ran into
  its 500 ms limit, and the kernel logged nothing during any run. The outage of
  the CM5 belongs to the RP1's DMA controller. The Pi 3 uses the one in the
  BCM2837.
- With the small buffer a thread stall of 0.5 or 1 ms broke the tone once in
  30. The idle worst gap of 1.88 ms leaves under a millisecond to spare there.

Last, 15 minutes of tone under the full load with the default buffer, 42 writes
of 128 MB to the SD card, throttled at 86 to 87 °C and 550 to 600 MHz: no break,
no gap over the buffer, 0 xruns, worst gap 6.37 ms.

#### What this settles

- A wake-up gap longer than the buffer is broken audio, on both boards. 395 gaps
  were heard on the CM5 and 902 on the Pi 3, and every one was a break.
- ALSA's xrun count is right when only the daemon is late, and not to be trusted
  when the interrupt is late. It counted 109 of 190 such breaks on the CM5 and
  205 of 548 on the Pi 3. The breaks it missed include the longest.
- The gap log is not to be trusted alone either. On the Pi 3, 17 breaks came with
  a gap just under the buffer, and all 17 were xruns. On the CM5 no break came
  without a gap. A run passes with no gap over the buffer and no xrun, which is
  the design's criterion.
- A gap of 500 ms is the daemon's wait limit and marks an outage. That happened
  on the CM5 only.

What it leaves open: the stalls were made, not found, and all at 48 kHz. How often
the full load makes the interrupt late is known only from the hour runs, once in
four hours on the CM5 and not at all on the Pi 3 with its default buffer.

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

## Stage 3a

The analog input and the plugins, measured on the Pi 3 test board and on the CM5
test board. Each figure names its board.

The daemon ran as root with real-time rights, built with the `alsa-output`
feature, its defaults on each card, and this config:

```toml
[analog]
enabled = true
volume_db = -20
```

So the detector ran with its defaults: -50 dBFS, a hold of 3 s, 50 ms windows.
The DAC output is cabled to the ADC input on both boards, so anything played
comes back through the analog input and is mixed in again, 20 dB down and
less the converters' loss. At -20 dB that loop dies away. Players reached
the daemon through the plugin as ALSA's `default` device.

The hour runs were made on commit 3c2efde. Everything else was built from
fe1b6f5, with the probes as committed in 55ed179. fe1b6f5 adds fixes to forced
bit-perfect and to plugin library paths, which change nothing measured here.

### Summary

| Criterion | Pi 3 result | CM5 result | Verdict |
|---|---|---|---|
| `detect` and `balance` under 1% of a period on the Pi 3 | p99.9 at most 0.3% at 48 kHz and 0.8% at 192 kHz and 256 frames. 1.0% at 192 kHz and 64 frames | p99.9 at most 0.1% | Pass, except p99.9 at 192 kHz and 64 frames on the Pi 3, which reads 1.0% |
| Shared clock: an hour with a tone and the input open, padding and dropping stay at their start-up value | 256 frames padded, all at the start, 0 dropped, 0 xruns, 0 wake-up gaps | 64 frames padded, all at the start, 0 dropped, 0 xruns, 0 wake-up gaps | Pass on both |
| Analog path, at most three periods and the converters | 720 frames, 15.00 ms, against 799 | 222 frames, 4.625 ms, against 242 | Pass on both |
| One hour at 44.1 kHz, full load | **2 xruns, 2 wake-up gaps, 27.1 and 33.1 ms against an 11.6 ms buffer.** Repeated: **1 xrun**, 0 wake-up gaps, worst gap 11.09 ms, 278 frames dropped | 0 xruns, 0 wake-up gaps, worst gap 1.54 ms against 2.90 ms | **Fail** on the Pi 3, twice, from memory compaction moving the daemon's locked pages, not from the input. Pass on the CM5 |
| One hour at 96 kHz, full load | 0 xruns, 0 wake-up gaps, worst gap 10.16 ms against 10.67 ms | 0 xruns, 0 wake-up gaps, worst gap 1.45 ms against 2.67 ms | Pass on both, with little room on the Pi 3 |
| One hour at 44.1 kHz with a second client at 96 kHz, full load | 0 xruns, 0 wake-up gaps, worst gap 6.82 ms against 11.61 ms | 0 xruns, 0 wake-up gaps, worst gap 1.59 ms against 2.90 ms | Pass on both |
| Detection: opens within 100 ms, closes 3.0 to 3.1 s after the tone | Opens 4 to 52 ms after, closes 3.012 to 3.055 s after, 20 of 20 | Opens 3 to 51 ms after, closes 3.001 to 3.041 s after, 20 of 20 | Pass on both |
| `dist/test-plugin.sh`, without and with `ANALOG=1` | 27 of 28 and 31 of 32. The failure is the `mpd` case: `mpd` is not installed | Not run | Pass, except the case that needs `mpd` |
| ADC start-up above -50 dBFS, covered by the 1 s of silence | Never above -50 dBFS. The loudest 50 ms window after a start was -83.8 dBFS | Above -50 dBFS for up to 700 ms at 44.1 kHz | The 1 s covers both. The DAC+ ADC does not need it |

### Processing cost of the plugins

`dsp-bench --mix-only` times each built-in plugin alone on one block per
cycle, 100000 cycles each, idle, at real-time priority. The detector sees a
-20 dBFS tone, so it is open and passes the samples. The balance is set to 0.5,
so it scales one channel. Share of a period, p99.9 and then max:

| Case | Pi 3 `i32` | Pi 3 `f32` | Pi 3 `f64` | CM5 `i32` | CM5 `f32` | CM5 `f64` |
|---|---|---|---|---|---|---|
| `detect`, 48 kHz, 64 frames | 0.3%, 0.9% | 0.3%, 2.8% | 0.2%, 2.5% | 0.0%, 0.3% | 0.0%, 0.1% | 0.0%, 0.2% |
| `detect`, 48 kHz, 256 frames | 0.2%, 3.2% | 0.2%, 1.1% | 0.2%, 1.6% | 0.0%, 0.1% | 0.0%, 0.1% | 0.0%, 0.0% |
| `detect`, 192 kHz, 64 frames | 1.0%, 5.2% | 1.0%, 3.4% | 1.0%, 19.3% | 0.1%, 0.5% | 0.1%, 0.5% | 0.1%, 0.5% |
| `detect`, 192 kHz, 256 frames | 0.8%, 8.9% | 0.8%, 6.1% | 0.7%, 1.1% | 0.1%, 0.2% | 0.1%, 0.2% | 0.1%, 0.3% |
| `balance`, 48 kHz, 64 frames | 0.2%, 0.8% | 0.0%, 0.7% | 0.0%, 0.8% | 0.0%, 0.1% | 0.0%, 0.1% | 0.0%, 0.1% |
| `balance`, 48 kHz, 256 frames | 0.2%, 0.2% | 0.0%, 0.2% | 0.0%, 0.2% | 0.0%, 0.0% | 0.0%, 0.0% | 0.0%, 0.0% |
| `balance`, 192 kHz, 64 frames | 0.8%, 2.8% | 0.1%, 3.1% | 0.1%, 2.3% | 0.0%, 0.5% | 0.0%, 0.4% | 0.0%, 0.8% |
| `balance`, 192 kHz, 256 frames | 0.6%, 1.4% | 0.2%, 9.5% | 0.2%, 6.0% | 0.0%, 0.3% | 0.0%, 1.5% | 0.0%, 0.2% |

On the mean, the detector takes 4.9 µs for 256 frames on the Pi 3 and 0.7 µs
on the CM5, about 19 ns a frame on the Pi 3 in any format. The balance takes
0.7 µs in float and 3.1 µs in `i32` on the Pi 3, where it rounds each product
in 128 bits. At 192 kHz and 64 frames a period is 0.33 ms, so the detector's
p99.9 of 3.4 µs reads 1.0%. That period is not a default on the Pi 3, which
uses 1024 frames at 192 kHz. The worst single cycles, up to 19.3%, are of the
same size as those of the mixes in the same run, single cycles that the board
took from the benchmark.

The Pi 3 ran at 1.2 GHz, from 64 to 71 °C, right after the hour runs. The CM5
ran from 70 to 73 °C and was still coming up from 1.7 GHz to 2.4 GHz, for the
same reason. The whole run of 96 cases took 75 s on the Pi 3 and 17 s on the
CM5.

### The shared clock

The ADC and the DAC run from the same clock, so once the capture is running the
engine should read one period each cycle and never need to pad or drop. A
1 kHz tone at -20 dBFS peak, made on the board and kept in RAM, was looped into
`aplay` through `aas` for an hour, on an otherwise idle board. The input opened
when the tone started and stayed open to the end.

| Board | Padded frames | Dropped frames | Xruns | Wake-up gaps over the buffer | Worst gap / buffer | Worst cycle |
|---|---|---|---|---|---|---|
| Pi 3, 48 kHz / 256 | 256 | 0 | 0 | 0 | 5.60 / 10.67 ms | 0.42 ms |
| CM5, 48 kHz / 64 | 64 | 0 | 0 | 0 | 1.45 / 2.67 ms | 0.03 ms |

The daemon prints its counters only when it stops, so the value after the
first second was taken from short runs with the same config on the CM5: 15
to 20 s each gave 64 frames padded and 0 dropped, the same as the hour. Each
run in this section pads exactly one period each time the capture starts, at
48 kHz at start-up and again at a rate change. The Pi 3 shows the same: 256
frames in its hour, and in every short run on the Pi 3 below. So nothing was
padded or dropped after the first period, in an hour on either board.

### Latency of the analog path

The boards have no second signal source, so the echo was measured.
`loopback-latency --echo` plays a chirp through `aas` and records the capture
device, which delivers the analog input. The chirp arrives once from the
player, through the DAC, the cable and the ADC, and again after the analog
input has mixed it into the output at -20 dB and it has made the same trip a
second time. The frames between the two arrivals are the analog path: from a
frame at the ADC to the same frame at the ADC again, through the engine, the
output queue, the DAC and the cable. Both arrivals are in one recording on one
clock, so the figure is exact to the frame. A 1 kHz tone plays for 0.2 s
first, so that the detector is open when the chirp comes. 20 repeats each, at
48 kHz.

| Board | Echo after | Expected at most | In periods, without the converters |
|---|---|---|---|
| Pi 3, 256 frames | 720 frames, 15.000 ms | 3 × 256 + 31 = 799 frames, 16.65 ms | 2.69 |
| CM5, 64 frames | 222 frames, 4.625 ms | 3 × 64 + 50 = 242 frames, 5.04 ms | 2.69 |

All 20 repeats on each board gave the same frame count. The converter delays
are the ones measured in stage 1. The second arrival was 21.9 dB below the
first on the Pi 3 and 19.7 dB on the CM5. Two periods are the output queue.
The rest, 0.69 of a period on both boards, is how long a captured frame waits
for the engine to read it. That it is the same fraction on both suggests the
phase between capture and playback is set when the two start, but that is not
proven.

### Hour runs with the analog input

The three runs of stage 2, made the same way, with the analog input on: a fresh
daemon per run, stopped when its players ended, the stage 1 full load, and the
same 5 minute stretches of public domain orchestral music at 44.1 kHz 16-bit
and 96 kHz 24-bit, kept in RAM and looped into `aplay -t raw`. The music played
through the cable into the ADC, so the input opened a second after each start
and stayed open, and the music was mixed in a second time at -20 dB. Each
daemon logs the time of every wake-up gap over the buffer.

On the CM5, 64 frames at 44.1 kHz and 128 at 96 kHz, with the heatsink. It sat
at 83 to 87 °C and ran between 1.5 and 2.4 GHz.

| Run | Daemon xruns | Wake-up gaps over the buffer | Worst gap / buffer | Worst cycle | Padded, dropped | `aplay` underruns |
|---|---|---|---|---|---|---|
| 44.1 kHz | 0 | 0 | 1.54 / 2.90 ms | 0.08 ms | 128, 0 | 0 |
| 96 kHz | 0 | 0 | 1.45 / 2.67 ms | 0.07 ms | 192, 0 | 0 |
| 44.1 kHz, then a second client at 96 kHz | 0 | 0 | 1.59 / 2.90 ms | 0.11 ms | 128, 0 | 0 and 0 |

All three pass. Each capture start padded one period: one at 48 kHz when the
daemon started, and one at the rate the first player set.

On the Pi 3, 256 frames at 44.1 kHz and 512 at 96 kHz, without a heatsink. It
sat at 86 to 87 °C and 496 to 603 MHz, throttled, as in stage 2.

| Run | Daemon xruns | Wake-up gaps over the buffer | Worst gap / buffer | Worst cycle | Padded, dropped | `aplay` underruns |
|---|---|---|---|---|---|---|
| 44.1 kHz | **2** | **2** | **33.13 / 11.61 ms** | 3.92 ms | 1536, 10 | 0 |
| 96 kHz | 0 | 0 | 10.16 / 10.67 ms | 3.84 ms | 768, 0 | 0 |
| 44.1 kHz, then a second client at 96 kHz | 0 | 0 | 6.82 / 11.61 ms | 1.45 ms | 512, 0 | 0 and 0 |
| 44.1 kHz, repeated | **1** | 0 | 11.09 / 11.61 ms | 3.76 ms | 512, 278 | 0 |

**The 44.1 kHz run on the Pi 3 fails.** Ten minutes in, the daemon logged two
wake-up gaps, of 27.09 ms ending at 535053.843 s and of 33.13 ms ending at
535053.880 s on `CLOCK_MONOTONIC`, so the audio thread did not run for about
60 ms. It counted 2 xruns. The capture overran too and restarted in place:
the 1024 frames padded beyond the two start-ups, and the 10 dropped, came
from that. `aplay` reported no underrun, since its own buffer is larger. The
load's write loop logged the end of a 128 MB synchronous write at 535053.49 s,
about 0.35 s before the gap. Each write ends with the file being deleted, so
the gap fell while the SD card was finishing that write and the delete. As
with the CM5's gap in stage 2, that points at the storage's completion work
and not at the daemon, but one event does not prove it. The kernel log has
nothing at that time.

This run was the third of the three on the Pi 3, not the first. The first
attempt at 44.1 kHz did not play: the throttled board took 3.9 s to start the
daemon under load, and the script started `aplay` after 3 s, so it found no
socket. The script now waits for the socket.

The 44.1 kHz run was repeated on its own, under the same load, with the daemon
built from fe1b6f5. It failed again, differently: 1 xrun and no wake-up gap
over the buffer, but a worst gap of 11.09 ms against an 11.61 ms buffer. A
wake-up that late leaves less than a millisecond to fill the period, and the
xrun fits that. The engine also dropped 278 captured frames once, which
happens only when more than two periods have queued at the ADC, so a cycle
came late there too. The daemon does not log the time of an xrun, so it
cannot be matched to the load.

In stage 2 the same run without the analog input passed, with a worst gap of
7.44 ms and a worst cycle of 0.83 ms. With the input on, both 44.1 kHz runs
had worst cycles of 3.8 and 3.9 ms. **So the Pi 3 did not hold 44.1 kHz at
256 frames under full load with the analog input on.** The board was throttled
to 496 to 603 MHz at 86 to 87 °C, a worst case. The cause was found later and
is not the analog input: see
[Why the 44.1 kHz hour failed on the Pi 3](#why-the-441-khz-hour-failed-on-the-pi-3).

The 96 kHz run passed, but its worst gap, 10.16 ms against a 10.67 ms buffer,
leaves 0.5 ms. In stage 2 the same run without the analog input had 6.60 ms.
When in the hour that gap fell is not known: the daemon logs only gaps over the
buffer.

#### Why the 44.1 kHz hour failed on the Pi 3

The analog input is not the cause. The audio thread stalls when the kernel
migrates one of the daemon's pages while compacting memory, and that happens
with or without the input, and with the stage 2 daemon as well.

The daemon locks all its memory with `mlockall`, so none of its pages can be
swapped out or dropped. They can still be moved: with
`vm.compact_unevictable_allowed = 1`, the default on this kernel, compaction
also migrates locked pages. While a page is being moved its entries point to a
migration entry, and a thread that touches it waits in
`migration_entry_wait` until the move is done. The move is done by
`kcompactd` or by whatever task asked for the compaction, at normal priority,
and under full load it shares its CPU with a `sha256sum`. The audio thread at
`SCHED_FIFO` 70 waits behind it. The full load's 128 MB written and deleted
every 22 s keeps compaction busy on this 1 GB board: the kernel had migrated
5.9 million pages in the 6 days since its boot, about 39000 an hour, a sixth
of the board's memory.

Measured with temporary instrumentation in the daemon: a histogram of the
wake-up gaps and of the cycles, the worst time of each phase of the cycle, and
the audio thread's page faults from `getrusage` after every cycle. A kernel
trace of the scheduler, the interrupts, the audio thread's system calls and
its faults ran in a ring buffer, and the daemon stopped it after any gap over
9 ms or cycle over 2 ms, so that each trace ends at the event. All runs at
44.1 kHz, 256 frames, the stage 1 full load, the board throttled at 86 to 87 °C
and 549 to 603 MHz, the same music from RAM as the hour runs.

Ten minutes per run, full load, without forcing anything:

| Daemon | Analog input | Xruns | Gaps over the buffer | Worst gap | Cycles over 1 ms |
|---|---|---|---|---|---|
| stage 3a | on | 0 | 0 | 8.36 ms | 2, the worst 4.56 ms |
| stage 3a | off | 0 | 0 | 6.91 ms | 1 |
| stage 2, a777850 | none | 0 | 0 | 6.75 ms | 0 |
| stage 3a | on | 0 | 0 | 6.93 ms | 0 |

Ten minutes does not reproduce the failure, and the histograms show no
difference that matters: with the input on most cycles take 0.1 to 0.2 ms
instead of under 0.1 ms, against a period of 5.8 ms. The worst cycles of 3.8
and 3.9 ms in the hour runs are not the input's steady cost: single cycles of
that size came with faults, below.

To make the event frequent, a full compaction was also asked for every 3 s
(`echo 1 > /proc/sys/vm/compact_memory`), on top of the same load, five
minutes per run, each daemon fresh:

| Daemon | Analog input | Xruns | Gaps over the buffer | Faults of the audio thread | Worst gap with a fault / without |
|---|---|---|---|---|---|
| stage 3a | on | 4 | 4, up to 21.3 ms | 11 in 5 cycles | 21.3 / 8.8 ms |
| stage 3a | off | 0 | 0 | 2 | 5.9 / 6.7 ms |
| stage 2 | none | 0 | 0 | 2 | 6.4 / 6.8 ms |
| stage 2 | none | 3 | 3, up to 31.3 ms | 7 in 3 cycles | 31.3 / 6.9 ms |
| stage 3a | off | 1 | 1, 33.9 ms | 5 in 3 cycles | 33.9 / 6.8 ms |
| stage 3a | on | 2 | 1, 14.8 ms | 5 in 3 cycles | 14.8 / 6.7 ms |
| stage 3a | on | 2 | 2, 14.8 ms | 10 in 5 cycles | 14.8 / 7.0 ms |

Every gap over the buffer and every xrun fell on a cycle in which the audio
thread had faulted, and no cycle without a fault came near the buffer: the
worst gap without one was 8.8 ms, and 7.0 ms in all but the first run. The
stage 2 daemon, which has no analog input, failed the same way. The traces
show each time the same chain: the audio thread faults on a page of its heap,
its stack or the daemon's code, enters `migration_entry_wait`, sleeps
uninterruptibly, and runs again when the migrating task finishes, 0.05 to
8.7 ms later per page, for up to seven pages in a row. 22 such faults were
traced. In 10 of them the migrating task was `kcompactd`, compacting on its
own, not the forced compaction. The first three rows were one session, in
that order, and the last four another. The stage 2 daemon ran last in the
first and first in the second, and failed in the second, so which daemon
failed follows when compaction caught it, not the daemon.

The same with `vm.compact_unevictable_allowed = 0`, which keeps locked pages
out of compaction, again with a full compaction every 3 s, 115 of them:

| Daemon | Analog input | Xruns | Gaps over the buffer | Worst gap | Faults of the audio thread |
|---|---|---|---|---|---|
| stage 3a | on | 0 | 0 | 6.77 ms | 0 |
| stage 2 | none | 0 | 0 | 6.64 ms | 0 |
| stage 3a | on | 0 | 0 | 8.01 ms | 0 |

Then the acceptance run itself, twice, without forcing compaction: an hour at
44.1 kHz under the full load with the analog input on, the stage 3a daemon,
first as the kernel is set, then with `vm.compact_unevictable_allowed = 0`.

| Setting | Xruns | Gaps over the buffer | Worst gap | Worst cycle | Faults of the audio thread | Pages migrated in the hour |
|---|---|---|---|---|---|---|
| 1, as set | **2** | **3, up to 22.8 ms** | **22.82 / 11.61 ms** | 8.44 ms | 21 in 7 cycles | 29109 |
| 0 | 0 | 0 | 7.44 / 11.61 ms | 0.60 ms | 0 | 209128 |

The first hour failed as the stage 3a hours did, and both traced events were
`kcompactd` moving the audio thread's pages, 12 faults in all, the second
0.07 s after a 128 MB write and its delete had ended. Without a fault its worst
gap was 6.93 ms and its worst cycle 0.86 ms. The failures came within 25 s of
the daemon's start, when its pages were new. The second hour passed, with
seven times as many pages migrated around the daemon, and with figures close
to stage 2's hour at 44.1 kHz without the input: 7.44 ms and 0.83 ms then,
7.44 ms and 0.60 ms now.

Why the failures came in pairs: after an xrun the daemon restarts the card
with `snd_pcm_prepare` on the audio thread, and on the DAC+ ADC that prepare
took 11.6 to 15.7 ms in the traces, longer than the 11.6 ms buffer. The
codec driver mutes and powers its outputs on a prepare over I²C, and waits
for the bus each time. So one late wake-up becomes two gaps, as in the first
hour run, 27.1 and 33.1 ms. The capture's prepare took about 1.6 ms.

What this means for the earlier results:

- **The analog input does not make the Pi 3 fail.** It adds a little work and
  a few pages to the cycle, and so perhaps a little to the chance that a
  migrated page is one the cycle touches, but the stage 2 daemon fails the
  same way when compaction runs. That stage 2 passed its hour and stage 3a
  failed two is within what a rare event gives.
- **44.1 kHz is not special.** At 44.1 kHz a cycle may be 5.8 ms late before
  the card runs dry, at 96 kHz 5.3 ms. The gaps the migrated pages caused
  were 15 to 34 ms, so whether a run fails depends on whether compaction moves a page the
  cycle needs in that hour, not on the rate. The 96 kHz hour's worst gap of
  10.16 ms was likely such a stall that ended in time.
- **Why the CM5 passes** was not measured. It writes the load to an NVMe
  drive, so the load may leave it less to compact, and its faster cores
  finish a migration sooner.
- **What fixes it** is the setting, `vm.compact_unevictable_allowed = 0`,
  which is what kernels built for real time default to for this reason. It is
  a setting of the whole system, so `aas` ships it as a file for
  `/etc/sysctl.d`, `dist/90-aas.conf`, installed with the daemon, and the
  daemon warns at its start while the setting is 1. The hour above passed with
  the setting made by hand. A second hour passed with the file installed and
  the board rebooted, so that the setting came from the file: 44.1 kHz, full
  load, analog input on, 0 xruns, 0 wake gaps, worst gap 7.41 ms, worst cycle
  0.43 ms over 620770 cycles, while compaction moved 83 thousand pages and the
  board throttled at 87 °C. Locking alone cannot keep the pages in place,
  and a process has no other way to. Memory taken for the contiguous
  allocator can still move locked pages; that was not tested.

### Detection

`analog-timing` plays a 1 kHz tone at -20 dBFS peak through `aas` for 2 s, 20
times, with 1 to 2 s of silence before each and 5 s after. It prints, on
`CLOCK_MONOTONIC`, when the tone's first and last frame leave the player's
buffer. The daemon's log lines were stamped with the same clock as they were
read from its standard error. The time from the player's buffer to the ADC is
taken from stage 2, through `aas` at 48 kHz: 12.11 ms on the Pi 3 and 4.01 ms
on the CM5. That figure includes about 40 frames, 0.8 ms, that the method adds,
so the times below can read up to 1 ms long. The silence before each tone
differs by a few blocks from tone to tone, so that the tones start at different
points of the detector's 50 ms windows.

| Board | Opens after the tone reaches the ADC | Closes after the tone ends at the ADC |
|---|---|---|
| Pi 3, 256 frames | 4.1 to 52.4 ms, mean 30.5 ms | 3.012 to 3.055 s, mean 3.033 s |
| CM5, 64 frames | 3.0 to 51.1 ms, mean 20.4 ms | 3.001 to 3.041 s, mean 3.018 s |

Every tone opened the input once and closed it once, and nothing opened it
between the tones. The spread of the opening is the window: the detector
decides at the end of each 50 ms window, so a tone that starts early in a window
opens it up to 50 ms later. Closing is 3 s after the end of the last loud window,
plus the same wait. The timing has a resolution of about 1 ms.

A first run on the CM5 used the same silence before every tone. All 20 tones
then started at the same point of a window and opened 15.5 to 16.5 ms after the
tone reached the ADC, which is why the silence now varies.

### Plugin tests and the ADC start-up on the Pi 3

`dist/test-plugin.sh` ran on the Pi 3 with the daemon installed as its unit and
the plugin installed, built from fe1b6f5. Without `ANALOG=1` 27 of 28 cases
passed, and with it 31 of 32. The one failure each time is `mpd_mixer_works`,
because `mpd` is not installed on the board. The four analog cases passed: the
input opened 68 ms after the tone started and closed 3.065 s after it ended,
counted from the player's side; it stayed closed for 10 s of silence; a rate
change from 44.1 to 96 kHz with the capture open gave 0 xruns and 0 wake-up
gaps; and the capture device recorded the tone at -19.3 dBFS, all of its power
at 1 kHz.

The daemon gives zeros for the first second after each capture start, because
the ADC of the DAC+ ADC Pro starts with a transient that its high-pass filter
takes up to 700 ms to bring below -50 dBFS. On the DAC+ ADC of the Pi 3 there
is no such transient. `arecord` straight from the card, with the daemon
stopped, 5 starts each from a card at rest and 3 each with the playback running,
at 44.1, 48 and 96 kHz:

| Board | Above -50 dBFS after a start | Loudest 50 ms window |
|---|---|---|
| Pi 3, DAC+ ADC | never | -83.8 dBFS |
| CM5, DAC+ ADC Pro, 44.1 kHz | 700 ms | -26.8 dBFS |
| CM5, DAC+ ADC Pro, 48 kHz | 650 ms | -27.7 dBFS |
| CM5, DAC+ ADC Pro, 96 kHz | 300 ms | -28.9 dBFS |

From a card at rest the DAC+ ADC delivers exact zeros for the first 150 ms at
44.1 and 48 kHz and 50 ms at 96 kHz, then noise at about -84 dBFS that falls to
-105 dBFS within a second. So the 1 s of zeros covers the Pro, with 300 ms to
spare at 44.1 kHz, and costs the DAC+ ADC a second of input it would not have
to lose. The constant was left as it is.

### The ADC after a capture overrun

`aas` recovers a capture overrun by preparing and starting the stream again,
without closing it. Whether that brings the start transient of the DAC+ ADC
Pro back, which would call for the 1 s mute again, was measured on the CM5
test board with the daemon stopped and the input quiet: a small program read
`hw:sndrpihifiberry` as S32 stereo with a buffer of about 85 ms, stalled for
300 ms to overrun it, recovered with `snd_pcm_prepare` and `snd_pcm_start`,
and measured the first 700 ms after each restart: the loudest 50 ms window
(RMS over both channels) and the peak. Twelve restarts at 44.1 kHz and twelve
at 48 kHz.

| | First open | Restarts, loudest 50 ms | Restarts, peak |
|---|---|---|---|
| 44.1 kHz | -26.8 dBFS | -102.4 to -97.9 dBFS | -92.3 to -90.5 dBFS |
| 48 kHz | -27.7 dBFS | -102.8 to -98.5 dBFS | -92.7 to -90.0 dBFS |

The transient comes with the open, not with a start: no restart came within
48 dB of the detector's -50 dBFS. So a restart after an overrun is not muted.

### Idle output with the analog input on

With the analog input on, `aas` keeps the playback open and writes exact zeros
when nothing plays, because the card's clock and the engine's cycle come from
the playback. The question was whether closing the playback while the capture
runs would change anything. Measured on both boards with the daemon stopped,
with `arecord` and `aplay` on the card's own device, S32 stereo at 48 kHz both
ways, and the mixer as it was. One run per board: the playback fed zeros (A),
then closed with the capture still running (B), then one attempt to play at
44.1 kHz, then zeros again (A), then closed again. The capture rate was taken
from its hardware position in `/proc/asound` every 0.25 s.

| | Pi 3, DAC+ ADC | CM5, DAC+ ADC Pro |
|---|---|---|
| Capture rate in A, in B, after B to A | 48001, 47976, 48006 Hz | 47991, 47998, 48001 Hz |
| Capture errors, kernel log | none, nothing | none, nothing |
| Noise at the ADC in A, 10 s, RMS and peak | -102.5, -88.6 dBFS | -103.0, -89.2 dBFS |
| The same in B | -99.9, -76.4 dBFS | -100.2, -77.0 dBFS |
| Peak within 100 ms of A to B, B to A, A to B | -95.1, -94.2, -89.0 dBFS | -91.8, -91.1, -91.9 dBFS |
| Another player at 44.1 kHz in B | Accepted. The capture stayed at 48 kHz. A peak of -53.8 dBFS within 100 ms of its start | Accepted. The capture slowed to about 44 kHz and stayed there, without an error, until a 48 kHz playback opened again |

The clocks keep running in B on both boards: the capture went on at 48 kHz
with no error across every change. B is about 2.7 dB noisier than A on both
boards, with peaks 12 dB higher, which shows as single peaks of about -77 dBFS
in the 10 s. Why is not known. No change from A to B or back made a click
above -89 dBFS.

The DAC's registers, read from the driver's register map, were the same in A
and in B on both boards: register 108, the analog mute monitor, 0x00, and the
clock detection registers 91 to 94 at 0x30, 0x00, 0x40, 0x40. With nothing
open, 91 was 0x00 and 94 was 0x68 on the CM5 and 0x6a on the Pi 3. Register
118, the power state, is not in the driver's register map and could not be
read. The card's "Auto Mute Switch" and "Auto Mute Mono Switch" were on, with
"Auto Mute Time" at 21 ms, on both boards.

The playback being open does matter in one way. In B the playback device is free,
and a player that opens it at another rate is accepted. On the CM5, where the Pi
drives the clock, that moved the capture to the 44.1 kHz clock with no error
reported anywhere, and it stayed there after the player had gone. On the Pi 3
the capture kept its rate, with a click at the start. Holding the playback open,
as `aas` does, prevents both.

What this cannot show without a scope: the DAC's analog output itself, below
the ADC's own noise of about -103 dBFS, whether the DAC's charge pump or output
stage changes state in B, and anything faster or quieter than a 50 ms window of
the capture shows.

### What these figures do not show

- The hour runs were made with music loud enough to keep the input open all
  hour. An input that opens and closes often during a run was tested only for
  20 tones, not under load.
- The shared clock was measured at 48 kHz only. The counters are printed only
  when the daemon stops, so the first second was taken from separate short runs.
- The analog path was measured at 48 kHz only.
- The detection times rest on the stage 2 latency figure for the player's side.
- Each card was tested on one board only.

## Stage 3b, part 1

The mixing matrix, the output's channel count (`channels`) and plugins for any
channel count, measured on the CM5 test board and on the Pi 3 test board. Both
carry stereo cards, so more than two channels were run against `snd-aloop`.
Each figure names its board.

The daemon was built from a5e9a5a with the `alsa-output` feature and ran as
root with real-time rights, a fresh daemon for each run. `bit-check` was built
with its `--channels` and `--expect` options as committed in 94715aa, and
`dist/test-plugin.sh` ran as committed in cef9447. Players reached the daemon
through the plugin as ALSA's `default` device. The `main` daemon in the cost
comparison was built from 2ce4e21. `/etc/sysctl.d/90-aas.conf` was installed
on both boards.

### Summary

| Criterion | Pi 3 result | CM5 result | Verdict |
|---|---|---|---|
| Bit-exact through `snd-aloop`: the default matrix, a swap, and eight channels with players on 1, 2 and 7, 8 | Not run | 36 of 36 cases: `differing 0, breaks 0`, and no frame off the routing | Pass on the CM5 |
| Stereo with the default matrix costs what `main` costs, 10 minutes at 48 kHz, 3 runs each | Worst cycle within `main`'s spread in `f32` and `f64`. **The mean CPU time of the audio thread per cycle is 16% higher in `f32` and 18% in `f64`**, 12 to 15 µs, against a spread of under 1 µs | Not run | **Fail** on the mean, by a margin far outside the spread. Pass on the worst cycle. After the direct mixing path: 0.7% in `f32`, within the spread, and 0.9% in `f64`, just above it (below) |
| Eight channels to `snd-aloop`, `gain` on all eight, 10 minutes at 48 kHz | 1.9% of a period on the mean in `f32`, 2.7% in `f64`; worst cycle 2.9% and 3.7% | Not run | Measured |
| One hour at 44.1 kHz, full load, real card, swap matrix, analog input on | 0 xruns, 0 wake-up gaps, worst gap 6.89 ms against 11.61 ms | 0 xruns, 0 wake-up gaps, worst gap 1.56 ms against 2.90 ms | Pass on both |
| One hour at 96 kHz, the same | 0 xruns, 0 wake-up gaps, worst gap 6.43 ms against 10.67 ms | 0 xruns, 0 wake-up gaps, worst gap 1.45 ms against 2.67 ms | Pass on both |
| A swap moves a left-only tone to the right output, through the cable | Not run | -20.1 dBFS on the left without the swap, -20.0 dBFS on the right with it; the other channel 110 dB or more below | Pass on the CM5 |
| `dist/test-plugin.sh` with the capture and `ANALOG=1` | Not run | 32 of 33. The failure is the `mpd` case: `mpd` is not installed | Pass, except the case that needs `mpd` |
| `channels = 8` on the stereo card | The card refuses 8 channels when probed. The null sink plays and the reason is logged once | **The card says it takes 8 channels when probed, then refuses them when opened. The null sink plays, but each retry costs the audio thread about 3.4 ms, which counts as a wake-up gap every second** | As designed on the Pi 3. A finding on the CM5, below |

The stereo cost and the `channels = 8` finding were taken up after the figures
here were taken. The default stereo matrix now takes a direct mixing path, as
`main` does. The figures in this section are those of the build named above,
before both changes, except the stereo cost measured again after the direct
path, under "Processing cost on the Pi 3".

### Bit-exactness through `snd-aloop`

The method of [stage 2](#bit-exactness): `bit-check` plays a seeded
pseudo-random stream at full scale through `default`, captures the other end
of `snd-aloop` and compares the two frame by frame. The stream is stereo. With
`--channels` the capture takes as many channels as the daemon opened, and
`--expect` names the channels that should carry the stream's left and right:
the first channel of each side is compared with the stream, every other channel
named must be a copy of it, and every channel not named must be zero. A frame
that breaks this counts as off the routing and fails the case.

The CM5 ran with `card = "Loopback"`, `latency_ms = 48`, the daemon's default
volume of 0 dB, and four configs, each in `i32`, `f32` and `f64`:

| Config | `channels` | Routes | Checked as |
|---|---|---|---|
| Default | 2 | none, so left to 1 and right to 2 | `1=L,2=R` |
| Swap | 2 | `players.left` to 2, `players.right` to 1 | `1=R,2=L` |
| Eight, default | 8 | none, so left to 1, right to 2, the rest silent | `1=L,2=R` |
| Eight | 8 | `players.left` to 1 and 7, `players.right` to 2 and 8 | `1=L,2=R,7=L,8=R` |

Each config and format ran S16 at 44.1 kHz, S24 at 96 kHz and S32 at 192 kHz,
10 seconds each. All 36 cases gave `differing 0, breaks 0` and 0 frames off
the routing, and each of the 12 daemons counted 0 xruns and 0 wake-up gaps,
with a worst gap of 28.0 to 31.7 ms against buffers of 42.7 to 46.4 ms. The
daemon chose 1024 frames per period on `snd-aloop` at 44.1 and 48 kHz, as in
stage 2, 2048 at 96 kHz and 4096 at 192 kHz. With one client at 0 dB and pure
routes the output is bit-perfect, so the samples are copied, swapped or
duplicated as they are, and the processing format made no difference.

Two negative checks, in `f32`, on a stream of 144000 frames, 3 s at 48 kHz and
shorter than the 10 s cases: the swap checked as `1=L,2=R` reported every
one of its 144000 frames as differing, with left and right exchanged; and the
eight channels checked as stereo, `1=L,2=R` with channels 3 to 8 to be silent,
reported every frame off the routing, the first one showing channels 7 and 8
equal to 1 and 2.

The Pi 3 was not run: bit-exactness does not depend on the board, and stage 2
found `snd-aloop` there needs 48 ms as on the CM5.

### Processing cost on the Pi 3

The daemon on the DAC+ ADC with its defaults, so 256 frames at 48 kHz and
2 periods buffered, and `bit_perfect = "off"`, so that every cycle mixes in
the processing format instead of copying. One `aplay` played music at 48 kHz,
the 44.1 kHz recording of the hour runs resampled to 48 kHz beforehand and kept
in RAM, for 10 minutes per run. The board was otherwise idle and sat at 54 to
67 °C, at 600 MHz in most readings and 1.2 GHz in 33 of 282, as the `ondemand`
governor chose; its throttle flags showed only past events. The runs of `main`
at 2ce4e21 and of this stage alternated, three of each per format, f32 first.

The worst cycle is the daemon's own figure: from the end of the wait for room
to the end of the cycle, without the write to the card. The mean is the CPU
time of the audio thread from `/proc/<pid>/task/<tid>/schedstat`, read just
before the daemon stopped, divided by the cycles it counted. So the mean also
holds the write to the card, the wait's system calls and the wake-up, which the
worst cycle does not.

| Daemon | Format | Mean per cycle, three runs | Share of a period | Worst cycle, three runs |
|---|---|---|---|---|
| `main` | `f32` | 75.7, 76.1, 76.1 µs | 1.4% | 0.244, 0.176, 0.153 ms |
| this stage | `f32` | 89.4, 87.8, 88.1 µs | 1.7% | 0.182, 0.190, 0.183 ms |
| `main` | `f64` | 81.5, 81.7, 82.2 µs | 1.5% | 0.136, 0.164, 0.210 ms |
| this stage | `f64` | 96.0, 96.5, 97.1 µs | 1.8% | 0.206, 0.261, 0.194 ms |

Every run counted 0 xruns and 0 wake-up gaps, with a worst gap of 5.45 to
5.67 ms against a 10.67 ms buffer.

**The stereo figures are not within the run-to-run spread of `main` on the
mean.** This stage takes 12.5 µs more per cycle in `f32`, 16%, and 14.7 µs more
in `f64`, 18%, where the three runs of `main` lie within 0.4 and 0.7 µs of each
other. Every run of this stage is above every run of `main`, and the clock
readings do not explain it: the first `main` run, with no reading at 1.2 GHz,
gave the same mean as the third, with five. The worst cycles are within
`main`'s spread in both formats. Where the extra time goes was not measured.
It is 0.2 to 0.3% of a period.

The same runs again after the direct mixing path, with the branch at 84965e4
and `main` at 2ce4e21, alternated as before, three of each per format:

| Daemon | Format | Mean per cycle, three runs | Mean | Worst cycle, three runs |
|---|---|---|---|---|
| `main` | `f32` | 77.6, 76.6, 76.9 µs | 77.0 µs | 0.276, 0.133, 0.158 ms |
| this stage | `f32` | 78.5, 76.7, 77.6 µs | 77.6 µs | 0.177, 0.201, 0.165 ms |
| `main` | `f64` | 83.0, 82.2, 82.1 µs | 82.4 µs | 0.181, 0.137, 0.183 ms |
| this stage | `f64` | 83.0, 83.1, 83.5 µs | 83.2 µs | 0.198, 0.196, 0.257 ms |

Every run counted 0 xruns and 0 wake-up gaps. In `f32` the stage's mean is
0.6 µs, 0.7%, above `main`'s and inside the range of `main`'s runs. In `f64` it
is 0.8 µs, 0.9%, above, and 0.2 µs above `main`'s highest run. Against 16% and
18% before, the matrix now costs the default config about what `main` costs;
what is left is within a microsecond, 0.02% of a period.

`i32`, which is not the default processing format, was not run on the board.
An x86 figure, not one of the Pi 3: timed with `tests/cycle_cost.rs` after the
direct mixing path, an `i32` cycle costs 1 to 3% more than `main`'s, and the
difference is in reading the analog input through its chain, not in the mix.

Eight channels, the same player, to `snd-aloop` with `channels = 8`,
`latency_ms = 48`, so 1024 frames per period, `bit_perfect = "off"`, a route
from `players.left` to 1, 3, 5 and 7 and from `players.right` to 2, 4, 6 and 8,
and the `gain` library at `place = "output"` with `gain = -3`, so that it
works on all eight channels. One run per format:

| Format | Mean per cycle | Share of a period | Per frame | Worst cycle | Xruns, gaps |
|---|---|---|---|---|---|
| `f32` | 395.7 µs | 1.9% | 0.39 µs | 0.608 ms, 2.9% | 0, 0 |
| `f64` | 576.9 µs | 2.7% | 0.56 µs | 0.785 ms, 3.7% | 0, 0 |

Per frame, eight channels with a plugin cost about as much as stereo without
one in `f32`, 0.39 against 0.35 µs, and half again as much in `f64`, 0.56
against 0.38 µs. `latency_ms = 48` was used because a first try at 24 ms,
512 frames, gave 1 xrun and a 32 ms wake-up gap in 20 seconds, against a
21 ms buffer, which stage 2 traced to `snd-aloop`'s timer on this kernel.

### Hour runs with a swap matrix

The runs of [stage 3a](#hour-runs-with-the-analog-input), made the same way
but with every route swapped: a fresh daemon per run on the real card with its
defaults, the stage 1 full load, and the same music from RAM looped into
`aplay -t raw` for an hour. The config:

```toml
[analog]
enabled = true
volume_db = -20

[[matrix]]
from = "players.left"
to = 2

[[matrix]]
from = "players.right"
to = 1

[[matrix]]
from = "analog.left"
to = 2

[[matrix]]
from = "analog.right"
to = 1
```

The music played through the cable into the ADC, so the input opened a second
after each start and stayed open, and came back swapped once more at -20 dB.
The two boards were run at overlapping times. The CM5 hours ran while the
Pi 3 measured its cost, and the Pi 3 hours came after. On the CM5 the logs were kept in
RAM and copied off every two minutes, and the kernel log was streamed off the
board.

On the CM5, 64 frames at 44.1 kHz and 128 at 96 kHz, with the heatsink. It sat
at 82 to 87 °C and ran between 1.5 and 2.4 GHz.

| Run | Daemon xruns | Wake-up gaps over the buffer | Worst gap / buffer | Worst cycle | Padded, dropped | `aplay` underruns |
|---|---|---|---|---|---|---|
| 44.1 kHz | 0 | 0 | 1.56 / 2.90 ms | 0.07 ms | 128, 0 | 0 |
| 96 kHz | 0 | 0 | 1.45 / 2.67 ms | 0.08 ms | 192, 0 | 0 |

Both pass, within 0.02 ms of stage 3a's figures without the matrix. The board did
not hang, and the kernel log has nothing from the runs. The kernel migrated
no pages in either hour.

On the Pi 3, 256 frames at 44.1 kHz and 512 at 96 kHz, without a heatsink. It
sat at 86 to 87 °C and 442 to 603 MHz, throttled, as in stage 3a.

| Run | Daemon xruns | Wake-up gaps over the buffer | Worst gap / buffer | Worst cycle | Padded, dropped | `aplay` underruns |
|---|---|---|---|---|---|---|
| 44.1 kHz | 0 | 0 | 6.89 / 11.61 ms | 0.65 ms | 512, 0 | 0 |
| 96 kHz | 0 | 0 | 6.43 / 10.67 ms | 0.76 ms | 256, 0 | 0 |

Both pass. The kernel migrated 17748 pages during the 44.1 kHz hour and 5552
during the 96 kHz hour, with `vm.compact_unevictable_allowed = 0` from the
installed file. Faults of the audio thread were not counted. The worst gaps are close
to stage 2's hours without the analog input, 7.44 and 6.60 ms, and far from
stage 3a's 96 kHz hour, 10.16 ms, made before the setting. The worst cycles,
0.65 and 0.76 ms, are a little above the 0.43 to 0.60 ms of the hours with the
setting in stage 3a, at an eighth of a period. In the 96 kHz run the capture
padded 256 frames, one period at 48 kHz at the start, and nothing when the rate
changed, where stage 3a padded one more period each time; why was not looked
into.

### A swap through the cable

The CM5 with its defaults on the DAC+ ADC Pro, the analog input off, and
`arecord` reading the card's ADC directly at 48 kHz while a player sent 3 s
of a 1 kHz tone at -20 dBFS peak on the left channel only, with silence on the
right, through `aas` at 48 kHz. Level at 1 kHz over the loudest second, per
channel:

| Config | Channel 1 | Channel 2 |
|---|---|---|
| No matrix | -20.1 dBFS | -134.7 dBFS |
| The swap of the hour runs | -131.2 dBFS | -20.0 dBFS |
| No matrix, again | -20.1 dBFS | -131.8 dBFS |

So the swap moved the tone to the right output, at the same level, and left
nothing of it on the left that the ADC can show. There was no ear at the
board, so it was not listened to.

`dist/test-plugin.sh` now has the same check as a case,
`matrix_swaps_the_channels`, run when `AAS_TEST_CAPTURE` names the capture
device. On the CM5, with the daemon installed as its unit for the run and
removed after, `AAS_TEST_CAPTURE=plughw:sndrpihifiberry ANALOG=1` passed 32 of
33 cases. The one failure is `mpd_mixer_works`, because `mpd` is not installed.
The matrix case read the tone at -20.1 dBFS on the left before and -20.0 dBFS
on the right with the swap, the other channel at -72 dBFS by the case's own
measure, which takes the loudest tenth of a second of the noise at 1 kHz.

### Eight channels on a stereo card

The plan treats a card that does not take the configured channel count as
missing: the null sink plays, the reason is logged once, and the open is tried
again every second. Both boards were run with only `channels = 8` in the config,
while a player played for 3 s.

On the Pi 3 that is what happens. The daemon logged
`hw:sndrpihifiberry: does not take 8 channels` once at the start, played to the
null sink, counted 0 xruns and 0 wake-up gaps, and the kernel logged nothing.
Nothing reached the ADC.

On the CM5 the card answers the probe differently from the open. The probe
logged `card hw:sndrpihifiberry takes 8 channels`: the card's configuration
space allows 8 channels. Opening the card then fails in `snd_pcm_hw_params`
with `Invalid argument`, and the kernel logs
`designware-i2s ... ASoC error (-22): at snd_soc_dai_hw_params()`. So the daemon
played to the null sink and nothing reached the ADC, as it should. But:

- The reason it logged is the failure of the call, `apply: ... Invalid argument
  (22)`, not that the card does not take 8 channels.
- Each retry, once a second, opens the card and fails in the kernel on the
  audio thread, and took 3.4 to 4.1 ms. With the null sink at the CM5's period
  of 64 frames, a 2.67 ms buffer, the daemon counted that as a wake-up gap
  every second: 7 in 7.5 s, the worst 4.11 ms.
- The kernel logged two lines per retry, so two a second for as long as the
  config stays.

Nothing played wrongly. This was taken up after the figures above were taken:
a refusal of a channel count above 2 is now logged as "does not take N
channels" once, and not retried every second. See Mixing matrix in
`doc/design.md`. This section does not hold a measurement of that fix.

### What these figures do not show

- No board has a card with more than two channels, so more than two channels
  were checked only through `snd-aloop`, which proves the samples, not timing
  on a real card.
- The cost comparison is on the Pi 3 only, at 48 kHz and 256 frames, idle,
  with the mixing forced by `bit_perfect = "off"`. Bit-perfect stereo, the
  usual case, was not timed. The mean includes the audio thread's system calls,
  so the extra time may be in the cycle's work or around it.
- The eight-channel cost ran at 1024 frames, not at the stereo runs' 256, so the
  two compare per frame only.
- The swap through the cable was checked on the CM5 only, at 48 kHz, with the
  analog input off. The hour runs had it on, but a swap of the analog input was
  checked only in that the hour passed.
- A matrix change at run time was not measured on a board: only the config sets
  a matrix in this stage, and the daemon reads it at its start.

## Stage 3b, part 2

Several outputs: a further output that follows the master's clock through an
adaptive resampler, held in time with it. Measured on the CM5 test board and on
the Pi 3 test board, each with its DAC as the master and `snd-aloop` as the
further output, since neither board has a second card. Each figure names its
board.

The daemon was built from 968e42d with the `alsa-output` feature and ran as
root with real-time rights, a fresh daemon for each run. `align-check` and the
`--tone` mode of `bit-check` were built as committed with this section, and
`dist/test-plugin.sh` ran as committed with it too. The `main` daemon in the
cost comparison was built from c95432e. `/etc/sysctl.d/90-aas.conf` was
installed on both boards. The 5000 ppm clamp and the Pi 3's 96 kHz hour failed
on that build; they were fixed (see Lock and drift and Hours below) and
measured again on b4fd1a6, built the same way. The further output was always:

```toml
[[output]]
card = "Loopback"
latency_ms = 48
```

with the flat `card` naming the DAC, which the master plays at its defaults:
on the CM5 64 frames at 44.1 and 48 kHz and 128 at 96 kHz, on the Pi 3 256 and
512. `snd-aloop` cannot run the master's short periods, so the loopback has its
own `latency_ms`: 1024 frames per period at 44.1 and 48 kHz and 2048 at
96 kHz, three buffered.

### Summary

| Criterion | Pi 3 result | CM5 result | Verdict |
|---|---|---|---|
| Lock at shifts of 0, +100, -300 and +1000 ppm, at 44.1 and 96 kHz: time to lock, ppm, delay error after lock | Not run | Locked 5.0 to 6.9 s after each rate change in 17 of 17 runs, and kept it. The daemon's ppm within 3 ppm of the measured drift in 14 runs, 3.4 to 11.6 off in 3. Delay error at a shift of 0 under a frame over the last minute; with a shift set 7 to 28 frames, up to 171 frames while settling | Pass. `snd-aloop`'s shift slows the card: 100010 is -100 ppm |
| At 5000 ppm the clamp holds and is logged | Not run | After the fix, 44.1 kHz, 180 s each way: the clamp line 30.6 s (card fast) and 31.7 s (card slow) after the rate change, never `locked`, the ppm at the stop +5069 and -4956, 0 xruns and 0 wake-up gaps on the master. Before it the line never came in 120 to 300 s and a fast card was logged `locked` four times | Pass after the fix |
| The tone reaches the loopback with the residual of the resampler's test; a swap of that output swaps only there | Not run | -132 to -135 dBFS in `f32` and -132 to -134 dBFS in `f64` at 48 kHz against a moving sine; the swap moved the tone on the loopback only | Pass |
| Alignment against the converters' delay | Not run | The master 14 to 15 frames after the loopback beyond the converters: +0.32 ms at 44.1 and 48 kHz, +0.15 ms at 96 kHz, steady within a frame | Measured; the method cannot place the frames |
| One hour at 44.1 kHz, full load, analog input on, loopback output at 100010 | 0 xruns, 0 wake-up gaps, worst gap 6.99 ms against 11.61 ms; the output locked throughout, 0 xruns, starves and overflows | 0 xruns, 0 wake-up gaps, worst gap 1.53 ms against 2.90 ms; the output locked throughout, 0 xruns, starves and overflows | Pass on both |
| One hour at 96 kHz, the same | After the fix: 0 xruns, 0 wake-up gaps, worst gap 6.45 ms against 10.67 ms; the output locked 7.2 s after it opened and throughout, 0 xruns, 0 starves, 0 overflows. Before it 1527 overflows | 0 xruns, 0 wake-up gaps, worst gap 1.44 ms against 2.67 ms; the output locked throughout, 0 xruns, starves and overflows | Pass on both after the fix |
| Cost without a further output against `main`, 48 kHz, 3 runs each | 78.0 against 77.1 µs per cycle in `f32`, +1.2%; 82.4 against 82.1 µs in `f64`, +0.4% | Not run | Pass: within or 0.2 µs above `main`'s spread |
| Cost with the loopback output | Audio thread +17.1 µs per cycle in `f32`, +11.1 µs in `f64`; the writer 17% of a core in `f32` and 28% in `f64`. One `f64` run counted 1 overflow | Writer 1.0% of a core at 44.1 kHz and 2.3% at 96 kHz, under the hours' load | Measured |
| `dist/test-plugin.sh` with the capture, the loopback and `ANALOG=1` | Not run | 33 of 34. The failure is the `mpd` case: `mpd` is not installed | Pass, except the case that needs `mpd` |

### The master's latency with a further output

Every output plays as late as the slowest needs, so the loopback's 48 ms set
the master's latency too. The daemon logs the delay it holds every output at,
from the mix to the card:

| Board, rate | The master alone, two periods | With the loopback output | The master's delay line |
|---|---|---|---|
| CM5, 44.1 kHz | 128 frames, 2.90 ms | 3288 frames, 74.6 ms | 3160 frames, 71.7 ms |
| CM5, 48 kHz | 128 frames, 2.67 ms | 3295 frames, 68.6 ms | 3167 frames, 66.0 ms |
| CM5, 96 kHz | 256 frames, 2.67 ms | 6527 frames, 68.0 ms | 6271 frames, 65.3 ms |
| Pi 3, 48 kHz | 512 frames, 10.67 ms | 3519 frames, 73.3 ms | 3007 frames, 62.6 ms |
| Pi 3, 44.1 kHz | 512 frames, 11.61 ms | 3519 frames, 79.8 ms | 3007 frames, 68.2 ms |
| Pi 3, 96 kHz | 1024 frames, 10.67 ms | 6975 frames, 72.7 ms | 5951 frames, 62.0 ms |

The delay is one master period for the feed, three loopback periods and the
resampler's delay, 160 to 260 frames. So with `snd-aloop` as a further output
the master is about 70 ms late, where it is 3 to 12 ms alone. A real second
card that runs the master's period would cost far less; none was at hand.

### The loopback's clock

`snd-aloop` has a `PCM Rate Shift 100000` control per cable end; the one of
device 1, subdevice 0 sets the rate of the cable from `hw:Loopback,0,0` to
`hw:Loopback,1,0`, both ends together. The control of device 0 changed nothing
measurable. `align-check` reads the rate of each capture against the system
clock from its timestamps (see below), so the loopback's rate, the DAC's rate
and their difference were measured in every run.

**A value above 100000 makes the loopback slower, not faster.** Against the
system clock, 100010 gave -99.4 to -100.5 ppm, 99970 gave +299.5 ppm, 100100
gave -998.2 to -999.4 ppm, 100500 gave -4973 and -4974 ppm and 99500
+5027 ppm, apart from the runs off their set rate below.
So the value sets the card's period, not its rate. The plan's "+100 ppm" for
100010 is a card 100 ppm slow.

At a shift of 0 the loopback runs on the system clock, within 0.2 ppm, and the
DAC on the CM5 runs 19.7 to 20.1 ppm slow against it, in every run. So the
loopback runs about 20 ppm fast against the master before any shift, and
**the drift the daemon has to follow is the loopback's own rate against the
system clock plus those 20 ppm**: -80 ppm at 100010, +320 ppm at 99970, -980 ppm
at 100100. The measured difference of the two captures agrees with that sum to
within 0.2 ppm in every run. So the measured drift is the set shift plus the
base drift, with the shift's sign turned.

In four of the 19 runs the loopback ran off its set rate, by 6 to 23 ppm: at
44.1 kHz at -91.8 ppm for 100010, +16.6 ppm for 100000 and +322.8 ppm for
99970, and at 96 kHz at +305.3 ppm for 99970. In the run at 100000 its
timestamps were 221 µs off a straight line, against 1.6 to 3.2 µs in every
other run at a shift of 0. A repeat of each setting ran true. With any shift
set, the loopback's timestamps were 31 to 313 µs off a straight line in every
run, and up to 20 to 90 µs off within any two seconds. So
`snd-aloop` with a shift is not a clean clock: its rate holds on average and
wanders around it. The figures below that depend on its timing carry that.

### Lock and drift

On the CM5, at 44.1 and 96 kHz, with the cable's shift at 100000, 100010,
99970 and 100100 and the analog input off: a fresh daemon per run, and
`align-check` playing a chirp a second through the plugin for 150 s, which
moved the master from its start-up rate of 48 kHz to the run's. The time to
lock is from the line that gives the loopback output its new rate to its
`locked` line. The daemon's ppm is from its counters at the stop, 150 s later,
against the loopback-to-DAC difference `align-check` measured. The delay error
is the spread of the offset between the two outputs (see Alignment) after the
`locked` line, and over the last 60 s of the run.

| Rate | Shift | Loopback against the system clock, ppm | Loopback against the DAC, ppm | Daemon, ppm | Daemon less measured | Lock after | Offset spread after lock, µs, frames | Last 60 s | Xruns, gaps, starves, overflows |
|---|---|---|---|---|---|---|---|---|---|
| 44.1 kHz | 100000 | +16.6 | +36.5 | +31.5 | -5.0 | 5.2 s | 459 µs, 20 | 356 µs, 16 | 0, 0, 0, 0 |
| 44.1 kHz | 100010 | -100.4 | -80.5 | -82.0 | -1.6 | 5.2 s | 610 µs, 27 | 462 µs, 20 | 0, 0, 0, 0 |
| 44.1 kHz | 99970 | +322.8 | +342.8 | +342.2 | -0.6 | 5.1 s | 699 µs, 31 | 250 µs, 11 | 0, 0, 0, 0 |
| 44.1 kHz | 100100 | -998.7 | -978.7 | -976.0 | +2.7 | 5.1 s | 1978 µs, 87 | 498 µs, 22 | 0, 0, 0, 0 |
| 48 kHz | 100000 | 0.0 | +19.9 | +19.9 | 0.0 | 5.0 s | 67 µs, 3 | 7 µs, 0 | 0, 0, 0, 0 |
| 96 kHz | 100000 | 0.0 | +20.0 | +20.0 | 0.0 | 6.9 s | 74 µs, 7 | 10 µs, 1 | 0, 0, 0, 0 |
| 96 kHz | 100010 | -99.4 | -79.5 | -81.1 | -1.7 | 5.1 s | 163 µs, 16 | 72 µs, 7 | 0, 0, 0, 0 |
| 96 kHz | 99970 | +305.3 | +325.2 | +313.6 | -11.6 | 5.1 s | 771 µs, 74 | 202 µs, 19 | 0, 0, 0, 0 |
| 96 kHz | 100100 | -998.2 | -978.3 | -980.4 | -2.1 | 5.1 s | 1784 µs, 171 | 287 µs, 28 | 0, 0, 0, 0 |

The table holds a second round of the eight runs, with one at 48 kHz added,
dated as Alignment describes. A first round, dated by one line through all of
a card's readings, gave the same lock times and counters, and ppm within the
same margins. In all 17 runs the output locked once, 5.0 to 6.9 s after the
rate change, and kept the lock to the end. The lock lines carried no ppm, since
the estimate needs 120 s. Every run counted 0 xruns and 0 wake-up gaps on both
outputs, and 0 starves and 0 overflows on the loopback. The daemon's ppm was
within 3 ppm of the measured drift in 14 of the 17 runs, and 3.4, 5.0 and
11.6 ppm off in the other three.

The delay error at a shift of 0, with a clean loopback clock, is a few frames:
the offset spread 63 to 74 µs from the lock to the end, and 7 to 12 µs, under
a frame, over the last minute. The 44.1 kHz run at 100000 in the table had one
of the irregular clocks above, and spread 459 and 356 µs; the first round's
run at the same setting had a clean one and spread 63 and 12 µs. With a shift
set the error is larger: up to 2.0 ms, 87 frames, after a shift
of 1000 ppm at 44.1 kHz, while the controller's integral took up the drift
over its 20 s, and 0.07 to 0.5 ms, 7 to 28 frames, in the last minute. That is
well inside the lock's window of a quarter of the loopback's period, 256 frames
at 44.1 kHz and 512 at 96 kHz, and less than `snd-aloop`'s own wander with a
shift set: the controller follows the card it has. The lock line comes at the
5 s minimum in nearly every run because the error is inside that window from
the start; it says the output plays, not that its delay has settled.

**At 5000 ppm the clamp is logged.** The first measurement, on 968e42d, did
not log it. With 100500, the loopback 4974 ppm slow and so about 4950 ppm
against the master, at 44.1 and 96 kHz for 120 s and at 48 kHz for 300 s, the
daemon never logged that the card runs more than 2000 ppm from the master; it
counted 36, 43 and 106 overflows, never locked and gave +0.000 ppm at the stop.
With 99500, 5047 ppm fast against the master, at 48 kHz for 300 s, it counted
131 starves, logged no clamp and logged `locked` four times, each followed by
`lost its lock` 0.6 to 12 s later. The cause was in the code: every overflow or
starve primed the path again, which restarted the smoothed error, the count at
the clamp and the estimate's settling, and kept the lock. The fix (b4fd1a6)
makes a priming end the lock until 5 s pass without another, keeps the count at
the clamp and the ppm estimate across it, and also calls a card clamped once
its estimate has stayed beyond the clamp for 10 s. The runs were repeated at
44.1 kHz for 180 s each, a fresh daemon with the rate set 99500 (card +5069 ppm
against the master) and 100500 (-4956 ppm):

| Shift | Clamp line after the rate change | Starves, overflows | `locked` | Daemon at the stop | Measured against the DAC | Master |
|---|---|---|---|---|---|---|
| 99500 | 30.6 s | 62, 0 | never | +5068.6 ppm, not locked | +5079.1 ppm | 0 xruns, 0 wake-up gaps, worst gap 1.49 ms |
| 100500 | 31.7 s | 0, 39 | never | -4955.6 ppm, not locked | -4954.1 ppm | 0 xruns, 0 wake-up gaps, worst gap 1.78 ms |

Both logged `its card runs more than 2000 ppm from the master, it plays on with
gaps` once. The daemon's ppm reads the clamp side, within 11 ppm of the
measurement. The starves and overflows, logged now with the time and the feed's
fill, came every 2 s for the fast card (the feed held about 1000 of 2329
frames) and every 5 s for the slow one (2275 to 2315 of 2329); the first ten
were logged one by one and then one line a minute, with the number since the
last. The output counted no skipped readings.

To check that normal lock was not broken, 99990 (the loopback +105.4 ppm
against the system clock and +126.0 ppm against the DAC) was run at 44.1 kHz
for 180 s: `locked` 5.1 s after the rate change, 0 xruns, 0 wake-up gaps,
0 starves, 0 overflows on both outputs, and the daemon's ppm +113.2, 12.8 ppm
below the measured difference. That is outside the 5 ppm of most runs above,
in the range of the three that were off by 3.4 to 11.6 ppm. The loopback's
timestamps were 233 µs off a straight line in the run, so its rate over the
180 s is not firmer than that; the offset between the outputs spread 632 µs,
28 frames, after the lock.

The controller's skipped readings are now counted in the stop line; none were
skipped in these runs.

### The tone through the further output

The resampled output cannot match the stream sample for sample, so `bit-check
--tone` plays a 997 Hz tone at -6 dBFS peak, as the resampler's own test does,
and measures each captured side in windows of 0.1 s from a second after the
tone starts: what is left once a steady sine, its frequency fitted per window,
is taken off, and once a sine is taken off whose amplitude and phase may move
as a cubic over the window. The second also takes off the slow drift of a
resampler that follows a clock; the first does not. The test asks for less
than -100 dBFS in `f32` and -110 dBFS in `f64`. The CM5, analog input off, the
cable's shift at 0, S32 at the client, 10 s per run, the daemon's volume at
0 dB:

| Config | Format | Rate | Left: steady, worst window | Left: moving, worst window | Right |
|---|---|---|---|---|---|
| Default | `f32` | 48 kHz | -117.5, -109.2 dBFS | -134.7, -128.2 dBFS | the same as the left |
| Default | `f64` | 48 kHz | -114.0, -98.2 dBFS | -131.9, -117.4 dBFS | the same as the left |
| Swap | `f32` | 48 kHz | -115.3, -101.4 dBFS | -132.0, -117.0 dBFS | exactly zero |
| Swap | `f64` | 48 kHz | -118.8, -108.6 dBFS | -134.1, -122.5 dBFS | exactly zero |
| Default | `f32` | 44.1 kHz | -106.5, -98.6 dBFS | -121.1, -117.4 dBFS | the same as the left |
| Default, again | `f32` | 44.1 kHz | **-51.5, -41.8 dBFS** | -114.9, -100.6 dBFS | the same as the left |
| Default | `f32` | 96 kHz | -108.3, -101.4 dBFS | -128.5, -113.6 dBFS | the same as the left |
| Default, again | `f32` | 96 kHz | -109.0, -103.1 dBFS | -133.6, -121.8 dBFS | the same as the left |

All pass against the moving sine, by 15 to 35 dB. Against the steady sine they
pass on the whole run, but single windows reach -98 dBFS, and the 44.1 kHz run
gave -51.5 dBFS: that run's loopback clock was one of the irregular ones above,
and a repeat at 44.1 kHz gave -106.5 dBFS. So the floor is the clock the
resampler follows, not its arithmetic: `f64` is no better than `f32` here.

The swap is a `[[output.matrix]]` of the further output only, `players.left`
to 2 and `players.right` to 1, with the master's matrix left at its default.
With the tone on the left only, the loopback carried it on channel 2 and
exactly zero on channel 1, and `arecord` on the DAC through the cable at the
same time read it on the left at -6.1 dBFS, with -121.1 and -122.2 dBFS at
997 Hz on the right. So the swap applies to that output only. Checked the other
way round, the swapped output as `1=L,2=R`, `bit-check` found no tone on the
left and failed, as it should.

`dist/test-plugin.sh` now has the same check as a case,
`further_output_follows_its_own_matrix`, run when `AAS_TEST_LOOPBACK` names
the loopback's capture end. On the CM5, with the daemon installed as its unit
for the run and removed after, `AAS_TEST_CAPTURE=plughw:sndrpihifiberry
AAS_TEST_LOOPBACK=hw:Loopback,1,0 ANALOG=1` passed 33 of 34 cases; the one
failure is `mpd_mixer_works`, because `mpd` is not installed. The new case read
the tone at -20.0 dBFS on the loopback's channel 2 with nothing on channel 1,
and at -20.1 dBFS on the left through the cable with the right at -72 dBFS.

### Alignment

`align-check` plays a chirp of 2048 frames through the plugin once a second
and captures the ADC, through the cable from the DAC, and the loopback's other
end at once, with the cards' timestamps on `CLOCK_MONOTONIC`. It finds the
chirp in each capture by correlation, to a fraction of a frame, and dates it by
the card's timestamps within a second either side, at their lower edge, which
dates a frame by the least delay with which its card's pointer was read. The
offset is the chirp's time at the ADC less its time at the loopback.

What the offset should be: the daemon holds the master's delay line so that a
frame leaves the DAC's buffer when it leaves the loopback's. From there it
reaches the ADC's buffer after the DAC's and the ADC's own delays, and the
loopback's capture at once. So two outputs in time read the converters'
delay through the cable, which stage 1 measured on the CM5 with the two
streams linked: 50 frames at 44.1 and 48 kHz, 52 at 96 kHz. The capture side's
wait for its reader, 0.69 of a period in [stage 3a](#latency-of-the-analog-path),
is not in it: a timestamp dates a frame when the card took it, not when it
was read. The method cannot tell the DAC's delay from the ADC's, or a pointer
that runs ahead of its card from a daemon that plays late; it gives the sum.

The CM5, the cable's shift at 0, over the last minute of a 150 s run:

| Rate | Offset | Expected, the converters | Measured less expected | Spread |
|---|---|---|---|---|
| 44.1 kHz | 1453 µs, 64.1 frames | 1134 µs, 50 frames | +319 µs, +14.1 frames | 12 µs |
| 48 kHz | 1359 µs, 65.2 frames | 1042 µs, 50 frames | +317 µs, +15.2 frames | 7 µs |
| 96 kHz | 688 µs, 66.0 frames | 542 µs, 52 frames | +146 µs, +14.0 frames | 10 µs |

**The master plays 14 to 15 frames after the loopback,** at every rate, so
0.32 ms at 44.1 and 48 kHz and 0.15 ms at 96 kHz. It is the same in frames at
each rate, which points at a count of frames somewhere in the path: the
daemon's reading of either card's delay, the DAC's FIFO, or `snd-aloop`'s
pointer. Which, the method cannot say. It is far below what the lock allows,
and stays put: under a frame over a minute. The 44.1 kHz figure is from the
first run, whose loopback clock was clean; the repeat, with the irregular
clock, gave 1445 µs.

### Hour runs with a further output

The runs of [part 1](#hour-runs-with-a-swap-matrix), made the same way: a
fresh daemon per run on the DAC with the analog input on at -20 dB, the stage 1
full load, and the same music from RAM looped into `aplay -t raw` for an hour,
at 44.1 kHz and at 96 kHz. Added is the loopback output, with the cable's
shift at 100010, so the loopback ran about 100 ppm slow against the system
clock. The analog input reaches the loopback through its default matrix. The
logs were kept in RAM and copied off every two or three minutes, and on the
CM5 the kernel log was streamed off the board.

On the CM5, 64 frames at 44.1 kHz and 128 at 96 kHz, with the heatsink. It sat
at 53 to 86 °C and ran between 1.5 and 2.4 GHz.

| Run | Master xruns, wake-up gaps | Master's worst gap / buffer | Worst cycle | Padded, dropped | Loopback output | Its worst gap | Its ppm |
|---|---|---|---|---|---|---|---|
| 44.1 kHz | 0, 0 | 1.53 / 2.90 ms | 0.09 ms | 128, 0 | locked 5.2 s after it opened, never lost; 0 xruns, 0 wake-up gaps, 0 starves, 0 overflows | 31.9 ms | -87.8 |
| 96 kHz | 0, 0 | 1.44 / 2.67 ms | 0.12 ms | 192, 0 | locked 5.1 s after it opened, never lost; 0 xruns, 0 wake-up gaps, 0 starves, 0 overflows | 28.1 ms | -89.4 |

Both pass. `aplay` counted no underruns, the kernel migrated no pages, and the
kernel log has nothing from the runs; the board did not hang. The master's
figures are within 0.04 ms of part 1's hours without the further output. The
writer thread used 36.0 s of CPU in the 44.1 kHz hour and 81.4 s in the 96 kHz
hour, 1.0% and 2.3% of a core; the audio thread 23.1 and 27.5 µs per cycle.
The daemon's ppm, -88 and -89, is 8 to 9 ppm further from the master than the
-80 measured for this shift on an idle board in Lock and drift; the loopback's
rate was not measured under load, so whether `snd-aloop`'s timer runs slower
under load or the estimate is off cannot be told.

On the Pi 3, 256 frames at 44.1 kHz and 512 at 96 kHz, without a heatsink. It
sat at 60 to 88 °C and 496 to 603 MHz, throttled, as in part 1.

| Run | Master xruns, wake-up gaps | Master's worst gap / buffer | Worst cycle | Padded, dropped | Loopback output | Its worst gap | Its ppm |
|---|---|---|---|---|---|---|---|
| 44.1 kHz | 0, 0 | 6.99 / 11.61 ms | 0.69 ms | 256, 0 | locked 5.3 s after it opened, never lost; 0 xruns, 0 wake-up gaps, 0 starves, 0 overflows | 28.2 ms | -102.8 |
| 96 kHz | 0, 0 | 6.44 / 10.67 ms | 1.16 ms | 256, 0 | locked 6.8 s after it opened, never lost; 0 xruns, 0 wake-up gaps, 0 starves, **1527 overflows** | 28.2 ms | +0.000 |
| 96 kHz, after the fix | 0, 0 | 6.45 / 10.67 ms | 0.90 ms | 256, 98 | locked 7.2 s after it opened, never lost; 0 xruns, 0 wake-up gaps, 0 starves, 0 overflows, 0 skipped readings | 28.2 ms | -103.7 |

The master passes both hours, within 0.1 ms of part 1's worst gaps; the worst
cycles, 0.69 and 1.16 ms, are above part 1's 0.65 and 0.76 ms. `aplay` counted
no underruns. The kernel migrated 131040 pages in the 44.1 kHz hour and
153525 in the 96 kHz hour.

**The first 96 kHz hour on the Pi 3 failed for the loopback output, on
968e42d: 1527 overflows,** about one every 2.4 s. Each dropped a master period
at the feed and primed the loopback's path again, a gap in what it played, and
the ppm of +0.000 at the stop said the estimate never got past its settling.
The writer thread used 1603 s of CPU, 44.5% of a core, 9.5 ms per loopback
period of 21.3 ms, on a board throttled to about 550 MHz; its worst wake-up gap
was 28.2 ms, under its buffer of three loopback periods. The feed held only
what the delay target left plus two master periods, and a writer that wakes
late and then spends 9.5 ms resampling lets the master push more than that
before it takes. The fix (b4fd1a6) sizes the feed at the target's share plus
two loopback periods plus two master periods, 5888 frames here against 4352,
with no change to the delay.

The hour was run again on b4fd1a6 exactly as before: full load, analog input
on, the loopback at `latency_ms = 48` and shift 100010. **It counted 0
overflows and 0 starves, the output locked 7.2 s after it opened and stayed
locked, and the master passed:** 0 xruns, 0 wake-up gaps, worst gap 6.45 ms
against 10.67 ms. The loopback's worst gap was 28.2 ms and its ppm -103.7, so
the estimate ran the whole hour, 3.7 ppm from the -100 shift, the Pi 3's DAC
being near the system clock. The writer used 1597 s of CPU, 44.2% of a core,
as before the fix, so the feed's size was the cause and the cost is unchanged.
The audio thread took 386 µs per cycle, 0.90 ms at worst, and the board sat at
86 to 87 °C and 549 MHz, throttled at its end. The kernel migrated 23378
pages. The stop line counts 256 frames padded and 98 dropped; the first hour
dropped none. The analog input went quiet 8 s before the stop, when the player
ended, and the counters do not say when the 98 were dropped.

### Processing cost on the Pi 3

The method of [part 1](#processing-cost-on-the-pi-3): the DAC+ ADC at its
defaults, 256 frames at 48 kHz, `bit_perfect = "off"`, one `aplay` playing the
music resampled to 48 kHz from RAM for 10 minutes per run, and the CPU time of
each thread from `/proc/<pid>/task/<tid>/schedstat` just before the stop.
Three daemons, alternated, three runs each per format, `f32` first: `main` at
c95432e, this branch without `[[output]]`, and this branch with the loopback
output at a shift of 0. `snd-aloop` was loaded for all runs. The board was
otherwise idle at 54 to 69 °C, at 600 MHz in most readings and 700 MHz to
1.2 GHz in 138 of 362, as the `ondemand` governor chose.

The audio thread, per cycle:

| Daemon | Format | Mean per cycle, three runs | Mean | Share of a period | Worst cycle, three runs |
|---|---|---|---|---|---|
| `main` | `f32` | 77.2, 77.9, 76.3 µs | 77.1 µs | 1.4% | 0.164, 0.198, 0.116 ms |
| no further output | `f32` | 78.1, 78.1, 77.7 µs | 78.0 µs | 1.5% | 0.156, 0.164, 0.099 ms |
| the loopback output | `f32` | 95.6, 95.3, 94.4 µs | 95.1 µs | 1.8% | 0.186, 0.248, 0.195 ms |
| `main` | `f64` | 82.5, 82.2, 81.6 µs | 82.1 µs | 1.5% | 0.164, 0.176, 0.318 ms |
| no further output | `f64` | 83.5, 82.4, 81.2 µs | 82.4 µs | 1.5% | 0.173, 0.098, 0.170 ms |
| the loopback output | `f64` | 94.0, 93.2, 93.4 µs | 93.5 µs | 1.8% | 0.364, 0.197, 0.255 ms |

Without a further output this branch costs what `main` costs: 0.9 µs, 1.2%,
more in `f32`, 0.2 µs above `main`'s highest run, and 0.3 µs more in `f64`,
inside `main`'s runs. The extra 2.6 to 4.6% a workstation benchmark had shown
in `f32` does not appear on the Pi 3. With the loopback output the audio thread
takes 17.1 µs more per cycle in `f32`, 22%, and 11.1 µs more in `f64`, 13%:
mixing a second output, the push into its feed and the master's delay line.
That is 0.2 to 0.3% of a period.

The writer thread of the further output, `aas-out-2`, per run of 600 s:

| Format | CPU time, three runs | Share of one core | Per writer cycle of 1024 frames, 21.3 ms |
|---|---|---|---|
| `f32` | 102.9, 102.8, 101.1 s | 16.9 to 17.2% | 3.58 to 3.64 ms |
| `f64` | 166.8, 165.2, 165.0 s | 27.5 to 27.8% | 5.84 to 5.90 ms |

That is the adaptive resampler at `high`, 128 taps with cubic interpolation
between phases, on two channels, and it is far more than the audio thread's
own work: on the Pi 3 one further output takes a sixth of a core in `f32` and
over a quarter in `f64`. The writer is one thread, so it fits; a board with
three further outputs at 48 kHz would want half to most of a core for them.

Every run counted 0 xruns and 0 wake-up gaps on the master, with a worst gap
of 5.46 to 5.87 ms against a 10.67 ms buffer, and the loopback output locked,
at -6.0 to +0.8 ppm, with 0 xruns, 0 wake-up gaps and 0 starves. **One `f64`
run counted 1 overflow** on the loopback output, on an idle board, while the
other five counted none. The counters do not say when it came, so whether it
was before the lock or after cannot be told.

A change of the delay makes the audio thread read the whole delay line once
to see whether it holds sound, which in silence it reads to the end. To see
what that costs, the daemon played 3 s of silence at 44.1, 96, 192 and 48 kHz,
three rounds, so twelve rate changes, each with a new delay of up to 13887
frames at 192 kHz, twice with the loopback output and twice without. The
worst cycle was 0.164 and 0.191 ms with it and 0.117 and 0.187 ms without, so
the read is not visible against the rest of a cycle. The loopback output
counted 1 and 4 overflows over the twelve rate changes, and did not lock,
since no rate lasted the 5 s a lock takes.

### What these figures do not show

- No board has a second real card, so the further output is `snd-aloop`, a
  card with a software clock that wanders when shifted. A real card with a
  crystal of its own may lock tighter, and may run the master's period, which
  would bring the master's latency back to a few ms.
- The offset between the outputs holds the DAC's and ADC's delays together,
  and anything either card's pointer adds. It cannot place the 14 to 15
  frames.
- Lock, alignment and the tone were measured on the CM5 only, and with the
  analog input off.
- The clamp was measured again after the fix at 44.1 kHz only, and the hour
  once; the cost run's overflow and those of the rate changes were not
  repeated, so the feed's new size is not checked against them.
- The loopback's rate was measured on an idle board only; under the hours'
  load only the daemon's own estimate of it is known.
- The cost with a further output was measured at 48 kHz only; the writer's
  share at 96 kHz on the Pi 3 is known only from the hour under load, 44.5% of
  a core at about 550 MHz.
