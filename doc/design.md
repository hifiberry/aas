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
- The 4 ms target holds on Pi 5, measured on a Compute Module 5, and is still to
  be confirmed on the Pi 4. More latency is fine on a Pi 3, so the default latency
  depends on the board. See [Latency](#latency) and `doc/measurements.md`.
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
- `aas` detects the card itself, so normal use needs no card in the config. The
  config can override it *(call)*.

## Architecture

Three parts *(call)*:

1. **The daemon** owns the sound card and runs the audio engine.
2. **An ALSA PCM and control plugin** is installed as the ALSA `default` device.
   Clients load it through libasound, and it passes audio to the daemon. It also
   provides a capture device for the analog input.
3. **Native plugins** are shared libraries loaded by the daemon.

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
idle when no source is active. While input detection is enabled, capture keeps
running and only the playback side goes off *(call: confirm on the hardware)*.

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
- If the card does not support a client's rate, the output uses the nearest
  supported rate and the client is resampled.
- A source that reports no signal (see Analog input) does not count as active.
- The card keeps its current rate when idle. It does not return to a default.
  The configured default rate applies only from startup until the first client
  sets a rate. So the ADC never forces a rate change: it starts at whatever rate
  the card is at, and no signal is lost to a card restart.

## Audio processing

There is one processing path *(call)*. Audio is converted to 64-bit float,
resampled if needed, processed, mixed, then written to the card as S32. Audio is
never dithered, and the conversion scales by an exact power of two.

64-bit float was chosen because it holds every S32 value exactly. So audio at
0 dB with no plugins comes out with the same bits it went in with. Measurement on
a Pi 3, the slowest supported board, supports keeping it, pending the owner's
approval *(call)*. Mixing costs about 1% of a period at 48 kHz. Resampling costs up
to 1.85 times as much as in 32-bit, and the quality setting controls that cost far
more than the float format does.

Resampling has two quality settings *(call)*. `high` is the default on the Pi 5.
`medium` is the default on the Pi 3, where `high` to 192 kHz uses about 80% of a
period and can overrun it. The Pi 4 default waits for its measurement. Either can
be set in the config.

Input volumes are calculated on this path. So is the output volume on cards
without a hardware volume (see Volume).

Volume changes and input detection opening or closing use short fades, so they do
not click *(call)*.

Mixed clients that sum above full scale clip at the S32 limits unless a limiter
plugin is in the chain. There is no built-in limiter.

### Bit-perfect

Bit-perfect means nothing alters the samples. It is not a separate path: it is
what the path above does when no volume, plugin or resampling applies. There are
two ways to get it:

- **Automatic:** applies when one source is active, all volumes that `aas`
  calculates are at 0 dB and no plugin is active. A hardware output volume does
  not touch the samples, so it does not block bit-perfect *(call)*. Automatic is
  the default *(call)*.
- **Forced:** a setting. Volumes that `aas` calculates have no effect and plugins
  are bypassed *(call for plugins)*. A hardware output volume keeps working
  *(call)*.

A setting can also turn automatic bit-perfect off for testing *(call)*.

With forced bit-perfect, a second client is mixed in. The output is not
bit-perfect while they overlap. Volumes that `aas` calculates still have no
effect, plugins stay bypassed *(call)*, and bit-perfect resumes once one client
is left.

Forced bit-perfect does not bypass input detection, because analog input must
never pass ungated *(call)*. Above threshold the ADC is mixed in like a second
client, which follows the rule above. This is the one exception to plugins being
bypassed.

A resampled client is never bit-perfect.

### DoP

DSD over PCM is supported. It looks like PCM, so `aas` detects its marker bytes.
A DoP stream must reach the card unaltered, because any volume, plugin or mixing
turns it into loud noise. While a DoP stream plays:

- volumes that `aas` calculates and all plugins are bypassed;
- the output runs at the stream's rate;
- it has exclusive use of the output. Other clients stay connected, but their
  audio is discarded until the DoP stream stops *(call)*. This is the one exception
  to mixing, because mixing would produce noise.

## Volume

Output volume has two modes:

- **Card with hardware volume in ALSA:** `aas` uses it.
- **Card without one:** `aas` calculates the volume itself.

`aas` exposes one ALSA volume control mapped to the output volume in both modes,
so clients such as MPD see the same thing on every card *(call)*. `squeezelite` is
reconfigured to use it, instead of writing to the card's mixer directly *(call)*.
The mode is detected from the card, and the config can override it and name the
control *(call)*.

## Latency

The 4 ms budget covers what `aas` adds, from the plugin receiving audio to the
audio leaving the card *(call)*. A player's own ALSA buffer is outside it. At
48 kHz, 4 ms is 192 frames.

Default periods, with 2 periods buffered *(call)*:

| Rate | Pi 5 | Pi 3 |
|---|---|---|
| 44.1 and 48 kHz | 64 frames | 256 frames |
| 96 kHz | 128 frames | 512 frames |
| 192 kHz | 256 frames | 1024 frames |

So the Pi 3 default is about 11 to 12 ms. It held for an hour under full load with
the board throttled. Its 64 frame period did not hold under that load. The Pi 4
default waits for its measurement.

Resampling adds roughly 1 to 2 ms. A resampled client may exceed 4 ms, in
preference to reducing resampling quality *(call)*. The Pi 3 is the exception: it
reduces quality because of CPU cost, not latency.

Latency added by a plugin counts on top of the budget, and `aas` reports it.

The target counts as met when one hour of playback at 44.1 kHz and at 96 kHz runs
on the test board with no xruns and no wake-up gap longer than the buffer, on an
otherwise idle system *(call)*. The xrun count alone is not enough, because ALSA can
miss an underrun when the interrupt itself is late.

## Analog input

The card stays open and running while input detection is enabled, because it
has to keep measuring *(call)*. With detection disabled, capture is not opened.

The ADC is a built-in source, mixed in like a client and with its own volume.
The ADC and DAC share the card's clock, so they run at the same rate and the ADC
is never resampled *(call: confirm on the hardware)*. It follows the output rate
and does not set it.

### Input detection plugin

The first plugin, placed on the ADC input. It is a native plugin.

- It has a threshold. Below the threshold it sends no audio at all.
- It can be enabled and disabled. When disabled, the ADC stays off.
- There is no mode where analog input passes audio regardless of level.

Further behaviour *(call)*:

- The threshold acts on a short level measurement, not on single samples.
- A configurable hold time, a few seconds by default, stops quiet passages and
  fade-outs from cutting in and out.
- The plugin keeps measuring at the card's current rate while no audio passes.
- Below threshold, the engine treats the input as not connected: it does not
  count as a client, does not affect the rate rules and does not block
  bit-perfect.
- Above threshold, it counts as a client. If it became active first, it counts
  as the first client, and a later player at another rate is resampled.

### Input processor plugin

`input-processor` becomes a native plugin on the ADC input. Today it is a LADSPA
plugin with RIAA equalisation, subsonic filter, gain, declick and notch filter,
at 44.1, 48, 88.2, 96, 176.4 and 192 kHz. It is part of the first shipped version.

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

## Plugins

There is a way to add plugins that process audio, such as routing, mixing and
filtering. The interface is native to `aas`. LADSPA is not supported, and
`speakereq` will be reimplemented as a native plugin later.

Details *(call)*:

- Plugins work on 64-bit float.
- A plugin runs at one of three places: on one input, on the mix, or just before
  the output.
- Plugins form an ordered chain defined in the config. Nothing rewires them on
  its own.
- A plugin can have different numbers of input and output channels.
- A plugin can report "no signal" for its input.
- Plugins run in-process on the audio thread, so a slow plugin causes dropouts.
- An active plugin stops automatic bit-perfect, unless it reports itself as
  neutral. Input detection below threshold and balance at centre are neutral
  *(call)*.

Balance and the limiter are plugins, not engine features. Balance goes on the
output. The limiter can be added to the chain where wanted.

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
  wake-up gaps longer than the buffer, which ALSA may not report as xruns.
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
  reports an error to the client, and the client reconnects.
- If the card is missing at startup, the daemon keeps running and waits for it.
- After an xrun on the card, the daemon recovers without client involvement.

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
default rate, the bit-perfect mode and the plugin chain.

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

Development starts in a local VM with a virtual sound card. `snd-aloop` and
`snd-dummy` are available. The VM proves logic and behaviour. Its timing is not
real, so it cannot prove the latency target.

The main test board is a Compute Module 5 with a DAC+ ADC Pro and a heatsink, and
it may be reinstalled freely. It needs the `hifiberry-dacplusadcpro,slave` overlay
before any sound card appears, and real-time rights for the daemon. Latency is
proven there. A Pi 3 with a DAC+ ADC is the second test board. A Pi 4 is still
needed, to confirm 4 ms and set its defaults.

Bit-exactness is tested automatically by comparing samples through `snd-aloop`.
Real latency is measured on the test board with a cable from the DAC output to the
ADC input *(call)*.

All players must be tested on real hardware with HiFiBerry OS before the first
shipped version: `mpd`, `squeezelite`, `librespot`, `shairport-sync`, `raat`,
`sendspin`, Bluetooth and analog input. Each is tested alone and together with a
second player, including at different sample rates *(call)*. Some of them need
outside services or senders: a Roon core, a Spotify account, an AirPlay sender
and a Lyrion server.

## Not yet known

- Whether a Pi 5 class board stays cool enough in the product's case. Without a
  heatsink, a Compute Module 5 throttled on one busy core.
- How `librespot`, `shairport-sync`, `raat` and `sendspin` connect to audio
  today. Their configuration is not in the `hifiberry-os` repository. Check this
  before relying on "ALSA clients need no changes" for them.
