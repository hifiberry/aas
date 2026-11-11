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
sudo systemctl daemon-reload
sudo systemctl enable --now aas
sh dist/install-plugin.sh
```

The unit runs the daemon as a dynamic user in group `audio`, with real-time
rights. Its socket is `/run/aas/aas.sock`, and a player must be in group `audio`
to connect.

`dist/install-plugin.sh` builds the plugin and installs it as
`libasound_module_pcm_aas.so` and `libasound_module_ctl_aas.so`, then installs
`dist/asound.conf` as `/etc/asound.conf`. An existing `/etc/asound.conf` that is
not this one is kept as `/etc/asound.conf.before-aas`. After that:

- ALSA's `default` PCM plays through the daemon. Players need no changes.
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
| `format` | `"f32"`, `"f64"`, `"i32"` | `"f32"` | The arithmetic used to mix and to apply volume. |
| `resampling` | `"high"`, `"medium"` | `"high"` | Quality of the resampler for a client at another rate than the output. |
| `bit_perfect` | `"auto"`, `"forced"`, `"off"` | `"auto"` | `auto` copies the samples untouched while one client plays at the output rate with no software volume. `forced` also ignores the software volume. `off` always mixes. |
| `idle` | `"off"`, `"zeros"` | `"off"` | What the output does when nothing plays: close the card after a delay, or keep it open and send zeros. |
| `idle_delay_s` | whole seconds | 60 | The delay before `idle = "off"` closes the card. An error with `idle = "zeros"`. |
| `volume_mode` | `"hardware"`, `"software"` | the card's control if it has one | Who applies the volume: the card's mixer, or the daemon. `hardware` on a card without a control warns and uses software. |
| `volume_control` | a mixer control name | the first of `Digital`, `Master`, `PCM` | The card's control that carries the volume. |
| `default_rate` | 8000 to 384000 | 48000 | The rate the output opens at before the first client plays. |

Two tables set up the analog input and the audio plugins.

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
```

`[analog]` turns on the card's analog input (`enabled`, default `false`) and sets
its level (`volume_db`, from -60 to 0, default 0). The input is off unless
enabled, and it works only on cards with an ADC that `aas` knows.

Each `[[plugin]]` table adds one plugin to the chain at `place`, in the order of
the file. `type` is a built-in (`detect`, which runs only at `analog` and may
appear once, or `balance`) or `library` with a `path`. Every other key is a
parameter and must be a number.

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

## Testing

- `cargo test` on any machine. The engine and server tests use a null sink, so
  they need no card.
- `sh dist/test-plugin.sh` on the board, with the unit running and the plugin
  installed. It needs `aplay`, `amixer`, `alsactl`, `cc` and `libasound2-dev`,
  and `mpd`, `mpc` and `python3` for the mixer case. It prints PASS or FAIL for
  each case. Case names as arguments run only those. On a board whose output is
  wired to its input, `AAS_TEST_CAPTURE=plughw:<card> sh dist/test-plugin.sh`
  also records what each sample format plays and checks the tone's level and
  purity.
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

`doc/measurements.md` has the method and the results.

## License

MIT, see [LICENSE](LICENSE).
