# Stage 4, part 1: the API for volumes, inputs and the matrix

> The design, `doc/design.md`, is the authority: where this plan disagrees with it, the
> design wins.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps
> use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The daemon answers HTTP on `127.0.0.1:2718`. Through it a caller reads the
state, sets each output's volume and idle rule, sets the volume of the analog input
and of each client by its process name, and replaces each output's matrix. Every
change is kept in the state file and announced on WebSocket `/events`.

**Architecture:** A thread `aas-api` runs `axum` on a single-threaded `tokio` runtime.
It decides nothing: each call becomes an `ApiCall` message to the control thread,
which is woken by an eventfd in its poll, handles the call between client requests
and answers on a one-shot channel. The control thread is still the only one that
sends commands to the audio thread. Changes are published on a `tokio` broadcast
channel, which the WebSocket handlers read.

**Tech Stack:** Rust (edition 2024, `rust-version` 1.85). New dependencies: `axum`
0.8 (features `http1`, `json`, `ws`, `tokio`, no defaults otherwise), `tokio` 1
(features `rt`, `net`, `sync`, `time`), `serde_json` 1. Dev: `tokio-tungstenite`
for the WebSocket test, and an HTTP client written on `std::net::TcpStream` in
the test (no `reqwest`).

**Spec:** `doc/design.md`, sections API (with "Stage 4 in parts" and "The API in
part 1"), Clients, Mixing matrix, Volume, Configuration, Monitoring and reporting.

## Stages

1. Measurement (done)
2. Core daemon and ALSA plugin (done)
3. a. Analog input and native plugins (done)
   b. Matrix, channels and several outputs (done)
   c. `input-processor` as a native plugin
4. API, monitoring, graph export
   - **Part 1: outputs, inputs, the matrix, the state file, `/events` (this plan)**
   - Part 2: `/metrics`, `/graph`, `/plugins`, `/settings`, `/levels`, the command
     line tool, the auth manifest and nginx
   - Part 3: a matrix row and a chain per client
   - Part 4: the clock master at run time, after its own design
5. Packaging, Bluetooth and the switch from PipeWire, with all players tested

## Decisions this plan makes

Everything the design marks *(call)* under "The API in part 1" was decided while
writing this plan, without the owner's answers. Beyond those:

- The control thread's API handling lives in a child module, `src/control/api.rs`
  (declared `mod api;` in `src/control.rs`), so it reaches `Control`'s private fields
  without growing `control.rs` further.
- The bound listener is passed into `server::run` in `Devices`, as
  `api: Option<std::net::TcpListener>`. `main` binds it from the config; the tests
  bind `127.0.0.1:0` and read the port, so no test races for a fixed port.
- The bit-perfect decision is published as `Counters::bit_perfect: AtomicU8`: 0 is
  bit-perfect, otherwise `1 + ` the `NotBitPerfect` variant's position in its
  declaration. `NotBitPerfect` gets `fn code(self) -> u8`,
  `fn from_code(u8) -> Option<Result<(), NotBitPerfect>>` and `fn name(self) -> &'static str`
  (`"disabled"`, `"several sources"`, `"volume"`, `"resampled"`, `"plugin"`,
  `"no source"`, `"routing"`).
- An API output id N is engine output index N - 1: the master is 1 in the API and 0 in
  `Command::SetGain` and `Command::SetMatrix`, output 2 is engine index 1.
- The state file becomes version 2 (below). A version 1 file, `output_percent`
  alone, still loads.

## Global Constraints

- Nothing on the audio thread allocates, locks or blocks. Every new engine path is
  covered by an `assert_no_alloc` test as the existing ones are.
- The API thread never calls into `Control` and never sends engine commands.
- No new dependency beyond those named under Tech Stack. `cargo build` without the
  `alsa-output` feature still works.
- `api_port` from 0 to 65535, default 2718; 0 turns the API off.
- Input `volume_db` from -60 to 0. Further output `volume_db` from -60 to 0. Master
  `volume` 0 to 100. Matrix `gain_db` from -100 to 0. Out of range is 422, never
  clamped.
- At most 64 stored client names. The 65th new name is 422 `"too many clients with
  a volume (64)"`.
- Fades: 10 ms, as the output gain's.
- A matrix reaches the engine at most once per 20 ms per output.
- A call the control thread has not answered in 2 s is 503.
- `/events`: broadcast capacity 256; a lagging subscriber gets `{"type":"resync"}`.
- Error bodies are `{"error": "..."}`; messages follow the config's wording.
- Public text (commit messages, docs) names no machine, address or tool. The test
  boards are "the CM5 test board" and "the Pi 3 test board".

## Review Focus

1. A client that connects while its name already has a stored volume must start at
   that volume, not at 0 dB with a fade down: the gain goes in the `Source` at `Add`.
2. Two streams of one process name: a change applies to both, and the second one
   started later also gets it.
3. A `PUT /matrix` sent in a burst of 100 calls: only the last reaches the engine
   within 20 ms of the burst's end, and no `OldMatrix` is lost (each one is freed on
   the control thread).
