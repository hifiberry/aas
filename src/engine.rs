//! The audio thread's work: one period per `cycle`.
//!
//! The control thread owns every decision and sends them as commands. This
//! code only carries them out. It does not allocate, free or lock. Whatever
//! it would free goes back to the control thread in an `Event`.

use crate::bitperfect::{Situation, decide, plugins_apply, volumes_apply};
use crate::capture::Capture;
use crate::config::BitPerfect;
use crate::counters::Counters;
use crate::dop;
use crate::format::{BlockStore, Processing, Ramp, mix_add, mix_add_acc, mix_clear, mix_finish};
use crate::latency::{MAX_PERIOD, PERIODS};
use crate::plugin::chain::Chain;
use crate::rate::ClientId;
use crate::resample::Resampler;
use crate::sink::{Sink, SinkError};
use aas_client::ring::Ring;
use rtrb::{Consumer, Producer, PushError};
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{Ordering, fence};
use std::time::Instant;

/// Sources the table of a new engine has room for. The control thread sends
/// `Grow` before it adds more.
pub const INITIAL_SOURCES: usize = 16;

/// Capture streams the tap table of a new engine has room for. The control
/// thread sends `GrowTaps` before it adds more.
pub const INITIAL_TAPS: usize = 4;

/// Length of a gain change.
const FADE_MS: u32 = 10;

/// How long a DoP stream that has run dry keeps the output.
const DOP_GRACE_MS: u32 = 100;

/// Most input frames a resampler is expected to want per output frame. The
/// scan looks that far ahead so that it can judge everything about to be read.
const MAX_DOWNSAMPLING: usize = 8;

/// Events held back while the queue is full.
const BACKLOG: usize = 8;
/// The most events one command can cause, with room for `Reconfigured`.
const EVENTS_PER_COMMAND: usize = 3;

/// What a client's stream has been seen to be.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// No frames so far, or too few to tell.
    Unknown,
    Pcm,
    Dop,
}

/// One client as the audio thread sees it.
pub struct Source {
    pub id: ClientId,
    pub ring: Ring,
    /// An eventfd, non-blocking, written after each cycle.
    pub wake: OwnedFd,
    /// The client's sample rate.
    pub rate: u32,
    /// Boxed so that swapping one moves a pointer and frees nothing.
    pub resampler: Option<Box<Resampler>>,
    pub gain: f32,
    pub running: bool,
    stream: Stream,
    /// Frames that may be read from the ring this cycle.
    ready: usize,
    /// Frames seen in the ring this cycle, up to a period.
    seen: usize,
    /// This source goes into the mix this cycle. Decided from what was seen,
    /// so frames written since then change nothing.
    plays: bool,
    /// Frames seen but held back, as they might be DoP.
    held: usize,
    /// The stream was under way before this cycle.
    settled: bool,
    /// The ring starts inside a DoP run that was left when the stream
    /// stopped. What remains can be too short to recognise.
    in_dop_run: bool,
    /// A discard, or a stop with nothing left, cut a DoP run whose tail may
    /// still arrive. The next window drops a marked prefix too short to
    /// judge, without taking the stream for DoP. Clear once a window
    /// starts unmarked or holds a run long enough to judge.
    cut_run: bool,
    /// Frames dropped as such a tail, to credit against the next discard so
    /// the client keeps real time.
    dropped: usize,
    /// The marker that continues the DoP stream.
    next_marker: u8,
    /// Output frames since a DoP stream last had any of its own.
    dop_dry_frames: usize,
    /// A DoP stream that held the output has had no frames for the grace
    /// time. It has given up its claim and is reported stopped, so that
    /// the output does not stay at its rate. Its next frames make a new
    /// claim. Kept as a flag, since the grace time in frames changes with
    /// the output rate, and a stream must not come back to life by that.
    stalled: bool,
    /// What a discard left over of a frame, in units of the output rate.
    discard_remainder: u64,
    dop_reported: bool,
    drained_reported: bool,
    /// When this stream was first seen as DoP, in the order of all claims.
    /// Zero while it is not DoP. The oldest claim holds the output.
    dop_claim: u64,
    /// The position in the header's `wake_at` that has had its wake-up.
    woken_at: u64,
    /// The underruns the client has been woken for.
    underruns_told: u64,
}

impl Source {
    /// A stopped source at unity gain, with no resampler.
    pub fn new(id: ClientId, ring: Ring, wake: OwnedFd, rate: u32) -> Source {
        Source {
            id,
            ring,
            wake,
            rate,
            resampler: None,
            gain: 1.0,
            running: false,
            stream: Stream::Unknown,
            ready: 0,
            seen: 0,
            plays: false,
            held: 0,
            settled: false,
            in_dop_run: false,
            cut_run: false,
            dropped: 0,
            next_marker: 0x05,
            dop_dry_frames: 0,
            stalled: false,
            discard_remainder: 0,
            dop_reported: false,
            drained_reported: false,
            dop_claim: 0,
            woken_at: 0,
            underruns_told: 0,
        }
    }

    /// Empties the ring and forgets everything about the stream so far, so
    /// that what the client writes next starts a new one.
    fn flush(&mut self) {
        self.ring.consume(usize::MAX);
        if let Some(r) = self.resampler.as_mut() {
            r.reset();
        }
        self.running = false;
        self.stream = Stream::Unknown;
        self.ready = 0;
        self.seen = 0;
        self.plays = false;
        self.held = 0;
        self.settled = false;
        self.in_dop_run = false;
        self.cut_run = false;
        self.dropped = 0;
        self.next_marker = 0x05;
        self.dop_dry_frames = 0;
        self.stalled = false;
        self.discard_remainder = 0;
        self.drained_reported = false;
    }

    fn draining(&self) -> bool {
        self.ring.header().draining.load(Ordering::Acquire) != 0
    }

    /// Everything the client wrote has been played.
    fn drained(&self) -> bool {
        // The flag comes first: a client sets it after its last frames.
        self.draining()
            && self.ring.available() == 0
            && self.resampler.as_ref().is_none_or(|r| r.flushed())
    }

    /// A DoP stream cannot be resampled, so it stays silent until the output
    /// runs at its rate.
    fn muted(&self, output_rate: u32) -> bool {
        self.stream == Stream::Dop && self.rate != output_rate
    }

    /// Input frames one period takes.
    fn period_input(&self, period: usize) -> usize {
        self.resampler
            .as_ref()
            .map_or(period, |r| r.input_frames_next())
    }

    /// Decides what may be read this cycle from `window`, the start of what
    /// the ring holds. No DoP frame may reach the mix or a resampler, so PCM
    /// stops short of it and DoP stops short of whatever follows it. Each
    /// kind then starts on a cycle of its own. Later reads go no further than
    /// this decision, whatever the client has written since.
    fn judge(&mut self, window: &[[i32; 2]], period: usize, draining: bool) {
        let input = self.period_input(period);
        self.settled = self.stream != Stream::Unknown;
        self.ready = 0;
        self.held = 0;
        self.plays = false;
        if self.in_dop_run && !window.is_empty() {
            self.in_dop_run = false;
            self.stream = Stream::Dop;
        }
        let window = if self.cut_run && self.stream != Stream::Dop {
            self.drop_cut_tail(window)
        } else {
            window
        };
        self.seen = window.len().min(period);
        if self.stream == Stream::Dop {
            self.ready = dop::marked_run(&window[..self.seen]);
            if self.ready > 0 || window.is_empty() {
                self.plays = self.flush_due(draining);
                return;
            }
            self.stream = Stream::Pcm;
        }
        let head = window.len().min(input);
        let pcm = match dop::find(window) {
            dop::Found::At(0) => {
                self.stream = Stream::Dop;
                self.ready = dop::marked_run(&window[..self.seen]);
                return;
            }
            dop::Found::At(start) => start,
            dop::Found::Unclear(0) if draining => {
                // Nothing more will come to judge it by, so it is not played.
                self.ring.consume(window.len());
                self.plays = self.flush_due(draining);
                return;
            }
            dop::Found::Unclear(start) => {
                if self.settled && !draining {
                    self.held = head.saturating_sub(start);
                }
                start
            }
            dop::Found::Nothing => head,
        };
        if pcm > 0 {
            self.stream = Stream::Pcm;
        }
        self.ready = pcm.min(head);
        self.plays = match self.resampler {
            None => self.ready > 0,
            // A block cut short by DoP plays, with silence for the rest.
            Some(_) if self.ready > 0 => window.len() >= input || draining,
            Some(_) => window.is_empty() && self.flush_due(draining),
        };
    }

    /// The filter still holds audio that only silence can push out.
    fn flush_due(&self, draining: bool) -> bool {
        let filled = self.resampler.as_ref().is_some_and(|r| !r.flushed());
        draining && filled && self.ready == 0
    }

    /// Counts an underrun if `got` falls short of `want`. Running out at the
    /// end of a drain is not one, and neither is a stream still starting.
    fn note_shortfall(&self, got: usize, want: usize) {
        let primed = self.resampler.as_ref().is_none_or(|r| r.primed());
        if got < want && self.settled && primed && !self.draining() {
            let underruns = &self.ring.header().underruns;
            underruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Fills `into` from the ring, padding with zeros.
    fn read_raw(&self, into: &mut [[i32; 2]]) {
        let n = self.ring.read(&mut into[..self.ready]);
        into[n..].fill([0; 2]);
        self.note_shortfall(self.seen, into.len());
    }

    /// Fills `into` with a period at the output rate.
    fn read_resampled(&mut self, into: &mut [[i32; 2]]) {
        let Some(resampler) = self.resampler.as_mut() else {
            return self.read_raw(into);
        };
        let n = resampler.process_up_to(&self.ring, into, self.ready);
        into[n..].fill([0; 2]);
        self.note_shortfall(n, into.len());
    }

    /// Fills `into` from the ring without touching a sample, then pads with
    /// DoP silence. Zeros would drop the DAC out of DoP mode.
    fn read_dop(&mut self, into: &mut [[i32; 2]]) {
        let n = self.ring.read(&mut into[..self.ready]);
        match into[..n].last() {
            Some(last) => {
                self.next_marker = dop::marker_after(last);
                self.dop_dry_frames = 0;
            }
            None => self.dop_dry_frames += into.len(),
        }
        self.next_marker = dop::fill_silence(&mut into[n..], self.next_marker);
        self.note_shortfall(self.seen, into.len());
    }

    /// Whether this DoP stream holds the output this cycle. One that has run
    /// dry keeps it for the grace time, unless it has ended.
    fn holds_output(&self, output_rate: u32) -> bool {
        let dry_for_now = !self.stalled && !self.draining();
        self.running
            && self.stream == Stream::Dop
            && self.rate == output_rate
            && (self.ready > 0 || dry_for_now)
    }

    /// Notes whether a DoP stream has stalled, from what this cycle's scan
    /// found. Frames end a stall, and so does the end of the DoP stream.
    fn note_stall(&mut self, output_rate: u32) {
        let grace = (output_rate * DOP_GRACE_MS / 1000) as usize;
        if self.stream != Stream::Dop || self.ready > 0 {
            self.stalled = false;
        } else if self.dop_dry_frames >= grace {
            self.stalled = true;
        }
    }

    /// Drops one period's worth of input at the client's own rate, carrying
    /// the fraction of a frame over, so the client keeps real time.
    fn discard(&mut self, period: usize, output_rate: u32) {
        if let Some(r) = self.resampler.as_mut() {
            r.skip();
        }
        let output_rate = u64::from(output_rate);
        let total = period as u64 * u64::from(self.rate) + self.discard_remainder;
        let want = (total / output_rate) as usize;
        self.discard_remainder = total % output_rate;
        // A dropped tail was consumed ahead of real time.
        let credit = self.dropped.min(want);
        self.dropped -= credit;
        let want = want - credit;
        let n = self.ring.available().min(want);
        if n > 0 && self.stream != Stream::Dop {
            // A marked frame last may be the middle of a run.
            let mut last = [[0; 2]];
            self.ring.consume(n - 1);
            self.ring.read(&mut last);
            self.cut_run = dop::marked_run(&last) > 0;
        } else {
            self.ring.consume(n);
            if n > 0 {
                // A DoP stream with frames of its own is not stalled, even
                // while it is not the one that plays.
                self.dop_dry_frames = 0;
            }
        }
        self.note_shortfall(n, want);
    }

    /// Drops the marked frames at the start of `window` when they are too
    /// few to be judged a DoP run: they are the rest of the run that was
    /// cut, and must not be mixed. Being marked is not enough to call the
    /// stream DoP, since ordinary PCM crosses the marker band too, so a run
    /// long enough is left for `find`. Returns what is left of the window.
    fn drop_cut_tail<'w>(&mut self, window: &'w [[i32; 2]]) -> &'w [[i32; 2]] {
        let run = dop::marked_run(window);
        if run >= dop::MIN_FRAMES {
            self.cut_run = false;
            return window;
        }
        self.ring.consume(run);
        self.dropped += run;
        let rest = &window[run..];
        self.cut_run = rest.is_empty();
        rest
    }

    /// Tells the client how much plays after its ring, and wakes it if it
    /// is due: after every period, or once the position it waits for is
    /// reached, and after each underrun. A client that waits for a position
    /// sleeps through the periods before it, and costs no call here.
    fn notify(&mut self, sink_delay: u32, output_rate: u32) {
        let filter = self.resampler.as_ref().map_or(0, |r| r.delay_frames());
        let delay = u64::from(sink_delay + filter) * u64::from(self.rate) / u64::from(output_rate);
        let header = self.ring.header();
        header
            .output_delay_frames
            .store(delay as u32, Ordering::Relaxed);
        // The client stores the position it waits for and then loads ours.
        // With a full barrier on both sides, one of the two sees the other.
        fence(Ordering::SeqCst);
        let wake_at = header.wake_at.load(Ordering::SeqCst);
        let reached = header.read_pos.load(Ordering::Relaxed) >= wake_at;
        let underruns = header.underruns.load(Ordering::Relaxed);
        let due = wake_at == 0 || (reached && wake_at != self.woken_at);
        if !due && underruns == self.underruns_told {
            return;
        }
        self.woken_at = wake_at;
        self.underruns_told = underruns;
        let one = 1u64;
        // SAFETY: a plain write of 8 bytes. The fd is non-blocking, and a full
        // counter only means the client has plenty of wake-ups waiting.
        unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                (&raw const one).cast(),
                size_of::<u64>(),
            )
        };
    }
}

/// The analog input as the audio thread sees it: the capture it reads, the
/// chain it runs through, and the period read this cycle. It is mixed like
/// a running source, but only while its chain reports signal.
struct Analog<P: Processing> {
    capture: Option<Box<dyn Capture>>,
    /// The capture has been closed, for a reopen or after a failure. It is
    /// not read again, and stays until the control thread replaces it.
    closed: bool,
    /// A capture whose call failed, to be handed back in
    /// `Event::CaptureFailed`, which closes it off the audio thread.
    failed: Option<Box<dyn Capture>>,
    chain: Chain,
    /// This cycle's period as captured.
    buf: Vec<[i32; 2]>,
    /// This cycle's period after a chain that altered it, in the
    /// processing format, so that a float format keeps its headroom until
    /// the mix.
    processed: Vec<[P::Acc; 2]>,
    /// `processed` holds the chain's output this cycle. Else the chain left
    /// `buf` as it was.
    altered: bool,
    /// The captured frames faded by the engine, in the processing format.
    faded: Vec<[P::Acc; 2]>,
    /// Which frames play this cycle.
    plays: Plays,
    /// Where the engine's own fade of the captured frames stands, from 0
    /// to 1. Only forced bit-perfect past a chain that alters the audio
    /// uses it: it plays the frames as captured, which the detector's fades
    /// are not in.
    raw_level: f32,
    /// The chain reports signal this cycle: a recorder gets its output.
    signal: bool,
    /// The chain's latency, in the cycles its output is used, else 0.
    latency: u32,
    gain: f32,
    /// This cycle's period goes into the mix, unless DoP has the output.
    audible: bool,
    /// What `Event::InputSignal` last said.
    audible_reported: bool,
}

impl<P: Processing> Analog<P> {
    /// No capture, an empty chain, unity gain.
    fn new(max_frames: usize) -> Analog<P> {
        Analog {
            capture: None,
            closed: false,
            failed: None,
            chain: Chain::empty(),
            buf: vec![[0; 2]; max_frames],
            processed: vec![[P::zero(); 2]; max_frames],
            altered: false,
            faded: vec![[P::zero(); 2]; max_frames],
            plays: Plays::Captured,
            raw_level: 0.0,
            signal: false,
            latency: 0,
            gain: 1.0,
            audible: false,
            audible_reported: false,
        }
    }

    /// Puts in another capture, and returns the one it replaces. That may
    /// be one that failed and was not reported yet: it goes back as the
    /// one replaced, and is not reported.
    fn replace(&mut self, capture: Option<Box<dyn Capture>>) -> Option<Box<dyn Capture>> {
        self.closed = false;
        self.audible = false;
        self.signal = false;
        self.raw_level = 0.0;
        let old = std::mem::replace(&mut self.capture, capture);
        old.or_else(|| self.failed.take())
    }

    /// Stops the capture, once, and silences the input.
    fn close(&mut self) {
        if let Some(capture) = self.capture.as_mut()
            && !self.closed
        {
            capture.close();
        }
        // One that failed and waits for room in the event queue holds the
        // device too. Closing it twice does no harm.
        if let Some(failed) = self.failed.as_mut() {
            failed.close();
        }
        self.closed = true;
        self.audible = false;
        self.signal = false;
        self.raw_level = 0.0;
    }

    /// Reads one period into `buf` and runs the chain on it, which decides
    /// whether it is audible. A capture that fails is put aside, to go back
    /// to the control thread, and the input is silent.
    /// `store` is the mix's, free until the mix starts. Without `plugins`,
    /// in forced bit-perfect, the frames that play stay as captured, faded
    /// in and out as the detector opens and closes; the chain's output
    /// still goes to the recorders.
    fn read(
        &mut self,
        period: usize,
        rate: u32,
        counters: &Counters,
        store: &mut BlockStore,
        plugins: bool,
    ) {
        self.audible = false;
        self.signal = false;
        self.altered = false;
        self.latency = 0;
        self.plays = Plays::Captured;
        let Some(capture) = self.capture.as_mut() else {
            return;
        };
        if self.closed {
            return;
        }
        let buf = &mut self.buf[..period];
        if fetch(capture.as_mut(), buf, counters).is_err() {
            // Closing a PCM frees memory and can wait, so the control
            // thread does it.
            self.failed = self.capture.take();
            self.closed = true;
            return;
        }
        if self.chain.is_empty() || !self.chain.fits(rate) {
            // Nothing measures it, so it has no signal: the analog input
            // never passes ungated. The control thread always sends a
            // chain with a detector, built for the rate the engine runs at
            // once the chain for a new rate has come.
            return;
        }
        // Into the processing format as the mix does it.
        let processed = &mut self.processed[..period];
        mix_clear::<P>(processed);
        mix_add::<P>(buf, Ramp::flat(1.0), processed);
        let mut block = P::to_block(processed, store);
        let status = self.chain.process(block.reborrow());
        let signal = !status.no_signal;
        let neutral = self.chain.neutral();
        // A neutral chain changed nothing unless the detector faded or
        // zeroed, so the frames stay as captured and can be copied
        // bit-perfect.
        if !neutral || status.changed {
            P::from_block(block, processed);
            self.altered = true;
            self.latency = self.chain.latency_frames();
        }
        self.signal = signal;
        if plugins || neutral {
            if self.altered {
                self.plays = Plays::Processed;
            }
            self.raw_level = if signal { 1.0 } else { 0.0 };
            self.audible = signal;
        } else {
            self.audible = self.fade_raw(signal, period, rate);
        }
    }

    /// Fades the captured frames towards full level with `signal`, else
    /// towards silence, over 10 ms from the start of the period, into
    /// `faded`. Returns whether anything of them is heard.
    fn fade_raw(&mut self, signal: bool, period: usize, rate: u32) -> bool {
        let target = if signal { 1.0 } else { 0.0 };
        if self.raw_level == target {
            return signal;
        }
        let fade = (rate * FADE_MS / 1000).max(1) as f32;
        let left = ((target - self.raw_level).abs() * fade).round() as usize;
        let frames = left.min(period);
        let to = if frames == left {
            target
        } else {
            self.raw_level + (target - self.raw_level) * frames as f32 / left as f32
        };
        let faded = &mut self.faded[..period];
        let (fading, steady) = faded.split_at_mut(frames);
        mix_clear::<P>(fading);
        mix_clear::<P>(steady);
        let ramp = Ramp {
            from: self.raw_level,
            to,
        };
        mix_add::<P>(&self.buf[..frames], ramp, fading);
        mix_add::<P>(&self.buf[frames..period], Ramp::flat(to), steady);
        self.raw_level = to;
        self.plays = Plays::Faded;
        true
    }

    /// Adds this cycle's period at `level` to the mix, along the fade.
    fn mix(&self, fade: Fade, level: f32, acc: &mut [[P::Acc; 2]]) {
        let period = acc.len();
        match self.plays {
            Plays::Captured => fade.add(&self.buf[..period], level, acc, mix_add::<P>),
            Plays::Processed => fade.add(&self.processed[..period], level, acc, mix_add_acc::<P>),
            Plays::Faded => fade.add(&self.faded[..period], level, acc, mix_add_acc::<P>),
        }
    }
}

/// Which of its buffers the analog input plays from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Plays {
    /// `buf`, as captured.
    Captured,
    /// `processed`, the chain's output.
    Processed,
    /// `faded`, as captured but faded by the engine.
    Faded,
}

/// A capture stream as the audio thread sees it: a ring it writes the
/// analog input into, after the input's chain, for a client to read. The
/// roles of the ring are those of a source swapped: the engine moves only
/// `write_pos`, and the client only `read_pos`.
pub struct Tap {
    pub id: ClientId,
    pub ring: Ring,
    /// An eventfd, non-blocking, written as `notify` decides.
    pub wake: OwnedFd,
    pub running: bool,
    /// The position in the header's `wake_at` that has had its wake-up.
    woken_at: u64,
    /// The overruns the client has been woken for.
    overruns_told: u64,
}

impl Tap {
    /// A stopped tap.
    pub fn new(id: ClientId, ring: Ring, wake: OwnedFd) -> Tap {
        Tap {
            id,
            ring,
            wake,
            running: false,
            woken_at: 0,
            overruns_told: 0,
        }
    }

    /// Writes what fits of `block`. The rest is dropped and counted in the
    /// header's `underruns`, which for a tap counts frames lost to a full
    /// ring. The engine never waits for a recorder.
    fn write(&mut self, block: &[[i32; 2]]) {
        let n = self.ring.write(block);
        if n < block.len() {
            let overruns = &self.ring.header().underruns;
            overruns.fetch_add((block.len() - n) as u64, Ordering::Relaxed);
        }
    }

    /// Wakes the client if it is due: after every period, or once the write
    /// position reaches the one it waits for, and after each overrun. The
    /// rule a source follows, with the read position swapped for the write
    /// position.
    fn notify(&mut self) {
        let header = self.ring.header();
        // The client stores the position it waits for and then loads ours.
        // With a full barrier on both sides, one of the two sees the other.
        fence(Ordering::SeqCst);
        let wake_at = header.wake_at.load(Ordering::SeqCst);
        let reached = header.write_pos.load(Ordering::Relaxed) >= wake_at;
        let overruns = header.underruns.load(Ordering::Relaxed);
        let due = wake_at == 0 || (reached && wake_at != self.woken_at);
        if !due && overruns == self.overruns_told {
            return;
        }
        self.woken_at = wake_at;
        self.overruns_told = overruns;
        let one = 1u64;
        // SAFETY: a plain write of 8 bytes. The fd is non-blocking, and a full
        // counter only means the client has plenty of wake-ups waiting.
        unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                (&raw const one).cast(),
                size_of::<u64>(),
            )
        };
    }
}

/// The capture streams, in a table that grows as the source table does:
/// the control thread sends a bigger empty one, and the old one goes back.
struct Taps {
    #[allow(clippy::vec_box)] // Moving a boxed tap moves only a pointer.
    table: Vec<Box<Tap>>,
    /// The period converted from the chain's processing format, or zeros.
    buf: Vec<[i32; 2]>,
}

impl Taps {
    fn new(max_frames: usize) -> Taps {
        Taps {
            table: Vec::with_capacity(INITIAL_TAPS),
            buf: vec![[0; 2]; max_frames],
        }
    }

    fn find(&mut self, id: ClientId) -> Option<&mut Tap> {
        let found = self.table.iter_mut().find(|t| t.id == id);
        found.map(|t| t.as_mut())
    }

    /// Takes the tap in, or hands it back if the table is full.
    fn add(&mut self, tap: Box<Tap>) -> Option<Box<Tap>> {
        if self.table.len() < self.table.capacity() {
            self.table.push(tap);
            None
        } else {
            Some(tap)
        }
    }

    fn remove(&mut self, id: ClientId) -> Option<Box<Tap>> {
        let index = self.table.iter().position(|t| t.id == id)?;
        Some(self.table.remove(index))
    }

    /// Moves the taps into `table` and returns the table they leave, or
    /// returns `table` itself if it has no room for them.
    #[allow(clippy::vec_box)] // Moving a boxed tap moves only a pointer.
    fn grow(&mut self, mut table: Vec<Box<Tap>>) -> Vec<Box<Tap>> {
        if table.capacity() - table.len() < self.table.len() {
            return table;
        }
        table.append(&mut self.table);
        std::mem::replace(&mut self.table, table)
    }

    /// Writes this cycle's analog period to every running tap and wakes
    /// it as due: the period after the chain while the input has signal,
    /// in forced bit-perfect too, else zeros, so that a tap keeps real time
    /// without a signal or a capture. Never the mix.
    fn deliver<P: Processing>(&mut self, analog: &Analog<P>, period: usize) {
        let Taps { table, buf } = self;
        if !table.iter().any(|t| t.running) {
            return;
        }
        let block = if !analog.signal {
            buf[..period].fill([0; 2]);
            &buf[..period]
        } else if analog.altered {
            // Into 32 bits as the mix's last step does it.
            mix_finish::<P>(&analog.processed[..period], &mut buf[..period]);
            &buf[..period]
        } else {
            &analog.buf[..period]
        };
        // As the sink's delay for a player: what the chain holds back.
        let latency = if analog.signal { analog.latency } else { 0 };
        for tap in table.iter_mut().filter(|t| t.running) {
            let header = tap.ring.header();
            header.output_delay_frames.store(latency, Ordering::Relaxed);
            tap.write(block);
            tap.notify();
        }
    }
}

/// Fills `buf` with the latest period the capture has. More than two periods
/// queued are cut back to two, and a short read is padded with zeros in
/// front. Both are counted.
fn fetch(
    capture: &mut dyn Capture,
    buf: &mut [[i32; 2]],
    counters: &Counters,
) -> Result<(), SinkError> {
    let period = buf.len();
    let queued = capture.available()?;
    if queued > 2 * period {
        let excess = queued - 2 * period;
        capture.skip(excess)?;
        let dropped = &counters.analog_dropped_frames;
        dropped.fetch_add(excess as u64, Ordering::Relaxed);
    }
    let n = capture.read(buf)?.min(period);
    if n < period {
        buf.copy_within(..n, period - n);
        buf[..period - n].fill([0; 2]);
        let padded = &counters.analog_padded_frames;
        padded.fetch_add((period - n) as u64, Ordering::Relaxed);
    }
    Ok(())
}

/// Fades `out` in after silence: `left` of `fade` frames of the fade are
/// still to come, and fewer after the call.
/// A `left` beyond `fade` is taken as `fade`: the fade starts from zero.
fn unmute(out: &mut [[i32; 2]], left: &mut usize, fade: usize) {
    *left = (*left).min(fade);
    for frame in out.iter_mut() {
        if *left == 0 {
            break;
        }
        let gain = (fade - *left) as f64 / fade as f64;
        *frame = frame.map(|v| (f64::from(v) * gain).round() as i32);
        *left -= 1;
    }
}

/// How the output gain moves over one period.
#[derive(Clone, Copy)]
struct Fade {
    from: f32,
    to: f32,
    /// Frames from the start over which the gain goes from `from` to `to`.
    frames: usize,
    /// Down to zero over the whole period, before a reopen.
    out: bool,
}

