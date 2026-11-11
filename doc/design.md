# aas design

Status: approved, 2026-11-05.

Items marked *(call)* are defaults chosen during design that the owner has not
explicitly confirmed. Everything else was stated or chosen by the owner.

## Intent

`aas` replaces PipeWire and WirePlumber on HiFiBerry OS. The reason is not
missing features. That stack is hard to understand and hard to configure, and
its behaviour does not follow from a few simple rules.

`aas` is one daemon whose behaviour can be predicted from the rules in this
document. It has no session manager and no external routing. Anyone reading
its config file should be able to say what it will do.

## Goals

- ALSA clients connect to `aas` and need no changes.
- ALSA sound cards are the outputs.
- Very low latency: below 4 ms by default, settable in the config.
- Bit-perfect playback when nothing needs to alter the audio.
- Analog input from the card's ADC.
- A plugin mechanism for processing audio: routing, mixing, filtering and so on.
- Settings can be changed at runtime through an API.
- Bluetooth audio, required before the first shipped version.

## Non-goals for the first version

These come at a later stage. The design must not block them, but nothing is
built for them now.

- Network audio, including AES67
- USB audio gadget
- Porting `speakereq` to a native plugin

The first version is a real replacement on shipped systems, not an opt-in. So it
includes Debian packaging and the changes to the `hifiberry-os` packages that
depend on PipeWire today: `hifiberryos`, `hifiberry-pipewire-configs`,
`analoginput` and the volume API users.

AES67 and speaker EQ are accepted as lost on update until their later stages land.

The switch needs a way back to PipeWire on a device. Devices using speaker EQ or
AES67 are held back from the switch and do not lose them silently *(call)*.

## Anti-goals

The project has failed, even if it works, if any of these become true:

- Understanding its behaviour requires knowing about a graph, a session manager
  or policy scripts.
- Routing changes at runtime because something other than the config or an
  explicit API call decided it.
- The config needs tuning to get normal playback working.

## Constraints

- Written in Rust. `rust-version` is declared in `Cargo.toml`; no nightly.
- The target is Debian trixie on a 64-bit Raspberry Pi. The first shipped version
  supports Pi 3, 4 and 5 and all HiFiBerry cards.
