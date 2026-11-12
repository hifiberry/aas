//! Plugins: processing stages that change the audio in place.
//!
//! A plugin sees blocks of interleaved frames in the processing format. Float blocks are
//! scaled so that full scale is 1.0. Plugins are built on the control thread
//! and run on the audio thread, so `process` must not allocate, lock or
//! block.

pub mod balance;
pub mod build;
pub mod builtin;
pub mod chain;
pub mod detect;
pub mod native;

use crate::config::Format;

/// Interleaved frames in one of the processing formats, changed in place.
#[derive(Debug)]
pub struct Block<'a> {
    /// The samples in one frame.
    pub channels: usize,
    pub samples: Samples<'a>,
}

/// The samples of a block, in the processing format.
#[derive(Debug)]
pub enum Samples<'a> {
    I32(&'a mut [i32]),
    F32(&'a mut [f32]),
    F64(&'a mut [f64]),
}

impl Block<'_> {
    /// The same frames, borrowed again, so the block can be handed to
    /// several plugins in turn.
    pub fn reborrow(&mut self) -> Block<'_> {
        let samples = match &mut self.samples {
            Samples::I32(d) => Samples::I32(d),
            Samples::F32(d) => Samples::F32(d),
            Samples::F64(d) => Samples::F64(d),
        };
        Block {
            channels: self.channels,
            samples,
        }
    }

    /// The format of the samples.
    pub fn format(&self) -> Format {
        match self.samples {
            Samples::I32(_) => Format::I32,
            Samples::F32(_) => Format::F32,
            Samples::F64(_) => Format::F64,
        }
    }

    /// The number of frames: the samples divided by the channels.
    pub fn frames(&self) -> usize {
        let samples = match &self.samples {
            Samples::I32(d) => d.len(),
            Samples::F32(d) => d.len(),
            Samples::F64(d) => d.len(),
        };
        samples / self.channels
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
    pub kind: ParamKind,
    pub min: f64,
    pub max: f64,
    pub default: f64,
}

/// Which values a parameter takes, so that a control can be built for it.
/// Values are `f64` whatever the type.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamKind {
    /// Any value in the range.
    Float,
    /// Whole numbers in the range.
    Integer,
    /// 0 is off, 1 is on.
    Switch,
    /// One of the named values, numbered from 0.
    Enumeration(Vec<String>),
    /// Text, set with `Plugin::set_text`, only while a chain is built.
    Text,
}

/// Why a plugin, or a chain, could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildError {
    pub kind: Failure,
    pub message: String,
}

/// What kind of failure a `BuildError` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// A plugin does not support the rate. The rate is not used while the
    /// daemon runs.
    Unsupported,
    /// A plugin failed, for a reason that may pass, such as memory. Tried
    /// again at the next rate decision.
    Failed,
    /// The config does not fit the plugins: a missing library, an unknown
    /// parameter, a value a plugin refuses.
    Config,
}

impl BuildError {
    pub fn new(kind: Failure, message: impl Into<String>) -> BuildError {
        BuildError {
            kind,
            message: message.into(),
        }
    }

    /// The same error, its message after `prefix`.
    pub fn context(self, prefix: &str) -> BuildError {
        BuildError {
            kind: self.kind,
            message: format!("{prefix}{}", self.message),
        }
    }
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// What a plugin reports about the block it processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Status {
    /// The block carried no signal worth sending on.
    pub no_signal: bool,
    /// The plugin changed the block although it is neutral. Only input
    /// detection does, while it fades in or out or sends zeros, so the
    /// engine then takes the chain's output rather than the captured
    /// frames. Not part of the plugin ABI.
    pub changed: bool,
}

/// The channel counts a plugin takes and gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels {
    /// Any count, which it keeps. It is told the count when it is created.
    Any,
    /// `input` channels in, `output` channels out.
    Fixed { input: usize, output: usize },
}

/// A processing stage.
pub trait Plugin: Send {
    fn name(&self) -> &str;
    /// The channel counts the plugin takes and gives. Two in and two out
    /// unless it says otherwise.
    fn channels(&self) -> Channels {
        Channels::Fixed {
            input: 2,
            output: 2,
        }
    }
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
    /// Processes one block of a plugin whose channel counts differ: reads
    /// `input`, writes `output`, the same number of frames. Runs on the audio
    /// thread. Never called for a plugin whose counts are equal, which
    /// needs not implement it.
    fn process_into(&mut self, _input: Block<'_>, _output: Block<'_>) -> Status {
        unreachable!("a plugin whose channel counts are equal is processed in place")
    }
    /// Sets text parameter `index`, on the control thread while the chain
    /// is built, never later. Fails with `Failure::Config` and why, for a
    /// value the plugin refuses, or `Failure::Unsupported` for one it cannot
    /// use at its rate.
    fn set_text(&mut self, _index: usize, _text: &str) -> Result<(), (Failure, String)> {
        Err((Failure::Config, "it takes no text".into()))
    }
    /// Forgets what it kept from earlier blocks, such as a filter's state.
    /// Called when the plugin runs again after blocks that did not pass it.
    /// Runs on the audio thread, so it must not allocate.
    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_of_six_samples_in_three_channels_has_two_frames() {
        let mut d = [0.0f64; 6];
        let mut block = Block {
            channels: 3,
            samples: Samples::F64(&mut d),
        };
        assert_eq!(block.frames(), 2);
        assert_eq!(block.reborrow().frames(), 2);
        assert_eq!(block.format(), Format::F64);
    }
}
