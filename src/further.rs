//! A further output: the feed the audio thread fills, and the writer thread
//! that plays it to the output's own card in time with the master.
//!
//! The audio thread pushes one master period a cycle into the output's
//! [`Feed`], a wait-free ring, and never waits for anything of the writer's.
//! The [`Writer`] runs on its own thread, `aas-out-N`, at
//! [`rt::WRITER_PRIORITY`]. It waits for its card, takes what one card
//! period needs from the feed, resamples it to the card through an
//! [`Adaptive`] and writes it. A [`Drift`] moves the resampler's ratio so
//! that the path's total delay stays on the target the control thread set,
//! which keeps the output in time with the master.
//!
//! # The measurement
//!
//! The delay is read at one instant, the writer's read time `t` just after
//! its wait, as the sum of three parts in frames at the master's rate. The
//! card is read after the commands, and what it played between `t` and the
//! moment its reading returned is added back.
//!
//! - the feed: the frames pushed less the frames taken, plus the master
//!   frames made since the last push, at most that push's length. The
//!   master hands over a period at a time; counting what it has made since
//!   keeps this part from jumping at each push.
//! - the resampler: its filter's delay, and the fraction of a frame it has
//!   taken beyond the position of its next output, which rubato keeps to
//!   itself and the writer follows from the ratios it set.
//! - the card: the frames queued in it at `t` (`Sink::delay_now`, which
//!   reads the card's status and moves it on by the time since its
//!   timestamp).
//!
//! Moving frames from the feed through the resampler into the card leaves
//! the sum as it is, so the sum moves only as the card's clock runs from
//! the master's. It carries no period of quantisation: the controller sees
//! the drift, a frame of rounding and the jitter of the timestamps.
//!
//! A push is stamped when the audio thread makes it, late by that thread's
//! wake-up latency. The writer runs a line through the times of the pushes
//! against the frames pushed, which follows the master's clock in phase and
//! in rate over 2 s, and measures from the line. The jitter of
//! the latency averages out; its mean, some tens of µs, stays as a constant
//! offset of a few frames. Simulated with pushes 20 to 150 µs late and the
//! writer waking 20 to 300 µs late, the ratio moves 4 to 8 ppm peak to peak
//! once settled, at 44.1 to 192 kHz and 64 to 1024 frames.
//!
//! A reading taken after the master's next push was due is short by the
//! frames not pushed yet, which says nothing about the clocks: the drift
//! does not see it, and the ratio holds.
//!
//! # Priming
//!
//! The writer primes: it writes zero periods to the card while the path
//! holds less than its target, and drops the oldest frames of the feed
//! while it holds more. When less than a card period is missing, the
//! missing frames go out as zeros in front of the first resampled frames,
//! so that the path starts on its target to the frame, and the drift's
//! first reading is that of the path with the zeros in.
//!
//! After an open, a reconfiguration or a card lost or back, the clocks
//! under the path are new: the drift starts over, and the resampler
//! empties. After a starve, an overflow, a stall or an xrun they are the
//! same: the resampler empties and the drift only starts its reading
//! afresh, keeping its integral, its lock and its estimate, so the ratio
//! stays where the card needs it and the lock is not lost.
//!
//! A starve plays a period of zeros and drops what is left in the feed. A
//! full feed refuses the new period and counts an overflow; the writer sees
//! it at its next cycle and drops the whole feed, as it does after a stall
//! longer than the feed lasts: all of it is from before the gap. Old audio
//! is never played twice, nor late.
//!
//! # A card that goes
//!
//! The `FallbackSink` plays to its null sink meanwhile, which keeps time
//! like a card and tells its queue through `delay_now`, so the writer
//! primes again and goes on taking the feed. The control thread hears of
//! it once through a [`FurtherEvent`] and looks for the card through the
//! output's `CardSearch`.
//!
//! # Threads
//!
//! The control thread talks to the writer through [`FurtherHandle`]: its
//! commands go through a ring the writer reads after each wait, as the
//! engine reads its own. Everything the writer replaces goes back through a
//! second ring and is dropped on the control thread, and its events through
//! a third. The writer allocates only to open its card.

use crate::adaptive::Adaptive;
use crate::counters::{OutputCounters, monotonic_ns};
use crate::drift::{Drift, SANE};
use crate::rt;
use crate::sink::{CardSearch, CardWatch, FallbackSink, Sink, SinkError};
use rtrb::{Consumer, Producer, PushError, RingBuffer};
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicU64, fence};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Master periods the feed holds.
pub const FEED_PERIODS: usize = 4;
/// Periods buffered in a further output's card.
pub const BUFFERED: u32 = 3;
/// Commands the writer has not read yet.
const COMMANDS: usize = 16;
/// Replaced paths the control thread has not dropped yet. More than can be
/// in flight, as the control thread drops them before each command.
const RETURNS: usize = 2 * COMMANDS;
/// Events the control thread has not read yet. More are dropped.
const EVENTS: usize = 64;
/// How long a closed writer sleeps unless a command wakes it.
const PARK: Duration = Duration::from_millis(100);
/// Seconds over which the line through the pushes follows them. The jitter
/// of the pushes averages out over that time.
const LINE: f64 = 2.0;
/// How far beyond a push length a push may come off the line before the
/// line starts again from it: the wake-up jitter of a loaded board, which
/// at short periods is more than a period.
const LINE_SLACK_NS: f64 = 2e6;
/// How far the master's clock may run from CLOCK_MONOTONIC.
const CLOCKS: f64 = 0.005;

/// Where the feed and the writer read the time, in ns.
#[derive(Clone)]
pub(crate) enum Clock {
    /// CLOCK_MONOTONIC, the clock of the card's timestamps.
    Monotonic,
    /// A clock the tests move.
    #[cfg(test)]
    Virtual(Arc<AtomicU64>),
}

impl Clock {
    fn now(&self) -> u64 {
        match self {
            Clock::Monotonic => monotonic_ns(),
            #[cfg(test)]
            Clock::Virtual(now) => now.load(Acquire),
        }
    }
}

/// The last push, as the writer reads it.
#[derive(Debug, Clone, Copy)]
struct Push {
    /// Frames pushed since the feed was made.
    frames: u64,
    /// When, in ns.
    at_ns: u64,
    /// Frames in that push.
    last: u64,
}

/// What the feed and the writer share. The last push is a sequence lock:
/// the audio thread writes it without waiting, and the writer reads it
/// again if the audio thread wrote meanwhile.
#[derive(Default)]
struct Shared {
    sequence: AtomicU64,
    frames: AtomicU64,
    at_ns: AtomicU64,
    last: AtomicU64,
    /// The writer plays. While it does not, the feed takes nothing.
    open: AtomicBool,
}

impl Shared {
    /// Only the feed calls this.
    fn publish(&self, push: Push) {
        let sequence = self.sequence.load(Relaxed);
        self.sequence.store(sequence + 1, Relaxed);
        fence(Release);
        self.frames.store(push.frames, Relaxed);
        self.at_ns.store(push.at_ns, Relaxed);
        self.last.store(push.last, Relaxed);
        self.sequence.store(sequence + 2, Release);
    }

    fn read(&self) -> Push {
        loop {
            let before = self.sequence.load(Acquire);
            if before % 2 == 0 {
                let push = Push {
                    frames: self.frames.load(Relaxed),
                    at_ns: self.at_ns.load(Relaxed),
                    last: self.last.load(Relaxed),
                };
                fence(Acquire);
                if self.sequence.load(Relaxed) == before {
                    return push;
                }
            }
            std::hint::spin_loop();
        }
    }
}

/// The audio thread's side of a further output. Every call returns at once.
pub struct Feed {
    ring: Producer<i32>,
    channels: usize,
    shared: Arc<Shared>,
    counters: Arc<OutputCounters>,
    clock: Clock,
    /// Frames pushed since the feed was made.
    pushed: u64,
}

impl Feed {
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The time on the feed's clock, in ns: what a cycle's pushes are
    /// stamped with, read once when the master's wait returns.
    pub fn now(&self) -> u64 {
        self.clock.now()
    }

    /// Hands a master period of interleaved frames to the writer, stamped
    /// now. A full feed takes none of it, counts an overflow and returns
    /// false. While the output is closed the period is thrown away and
    /// this returns true. Wait-free; does not allocate.
    pub fn push(&mut self, samples: &[i32]) -> bool {
        let now = self.now();
        self.push_at(samples, now)
    }