impl Fade {
    /// Adds `samples` at `level` to the mix, along the fade.
    fn mix<P: Processing>(self, samples: &[[i32; 2]], level: f32, acc: &mut [[P::Acc; 2]]) {
        self.add(samples, level, acc, mix_add::<P>);
    }

    /// Adds `samples` with `add`, which takes a ramp over all of its `acc`.
    fn add<T, A>(self, samples: &[T], level: f32, acc: &mut [A], add: fn(&[T], Ramp, &mut [A])) {
        if self.out {
            let ramp = Ramp {
                from: self.from * level,
                to: 0.0,
            };
            add(samples, ramp, acc);
            return;
        }
        let (fading, steady) = samples.split_at(self.frames);
        let (acc_fading, acc_steady) = acc.split_at_mut(self.frames);
        let ramp = Ramp {
            from: self.from * level,
            to: self.to * level,
        };
        add(fading, ramp, acc_fading);
        add(steady, Ramp::flat(self.to * level), acc_steady);
    }
}

/// Where a chain of plugins runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// On the analog input, before it is mixed.
    Analog,
    /// On the mix of all sources, after the output volume.
    Mix,
    /// On what goes to the card, after the mix chain.
    Output,
}

/// Decisions from the control thread, carried out in order.
pub enum Command {
    /// Refused with `Event::Dropped` if the table is full.
    Add(Box<Source>),
    Remove(ClientId),
    SetRunning(ClientId, bool),
    /// Stops the source and empties its ring. Answered by `Event::Flushed`.
    Flush(ClientId),
    SetResampler(ClientId, Option<Box<Resampler>>),
    /// Opens the sink, or reopens it after a period of fading out. The fade
    /// comes first even for commands queued ahead of this one, so resamplers
    /// sent just before it take effect together with it.
    Reconfigure {
        rate: u32,
        period: u32,
    },
    CloseOutput,
    /// The output gain. Fades over 10 ms from the current level.
    SetGain(f32),
    /// An empty table with more capacity. It replaces the current one.
    #[allow(clippy::vec_box)] // Moving a boxed source moves only a pointer.
    Grow(Vec<Box<Source>>),
    /// Replaces the chain at a place. Answered by `Event::OldChain`. The
    /// chain is in the engine's processing format and, if it has a plugin,
    /// built for `MAX_PERIOD` frames, so that no period change can outgrow
    /// it. A shorter one is refused: it comes back as the `OldChain` and the
    /// chain in place stays. The control thread sends the chains for a new
    /// rate just before its `Reconfigure`, so that they take effect with the
    /// reopen, as resamplers do. In forced bit-perfect the mix and output
    /// chains are bypassed, and the analog chain runs only for its
    /// detector: its output is not used.
    SetChain(Place, Box<Chain>),
    /// Sets a parameter of one plugin in a chain. One that is not there is
    /// ignored.
    SetParam {
        place: Place,
        plugin: u8,
        index: u8,
        value: f64,
    },
    /// Replaces the capture the analog input reads. Answered by
    /// `Event::OldCapture`, also when there was none, so that every one is
    /// answered. The engine closes a capture before it reopens the output
    /// and keeps the closed one until it is replaced here. A capture comes
    /// back closed or not, so its `Drop` must release the device. A
    /// `CaptureFailed` that arrives before the `OldCapture` belongs to the
    /// capture this command replaced.
    SetCapture(Option<Box<dyn Capture>>),
    /// The gain of the analog input. Applies at once, as a source's does.
    SetAnalogGain(f32),
    /// A capture stream. Refused with `Event::DroppedTap` if the table is
    /// full. `SetRunning` and `Flush` find it by its id; a flushed tap is
    /// stopped, and its ring is left for the client to empty.
    AddTap(Box<Tap>),
    /// Answered by `Event::DroppedTap`.
    RemoveTap(ClientId),
    /// An empty tap table with more capacity. It replaces the current one.
    #[allow(clippy::vec_box)] // Moving a boxed tap moves only a pointer.
    GrowTaps(Vec<Box<Tap>>),
    Shutdown,
}

/// What the audio thread hands back or has noticed.
pub enum Event {
    Dropped(Box<Source>),
    Replaced(Option<Box<Resampler>>),
    OldTable(Vec<Box<Source>>),
    /// The chain a `SetChain` replaced, to be freed off the audio thread.
    OldChain(Box<Chain>),
    /// The client plays DoP, with the number of its claim on the output.
    /// The lowest number among the claims is the oldest, whatever order
    /// the events arrive in.
    DopStarted(ClientId, u64),
    DopStopped(ClientId),
    Drained(ClientId),
    Flushed(ClientId),
    /// One for every `Reconfigure`: when the sink is open, or when a later
    /// `Reconfigure`, a close or the shutdown has taken its place.
    Reconfigured,
    /// The capture a `SetCapture` replaced, to be dropped off the audio
    /// thread.
    OldCapture(Option<Box<dyn Capture>>),
    /// The analog input has signal (`true`) or not, as its chain reports.
    /// While DoP holds the output it has signal but is not played. Sent
    /// when that changes.
    /// It starts out not mixed. While the queue is full only the latest
    /// state is sent, once there is room.
    InputSignal(bool),
    /// A capture call failed. The engine reads it no more and hands it
    /// back, still open, for the control thread to close. Sent once per
    /// capture. One that arrives before the `OldCapture` answering a
    /// `SetCapture` belongs to the capture that command replaced.
    CaptureFailed(Box<dyn Capture>),
    /// A tap removed, or refused for lack of room.
    DroppedTap(Box<Tap>),
    /// The tap table a `GrowTaps` replaced, or the one it brought if that
    /// had no room for the taps.
    #[allow(clippy::vec_box)] // Moving a boxed tap moves only a pointer.
    OldTaps(Vec<Box<Tap>>),
}

/// Runs the cycles of one output.
pub struct Engine<P: Processing, S: Sink> {
    sink: S,
    commands: Consumer<Command>,
    events: Producer<Event>,
    counters: Arc<Counters>,
    mode: BitPerfect,
    /// In the order the sources were added.
    #[allow(clippy::vec_box)] // Moving a boxed source moves only a pointer.
    table: Vec<Box<Source>>,
    /// Events the full queue would not take, oldest first.
    backlog: VecDeque<Event>,
    out: Vec<[i32; 2]>,
    scratch: Vec<[i32; 2]>,
    acc: Vec<[P::Acc; 2]>,
    /// Where the mix is handed to the plugins.
    store: BlockStore,
    analog: Analog<P>,
    taps: Taps,
    mix: Chain,
    output: Chain,
    rate: u32,
    period: usize,
    open: bool,
    /// A format waiting to be opened.
    reopen: Option<(u32, u32)>,
    /// The fade for a queued `Reconfigure` has been played.
    faded: bool,
    stopped: bool,
    /// The output gain at the start of the next cycle.
    gain: f32,
    target_gain: f32,
    /// Frames until `gain` reaches `target_gain`.
    fade_left: usize,
    /// Frames of the fade in left after the mix or output chain was built
    /// for another rate than the engine runs at, and the output was
    /// silent for it.
    unmute_left: usize,
    last_start: Option<Instant>,
    /// The last DoP claim handed out.
    claims: u64,
}

impl<P: Processing, S: Sink> Engine<P, S> {
    /// A closed engine. The first `Reconfigure` opens the output.
    pub fn new(
        sink: S,
        commands: Consumer<Command>,
        events: Producer<Event>,
        counters: Arc<Counters>,
        mode: BitPerfect,
    ) -> Self {
        let frames = MAX_PERIOD as usize;
        Engine {
            sink,
            commands,
            events,
            counters,
            mode,
            table: Vec::with_capacity(INITIAL_SOURCES),
            backlog: VecDeque::with_capacity(BACKLOG),
            out: vec![[0; 2]; frames],
            scratch: vec![[0; 2]; frames * MAX_DOWNSAMPLING + dop::MIN_FRAMES],
            acc: vec![[P::zero(); 2]; frames],
            store: BlockStore::new(frames),
            analog: Analog::new(frames),
            taps: Taps::new(frames),
            mix: Chain::empty(),
            output: Chain::empty(),
            rate: 0,
            period: 0,
            open: false,
            reopen: None,
            faded: false,
            stopped: false,
            gain: 1.0,
            target_gain: 1.0,
            fade_left: 0,
            unmute_left: 0,
            last_start: None,
            claims: 0,
        }
    }

    /// True while there is no open sink. `cycle` then returns at once, so the
    /// caller must wait for a command rather than call it in a loop.
    pub fn is_closed(&self) -> bool {
        !self.open
    }

    /// True after `Shutdown`. Nothing more will happen.
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// True while events wait for the control thread, in the queue or held
    /// back. Lets the caller wake the control thread only when needed.
    pub fn has_unread_events(&self) -> bool {
        !self.backlog.is_empty() || self.events.slots() < self.events.buffer().capacity()
    }

    /// Carries out queued commands and, with an open sink, plays one period.
    ///
    /// An error from the sink comes after the rest of the cycle is done. A
    /// failed open is retried on the next call.
    pub fn cycle(&mut self) -> Result<(), SinkError> {
        if self.stopped {
            return Ok(());
        }
        self.flush_backlog();
        // The wait comes before the commands are read, so that one sent
        // during it, such as a stop, takes effect in this cycle.
        let waited = self.wait_room();
        let mut start = Instant::now();
        let result = if self.open && !self.faded && self.reconfigure_queued() {
            self.faded = true;
            self.render(true, start)
        } else {
            self.drain_commands();
            let reopening = self.reopen.is_some();
            match self.apply_reopen() {
                // The sources are still reported below.
                Err(e) => Err(e),
                Ok(()) => {
                    if reopening {
                        // Opening the card is not part of the cycle's work.
                        start = Instant::now();
                    }
                    if self.open {
                        self.render(false, start)
                    } else {
                        Ok(())
                    }
                }
            }
        };
        self.report();
        waited.and(result)
    }

    /// Waits until the open sink has room for a period. This is the audio
    /// thread's only wait.
    fn wait_room(&mut self) -> Result<(), SinkError> {
        if !self.open {
            return Ok(());
        }
        let waited = self.sink.wait_room();
        if waited == Err(SinkError::Xrun) {
            self.counters.xruns.fetch_add(1, Ordering::Relaxed);
        }
        waited
    }

    fn flush_backlog(&mut self) {
        while let Some(event) = self.backlog.pop_front() {
            if let Err(PushError::Full(event)) = self.events.push(event) {
                self.backlog.push_front(event);
                break;
            }
        }
    }

    /// Sends an event, or keeps it until the queue has room. `drain_commands`
    /// makes sure the backlog has space.
    fn emit(&mut self, event: Event) {
        if !self.backlog.is_empty() {
            self.backlog.push_back(event);
        } else if let Err(PushError::Full(event)) = self.events.push(event) {
            self.backlog.push_back(event);
        }
    }

    /// Looks through the queue without taking anything from it.
    fn reconfigure_queued(&mut self) -> bool {
        let waiting = self.commands.slots();
        let Ok(chunk) = self.commands.read_chunk(waiting) else {
            return false;
        };
        let (first, second) = chunk.as_slices();
        let is_reconfigure = |c: &Command| matches!(c, Command::Reconfigure { .. });
        first.iter().chain(second).any(is_reconfigure)
    }

    /// Stops early when the backlog is nearly full, which leaves the rest of
    /// the commands queued until the control thread has caught up.
    fn drain_commands(&mut self) {
        while !self.stopped && self.backlog.len() + EVENTS_PER_COMMAND <= BACKLOG {
            let Ok(command) = self.commands.pop() else {
                break;
            };
            self.execute(command);
        }
    }

    fn execute(&mut self, command: Command) {
        match command {
            Command::Add(source) => self.add(source),
            Command::Remove(id) => self.remove(id),
            Command::SetRunning(id, running) => self.set_running(id, running),
            Command::Flush(id) => {
                self.set_running(id, false);
                if let Some(source) = self.find(id) {
                    source.flush();
                }
                self.emit(Event::Flushed(id));
            }
            Command::SetResampler(id, resampler) => {
                let replaced = match self.find(id) {
                    Some(source) => std::mem::replace(&mut source.resampler, resampler),
                    None => resampler,
                };
                self.emit(Event::Replaced(replaced));
            }
            Command::Reconfigure { rate, period } => {
                let overtaken = self.reopen.replace((rate, period.min(MAX_PERIOD)));
                self.faded = false;
                if overtaken.is_some() {
                    self.emit(Event::Reconfigured);
                }
            }
            Command::CloseOutput => {
                self.cancel_reopen();
                self.close();
            }
            Command::SetGain(gain) => self.set_gain(gain),
            Command::Grow(table) => self.grow(table),
            Command::SetChain(place, mut chain) => {
                // A chain that could not take the longest period would fail
                // after a rate change, so it is sent straight back.
                if chain.max_frames() >= MAX_PERIOD as usize {
                    // The old chain goes back in the box the new one came in.
                    std::mem::swap(self.chain(place), &mut chain);
                }
                self.emit(Event::OldChain(chain));
            }
            Command::SetParam {
                place,
                plugin,
                index,
                value,
            } => self
                .chain(place)
                .set_param(plugin.into(), index.into(), value),
            Command::SetCapture(capture) => {
                if capture.is_some() {
                    let enabled = &self.counters.analog_enabled;
                    enabled.store(true, Ordering::Relaxed);
                }
                let old = self.analog.replace(capture);
                self.emit(Event::OldCapture(old));
            }
            Command::SetAnalogGain(gain) => self.analog.gain = gain,
            Command::AddTap(tap) => {
                if let Some(refused) = self.taps.add(tap) {
                    self.emit(Event::DroppedTap(refused));
                }
            }
            Command::RemoveTap(id) => {
                if let Some(tap) = self.taps.remove(id) {
                    self.emit(Event::DroppedTap(tap));
                }
            }
            Command::GrowTaps(table) => {
                let old = self.taps.grow(table);
                self.emit(Event::OldTaps(old));
            }
            Command::Shutdown => {
                self.cancel_reopen();
                self.close();
                self.stopped = true;
            }
        }
    }

    /// Drops an open that is still waiting. It is answered like one that
    /// was done, so that the control thread, which counts them, is never
    /// left waiting for an answer.
    fn cancel_reopen(&mut self) {
        if self.reopen.take().is_some() {
            self.emit(Event::Reconfigured);
        }
    }

    fn chain(&mut self, place: Place) -> &mut Chain {
        match place {
            Place::Analog => &mut self.analog.chain,
            Place::Mix => &mut self.mix,
            Place::Output => &mut self.output,
        }
    }

    fn find(&mut self, id: ClientId) -> Option<&mut Source> {
        let found = self.table.iter_mut().find(|s| s.id == id);
        found.map(|s| s.as_mut())
    }

    /// A stop ends a DoP stream's claim at once, so that a restart in the
    /// same cycle makes it again. The stream resumes as DoP if it left
    /// frames behind. With nothing left there is nothing to protect, so the
    /// next frames are judged afresh, bar a tail of the run.
    /// A tap, found when no source has the id, is only started or stopped.
    fn set_running(&mut self, id: ClientId, running: bool) {
        let Some(source) = self.find(id) else {
            if let Some(tap) = self.taps.find(id) {
                tap.running = running;
            }
            return;
        };
        source.running = running;
        if running {
            // A restarted stream drains afresh, and reports it again.
            source.drained_reported = false;
            return;
        }
        // The credit for a dropped tail belongs to the stream that ends
        // here. The memory of a cut run stays: frames left in the ring can
        // be the rest of that run, and must not be mixed after a restart.
        source.dropped = 0;
        if source.stream == Stream::Dop {
            if source.ring.available() > 0 {
                source.in_dop_run = true;
            } else {
                source.cut_run = true;
            }
        }
        source.stream = Stream::Unknown;
        if std::mem::take(&mut source.dop_reported) {
            self.emit(Event::DopStopped(id));
        }
    }

    fn add(&mut self, source: Box<Source>) {
        if self.table.len() < self.table.capacity() {
            self.table.push(source);
        } else {
            self.emit(Event::Dropped(source));
        }
    }

    fn remove(&mut self, id: ClientId) {
        let Some(index) = self.table.iter().position(|s| s.id == id) else {
            return;
        };
        let source = self.table.remove(index);
        if source.dop_reported {
            self.emit(Event::DopStopped(id));
        }
        self.emit(Event::Dropped(source));
    }

    #[allow(clippy::vec_box)] // Moving a boxed source moves only a pointer.
    fn grow(&mut self, mut table: Vec<Box<Source>>) {
        if table.capacity() - table.len() < self.table.len() {
            self.emit(Event::OldTable(table));
            return;
        }
        table.append(&mut self.table);
        let old = std::mem::replace(&mut self.table, table);
        self.emit(Event::OldTable(old));
    }

    /// Closes the capture too, and every capture still waiting to be handed
    /// back: these cards take one rate at a time, so no capture may hold
    /// the card when the output opens at another. One already in the event
    /// queue is the control thread's, which closes it when it takes it. If
    /// that comes after the reopen, the playback open at the new rate can
    /// fail while that capture holds the card; it is tried again in the
    /// next cycle.
    fn close(&mut self) {
        self.analog.close();
        for event in self.backlog.iter_mut() {
            if let Event::OldCapture(Some(c)) | Event::CaptureFailed(c) = event {
                c.close();
            }
        }
        if self.open {
            self.sink.close();
            self.open = false;
        }
        self.finish_fade();
    }

    fn apply_reopen(&mut self) -> Result<(), SinkError> {
        let Some((rate, period)) = self.reopen else {
            return Ok(());
        };
        self.close();
        self.sink.open(rate, period)?;
        self.reopen = None;
        self.open = true;
        self.rate = rate;
        // A fade in still due is counted in frames of the old rate: it
        // starts again, a whole 10 ms at this one.
        if self.unmute_left > 0 {
            self.unmute_left = (rate * FADE_MS / 1000) as usize;
        }
        self.period = period as usize;
        self.last_start = None;
        self.emit(Event::Reconfigured);
        Ok(())
    }

    /// A change fades only while something is audible, to avoid a click.
    /// Otherwise it applies at once, so that a stream starting in the same
    /// cycle as the change plays at the new level from its first frame.
    fn set_gain(&mut self, gain: f32) {
        if gain == self.target_gain {
            return;
        }
        self.target_gain = gain;
        let heard = self.audible().next().is_some() || self.analog.audible;
        if self.open && heard {
            self.fade_left = (self.rate * FADE_MS / 1000) as usize;
        } else {
            self.finish_fade();
        }
    }

    /// Running sources that are heard this cycle.
    fn audible(&self) -> impl Iterator<Item = &Source> {
        let rate = self.rate;
        let sources = self.table.iter().map(|s| &**s);
        sources.filter(move |s| s.running && !s.muted(rate) && s.plays)
    }

    fn finish_fade(&mut self) {
        self.gain = self.target_gain;
        self.fade_left = 0;
    }

    /// The gain after this cycle's part of the fade, and how many frames of
    /// the period that part covers.
    fn next_fade_step(&self) -> (f32, usize) {
        let frames = self.fade_left.min(self.period);
        if frames == self.fade_left {
            return (self.target_gain, frames);
        }
        let share = frames as f32 / self.fade_left as f32;
        (self.gain + (self.target_gain - self.gain) * share, frames)
    }

    /// Plays one period. `fade_out` takes the level down to zero over it.
    ///
    /// The cycle has waited for room on the card before this and only now
    /// reads the clients, so what it reads is written at once and waits
    /// behind no more than the card's buffer. The cycle is timed from
    /// `start`, the end of that wait.
    fn render(&mut self, fade_out: bool, start: Instant) -> Result<(), SinkError> {
        let store = &mut self.store;
        let plugins = plugins_apply(self.mode);
        self.analog
            .read(self.period, self.rate, &self.counters, store, plugins);
        self.taps.deliver(&self.analog, self.period);
        self.scan();
        self.order_claims();
        let rate = self.rate;
        let holders = self.table.iter().enumerate();
        let holders = holders.filter(|(_, s)| s.holds_output(rate));
        let owner = holders.min_by_key(|(_, s)| s.dop_claim).map(|(i, _)| i);
        let chains_ran = match owner {
            Some(owner) => {
                self.play_dop(owner);
                false
            }
            None => self.play_pcm(fade_out),
        };
        let before_write = Instant::now();
        let result = self.sink.write(&self.out[..self.period]);
        let after_write = Instant::now();
        if result == Err(SinkError::Xrun) {
            self.counters.xruns.fetch_add(1, Ordering::Relaxed);
        }
        // What the plugins hold back plays after the ring too, in the
        // cycles they ran in.
        let plugins = if chains_ran {
            self.mix.latency_frames() + self.output.latency_frames()
        } else {
            0
        };
        let delay = self.sink.delay_frames() + plugins;
        for source in self.table.iter_mut().filter(|s| s.running) {
            source.notify(delay, rate);
        }
        let work = before_write - start + after_write.elapsed();
        self.record_timing(start, work.as_nanos() as u64);
        if self.sink.switched() {
            // The write opened another device. The next cycle has nothing
            // to measure from, as after any open.
            self.last_start = None;
        }
        result
    }

    fn record_timing(&mut self, start: Instant, cycle_ns: u64) {
        let last = self.last_start.replace(start);
        let frames = u64::from(PERIODS) * self.period as u64;
        let buffer_ns = frames * 1_000_000_000 / u64::from(self.rate);
        // The first cycle after opening has nothing to measure from.
        let gap_ns = last.map_or(0, |l| (start - l).as_nanos() as u64);
        self.counters.on_cycle(gap_ns, cycle_ns, buffer_ns);
    }

    /// Gives each stream newly seen as DoP the next claim, in table order
    /// within a cycle, as `report` sends `DopStarted`. A stream that is no
    /// longer DoP, or has stalled, gives its claim up.
    fn order_claims(&mut self) {
        let rate = self.rate;
        for source in self.table.iter_mut() {
            source.note_stall(rate);
            if source.stream != Stream::Dop || source.stalled {
                source.dop_claim = 0;
            } else if source.dop_claim == 0 {
                self.claims += 1;
                source.dop_claim = self.claims;
            }
        }
    }

    /// Looks at what each running source has, without consuming it. The look
    /// goes a little past what a cycle reads, to judge frames at the end.
    fn scan(&mut self) {
        let period = self.period;
        let scratch = &mut self.scratch;
        for source in self.table.iter_mut().filter(|s| s.running) {
            let draining = source.draining();
            let input = source.period_input(period).max(period);
            let end = (input + dop::MIN_FRAMES).min(scratch.len());
            let n = source.ring.peek(&mut scratch[..end]);
            source.judge(&scratch[..n], period, draining);
        }
    }

    /// DoP has the output to itself and nothing touches its samples. The
    /// analog input has been read and measured, and is not played.
    fn play_dop(&mut self, owner: usize) {
        self.finish_fade();
        let (rate, period) = (self.rate, self.period);
        for (index, source) in self.table.iter_mut().enumerate() {
            if index == owner {
                source.read_dop(&mut self.out[..period]);
            } else if source.running {
                source.discard(period, rate);
            }
        }
    }

    /// What the bit-perfect rules need to know about this cycle.
    /// The analog input takes part only while it is audible.
    fn situation(&self) -> Situation {
        let audible = || self.audible();
        let analog = self.analog.audible;
        Situation {
            mode: self.mode,
            active: audible().count() + usize::from(analog),
            software_volume_at_0db: self.gain == 1.0
                && self.target_gain == 1.0
                && audible().all(|s| s.gain == 1.0)
                && (!analog || self.analog.gain == 1.0),
            resampled: audible().any(|s| s.resampler.is_some()),
            plugins_neutral: self.mix.neutral()
                && self.output.neutral()
                && (!analog || self.analog.chain.neutral()),
        }
    }

    /// Mixes the sources and the analog input while it is audible, or copies
    /// the one that is bit-perfect. A mix then runs the mix chain and the
    /// output chain, in that order, except in forced bit-perfect, which
    /// bypasses them. Returns whether a chain with plugins ran.
    fn play_pcm(&mut self, fade_out: bool) -> bool {
        let (rate, period) = (self.rate, self.period);
        let volumes = volumes_apply(self.mode);
        // A detector fading the analog input has changed its frames.
        let fading = self.analog.audible && self.analog.plays != Plays::Captured;
        // Nor is a fade in after a mute for a chain at another rate over.
        let unmuting = self.unmute_left > 0;
        let copy = !fade_out && !fading && !unmuting && decide(&self.situation()).is_ok();
        let (next_gain, fade_frames) = self.next_fade_step();
        let (from, to) = if volumes {
            (self.gain, next_gain)
        } else {
            (1.0, 1.0)
        };
        let fade = Fade {
            from,
            to,
            frames: fade_frames,
            out: fade_out,
        };
        let out = &mut self.out[..period];
        let scratch = &mut self.scratch[..period];
        let acc = &mut self.acc[..period];
        if !copy {
            mix_clear::<P>(acc);
        }
        for source in self.table.iter_mut().filter(|s| s.running) {
            if source.held > 0 {
                let held = &self.counters.held_frames;
                held.fetch_add(source.held as u64, Ordering::Relaxed);
            }
            if source.muted(rate) {
                source.discard(period, rate);
            } else if !source.plays {
                source.note_shortfall(0, 1);
            } else if copy {
                source.read_raw(out);
            } else {
                source.read_resampled(scratch);
                let level = if volumes { source.gain } else { 1.0 };
                fade.mix::<P>(scratch, level, acc);
            }
        }
        if self.analog.audible {
            if copy {
                // Only with a neutral chain, so `buf` holds the period.
                out.copy_from_slice(&self.analog.buf[..period]);
            } else {
                let level = if volumes { self.analog.gain } else { 1.0 };
                self.analog.mix(fade, level, acc);
            }
        }
        let has_plugins = !self.mix.is_empty() || !self.output.is_empty();
        let chains_ran = !copy && plugins_apply(self.mode) && has_plugins;
        // A chain never runs at a rate it was not built for, such as after
        // a DoP stream, until the one for this rate comes. Silence, not a
        // bypass: a crossover or a protection filter must not be skipped.
        if chains_ran && !(self.mix.fits(rate) && self.output.fits(rate)) {
            out.fill([0; 2]);
            self.unmute_left = (rate * FADE_MS / 1000) as usize;
            self.gain = next_gain;
            self.fade_left -= fade_frames;
            return false;
        }
        if !copy {
            // Without plugins the mix is not copied for them at all.
            if chains_ran {
                let mut block = P::to_block(acc, &mut self.store);
                self.mix.process(block.reborrow());
                self.output.process(block.reborrow());
                P::from_block(block, acc);
            }
            mix_finish::<P>(acc, out);
            if self.unmute_left > 0 {
                let fade = (rate * FADE_MS / 1000) as usize;
                unmute(out, &mut self.unmute_left, fade);
            }
        }
        self.gain = next_gain;
        self.fade_left -= fade_frames;
        chains_ran
    }

