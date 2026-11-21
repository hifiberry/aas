# The volume limit Implementation Plan

**Goal:** A ceiling on what the master plays, set in dB, that no player can get
around: not by the card's volume control, not by forced bit-perfect, not by a
DoP stream.

**Architecture:** The limit is a gain the engine applies to the master's
samples, on top of the output volume, whatever carries that volume. It is not
part of the volume's mapping from percent to dB, and it never touches the card's
control: a player that sets the card's control to 0 dB still hears the output
at the limit. The control thread folds it into the master's `SetGain`, and
into every further output's gain, and tells the engine with
`Command::SetLimited` that a limit is set. While it is:

- the master's gain applies in forced bit-perfect too, which otherwise sets
  aside every volume `aas` calculates; the clients' and the analog input's
  volumes stay set aside there;
- the master is never bit-perfect, with the new reason `limit`: the copy path
  would bypass the gain;
- a DoP stream is muted, as one is today while the matrix or the chains would
  alter it: DoP cannot be scaled, and passed through it would play at the
  card's volume, past the limit.

At 0 dB nothing changes.

**Spec:** this conversation's decisions, recorded in `doc/switch.md` ("Volume
limit"): independent of the card's control so that a player cannot override
it; a DoP stream is refused while a limit is set.

## Global Constraints

- Range -60 to 0 dB, 0 by default. Config key `volume_limit_db` at the top
  level. Through the API `limit_db` on `GET`/`PATCH /outputs/1` only; a
  further output refuses it with 422, as `volume` is refused there.
- Stored in the state file in the master card's entry, `limit_db`, as a
  `Stored<f32>`: the value applies while the config still says what it said
  when the value was stored, as for a further output's `volume_db`.
- A change fades over 10 ms, as every `SetGain` does.
- Bit-perfect reason code 8, name `"limit"`. Codes 1 to 7 keep their numbers.
- Test first; each new test seen failing with the line it covers removed.

## Tasks

1. `src/bitperfect.rs`: `NotBitPerfect::Limit`, `Situation::limited`; `decide`
   gives `Limit` after the source count, in every mode but `Off`.
2. `src/volume.rs`: `OutputVolume::set_limit(db)`, `limit_db()`, `limited()`;
   `gain()` and `further_gain()` multiply by the limit's gain, `gain()` also in
   forced bit-perfect and in hardware mode.
3. `src/engine.rs`, `src/output_mix.rs`: `Command::SetLimited(bool)`. The
   master's gain applies when volumes apply or a limit is set (`play_pcm`, and
   `mix_shared` for the master in `mix_outputs`); `situation()` passes
   `limited`; `OutputMix::routing_for_dop` gives none while limited.
4. `src/config.rs`: `volume_limit_db`, checked like the other dB keys.
5. `src/state.rs`: `StoredOutput::limit_db`, dropped alone when out of range.
6. `src/control.rs`, `src/control/api.rs`, `src/api.rs`: restore the limit,
   send `SetLimited` with the first gain at `start`, `limit_db` in the master's
   view, its event and its patch; a change re-sends every output's gain, is
   published and saved.
7. `README.md`, `doc/switch.md`, `TODO.md`.

## Not in this part

- The web UI (`hbos-ui`): show the control under `aas` and drop it from the
  list of what stops working. `aas-config` carrying PipeWire's master gain over
  on a switch.
