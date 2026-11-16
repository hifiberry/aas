# Switching a HiFiBerry OS device between PipeWire and `aas`

Stage 5 ships `aas` beside PipeWire, and the user chooses which one runs. This
is the design of that choice: what is stored, what changes on the device, and
what the user is told. It starts from the list at the end of
`doc/pipewire-device.md`, "What a switch to `aas` touches", and follows its
numbering where it can.

Most of the work is in the HiFiBerry OS repositories, not here. What `aas`
itself needs is under "Volume" and "Work in this repository".

## Decisions

The owner decided on 2026-11-16:

- **The card's hardware volume control is shared, and `aas` follows it.**
  Players and `audiocontrol` keep writing the card's control as they do under
  PipeWire. `aas` takes every change made to it as its own output volume.
- **A switch takes a reboot.** The choice is stored, the files and services are
  changed, and the device reboots into the other server. No live switch.
- **Soloist moves to the PulseAudio interface,** under both servers, instead of
  linking `pw_stream`.
- **Bluetooth audio under `aas` is the next phase.** Until then it is on the
  list of what stops working.

From before (`doc/design.md`, "Non-goals for the first version"): both servers
are installed side by side, exactly one runs, a switch works in both directions
and each server keeps its own settings. AES67 and speaker EQ stay PipeWire's
until their `aas` stages.

Marked *(call)* below: decisions taken in this design, open to the owner's
review.

## The choice

One file holds it: `/etc/hifiberry/audio-server`, one line, `pipewire` or
`aas`. A missing file means `pipewire`, so a device that never switched stays
as it is *(call)*.

One root command applies it: `hifiberry-audio-server set pipewire|aas`. It
writes the file, makes the changes below and returns; the reboot is separate.
`hifiberry-audio-server get` prints the server that runs now and the one chosen
for the next boot, which differ between a `set` and the reboot. The command is
idempotent: `set` to the server already chosen changes nothing *(call)*.

The config server runs the command, through its own route,
`GET` and `POST /api/v1/audio-server`: its generic script route reads only the
main `configserver.json` and no drop-in, so a route of its own is simpler than
editing that file. `GET` answers what `get` prints; on a device without the
command it answers `pipewire` for both, so the web UI works the same before
`aas` is installed. The web UI changes the setting through it, then offers the
reboot through the existing `POST /api/v1/system/reboot` *(call)*.

## What `set` changes

### The ALSA default (1)

Both servers name `pcm.!default` and `ctl.!default`: PipeWire through
`pipewire-alsa`'s links in `/etc/alsa/conf.d/`, `aas` through the configuration
in `dist/asound.conf`. `set aas` links that configuration as
`/etc/alsa/conf.d/99-zz-aas.conf`, read after `99-pipewire-default.conf`, so
its `!default` wins. `set pipewire` removes the link. `pipewire-alsa`'s files
are never touched, so a Debian upgrade of it changes nothing, and
`/etc/asound.conf` stays the user's *(call)*.

### The PulseAudio client (new)

`aas` serves the PulseAudio protocol on `/run/aas/pulse`; PipeWire serves it
through `pipewire-pulse` at libpulse's default place. `set aas` links
`dist/pulse-client.conf` as `/etc/pulse/client.conf.d/90-aas.conf`;
`set pipewire` removes it. `/etc/pulse/client.conf` is not touched, unlike
`dist/install-pulse.sh`, which is for a device that only runs `aas` *(call)*.

### Services (2)

PipeWire's units are user units of the player user, enabled globally by
Debian's presets. `set aas`:

- masks globally, with `systemctl --global mask`, the user units `pipewire`,
  `pipewire-pulse` (socket and service), `wireplumber`, `filter-chain`,
  `pipewire-api` and `vu-meter`;