4. A config edit after an API change: the edited key wins at the next start; an
   unedited key keeps the API's value. Reordering `[[output]]` tables loses nothing.
5. The API thread blocked (a slow WebSocket reader, a call while the control thread
   is busy) never delays a client request or a cycle; the control thread never
   waits on the API thread.

Each has a test in the task that owns the code: 1 and 2 in Task 3, 3 in Task 4, 4 in
Task 2, 5 in Task 5.

---

### Task 1: input volumes fade, and the engine publishes its bit-perfect decision

**Files:**
- Modify: `src/engine.rs` (`Command`, `Source`, the analog input, `play_pcm`, `situation`)
- Modify: `src/bitperfect.rs`, `src/counters.rs`

**Interfaces:**
- Produces: `Command::SetSourceGain(ClientId, f32)` — fades the source's gain over 10 ms
  from where it is; one for a source not in the table is ignored.
  `Command::SetAnalogGain(f32)` now fades over 10 ms too (its doc comment changes).
  `Counters::bit_perfect: AtomicU8`, written by the audio thread only when it changes.
  `NotBitPerfect::{code, from_code, name}` as in Decisions.

- [ ] **Step 1: Write the failing tests** in `src/engine.rs`'s tests, using the rig the
  existing gain tests use:
  - `a_source_gain_change_fades_over_10_ms`: a source of constant 0.5 at 48 kHz, gain 1.0;
    send `SetSourceGain(id, 0.25)`; the first period after it starts at 0.5, the sample at
    frame 480 after the command is 0.125 within 1e-6, and no sample steps by more than
    `0.375 / 480 + 1e-6`.
  - `the_analog_gain_fades_over_10_ms`: the same for `SetAnalogGain`.
  - `a_fading_source_gain_is_not_bit_perfect`: one source, bit-perfect auto, gain 1.0 →
    `decide` is `Ok`; during a fade to 1.0 from 0.5 it is `Err(Volume)`; after it, `Ok`.
  - `set_source_gain_does_not_allocate`: under `assert_no_alloc`.
  - `the_bit_perfect_decision_is_published`: after a cycle with one 0 dB source,
    `counters.bit_perfect` is 0; after `SetSourceGain(id, 0.5)` and a cycle, it is
    `NotBitPerfect::Volume.code()`.
  - in `src/bitperfect.rs`: `codes_round_trip` for every variant, and `0` gives `Some(Ok(()))`,
    `200` gives `None`.
- [ ] **Step 2: Run** `cargo test --lib engine::tests::a_source_gain bitperfect` — fails to compile.
- [ ] **Step 3: Implement.** A source keeps `gain` (the target) and a ramp position; the
  mix applies a linear ramp per frame while it runs, as `Fade` does for the master.
  `situation()` treats a ramping source or analog gain as not at 0 dB. `play_pcm` calls
  `decide(&self.situation())` once per cycle and stores the code when it differs from the
  last stored one.
- [ ] **Step 4: Run** `cargo test` — all pass, and `cargo clippy --all-targets` is clean.
- [ ] **Step 5: Commit** `feat: input volumes fade, and the bit-perfect decision is published`

### Task 2: state file version 2

**Files:**
- Modify: `src/state.rs`
- Test: `src/state.rs` tests

**Interfaces:**
- Produces, in `src/state.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Stored<T> { pub value: T, pub config: T }  // `config`: what the config said then

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredInput { pub volume_db: f32, pub muted: bool }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRoute { pub from: String, pub to: usize, pub gain_db: f32 }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StoredOutput {
    pub volume_db: Option<Stored<f32>>,              // further outputs only
    pub idle: Option<Stored<String>>,                 // "zeros" or "off"
    pub idle_delay_s: Option<Stored<u32>>,
    pub matrix: Option<Stored<Vec<StoredRoute>>>,     // empty = the default matrix
}

pub struct State {                                     // no longer Copy
    pub version: u32,                                  // 2
    pub output_percent: u8,
    pub analog: Option<Stored<StoredInput>>,
    pub clients: BTreeMap<String, StoredInput>,        // at most 64
    pub outputs: BTreeMap<String, StoredOutput>,       // keyed by card
}

impl<T: PartialEq + Clone> Stored<T> {
    /// The value to use now: the stored one while the config still says `config`,
    /// else what the config says.
    pub fn resolve(stored: Option<&Stored<T>>, config_now: &T) -> T;
}
```

