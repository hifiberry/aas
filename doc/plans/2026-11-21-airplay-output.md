# AirPlay output Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `aas` plays what its master plays to AirPlay receivers named in the
config file, as AirPlay 1 or AirPlay 2. It connects when there is a signal and
disconnects after a configurable silence, or stays connected.

**Architecture:** Each `[[airplay]]` table becomes a thread inside `aas`. The
thread reads the master monitor through an internal tap, made the way a
PulseAudio recorder's is (`Ring::create`, `Command::AddTap` with `monitor =
true`, `source = 0`): what the master card gets, after the mix, the plugins, the
master gain and the volume limit, in real time and also while the card is
parked.
- A gate on those samples decides when to connect and when to disconnect.
- The thread resamples to 44.1 kHz with an `Adaptive`. A `Drift` keeps the
  path's delay on target against the system clock, which paces the packets:
  the same controller a further output uses against its card.
- The protocol layer opens the session, sends the RTP packets, and answers
  timing and retransmit requests.
- AirPlay output is not a further output. It does not take part in
  `align_frames`, so the receivers' latency of a second or more never delays
  the master or the further outputs.

**Tech Stack:** Rust; `std::net` (blocking TCP/UDP with timeouts, on the
output's own thread); the existing `rubato`-based `Adaptive`, `Drift` and
`aas_client::Ring`. AirPlay 2 adds `srp`, `sha2` and `num-bigint` (Task 8),
`hkdf` and `chacha20poly1305` (Task 9) and `plist` (Task 10): all MIT or
Apache-2.0, and so compatible with `aas`'s MIT.

**Spec:** the owner's requirements of 2026-11-21:
- an internal AirPlay sender with AirPlay 1 and AirPlay 2 as options;
- the receiver fixed in the config file;
- no signal disconnects and a signal connects, configurable.

Evidence:
- `doc/pipewire-device.md`;
- a device under PipeWire that plays to one AirPlay receiver, read 2026-11-21: the receiver advertises `et=0,4`, `cn=0,1`, `tp=UDP`, port 7000, and a helper script pauses the stream after a silence;
- the reference implementation, PipeWire's `src/modules/module-raop-sink.c` (MIT, commit `ae12134`), which has both the legacy path and the AirPlay 2 path.

No Rust crate fits: the senders are GPL-3.0 (`cidermill-airplay`,
`rust-airplay`) or AGPL (`airplay-rs`). `tvpipe` (Apache-2.0) is a mirroring
sender with video, and was two days old when checked.

## Global Constraints

- Config: up to 4 `[[airplay]]` tables. Keys and checks:

  | Key | Values | Default |
  |---|---|---|
  | `host` | name or address, required | |
  | `port` | 1–65535 | 7000 |
  | `name` | string, unique across the tables | `host` |
  | `protocol` | `"airplay1"`, `"airplay2"` | `"airplay1"` |
  | `connect` | `"signal"`, `"always"` | `"signal"` |
  | `threshold_dbfs` | -90 to 0 | -50 |
  | `idle_s` | 1 to 86400, whole seconds | 120 |
  | `latency_ms` | 250 to 5000 | 1500 |
  | `volume_db` | -30 to 0, or `"off"` (never sent) | 0 |

  - The error for each check is `airplay {n}: <key> must be …`.
  - The defaults of `threshold_dbfs` and `idle_s` are those of the turntable
    box's record detection and its pause helper.
- Wire format for AirPlay 1, all of it from PipeWire's legacy path:
  - 44100 Hz, stereo, 352 frames per packet;
  - SDP `AppleLossless` with `a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100`;
  - the payload is an ALAC uncompressed escape frame of 16-bit big-endian
    samples, 4n+8 bytes;
  - RTP payload type 96 (0x60), with the marker bit on the first packet after
    RECORD only.
- `POST /auth-setup` is always sent after OPTIONS, with PipeWire's 33-byte body.
  - A 200 goes on.
  - Any other status but 401 is logged once per receiver and the session goes
    on without it, because a receiver without `et=4` may refuse it.
  - A 401 ends the attempt.
- Sync packet every 121 packets and before the first one. Timestamps are the
  NTP time of `CLOCK_REALTIME`, read once at connect and then advanced by the
  monotonic clock, so a step of the wall clock does not move the stream.
- Retransmit history: the last 512 packets (about 4 s). A resend is the data
  packet itself, sent again on the data socket, as PipeWire does it.
- Disconnect is `TEARDOWN` and then closing the TCP connection. A handshake has
  15 s to reach RECORD.
- Failure with a signal present, or with `connect = "always"`: retry after
  1, 2, 4, 8, 16, then every 30 s.
- Errors go to stderr as `aas: airplay "<name>": …`, the daemon's style.
- No new thread at real-time priority: the output's thread runs at normal
  priority and never blocks the audio thread, because the tap drops what does
  not fit.
- Test first. Each new test is seen failing, and each production line is
  checked by removing it.

## Review Focus

1. **A receiver that refuses `/auth-setup`** (a Mac, a shairport-sync
   receiver) still plays. Task 4 pins it with a fake receiver answering 404.
2. **A receiver that goes away mid-stream** (power off, TCP reset): the thread
   notices within 15 s and retries with backoff. It never blocks shutdown or
   the audio thread. Tasks 4 and 6 pin it with a fake receiver that closes its
   socket.
3. **A host name that does not resolve** (`soundbar.local` without mDNS, a
   typo): it counts as a failed connect and is retried, never a start error.
   Task 6 pins it with `host = "nonexistent.invalid"`.
4. **The master changes rate** (a 96 kHz client): the tap's rate changes, and
   the resampler and drift start over without a reconnect. The RTP timeline
   continues, at most one packet of silence. Task 5 pins it with a rate change
   mid-stream.
5. **Silence of exact zeros while connected for less than `idle_s`, then a
   signal again**: no reconnect, and the stream never stopped. Task 3 pins the
   gate, Task 6 the session.

---

## Part 1: the shared base and AirPlay 1

### Task 1: The `[[airplay]]` config

**Files:**
- Modify: `src/config.rs` (the `Raw` struct, `Config`, the checks, tests)
- Modify: `README.md` (config table, new section "AirPlay")

**Interfaces:**
- Produces:
  - `pub struct AirplayConfig { pub name: String, pub host: String, pub port: u16, pub protocol: AirplayProtocol, pub connect: Connect, pub threshold_dbfs: f32, pub idle_s: u32, pub latency_ms: u32, pub volume_db: Option<f32> }`
  - `pub enum AirplayProtocol { AirPlay1, AirPlay2 }`
  - `pub enum Connect { Signal, Always }`
  - `Config.airplay: Vec<AirplayConfig>`

- [ ] **Step 1: Write failing tests** in `config.rs`:
  - `airplay_tables_are_loaded_with_their_defaults`: `host = "soundbar.local"` alone gives `port 7000`, `name "soundbar.local"`, `AirPlay1`, `Signal`, `-50.0`, `120`, `1500`, `Some(0.0)`.
  - `airplay_keys_are_checked`: one case per key out of range, each asserting its exact error.
  - `volume_db = "off"` gives `None`.
  - five tables give `"at most 4 [[airplay]] tables"`.
  - two tables with the same `name` give `"airplay 2: name \"x\" is already used"`.
- [ ] **Step 2:** `cargo test config::` fails to compile (`airplay` unknown).
- [ ] **Step 3:** Add the types and the parsing, following the `[[output]]` pattern. An integer is accepted wherever a float is.
- [ ] **Step 4:** `cargo test config::` passes.
- [ ] **Step 5:** Commit: `feat(config): [[airplay]] receivers`.

### Task 2: The packets (`src/airplay/rtp.rs`)

Pure functions, byte for byte as PipeWire sends them.

**Files:**
- Create: `src/airplay/mod.rs` (module declarations only)
- Create: `src/airplay/rtp.rs`
- Modify: `src/lib.rs` (`pub mod airplay;`)

**Interfaces:**
- Produces:
  - `pub const FRAMES: usize = 352;`
  - `pub const RATE: u32 = 44100;`
  - `pub const SYNC_EVERY: u32 = 121;`
  - `pub fn alac_escape(frames: &[[i16; 2]], out: &mut Vec<u8>)`: appends 4n+8 bytes.
  - `pub fn audio_packet(first: bool, seq: u16, timestamp: u32, ssrc: u32, payload: &[u8], out: &mut Vec<u8>)`
  - `pub fn sync_packet(first: bool, rtp_now: u32, latency: u32, ntp: u64) -> [u8; 20]`
  - `pub fn timing_reply(request: &[u8], received: u64, transmitted: u64) -> Option<[u8; 32]>`: `None` unless the request is 32 bytes and starts `80 d2 00 07`.
  - `pub fn retransmit_request(packet: &[u8]) -> Option<(u16, u16)>`: `(first seq, count)`, `None` for count 0 or a packet that is not `0x80 0xd5`.
  - `pub struct History`: 512 packets by seq, wrapping.
    - `fn push(&mut self, seq: u16, packet: &[u8])`
    - `fn get(&self, seq: u16) -> Option<&[u8]>`
  - `pub fn ntp_from(realtime: Duration) -> u64`: seconds since 1970 plus 2208988800 in the high word, the fraction in the low word.

- [ ] **Step 1: Write failing tests:**
  - `an_escape_frame_of_352_frames_is_1416_bytes_and_starts_20_00_12`.
  - `the_samples_are_big_endian_shifted_by_7_bits`: frames `[[0x1234, -2]]` give exactly the bytes worked out by hand from PipeWire's `bit_writer`. Put the derivation in a comment.
  - `the_end_tag_is_7_in_3_bits`.
  - `the_first_audio_packet_has_the_marker`: byte 1 is `0xe0`, later ones `0x60`; seq, timestamp and ssrc are big-endian.
  - `a_sync_packet_carries_the_time_less_the_latency`: bytes 0..4 are `90 d4 00 07` first and `80 d4 00 07` after. Then `rtp_now - latency`, the NTP time, then `rtp_now`. The sequence is network-order 7, deliberately not PipeWire's little-endian 07 00.
  - `a_timing_reply_echoes_the_request_time`: bytes `80 d3 00 00`, 4 zero bytes, then the request's last 8 bytes, `received`, `transmitted`.
  - `timing_reply_rejects_31_bytes_and_a_wrong_header`.
  - `retransmit_request_reads_seq_and_count`, and `ignores_count_0`.
  - `history_keeps_the_last_512_across_the_wrap`: seq 65500 to 65535 then 0 to 600. `get(65535)` is `None` after 512 newer pushes, and `get(600)` is `Some`.
  - `ntp_of_the_unix_epoch_is_2208988800_seconds`, and `half_a_second_is_0x80000000`.
- [ ] **Step 2:** `cargo test airplay::rtp` fails (module missing).
- [ ] **Step 3:** Implement. `alac_escape` writes PipeWire's header bits
  `1/3, 0/4, 0/8, 0/4, 1/1, 0/2, 1/1`, then the frame count in 32 bits, then
  each sample's 16 bits MSB first, then `7/3`. A bit writer of about ten lines,
  with no allocation beyond `out`.
- [ ] **Step 4:** The tests pass.
- [ ] **Step 5:** Commit: `feat(airplay): RTP, sync, timing and retransmit packets`.

### Task 3: The gate (`src/airplay/gate.rs`)

**Interfaces:**
- Produces:
  - `pub struct Gate`
  - `Gate::new(rate: u32, threshold_dbfs: f32, idle_s: u32) -> Gate`
  - `Gate::feed(&mut self, interleaved: &[i32]) -> Option<GateEvent>` (stereo)
  - `pub enum GateEvent { Signal, Silence }`
  - `Gate::set_rate(&mut self, rate: u32)`
  - `Gate::signal(&self) -> bool`

The level is the RMS of both channels over 50 ms windows, as `detect` measures
it (`src/plugin/detect.rs`).
- A window above the threshold gives `Signal` once, when the gate was closed.
- `Silence` comes once no window has been above for `idle_s`.
- NaN and infinity count as they do in `detect`.

- [ ] **Step 1: Write failing tests:**
  - `a_window_above_the_threshold_signals_once`.
  - `silence_after_idle_s_not_before`, with `idle_s = 2` at 48000:
    - 1.95 s of zeros gives `None`;
    - 2.0 s gives `Silence`;
    - more zeros give `None`.
  - `a_signal_within_idle_s_keeps_it_open` (Review Focus 5).
  - `minus_60_dbfs_sine_is_silence_at_minus_50_and_signal_at_minus_70`.
  - `a_rate_change_keeps_the_state`.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement the gate.
- [ ] **Step 4:** The tests pass.
- [ ] **Step 5:** Commit: `feat(airplay): signal gate`.

### Task 4: RTSP and the AirPlay 1 handshake

**Files:**
- Create: `src/airplay/rtsp.rs`: the request writer and response parser, sans-io, plus `Conn` over `TcpStream`.
- Create: `src/airplay/raop.rs`: the AirPlay 1 session.
- Create: `src/airplay/fake.rs` (`#[cfg(test)]`): a fake receiver.
  - It is a `TcpListener` plus two `UdpSocket`s, answering per a script.
  - It records every request and every datagram.

