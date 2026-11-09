//! Shared by the aas daemon and the ALSA plugin: the sample ring placed in
//! shared memory, client sample conversion and the control protocol.

pub mod client;
pub mod convert;
pub mod proto;
pub mod ring;
