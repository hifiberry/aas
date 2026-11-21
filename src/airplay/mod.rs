//! The AirPlay output: the master's mix sent to AirPlay receivers named in
//! the config file. See `doc/plans/2026-11-21-airplay-output.md`.

pub mod config;
pub mod gate;
pub mod raop;
pub mod rtp;
pub mod rtsp;
pub mod stream;

#[cfg(test)]
mod fake;