**Interfaces:**
- Produces:
  - `pub struct Request<'a> { method: &'a str, url: &'a str, headers: &'a [(&'a str, String)], content_type: Option<&'a str>, body: &'a [u8] }`
  - `fn write(&self, cseq: u32, out: &mut Vec<u8>)`
  - `pub struct Response { status: u16, headers: Vec<(String, String)>, body: Vec<u8> }`
  - `pub fn parse(buf: &[u8]) -> Result<Option<(Response, usize)>, RtspError>`: incremental; a body of more than 64 KiB is an error.
  - `pub struct Conn`:
    - `Conn::connect(host: &str, port: u16, timeout: Duration) -> io::Result<Conn>`
    - `Conn::request(&mut self, req: &Request) -> Result<Response, RtspError>`
    - `Conn::local_ip(&self) -> IpAddr`
  - `pub struct Raop` (the AirPlay 1 session):
    - `Raop::open(conn: Conn, latency_frames: u32, control_port: u16, timing_port: u16, first_seq: u16, first_rtptime: u32) -> Result<(Raop, Ports), RtspError>`
    - `Raop::volume(&mut self, db: f32) -> Result<(), RtspError>`
    - `Raop::teardown(self)`
  - `pub struct Ports { pub server: u16, pub control: u16, pub timing: Option<u16> }`
  - `Raop::audio_latency(&self) -> Option<u32>` (from the RECORD reply)