    /// `push`, stamped `at_ns` on the feed's clock. The engine stamps each
    /// push with the time its cycle woke, when the master's wait returned:
    /// the time of the push itself is later by the work of the cycle up to
    /// it, which would shift the writer's line through the pushes by a
    /// constant.
    pub fn push_at(&mut self, samples: &[i32], at_ns: u64) -> bool {
        debug_assert_eq!(samples.len() % self.channels, 0);
        self.put(samples.len(), samples.iter().copied(), at_ns)
    }

    /// `push` for a period of `frames` zeros.
    pub fn silence(&mut self, frames: u32) -> bool {
        let now = self.now();
        self.silence_at(frames, now)
    }

    /// `push_at` for a period of `frames` zeros.
    pub fn silence_at(&mut self, frames: u32, at_ns: u64) -> bool {
        let samples = frames as usize * self.channels;
        self.put(samples, std::iter::repeat_n(0, samples), at_ns)
    }

    fn put(&mut self, samples: usize, from: impl Iterator<Item = i32>, at_ns: u64) -> bool {
        if samples == 0 || !self.shared.open.load(Acquire) {
            return true;
        }
        let Ok(chunk) = self.ring.write_chunk_uninit(samples) else {
            self.counters.overflows.fetch_add(1, Relaxed);
            return false;
        };
        chunk.fill_from_iter(from);
        let frames = (samples / self.channels) as u64;
        self.pushed += frames;
        self.shared.publish(Push {
            frames: self.pushed,
            at_ns,
            last: frames,
        });
        true
    }
}

/// A line through the times of the master's pushes against the frames
/// pushed, which follows the master's clock in phase and in rate. A push
/// comes late by the audio thread's wake-up latency; the line runs through
/// the mean of that, with the jitter left out.
#[derive(Debug, Clone, Copy)]
struct Line {
    /// The frames pushed at the last push seen.
    frames: u64,
    /// Where the line has that push, ns.
    at: f64,
    /// The length of a master frame on CLOCK_MONOTONIC, ns.
    frame_ns: f64,
}

impl Line {
    fn start(push: Push, rate: f64) -> Line {
        Line {
            frames: push.frames,
            at: push.at_ns as f64,
            frame_ns: 1e9 / rate,
        }
    }

    /// The line moved by a push, or `None` for a push off the line by more
    /// than its length and [`LINE_SLACK_NS`], or with a count from before,
    /// from which the line starts again.
    fn follow(self, push: Push, rate: f64) -> Option<Line> {
        if push.frames == self.frames {
            return Some(self);
        }
        if push.frames < self.frames {
            return None;
        }
        let span = (push.frames - self.frames) as f64;
        let expected = self.at + span * self.frame_ns;
        let late = push.at_ns as f64 - expected;
        if late.abs() >= push.last as f64 * self.frame_ns + LINE_SLACK_NS {
            return None;
        }
        // A critically damped loop on the phase and the rate, with gains
        // for the time since the last push seen.
        let follow = (span * self.frame_ns / 1e9 / LINE).min(0.25);
        let nominal = 1e9 / rate;
        let frame_ns = (self.frame_ns + follow * follow / 4.0 * late / span)
            .clamp(nominal * (1.0 - CLOCKS), nominal * (1.0 + CLOCKS));
        Some(Line {
            frames: push.frames,
            at: expected + follow * late,
            frame_ns,
        })
    }
}

/// What the writer tells the control thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FurtherEvent {
    /// The card has gone; the output plays to its null sink.
    Gone,
    /// The card plays again.
    Back,
    /// The path's delay has stayed on its target for the lock time.
    Locked { ppm: f64 },
    /// It no longer is, or the writer started over.
    Unlocked,
    /// The card needs more than the ratio's clamp allows.
    Clamped,
    /// The feed held too little for a period, which played as zeros.
    Starved,
    /// The feed was full and refused a master period.
    Overflowed,
}

/// What a writer plays with at one configuration. Made on the control
/// thread, and dropped there when replaced.
struct Path {
    master_rate: u32,
    card_rate: u32,
    /// The card's period.
    period: u32,
    /// The path's delay to hold, master frames.
    target: f64,
    resampler: Box<Adaptive>,
    drift: Drift,
    input: Vec<i32>,
    output: Vec<i32>,
}

impl Path {
    /// Master frames per card frame.
    fn scale(&self) -> f64 {
        f64::from(self.master_rate) / f64::from(self.card_rate)
    }
}

enum Command {
    Reconfigure(Box<Path>),
    Open(bool),
    Stop,
}

/// Plays a further output's feed to its card, one card period a cycle.
pub struct Writer {
    sink: FallbackSink,
    ring: Consumer<i32>,
    channels: usize,
    shared: Arc<Shared>,
    counters: Arc<OutputCounters>,
    clock: Clock,
    commands: Consumer<Command>,
    returns: Producer<Box<Path>>,
    events: Producer<FurtherEvent>,
    watch: CardWatch,
    search: CardSearch,
    path: Option<Box<Path>>,
    /// The control thread wants the output open.
    open: bool,
    /// Open with a path: the card is open and the feed takes frames.
    playing: bool,
    /// Past priming: the path is on its target and the drift runs.
    running: bool,
    stopped: bool,
    /// Frames taken from the feed, or dropped, since it was made.
    taken: u64,
    /// Input frames the resampler holds beyond the exact position of its
    /// next output frame, less what it holds after a reset: up to a frame,
    /// which rubato keeps as a fraction of its own.
    held: f64,
    /// The line through the pushes.
    line: Option<Line>,
    last_wake: Option<u64>,
    /// The card is gone, as last told.
    gone: bool,
    locked: bool,
    clamped: bool,
    /// Overflows seen.
    overflows: u64,
    /// The ratio the drift last gave, kept over a re-prime.
    ratio: f64,
    /// The ratio the resampler last ran at: 1 after its reset.
    applied: f64,
    /// The last reading came after the master's next push was due: frames
    /// it has not pushed are missing from it, which says nothing about the
    /// clocks, so the drift does not see it.
    overdue: bool,
}

