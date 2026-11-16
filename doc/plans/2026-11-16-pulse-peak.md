# PulseAudio peak meters and per-stream monitors Implementation Plan

**Goal:** Volume meters such as pavucontrol's work against `aas`: peak-detect record streams at any rate, and record streams with `direct_on_input` that hear one sink input alone.

**Architecture:** The daemon's monitor tap may name one source stream, and then gets that stream's frames after its own gain, written from the mix loop; a new request tells a connection its stream's id. The PulseAudio endpoint keeps each sink input's daemon stream id in the registry, opens per-stream monitors for `direct_on_input`, and turns a record stream with `peak_detect` into one value per client sample, the peak of the daemon frames it covers.

**Tech Stack:** Rust edition 2024, `rust-version` 1.85. No new dependencies.

**Spec:** `doc/design.md`, "#### Peak meters" under "### Recording"; "### Recording" for the record streams and monitor taps it extends.

## Global Constraints

- No new dependencies.
- The audio thread allocates nothing, takes no locks and makes no blocking calls. Peak computation runs on the PA connection's thread.
- `REQUEST_LEN` stays 20 and `proto::VERSION` stays 3. `Configure` with `capture` and `monitor` carries the stream to record in its rate word, 0 for the master. New request `GetStreamId` (tag 14) is answered by new reply `StreamId { id }` (tag 10); `id` is 0 for a connection without a playback stream.
- A per-stream tap gets the stream's frames after its own gain (the per-client gain), at the output rate, before the matrix, the plugins and the output volume. A period in which the stream is not mixed, or the master is parked or plays DoP, gives zeros. In the copy path it gets the frames as copied.
- A per-stream tap ends as at a rate change (ring `ended`) when its stream is removed or replaced. `Configure` for a stream id that does not exist fails with `ENOENT`.
- A per-stream tap, like every monitor, never holds the output open, never sets its rate and never touches bit-perfect.
- With `peak_detect`, rates 1 Hz to 384 kHz are accepted; without it, 8 kHz to 384 kHz as now. No resampler: each sample is the largest absolute value of the daemon frames its interval covers, counted by the rate ratio; above the daemon rate the last peak repeats. Mono gets the louder channel. Every supported sample format works.
- `direct_on_input` is honoured only on `aas.monitor`: on `aas.analog` it is `ERR_INVALID`; a sink input that does not exist at create is `ERR_NOENTITY`; one gone at a reopen ends the stream with `RECORD_STREAM_KILLED`.
- Unsupported or malformed input answers a PA error or closes only its own connection, never panics.
- Public text (commits, docs, comments) names no tooling and no test machines; commits follow the repository's lowercase `area: summary` style.
- Test with `cargo test -p aas -p aas-client` (libasound is not on the build host for the whole workspace); `cargo clippy -p aas -p aas-client --all-targets` must stay clean of new warnings.

## Review Focus

1. pavucontrol's exact sink-input meter request — float32 mono at 144 Hz, `fragsize` 4, `peak_detect`, `direct_on_input` naming a playing sink input on `aas.monitor` — gets values in 0..=1 at about 144 a second that follow that stream's level, not the mix (Task 5).
2. A sink input deleted while its meter records — the meter's stream gets `RECORD_STREAM_KILLED` and the connection goes on (Task 5).
3. A per-stream tap whose stream is stopped, short of frames or muted — the tap still gets a period of zeros every cycle, so the meter keeps real time and falls to zero (Task 2).
4. A full-scale negative sample, `i32::MIN` — its peak is `i32::MAX`, never an overflow panic (Task 4).
5. A per-stream tap on a stream played through the bit-perfect copy path — it still gets the stream's frames, and the tap does not end bit-perfect (Task 2).

---

## File Structure

- Modify `crates/aas-client/src/proto.rs` — `GetStreamId`, `StreamId`, the rate word of a monitor's `Configure`.
- Modify `crates/aas-client/src/client.rs` — `stream_id`, `configure_stream_monitor`.
- Modify `src/engine.rs` — `Tap::source`, per-stream delivery in the mix loop.
- Modify `src/control.rs` — `GetStreamId`, the target check, ending watchers with their stream.
- Modify `src/pulse/stream.rs` — `client_spec` peak rates, the playback stream's daemon id.
- Modify `src/pulse/registry.rs` — `SinkInput::stream`.
- Modify `src/pulse/record.rs` — `Peaks`, peak streams, `direct_on_input`.
- Modify `src/pulse/conn.rs` — pass the registry to `Record::create`.
- Tests: `tests/server.rs`, `tests/pulse.rs`, unit tests beside the code.
- Docs: `README.md`, `TODO.md`.