The handshake, in this order:
1. `OPTIONS` to the session URL, as PipeWire sends it, with `Apple-Challenge`: base64 of 16 random bytes, without
   padding.
2. `POST /auth-setup`.
3. `ANNOUNCE rtsp://<local ip>/<session id>`, with the SDP of the Global
   Constraints plus `a=min-latency:11025` after the fmtp line.
4. `SETUP`, with
   `Transport: RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=<c>;timing_port=<t>`.
5. `RECORD`, with `Range: npt=0-` and `RTP-Info: seq=<s>;rtptime=<t>`.
6. `SET_PARAMETER` with `volume: <db>\r\n` unless volume is off, then
   `progress: 0/0/0\r\n`.

The headers on every request:
- `CSeq`, from 1;
- `User-Agent: aas/<version>`;
- `Client-Instance` and `DACP-ID`, the same 16 hex digits;
- `Session`, after SETUP.

- [ ] **Step 1: Write failing tests:**
  - `a_request_is_written_as_rtsp`, with exact bytes.
  - `a_response_is_parsed_across_reads`, with byte-by-byte feeding.
  - `content_length_over_64k_is_an_error`.
  - `the_handshake_sends_options_auth_setup_announce_setup_record_in_order`: against the fake receiver, asserting the method sequence, the auth body hex `01 5902ede9…7e1d4e` and the SDP text exactly.
  - `setup_reads_server_control_and_timing_ports`, and `a_missing_timing_port_is_none`.
  - `a_404_to_auth_setup_goes_on` (Review Focus 1).
  - `a_401_to_auth_setup_ends_the_attempt`.
  - `a_non_200_to_announce_is_an_error_naming_it`.
  - `record_carries_seq_and_rtptime`.
  - `audio_latency_is_read_from_record`.
  - `volume_minus_12_5_is_sent_as_text`.
  - `a_closed_socket_mid_handshake_is_an_error_not_a_hang`: it fails within the 2 s test timeout (Review Focus 2).
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement. Reads use `set_read_timeout(15 s)`, the handshake budget. Random values come from `/dev/urandom` through `libc::getrandom`, so no new crate.
- [ ] **Step 4:** The tests pass.
- [ ] **Step 5:** Commit: `feat(airplay): RTSP and the AirPlay 1 handshake`.