impl Writer {
    /// One card period: waits for room, reads the commands, measures the
    /// path, and writes a period of the feed or of zeros. Does not
    /// allocate, except to open the card. An error is an xrun, counted.
    pub fn cycle(&mut self) -> Result<(), SinkError> {
        if self.stopped {
            return Ok(());
        }
        // The wait comes before the commands are read, so that one sent
        // during it takes effect in this cycle.
        let waited = if self.playing {
            self.sink.wait_room()
        } else {
            Ok(())
        };
        let now = self.clock.now();
        self.drain_commands();
        if !self.playing {
            self.discard();
            return Ok(());
        }
        if let Err(e) = waited {
            self.failed(e);
        }
        if self.count_wake(now) > self.feed_ns() {
            // The writer stalled longer than the feed lasts: what it holds
            // is from before the gap.
            self.discard();
            self.reprime();
        }
        self.watch_card();
        self.watch_overflows();
        let played = self.play(now);
        self.report();
        waited.and(played)
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Whether the output is open and has its card, or its null sink.
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    fn drain_commands(&mut self) {
        while !self.stopped {
            let Ok(command) = self.commands.pop() else {
                break;
            };
            match command {
                Command::Reconfigure(path) => {
                    if let Some(old) = self.path.replace(path) {
                        self.retire(old);
                    }
                    // Another master rate draws another line.
                    self.line = None;
                    if self.open {
                        self.start();
                    }
                }
                Command::Open(open) => {
                    self.open = open;
                    if open {
                        self.start();
                    } else {
                        self.halt();
                    }
                }
                Command::Stop => {
                    self.halt();
                    self.stopped = true;
                }
            }
        }
    }

    fn retire(&mut self, old: Box<Path>) {
        if let Err(PushError::Full(old)) = self.returns.push(old) {
            // Cannot happen: the control thread empties the ring before
            // each command. Keeping the memory beats freeing it here.
            std::mem::forget(old);
        }
    }

    /// Opens the card at the path's format, empties the feed of what it
    /// held from before, and primes.
    fn start(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        // A card that does not open plays to the null sink meanwhile.
        let _ = self.sink.open(path.card_rate, path.period);
        self.discard();
        self.shared.open.store(true, Release);
        self.playing = true;
        self.last_wake = None;
        self.restart();
    }

    fn halt(&mut self) {
        if self.playing {
            self.sink.close();
        }
        self.playing = false;
        self.shared.open.store(false, Release);
        self.discard();
        self.last_wake = None;
        self.restart();
    }

    /// Takes everything in the feed and throws it away.
    fn discard(&mut self) {
        let samples = self.ring.slots();
        self.drop_samples(samples);
    }

    /// Throws away the oldest `frames` of the feed.
    fn skip(&mut self, frames: usize) {
        self.drop_samples(frames * self.channels);
    }

    fn drop_samples(&mut self, samples: usize) {
        if samples == 0 {
            return;
        }
        if let Ok(chunk) = self.ring.read_chunk(samples) {
            chunk.commit_all();
            self.taken += (samples / self.channels) as u64;
        }
    }

    /// Starts over on another card, clock or configuration: the drift
    /// forgets all, the resampler empties, and the writer primes.
    fn restart(&mut self) {
        self.reprime();
        self.ratio = 1.0;
        if let Some(path) = &mut self.path {
            path.drift.reset();
        }
    }

    /// Primes again after a starve, an overflow, a stall or an xrun, with
    /// the clocks as they were: the resampler empties, the drift starts its
    /// reading afresh and keeps its integral, its lock and its estimate.
    fn reprime(&mut self) {
        self.running = false;
        self.held = 0.0;
        self.applied = 1.0;
        if let Some(path) = &mut self.path {
            path.drift.reprime();
            path.resampler.reset();
        }
    }

    fn emit(&mut self, event: FurtherEvent) {
        let _ = self.events.push(event);
    }

    /// A wait or a write failed: an xrun, which the card has recovered
    /// from with zeros of its own.
    fn failed(&mut self, error: SinkError) {
        if error == SinkError::Xrun {
            self.counters.xruns.fetch_add(1, Relaxed);
        }
        self.reprime();
    }

    /// Counts a late wake-up and returns the time since the last, ns.
    fn count_wake(&mut self, now: u64) -> u64 {
        let mut gap = 0;
        if let (Some(path), Some(last)) = (&self.path, self.last_wake) {
            gap = now.saturating_sub(last);
            self.counters.worst_gap_ns.fetch_max(gap, Relaxed);
            let buffer = u64::from(BUFFERED * path.period) * 1_000_000_000;
            if gap > buffer / u64::from(path.card_rate) {
                self.counters.wake_gaps.fetch_add(1, Relaxed);
            }
        }
        self.last_wake = Some(now);
        gap
    }

    /// How long a full feed lasts, ns.
    fn feed_ns(&self) -> u64 {
        let frames = (self.ring.buffer().capacity() / self.channels) as u64;
        let rate = self.path.as_ref().map_or(1, |p| p.master_rate);
        frames * 1_000_000_000 / u64::from(rate)
    }

    /// The card gone or back puts another clock under the path, the null
    /// sink's or the card's opened anew: the writer starts over, and the
    /// control thread hears of it once.
    fn watch_card(&mut self) {
        if !self.watch.changed() {
            return;
        }
        let gone = self.search.is_gone();
        if gone != self.gone {
            self.gone = gone;
            self.emit(if gone {
                FurtherEvent::Gone
            } else {
                FurtherEvent::Back
            });
        }
        self.restart();
    }

    fn watch_overflows(&mut self) {
        let overflows = self.counters.overflows.load(Relaxed);
        if overflows != self.overflows {
            self.overflows = overflows;
            self.emit(FurtherEvent::Overflowed);
            // The writer fell behind: all the feed holds is from before
            // the gap.
            self.discard();
            self.reprime();
        }
    }

    fn play(&mut self, now: u64) -> Result<(), SinkError> {
        let card = match self.sink.delay_now() {
            Ok(frames) => frames,
            Err(e) => {
                self.failed(e);
                return Err(e);
            }
        };
        // The card is read as of the moment `delay_now` returned, after the
        // commands and the card's own call; at `now` it held what it has
        // played since, too.
        let read_ns = self.clock.now().saturating_sub(now);
        let rate = self.path.as_ref().map_or(0, |p| p.card_rate);
        let card = f64::from(card) + read_ns as f64 * f64::from(rate) / 1e9;
        let delay = self.measure(now, card);
        if self.running {
            self.run(delay, 0)
        } else {
            self.prime(delay)
        }
    }

    /// The path's delay at `now` in master frames, with `card` frames
    /// queued in the card.
    fn measure(&mut self, now: u64, card: f64) -> f64 {
        let push = self.shared.read();
        let since = self.since_push(push, now);
        let path = self.path.as_ref().expect("playing has a path");
        let feed = push.frames as f64 - self.taken as f64 + self.held + since;
        feed + (path.resampler.delay_frames() + card) * path.scale()
    }

    /// Master frames made since the last push, from the line through the
    /// pushes, at most that push's length: a master that has stopped
    /// pushing has stopped making frames for this output.
    fn since_push(&mut self, push: Push, now: u64) -> f64 {
        self.overdue = false;
        if push.frames == 0 {
            return 0.0;
        }
        let rate = f64::from(self.path.as_ref().map_or(1, |p| p.master_rate));
        let line = self
            .line
            .and_then(|line| line.follow(push, rate))
            .unwrap_or_else(|| Line::start(push, rate));
        self.line = Some(line);
        let since = (now as f64 - line.at) / line.frame_ns;
        self.overdue = since > push.last as f64;
        since.clamp(0.0, push.last as f64)
    }

    /// Zeros until the path holds its target, dropping the oldest frames
    /// of the feed while it holds more.
    fn prime(&mut self, mut delay: f64) -> Result<(), SinkError> {
        let path = self.path.as_ref().expect("playing has a path");
        let (target, scale, period) = (path.target, path.scale(), path.period as f64);
        if delay > target + 1.0 {
            let held = self.ring.slots() / self.channels;
            let over = ((delay - target).floor() as usize).min(held);
            self.skip(over);
            delay -= over as f64;
        }
        // Card frames missing.
        let missing = (target - delay) / scale;
        if missing >= period - 0.5 {
            return self.write(true);
        }
        self.running = true;
        // The drift reads the delay the card holds once the zeros are in.
        let zeros = missing.round().max(0.0) as usize;
        self.run(delay + zeros as f64 * scale, zeros)
    }

    /// A period of the feed through the resampler, after `zeros` frames of
    /// zeros.
    fn run(&mut self, delay: f64, zeros: usize) -> Result<(), SinkError> {
        let channels = self.channels;
        let path = self.path.as_deref_mut().expect("playing has a path");
        let period = path.period as usize;
        if !self.overdue {
            self.ratio = path.drift.update(delay);
        }
        path.resampler.set_ratio(self.ratio);
        let out = period - zeros;
        let need =
            (path.resampler.input_needed(out as u32) as usize).min(path.input.len() / channels);
        if self.ring.slots() / channels < need {
            return self.starve();
        }
        let samples = need * channels;
        // Copied, not taken yet: the resampler may take a frame less.
        let chunk = self.ring.read_chunk(samples);
        let (first, second) = chunk.as_ref().expect("the slots were counted").as_slices();
        path.input[..first.len()].copy_from_slice(first);
        path.input[first.len()..samples].copy_from_slice(second);
        let (silent, sound) = path.output[..period * channels].split_at_mut(zeros * channels);
        silent.fill(0);
        let (consumed, produced) = path.resampler.process(&path.input[..samples], sound);
        if produced != out {
            return self.starve();
        }
        let used = self.ring.read_chunk(consumed * channels);
        used.expect("taken from what was there").commit_all();
        self.taken += consumed as u64;
        // The ratio ramps from the last one to this one over the call, so
        // the position moves by the output over their mean.
        let mean = 0.5 * (self.applied + self.ratio) / path.scale();
        self.held += consumed as f64 - produced as f64 / mean;
        self.applied = self.ratio;
        self.write(false)
    }

    /// Too little in the feed: a period of zeros, and priming again. What
    /// is left in the feed goes too: it was made before the gap, which may
    /// last.
    fn starve(&mut self) -> Result<(), SinkError> {
        self.discard();
        self.counters.starves.fetch_add(1, Relaxed);
        self.emit(FurtherEvent::Starved);
        self.reprime();
        self.write(true)
    }

    /// Writes the output buffer, or a period of zeros.
    fn write(&mut self, zeros: bool) -> Result<(), SinkError> {
        let path = self.path.as_deref_mut().expect("playing has a path");
        let samples = &mut path.output[..path.period as usize * self.channels];
        if zeros {
            samples.fill(0);
        }
        let result = self.sink.write(samples);
        // Going back to the card took that write far longer than a period.
        if self.sink.switched() {
            self.last_wake = None;
        }
        if let Err(e) = result {
            self.failed(e);
        }
        result
    }

    /// The counters, and an event for a change of lock or the clamp.
    fn report(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        let (ppm, locked, clamped) = (path.drift.ppm(), path.drift.locked(), path.drift.clamped());
        self.counters
            .ppm_milli
            .store((ppm * 1e3).round() as i64, Relaxed);
        if locked != self.locked {
            self.locked = locked;
            self.counters.locked.store(locked, Relaxed);
            self.emit(if locked {
                FurtherEvent::Locked { ppm }
            } else {
                FurtherEvent::Unlocked
            });
        }
        if clamped && !self.clamped {
            self.emit(FurtherEvent::Clamped);
        }
        self.clamped = clamped;
    }
}

/// The writer's side of a feed, for the engine's tests: what was pushed,
/// and when.
#[cfg(test)]
pub(crate) struct FeedProbe {
    ring: Consumer<i32>,
    shared: Arc<Shared>,
    counters: Arc<OutputCounters>,
}

#[cfg(test)]
impl FeedProbe {
    /// An open feed of `channels` channels that holds `frames` frames, on
    /// the system's clock, and its probe. One of 0 frames refuses every
    /// push.
    pub(crate) fn new(channels: usize, frames: usize) -> (Feed, FeedProbe) {
        let (ring, consumer) = RingBuffer::new(frames * channels);
        let shared = Arc::new(Shared::default());
        shared.open.store(true, Release);
        let counters = Arc::new(OutputCounters::default());
        let feed = Feed {
            ring,
            channels,
            shared: shared.clone(),
            counters: counters.clone(),
            clock: Clock::Monotonic,
            pushed: 0,
        };
        let probe = FeedProbe {
            ring: consumer,
            shared,
            counters,
        };
        (feed, probe)
    }