- The 4 ms target holds on Pi 5, measured on a Compute Module 5 with the daemon
  running, and is still to be confirmed on the Pi 4. More latency is fine on a
  Pi 3, so the default latency depends on the board. See [Latency](#latency) and
  `doc/measurements.md`.
- The kernel on the test boards is 6.18 with standard `PREEMPT`, not
  `PREEMPT_RT`. Measurement shows the defaults hold on that kernel under full load,
  so `PREEMPT_RT` is not needed.
- `squeezelite` plays to the ALSA `default` device, but its volume control goes
  straight to the card's mixer (`-V <control> -O hw:N`), not through `default`.
- MPD uses `type "alsa"` with `mixer_type "hardware"` on `default`, so `aas` must
  provide an ALSA control device as well as a PCM device.
- On Digi cards, opening capture slaves the card clock to S/PDIF in and breaks
  playback. So `aas` needs a table of card capabilities, and it opens capture only
  on cards marked as having an ADC *(call)*.
- There is no minimum cooling requirement. A Compute Module 5 without a heatsink
  throttled to 1 GHz under full load and still had no xruns.
- `aas` detects the card itself, so normal use needs no card in the config. The
  config can override it *(call)*.

## Architecture

Three parts *(call)*:

1. **The daemon** owns the sound card and runs the audio engine.
2. **An ALSA PCM and control plugin** is installed as the ALSA `default` device.
   Clients load it through libasound, and it passes audio to the daemon. It also
   provides a capture device for the analog input.
3. **Native plugins** are shared libraries loaded by the daemon through a C ABI
   (see [Plugins](#plugins)).

The handover between the ALSA plugin and the daemon uses shared memory. A socket
carries setup and teardown only. A socket round trip per period would use too
much of the latency budget *(call)*.

The audio thread runs with real-time priority. It does not allocate memory,
take locks or make blocking calls during playback *(call)*.

The daemon runs as a system service with real-time rights, not in a user
session. Access is limited to the `audio` group *(call)*.

## Clients

A client can always connect, in every mode. No client is ever refused.

Clients playing at the same time are mixed together.

A client counts as **active** while its ALSA stream is running *(call)*. A client
that is open but stopped, prepared or paused does not count, so a paused player
cannot hold the rate. A running client that is writing digital silence does
count. When a paused player resumes after the rate has changed, it is resampled.

Clients are identified by process name, so a per-input volume survives a
reconnect *(call)*.

`aas` accepts mono, stereo and float clients, and converts them to the output's
format and channel count *(call)*.

The plugin converts every client to S32 stereo *(call)*. Conversion runs in the
client's process, in the plugin's `transfer` callback, so it costs the audio
thread nothing and the shared ring has one format. S16 and S24 are shifted left
exactly. Mono is copied to both channels.

A client's buffer is its shared ring. The plugin offers at least 8192 bytes, so
the usual small requests round up to something the daemon takes. The daemon
refuses, at `hw_params`, a buffer too small to keep it fed: what one of its periods
takes from the ring, with room for the resampler and for DoP detection, and one
period more to refill it in. The largest ring is 2^20 frames. "No client is ever refused"
is about the number of clients, not the size of a ring.

The daemon wakes a waiting client through an eventfd. The plugin stores in the
ring's header the read position it waits for, taken from the player's `avail_min`.
The daemon wakes it once when that position is reached, or when the stream
underruns *(call)*. A player that waits for half a buffer of room is woken once
per half buffer, not once per daemon period. A client that stores no position is
woken after every period.

A prepare keeps the stream. The plugin asks the daemon to reset it. The audio
thread stops the stream, empties its ring and its resampler, and the daemon
answers when that is done. The ring and the poll descriptors stay the same.

A player that does not use ALSA `default` today is reconfigured to use it *(call)*.
A direct `hw:` open cannot work while the daemon owns the card, and no PulseAudio
endpoint is planned.

## Output

`aas` can drive multiple outputs. Which cards are outputs is set in the config.

Separate cards have separate clocks and drift apart. One output is the clock
master. Every other output is resampled adaptively to follow it, so bit-perfect
applies only on the master output *(call)*. Each output has its own volume, idle
behaviour and plugin chain *(call)*.

### Mixing matrix

A mixing matrix decides which sources reach which outputs, and at what gain. The
standard case is every output playing the same audio, and that is the default
when the config sets no matrix.

Details *(call)*:

- Each entry is a gain from one source channel to one output channel, so the same
  matrix can also swap channels or sum to mono.
- The matrix is set in the config and can be changed through the API. Nothing else
  changes it.
- Each output has its own mix, followed by its own plugin chain.
- An entry other than 0 dB or off on the path to the master output stops automatic
  bit-perfect.
- The sample rate rules are applied to the master output. All outputs run at its
  rate, so sources routed only to another output still follow those rules.

The card runs at S32 whenever it supports it, on both audio paths. S16 and S24
samples fit into S32 exactly, so the values reaching the DAC are unchanged. This
means going in and out of bit-perfect never requires reopening the card *(call)*.

When idle, an output does one of two things, chosen in the config:

- **Send zeros:** the output keeps running and sends digital silence.
- **Go off:** the output stops after a configurable delay, for example 60
  seconds. Until the delay has passed it sends zeros, so a client starting within
  that time needs no restart of the output.

"Go off" is the default, with a 60 second delay *(call)*. The output counts as
idle when no source is active. While the analog input is enabled the output never
goes off, and `idle = "off"` is ignored with one line in the log at startup
*(call)*. The engine's cycle is paced by the playback card, and input detection
has to keep measuring.

## Sample rate rules

- With one client, the output uses that client's rate.
- With several clients at the same rate, the output uses that rate.
- With clients at different rates, the first one wins and the others are
  resampled to its rate.

Further rules *(call)*:

- If several clients were active and one remains, the output switches to the
  remaining client's rate after a short settle time, with a brief fade. So a
  handover between two players does not leave the new one resampled for the whole
  session.
- More generally, when a client stops and no remaining client is at the output
  rate, the output switches after the settle time to the rate of the oldest
  remaining client. A client that has left never holds the rate. A client at the
  output rate arriving during the settle time cancels the switch.
- The settle time is 500 ms.
- If the card does not support a client's rate, the output uses the nearest
  supported rate and the client is resampled. In a tie, the lower rate wins.
- A rate at which a plugin chain cannot be built counts as a rate the card does
  not support (see Plugins). A DoP stream needs only the analog chain, so only
  that one can refuse it a rate.
- A source that reports no signal (see Analog input) does not count as active.
- The analog input takes part in these rules at the rate the output runs at, so
  it never sets a rate. If it has signal first it counts as the first client,
  and a later player at another rate is resampled. When the output rate changes,
  for DoP included, the input follows it.
- After a DoP stream ends with only the analog input left, the card keeps the
  DoP rate. The reopen for DoP closes the capture, which is silent for 1 s once
  it opens again, so the input is quiet within the settle time and DoP is left
  as the only client. When its signal is back, the input runs at that rate.
- A reopen of the output closes the card's capture and opens it again, so the
  analog input goes quiet for a moment. If its signal is not back within the
  settle time, it counts as a client that stopped. A detector built for the new
  rate has no hold yet, so in a quiet passage this can happen at the moment the
  rate changes.
- The card keeps its current rate when idle, a rate a DoP stream set included.
  It does not return to a default.
  The configured default rate applies only from startup until the first client
  sets a rate. So the ADC never forces a rate change: it starts at whatever rate
  the card is at, and no signal is lost to a card restart.

## Audio processing

There is one processing path *(call)*. Audio is converted to the processing
format, resampled if needed, mixed, run through the plugin chains, then written
to the card as S32. The analog input's chain runs before the mix.
Audio is never dithered, and float conversion scales by an exact power of two.

### Processing format

The processing format is set in the config. It applies to the whole daemon and
takes effect at startup *(call)*.

| Format | Holds S32 exactly | Good for |
|---|---|---|
| `i32` | Yes | Simple mixing and volume |
| `f32` | No, S16 and S24 only | Everything, the default |
| `f64` | Yes | Anyone who wants the extra precision |

Losing bits 25 to 32 in `f32` does not matter for sound. Real audio data has at most
24 bits, and converters resolve less: the PCM5122 is rated at 112 dB and the PCM186x
ADCs at 110 dB, against 144 dB for 24 bits. In a numeric check, `f32` error from an
8-source mix with volume was -164 dBFS, and from 20 gain stages -162 dBFS. Both are
below the rounding noise of 24-bit audio itself, about -149 dBFS. Only bit-perfect
needs every bit, and it does not use the processing format.

`f32` is the default on every board. `f64` stays fully supported. Mixing costs about
1% of a period on a Pi 3, the slowest supported board, in either format. Resampling
costs 1.35 to 1.7 times as much in `f64` as in `f32`.

`i32` is 32-bit signed integer, the same format the card uses:

- Mixing sums in a 64-bit accumulator and saturates once at the end, so a sum that
  briefly goes over full scale in the middle of the mix does not clip *(call)*.
- Volume multiplies in 64 bits and rounds once per stage *(call)*. An exact half
  rounds away from zero, as in the float formats, so the negative of a signal
  gives the negative of the result. The rounding error is near -194 dBFS, far
  below the noise of any DAC.
- It has no headroom between stages, unlike float. A stage that goes over full
  scale clips there.
- The resampler only works on float, so a resampled client is resampled in `f32`
  and converted back *(call)*. That client is never bit-perfect anyway.

On the Pi 3, mixing in `i32` costs about 3.5 times as much as in `f32` and 1.8 times
as much as in `f64`. On the CM5 it costs about 7 times `f32` and 3.3 times `f64`.
That is still about 1% of a period or less at 48 kHz with 8 sources on both boards,
but `i32` saves no CPU.

### Resampling

Resampling has two quality settings *(call)*. `high` is the default on every board.
`medium` can be set in the config.

The resampler is a windowed sinc filter. Between two fixed rates it only ever
needs a fixed set of filter phases: the output rate divided by the greatest common
divisor of the two rates. That is 160 phases from 44.1 to 48 kHz and 4 from 48 to
192 kHz. The filter table holds exactly those phases, so no output sample is
interpolated between two of them *(call)*. This costs 1.6 to 4.8 times less than
interpolating, with the same delay and the same purity.

On the Pi 3, one stream at `high` to 192 kHz uses about 10% of a period in `f32`
and 17% in `f64`. The worst single cycle at any period was 27% and 33%. So `high`
fits on a Pi 3 in either format, and so do several resampled streams. `medium`
costs about two thirds of `high` and is no longer needed to make a Pi 3 fit. The
Pi 4 is still to be measured.

### Volume and clipping

Input volumes are calculated on this path. So is the output volume on cards
without a hardware volume (see Volume). The analog input's volume applies after
its chain, as it is mixed. It is set with `volume_db`, from -60 to 0 dB, 0 by
default *(call)*.

Volume changes and input detection opening or closing use short fades, so they do
not click *(call)*. A fade lasts 10 ms *(call)*. A gain change applies at once
when nothing is audible, since there is nothing to fade.

Mixed clients that sum above full scale clip at the S32 limits unless a limiter
plugin is in the chain. There is no built-in limiter.

### Bit-perfect

Bit-perfect means nothing alters the samples. When no volume, plugin or resampling
applies, samples are copied to the card as S32 without entering the processing
format. This is the same for all three formats, and `f32` needs it because it
cannot hold every S32 value *(call)*. Chains whose plugins are all neutral are
skipped on this path (see Plugins). There are two ways to get it:

- **Automatic:** applies when one source is active, all volumes that `aas`
  calculates are at 0 dB and every plugin is neutral. A hardware output volume
  does not touch the samples, so it does not block bit-perfect *(call)*.
  Automatic is the default *(call)*.
- **Forced:** a setting. Volumes that `aas` calculates have no effect and plugins
  are bypassed *(call for plugins)*. A hardware output volume keeps working
  *(call)*.

A setting can also turn automatic bit-perfect off for testing *(call)*.

With forced bit-perfect, a second client is mixed in. The output is not
bit-perfect while they overlap. Volumes that `aas` calculates still have no
effect, plugins stay bypassed *(call)*, and bit-perfect resumes once one client
is left.

Forced bit-perfect does not bypass input detection, because analog input must
never pass ungated *(call)*. This is the one exception to plugins being
bypassed. The analog chain still runs, so that the detector can measure and a
recorder gets the input after its plugins (see Capture device), but its output
is not played: the captured frames are played as they are *(call)*. Past a
chain that alters them, the engine fades them in and out itself, over 10 ms from
the start of the period in which the detector opens or has closed, so a few
frames before the detector opened are heard, rising from silence. Below
the threshold the analog input is not a source, so it does not stop bit-perfect
in either mode. Above it the ADC is mixed in like a second client, which follows
the rule above.

In automatic mode, when the analog input is the only source and its chain alters
nothing, its captured frames are copied to the card as they are, so the copy
path stays bit-exact *(call)*. While input detection fades in or out, the
detector's output is mixed instead, so nothing it silences is heard.

A resampled client is never bit-perfect.

When the output is reconfigured, for example for a rate change, the stream being
cut is faded out even in forced bit-perfect. The audio is being interrupted
anyway, and a fade stops the click. DoP is exempt, as below.

### DoP

DSD over PCM is supported. It looks like PCM, so `aas` detects its marker bytes.
A DoP stream must reach the card unaltered, because any volume, plugin or mixing
turns it into loud noise. While a DoP stream plays:

- volumes that `aas` calculates and all plugins are bypassed;
- the analog input is still measured and can have signal, but it is not played
  *(call)*;
- the output runs at the stream's rate;
- it has exclusive use of the output. Other clients stay connected, but their
  audio is discarded until the DoP stream stops *(call)*. This is the one exception
  to mixing, because mixing would produce noise.

How a DoP stream ends and what fills its gaps:

- A DoP stream that underruns keeps the output for 100 ms and sends DoP silence:
  the markers are kept and the payload is `0x69`. Then it releases the output
  and counts as stopped, so the rate rules apply again as if it had. Its next
  frames start it anew, behind any DoP stream that started meanwhile.
  Stopping or disconnecting releases it at once. So a stalled DoP client costs
  other clients at most 100 ms of audio.
- A short DoP period is padded with DoP silence, not zeros. Zeros drop the
  markers, and the DAC would leave DoP mode with a pop.
- At the end of a drain, a tail too short to judge that carries markers is
  replaced by DoP silence rather than mixed.
- A DoP stream at a rate the card does not support is discarded, not resampled.
  Resampling would turn it into noise, so that client plays silence.
- Detection needs a run of 16 frames. A PCM frame that looks like a marker is
  held back until the run decides, and each hold is counted. In loud PCM from a
  client that keeps less than a period and 16 frames buffered, this can cause a
  one frame gap.

## Volume

Output volume has two modes:

- **Card with hardware volume in ALSA:** `aas` uses it.
- **Card without one:** `aas` calculates the volume itself.

`aas` exposes one ALSA volume control mapped to the output volume in both modes,
so clients such as MPD see the same thing on every card *(call)*. `squeezelite` is
reconfigured to use it, instead of writing to the card's mixer directly *(call)*.
The mode is detected from the card, and the config can override it and name the
control *(call)*.

The control is called `Master` *(call)*, which `mpd.conf.default` already uses
on device `default`. Its range is 0 to 100, linear in dB from -60 dB to 0 dB,
and 0 mutes *(call)*. A request above 100 is an error, not clamped. Every client
holding the control is told when the volume changes, so a second mixer sees a
change made by the first.

A card's control can stop above -60 dB. Below its lowest level `aas` sets the
control to that level and calculates the rest itself, so each step is a step on
every card. The output is then not bit-perfect, at a level where that is not
heard.

In forced bit-perfect mode `aas` never touches the samples, so the volume stops
at the control's lowest level. On a card whose
hardware control has neither a mute switch nor a mute step, 0 then plays at the
control's lowest level, not silence.

## Latency

The 4 ms budget covers what `aas` adds, from the plugin receiving audio to the
audio leaving the card *(call)*. A player's own ALSA buffer is outside it. At
48 kHz, 4 ms is 192 frames.

One rule sets the period from a latency target, in place of a table per board
*(call)*. The period is the largest power of two where 2 periods fit in the
target, with a minimum of 32 frames. The default target is 4 ms on Pi 5 class
boards and 12 ms on everything else *(call)*. The Pi 4 uses 12 ms until it is
measured *(call)*. That is safe, not final. The target is set with `latency_ms`
in the config.

The rule gives exactly the periods the measurements chose, with 2 periods
buffered:

| Rate | 4 ms, Pi 5 | 12 ms, Pi 3 |
|---|---|---|
| 44.1 and 48 kHz | 64 frames | 256 frames |
| 96 kHz | 128 frames | 512 frames |
| 192 kHz | 256 frames | 1024 frames |

So the Pi 3 default is about 11 to 12 ms. In stage 1 a bare ALSA probe held that
period for an hour under full load with the board throttled, and the 64 frame
period did not. For the daemon itself, see stage 2 in `doc/measurements.md`. The
Pi 4 default waits for its measurement.

The audio loop waits until the card has room for a period, then reads the clients,
mixes and writes the period at once. So a frame waits behind the card's two
periods and nothing else, and `aas` adds two periods. Measured through the cable
at 48 kHz, that is 10.6 ms on the Pi 3 and 2.6 ms on the CM5. The cost is
headroom. When the loop wakes, one period is still queued, so the wake-up and the
cycle's work together must fit in one period: 5.3 ms at 48 kHz on the Pi 3, and
1.3 ms with the 4 ms target. On the CM5 the worst cycle in an hour under full load
was 0.11 ms. A loop that read the clients first and then waited for room would
hide its work inside the full buffer, but would add a third period of latency.

The loop reads its commands after the wait. A stop or a pause that arrives during
the wait takes effect in that cycle, so the stream does not play one period more.

Resampling adds roughly 1 to 2 ms. A resampled client may exceed 4 ms, in
preference to reducing resampling quality *(call)*.

Latency added by a plugin counts on top of the budget, and `aas` reports it. The
reported delay includes it only in cycles where the chains ran, not on the
bit-perfect copy path or while a DoP stream plays *(call)*.

The analog input adds at most one period to the two the output has, because the
engine keeps at most two periods of captured audio and reads one per cycle
*(call)*.

The target counts as met when one hour of playback at 44.1 kHz and at 96 kHz runs
on the test board with no xruns and no wake-up gap longer than the buffer, on an
otherwise idle system *(call)*. The xrun count alone is not enough, because ALSA
misses an underrun when the interrupt itself is late. A tone played through the
cable on both test boards showed that every gap longer than the buffer is a
break in the sound, and that ALSA counted only some of them. The gap alone is
not enough either: on the Pi 3 some underruns came with a gap just under the
buffer, and those ALSA did count.

Stage 2 made these runs under full load instead, which is harder, on both test
boards. All passed on the Pi 3. On the CM5 one 96 kHz run had one wake-up gap over
the buffer, 4.0 ms against 2.7 ms, and the same run repeated passed. A gap with
those figures, made on purpose later, played one buffer of old audio again. No
run has been made on an idle system yet.

## Analog input

The analog input is off unless the config turns it on, with `[analog] enabled =
true` *(call: the owner should confirm this one)*. It keeps the card's capture
open all the time, which changes the idle behaviour, so it is not a default.
While it is on, the card stays open and running, because input detection has to
keep measuring, and the output never goes off when idle (see Output). While it is
off, capture is not opened.

Capture opens only on cards in the card table marked as having an ADC *(call)*.
The table starts with the two cards the test boards carry,
`snd_rpi_hifiberry_dacplusadc` and `snd_rpi_hifiberry_dacplusadcpro`, and the
Digi as a card without one. It grows as cards are verified. On any other card,
`enabled = true` logs one line and opens nothing. Whether the card has an ADC is
decided at startup if the card is there, and otherwise at each open of the output.

The ADC is a built-in source, mixed in like a client and with its own volume.
The ADC and DAC share the card's clock, so they run at the same rate and the ADC
is never resampled. It follows the output rate and does not set it. On the CM5
test board, five minutes of tone with the input open gave 0 dropped frames, and
64 padded frames in the first cycle only *(call: confirm on the Pi 3 test board,
whose hour runs are recorded in `doc/measurements.md`)*.

Details *(call)*:

- The control thread opens the capture and hands it to the audio thread, because
  opening a PCM takes milliseconds. Before a reopen of the output the engine
  closes the capture, because these cards refuse two rates at once, and the
  control thread opens a new one once the reopen is done.
- Closing a capture closes the device, because these cards also refuse a second
  open while the first is held. A capture is opened without waiting: a device
  that another program holds fails at once, is logged once, and is tried again
  every second.
- The engine reads one period of captured audio per cycle and keeps at most two.
  A shortfall is padded with zeros at the front, and anything over two periods is
  dropped. Both are counted. Capture overruns are not counted on their own, they
  show as padded frames.
- A capture delivers silence for 1 s after each start. The ADC of the DAC+ ADC
  Pro starts with a DC offset of about -28 dBFS that stays above -50 dBFS for
  about 650 ms, and would open the input by itself. This was measured on that
  card only. The cost is that the analog input is muted for 1 s at every start
  and every rate change.
- The engine tells the control thread whether the input has signal, by sending
  the current state when its event queue has room. If the queue is full while the
  input opens and closes again, no event is sent for that pair.

### Input detection plugin

The first plugin, placed on the analog input. It is built into the daemon as
`detect`.

- It has a threshold. Below the threshold it sends no audio at all.
- It is always in the analog chain. There is no way to run the analog input
  without it. When the analog input is off, the ADC stays off.
- There is no mode where analog input passes audio regardless of level.

Further behaviour *(call)*:

- The threshold acts on the level of both channels as RMS over windows of 50 ms,
  not on single samples. One window above the threshold opens it. The threshold
  is -50 dBFS by default.
- A configurable hold time, 3 s by default, stops quiet passages and fade-outs
  from cutting in and out.
- Opening and closing fade over 10 ms (see Volume and clipping). The fade in
  starts at the first frame after the window that opens it, the fade out at the
  frame where it closes. A block counts as without signal only when nothing of
  it is heard, so the fade out is played to its end.
- If the config lists no `detect`, it goes last in the analog chain, so it sees
  the signal after every other plugin there.
- "Has signal" is what the analog chain reports.
- The plugin keeps measuring at the card's current rate while no audio passes.
- Below threshold, the engine treats the input as not connected: it does not
  count as a client, does not affect the rate rules and does not block
  bit-perfect.
- Above threshold, it counts as a client. If it became active first, it counts
  as the first client, and a later player at another rate is resampled.

### Input processor plugin

`input-processor` becomes a native plugin on the ADC input. Today it is a LADSPA
plugin with RIAA equalisation, subsonic filter, gain, declick and notch filter,
at 44.1, 48, 88.2, 96, 176.4 and 192 kHz. It is part of the first shipped version,
and is not built yet.

The order of plugins, including whether it runs before or after input detection,
is set in the config. The shipped default puts it first, so the threshold acts on
the equalised signal as play/stop detection does today *(call)*.

## VU meter

The daemon provides level metering. Today's `vu-meter` service monitors the output
and sends RMS, peak and clipping per channel at 10 Hz. Input levels are available
too, because analog play/stop detection needs them *(call)*. Levels are served by the
`/levels` WebSocket (see API).

Levels are calculated on the audio thread and read by other threads without locks
*(call)*.

## Song recognition

Song recognition runs outside the daemon. `analog-recognition` stays a separate
service and reads the analog input through the ALSA capture device that `aas`
provides, instead of a PipeWire monitor.

The capture device delivers the analog input after its plugins. It never delivers
the output mix, or a network player's audio would be detected as analog activity.
While input detection is below threshold it delivers silence *(call)*.

### Capture device

Details *(call)*:

- The capture device is `pcm.aas` opened for capture, and the ALSA `default`
  device covers both directions.
- A recorder gets the analog input after its plugins and before the input
  volume, so it hears the same level whatever the volume is. That holds in
  forced bit-perfect too, where the card plays the input as captured.
- It runs at the output rate only, as S32 stereo. `plug` in `default` converts in
  the recorder's process. Capture offers mmap access, which `plug` needs to
  convert. The plugin learns the daemon's rate when the PCM is opened, by setting
  up a one-frame stream that it replaces at `hw_params`.
- When the output rate changes, an open capture stream ends. The recorder gets
  `ENODEV` and opens the device again at the new rate. A stream is marked ended
  only once the audio thread has handed it back, and the recorder is woken then.
  Frames still in its ring at that moment are not delivered. While the analog
  input has signal only a DoP stream changes the rate, so otherwise only silence
  is cut.
- A recorder is not a client. It cannot hold the rate or keep the output open.
  While the output is closed for idle it gets no frames and no wake-up. With the
  analog input on, the output never closes.
- A full ring takes what fits. For a capture stream the ring's `underruns`
  counter counts the dropped frames, and the recorder sees an overrun (`EPIPE`).
- The capture flag travels in bit 31 of the channels word of Configure. The
  protocol and the ring are version 3.

## Plugins

There is a way to add plugins that process audio, such as routing, mixing and
filtering. The interface is native to `aas`. LADSPA is not supported, and
`speakereq` will be reimplemented as a native plugin later.

Details *(call)*:

- A plugin runs at one of three places: `analog`, on the analog input before it
  is mixed; `mix`, on the mix; or `output`, just before the output. With one
  output the mix chain runs first and the output chain after it. The output
  volume stays in the mix, so both run after it. Chains on a single client's
  input are not built yet.
- Plugins form an ordered chain at each place, defined in the config. Nothing
  rewires them on its own, and a chain never runs without one of its plugins.
- A rate at which any chain cannot be built, such as one a library plugin
  refuses, is not a rate the output uses. The rate rules take the nearest rate
  every chain accepts, as for a rate the card does not take, and clients are
  resampled to it. It is found when a chain is first built at that rate. If no
  rate works, the output stays as it is and this is logged; at startup a
  chain that cannot be built at any rate the card takes stops the daemon. The
  analog chain has no fallback to its detector alone.
- Plugins see stereo blocks in the processing format. Float blocks are scaled so
  full scale is 1.0 and are not clipped between plugins, and the analog chain's
  output keeps that headroom until the mix. In `i32` a block is clipped before
  the first plugin, as every stage clips there.
- A plugin declares which processing formats it takes. Where it does not take the
  configured one, the chain converts its blocks to `f64`, else `f32`, else
  `i32`, and back. The graph export is to show this.
- Recursive filters keep their state and coefficients in 64-bit float, whatever the
  processing format *(call)*. A 20 Hz high-pass computed entirely in `f32` had an
  error of -90 dBFS at 48 kHz and -73 dBFS at 192 kHz, which is above the DAC's noise
  floor. This matters for the input processor and `speakereq`.
- Plugins take two channels and give two. Other channel counts come with the
  matrix.
- A plugin can report "no signal" for its block.
- Plugins run in-process on the audio thread, so a slow plugin causes dropouts.
- Neutral is a property of a plugin's parameters, not of the audio. A plugin is
  neutral when, with the parameters it has now, it leaves every block as it is.
  The engine asks before the cycle, so the bit-perfect path can skip the chains.
  A plugin that is not neutral stops bit-perfect. Input detection is always
  neutral, since below the threshold the analog input is not a source at all.
  Balance at centre is neutral.
- A chain never depends on the period. A chain with a plugin is built for the
  longest period the engine can have, 16384 frames, and the engine refuses a
  shorter one. A plugin is created for blocks of up to that many frames. This
  costs about 0.5 MiB of scratch memory per chain that holds a plugin, on top of
  about 0.5 MiB in the engine.
- Chains are built on the control thread. A rate change builds new instances
  there. The chains for a new rate are sent just before the reopen and take
  effect with it, as resamplers do. When the rate does not change the chains
  stay, so filters keep their state. A DoP stream bypasses the mix and output
  chains, so they are not built for its rate and keep their state through it;
  only the analog chain, which keeps measuring, is. If the output stays at the
  DoP rate afterwards, the others are built for it then, without a reopen.

Built in are `detect`, the input detection plugin, and `balance`, from -1 (left)
to 1 (right). Balance and the limiter are plugins, not engine features. Balance
goes on the output. The limiter can be added to the chain where wanted once it
exists. It is not built yet.

### Plugin libraries

A plugin in a shared library uses a C ABI, version 1 *(call)*. The crate
`aas-plugin` describes it, and `plugins/gain` is an example. A library exports
one symbol, `aas_plugin`, a function that returns a descriptor: the ABI version,
a name, the formats it takes, its channels, its parameters and six functions. A
library is loaded once and never unloaded.

Details *(call)*:

- The loader refuses a library without the symbol, another ABI version, a null or
  misaligned descriptor, a missing function, no format, other than two channels,
  more than 4096 parameters, a parameter range that is not a range, or a default
  outside its range. Each refusal comes with a message.
- A plugin that cannot be loaded stops the daemon at startup, like any other
  config error. Nothing is skipped without a word.
- At run time the daemon checks that a block is in a format the plugin declared,
  and never hands it more frames than it was created for.
- A panic that leaves a plugin's function aborts the daemon. A plugin written in
  Rust must catch its own panics.

## Graph export

`aas` can export its processing graph: sources, plugins in their order, mix and
outputs, with the current rate, format and latency of each. It shows the live
state, so it also explains what the daemon is doing right now.

The export is JSON, with Graphviz DOT as a second format, available from a command
line tool and the API *(call)*.

## Monitoring and reporting

`aas` reports how processing behaves while it runs, so a problem can be seen and
explained rather than guessed at.

Details *(call)*:

- **Timing:** time spent per audio cycle, average and worst case, in total and per
  plugin, against the time available. This shows how close the system is to a
  dropout.
- **Dropouts:** xruns per output and underruns per client, with timestamps, and
  wake-up gaps longer than the buffer, each of which is a break in the sound
  and which ALSA does not always report as xruns.
- **Latency:** the real current latency per client and output.
- **Buffers:** fill level per client and per output.
- **Resampling:** which clients and outputs are resampled, and the current
  adaptive ratio and clock drift on outputs other than the master.
- **State:** the current rate, format, active clients and whether output is
  bit-perfect, and if it is not, the reason.
- **Events:** a log of what changed and why, such as a rate change, a client
  connecting, input detection opening or an output going off.

The audio thread only writes counters and timings without locks, and a normal
thread reads and publishes them, so monitoring cannot delay audio. Values are
available from the command line tool and the API, and they are
added to the graph export. Events also go to the system journal. Monitoring is
always on, so a problem can be diagnosed after it has happened.

## Failure behaviour

All of this is *(call)*:

- A stalled or crashed client loses only its own audio. It never blocks the audio
  thread.
- The daemon runs under systemd and restarts if it crashes. The ALSA plugin
  reports an error to the client, and the client reconnects by reopening the
  device, as ALSA players do after a write error. The PCM does not reconnect
  an open handle by itself. The control does: a mixer that holds it open is
  woken with an error when the daemon goes, and its next read or write of the
  volume connects again.
- If the card is missing at startup, the daemon keeps running and waits for it.
  Meanwhile audio goes to a timed null sink, so players keep running in real
  time rather than hanging, and the daemon looks for the card every second
  *(call)*. The control thread does the looking. The audio thread opens the
  card once it is there, and does nothing for it before.
- After an xrun on the card, the daemon recovers without client involvement.
- A capture that fails never fails a cycle, and costs only the analog input. The
  input goes quiet, the control thread is told, and it tries again every second.
  A suspended or disconnected capture is reported gone.
- A recorder that stops reading loses only its own audio.
- If a chain cannot be built at a rate, the daemon logs it and does not use
  that rate (see Plugins). It never runs a chain short of its plugins.
- The daemon locks each client's ring in memory, so that the audio thread never
  waits for a page. A ring is at most 8 MiB, for the largest buffer a client may
  ask for, and the control thread adds one page for its view of the header.
  There is no bound over all clients, and the unit sets `LimitMEMLOCK=infinity`.
  This follows the rule that no client is refused: many clients with large
  buffers cost memory that cannot be swapped, not a refusal.

## Bluetooth

A device only receives Bluetooth audio. It does not send audio out to Bluetooth
headphones or speakers.

Bluetooth stays outside the daemon *(call)*. `bluez-alsa` receives A2DP from
BlueZ, and `bluealsa-aplay` plays it to the ALSA `default` device as an ordinary
client. This replaces PipeWire's `bluez5` plugin and needs nothing
Bluetooth-specific in `aas`. Availability on trixie and its latency are to be
confirmed.

## Configuration

One plain TOML file *(call)* holds the output card, the latency target, the
default rate, the processing format, the bit-perfect mode and the plugin chain.
It is flat, and every key is optional. Idle behaviour is `idle = "zeros"` or
`idle = "off"` with `idle_delay_s`, 60 by default. A delay with `zeros` is an
error.

Two tables are not flat *(call)*. `[analog]` holds `enabled`, `false` by default,
and `volume_db`. Each `[[plugin]]` table adds one plugin to the chain at its
`place`, `analog`, `mix` or `output`, in the order of the file. Its `type` is
`detect`, `balance` or `library`, and a `library` has a `path` to the shared
library, which is absolute. `detect` runs only at `analog` and appears at most
once. Every other key is a parameter and must be a finite number.

Volumes and other values changed at runtime are state, not configuration. They
are kept in a separate state file owned by the daemon and restored at startup
*(call)*. The config file is never rewritten by the daemon. A fresh install
starts at 0 dB, as HiFiBerry OS does today *(call)*.

## API

A new API, with no compatibility layer for `/api/pipewire`. The web UI is changed
to use it. It replaces both `pw-api` and the `vu-meter` service, freeing ports 2716
and 2717.

- HTTP with JSON, plus WebSocket, built with `axum` and `tokio`.
- It binds to `127.0.0.1` on one new port and sits behind nginx at `/api/aas/v1`.
- `aas` implements no authentication. `hifiberry-auth` handles it at the gateway. A
  manifest in `/etc/hifiberry/auth.d/` marks volume as `ok`, and plugin, matrix and
  output changes as `risky`.
- It runs inside the daemon on a normal thread. Changes reach the audio thread
  through a lock-free queue, so an API call cannot cause a dropout.
- The command line tool uses the same API.

| Path | Purpose |
|---|---|
| `GET /state` | Rate, format, active clients, bit-perfect status and the reason if not |
| `GET /graph` | The processing graph, as JSON or DOT |
| `GET /metrics` | Timing, dropouts, latency, buffers, drift |
| `GET`, `PATCH /outputs/{id}` | Volume, idle behaviour |
| `GET`, `PATCH /inputs/{id}` | Input volume |
| `GET`, `PUT /matrix` | Mixing matrix |
| `GET /plugins`, `PATCH /plugins/{id}` | Plugin list and parameters, enable and disable |
| `GET`, `PATCH /settings` | Bit-perfect mode, latency target, default rate |
| WebSocket `/events` | State changes as JSON |
| WebSocket `/levels` | VU levels as compact binary frames at 10 Hz |

Each plugin describes its parameters: name, type, range and unit. The API and web
UI build their controls from that, so a new plugin needs no API or UI changes.

Changes to outputs, inputs, the matrix and plugin parameters are saved to the state
file and survive a reboot. Changes through `/settings` last until the next restart.
A config edit is the permanent way to change them, and the API never rewrites the
config *(call)*.

## Testing

Testing uses the test boards, not a VM *(call)*. Both boards can load
`snd-aloop` and may be reinstalled, so a VM would add a step and prove nothing
more. Logic is tested on any machine with a null sink.

The main test board is a Compute Module 5 with a DAC+ ADC Pro and a heatsink, and
it may be reinstalled freely. It needs the `hifiberry-dacplusadcpro,slave` overlay
before any sound card appears, and real-time rights for the daemon. Latency is
proven there. A Pi 3 with a DAC+ ADC is the second test board. A Pi 4 is still
needed, to confirm 4 ms and set its defaults.

Bit-exactness is tested automatically by comparing samples through `snd-aloop`,
with `bit-check`.
Real latency is measured on the test board with a cable from the DAC output to the
ADC input *(call)*. The same cable carries the board tests of the analog input,
which `dist/test-plugin.sh` runs with `ANALOG=1`.

All players must be tested on real hardware with HiFiBerry OS before the first
shipped version: `mpd`, `squeezelite`, `librespot`, `shairport-sync`, `raat`,
`sendspin`, Bluetooth and analog input. Each is tested alone and together with a
second player, including at different sample rates *(call)*. Some of them need
outside services or senders: a Roon core, a Spotify account, an AirPlay sender
and a Lyrion server.

## Not yet known

- How `librespot`, `shairport-sync`, `raat` and `sendspin` connect to audio
  today. Their configuration is not in the `hifiberry-os` repository. Check this
  before relying on "ALSA clients need no changes" for them.
