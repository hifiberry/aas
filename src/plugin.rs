//! Plugins: processing stages that change the audio in place.
//!
//! A plugin sees stereo blocks in the processing format. Float blocks are
//! scaled so that full scale is 1.0. Plugins are built on the control thread
//! and run on the audio thread, so `process` must not allocate, lock or
//! block.

pub mod balance;
pub mod builtin;
pub mod chain;
pub mod detect;
pub mod native;

use crate::config::Format;

/// Stereo frames in one of the processing formats, changed in place.
#[derive(Debug)]
pub enum Block<'a> {
    I32(&'a mut [[i32; 2]]),
    F32(&'a mut [[f32; 2]]),
    F64(&'a mut [[f64; 2]]),
}

impl Block<'_> {
    /// The same frames, borrowed again, so the block can be handed to
    /// several plugins in turn.
    pub fn reborrow(&mut self) -> Block<'_> {
        match self {
            Block::I32(d) => Block::I32(d),
            Block::F32(d) => Block::F32(d),
            Block::F64(d) => Block::F64(d),
        }
    }

    /// The format of the samples.
    pub fn format(&self) -> Format {
        match self {
            Block::I32(_) => Format::I32,
            Block::F32(_) => Format::F32,
            Block::F64(_) => Format::F64,
        }
    }
}

/// The formats a plugin takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Formats {
    pub i32: bool,
    pub f32: bool,
    pub f64: bool,
}

impl Formats {
    /// True if the plugin takes `format`.
    pub fn has(self, format: Format) -> bool {
        match format {
            Format::I32 => self.i32,
            Format::F32 => self.f32,
            Format::F64 => self.f64,
        }
    }
}

/// One parameter a plugin can be given.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub unit: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
}

/// What a plugin reports about the block it processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Status {
    /// The block carried no signal worth sending on.
    pub no_signal: bool,
}

/// A processing stage.
pub trait Plugin: Send {
    fn name(&self) -> &str;
    /// The formats `process` takes. A chain converts the others.
    fn formats(&self) -> Formats;
    fn params(&self) -> &[Param];
    /// Sets parameter `index`. Called between blocks, never during one.
    fn set_param(&mut self, index: usize, value: f64);
    /// True if, with the parameters it has now, the plugin leaves every
    /// block as it is. It depends on the parameters, not on the audio.
    fn neutral(&self) -> bool;
    /// How many frames the plugin delays the audio by.
    fn latency_frames(&self) -> u32;
    /// Processes one block in place. Runs on the audio thread.
    fn process(&mut self, block: Block<'_>) -> Status;
}
