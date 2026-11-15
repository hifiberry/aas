# A HiFiBerry OS device under PipeWire

What a HiFiBerry OS device runs and has configured for audio today, so that
switching it to `aas` can be written as the exact set of changes. Stage 5 in
`TODO.md` asks for this; the design is in `doc/design.md`, "Non-goals for the
first version".

Taken on 2026-11-15 from two sources:

- **A device:** a Pi 4 with a DAC+, HiFiBerry OS on Debian trixie, about 15
  `hifiberry-*` packages, read without changing anything. Marked **[device]**.
- **The sources:** the `hifiberry-os` packaging, the web UI, `audiocontrol`
  and the player wrappers. Marked **[source]**. Packages whose source is only
  cloned at build time (`pipewire-api`, `hifiberry-input-processor`,
  `roomeq`, `analog-recognition`, the web UI build) are known only from their
  installed files, docs and callers.

## The audio server

PipeWire runs per user, not system-wide. The installer writes the player
user's name to `/etc/hifiberry.user` and enables linger for it; every player
and PipeWire helper is a user unit of that user **[source, device]**.

| User unit | Package | What it is |
|---|---|---|
| `pipewire.socket`, `pipewire.service` | Debian `pipewire` | the daemon, with the filter chains below loaded into it |
| `pipewire-pulse.socket`, `.service` | Debian `pipewire-pulse` | installed and running; no shipped client uses it |
| `wireplumber.service` | Debian `wireplumber` | session manager |
| `filter-chain.service` | Debian `pipewire` | a second PipeWire process, `pipewire -c filter-chain.conf`, enabled and running **[device]** |
| `pipewire-api.service` | `pipewire-api` | the REST API on port 2716, at `/api/pipewire` |
| `vu-meter.service` | `hifiberry-vu-meter` | a libpipewire capture of the default sink's monitor, port 2717, at `/api/vu-meter` |

The graph on the device **[device]**:

- One sink for the card, `Built-in Audio Stereo`, volume 1.0.
- Two filter chains in the main daemon, from `/etc/pipewire/pipewire.conf.d/`:
  `speakereq2x2` (`90-speakereq.conf`, LADSPA `/usr/lib/ladspa/speakereq2x2.so`,
  from `ladspa-speakereq`) and `input-processor` (`90-input-processor.conf`,
  LADSPA `/usr/lib/ladspa/riaa.so`, from `hifiberry-input-processor`).
- `speakereq2x2` is the default sink and source in the `default` metadata. It
  takes that role by its priority (`priority.session = 2000`), not by a
  configured default **[source]**. Every player plays through it; `vu-meter`
  captures its monitor.

PipeWire configuration shipped by HiFiBerry packages, all static **[source,
device]**:

| File | Package | Sets |
|---|---|---|
| `/etc/pipewire/pipewire.conf.d/10-hifiberry-rates.conf` | `hifiberry-pipewire-configs` | `default.clock.rate = 48000`, allowed rates 44.1 to 192 kHz |
| `/etc/pipewire/pipewire.conf.d/99-hifiberry-headless.conf` | same | no portal, JACK D-Bus or X11 bell |
| `/etc/xdg/wireplumber/wireplumber.conf.d/90-default-volume.conf` | same | default sink volume 1.0 |
| `.../91-hide-midi-ui.conf` | same | MIDI nodes hidden |
| `.../98-hifiberry-digi-no-capture.conf` | same | capture disabled on the Digi |
| `.../99-hifiberry-bluetooth.conf` | same | headless BlueZ monitor, A2DP and HFP roles |
| `/etc/pipewire/pipewire.conf.d/90-speakereq.conf` | `ladspa-speakereq` | the speaker EQ filter chain |
| `/etc/pipewire/pipewire.conf.d/90-input-processor.conf` | `hifiberry-input-processor` | the RIAA filter chain |
| `.../99-input-processor-autoconnect.conf` | same | its WirePlumber linking |

The one generated file is AES67's: `hifiberry-aes67` writes
`~/.config/pipewire/pipewire.conf.d/60-hifiberry-aes67.conf` (the
`rtp-sap` module, with a latency from the web UI) and restarts the user's
PipeWire **[source, device]**.

`hifiberry-pipewire-configs` depends on `pipewire`, `hifiberry-input-processor`,
`ladspa-speakereq` and `pipewire-api`. Both meta-packages, `hbos-minimal` and
`hbos-full`, pull in `pipewire`, `pipewire-alsa`, `pipewire-pulse`,
`wireplumber` and `libspa-0.2-bluetooth` **[source]**.

## How players reach the card

No package writes `/etc/asound.conf` **[source, device]**. ALSA's `default`
is PipeWire through Debian's `pipewire-alsa`, which links
`/etc/alsa/conf.d/50-pipewire.conf` and `99-pipewire-default.conf`:
`pcm.!default` and `ctl.!default` of type `pipewire`.