`Saver::save` takes `State` by value as now.

- [ ] **Step 1: Write the failing tests:**
  - `a_version_1_file_still_loads`: `output_percent = 40\n` loads as `State { version: 2,
    output_percent: 40, ..Default }`.
  - `version_2_round_trips`: a state with every field set saves and loads equal.
  - `a_stored_value_applies_while_the_config_is_unchanged`:
    `resolve(Some(&Stored { value: -6.0, config: 0.0 }), &0.0) == -6.0`.
  - `a_config_edit_wins`: `resolve(Some(&Stored { value: -6.0, config: 0.0 }), &-3.0) == -3.0`.
  - `nothing_stored_takes_the_config`: `resolve(None, &-3.0) == -3.0`.
  - `outputs_are_keyed_by_card`: two outputs stored under `"Loopback"` and `"sndrpihifiberry"`
    survive a save and load in either insertion order.
  - `a_client_entry_out_of_range_is_dropped_on_load`: `volume_db = -70` for one name drops that
    name only, and the rest loads.
- [ ] **Step 2: Run** `cargo test --lib state` — fails to compile.
- [ ] **Step 3: Implement** with serde; `load` keeps the rule that an unreadable file gives the
  defaults, and drops individual entries that are out of range rather than the file.
- [ ] **Step 4: Run** `cargo test` — all pass (callers of `State { output_percent }` updated).
- [ ] **Step 5: Commit** `feat: the state file keeps inputs, clients, outputs and matrices`

### Task 3: the control thread handles API calls for outputs and inputs

**Files:**
- Create: `src/api.rs` (message and view types, shared by the control and the HTTP side)
- Create: `src/control/api.rs` (`impl Control` for calls)
- Modify: `src/control.rs` (`mod api;`, client gain at `Add`, restoring from `State`),
  `src/config.rs` (`api_port`, `pub fn route_from_parts`), `src/lib.rs`, `README.md` (config key)

**Interfaces:**
- Produces, in `src/api.rs`:

```rust
pub enum Call {
    State, Outputs, Output(usize), PatchOutput(usize, OutputPatch),
    Inputs, Analog, PatchAnalog(InputPatch), Client(String), PatchClient(String, InputPatch),
    Matrix, PutMatrix(MatrixPut),
}
pub struct ApiCall { pub call: Call, pub answer: tokio::sync::oneshot::Sender<Answer> }
pub type Answer = Result<serde_json::Value, ApiError>;
pub struct ApiError { pub status: u16, pub message: String }   // 400, 404, 422, 503

#[serde(deny_unknown_fields)] pub struct OutputPatch {
    pub volume: Option<u8>, pub volume_db: Option<f32>,
    pub idle: Option<String>, pub idle_delay_s: Option<u32>,
}
#[serde(deny_unknown_fields)] pub struct InputPatch { pub volume_db: Option<f32>, pub muted: Option<bool> }
#[serde(deny_unknown_fields)] pub struct MatrixPut { pub outputs: Vec<OutputRoutes> }
#[serde(deny_unknown_fields)] pub struct OutputRoutes { pub output: usize, pub routes: Vec<RouteJson> }
#[serde(deny_unknown_fields)] pub struct RouteJson { pub from: String, pub to: i64, pub gain_db: Option<f64> }

#[serde(tag = "type", rename_all = "lowercase")]
pub enum ApiEvent {
    Output { id: usize, volume: Option<u8>, volume_db: Option<f32>, idle: String,
             idle_delay_s: Option<u32>, state: String },     // "open", "parked", "off"
    Input { id: String, volume_db: f32, muted: bool },       // "analog" or "clients/<name>"
    Client { name: String, change: String },                 // connected, running, stopped, gone
    Matrix { output: usize, routes: Vec<RouteJson> },
    Rate { rate: u32 },
    Resync,
}
```

  In `Control`: `pub fn set_api(&mut self, events: tokio::sync::broadcast::Sender<ApiEvent>)`
  and `pub fn call(&mut self, call: Call, now: Instant) -> Answer` (in `src/control/api.rs`).
  `Control::restore_volume` becomes `pub fn restore(&mut self, volume: OutputVolume, state:
  PathBuf, cards: &[String])`, applying every stored value through `Stored::resolve`;
  `cards` are the outputs' card names, the master's first.
  In `src/config.rs`: `pub api_port: u16` on `Config`, and
  `pub fn route_from_parts(n: usize, from: &str, to: i64, gain_db: f64) -> Result<Route, String>`,
  which `route_from_table` now calls so both give the same messages.