### Task 1: Stream ids and per-stream monitors in the client protocol

**Files:**
- Modify: `crates/aas-client/src/proto.rs`
- Modify: `crates/aas-client/src/client.rs`

**Interfaces:**
- Produces: `Request::GetStreamId` (tag 14, no arguments); `Reply::StreamId { id: u32 }` (tag 10); with `capture && monitor`, `Request::Configure::rate` is the stream to record, 0 the master (document this on the variant and in the module comment, no new field).
- Produces: `Client::stream_id(&mut self) -> io::Result<u32>`; `Client::configure_stream_monitor(&mut self, buffer_frames: u32, stream: u32) -> io::Result<u32>` (the rate, as `configure_monitor`, which becomes `configure_stream_monitor(buffer_frames, 0)`).

- [ ] **Step 1: Write the failing tests** in `proto.rs`'s tests: `get_stream_id_round_trips` (`decode_request(&encode_request(&Request::GetStreamId)) == Some(Request::GetStreamId)`, and `Reply::StreamId { id: 7 }` round-trips through `encode_reply`/`decode_reply`), and `a_monitors_rate_word_names_its_stream` (a `Configure { rate: 42, capture: true, monitor: true, .. }` decodes with `rate == 42`).
- [ ] **Step 2: Run** `cargo test -p aas-client proto` — expected FAIL: no `GetStreamId`.
- [ ] **Step 3: Implement** the variants, tags, client methods and doc comments. `stream_id` sends the request and expects `StreamId`; another reply is `io::ErrorKind::InvalidData`, an `Error` the usual errno error.
- [ ] **Step 4: Run** `cargo test -p aas-client` — expected PASS.
- [ ] **Step 5: Commit** `client: ask for a stream's id, and monitor one stream`.

### Task 2: Per-stream taps in the engine

