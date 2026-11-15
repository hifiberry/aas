# Stage 5: the PulseAudio endpoint Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When `aas` runs instead of PipeWire, libpulse players connect to a PulseAudio native-protocol endpoint inside the daemon and are mixed, resampled, processed and controlled exactly like ALSA clients.

**Architecture:** A `src/pulse/` module listens on a Unix socket, speaks PulseAudio protocol 35, and translates each playback stream into an ordinary `aas_client::Client` stream over the daemon's existing socket. The PA wire codec, per-connection state machine and per-stream bridge are separate files. The audio thread, `Control`, the mixer, the resampler, plugins, bit-perfect and the state file are untouched except for small client-protocol additions.

**Tech Stack:** Rust edition 2024, `rust-version` 1.85. No new dependencies. Existing `aas-client`, `libc`, `rtrb`; tests use `tempfile` and the existing null-sink harness.

**Spec:** `doc/design.md`, "PulseAudio clients", and the sections it builds on: Clients, Volume, Bit-perfect, Configuration, API, Testing.

## Global Constraints

- No new dependencies; no `unsafe` beyond the small, documented syscall helpers already used for sockets.
- The audio thread allocates nothing, takes no locks and makes no blocking calls; every PulseAudio activity happens on its own thread.
- Every config key is optional; an unknown key or bad value stops the daemon at startup with a message naming the key.
- The PA socket is mode 0660 and group `audio`; `/run/aas/pulse` in production, derived from the client socket's directory.
- Protocol version 35 (PulseAudio 17); SHM, SRB and memfd are declined; the socket permission is the only authentication.
- Unsupported commands answer `PA_COMMAND_ERROR` + `PA_ERR_NOTSUPPORTED`; a malformed packet closes only its connection.
- Playback only; recording and source objects are out of scope.
- Public text (commits, docs, comments) names no tooling; commits follow the repository's lowercase `area: summary` style.

## Review Focus

These are the conditions most likely to hurt a player, and each has a test in the task that owns the code:

1. A client that writes half a packet and stalls — the connection thread must keep serving other streams and never block the daemon.
2. A memblock whose length is not a whole frame, or that names an unknown stream — dropped, never a panic or a partial write.
3. A `tlength` far above `pulse_buffer_ms`, or a rate/format outside the accepted set — a clean PA error, never a half-configured stream.
4. A player killed mid-stream — its `aas` connection is removed, the ring is freed, and other streams keep playing.
5. `pactl set-sink-volume` racing the API or ALSA — last write wins, events are not lost, and nothing deadlocks.

---

## File Structure

- Create `src/pulse/mod.rs` — module entry, socket bind, accept loop, lifecycle and shutdown.
- Create `src/pulse/packet.rs` — 5×u32 descriptor, big-endian framing, seek flags, incremental reader/writer.
- Create `src/pulse/tagstruct.rs` — PulseAudio tagstruct codec (strings, numbers, sample spec, channel map, cvolume, proplist, timeval).
- Create `src/pulse/wire.rs` — command numbers, error codes, subscription event constants, payload parsing/building for the implemented commands.
- Create `src/pulse/conn.rs` — one thread per PA connection: handshake, dispatch, introspection, subscriptions, output queue.
- Create `src/pulse/stream.rs` — one playback stream: `aas_client::Client`, flow control, timing, drain/cork/flush/delete.
- Create `src/pulse/volume.rs` — percent/cvolume and dB/cvolume conversion, mute state.
- Modify `crates/aas-client/src/proto.rs` — `Identify`, `GetClientVolume`, `SetClientVolume`, `GetOutputRate`, two reply variants.
- Modify `crates/aas-client/src/client.rs` — the three methods, notice queue, `poll_notice`, ring size helpers.
- Modify `crates/aas-client/src/convert.rs` — `ClientFormat::S24In32` for PA's `S24_32LE`.
- Modify `src/config.rs` — `pulse_socket`, `pulse_buffer_ms`.
- Modify `src/control.rs`, `src/control/api.rs` — identify, client volume, output rate, notices.
- Modify `src/server.rs` — reuse the socket group helper and start/stop the listener.
- Modify `src/lib.rs` — `pub mod pulse;`
- Create `tests/pulse.rs` — raw PulseAudio client and end-to-end tests.
- Create `dist/pulse-client.conf`, `dist/install-pulse.sh`, `dist/test-pulse.sh`.
- Modify `README.md`, `TODO.md`.

---

### Task 1: Client-protocol additions

**Files:**
- Modify: `crates/aas-client/src/proto.rs`
- Modify: `crates/aas-client/src/client.rs`
- Modify: `crates/aas-client/src/convert.rs`
- Modify: `src/control.rs`
- Modify: `src/control/api.rs`
- Test: inline modules in those files