    /// Everything pushed since the last call, interleaved.
    pub(crate) fn take(&mut self) -> Vec<i32> {
        let slots = self.ring.slots();
        let chunk = self.ring.read_chunk(slots).unwrap();
        chunk.into_iter().collect()
    }

    /// Empties the ring without allocating.
    pub(crate) fn discard(&mut self) {
        let slots = self.ring.slots();
        self.ring.read_chunk(slots).unwrap().commit_all();
    }

    /// Frames pushed since the feed was made, and the stamp of the last
    /// push.
    pub(crate) fn last_push(&self) -> (u64, u64) {
        let push = self.shared.read();
        (push.frames, push.at_ns)
    }

    pub(crate) fn overflows(&self) -> u64 {
        self.counters.overflows.load(Relaxed)
    }

    /// The writer closes the output: the feed takes nothing from now on.
    pub(crate) fn close(&self) {
        self.shared.open.store(false, Release);
    }
}

#[cfg(test)]
impl Feed {
    /// Frames in the ring.
    fn queued(&self) -> usize {
        (self.ring.buffer().capacity() - self.ring.slots()) / self.channels
    }

    fn pushed(&self) -> u64 {
        self.pushed
    }
}

#[cfg(test)]
impl Writer {
    fn ratio(&self) -> f64 {
        self.ratio
    }

    /// The resampler's delay in master frames.
    fn resampler_delay(&self) -> f64 {
        let path = self.path.as_ref().unwrap();
        path.resampler.delay_frames() * path.scale()
    }

    /// The first and the last sample in the feed.
    fn feed_ends(&mut self) -> Option<(i32, i32)> {
        let slots = self.ring.slots();
        let chunk = self.ring.read_chunk(slots).ok()?;
        let (first, second) = chunk.as_slices();
        let last = second.last().or(first.last())?;
        Some((*first.first()?, *last))
    }
}

/// The control thread's side of a further output.
pub struct FurtherHandle {
    index: usize,
    channels: usize,
    feed_frames: usize,
    commands: Mutex<Producer<Command>>,
    returns: Mutex<Consumer<Box<Path>>>,
    events: Mutex<Consumer<FurtherEvent>>,
    search: CardSearch,
    thread: Option<JoinHandle<()>>,
}

impl FurtherHandle {
    /// The output's index, 0 being the master.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Plays from now on with the master at `master_rate`, the card at
    /// `card_rate` with `period` frames a period, through `resampler`, made
    /// for those rates, the output's channels and at most `period` frames.
    /// The path's delay is held at `target` master frames. Allocates the
    /// writer's buffers here, on the control thread. An open output opens
    /// its card again.
    pub fn reconfigure(
        &self,
        master_rate: u32,
        card_rate: u32,
        period: u32,
        resampler: Box<Adaptive>,
        target: f64,
    ) {
        assert!(master_rate > 0 && card_rate > 0 && period > 0);
        let scale = f64::from(master_rate) / f64::from(card_rate);
        let writer_period = f64::from(period) * scale;
        // A full feed has to read as a delay the drift believes, or a path
        // that overflows starts over every second.
        let believed = target + SANE * writer_period;
        assert!(
            (self.feed_frames as f64) < believed,
            "output {}: a full feed of {} frames reads over {believed:.0}, \
             the most the drift believes at a target of {target:.0}",
            self.index + 1,
            self.feed_frames
        );
        let path = Path {
            master_rate,
            card_rate,
            period,
            target,
            drift: Drift::new(target, master_rate, writer_period.round() as u32),
            input: vec![0; resampler.input_frames_max() * self.channels],
            output: vec![0; period as usize * self.channels],
            resampler,
        };
        self.send(Command::Reconfigure(Box::new(path)));
    }

    /// Opens the output, or closes it: a closed output's card is closed and
    /// its feed takes nothing.
    pub fn set_open(&self, open: bool) {
        self.send(Command::Open(open));
    }

    /// Where the control thread looks for the card while it is gone.
    pub fn search(&self) -> CardSearch {
        self.search.clone()
    }

    /// The next event from the writer, if any.
    pub fn event(&self) -> Option<FurtherEvent> {
        self.collect();
        self.events.lock().unwrap().pop().ok()
    }

    /// Drops what the writer has replaced, here on the control thread.
    /// Returns how many.
    pub fn collect(&self) -> usize {
        let mut returns = self.returns.lock().unwrap();
        let mut count = 0;
        while returns.pop().is_ok() {
            count += 1;
        }
        count
    }

    fn send(&self, command: Command) {
        self.collect();
        if self.commands.lock().unwrap().push(command).is_err() {
            eprintln!(
                "aas: output {}: the writer has {COMMANDS} commands unread, one is lost",
                self.index + 1
            );
        }
        if let Some(thread) = &self.thread {
            thread.thread().unpark();
        }
    }
}

impl Drop for FurtherHandle {
    /// Stops the writer thread, if there is one, and waits for it.
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let commands = self.commands.get_mut().unwrap();
        // A writer that has died reads nothing more.
        while commands.push(Command::Stop).is_err() && !thread.is_finished() {
            thread.thread().unpark();
            thread::sleep(Duration::from_millis(1));
        }
        thread.thread().unpark();
        if thread.join().is_err() {
            eprintln!("aas: output {}: the writer thread panicked", self.index + 1);
        }
        self.collect();
    }
}