    /// Sends the events that follow from the state of the sources and of the
    /// analog input, and sets the input's counter. The events need no
    /// storage: what the queue will not take now is sent on a later cycle.
    fn report(&mut self) {
        let open = u64::from(self.analog.audible);
        self.counters.analog_open.store(open, Ordering::Relaxed);
        let latency = u64::from(self.analog.latency);
        let counted = &self.counters.analog_latency_frames;
        counted.store(latency, Ordering::Relaxed);
        if !self.backlog.is_empty() {
            return;
        }
        if let Some(capture) = self.analog.failed.take() {
            if let Err(rtrb::PushError::Full(event)) =
                self.events.push(Event::CaptureFailed(capture))
            {
                if let Event::CaptureFailed(capture) = event {
                    self.analog.failed = Some(capture);
                }
                return;
            }
        }
        let audible = self.analog.audible;
        if audible != self.analog.audible_reported {
            if self.events.push(Event::InputSignal(audible)).is_err() {
                return;
            }
            self.analog.audible_reported = audible;
        }
        for source in self.table.iter_mut() {
            let dop = source.stream == Stream::Dop && !source.stalled;
            if dop != source.dop_reported {
                let event = if dop {
                    Event::DopStarted(source.id, source.dop_claim)
                } else {
                    Event::DopStopped(source.id)
                };
                if self.events.push(event).is_err() {
                    return;
                }
                source.dop_reported = dop;
            }
            let drained = source.drained();
            let event = Event::Drained(source.id);
            if drained && !source.drained_reported && self.events.push(event).is_err() {
                return;
            }
            source.drained_reported = drained;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitperfect::NotBitPerfect;
    use crate::capture::{Capture, CaptureFeed, MemoryCapture};
    use crate::config::{Format, Quality};
    use crate::format::{BlockStore, F32, F64, I32, db_to_gain};
    use crate::plugin::detect::Detect;
    use crate::plugin::{Block, Formats, Param, ParamKind, Plugin, Status as PluginStatus};
    use crate::sink::{MemorySink, SinkLog};
    use aas_client::ring::Ring;
    use assert_no_alloc::assert_no_alloc;
    use rtrb::RingBuffer;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};
    use std::sync::atomic::Ordering::{Relaxed, Release};
    use std::time::{Duration, Instant};

    const RATE: u32 = 48000;
    const PERIOD: usize = 64;
    /// Not exact in `f32`, so it shows whether a path kept all 32 bits.
    const ODD: i32 = 0x1234_5679;

    struct Rig<P: Processing> {
        engine: Engine<P, MemorySink>,
        commands: rtrb::Producer<Command>,
        events: rtrb::Consumer<Event>,
        log: SinkLog,
        counters: Arc<Counters>,
    }

    impl<P: Processing> Rig<P> {
        fn new(mode: BitPerfect) -> Self {
            Self::with_event_slots(mode, 1024)
        }

        /// An engine that has not been told to open its output.
        fn closed(mode: BitPerfect, event_slots: usize) -> Self {
            let sink = MemorySink::new();
            let log = sink.log();
            log.reserve(1 << 20);
            let (commands, consumer) = RingBuffer::new(1024);
            let (producer, events) = RingBuffer::new(event_slots);
            let counters = Arc::new(Counters::default());
            let engine = Engine::new(sink, consumer, producer, counters.clone(), mode);
            Rig {
                engine,
                commands,
                events,
                log,
                counters,
            }
        }

        fn with_event_slots(mode: BitPerfect, event_slots: usize) -> Self {
            let mut rig = Self::closed(mode, event_slots);
            rig.send(Command::Reconfigure {
                rate: RATE,
                period: PERIOD as u32,
            });
            rig.cycle();
            rig.events();
            rig
        }

        fn send(&mut self, command: Command) {
            assert!(self.commands.push(command).is_ok());
        }

        fn cycle(&mut self) {
            self.engine.cycle().unwrap();
        }

        fn add(&mut self, client: &mut Client) {
            let source = client.source.take().unwrap();
            self.send(Command::Add(source));
            self.send(Command::SetRunning(client.id, true));
        }

        fn events(&mut self) -> Vec<Event> {
            let mut all = Vec::new();
            while let Ok(event) = self.events.pop() {
                all.push(event);
            }
            all
        }

        /// The frames of the latest `n` periods.
        fn last_periods(&self, n: usize) -> Vec<[i32; 2]> {
            let frames = self.log.frames();
            frames[frames.len() - n * PERIOD..].to_vec()
        }
    }

    struct Client {
        id: ClientId,
        /// The client's own mapping of the ring.
        ring: Ring,
        wake: OwnedFd,
        source: Option<Box<Source>>,
    }

    fn eventfd() -> OwnedFd {
        let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(raw >= 0);
        unsafe { OwnedFd::from_raw_fd(raw) }
    }

    fn client_at(id: ClientId, rate: u32) -> Client {
        let (ring, fd) = Ring::create(1 << 16).unwrap();
        let peer = Ring::map(fd.as_fd()).unwrap();
        let wake = eventfd();
        let theirs = wake.try_clone().unwrap();
        Client {
            id,
            ring: peer,
            wake,
            source: Some(Box::new(Source::new(id, ring, theirs, rate))),
        }
    }

    fn client(id: ClientId) -> Client {
        client_at(id, RATE)
    }

    fn underruns(client: &Client) -> u64 {
        client.ring.header().underruns.load(Relaxed)
    }

    fn frames(value: i32, n: usize) -> Vec<[i32; 2]> {
        vec![[value, -value]; n]
    }

    fn dop_frames(n: usize) -> Vec<[i32; 2]> {
        (0..n)
            .map(|i| {
                let marker: u32 = if i % 2 == 0 { 0x05 } else { 0xFA };
                let s = ((marker << 24) | 0x0012_3456) as i32;
                [s, s ^ 0x0000_FF00]
            })
            .collect()
    }

    /// DoP silence starting on `first`, the marker of the first frame.
    fn dop_silence(first: u32, n: usize) -> Vec<[i32; 2]> {
        (0..n)
            .map(|i| {
                let marker = if (i % 2 == 0) == (first == 0x05) {
                    0x05u32
                } else {
                    0xFA
                };
                [((marker << 24) | 0x0069_6900) as i32; 2]
            })
            .collect()
    }

    fn resampled(id: ClientId, format: Format) -> Client {
        let mut c = client_at(id, 44100);
        let r = Resampler::new(44100, RATE, PERIOD as u32, Quality::High, format);
        c.source.as_mut().unwrap().resampler = Some(Box::new(r));
        c
    }

    /// An engine whose fade to this gain has finished.
    fn rig_at_gain<P: Processing>(gain: f32) -> Rig<P> {
        let mut rig = Rig::<P>::new(BitPerfect::Auto);
        rig.send(Command::SetGain(gain));
        for _ in 0..10 {
            rig.cycle();
        }
        rig
    }

    fn count(events: &[Event], test: impl Fn(&Event) -> bool) -> usize {
        events.iter().filter(|e| test(e)).count()
    }

    fn bit_exact<P: Processing>() {
        let mut rig = Rig::<P>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        let input: Vec<[i32; 2]> = (0..PERIOD as i32 * 3)
            .map(|i| [ODD + i, i32::MIN + i])
            .collect();
        a.ring.write(&input);
        for _ in 0..3 {
            rig.cycle();
        }
        assert_eq!(rig.log.frames()[PERIOD..], input);
    }

    #[test]
    fn one_source_at_0db_reaches_the_sink_bit_exact() {
        bit_exact::<I32>();
        bit_exact::<F32>();
        bit_exact::<F64>();
    }

    #[test]
    fn with_bit_perfect_off_the_source_goes_through_the_mix() {
        let mut rig = Rig::<F32>::new(BitPerfect::Off);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(ODD, PERIOD));
        rig.cycle();
        assert_ne!(rig.last_periods(1), frames(ODD, PERIOD));
    }

