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

On a Pi 3, add `-j2`, or the build can run out of memory. `cargo test
--workspace` also runs the plugin's tests, which need `cc` and the ALSA headers.

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
board. The keys are in `src/config.rs`. The volume is kept in
`/var/lib/aas/state.toml`.

### After a daemon restart

A player that is playing gets an error from its next write and must reopen the
device, which ALSA players do.

The control connects again by itself. A mixer that holds it open is told once
that the daemon has gone, as for a card that is unplugged: its poll reports an
error and reading an event fails. Its next read or write of the volume connects
again, and from then on it gets events as before, starting with one for the
volume, which may have changed meanwhile. While the daemon is away, reads and
writes fail. A mixer that quits when a card is unplugged still has to be
started again.

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
  each case.
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
