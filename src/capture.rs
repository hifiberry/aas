//! Capture: where the analog input's frames come from.
//!
//! The control thread opens a capture and hands it to the audio thread,
//! which reads one period per cycle. No call waits: the cycle is paced by
//! the playback card, and the capture only gives what it already has.

use crate::sink::SinkError;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

/// Why a capture could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// It may open later: the card is not there yet, or another program
    /// holds it. Tried again every second.
    Again(String),
    /// It never will: the card has no analog input aas knows. The analog
    /// input stays off from then on.
    Never(String),
}

impl From<String> for OpenError {
    fn from(why: String) -> OpenError {
        OpenError::Again(why)
    }
}

impl From<&str> for OpenError {
    fn from(why: &str) -> OpenError {
        OpenError::Again(why.into())
    }
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Again(why) | OpenError::Never(why) => f.write_str(why),
        }
    }
}

/// A device that delivers S32 stereo frames. None of the calls waits.
pub trait Capture: Send {
    /// Frames queued for reading.
    fn available(&mut self) -> Result<usize, SinkError>;
    /// Reads up to `out.len()` frames into `out` and returns how many.
    fn read(&mut self, out: &mut [[i32; 2]]) -> Result<usize, SinkError>;
    /// Drops `frames` queued frames.
    fn skip(&mut self, frames: usize) -> Result<(), SinkError>;
    /// Stops the stream and lets go of the device. Called on the audio
    /// thread before the output reopens, so it frees nothing it can avoid
    /// freeing, and on the control thread for a capture that failed. The
    /// capture itself is dropped on the control thread.
    fn close(&mut self);
}

#[derive(Default)]
struct Shared {
    frames: VecDeque<[i32; 2]>,
    fail: bool,
    closed: bool,
}

/// A capture for tests. Frames come from its `CaptureFeed`.
pub struct MemoryCapture(Arc<Mutex<Shared>>);

/// The test's side of a `MemoryCapture`.
#[derive(Clone)]
pub struct CaptureFeed(Arc<Mutex<Shared>>);

impl MemoryCapture {
    pub fn new() -> (MemoryCapture, CaptureFeed) {
        let shared = Arc::new(Mutex::new(Shared {
            // Room for a while, so that a test that pushes during a check
            // for allocation does not grow the queue.
            frames: VecDeque::with_capacity(1 << 16),
            ..Shared::default()
        }));
        (MemoryCapture(shared.clone()), CaptureFeed(shared))
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.0.lock().unwrap()
    }

    /// Fails, and sticks to it, once the feed has asked for it.
    fn check(&self) -> Result<MutexGuard<'_, Shared>, SinkError> {
        let shared = self.shared();
        if shared.fail {
            return Err(SinkError::Gone);
        }
        Ok(shared)
    }
}

impl Capture for MemoryCapture {
    fn available(&mut self) -> Result<usize, SinkError> {
        Ok(self.check()?.frames.len())
    }

    fn read(&mut self, out: &mut [[i32; 2]]) -> Result<usize, SinkError> {
        let mut shared = self.check()?;
        let n = out.len().min(shared.frames.len());
        for (o, f) in out.iter_mut().zip(shared.frames.drain(..n)) {
            *o = f;
        }
        Ok(n)
    }

    fn skip(&mut self, frames: usize) -> Result<(), SinkError> {
        let mut shared = self.check()?;
        let n = frames.min(shared.frames.len());
        shared.frames.drain(..n);
        Ok(())
    }

    fn close(&mut self) {
        self.shared().closed = true;
    }
}

impl CaptureFeed {
    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.0.lock().unwrap()
    }

    /// Queues frames for the capture to deliver.
    pub fn push(&self, frames: &[[i32; 2]]) {
        self.shared().frames.extend(frames.iter().copied());
    }

    /// Makes every capture call from now on fail with `SinkError::Gone`.
    pub fn fail(&self) {
        self.shared().fail = true;
    }

    /// True once the capture has been closed.
    pub fn closed(&self) -> bool {
        self.shared().closed
    }

    /// Frames queued and not yet read or dropped.
    pub fn queued(&self) -> usize {
        self.shared().frames.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_pushed_are_read_in_order_and_skipped() {
        let (mut c, feed) = MemoryCapture::new();
        feed.push(&[[1, -1], [2, -2], [3, -3], [4, -4]]);
        assert_eq!(c.available(), Ok(4));
        c.skip(1).unwrap();
        let mut out = [[0; 2]; 2];
        assert_eq!(c.read(&mut out), Ok(2));
        assert_eq!(out, [[2, -2], [3, -3]]);
        let mut out = [[0; 2]; 4];
        assert_eq!(c.read(&mut out), Ok(1));
        assert_eq!(out[0], [4, -4]);
        assert_eq!(feed.queued(), 0);
    }

    #[test]
    fn a_failed_capture_fails_every_call_and_records_a_close() {
        let (mut c, feed) = MemoryCapture::new();
        feed.push(&[[1, 1]]);
        feed.fail();
        assert_eq!(c.available(), Err(SinkError::Gone));
        assert_eq!(c.read(&mut [[0; 2]]), Err(SinkError::Gone));
        assert_eq!(c.skip(1), Err(SinkError::Gone));
        assert!(!feed.closed());
        c.close();
        assert!(feed.closed());
    }
}