**Interfaces:**
- Consumes: `Control::conns`, `Control::state.clients`, `patch_client` behavior, `Ring::free`.
- Produces:
  - `Request::Identify { pid: u32 }`
  - `Request::GetClientVolume`
  - `Request::SetClientVolume { db_millis: i32, muted: bool }`
  - `Request::GetOutputRate`
  - `Reply::ClientVolume { db_millis: i32, muted: bool }`
  - `Reply::ClientVolumeChanged { db_millis: i32, muted: bool }`
  - `Reply::OutputRate { rate: u32 }`
  - `Client::identify(pid: u32) -> io::Result<()>`
  - `Client::client_volume() -> io::Result<(i32, bool)>`
  - `Client::set_client_volume(db_millis: i32, muted: bool) -> io::Result<()>`
  - `Client::output_rate() -> io::Result<u32>`
  - `Client::volume() -> io::Result<u8>`, `set_volume(percent: u8)`, `subscribe_volume() -> io::Result<u8>`
  - `Client::poll_notice() -> io::Result<Option<Reply>>`
  - `Client::free_frames() -> io::Result<usize>`, `capacity_frames() -> io::Result<usize>`
  - `ClientFormat::S24In32`

- [ ] **Step 1: Write failing proto tests**

Append to the tests in `crates/aas-client/src/proto.rs`:

```rust
#[test]
fn identify_and_client_volume_round_trip() {
    let requests = [
        Request::Identify { pid: 4321 },
        Request::GetClientVolume,
        Request::SetClientVolume { db_millis: -6000, muted: true },
        Request::GetOutputRate,
    ];
    for r in requests {
        assert_eq!(decode_request(&encode_request(&r)), Some(r));
    }
}

#[test]
fn client_volume_replies_round_trip() {
    for (db, muted) in [(0, false), (-6000, true), (-60000, false), (-1, true)] {
        let r = Reply::ClientVolume { db_millis: db, muted };
        assert_eq!(decode_reply(&encode_reply(&r)), Some(r));
        let r = Reply::ClientVolumeChanged { db_millis: db, muted };
        assert_eq!(decode_reply(&encode_reply(&r)), Some(r));
    }
}

#[test]
fn a_client_volume_outside_the_range_does_not_decode() {
    let bad = Reply::ClientVolume { db_millis: -60001, muted: false };
    assert_eq!(decode_reply(&encode_reply(&bad)), None);
    let bad = Reply::ClientVolume { db_millis: 1, muted: false };
    assert_eq!(decode_reply(&encode_reply(&bad)), None);
}
```

- [ ] **Step 2: Run and see them fail**

Run: `cargo test -p aas-client proto::tests::identify_and_client_volume_round_trip`
Expected: compile error, the variants do not exist.

- [ ] **Step 3: Add the variants and encoding**

In `proto.rs` add the variants to `Request` and `Reply`, then extend `encode_request`, `decode_request`, `encode_reply`, `decode_reply`. Use tags 10 `Identify`, 11 `GetClientVolume`, 12 `SetClientVolume`, 13 `GetOutputRate`; replies 7 `ClientVolume`, 8 `ClientVolumeChanged`, 9 `OutputRate`. A client volume is `((db_millis + 60_000) as u32) | (muted << 17)`; decoding rejects any bit above 17 and any offset above 60 000:

```rust
const CLIENT_DB_MIN: i32 = -60_000;

fn client_volume_word(db_millis: i32, muted: bool) -> u32 {
    let db = db_millis.clamp(CLIENT_DB_MIN, 0) - CLIENT_DB_MIN;
    (db as u32) | (u32::from(muted) << 17)
}

fn client_volume_from_word(word: u32) -> Option<(i32, bool)> {
    if word & !((1 << 18) - 1) != 0 || (word & 0x1_ffff) > 60_000 {
        return None;
    }
    Some(((word & 0x1_ffff) as i32 + CLIENT_DB_MIN, word & (1 << 17) != 0))
}
```

`SetClientVolume` uses two words so its `db_millis` is an exact `i32`; decode rejects `muted > 1`.

- [ ] **Step 4: Write failing client and convert tests**

In `client.rs`, extend the fake-daemon test module with a script that answers `Hello`, then sends `ClientVolumeChanged` before the reply to `GetClientVolume`:

```rust
#[test]
fn a_client_volume_notice_does_not_steal_the_reply() {
    // fake daemon: hello, then on GetClientVolume:
    //   send ClientVolumeChanged { -6000, false }
    //   send ClientVolume { -6000, false }
    // assert client.client_volume() == (-6000, false)
    // assert client.poll_notice() == Some(ClientVolumeChanged { -6000, false })
}
```

In `convert.rs`:

```rust
#[test]
fn s24_in_32_reads_the_low_24_bits() {
    // 0x00123400 little endian -> left 24-bit sample 0x1234_0000
    let input = [0x00, 0x34, 0x12, 0x00];
    let out = one(ClientFormat::S24In32, 1, &input);
    assert_eq!(out, [[0x1234_0000, 0x1234_0000]]);
}
```

- [ ] **Step 5: Implement the client and conversion**

Add `S24In32` to `ClientFormat`, `sample_bytes()` 4, and `sample()`:

```rust
ClientFormat::S24In32 => i32::from_le_bytes([b[0], b[1], b[2], 0]) << 8,
```

Add a `notices: VecDeque<Reply>` field. In `exchange`, after each `receive`, stash `VolumeChanged` and `ClientVolumeChanged` and continue until a real reply. Add `poll_notice` using `receive_flags(libc::MSG_DONTWAIT)`. Add the request wrappers, `free_frames` (`stream.ring.free()`), and `capacity_frames` (`stream.ring.capacity()`).

- [ ] **Step 6: Write failing control tests**