/// The three sides of a further output, without a thread.
fn parts(
    index: usize,
    mut sink: FallbackSink,
    feed_frames: usize,
    channels: usize,
    counters: Arc<OutputCounters>,
    clock: Clock,
) -> (Writer, Feed, FurtherHandle) {
    assert!(index > 0, "output 0 is the master");
    assert_eq!(sink.channels(), channels);
    let (feed_in, feed_out) = RingBuffer::new(feed_frames * channels);
    let (commands_in, commands_out) = RingBuffer::new(COMMANDS);
    let (returns_in, returns_out) = RingBuffer::new(RETURNS);
    let (events_in, events_out) = RingBuffer::new(EVENTS);
    let shared = Arc::new(Shared::default());
    let search = sink.card_search();
    let watch = sink.card_watch();
    let writer = Writer {
        sink,
        ring: feed_out,
        channels,
        shared: shared.clone(),
        counters: counters.clone(),
        clock: clock.clone(),
        commands: commands_out,
        returns: returns_in,
        events: events_in,
        watch,
        search: search.clone(),
        path: None,
        open: false,
        playing: false,
        running: false,
        stopped: false,
        taken: 0,
        held: 0.0,
        line: None,
        last_wake: None,
        gone: false,
        locked: false,
        clamped: false,
        overflows: counters.overflows.load(Relaxed),
        ratio: 1.0,
        applied: 1.0,
        overdue: false,
    };
    let feed = Feed {
        ring: feed_in,
        channels,
        shared,
        counters,
        clock,
        pushed: 0,
    };
    let handle = FurtherHandle {
        index,
        channels,
        feed_frames,
        commands: Mutex::new(commands_in),
        returns: Mutex::new(returns_out),
        events: Mutex::new(events_out),
        search,
        thread: None,
    };
    (writer, feed, handle)
}

/// Starts the writer of output `index`, 0 being the master, on a thread
/// `aas-out-N` with N = `index + 1`, at [`rt::WRITER_PRIORITY`]. Its feed
/// holds `feed_frames` frames of `channels` channels: [`FEED_PERIODS`]
/// master periods. The output stays closed until the handle configures and
/// opens it. Dropping the handle stops the thread.
pub fn spawn(
    index: usize,
    sink: FallbackSink,
    feed_frames: usize,
    channels: usize,
    counters: Arc<OutputCounters>,
) -> (Feed, FurtherHandle) {
    let (writer, feed, mut handle) = parts(
        index,
        sink,
        feed_frames,
        channels,
        counters,
        Clock::Monotonic,
    );
    let thread = thread::Builder::new()
        .name(format!("aas-out-{}", index + 1))
        .spawn(move || writer_loop(writer, index + 1))
        .expect("the writer thread starts");
    handle.thread = Some(thread);
    (feed, handle)
}

