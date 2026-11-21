//! AirPlay 2 as PipeWire's RAOP sender speaks it to third-party receivers:
//! transient pairing with the PIN 3939, the encrypted control channel, the
//! session set up with binary plists, and audio sealed with ChaCha20-Poly1305.

pub mod audio;
pub mod bplist;
pub mod channel;
pub mod events;
pub mod pair;
pub mod session;
pub mod srp;
pub mod tlv;
