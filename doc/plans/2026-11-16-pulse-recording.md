# PulseAudio recording and the output monitor Implementation Plan

**Goal:** libpulse clients can record two sources from `aas`: `aas.analog`, the analog input after its chain, and `aas.monitor`, what the master card is sent.

**Architecture:** The daemon gets a second kind of capture tap, a monitor, fed from the master's final block where the `/levels` output meter reads, and asked for with a new flag bit in `Configure`. The PulseAudio endpoint gains record streams (`src/pulse/record.rs`) that open a capture or monitor stream on the daemon, convert stereo S32 at the daemon's rate into the client's spec on the connection thread, and send it as memblocks; plus the source and source-output objects.

**Tech Stack:** Rust edition 2024, `rust-version` 1.85. No new dependencies. Existing `aas-client`, `rubato` through `crate::resample::Resampler`.

**Spec:** `doc/design.md`, "### Recording" under "PulseAudio clients", and "Clients" for the capture stream it extends.

## Global Constraints

- No new dependencies.
- The audio thread allocates nothing, takes no locks and makes no blocking calls; conversion and resampling for a record stream run on the PA connection's thread.
- `Configure` keeps `REQUEST_LEN` 20 bytes and `proto::VERSION` stays 3: the monitor is one more flag bit (bit 30) beside `CAPTURE` (bit 31).
- A monitor tap never holds the output open, never sets its rate and never touches bit-perfect: it only reads.
- Only the master output is monitored, its first two channels; zeros while it plays DoP and while it is parked; nothing while the output is off.
- Source names are exactly `aas.monitor` and `aas.analog`; `aas.analog` exists only when `config.analog.enabled`.
- Default source: `aas.analog` if it exists, else `aas.monitor`. The sink's monitor source is `aas.monitor`.
- Source and source-output volume and mute are fixed at `VOLUME_NORM` and unmuted; `SET_SOURCE_VOLUME`, `SET_SOURCE_MUTE`, `SET_SOURCE_OUTPUT_VOLUME`, `SET_SOURCE_OUTPUT_MUTE` answer `ERR_NOTSUPPORTED`.
- Default `fragsize` is 25 ms of the client's spec.
- Unsupported or malformed input answers a PA error or closes only its own connection, never panics.
- Public text (commits, docs, comments) names no tooling and no test machines; commits follow the repository's lowercase `area: summary` style.
- Test with `cargo test -p aas -p aas-client` (libasound is not on the build host for the whole workspace); `cargo clippy -p aas -p aas-client --all-targets` must stay clean of new warnings.

## Review Focus

These are the conditions most likely to hurt a recording client. Each has a test in the task that owns the code:

1. The daemon's rate changes during a recording — the endpoint opens a new tap at the new rate and the client keeps receiving data after a short gap, with no error and no `RECORD_STREAM_KILLED` (Task 6).
2. A client that stops reading its socket — the endpoint's buffer for that stream stays bounded by `maxlength`, other connections stay responsive, and an `OVERFLOW` is sent once data was lost (Task 6).
3. A monitor while the master plays DoP or is parked — the tap gets zeros, the DoP stream is untouched and the output's rate is not changed by the tap (Task 2, Task 3).
4. A `fragsize` of 0, `u32::MAX`, not a whole frame, or larger than 64 KiB — rounded to whole frames and clamped, never a zero-length memblock or a panic (Task 6).
5. A record stream asking for a source that does not exist (`aas.analog` without an analog input, an unknown name or index) — `ERR_NOENTITY`, and the connection goes on (Task 6).

---

## File Structure

- Modify `crates/aas-client/src/proto.rs` — `MONITOR` bit, `Request::Configure { monitor }`.
- Modify `crates/aas-client/src/client.rs` — `configure_monitor`.
- Modify `crates/aas-client/src/convert.rs` — `from_stereo_s32`, the inverse of `to_stereo_s32`.
- Modify `src/engine.rs` — `Tap::monitor`, `Taps::deliver` skips monitors, `Taps::deliver_monitor` at the master's meter point.
- Modify `src/control.rs` — `Recorder::monitor`, `configure_recorder(…, monitor, …)`.
- Modify `src/pulse/mod.rs` — `Settings` passed to every connection.
- Modify `src/pulse/wire.rs` — the new command numbers, masks and facilities, `overflow`, `record_killed`.
- Modify `src/pulse/objects.rs` — `Source`, `source_info`, `source_output_info`, sink and server info changes.
- Modify `src/pulse/registry.rs` — default source, source outputs.
- Create `src/pulse/record.rs` — one record stream.
- Modify `src/pulse/conn.rs` — the source and record-stream commands, ticking record streams.
- Modify `tests/pulse.rs`, `tests/server.rs`, `dist/test-pulse.sh`, `README.md`, `TODO.md`.

