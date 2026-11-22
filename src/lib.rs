//! Alternative audio server.

pub mod adaptive;
pub mod airplay;
pub mod api;
pub mod bitperfect;
pub mod board;
pub mod capture;
pub mod cards;
pub mod clock_sink;
pub mod config;
pub mod control;
pub mod counters;
pub mod dop;
pub mod drift;
pub mod engine;
pub mod format;
pub mod further;
pub mod http;
pub mod latency;
pub mod levels;
pub mod matrix;
pub mod output_mix;
pub mod plugin;
pub mod pulse;
pub mod rate;
pub mod resample;
pub mod rt;
pub mod server;
pub mod sink;
pub mod sink_alsa;
pub mod state;
pub mod volume;

// Makes `assert_no_alloc` see every allocation in the unit tests.
#[cfg(test)]
#[global_allocator]
static ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
