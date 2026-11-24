# Ideas

Things that could be done, with what is known about them. Not planned; `TODO.md`
holds what is.

## AES67 input without the ring: the engine reads the jitter buffer

The AES67 input runs in its own thread and writes into a client ring, which the
engine drains a period at a time. The ring therefore holds about one engine
period plus the thread's 1 ms step, on top of the card's buffer, the resampler's
filter and the network margin. `GET /aes67` shows it as `input.delay.ring_ms`.

Measured parts of the input's delay, PTP mode:

| Board, engine period | Card | Ring | Filter | Network | Latency set |
|---|---|---|---|---|---|
| Pi 3, 256 frames | 10.7 ms | 6.5 ms | 1.3 ms | 1.5 ms | 20 ms |
| Pi 3, 128 frames | 5.3 ms | 4.5 ms | 1.3 ms | 0.9 ms | 12 ms |

If the engine pulled the stream's frames from the jitter buffer itself, as an
in-engine source, and resampled them in its own cycle, the ring's share would
mostly go: the Pi 3 input could hold about 8 ms at 128-frame periods, a CM5
about 4 ms.

What it costs: the receiver's resampling and the jitter buffer's reads move into
the real-time audio thread, so their worst case counts against the period. The
jitter buffer is filled by the network thread and read by the engine, so it needs
a lock-free handover. The delay controller has to read the engine's position
directly instead of the ring's fill. Weigh that against the ~4 ms gained on a
Pi 3 and ~1-2 ms on a CM5.

First step: time the input's resampling per period on the Pi 3 (it is the
`high` quality sinc filter, see `doc/measurements.md`) to see what share of a
128-frame period it would take inside the engine.