---

### Task 1: The monitor flag in the client protocol

**Files:**
- Modify: `crates/aas-client/src/proto.rs` (constant beside `CAPTURE`, `Request::Configure`, `encode_request`, `decode_request`, module doc)
- Modify: `crates/aas-client/src/client.rs` (`set_up`, new `configure_monitor`)
- Modify: every constructor of `Request::Configure` in the workspace (`src/control.rs` tests, `crates/*`), adding `monitor: false`

**Interfaces:**
- Produces: `Request::Configure { rate, format, channels, buffer_frames, capture: bool, monitor: bool }`; `Client::configure_monitor(&mut self, buffer_frames: u32) -> io::Result<u32>` (returns the daemon's rate, like `configure_capture`).

- [ ] **Step 1: Write the failing tests** in `proto.rs`'s test module:

```rust
#[test]
fn a_monitor_configure_round_trips_in_twenty_bytes() {
    let r = Request::Configure { rate: 0, format: ClientFormat::S32, channels: 2,
        buffer_frames: 4800, capture: true, monitor: true };
    let bytes = encode_request(&r);
    assert_eq!(bytes.len(), REQUEST_LEN);
    assert_eq!(word(&bytes, 3), 2 | (1 << 31) | (1 << 30));
    assert_eq!(decode_request(&bytes), Some(r));
}

#[test]
fn a_monitor_without_capture_is_refused() {
    let mut bytes = encode_request(&Request::Configure { rate: 48000,
        format: ClientFormat::S16, channels: 2, buffer_frames: 4800,
        capture: false, monitor: false });
    bytes[12..16].copy_from_slice(&(2u32 | (1 << 30)).to_le_bytes());
    assert_eq!(decode_request(&bytes), None);
}
```

- [ ] **Step 2: Run** `cargo test -p aas-client a_monitor` — expect a compile failure (no field `monitor`).
- [ ] **Step 3: Implement.** `const MONITOR: u32 = 1 << 30;` with a doc comment like `CAPTURE`'s. Decode masks both bits off the channels; `monitor` set without `capture` decodes to `None`. `configure_monitor` documents the stream as `configure_capture` does, but carrying what the master card is sent. Update the module doc's sentence about the channels word.
- [ ] **Step 4: Run** `cargo test -p aas -p aas-client` — all pass (the daemon still treats a monitor as an analog capture; Task 3 changes that).
- [ ] **Step 5: Commit** `proto: a monitor flag beside the capture flag in configure`.

### Task 2: The monitor tap in the engine

**Files:**
- Modify: `src/engine.rs` (`Tap`, `Taps::deliver`, new `Taps::deliver_monitor`, `render` at the master meter point ~line 2283)
- Test: `src/engine.rs` tests, beside `a_tap_gets_the_analog_input_after_its_chain`

**Interfaces:**
- Produces: `pub monitor: bool` on `Tap` (false from `Tap::new`); `fn deliver_monitor(&mut self, samples: &[i32], channels: usize, silent: bool)` on `Taps`.

- [ ] **Step 1: Write the failing tests** (use the existing `tap_of`/`add_tap` helpers; set `tap.monitor = true` before `Command::AddTap`):
  - `a_monitor_gets_what_the_master_card_gets`: with a source playing a known non-zero block, after a cycle the monitor's frames equal the first two channels of what the rig's sink received in that cycle.
  - `a_monitor_never_gets_the_analog_input`: with analog signal and no source, the monitor reads zeros, while an analog tap in the same rig reads the input.
  - `a_monitor_takes_the_first_two_channels_of_a_wide_card`: with the `wide(..)` rig at 4 channels, the monitor's frame `i` equals `[out[4i], out[4i+1]]`.
  - `a_monitor_gets_zeros_during_dop`: a DoP stream playing (as the existing DoP tests set one up) — the monitor reads zeros and the sink's samples are the DoP stream's unaltered.
  - `a_monitor_gets_zeros_while_parked`: after `park(..)` with a source playing, the monitor reads zeros, one period per cycle.
  - Extend `taps_without_allocation` with a monitor tap added and removed during the run.
- [ ] **Step 2: Run** `cargo test -p aas monitor` — expect FAIL (field missing).
- [ ] **Step 3: Implement.** `Taps::deliver` skips taps with `monitor`. `deliver_monitor` fills `self.buf[..frames]` with channels 0 and 1 of each interleaved frame (channel 0 twice for a one-channel card), or zeros if `silent`, sets `output_delay_frames` to 0, then `write` and `notify` every running monitor tap; it returns at once when none runs. Call it in `render` beside the output metering, before the sink write, with `silent = self.parked || owner.is_some()`, and also while parked (unlike `deliver`).
- [ ] **Step 4: Run** `cargo test -p aas engine` — all pass, including `taps_do_not_allocate`.
- [ ] **Step 5: Commit** `engine: a monitor tap fed from what the master card gets`.

### Task 3: Monitor streams in the control thread

**Files:**
- Modify: `src/control.rs` (`Recorder`, `request` dispatch, `configure_recorder`)
- Test: `src/control.rs` tests beside the capture-stream tests (~line 3577); `tests/server.rs` beside the existing capture-stream test

**Interfaces:**
- Consumes: Task 1's `Request::Configure { capture: true, monitor }`, Task 2's `Tap::monitor`.
- Produces: `configure_recorder(&mut self, conn: ConnId, buffer_frames: u32, monitor: bool, now: Instant) -> Answer`.

- [ ] **Step 1: Write the failing tests:**
  - control: `a_monitor_does_not_open_the_output` — with the output closed and idle `off`, configure and start a monitor: no `Reconfigure`/open command is sent and the output stays closed; the reply's rate is `policy.current()`.
  - control: `a_monitor_ends_when_the_rate_changes` — as the existing recorder-ends test, with `monitor: true`; the ring is marked ended.
  - control: `a_monitor_tap_is_sent_as_a_monitor` — the `AddTap` the audio thread gets has `monitor == true`.
  - `tests/server.rs`: `a_monitor_records_what_a_player_plays` — one client plays a 1 kHz tone at the daemon's rate, another calls `configure_monitor` and `start`; within 1 s it reads frames whose peak exceeds half the tone's amplitude.
- [ ] **Step 2: Run** `cargo test -p aas monitor` — expect FAIL.
- [ ] **Step 3: Implement.** Pass `monitor` through `request` into `configure_recorder`; store it on `Recorder` and set it on the `Tap` before sending it. Nothing else about recorders changes: a monitor is never a client of the rate rules, never counts as active, and ends with the others in `end_recorders`. If the API lists recorders by direction, list a monitor the same way as a recorder.
- [ ] **Step 4: Run** `cargo test -p aas -p aas-client` — all pass.
- [ ] **Step 5: Commit** `control: monitor streams that record what the master card gets`.

### Task 4: Converting a capture to a client's format

**Files:**
- Modify: `crates/aas-client/src/convert.rs`

**Interfaces:**
- Produces: `pub fn from_stereo_s32(frames: &[[i32; 2]], format: ClientFormat, channels: u8, out: &mut Vec<u8>)` — appends `frames.len()` frames of `channels` (1 or 2) samples in `format`, little endian.

- [ ] **Step 1: Write the failing tests:**
  - `from_stereo_s32_inverts_to_stereo_s32`: for each of `S16`, `S24Packed`, `S24In32`, `S32`, `Float`, stereo, values representable in that format (e.g. `[i32::MIN & mask, 0x1234_5600 & mask]` where `mask` clears the bits the format drops), converting out and back with `to_stereo_s32` returns the input.
  - `mono_is_the_average_of_both_channels`: `[[100 << 16, 300 << 16]]` as `S16` mono gives the bytes of `200i16`; `[[i32::MAX, i32::MAX]]` as `S32` mono gives `i32::MAX` (no overflow).
- [ ] **Step 2: Run** `cargo test -p aas-client from_stereo` — expect FAIL.
- [ ] **Step 3: Implement** as the exact inverse of `to_stereo_s32` per format (read that function for each format's scaling and placement); mono is `((l as i64 + r as i64) / 2) as i32` before conversion. No dither.
- [ ] **Step 4: Run** `cargo test -p aas-client` — PASS.
- [ ] **Step 5: Commit** `convert: from S32 stereo into a client's format`.

### Task 5: Sources in the PulseAudio endpoint

**Files:**
- Modify: `src/pulse/mod.rs` (`Settings`), `src/pulse/conn.rs` (`Conn::new`, dispatch, the source commands), `src/pulse/wire.rs`, `src/pulse/objects.rs`, `src/pulse/registry.rs`
- Test: `tests/pulse.rs`

**Interfaces:**
- Produces:
  - `#[derive(Clone, Copy, Debug)] pub struct Settings { pub buffer_ms: u32, pub analog: bool, pub quality: config::Quality, pub format: config::Format }` in `mod.rs`, built in `spawn` from the config and passed to `accept_loop`, `serve` and `Conn::new` in place of `buffer_ms`.
  - In `objects.rs`: `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Source { Monitor, Analog }` with `index()` (`Monitor` 0, `Analog` 1), `name()` (`"aas.monitor"`, `"aas.analog"`), `description()` (`"Monitor of aas"`, `"aas analog input"`), and `pub fn find_source(index: Option<u32>, name: Option<&str>, default: Source, analog: bool) -> Option<Source>` (index `INVALID_INDEX` or absent means by name; name absent, `@DEFAULT_SOURCE@` means `default`; `@DEFAULT_MONITOR@` means `Monitor`; `Analog` only if `analog`); `pub fn sources(analog: bool) -> Vec<Source>`; `pub fn source_info(source: Source, rate: u32, latency_usec: u64) -> Vec<u8>`.
  - In `registry.rs`: `Registry::new(analog: bool)` replacing `Default`, `default_source() -> Source`, `set_default_source(Source)`.
  - In `wire.rs`: `CREATE_RECORD_STREAM` 5, `DELETE_RECORD_STREAM` 6, `LOOKUP_SOURCE` 11, `GET_SOURCE_INFO` 23, `GET_SOURCE_OUTPUT_INFO` 31, `GET_SOURCE_OUTPUT_INFO_LIST` 32, `SET_SOURCE_VOLUME` 38, `SET_SOURCE_MUTE` 40, `SET_DEFAULT_SOURCE` 45, `SET_RECORD_STREAM_NAME` 47, `GET_RECORD_LATENCY` 57, `CORK_RECORD_STREAM` 58, `FLUSH_RECORD_STREAM` 59, `OVERFLOW` 62, `RECORD_STREAM_KILLED` 65, `SET_RECORD_STREAM_BUFFER_ATTR` 73, `UPDATE_RECORD_STREAM_PROPLIST` 80, `REMOVE_RECORD_STREAM_PROPLIST` 83, `SET_SOURCE_OUTPUT_VOLUME` 98, `SET_SOURCE_OUTPUT_MUTE` 99; `MASK_SOURCE` 0x0002, `MASK_SOURCE_OUTPUT` 0x0008, `EVENT_SOURCE` 1, `EVENT_SOURCE_OUTPUT` 3; `pub fn overflow(stream: u32) -> Vec<u8>`, `pub fn record_killed(stream: u32) -> Vec<u8>`. Extend the test that pins the numbers.

The source info body for protocol 35, in order (check it against `src/pulsecore/protocol-native.c`, `source_fill_tagstruct`, of PulseAudio 17 on sources.debian.org; never raw GitHub): index u32, name, description, sample spec (S32LE, 2 ch, the daemon's rate), channel map (FL FR), owner module u32 `INVALID_INDEX`, cvolume `[NORM, NORM]`, mute false, monitor of sink u32 (0 for `Monitor`, `INVALID_INDEX` for `Analog`), monitor of sink name (`"aas"` or null), latency usec, driver `"aas"`, flags u32 (`PA_SOURCE_LATENCY` 0x2 | `PA_SOURCE_HARDWARE` 0x4 for `Analog`, `PA_SOURCE_LATENCY` for `Monitor`), proplist (`device.description`, and `device.class` `"monitor"` or `"sound"`), configured latency usec, base volume `NORM`, state u32 (0 running), n_volume_steps 65537, card `INVALID_INDEX`, n_ports 0, active port null, n_formats 1 + one PCM format_info.

- [ ] **Step 1: Write the failing tests** in `tests/pulse.rs`, with a `read_source_info_as_libpulse` reader in the order above, modelled on `read_sink_info_as_libpulse`:
  - `the_sources_are_listed`: without an analog input, `GET_SOURCE_INFO_LIST` gives exactly one source, index 0, `aas.monitor`, monitor of sink 0 named `aas`.
  - `the_sink_names_its_monitor`: the sink info's monitor source is 0, named `aas.monitor`.
  - `the_default_source_is_the_monitor_without_an_analog_input`: server info names `aas.monitor`; `LOOKUP_SOURCE` of `@DEFAULT_SOURCE@` and `@DEFAULT_MONITOR@` answers 0; of `aas.analog` answers `ERR_NOENTITY`.
  - `source_volume_is_fixed`: `SET_SOURCE_VOLUME` and `SET_SOURCE_MUTE` on index 0 answer `ERR_NOTSUPPORTED`, and the connection still answers `GET_SERVER_INFO`.
  - `set_default_source_names_one_of_ours`: `SET_DEFAULT_SOURCE` `aas.monitor` answers a reply; `"nonsense"` answers `ERR_NOENTITY`.
  - Unit tests in `objects.rs` for `find_source` with `analog: true`: no name gives `Analog` as default; `@DEFAULT_MONITOR@` gives `Monitor`; index 1 gives `Analog`; index 2 gives `None`; and `sources(true)` is `[Monitor, Analog]`.
- [ ] **Step 2: Run** `cargo test -p aas --test pulse source` — expect FAIL.
- [ ] **Step 3: Implement** the handlers in `conn.rs`: `GET_SOURCE_INFO` (by index or name), `GET_SOURCE_INFO_LIST`, `LOOKUP_SOURCE`, `SET_DEFAULT_SOURCE` (changes the registry, notifies `EVENT_SERVER | EVENT_CHANGE`), the four volume/mute commands with `ERR_NOTSUPPORTED`; server info's default source is `registry.default_source().name()`; sink info's monitor source fields are 0 and `aas.monitor`; subscriptions pass the source masks through. `GET_SOURCE_OUTPUT_INFO_LIST` answers an empty list until Task 6.
- [ ] **Step 4: Run** `cargo test -p aas -p aas-client` — PASS.
- [ ] **Step 5: Commit** `pulse: the analog input and the monitor as sources`.

### Task 6: Record streams

**Files:**
- Create: `src/pulse/record.rs`
- Modify: `src/pulse/conn.rs` (record streams beside `streams`, the record commands, `tick`, `tick_interval`, `Drop`), `src/pulse/registry.rs` (source outputs), `src/pulse/objects.rs` (`source_output_info`), `src/pulse/mod.rs` (`pub mod record`)
- Test: `tests/pulse.rs`, unit tests in `record.rs`

**Interfaces:**
- Consumes: Task 1's `Client::configure_capture`/`configure_monitor`, Task 4's `from_stereo_s32`, Task 5's `Settings`, `Source`, `find_source`, wire constants; `crate::resample::Resampler`; `aas_client::ring::Ring` as a bounce buffer for it; `stream::{pcm_spec, pcm_props, CreateError}`.
- Produces:
  - `registry::SourceOutput { index, client, name, source: Source, rate, channels, format, corked, buffer_usec, source_usec }` with `add_source_output`, `update_source_output`, `remove_source_output`, `source_outputs`, `source_output`, each notifying `EVENT_SOURCE_OUTPUT` like the sink-input ones.
  - `objects::source_output_info(output: &SourceOutput) -> Vec<u8>`.
  - In `record.rs`: `pub struct Record` with `pub index`, `pub client`, `pub name`; `Record::create(settings: Settings, daemon_socket: &Path, peer_pid: Option<u32>, client_index: u32, index: u32, default: Source, reader: &mut Reader) -> Result<CreatedRecord, CreateError>`; `poll(&mut self) -> RecordPolled { packets: Vec<Vec<u8>>, gone: bool }`; `cork(&mut self, corked: bool) -> io::Result<()>`; `flush(&mut self) -> io::Result<()>`; `set_buffer_attr(&mut self, maxlength: u32, fragsize: u32) -> [u32; 2]`; `latency(&self) -> RecordLatency { source_usec, playing, write_index, read_index }`; `entry(&self) -> SourceOutput`; `interval(&self) -> Duration`; `release(&mut self)`.

`CREATE_RECORD_STREAM` for protocol 35, in order (check against `command_create_record_stream` in the same file): sample spec, channel map, source index u32, source name string, maxlength u32, corked bool, fragsize u32, 7 bools (no_remap … variable_rate), peak_detect bool, adjust_latency bool, proplist, direct_on_input u32, early_requests bool, dont_inhibit_auto_suspend bool, fail_on_suspend bool, n_formats u8 + format_infos, cvolume, muted bool, volume_set bool, muted_set bool, relative_volume bool, passthrough bool. The reply: stream index u32, source output index u32 (the same number), maxlength u32, fragsize u32, sample spec, channel map, source index u32, source name, suspended false, configured latency usec, format_info. `GET_RECORD_LATENCY` replies monitor usec 0, source usec, playing bool, the request's timeval, now, write_index s64, read_index s64. The source output info body: index, name, owner module `INVALID_INDEX`, client, source, sample spec, channel map, buffer usec, source usec, resample method `"aas"` or `"trivial"`, driver `"aas"`, proplist, corked, cvolume `[NORM; channels]`, mute false, has_volume true, volume_writable false, format_info.

Behaviour decided here (the spec's calls, made concrete):
- The source comes from `find_source(index, name, default, settings.analog)`; `None` is `ERR_NOENTITY`. `Analog` opens `configure_capture`, `Monitor` `configure_monitor`, each on its own daemon connection with `identify(peer_pid)`, ring `settings.buffer_ms` of the daemon's rate. Formats and channels are checked as `Stream::create` checks them (1–2 channels, the five PCM formats, 8–384 kHz, a format list takes its first PCM entry).
- `fragsize` in bytes: `u32::MAX` or 0 is 25 ms; otherwise rounded down to whole frames, at least one frame, at most 64 KiB rounded down to whole frames. `maxlength`: `u32::MAX` or 0 is 2 s; at least `fragsize`.
- `poll` reads everything the tap ring holds; at another rate than the client's it goes through a bounce `Ring` and a `Resampler::new(daemon_rate, client_rate, period, settings.quality, settings.format)` built at creation and on every reopen; then `from_stereo_s32` into a pending byte buffer; whole `fragsize` chunks leave as `Encoder::memblock(index, chunk)`. Pending bytes beyond `maxlength` drop the oldest, and that counts as an overrun.
- A new overrun (the ring header's `underruns`, or a drop from pending) sends one `OVERFLOW` per poll.
- When `daemon.ended()` the stream configures the same source again, rebuilds or drops the resampler for the new rate, starts it unless corked, and goes on. A daemon connection that fails is `gone`: the conn sends `RECORD_STREAM_KILLED` and removes the stream.
- Corked: the tap is stopped and nothing is sent. `flush` resets the tap and clears pending.
- `interval()` is the fragsize's duration halved, clamped to `MIN_TICK..=MAX_TICK`; `tick_interval` takes the minimum over uncorked playback and record streams.

- [ ] **Step 1: Write the failing tests** in `tests/pulse.rs` (a helper `create_record_stream(conn, spec, source_name, fragsize)` returning the stream index, and a helper collecting memblocks for the stream for a duration):
  - `a_monitor_recording_hears_a_player`: one PA connection plays a 1 kHz tone S16 stereo 48 kHz; another records `@DEFAULT_MONITOR@` S16 stereo 48 kHz; within 1 s memblocks arrive, each `fragsize` bytes, and the peak exceeds half the tone's.
  - `a_recording_is_converted_to_the_clients_spec`: recording S16 mono 44.1 kHz while the daemon runs at 48 kHz gives `44100 ± 5%` frames per second of 2-byte frames, with the tone audible.
  - `fragsize_is_rounded_and_clamped`: fragsize 0, `u32::MAX`, 1001 (S16 stereo), 1 << 20 give reply fragsizes 4800, 4800, 1000, 65536 at 48 kHz S16 stereo.
  - `an_unknown_source_is_noentity`: `aas.analog` (no analog input in tests) and index 7 answer `ERR_NOENTITY`, and `GET_SERVER_INFO` still answers.
  - `a_corked_recording_sends_nothing`: created corked, no memblock in 300 ms; uncorked, memblocks arrive.
  - `a_recording_survives_a_rate_change`: record the monitor; a player at 48 kHz, then one at 44.1 kHz so the daemon's rate changes; memblocks keep arriving after the change, and no `RECORD_STREAM_KILLED` comes.
  - `a_client_that_stops_reading_gets_an_overflow`: record with maxlength 9600 (50 ms), read nothing for 1 s while another connection's `GET_SERVER_INFO` is answered within 200 ms, then read: an `OVERFLOW` for the stream is among the packets.
  - `source_outputs_are_listed_and_announced`: a subscribed connection sees NEW then REMOVE for `EVENT_SOURCE_OUTPUT`; while it runs, `GET_SOURCE_OUTPUT_INFO_LIST` has one entry with the source 0, the client's index and the stream's name.
  - `source_output_volume_is_fixed`: `SET_SOURCE_OUTPUT_VOLUME` and `SET_SOURCE_OUTPUT_MUTE` answer `ERR_NOTSUPPORTED`.
  - `record_latency_is_answered`: `GET_RECORD_LATENCY` replies in the order above with `write_index >= read_index`.
- [ ] **Step 2: Run** `cargo test -p aas --test pulse record` — expect FAIL.
- [ ] **Step 3: Implement** `record.rs` and the `conn.rs` handlers: `CREATE_RECORD_STREAM`, `DELETE_RECORD_STREAM`, `CORK_RECORD_STREAM`, `FLUSH_RECORD_STREAM`, `GET_RECORD_LATENCY`, `SET_RECORD_STREAM_BUFFER_ATTR` (replies maxlength, fragsize, configured latency usec), `SET_RECORD_STREAM_NAME`, `UPDATE_RECORD_STREAM_PROPLIST`, `REMOVE_RECORD_STREAM_PROPLIST` (accepted, name taken from `media.name` as for playback), `GET_SOURCE_OUTPUT_INFO(_LIST)`, `SET_SOURCE_OUTPUT_VOLUME`/`MUTE` with `ERR_NOTSUPPORTED`. `tick` polls record streams after playback streams and updates the registry entry's latencies. Record streams are released in `Drop` like playback streams.
- [ ] **Step 4: Run** `cargo test -p aas -p aas-client` and clippy — PASS, no new warnings.
- [ ] **Step 5: Commit** `pulse: record streams from the analog input and the monitor`.

### Task 7: Documentation and the board check

**Files:**
- Modify: `README.md` (the PulseAudio section: recording, the two sources, the default, fixed source volume), `doc/design.md` (only if the implementation settled something the section leaves open), `TODO.md` (remove any recording entry), `dist/test-pulse.sh`

- [ ] **Step 1:** Add a `record` check to `dist/test-pulse.sh`: play a 2 s tone with `paplay` and at the same time `parecord -d aas.monitor --channels=1 --format=s16le --rate=44100` for 1 s into a file; PASS if the file holds at least 0.8 s of audio and its peak exceeds a quarter of full scale (in Python, as the script's other checks do). If `pactl list short sources` shows `aas.analog`, also record 1 s from it and PASS on a file of at least 0.8 s (its content depends on the input).
- [ ] **Step 2:** Run `bash -n dist/test-pulse.sh` — no output.
- [ ] **Step 3:** Update the docs; `cargo test -p aas -p aas-client` once more.
- [ ] **Step 4: Commit** `doc: recording through the PulseAudio endpoint`.

The board run (both test boards, with their analog inputs) is done by the controller after the final review, as for the earlier PulseAudio work.