### Task 5: The stream (`src/airplay/stream.rs`)

What runs between RECORD and TEARDOWN: tap samples in, paced packets out.

**Interfaces:**
- Consumes: `Adaptive::{new, set_ratio, input_needed, process, delay_frames, reset}`, `Drift::{new, update, reset}`, everything in Task 2.
- Produces:
  - `pub trait Clock { fn now(&self) -> Duration; }`, with `Monotonic` and a test `FakeClock`.
  - `pub struct Stream`:
    - `Stream::new(master_rate: u32, latency_frames: u32, first_seq: u16, first_rtptime: u32, ntp_at_start: u64, start: Duration) -> Stream`
    - `Stream::take(&mut self, interleaved: &[i32])`: master-rate stereo from the tap.
    - `Stream::set_rate(&mut self, master_rate: u32)`
    - `Stream::due(&mut self, now: Duration, out: &mut Vec<Outgoing>)`: every packet whose time has come, plus a sync packet when one is due.
    - `Stream::resend(&self, first: u16, count: u16, out: &mut Vec<Vec<u8>>)`
    - `Stream::ntp(&self, now: Duration) -> u64`
    - `Stream::next_due(&self) -> Duration`
  - `pub enum Outgoing { Audio(Vec<u8>), Sync([u8; 20]) }`

