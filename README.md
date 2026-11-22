# aas

Alternative audio server, written in Rust. One daemon owns the sound card, and
players reach it through an ALSA plugin installed as the `default` device. See
[doc/design.md](doc/design.md) for how it behaves and
[doc/measurements.md](doc/measurements.md) for what the boards hold.

Requires Rust 1.85 or newer. The target is Debian trixie, 64-bit, on a
Raspberry Pi.

## Parts

- `aas`, the daemon, in `src/`, with the PulseAudio endpoint in
  `src/pulse/`.
- `crates/aas-client`, the client side of the shared ring and the socket
  protocol.
- `crates/aas-alsa`, the ALSA PCM and control plugin, one library.
- `crates/aas-plugin`, the C ABI of audio plugins, and `plugins/gain`, an
  example plugin.
- `tools/measure`, probes that measure the hardware. Not part of the daemon.
- `dist/`, the systemd unit, the ALSA configuration, and the install and board
  test scripts.

## Building

Anything that links ALSA needs `libasound2-dev`. On a machine without it, such
as a workstation, build and test the parts that do not:

```sh
cargo build
cargo test
```

`crates/aas-alsa` is a workspace member but not a default member, so these skip
it. Without the `alsa-output` feature the daemon has no card output and plays to
a null sink, which is enough for the tests.

On the board, with `libasound2-dev` installed, build the daemon with its card
output, the plugin and the probes:

```sh
cargo build --release --features alsa-output
cargo build --release -p aas-alsa
cargo build --release -p measure --features alsa-probes --bins
```

On a Pi 3, add `-j2`, or the build can run out of memory.

The plugin's own tests run where it builds, on the board:

```sh
cargo test -p aas-alsa
```

A plain `cargo test` skips them on every machine, because the crate is not a
default member. `cargo test --workspace` includes them. They need `cc` and the
ALSA headers: they compare the hand-written declarations in
`crates/aas-alsa/src/ffi.rs` with the installed headers, and play the daemon on
a socket for the control. On a machine without `libasound2-dev`,
`cargo check -p aas-alsa --tests` and `cargo clippy -p aas-alsa --all-targets`
still compile the crate and its tests, since neither links.

## Installing

### Debian packages

`./build.sh` builds `aas` and `libasound2-plugin-aas` (with `sbuild` when it
is installed, else `dpkg-buildpackage`; it needs `libasound2-dev`). Installing
them changes nothing on a device: `hifiberry-audio-server set aas` switches
from PipeWire to aas and `set pipewire` back. The steps below are the manual
install.

On the board, from the repository root:

```sh
sudo install -m 0755 target/release/aas /usr/bin/aas
sudo install -m 0644 dist/aas.service /etc/systemd/system/aas.service
sudo install -m 0644 dist/90-aas.conf /etc/sysctl.d/90-aas.conf
sudo sysctl --system
sudo systemctl daemon-reload
sudo systemctl enable --now aas
sh dist/install-plugin.sh
sh dist/install-pulse.sh
```

`dist/90-aas.conf` sets `vm.compact_unevictable_allowed = 0`, system-wide, so
that the kernel does not move the daemon's locked memory when it compacts
memory. Without it a Pi 3 under load has dropouts about once an hour; the
daemon logs a warning at start while the setting is 1. `doc/measurements.md`
has the measurement.

The unit runs the daemon as a dynamic user in group `audio`, with real-time
rights. Its socket is `/run/aas/aas.sock`, and a player must be in group `audio`
to connect.

`dist/install-plugin.sh` builds the plugin and installs it as
`libasound_module_pcm_aas.so` and `libasound_module_ctl_aas.so`, then installs
`dist/asound.conf` as `/etc/asound.conf`. An existing `/etc/asound.conf` that is
not this one is kept as `/etc/asound.conf.before-aas`. After that:

- ALSA's `default` PCM plays through the daemon. Players need no changes.
- ALSA's `default` PCM also records the daemon's analog input. See
  [Recording](#recording).
- ALSA's `default` control is the daemon's output volume, one element called
  `Master`, 0 to 100. Plain `amixer` shows only `Master`. The card's own
  controls are reached with `amixer -c <card>`. A change made to the card's
  control by another program, a player's mixer or `amixer -c`, becomes the
  volume; at start the card's control is read, not written.

`dist/install-pulse.sh` installs `/etc/pulse/client.conf` with
`default-server = unix:/run/aas/pulse` and `autospawn = no`, so players that
use the PulseAudio API reach the daemon as well. See
[PulseAudio clients](#pulseaudio-clients). This is part of the switch to `aas`:
while PipeWire runs, its own `pipewire-pulse` discovery stays, and switching
back restores it. An existing `/etc/pulse/client.conf` that is not this one is
kept as `/etc/pulse/client.conf.before-aas`.

The daemon reads `/etc/aas/aas.toml` if it exists. Every key is optional.
Without the file the daemon finds the card itself and picks the latency for the
board. An unknown key or a bad value stops the daemon with an error. The volume
is kept in `/var/lib/aas/state.toml`.

| Key | Values | Default | Meaning |
|---|---|---|---|
| `latency_ms` | a number above 0 | 4 on a Pi 5 or CM5, 12 elsewhere | Target for the latency of the card's buffer. The period is the largest power of two that keeps two periods within it. |
| `card` | an ALSA card id or index | the first card that is not `Loopback`, `Headphones` or HDMI | The card to play to. |
| `channels` | 1 to 8 | 2 | The number of channels the card is opened with. A card that does not take the count is not opened: the output plays to nothing and the reason is logged once. |
| `clock` | `"auto"`, `"playback"`, `"capture"` | `"auto"` | What clocks the output. `capture` runs it on the card's capture, which then also feeds the analog input, and plays nowhere: the mix reaches only the monitor and the further outputs. `auto` does that for a card that captures and does not play, such as the HiFiBerry ADC, and plays to any other. See [A card that only captures](#a-card-that-only-captures). |
| `format` | `"f32"`, `"f64"`, `"i32"` | `"f32"` | The arithmetic used to mix and to apply volume. |
| `resampling` | `"high"`, `"medium"` | `"high"` | Quality of the resampler for a client at another rate than the output. |
| `bit_perfect` | `"auto"`, `"forced"`, `"off"` | `"auto"` | `auto` copies the samples untouched while one client plays at the output rate with no software volume. `forced` also ignores the software volume, except the volume limit, and bypasses plugins, except input detection. `off` always mixes. |
| `idle` | `"off"`, `"zeros"` | `"off"` | What the output does when nothing plays: close the card after a delay, or keep it open and send zeros. |
| `idle_delay_s` | whole seconds | 60 | The delay before `idle = "off"` closes the card. An error with `idle = "zeros"`. |
| `volume_mode` | `"hardware"`, `"software"`, `"follow"` | the card's control if it has one | Who applies the volume: the card's mixer, or the daemon. `hardware` on a card without a control warns and uses software. `follow` follows and writes `volume_control`, such as a user-space `Softvol`, and applies the volume in software. |
| `volume_control` | a mixer control name | the first of `Digital`, `Master`, `PCM` | The card's control that carries the volume. |
| `volume_poll_ms` | 50 to 5000 | 100 | How often, in ms, the card's volume control is checked for a change made by another program. |
| `volume_limit_db` | -60 to 0 | 0 | The ceiling of the output, in dB below what the volume plays at: 100 % plays at the limit. Applied in software whatever carries the volume, so a player that sets the card's control cannot lift it. 0 sets none. See [the volume limit](#the-volume-limit). |
| `default_rate` | 8000 to 384000 | 48000 | The rate the output opens at before the first client plays. |
| `api_port` | 0 to 65535 | 2718 | The port of the HTTP API, on 127.0.0.1. 0 turns the API off. |
| `pulse_socket` | a path, or `""` | the client socket's directory plus `pulse` | The PulseAudio native-protocol socket. An empty value turns the endpoint off. |
| `pulse_buffer_ms` | 20 to 2000 | 200 | The default and the cap for a PulseAudio stream's buffer. An error with the endpoint off. |

Three tables set up the analog input, the audio plugins and the mixing matrix.

```toml
[analog]
enabled = true
volume_db = 0

[[plugin]]
place = "analog"          # analog, mix or output
type = "detect"           # a built-in name, or "library" with a path
threshold = -50
hold = 3

[[plugin]]
place = "output"
type = "library"
path = "/usr/lib/aas/libaas_gain.so"
gain = -3

[[matrix]]
from = "players.left"     # players.left, players.right, analog.left or analog.right
to = 2                    # the output channel, counted from 1
gain_db = 0               # -100 to 0, default 0

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

`[analog]` turns on the card's analog input (`enabled`, default `false`) and sets
its level (`volume_db`, from -60 to 0, default 0). `volume_db` is the level the
input starts at: a volume set through [the API](#the-api) is kept in the state
file, and applies while `volume_db` in the config stays as it was. The input is
off unless
enabled, and it works only on cards with an ADC that `aas` knows: today the
DAC+ ADC and the DAC+ ADC Pro. On any other card it logs one line, opens
nothing, and the output's idle rule applies as configured. While the input
runs, on a card with an ADC `aas` knows, the output never closes and
`idle = "off"` does not apply. A card that is not there at the start is
looked for every second; if it turns out to have no ADC `aas` knows, the
input stops there with one line in the log, and `idle = "off"` applies again.
The input plays only while its level is above a threshold, -50 dBFS
by default, and stays on for a hold time, 3 s by default, after it falls below.
For 1 s after the capture starts, and after each change of rate, it is silent.

Each `[[plugin]]` table adds one plugin to the chain at `place`, in the order of
the file. `analog` runs on the analog input before it is mixed, `mix` on the mix
and `output` last. Both `mix` and `output` run after the output volume. `type`
is a built-in (`detect`, which runs only at `analog` and may appear once, or
`balance`) or `library` with a `path`, which is absolute. Every other key is a
parameter: a finite number, or text in quotes for a parameter that takes
text, such as a file a plugin reads. Text is passed to the plugin as written,
so a path should be absolute. Each value must fit its parameter: text for
text, a whole number for an integer, 0 or 1 for a switch, and one of the
values for an enumeration. `detect` takes `threshold` in dBFS and
`hold` in seconds. Without a `detect` table it runs last at `analog` with its
defaults. `balance` takes `balance`, from -1 (left) to 1 (right). A plugin that
cannot be loaded stops the daemon with an error.

Each `[[matrix]]` table adds one route: a gain, `gain_db` from -100 to 0 and 0 by
default, from the input `from` to the output channel `to`, counted from 1. The
inputs are `players.left` and `players.right`, the sum of all clients after their
volumes, and `analog.left` and `analog.right`. What no route names is off. Without
any `[[matrix]]` table, left goes to channel 1 and right to channel 2 for both
sources, and further channels are silent. With any `[[matrix]]` table only the
routes listed exist, the analog input included, so a matrix that names only
`players` leaves the analog input silent. The four routes above swap left and right
for both sources. A sum to mono is two routes to the same channel, lowered by 6.03 dB
each (a gain of 0.4995) so that full-scale input on both channels does not clip:

```toml
[[matrix]]
from = "players.left"
to = 1
gain_db = -6.03

[[matrix]]
from = "players.right"
to = 1
gain_db = -6.03

[[matrix]]
from = "analog.left"
to = 1
gain_db = -6.03

[[matrix]]
from = "analog.right"
to = 1
gain_db = -6.03
```

The same `from` and `to` may not appear twice, and `to` may not be above the
width of the matrix. That is the number of channels the first plugin with a fixed
count takes, looking at the `mix` and then the `output` plugins, and `channels`
if there is none. The `mix` and `output` plugins must end at `channels`, or the
daemon stops with an error that gives both counts. An unknown key, a `from` that
is not one of the four names, a `to` that is not a whole number from 1 to 8 and a
`gain_db` outside -100 to 0 stop the daemon with an error naming the table by its
position in the file.

Each further card is an `[[output]]` table, up to three, so four outputs with the
master, whose settings are the flat keys. With any `[[output]]` table the flat `card`
is required, and no two outputs name the same card. An output takes `card`
(required), `channels` (1 to 8, 2 by default), `latency_ms`, `idle` and
`idle_delay_s` as the flat keys do, and `volume_db`, its offset to the `Master`
volume, from -60 to 0 and 0 by default. Its `latency_ms` is the master's target by
default; its card's period follows from it by the same rule, and the card buffers
three periods. A card that cannot run short periods, such as `snd-aloop`, wants a
longer one, such as 48. Every output, the master included, plays as late as the
slowest of them needs, so a long `latency_ms` on one output delays all. One much
shorter than the master's, which the drift could not follow, stops the start with
an error naming the output. Its own `[[output.matrix]]` and `[[output.plugin]]` tables follow the rules
of `[[matrix]]` and `[[plugin]]`, except that `place = "analog"` and `detect` are only
for the master. An output without a matrix plays left to channel 1 and right to
channel 2 for both sources at 0 dB. A further output follows the master's clock
through a resampler, so it is never bit-perfect, and it is silent while the master
plays a DoP stream. If the master's card is lost, the others play on. Each output
follows its own `idle`: when the master's card goes off, a further output with
`idle = "zeros"` plays on, and one with `idle = "off"` closes after its own
`idle_delay_s`. An error names the output, counting the master as output 1:

```toml
card = "sndrpihifiberry"

[[output]]
card = "Loopback"
channels = 2
latency_ms = 48
volume_db = -3
idle = "zeros"

[[output.matrix]]
from = "players.left"
to = 1
gain_db = -6
```

### AirPlay

`aas` can send what the master plays to AirPlay receivers. Each
`[[airplay]]` table, up to four, names one receiver by `host` (a name or an
address), and `aas` connects to it when the mix has a signal and disconnects
after a silence, or stays connected:

```toml
[[airplay]]
host = "soundbar.local"
connect = "signal"        # or "always"
threshold_dbfs = -50      # a 50 ms window above this is a signal
idle_s = 120              # silence before the session ends
volume_db = -20           # the receiver's volume, sent once a session
```

| Key | Values | Default | |
|---|---|---|---|
| `host` | a name or an address | none, it is needed | The receiver. A name that does not resolve is retried like any failure. |
| `port` | 1 to 65535 | 7000 | Its RTSP port. Older receivers, such as an AirPort Express, take 5000. |
| `name` | text | the host | How the API and the log name it. |
| `protocol` | `"airplay1"`, `"airplay2"` | `"airplay1"` | AirPlay 2 as third-party receivers take it: transient pairing with the PIN 3939. A receiver that asks for a PIN of its own or HomeKit pairing, as an Apple TV or a HomePod does, is refused with a message saying so. |
| `connect` | `"signal"`, `"always"` | `"signal"` | When to connect: on the first 50 ms window above `threshold_dbfs`, or from the start and again after any failure. |
| `threshold_dbfs` | -90 to 0 | -50 | The level that counts as a signal: the RMS of both channels, as `detect` measures it. |
| `idle_s` | 1 to 86400 | 120 | Seconds without a signal after which the session ends with TEARDOWN, which frees the receiver's input. Time in which the master sends nothing at all, closed when idle, counts as silence too. |
| `latency_ms` | 50 to 5000 | 1500 | How far ahead of the receiver's clock the audio is sent. Below 250 ms it is also offered to the receiver as its minimum; a receiver that asks for more gets more. A short latency leaves less time to resend a lost packet. |
| `volume_db` | -30 to 0, or `"off"` | 0 | The receiver's own volume, sent once after each connect; `"off"` sends none. |

What is sent is what the master card gets, as `aas.monitor` records it: after
the mix, the plugins, the master gain and the volume limit. A master that plays
nowhere, on a [capture clock](#a-card-that-only-captures), sends its mix all the
same. The audio goes as 44.1 kHz, 16 bits, stereo, uncompressed. With AirPlay
1 (RAOP) it goes in the clear, after the `auth-setup` that sound bars ask for; a
receiver that refuses that step is played to without it. With AirPlay 2 `aas`
pairs first, then seals the control connection, the event connection the
receiver opens a port for, and every audio packet with ChaCha20-Poly1305, and
tells the receiver every 2 s that it is still there. The master's clock and
the receiver's are kept together as a further output's are, by a resampler
whose ratio follows the frames waiting.

A failure, a receiver that closes the connection, one whose audio port refuses
the stream (as after a reboot) or one that stops answering at all (found by TCP
keepalive within about 15 s) is retried after 1, 2, 4, 8 and 16 s, then every
30 s, as long as there is a signal, or always with `connect = "always"`. A new
signal after a failed one starts again at 1 s. Each session logs, when it ends,
how long it lasted, its packets, those the receiver asked for again and those
that were short of frames. The first second or so of a
signal is not heard: the session is still being set up. `GET /airplay` gives
each receiver's `state`, `idle`, `connecting`, `playing` or `retrying`, and the
last failure as `error` until a session plays again.

### Recording

The `default` device records the analog input, for example:

```sh
arecord -D default -f S16_LE -c 2 -r 44100 take.wav
```

It delivers the analog input after its plugins and before the input volume,
which belongs to the mix. It never delivers the mix itself, so nothing a player
plays is recorded except through the card's input. With no signal at the input,
with the input disabled, or on a card without an ADC, it records silence. With
the input disabled and `idle = "off"`, the output closes `idle_delay_s` after
the last player stops, and a recording then gets nothing and waits until a
player opens the output again. While the analog input runs the output stays
open.

`pcm.aas` records S32 stereo at the rate the daemon's output runs at, and
nothing else. `plug` in the `default` device converts that to what the recorder
asks for. When the output changes rate, a recording that is open ends with an
error, `No such device`, and the recorder has to open the device again. A new
recording runs at the new rate. With the analog input enabled the rate does not
change while the input has signal, except for a DoP stream, so otherwise only
silence is cut. A recorder that falls behind loses only its own frames, and
sees an overrun.

### After a daemon restart

A player that is playing gets an error from its next write and must reopen the
device, which ALSA players do.

The control connects again by itself. A mixer that holds it open and waits for
events is woken when the daemon goes, as for a card that is unplugged: its poll
reports an error and reading an event fails. After that its descriptor is
quiet, and reading an event keeps failing, until a read or write of the volume
connects again. From then on it gets events as before, starting with one for
the volume, which may have changed meanwhile. While the daemon is away, reads
and writes of the volume fail. A mixer that quits when a card is unplugged
still has to be started again.

### Removing

```sh
sudo systemctl disable --now aas
sudo rm /etc/systemd/system/aas.service /usr/bin/aas /etc/asound.conf
sudo rm /usr/lib/aarch64-linux-gnu/alsa-lib/libasound_module_*_aas.so
```

Then move `/etc/asound.conf.before-aas` back, if there is one.

## The API

The daemon serves an HTTP API with JSON on `127.0.0.1`, port `api_port`, 2718
by default; `api_port = 0` turns it off. A port that cannot be bound is logged
once and the daemon plays on without the API. It has no authentication of its
own: it is meant to sit behind a gateway, at `/api/aas/v1`. The paths below are
without that prefix.

| Method and path | What it does |
|---|---|
| `GET /state` | The rate, the processing format, every output with its card and whether it is `open`, `parked` or `off`, every connected client with its name, rate and whether it runs, and whether the master is bit-perfect, with the reason if not |
| `GET /outputs`, `GET /outputs/{id}` | Every output, or one |
| `GET /airplay` | Every AirPlay receiver, with its state and last failure |
| `PATCH /outputs/{id}` | Change an output's volume and idle rule |
| `GET /inputs` | The analog input, if enabled, every client that plays and every name given a volume |
| `GET`, `PATCH /inputs/analog` | The analog input's volume and mute |
| `GET`, `PATCH /inputs/clients/{name}` | A client's volume and mute |
| `GET`, `PUT /matrix` | Every output's routes |
| `GET /plugins`, `GET /plugins/{output}/{place}/{index}` | Every plugin, or one |
| `PATCH /plugins/{output}/{place}/{index}` | Change a plugin's values, or enable or disable it |
| `GET`, `PATCH /settings` | The bit-perfect mode, the latency target and the default rate, until the next restart |
| WebSocket `/events` | One JSON object per change |
| WebSocket `/levels` | Every output's and the analog input's levels, a binary frame every 100 ms |

Outputs are numbered as in the config's error messages: the master is 1, the
`[[output]]` tables 2 to 4 in file order. Their fields:

- `volume`, the master only: 0 to 100. It is the `Master` control, so a change
  through either is seen by both. A change made to the card's control by
  another program, a player's mixer or `amixer -c`, becomes the volume; at
  start the card's control is read, not written.
- `volume_db`, a further output only: its offset to the master, -60 to 0.
- `limit_db`, the master only: the [volume limit](#the-volume-limit), -60 to
  0, 0 for none. A value set through the API is kept in the state file, and
  applies while `volume_limit_db` in the config stays as it was.
- `idle`, `"off"` or `"zeros"`, and `idle_delay_s`, with the rules of the
  config keys: a delay with `zeros` is refused. A change takes effect at the
  next idle decision; an output that is off stays off until a client plays.

Inputs have `volume_db`, -60 to 0, and `muted`. A client is known by its
process name, percent-encoded in the path: `/inputs/clients/mpd`. The kernel
keeps at most 15 bytes of a process name, so a longer name is refused with 422.
Its volume applies to every stream of that name and survives a reconnect, and a
name not connected can be given one, so a player can be set before it starts.
A name without a volume plays at 0 dB. At most 64 names are kept; one set back
to 0 dB, unmuted, is no longer kept and frees its place. While a client plays
alone at a volume other than 0 dB the master is not bit-perfect, with the reason
`"volume"`. Every change of an input's volume fades over 10 ms.

#### A card that only captures

On a card with an ADC and no DAC, such as the HiFiBerry ADC, the output runs on
the card's capture clock: each cycle starts when the card has captured a
period. Nothing is played to the card. What the master mixes, the analog input
and any player, reaches `aas.monitor` and every further output, which follow
the capture's clock as they follow a DAC's. The volume is software, since
there is no control to play through. `GET /outputs/1` then says `"clock":
"capture"`. `clock = "capture"` does the same on a card that also plays,
whose DAC then stays closed. `auto` decides once, at the start, from the kernel's list of
PCM devices: a card that is not there yet is played to.

#### The volume limit

The limit is a gain the daemon applies to the master's samples on top of the
volume, and to every further output's. It never touches the card's control: a
player that sets that control to 0 dB still plays at the limit. So that no mode
gets around it, while a limit is set:

- forced bit-perfect applies it too, although it still sets the volumes aside;
- the master is never bit-perfect: with one client playing the reason is
  `"limit"`;
- a DoP stream is silent, and discarded in real time: it cannot be scaled, and
  passed through it would play at the card's volume, past the limit.

A change from one limit to another fades over 10 ms. Setting or lifting a
limit while something plays fades out and back in, as a change of bit-perfect
mode does. A limit of 0 dB changes nothing.

`PUT /matrix` takes `{"outputs": [{"output": N, "routes": [...]}]}`, each
route `{"from": ..., "to": ..., "gain_db": ...}` with the names and rules of a
`[[matrix]]` table. An output it does not name keeps its matrix, and an empty
route list is the default matrix. A matrix must have the width of the one in
place. A change fades over 10 ms, and an output takes at most one matrix every
20 ms; a later one in between replaces the one waiting.

A plugin is named by its output, its place (`analog`, `mix` or `output`) and
its position in that chain from 0: `/plugins/1/mix/0` is the first plugin of
the master's `mix` chain. `GET /plugins` lists every plugin of every output in
chain order, each with its `id` (`"1/mix/0"`), `type` (`detect`, `balance` or
`library`), `path` for a library, the `name` its descriptor gives, whether it
is `enabled` and `neutral`, its `latency_frames`, and its `params`: each with
`name`, `unit`, `type` (`float`, `integer`, `switch`, `enumeration` or `text`),
`min`, `max`, `default`, an enumeration's `values`, and the `value` it has now.
`PATCH` takes `{"params": {"name": value}, "enabled": bool}`, either or both. A
value is refused with 422 when it is out of range, not a whole number for an
integer, a switch or an enumeration, or for a parameter the plugin does not
have; a float is not clamped as it is in the config. A text parameter changes
only in the config, so setting one is 422 too. A change takes effect between
two blocks, and a plugin has to smooth it itself: `balance` ramps to a new
value over 10 ms, while the example `gain` jumps. A disabled plugin is passed
over and its latency counts as 0; enabling or disabling fades over 10 ms, and a
plugin enabled again starts from silence, with no old tail. `detect` cannot be
disabled, nor can a plugin whose channel counts differ: both are 422.

`GET /settings` gives `bit_perfect`, `latency_ms` and `default_rate`, which
`PATCH` changes with the values and messages of the config keys, and `format`
and `resampling`, which change only in the config: naming either in a `PATCH`
is 422. A change lasts until the next restart; it is not kept in the state file.
A new `bit_perfect` applies at once, with no reopen: while something plays, the
output fades out over a period and in again over 10 ms. `forced` is 422 while a
chain changes the channel count, as in the config. A new `latency_ms` reopens
every output with its new period, as a change of rate does, and waits for a DoP
stream to end; a further output with its own `latency_ms` keeps it. A latency
above the one the daemon started with is 422, since the clients' rings, the
further outputs' feeds and the master's delay line were sized for that one; a
client that connects later still gets a ring for it, and needs a buffer as long.
A new `default_rate` applies when no client plays: at once, or when the last
one stops, in place of the rate the card ran at.

A field left out of a `PATCH` stays as it is. A body that does not parse or has
an unknown field is 400, an unknown output or input is 404, and a value out of
range or a matrix that breaks a rule is 422; nothing is clamped. A body over
64 KiB is 413. A call the daemon has not answered in 2 s is 503; a `PATCH` or
`PUT` answered so may still have applied, and can safely be sent again. Each
error is `{"error": "..."}`, with the message the config would give. At most 32
connections are open at once; one more is answered 503 and closed. A connection
that sends nothing for 30 s is closed.

```sh
curl -s 127.0.0.1:2718/state
curl -s -X PATCH -d '{"volume": 60}' 127.0.0.1:2718/outputs/1
curl -s -X PATCH -d '{"params": {"balance": 0.2}}' 127.0.0.1:2718/plugins/1/output/0
curl -s -X PATCH -d '{"volume_db": -6}' 127.0.0.1:2718/inputs/clients/mpd
curl -s -X PATCH -d '{"bit_perfect": "forced"}' 127.0.0.1:2718/settings
curl -s -X PUT 127.0.0.1:2718/matrix -d \
  '{"outputs": [{"output": 1, "routes": [{"from": "players.left", "to": 2}, {"from": "players.right", "to": 1}]}]}'
```

The first gives, with nothing playing:

```json
{"bit_perfect":false,"clients":[],"format":"f32","outputs":[{"card":"","id":1,"state":"open"}],"rate":48000,"reason":"no source"}
```

and a value out of range is answered like this, with status 422:

```json
{"error":"volume_db must be from -60 to 0"}
```

WebSocket `/events` sends a JSON object with a `type` for each change:
`output` (its volume or offset, idle and state), `input` (`id` is `analog` or
`clients/<name>`, with `volume_db` and `muted`), `client` (a name `connected`,
`running`, `stopped` or `gone`), `matrix` (an output's routes), `rate`,
`plugin` (its `id`, whether it is `enabled`, and its `params` by name),
`settings` (`bit_perfect`, `latency_ms` and `default_rate`) and `airplay` (a
receiver's `name`, its `state` and any `error`). A
subscriber that falls more than 256 events behind gets `{"type": "resync"}`
and should read `/state` again; nothing waits for it. Clients are told apart by
name alone, so with two processes of one name, `gone` for the name means that
one of them has gone, not that none plays. A message from a subscriber is
ignored; one over 4 KiB closes its socket. A subscriber is sent a ping every
10 s and has to answer it, as WebSocket libraries and browsers do on their own;
one that answers nothing for 30 s is closed.

WebSocket `/levels` sends a binary frame every 100 ms with the levels of every
output and of the analog input, if enabled, measured by the audio thread over
windows of 100 ms. Each frame is one meter after another:

| Bytes | What they are |
|---|---|
| 1 | The meter: 1 to 4 the output, 16 the analog input |
| 1 | Its channels, n |
| 2 per channel | RMS, then peak, of channel 1, then of channel 2, and so on |
| 1 | A bit per channel that clipped, channel 1 the lowest |

A stereo master alone gives `[1, 2, rms1, peak1, rms2, peak2, clip]`. RMS and
peak run from 0 at -60 dBFS or less to 255 at 0 dBFS or more,
`round((dB + 60) / 60 * 255)` between, so a full-scale sine reads 242 RMS and
255 peak. A sample at or past full scale is a clip. An output is measured after
its chains, before the conversion to the card's format: what the card plays.
During DoP it reads as silence. The analog input is measured as captured,
before its chain. A meter whose newest window ended more than 200 ms ago, of an
output that is off or parked or of an analog input with no capture open, reads
as silence; it stays in the frame, so the layout does not change while the
daemon runs. A subscriber that does not read loses frames and holds up no one.
What a subscriber sends is ignored, one message over 4 KiB closes its socket,
and it is pinged as on `/events`.

What the API sets is kept in the state file and survives a restart: the
master's volume, each client's volume and mute, the analog input's, each
further output's offset and idle rule, each output's matrix, and each plugin's
values and whether it is enabled. A plugin's are kept by the output's card, the
place, the position and the plugin's type and path, and apply only while the
config has that plugin there. A stored value
that has a config key applies only while the config still says what it said
when the value was stored, so an edit of the config always wins. The API never
rewrites the config, and what `/settings` changes is not kept.

### Behind nginx and hifiberry-auth

On HiFiBerry OS the web UI reaches the API through nginx at `/api/aas/v1`, and
`hifiberry-auth` decides at the gateway which calls need the device password.
Two files in `dist/` set that up:

- `dist/nginx/hifiberry-aas.nginx`, the location `/api/aas/v1/`, which passes
  a request to `127.0.0.1:2718` without the prefix and lets `/events` and
  `/levels` upgrade to WebSockets. It goes to `/etc/nginx/hifiberry-api.d/`.
- `dist/auth.d/aas.json`, the manifest for `hifiberry-auth`. Every `GET`, so
  `/events` and `/levels` too, `PATCH /outputs/{id}`, `PATCH /inputs/...`,
  `PUT /matrix` and `PATCH /plugins/...` are `ok`, everyday music use that
  needs no password. `PATCH /settings` is `risky`, since it changes how the
  daemon runs, and so is anything else. It goes to `/etc/hifiberry/auth.d/`.

`dist/install-api.sh` installs both. Run it from the repository root as a
user that may sudo:

```sh
dist/install-api.sh
```

It checks the nginx config with `nginx -t` and reloads nginx, and restarts
`hifiberry-auth`, which reads its manifests only when it starts; each only if
it is running. The location names port 2718. When `/etc/aas/aas.toml` sets another
`api_port`, the script says so, and the `proxy_pass` line has to be changed to
match. To remove them:

```sh
sudo rm /etc/nginx/hifiberry-api.d/hifiberry-aas.nginx /etc/hifiberry/auth.d/aas.json
sudo systemctl reload nginx; sudo systemctl restart hifiberry-auth
```

### aasctl

`aasctl` is the command line tool on top of the API, a second binary of this
crate. It talks to `127.0.0.1` on port 2718, or `--port N`, or the `api_port` of
the config given with `--config PATH`; the flag wins.

```sh
aasctl state                         # also metrics, outputs, inputs, matrix, plugins, settings
aasctl graph --dot | dot -Tsvg > graph.svg
aasctl output 1 volume 60
aasctl output 2 volume-db -6
aasctl output 2 idle zeros           # or: idle off 120
aasctl analog volume-db -6           # or: analog mute, analog unmute
aasctl client mpd mute               # or: client mpd volume-db -6
aasctl plugin 1/mix/0 set gain_db=-3 # or: plugin 1/mix/0 disable
aasctl settings latency_ms=5 bit_perfect=off
aasctl matrix put matrix.json        # the body of PUT /matrix
aasctl events                        # one JSON line per event, until stopped
aasctl levels                        # one line per frame
```

A `GET` prints the answer as indented JSON, `graph --dot` as DOT, and a change
prints the daemon's answer. `levels` prints each meter as `id:` followed by the
RMS and peak of every channel in dBFS with one decimal, `-inf` below -60 and
`CLIP` where the channel clipped. An error answer prints its message on stderr
and exits 1, as does a daemon that cannot be reached; a usage error exits 2.

## PulseAudio clients

Some players cannot output through ALSA and use the PulseAudio API instead.
That is also how most players that advertise PipeWire support emit audio: they
link `libpulse` and rely on `pipewire-pulse`, not the native PipeWire protocol.
`aas` serves them the PulseAudio native protocol itself, on a Unix socket at
`/run/aas/pulse`, mode 0660 and group `audio`. PipeWire is neither required nor
started.

Every playback stream is a client of the daemon, like an ALSA client: it is
mixed, resampled, processed by the same plugins and counted by the same API.
The client's process name is its name in the API, and its stream volume is the
per-client gain, on PulseAudio's curve: 50 percent is -18 dB, as `pactl`
shows it, and volume 0 mutes the client. The sink volume
is the output volume: `pactl set-sink-volume`, `GET /outputs/1` and `amixer`
show the same number.

The endpoint advertises protocol version 35 and implements playback
introspection and control, and recording. SHM, loadable modules, moving or
suspending streams and the native PipeWire protocol are not implemented; those
commands get a PulseAudio error and the connection stays usable. A client
that cannot be served is told why, alone: one malformed connection never affects
another one or the audio thread.

`pactl info`, `paplay`, `parecord` and `mpv --ao=pulse` are the usual checks.
On a board, `sh dist/test-pulse.sh` runs them, compares a `pactl` volume change
with `aasctl outputs` and records from the sources.

### Recording through PulseAudio

Two sources can be recorded, for example with `parecord`:

- `aas.monitor`, the monitor of the sink `aas`: what the master card gets, after
  the mix, the plugins and the software volume. It carries the first two
  channels of the card, zeros while the output plays DoP or is parked, and it
  is also the sink's monitor source.
- `aas.analog`, the analog input after its chain, what an ALSA recorder gets
  from `default`. It exists only while the analog input is enabled.

The default source is `aas.analog` where it exists, otherwise `aas.monitor`:

```sh
parecord -d aas.monitor --channels=2 --format=s16le --rate=48000 take.wav
```

A recording asks for its own format: mono or stereo, S16, S24, S24_32, S32 or
float, at any rate; the daemon converts and resamples. The default fragment is
25 ms. When the daemon's rate changes, a recording goes on after a short gap
and is not ended with an error. A client that stops reading gets overflows, and
loses its oldest frames, instead of making the daemon buffer without limit.

The volume and mute of a source and of a recording stream are fixed at 100
percent and unmuted. `pactl set-source-volume` and the like answer "not
supported", so a recording is never changed behind another client's back. A
recording client appears in the API by its process name, like a player.

Volume meters work: pavucontrol, and the GNOME and KDE settings, open record
streams with peak detection. Such a stream may run at any rate from 1 Hz to
384 kHz, and each value is the peak of the audio it covers (a mono stream
takes the louder channel). Meters of the two sources show what they carry. The
meter of one playing stream records that sink input alone, only on
`aas.monitor`, after the stream's own volume and before the output volume, so
it does not follow the volume of the output. Like PulseAudio, the endpoint
takes a name that is a number for an index: pavucontrol names `aas.monitor`
"0".

## Writing a plugin

A plugin is a shared library built against `crates/aas-plugin`, which describes
the C ABI. `plugins/gain` is a complete example, a gain in dB. The library
exports one function, `aas_plugin`, that returns its descriptor. In Rust the
crate type is `cdylib`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
aas-plugin = { path = "../../crates/aas-plugin" }
```

Build the example and install it where the config above expects it:

```sh
cargo build --release -p aas-plugin-gain
sudo install -D -m 0644 target/release/libaas_gain.so /usr/lib/aas/libaas_gain.so
```

The descriptor and each parameter start with `size`, set to `DESCRIPTOR_SIZE`
and `PARAM_INFO_SIZE`: fields are only ever appended within an ABI version, and
the daemon reads what it knows. `abi` is `ABI`, 2.

A plugin takes a fixed count of channels and gives a fixed count: `channels_in`
and `channels_out`, each 1 to 8. A plugin that takes any count and keeps it, as
`plugins/gain` does, has both 0, and `create` tells it the count in its
`channels` argument; for a fixed plugin that is `channels_in`. A plugin whose
counts are equal processes in place with `process`, `frames * channels`
interleaved samples. One whose counts differ gives `process_into`, which reads
`frames * channels_in` samples at `input` and writes `frames * channels_out` at
`output`, two buffers that do not overlap; it may be left out otherwise. Every
plugin still gives `process`, which is never called when the counts differ. The
daemon refuses a library with one count 0 and the other not, a count above 8,
or counts that differ and no `process_into`, and refuses a chain in which a
plugin does not get the count it takes. A plugin whose counts differ is never
neutral, and runs only at `mix` or `output`: the analog input keeps two channels
throughout, and such a plugin there stops the daemon with an error.

Libraries of ABI version 1, which are stereo and in place and whose `create` has
no `channels` argument, still load, as plugins with two channels in and out.

`create` stores the instance through its last argument and returns
`CREATE_OK`, `CREATE_UNSUPPORTED` for a rate (or block size) it does not
support, which the daemon then never uses, or `CREATE_FAILED`, which stops the
daemon at startup and is tried again at the next change of clients later.
`reset`, which may be left out, clears what an instance keeps from earlier
blocks; the daemon calls it when a chain runs again after a gap, such as after
a DoP stream.

Each parameter has a type, so that a user interface can build a control for it:
`PARAM_FLOAT`, `PARAM_INTEGER`, `PARAM_SWITCH` (0 or 1), `PARAM_ENUMERATION`,
whose values are numbered from 0 and named in `value_names`, or `PARAM_TEXT`.
Numeric values travel as `f64`, whole numbers for every type but a float. A
text parameter, such as a file to read, is set through `set_param_text` with
UTF-8 that is not NUL terminated and is valid only for the call; it returns
`TEXT_OK`, `TEXT_REFUSED`, which the daemon reports as a config error, or
`TEXT_UNSUPPORTED` for a value that cannot be used at the instance's rate,
which the daemon takes as a rate the plugin does not support.

`create`, `destroy`, `set_param_text` and `set_param` while the config's
values are set run on the daemon's control thread. `process`, `process_into`,
`reset`, `neutral` and `latency` run on the audio thread, and so will
`set_param` once the API changes parameters at run time; these must not
allocate, lock or block. Another instance of the same library may be processing while one is
created or destroyed. A panic that leaves one of the plugin's
functions aborts the daemon, so a plugin in Rust must not panic there, or must
catch the panic with `std::panic::catch_unwind`. The crate's documentation
(`cargo doc -p aas-plugin --open`) has the rest.

## Testing

- `cargo test` on any machine. The engine and server tests use a null sink, so
  they need no card.
- `sh dist/test-plugin.sh` on the board, with the unit running and the plugin
  installed. It needs `aplay`, `arecord`, `amixer`, `alsactl`, `cc` and
  `libasound2-dev`, and `mpd`, `mpc` and `python3` for the mixer case. It
  prints PASS or FAIL for each case. Case names as arguments run only those. On
  a board whose output is wired to its input,
  `AAS_TEST_CAPTURE=plughw:<card> sh dist/test-plugin.sh` also records what each
  sample format plays and checks the tone's level and purity, and with
  `ANALOG=1` the cases of the analog input run too.
- `cargo test -p aas-alsa` on the board, for the plugin's own tests.
- `cargo test --test pulse` on any machine, for the endpoint against a daemon
  with a null sink, and `sh dist/test-pulse.sh` on the board, with the unit
  running and `/etc/pulse/client.conf` installed. It needs `pactl`, `paplay`,
  `mpv`, `aasctl` and `python3`, and prints PASS or FAIL for each case. With
  `parecord` it also checks recording.
- `bit-check` proves that what a player sends reaches the card unchanged. Load
  `snd-aloop`, set `card = "Loopback"` and `latency_ms = 48` in the daemon's
  config, then run for example
  `bit-check --format S24 --rate 96000 --seconds 10 --device default --capture hw:Loopback,1,0`.
  It prints `frames compared N, differing N, breaks N`, and fails unless both
  counts are 0. With a kernel at HZ = 250, `snd-aloop` cannot hold a smaller
  buffer while its capture runs, and its xruns show up as breaks.
- `loopback-latency --device default --capture hw:0,0 --rate 48000 --period 256 --periods 4`
  measures the delay through a cable from the card's output to its input. With
  `--capture` it reports the delay after the player's own buffer. Run it once
  more against `hw:` directly, with `--capture` naming the same card another way,
  and subtract: the difference is what `aas` adds.
- `tone-probe --seconds 300` plays a steady sine through `default` and captures
  it from the card's input through the same cable. It prints every break in the
  tone with its time on `CLOCK_MONOTONIC`, the clock of the daemon's wake-up gap
  log, and its length, and counts its own capture overruns apart. `--device
  hw:0,0` with the daemon stopped gives the card alone.
- `stall` makes a wake-up late on purpose, to see what `tone-probe` hears.
  `stall --mode thread --cpu 2 --ms 3` keeps an audio thread pinned to that CPU
  from running. `stall --mode irq --cpu 0 --timers 40000` keeps the CPU that
  serves the card's interrupt in the kernel with interrupts off. On a Pi 3 the
  interrupt goes to any CPU, so name them all: `--cpu 0,1,2,3`.

`doc/measurements.md` has the method and the results.

## License

MIT, see [LICENSE](LICENSE).
