//! ALSA plugins for aas. libasound loads the library for `type aas`: as a
//! PCM it plays through the daemon, as a control it sets the daemon's
//! output volume.

pub mod ctl;
pub mod ffi;
pub mod pcm;