Pacing and delay:
- Packet k is due at `start + k·352/44100 s`.
- The path's delay is the frames waiting in `Stream`, plus the resampler's
  delay, in 44.1 kHz frames.
- `Drift` holds the delay at `2·352 + one master period`, read when each packet
  is made, and its ratio goes to the `Adaptive`.
- With too little at a packet's time the packet is padded with zeros, counted,
  and the drift is reprimed.
- `rtptime` advances by 352 per packet, from `first_rtptime`.
- A sync carries `rtp_now = rtptime` of the packet it precedes, with
  `latency_frames`.

- [ ] **Step 1: Write failing tests**, with `FakeClock`:
  - `packets_are_due_every_352_frames_of_44100`: 1 s gives 125 or 126 packets.
  - `the_first_packet_has_the_marker_and_a_first_sync_before_it`.
  - `a_sync_follows_every_121_packets`.
  - `48k_in_gives_44100_out_and_the_delay_settles`: 60 s of a 1 kHz sine at 48000, fed in 256-frame periods with the master 100 ppm fast. The waiting frames end within ±16 of the target, with no padding after the first second.
  - `a_starved_packet_is_zeros_and_counted`.
  - `a_rate_change_keeps_rtptime_continuous` (Review Focus 4): 48000 to 96000 mid-stream; consecutive timestamps step by 352, at most one packet of zeros.
  - `resend_returns_the_stored_packets`.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement. Samples are converted `i32 → i16` with rounding and
  clipping after resampling. No TPDF dither here, because the monitor already
  carries the master's dither where the card's format calls for it.