- Consumes: `Command::SetSourceGain`, `Command::SetAnalogGain` (Task 1); `State`, `Stored`
  (Task 2).

- [ ] **Step 1: Write the failing tests** in `src/control/api.rs`, with the control-thread test
  harness `src/control.rs` already uses:
  - `the_master_volume_through_the_api_tells_the_mixer_subscribers`: `PatchOutput(1, volume 30)`
    sends `SetGain(0, …)`, queues a `VolumeChanged { percent: 30 }` notice, and publishes
    `ApiEvent::Output { id: 1, volume: Some(30), .. }`.
  - `a_master_volume_above_100_is_422`.
  - `volume_db_on_the_master_is_422` and `volume_on_a_further_output_is_422`.
  - `a_further_offset_sets_its_gain`: `PatchOutput(2, volume_db -6)` sends
    `SetGain(1, further_gain(-6))` (engine index 1).
  - `zeros_with_a_delay_is_422`: `idle "zeros"`, `idle_delay_s 5`.
  - `output_5_is_404`.
  - `a_client_volume_applies_to_every_stream_of_the_name` (Review Focus 2): two connections
    named `mpd` with running streams; `PatchClient("mpd", -6)` sends `SetSourceGain` for both
    ids; a third `mpd` stream configured after it is `Add`ed with gain `db_to_gain(-6)`.
  - `a_stored_client_volume_is_in_the_source_at_add` (Review Focus 1): no
    `SetSourceGain` follows the `Add`.
  - `a_muted_client_has_gain_0`.
  - `a_name_not_connected_can_be_given_a_volume`, and `the_65th_name_is_422`.
  - `the_analog_volume_sets_its_gain` and `input_volume_db_minus_61_is_422`.
  - `every_change_is_saved`: each successful patch sets `save_at`; the saved `State` holds it,
    with the config value it was set against.
  - `restore_applies_the_rule_of_the_config` (Review Focus 4): a state with a further offset
    stored against config 0 and a config now at -3 starts at -3; one stored against -3 with
    the config at -3 starts at the stored value; the same with the tables in another order.
  - `client_events_are_published`: connect, start, stop, disconnect give
    `connected`, `running`, `stopped`, `gone`.
  - in `src/config.rs`: `api_port_defaults_to_2718`, `api_port_0_turns_it_off`,
    `api_port_70000_is_an_error`.
- [ ] **Step 2: Run** `cargo test --lib control::api config` — fails to compile.
- [ ] **Step 3: Implement.** Views are built from what `Control` holds; nothing waits on the
  engine. Publishing ignores a send error (no subscriber).
- [ ] **Step 4: Run** `cargo test` — all pass; clippy clean.
- [ ] **Step 5: Commit** `feat: the control thread sets volumes and idle rules on request`

### Task 4: the matrix at run time

**Files:**
- Modify: `src/control/api.rs`, `src/control.rs` (pacing in `tick`, `OldMatrix` handling)

**Interfaces:**
- Consumes: `Call::Matrix`, `Call::PutMatrix` (Task 3), `config::route_from_parts`,
  `Matrix::from_routes(routes, width)`, `Command::SetMatrix(output, Box<Matrix>)`,
  `Event::OldMatrix`.
- Produces: `GET /matrix` view `{"outputs": [{"output": N, "routes": [RouteJson…]}]}`, routes
  as last set (the default matrix written out route by route).

- [ ] **Step 1: Write the failing tests:**
  - `a_matrix_put_reaches_the_engine` for output 1 and output 2.
  - `a_matrix_of_another_width_is_422`, `a_route_above_the_width_is_422`,
    `an_unknown_source_is_422` with the config's message, `output_9_is_404`,
    `an_empty_route_list_is_the_default_matrix`.
  - `an_output_not_named_keeps_its_matrix`.
  - `matrices_are_paced_at_20_ms` (Review Focus 3): 100 puts for output 1 within 1 ms of
    simulated time send one `SetMatrix` at once and exactly one more, the last, at or after
    20 ms; each `OldMatrix` that comes back is dropped on the control thread.
  - `a_matrix_put_is_saved_and_published`.
  - `a_stored_matrix_is_restored_while_the_config_matrix_is_unchanged`, and
    `an_edited_config_matrix_wins`.