- masks the user units that only work under PipeWire: the AES67 units, the USB
  gadget and the `analoginput` helper (under `aas` the analog input is the
  daemon's own, `[analog]` in its config);
- enables the system unit `aas.service`.

`set pipewire` unmasks the same units and disables `aas.service`. A global mask
rather than a disable keeps whatever each unit's own enablement was, so
switching back restores it exactly *(call)*.

Players are not touched: they start after the reboot, find `default` pointing
at the running server and play. Units ordered `After=pipewire.service` (8)
start unordered under `aas`, which does no harm since a player that starts
before `aas` gets an error from the plugin and retries as it does today when
PipeWire is late *(call; to verify per player)*.

### The daemon's configuration

`aas` has to follow the same control the players and `audiocontrol` write,
which they find through the config server's system info and
`config-soundcard`, not through `aas`'s own list of names. So the device does
not rely on `aas`'s detection: a oneshot unit, `aas-config.service`, ordered
before `aas.service`, writes `/etc/aas/aas.toml` at every boot from the same
system info: the card, its volume control by name, its channels, and the
analog input only on a card that has one and is not a Digi, whose clock a
capture would tie to its S/PDIF input. Rewriting it at every boot follows a
change of card, as `config-soundcard` does. A user's own additions are kept
in a second file the generated one includes *(call)*.

`set aas` also links `/etc/sysctl.d/90-aas.conf`, the memory compaction
setting `aas` needs, and `set pipewire` removes it, so a device that stays on
PipeWire is not changed by installing the package *(call)*.

### Settings of each server

Nothing is deleted. PipeWire's configuration and `pipewire-api`'s settings
stay in their files while masked; `aas`'s state stays in `/var/lib/aas`. A
switch back finds each as it was left.

## Volume (6)

Under PipeWire the card's control (`Digital` on the DAC+) is written by MPD,
Shairport Sync, librespot, sendspin, squeezelite and `audiocontrol`, and by
nothing in PipeWire. Under `aas` all of them go on doing exactly that.

`aas` already sets the card's control for its output volume. What it adds is
the other direction:

- **It watches the control.** A change that `aas` did not make itself becomes
  the output volume: the control's dB mapped back to a `Master` percent with
  the inverse of the curve `aas` sets it with. It is published to every
  `Master` subscriber and the API, and saved, like a change made through
  `aas`. A change `aas` makes is not taken back in as an outside one.
- **The card's control is the truth.** At start and whenever the output opens,
  `aas` reads the control and takes its value, instead of writing its saved
  volume over it *(call)*. A player that moved the control while `aas` was
  idle is then not undone.
- **Below the control's lowest step**, where `aas` attenuates the rest in
  software, an outside change ends that: the control's value is the volume,
  with no software part, since the outside writer set the control and nothing
  below it *(call)*.
- **Below -60 dB.** `Master` spans -60 to 0 dB, and a card's control and the
  players' own curves reach lower (the DAC+'s `Digital` to about -103 dB). An
  outside value below -60 dB that is not the control's mute step reads as
  `Master` 1, the lowest audible step, not as a mute: a device that plays
  quietly must not show as muted. The card keeps the value the player set
  *(call)*.
- **The mute.** An outside write that turns the control's switch off, or sets
  a control without a switch to its mute step, is a mute: `Master` reads 0,
  and unmuting restores the volume as a mute through `aas` does. A control
  with neither has no outside mute.
- **Bit-perfect** is not affected: the card's control is outside the samples.

A card without a hardware control gets `config-soundcard`'s user-space
`Softvol` control on `hw:N`, and the players' mixers write it as on any other
card. Under `aas` it stays: `aas` follows `Softvol` exactly as it follows a
hardware control, and applies the volume in software, as it does on a card
without a control. The generated configuration names the control and marks it
as one `aas` applies itself. So no player's configuration changes on any card
*(call)*.

`config-volume --store/--restore` keeps saving and restoring the card's
control; `volume-restore` runs before `aas.service`, so `aas` reads the
restored value at start. Under `aas` it skips the `default` ctl's `Master`:
that is `aas`'s, served only while the daemon runs, and `aas` keeps its own
state *(call)*.

## Speaker EQ, input processor, volume limit, balance, mono (3, 4)

PipeWire's speaker EQ and the RIAA input processor are LADSPA chains inside
PipeWire and stop working under `aas` until their native plugins exist. The web
UI's speaker EQ page is not one of them: it runs on the DSP toolkit and works
under both servers. The web UI shows no setting of PipeWire's speaker EQ bands;
its sound page uses that chain for three things only:

- **Balance:** under `aas`, the output's `balance` plugin, through `aas`'s API.
- **Mono:** under `aas`, the matrix, through `aas`'s API.
- **Volume limit:** `aas` has no output ceiling yet. Under `aas` the control is
  hidden until one exists *(call)*.

## What the user is told before switching

Before `set aas`, the web UI lists what will stop working on this device, and
only what is in use there: the input processor enabled (`pipewire-api`'s RIAA
state), AES67 or the USB gadget enabled (their units, through the config
server), a paired Bluetooth device (the system info), a volume limit below
100 %. The user confirms or stays on PipeWire. The
settings stay, so switching back restores them *(call)*.

Switching back to PipeWire needs no warning: nothing is lost.

## The web UI and the configurator (7)

- The UI asks the config server which server runs (`get`) and uses
  `/api/aas/v1` under `aas`, `/api/pipewire` and `/api/vu-meter` under
  PipeWire. The VU meter in the sidebar reads `/api/aas/v1/levels` under
  `aas`: the same bytes as the VU meter's frame, after the meter's id and its
  channel count. The PipeWire filter-chain page and the system info's probes
  of `pipewire-api` and `vu-meter` are left out under `aas`.
- The configurator's factory reset calls `pipewire-api`'s `settings/reset`
  only when PipeWire runs, and resets `aas`'s state when `aas` runs. A factory
  reset sets the choice back to `pipewire` *(call)*.
- `features.d/pipewire.json` names the PipeWire units as the audio engine; an
  `aas.json` beside it names `aas.service`, and the configurator takes the one
  the choice names.
- `volume.py` treats the `default` ctl's `Master` as PipeWire's. Under `aas` it
  is `aas`'s `Master`, which follows the card control, so its readings stay
  right.

## Soloist

Soloist is a closed binary that the device downloads; it plays to PipeWire
on its own, and the wrapper passes it no output option. Whether it can play
through libpulse is checked on a device first; until then it is on the list
of what stops working under `aas`. If it can: Soloist plays through libpulse instead of linking `pw_stream`. Under PipeWire
it reaches `pipewire-pulse`, which is installed and running already; under
`aas` it reaches `/run/aas/pulse`. Its volume stays pinned at 100 % as now.
Its user has to be in group `audio` for the socket *(to verify)*.

## Packages

The packaging lives in this repository, in `debian/`, as it does for every Rust
package of HiFiBerry OS (`acr`, `sendspin`): `hifiberry-os` only has
`packages/aas/build.sh`, which clones this repository and builds it.

- **`aas`:** the daemon, `aasctl`, `aas.service` (installed disabled),
  `hifiberry-audio-server`, `aas-config.service` and the script it runs, the
  API's nginx location and auth manifest (restarting `hifiberry-auth` and
  reloading nginx as `dist/install-api.sh` does), and under `/usr/share/aas/`
  `asound.conf`, `pulse-client.conf` and `90-aas.conf` for `set` to link. It
  installs nothing that changes a device that stays on PipeWire *(call)*.
- **`libasound2-plugin-aas`:** the ALSA plugin.
- **`hifiberryos`:** `hbos-minimal` and `hbos-full` depend on both, beside the
  PipeWire packages they have *(call)*.

Neither package conflicts with a PipeWire package.

## Work in this repository

- The volume follow above (done).
- `debian/` for both packages, `hifiberry-audio-server`, and
  `aas-config.service` with its script, which writes `/etc/aas/aas.toml` from
  the config server's `GET /api/v1/systeminfo`.

In the HiFiBerry OS repositories:

- `hifiberry-os`: `packages/aas/build.sh`, and the two meta packages' Depends.
- `configurator`: the `audio-server` route, factory reset by server,
  `config-volume` skipping `Master` under `aas`, `volume-restore` before
  `aas.service`, and `features.d/aas.json`.
- `hbos-ui`: the server switch page and the rest of "The web UI and the
  configurator".
- `soloist-wrapper`: see "Soloist".

## Next phase

- Bluetooth audio under `aas` through `bluez-alsa`, installed beside
  PipeWire's `bluez5` and switched with the same command.
- The volume limit as an `aas` output ceiling.
- Speaker EQ and the input processor as native plugins, and AES67, as their
  stages in `doc/design.md`.