- [ ] **Step 4:** The tests pass.
- [ ] **Step 5:** Commit: `feat(airplay): paced stream with drift against the system clock`.

### Task 6: The output thread, lifecycle and API

**Files:**
- Create: `src/airplay/output.rs`: the thread.
- Modify: `src/control.rs`: make the tap per receiver at start, spawn, stop on shutdown, collect events.
- Modify: `src/api.rs`, `src/control/api.rs`: `GET /airplay`, events.
- Modify: `README.md` (section "AirPlay"), `doc/switch.md`, `TODO.md`.

**Interfaces:**
- Consumes: Tasks 1 to 5; `Ring::create`/`Ring::map`, `Command::AddTap`/`RemoveTap`/`SetRunning`, the eventfd pattern of `Control::configure_recorder` (`control.rs` around 1895).
- Produces:
  - `pub fn spawn(config: AirplayConfig, tap: TapReader, events: Sender<AirplayEvent>) -> AirplayHandle`
  - `AirplayHandle::stop(self)`: joins within 1 s, sends TEARDOWN if connected.
  - `pub struct AirplayEvent { pub index: usize, pub state: AirplayState, pub error: Option<String> }`
  - `pub enum AirplayState { Idle, Connecting, Playing, Retrying { in_s: u32 } }`
  - API view: `GET /airplay` returns `[{"name","host","port","protocol","connect","state","error"}]`, and an `airplay` event on every state change.

The thread's loop:
1. Wait on the tap's eventfd and on a timer, the earlier of `Stream::next_due` and the retry time.
2. Read the tap and feed the gate.
3. In `Connecting`/`Playing`, feed the stream.
4. Send what is due.
5. Answer timing and retransmit packets (the UDP sockets are non-blocking).

| State | On | Goes to |
|---|---|---|
| `Idle` | `Signal`, or `connect = always` at start | `Connecting` |
| `Connecting` | RECORD | `Playing` |
| `Connecting` | failure | `Retrying` |
| `Playing` | `Silence`, with `connect = signal` | TEARDOWN, then `Idle` |
| `Playing` | a socket error, or an RTSP error | `Retrying` |
| `Retrying` | the time is up, with the signal still present or `connect = always` | `Connecting` |
| `Retrying` | the time is up, otherwise | `Idle` |

- With `connect = signal`, the tap keeps running while idle, so the gate keeps
  seeing samples.
- Audio from before RECORD is dropped. The receiver hears the record from about
  a second after its first groove; on a record that is the lead-in.

- [ ] **Step 1: Write failing tests:**
  - Against the fake receiver and a test tap ring fed by the test:
    - `a_signal_connects_and_silence_tears_down` (`idle_s = 1`);
    - `always_connects_at_start_without_a_signal`;
    - `a_receiver_that_closes_its_socket_is_retried_with_backoff` (Review Focus 2): the event sequence `Playing, Retrying{1}, Connecting, Playing`;
    - `an_unresolvable_host_retries_and_never_ends_the_thread` (Review Focus 3);
    - `stop_tears_down_and_joins_within_1_s`.
  - In `control/api.rs`: `get_airplay_lists_each_receiver_with_its_state`, and `a_state_change_is_published`.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement. Retry times are `[1, 2, 4, 8, 16]` and then 30 s.
- [ ] **Step 4:** The tests pass. Then run the full `cargo test` and `cargo clippy --all-targets`.
- [ ] **Step 5:** Docs:
  - README "AirPlay": the keys; what is sent (the master monitor, so the limit applies); the latency; that the receiver's own volume stays its own; the gate.
  - `doc/switch.md`: PipeWire's `raop-discover` setup with a pause helper maps to `[[airplay]]` with `connect = "signal"`.
  - `TODO.md`.