    #[test]
    fn two_sources_are_mixed() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut b);
        a.ring.write(&frames(1000, PERIOD));
        b.ring.write(&frames(234, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(1234, PERIOD));
    }

    #[test]
    fn a_source_gain_applies() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut a = client(1);
        a.source.as_mut().unwrap().gain = 0.5;
        rig.add(&mut a);
        a.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(500, PERIOD));
    }

    #[test]
    fn a_source_that_is_not_running_is_left_alone() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        rig.send(Command::SetRunning(1, false));
        a.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        assert_eq!(a.ring.available(), PERIOD);
        assert_eq!(underruns(&a), 0);
        assert_eq!(rig.last_periods(1), frames(0, PERIOD));
    }

    #[test]
    fn an_underrunning_source_gets_silence_and_counts_it() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(ODD, PERIOD + 20));
        rig.cycle();
        assert_eq!(underruns(&a), 0);
        rig.cycle();
        assert_eq!(underruns(&a), 1);
        let mut want = frames(ODD, 20);
        want.extend(frames(0, PERIOD - 20));
        assert_eq!(rig.last_periods(1), want);
        rig.cycle();
        assert_eq!(underruns(&a), 2);
        assert_eq!(rig.last_periods(1), frames(0, PERIOD));
    }

    #[test]
    fn a_stalled_source_does_not_delay_the_cycle() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let (mut stalled, mut playing) = (client(1), client(2));
        rig.add(&mut stalled);
        rig.add(&mut playing);
        stalled.ring.write(&frames(ODD, PERIOD));
        playing.ring.write(&frames(ODD, PERIOD));
        rig.cycle();
        let start = Instant::now();
        for _ in 0..100 {
            playing.ring.write(&frames(ODD, PERIOD));
            rig.cycle();
        }
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(underruns(&stalled), 100);
        assert_eq!(underruns(&playing), 0);
        assert_eq!(rig.last_periods(100), frames(ODD, 100 * PERIOD));
    }

    #[test]
    fn removing_a_source_returns_it_through_an_event() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(7);
        rig.add(&mut a);
        rig.cycle();
        rig.send(Command::Remove(7));
        rig.cycle();
        let events = rig.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::Dropped(s) if s.id == 7));
    }

    #[test]
    fn a_source_the_table_has_no_room_for_comes_back() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut clients: Vec<Client> = (0..INITIAL_SOURCES as u64 + 1).map(client).collect();
        for c in clients.iter_mut() {
            rig.add(c);
        }
        rig.cycle();
        let events = rig.events();
        assert_eq!(events.len(), 1);
        let last = INITIAL_SOURCES as u64;
        assert!(matches!(&events[0], Event::Dropped(s) if s.id == last));
    }

    #[test]
    fn a_replaced_resampler_comes_back() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client_at(1, 44100);
        rig.add(&mut a);
        let make = || Box::new(Resampler::new(44100, RATE, 64, Quality::High, Format::F32));
        rig.send(Command::SetResampler(1, Some(make())));
        rig.send(Command::SetResampler(1, Some(make())));
        rig.send(Command::SetResampler(1, None));
        rig.send(Command::SetResampler(9, Some(make())));
        rig.cycle();
        let events = rig.events();
        let replaced = |e: &Event, some| matches!(e, Event::Replaced(r) if r.is_some() == some);
        assert_eq!(events.len(), 4);
        assert!(replaced(&events[0], false));
        assert!(replaced(&events[1], true));
        assert!(replaced(&events[2], true));
        assert!(replaced(&events[3], true));
    }

    fn no_allocation<P: Processing>(format: Format) {
        let mut rig = Rig::<P>::new(BitPerfect::Auto);
        let mut clients: Vec<Client> = (0..8).map(client).collect();
        clients.push(client_at(8, 44100));
        let mut late = client(9);
        let mut swapped = client_at(10, 44100);
        let make = || Resampler::new(44100, RATE, 64, Quality::High, format);
        clients[8].source.as_mut().unwrap().resampler = Some(Box::new(make()));
        for c in clients.iter_mut() {
            rig.add(c);
        }
        rig.add(&mut swapped);
        rig.cycle();
        let input = frames(1 << 24, PERIOD);
        // Everything the audio thread will be handed is made out here.
        rig.send(Command::SetGain(db_to_gain(-20.0)));
        rig.send(Command::Grow(Vec::with_capacity(4 * INITIAL_SOURCES)));
        rig.send(Command::Add(late.source.take().unwrap()));
        rig.send(Command::SetRunning(late.id, true));
        rig.send(Command::SetResampler(10, Some(Box::new(make()))));
        rig.send(Command::SetResampler(10, Some(Box::new(make()))));
        rig.send(Command::Remove(3));
        let engine = &mut rig.engine;
        assert_no_alloc(|| {
            for _ in 0..200 {
                for c in clients.iter() {
                    c.ring.write(&input);
                }
                engine.cycle().unwrap();
            }
        });
        assert!(rig.log.frames().iter().any(|f| f[0] != 0));
        // The returned items are freed here, off the audio thread.
        assert_eq!(rig.events().len(), 4);
    }

    #[test]
    fn cycle_does_not_allocate() {
        no_allocation::<I32>(Format::I32);
        no_allocation::<F32>(Format::F32);
        no_allocation::<F64>(Format::F64);
    }

    fn dop_paths_do_not_allocate<P: Processing>(format: Format) {
        const OUTPUT: u32 = 96000;
        let mut rig = Rig::<P>::new(BitPerfect::Auto);
        // The DoP stream runs at the rate the output is about to move to.
        let (mut a, mut b) = (client_at(1, OUTPUT), client_at(2, OUTPUT));
        let mut c = client_at(3, 44100);
        let r = Resampler::new(44100, OUTPUT, PERIOD as u32, Quality::High, format);
        c.source.as_mut().unwrap().resampler = Some(Box::new(r));
        let mut wrong = client_at(4, 176400);
        rig.add(&mut a);
        rig.add(&mut b);
        rig.add(&mut c);
        rig.add(&mut wrong);
        rig.cycle();
        let dop = dop_frames(PERIOD);
        let pcm = frames(1000, PERIOD);
        let prefix: Vec<[i32; 2]> = pcm[..20].iter().chain(&dop[..44]).copied().collect();
        let tail: Vec<[i32; 2]> = dop[..40].iter().chain(&pcm[..24]).copied().collect();
        // Marked at both ends, so each discard of it remembers a cut run and
        // the next scan drops the marked frames that follow.
        let cut: Vec<[i32; 2]> = dop[..4]
            .iter()
            .chain(&pcm[..56])
            .chain(&dop[..4])
            .copied()
            .collect();
        rig.send(Command::Reconfigure {
            rate: OUTPUT,
            period: PERIOD as u32,
        });
        let start = rig.log.frames().len();
        let engine = &mut rig.engine;
        assert_no_alloc(|| {
            // Joins, tails and short periods. Then the stream runs dry for
            // longer than the grace time, 150 periods here, and lets go.
            for round in 0..300 {
                let block = match round % 8 {
                    0 => &prefix[..],
                    1 | 2 => &dop[..],
                    3 => &tail[..],
                    4 => &dop[..10],
                    _ => &pcm[..],
                };
                if round < 99 {
                    a.ring.write(block);
                }
                wrong.ring.write(&dop);
                // Plain PCM first, so that the blocks line up with the discards.
                let block = if (1..99).contains(&round) { &cut } else { &pcm };
                b.ring.write(block);
                c.ring.write(&pcm);
                engine.cycle().unwrap();
            }
        });
        // Show that each path ran.
        let log = rig.log.frames();
        let periods = || log[start..].chunks(PERIOD);
        assert_eq!(rig.log.opens().last(), Some(&(OUTPUT, PERIOD as u32)));
        assert!(periods().any(|p| p == dop), "a whole DoP period");
        let silence = dop_silence(0x05, PERIOD);
        assert!(periods().any(|p| p == silence), "the grace time");
        let released = log.last().unwrap();
        assert!(released[0].abs() >= 1000, "output after the release");
        assert!(matches!(rig.events()[..], [Event::DopStarted(_, _), ..]));
    }

    #[test]
    fn dop_and_reconfiguring_do_not_allocate() {
        dop_paths_do_not_allocate::<I32>(Format::I32);
        dop_paths_do_not_allocate::<F32>(Format::F32);
        dop_paths_do_not_allocate::<F64>(Format::F64);
    }

    #[test]
    fn copying_holding_and_draining_do_not_allocate() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(ODD, PERIOD));
        rig.cycle();
        let mut held = frames(ODD, PERIOD);
        held[PERIOD - 1] = [0x0512_3400; 2];
        let (pcm, tail) = (frames(ODD, PERIOD - 1), dop_frames(10));
        let start = rig.log.frames().len();
        let engine = &mut rig.engine;
        assert_no_alloc(|| {
            a.ring.write(&pcm);
            engine.cycle().unwrap();
            a.ring.write(&held);
            engine.cycle().unwrap();
            a.ring.write(&pcm);
            engine.cycle().unwrap();
            a.ring.write(&tail);
            a.ring.header().draining.store(1, Release);
            engine.cycle().unwrap();
            engine.cycle().unwrap();
        });
        assert_eq!(rig.log.frames()[start], [ODD, -ODD]);
        assert!(rig.counters.held_frames.load(Relaxed) > 0);
        assert_eq!(a.ring.available(), 0);
        assert!(matches!(rig.events()[..], [Event::Drained(1)]));
    }

    #[test]
    fn a_gain_change_ramps_over_10_ms() {
        let mut rig = Rig::<F64>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        const LEVEL: f64 = (1 << 24) as f64;
        let total = 11 * PERIOD;
        a.ring.write(&frames(1 << 24, total));
        // The source is heard before the change, so the change fades.
        rig.cycle();
        rig.send(Command::SetGain(0.5));
        for _ in 0..10 {
            rig.cycle();
        }
        let got = rig.last_periods(10);
        // 10 ms at 48 kHz is 480 frames, which is 7.5 periods.
        for (i, f) in got.iter().enumerate() {
            let want = if i < 480 {
                1.0 - 0.5 * i as f64 / 480.0
            } else {
                0.5
            };
            let error = (f64::from(f[0]) - want * LEVEL).abs();
            assert!(error < 64.0, "frame {i}: {} against {want}", f[0]);
        }
    }

    #[test]
    fn a_gain_change_before_a_stream_is_heard_applies_at_once() {
        let mut rig = Rig::<F64>::new(BitPerfect::Auto);
        let mut a = client(1);
        // The stream starts in the same cycle as the change.
        rig.add(&mut a);
        a.ring.write(&frames(1 << 24, PERIOD));
        rig.send(Command::SetGain(0.5));
        rig.cycle();
        let got = rig.last_periods(1);
        for (i, f) in got.iter().enumerate() {
            let error = (f64::from(f[0]) - 0.5 * (1 << 24) as f64).abs();
            assert!(error < 64.0, "frame {i}: {}", f[0]);
        }
    }

    #[test]
    fn a_new_gain_during_a_fade_starts_from_where_the_fade_is() {
        let mut rig = Rig::<F64>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(1 << 24, 4 * PERIOD));
        rig.send(Command::SetGain(0.0));
        rig.cycle();
        rig.cycle();
        rig.send(Command::SetGain(1.0));
        rig.cycle();
        rig.cycle();
        let got = rig.last_periods(4);
        for pair in got.windows(2) {
            let step = (i64::from(pair[0][0]) - i64::from(pair[1][0])).abs();
            // One frame of a full fade moves the level by 1/480.
            assert!(step < (1 << 24) / 400, "step of {step}");
        }
    }

    #[test]
    fn a_fade_back_to_0db_ends_bit_perfect() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(ODD, 30 * PERIOD));
        rig.send(Command::SetGain(0.5));
        for _ in 0..10 {
            rig.cycle();
        }
        rig.send(Command::SetGain(1.0));
        for _ in 0..10 {
            rig.cycle();
        }
        assert_ne!(rig.last_periods(11)[0], [ODD, -ODD]);
        assert_eq!(rig.last_periods(1), frames(ODD, PERIOD));
    }

    #[test]
    fn forced_bit_perfect_ignores_volumes() {
        let mut rig = Rig::<F32>::new(BitPerfect::Forced);
        let mut a = client(1);
        a.source.as_mut().unwrap().gain = 0.25;
        rig.add(&mut a);
        a.ring.write(&frames(ODD, 2 * PERIOD));
        rig.send(Command::SetGain(0.1));
        rig.cycle();
        rig.cycle();
        assert_eq!(rig.last_periods(2), frames(ODD, 2 * PERIOD));
    }

    #[test]
    fn forced_bit_perfect_mixes_a_second_client_at_unity() {
        let mut rig = Rig::<I32>::new(BitPerfect::Forced);
        let (mut a, mut b) = (client(1), client(2));
        a.source.as_mut().unwrap().gain = 0.25;
        rig.add(&mut a);
        rig.add(&mut b);
        a.ring.write(&frames(1000, PERIOD));
        b.ring.write(&frames(234, PERIOD));
        rig.send(Command::SetGain(0.1));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(1234, PERIOD));
    }

    /// 1 kHz at `from` through the engine at 48 kHz, as measured at the sink.
    fn measured_frequency(from: u32, format: Format) -> f64 {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client_at(1, from);
        let resampler = Resampler::new(from, RATE, PERIOD as u32, Quality::High, format);
        a.source.as_mut().unwrap().resampler = Some(Box::new(resampler));
        rig.add(&mut a);
        let mut written = 0u64;
        let step = 1000.0 / f64::from(from) * std::f64::consts::TAU;
        for _ in 0..800 {
            let block: Vec<[i32; 2]> = (written..written + PERIOD as u64)
                .map(|i| [((i as f64 * step).sin() * 1e9) as i32; 2])
                .collect();
            written += a.ring.write(&block) as u64;
            rig.cycle();
        }
        let got = rig.log.frames();
        // Skip the filter's start, then time the rising zero crossings.
        let mut crossings = Vec::new();
        for i in 2048..got.len() - 1 {
            let (x, y) = (f64::from(got[i][0]), f64::from(got[i + 1][0]));
            if x < 0.0 && y >= 0.0 {
                crossings.push(i as f64 + x / (x - y));
            }
        }
        assert!(crossings.len() > 500, "{} crossings", crossings.len());
        let span = crossings[crossings.len() - 1] - crossings[0];
        (crossings.len() - 1) as f64 * f64::from(RATE) / span
    }

    #[test]
    fn resampled_output_has_the_expected_frequency() {
        let hz = measured_frequency(44100, Format::F32);
        assert!((hz - 1000.0).abs() < 0.1, "{hz} Hz");
    }

    #[test]
    fn a_resampled_source_short_of_input_counts_an_underrun() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = resampled(1, Format::F32);
        rig.add(&mut a);
        a.ring.write(&frames(1000, 200));
        rig.cycle();
        rig.cycle();
        assert_eq!(underruns(&a), 0);
        a.ring.consume(a.ring.available());
        a.ring.write(&frames(1000, 20));
        rig.cycle();
        assert_eq!(underruns(&a), 1);
        assert_eq!(a.ring.available(), 20);
    }

    #[test]
    fn a_resampled_source_is_never_bit_perfect() {
        let mut rig = Rig::<F32>::new(BitPerfect::Forced);
        let mut a = client(1);
        let resampler = Resampler::new(RATE, RATE, PERIOD as u32, Quality::High, Format::F32);
        a.source.as_mut().unwrap().resampler = Some(Box::new(resampler));
        rig.add(&mut a);
        a.ring.write(&frames(ODD, 4 * PERIOD));
        // The filter's delay is under two periods, so the third is fully audio.
        for _ in 0..3 {
            rig.cycle();
        }
        assert_ne!(rig.last_periods(1), frames(ODD, PERIOD));
    }

    #[test]
    fn first_dop_period_reaches_the_sink_unaltered() {
        fn check<P: Processing>() {
            let mut rig = Rig::<P>::new(BitPerfect::Auto);
            let mut pcm = client(1);
            rig.add(&mut pcm);
            pcm.ring.write(&frames(1000, 3 * PERIOD));
            rig.send(Command::SetGain(db_to_gain(-20.0)));
            // One period in, so a fade is under way when the DoP stream arrives.
            rig.cycle();
            let mut dop = client(2);
            rig.add(&mut dop);
            dop.ring.write(&dop_frames(PERIOD));
            rig.cycle();
            assert_eq!(rig.last_periods(1), dop_frames(PERIOD));
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn dop_ignores_a_source_gain_and_bit_perfect_off() {
        let mut rig = Rig::<F32>::new(BitPerfect::Off);
        let mut a = client(1);
        a.source.as_mut().unwrap().gain = 0.1;
        rig.add(&mut a);
        a.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop_frames(PERIOD));
    }

    #[test]
    fn a_short_dop_period_is_not_altered() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        rig.send(Command::SetGain(0.1));
        let input = dop_frames(PERIOD + 10);
        a.ring.write(&input);
        rig.cycle();
        rig.cycle();
        // Frame 73 is marked 0xFA, so the padding starts on 0x05.
        let mut want = input[PERIOD..].to_vec();
        want.extend(dop_silence(0x05, PERIOD - 10));
        assert_eq!(rig.last_periods(1), want);
        assert_eq!(underruns(&a), 1);
        assert_eq!(
            count(&rig.events(), |e| matches!(e, Event::DopStopped(_))),
            0
        );
    }

    #[test]
    fn a_stream_starting_with_too_few_frames_to_judge_waits() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        rig.send(Command::SetGain(0.1));
        let input = dop_frames(PERIOD);
        a.ring.write(&input[..10]);
        rig.cycle();
        assert_eq!(a.ring.available(), 10);
        assert_eq!(rig.last_periods(1), frames(0, PERIOD));
        a.ring.write(&input[10..]);
        rig.cycle();
        assert_eq!(rig.last_periods(1), input);
    }

    #[test]
    fn dop_silences_other_sources_and_keeps_consuming_them() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let (mut pcm, mut dop) = (client(1), client(2));
        let mut resampled = client_at(3, 44100);
        let r = Resampler::new(44100, RATE, PERIOD as u32, Quality::High, Format::F32);
        resampled.source.as_mut().unwrap().resampler = Some(Box::new(r));
        rig.add(&mut pcm);
        rig.add(&mut dop);
        rig.add(&mut resampled);
        pcm.ring.write(&frames(1000, 2 * PERIOD));
        resampled.ring.write(&frames(1000, 1000));
        dop.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop_frames(PERIOD));
        assert_eq!(pcm.ring.available(), PERIOD);
        assert!(resampled.ring.available() < 1000);
        let events = rig.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Event::DopStarted(2, _)));
        // The stream turns back into PCM, and mixing resumes.
        dop.ring.write(&frames(500, PERIOD));
        rig.cycle();
        // The resampled source starts again, so allow for its filter ringing.
        assert!(rig.last_periods(1).iter().all(|f| f[0] >= 1400));
        let events = rig.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Event::DopStopped(2)));
    }

    #[test]
    fn the_oldest_dop_claim_holds_the_output() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        // The later claim comes first in the table.
        let (mut later, mut first) = (client(1), client(2));
        rig.add(&mut later);
        rig.add(&mut first);
        let other: Vec<[i32; 2]> = dop_frames(PERIOD)
            .iter()
            .map(|f| [f[0] ^ 0x00AB_0000, f[1] ^ 0x00AB_0000])
            .collect();
        first.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::DopStarted(2, _)]));
        first.ring.write(&dop_frames(PERIOD));
        later.ring.write(&other);
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop_frames(PERIOD));
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, _)]));
        // Once the first claim stops, the next one takes over.
        rig.send(Command::SetRunning(2, false));
        later.ring.write(&other);
        rig.cycle();
        assert_eq!(rig.last_periods(1), other);
    }

    #[test]
    fn a_dop_start_sent_late_carries_the_number_of_its_claim() {
        // One slot, so that an unread event keeps the next from being sent.
        let mut rig = Rig::<I32>::with_event_slots(BitPerfect::Auto, 1);
        let (mut later, mut first) = (client(1), client(2));
        rig.add(&mut later);
        rig.add(&mut first);
        rig.send(Command::Flush(99));
        rig.cycle();
        // Both claims are made while the queue is full.
        first.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        first.ring.write(&dop_frames(PERIOD));
        later.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::Flushed(99)]));
        // They are sent in table order, the later claim first.
        let mut claims = Vec::new();
        for _ in 0..2 {
            first.ring.write(&dop_frames(PERIOD));
            later.ring.write(&dop_frames(PERIOD));
            rig.cycle();
            match rig.events()[..] {
                [Event::DopStarted(id, claim)] => claims.push((id, claim)),
                _ => panic!("no DopStarted"),
            }
        }
        assert_eq!(claims[0].0, 1);
        assert_eq!(claims[1].0, 2);
        assert!(claims[1].1 < claims[0].1, "{claims:?}");
    }

    /// A DoP stream that has played, with a PCM client waiting behind it.
    fn dop_with_a_second_client() -> (Rig<I32>, Client, Client) {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let (mut dop, mut pcm) = (client(1), client(2));
        rig.add(&mut dop);
        rig.add(&mut pcm);
        dop.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        rig.events();
        (rig, dop, pcm)
    }

    #[test]
    fn a_dop_underrun_stays_exclusive_within_the_grace_time() {
        let (mut rig, dop, pcm) = dop_with_a_second_client();
        // 100 ms at 48 kHz is 75 periods.
        for i in 0..74 {
            pcm.ring.write(&frames(1000, PERIOD));
            rig.cycle();
            assert_eq!(rig.last_periods(1), dop_silence(0x05, PERIOD), "cycle {i}");
        }
        assert_eq!(underruns(&dop), 74);
        // The stream comes back before the time is up, and starts it afresh.
        dop.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop_frames(PERIOD));
        pcm.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop_silence(0x05, PERIOD));
        // An underrun is not the end of the stream.
        assert!(rig.events().is_empty());
    }

    #[test]
    fn a_stalled_dop_stream_releases_the_output_after_the_grace_time() {
        let (mut rig, _dop, pcm) = dop_with_a_second_client();
        for _ in 0..75 {
            pcm.ring.write(&frames(1000, PERIOD));
            rig.cycle();
            assert_eq!(rig.last_periods(1), dop_silence(0x05, PERIOD));
        }
        pcm.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
    }

    #[test]
    fn a_stalled_dop_stream_is_reported_stopped_when_it_releases_the_output() {
        let (mut rig, dop, pcm) = dop_with_a_second_client();
        for _ in 0..75 {
            pcm.ring.write(&frames(1000, PERIOD));
            rig.cycle();
        }
        assert!(rig.events().is_empty());
        // The grace time has passed: the rate rules must hear of it, or the
        // output stays at the rate of a stream that plays nothing.
        pcm.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
        assert!(matches!(rig.events()[..], [Event::DopStopped(1)]));
        for _ in 0..10 {
            pcm.ring.write(&frames(1000, PERIOD));
            rig.cycle();
            assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
        }
        assert!(rig.events().is_empty());
        // When it plays again it is a new claim, and takes the output back.
        dop.ring.write(&dop_frames(PERIOD));
        pcm.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop_frames(PERIOD));
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, 2)]));
    }

    #[test]
    fn a_dop_stream_that_stalled_ranks_behind_one_that_started_meanwhile() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let (mut stalls, mut other) = (client(1), client(2));
        rig.add(&mut stalls);
        rig.add(&mut other);
        stalls.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        for _ in 0..76 {
            rig.cycle();
        }
        let theirs: Vec<[i32; 2]> = dop_frames(PERIOD)
            .iter()
            .map(|f| [f[0] ^ 0x00AB_0000, f[1] ^ 0x00AB_0000])
            .collect();
        other.ring.write(&theirs);
        rig.cycle();
        assert!(matches!(
            rig.events()[..],
            [
                Event::DopStarted(1, 1),
                Event::DopStopped(1),
                Event::DopStarted(2, 2)
            ]
        ));
        // Both have frames now. The one that kept playing holds the output.
        stalls.ring.write(&dop_frames(PERIOD));
        other.ring.write(&theirs);
        rig.cycle();
        assert_eq!(rig.last_periods(1), theirs);
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, 3)]));
    }

    #[test]
    fn a_stalled_dop_stream_resuming_at_another_output_rate_claims_once() {
        let (mut rig, dop, _pcm) = dop_with_a_second_client();
        for _ in 0..76 {
            rig.cycle();
        }
        assert!(matches!(rig.events()[..], [Event::DopStopped(1)]));
        // The rate rules have moved the output on meanwhile.
        rig.send(Command::Reconfigure {
            rate: 2 * RATE,
            period: PERIOD as u32,
        });
        rig.cycle();
        rig.cycle();
        // The grace time is longer in frames at this rate. The stream is
        // no less stalled for that.
        assert!(matches!(rig.events()[..], [Event::Reconfigured]));
        // It plays again, in pieces with cycles between that find nothing.
        // That is one claim, until the output is at its rate again.
        for _ in 0..5 {
            dop.ring.write(&dop_frames(PERIOD / 2));
            rig.cycle();
            rig.cycle();
        }
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, 2)]));
    }

    #[test]
    fn a_muted_dop_stream_that_runs_dry_keeps_its_claim() {
        // At another rate than the output it never holds the output, so no
        // grace time runs for it, and a cycle without frames ends nothing.
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client_at(1, 2 * RATE);
        rig.add(&mut a);
        a.ring.write(&dop_frames(2 * PERIOD));
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, 1)]));
        for _ in 0..100 {
            rig.cycle();
        }
        a.ring.write(&dop_frames(2 * PERIOD));
        rig.cycle();
        rig.cycle();
        assert!(rig.events().is_empty());
    }

    #[test]
    fn a_stopped_dop_stream_releases_the_output_at_once() {
        let (mut rig, _dop, pcm) = dop_with_a_second_client();
        pcm.ring.write(&frames(1000, 2 * PERIOD));
        rig.send(Command::SetRunning(1, false));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
        let (mut rig, _dop, pcm) = dop_with_a_second_client();
        pcm.ring.write(&frames(1000, PERIOD));
        rig.send(Command::Remove(1));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
    }

    #[test]
    fn a_drained_dop_stream_releases_the_output_at_once() {
        let (mut rig, dop, pcm) = dop_with_a_second_client();
        dop.ring.header().draining.store(1, Release);
        pcm.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
    }

    #[test]
    fn dop_after_a_pcm_prefix_is_not_altered() {
        let mut rig = rig_at_gain::<F32>(db_to_gain(-20.0));
        let mut a = client(1);
        rig.add(&mut a);
        let dop = dop_frames(2 * PERIOD);
        let mut block = frames(0, 20);
        block.extend_from_slice(&dop);
        a.ring.write(&block);
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(0, PERIOD));
        assert_eq!(underruns(&a), 0);
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop[..PERIOD]);
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop[PERIOD..]);
    }

    #[test]
    fn dop_that_starts_at_the_end_of_a_period_is_not_altered() {
        let mut rig = rig_at_gain::<F32>(db_to_gain(-20.0));
        let mut a = client(1);
        rig.add(&mut a);
        let dop = dop_frames(PERIOD);
        let mut block = frames(0, PERIOD - 4);
        block.extend_from_slice(&dop);
        a.ring.write(&block);
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(0, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop);
    }

    #[test]
    fn a_dop_tail_before_padding_plays_raw() {
        let mut rig = rig_at_gain::<F32>(db_to_gain(-20.0));
        let mut a = client(1);
        rig.add(&mut a);
        let dop = dop_frames(PERIOD + 40);
        let mut block = dop.clone();
        block.extend(frames(ODD, PERIOD - 40));
        a.ring.write(&block);
        rig.cycle();
        rig.cycle();
        let mut want = dop[PERIOD..].to_vec();
        want.extend(dop_silence(0x05, PERIOD - 40));
        assert_eq!(rig.last_periods(1), want);
        assert_eq!(underruns(&a), 0);
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, _)]));
        // What follows the markers is PCM again.
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::DopStopped(1)]));
        assert_ne!(rig.last_periods(1)[0], [0, 0]);
        assert_ne!(rig.last_periods(1)[0], [ODD, -ODD]);
    }

    #[test]
    fn a_marker_slip_inside_dop_changes_nothing() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut b);
        // Two tracks joined: the second restarts on the same marker.
        let mut block = dop_frames(PERIOD + 31);
        block.extend(dop_frames(PERIOD - 31));
        b.ring.write(&frames(1000, 4 * PERIOD));
        a.ring.write(&block);
        rig.cycle();
        rig.cycle();
        assert_eq!(rig.last_periods(2), block);
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, _)]));
    }

    #[test]
    fn a_few_dop_frames_in_a_pcm_stream_wait_to_be_judged() {
        let mut rig = rig_at_gain::<F32>(0.1);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(0, PERIOD));
        rig.cycle();
        let dop = dop_frames(PERIOD);
        a.ring.write(&dop[..10]);
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(0, PERIOD));
        assert_eq!(a.ring.available(), 10);
        a.ring.write(&dop[10..]);
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop);
    }

    #[test]
    fn a_marker_like_sample_in_pcm_with_more_to_come_is_not_held() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        let mut block = frames(1 << 24, 2 * PERIOD);
        block[PERIOD - 1] = [0x0512_3400; 2];
        a.ring.write(&block);
        rig.cycle();
        assert_eq!(rig.last_periods(1), block[..PERIOD]);
    }

    #[test]
    fn dop_never_enters_a_resampler() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = resampled(1, Format::F32);
        rig.add(&mut a);
        let mut block = frames(0, 300);
        block.extend(dop_frames(1000));
        a.ring.write(&block);
        for _ in 0..12 {
            rig.cycle();
        }
        assert!(rig.log.frames().iter().all(|f| *f == [0, 0]));
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, _)]));
    }

    #[test]
    fn stopping_or_removing_a_dop_stream_reports_it() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&dop_frames(2 * PERIOD));
        rig.cycle();
        rig.send(Command::SetRunning(1, false));
        rig.cycle();
        let events = rig.events();
        assert!(matches!(
            events[..],
            [Event::DopStarted(1, _), Event::DopStopped(1)]
        ));
        rig.send(Command::SetRunning(1, true));
        rig.cycle();
        rig.send(Command::Remove(1));
        rig.cycle();
        let events = rig.events();
        assert!(matches!(
            events[..],
            [
                Event::DopStarted(1, _),
                Event::DopStopped(1),
                Event::Dropped(_)
            ]
        ));
    }

    #[test]
    fn dop_at_the_wrong_rate_is_muted_until_reconfigured() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let (mut dop, mut pcm) = (client_at(1, 176400), client(2));
        rig.add(&mut dop);
        rig.add(&mut pcm);
        dop.ring.write(&dop_frames(300));
        pcm.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        // Muted, but discarded in real time. Other clients keep playing.
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
        assert_eq!(dop.ring.available(), 300 - 235);
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, _)]));
        rig.send(Command::Reconfigure {
            rate: 176400,
            period: PERIOD as u32,
        });
        rig.cycle();
        assert!(rig.last_periods(1).iter().all(|f| f[0] >> 24 != 0x05));
        dop.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop_frames(PERIOD));
    }

    #[test]
    fn table_growth_keeps_every_source() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut clients: Vec<Client> = (0..INITIAL_SOURCES as u64).map(client).collect();
        for c in clients.iter_mut() {
            rig.add(c);
        }
        rig.cycle();
        rig.send(Command::Grow(Vec::with_capacity(2 * INITIAL_SOURCES)));
        let mut extra = client(100);
        rig.add(&mut extra);
        clients.push(extra);
        for c in clients.iter() {
            c.ring.write(&frames(10, PERIOD));
        }
        rig.cycle();
        let events = rig.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::OldTable(t) if t.is_empty()));
        let want = 10 * clients.len() as i32;
        assert_eq!(rig.last_periods(1), frames(want, PERIOD));
    }

    #[test]
    fn a_table_too_small_to_grow_into_comes_back_unused() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut b);
        rig.send(Command::Grow(Vec::with_capacity(1)));
        a.ring.write(&frames(10, PERIOD));
        b.ring.write(&frames(10, PERIOD));
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::OldTable(_)]));
        assert_eq!(rig.last_periods(1), frames(20, PERIOD));
    }

    #[test]
    fn a_gap_longer_than_the_buffer_is_counted_with_no_xrun() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        rig.cycle();
        // The buffer is two periods, 2.67 ms.
        std::thread::sleep(Duration::from_millis(20));
        rig.cycle();
        assert!(rig.counters.wake_gaps.load(Relaxed) >= 1);
        assert_eq!(rig.counters.xruns.load(Relaxed), 0);
        assert!(rig.counters.worst_gap_ns.load(Relaxed) >= 20_000_000);
    }

    #[test]
    fn the_counters_follow_the_cycles() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        rig.cycle();
        rig.cycle();
        assert!(rig.counters.cycles.load(Relaxed) >= 3);
        assert!(rig.counters.worst_cycle_ns.load(Relaxed) > 0);
    }

    #[test]
    fn an_xrun_is_counted_and_returned_and_the_cycle_finishes() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(1, PERIOD));
        rig.log.fail(Some(SinkError::Xrun));
        assert_eq!(rig.engine.cycle(), Err(SinkError::Xrun));
        assert_eq!(rig.counters.xruns.load(Relaxed), 1);
        assert_eq!(a.ring.available(), 0);
        rig.log.fail(None);
        rig.cycle();
    }

    #[test]
    fn a_gone_sink_is_returned_and_is_not_an_xrun() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        rig.log.fail(Some(SinkError::Gone));
        assert_eq!(rig.engine.cycle(), Err(SinkError::Gone));
        assert_eq!(rig.counters.xruns.load(Relaxed), 0);
    }

    #[test]
    fn every_cycle_publishes_the_delay_and_wakes_the_client() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        rig.log.set_delay(128);
        rig.cycle();
        rig.cycle();
        assert_eq!(a.ring.header().output_delay_frames.load(Relaxed), 128);
        let mut wakes = 0u64;
        let n = unsafe {
            libc::read(
                a.wake.as_raw_fd(),
                (&raw mut wakes).cast(),
                size_of::<u64>(),
            )
        };
        assert_eq!(n, 8);
        assert_eq!(wakes, 2);
    }

    #[test]
    fn the_delay_is_published_at_the_clients_rate_and_includes_the_resampler() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client_at(1, 24000);
        let r = Resampler::new(24000, RATE, PERIOD as u32, Quality::High, Format::F32);
        let filter = r.delay_frames();
        a.source.as_mut().unwrap().resampler = Some(Box::new(r));
        rig.add(&mut a);
        rig.log.set_delay(128);
        rig.cycle();
        let got = a.ring.header().output_delay_frames.load(Relaxed);
        assert_eq!(got, (128 + filter) / 2);
    }

    #[test]
    fn a_full_wake_counter_does_not_stop_the_cycle() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        let most = u64::MAX - 1;
        let n = unsafe { libc::write(a.wake.as_raw_fd(), (&raw const most).cast(), 8) };
        assert_eq!(n, 8);
        rig.cycle();
    }

    #[test]
    fn a_draining_source_is_padded_without_an_underrun_and_reports_once() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(ODD, 100));
        a.ring.header().draining.store(1, Release);
        rig.cycle();
        assert!(rig.events().is_empty());
        rig.cycle();
        let mut want = frames(ODD, 100);
        want.extend(frames(0, 2 * PERIOD - 100));
        assert_eq!(rig.last_periods(2), want);
        assert_eq!(underruns(&a), 0);
        assert!(matches!(rig.events()[..], [Event::Drained(1)]));
        rig.cycle();
        rig.cycle();
        assert!(rig.events().is_empty());
        assert_eq!(underruns(&a), 0);
    }

    #[test]
    fn a_new_drain_reports_again() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.header().draining.store(1, Release);
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::Drained(1)]));
        a.ring.header().draining.store(0, Release);
        rig.cycle();
        a.ring.header().draining.store(1, Release);
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::Drained(1)]));
    }

    #[test]
    fn a_restarted_source_reports_a_new_drain() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.header().draining.store(1, Release);
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::Drained(1)]));
        // No cycle sees the flag clear before the next drain, of nothing.
        rig.send(Command::SetRunning(1, false));
        rig.send(Command::SetRunning(1, true));
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::Drained(1)]));
    }

    #[test]
    fn a_draining_stream_of_a_few_frames_still_plays() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(ODD, 5));
        a.ring.header().draining.store(1, Release);
        rig.cycle();
        let mut want = frames(ODD, 5);
        want.extend(frames(0, PERIOD - 5));
        assert_eq!(rig.last_periods(1), want);
    }

    #[test]
    fn a_draining_resampled_source_reports_once_the_filter_has_emptied() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client_at(1, 44100);
        let r = Resampler::new(44100, RATE, PERIOD as u32, Quality::High, Format::F32);
        a.source.as_mut().unwrap().resampler = Some(Box::new(r));
        rig.add(&mut a);
        a.ring.write(&frames(1 << 28, 200));
        a.ring.header().draining.store(1, Release);
        let mut cycles = 0;
        while !matches!(rig.events()[..], [Event::Drained(1)]) {
            assert_eq!(a.ring.header().underruns.load(Relaxed), 0);
            rig.cycle();
            cycles += 1;
            assert!(cycles < 50);
        }
        let got = rig.log.frames();
        let loud = got.iter().filter(|f| f[0] > 1 << 27).count();
        assert!((215..=220).contains(&loud), "{loud}");
    }

    #[test]
    fn a_discarded_draining_source_still_reports() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let (mut dop, mut other) = (client(1), client_at(2, 44100));
        let r = Resampler::new(44100, RATE, PERIOD as u32, Quality::High, Format::F32);
        other.source.as_mut().unwrap().resampler = Some(Box::new(r));
        rig.add(&mut dop);
        rig.add(&mut other);
        dop.ring.write(&dop_frames(4 * PERIOD));
        other.ring.write(&frames(1000, 10));
        other.ring.header().draining.store(1, Release);
        rig.cycle();
        rig.cycle();
        let events = rig.events();
        assert_eq!(count(&events, |e| matches!(e, Event::Drained(2))), 1);
    }

    #[test]
    fn a_full_event_queue_loses_nothing() {
        let mut rig = Rig::<F32>::with_event_slots(BitPerfect::Auto, 1);
        let mut clients: Vec<Client> = (0..INITIAL_SOURCES as u64).map(client).collect();
        for c in clients.iter_mut() {
            rig.add(c);
        }
        rig.cycle();
        for id in 0..INITIAL_SOURCES as u64 {
            rig.send(Command::Remove(id));
        }
        let mut back = Vec::new();
        for _ in 0..4 * INITIAL_SOURCES {
            rig.cycle();
            if let Ok(Event::Dropped(s)) = rig.events.pop() {
                back.push(s.id);
            }
        }
        assert_eq!(back, (0..INITIAL_SOURCES as u64).collect::<Vec<_>>());
    }

    #[test]
    fn a_full_event_queue_delays_cycle_events_without_losing_them() {
        let mut rig = Rig::<F32>::with_event_slots(BitPerfect::Auto, 1);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut b);
        a.ring.write(&dop_frames(PERIOD));
        b.ring.header().draining.store(1, Release);
        rig.cycle();
        rig.cycle();
        assert!(matches!(rig.events.pop(), Ok(Event::DopStarted(1, _))));
        rig.cycle();
        assert!(matches!(rig.events.pop(), Ok(Event::Drained(2))));
        rig.cycle();
        assert!(rig.events.pop().is_err());
    }

    #[test]
    fn reconfigure_fades_one_period_then_reopens_the_sink() {
        let mut rig = Rig::<F64>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(1 << 24, PERIOD));
        rig.cycle();
        a.ring.write(&frames(1 << 24, 4 * PERIOD));
        // The new client and its resampler must not take part in the fade.
        let mut b = client(2);
        rig.add(&mut b);
        b.ring.write(&frames(1 << 24, PERIOD));
        rig.send(Command::Reconfigure {
            rate: 96000,
            period: 128,
        });
        rig.cycle();
        let got = rig.last_periods(1);
        assert_eq!(got[0][0], 1 << 24);
        for (i, f) in got.iter().enumerate() {
            let want = (1.0 - i as f64 / PERIOD as f64) * (1 << 24) as f64;
            assert!((f64::from(f[0]) - want).abs() < 2.0, "frame {i}");
        }
        assert_eq!(b.ring.available(), PERIOD);
        assert_eq!(rig.log.opens().len(), 1);
        assert!(rig.events().is_empty());
        // The next cycle reopens and plays at the new size, at full level.
        let before = rig.log.frames().len();
        rig.cycle();
        assert_eq!(rig.log.opens()[1..], [(96000, 128)]);
        assert_eq!(rig.log.closes(), 1);
        assert!(matches!(rig.events()[..], [Event::Reconfigured]));
        let got = rig.log.frames();
        assert_eq!(got.len(), before + 128);
        assert_eq!(got[before], [(1 << 24) * 2, -(1 << 24) * 2]);
    }

    #[test]
    fn a_dop_stream_is_not_faded_by_a_reconfigure() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&dop_frames(2 * PERIOD));
        rig.cycle();
        rig.send(Command::Reconfigure {
            rate: RATE,
            period: PERIOD as u32,
        });
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop_frames(PERIOD));
    }

    #[test]
    fn a_failed_reopen_is_returned_and_retried() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        rig.send(Command::Reconfigure {
            rate: 96000,
            period: 128,
        });
        rig.cycle();
        rig.log.fail(Some(SinkError::Gone));
        assert_eq!(rig.engine.cycle(), Err(SinkError::Gone));
        assert!(rig.engine.is_closed());
        assert!(rig.events().is_empty());
        rig.log.fail(None);
        rig.cycle();
        assert!(!rig.engine.is_closed());
        assert!(matches!(rig.events()[..], [Event::Reconfigured]));
    }

    #[test]
    fn a_failed_reopen_still_reports_what_the_sources_did() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        rig.send(Command::Reconfigure {
            rate: 96000,
            period: 128,
        });
        rig.cycle();
        rig.events();
        // The stream ends while the card cannot be opened.
        rig.log.fail(Some(SinkError::Gone));
        a.ring.header().draining.store(1, Release);
        assert_eq!(rig.engine.cycle(), Err(SinkError::Gone));
        assert!(matches!(rig.events()[..], [Event::Drained(1)]));
    }

    /// A sink that moves to another device inside one `write`, which takes
    /// far longer than its buffer lasts, as opening a card does.
    struct Switching {
        took: Duration,
        switch_next: bool,
        reports: bool,
        switched: bool,
    }

    impl Sink for Switching {
        fn open(&mut self, _rate: u32, _period: u32) -> Result<(), SinkError> {
            Ok(())
        }

        fn write(&mut self, _frames: &[[i32; 2]]) -> Result<(), SinkError> {
            if std::mem::take(&mut self.switch_next) {
                std::thread::sleep(self.took);
                self.switched = self.reports;
            }
            Ok(())
        }

        fn close(&mut self) {}

        fn delay_frames(&self) -> u32 {
            0
        }

        fn supported_rates(&self) -> Vec<u32> {
            Vec::new()
        }

        fn switched(&mut self) -> bool {
            std::mem::take(&mut self.switched)
        }
    }

    /// Wake-up gaps counted over a switch of the device in the third cycle.
    fn gaps_over_a_switch(reports: bool) -> u64 {
        // A buffer of 200 ms, so that only the switch is longer.
        let (rate, period) = (48000, 4800);
        let sink = Switching {
            took: Duration::from_millis(300),
            switch_next: false,
            reports,
            switched: false,
        };
        let (mut commands, consumer) = RingBuffer::new(16);
        let (producer, _events) = RingBuffer::new(16);
        let counters = Arc::new(Counters::default());
        let mut engine =
            Engine::<F32, _>::new(sink, consumer, producer, counters.clone(), BitPerfect::Auto);
        assert!(commands.push(Command::Reconfigure { rate, period }).is_ok());
        for cycle in 0..5 {
            engine.sink.switch_next = cycle == 2;
            engine.cycle().unwrap();
        }
        counters.wake_gaps.load(Relaxed)
    }

    #[test]
    fn the_time_a_sink_takes_to_switch_devices_is_not_a_wake_up_gap() {
        assert_eq!(gaps_over_a_switch(true), 0);
        // The same wait inside a write that switches nothing is one.
        assert_eq!(gaps_over_a_switch(false), 1);
    }

    #[test]
    fn every_reconfigure_is_answered_even_if_it_never_opens_the_sink() {
        let reconfigure = |rate| Command::Reconfigure { rate, period: 64 };
        // Two in one cycle: the first is overtaken, the second opens.
        let mut rig = Rig::<F32>::closed(BitPerfect::Auto, 16);
        rig.send(reconfigure(44100));
        rig.send(reconfigure(96000));
        rig.cycle();
        assert_eq!(rig.log.opens(), [(96000, 64)]);
        assert!(matches!(
            rig.events()[..],
            [Event::Reconfigured, Event::Reconfigured]
        ));
        // One taken back by a close before the sink could be opened.
        rig.send(reconfigure(48000));
        rig.cycle();
        rig.log.fail(Some(SinkError::Gone));
        assert_eq!(rig.engine.cycle(), Err(SinkError::Gone));
        assert!(rig.events().is_empty());
        rig.send(Command::CloseOutput);
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::Reconfigured]));
        // A close with nothing waiting says nothing.
        rig.send(Command::CloseOutput);
        rig.cycle();
        assert!(rig.events().is_empty());
    }

    #[test]
    fn a_new_engine_stays_closed_until_reconfigured() {
        let mut rig = Rig::<F32>::closed(BitPerfect::Auto, 16);
        assert!(rig.engine.is_closed());
        rig.cycle();
        assert!(rig.log.opens().is_empty());
        assert!(rig.log.frames().is_empty());
    }

    #[test]
    fn a_closed_engine_does_not_touch_the_sink_or_the_rings() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(ODD, PERIOD));
        rig.send(Command::SetRunning(1, false));
        rig.cycle();
        rig.send(Command::SetRunning(1, true));
        rig.send(Command::CloseOutput);
        rig.cycle();
        assert!(rig.engine.is_closed());
        assert_eq!(rig.log.closes(), 1);
        let written = rig.log.frames().len();
        rig.cycle();
        rig.cycle();
        assert_eq!(rig.log.frames().len(), written);
        assert_eq!(rig.log.closes(), 1);
        assert_eq!(a.ring.available(), PERIOD);
        assert_eq!(underruns(&a), 0);
        // Commands still work, and reopening needs no fade.
        rig.send(Command::Reconfigure {
            rate: RATE,
            period: PERIOD as u32,
        });
        rig.cycle();
        assert!(!rig.engine.is_closed());
        assert_eq!(rig.last_periods(1), frames(ODD, PERIOD));
    }

    #[test]
    fn a_gain_set_while_closed_takes_effect_at_once() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        rig.send(Command::CloseOutput);
        rig.cycle();
        rig.send(Command::SetGain(0.5));
        a.ring.write(&frames(1000, PERIOD));
        rig.send(Command::Reconfigure {
            rate: RATE,
            period: PERIOD as u32,
        });
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(500, PERIOD));
    }

    #[test]
    fn shutdown_closes_the_sink_and_ends_the_cycles() {
        let mut rig = Rig::<F32>::closed(BitPerfect::Auto, 16);
        rig.send(Command::Reconfigure {
            rate: RATE,
            period: PERIOD as u32,
        });
        rig.cycle();
        assert!(!rig.engine.is_stopped());
        // A reopen still waiting when the shutdown comes does not happen.
        rig.send(Command::Reconfigure {
            rate: RATE,
            period: PERIOD as u32,
        });
        rig.cycle();
        rig.send(Command::Shutdown);
        rig.cycle();
        rig.send(Command::Reconfigure {
            rate: RATE,
            period: PERIOD as u32,
        });
        rig.cycle();
        assert!(rig.engine.is_stopped());
        assert_eq!(rig.log.opens().len(), 1);
        assert_eq!(rig.log.closes(), 1);
        let written = rig.log.frames().len();
        rig.cycle();
        assert_eq!(rig.log.frames().len(), written);
    }

    #[test]
    fn commands_and_events_can_cross_threads() {
        fn sendable<T: Send>() {}
        sendable::<Command>();
        sendable::<Event>();
        sendable::<Engine<F32, Box<dyn crate::sink::Sink>>>();
    }

    #[test]
    fn a_period_beyond_the_limit_is_clamped() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        rig.send(Command::Reconfigure {
            rate: RATE,
            period: MAX_PERIOD * 2,
        });
        rig.cycle();
        rig.cycle();
        assert_eq!(*rig.log.opens().last().unwrap(), (RATE, MAX_PERIOD));
    }

    /// Puts a period into a client's ring each time the engine waits for
    /// room, as a client woken by the engine refills it, and keeps what it
    /// is given to play.
    struct RefillOnWait {
        ring: Ring,
        next: i32,
        played: Arc<std::sync::Mutex<Vec<[i32; 2]>>>,
    }

    impl crate::sink::Sink for RefillOnWait {
        fn open(&mut self, _rate: u32, _period: u32) -> Result<(), SinkError> {
            Ok(())
        }

        fn wait_room(&mut self) -> Result<(), SinkError> {
            self.next += 1;
            self.ring.write(&frames(self.next, PERIOD));
            Ok(())
        }

        fn write(&mut self, frames: &[[i32; 2]]) -> Result<(), SinkError> {
            self.played.lock().unwrap().extend_from_slice(frames);
            Ok(())
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
    fn audio_written_while_the_engine_waits_for_room_plays_in_that_cycle() {
        let (mut commands, consumer) = RingBuffer::new(16);
        let (producer, _events) = RingBuffer::new(64);
        let counters = Arc::new(Counters::default());
        let mut a = client(1);
        let played = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = RefillOnWait {
            ring: a.ring,
            next: 0,
            played: played.clone(),
        };
        let mode = BitPerfect::Auto;
        let mut engine = Engine::<F32, _>::new(sink, consumer, producer, counters, mode);
        let reconfigure = Command::Reconfigure {
            rate: RATE,
            period: PERIOD as u32,
        };
        for command in [
            reconfigure,
            Command::Add(a.source.take().unwrap()),
            Command::SetRunning(a.id, true),
        ] {
            assert!(commands.push(command).is_ok());
        }
        // The cycle that opens the sink does not wait; four more do.
        for _ in 0..5 {
            engine.cycle().unwrap();
        }
        // Each period read from the ring is the one put there while the
        // engine waited in that same cycle, not the one before.
        let played = played.lock().unwrap();
        assert_eq!(played[played.len() - PERIOD..], frames(4, PERIOD)[..]);
    }

    /// A sink that sends a stop for a client while the engine waits for
    /// room, as a player's pause arriving then does.
    struct StopOnWait {
        commands: Option<rtrb::Producer<Command>>,
        id: ClientId,
        /// The call to `wait_room` that sends the stop.
        at: usize,
        waits: usize,
        played: Arc<std::sync::Mutex<Vec<[i32; 2]>>>,
        /// Frames played when the stop was sent.
        before: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::sink::Sink for StopOnWait {
        fn open(&mut self, _rate: u32, _period: u32) -> Result<(), SinkError> {
            Ok(())
        }

        fn wait_room(&mut self) -> Result<(), SinkError> {
            self.waits += 1;
            if self.waits == self.at {
                let commands = self.commands.as_mut().unwrap();
                assert!(commands.push(Command::SetRunning(self.id, false)).is_ok());
                let played = self.played.lock().unwrap().len();
                self.before.store(played, Ordering::Relaxed);
            }
            Ok(())
        }

        fn write(&mut self, frames: &[[i32; 2]]) -> Result<(), SinkError> {
            self.played.lock().unwrap().extend_from_slice(frames);
            Ok(())
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
    fn a_stop_that_comes_while_the_engine_waits_for_room_plays_no_more() {
        let (mut commands, consumer) = RingBuffer::new(16);
        let (producer, _events) = RingBuffer::new(64);
        let counters = Arc::new(Counters::default());
        let mut a = client(1);
        a.ring.write(&frames(1, 8 * PERIOD));
        let played = Arc::new(std::sync::Mutex::new(Vec::new()));
        let before = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reconfigure = Command::Reconfigure {
            rate: RATE,
            period: PERIOD as u32,
        };
        for command in [
            reconfigure,
            Command::Add(a.source.take().unwrap()),
            Command::SetRunning(a.id, true),
        ] {
            assert!(commands.push(command).is_ok());
        }
        let sink = StopOnWait {
            commands: Some(commands),
            id: a.id,
            at: 3,
            waits: 0,
            played: played.clone(),
            before: before.clone(),
        };
        let mode = BitPerfect::Auto;
        let mut engine = Engine::<F32, _>::new(sink, consumer, producer, counters, mode);
        for _ in 0..5 {
            engine.cycle().unwrap();
        }
        // The cycle that was waiting when the stop came takes nothing
        // more from the ring, and neither does any after it.
        let before = before.load(Ordering::Relaxed);
        assert!(before >= PERIOD, "the stop came before anything played");
        assert_eq!(a.ring.available(), 8 * PERIOD - before);
        let played = played.lock().unwrap();
        assert!(played[before..].iter().all(|f| *f == [0, 0]));
    }

    struct SlowOpen;

    impl crate::sink::Sink for SlowOpen {
        fn open(&mut self, _rate: u32, _period: u32) -> Result<(), SinkError> {
            std::thread::sleep(Duration::from_millis(50));
            Ok(())
        }

        fn write(&mut self, _frames: &[[i32; 2]]) -> Result<(), SinkError> {
            Ok(())
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
    fn opening_the_card_is_not_a_gap_or_cycle_time() {
        let (mut commands, consumer) = RingBuffer::new(16);
        let (producer, _events) = RingBuffer::new(16);
        let counters = Arc::new(Counters::default());
        let mode = BitPerfect::Auto;
        let mut engine =
            Engine::<F32, SlowOpen>::new(SlowOpen, consumer, producer, counters.clone(), mode);
        // The buffer lasts 42 ms: shorter than the open, and far longer than
        // a busy machine can keep a test from running.
        let reconfigure = Command::Reconfigure {
            rate: RATE,
            period: 1024,
        };
        assert!(commands.push(reconfigure).is_ok());
        for _ in 0..3 {
            engine.cycle().unwrap();
        }
        assert_eq!(counters.wake_gaps.load(Relaxed), 0);
        assert!(counters.worst_cycle_ns.load(Relaxed) < 40_000_000);
    }

    #[test]
    fn a_second_short_drain_finishes() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = resampled(1, Format::F32);
        rig.add(&mut a);
        a.ring.write(&frames(1000, 200));
        a.ring.header().draining.store(1, Release);
        for _ in 0..20 {
            rig.cycle();
        }
        assert_eq!(count(&rig.events(), |e| matches!(e, Event::Drained(1))), 1);
        a.ring.header().draining.store(0, Release);
        rig.cycle();
        rig.events();
        a.ring.write(&frames(1000, 30));
        a.ring.header().draining.store(1, Release);
        for _ in 0..50 {
            rig.cycle();
        }
        assert_eq!(a.ring.available(), 0);
        assert_eq!(count(&rig.events(), |e| matches!(e, Event::Drained(1))), 1);
    }

    #[test]
    fn a_short_drain_after_dop_finishes() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let (mut d, mut a) = (client(2), resampled(1, Format::F32));
        rig.add(&mut d);
        rig.add(&mut a);
        d.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        rig.cycle();
        rig.send(Command::Remove(2));
        rig.cycle();
        rig.events();
        a.ring.write(&frames(1000, 30));
        a.ring.header().draining.store(1, Release);
        for _ in 0..50 {
            rig.cycle();
        }
        assert_eq!(a.ring.available(), 0);
        assert_eq!(count(&rig.events(), |e| matches!(e, Event::Drained(1))), 1);
    }

    #[test]
    fn a_stopped_resampled_source_that_was_never_fed_reports_a_drain() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = resampled(1, Format::F32);
        rig.add(&mut a);
        rig.send(Command::SetRunning(1, false));
        a.ring.header().draining.store(1, Release);
        rig.cycle();
        assert_eq!(count(&rig.events(), |e| matches!(e, Event::Drained(1))), 1);
    }

    /// Frames per cycle taken from `victim` while client 2 holds the output.
    fn discard_rate(mut victim: Client, cycles: usize) -> f64 {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut d = client(2);
        rig.add(&mut d);
        rig.add(&mut victim);
        victim.ring.write(&frames(1000, 60_000));
        let dop = dop_frames(PERIOD);
        d.ring.write(&dop);
        rig.cycle();
        let before = victim.ring.available();
        for _ in 0..cycles {
            d.ring.write(&dop);
            rig.cycle();
        }
        (before - victim.ring.available()) as f64 / cycles as f64
    }

    #[test]
    fn discarding_keeps_real_time_over_a_long_run() {
        let per_cycle = discard_rate(resampled(1, Format::F32), 900);
        assert!((per_cycle - 58.8).abs() < 0.01, "{per_cycle}");
        let per_cycle = discard_rate(client_at(1, 176400), 200);
        assert!((per_cycle - 235.2).abs() < 0.01, "{per_cycle}");
        let per_cycle = discard_rate(client(1), 200);
        assert_eq!(per_cycle, PERIOD as f64);
    }

    #[test]
    fn waiting_for_the_first_frames_is_not_an_underrun() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        rig.cycle();
        rig.cycle();
        assert_eq!(underruns(&a), 0);
        a.ring.write(&dop_frames(10));
        rig.cycle();
        assert_eq!(underruns(&a), 0);
    }

    #[test]
    fn a_small_first_write_is_not_an_underrun() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(ODD, 20));
        rig.cycle();
        assert_eq!(underruns(&a), 0);
        rig.cycle();
        assert_eq!(underruns(&a), 1);
    }

    #[test]
    fn filling_a_new_resampler_is_not_an_underrun() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = resampled(1, Format::F32);
        rig.add(&mut a);
        // Less than the first call asks for, so nothing can be played yet.
        a.ring.write(&frames(1000, 30));
        rig.cycle();
        rig.cycle();
        assert_eq!(underruns(&a), 0);
        assert_eq!(a.ring.available(), 30);
    }

    #[test]
    fn the_reconfigure_fade_also_happens_in_forced_bit_perfect() {
        let mut rig = Rig::<F64>::new(BitPerfect::Forced);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(1 << 24, 2 * PERIOD));
        rig.cycle();
        rig.send(Command::Reconfigure {
            rate: 96000,
            period: PERIOD as u32,
        });
        rig.cycle();
        let got = rig.last_periods(1);
        assert_eq!(got[0][0], 1 << 24);
        assert!(got[PERIOD - 1][0] < (1 << 24) / 32);
    }

    /// A small repeatable random number source.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n
        }
    }

    /// Silence or an untouched frame of `dop_frames` or DoP silence. With
    /// `tones`, PCM is a tone that is the same on both channels, and at a
    /// gain of 0.1 it stays well under the marker band. Scaled DoP differs
    /// between the channels, so it is never taken for a tone.
    fn is_clean_output(f: &[i32; 2], tones: bool) -> bool {
        let (left, right) = (f[0] as u32, f[1] as u32);
        let marked = matches!(left >> 24, 0x05 | 0xFA);
        let raw = left & 0xFF_FFFF == 0x12_3456 && right == left ^ 0xFF00;
        let silent = left & 0xFF_FFFF == 0x69_6900 && left == right;
        let pcm = if tones {
            left == right && f[0].unsigned_abs() < 0x0500_0000
        } else {
            *f == [0, 0]
        };
        (marked && (raw || silent)) || (!marked && pcm)
    }

    /// A mono tone, the same on both channels, at `amp` of full scale.
    fn tone(i: usize, rate: f64, hz: f64, amp: f64) -> [i32; 2] {
        let t = i as f64 / rate;
        let v = (amp * (2.0 * std::f64::consts::PI * hz * t).sin() * 2_147_483_647.0) as i32;
        [v, v]
    }

    /// One cycle each of two tones at 48 kHz. The slow one peaks inside the
    /// marker band, so a third of its frames carry a marker-like top byte.
    /// The fast one crosses the band on its way to a tenth of full scale.
    static TONES: std::sync::LazyLock<[Vec<[i32; 2]>; 2]> = std::sync::LazyLock::new(|| {
        let table = |n: usize, hz: f64, amp: f64| {
            (0..n)
                .map(|i| tone(i, RATE as f64, hz, amp))
                .collect::<Vec<_>>()
        };
        [table(1600, 30.0, 0.045), table(200, 240.0, 0.1)]
    });

    /// What a fuzz run does besides writing to client 1.
    #[derive(Clone, Copy, Default)]
    struct Fuzz {
        resampled_first: bool,
        wrong_rate_first: bool,
        second: bool,
        resampled_second: bool,
        /// Client 2 writes only PCM: silence and tones through the marker
        /// band. It must never be taken for DoP, and never play raw.
        tone_second: bool,
        /// Client 2 comes first in the table.
        b_first: bool,
        /// DoP runs are written in pieces, many of them under 16 frames.
        split_runs: bool,
        stops: bool,
        drains: bool,
        reconfigures: bool,
        final_drain: bool,
    }

    /// What one fuzz client writes.
    struct Writer<'a> {
        client: &'a Client,
        pcm_only: bool,
        split_runs: bool,
        dop_phase: usize,
        /// Frames of the current run still to be written.
        run_left: usize,
        /// Where each tone continues.
        tone_at: [usize; 2],
    }

    impl<'a> Writer<'a> {
        fn new(client: &'a Client, pcm_only: bool, split_runs: bool) -> Self {
            Writer {
                client,
                pcm_only,
                split_runs,
                dop_phase: 0,
                run_left: 0,
                tone_at: [0; 2],
            }
        }

        /// Writes `n` frames of the run, in phase.
        fn write_run(&mut self, n: usize) {
            let phase = self.dop_phase;
            self.client.ring.write(&dop_frames(n + 2)[phase..phase + n]);
            self.dop_phase = (phase + n) % 2;
            self.run_left = self.run_left.saturating_sub(n);
        }

        /// Writes what is left of the run, so that a drain does not cut it
        /// into a run under 16 frames, which is PCM by ruling.
        fn finish_run(&mut self) {
            if self.run_left > 0 {
                self.write_run(self.run_left);
            }
        }

        /// Writes silent PCM or an in-phase DoP run of 16 frames or more, in
        /// a random size. Shorter runs are PCM by ruling, so they are not
        /// written. A split run goes in pieces of up to 40 frames, one per
        /// call. A PCM-only client writes silence or one of the tones.
        fn write_randomly(&mut self, random: &mut Lcg) {
            let n = match random.next(4) {
                0 => random.next(10),
                1 => random.next(70),
                _ => random.next(300),
            };
            if self.run_left > 0 {
                let piece = (random.next(40) + 1).min(self.run_left);
                self.write_run(piece);
            } else if self.pcm_only {
                if random.next(3) == 0 {
                    self.client.ring.write(&frames(0, n));
                } else {
                    let which = random.next(2);
                    let table = &TONES[which];
                    let at = &mut self.tone_at[which];
                    let block: Vec<[i32; 2]> =
                        (0..n).map(|k| table[(*at + k) % table.len()]).collect();
                    *at = (*at + n) % table.len();
                    self.client.ring.write(&block);
                }
            } else if random.next(2) == 0 {
                self.client.ring.write(&frames(0, n));
            } else {
                let n = n.max(dop::MIN_FRAMES);
                if self.split_runs {
                    self.run_left = n;
                    let piece = (random.next(40) + 1).min(n);
                    self.write_run(piece);
                } else {
                    self.write_run(n);
                }
            }
        }
    }

    /// Plays random writes at a gain of 0.1. Any frame scaled or filtered
    /// from DoP shows at the sink, and so does raw PCM from the tone client.
    /// Returns how many frames did, and how many times the tone client was
    /// reported as DoP.
    fn fuzz_run(seed: u64, fuzz: Fuzz) -> (usize, usize) {
        let mut random = Lcg(seed);
        let mut rig = rig_at_gain::<F32>(0.1);
        let start = rig.log.frames().len();
        let mut a = match (fuzz.resampled_first, fuzz.wrong_rate_first) {
            (true, _) => resampled(1, Format::F32),
            (_, true) => client_at(1, 96000),
            _ => client(1),
        };
        let mut b = if fuzz.resampled_second {
            resampled(2, Format::F32)
        } else {
            client(2)
        };
        if fuzz.second && fuzz.b_first {
            rig.add(&mut b);
        }
        rig.add(&mut a);
        if fuzz.second && !fuzz.b_first {
            rig.add(&mut b);
        }
        let mut writer_a = Writer::new(&a, false, fuzz.split_runs);
        let mut writer_b = Writer::new(&b, fuzz.tone_second, fuzz.split_runs);
        let (mut running_a, mut running_b) = (true, true);
        let mut false_starts = 0;
        for _ in 0..200 {
            writer_a.write_randomly(&mut random);
            if fuzz.second {
                writer_b.write_randomly(&mut random);
            }
            for _ in 0..random.next(4) {
                rig.cycle();
            }
            let events = rig.events();
            if fuzz.tone_second {
                false_starts += count(&events, |e| matches!(e, Event::DopStarted(2, _)));
            }
            if fuzz.stops && random.next(4) == 0 {
                if random.next(2) == 0 {
                    running_a = !running_a;
                    rig.send(Command::SetRunning(1, running_a));
                } else if fuzz.second {
                    running_b = !running_b;
                    rig.send(Command::SetRunning(2, running_b));
                }
            }
            if fuzz.drains && random.next(6) == 0 {
                let writer = if random.next(2) == 0 || !fuzz.second {
                    &mut writer_a
                } else {
                    &mut writer_b
                };
                writer.finish_run();
                let client = writer.client;
                client.ring.header().draining.store(1, Release);
                for _ in 0..80 {
                    rig.cycle();
                    if client.ring.available() == 0 && random.next(3) == 0 {
                        break;
                    }
                }
                client.ring.header().draining.store(0, Release);
            }
            if fuzz.reconfigures && random.next(8) == 0 {
                let rate = if random.next(2) == 0 { RATE } else { 96000 };
                let period = [32, 64, 128][random.next(3)];
                rig.send(Command::Reconfigure { rate, period });
            }
        }
        if fuzz.final_drain {
            writer_a.finish_run();
            writer_b.finish_run();
            a.ring.header().draining.store(1, Release);
            b.ring.header().draining.store(1, Release);
        }
        for _ in 0..1000 {
            rig.cycle();
        }
        if fuzz.tone_second {
            false_starts += count(&rig.events(), |e| matches!(e, Event::DopStarted(2, _)));
        }
        let log = rig.log.frames();
        let output = log[start..].iter();
        let altered = output
            .filter(|f| !is_clean_output(f, fuzz.tone_second))
            .count();
        (altered, false_starts)
    }

    fn fuzz_seeds(fuzz: Fuzz) {
        let (mut altered, mut reported) = (Vec::new(), Vec::new());
        for seed in 0..FUZZ_SEEDS {
            let (frames, starts) = fuzz_run(seed, fuzz);
            if frames > 0 {
                altered.push(seed);
            }
            if starts > 0 {
                reported.push(seed);
            }
        }
        assert!(
            altered.is_empty() && reported.is_empty(),
            "{} seeds with altered frames: {altered:?}, {} with a false DopStarted: {reported:?}",
            altered.len(),
            reported.len()
        );
    }

    const FUZZ_SEEDS: u64 = 100;
    const TWO: Fuzz = Fuzz {
        resampled_first: false,
        wrong_rate_first: false,
        second: true,
        resampled_second: false,
        tone_second: false,
        b_first: false,
        split_runs: false,
        stops: false,
        drains: false,
        reconfigures: false,
        final_drain: false,
    };
    const ONE: Fuzz = Fuzz {
        second: false,
        ..TWO
    };

    #[test]
    fn fuzz_one_source() {
        fuzz_seeds(ONE);
    }

    #[test]
    fn fuzz_one_source_with_a_final_drain() {
        fuzz_seeds(Fuzz {
            final_drain: true,
            ..ONE
        });
    }

    #[test]
    fn fuzz_one_resampled_source() {
        fuzz_seeds(Fuzz {
            resampled_first: true,
            ..ONE
        });
    }

    #[test]
    fn fuzz_one_resampled_source_with_a_final_drain() {
        fuzz_seeds(Fuzz {
            resampled_first: true,
            final_drain: true,
            ..ONE
        });
    }

    #[test]
    fn fuzz_one_source_with_drains() {
        fuzz_seeds(Fuzz {
            drains: true,
            ..ONE
        });
    }

    #[test]
    fn fuzz_one_source_with_stops() {
        fuzz_seeds(Fuzz { stops: true, ..ONE });
    }

    #[test]
    fn fuzz_one_source_with_reconfigures() {
        fuzz_seeds(Fuzz {
            reconfigures: true,
            ..ONE
        });
    }

    #[test]
    fn fuzz_a_source_at_the_wrong_rate() {
        fuzz_seeds(Fuzz {
            wrong_rate_first: true,
            ..ONE
        });
    }

    #[test]
    fn fuzz_a_source_at_the_wrong_rate_with_reconfigures_and_drains() {
        fuzz_seeds(Fuzz {
            wrong_rate_first: true,
            reconfigures: true,
            drains: true,
            ..ONE
        });
    }

    #[test]
    fn fuzz_two_sources() {
        fuzz_seeds(TWO);
    }

    #[test]
    fn fuzz_two_sources_one_resampled() {
        fuzz_seeds(Fuzz {
            resampled_second: true,
            ..TWO
        });
    }

    #[test]
    fn fuzz_two_sources_with_drains() {
        fuzz_seeds(Fuzz {
            drains: true,
            ..TWO
        });
    }

    #[test]
    fn fuzz_two_sources_one_resampled_with_drains() {
        fuzz_seeds(Fuzz {
            resampled_second: true,
            drains: true,
            ..TWO
        });
    }

    #[test]
    fn fuzz_two_sources_with_stops() {
        fuzz_seeds(Fuzz { stops: true, ..TWO });
    }

    #[test]
    fn fuzz_two_sources_one_resampled_with_stops() {
        fuzz_seeds(Fuzz {
            resampled_second: true,
            stops: true,
            ..TWO
        });
    }

    #[test]
    fn fuzz_everything_at_once() {
        fuzz_seeds(Fuzz {
            resampled_second: true,
            stops: true,
            drains: true,
            reconfigures: true,
            final_drain: true,
            ..TWO
        });
    }

    /// Client 2 only ever writes PCM, through the marker band.
    const TONE: Fuzz = Fuzz {
        tone_second: true,
        ..TWO
    };

    #[test]
    fn fuzz_a_pcm_tone_beside_dop() {
        fuzz_seeds(TONE);
    }

    #[test]
    fn fuzz_a_resampled_pcm_tone_beside_dop() {
        fuzz_seeds(Fuzz {
            resampled_second: true,
            ..TONE
        });
    }

    #[test]
    fn fuzz_a_pcm_tone_beside_dop_with_stops() {
        fuzz_seeds(Fuzz {
            stops: true,
            ..TONE
        });
    }

    #[test]
    fn fuzz_a_pcm_tone_beside_dop_with_drains() {
        fuzz_seeds(Fuzz {
            drains: true,
            final_drain: true,
            ..TONE
        });
    }

    #[test]
    fn fuzz_a_pcm_tone_beside_dop_with_everything() {
        fuzz_seeds(Fuzz {
            resampled_second: true,
            stops: true,
            drains: true,
            reconfigures: true,
            final_drain: true,
            ..TONE
        });
    }

    #[test]
    fn fuzz_a_pcm_tone_first_in_the_table_beside_dop() {
        fuzz_seeds(Fuzz {
            b_first: true,
            stops: true,
            ..TONE
        });
    }

    #[test]
    fn fuzz_runs_written_in_pieces() {
        fuzz_seeds(Fuzz {
            split_runs: true,
            ..ONE
        });
    }

    #[test]
    fn fuzz_runs_written_in_pieces_with_stops() {
        fuzz_seeds(Fuzz {
            split_runs: true,
            stops: true,
            ..ONE
        });
    }

    #[test]
    fn fuzz_two_sources_writing_runs_in_pieces() {
        fuzz_seeds(Fuzz {
            split_runs: true,
            ..TWO
        });
    }

    #[test]
    fn fuzz_two_sources_writing_runs_in_pieces_with_everything() {
        fuzz_seeds(Fuzz {
            split_runs: true,
            resampled_second: true,
            stops: true,
            drains: true,
            reconfigures: true,
            final_drain: true,
            ..TWO
        });
    }

    #[test]
    fn fuzz_runs_in_pieces_beside_a_pcm_tone_first_in_the_table() {
        fuzz_seeds(Fuzz {
            split_runs: true,
            b_first: true,
            stops: true,
            reconfigures: true,
            ..TONE
        });
    }

    #[test]
    fn a_short_drain_does_not_push_dop_into_a_resampler() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = resampled(1, Format::F32);
        rig.add(&mut a);
        let mut block = frames(0, 20);
        block.extend(dop_frames(20));
        a.ring.write(&block);
        a.ring.header().draining.store(1, Release);
        for _ in 0..20 {
            rig.cycle();
        }
        assert!(rig.log.frames().iter().all(|f| *f == [0, 0]));
        let events = rig.events();
        assert_eq!(count(&events, |e| matches!(e, Event::DopStarted(1, _))), 1);
        assert_eq!(count(&events, |e| matches!(e, Event::Drained(1))), 1);
    }

    /// A resampled source judged on 30 silent frames, with 30 DoP frames
    /// written straight after the judgement.
    fn judged_before_a_write() -> (Client, Box<Source>) {
        let mut a = resampled(1, Format::F32);
        let mut source = a.source.take().unwrap();
        a.ring.write(&frames(0, 30));
        let mut window = [[0; 2]; 30];
        assert_eq!(source.ring.peek(&mut window), 30);
        source.judge(&window, PERIOD, false);
        a.ring.write(&dop_frames(30));
        (a, source)
    }

    #[test]
    fn frames_written_after_the_scan_are_not_read() {
        let (a, mut source) = judged_before_a_write();
        let mut out = [[0; 2]; PERIOD];
        source.read_resampled(&mut out);
        assert!(a.ring.available() >= 30);
    }

    #[test]
    fn frames_written_after_the_scan_do_not_count_as_input() {
        let (_a, source) = judged_before_a_write();
        assert!(!source.plays);
    }

    #[test]
    fn a_tail_too_short_to_judge_with_markers_is_replaced_by_silence() {
        fn check(mut a: Client) {
            let mut rig = rig_at_gain::<F32>(0.1);
            rig.add(&mut a);
            a.ring.write(&frames(0, 200));
            rig.cycle();
            rig.cycle();
            a.ring.write(&dop_frames(10));
            a.ring.header().draining.store(1, Release);
            for _ in 0..12 {
                rig.cycle();
            }
            assert_eq!(a.ring.available(), 0);
            assert!(rig.log.frames().iter().all(|f| *f == [0, 0]));
            assert_eq!(count(&rig.events(), |e| matches!(e, Event::Drained(1))), 1);
        }
        check(client(1));
        check(resampled(1, Format::F32));
    }

    #[test]
    fn pcm_before_a_short_tail_with_markers_still_plays() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(ODD, PERIOD));
        rig.cycle();
        let mut block = frames(ODD, 20);
        block.extend(dop_frames(10));
        a.ring.write(&block);
        a.ring.header().draining.store(1, Release);
        rig.cycle();
        let mut want = frames(ODD, 20);
        want.extend(frames(0, PERIOD - 20));
        assert_eq!(rig.last_periods(1), want);
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(0, PERIOD));
        assert_eq!(a.ring.available(), 0);
    }

    #[test]
    fn a_short_tail_without_markers_plays_normally() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(ODD, PERIOD));
        rig.cycle();
        a.ring.write(&frames(ODD, 10));
        a.ring.header().draining.store(1, Release);
        rig.cycle();
        assert_eq!(rig.last_periods(1)[..10], frames(ODD, 10));
    }

    #[test]
    fn a_held_frame_is_counted_and_is_not_an_underrun() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        let mut block = frames(1 << 24, PERIOD);
        a.ring.write(&block);
        rig.cycle();
        assert_eq!(rig.counters.held_frames.load(Relaxed), 0);
        block[PERIOD - 1] = [0x0512_3400; 2];
        a.ring.write(&block);
        rig.cycle();
        assert_eq!(rig.counters.held_frames.load(Relaxed), 1);
        assert_eq!(underruns(&a), 0);
        assert_eq!(a.ring.available(), 1);
        // The frame is delayed, not lost.
        a.ring.write(&frames(1 << 24, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1)[0], block[PERIOD - 1]);
    }

    #[test]
    fn waiting_to_judge_a_new_stream_is_not_counted_as_held() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&dop_frames(10));
        rig.cycle();
        assert_eq!(rig.counters.held_frames.load(Relaxed), 0);
    }

    /// Client 1 plays a DoP period while client 2 holds PCM with a DoP run
    /// of `dop_frames` that the discard cuts, leaving part of it before more
    /// PCM.
    fn a_run_cut_by_a_discard(
        second: Client,
        pcm_frames: usize,
        dop_run: usize,
    ) -> (Rig<F32>, Client, Client, usize) {
        let mut rig = rig_at_gain::<F32>(0.1);
        let start = rig.log.frames().len();
        let (mut a, mut b) = (client(1), second);
        rig.add(&mut a);
        rig.add(&mut b);
        rig.cycle();
        a.ring.write(&dop_frames(PERIOD));
        let mut block = frames(0, pcm_frames);
        block.extend(dop_frames(dop_run));
        block.extend(frames(0, 500));
        b.ring.write(&block);
        (rig, a, b, start)
    }

    fn assert_clean(rig: &Rig<F32>, start: usize) {
        let log = rig.log.frames();
        let altered = log[start..].iter().filter(|f| !is_clean_output(f, false));
        assert_eq!(altered.count(), 0);
    }

    #[test]
    fn a_run_cut_by_a_discard_is_not_mixed_when_the_owner_stops() {
        let (mut rig, _a, _b, start) = a_run_cut_by_a_discard(client(2), 60, 18);
        rig.cycle();
        rig.send(Command::SetRunning(1, false));
        rig.cycle();
        rig.cycle();
        assert_clean(&rig, start);
    }

    #[test]
    fn a_run_cut_by_a_discard_is_not_mixed_when_the_owner_drains() {
        let (mut rig, a, _b, start) = a_run_cut_by_a_discard(client(2), 60, 18);
        a.ring.header().draining.store(1, Release);
        for _ in 0..3 {
            rig.cycle();
        }
        assert_clean(&rig, start);
    }

    #[test]
    fn a_run_cut_by_a_discard_does_not_reach_a_resampler() {
        // A resampled source loses 58 or 59 frames of input in a period.
        let source = resampled(2, Format::F32);
        let (mut rig, _a, _b, start) = a_run_cut_by_a_discard(source, 50, 18);
        rig.cycle();
        rig.send(Command::SetRunning(1, false));
        for _ in 0..4 {
            rig.cycle();
        }
        assert_clean(&rig, start);
    }

    #[test]
    fn a_cut_run_long_enough_to_judge_plays_as_dop() {
        // The discard of 64 leaves 16 of the run, enough for `find`.
        let (mut rig, _a, _b, start) = a_run_cut_by_a_discard(client(2), 60, 20);
        rig.cycle();
        rig.send(Command::SetRunning(1, false));
        rig.cycle();
        assert_clean(&rig, start);
        let mut want = dop_frames(20)[4..].to_vec();
        want.extend(dop_silence(0x05, PERIOD - 16));
        assert_eq!(rig.last_periods(1), want);
    }

    #[test]
    fn pcm_after_a_cut_run_is_not_taken_for_dop() {
        let (mut rig, _a, b, _start) = a_run_cut_by_a_discard(client(2), 60, 18);
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, _)]));
        // Past the run, and with nothing marked left to find.
        b.ring.consume(b.ring.available());
        b.ring.write(&frames(1000, PERIOD));
        rig.send(Command::SetRunning(1, false));
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::DopStopped(1)]));
    }

    /// A DoP stream stopped with 10 frames of its run still in the ring.
    fn a_stopped_dop_stream() -> (Rig<F32>, Client, usize) {
        let mut rig = rig_at_gain::<F32>(0.1);
        let start = rig.log.frames().len();
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&dop_frames(PERIOD + 10));
        rig.cycle();
        rig.send(Command::SetRunning(1, false));
        rig.cycle();
        (rig, a, start)
    }

    #[test]
    fn a_stopped_dop_stream_resumes_as_dop() {
        let (mut rig, a, start) = a_stopped_dop_stream();
        a.ring.write(&frames(0, 500));
        rig.send(Command::SetRunning(1, true));
        rig.cycle();
        assert_clean(&rig, start);
        let mut want = dop_frames(PERIOD + 10)[PERIOD..].to_vec();
        want.extend(dop_silence(0x05, PERIOD - 10));
        assert_eq!(rig.last_periods(1), want);
        assert_eq!(underruns(&a), 0);
        let events = rig.events();
        assert!(matches!(
            events[..],
            [
                Event::DopStarted(1, _),
                Event::DopStopped(1),
                Event::DopStarted(1, _)
            ]
        ));
    }

    #[test]
    fn a_dop_stream_restarted_with_an_empty_ring_is_a_new_start() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut b);
        a.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        rig.send(Command::SetRunning(1, false));
        rig.cycle();
        rig.events();
        rig.send(Command::SetRunning(1, true));
        b.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        // It claims nothing until it has something to play.
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
        assert!(rig.events().is_empty());
        assert_eq!(underruns(&a), 0);
    }

    /// Where two outputs first differ, with the frames there.
    fn first_difference(got: &[[i32; 2]], want: &[[i32; 2]]) -> Option<String> {
        if got.len() != want.len() {
            return Some(format!("{} frames, wanted {}", got.len(), want.len()));
        }
        let differing = got.iter().zip(want).filter(|(g, w)| g != w).count();
        let at = got.iter().zip(want).position(|(g, w)| g != w)?;
        Some(format!(
            "{differing} frames differ, the first at {at} in period {}: {:08x} for {:08x}",
            at / PERIOD,
            got[at][0] as u32,
            want[at][0] as u32
        ))
    }

    /// Client 2, PCM at the output rate, was added first, so it comes first
    /// in the table. Client 1 plays DoP, and nothing of client 2 may show.
    #[test]
    fn a_pcm_client_added_first_does_not_take_the_output_from_dop() {
        let mut rig = rig_at_gain::<F32>(0.1);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut b);
        rig.add(&mut a);
        let start = rig.log.frames().len();
        let cycles = 2000;
        let dop = dop_frames(cycles * PERIOD);
        for c in 0..cycles {
            let period = c * PERIOD..(c + 1) * PERIOD;
            a.ring.write(&dop[period.clone()]);
            let block: Vec<[i32; 2]> = period.map(|i| tone(i, RATE as f64, 220.0, 0.1)).collect();
            b.ring.write(&block);
            rig.cycle();
        }
        // Every frame is client 1's, in order, with the markers unbroken.
        let log = rig.log.frames();
        assert_eq!(first_difference(&log[start..], &dop), None);
        let events = rig.events();
        assert_eq!(count(&events, |e| matches!(e, Event::DopStarted(2, _))), 0);
        assert_eq!(count(&events, |e| matches!(e, Event::DopStarted(1, _))), 1);
    }

    /// An engine at a gain of 0.1 and this format.
    fn rig_at(rate: u32, period: usize) -> Rig<F32> {
        let mut rig = Rig::<F32>::closed(BitPerfect::Auto, 1024);
        rig.send(Command::Reconfigure {
            rate,
            period: period as u32,
        });
        rig.send(Command::SetGain(0.1));
        rig.cycle();
        rig.events();
        rig
    }

    /// `DopStarted` events for a PCM client that was added first, playing a
    /// 220 Hz tone at `amp` beside a DoP client, over `seconds` of output.
    fn false_dop_starts(
        pcm_rate: u32,
        dop_rate: u32,
        period: usize,
        amp: f64,
        seconds: usize,
    ) -> usize {
        let mut rig = rig_at(dop_rate, period);
        let (mut a, mut b) = (client_at(1, dop_rate), client_at(2, pcm_rate));
        if pcm_rate != dop_rate {
            let r = Resampler::new(
                pcm_rate,
                dop_rate,
                period as u32,
                Quality::High,
                Format::F32,
            );
            b.source.as_mut().unwrap().resampler = Some(Box::new(r));
        }
        rig.add(&mut b);
        rig.add(&mut a);
        let cycles = dop_rate as usize * seconds / period;
        let dop = dop_frames(period + 2);
        let (mut written_a, mut written_b) = (0usize, 0usize);
        let mut starts = 0;
        for c in 0..cycles {
            let phase = written_a % 2;
            a.ring.write(&dop[phase..phase + period]);
            written_a += period;
            // Keeps the PCM client about 400 frames ahead of the output.
            let taken = (c + 1) * period * pcm_rate as usize / dop_rate as usize;
            let want = taken + 400 - written_b;
            let block: Vec<[i32; 2]> = (written_b..written_b + want)
                .map(|i| tone(i, f64::from(pcm_rate), 220.0, amp))
                .collect();
            b.ring.write(&block);
            written_b += want;
            rig.cycle();
            starts += count(&rig.events(), |e| matches!(e, Event::DopStarted(2, _)));
        }
        starts
    }

    /// The three rows measured before the fix: 319, 64 and 531 in 10 s.
    #[test]
    fn a_pcm_client_beside_dop_is_never_reported_as_dop() {
        let rows = [
            false_dop_starts(48000, 176400, 1024, 0.05, 2),
            false_dop_starts(48000, 176400, 1024, 0.1, 2),
            false_dop_starts(48000, 48000, 64, 0.1, 2),
        ];
        assert_eq!(rows, [0, 0, 0]);
    }

    /// The rate policy drops a stopped client's claim, so a restart has to
    /// make it again, even when both happen between two cycles.
    #[test]
    fn a_dop_stream_restarted_within_one_cycle_resends_dop_started() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        let dop = dop_frames(3 * PERIOD);
        a.ring.write(&dop);
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, _)]));
        rig.send(Command::SetRunning(1, false));
        rig.send(Command::SetRunning(1, true));
        rig.cycle();
        assert!(matches!(
            rig.events()[..],
            [Event::DopStopped(1), Event::DopStarted(1, _)]
        ));
        assert_eq!(rig.last_periods(1), dop[PERIOD..2 * PERIOD]);
    }

    /// A stream known as DoP keeps what a discard leaves of its run, however
    /// short, because it is DoP by that knowledge and not by the frames.
    #[test]
    fn a_second_dop_stream_keeps_its_cut_run_while_discarded() {
        let mut rig = rig_at_gain::<F32>(0.1);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut b);
        let dop = dop_frames(PERIOD + 10);
        a.ring.write(&dop[..PERIOD]);
        b.ring.write(&dop);
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop[..PERIOD]);
        rig.send(Command::SetRunning(1, false));
        rig.cycle();
        let mut want = dop[PERIOD..].to_vec();
        want.extend(dop_silence(0x05, PERIOD - 10));
        assert_eq!(rig.last_periods(1), want);
    }

    /// Client 1 plays one DoP period while client 2, PCM, has written 40
    /// frames of PCM and the first `before` of a DoP run. The discard takes
    /// all of them, or all but a few. Client 1 then stops, and client 2
    /// writes the run's last `tail` frames with silent PCM after them.
    fn a_run_split_by_a_discard(before: usize, tail: usize) -> (Rig<F32>, usize) {
        let mut rig = rig_at_gain::<F32>(0.1);
        let start = rig.log.frames().len();
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut b);
        rig.cycle();
        a.ring.write(&dop_frames(PERIOD));
        let run = dop_frames(before + tail);
        let mut block = frames(0, 40);
        block.extend(&run[..before]);
        b.ring.write(&block);
        rig.cycle();
        rig.send(Command::SetRunning(1, false));
        let mut next = run[before..].to_vec();
        next.extend(frames(0, 3 * PERIOD));
        b.ring.write(&next);
        rig.cycle();
        rig.cycle();
        (rig, start)
    }

    #[test]
    fn a_run_tail_written_after_the_discard_is_dropped_and_not_mixed() {
        for tail in [1, 8, 15] {
            let (mut rig, start) = a_run_split_by_a_discard(24, tail);
            assert_clean(&rig, start);
            assert_eq!(rig.last_periods(2), frames(0, 2 * PERIOD), "tail {tail}");
            let starts = count(&rig.events(), |e| matches!(e, Event::DopStarted(2, _)));
            assert_eq!(starts, 0, "tail {tail}");
        }
    }

    #[test]
    fn the_credit_for_a_dropped_tail_ends_with_the_stream() {
        let mut rig = rig_at_gain::<F32>(0.1);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut b);
        rig.cycle();
        // The discard cuts a run of client 2, and the owner stops.
        a.ring.write(&dop_frames(PERIOD));
        let run = dop_frames(32);
        let mut block = frames(0, 40);
        block.extend(&run[..24]);
        b.ring.write(&block);
        rig.cycle();
        rig.send(Command::SetRunning(1, false));
        // The tail of 8 is dropped, with no discard after it to credit.
        let mut next = run[24..].to_vec();
        next.extend(frames(0, 3 * PERIOD));
        b.ring.write(&next);
        rig.cycle();
        rig.cycle();
        // Client 2 stops and starts: a new stream, in real time from here.
        rig.send(Command::SetRunning(2, false));
        rig.cycle();
        rig.send(Command::SetRunning(2, true));
        rig.send(Command::SetRunning(1, true));
        a.ring.write(&dop_frames(PERIOD));
        b.ring.write(&frames(0, 2 * PERIOD));
        let before = b.ring.header().read_pos.load(Relaxed);
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop_frames(PERIOD));
        // A whole period is discarded, not one short of the old tail.
        let taken = b.ring.header().read_pos.load(Relaxed) - before;
        assert_eq!(taken, PERIOD as u64);
    }

    #[test]
    fn a_run_tail_long_enough_to_judge_plays_as_dop() {
        for tail in [16, 20] {
            let (rig, start) = a_run_split_by_a_discard(24, tail);
            assert_clean(&rig, start);
            let mut want = dop_frames(24 + tail)[24..].to_vec();
            want.extend(dop_silence(0x05, PERIOD - tail));
            want.extend(frames(0, PERIOD));
            assert_eq!(rig.last_periods(2), want, "tail {tail}");
        }
    }

    /// The discard leaves 10 of the run, and 8 more arrive: a run of 18,
    /// which `find` judges DoP.
    #[test]
    fn a_run_left_behind_and_completed_later_plays_as_dop() {
        let (rig, start) = a_run_split_by_a_discard(34, 8);
        assert_clean(&rig, start);
        let mut want = dop_frames(42)[24..].to_vec();
        want.extend(dop_silence(0x05, PERIOD - 18));
        want.extend(frames(0, PERIOD));
        assert_eq!(rig.last_periods(2), want);
    }

    /// Discarding a PCM client that crosses the marker band drops frames,
    /// which must count against the next discard.
    #[test]
    fn discarding_a_tone_in_the_band_keeps_real_time() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut b);
        let at = |i: usize| tone(i, RATE as f64, 220.0, 0.05);
        let cushion = 60_000;
        b.ring.write(&(0..cushion).map(at).collect::<Vec<_>>());
        let dop = dop_frames(PERIOD);
        let cycles = 900;
        let mut written = cushion;
        for _ in 0..cycles {
            a.ring.write(&dop);
            let block: Vec<[i32; 2]> = (written..written + PERIOD).map(at).collect();
            b.ring.write(&block);
            written += PERIOD;
            rig.cycle();
        }
        assert_eq!(written - b.ring.available(), cycles * PERIOD);
        assert_eq!(underruns(&b), 0);
    }

    /// A DoP stream that played all it had, then stopped, 50 cycles ago.
    fn a_dop_stream_stopped_with_an_empty_ring() -> (Rig<F32>, Client) {
        let mut rig = rig_at_gain::<F32>(0.1);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        rig.send(Command::SetRunning(1, false));
        for _ in 0..50 {
            rig.cycle();
        }
        rig.events();
        (rig, a)
    }

    /// 40 frames of PCM in the marker band, then PCM well above it.
    fn pcm_in_the_band() -> Vec<[i32; 2]> {
        let mut block = vec![[0x0510_0000; 2]; 40];
        block.extend(frames(1 << 24, 200));
        block
    }

    #[test]
    fn a_dop_stream_stopped_with_an_empty_ring_does_not_claim_pcm_in_the_band() {
        let (mut rig, a) = a_dop_stream_stopped_with_an_empty_ring();
        let start = rig.log.frames().len();
        rig.send(Command::SetRunning(1, true));
        a.ring.write(&pcm_in_the_band());
        rig.cycle();
        rig.cycle();
        let log = rig.log.frames();
        let raw = log[start..].iter().filter(|f| f[0] == 0x0510_0000);
        assert_eq!(raw.count(), 0);
        // Mixed at a gain of 0.1 instead.
        assert!(
            (log[start][0] - 0x0510_0000 / 10).abs() < 1 << 12,
            "{:08x}",
            log[start][0]
        );
        assert!(rig.events().is_empty());
    }

    #[test]
    fn a_dop_stream_stopped_with_an_empty_ring_does_not_displace_a_dop_owner() {
        let (mut rig, a) = a_dop_stream_stopped_with_an_empty_ring();
        let mut c = client(3);
        rig.add(&mut c);
        let dop = dop_frames(4 * PERIOD);
        c.ring.write(&dop);
        rig.cycle();
        assert!(matches!(rig.events()[..], [Event::DopStarted(3, _)]));
        let start = rig.log.frames().len();
        rig.send(Command::SetRunning(1, true));
        a.ring.write(&pcm_in_the_band());
        rig.cycle();
        rig.cycle();
        assert_eq!(rig.log.frames()[start..], dop[PERIOD..3 * PERIOD]);
        assert!(rig.events().is_empty());
    }

    #[test]
    fn a_run_tail_written_after_a_stop_with_an_empty_ring_is_not_mixed() {
        let (mut rig, a) = a_dop_stream_stopped_with_an_empty_ring();
        let start = rig.log.frames().len();
        rig.send(Command::SetRunning(1, true));
        let mut block = dop_frames(PERIOD + 8)[PERIOD..].to_vec();
        block.extend(frames(0, 3 * PERIOD));
        a.ring.write(&block);
        rig.cycle();
        rig.cycle();
        assert_clean(&rig, start);
        assert_eq!(rig.last_periods(2), frames(0, 2 * PERIOD));
        assert!(rig.events().is_empty());
    }

    /// The discard of a PCM client ends on frames in the band, so a cut run
    /// is remembered. The next period starts outside the band, which has to
    /// clear that memory: the periods after it start with a few frames in
    /// the band, and every one of them must be heard, bit-exact at unity.
    #[test]
    fn the_cut_run_memory_clears_once_a_window_starts_unmarked() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut b);
        rig.add(&mut a);
        let (band, plain) = ([0x0510_0000; 2], [0x0100_0000; 2]);
        let mut cut = vec![plain; PERIOD - 3];
        cut.extend([band; 3]);
        let mut rest = vec![plain; PERIOD];
        for _ in 0..20 {
            rest.extend([band; 3]);
            rest.extend(vec![plain; PERIOD - 3]);
        }
        b.ring.write(&cut);
        b.ring.write(&rest);
        a.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        rig.send(Command::SetRunning(1, false));
        for _ in 0..21 {
            rig.cycle();
        }
        assert_eq!(first_difference(&rig.last_periods(21), &rest), None);
    }

    /// What a test plugin does to each sample, seen at full scale 1.0.
    #[derive(Clone, Copy)]
    enum Op {
        Add,
        Mul,
        Nan,
    }

    /// A plugin for the tests, with one parameter, `value`.
    struct Probe {
        op: Op,
        value: f64,
        neutral: bool,
        latency: u32,
        formats: Formats,
        params: Vec<Param>,
        /// Reports `no_signal` for every block.
        quiet: bool,
    }

    const ALL_FORMATS: Formats = Formats {
        i32: true,
        f32: true,
        f64: true,
    };

    impl Probe {
        fn new(op: Op, value: f64) -> Probe {
            Probe {
                op,
                value,
                neutral: false,
                latency: 0,
                formats: ALL_FORMATS,
                params: vec![Param {
                    name: "value".into(),
                    unit: String::new(),
                    kind: ParamKind::Float,
                    min: -100.0,
                    max: 100.0,
                    default: value,
                }],
                quiet: false,
            }
        }

        fn quiet(mut self) -> Probe {
            self.quiet = true;
            self
        }

        fn neutral(mut self) -> Probe {
            self.neutral = true;
            self
        }

        fn latency(mut self, frames: u32) -> Probe {
            self.latency = frames;
            self
        }

        fn only(mut self, formats: Formats) -> Probe {
            self.formats = formats;
            self
        }

        fn apply(&self, s: f64) -> f64 {
            match self.op {
                Op::Add => s + self.value,
                Op::Mul => s * self.value,
                Op::Nan => f64::NAN,
            }
        }
    }

    impl Plugin for Probe {
        fn name(&self) -> &str {
            "probe"
        }

        fn formats(&self) -> Formats {
            self.formats
        }

        fn params(&self) -> &[Param] {
            &self.params
        }

        fn set_param(&mut self, index: usize, value: f64) {
            assert_eq!(index, 0);
            self.value = value;
        }

        fn neutral(&self) -> bool {
            self.neutral
        }

        fn latency_frames(&self) -> u32 {
            self.latency
        }

        fn process(&mut self, block: Block<'_>) -> PluginStatus {
            const FULL: f64 = 2_147_483_648.0;
            assert!(self.formats.has(block.format()));
            match block {
                Block::I32(d) => {
                    for s in d.iter_mut().flatten() {
                        *s = (self.apply(f64::from(*s) / FULL) * FULL).round() as i32;
                    }
                }
                Block::F32(d) => {
                    for s in d.iter_mut().flatten() {
                        *s = self.apply(f64::from(*s)) as f32;
                    }
                }
                Block::F64(d) => {
                    for s in d.iter_mut().flatten() {
                        *s = self.apply(*s);
                    }
                }
            }
            PluginStatus {
                no_signal: self.quiet,
                changed: false,
            }
        }
    }

    /// Adds `frames` of S32 to every sample.
    fn plus(frames: i32) -> Probe {
        Probe::new(Op::Add, f64::from(frames) / 2_147_483_648.0)
    }

    fn times(factor: f64) -> Probe {
        Probe::new(Op::Mul, factor)
    }

    /// The format a rig of `P` processes in.
    fn format_of<P: Processing>() -> Format {
        P::to_block(&mut [], &mut BlockStore::new(0)).format()
    }

    /// A chain in the processing format of `P`, built for the longest period,
    /// as the engine takes them.
    fn chain_of<P: Processing>(plugins: Vec<Probe>) -> Box<Chain> {
        chain_for::<P>(plugins, MAX_PERIOD as usize)
    }

    fn chain_for<P: Processing>(plugins: Vec<Probe>, max_frames: usize) -> Box<Chain> {
        let plugins = plugins
            .into_iter()
            .map(|p| Box::new(p) as Box<dyn Plugin>)
            .collect();
        Box::new(Chain::new(plugins, format_of::<P>(), max_frames))
    }

    /// Takes only floats, so an engine in another format converts around it.
    const F64_ONLY: Formats = Formats {
        i32: false,
        f32: false,
        f64: true,
    };

    fn set_chain<P: Processing>(rig: &mut Rig<P>, place: Place, plugins: Vec<Probe>) {
        rig.send(Command::SetChain(place, chain_of::<P>(plugins)));
    }

    #[test]
    fn a_chain_built_for_another_rate_never_runs_and_its_place_is_silent() {
        fn check<P: Processing>() {
            let mut rig = Rig::<P>::new(BitPerfect::Auto);
            let mut a = client(1);
            rig.add(&mut a);
            // Built for 96 kHz, while the engine runs at 48 kHz.
            let chain = chain_of::<P>(vec![times(2.0)]);
            rig.send(Command::SetChain(
                Place::Mix,
                Box::new((*chain).for_rate(96000)),
            ));
            for _ in 0..4 {
                a.ring.write(&frames(1 << 20, PERIOD));
                rig.cycle();
            }
            assert_eq!(rig.last_periods(4), frames(0, 4 * PERIOD));
            // The chain for the engine's rate arrives: the audio fades back
            // in over 10 ms, through it.
            let chain = chain_of::<P>(vec![times(2.0)]);
            rig.send(Command::SetChain(
                Place::Mix,
                Box::new((*chain).for_rate(RATE)),
            ));
            let mut out = Vec::new();
            for _ in 0..12 {
                a.ring.write(&frames(1 << 20, PERIOD));
                rig.cycle();
                out.extend(rig.last_periods(1));
            }
            let full = 1 << 21;
            assert!(out[0][0] <= full / 480 + 1, "{:?}", out[0]);
            let step = out.windows(2).map(|w| (w[1][0] - w[0][0]).abs()).max();
            assert!(step.unwrap() <= full / 480 + 2, "{:?}", format_of::<P>());
            assert!(
                out[480..].iter().all(|f| f[0] == full),
                "{:?}",
                format_of::<P>()
            );
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_fade_in_due_at_a_reopen_lasts_10_ms_at_the_new_rate() {
        // Muted at one rate, the chain fits once the output reopens at the
        // other: lower, then higher.
        for (muted_at, opens_at) in [(192000, RATE), (RATE, 192000)] {
            let mut rig = Rig::<I32>::new(BitPerfect::Auto);
            let mut a = client(1);
            rig.add(&mut a);
            let reopen = |rig: &mut Rig<I32>, rate| {
                rig.send(Command::Reconfigure {
                    rate,
                    period: PERIOD as u32,
                });
                rig.cycle();
                rig.cycle();
            };
            if muted_at != RATE {
                reopen(&mut rig, muted_at);
            }
            let chain = chain_of::<I32>(vec![times(2.0)]);
            rig.send(Command::SetChain(
                Place::Mix,
                Box::new((*chain).for_rate(opens_at)),
            ));
            a.ring.write(&frames(1 << 20, PERIOD));
            rig.cycle();
            assert_eq!(rig.last_periods(1), frames(0, PERIOD));
            reopen(&mut rig, opens_at);
            let mut out = Vec::new();
            for _ in 0..40 {
                a.ring.write(&frames(1 << 20, PERIOD));
                rig.cycle();
                out.extend(rig.last_periods(1));
            }
            let full = 1 << 21;
            let what = format!("{muted_at} to {opens_at}");
            assert!(out.iter().all(|f| f[0] <= full), "{what}");
            let first = out.iter().position(|f| f[0] != 0).expect("audio");
            let reached = out.iter().position(|f| f[0] == full).expect("full level");
            // The fade runs from the first period at the new rate, which
            // the empty ring may leave silent.
            let fade = (opens_at * FADE_MS / 1000) as usize;
            let took = reached - first;
            assert!(took < fade && took + PERIOD >= fade - 1, "{what}: {took}");
        }
    }

    #[test]
    fn after_a_mute_the_copy_path_fades_in_too_and_no_dip_comes_later() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut b);
        // A neutral mix chain built for 96 kHz: two clients are mixed, so
        // the chain would run, and the output is silent.
        let chain = chain_of::<I32>(vec![times(1.0).neutral()]);
        rig.send(Command::SetChain(
            Place::Mix,
            Box::new((*chain).for_rate(96000)),
        ));
        a.ring.write(&frames(1 << 20, PERIOD));
        b.ring.write(&frames(0, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(0, PERIOD));
        // One client is left, and the chain fits: the copy path would
        // start at full level at once.
        rig.send(Command::Remove(2));
        let chain = chain_of::<I32>(vec![times(1.0).neutral()]);
        rig.send(Command::SetChain(
            Place::Mix,
            Box::new((*chain).for_rate(RATE)),
        ));
        let mut out = Vec::new();
        for _ in 0..20 {
            a.ring.write(&frames(1 << 20, PERIOD));
            rig.cycle();
            out.extend(rig.last_periods(1));
        }
        assert!(out[0][0] <= (1 << 20) / 480 + 1, "{:?}", out[0]);
        assert!(out[480..].iter().all(|f| f[0] == 1 << 20));
        // Once the fade is over the copy path is bit-exact again.
        let input = ramp(4 * PERIOD);
        a.ring.write(&input);
        for _ in 0..4 {
            rig.cycle();
        }
        assert_eq!(rig.last_periods(4), input);
        assert_eq!(decide(&rig.engine.situation()), Ok(()));
    }

    #[test]
    fn an_analog_chain_built_for_another_rate_gives_no_signal() {
        let (mut rig, feed) = with_capture::<F32>(BitPerfect::Auto);
        let chain = chain_of::<F32>(vec![times(1.0).neutral()]);
        rig.send(Command::SetChain(
            Place::Analog,
            Box::new((*chain).for_rate(96000)),
        ));
        for _ in 0..3 {
            feed.push(&frames(1 << 20, PERIOD));
            rig.cycle();
        }
        assert_eq!(rig.last_periods(3), frames(0, 3 * PERIOD));
        assert!(signals(&rig.events()).is_empty());
    }

    #[test]
    fn a_plugin_on_the_mix_changes_what_reaches_the_sink() {
        fn check<P: Processing>() {
            let mut rig = Rig::<P>::new(BitPerfect::Auto);
            let mut a = client(1);
            rig.add(&mut a);
            set_chain(&mut rig, Place::Mix, vec![times(2.0)]);
            a.ring.write(&frames(1000, PERIOD));
            rig.cycle();
            assert_eq!(
                rig.last_periods(1),
                frames(2000, PERIOD),
                "{:?}",
                format_of::<P>()
            );
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn the_mix_chain_runs_before_the_output_chain() {
        let mut rig = Rig::<F64>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        set_chain(&mut rig, Place::Output, vec![times(2.0)]);
        set_chain(&mut rig, Place::Mix, vec![plus(1 << 20)]);
        a.ring.write(&frames(1 << 20, PERIOD));
        rig.cycle();
        // (x + c) * 2, not x * 2 + c.
        assert_eq!(rig.last_periods(1), vec![[1 << 22, 0]; PERIOD]);
    }

    #[test]
    fn a_plugin_that_is_not_neutral_stops_bit_perfect() {
        for place in [Place::Mix, Place::Output] {
            let mut rig = Rig::<F32>::new(BitPerfect::Auto);
            let mut a = client(1);
            rig.add(&mut a);
            set_chain(&mut rig, place, vec![times(1.0)]);
            a.ring.write(&frames(ODD, 2 * PERIOD));
            rig.cycle();
            assert_eq!(decide(&rig.engine.situation()), Err(NotBitPerfect::Plugin));
            // An f32 mix cannot hold all 32 bits.
            assert_ne!(rig.last_periods(1), frames(ODD, PERIOD));
        }
    }

    fn bit_exact_beside_neutral_plugins<P: Processing>() {
        let mut rig = Rig::<P>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        set_chain(&mut rig, Place::Mix, vec![times(1.0).neutral()]);
        set_chain(
            &mut rig,
            Place::Output,
            vec![times(1.0).neutral().latency(4)],
        );
        let input: Vec<[i32; 2]> = (0..PERIOD as i32 * 3)
            .map(|i| [ODD + i, i32::MIN + i])
            .collect();
        a.ring.write(&input);
        for _ in 0..3 {
            rig.cycle();
        }
        assert_eq!(decide(&rig.engine.situation()), Ok(()));
        assert_eq!(rig.log.frames()[PERIOD..], input, "{:?}", format_of::<P>());
    }

    #[test]
    fn a_neutral_plugin_leaves_one_source_bit_exact() {
        bit_exact_beside_neutral_plugins::<I32>();
        bit_exact_beside_neutral_plugins::<F32>();
        bit_exact_beside_neutral_plugins::<F64>();
    }

    #[test]
    fn forced_bit_perfect_bypasses_plugins() {
        fn check<P: Processing>() {
            let mut rig = Rig::<P>::new(BitPerfect::Forced);
            let mut a = client(1);
            a.source.as_mut().unwrap().gain = 0.25;
            rig.add(&mut a);
            set_chain(&mut rig, Place::Mix, vec![times(2.0)]);
            set_chain(&mut rig, Place::Output, vec![times(2.0)]);
            rig.send(Command::SetGain(0.1));
            // One source is copied untouched, whatever the plugins.
            let input = ramp(2 * PERIOD);
            a.ring.write(&input);
            rig.cycle();
            rig.cycle();
            assert_eq!(rig.last_periods(2), input, "{:?}", format_of::<P>());
            assert_eq!(decide(&rig.engine.situation()), Ok(()));
            // A second is mixed in at unity, and the plugins stay out.
            let mut b = client(2);
            rig.add(&mut b);
            a.ring.write(&frames(1000, PERIOD));
            b.ring.write(&frames(234, PERIOD));
            rig.cycle();
            assert_eq!(rig.last_periods(1), frames(1234, PERIOD));
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn plugin_latency_is_not_in_the_delay_in_forced_bit_perfect() {
        let mut rig = Rig::<F32>::new(BitPerfect::Forced);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut b);
        set_chain(&mut rig, Place::Mix, vec![plus(0).latency(10)]);
        set_chain(&mut rig, Place::Output, vec![plus(0).latency(4)]);
        rig.log.set_delay(128);
        a.ring.write(&frames(1000, PERIOD));
        b.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        // Mixed, and still without the plugins' delay.
        let delay = |c: &Client| c.ring.header().output_delay_frames.load(Relaxed);
        assert_eq!(delay(&a), 128);
        assert_eq!(delay(&b), 128);
    }

    #[test]
    fn dop_passes_by_every_chain() {
        fn check<P: Processing>() {
            let mut rig = Rig::<P>::new(BitPerfect::Auto);
            let mut a = client(1);
            rig.add(&mut a);
            set_chain(&mut rig, Place::Mix, vec![plus(1000), times(0.5)]);
            set_chain(
                &mut rig,
                Place::Output,
                vec![times(2.0), Probe::new(Op::Nan, 0.0)],
            );
            a.ring.write(&dop_frames(2 * PERIOD));
            rig.cycle();
            rig.cycle();
            assert_eq!(rig.last_periods(2), dop_frames(2 * PERIOD));
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_replaced_chain_comes_back() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        set_chain(&mut rig, Place::Mix, vec![plus(0).latency(1)]);
        set_chain(&mut rig, Place::Mix, vec![plus(0).latency(2)]);
        set_chain(&mut rig, Place::Output, vec![plus(0).latency(3)]);
        set_chain(&mut rig, Place::Analog, vec![plus(0).latency(4)]);
        rig.cycle();
        let latencies = old_chains;
        // The engine starts with empty chains.
        assert_eq!(
            latencies(rig.events()),
            [Some(0), Some(1), Some(0), Some(0)]
        );
        // Each place keeps its own.
        set_chain(&mut rig, Place::Analog, vec![]);
        set_chain(&mut rig, Place::Output, vec![]);
        set_chain(&mut rig, Place::Mix, vec![]);
        rig.cycle();
        assert_eq!(latencies(rig.events()), [Some(4), Some(3), Some(2)]);
    }

    #[test]
    fn a_parameter_set_takes_effect_on_the_next_cycle() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        set_chain(&mut rig, Place::Mix, vec![plus(0), times(2.0)]);
        set_chain(&mut rig, Place::Output, vec![plus(0)]);
        a.ring.write(&frames(1000, 3 * PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(2000, PERIOD));
        assert_eq!(rig.events().len(), 2);
        let set = |place, plugin, value| Command::SetParam {
            place,
            plugin,
            index: 0,
            value,
        };
        rig.send(set(Place::Mix, 1, 3.0));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(3000, PERIOD));
        rig.send(set(Place::Output, 0, 7.0 / 2_147_483_648.0));
        rig.cycle();
        assert_eq!(rig.last_periods(1), vec![[3007, -2993]; PERIOD]);
        assert!(rig.events().is_empty());
    }

    #[test]
    fn a_parameter_for_a_plugin_that_is_not_there_is_ignored() {
        let mut rig = Rig::<F64>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        set_chain(&mut rig, Place::Mix, vec![times(2.0)]);
        rig.cycle();
        rig.events();
        a.ring.write(&frames(1000, PERIOD));
        // The probe panics on any index but 0.
        for (place, plugin, index) in [
            (Place::Mix, 0, 1),
            (Place::Mix, 1, 0),
            (Place::Output, 0, 0),
            (Place::Analog, 3, 3),
        ] {
            rig.send(Command::SetParam {
                place,
                plugin,
                index,
                value: 5.0,
            });
        }
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(2000, PERIOD));
        assert!(rig.events().is_empty());
    }

    /// The latencies of the `OldChain` events, `None` for any other event.
    fn old_chains(events: Vec<Event>) -> Vec<Option<u32>> {
        let old = events.into_iter().map(|e| match e {
            Event::OldChain(c) => Some(c.latency_frames()),
            _ => None,
        });
        old.collect()
    }

    #[test]
    fn a_chain_built_for_less_than_the_longest_period_is_refused() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        set_chain(&mut rig, Place::Mix, vec![times(2.0).latency(1)]);
        rig.cycle();
        rig.events();
        let short = MAX_PERIOD as usize - 1;
        let refused = chain_for::<I32>(vec![times(3.0).latency(7)], short);
        rig.send(Command::SetChain(Place::Mix, refused));
        a.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        // The refused chain comes back, and the one in place keeps playing.
        assert_eq!(old_chains(rig.events()), [Some(7)]);
        assert_eq!(rig.last_periods(1), frames(2000, PERIOD));
        // A chain without plugins takes any period.
        rig.send(Command::SetChain(Place::Mix, Box::new(Chain::empty())));
        rig.cycle();
        assert_eq!(old_chains(rig.events()), [Some(1)]);
    }

    #[test]
    fn a_reconfigure_to_a_longer_period_keeps_the_chains_working() {
        fn check<P: Processing>() {
            const LONGER: usize = 2 * PERIOD;
            let mut rig = Rig::<P>::new(BitPerfect::Auto);
            let mut a = client(1);
            rig.add(&mut a);
            set_chain(&mut rig, Place::Mix, vec![times(2.0).only(F64_ONLY)]);
            set_chain(&mut rig, Place::Output, vec![plus(0)]);
            a.ring.write(&frames(1000, PERIOD));
            rig.cycle();
            rig.send(Command::Reconfigure {
                rate: RATE,
                period: LONGER as u32,
            });
            a.ring.write(&frames(1000, 10 * LONGER));
            for _ in 0..4 {
                rig.cycle();
            }
            assert_eq!(rig.log.opens().last(), Some(&(RATE, LONGER as u32)));
            let log = rig.log.frames();
            let last = &log[log.len() - LONGER..];
            assert_eq!(last, frames(2000, LONGER), "{:?}", format_of::<P>());
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_chain_sent_just_before_a_reconfigure_takes_effect_with_the_reopen() {
        const LONGER: usize = 2 * PERIOD;
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        set_chain(&mut rig, Place::Output, vec![times(2.0).latency(2)]);
        a.ring.write(&frames(1000, 20 * PERIOD));
        rig.cycle();
        rig.events();
        rig.send(Command::SetChain(
            Place::Output,
            chain_of::<I32>(vec![times(3.0).latency(3)]),
        ));
        rig.send(Command::Reconfigure {
            rate: RATE,
            period: LONGER as u32,
        });
        // The fade before the reopen still runs the old chain. It starts at
        // full level.
        rig.cycle();
        assert_eq!(rig.last_periods(1)[0], [2000, -2000]);
        assert!(old_chains(rig.events()).is_empty());
        rig.cycle();
        let log = rig.log.frames();
        assert_eq!(log[log.len() - LONGER..], frames(3000, LONGER));
        let events = old_chains(rig.events());
        assert!(events.contains(&Some(2)), "{events:?}");
    }

    #[test]
    fn the_delay_leaves_out_plugins_that_did_not_run() {
        let delay = |c: &Client| c.ring.header().output_delay_frames.load(Relaxed);
        // A bit-perfect copy runs no chain.
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        set_chain(
            &mut rig,
            Place::Output,
            vec![times(1.0).neutral().latency(20)],
        );
        rig.log.set_delay(128);
        a.ring.write(&frames(ODD, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(ODD, PERIOD));
        assert_eq!(delay(&a), 128);
        // Nor does DoP.
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        set_chain(&mut rig, Place::Mix, vec![times(2.0).latency(20)]);
        rig.log.set_delay(128);
        a.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop_frames(PERIOD));
        assert_eq!(delay(&a), 128);
    }

    #[test]
    fn plugin_latency_is_added_to_the_published_delay() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        let mut b = client_at(2, 24000);
        let r = Resampler::new(24000, RATE, PERIOD as u32, Quality::High, Format::F32);
        let filter = r.delay_frames();
        b.source.as_mut().unwrap().resampler = Some(Box::new(r));
        rig.add(&mut a);
        rig.add(&mut b);
        set_chain(
            &mut rig,
            Place::Mix,
            vec![plus(0).latency(10), plus(0).latency(2)],
        );
        set_chain(&mut rig, Place::Output, vec![plus(0).latency(4)]);
        // Not run in the output's path, so not part of its delay.
        set_chain(&mut rig, Place::Analog, vec![plus(0).latency(1000)]);
        rig.log.set_delay(128);
        rig.cycle();
        let delay = |c: &Client| c.ring.header().output_delay_frames.load(Relaxed);
        assert_eq!(delay(&a), 128 + 16);
        assert_eq!(delay(&b), (128 + 16 + filter) / 2);
    }

    #[test]
    fn a_nan_from_a_plugin_reaches_the_card_as_zero() {
        fn check<P: Processing>() {
            let mut rig = Rig::<P>::new(BitPerfect::Auto);
            let mut a = client(1);
            rig.add(&mut a);
            // The output chain gets the NaN, whatever comes after it.
            set_chain(&mut rig, Place::Mix, vec![Probe::new(Op::Nan, 0.0)]);
            set_chain(&mut rig, Place::Output, vec![times(2.0)]);
            a.ring.write(&frames(ODD, PERIOD));
            rig.cycle();
            assert_eq!(
                rig.last_periods(1),
                frames(0, PERIOD),
                "{:?}",
                format_of::<P>()
            );
        }
        check::<F32>();
        check::<F64>();
    }

    fn no_allocation_with_chains<P: Processing>(other: Formats) {
        let mut rig = Rig::<P>::new(BitPerfect::Auto);
        let (mut a, mut b) = (client(1), client_at(2, 44100));
        let r = Resampler::new(44100, RATE, PERIOD as u32, Quality::High, format_of::<P>());
        b.source.as_mut().unwrap().resampler = Some(Box::new(r));
        rig.add(&mut a);
        rig.add(&mut b);
        set_chain(&mut rig, Place::Mix, vec![plus(0), times(0.5).only(other)]);
        set_chain(&mut rig, Place::Output, vec![times(2.0).only(other)]);
        rig.cycle();
        rig.events();
        let input = frames(1 << 24, PERIOD);
        // Everything the audio thread will be handed is made out here.
        rig.send(Command::SetGain(db_to_gain(-6.0)));
        rig.send(Command::SetParam {
            place: Place::Mix,
            plugin: 1,
            index: 0,
            value: 0.25,
        });
        rig.send(Command::SetChain(
            Place::Output,
            chain_of::<P>(vec![times(3.0).only(other).latency(8)]),
        ));
        rig.send(Command::SetChain(
            Place::Analog,
            chain_of::<P>(vec![plus(0).only(other)]),
        ));
        let engine = &mut rig.engine;
        assert_no_alloc(|| {
            for _ in 0..200 {
                a.ring.write(&input);
                b.ring.write(&input);
                engine.cycle().unwrap();
            }
        });
        // Two sources at -6 dB, a quarter, then three times. The resampled
        // source is near its level, not exactly at it.
        let want = f64::from(1 << 24) * f64::from(db_to_gain(-6.0)) * 2.0 * 0.25 * 3.0;
        let last = rig.log.frames().last().copied().unwrap();
        let error = (f64::from(last[0]) - want).abs() / want;
        assert!(
            error < 1e-3,
            "{last:?}, want {want} in {:?}",
            format_of::<P>()
        );
        // The returned chains are freed here, off the audio thread.
        assert_eq!(rig.events().len(), 2);
    }

    #[test]
    fn cycles_with_chains_do_not_allocate() {
        let only = |i32, f32, f64| Formats { i32, f32, f64 };
        no_allocation_with_chains::<I32>(only(false, false, true));
        no_allocation_with_chains::<F32>(only(true, false, false));
        no_allocation_with_chains::<F64>(only(false, true, false));
    }

    // The analog input.

    /// A rig with a capture handed over, not yet cycled, and an analog
    /// chain that always has signal and alters nothing.
    fn with_capture<P: Processing>(mode: BitPerfect) -> (Rig<P>, CaptureFeed) {
        let mut rig = Rig::<P>::new(mode);
        let (capture, feed) = MemoryCapture::new();
        rig.send(Command::SetCapture(Some(Box::new(capture))));
        set_chain(&mut rig, Place::Analog, vec![times(1.0).neutral()]);
        (rig, feed)
    }

    #[test]
    fn without_a_chain_the_analog_input_is_silent() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let (capture, feed) = MemoryCapture::new();
        rig.send(Command::SetCapture(Some(Box::new(capture))));
        let recorder = add_tap(&mut rig, 5);
        for _ in 0..3 {
            feed.push(&frames(1 << 28, PERIOD));
            rig.cycle();
        }
        // Nothing measures it, so nothing of it passes.
        assert_eq!(rig.last_periods(3), frames(0, 3 * PERIOD));
        assert_eq!(recorder.take(), frames(0, 3 * PERIOD));
        assert!(signals(&rig.events()).is_empty());
        assert_eq!(feed.queued(), 0);
    }

    /// Frames that differ from each other, with the low bits set.
    fn ramp(n: usize) -> Vec<[i32; 2]> {
        (0..n as i32).map(|i| [ODD + i, i32::MIN + 7 * i]).collect()
    }

    fn signals(events: &[Event]) -> Vec<bool> {
        let signal = |e: &Event| match e {
            Event::InputSignal(on) => Some(*on),
            _ => None,
        };
        events.iter().filter_map(signal).collect()
    }

    fn failures(events: &[Event]) -> usize {
        count(events, |e| matches!(e, Event::CaptureFailed(_)))
    }

    fn detect_chain<P: Processing>(hold_s: f64) -> Box<Chain> {
        let mut detect = Detect::new(RATE);
        detect.set_param(1, hold_s);
        let plugins: Vec<Box<dyn Plugin>> = vec![Box::new(detect)];
        Box::new(Chain::new(plugins, format_of::<P>(), MAX_PERIOD as usize))
    }

    #[test]
    fn a_captured_period_reaches_the_sink() {
        fn check<P: Processing>() {
            // Through the mix, at values every format holds exactly.
            let (mut rig, feed) = with_capture::<P>(BitPerfect::Off);
            let input: Vec<[i32; 2]> = (0..PERIOD as i32).map(|i| [i << 20, -i << 12]).collect();
            feed.push(&input);
            rig.cycle();
            assert_eq!(rig.last_periods(1), input, "{:?}", format_of::<P>());
            assert_eq!(feed.queued(), 0);
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn the_analog_gain_applies() {
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Auto);
        rig.send(Command::SetAnalogGain(0.5));
        feed.push(&frames(1000, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(500, PERIOD));
        assert_eq!(decide(&rig.engine.situation()), Err(NotBitPerfect::Volume));
    }

    #[test]
    fn the_analog_input_is_mixed_with_a_client() {
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(1000, PERIOD));
        feed.push(&frames(234, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(1234, PERIOD));
    }

    #[test]
    fn below_the_threshold_nothing_of_it_reaches_the_sink() {
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Off);
        // Reports no signal and leaves the samples, so only the engine
        // keeps them out.
        set_chain(&mut rig, Place::Analog, vec![times(1.0).quiet()]);
        for _ in 0..3 {
            feed.push(&frames(1000, PERIOD));
            rig.cycle();
            assert_eq!(rig.counters.analog_open.load(Relaxed), 0);
        }
        assert_eq!(rig.last_periods(3), frames(0, 3 * PERIOD));
        assert_eq!(feed.queued(), 0);
        assert!(signals(&rig.events()).is_empty());
    }

    #[test]
    fn below_the_threshold_one_client_stays_bit_exact() {
        let (mut rig, feed) = with_capture::<F32>(BitPerfect::Auto);
        set_chain(&mut rig, Place::Analog, vec![times(2.0).quiet()]);
        let mut a = client(1);
        rig.add(&mut a);
        let input = ramp(3 * PERIOD);
        a.ring.write(&input);
        for _ in 0..3 {
            feed.push(&frames(ODD, PERIOD));
            rig.cycle();
        }
        assert_eq!(decide(&rig.engine.situation()), Ok(()));
        assert_eq!(rig.last_periods(3), input);
    }

    #[test]
    fn above_the_threshold_a_client_is_no_longer_bit_perfect() {
        let (mut rig, feed) = with_capture::<F32>(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(ODD, PERIOD));
        feed.push(&frames(0, PERIOD));
        rig.cycle();
        assert_eq!(
            decide(&rig.engine.situation()),
            Err(NotBitPerfect::SeveralSources)
        );
        assert_eq!(rig.counters.analog_open.load(Relaxed), 1);
    }

    #[test]
    fn the_analog_input_alone_is_copied_bit_exact() {
        fn check<P: Processing>() {
            let (mut rig, feed) = with_capture::<P>(BitPerfect::Auto);
            set_chain(&mut rig, Place::Analog, vec![times(1.0).neutral()]);
            let input = ramp(3 * PERIOD);
            for period in input.chunks(PERIOD) {
                feed.push(period);
                rig.cycle();
            }
            assert_eq!(decide(&rig.engine.situation()), Ok(()));
            assert_eq!(rig.last_periods(3), input, "{:?}", format_of::<P>());
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_plugin_on_the_analog_input_changes_it_and_stops_bit_perfect() {
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Auto);
        set_chain(&mut rig, Place::Analog, vec![times(2.0)]);
        feed.push(&frames(1000, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(2000, PERIOD));
        assert_eq!(decide(&rig.engine.situation()), Err(NotBitPerfect::Plugin));
    }

    #[test]
    fn a_chain_that_raises_the_level_has_headroom_before_the_analog_gain() {
        fn check<P: Processing>(want: Vec<[i32; 2]>) {
            let (mut rig, feed) = with_capture::<P>(BitPerfect::Auto);
            set_chain(&mut rig, Place::Analog, vec![times(4.0)]);
            rig.send(Command::SetAnalogGain(0.25));
            feed.push(&frames(0x7000_0000, PERIOD));
            rig.cycle();
            assert_eq!(rig.last_periods(1), want, "{:?}", format_of::<P>());
        }
        check::<F32>(frames(0x7000_0000, PERIOD));
        check::<F64>(frames(0x7000_0000, PERIOD));
        // An i32 block is clipped before each plugin, as the design says.
        check::<I32>(frames(1 << 29, PERIOD));
    }

    #[test]
    fn forced_bit_perfect_plays_the_analog_input_as_captured_and_still_gates_it() {
        fn check<P: Processing>() {
            let (mut rig, feed) = with_capture::<P>(BitPerfect::Forced);
            let mut detect = Detect::new(RATE);
            detect.set_param(1, 0.0);
            let plugins: Vec<Box<dyn Plugin>> = vec![Box::new(times(4.0)), Box::new(detect)];
            let chain = Chain::new(plugins, format_of::<P>(), MAX_PERIOD as usize);
            rig.send(Command::SetChain(Place::Analog, Box::new(chain)));
            let recorder = add_tap(&mut rig, 5);
            // A loud ramp, with the low bits set, for two windows of 50 ms.
            let loud: Vec<[i32; 2]> = (0..75 * PERIOD as i32)
                .map(|i| [(1 << 28) + 7 * i + 1, -(1 << 28) - i])
                .collect();
            for period in loud.chunks(PERIOD) {
                feed.push(period);
                rig.cycle();
            }
            // Open from the end of the first window on, and as captured.
            let open = loud.len() - 30 * PERIOD;
            assert_eq!(rig.last_periods(30), loud[open..], "{:?}", format_of::<P>());
            // A recorder gets the input after its plugins all the same,
            // as near as `f32` holds it.
            let tapped = recorder.take();
            let tapped = &tapped[tapped.len() - 30 * PERIOD..];
            for (t, l) in tapped.iter().zip(&loud[open..]) {
                let error = (i64::from(t[0]) - 4 * i64::from(l[0])).abs();
                assert!(error <= 256, "{t:?} {l:?} {:?}", format_of::<P>());
            }
            // Below the threshold nothing of it plays, even times four,
            // once it has faded out.
            for _ in 0..100 {
                feed.push(&frames(1000, PERIOD));
                rig.cycle();
            }
            assert_eq!(rig.last_periods(30), frames(0, 30 * PERIOD));
            assert_eq!(signals(&rig.events()), [true, false]);
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn opening_and_closing_send_one_event_each() {
        let (mut rig, feed) = with_capture::<F32>(BitPerfect::Auto);
        rig.send(Command::SetChain(Place::Analog, detect_chain::<F32>(0.0)));
        let mut events = Vec::new();
        // A 50 ms window is 37.5 periods. Two windows of signal at -18 dBFS,
        // then two far below the threshold.
        for round in 0..150 {
            let value = if round < 75 { 1 << 28 } else { 1000 };
            feed.push(&frames(value, PERIOD));
            rig.cycle();
            events.extend(rig.events());
            let open = rig.counters.analog_open.load(Relaxed);
            assert_eq!(open, u64::from(signals(&events) == [true]), "{round}");
        }
        assert_eq!(signals(&events), [true, false]);
        // Nothing of the quiet input after the close reaches the sink.
        assert_eq!(rig.last_periods(30), frames(0, 30 * PERIOD));
    }

    /// The largest change from one frame to the next, on the left channel.
    fn largest_step(frames: &[[i32; 2]]) -> i64 {
        let step = |w: &[[i32; 2]]| (i64::from(w[1][0]) - i64::from(w[0][0])).abs();
        frames.windows(2).map(step).max().unwrap_or(0)
    }

    /// Plays a loud input through `analog`, a chain with a detector at
    /// -30 dBFS and no hold, until it opens, then an input just below the
    /// threshold until it has closed. Each period of the input is played
    /// in the cycle it is captured in. With `client`, a client playing
    /// zeros is mixed in. Returns what reached the sink, and where the
    /// input turned quiet.
    fn open_and_close<P: Processing>(
        mode: BitPerfect,
        analog: Vec<Box<dyn Plugin>>,
        client: bool,
    ) -> (Vec<[i32; 2]>, usize) {
        open_and_close_at::<P>(RATE, mode, analog, client)
    }

    /// `open_and_close` with the output at `rate`.
    fn open_and_close_at<P: Processing>(
        rate: u32,
        mode: BitPerfect,
        analog: Vec<Box<dyn Plugin>>,
        client: bool,
    ) -> (Vec<[i32; 2]>, usize) {
        let mut rig = Rig::<P>::new(mode);
        if rate != RATE {
            rig.send(Command::Reconfigure {
                rate,
                period: PERIOD as u32,
            });
            rig.cycle();
            rig.cycle();
        }
        // After the reopen, which closes a capture, as the control thread
        // does it.
        let (capture, feed) = MemoryCapture::new();
        rig.send(Command::SetCapture(Some(Box::new(capture))));
        let chain = Chain::new(analog, format_of::<P>(), MAX_PERIOD as usize);
        rig.send(Command::SetChain(Place::Analog, Box::new(chain)));
        let mut a = client.then(|| client_at(1, rate));
        if let Some(a) = a.as_mut() {
            rig.add(a);
        }
        // 200 ms of each at 48 kHz, as long at any rate.
        let half = (150 * rate as usize).div_ceil(RATE as usize);
        let mut out = Vec::new();
        for round in 0..2 * half {
            let value = if round < half { LOUD } else { QUIET };
            feed.push(&frames(value, PERIOD));
            if let Some(a) = &a {
                a.ring.write(&frames(0, PERIOD));
            }
            rig.cycle();
            out.extend(rig.last_periods(1));
        }
        (out, half * PERIOD)
    }

    /// -6 dBFS, and just below -30 dBFS.
    const LOUD: i32 = 1 << 30;
    const QUIET: i32 = 1 << 26;

    fn gate(with: Option<Probe>) -> Vec<Box<dyn Plugin>> {
        gate_at(RATE, with)
    }

    fn gate_at(rate: u32, with: Option<Probe>) -> Vec<Box<dyn Plugin>> {
        let mut detect = Detect::new(rate);
        detect.set_param(0, -30.0);
        detect.set_param(1, 0.0);
        let mut plugins: Vec<Box<dyn Plugin>> = Vec::new();
        if let Some(p) = with {
            plugins.push(Box::new(p));
        }
        plugins.push(Box::new(detect));
        plugins
    }

    /// No step is larger than a 10 ms ramp to the level allows, the first
    /// frame heard is near zero, and once closed the output is exact zeros.
    fn check_fades(out: &[[i32; 2]], quiet_at: usize, what: &str) {
        check_fades_at(RATE, out, quiet_at, what)
    }

    fn check_fades_at(rate: u32, out: &[[i32; 2]], quiet_at: usize, what: &str) {
        // One step of a 10 ms ramp to `level`, give or take the rounding
        // of a gain in `f32`.
        let ramp = |level: i32| f64::from(level) / f64::from(rate * FADE_MS / 1000) * 1.001 + 2.0;
        let first = out.iter().position(|f| f[0] != 0).expect(what);
        assert!(f64::from(out[first][0]) <= ramp(LOUD), "{what}");
        let opening = largest_step(&out[..quiet_at]);
        assert!(opening as f64 <= ramp(LOUD), "{what}: {opening}");
        // The input itself steps down at `quiet_at`.
        let closing = largest_step(&out[quiet_at..]);
        assert!(closing as f64 <= ramp(QUIET), "{what}: {closing}");
        assert!(out[out.len() - 10 * PERIOD..].iter().all(|f| *f == [0, 0]));
        // It did open, to the full level.
        assert!(out[..quiet_at].iter().any(|f| f[0] == LOUD), "{what}");
    }

    #[test]
    fn input_detection_fades_over_10_ms_at_44_1_and_192_khz() {
        for rate in [44100, 192000] {
            let auto = BitPerfect::Auto;
            let (out, at) = open_and_close_at::<F32>(rate, auto, gate_at(rate, None), false);
            check_fades_at(rate, &out, at, &format!("copy at {rate}"));
            let (out, at) = open_and_close_at::<F32>(rate, auto, gate_at(rate, None), true);
            check_fades_at(rate, &out, at, &format!("mix at {rate}"));
            let forced = BitPerfect::Forced;
            let gate = gate_at(rate, Some(times(1.0)));
            let (out, at) = open_and_close_at::<F32>(rate, forced, gate, false);
            check_fades_at(rate, &out, at, &format!("forced at {rate}"));
        }
    }

    #[test]
    fn the_copy_path_is_bit_exact_again_once_the_fade_is_over() {
        fn check<P: Processing>() {
            let (mut rig, feed) = with_capture::<P>(BitPerfect::Auto);
            let chain = Chain::new(gate(None), format_of::<P>(), MAX_PERIOD as usize);
            rig.send(Command::SetChain(Place::Analog, Box::new(chain)));
            // Loud, with the low bits set, through the open and its fade.
            let loud: Vec<[i32; 2]> = (0..75 * PERIOD as i32)
                .map(|i| [(1 << 28) + 7 * i + 1, -(1 << 28) - i])
                .collect();
            for period in loud.chunks(PERIOD) {
                feed.push(period);
                rig.cycle();
            }
            let n = loud.len();
            assert_eq!(decide(&rig.engine.situation()), Ok(()));
            let f = format_of::<P>();
            assert_eq!(rig.last_periods(20), loud[n - 20 * PERIOD..], "{f:?}");
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn input_detection_fades_in_and_out_on_every_path() {
        fn check<P: Processing>() {
            let f = format_of::<P>();
            // Alone, so copied when nothing fades.
            let (out, at) = open_and_close::<P>(BitPerfect::Auto, gate(None), false);
            check_fades(&out, at, &format!("copy {f:?}"));
            // Mixed with a client.
            let (out, at) = open_and_close::<P>(BitPerfect::Auto, gate(None), true);
            check_fades(&out, at, &format!("mix {f:?}"));
            // Past a plugin that alters it.
            let (out, at) = open_and_close::<P>(BitPerfect::Off, gate(Some(times(1.0))), false);
            check_fades(&out, at, &format!("plugin {f:?}"));
            // Forced bit-perfect, which plays it as captured past that
            // plugin, and with the detector alone.
            let forced = BitPerfect::Forced;
            let (out, at) = open_and_close::<P>(forced, gate(Some(times(1.0))), false);
            check_fades(&out, at, &format!("forced {f:?}"));
            let (out, at) = open_and_close::<P>(forced, gate(None), false);
            check_fades(&out, at, &format!("forced alone {f:?}"));
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_short_capture_is_padded_at_the_front_and_counted() {
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Auto);
        feed.push(&frames(ODD, PERIOD - 10));
        rig.cycle();
        let mut want = frames(0, 10);
        want.extend(frames(ODD, PERIOD - 10));
        assert_eq!(rig.last_periods(1), want);
        assert_eq!(rig.counters.analog_padded_frames.load(Relaxed), 10);
        assert_eq!(rig.counters.analog_dropped_frames.load(Relaxed), 0);
    }

    #[test]
    fn a_capture_more_than_two_periods_ahead_is_cut_back_and_counted() {
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Auto);
        for value in 1..=5 {
            feed.push(&frames(value, PERIOD));
        }
        rig.cycle();
        // Three periods are dropped, the fourth plays, the fifth waits.
        assert_eq!(rig.last_periods(1), frames(4, PERIOD));
        assert_eq!(feed.queued(), PERIOD);
        let dropped = rig.counters.analog_dropped_frames.load(Relaxed);
        assert_eq!(dropped, 3 * PERIOD as u64);
        assert_eq!(rig.counters.analog_padded_frames.load(Relaxed), 0);
        // Two periods ahead is not too many.
        feed.push(&frames(6, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(5, PERIOD));
        assert_eq!(rig.counters.analog_dropped_frames.load(Relaxed), dropped);
    }

    #[test]
    fn a_capture_that_fails_costs_only_the_analog_input() {
        let (mut rig, feed) = with_capture::<F32>(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(1 << 20, PERIOD));
        feed.push(&frames(1 << 20, 2 * PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(1 << 21, PERIOD));
        feed.fail();
        let input = ramp(4 * PERIOD);
        a.ring.write(&input);
        let mut events = rig.events();
        for _ in 0..4 {
            rig.cycle();
            events.extend(rig.events());
        }
        // The client plays on alone, untouched.
        assert_eq!(rig.last_periods(4), input);
        assert_eq!(decide(&rig.engine.situation()), Ok(()));
        assert_eq!(failures(&events), 1);
        assert_eq!(signals(&events), [true, false]);
        assert_eq!(rig.counters.analog_open.load(Relaxed), 0);
        // The audio thread does not close it: it hands it back, and the
        // control thread closes it.
        assert!(!feed.closed());
        let back = events.into_iter().find_map(|e| match e {
            Event::CaptureFailed(c) => Some(c),
            _ => None,
        });
        back.unwrap().close();
        assert!(feed.closed());
    }

    /// A sink that notes at each open whether the capture was closed by then.
    struct WatchesCapture {
        feed: CaptureFeed,
        closed_at_open: Arc<std::sync::Mutex<Vec<bool>>>,
    }

    impl Sink for WatchesCapture {
        fn open(&mut self, _rate: u32, _period: u32) -> Result<(), SinkError> {
            self.closed_at_open.lock().unwrap().push(self.feed.closed());
            Ok(())
        }

        fn write(&mut self, _frames: &[[i32; 2]]) -> Result<(), SinkError> {
            Ok(())
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
    fn the_capture_is_closed_before_the_output_reopens() {
        let (capture, feed) = MemoryCapture::new();
        let closed_at_open = Arc::default();
        let sink = WatchesCapture {
            feed: feed.clone(),
            closed_at_open: Arc::clone(&closed_at_open),
        };
        let (mut commands, consumer) = RingBuffer::new(16);
        let (producer, mut events) = RingBuffer::new(16);
        let counters = Arc::new(Counters::default());
        let mut engine =
            Engine::<F32, _>::new(sink, consumer, producer, counters, BitPerfect::Auto);
        let mut send = |c| assert!(commands.push(c).is_ok());
        send(Command::Reconfigure {
            rate: RATE,
            period: PERIOD as u32,
        });
        engine.cycle().unwrap();
        // As the control thread does, after the output is open.
        send(Command::SetChain(
            Place::Analog,
            chain_of::<F32>(vec![times(1.0).neutral()]),
        ));
        send(Command::SetCapture(Some(Box::new(capture))));
        feed.push(&frames(1000, PERIOD));
        engine.cycle().unwrap();
        send(Command::Reconfigure {
            rate: 96000,
            period: PERIOD as u32,
        });
        // The fade, which still reads the capture.
        engine.cycle().unwrap();
        assert!(!feed.closed());
        engine.cycle().unwrap();
        assert_eq!(*closed_at_open.lock().unwrap(), [false, true]);
        // A closed capture is not read, and costs no failure.
        feed.push(&frames(1000, 3 * PERIOD));
        for _ in 0..3 {
            engine.cycle().unwrap();
        }
        assert_eq!(feed.queued(), 3 * PERIOD);
        let events: Vec<Event> = std::iter::from_fn(|| events.pop().ok()).collect();
        assert_eq!(signals(&events), [true, false]);
        assert_eq!(failures(&events), 0);
    }

    #[test]
    fn a_dop_stream_silences_the_analog_input() {
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Auto);
        rig.send(Command::SetChain(Place::Analog, detect_chain::<I32>(3.0)));
        let mut a = client(1);
        rig.add(&mut a);
        let dop = dop_frames(40 * PERIOD);
        a.ring.write(&dop);
        for _ in 0..40 {
            feed.push(&frames(1 << 28, PERIOD));
            rig.cycle();
        }
        assert_eq!(rig.last_periods(40), dop);
        // The detector kept measuring all along.
        assert_eq!(feed.queued(), 0);
        assert_eq!(signals(&rig.events()), [true]);
    }

    #[test]
    fn the_fade_out_before_a_reopen_fades_the_analog_input_too() {
        let (mut rig, feed) = with_capture::<F64>(BitPerfect::Auto);
        feed.push(&frames(1 << 24, 2 * PERIOD));
        rig.cycle();
        rig.send(Command::Reconfigure {
            rate: 96000,
            period: 128,
        });
        rig.cycle();
        let got = rig.last_periods(1);
        assert_eq!(got[0][0], 1 << 24);
        for (i, f) in got.iter().enumerate() {
            let want = (1.0 - i as f64 / PERIOD as f64) * (1 << 24) as f64;
            assert!((f64::from(f[0]) - want).abs() < 2.0, "frame {i}");
        }
        assert_eq!(rig.log.opens().len(), 1);
    }

    #[test]
    fn a_full_event_queue_delays_the_input_signal_without_losing_it() {
        let mut rig = Rig::<I32>::with_event_slots(BitPerfect::Auto, 1);
        set_chain(&mut rig, Place::Analog, vec![times(1.0).neutral()]);
        rig.cycle();
        assert!(matches!(rig.events.pop(), Ok(Event::OldChain(_))));
        let (capture, feed) = MemoryCapture::new();
        rig.send(Command::SetCapture(Some(Box::new(capture))));
        feed.push(&frames(1, PERIOD));
        rig.cycle();
        assert!(matches!(rig.events.pop(), Ok(Event::OldCapture(None))));
        assert!(rig.events.pop().is_err());
        assert_eq!(rig.counters.analog_open.load(Relaxed), 1);
        feed.fail();
        rig.cycle();
        assert!(matches!(rig.events.pop(), Ok(Event::CaptureFailed(_))));
        rig.cycle();
        // It opened and closed while the queue was full. That is no change.
        assert!(rig.events.pop().is_err());
        assert_eq!(rig.counters.analog_open.load(Relaxed), 0);
    }

    #[test]
    fn a_failed_capture_still_held_is_closed_before_the_output_reopens() {
        let mut rig = Rig::<I32>::with_event_slots(BitPerfect::Auto, 1);
        set_chain(&mut rig, Place::Analog, vec![times(1.0).neutral()]);
        let (capture, feed) = MemoryCapture::new();
        rig.send(Command::SetCapture(Some(Box::new(capture))));
        feed.push(&frames(1, PERIOD));
        // The queue stays full, so the failed capture is not handed back.
        rig.cycle();
        feed.fail();
        rig.cycle();
        assert!(!feed.closed());
        rig.send(Command::Reconfigure {
            rate: 96000,
            period: PERIOD as u32,
        });
        rig.cycle();
        rig.cycle();
        // The card may refuse a second open while it is held.
        assert!(feed.closed());
    }

    #[test]
    fn a_replaced_capture_comes_back() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let (first, first_feed) = MemoryCapture::new();
        let (second, second_feed) = MemoryCapture::new();
        rig.send(Command::SetCapture(Some(Box::new(first))));
        rig.send(Command::SetCapture(Some(Box::new(second))));
        rig.send(Command::SetCapture(None));
        rig.cycle();
        let back: Vec<bool> = rig
            .events()
            .into_iter()
            .filter_map(|e| match e {
                Event::OldCapture(c) => Some(c.is_some()),
                _ => None,
            })
            .collect();
        assert_eq!(back, [false, true, true]);
        // Neither is read any more, nor closed by the engine.
        first_feed.push(&frames(1, PERIOD));
        second_feed.push(&frames(1, PERIOD));
        rig.cycle();
        assert_eq!(first_feed.queued() + second_feed.queued(), 2 * PERIOD);
        assert!(!first_feed.closed() && !second_feed.closed());
    }

    #[test]
    fn the_summary_names_the_analog_input_once_a_capture_was_set() {
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Auto);
        let before = rig.counters.to_string();
        assert!(!before.contains("analog"), "{before}");
        feed.push(&frames(1, PERIOD - 3));
        rig.cycle();
        let after = rig.counters.to_string();
        assert!(before.ends_with(" cycles"), "{before}");
        assert!(
            after.ends_with("; analog input open, 3 frames padded, 0 frames dropped"),
            "{after}"
        );
    }

    fn no_allocation_with_a_capture<P: Processing>(other: Formats) {
        let (mut rig, feed) = with_capture::<P>(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        // A plugin to convert around, and a real detector that opens and
        // closes during the checked cycles.
        let mut detect = Detect::new(RATE);
        detect.set_param(1, 0.0);
        let plugins: Vec<Box<dyn Plugin>> =
            vec![Box::new(times(1.0).only(other)), Box::new(detect)];
        let chain = Chain::new(plugins, format_of::<P>(), MAX_PERIOD as usize);
        rig.send(Command::SetChain(Place::Analog, Box::new(chain)));
        rig.cycle();
        rig.events();
        let (spare, spare_feed) = MemoryCapture::new();
        let input = frames(1 << 24, PERIOD);
        let loud = frames(1 << 28, PERIOD);
        let quiet = frames(1000, PERIOD);
        // Everything the audio thread will be handed is made out here.
        rig.send(Command::SetAnalogGain(0.5));
        let mut spare = Some(Box::new(spare) as Box<dyn Capture>);
        let engine = &mut rig.engine;
        let commands = &mut rig.commands;
        assert_no_alloc(|| {
            for round in 0..300 {
                a.ring.write(&input);
                // Above the threshold for a window and more, then below it
                // for two.
                let level = if round < 60 { &loud } else { &quiet };
                // Short, then on time, then too much.
                let n = [PERIOD / 2, PERIOD, 4 * PERIOD][round % 3];
                feed.push(&level[..n.min(PERIOD)]);
                if n > PERIOD {
                    feed.push(level);
                    feed.push(level);
                    feed.push(level);
                }
                if round == 140 {
                    feed.fail();
                }
                if round == 160 {
                    let _ = commands.push(Command::SetCapture(spare.take()));
                }
                if round == 240 {
                    let _ = commands.push(Command::Reconfigure {
                        rate: RATE,
                        period: PERIOD as u32,
                    });
                }
                spare_feed.push(&loud);
                engine.cycle().unwrap();
            }
        });
        let counters = &rig.counters;
        assert!(counters.analog_padded_frames.load(Relaxed) > 0);
        assert!(counters.analog_dropped_frames.load(Relaxed) > 0);
        assert!(spare_feed.closed(), "{:?}", format_of::<P>());
        // The returned items are freed here, off the audio thread.
        let events = rig.events();
        // Opened and closed by the detector, opened again by the spare,
        // closed for the reopen.
        assert_eq!(signals(&events), [true, false, true, false]);
        assert_eq!(failures(&events), 1);
        assert_eq!(count(&events, |e| matches!(e, Event::OldCapture(_))), 1);
    }

    #[test]
    fn cycles_with_a_capture_do_not_allocate() {
        let only = |i32, f32, f64| Formats { i32, f32, f64 };
        no_allocation_with_a_capture::<I32>(only(false, false, true));
        no_allocation_with_a_capture::<F32>(only(true, false, false));
        no_allocation_with_a_capture::<F64>(only(false, true, false));
    }

    // Capture streams.

    /// A recorder's side of a tap: its own mapping of the ring, and the
    /// wake-up.
    struct Recorder {
        id: ClientId,
        ring: Ring,
        wake: OwnedFd,
    }

    impl Recorder {
        /// Everything the ring holds.
        fn take(&self) -> Vec<[i32; 2]> {
            let mut out = vec![[0; 2]; self.ring.capacity()];
            let n = self.ring.read(&mut out);
            out.truncate(n);
            out
        }

        fn overruns(&self) -> u64 {
            self.ring.header().underruns.load(Relaxed)
        }

        /// How often the wake-up was written since it was last read.
        fn wake_ups(&self) -> u64 {
            let mut count = 0u64;
            // SAFETY: 8 bytes from a non-blocking eventfd.
            unsafe { libc::read(self.wake.as_raw_fd(), (&raw mut count).cast(), 8) };
            count
        }
    }

    /// A stopped tap with a ring of `capacity` frames, and its recorder.
    fn tap_of(id: ClientId, capacity: u32) -> (Recorder, Box<Tap>) {
        let (ring, fd) = Ring::create(capacity).unwrap();
        let peer = Ring::map(fd.as_fd()).unwrap();
        let wake = eventfd();
        let theirs = wake.try_clone().unwrap();
        let recorder = Recorder {
            id,
            ring: peer,
            wake,
        };
        (recorder, Box::new(Tap::new(id, ring, theirs)))
    }

    /// A running tap with room for many periods.
    fn add_tap<P: Processing>(rig: &mut Rig<P>, id: ClientId) -> Recorder {
        let (recorder, tap) = tap_of(id, 1 << 16);
        rig.send(Command::AddTap(tap));
        rig.send(Command::SetRunning(id, true));
        recorder
    }

    #[test]
    fn a_tap_gets_the_analog_input_after_its_chain() {
        fn check<P: Processing>() {
            let (mut rig, feed) = with_capture::<P>(BitPerfect::Auto);
            set_chain(&mut rig, Place::Analog, vec![times(2.0)]);
            let recorder = add_tap(&mut rig, 5);
            feed.push(&frames(1000, PERIOD));
            rig.cycle();
            assert_eq!(
                recorder.take(),
                frames(2000, PERIOD),
                "{:?}",
                format_of::<P>()
            );
            // Clipped to 32 bits as the mix's last step does it.
            set_chain(&mut rig, Place::Analog, vec![times(4.0)]);
            feed.push(&frames(0x4000_0000, PERIOD));
            rig.cycle();
            assert_eq!(recorder.take(), vec![[i32::MAX, i32::MIN]; PERIOD]);
            // A neutral chain hands over the frames as captured.
            set_chain(&mut rig, Place::Analog, vec![times(1.0).neutral()]);
            let input = ramp(3 * PERIOD);
            for period in input.chunks(PERIOD) {
                feed.push(period);
                rig.cycle();
            }
            assert_eq!(recorder.take(), input, "{:?}", format_of::<P>());
            // The analog gain is the mix's, and does not apply.
            rig.send(Command::SetAnalogGain(0.5));
            feed.push(&input[..PERIOD]);
            rig.cycle();
            assert_eq!(recorder.take(), input[..PERIOD]);
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn the_analog_chains_latency_is_reported_to_recorders_and_in_the_counters() {
        let (mut rig, feed) = with_capture::<F32>(BitPerfect::Auto);
        set_chain(&mut rig, Place::Analog, vec![times(2.0).latency(8)]);
        let recorder = add_tap(&mut rig, 5);
        feed.push(&frames(1000, PERIOD));
        rig.cycle();
        let header = recorder.ring.header();
        assert_eq!(header.output_delay_frames.load(Relaxed), 8);
        assert_eq!(rig.counters.analog_latency_frames.load(Relaxed), 8);
        let summary = rig.counters.to_string();
        assert!(summary.ends_with(", plugin latency 8 frames"), "{summary}");
        // A chain whose output is not used delays nothing.
        set_chain(&mut rig, Place::Analog, vec![times(1.0).neutral()]);
        feed.push(&frames(1000, PERIOD));
        rig.cycle();
        assert_eq!(header.output_delay_frames.load(Relaxed), 0);
        assert_eq!(rig.counters.analog_latency_frames.load(Relaxed), 0);
    }

    #[test]
    fn a_tap_gets_silence_while_there_is_no_signal() {
        // No capture at all: a period of zeros every cycle, in real time.
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let recorder = add_tap(&mut rig, 5);
        for _ in 0..3 {
            rig.cycle();
        }
        assert_eq!(recorder.take(), frames(0, 3 * PERIOD));
        // Below the threshold.
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Auto);
        set_chain(&mut rig, Place::Analog, vec![times(1.0).quiet()]);
        let recorder = add_tap(&mut rig, 5);
        for _ in 0..3 {
            feed.push(&frames(1000, PERIOD));
            rig.cycle();
        }
        assert_eq!(recorder.take(), frames(0, 3 * PERIOD));
        // A capture that has failed.
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Auto);
        let recorder = add_tap(&mut rig, 5);
        feed.push(&frames(1000, PERIOD));
        rig.cycle();
        feed.fail();
        rig.cycle();
        rig.cycle();
        let mut want = frames(1000, PERIOD);
        want.extend(frames(0, 2 * PERIOD));
        assert_eq!(recorder.take(), want);
    }

    #[test]
    fn a_tap_never_gets_the_mix() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let recorder = add_tap(&mut rig, 5);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(ODD, 3 * PERIOD));
        for _ in 0..3 {
            rig.cycle();
        }
        assert_eq!(rig.last_periods(3), frames(ODD, 3 * PERIOD));
        assert_eq!(recorder.take(), frames(0, 3 * PERIOD));
        // With an input, the tap gets the input alone, the card both.
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Auto);
        let recorder = add_tap(&mut rig, 5);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(1000, PERIOD));
        feed.push(&frames(234, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(1234, PERIOD));
        assert_eq!(recorder.take(), frames(234, PERIOD));
        // While DoP holds the output, the input is still measured and taken.
        let mut d = client(2);
        rig.add(&mut d);
        let dop = dop_frames(4 * PERIOD);
        d.ring.write(&dop);
        for _ in 0..4 {
            feed.push(&frames(234, PERIOD));
            rig.cycle();
        }
        assert_eq!(rig.last_periods(4), dop);
        assert_eq!(recorder.take(), frames(234, 4 * PERIOD));
    }

    #[test]
    fn a_recorder_that_stops_reading_loses_only_its_own_audio() {
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Auto);
        let capacity = 2 * PERIOD + 10;
        let (stalled, tap) = tap_of(5, capacity as u32);
        rig.send(Command::AddTap(tap));
        rig.send(Command::SetRunning(5, true));
        let reading = add_tap(&mut rig, 6);
        let mut a = client(1);
        rig.add(&mut a);
        let input = ramp(10 * PERIOD);
        a.ring.write(&frames(1000, 10 * PERIOD));
        let mut got = Vec::new();
        for period in input.chunks(PERIOD) {
            feed.push(period);
            rig.cycle();
            got.extend(reading.take());
        }
        // The full ring kept the oldest frames, and counts the rest.
        assert_eq!(stalled.take(), input[..capacity]);
        assert_eq!(stalled.overruns(), (10 * PERIOD - capacity) as u64);
        // The other recorder and the card lost nothing.
        assert_eq!(got, input);
        assert_eq!(reading.overruns(), 0);
        assert_eq!(underruns(&a), 0);
        assert_eq!(a.ring.available(), 0);
        assert_eq!(rig.log.frames().len(), PERIOD + 10 * PERIOD);
    }

    #[test]
    fn a_stopped_tap_is_not_written() {
        let (mut rig, feed) = with_capture::<I32>(BitPerfect::Auto);
        let (recorder, tap) = tap_of(5, 1 << 16);
        rig.send(Command::AddTap(tap));
        feed.push(&frames(1, PERIOD));
        rig.cycle();
        assert!(recorder.take().is_empty());
        assert_eq!(recorder.wake_ups(), 0);
        rig.send(Command::SetRunning(5, true));
        feed.push(&frames(2, PERIOD));
        rig.cycle();
        assert_eq!(recorder.take(), frames(2, PERIOD));
        rig.send(Command::SetRunning(5, false));
        feed.push(&frames(3, PERIOD));
        rig.cycle();
        assert!(recorder.take().is_empty());
        // A flush stops it, and is answered. The ring is the reader's to
        // empty, so the engine leaves it alone.
        rig.send(Command::SetRunning(5, true));
        feed.push(&frames(4, PERIOD));
        rig.cycle();
        rig.events();
        rig.send(Command::Flush(5));
        feed.push(&frames(5, PERIOD));
        rig.cycle();
        let events = rig.events();
        assert!(matches!(events[..], [Event::Flushed(5)]));
        assert_eq!(recorder.take(), frames(4, PERIOD));
        assert_eq!(
            recorder.ring.header().read_pos.load(Relaxed),
            2 * PERIOD as u64
        );
    }

    #[test]
    fn a_tap_waiting_for_a_position_is_woken_once_when_it_is_reached() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let recorder = add_tap(&mut rig, 5);
        // Zero asks for a wake-up after every period.
        rig.cycle();
        rig.cycle();
        assert_eq!(recorder.wake_ups(), 2);
        let header = recorder.ring.header();
        header.wake_at.store(5 * PERIOD as u64, Ordering::SeqCst);
        for _ in 0..2 {
            rig.cycle();
            assert_eq!(recorder.wake_ups(), 0);
        }
        rig.cycle();
        assert_eq!(recorder.wake_ups(), 1);
        rig.cycle();
        assert_eq!(recorder.wake_ups(), 0);
        // A full ring wakes it as an underrun wakes a player.
        let (stalled, tap) = tap_of(6, PERIOD as u32);
        rig.send(Command::AddTap(tap));
        rig.send(Command::SetRunning(6, true));
        stalled
            .ring
            .header()
            .wake_at
            .store(1 << 40, Ordering::SeqCst);
        rig.cycle();
        assert_eq!(stalled.wake_ups(), 0);
        rig.cycle();
        assert_eq!(stalled.wake_ups(), 1);
        assert_eq!(stalled.overruns(), PERIOD as u64);
    }

    #[test]
    fn a_removed_or_refused_tap_comes_back() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let recorders: Vec<Recorder> = (0..INITIAL_TAPS as u64 + 1)
            .map(|id| {
                let (recorder, tap) = tap_of(id, 1024);
                rig.send(Command::AddTap(tap));
                recorder
            })
            .collect();
        rig.send(Command::RemoveTap(2));
        rig.send(Command::RemoveTap(99));
        rig.cycle();
        let back: Vec<ClientId> = rig
            .events()
            .iter()
            .filter_map(|e| match e {
                Event::DroppedTap(t) => Some(t.id),
                _ => None,
            })
            .collect();
        assert_eq!(back, [INITIAL_TAPS as u64, 2]);
        // A bigger table keeps every tap, and the old one comes back.
        rig.send(Command::GrowTaps(Vec::with_capacity(2 * INITIAL_TAPS)));
        for id in [0, 1, 3] {
            rig.send(Command::SetRunning(id, true));
        }
        rig.cycle();
        let events = rig.events();
        assert!(matches!(&events[..], [Event::OldTaps(t)] if t.is_empty()));
        for r in &recorders {
            let want = if [0, 1, 3].contains(&r.id) { PERIOD } else { 0 };
            assert_eq!(r.take().len(), want, "tap {}", r.id);
        }
    }

    fn taps_without_allocation<P: Processing>() {
        let (mut rig, feed) = with_capture::<P>(BitPerfect::Auto);
        // Converted from the processing format.
        set_chain(&mut rig, Place::Analog, vec![times(2.0)]);
        let mut a = client(1);
        rig.add(&mut a);
        let reading = add_tap(&mut rig, 5);
        let (full, tap) = tap_of(6, PERIOD as u32);
        rig.send(Command::AddTap(tap));
        rig.send(Command::SetRunning(6, true));
        rig.cycle();
        rig.events();
        let (late, tap) = tap_of(7, 1 << 12);
        let mut late_tap = Some(tap);
        let mut grown = Some(Vec::with_capacity(4 * INITIAL_TAPS));
        let input = frames(1 << 20, PERIOD);
        let mut out = vec![[0; 2]; 4 * PERIOD];
        let engine = &mut rig.engine;
        let commands = &mut rig.commands;
        assert_no_alloc(|| {
            for round in 0..200 {
                a.ring.write(&input);
                feed.push(&input);
                match round {
                    20 => {
                        let _ = commands.push(Command::GrowTaps(grown.take().unwrap()));
                        let _ = commands.push(Command::AddTap(late_tap.take().unwrap()));
                        let _ = commands.push(Command::SetRunning(7, true));
                    }
                    60 => {
                        let _ = commands.push(Command::Flush(5));
                    }
                    80 => {
                        let _ = commands.push(Command::SetRunning(5, true));
                    }
                    100 => {
                        let _ = commands.push(Command::RemoveTap(6));
                    }
                    150 => feed.fail(),
                    _ => {}
                }
                engine.cycle().unwrap();
                reading.ring.read(&mut out);
                late.ring.read(&mut out);
            }
        });
        assert!(full.overruns() > 0);
        assert_eq!(reading.overruns(), 0);
        // The returned items are freed here, off the audio thread.
        let events = rig.events();
        assert_eq!(count(&events, |e| matches!(e, Event::OldTaps(_))), 1);
        assert_eq!(count(&events, |e| matches!(e, Event::DroppedTap(_))), 1);
    }

    #[test]
    fn taps_do_not_allocate() {
        taps_without_allocation::<I32>();
        taps_without_allocation::<F32>();
        taps_without_allocation::<F64>();
    }
}
