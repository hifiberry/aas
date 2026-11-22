# AirPlay 2 PIN pairing: design

## Intent

aas plays to AirPlay 2 receivers with transient pairing, which a sound bar
accepts. An Apple TV refuses it: its `/info` `statusFlags` (0x644 on the
test device) ask for one-time pairing, and the session ends before any
audio. The owner decided (2026-11-22) to add PIN pairing: the receiver shows
a PIN once, the owner enters it, aas keeps the keys, and every later session
to that receiver verifies with them on its own.

The PIN is entered through an aas command and API call (the owner's choice
of the two offered); the web UI can use the same API later.

## Goals

- `aasctl airplay pair NAME` asks the receiver named `NAME` (an `[[airplay]]`
  table's `name`) to show a PIN; `aasctl airplay pair NAME PIN` finishes.
- The keys survive restarts. A receiver with stored keys is connected to
  with pair-verify instead of transient pairing, then sealed and played to
  exactly as today.
- A receiver that needs pairing and has no keys fails with an error that
  says how to pair it, in the log and in `GET /airplay`.
- Transient pairing stays as it is for receivers that take it.

## Non-goals

- A web UI dialog (later, on the same API).
- HomeKit pairing through the Home app, or receivers whose access is limited
  to "people sharing this home". The Apple TV's AirPlay access must allow
  devices on the network ("Anyone on the same network" or "Everyone").
- A password (the AirPlay "Require Password" setting); it stays unsupported.
- Unpairing on the receiver's side. Removing aas's stored keys is enough to
  pair again.
- AirPlay 1 (RAOP) pairing.
- Receivers that want a PIN on every connection (`statusFlags` bit 3,
  `PIN_REQUIRED`) *(call)*: they fail with their own error ("the receiver
  wants a PIN for every connection, which aas does not do"); only bit 9,
  one-time pairing, is pairable.

## Anti-goals

- No secret in the log, the API answers or the config. The keys live in a
  file only aas can read.
- Pairing must not disturb a receiver that is playing, nor the audio thread.
- A wrong PIN must not lock the receiver into a state aas cannot leave: it
  fails with a clear error and can be started again.

## Constraints

- The protocol is HomeKit's (HAP) pair-setup and pair-verify over AirPlay's
  RTSP connection, as pyatv (`protocols/airplay/auth/hap.py`,
  `auth/hap_srp.py`) and OwnTone (`src/pair_ap/`) implement it:
  - `POST /pair-pin-start` makes the receiver show a 4-digit PIN.
  - `POST /pair-setup` with `X-Apple-HKP: 3` (not 4, transient), TLV8:
    M1 (Method 0, State 1) → M2 (Salt, PublicKey); M3 (PublicKey, Proof from
    SRP-6a with user `Pair-Setup` and the PIN) → M4 (Proof); M5 (State 5,
    EncryptedData) → M6 (EncryptedData).
  - M5's sub-TLV: Identifier (aas's pairing id), PublicKey (aas's Ed25519
    long-term key), Signature over `X || id || ltpk` with
    `X = HKDF-SHA512(K, "Pair-Setup-Controller-Sign-Salt",
    "Pair-Setup-Controller-Sign-Info")`; sealed with ChaCha20-Poly1305 under
    `HKDF(K, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info")`, nonce
    `PS-Msg05`. M6 opens with nonce `PS-Msg06` and gives the receiver's
    Identifier, Ed25519 PublicKey and Signature, checked with
    `HKDF(K, "Pair-Setup-Accessory-Sign-Salt", "Pair-Setup-Accessory-Sign-Info")`.
  - `POST /pair-verify`: M1 (State 1, aas's X25519 public key) → M2 (the
    receiver's X25519 key, EncryptedData under
    `HKDF(shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info")`,
    nonce `PV-Msg02`, holding its Identifier and a Signature over
    `its key || its id || our key`); M3 (EncryptedData, nonce `PV-Msg03`,
    our Identifier and Signature over `our key || our id || its key`) → M4.
    The X25519 shared secret then takes the place of the transient pairing's
    secret: `Keys::control(shared)` and `Keys::events(shared)` as today.
- aas already has TLV8, SRP-6a (3072-bit, SHA-512), HKDF-SHA512 and
  ChaCha20-Poly1305 (`src/airplay/ap2/`). New: Ed25519 and X25519, from
  `ed25519-dalek` and `x25519-dalek`.
- The receiver's thread owns its RTSP connection. M1 must come after
  pair-pin-start on the same connection, and M3 needs the PIN, which comes
  later from the owner; the connection is held open between the two
  commands.
- aas runs as a systemd `DynamicUser` with `StateDirectory=aas`; its state
  file is `--state` (`/var/lib/aas/state.toml` on HiFiBerry OS).
- `POST` under the API falls to the `risky` tier in `dist/auth.d/aas.json`,
  so through nginx pairing needs the device password; `aasctl` talks to the
  daemon directly.

## Approaches

- **Native in aas** *(chosen)*: a third pairing mode beside transient, with
  two small crypto crates. Everything else (SRP, HKDF, sealing) is reused.
- **An outside tool (pyatv's `atvremote`) to pair, aas reads its keys:**
  brings Python into the audio server's path and a key format aas would
  still have to parse; rejected.

## Design

### The keys

- aas's own identity: a pairing id (a random UUID string) and an Ed25519
  key, made the first time it pairs, one for the whole installation.
- Per receiver, by its config `name`: the receiver's pairing id and its
  Ed25519 public key.
- Stored in `airplay-pairing.toml` beside the state file *(call)*, written
  atomically with mode 0600; never in the state file itself, which the API
  can reset. A file that cannot be read is logged once and treated as empty.

### Pairing

- `aasctl airplay pair NAME` → `POST /airplay/{name}/pair` with `{}`. The
  receiver's thread opens a new RTSP connection (beside a playing session,
  if any), sends pair-pin-start and M1, keeps the connection and M2, and
  answers once the receiver shows the PIN.
- `aasctl airplay pair NAME PIN` → the same call with `{"pin": "1234"}`. The
  thread runs M3–M6 on the held connection, checks M4's proof and M6's
  signature, stores the keys, and closes the connection.
- A held connection waits at most 2 minutes for the PIN *(call)*; then it is
  dropped and a PIN gives an error saying to start again.
- Errors answer the call: unknown name (404), not an AirPlay 2 receiver
  (422), a PIN without a start (409), a wrong PIN (M4 refused or its proof
  wrong: 422 "the receiver did not take the PIN"), a receiver that cannot be
  reached (502), and a receiver that refuses pair-pin-start or M1 (422 "the
  receiver refused pairing: allow AirPlay access for everyone on the
  network").
- The pairing exchange runs in a helper thread the receiver's thread owns
  *(call)*: the SRP maths and the blocking requests must not hold up the
  packets of a playing session. The API call waits for its answer longer
  than the usual 2 s (up to the handshake's 5 s per step), and `aasctl`
  waits with it.

### Sessions

- `Ap2::open` reads `/info` as today. With stored keys for the receiver it
  runs pair-verify; without, transient pairing, or, if the flags ask for
  pairing, fails with "the receiver needs pairing: aasctl airplay pair NAME".
- A pair-verify the receiver refuses (it was reset or the pairing removed on
  its side) fails the session with "the pairing was refused: pair again";
  the stored keys are kept *(call)*, so a passing glitch does not lose them.
- `GET /airplay` gains `"paired": true|false` per AirPlay 2 receiver.

## Left to the builder

- Where in the receiver's thread the held pairing connection lives, and how
  the API call waits for its answer (a reply channel with a timeout).
- The exact error texts beyond the ones above.
- Whether pair-setup goes as RTSP/1.0 or HTTP/1.1 on the wire (the
  receivers take what the transient pairing already sends).
- Test fakes: a fake receiver's side of pair-setup with a known PIN, and of
  pair-verify, from the same crates.

## How it is confirmed

On the Apple TV from the turntable board: pair with the PIN it shows, restart aas, a
record plays to it without asking again, and song metadata shows on its
screen (the open check of the song metadata work).