- [ ] **Step 6:** Commit: `feat(airplay): AirPlay 1 output with connect on signal`.

### Task 7: On the device

**Files:**
- Modify: `doc/measurements.md` (a new section, "AirPlay output")

This task has no code, only checks. The ADC-only turntable box runs `aas` on
its capture clock (`clock = "auto"`) and plays to the sound bar on its own
subnet, by its mDNS name, then to a Mac.

- [ ] **Step 1:** Run the branch's `aas` as root with PipeWire stopped, as for the capture clock. Set `[[airplay]] host = "<the sound bar's name>"`, `idle_s = 30`.
- [ ] **Step 2:** Play a record, or a 1 kHz tone with `aplay` to `aas`. The sound bar switches to AirPlay and plays it, and `GET /airplay` says `playing`.
- [ ] **Step 3:** Stop the tone. After 30 s the state goes to `idle`, and the receiver reports that its AirPlay input is no longer playing.
- [ ] **Step 4:** Ten minutes of music, then check the stderr log: no padding and no retransmits beyond a handful.
- [ ] **Step 5:** A Mac as the receiver (Review Focus 1).
- [ ] **Step 6:** Record the results and commit: `docs: the AirPlay output on a sound bar and a Mac`.

---

## Part 2: AirPlay 2

> **As built (2026-11-21).** Done, with these differences from the tasks
> below, found on a sound bar: the receiver answers RECORD only once the
> sender has connected to the `eventPort` of its session SETUP answer
> (`src/airplay/ap2/events.rs`, keys `Events-Salt`, read and write swapped,
> as OwnTone and pyatv have them; PipeWire's sender does not open it). The
> RTSP connection seals itself after pairing (`Conn::seal`) instead of a
> separate `Sealed` type, and skips answers to requests sent without waiting,
> such as `/feedback`. `shk` is a random key per session. Binary plists are
> written and read by `ap2/bplist.rs`, checked against Python's `plistlib`.
> Task 11's reconnect after the bar is switched to another input and back is
> not tested yet.

The same thread, gate, stream timing and packets as Part 1. What differs is
how the session is opened and that the RTP payload is encrypted. The
reference is PipeWire's AirPlay 2 path in `module-raop-sink.c`
(`CRYPTO_PAIR_TRANSIENT`, `encrypt_ap2` around line 448, `rtsp_do_ap2_*`) and
`module-raop/srp.c` (SRP-6a client, 3072-bit group, SHA-512, `PAD()`).
Third-party receivers pair transiently with PIN 3939. HomeKit pairing with a
stored key, and FairPlay, are not in this plan.

Before Task 8, read PipeWire's AirPlay 2 request sequence and write the exact
messages into `src/airplay/ap2/README.md` as the next task's fixtures, from
the code, the way Part 1's were taken. Its fixtures replace the
"PipeWire's …" references below with bytes.

### Task 8: Transient pairing (`src/airplay/ap2/pair.rs`)

**Interfaces:**
- Produces:
  - `pub fn tlv8_encode(items: &[(u8, &[u8])]) -> Vec<u8>`, `pub fn tlv8_decode(b: &[u8]) -> Result<Vec<(u8, Vec<u8>)>, PairError>`: values over 255 bytes split and rejoin.
  - `pub fn pair_transient(conn: &mut Conn) -> Result<SharedSecret, PairError>`: `POST /pair-setup` M1 to M4, user `"Pair-Setup"`, PIN `"3939"`, `X-Apple-HKP: 4`.
  - `pub struct SharedSecret([u8; 64])`

- [ ] **Step 1: Write failing tests:**
  - `tlv8_splits_values_over_255_and_rejoins_them`.
  - `srp_matches_rfc5054_appendix_b`, with the RFC's test vector run at 1024 bits through the same code path.
  - `pair_transient_against_a_scripted_receiver`: M2 and M4 recorded from PipeWire's `test-auth.c` fixtures, or worked out with the same SRP.
  - `a_wrong_m4_proof_is_an_error`.
- [ ] **Steps 2–5:** see the failure, implement with `srp` and `sha2`, see the pass, commit `feat(airplay): AirPlay 2 transient pairing`.

### Task 9: The encrypted control channel (`src/airplay/ap2/channel.rs`)

**Interfaces:**
- Produces: `pub struct Sealed { conn: Conn, … }`, with `Sealed::new(conn: Conn, secret: &SharedSecret) -> Sealed` and the same `request()` as `Conn`.

The keys are derived with HKDF-SHA512, salt `"Control-Salt"`, infos
`"Control-Write-Encryption-Key"` and `"Control-Read-Encryption-Key"`. Frames
are at most 1024 bytes of plaintext. Each frame carries its 2-byte
little-endian length as AAD and is sealed with ChaCha20-Poly1305. The nonce
is a 64-bit little-endian counter per direction behind 4 zero bytes.

- [ ] **Step 1: Write failing tests:**
  - `a_request_over_1024_bytes_is_two_frames`.
  - `nonces_count_per_direction`.
  - `a_tampered_tag_is_an_error`.
  - `round_trip_against_a_receiver_side_built_from_the_same_keys`.
- [ ] **Steps 2–5:** as above, commit `feat(airplay): AirPlay 2 control channel`.

### Task 10: The AirPlay 2 session and encrypted audio

**Files:**
- Create: `src/airplay/ap2/session.rs`
- Modify: `src/airplay/stream.rs` (a payload sealer), `src/airplay/output.rs` (dispatch on `protocol`)

**Interfaces:**
- Produces:
  - `Ap2::open(sealed: Sealed, latency_frames: u32, ports…) -> Result<(Ap2, Ports), RtspError>`
  - the same `volume`, `teardown` and `audio_latency` as `Raop`
  - `pub trait Payload { fn seal(&mut self, header: &[u8; 12], payload: &mut Vec<u8>); }`, with `Clear` for AirPlay 1 and `ChaCha { key }` for AirPlay 2

The session:
- the first `SETUP` with a binary plist: NTP timing, realtime audio type 96;
- the stream `SETUP` with the audio format: PCM 44100/16/2, `shk` a random
  32-byte key, and the `ct`/`spf` values as PipeWire sends them;
- `RECORD` and `SET_PARAMETER`.

The payload is sealed with ChaCha20-Poly1305 under `shk`. The AAD is RTP bytes
4..12. The nonce is the 8-byte packet counter behind 4 zero bytes, and the tag
and nonce are appended as PipeWire's `encrypt_ap2` does.

- [ ] **Step 1: Write failing tests:**
  - `setup_plist_has_ntp_timing_and_realtime_type_96`, by decoding the request.
  - `the_stream_setup_carries_shk`.
  - `a_sealed_payload_opens_with_shk`.
  - `the_ap2_handshake_against_a_scripted_receiver`.
  - `protocol_airplay2_uses_the_sealed_payload`.
- [ ] **Steps 2–5:** as above, commit `feat(airplay): AirPlay 2 session and encrypted audio`.

### Task 11: AirPlay 2 on the device

As Task 7, with `protocol = "airplay2"` to the sound bar. It also covers a
reconnect after the sound bar has been switched to another input and back,
the case the pause helper handles under PipeWire. Record the results in `doc/measurements.md`.

---

## Not in this plan

- **The receiver's volume following `Master`.** `volume_db` is fixed and sent
  once per session.
- **mDNS discovery and a receiver list in the web UI.** `host` is fixed in the
  config, as asked.
- **Several receivers in sync.** Each `[[airplay]]` table is independent. They
  share the system clock, so they stay close, but that is not promised.
- **HomeKit pairing with a stored key, FairPlay, PTP timing and buffered
  audio.** Apple TVs and HomePods that refuse transient pairing need these.
- **Sending the audio from before RECORD.** The first second after a signal
  starts is not heard.
