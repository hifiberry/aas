# AirPlay 2 PIN Pairing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pair aas once with an AirPlay 2 receiver that wants one-time pairing (an Apple TV) using the PIN it shows, keep the keys, and connect with pair-verify from then on.

**Architecture:** HAP pair-setup (M1–M6) and pair-verify (M1–M4) in a new `src/airplay/ap2/hap.rs`, reusing TLV8, SRP-6a, HKDF and ChaCha20-Poly1305; keys in `src/airplay/ap2/keys.rs`, a file beside the state file. The receiver's thread runs pairing in a helper thread on request; the API and `aasctl` reach it through the control thread, which forwards the call's answer channel instead of answering itself.

**Tech Stack:** Rust 2024; new crates `ed25519-dalek` 2 (features `rand_core`) and `x25519-dalek` 2 (features `static_secrets`).

**Spec:** `doc/plans/2026-11-22-airplay-pin-pairing-design.md`

## Global Constraints

- Only `statusFlags` bit 9 (0x200, one-time pairing) is pairable; bit 3 (0x8, PIN on every connection) fails with "the receiver wants a PIN for every connection, which aas does not do".
- `X-Apple-HKP: 3` on pair-setup and pair-verify (transient keeps 4).
- No key, seed or shared secret in a log line, an API answer or the config.
- The keys file is `airplay-pairing.toml` in the state file's directory, written atomically (temp file + rename) with mode 0600.
- A held pairing connection waits at most 120 s for the PIN.
- The PIN is exactly 4 digits.
- Pairing never runs on the receiver's packet loop; it runs in a helper thread.
- Format single files with `rustfmt --edition 2024 <file>`; `src/control.rs` and `src/control/api.rs` stay unformatted (they were not clean before). Conventional commits, no tool names.

## Review Focus

1. A wrong PIN: the call answers 422 "the receiver did not take the PIN", no keys are stored, and a new `pair` start works. Test in Task 2.
2. A PIN sent after 120 s, or with no start: 409 "start pairing first: aasctl airplay pair NAME". Test in Task 4.
3. The receiver forgot the pairing (reset): the session fails with "the receiver refused the pairing: pair again with aasctl airplay pair NAME"; the stored keys stay. Test in Task 3.
4. A keys file that is unreadable or garbage: logged once, treated as empty, aas runs on. Test in Task 1.
5. Pairing while the receiver is playing: the session keeps playing. Test in Task 4.

---

### Task 1: Keys

**Files:**
- Create: `src/airplay/ap2/keys.rs`; Modify: `src/airplay/ap2/mod.rs`, `Cargo.toml`

**Interfaces:**
- Produces:
  - `pub struct Identity { pub id: String, pub signing: ed25519_dalek::SigningKey }`
  - `#[derive(Clone, Debug, PartialEq)] pub struct Peer { pub id: String, pub public: [u8; 32] }`
  - `pub struct Pairings` with `new() -> Arc<Pairings>`, `load(&self, path: PathBuf)`, `identity(&self) -> Identity` (made, with a random UUID id, and saved on first use), `peer(&self, name: &str) -> Option<Peer>`, `store(&self, name: &str, peer: Peer) -> std::io::Result<()>`
- File format: `[identity]` `id`, `seed` (64 hex chars); `[receivers."<name>"]` `id`, `public` (64 hex).

- [ ] **Step 1: Failing tests** (`tempfile` dir):

```rust
#[test] fn a_stored_peer_is_read_back_after_a_load()     // store("Apple TV", p); new Pairings, load(same path): peer("Apple TV") == Some(p)
#[test] fn the_identity_is_made_once_and_kept()          // identity().id twice equal; after load in a new Pairings equal; signing key bytes equal
#[test] fn the_file_is_only_the_owners()                 // after store: metadata mode & 0o777 == 0o600
#[test] fn garbage_is_read_as_empty()                    // write "not toml"; load: peer(..) None, identity() works and rewrites the file
#[test] fn no_path_keeps_keys_in_memory()                // no load(): store Ok, peer Some, identity stable
```

- [ ] **Steps 2–4:** run (fails to compile), implement (a `Mutex` around path and data; hex by hand), pass.
- [ ] **Step 5:** `git commit -m "feat(airplay): keep AirPlay 2 pairing keys beside the state file"`

### Task 2: Pair-setup and pair-verify