In `src/control/api.rs` tests (or `control.rs` tests) add:

```rust
#[test]
fn identify_names_a_connection_from_its_pid() {
    // connect a control as "aas", send Identify { pid: std::process::id() },
    // assert the client name is this test process's comm, not "aas".
}

#[test]
fn a_client_volume_change_is_sent_to_its_connection() {
    // connect, Identify this process, SetClientVolume { -6000, false },
    // assert take_notices() has ClientVolumeChanged { -6000, false },
    // and GetClientVolume returns the same value.
}
```

- [ ] **Step 7: Implement the control side**

In `handle_request` add arms after the `hello` check:

```rust
Request::Identify { pid } => self.identify(conn, pid),
Request::GetClientVolume => self.client_volume_reply(conn),
Request::SetClientVolume { db_millis, muted } => self.set_client_volume(conn, db_millis, muted, now),
Request::GetOutputRate => Answer::reply(Reply::OutputRate { rate: self.cycle_rate().unwrap_or(0) }),
```

`identify` reads `/proc/<pid>/comm` (the same trim `peer_name` uses), refuses a stream or recorder already on the connection with `EINVAL`, updates `Conn::name`/`label`, and publishes the client change. `client_volume_reply` packs the stored `StoredInput` for the connection's name (unity when absent). `set_client_volume` converts to `InputPatch`, calls the shared `set_client_input` helper extracted from `patch_client`, and returns `Ok`, `EINVAL` for an out-of-range dB, or `EBUSY` when 64 client volumes are already stored. After any change to a client's input, queue `Reply::ClientVolumeChanged` for every connection whose name matches, gathered into a `Vec<ConnId>` first.

- [ ] **Step 8: Run everything and commit**

Run: `cargo test -p aas-client && cargo test control:: && cargo clippy -p aas-client --all-targets`
Expected: PASS.
Commit:
```sh
git add crates/aas-client src/control.rs src/control/api.rs
git commit -m "feat: name and volume for a client of the daemon"
```

---

### Task 2: Configuration keys

**Files:**
- Modify: `src/config.rs`
- Modify: `README.md` (table row only; the prose comes in Task 9)
- Test: `src/config.rs` tests

**Interfaces:**
- Consumes: `Config`, `Config::default()`.
- Produces:
  - `Config::pulse_socket: Option<PathBuf>` — `None` default path, `Some(empty)` off, `Some(path)` that path.
  - `Config::pulse_buffer_ms: Option<u16>` — `None` means 200.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn pulse_keys_have_their_defaults() {
    let c = load_str("").unwrap();
    assert_eq!(c.pulse_socket, None);
    assert_eq!(c.pulse_buffer_ms, None);
}

#[test]
fn an_empty_pulse_socket_turns_the_endpoint_off() {
    assert_eq!(load_str("pulse_socket = \"\"\n").unwrap().pulse_socket, Some(PathBuf::new()));
}

#[test]
fn pulse_buffer_ms_is_20_to_2000() {
    assert!(load_str("pulse_buffer_ms = 20\n").is_ok());
    assert!(load_str("pulse_buffer_ms = 2000\n").is_ok());
    assert!(load_str("pulse_buffer_ms = 19\n").is_err());
    assert!(load_str("pulse_buffer_ms = 2001\n").is_err());
    assert!(load_str("pulse_buffer_ms = \"200\"\n").is_err());
}