- [ ] **Step 2: Run** `cargo test --lib control::api::tests::a_matrix` — fails.
- [ ] **Step 3: Implement.** Each output keeps `matrix_due: Option<(Instant, Box<Matrix>)>`;
  `tick` sends what is due. The whole put is checked before any output's matrix changes, so a
  422 changes nothing.
- [ ] **Step 4: Run** `cargo test` — all pass; clippy clean.
- [ ] **Step 5: Commit** `feat: the matrix can be replaced at run time`

### Task 5: the API thread and the HTTP routes

**Files:**
- Create: `src/http.rs` (router, handlers, the `aas-api` thread)
- Modify: `src/server.rs` (`Devices::api`, the eventfd in `step`, draining calls), `src/main.rs`
  (bind `127.0.0.1:api_port`), `src/lib.rs`, `Cargo.toml`
- Test: `tests/api.rs`

**Interfaces:**
- Consumes: `ApiCall`, `Call`, `Answer`, `ApiEvent` (Task 3); `Control::call`, `Control::set_api`.
- Produces: `pub fn spawn(listener: std::net::TcpListener, calls: std::sync::mpsc::Sender<ApiCall>,
  wake: OwnedFd, events: tokio::sync::broadcast::Sender<ApiEvent>) -> io::Result<JoinHandle<()>>`
  in `src/http.rs`. Routes: `GET /state`, `GET /outputs`, `GET|PATCH /outputs/{id}`,
  `GET /inputs`, `GET|PATCH /inputs/analog`, `GET|PATCH /inputs/clients/{name}`,
  `GET|PUT /matrix`, `GET /events` (WebSocket). Paths without the `/api/aas/v1` prefix; nginx
  adds it in part 2.

- [ ] **Step 1: Write the failing tests** in `tests/api.rs`, starting the daemon with a
  `MemorySink` as `tests/server.rs` does and a listener on `127.0.0.1:0`:
  - `get_state_lists_a_playing_client`: a client named after the test binary plays; `/state`
    has its name, `running: true`, the rate, and `bit_perfect: true`.
  - `patch_a_client_volume_and_read_it_back`: `PATCH /inputs/clients/<name>` with
    `{"volume_db": -6}` is 200; `GET` gives -6; `bit_perfect` in `/state` turns false with
    reason `"volume"`.
  - `an_unknown_field_is_400`, `output_7_is_404`, `volume_101_is_422`, each with a JSON `error`.
  - `events_announce_a_volume_change`: a WebSocket on `/events`, then `PATCH /outputs/1`
    `{"volume": 50}` → one message `{"type":"output","id":1,"volume":50,…}`.
  - `a_reader_that_never_reads_does_not_slow_the_daemon` (Review Focus 5): a WebSocket that is
    never read, 1000 patches; a playing client sees no underrun and each patch answers in under
    100 ms.
  - `the_api_off_leaves_no_thread`: with no listener the daemon starts and plays.
- [ ] **Step 2: Run** `cargo test --test api` — fails to compile.
- [ ] **Step 3: Implement.** The handlers send an `ApiCall`, write 1 to the eventfd, and wait on
  the one-shot with a 2 s `tokio::time::timeout` (503 after it). `Server::step` polls the eventfd
  and, after `handle_events`, drains the channel with `try_recv`, answering each through
  `Control::call`. A bind failure in `main` is logged once and the daemon goes on without the
  API.
- [ ] **Step 4: Run** `cargo test` and `cargo build --release --features alsa-output` — pass.
- [ ] **Step 5: Commit** `feat: the HTTP API on its own thread`

### Task 6: on the test boards, and the docs

**Files:**
- Modify: `README.md` (an API section: the routes, the fields, an example with `curl`),
  `doc/measurements.md` (a section "Stage 4 part 1: the API under load"), `TODO.md`

- [ ] **Step 1:** On the CM5 test board, build at the branch and run the daemon with a player
  at 48 kHz for 10 minutes while a script patches the volume, a client volume and the matrix
  10 times a second and holds a WebSocket on `/events`. Record xruns, wake-up gaps, the worst
  cycle and the worst gap against the same 10 minutes without the API calls.
- [ ] **Step 2:** The same on the Pi 3 test board, at its default latency.
- [ ] **Step 3:** Listen on the CM5 test board to a volume change of a client and a matrix
  change: no click.
- [ ] **Step 4:** Write the results into `doc/measurements.md`; in `TODO.md` remove "Matrix
  changes through the API" (decided in the design), and add entries for parts 2, 3 and 4 written
  to be picked up cold.
- [ ] **Step 5: Commit** `docs: the API on the test boards`