fn writer_loop(mut writer: Writer, number: usize) {
    match rt::set_fifo(rt::WRITER_PRIORITY) {
        Ok(()) => eprintln!(
            "aas: output {number} writer at SCHED_FIFO {}",
            rt::WRITER_PRIORITY
        ),
        Err(e) => eprintln!("aas: warning: output {number} writer not real time: {e}"),
    }
    while !writer.is_stopped() {
        // Errors are counted where they happen.
        let _ = writer.cycle();
        if !writer.is_playing() && !writer.is_stopped() {
            thread::park_timeout(PARK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Format, Quality};
    use crate::sink::{MemorySink, Sink, SinkError};
    use assert_no_alloc::assert_no_alloc;
    use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
    use std::sync::{Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    /// A small deterministic generator for timing jitter.
    struct Lcg(u64);

    impl Lcg {
        /// Uniform in [0, 1).
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }

        fn between(&mut self, (low, high): (f64, f64)) -> f64 {
            low + (high - low) * self.next()
        }
    }

    /// What the master pushes each period.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Source {
        /// Nothing: the master does not feed this output.
        Off,
        /// A period of this value in every sample.
        Dc(i32),
    }

    /// A master and a further card on one virtual clock.
    #[derive(Clone, Copy)]
    struct Sim {
        master_rate: u32,
        master_period: usize,
        card_rate: u32,
        card_period: u32,
        /// How fast the card runs against the master.
        ppm: f64,
        /// How fast the master's card runs against the system clock.
        master_ppm: f64,
        channels: usize,
        /// How late the audio thread pushes after its card's period, µs.
        push_late_us: (f64, f64),
        /// How late the writer wakes after its card has room, µs.
        wake_late_us: (f64, f64),
        /// Keep what the card plays.
        record: bool,
        /// How long reading the card's delay takes, µs.
        read_cost_us: f64,
        /// Master frames added to the plan's target, which holds exactly
        /// a card period in the feed when the writer wakes, and starves at
        /// a frame of rounding.
        margin: f64,
    }

    impl Sim {
        fn new(master_rate: u32, master_period: usize, card_rate: u32, card_period: u32) -> Sim {
            Sim {
                master_rate,
                master_period,
                card_rate,
                card_period,
                ppm: 0.0,
                master_ppm: 0.0,
                channels: 1,
                push_late_us: (20.0, 150.0),
                wake_late_us: (20.0, 300.0),
                record: false,
                read_cost_us: 0.0,
                margin: master_period as f64 / 2.0,
            }
        }
    }

    struct World {
        sim: Sim,
        now: Arc<AtomicU64>,
        feed: Option<Feed>,
        source: Source,
        /// Master periods so far, pushed or not.
        periods: u64,
        /// When the next one is pushed, ns.
        push_at: u64,
        /// Pushes the feed refused.
        refused: u64,
        /// Master periods to leave out from now on, as if lost.
        skip: u32,
        /// Frames the master made and never pushed, refused or left out.
        lost: u64,
        period: Vec<i32>,
        push_jitter: Lcg,
        /// The card's period, as opened.
        card_period: usize,
        buffer: f64,
        /// Frames in the card, as of `at`.
        queued: f64,
        at: u64,
        started: bool,
        underruns: u64,
        written: Vec<i32>,
        gone: bool,
        wake_jitter: Lcg,
        opens: usize,
    }

    impl World {
        fn new(sim: Sim, now: Arc<AtomicU64>) -> World {
            let mut world = World {
                sim,
                now,
                feed: None,
                source: Source::Off,
                periods: 0,
                push_at: 0,
                refused: 0,
                skip: 0,
                lost: 0,
                period: vec![0; sim.master_period * sim.channels],
                push_jitter: Lcg(0x5eed),
                card_period: sim.card_period as usize,
                buffer: f64::from(BUFFERED * sim.card_period),
                queued: 0.0,
                at: 0,
                started: false,
                underruns: 0,
                written: Vec::new(),
                gone: false,
                wake_jitter: Lcg(0xfeed),
                opens: 0,
            };
            world.schedule();
            world
        }

        fn now(&self) -> u64 {
            self.now.load(Acquire)
        }

        fn set_now(&mut self, t: u64) {
            assert!(t >= self.now(), "time runs backwards");
            self.now.store(t, Release);
        }

        /// The master's rate on the system clock.
        fn master_rate(&self) -> f64 {
            f64::from(self.sim.master_rate) * (1.0 + self.sim.master_ppm * 1e-6)
        }

        fn schedule(&mut self) {
            let nominal =
                self.periods as f64 * self.sim.master_period as f64 * 1e9 / self.master_rate();
            let range = self.sim.push_late_us;
            let late = self.push_jitter.between(range) * 1e3;
            // A late push delays the next: they never overtake each other.
            self.push_at = ((nominal + late) as u64).max(self.push_at);
        }

        /// Runs the master up to `t`.
        fn advance_to(&mut self, t: u64) {
            while self.push_at <= t {
                self.set_now(self.push_at);
                if let Source::Dc(value) = self.source {
                    self.period.fill(value);
                    if self.skip > 0 {
                        self.skip -= 1;
                        self.lost += self.sim.master_period as u64;
                    } else if !self.feed.as_mut().unwrap().push(&self.period) {
                        self.refused += 1;
                        self.lost += self.sim.master_period as u64;
                    }
                }
                self.periods += 1;
                self.schedule();
            }
            self.set_now(t);
        }

        fn card_rate(&self) -> f64 {
            let master = 1.0 + self.sim.master_ppm * 1e-6;
            f64::from(self.sim.card_rate) * master * (1.0 + self.sim.ppm * 1e-6)
        }

        /// Plays the card up to now.
        fn drain(&mut self) {
            let now = self.now();
            if self.started {
                self.queued -= (now - self.at) as f64 * self.card_rate() / 1e9;
                if self.queued < 0.0 {
                    self.underruns += 1;
                    self.queued = 0.0;
                    self.started = false;
                }
            }
            self.at = now;
        }

        /// What the card has played, in order.
        fn written(&self) -> &[i32] {
            &self.written
        }
    }

    /// The further card: drains at its own rate on the virtual clock, and
    /// its `wait_room` is where the time passes.
    struct SimCard {
        world: Arc<Mutex<World>>,
        channels: usize,
    }

    impl SimCard {
        fn world(&self) -> MutexGuard<'_, World> {
            self.world.lock().unwrap()
        }
    }

    impl Sink for SimCard {
        fn open(&mut self, rate: u32, period: u32) -> Result<(), SinkError> {
            let mut w = self.world();
            if w.gone {
                return Err(SinkError::Gone);
            }
            assert_eq!(rate, w.sim.card_rate);
            w.card_period = period as usize;
            w.buffer = f64::from(BUFFERED * period);
            w.queued = 0.0;
            w.started = false;
            w.at = w.now();
            w.opens += 1;
            Ok(())
        }

        fn wait_room(&mut self) -> Result<(), SinkError> {
            let mut w = self.world();
            if w.gone {
                return Err(SinkError::Gone);
            }
            w.drain();
            let room_at = w.buffer - w.card_period as f64;
            if w.started && w.queued > room_at {
                let until = (w.queued - room_at) / w.card_rate() * 1e9;
                let range = w.sim.wake_late_us;
                let late = w.wake_jitter.between(range) * 1e3;
                let wake = w.now() + until.ceil() as u64 + late as u64;
                w.advance_to(wake);
            }
            Ok(())
        }

        fn write(&mut self, samples: &[i32]) -> Result<(), SinkError> {
            let mut w = self.world();
            if w.gone {
                return Err(SinkError::Gone);
            }
            w.drain();
            w.queued += (samples.len() / self.channels) as f64;
            assert!(
                w.queued <= w.buffer + 1e-6,
                "{} over {}",
                w.queued,
                w.buffer
            );
            if w.queued >= w.buffer - 1e-6 {
                w.started = true;
            }
            if w.sim.record {
                w.written.extend_from_slice(samples);
            }
            Ok(())
        }

        /// Takes `read_cost_us` and tells the queue at its end, as
        /// `AlsaSink` moves its reading on to the moment it returns.
        fn delay_now(&mut self) -> Result<u32, SinkError> {
            let mut w = self.world();
            if w.gone {
                return Err(SinkError::Gone);
            }
            let done = w.now() + (w.sim.read_cost_us * 1e3) as u64;
            w.advance_to(done);
            w.drain();
            Ok(w.queued.floor() as u32)
        }

        fn channels(&self) -> usize {
            self.channels
        }

        fn close(&mut self) {
            let mut w = self.world();
            w.queued = 0.0;
            w.started = false;
        }

        fn delay_frames(&self) -> u32 {
            self.world().buffer as u32
        }

        fn supported_rates(&self) -> Vec<u32> {
            vec![self.world().sim.card_rate]
        }
    }

    /// A writer on a simulated card, driven on the test thread.
    struct Rig {
        world: Arc<Mutex<World>>,
        writer: Writer,
        handle: FurtherHandle,
        counters: Arc<OutputCounters>,
        target: f64,
        /// The writer's period at the master's rate.
        period: f64,
        sim: Sim,
    }

    impl Rig {
        fn new(sim: Sim) -> Rig {
            let now = Arc::new(AtomicU64::new(0));
            let world = Arc::new(Mutex::new(World::new(sim, now.clone())));
            if sim.record {
                world.lock().unwrap().written.reserve(1 << 22);
            }
            let card = SimCard {
                world: world.clone(),
                channels: sim.channels,
            };
            let counters = Arc::new(OutputCounters::default());
            let (writer, feed, handle) = parts(
                1,
                FallbackSink::new(Box::new(card)),
                FEED_PERIODS * sim.master_period,
                sim.channels,
                counters.clone(),
                Clock::Virtual(now),
            );
            world.lock().unwrap().feed = Some(feed);
            let resampler = Box::new(Adaptive::new(
                sim.master_rate,
                sim.card_rate,
                sim.channels,
                sim.card_period,
                Quality::Medium,
                Format::F32,
            ));
            let scale = f64::from(sim.master_rate) / f64::from(sim.card_rate);
            let period = f64::from(sim.card_period) * scale;
            // The plan's target: a master period in the feed, the card's
            // buffer and the resampler.
            let target = sim.master_period as f64
                + f64::from(BUFFERED) * period
                + resampler.delay_frames() * scale
                + sim.margin;
            handle.reconfigure(
                sim.master_rate,
                sim.card_rate,
                sim.card_period,
                resampler,
                target,
            );
            handle.set_open(true);
            Rig {
                world,
                writer,
                handle,
                counters,
                target,
                period,
                sim,
            }
        }

        fn world(&self) -> MutexGuard<'_, World> {
            self.world.lock().unwrap()
        }

        fn cycle(&mut self) {
            let _ = self.writer.cycle();
        }

        fn seconds(&self) -> f64 {
            self.world().now() as f64 / 1e9
        }

        /// Cycles until `seconds` on the virtual clock.
        fn run_until(&mut self, seconds: f64) {
            while self.seconds() < seconds {
                self.cycle();
            }
        }

        /// The path's true delay in master frames, from the simulation's
        /// own state: what the master has made and the feed holds, the
        /// resampler, and the card's exact fill.
        fn true_delay(&self) -> f64 {
            let mut w = self.world();
            w.drain();
            let rate = f64::from(self.sim.master_rate);
            // The master makes frames all the time and hands over a period
            // at each push; the first push is at 0.
            let made = w.now() as f64 * w.master_rate() / 1e9 + self.sim.master_period as f64;
            let feed = w.feed.as_ref().unwrap();
            let in_feed = feed.queued() as f64 + made - (feed.pushed() + w.lost) as f64;
            let scale = rate / f64::from(self.sim.card_rate);
            in_feed + self.writer.resampler_delay() + w.queued * scale
        }

        fn events(&self) -> Vec<FurtherEvent> {
            std::iter::from_fn(|| self.handle.event()).collect()
        }

        fn count(&self, counter: &AtomicU64) -> u64 {
            counter.load(Relaxed)
        }
    }

    #[test]
    fn a_further_output_locks_and_holds_its_target() {
        let mut sim = Sim::new(44_100, 256, 48_000, 256);
        sim.ppm = 100.0;
        let mut rig = Rig::new(sim);
        rig.world().source = Source::Dc(1 << 20);
        let quarter = rig.period / 4.0;
        let mut lock = None;
        let mut worst = 0.0f64;
        while rig.seconds() < 60.0 {
            rig.cycle();
            if lock.is_none() && rig.counters.locked.load(Relaxed) {
                lock = Some(rig.seconds());
            }
            if lock.is_some() {
                assert!(
                    rig.counters.locked.load(Relaxed),
                    "lost lock at {}",
                    rig.seconds()
                );
                let error = rig.true_delay() - rig.target;
                worst = worst.max(error.abs());
                assert!(
                    error.abs() < quarter,
                    "error {error:.1} frames at {:.1} s",
                    rig.seconds()
                );
            }
        }
        let lock = lock.expect("never locked");
        eprintln!("locked at {lock:.1} s, worst error after {worst:.2} frames");
        assert!(lock < 30.0, "locked only at {lock:.1} s");
        assert_eq!(rig.count(&rig.counters.starves), 0);
        assert_eq!(rig.count(&rig.counters.overflows), 0);
        assert_eq!(rig.count(&rig.counters.xruns), 0);
        assert_eq!(rig.world().underruns, 0);
        let ppm = rig.counters.ppm_milli.load(Relaxed) as f64 / 1e3;
        assert!((ppm - 100.0).abs() < 10.0, "{ppm} ppm");
        let events = rig.events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, FurtherEvent::Locked { .. })),
            "{events:?}"
        );
    }

    #[test]
    fn the_ratio_holds_still_once_locked() {
        // The delay is read from timestamps, not counted in periods, so the
        // ratio follows the card's clock and the jitter of the timestamps,
        // not where in its period the master happened to be. At 16 kHz,
        // where a frame of rounding weighs three times as much as at 48 kHz,
        // ten minutes resample in a few seconds of a debug build.
        let mut sim = Sim::new(16_000, 128, 16_000, 128);
        sim.ppm = 100.0;
        let mut rig = Rig::new(sim);
        rig.world().source = Source::Dc(1 << 20);
        while !rig.counters.locked.load(Relaxed) {
            rig.cycle();
            assert!(rig.seconds() < 30.0, "never locked");
        }
        // Lock comes when the delay has held for 5 s, while the integral
        // still moves the ratio to the card's offset over its 20 s: right
        // after lock the ratio swings some 60 ppm as the integral settles,
        // slowly, which is not jitter. The ten minutes start after that.
        while rig.seconds() < 60.0 {
            rig.cycle();
        }
        let end = rig.seconds() + 600.0;
        let (mut low, mut high) = (f64::MAX, f64::MIN);
        while rig.seconds() < end {
            rig.cycle();
            assert!(
                rig.counters.locked.load(Relaxed),
                "lost lock at {}",
                rig.seconds()
            );
            let ratio = rig.writer.ratio();
            low = low.min(ratio);
            high = high.max(ratio);
        }
        let swing = (high - low) * 1e6;
        eprintln!(
            "ratio {:+.2} to {:+.2} ppm, {swing:.2} peak to peak",
            (low - 1.0) * 1e6,
            (high - 1.0) * 1e6
        );
        assert!(swing < 20.0, "{swing:.2} ppm peak to peak");
        assert_eq!(rig.count(&rig.counters.starves), 0);
    }

    #[test]
    fn an_empty_feed_plays_zeros_not_old_audio() {
        let mut sim = Sim::new(48_000, 256, 48_000, 256);
        sim.record = true;
        let mut rig = Rig::new(sim);
        let frames = |seconds: f64| (seconds * 48_000.0) as usize;
        // The writer starts before the feed has anything: zeros.
        rig.run_until(0.2);
        assert!(!rig.world().written().is_empty());
        assert!(rig.world().written().iter().all(|&s| s == 0));
        assert_eq!(rig.count(&rig.counters.starves), 0);

        let old = 1 << 28;
        rig.world().source = Source::Dc(old);
        rig.run_until(2.0);
        assert!(rig.world().written().iter().any(|&s| s > old / 2));

        // The master stops feeding: the feed runs dry.
        rig.world().source = Source::Off;
        let dry = rig.world().written().len();
        rig.run_until(2.5);
        assert!(rig.count(&rig.counters.starves) >= 1);
        assert!(rig.events().contains(&FurtherEvent::Starved));
        {
            let w = rig.world();
            let after = &w.written()[dry..];
            // What the feed held when the master stopped plays, and the
            // filter's tail; then zeros. What is left at the starve goes.
            let last_sound = after.iter().rposition(|&s| s != 0).unwrap_or(0);
            assert!(last_sound < FEED_PERIODS * 256 + 64, "{last_sound}");
            assert!(after.len() - last_sound > frames(0.4));
        }

        let new = -(1 << 28);
        rig.world().source = Source::Dc(new);
        let back = rig.world().written().len();
        rig.run_until(4.0);
        let w = rig.world();
        let after = &w.written()[back..];
        assert!(after.iter().all(|&s| s < old / 4), "old audio again");
        assert!(after.iter().any(|&s| s < new / 2));
    }

    #[test]
    fn a_push_carries_the_stamp_it_is_given() {
        let (mut feed, probe) = FeedProbe::new(2, 256);
        assert!(feed.push_at(&[1; 128], 1_000));
        assert_eq!(probe.last_push(), (64, 1_000));
        assert!(feed.silence_at(32, 2_000));
        assert_eq!(probe.last_push(), (96, 2_000));
        // Unstamped, a push takes the time it is made.
        let before = feed.now();
        assert!(feed.push(&[1; 2]));
        let (frames, at) = probe.last_push();
        assert_eq!(frames, 97);
        assert!(at >= before && at <= feed.now());
    }

    #[test]
    fn a_full_feed_drops_the_new_period_and_counts_it() {
        let mut rig = Rig::new(Sim::new(48_000, 256, 48_000, 256));
        // Opens and primes; the master pushes nothing by itself.
        rig.cycle();
        {
            let mut w = rig.world();
            let feed = w.feed.as_mut().unwrap();
            for value in 1..=4 {
                assert!(feed.push(&[value; 256]));
            }
            let start = Instant::now();
            assert!(!feed.push(&[5; 256]));
            assert!(start.elapsed() < Duration::from_millis(1));
            assert_eq!(feed.queued(), 4 * 256);
        }
        assert_eq!(rig.count(&rig.counters.overflows), 1);
        // The new period went, the four before it stay.
        assert_eq!(rig.writer.feed_ends(), Some((1, 4)));
        rig.cycle();
        assert!(rig.events().contains(&FurtherEvent::Overflowed));
        // A full feed means the writer fell behind: all of it is from before
        // the gap, and goes.
        assert_eq!(rig.world().feed.as_ref().unwrap().queued(), 0);
    }

    /// Plays `before` for a second, holds the writer for 100 ms while the
    /// master plays `during`, then plays a negative value for a second, and
    /// returns what the card played after the hold.
    fn after_a_stall(during: Source) -> (Rig, Vec<i32>) {
        let mut sim = Sim::new(48_000, 256, 48_000, 256);
        sim.record = true;
        let mut rig = Rig::new(sim);
        rig.world().source = Source::Dc(1 << 28);
        rig.run_until(1.0);
        {
            let mut w = rig.world();
            w.source = during;
            let until = w.now() + 100_000_000;
            w.advance_to(until);
            w.source = Source::Dc(-(1 << 28));
        }
        let mark = rig.world().written().len();
        let until = rig.seconds() + 1.0;
        rig.run_until(until);
        let after = rig.world().written()[mark..].to_vec();
        assert!(after.iter().any(|&s| s < -(1 << 27)));
        (rig, after)
    }

    #[test]
    fn a_writer_stalled_longer_than_its_feed_plays_nothing_from_before() {
        // The master pushes on and the feed overflows.
        let (rig, after) = after_a_stall(Source::Dc(1 << 28));
        assert!(rig.world().refused > 0);
        assert!(
            after.iter().all(|&s| s < 1 << 26),
            "audio from before the stall"
        );
        // The master is quiet meanwhile: what the feed held is as old.
        let (rig, after) = after_a_stall(Source::Off);
        assert_eq!(rig.world().refused, 0);
        assert!(
            after.iter().all(|&s| s < 1 << 26),
            "audio from before the stall"
        );
    }

    #[test]
    fn a_starve_after_lock_keeps_the_ratio_and_the_lock() {
        let mut sim = Sim::new(44_100, 256, 48_000, 256);
        sim.ppm = 100.0;
        let mut rig = Rig::new(sim);
        rig.world().source = Source::Dc(1 << 20);
        rig.run_until(40.0);
        assert!(rig.counters.locked.load(Relaxed));
        let before = rig.writer.ratio();
        let quarter = rig.period / 4.0;
        let _ = rig.events();
        // Two master periods lost on the way: the feed runs short once.
        rig.world().skip = 2;
        let start = rig.seconds();
        let mut moved = 0.0f64;
        while rig.seconds() < start + 10.0 {
            rig.cycle();
            moved = moved.max((rig.writer.ratio() - before).abs() * 1e6);
            let at = rig.seconds() - start;
            assert!(
                rig.counters.locked.load(Relaxed),
                "lost lock {at:.2} s after"
            );
            let ratio = rig.writer.ratio();
            assert!(
                (ratio - before).abs() < 20e-6,
                "ratio {:+.1} ppm {at:.2} s after, {:+.1} before",
                (ratio - 1.0) * 1e6,
                (before - 1.0) * 1e6
            );
            if at > 2.0 {
                let error = rig.true_delay() - rig.target;
                assert!(error.abs() < quarter, "error {error:.1} {at:.2} s after");
            }
        }
        eprintln!("the ratio moved {moved:.2} ppm over the starve");
        assert_eq!(rig.count(&rig.counters.starves), 1);
        let events = rig.events();
        assert!(events.contains(&FurtherEvent::Starved), "{events:?}");
        assert!(!events.contains(&FurtherEvent::Unlocked), "{events:?}");
    }

    #[test]
    fn the_card_is_read_at_the_instant_of_the_feed() {
        // Reading the card takes a millisecond, 48 frames.
        let mut sim = Sim::new(48_000, 256, 48_000, 256);
        sim.ppm = 100.0;
        sim.read_cost_us = 1_000.0;
        let mut rig = Rig::new(sim);
        rig.world().source = Source::Dc(1 << 20);
        rig.run_until(20.0);
        assert!(rig.counters.locked.load(Relaxed));
        let mut worst = 0.0f64;
        while rig.seconds() < 30.0 {
            rig.cycle();
            worst = worst.max((rig.true_delay() - rig.target).abs());
        }
        assert!(worst < 8.0, "error up to {worst:.1} frames");
    }

    #[test]
    fn the_line_rides_out_wake_jitter_at_short_periods() {
        // 64 frames at 48 kHz, a push every 1.3 ms, each 20 µs to 4 ms
        // late, and never before the one before it.
        let rate = 48_000.0;
        let last = 64;
        let mut jitter = Lcg(7);
        let mut line: Option<Line> = None;
        let mut restarts = 0;
        let mut at = 0;
        let (mut low, mut high) = (f64::MAX, f64::MIN);
        for k in 1..=20_000u64 {
            let nominal = (k * last) as f64 * 1e9 / rate;
            let late = jitter.between((20.0, 4_000.0)) * 1e3;
            at = ((nominal + late) as u64).max(at);
            let push = Push {
                frames: k * last,
                at_ns: at,
                last,
            };
            let next = match line {
                None => Line::start(push, rate),
                Some(line) => line.follow(push, rate).unwrap_or_else(|| {
                    restarts += 1;
                    Line::start(push, rate)
                }),
            };
            line = Some(next);
            // Past the line's settling, about 5 s.
            if k > 4_000 {
                low = low.min(next.at - nominal);
                high = high.max(next.at - nominal);
            }
        }
        assert_eq!(restarts, 0);
        let swing = (high - low) / 1e3;
        assert!(swing < 200.0, "the line moves {swing:.0} µs");
    }

    /// A card whose driver panics at the open, which takes the writer
    /// thread with it.
    struct Doomed;

    impl Sink for Doomed {
        fn open(&mut self, _rate: u32, _period: u32) -> Result<(), SinkError> {
            panic!("the card's driver fails");
        }

        fn write(&mut self, _samples: &[i32]) -> Result<(), SinkError> {
            Ok(())
        }

        fn channels(&self) -> usize {
            2
        }

        fn close(&mut self) {}

        fn delay_frames(&self) -> u32 {
            0
        }

        fn supported_rates(&self) -> Vec<u32> {
            Vec::new()
        }
    }

    #[test]
    fn dropping_the_handle_of_a_dead_writer_returns() {
        let sink = FallbackSink::new(Box::new(Doomed));
        let (_feed, handle) = spawn(1, sink, 1024, 2, Arc::default());
        let resampler = Box::new(Adaptive::new(
            48_000,
            48_000,
            2,
            256,
            Quality::Medium,
            Format::F32,
        ));
        handle.reconfigure(48_000, 48_000, 256, resampler, 1_000.0);
        // The writer dies at the open; the commands after it pile up.
        for _ in 0..2 * COMMANDS {
            handle.set_open(true);
        }
        let (done, dropped) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(handle);
            done.send(()).unwrap();
        });
        assert!(
            dropped.recv_timeout(Duration::from_secs(5)).is_ok(),
            "dropping the handle hangs"
        );
    }

    #[test]
    fn a_lost_further_output_never_stalls_the_master() {
        let card = MemorySink::paced();
        let log = card.log();
        let counters = Arc::new(OutputCounters::default());
        let (mut feed, handle) = spawn(
            1,
            FallbackSink::new(Box::new(card)),
            FEED_PERIODS * 256,
            2,
            counters.clone(),
        );
        let resampler = Box::new(Adaptive::new(
            48_000,
            48_000,
            2,
            256,
            Quality::Medium,
            Format::F32,
        ));
        // The paced sink keeps two periods.
        let target = 3.0 * 256.0 + resampler.delay_frames();
        handle.reconfigure(48_000, 48_000, 256, resampler, target);
        handle.set_open(true);
        let period = vec![1 << 20; 2 * 256];
        let length = Duration::from_nanos(256 * 1_000_000_000 / 48_000);
        let start = Instant::now();
        let mut slowest = Duration::ZERO;
        let mut refused_after = 0;
        for n in 0..150u32 {
            if n == 50 {
                assert!(!log.samples().is_empty(), "nothing reached the card");
                log.fail(Some(SinkError::Gone));
            }
            let pushed = Instant::now();
            let ok = feed.push(&period);
            slowest = slowest.max(pushed.elapsed());
            if n >= 60 && !ok {
                refused_after += 1;
            }
            let next = start + length * (n + 1);
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
        assert!(
            slowest < Duration::from_millis(2),
            "a push took {slowest:?}"
        );
        // The writer plays on to its null sink and keeps taking the feed.
        assert!(refused_after <= 5, "{refused_after} pushes refused");
        let events: Vec<_> = std::iter::from_fn(|| handle.event()).collect();
        let gone = events.iter().filter(|&&e| e == FurtherEvent::Gone).count();
        assert_eq!(gone, 1, "{events:?}");
        assert!(!events.contains(&FurtherEvent::Back));
        assert!(handle.search().is_gone());
        drop(handle);
    }

    #[test]
    fn a_closed_writer_discards_its_feed() {
        let mut sim = Sim::new(48_000, 256, 48_000, 256);
        sim.record = true;
        let mut rig = Rig::new(sim);
        let first = 1 << 28;
        rig.world().source = Source::Dc(first);
        rig.run_until(1.0);
        assert!(rig.world().written().iter().any(|&s| s > first / 2));

        rig.handle.set_open(false);
        rig.cycle();
        // What the master pushes while the output is closed is taken and
        // thrown away.
        let closed = 1 << 27;
        {
            let mut w = rig.world();
            w.source = Source::Dc(closed);
            let later = w.now() + 200_000_000;
            w.advance_to(later);
            assert_eq!(w.refused, 0);
            assert_eq!(w.feed.as_ref().unwrap().queued(), 0);
        }
        rig.cycle();

        let next = -(1 << 28);
        rig.world().source = Source::Dc(next);
        let reopened = rig.world().written().len();
        rig.handle.set_open(true);
        let until = rig.seconds() + 1.0;
        rig.run_until(until);
        let w = rig.world();
        assert_eq!(w.opens, 2);
        let after = &w.written()[reopened..];
        assert!(after.iter().all(|&s| s < 1 << 26), "audio from before");
        assert!(after.iter().any(|&s| s < next / 2));
    }

    #[test]
    fn writer_cycle_does_not_allocate() {
        let mut rig = Rig::new(Sim::new(44_100, 256, 48_000, 256));
        rig.world().source = Source::Dc(1 << 20);
        rig.run_until(2.0);
        assert!(rig.writer.ratio() != 0.0);
        assert_no_alloc(|| {
            // Playing, a starve, priming again and an overflow.
            for _ in 0..200 {
                rig.cycle();
            }
            rig.world().source = Source::Off;
            for _ in 0..50 {
                rig.cycle();
            }
            rig.world().source = Source::Dc(1 << 20);
            for _ in 0..200 {
                rig.cycle();
            }
            {
                let mut w = rig.world();
                let feed = w.feed.as_mut().unwrap();
                while feed.silence(256) {}
            }
            for _ in 0..200 {
                rig.cycle();
            }
        });
        assert!(rig.count(&rig.counters.starves) >= 1);
        assert!(rig.count(&rig.counters.overflows) >= 1);
        assert!(rig.events().contains(&FurtherEvent::Overflowed));
    }

    #[test]
    fn a_replaced_resampler_is_dropped_on_the_control_thread() {
        let mut rig = Rig::new(Sim::new(48_000, 256, 48_000, 256));
        rig.cycle();
        assert_eq!(rig.handle.collect(), 0);
        let resampler = Box::new(Adaptive::new(
            48_000,
            48_000,
            1,
            256,
            Quality::Medium,
            Format::F32,
        ));
        rig.handle
            .reconfigure(48_000, 48_000, 256, resampler, rig.target);
        rig.cycle();
        assert_eq!(rig.handle.collect(), 1);
        // Reopened at the new configuration.
        assert_eq!(rig.world().opens, 2);
    }

    #[test]
    #[should_panic(expected = "feed")]
    fn a_target_the_full_feed_would_pass_is_refused() {
        let rig = Rig::new(Sim::new(48_000, 256, 48_000, 256));
        // A full feed of 1024 frames reads over 100 + 8 periods of 64.
        let resampler = Box::new(Adaptive::new(
            48_000,
            48_000,
            1,
            64,
            Quality::Medium,
            Format::F32,
        ));
        rig.handle.reconfigure(48_000, 48_000, 64, resampler, 100.0);
    }
}