#[test]
fn a_pulse_buffer_with_the_endpoint_off_is_an_error() {
    let e = err("pulse_socket = \"\"\npulse_buffer_ms = 200\n");
    assert!(e.contains("pulse_buffer_ms") && e.contains("off"), "{e}");
}
```

- [ ] **Step 2: Run and see them fail**

Run: `cargo test config::tests::pulse_keys_have_their_defaults`
Expected: compile error.

- [ ] **Step 3: Add the fields and parsing**

Add the fields to `Config` and `Raw`, default them in `Config::default()`, parse in `Config::from_raw` with the error messages the tests name, and add `if pulse_socket == Some(PathBuf::new()) && pulse_buffer_ms.is_some() { return Err(...) }`.

- [ ] **Step 4: Run and commit**

Run: `cargo test config::`
Expected: PASS.
Commit: `git add src/config.rs README.md && git commit -m "feat: the PulseAudio socket and buffer keys"`

---

### Task 3: Packet framing and tagstruct codec

**Files:**
- Create: `src/pulse/mod.rs`
- Create: `src/pulse/packet.rs`
- Create: `src/pulse/tagstruct.rs`
- Modify: `src/lib.rs`
- Test: inline modules in both files

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `packet::Frame { channel: u32, payload: Vec<u8>, memblock: Option<Memblock> }` where `Memblock { stream: u32, offset: i64, seek: Seek, data: Vec<u8> }`
  - `packet::Encoder::packet(&self, channel: u32, payload: &[u8]) -> Vec<u8>` and `packet::Encoder::memblock(&self, stream: u32, data: &[u8]) -> Vec<u8>`
  - `packet::Reader::read(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, Error>` (buffers partial input)
  - `tagstruct::Builder` and `tagstruct::Reader<'a>` with `string`, `u8`, `u32`, `u64`, `i64`, `boolean`, `sample_spec`, `channel_map`, `cvolume`, `proplist`, `usec`, `timeval`, `arbitrary`.

- [ ] **Step 1: Write failing packet tests**

```rust
#[test]
fn a_packet_split_across_reads_is_one_frame() {
    let bytes = Encoder.packet(0xffff_ffff, &[1, 2, 3]);
    let mut reader = Reader::new();
    assert!(reader.read(&bytes[..7]).unwrap().is_empty());
    let frames = reader.read(&bytes[7..]).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].channel, 0xffff_ffff);
    assert_eq!(frames[0].payload, [1, 2, 3]);
}

#[test]
fn a_memblock_carries_its_stream_and_seek() {
    let bytes = Encoder.memblock(7, &[9, 8]);
    let frames = Reader::new().read(&bytes).unwrap();
    let m = frames[0].memblock.as_ref().unwrap();
    assert_eq!((m.stream, m.seek, m.data.as_slice()), (7, Seek::Relative, &[9, 8][..]));
}

#[test]
fn an_oversized_length_is_refused() {
    let mut bytes = Encoder.packet(0xffff_ffff, &[]);
    bytes[..4].copy_from_slice(&(16 * 1024 * 1024u32).to_be_bytes());
    assert!(Reader::new().read(&bytes).is_err());
}
```

- [ ] **Step 2: Write failing tagstruct tests**

```rust
#[test]
fn every_tag_round_trips() {
    let mut b = Builder::new();
    b.string("aas"); b.u8(3); b.u32(48_000); b.u64(9); b.i64(-2);
    b.boolean(true); b.sample_spec(SampleSpec { format: 3, channels: 2, rate: 48_000 });
    b.channel_map(&[1, 2]);
    b.cvolume(&[65_536, 32_768]);
    b.proplist(&[("application.name".into(), "mpv".into())]);
    let bytes = b.into_vec();
    let mut r = Reader::new(&bytes);
    assert_eq!(r.string(), Some("aas"));
    assert_eq!(r.u8(), Some(3));
    // ... every field, then assert r.eof()
}

#[test]
fn reading_past_the_end_is_none() {
    let mut r = Reader::new(&[b'L', 0, 0]);
    assert_eq!(r.u32(), None);
}
```

- [ ] **Step 3: Implement**

All wire integers are big-endian (`to_be_bytes`). The descriptor is `[length, channel, offset_hi, offset_lo, flags]`; a packet has `channel == 0xffff_ffff` and flags 0, a memblock has `seek = flags & 0xff` and no SHM flags accepted (`flags & 0xff00_0000 != 0` is an error). `Reader` keeps a `Vec<u8>` buffer and only consumes a complete `20 + length` frame. Tag characters are the PulseAudio ones: `t` string, `N` null string, `L` u32, `B` u8, `R` u64, `r` i64, `a` sample spec, `x` arbitrary, `1`/`0` booleans, `T` timeval, `U` usec, `m` channel map, `v` cvolume, `V` single volume, `P` proplist, `f` format info. The reader must reject a tag byte it does not expect.

- [ ] **Step 4: Run and commit**

Run: `cargo test pulse::`
Expected: PASS.
Commit: `git add src/lib.rs src/pulse && git commit -m "feat: the PulseAudio wire format"`

---

### Task 4: Listener, handshake and lifecycle

**Files:**
- Create: `src/pulse/wire.rs` (constants and helpers)
- Create: `src/pulse/conn.rs`
- Modify: `src/pulse/mod.rs`
- Modify: `src/server.rs` (`set_audio_group` becomes `pub(crate)`)
- Test: `tests/pulse.rs`, plus unit tests in `mod.rs`

**Interfaces:**
- Consumes: `Config::pulse_socket`, `Config::pulse_buffer_ms`, `aas_client::proto::default_socket_path()`, `server::set_audio_group`.
- Produces:
  - `pulse::spawn(config: &Config, daemon_socket: &Path, shutdown: Arc<AtomicBool>) -> io::Result<Option<Pulse>>`
  - `Pulse { path: PathBuf }` with `Pulse::stop(self)` joining the listener and its connections.
  - `wire::PROTOCOL_VERSION`, command and error constants, `wire::reply(tag, payload)`, `wire::error(tag, code, message)`.

- [ ] **Step 1: Write the failing integration test**

`tests/pulse.rs` gets `mod common;` and a raw client helper:

```rust
struct RawPulse { stream: UnixStream }

impl RawPulse {
    fn connect(path: &Path) -> RawPulse { /* UnixStream::connect */ }
    fn send(&mut self, channel: u32, payload: &[u8]) { /* 20-byte BE header + payload */ }
    fn recv(&mut self) -> (u32, Vec<u8>) { /* header + payload */ }
    fn auth(&mut self) { /* AUTH 35 + 256 zero cookie, expect REPLY 35 */ }
    fn set_name(&mut self, name: &str) { /* SET_CLIENT_NAME, expect REPLY */ }
    fn command(&mut self, tag: u32, payload: &[u8]) -> (u32, Vec<u8>) { /* send, recv */ }
}

#[test]
fn a_pulse_client_authenticates_and_is_refused_before_it_does() {
    // start a Daemon, connect RawPulse, send GET_SERVER_INFO before AUTH,
    // expect ERROR + PA_ERR_ACCESS, then AUTH and SET_CLIENT_NAME succeed.
}

#[test]
fn the_pulse_socket_is_group_audio() {
    // assert mode 0660 on daemon.pulse_socket()
}
```

Add `Daemon::pulse_socket(&self) -> PathBuf` to `tests/common/mod.rs`, returning the client socket's directory joined with `pulse`.

- [ ] **Step 2: Run and see it fail**

Run: `cargo test --test pulse`
Expected: compile error, `pulse::spawn` does not exist.

- [ ] **Step 3: Implement the listener**

`spawn` derives the path (`config.pulse_socket` or `daemon_socket.parent().join("pulse")`; `Some(empty)` returns `Ok(None)`), binds a `UnixListener` to `<path>.<pid>`, sets 0660 and `server::set_audio_group`, renames over the old path, sets non-blocking, and spawns `aas-pulse` with a 200 ms accept loop that checks `shutdown`. Each accepted `UnixStream` gets a `thread::Builder` named `aas-pulse-conn` and `conn::serve`.

`conn::serve` has a `Conn { stream, out: Vec<u8>, authorized: bool, version: u32, client_index: u32, name: String, proplist: Vec<(String,String)>, streams: Vec<Stream> }`. It reads with `recv` into the `packet::Reader`, writes queued bytes with `send` handling partial writes, and returns when the socket closes or `shutdown` is set.

`wire.rs` holds the command numbers from PulseAudio 17's `native-common.h`: `AUTH = 8`, `SET_CLIENT_NAME = 9`, `EXIT = 7`, `REPLY = 2`, `ERROR = 0`, `GET_SERVER_INFO = 20`, `GET_SINK_INFO = 21`, `GET_SINK_INFO_LIST = 22`, `GET_SINK_INPUT_INFO = 29`, `GET_SINK_INPUT_INFO_LIST = 30`, `GET_CLIENT_INFO = 27`, `GET_CLIENT_INFO_LIST = 28`, `GET_CARD_INFO_LIST = 89`, `GET_MODULE_INFO_LIST = 26`, `GET_SAMPLE_INFO_LIST = 34`, `GET_SOURCE_INFO_LIST = 24`, `STAT = 13`, `LOOKUP_SINK = 10`, `SUBSCRIBE = 35`, `CREATE_PLAYBACK_STREAM = 3`, `DELETE_PLAYBACK_STREAM = 4`, `DRAIN_PLAYBACK_STREAM = 12`, `CORK_PLAYBACK_STREAM = 41`, `FLUSH_PLAYBACK_STREAM = 42`, `TRIGGER_PLAYBACK_STREAM = 43`, `PREBUF_PLAYBACK_STREAM = 60`, `SET_PLAYBACK_STREAM_BUFFER_ATTR = 72`, `SET_PLAYBACK_STREAM_NAME = 46`, `UPDATE_PLAYBACK_STREAM_PROPLIST = 81`, `REMOVE_PLAYBACK_STREAM_PROPLIST = 84`, `GET_PLAYBACK_LATENCY = 14`, `SET_SINK_VOLUME = 36`, `SET_SINK_MUTE = 39`, `SET_SINK_INPUT_VOLUME = 37`, `SET_SINK_INPUT_MUTE = 69`, `SET_DEFAULT_SINK = 44`, `REQUEST = 61`, `UNDERFLOW = 63`, `PLAYBACK_STREAM_KILLED = 64`, `SUBSCRIBE_EVENT = 66`, `STARTED = 86`, `PLAYBACK_BUFFER_ATTR_CHANGED = 94`. Errors: `PA_ERR_ACCESS = 1`, `PA_ERR_COMMAND = 2`, `PA_ERR_INVALID = 3`, `PA_ERR_NOENTITY = 5`, `PA_ERR_PROTOCOL = 6`, `PA_ERR_NOTSUPPORTED = 18`.

- [ ] **Step 4: Implement the handshake**

`AUTH` payload is `u32 version` followed by 256 cookie bytes; accept any cookie, mask the version with `0xffff`, reply `REPLY` with `35`. `SET_CLIENT_NAME` payload is a string and, from version 13, a proplist; reply `REPLY` with the client index from version 13. `EXIT` closes. Every other command before `AUTH` gets `PA_ERR_ACCESS`; a bad payload gets `PA_ERR_PROTOCOL` and a close.

- [ ] **Step 5: Wire it into the daemon and run**

In `server::run_outputs`, after the listener is bound and `Control` is built, call `pulse::spawn(&config, socket, shutdown.clone())`. Keep the returned `Pulse` beside the `Server`; after the control loop, `stop()` it before dropping the server. A spawn error is logged as `aas: pulse: <error>, playing on without it`.

Modify `tests/common/mod.rs` so its daemon resolves the same socket path.

Run: `cargo test --test pulse && cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```sh
git add src/pulse src/server.rs src/lib.rs tests/
git commit -m "feat: a PulseAudio socket and handshake"
```

---

### Task 5: Introspection and subscriptions

**Files:**
- Create: `src/pulse/objects.rs`
- Modify: `src/pulse/conn.rs`, `src/pulse/wire.rs`
- Test: `tests/pulse.rs`

**Interfaces:**
- Consumes: `Client::volume()`, `Client::output_rate()`, the connection's `client_index` and streams.
- Produces:
  - `objects::server_info(rate, cookie) -> Vec<u8>`
  - `objects::sink_info(index, rate, volume, mute, latency_usec, name, description) -> Vec<u8>`
  - `objects::sink_input_info(stream, client_index, volume, mute, latency_usec) -> Vec<u8>`
  - `objects::client_info(index, name, proplist) -> Vec<u8>`

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn pactl_style_introspection_sees_one_sink() {
    // connect, auth, name, then:
    // GET_SERVER_INFO -> name "aas", version, default sink "aas", no default source
    // GET_SINK_INFO_LIST -> exactly one sink named "aas", stereo, volume 100%
    // GET_SINK_INPUT_INFO_LIST -> empty before a stream
    // GET_CARD_INFO_LIST and GET_MODULE_INFO_LIST -> empty replies
}

#[test]
fn subscribe_answers_then_sends_events() {
    // SUBSCRIBE with SINK|SINK_INPUT|CLIENT returns REPLY; then a volume
    // change causes a SUBSCRIBE_EVENT packet with the sink facility.
}
```

- [ ] **Step 2: Run and see them fail**

Run: `cargo test --test pulse introspection`
Expected: FAIL, no replies or wrong payloads.

- [ ] **Step 3: Implement the reply layouts exactly**

Field order is what PulseAudio 17 parses:

- Server info: string `aas`, string `0.1.0`, string user, string host, sample spec (s32le=7, 2, rate), string default sink `aas`, null string default source, u32 cookie, channel map `[1, 2]`.
- Sink info: u32 index, string name, string description, sample spec, channel map, u32 module `0xffff_ffff`, cvolume (2 channels), boolean mute, u32 monitor source `0xffff_ffff`, null string monitor name, usec latency, string driver `aas`, u32 flags 0, proplist (device.description), usec requested latency 0, u32 base volume `0x10000`, u32 state (0 running, 1 idle, 2 suspended), u32 volume steps 100, u32 card `0xffff_ffff`, u32 ports 0, null string active port, u8 formats 0.
- Sink input info: u32 index, string media name, u32 module `0xffff_ffff`, u32 client index, u32 sink 0, sample spec, channel map, cvolume, usec stream latency, usec sink latency, string resample method `null`, string driver `aas`, boolean mute, proplist, boolean corked, boolean has volume true, boolean volume writable true, format info (u8 encoding 0 + empty proplist).
- Client info: u32 index, string application name, u32 module `0xffff_ffff`, string driver `aas`, proplist.
- `LOOKUP_SINK`: u32 index + string name; accept index 0 or name `aas`, answer `REPLY` with u32 0, else `PA_ERR_NOENTITY`.
- `STAT`: five u32 zeros.
- Lists are one `REPLY` containing every object in sequence; empty lists are an empty `REPLY`.
- `SUBSCRIBE`: validate the mask against `0x02ff`, store it, answer `REPLY`; every stored mask determines which `SUBSCRIBE_EVENT` packets go out. An event packet is not a reply: command `SUBSCRIBE_EVENT`, tag `0xffff_ffff`, then u32 event type and u32 index.

- [ ] **Step 4: Run and commit**

Run: `cargo test --test pulse`
Expected: PASS.
Commit: `git add src/pulse tests/pulse.rs && git commit -m "feat: PulseAudio introspection and events"`

---

### Task 6: Playback streams and flow control

**Files:**
- Create: `src/pulse/stream.rs`
- Modify: `src/pulse/conn.rs`, `src/pulse/wire.rs`
- Test: `tests/pulse.rs`

**Interfaces:**
- Consumes: `aas_client::Client::{identify, configure, start, stop, reset, start_drain, poll_drain, write, delay_frames, hw_position, underruns, free_frames, capacity_frames, poll_fds}`, `ClientFormat`, `wire::SampleSpec`, `wire::BufferAttr`.
- Produces:
  - `Stream { index, client, ss, map, name, buffer, corked, write_index, read_base, underruns, drain_tag }`
  - `Stream::create(...) -> io::Result<Stream>`
  - `Stream::handle(&mut self, frame) -> Result<Vec<Vec<u8>>, StreamGone>` — returns reply/request packets to send.
  - `Stream::request(&mut self) -> Option<Vec<u8>>`
  - `Stream::latency(&self) -> Latency`
  - `Stream::flush`, `cork`, `drain_reply`, `tick`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn create_playback_stream_returns_a_ring_and_asks_for_data() {
    // auth, name, CREATE_PLAYBACK_STREAM with S16LE stereo 48k, tlength 200ms,
    // expect REPLY fields: stream 0, sink input 0, missing > 0, buffers,
    // sample spec, channel map, sink 0 "aas", suspended false, latency.
    // Then expect a REQUEST for the missing bytes.
}

#[test]
fn written_memblock_plays_and_positions_advance() {
    // answer each REQUEST with zero-filled memblocks for 1 second;
    // GET_PLAYBACK_LATENCY shows read_index advancing and sink_usec > 0.
}

#[test]
fn a_misaligned_memblock_is_ignored() {
    // send 3 bytes to a stereo S16 stream; latency positions do not move.
}

#[test]
fn delete_stops_the_stream_and_another_keeps_playing() {
    // two streams, delete one, the other still answers GET_PLAYBACK_LATENCY.
}
```

- [ ] **Step 2: Parse `CREATE_PLAYBACK_STREAM` exactly**

After the sample spec, channel map, sink index, sink name, there are: u32 maxlength, boolean corked, u32 tlength, u32 prebuf, u32 minreq, u32 syncid, cvolume, seven booleans (no_remap, no_remix, fix_format, fix_rate, fix_channels, no_move, variable_rate), boolean muted, boolean adjust_latency, proplist, boolean volume_set, boolean early_requests, boolean muted_set, boolean dont_inhibit_auto_suspend, boolean fail_on_suspend, boolean relative_volume, boolean passthrough, then u8 n_formats followed by format infos. Refuse any `n_formats` other than 0.

- [ ] **Step 3: Negotiate and create the daemon stream**

Map formats: `S16LE 3 -> S16`, `FLOAT32LE 5 -> Float`, `S32LE 7 -> S32`, `S24LE 9 -> S24Packed`, `S24_32LE 11 -> S24In32`. Accept 1–2 channels and 8 000–384 000 Hz. Compute `frames = (tlength_bytes / frame_bytes)`, or `rate * pulse_buffer_ms / 1000` when `tlength` is 0, clamped to `[rate * 20 / 1000, rate * pulse_buffer_ms.max(20) / 1000]`. Open the client, `identify(pid)`, `configure(rate, format, channels, frames)`, then `start()` unless corked. Reply with: u32 stream index, u32 sink-input index, u32 missing (first request size), u32 maxlength, u32 tlength (ring frames × frame bytes), u32 prebuf 0, u32 minreq 0, sample spec, channel map, u32 sink 0, string `aas`, boolean suspended false, usec configured latency, format info.

- [ ] **Step 4: Flow control and timing**

`request()` computes `min(free_frames * frame_bytes, 65_536)`, rounded down to a whole frame, and returns a `REQUEST` packet (tag `0xffff_ffff`, u32 stream, u32 length). Keep at most one request outstanding; after a memblock is written, ask again. A memblock for an unknown stream or with `len % frame_bytes != 0` is dropped. `write_index += data.len()`. `read_index = read_base + hw_position() * frame_bytes`. `sink_usec = delay_frames() * 1_000_000 / rate`. Send `UNDERFLOW` when `underruns()` grows. A stream whose `poll_fds` socket hangs up sends `PLAYBACK_STREAM_KILLED` and is removed.

`CORK` -> `stop()` or `start()` and remember the state. `FLUSH` -> `reset()`, zero `read_base` to the current write index, and restart if not corked. `DRAIN` -> `start_drain()` and remember the tag; `tick()` calls `poll_drain()` and answers `REPLY` when it returns true. `DELETE` -> `release()` and remove.

- [ ] **Step 5: Run and commit**

Run: `cargo test --test pulse`
Expected: PASS.
Commit: `git add src/pulse tests/pulse.rs && git commit -m "feat: PulseAudio playback streams"`

---

### Task 7: Volume and mute

**Files:**
- Create: `src/pulse/volume.rs`
- Modify: `src/pulse/conn.rs`, `src/pulse/stream.rs`
- Test: `tests/pulse.rs`, inline tests in `volume.rs`

**Interfaces:**
- Consumes: `Client::{volume, set_volume, subscribe_volume, client_volume, set_client_volume, poll_notice}`, `ClientVolumeChanged`.
- Produces:
  - `volume::percent(cvolume: &[u32]) -> u8`
  - `volume::cvolume(percent: u8) -> [u32; 2]`
  - `volume::db_millis(cvolume: &[u32]) -> i32`
  - `volume::cvolume_from_db(db_millis: i32) -> [u32; 2]`
  - `Conn::sink_mute: Option<u8>` (remembered percent)

- [ ] **Step 1: Write failing conversion tests**

```rust
#[test]
fn a_cvolume_reports_the_aas_percent() {
    assert_eq!(percent(&[65_536, 65_536]), 100);
    assert_eq!(percent(&[32_768, 32_768]), 50);
    assert_eq!(percent(&[0, 0]), 0);
}

#[test]
fn a_stream_volume_becomes_db() {
    assert_eq!(db_millis(&[65_536, 65_536]), 0);
    assert_eq!(db_millis(&[32_768, 32_768]), -6020);
    assert_eq!(db_millis(&[0, 0]), -60_000);
}
```

- [ ] **Step 2: Write failing end-to-end tests**

```rust
#[test]
fn sink_volume_and_the_api_are_one_number() {
    // SET_SINK_VOLUME 40%, then GET_SINK_INFO shows 40 and GET /outputs/1 shows 40.
}

#[test]
fn stream_volume_is_the_client_gain() {
    // SET_SINK_INPUT_VOLUME 50%, then GET /inputs/clients/{name} shows ~-6 dB.
}

#[test]
fn mute_sets_zero_and_restores_the_remembered_percent() {
    // SET_SINK_VOLUME 40, SET_SINK_MUTE true -> 0, SET_SINK_MUTE false -> 40.
}

#[test]
fn a_volume_change_made_elsewhere_reaches_a_subscriber() {
    // SUBSCRIBE, then PATCH /outputs/1 {"volume": 25}; expect a sink CHANGE event.
}
```

- [ ] **Step 3: Implement the conversion and commands**

`percent` is `round(avg(channel) / 65536 * 100)`; `cvolume` sets both channels to `percent * 65536 / 100` rounded. `db_millis` is `20 log10(avg / 65536)` in millidB, clamped to `-60_000..=0`; `cvolume_from_db` is the inverse. The connection owns one control `aas_client::Client` opened at `AUTH`, subscribes with `subscribe_volume()`, and is polled in the read loop.

`SET_SINK_VOLUME` stores the percent through `set_volume`; `SET_SINK_MUTE` implements the remembered-percent rule. `SET_SINK_INPUT_VOLUME`/`SET_SINK_INPUT_MUTE` call the stream's `set_client_volume`. `GET_SINK_INFO` reads `volume()` fresh; `GET_SINK_INPUT_INFO` reads `client_volume()` fresh. `poll_notice` results become `SUBSCRIBE_EVENT` packets for the masks the client asked for.

- [ ] **Step 4: Run and commit**

Run: `cargo test --test pulse && cargo test pulse::volume`
Expected: PASS.
Commit: `git add src/pulse tests/pulse.rs && git commit -m "feat: PulseAudio volume maps onto aas"`

---

### Task 8: Robustness and coexistence

**Files:**
- Modify: `src/pulse/conn.rs`, `src/pulse/stream.rs`
- Test: `tests/pulse.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: no new API; a connection that fails never takes down the listener or another connection.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn a_malformed_packet_closes_only_its_connection() {
    // one RawPulse sends a length of 0xffff_ffff; it is closed;
    // a second RawPulse still answers GET_SERVER_INFO.
}

#[test]
fn an_unknown_command_is_a_not_supported_error() {
    // command 104 (SEND_OBJECT_MESSAGE) -> ERROR + 18, connection still usable.
}

#[test]
fn a_pulse_and_an_alsa_client_play_together() {
    // create a common::Daemon, an aas_client::Client, and a RawPulse stream;
    // write to both; GET /state lists both clients; deleting the Pulse
    // stream leaves the ALSA stream running.
}

#[test]
fn a_client_killed_mid_stream_does_not_block_the_daemon() {
    // drop the RawPulse socket without DELETE; another Pulse client plays.
}
```

- [ ] **Step 2: Implement the guards**

Cap a connection's output queue (1 MiB); a peer that stops reading is closed. Reject a frame length above 16 MiB. Poll each connection socket with a 200 ms timeout and check the shared `shutdown`. Catch a panic in a connection thread with `std::panic::catch_unwind` and log it, so one malformed packet cannot stop the listener. Never hold a lock across a blocking socket send (there is no shared mutable state between connection threads).

- [ ] **Step 3: Run and commit**

Run: `cargo test --test pulse && cargo test`
Expected: PASS.
Commit: `git add src/pulse tests/pulse.rs && git commit -m "fix: PulseAudio connections fail alone"`

---

### Task 9: Distribution, documentation and board check

**Files:**
- Create: `dist/pulse-client.conf`
- Create: `dist/install-pulse.sh`
- Create: `dist/test-pulse.sh`
- Modify: `README.md`, `TODO.md`
- Test: shell syntax and the documented commands

- [ ] **Step 1: Add the installed client configuration**

`dist/pulse-client.conf`:

```ini
# aas serves the PulseAudio native protocol on this socket.
default-server = unix:/run/aas/pulse
autospawn = no
```

`dist/install-pulse.sh` installs it as `/etc/pulse/client.conf`, keeping an existing different file as `/etc/pulse/client.conf.before-aas`, and is idempotent.

- [ ] **Step 2: Add the board check**

`dist/test-pulse.sh` runs `pactl info`, proves the sink is `aas`, plays a generated WAV with `paplay`, and checks `GET /outputs/1` moved after `pactl set-sink-volume`. It exits 2 with a usage line if `pactl`, `paplay` or `mpv` is missing, mirroring `dist/test-plugin.sh`.

- [ ] **Step 3: Document**

README: add `src/pulse/` to Parts, add "PulseAudio clients" with the `/etc/pulse/client.conf` install step and the `pactl`/`paplay`/`mpv` checks, and add `pulseaudio-utils` and `mpv` to the board test dependencies. The two config rows were added in Task 2.

`TODO.md`: park recording and `parec`, native `libpipewire` clients, per-channel volume, and SHM/SRB/memfd.

- [ ] **Step 4: Run the full check and commit**

Run: `sh -n dist/install-pulse.sh dist/test-pulse.sh && cargo test && cargo clippy --all-targets`
Expected: PASS.
Commit:
```sh
git add dist README.md TODO.md
git commit -m "docs: install and test the PulseAudio endpoint"
```

---

## Self-Review

- Every spec section maps to a task: discovery/access (4), command subset (4–7), streams (6), objects/events (5, 7), volume (7), configuration and failure (2, 4, 8), client-protocol additions (1), later work (9).
- No placeholders remain; each task names its files, interfaces, failing test and verification command.
- Types used later are defined earlier: `Client` methods in Task 1, `Frame` in Task 3, `Stream` in Task 6, volume helpers in Task 7.
- The review-focus conditions have tests in Tasks 3, 5, 6 and 8; the volume race is covered by Task 7's last test.