**Files:**
- Create: `src/airplay/ap2/hap.rs`, `src/airplay/ap2/fake_hap.rs` (`#[cfg(test)]`, the receiver's side)
- Modify: `src/airplay/ap2/tlv.rs` (`IDENTIFIER = 1`, `ENCRYPTED_DATA = 5`, `SIGNATURE = 10`), `src/airplay/rtsp.rs` (`RtspError::NeedsPairing`, `RtspError::PairingRefused`, `RtspError::WrongPin`)

**Interfaces:**
- Consumes: `Identity`, `Peer` (Task 1); `srp::Client`, `channel::derive`, `tlv`.
- Produces:
  - `pub fn pin_start(conn: &mut Conn, headers: &[(&'static str, String)]) -> Result<(), RtspError>` — `POST /pair-pin-start`, empty body.
  - `pub struct Started` (holds the SRP secret, salt and B) and `pub fn setup_start(conn, headers) -> Result<Started, RtspError>` — M1 = State 1, Method 0.
  - `pub fn setup_finish(conn, headers, started: Started, pin: &str, me: &Identity) -> Result<Peer, RtspError>` — M3–M6 as the design lists; a missing or wrong M4 proof, or an M4 Error item, is `WrongPin`; a bad M6 signature is `Pairing("the receiver's signature is wrong")`.
  - `pub fn verify(conn, headers, me: &Identity, peer: &Peer) -> Result<[u8; 32], RtspError>` — M1–M4; an Error item or a bad signature is `PairingRefused`; gives the X25519 shared secret.
  - Nonces: 4 zero bytes then the 8 ASCII bytes `PS-Msg05`, `PS-Msg06`, `PV-Msg02`, `PV-Msg03`.
  - Fake: `pub struct FakeAccessory` with `new(pin: &str) -> Arc<FakeAccessory>`, `answer(&self, seen: &Seen) -> Option<Answer>` for `/pair-pin-start`, `/pair-setup`, `/pair-verify` (others `None`), `forget(&self)` (drops its paired controllers), and after a verify the answer is `Answer::SealAfter(.., shared)`.

- [ ] **Step 1: Failing tests** against `FakeReceiver` + `FakeAccessory`:

```rust
#[test] fn pairing_with_the_right_pin_gives_the_receivers_keys()   // pin_start, setup_start, setup_finish("1234"): Peer { id: accessory id, public: accessory key }
#[test] fn a_wrong_pin_is_told_as_such()                           // setup_finish("0000") -> Err(RtspError::WrongPin)
#[test] fn verify_after_pairing_gives_the_same_secret_on_both_sides() // verify(..) == the fake's shared secret
#[test] fn verify_with_a_forgotten_pairing_is_refused()            // forget(); verify -> Err(RtspError::PairingRefused)
#[test] fn the_m5_signature_is_over_x_id_and_key()                 // the fake checks it; a tampered identity (other signing key, same id) -> the fake answers an Error item, setup_finish -> Err(WrongPin | Pairing)
#[test] fn pair_setup_says_hkp_3()                                 // every /pair-setup and /pair-verify request carries X-Apple-HKP: 3
```

- [ ] **Steps 2–4:** run, implement, pass; check the HKDF salts and infos against `ref/pyatv/pyatv/auth/hap_srp.py`.
- [ ] **Step 5:** `git commit -m "feat(airplay): HomeKit pair-setup with a PIN, and pair-verify"`

### Task 3: Sessions use the keys

**Files:**
- Modify: `src/airplay/ap2/session.rs` (`Ap2::open` takes `pairing: Option<(&Identity, &Peer)>`), `src/airplay/output.rs` (passes `pairings.identity()` and `pairings.peer(&config.name)`; turns `NeedsPairing` and `PairingRefused` into messages with the name), `src/airplay/raop.rs` (`Setup` unchanged)

**Interfaces:**
- Consumes: Tasks 1–2.
- Produces: `output::spawn(config, now_playing, pairings: Arc<Pairings>)`; `Receiver::new(config, now_playing, pairings)`.

- [ ] **Step 1: Failing tests** in `session.rs` (fake receiver with `statusFlags` 0x644 and a `FakeAccessory`):

```rust
#[test] fn a_receiver_that_needs_pairing_and_has_no_keys_says_so()   // open(.., None) -> Err(NeedsPairing)
#[test] fn stored_keys_verify_and_the_session_plays()               // pair first via hap, then open(.., Some(keys)) -> Ok; the requests after /pair-verify are sealed (SETUP seen)
#[test] fn a_forgotten_pairing_fails_the_session()                  // forget(); open -> Err(PairingRefused)
#[test] fn a_pin_for_every_connection_is_not_done()                 // statusFlags 0x8 -> Err(Pairing("the receiver wants a PIN for every connection, which aas does not do"))
#[test] fn transient_receivers_are_unchanged()                     // statusFlags 0x4, no keys -> pair_transient as before (existing tests keep passing)
// output.rs
#[test] fn the_error_names_the_command_to_pair()                    // spawn against 0x644 with no keys: status.error == Some("the receiver needs pairing: aasctl airplay pair \"test\"")
```

- [ ] **Steps 2–4:** run, implement, pass.
- [ ] **Step 5:** `git commit -m "feat(airplay): a paired receiver is verified, not paired, at each session"`

### Task 4: Pairing on request

**Files:**
- Modify: `src/airplay/output.rs` (`Msg::Pair { pin: Option<String>, answer: oneshot::Sender<Answer> }`, a `Held { conn, headers, started, since }` behind a `Mutex` the helper thread uses), `src/airplay/mod.rs` (`Receiver::pair(&mut self, pin: Option<String>, answer)`, spawning the thread if needed; `view()` gains `paired: Option<bool>`, `None` for AirPlay 1), `src/api.rs` (`Call::PairAirplay { name: String, pin: Option<String> }`, `AirplayView::paired` with `skip_serializing_if = "Option::is_none"`), `src/control/api.rs` (`answer()` forwards `PairAirplay` with its `answer` sender; unknown name 404, AirPlay 1 422), `src/http.rs` (`POST /airplay/{name}/pair`, body `{}` or `{"pin": "1234"}`, a `fetch_within(.., PAIR_WITHIN = 40 s)`), `src/control.rs` (one `Arc<Pairings>`; `restore()` calls `pairings.load(state.with_file_name("airplay-pairing.toml"))`)

Answers: start → `{"pin": "shown"}`; finish → `{"paired": true}`. Errors: PIN not 4 digits 400 "pin is 4 digits"; no held start or older than 120 s 409 "start pairing first: aasctl airplay pair NAME"; `WrongPin` 422 "the receiver did not take the PIN"; a refused pin-start or M1 (non-200) 422 "the receiver refused pairing: allow AirPlay access for everyone on the network"; unreachable 502 with the error text.

- [ ] **Step 1: Failing tests:**

```rust
// output.rs, against FakeReceiver + FakeAccessory (0x644)
#[test] fn pairing_then_a_session_plays()           // pair(None) -> {"pin":"shown"}; pair(Some("1234")) -> {"paired":true}; pairings.peer("test").is_some(); with Connect::Always the state reaches Playing
#[test] fn a_pin_without_a_start_is_409()
#[test] fn a_wrong_pin_stores_nothing_and_a_new_start_works() // "0000" -> 422; peer None; pair(None) again -> {"pin":"shown"}
#[test] fn pairing_beside_a_playing_session_keeps_it_playing() // a second accessory connection on the same fake: state stays Playing and packets keep coming while pairing runs
// control/api.rs
#[test] fn pairing_an_unknown_receiver_is_404()
#[test] fn pairing_an_airplay1_receiver_is_422()
#[test] fn get_airplay_says_whether_an_airplay2_receiver_is_paired() // "paired": false; absent for airplay1
// http (tests/api.rs pattern)
#[test] fn a_pin_that_is_not_four_digits_is_400()
```

- [ ] **Steps 2–4:** run, implement, pass. `cargo test -p aas`, `cargo clippy --all-targets` clean.
- [ ] **Step 5:** `git commit -m "feat(airplay): pair a receiver through the API"`

### Task 5: aasctl and docs

**Files:**
- Modify: `src/bin/aasctl/args.rs` (`["airplay", "pair", name]` and `["airplay", "pair", name, pin]` → `Command::Post { path: "/airplay/<encoded>/pair", body }`), `src/bin/aasctl/client.rs` (POST, patience 45 s for it), `src/bin/aasctl/main.rs` (usage line), `README.md` (pairing paragraph under AirPlay, the API table row, the Apple TV access setting)

- [ ] **Step 1: Failing tests** in `args.rs`:

```rust
#[test] fn airplay_pair_starts()    // parse(["airplay","pair","Apple TV"]) == Command::Post { path: "/airplay/Apple%20TV/pair", body: json!({}) }
#[test] fn airplay_pair_finishes()  // with "1234": body json!({"pin": "1234"})
```

- [ ] **Steps 2–4:** run, implement, pass.
- [ ] **Step 5:** `git commit -m "feat(aasctl): airplay pair NAME [PIN]"`

### Task 6: On the Apple TV

- [ ] **Step 1:** Owner sets the Apple TV's AirPlay access to everyone on the network, no password.
- [ ] **Step 2:** Build on the build host, install on the turntable board; `aasctl airplay pair "Apple TV"`, read the PIN off the TV (ask the owner), `aasctl airplay pair "Apple TV" <PIN>`.
- [ ] **Step 3:** `sudo systemctl restart aas`; with a record playing, `GET /airplay` shows the Apple TV `playing`, no PIN asked; the session lasts a record side (log: session ended with few resends). Note whether song metadata shows.
- [ ] **Step 4:** `### AirPlay 2 pairing` in `doc/measurements.md`; `git commit -m "docs: pairing with an Apple TV"`.