**Files:**
- Modify: `src/engine.rs` (`Tap`, `Taps`, `play_pcm`, `render`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub source: ClientId` on `Tap`, 0 by default in `Tap::new`. A tap with `monitor && source == 0` is the master monitor as now; `monitor && source != 0` is a per-stream tap.

- [ ] **Step 1: Write the failing tests** in the engine's test module, using its existing rig:
  - `a_stream_tap_gets_its_stream_after_its_gain`: two sources play different constant values; a tap with `source` = the first's id records exactly the first's frames times its gain (set gain 0.5, glide finished), nothing of the second.
  - `a_stream_tap_gets_zeros_while_its_stream_does_not_play`: the watched source stopped (and, second case, running with an empty ring) — every cycle still writes a period of zeros to the tap.
  - `a_stream_tap_follows_the_copy_path_and_keeps_bit_perfect`: one source alone in bit-perfect mode; the tap gets its frames bit-exact and the published bit-perfect decision stays OK.
  - `the_master_monitor_ignores_stream_taps`: a per-stream tap does not receive the master block, and the master monitor still does.
  - `a_stream_tap_gets_zeros_while_dop_owns_the_output`.
- [ ] **Step 2: Run** `cargo test -p aas --lib engine::tests::a_stream_tap` — expected FAIL: no field `source`.
- [ ] **Step 3: Implement.** `deliver_monitor` filters on `t.source == 0`. Add to `Taps`: a count or check of running per-stream taps so the mix loop pays nothing without one; `deliver_stream(&mut self, id: ClientId, frames: &[[i32; 2]], gain: Fade)` writing `frames` scaled per frame by `gain.at(frame)` (as `i64` multiply-and-clamp or `f32`, saturating to `i32`) into its `buf` and then to every running tap with that `source`, marking them delivered this cycle; `finish_streams(&mut self, period: usize)` writing a period of zeros to every running per-stream tap not delivered this cycle, then clearing the marks. In `play_pcm`'s source loop call `deliver_stream` after `read_resampled` with the step the source mixed at (`Fade::flat(1.0)` when volumes do not apply) and after `read_raw` with `Fade::flat(1.0)`; skip it while parked. Call `finish_streams` once per `render`, on every path (PCM, DoP, parked), after the mix and before the write. Wake taps with `notify` as the other deliveries do; `output_delay_frames` 0.
- [ ] **Step 4: Run** `cargo test -p aas --lib engine` — expected PASS, the existing tap and bit-perfect tests unchanged.
- [ ] **Step 5: Commit** `engine: a monitor tap of one stream, after its gain`.

### Task 3: Stream ids and per-stream monitors in the control thread

**Files:**
- Modify: `src/control.rs` (request dispatch, `configure_recorder`, `Recorder`, `remove_stream`)
- Test: `tests/server.rs`

**Interfaces:**
- Consumes: Task 1's `Request::GetStreamId`, `Reply::StreamId`, `Client::stream_id`, `Client::configure_stream_monitor`; Task 2's `Tap::source`.
- Produces: the daemon behaviour of the Global Constraints: `GetStreamId` answers the id of the connection's playback stream (0 with none, or an id above `u32::MAX`); a monitor `Configure` with a nonzero rate word sets `tap.source` to the playback stream with that id, else `ENOENT`; removing or replacing that stream ends the tap.

- [ ] **Step 1: Write the failing tests** in `tests/server.rs`:
  - `a_stream_monitor_records_only_its_stream`: two players at 48 kHz play different tones or constants; a monitor configured with the first's `stream_id()` hears the first and not the second (as `a_monitor_records_what_a_player_plays` measures).
  - `a_stream_monitor_ends_when_its_stream_goes`: the watched player's client is dropped; the monitor's `ended()` turns true within a second, and its `start()` then fails.
  - `a_stream_monitor_of_no_stream_is_enoent`: `configure_stream_monitor(4096, 999_999)` fails with `ENOENT`; `stream_id()` on a fresh connection is 0.
- [ ] **Step 2: Run** `cargo test -p aas --test server stream_monitor` — expected FAIL.
- [ ] **Step 3: Implement.** Pass the rate word to `configure_recorder` as `watch: ClientId` (0 for none, ignored without `monitor`). Look the stream up among all connections' `stream`s after this connection's own old stream is removed; not found → `Answer::error(libc::ENOENT)` before any ring is made. Keep `watching: ClientId` in `Recorder`. In `remove_stream`, end every recorder watching that id the way `end_recorders` ends one (factor a shared helper: mark `ended`, clear `running` with its `client_changed`, `RemoveTap`, `taps_len -= 1`). Unit tests in `control.rs` for the dispatch where its existing tests cover `Configure`.
- [ ] **Step 4: Run** `cargo test -p aas -p aas-client` — expected PASS.
- [ ] **Step 5: Commit** `control: monitor one stream, ended with it`.

### Task 4: Peak-detect record streams

**Files:**
- Modify: `src/pulse/stream.rs` (`client_spec`)
- Modify: `src/pulse/record.rs`
- Test: `tests/pulse.rs`

**Interfaces:**
- Produces: `client_spec(spec, map, formats, peak: bool)` — `peak` widens the rate range to `1..=384_000`; playback passes `false`.
- Produces in `record.rs`: `struct Peaks` with `fn new(daemon_rate: u32, client_rate: u32) -> Peaks`, `fn push(&mut self, frames: &[[i32; 2]], channels: u8, out: &mut Vec<[i32; 2]>)`, `fn reset(&mut self)`. Each output frame is `[pl, pr]` for stereo and `[m, m]` with `m = max(pl, pr)` for mono, so `from_stereo_s32` turns it into the client's format unchanged. A sample's magnitude is `v.unsigned_abs().min(i32::MAX as u32) as i32`.

The counting, so that any ratio keeps real time:

```text
for each daemon frame f:
    peak = max(peak, |f|)            per channel
    acc += client_rate
    if acc >= daemon_rate:
        while acc >= daemon_rate: emit peak; acc -= daemon_rate
        peak = 0
```

- [ ] **Step 1: Write the failing unit tests** in `record.rs`: `peaks_take_the_largest_magnitude_per_interval` (daemon 48 000, client 25: 1920 frames in → one frame out holding the largest magnitude; 3840 in → two); `peaks_of_i32_min_are_i32_max`; `mono_peaks_take_the_louder_channel` (`[100, -300]` → `[300, 300]`); `peaks_above_the_daemon_rate_repeat` (daemon 8 000, client 16 000: n frames in → 2n out); `peaks_keep_real_time_at_an_uneven_ratio` (daemon 44 100, client 144: 44 100 frames in → exactly 144 out).
- [ ] **Step 2: Write the failing endpoint tests** in `tests/pulse.rs`, extending `record_body` with `peak_detect: bool` and `direct_on_input: u32` parameters (existing callers pass `false`, `wire::INVALID_INDEX`): `record_peak_a_meter_at_144_hz_reads_the_monitor` (float32 mono 144 Hz, `fragsize` 4, `peak_detect`, on `aas.monitor`, while a PA stream plays a constant at 0.5 full scale: the memblocks are 4 bytes each, values about 0.5, roughly 144 values a second); `record_peak_low_rates_need_peak_detect` (25 Hz without `peak_detect` is an error, with it is created).
- [ ] **Step 3: Run** `cargo test -p aas record::tests::peaks` and `cargo test -p aas --test pulse record_peak` — expected FAIL.
- [ ] **Step 4: Implement.** Read `peak_detect`, the eighth of the nine flags. With it, `Record` holds `Option<Peaks>`, built in `open` for the daemon's rate (rebuilt at a reopen, reset at `flush`) and no resampler; `pull` feeds the daemon frames through `Peaks::push` into a frame buffer and then `from_stereo_s32`.
- [ ] **Step 5: Run** `cargo test -p aas -p aas-client` — expected PASS.
- [ ] **Step 6: Commit** `pulse: peak-detect record streams`.

### Task 5: Record streams of one sink input

**Files:**
- Modify: `src/pulse/registry.rs` (`SinkInput`)
- Modify: `src/pulse/stream.rs` (the daemon id, `entry`)
- Modify: `src/pulse/record.rs` (`Record::create`, `open`)
- Modify: `src/pulse/conn.rs` (`create_record`)
- Test: `tests/pulse.rs`

**Interfaces:**
- Consumes: Task 1's `Client::stream_id`, `Client::configure_stream_monitor`; Task 4's `record_body` parameters.
- Produces: `pub stream: u32` on `SinkInput`, the daemon id of its stream, asked once after the playback stream's `configure` (0 if the request fails). `Record::create` gains `registry: &Arc<Registry>`; a `Record` with `direct_on_input` keeps the sink input index and the registry, and in `open` looks the sink input up and calls `configure_stream_monitor(frames, input.stream)`.

- [ ] **Step 1: Write the failing tests** in `tests/pulse.rs`:
  - `record_direct_a_meter_hears_one_sink_input`: two PA streams play constants at 0.5 and 0.1 full scale; a peak meter (Review Focus 1's request) with `direct_on_input` = the second's index reads about 0.1.
  - `record_direct_a_deleted_sink_input_kills_its_meter`: delete that playback stream; the record connection receives `RECORD_STREAM_KILLED` for the meter within a second and still answers a `GET_SERVER_INFO`.
  - `record_direct_errors`: `direct_on_input` of an index with no sink input → `ERR_NOENTITY`; on `aas.analog` with an analog input configured → `ERR_INVALID`; the connection goes on.
- [ ] **Step 2: Run** `cargo test -p aas --test pulse record_direct` — expected FAIL.
- [ ] **Step 3: Implement.** In `create`, check the source before the sink input: `Source::Analog` with `direct_on_input != INVALID_INDEX` is `ERR_INVALID`; a missing sink input is `ERR_NOENTITY`; a daemon `ENOENT` at create is `ERR_NOENTITY`. A failed `open` at a reopen already ends the stream as gone, which sends `RECORD_STREAM_KILLED`.
- [ ] **Step 4: Run** `cargo test -p aas -p aas-client` and `cargo clippy -p aas -p aas-client --all-targets` — expected PASS, no new warnings.
- [ ] **Step 5: Commit** `pulse: record streams of one sink input`.

### Task 6: Documentation

**Files:**
- Modify: `README.md` ("Recording through PulseAudio")
- Modify: `TODO.md` ("PulseAudio recording: what is left")

- [ ] **Step 1:** In `README.md`, say that volume meters (pavucontrol, the GNOME and KDE settings) work for both sources and for each playing stream, and how the per-stream meter reads (after the stream's own volume, before the output volume).
- [ ] **Step 2:** In `TODO.md`, delete the peak-detect item; keep the other two.
- [ ] **Step 3: Commit** `doc: peak meters and per-stream monitors`.
