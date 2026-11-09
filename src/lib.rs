//! Alternative audio server.

pub mod bitperfect;
pub mod board;
pub mod config;
pub mod control;
pub mod counters;
pub mod dop;
pub mod engine;
pub mod format;
pub mod latency;
pub mod rate;
pub mod resample;
pub mod rt;
pub mod server;
pub mod sink;
pub mod sink_alsa;

// Makes `assert_no_alloc` see every allocation in the unit tests.
#[cfg(test)]
#[global_allocator]
static ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