| Player | Unit | Output | Mixer |
|---|---|---|---|
| MPD | user `mpd` | `type "alsa"`, no device: `default` | `hardware`, `hw:N`, `Digital` |
| Shairport Sync | user `shairport` | `output_backend = "alsa"`, no device: `default`; built with ALSA only | `hw:N`, `Digital` |
| librespot | user `librespot` | `--backend rodio`: ALSA `default` | `--mixer alsa`, `hw:N`, `Digital` |
| sendspin | user `sendspin` | `--alsa-device default` | `hw:N` |
| squeezelite | user `squeezelite` | `-o default` | `hw:N` |
| RAAT | user `raat` | `"device": "default"` | `hw:N` |
| Tidal Connect | user `tdcn` | own config, `/etc/tdcn/config.toml` | not read |
| Soloist | user `soloist` | native PipeWire, the default sink | none, volume pinned at 100 |
| AES67 | user `aes67`, `aes67-agent` | PipeWire nodes, linked with `pw-link` to the default sink | none |
| Bluetooth A2DP | system `bluetooth`, `hifiberry-bluetooth` | PipeWire's `bluez5`, routed by WirePlumber; `hifiberry-bluetooth` is only a pairing agent | none |
| USB audio gadget | user `usbaudio` (unpublished) | `pw-link` to a hard-coded node, past the speaker EQ | none |
| analog input | user `analoginput` (in neither meta-package) | `pw-link` to a node the configs no longer create | none |

Every player that uses ALSA `default` needs no change for `aas`: its default
becomes the `aas` plugin. Soloist, AES67, Bluetooth, the USB gadget and the
analog input helper are PipeWire clients or graph-linkers and do.

## Volume

Volume does not go through PipeWire **[source, device]**:

- The web UI sets volume through `audiocontrol` (`/api/audiocontrol/volume/*`),
  a system service. It writes the card's hardware control (`Digital` on the
  DAC+) on `hw:N`, found through the config server's system info.
- MPD, Shairport Sync, librespot, sendspin and squeezelite drive the same
  control through their own ALSA mixers.
- PipeWire's sink stays at 1.0. `pipewire-api` reports it but the UI does not
  set it.
- On a card without a hardware control, `config-soundcard
  --get-or-create-volume-control Softvol` creates a user-space control.
- `config-volume --store/--restore` (system units `volume-store` and
  `volume-restore`) saves the hardware control and the `Master` and `Capture`
  controls of the ALSA `default` ctl, which it calls PipeWire's.

The web UI's sound page uses `pipewire-api` for two things beyond volume
**[source]**: the volume limit (`/module/speakereq/gain/master`, the speaker
EQ's master gain) and balance and mono (`/module/speakereq/crossbar`).

## The web UI and the config server

The web UI's calls to PipeWire services **[source]**:

- `views/sound/general-sound.vue`: speaker EQ crossbar and master gain, as
  above.
- `views/services/system-info.vue`: the versions of `pipewire-api` and
  `vu-meter`.
- `stores/vu-meter.ts`: WebSocket `/api/vu-meter/api/v1/levels`, shown in the
  sidebar on a Pi 5 or later when the VU meter is enabled.
- `stores/settings.ts`: enables and disables `vu-meter` through the config
  server. No config-server drop-in allows that, so the call is likely refused.
- `views/services/pipewire-filter-chain.vue`: calls a config-server route that
  does not exist.

The config server (`hifiberry-configurator`, a root system service on port
1081) starts, stops, enables and disables services by name, at system or user
scope **[source]**. Its allowlist is `/etc/configserver/configserver.json`
plus drop-ins in `/etc/configserver/conf.d/`: the players, Bluetooth, AES67,
the USB gadget, sendspin and Soloist. PipeWire, WirePlumber, `pipewire-api`,
`vu-meter` and `audiocontrol` are not on it. Its factory reset calls
`pipewire-api`'s `settings/reset`.

Both PipeWire services sit behind nginx (`/etc/nginx/hifiberry-api.d/`
`hifiberry-pipewire.nginx` and `hifiberry-vu-meter.nginx`) and have auth
manifests, both `ok` throughout **[device]**.

## What a switch to `aas` touches

From the above, the parts tied to PipeWire:

1. **The ALSA default.** `pipewire-alsa`'s two links in `/etc/alsa/conf.d/`
   against `aas`'s `/etc/asound.conf`. Only one may name `pcm.!default`.
2. **Services.** The user units `pipewire`, `pipewire-pulse`, `wireplumber`,
   `filter-chain`, `pipewire-api` and `vu-meter` against the `aas` system
   service. `aas`'s API and its levels replace `pipewire-api` and `vu-meter`.
3. **Speaker EQ and the input processor.** Both are LADSPA filter chains
   inside PipeWire. Under `aas` they need native plugins, and are lost until
   then; this is the case the design holds back.
4. **The volume limit, balance and mono** in the web UI, today the speaker
   EQ's master gain and crossbar. Under `aas`: the output volume, the
   `balance` plugin and the matrix.
5. **AES67, Soloist, Bluetooth A2DP, the USB gadget and the analog input
   helper.** All are PipeWire-native. Under `aas` AES67 is a later stage,
   Bluetooth needs `bluez-alsa` as the design says (today it is PipeWire's
   `bluez5`, not `bluez-alsa`), Soloist needs an ALSA output, and the analog
   input is `aas`'s own.
6. **The hardware volume control.** Five players and `audiocontrol` write the
   card's control directly. `aas` must not fight them: either it leaves that
   control alone, or the players' mixers move to `aas`'s control.
7. **The config server and configurator.** Its factory reset calls
   `pipewire-api`; `volume.py` treats the `default` ctl's `Master` as
   PipeWire's; `features.d/pipewire.json` names the PipeWire units as the
   required audio engine.
8. **Units ordered `After=pipewire.service`:** MPD, Shairport Sync, librespot,
   sendspin, Soloist, RAAT, `vu-meter`, the analog input helper, AES67, the USB
   gadget and `audiocontrol`. Harmless under `aas`, but none of them waits for
   it.

Not known yet: what `pipewire-api`, `hifiberry-input-processor`, `roomeq` and
`analog-recognition` do internally (no source here), what `filter-chain.service`
runs on the device, and how Tidal Connect outputs.
