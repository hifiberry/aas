# aas

Alternative audio server, written in Rust. One daemon owns the sound card, and
players reach it through an ALSA plugin installed as the `default` device. See
[doc/design.md](doc/design.md) for how it behaves and
[doc/measurements.md](doc/measurements.md) for what the boards hold.

Requires Rust 1.85 or newer. The target is Debian trixie, 64-bit, on a
Raspberry Pi.

## Parts

- `aas`, the daemon, in `src/`.
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

On the board, from the repository root:

```sh
sudo install -m 0755 target/release/aas /usr/bin/aas
sudo install -m 0644 dist/aas.service /etc/systemd/system/aas.service
sudo install -m 0644 dist/90-aas.conf /etc/sysctl.d/90-aas.conf
sudo sysctl --system
sudo systemctl daemon-reload
sudo systemctl enable --now aas
sh dist/install-plugin.sh
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
  controls are reached with `amixer -c <card>`.

The daemon reads `/etc/aas/aas.toml` if it exists. Every key is optional.
Without the file the daemon finds the card itself and picks the latency for the
board. An unknown key or a bad value stops the daemon with an error. The volume
is kept in `/var/lib/aas/state.toml`.

| Key | Values | Default | Meaning |
|---|---|---|---|
| `latency_ms` | a number above 0 | 4 on a Pi 5 or CM5, 12 elsewhere | Target for the latency of the card's buffer. The period is the largest power of two that keeps two periods within it. |
| `card` | an ALSA card id or index | the first card that is not `Loopback`, `Headphones` or HDMI | The card to play to. |
| `channels` | 1 to 8 | 2 | The number of channels the card is opened with. A card that does not take the count is treated as missing. |
| `format` | `"f32"`, `"f64"`, `"i32"` | `"f32"` | The arithmetic used to mix and to apply volume. |
| `resampling` | `"high"`, `"medium"` | `"high"` | Quality of the resampler for a client at another rate than the output. |
| `bit_perfect` | `"auto"`, `"forced"`, `"off"` | `"auto"` | `auto` copies the samples untouched while one client plays at the output rate with no software volume. `forced` also ignores the software volume and bypasses plugins, except input detection. `off` always mixes. |
| `idle` | `"off"`, `"zeros"` | `"off"` | What the output does when nothing plays: close the card after a delay, or keep it open and send zeros. |
| `idle_delay_s` | whole seconds | 60 | The delay before `idle = "off"` closes the card. An error with `idle = "zeros"`. |
| `volume_mode` | `"hardware"`, `"software"` | the card's control if it has one | Who applies the volume: the card's mixer, or the daemon. `hardware` on a card without a control warns and uses software. |
| `volume_control` | a mixer control name | the first of `Digital`, `Master`, `PCM` | The card's control that carries the volume. |
| `default_rate` | 8000 to 384000 | 48000 | The rate the output opens at before the first client plays. |

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
input starts at: once the API can change the input volume (stage 4), that
volume is kept in the state file like the output volume, and the config gives
only its value on a fresh install. The input is off unless
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
neutral.

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
