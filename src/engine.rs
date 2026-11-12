//! The audio thread's work: one period per `cycle`.
//!
//! The control thread owns every decision and sends them as commands. This
//! code only carries them out. It does not allocate, free or lock. Whatever
//! it would free goes back to the control thread in an `Event`.
//!
//! The engine plays to the master card, whose wait is the only one, and
//! feeds the further outputs. Each cycle reads the sources once into the
//! `players` and `analog` buses, and every output mixes them through its
//! own [`OutputMix`]. A further output's period goes into its [`Feed`]; the
//! master's goes through its delay line to the card.

use crate::bitperfect::{Situation, decide, plugins_apply, volumes_apply};
use crate::capture::Capture;
use crate::config::{BitPerfect, MAX_OUTPUTS};
use crate::counters::Counters;
use crate::dop;
use crate::format::{BlockStore, Processing, Ramp, mix_add, mix_add_acc, mix_clear, mix_finish};
use crate::further::Feed;
use crate::latency::{MAX_PERIOD, PERIODS};
use crate::matrix::{Bus, MAX_CHANNELS, Matrix};
use crate::output_mix::{OutputMix, Routing};
use crate::plugin::chain::Chain;
use crate::rate::ClientId;
use crate::resample::Resampler;
use crate::sink::{Sink, SinkError};
use aas_client::ring::Ring;
use rtrb::{Consumer, Producer, PushError};
use std::any::Any;
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
pub(crate) const FADE_MS: u32 = 10;

/// How long a DoP stream that has run dry keeps the output.
const DOP_GRACE_MS: u32 = 100;

/// Most input frames a resampler is expected to want per output frame. The
/// scan looks that far ahead so that it can judge everything about to be read.
const MAX_DOWNSAMPLING: usize = 8;

/// The longest delay line the master can have, in frames.
pub const MAX_DELAY: u32 = 8192;

/// The outputs there can be besides the master.
const FURTHER: usize = MAX_OUTPUTS - 1;

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
    /// runs at its rate. Nor can it be scaled or summed: while `routed` is
    /// false, because the matrix or the chains would alter it, it is silent
    /// the same way.
    fn muted(&self, output_rate: u32, routed: bool) -> bool {
        self.stream == Stream::Dop && (self.rate != output_rate || !routed)
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
    /// dry keeps it for the grace time, unless it has ended. One that is
    /// not `routed` never holds it.
    fn holds_output(&self, output_rate: u32, routed: bool) -> bool {
        let dry_for_now = !self.stalled && !self.draining();
        self.running
            && self.stream == Stream::Dop
            && self.rate == output_rate
            && routed
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
        let delay = (u64::from(sink_delay) + u64::from(filter)) * u64::from(self.rate)
            / u64::from(output_rate);
        let header = self.ring.header();
        header
            .output_delay_frames
            .store(delay.min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
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
    /// A period passed without the chain running, such as with no capture:
    /// it is reset before it runs again.
    chain_idle: bool,
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
            chain_idle: false,
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
        // Until the chain runs below, this period passes it by.
        let idle = std::mem::replace(&mut self.chain_idle, true);
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
        let mut block = P::to_block(processed.as_flattened_mut(), 2, store);
        if idle {
            self.chain.reset();
        }
        self.chain_idle = false;
        let status = self.chain.process(block.reborrow());
        let signal = !status.no_signal;
        let neutral = self.chain.neutral();
        // A neutral chain changed nothing unless the detector faded or
        // zeroed, so the frames stay as captured and can be copied
        // bit-perfect.
        if !neutral || status.changed {
            P::from_block(block, processed.as_flattened_mut());
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

/// Writes stereo `frames` to `out`, interleaved frames of `channels`
/// samples, unaltered: each output channel gets the channel `routing`
/// names, and zeros where it names none.
fn place_routed(frames: &[[i32; 2]], routing: &Routing, channels: usize, out: &mut [i32]) {
    for (o, f) in out.chunks_exact_mut(channels).zip(frames) {
        for (v, side) in o
            .iter_mut()
            .zip(routing.iter().chain(std::iter::repeat(&None)))
        {
            *v = side.map_or(0, |side| f.get(usize::from(side)).copied().unwrap_or(0));
        }
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
    /// On the mix of all sources through the matrix, after the output
    /// volume. It takes as many channels as the matrix is wide.
    Mix,
    /// On what goes to the card, after the mix chain.
    Output,
}

/// Which output a chain is for, 0 being the master, and where on it. Only
/// the master has the analog input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub output: usize,
    pub place: Place,
}

impl Target {
    /// `place` on the master.
    pub fn master(place: Place) -> Target {
        Target { output: 0, place }
    }
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
    /// The gain of an output, 0 being the master. Fades over 10 ms from
    /// the current level. One for an output that is not there is ignored.
    SetGain(usize, f32),
    /// An empty table with more capacity. It replaces the current one.
    #[allow(clippy::vec_box)] // Moving a boxed source moves only a pointer.
    Grow(Vec<Box<Source>>),
    /// Replaces the chain at a place of an output. Answered by
    /// `Event::OldChain`. One for an output that is not there, and one for
    /// the analog input of another output than the master, is refused. The
    /// chain is in the engine's processing format and, if it has a plugin,
    /// built for `MAX_PERIOD` frames, so that no period change can outgrow
    /// it. A shorter one is refused: it comes back as the `OldChain` and the
    /// chain in place stays. So is one built for other channel counts: an
    /// analog chain keeps two throughout, a `mix` chain takes as many as the
    /// matrix is wide, an `output` chain takes what the `mix` chain gives,
    /// and with both in place they end at the sink's count. An empty chain
    /// fits any count. An empty `output` chain in place does not hold back
    /// a `mix` chain, since an `output` chain that brings the count back to
    /// the sink's follows it; until then the output is silent. The control thread sends the chains for a new
    /// rate just before its `Reconfigure`, so that they take effect with the
    /// reopen, as resamplers do. In forced bit-perfect the mix and output
    /// chains are bypassed, and the analog chain runs only for its
    /// detector: its output is not used.
    SetChain(Target, Box<Chain>),
    /// Sets a parameter of one plugin in a chain. One that is not there is
    /// ignored.
    SetParam {
        target: Target,
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
    /// Replaces the matrix of an output, 0 being the master, each gain
    /// moving to its new value over 10 ms. Answered by `Event::OldMatrix`
    /// with the one it replaced. One of another width than the matrix in
    /// place is sent back as the `OldMatrix`, and the one in place stays:
    /// the buffers and the chains are built for its width. So is one for
    /// an output that is not there.
    SetMatrix(usize, Box<Matrix>),
    /// Puts further output `index`, 1 to 3, in place: its mix, an
    /// `OutputMix` in the engine's processing format, and the feed of its
    /// writer. It replaces the one there, which comes back as
    /// `Event::OldOutput`. So does what this brings if the mix is in
    /// another format, if it is for another channel count than the feed,
    /// if its buffers or its chains are built for a shorter period than
    /// `MAX_PERIOD`, or if its chains do not fit, or for an index that is
    /// not a further output's: it is never played. The mix plays at the
    /// gain set in it and at the master's rate, and is fed a master period
    /// a cycle from the next one on.
    AddOutput(usize, Box<dyn Any + Send>, Feed),
    /// Takes further output `index` out. Its mix and its feed come back as
    /// `Event::OldOutput`. One that is not there is ignored.
    RemoveOutput(usize),
    /// The master's delay line, in frames: what goes to the master's card
    /// is held back by exactly that many, bit for bit, the copy path and
    /// DoP included. At most the capacity the engine was made with, more
    /// is taken as that. While something is heard, or the line still
    /// holds sound, the master fades out over 10 ms, the delay changes,
    /// and it fades in again: the frames in the line at the change are
    /// lost. The fades alter what the copy path gives for those 10 ms.
    /// While a DoP stream holds the output it changes at once, as a reopen
    /// would cut the stream.
    SetDelay(u32),
    Shutdown,
}

/// What the audio thread hands back or has noticed.
pub enum Event {
    Dropped(Box<Source>),
    Replaced(Option<Box<Resampler>>),
    OldTable(Vec<Box<Source>>),
    /// The chain a `SetChain` replaced, or the one it brought if that was
    /// refused, with the output it was for, to be freed off the audio
    /// thread.
    OldChain(usize, Box<Chain>),
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
    /// The matrix a `SetMatrix` replaced, or the one it brought if that
    /// was refused, with the output it was for, to be freed off the audio
    /// thread.
    OldMatrix(usize, Box<Matrix>),
    /// A further output's mix and feed that `AddOutput` replaced or
    /// refused, or that `RemoveOutput` took out, with the output's index,
    /// to be dropped off the audio thread.
    OldOutput(usize, Box<dyn Any + Send>, Feed),
}

/// A further output, as the engine holds it.
struct Further<P: Processing> {
    mix: Box<OutputMix<P>>,
    feed: Feed,
}

/// How the master's delay line fades around a change of its length.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Moving {
    Still,
    /// Fading out, after the line: frames of the fade left.
    Down(usize),
    /// Fading in, before the line: frames of the fade left.
    Up(usize),
}

/// The master's delay line: holds what goes to the card back by a whole
/// number of frames, and changes that number between a fade out and a fade
/// in. Made with its full capacity; nothing here allocates.
struct DelayLine {
    /// The frames in the line, oldest first from `pos`, within
    /// `ring[..frames * channels]`.
    ring: Vec<i32>,
    channels: usize,
    frames: usize,
    /// Where the next sample goes in and the oldest comes out.
    pos: usize,
    /// A length waiting for the fade out to end.
    pending: Option<usize>,
    moving: Moving,
    /// Frames of a fade.
    fade: usize,
}

impl DelayLine {
    fn new(capacity: usize, channels: usize) -> DelayLine {
        DelayLine {
            ring: vec![0; capacity * channels],
            channels,
            frames: 0,
            pos: 0,
            pending: None,
            moving: Moving::Still,
            fade: 1,
        }
    }

    fn capacity(&self) -> usize {
        self.ring.len() / self.channels
    }

    /// Whether anything audible is still in the line. Reads all of it, up to
    /// `MAX_DELAY` frames, only for a `SetDelay` or a queued close.
    fn holds_sound(&self) -> bool {
        self.ring[..self.frames * self.channels]
            .iter()
            .any(|&s| s != 0)
    }

    /// Changes the length to `frames`, at most the capacity: after a fade
    /// out over `fade` frames and with a fade in after it, or at once for
    /// `None`.
    fn set(&mut self, frames: u32, fade: Option<usize>) {
        let frames = (frames as usize).min(self.capacity());
        if frames == self.pending.unwrap_or(self.frames) {
            return;
        }
        let Some(fade) = fade else {
            self.pending = Some(frames);
            self.settle();
            return;
        };
        self.pending = Some(frames);
        self.moving = match self.moving {
            Moving::Down(left) => Moving::Down(left),
            // From where the fade in is.
            Moving::Up(left) => Moving::Down(self.fade.saturating_sub(left)),
            Moving::Still => Moving::Down(fade),
        };
        self.fade = fade;
    }

    /// Ends a fade at once: a change waiting for it applies, with the line
    /// empty.
    fn settle(&mut self) {
        if let Some(frames) = self.pending.take() {
            self.frames = frames;
            self.clear();
        }
        self.moving = Moving::Still;
    }

    /// Empties the line.
    fn clear(&mut self) {
        self.ring[..self.frames * self.channels].fill(0);
        self.pos = 0;
    }

    /// The line holds frames back or fades: `process` has work to do.
    fn active(&self) -> bool {
        self.frames > 0 || self.moving != Moving::Still
    }

    /// Takes `samples`, interleaved frames that left the line, down to
    /// silence over them as the fade before a reopen does at delay 0:
    /// frame `i` of `n` at `1 - i / n`. The frames that entered the line
    /// this period, from frame `frames` on, carry that fade already, at
    /// `1 - j / n` for input frame `j`; they get only what is missing.
    fn fade_out(&self, samples: &mut [i32]) {
        let n = (samples.len() / self.channels) as f64;
        for (i, frame) in samples.chunks_exact_mut(self.channels).enumerate() {
            let mut gain = 1.0 - i as f64 / n;
            if let Some(j) = i.checked_sub(self.frames) {
                gain /= 1.0 - j as f64 / n;
            }
            for v in frame.iter_mut() {
                *v = (f64::from(*v) * gain).round() as i32;
            }
        }
    }

    /// Passes `samples`, interleaved frames, through the line and the fades.
    fn process(&mut self, samples: &mut [i32]) {
        let channels = self.channels;
        // A fade in goes before the line, so it starts with the first
        // frames that enter it.
        if let Moving::Up(mut left) = self.moving {
            crate::output_mix::unmute(samples, channels, &mut left, self.fade);
            self.moving = if left == 0 {
                Moving::Still
            } else {
                Moving::Up(left)
            };
        }
        let line = &mut self.ring[..self.frames * channels];
        let mut done = 0;
        while done < samples.len() && !line.is_empty() {
            let n = (line.len() - self.pos).min(samples.len() - done);
            samples[done..done + n].swap_with_slice(&mut line[self.pos..self.pos + n]);
            done += n;
            self.pos = (self.pos + n) % line.len();
        }
        // A fade out goes after it, on what is heard.
        if let Moving::Down(mut left) = self.moving {
            for frame in samples.chunks_exact_mut(channels) {
                left = left.saturating_sub(1);
                let gain = left as f64 / self.fade as f64;
                for v in frame.iter_mut() {
                    *v = (f64::from(*v) * gain).round() as i32;
                }
            }
            if left == 0 {
                self.settle();
                self.moving = Moving::Up(self.fade);
            } else {
                self.moving = Moving::Down(left);
            }
        }
    }
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
    /// The master card's mix: its matrix, chains and gain.
    master: OutputMix<P>,
    /// The further outputs, output 1 first.
    further: [Option<Further<P>>; FURTHER],
    /// A further output's period, interleaved, on its way to its feed.
    further_samples: Vec<i32>,
    /// There is a further output: the cycle feeds it.
    feeding: bool,
    /// When this cycle woke, on the feeds' clock: the stamp of its pushes.
    woke_ns: Option<u64>,
    delay: DelayLine,
    /// A period of stereo frames that play unaltered: the one source the
    /// copy path copies, or a DoP stream.
    out: Vec<[i32; 2]>,
    /// The period as it goes to the sink, interleaved.
    samples: Vec<i32>,
    scratch: Vec<[i32; 2]>,
    /// The `players` bus: the clients, at their gains and the output's.
    players: Vec<[P::Acc; 2]>,
    /// The `analog` bus: the analog input, at its gain and the output's.
    analog_bus: Vec<[P::Acc; 2]>,
    /// Where the analog input is handed to its chain.
    store: BlockStore,
    analog: Analog<P>,
    taps: Taps,
    rate: u32,
    period: usize,
    open: bool,
    /// A format waiting to be opened.
    reopen: Option<(u32, u32)>,
    /// The fade for a queued `Reconfigure` has been played.
    faded: bool,
    stopped: bool,
    last_start: Option<Instant>,
    /// The last DoP claim handed out.
    claims: u64,
}

impl<P: Processing, S: Sink> Engine<P, S> {
    /// A closed engine. The first `Reconfigure` opens the output. It plays
    /// as many channels as `sink` has, mixed through `matrix`. Its buffers
    /// are sized here for the wider of the two, and its delay line for
    /// `delay` frames, 0 for none.
    pub fn new(
        sink: S,
        commands: Consumer<Command>,
        events: Producer<Event>,
        counters: Arc<Counters>,
        mode: BitPerfect,
        matrix: Matrix,
        delay: usize,
    ) -> Self {
        let frames = MAX_PERIOD as usize;
        let channels = sink.channels();
        let forced = mode == BitPerfect::Forced;
        Engine {
            sink,
            commands,
            events,
            counters,
            mode,
            table: Vec::with_capacity(INITIAL_SOURCES),
            backlog: VecDeque::with_capacity(BACKLOG),
            master: OutputMix::new(matrix, channels, forced, frames),
            further: std::array::from_fn(|_| None),
            further_samples: vec![0; frames * MAX_CHANNELS],
            feeding: false,
            woke_ns: None,
            delay: DelayLine::new(delay, channels),
            out: vec![[0; 2]; frames],
            samples: vec![0; frames * channels],
            scratch: vec![[0; 2]; frames * MAX_DOWNSAMPLING + dop::MIN_FRAMES],
            players: vec![[P::zero(); 2]; frames],
            analog_bus: vec![[P::zero(); 2]; frames],
            store: BlockStore::new(frames, 2),
            analog: Analog::new(frames),
            taps: Taps::new(frames),
            rate: 0,
            period: 0,
            open: false,
            reopen: None,
            faded: false,
            stopped: false,
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
        self.woke_ns = self.wake_stamp();
        let result = if self.open && !self.faded && self.fade_queued() {
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
                        self.woke_ns = self.wake_stamp();
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

    /// The time on the feeds' clock, if there is a feed: when this cycle
    /// woke, which stamps its pushes.
    fn wake_stamp(&self) -> Option<u64> {
        if !self.feeding {
            return None;
        }
        self.outputs().next().map(|f| f.feed.now())
    }

    /// The stamp of this cycle's pushes: when it woke, or, in the cycle
    /// that added the first further output, when it looked first, which is
    /// as soon as there was a feed to ask.
    fn stamp(&mut self) -> u64 {
        if self.woke_ns.is_none() {
            self.woke_ns = self.wake_stamp();
        }
        self.woke_ns.unwrap_or(0)
    }

    /// The further outputs in place.
    fn outputs(&self) -> impl Iterator<Item = &Further<P>> {
        self.further.iter().flatten()
    }

    /// Every output's mix, the master's first.
    fn mixes_mut(&mut self) -> impl Iterator<Item = &mut OutputMix<P>> {
        let further = self.further.iter_mut().flatten().map(|f| &mut *f.mix);
        std::iter::once(&mut self.master).chain(further)
    }

    /// The mix of output `index`, 0 being the master, if it is there.
    fn output_mix(&self, index: usize) -> Option<&OutputMix<P>> {
        match index {
            0 => Some(&self.master),
            i => self.further.get(i - 1)?.as_ref().map(|f| &*f.mix),
        }
    }

    fn output_mix_mut(&mut self, index: usize) -> Option<&mut OutputMix<P>> {
        match index {
            0 => Some(&mut self.master),
            i => self.further.get_mut(i - 1)?.as_mut().map(|f| &mut *f.mix),
        }
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

    /// Whether the next commands end what the card plays, so that a
    /// period fades to silence first: a reconfigure, or a close or the
    /// shutdown while the delay line still holds sound. Without the line a
    /// close comes when nothing plays, and cuts at once as it always did.
    /// Looks through the queue without taking anything from it.
    fn fade_queued(&mut self) -> bool {
        let waiting = self.commands.slots();
        let Ok(chunk) = self.commands.read_chunk(waiting) else {
            return false;
        };
        let (first, second) = chunk.as_slices();
        let (mut reconfigure, mut close) = (false, false);
        for command in first.iter().chain(second) {
            match command {
                Command::Reconfigure { .. } => reconfigure = true,
                Command::CloseOutput | Command::Shutdown => close = true,
                _ => {}
            }
        }
        reconfigure || close && self.delay.holds_sound()
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
            Command::SetGain(output, gain) => self.set_gain(output, gain),
            Command::Grow(table) => self.grow(table),
            Command::SetChain(target, mut chain) => {
                // A chain that could not take the longest period would fail
                // after a rate change, so it is sent straight back, as is
                // one built for other channels.
                if chain.max_frames() >= MAX_PERIOD as usize && self.chain_fits(target, &chain) {
                    // The old chain goes back in the box the new one came in.
                    if let Some(old) = self.chain(target) {
                        std::mem::swap(old, &mut chain);
                    }
                }
                self.emit(Event::OldChain(target.output, chain));
            }
            Command::SetParam {
                target,
                plugin,
                index,
                value,
            } => {
                if let Some(chain) = self.chain(target) {
                    chain.set_param(plugin.into(), index.into(), value);
                }
            }
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
            Command::SetMatrix(output, matrix) => self.set_matrix(output, matrix),
            Command::AddOutput(index, mix, feed) => self.add_output(index, mix, feed),
            Command::RemoveOutput(index) => {
                if let Some(slot) = index.checked_sub(1).and_then(|i| self.further.get_mut(i))
                    && let Some(Further { mix, feed }) = slot.take()
                {
                    self.feeding = self.further.iter().any(Option::is_some);
                    self.emit(Event::OldOutput(index, mix, feed));
                }
            }
            Command::SetDelay(frames) => {
                let dop = self.table.iter().any(|s| s.dop_claim != 0);
                let sound = self.heard() || self.delay.holds_sound();
                let fade = (self.rate * FADE_MS / 1000).max(1) as usize;
                let faded = self.open && !dop && sound;
                self.delay.set(frames, faded.then_some(fade));
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

    /// Whether `chain` fits the channel counts at its target, as
    /// `Command::SetChain` says. A count of 0 is any count.
    fn chain_fits(&self, target: Target, chain: &Chain) -> bool {
        let takes = |n: usize, count: usize| n == 0 || n == count;
        match (target.output, target.place) {
            // The analog input runs its chain in place, which a chain that
            // changes the count anywhere inside it cannot do.
            (0, Place::Analog) => {
                takes(chain.channels_in(), 2)
                    && takes(chain.channels_out(), 2)
                    && !chain.changes_channels()
            }
            (output, place) => self
                .output_mix(output)
                .is_some_and(|mix| mix.chain_fits(place, chain)),
        }
    }

    /// The chain at `target`, if there is one.
    fn chain(&mut self, target: Target) -> Option<&mut Chain> {
        match (target.output, target.place) {
            (0, Place::Analog) => Some(&mut self.analog.chain),
            (output, place) => self.output_mix_mut(output)?.chain(place),
        }
    }

    /// Puts a further output in place, as `Command::AddOutput` says.
    fn add_output(&mut self, index: usize, mix: Box<dyn Any + Send>, feed: Feed) {
        let slot = index.checked_sub(1).filter(|&i| i < FURTHER);
        let mix = match (slot, mix.downcast::<OutputMix<P>>()) {
            (Some(slot), Ok(mix)) if mix.fits_engine(feed.channels()) => {
                let new = Further { mix, feed };
                self.feeding = true;
                if let Some(Further { mix, feed }) = self.further[slot].replace(new) {
                    self.emit(Event::OldOutput(index, mix, feed));
                }
                return;
            }
            (_, Ok(mix)) => mix as Box<dyn Any + Send>,
            (_, Err(mix)) => mix,
        };
        self.emit(Event::OldOutput(index, mix, feed));
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
        for mix in self.mixes_mut() {
            mix.finish_fade();
            mix.finish_ramp();
        }
        self.delay.settle();
        self.delay.clear();
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
        for mix in self.mixes_mut() {
            mix.reopened(rate);
        }
        self.period = period as usize;
        self.last_start = None;
        self.emit(Event::Reconfigured);
        Ok(())
    }

    /// A change fades only while something is audible, to avoid a click.
    /// Otherwise it applies at once, so that a stream starting in the same
    /// cycle as the change plays at the new level from its first frame.
    fn set_gain(&mut self, output: usize, gain: f32) {
        let fade = (self.open && self.heard()).then(|| (self.rate * FADE_MS / 1000) as usize);
        let Some(mix) = self.output_mix_mut(output) else {
            return;
        };
        if gain != mix.target_gain {
            mix.set_gain(gain, fade);
        }
    }

    /// Whether a source or the analog input is heard this cycle, so that
    /// a change of gain has something to fade.
    fn heard(&self) -> bool {
        self.audible().next().is_some() || self.analog.audible
    }

    /// Replaces the matrix, as `Command::SetMatrix` says.
    ///
    /// Each gain moves in a straight line from where it is to its new
    /// value over 10 ms of frames, also when a change arrives while
    /// another still ramps. In forced bit-perfect the new gains are taken
    /// as `bus_gains` takes them, so a change only turns routes on or off.
    /// While the gains move the copy path is not used, and no DoP stream
    /// takes the output; both follow the new matrix once the ramp is over.
    ///
    /// The change applies at once when nothing is heard, as the output
    /// gain's does, when no gain moves, when the output is closed, and
    /// while a DoP stream has a claim on the output: DoP is never faded,
    /// and a stream whose routing changes is cut anyway, as by a reopen.
    /// It then plays on, unaltered, through the new routing, or, if that
    /// would alter it, goes silent and gives up its claim. A ramp also
    /// ends in a cycle where nothing is heard any more, so that a DoP
    /// stream that starts then plays at once, and at a close or reopen of
    /// the output, which has faded to silence by then.
    fn set_matrix(&mut self, output: usize, matrix: Box<Matrix>) {
        // Forced bit-perfect takes the master's routes at 0 dB; a further
        // output's gains always apply.
        let forced = output == 0 && self.mode == BitPerfect::Forced;
        let dop = self.table.iter().any(|s| s.dop_claim != 0);
        let ramps = self.open && self.heard() && !dop;
        let ramp = ramps.then(|| (self.rate * FADE_MS / 1000) as usize);
        let old = match self.output_mix_mut(output) {
            Some(mix) => mix.set_matrix(matrix, forced, ramp),
            None => matrix,
        };
        self.emit(Event::OldMatrix(output, old));
    }

    /// Running sources that are heard this cycle.
    fn audible(&self) -> impl Iterator<Item = &Source> {
        let (rate, routed) = (self.rate, self.master.dop_routing.is_some());
        let sources = self.table.iter().map(|s| &**s);
        sources.filter(move |s| s.running && !s.muted(rate, routed) && s.plays)
    }

    /// Plays one period. `fade_out` takes the level down to zero over it.
    ///
    /// The cycle has waited for room on the card before this and only now
    /// reads the clients, so what it reads is written at once and waits
    /// behind no more than the card's buffer. The cycle is timed from
    /// `start`, the end of that wait.
    fn render(&mut self, fade_out: bool, start: Instant) -> Result<(), SinkError> {
        self.master.route_dop();
        let store = &mut self.store;
        let plugins = plugins_apply(self.mode);
        self.analog
            .read(self.period, self.rate, &self.counters, store, plugins);
        self.taps.deliver(&self.analog, self.period);
        self.scan();
        // A ramp with nothing left to hear has nothing to fade.
        let ramping = self.master.ramping()
            || self.feeding && self.outputs().any(|output| output.mix.ramping());
        if ramping && !self.heard() {
            for mix in self.mixes_mut() {
                mix.finish_ramp();
            }
            self.master.route_dop();
        }
        self.order_claims();
        let (rate, routed) = (self.rate, self.master.dop_routing.is_some());
        let holders = self.table.iter().enumerate();
        let holders = holders.filter(|(_, s)| s.holds_output(rate, routed));
        let owner = holders.min_by_key(|(_, s)| s.dop_claim).map(|(i, _)| i);
        let chains_ran = match owner {
            Some(owner) => {
                self.play_dop(owner);
                // Nothing may alter the stream on its way to the card.
                self.delay.settle();
                false
            }
            None => self.play_pcm(fade_out),
        };
        // The mix and output chains missed this period's audio.
        self.master.chains_ran(chains_ran);
        let samples = &mut self.samples[..self.period * self.master.channels];
        if self.delay.active() {
            self.delay.process(samples);
        }
        // The fade before a reopen or a close was played into the line,
        // which the close empties: it fades what leaves the line instead.
        if fade_out && owner.is_none() && self.delay.frames > 0 {
            self.delay.fade_out(samples);
        }
        let before_write = Instant::now();
        let result = self.sink.write(samples);
        let after_write = Instant::now();
        if result == Err(SinkError::Xrun) {
            self.counters.xruns.fetch_add(1, Ordering::Relaxed);
        }
        // What the plugins hold back plays after the ring too, in the
        // cycles they ran in.
        let plugins = if chains_ran {
            self.master.latency_frames()
        } else {
            0
        };
        let held = self.delay.frames as u32;
        let delay = self
            .sink
            .delay_frames()
            .saturating_add(plugins)
            .saturating_add(held);
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
    /// longer DoP, or has stalled, gives its claim up, and so does every
    /// one while DoP cannot reach the sink unaltered.
    fn order_claims(&mut self) {
        let (rate, routed) = (self.rate, self.master.dop_routing.is_some());
        for source in self.table.iter_mut() {
            source.note_stall(rate);
            if source.stream != Stream::Dop || source.stalled || !routed {
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
    /// analog input has been read and measured, and is not played. Only a
    /// stream the `players` routing takes unaltered holds the output.
    fn play_dop(&mut self, owner: usize) {
        debug_assert!(!self.master.ramping(), "DoP never plays during a ramp");
        self.master.finish_fade();
        // The other outputs are silent meanwhile.
        let woke = self.stamp();
        for output in self.further.iter_mut().flatten() {
            output.mix.silenced();
            output.feed.silence_at(self.period as u32, woke);
        }
        let (rate, period) = (self.rate, self.period);
        for (index, source) in self.table.iter_mut().enumerate() {
            if index == owner {
                source.read_dop(&mut self.out[..period]);
            } else if source.running {
                source.discard(period, rate);
            }
        }
        let routing = self.master.dop_routing.unwrap_or([None; MAX_CHANNELS]);
        self.place_unaltered(&routing);
    }

    /// Writes the period in `out`, from the one source the copy path copies
    /// or from a DoP stream, to the sink's buffer unaltered: to the channels
    /// `routing` names, with zeros on the others.
    fn place_unaltered(&mut self, routing: &Routing) {
        let (period, channels) = (self.period, self.master.channels);
        let samples = &mut self.samples[..period * channels];
        place_routed(&self.out[..period], routing, channels, samples);
    }

    /// The bus of the one source the copy path would copy: the analog
    /// input's while it is audible, else the players'.
    fn copy_bus(&self) -> Bus {
        if self.analog.audible {
            Bus::Analog
        } else {
            Bus::Players
        }
    }

    /// How the copy path places the bus of the one active source, if the
    /// matrix routes it purely. In forced bit-perfect every route that is
    /// not off counts as 0 dB.
    fn copy_routing(&self) -> Option<Routing> {
        self.master.routes.copy[self.copy_bus().index()]
    }

    /// What the bit-perfect rules need to know about this cycle.
    /// The analog input takes part only while it is audible.
    fn situation(&self) -> Situation {
        let audible = || self.audible();
        let analog = self.analog.audible;
        let active = audible().count() + usize::from(analog);
        Situation {
            mode: self.mode,
            active,
            software_volume_at_0db: self.master.gain == 1.0
                && self.master.target_gain == 1.0
                && audible().all(|s| s.gain == 1.0)
                && (!analog || self.analog.gain == 1.0),
            resampled: audible().any(|s| s.resampler.is_some()),
            plugins_neutral: self.master.neutral() && (!analog || self.analog.chain.neutral()),
            routing_pure: !self.master.ramping()
                && (active == 0 || self.master.copy_allowed() && self.copy_routing().is_some()),
        }
    }

    /// Mixes the sources into the `players` bus and the analog input, while
    /// it is audible, into the `analog` bus, or copies the one that is
    /// bit-perfect. A mix then goes through the matrix and runs the mix
    /// chain and the output chain, in that order, except in forced
    /// bit-perfect, which bypasses them. Returns whether a chain with
    /// plugins ran.
    fn play_pcm(&mut self, fade_out: bool) -> bool {
        let (rate, period, channels) = (self.rate, self.period, self.master.channels);
        let volumes = volumes_apply(self.mode);
        // A detector fading the analog input has changed its frames.
        let fading = self.analog.audible && self.analog.plays != Plays::Captured;
        // Nor is a fade in after a mute for a chain at another rate over.
        let unmuting = self.master.unmuting();
        let copy_routing = if !fade_out
            && !fading
            && !unmuting
            && self.master.copy_allowed()
            && decide(&self.situation()).is_ok()
        {
            self.copy_routing()
        } else {
            None
        };
        let copy = copy_routing.is_some();
        let routed = self.master.dop_routing.is_some();
        // With further outputs the buses are at unity, and each output
        // applies its own gain.
        let feeding = self.feeding;
        let fade_step = self.master.next_fade_step(period);
        let (next_gain, fade_frames) = fade_step;
        // A matrix that passes both buses to channels 1 and 2 as they are
        // adds nothing to what the buses hold: the analog input is mixed
        // straight into the players' bus, which is then the mix itself.
        // Not while other outputs read the buses too.
        let direct = !feeding && self.master.direct(period);
        let (from, to) = if volumes && !feeding {
            (self.master.gain, next_gain)
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
        let players = &mut self.players[..period];
        if !copy {
            mix_clear::<P>(players);
        }
        for source in self.table.iter_mut().filter(|s| s.running) {
            if source.held > 0 {
                let held = &self.counters.held_frames;
                held.fetch_add(source.held as u64, Ordering::Relaxed);
            }
            if source.muted(rate, routed) {
                source.discard(period, rate);
            } else if !source.plays {
                source.note_shortfall(0, 1);
            } else if copy {
                source.read_raw(out);
            } else {
                source.read_resampled(scratch);
                let level = if volumes { source.gain } else { 1.0 };
                fade.mix::<P>(scratch, level, players);
            }
        }
        let analog_bus = &mut self.analog_bus[..period];
        if self.analog.audible {
            if copy {
                // Only with a neutral chain, so `buf` holds the period.
                out.copy_from_slice(&self.analog.buf[..period]);
            } else {
                let level = if volumes { self.analog.gain } else { 1.0 };
                if direct {
                    self.analog.mix(fade, level, players);
                } else {
                    mix_clear::<P>(analog_bus);
                    self.analog.mix(fade, level, analog_bus);
                }
            }
        }
        if feeding {
            return self.mix_outputs(copy_routing, fade_step);
        }
        if let Some(routing) = copy_routing {
            self.place_unaltered(&routing);
            self.master.step_fade(fade_step);
            return false;
        }
        let analog = self.analog.audible.then_some(&*analog_bus);
        let samples = &mut self.samples[..period * channels];
        let plugins = plugins_apply(self.mode);
        let chains_ran = self.master.mix(players, analog, samples, rate, plugins);
        self.master.step_fade(fade_step);
        chains_ran
    }

    /// The rest of `play_pcm` with further outputs: the buses, at unity,
    /// are mixed by every output through its own `OutputMix`, at its own
    /// gain, and each further output's period goes to its feed. While the
    /// master copies its one source, the others mix it from the buses.
    /// Out of line, so that the master alone plays as it did before there
    /// were further outputs.
    #[inline(never)]
    fn mix_outputs(&mut self, copy_routing: Option<Routing>, fade_step: (f32, usize)) -> bool {
        let (rate, period, channels) = (self.rate, self.period, self.master.channels);
        let volumes = volumes_apply(self.mode);
        let plugins = plugins_apply(self.mode);
        let woke = self.stamp();
        let out = &self.out[..period];
        let players = &mut self.players[..period];
        let analog_bus = &mut self.analog_bus[..period];
        if copy_routing.is_some() {
            // The one source the master copies, at unity, for the others.
            mix_clear::<P>(players);
            mix_clear::<P>(analog_bus);
            let bus = if self.analog.audible {
                &mut *analog_bus
            } else {
                &mut *players
            };
            mix_add::<P>(out, Ramp::flat(1.0), bus);
        }
        let players = &*players;
        let analog = self.analog.audible.then_some(&*analog_bus);
        let samples = &mut self.samples[..period * channels];
        let chains_ran = match copy_routing {
            Some(routing) => {
                place_routed(out, &routing, channels, samples);
                self.master.step_fade(fade_step);
                false
            }
            None => self
                .master
                .mix_shared(players, analog, samples, rate, plugins, volumes),
        };
        for output in self.further.iter_mut().flatten() {
            let samples = &mut self.further_samples[..period * output.mix.channels];
            let mix = &mut output.mix;
            // Never bit-perfect: their gains and chains apply in every mode.
            let ran = mix.mix_shared(players, analog, samples, rate, true, true);
            mix.chains_ran(ran);
            output.feed.push_at(samples, woke);
        }
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
        // A DoP stream that cannot reach the sink unaltered is not reported:
        // it does not take the output.
        let routed = self.master.dop_routing.is_some();
        for source in self.table.iter_mut() {
            let dop = source.stream == Stream::Dop && !source.stalled && routed;
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
    use crate::further::FeedProbe;
    use crate::plugin::detect::Detect;
    use crate::plugin::{
        Block, Channels, Formats, Param, ParamKind, Plugin, Samples, Status as PluginStatus,
    };
    use crate::sink::{MemorySink, SinkLog};
    use aas_client::ring::Ring;
    use assert_no_alloc::assert_no_alloc;
    use rtrb::RingBuffer;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};
    use std::sync::atomic::Ordering::{Relaxed, Release};
    use std::time::{Duration, Instant};

    const RATE: u32 = 48000;
    const PERIOD: usize = 64;
    /// The delay line's capacity in frames.
    const DELAY: usize = 1024;

    /// The default matrix on a stereo sink.
    fn stereo() -> Matrix {
        Matrix::default_for(2)
    }
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
            Self::closed_with(mode, event_slots, Matrix::default_for(2), 2)
        }

        /// The same, for a sink of `channels` channels and `matrix`.
        fn closed_with(
            mode: BitPerfect,
            event_slots: usize,
            matrix: Matrix,
            channels: usize,
        ) -> Self {
            let sink = MemorySink::with_channels(channels);
            let log = sink.log();
            log.reserve(1 << 20);
            let (commands, consumer) = RingBuffer::new(1024);
            let (producer, events) = RingBuffer::new(event_slots);
            let counters = Arc::new(Counters::default());
            let engine = Engine::new(
                sink,
                consumer,
                producer,
                counters.clone(),
                mode,
                matrix,
                DELAY,
            );
            Rig {
                engine,
                commands,
                events,
                log,
                counters,
            }
        }

        fn with_event_slots(mode: BitPerfect, event_slots: usize) -> Self {
            Self::opened(Self::closed(mode, event_slots))
        }

        /// An open engine with a sink of `channels` channels and `matrix`.
        fn wide(mode: BitPerfect, matrix: Matrix, channels: usize) -> Self {
            Self::opened(Self::closed_with(mode, 1024, matrix, channels))
        }

        fn opened(mut rig: Self) -> Self {
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

        /// The frames of the latest `n` periods of a sink of any width.
        fn last_wide(&self, n: usize) -> Vec<Vec<i32>> {
            let channels = self.engine.master.channels;
            let samples = self.log.samples();
            let start = samples.len() - n * PERIOD * channels;
            samples[start..]
                .chunks(channels)
                .map(<[i32]>::to_vec)
                .collect()
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
        rig.send(Command::SetGain(0, gain));
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
        rig.send(Command::SetGain(0, db_to_gain(-20.0)));
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
        rig.send(Command::SetGain(0, 0.5));
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
        rig.send(Command::SetGain(0, 0.5));
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
        rig.send(Command::SetGain(0, 0.0));
        rig.cycle();
        rig.cycle();
        rig.send(Command::SetGain(0, 1.0));
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
        rig.send(Command::SetGain(0, 0.5));
        for _ in 0..10 {
            rig.cycle();
        }
        rig.send(Command::SetGain(0, 1.0));
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
        rig.send(Command::SetGain(0, 0.1));
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
        rig.send(Command::SetGain(0, 0.1));
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
            rig.send(Command::SetGain(0, db_to_gain(-20.0)));
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
        rig.send(Command::SetGain(0, 0.1));
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
        rig.send(Command::SetGain(0, 0.1));
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

        fn write(&mut self, _samples: &[i32]) -> Result<(), SinkError> {
            if std::mem::take(&mut self.switch_next) {
                std::thread::sleep(self.took);
                self.switched = self.reports;
            }
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
        let mut engine = Engine::<F32, _>::new(
            sink,
            consumer,
            producer,
            counters.clone(),
            BitPerfect::Auto,
            Matrix::default_for(2),
            0,
        );
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
        rig.send(Command::SetGain(0, 0.5));
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

        fn write(&mut self, samples: &[i32]) -> Result<(), SinkError> {
            self.played
                .lock()
                .unwrap()
                .extend(samples.chunks_exact(2).map(|f| [f[0], f[1]]));
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
        let mut engine =
            Engine::<F32, _>::new(sink, consumer, producer, counters, mode, stereo(), 0);
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

        fn write(&mut self, samples: &[i32]) -> Result<(), SinkError> {
            self.played
                .lock()
                .unwrap()
                .extend(samples.chunks_exact(2).map(|f| [f[0], f[1]]));
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
        let mut engine =
            Engine::<F32, _>::new(sink, consumer, producer, counters, mode, stereo(), 0);
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
    fn opening_the_card_is_not_a_gap_or_cycle_time() {
        let (mut commands, consumer) = RingBuffer::new(16);
        let (producer, _events) = RingBuffer::new(16);
        let counters = Arc::new(Counters::default());
        let mode = BitPerfect::Auto;
        let mut engine = Engine::<F32, SlowOpen>::new(
            SlowOpen,
            consumer,
            producer,
            counters.clone(),
            mode,
            stereo(),
            0,
        );
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
        rig.send(Command::SetGain(0, 0.1));
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
        /// Takes any channel count, rather than two.
        any: bool,
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
                any: false,
            }
        }

        /// The same for any channel count.
        fn any(mut self) -> Probe {
            self.any = true;
            self
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

        fn channels(&self) -> Channels {
            if self.any {
                Channels::Any
            } else {
                Channels::Fixed {
                    input: 2,
                    output: 2,
                }
            }
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
            match block.samples {
                Samples::I32(d) => {
                    for s in d.iter_mut() {
                        *s = (self.apply(f64::from(*s) / FULL) * FULL).round() as i32;
                    }
                }
                Samples::F32(d) => {
                    for s in d.iter_mut() {
                        *s = self.apply(f64::from(*s)) as f32;
                    }
                }
                Samples::F64(d) => {
                    for s in d.iter_mut() {
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
        P::to_block(&mut [], 2, &mut BlockStore::new(0, 2)).format()
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
        Box::new(Chain::new(plugins, format_of::<P>(), max_frames, 2).unwrap())
    }

    /// Takes only floats, so an engine in another format converts around it.
    const F64_ONLY: Formats = Formats {
        i32: false,
        f32: false,
        f64: true,
    };

    fn set_chain<P: Processing>(rig: &mut Rig<P>, place: Place, plugins: Vec<Probe>) {
        rig.send(Command::SetChain(
            Target::master(place),
            chain_of::<P>(plugins),
        ));
    }

    /// Delays the audio by one frame: it keeps the last frame of a block
    /// for the next. Reset, it starts from silence.
    struct Delay {
        kept: [f64; 2],
    }

    impl Plugin for Delay {
        fn name(&self) -> &str {
            "delay"
        }

        fn formats(&self) -> Formats {
            ALL_FORMATS
        }

        fn params(&self) -> &[Param] {
            &[]
        }

        fn set_param(&mut self, _: usize, _: f64) {}

        fn neutral(&self) -> bool {
            false
        }

        fn latency_frames(&self) -> u32 {
            1
        }

        fn process(&mut self, block: Block<'_>) -> PluginStatus {
            if let Samples::F64(d) = block.samples {
                for frame in d.chunks_exact_mut(2) {
                    frame.swap_with_slice(&mut self.kept);
                }
            }
            PluginStatus::default()
        }

        fn reset(&mut self) {
            self.kept = [0.0; 2];
        }
    }

    #[test]
    fn a_chain_that_resumes_after_dop_does_not_play_an_old_tail() {
        let mut rig = Rig::<F64>::new(BitPerfect::Auto);
        let plugins: Vec<Box<dyn Plugin>> = vec![Box::new(Delay { kept: [0.0; 2] })];
        let chain = Chain::new(plugins, Format::F64, MAX_PERIOD as usize, 2).unwrap();
        rig.send(Command::SetChain(
            Target::master(Place::Mix),
            Box::new(chain),
        ));
        let mut a = client(1);
        rig.add(&mut a);
        // The delay keeps the last frame of this period.
        a.ring.write(&frames(1 << 20, PERIOD));
        rig.cycle();
        // A DoP stream has the output: the chain does not run.
        let mut d = client(2);
        rig.add(&mut d);
        d.ring.write(&dop_frames(4 * PERIOD));
        for _ in 0..4 {
            a.ring.write(&frames(1 << 20, PERIOD));
            rig.cycle();
        }
        rig.send(Command::Remove(2));
        // PCM again: the first frame is the reset delay's silence, not the
        // frame it kept from before DoP.
        a.ring.write(&frames(1 << 21, PERIOD));
        rig.cycle();
        let out = rig.last_periods(1);
        assert_eq!(out[0], [0, 0]);
        assert_eq!(out[1], [1 << 21, -(1 << 21)]);
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
                Target::master(Place::Mix),
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
                Target::master(Place::Mix),
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
                Target::master(Place::Mix),
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
            Target::master(Place::Mix),
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
            Target::master(Place::Mix),
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
            Target::master(Place::Analog),
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
            rig.send(Command::SetGain(0, 0.1));
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
            target: Target::master(place),
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
                target: Target::master(place),
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
            Event::OldChain(_, c) => Some(c.latency_frames()),
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
        rig.send(Command::SetChain(Target::master(Place::Mix), refused));
        a.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        // The refused chain comes back, and the one in place keeps playing.
        assert_eq!(old_chains(rig.events()), [Some(7)]);
        assert_eq!(rig.last_periods(1), frames(2000, PERIOD));
        // A chain without plugins takes any period.
        rig.send(Command::SetChain(
            Target::master(Place::Mix),
            Box::new(Chain::empty()),
        ));
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
            Target::master(Place::Output),
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
    fn a_huge_plugin_latency_does_not_overflow_the_delay() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        let mut b = client_at(2, 24000);
        let r = Resampler::new(24000, RATE, PERIOD as u32, Quality::High, Format::F32);
        let filter = u64::from(r.delay_frames());
        b.source.as_mut().unwrap().resampler = Some(Box::new(r));
        rig.add(&mut b);
        set_chain(&mut rig, Place::Mix, vec![plus(0).latency(u32::MAX)]);
        set_chain(&mut rig, Place::Output, vec![plus(0).latency(u32::MAX)]);
        rig.log.set_delay(128);
        a.ring.write(&frames(1, PERIOD));
        rig.cycle();
        let delay = |c: &Client| c.ring.header().output_delay_frames.load(Relaxed);
        assert_eq!(delay(&a), u32::MAX);
        assert_eq!(u64::from(delay(&b)), (u64::from(u32::MAX) + filter) / 2);
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
        rig.send(Command::SetGain(0, db_to_gain(-6.0)));
        rig.send(Command::SetParam {
            target: Target::master(Place::Mix),
            plugin: 1,
            index: 0,
            value: 0.25,
        });
        rig.send(Command::SetChain(
            Target::master(Place::Output),
            chain_of::<P>(vec![times(3.0).only(other).latency(8)]),
        ));
        rig.send(Command::SetChain(
            Target::master(Place::Analog),
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
        Box::new(Chain::new(plugins, format_of::<P>(), MAX_PERIOD as usize, 2).unwrap())
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
            let chain = Chain::new(plugins, format_of::<P>(), MAX_PERIOD as usize, 2).unwrap();
            rig.send(Command::SetChain(
                Target::master(Place::Analog),
                Box::new(chain),
            ));
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
        rig.send(Command::SetChain(
            Target::master(Place::Analog),
            detect_chain::<F32>(0.0),
        ));
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
        let chain = Chain::new(analog, format_of::<P>(), MAX_PERIOD as usize, 2).unwrap();
        rig.send(Command::SetChain(
            Target::master(Place::Analog),
            Box::new(chain),
        ));
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
            let chain = Chain::new(gate(None), format_of::<P>(), MAX_PERIOD as usize, 2).unwrap();
            rig.send(Command::SetChain(
                Target::master(Place::Analog),
                Box::new(chain),
            ));
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
        let mut engine = Engine::<F32, _>::new(
            sink,
            consumer,
            producer,
            counters,
            BitPerfect::Auto,
            stereo(),
            0,
        );
        let mut send = |c| assert!(commands.push(c).is_ok());
        send(Command::Reconfigure {
            rate: RATE,
            period: PERIOD as u32,
        });
        engine.cycle().unwrap();
        // As the control thread does, after the output is open.
        send(Command::SetChain(
            Target::master(Place::Analog),
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
        rig.send(Command::SetChain(
            Target::master(Place::Analog),
            detect_chain::<I32>(3.0),
        ));
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
        assert!(matches!(rig.events.pop(), Ok(Event::OldChain(_, _))));
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
        let chain = Chain::new(plugins, format_of::<P>(), MAX_PERIOD as usize, 2).unwrap();
        rig.send(Command::SetChain(
            Target::master(Place::Analog),
            Box::new(chain),
        ));
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

    // The matrix.

    fn route(from: usize, to: usize, gain_db: f32) -> crate::config::Route {
        crate::config::Route { from, to, gain_db }
    }

    fn matrix(routes: &[crate::config::Route], width: usize) -> Matrix {
        Matrix::from_routes(routes, width).unwrap()
    }

    /// A chain in the processing format of `P` for blocks of `channels`.
    fn chain_at<P: Processing>(plugins: Vec<Box<dyn Plugin>>, channels: usize) -> Box<Chain> {
        let chain = Chain::new(plugins, format_of::<P>(), MAX_PERIOD as usize, channels);
        Box::new(chain.unwrap())
    }

    fn boxed(probes: Vec<Probe>) -> Vec<Box<dyn Plugin>> {
        probes
            .into_iter()
            .map(|p| Box::new(p) as Box<dyn Plugin>)
            .collect()
    }

    fn widen() -> Box<dyn Plugin> {
        crate::plugin::builtin::create("widen", RATE, 2)
            .unwrap()
            .unwrap()
    }

    /// A wide rig with the analog input on, its chain always with signal
    /// and altering nothing.
    fn wide_with_capture<P: Processing>(
        mode: BitPerfect,
        matrix: Matrix,
        channels: usize,
    ) -> (Rig<P>, CaptureFeed) {
        let mut rig = Rig::<P>::wide(mode, matrix, channels);
        let (capture, feed) = MemoryCapture::new();
        rig.send(Command::SetCapture(Some(Box::new(capture))));
        set_chain(&mut rig, Place::Analog, vec![times(1.0).neutral()]);
        (rig, feed)
    }

    #[test]
    fn the_default_matrix_plays_what_the_engine_played_before() {
        /// The output gain as the engine moved it before the matrix.
        struct OldFade {
            gain: f32,
            target: f32,
            left: usize,
        }

        impl OldFade {
            /// This period's fade, as `next_fade_step` and `Fade` made it,
            /// and the gain it ends on.
            fn step(&mut self) -> Fade {
                let frames = self.left.min(PERIOD);
                let next = if frames == self.left {
                    self.target
                } else {
                    let share = frames as f32 / self.left as f32;
                    self.gain + (self.target - self.gain) * share
                };
                let fade = Fade {
                    from: self.gain,
                    to: next,
                    frames,
                    out: false,
                };
                self.gain = next;
                self.left -= frames;
                fade
            }
        }

        /// One stereo period as the engine made it before the matrix: both
        /// clients and the analog chain's output into one accumulator along
        /// the output fade, at their gains, then the mix chain and the
        /// output chain.
        fn before<P: Processing>(
            inputs: [&[[i32; 2]]; 3],
            gains: [f32; 3],
            fade: Fade,
        ) -> Vec<[i32; 2]> {
            let [a, b, analog] = inputs;
            let [ga, gb, gin] = gains;
            let n = a.len();
            let mut store = BlockStore::new(n, 2);
            let mut processed = vec![[P::zero(); 2]; n];
            mix_add::<P>(analog, Ramp::flat(1.0), &mut processed);
            let mut analog_chain = chain_of::<P>(vec![times(0.75)]);
            let mut block = P::to_block(processed.as_flattened_mut(), 2, &mut store);
            analog_chain.process(block.reborrow());
            P::from_block(block, processed.as_flattened_mut());
            let mut acc = vec![[P::zero(); 2]; n];
            fade.add(a, ga, &mut acc, mix_add::<P>);
            fade.add(b, gb, &mut acc, mix_add::<P>);
            fade.add(&processed, gin, &mut acc, mix_add_acc::<P>);
            let mut mix = chain_of::<P>(vec![times(1.5)]);
            let mut output = chain_of::<P>(vec![plus(-12_345)]);
            let mut block = P::to_block(acc.as_flattened_mut(), 2, &mut store);
            mix.process(block.reborrow());
            output.process(block.reborrow());
            P::from_block(block, acc.as_flattened_mut());
            let mut out = vec![[0; 2]; n];
            mix_finish::<P>(&acc, &mut out);
            out
        }
        fn check<P: Processing>() {
            let gains = [0.3, db_to_gain(-1.5), 0.6];
            let mut old = OldFade {
                gain: db_to_gain(-3.0),
                target: db_to_gain(-3.0),
                left: 0,
            };
            let mut rig = rig_at_gain::<P>(old.gain);
            let (capture, feed) = MemoryCapture::new();
            rig.send(Command::SetCapture(Some(Box::new(capture))));
            rig.send(Command::SetAnalogGain(gains[2]));
            set_chain(&mut rig, Place::Analog, vec![times(0.75)]);
            set_chain(&mut rig, Place::Mix, vec![times(1.5)]);
            set_chain(&mut rig, Place::Output, vec![plus(-12_345)]);
            let (mut a, mut b) = (client(1), client(2));
            a.source.as_mut().unwrap().gain = gains[0];
            b.source.as_mut().unwrap().gain = gains[1];
            rig.add(&mut a);
            rig.add(&mut b);
            for k in 0..12 {
                if k == 2 {
                    // Heard, so it fades over 10 ms, 7.5 periods.
                    rig.send(Command::SetGain(0, db_to_gain(-7.0)));
                    old.target = db_to_gain(-7.0);
                    old.left = (RATE * FADE_MS / 1000) as usize;
                }
                // Odd values near full scale, which every format rounds,
                // and a sum that goes over it.
                let pa: Vec<[i32; 2]> = (0..PERIOD as i32)
                    .map(|i| [ODD + 977 * i + k, i32::MIN + 13 * i])
                    .collect();
                let pb: Vec<[i32; 2]> = (0..PERIOD as i32)
                    .map(|i| [i32::MAX - 31 * i - k, -ODD + i])
                    .collect();
                let pc: Vec<[i32; 2]> = (0..PERIOD as i32)
                    .map(|i| [0x7654_3211 - i * k, -0x0765_4321 * (i % 3)])
                    .collect();
                a.ring.write(&pa);
                b.ring.write(&pb);
                feed.push(&pc);
                rig.cycle();
                let want = before::<P>([&pa, &pb, &pc], gains, old.step());
                assert_eq!(
                    rig.last_periods(1),
                    want,
                    "period {k} in {:?}",
                    format_of::<P>()
                );
            }
            assert_eq!(old.left, 0, "the fade has ended");
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_swap_matrix_swaps_left_and_right() {
        fn check<P: Processing>() {
            let swap = matrix(
                &[
                    route(0, 2, 0.0),
                    route(1, 1, 0.0),
                    route(2, 2, 0.0),
                    route(3, 1, 0.0),
                ],
                2,
            );
            let (mut rig, feed) = wide_with_capture::<P>(BitPerfect::Auto, swap, 2);
            let mut a = client(1);
            rig.add(&mut a);
            a.ring.write(&[[1 << 20, -(3 << 24)]; PERIOD]);
            feed.push(&[[5 << 8, 7 << 16]; PERIOD]);
            rig.cycle();
            let want = [-(3 << 24) + (7 << 16), (1 << 20) + (5 << 8)];
            assert_eq!(
                rig.last_periods(1),
                vec![want; PERIOD],
                "{:?}",
                format_of::<P>()
            );
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_mono_sum_at_minus_6_db_gives_half_of_each_channel_on_both() {
        fn check<P: Processing>() {
            let half = 20.0 * 0.5f32.log10();
            let mono = matrix(
                &[
                    route(0, 1, half),
                    route(1, 1, half),
                    route(0, 2, half),
                    route(1, 2, half),
                ],
                2,
            );
            let mut rig = Rig::<P>::wide(BitPerfect::Auto, mono, 2);
            let mut a = client(1);
            rig.add(&mut a);
            a.ring.write(&[[3 << 26, 1 << 26]; PERIOD]);
            rig.cycle();
            for frame in rig.last_periods(1) {
                for v in frame {
                    assert!((v - (2 << 26)).abs() <= 8, "{v} in {:?}", format_of::<P>());
                }
            }
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_sum_of_two_full_scale_channels_clips_and_does_not_wrap() {
        fn check<P: Processing>() {
            // Both channels onto the one channel of the card.
            let sum = matrix(&[route(0, 1, 0.0), route(1, 1, 0.0)], 1);
            let mut rig = Rig::<P>::wide(BitPerfect::Auto, sum, 1);
            let mut a = client(1);
            rig.add(&mut a);
            let mut input = vec![[i32::MAX, i32::MAX]; PERIOD / 2];
            input.extend(vec![[i32::MIN, i32::MIN]; PERIOD / 2]);
            a.ring.write(&input);
            rig.cycle();
            let mut want = vec![vec![i32::MAX]; PERIOD / 2];
            want.extend(vec![vec![i32::MIN]; PERIOD / 2]);
            assert_eq!(rig.last_wide(1), want, "{:?}", format_of::<P>());
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn the_analog_input_can_go_to_other_channels_than_the_players() {
        fn check<P: Processing>() {
            let apart = matrix(
                &[
                    route(0, 1, 0.0),
                    route(1, 2, 0.0),
                    route(2, 3, 0.0),
                    route(3, 4, 0.0),
                ],
                4,
            );
            let (mut rig, feed) = wide_with_capture::<P>(BitPerfect::Auto, apart, 4);
            let mut a = client(1);
            rig.add(&mut a);
            a.ring.write(&frames(ODD, PERIOD));
            feed.push(&frames(0x0123_4500, PERIOD));
            rig.cycle();
            // The client is not exact in f32: it went through the mix.
            let played = rig.last_wide(1);
            for frame in &played {
                assert_eq!(&frame[2..], [0x0123_4500, -0x0123_4500]);
                assert!((frame[0] - ODD).abs() <= 128, "{:?}", format_of::<P>());
                assert!((frame[1] + ODD).abs() <= 128, "{:?}", format_of::<P>());
            }
            if format_of::<P>() != Format::F32 {
                assert_eq!(played[0][..2], [ODD, -ODD]);
            }
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn channels_no_route_names_are_silent() {
        fn check<P: Processing>() {
            let (mut rig, feed) =
                wide_with_capture::<P>(BitPerfect::Auto, Matrix::default_for(4), 4);
            let mut a = client(1);
            rig.add(&mut a);
            a.ring.write(&frames(1 << 24, PERIOD));
            feed.push(&frames(1 << 20, PERIOD));
            rig.cycle();
            let want = vec![(1 << 24) + (1 << 20), -(1 << 24) - (1 << 20), 0, 0];
            assert_eq!(
                rig.last_wide(1),
                vec![want; PERIOD],
                "{:?}",
                format_of::<P>()
            );
            // A matrix with routes names only them: the analog input is off.
            let players = matrix(&[route(0, 3, 0.0), route(1, 4, 0.0)], 4);
            let (mut rig, feed) = wide_with_capture::<P>(BitPerfect::Auto, players, 4);
            let mut a = client(1);
            rig.add(&mut a);
            a.ring.write(&frames(1 << 24, PERIOD));
            feed.push(&frames(1 << 20, PERIOD));
            rig.cycle();
            let want = vec![0, 0, 1 << 24, -(1 << 24)];
            assert_eq!(
                rig.last_wide(1),
                vec![want; PERIOD],
                "{:?}",
                format_of::<P>()
            );
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_chain_that_widens_feeds_a_four_channel_card() {
        fn check<P: Processing>() {
            // The chains come before the output opens, as the control
            // thread sends them.
            let mut rig = Rig::<P>::closed_with(BitPerfect::Auto, 1024, Matrix::default_for(2), 4);
            rig.send(Command::SetChain(
                Target::master(Place::Mix),
                chain_at::<P>(vec![widen()], 2),
            ));
            let halve = boxed(vec![times(0.5).any()]);
            rig.send(Command::SetChain(
                Target::master(Place::Output),
                chain_at::<P>(halve, 4),
            ));
            let mut rig = Rig::opened(rig);
            let mut a = client(1);
            rig.add(&mut a);
            a.ring.write(&[[1 << 24, -(1 << 22)]; PERIOD]);
            rig.cycle();
            let want = vec![1 << 23, -(1 << 21), 1 << 23, -(1 << 21)];
            assert_eq!(
                rig.last_wide(1),
                vec![want; PERIOD],
                "{:?}",
                format_of::<P>()
            );
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    /// Players left to channels 1 and 3, right to 2 and 4.
    fn doubled() -> Matrix {
        matrix(
            &[
                route(0, 1, 0.0),
                route(1, 2, 0.0),
                route(0, 3, 0.0),
                route(1, 4, 0.0),
            ],
            4,
        )
    }

    #[test]
    fn the_output_volume_applies_on_every_channel() {
        fn check<P: Processing>() {
            let mut rig = Rig::<P>::wide(BitPerfect::Auto, doubled(), 4);
            rig.send(Command::SetGain(0, 0.5));
            let mut a = client(1);
            rig.add(&mut a);
            a.ring.write(&frames(1000, PERIOD));
            rig.cycle();
            let want = vec![500, -500, 500, -500];
            assert_eq!(
                rig.last_wide(1),
                vec![want; PERIOD],
                "{:?}",
                format_of::<P>()
            );
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_reopen_fades_every_channel_out() {
        fn check<P: Processing>() {
            let mut rig = Rig::<P>::wide(BitPerfect::Auto, doubled(), 4);
            let mut a = client(1);
            rig.add(&mut a);
            a.ring.write(&frames(1 << 24, 2 * PERIOD));
            rig.cycle();
            rig.send(Command::Reconfigure {
                rate: 96000,
                period: PERIOD as u32,
            });
            rig.cycle();
            let got = rig.last_wide(1);
            assert_eq!(got[0], [1 << 24, -(1 << 24), 1 << 24, -(1 << 24)]);
            for (i, frame) in got.iter().enumerate() {
                let want = (1.0 - i as f64 / PERIOD as f64) * f64::from(1 << 24);
                for (c, &v) in frame.iter().enumerate() {
                    let sign = if c % 2 == 0 { 1.0 } else { -1.0 };
                    assert!(
                        (f64::from(v) - sign * want).abs() < 2.0,
                        "frame {i} channel {c} in {:?}",
                        format_of::<P>()
                    );
                }
            }
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_chain_for_another_rate_mutes_every_channel_and_fades_back_in() {
        fn check<P: Processing>() {
            let mut rig = Rig::<P>::wide(BitPerfect::Auto, doubled(), 4);
            let mut a = client(1);
            rig.add(&mut a);
            let twice = || chain_at::<P>(boxed(vec![times(2.0).any()]), 4);
            rig.send(Command::SetChain(
                Target::master(Place::Mix),
                Box::new(twice().for_rate(96000)),
            ));
            for _ in 0..4 {
                a.ring.write(&frames(1 << 20, PERIOD));
                rig.cycle();
            }
            assert!(rig.last_wide(4).iter().all(|f| f == &[0; 4]));
            rig.send(Command::SetChain(
                Target::master(Place::Mix),
                Box::new(twice().for_rate(RATE)),
            ));
            let mut out = Vec::new();
            for _ in 0..12 {
                a.ring.write(&frames(1 << 20, PERIOD));
                rig.cycle();
                out.extend(rig.last_wide(1));
            }
            let full = 1 << 21;
            for c in 0..4 {
                let sign = if c % 2 == 0 { 1 } else { -1 };
                let level: Vec<i32> = out.iter().map(|f| sign * f[c]).collect();
                assert!(level[0] <= full / 480 + 1, "{:?}", out[0]);
                let step = level.windows(2).map(|w| (w[1] - w[0]).abs()).max();
                assert!(step.unwrap() <= full / 480 + 2, "{:?}", format_of::<P>());
                assert!(
                    level[480..].iter().all(|&v| v == full),
                    "{:?}",
                    format_of::<P>()
                );
                assert!(level[240] > full / 4, "channel {c} fades in");
            }
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn an_analog_chain_that_changes_the_count_inside_it_is_sent_back() {
        // Two in and two out, but four between: the analog input runs its
        // chain in place, so such a chain must never reach it.
        let (mut rig, feed) = with_capture::<F32>(BitPerfect::Off);
        feed.push(&frames(1000, PERIOD));
        rig.cycle();
        rig.events();
        let narrow = crate::plugin::builtin::create("narrow", RATE, 4);
        let mut plugins = vec![widen(), narrow.unwrap().unwrap()];
        plugins.push(Box::new(Detect::new(RATE)));
        let chain = chain_at::<F32>(plugins, 2);
        assert!(chain.changes_channels());
        assert_eq!((chain.channels_in(), chain.channels_out()), (2, 2));
        rig.send(Command::SetChain(Target::master(Place::Analog), chain));
        feed.push(&frames(1000, PERIOD));
        rig.cycle();
        let back = rig.events().into_iter().find_map(|e| match e {
            Event::OldChain(_, c) => Some(c),
            _ => None,
        });
        assert!(back.is_some_and(|c| c.changes_channels()));
        // The chain in place keeps playing.
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
    }

    /// In f32 only: which chain is refused depends on its counts, not on
    /// the format.
    #[test]
    fn a_mix_chain_of_the_wrong_width_is_sent_back() {
        let mut rig = Rig::<F32>::wide(BitPerfect::Auto, Matrix::default_for(2), 2);
        let mut a = client(1);
        rig.add(&mut a);
        set_chain(&mut rig, Place::Mix, vec![times(2.0).latency(1)]);
        rig.cycle();
        rig.events();
        let refused = |rig: &mut Rig<F32>, place, chain| {
            rig.send(Command::SetChain(Target::master(place), chain));
            a.ring.write(&frames(1000, PERIOD));
            rig.cycle();
            let latencies = old_chains(rig.events());
            assert_eq!(latencies, [Some(7)], "{place:?}");
            // The chain in place keeps playing.
            assert_eq!(rig.last_periods(1), frames(2000, PERIOD));
        };
        // Four channels into a matrix two wide.
        let four = boxed(vec![times(3.0).any().latency(7)]);
        refused(&mut rig, Place::Mix, chain_at::<F32>(four, 4));
        // A mix chain that ends at four, on a stereo sink, beside an output
        // chain that keeps the count.
        set_chain(&mut rig, Place::Output, vec![times(1.0)]);
        rig.cycle();
        rig.events();
        let mut wide = vec![widen()];
        wide.extend(boxed(vec![times(1.0).any().latency(7)]));
        refused(&mut rig, Place::Mix, chain_at::<F32>(wide, 2));
        // An output chain that does not take what the mix chain gives, or
        // does not end at the sink's count.
        let four = boxed(vec![times(1.0).any().latency(7)]);
        refused(&mut rig, Place::Output, chain_at::<F32>(four, 4));
        let mut wide = vec![widen()];
        wide.extend(boxed(vec![times(1.0).any().latency(7)]));
        refused(&mut rig, Place::Output, chain_at::<F32>(wide, 2));
        // An analog chain takes two.
        let four = boxed(vec![times(1.0).any().latency(7)]);
        refused(&mut rig, Place::Analog, chain_at::<F32>(four, 4));
        // An empty chain fits anywhere.
        rig.send(Command::SetChain(
            Target::master(Place::Output),
            Box::new(Chain::empty()),
        ));
        rig.cycle();
        assert_eq!(old_chains(rig.events()), [Some(0)]);
    }

    #[test]
    fn a_mix_chain_that_widens_waits_for_the_output_chain_that_narrows() {
        // Sent in the order the control thread sends them: the mix chain
        // first, while the output chain in place is still empty.
        let mut rig = Rig::<F64>::wide(BitPerfect::Auto, Matrix::default_for(2), 2);
        let mut a = client(1);
        rig.add(&mut a);
        rig.send(Command::SetChain(
            Target::master(Place::Mix),
            chain_at::<F64>(vec![widen()], 2),
        ));
        a.ring.write(&frames(1 << 20, PERIOD));
        rig.cycle();
        assert_eq!(old_chains(rig.events()), [Some(0)]);
        // Four channels for a stereo sink: nothing is routed there.
        assert_eq!(rig.last_periods(1), frames(0, PERIOD));
        let narrow = crate::plugin::builtin::create("narrow", RATE, 4);
        let narrow = narrow.unwrap().unwrap();
        rig.send(Command::SetChain(
            Target::master(Place::Output),
            chain_at::<F64>(vec![narrow], 4),
        ));
        let mut out = Vec::new();
        for _ in 0..10 {
            a.ring.write(&frames(1 << 20, PERIOD));
            rig.cycle();
            out.extend(rig.last_periods(1));
        }
        assert_eq!(old_chains(rig.events()), [Some(0)]);
        // The pair plays, faded in over 10 ms as after a mute.
        assert!(out[0][0] <= (1 << 20) / 480 + 1, "{:?}", out[0]);
        assert!(out[240][0] > 1 << 18);
        assert_eq!(out[480..], frames(1 << 20, 640 - 480));
    }

    #[test]
    fn a_mix_chain_that_widens_alone_at_the_longest_period_is_silent() {
        // Four channels from the chain would not fit the buffers sized for
        // a stereo sink at the longest period: the chain does not run.
        fn check<P: Processing>() {
            let mut rig = Rig::<P>::wide(BitPerfect::Auto, Matrix::default_for(2), 2);
            let longest = MAX_PERIOD as usize;
            rig.send(Command::Reconfigure {
                rate: RATE,
                period: MAX_PERIOD,
            });
            rig.cycle();
            rig.cycle();
            let mut a = client(1);
            rig.add(&mut a);
            rig.send(Command::SetChain(
                Target::master(Place::Mix),
                chain_at::<P>(vec![widen()], 2),
            ));
            for _ in 0..2 {
                a.ring.write(&frames(1 << 20, longest));
                rig.cycle();
            }
            let samples = rig.log.samples();
            let last = &samples[samples.len() - 2 * longest..];
            assert!(last.iter().all(|&v| v == 0), "{:?}", format_of::<P>());
            assert_eq!(a.ring.available(), 0);
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_matrix_wider_than_the_card_narrows_through_the_mix_chain() {
        fn check<P: Processing>() {
            // Players on 1 and 2, the analog input on 3 and 4, which the
            // chain leaves out.
            let apart = matrix(
                &[
                    route(0, 1, 0.0),
                    route(1, 2, 0.0),
                    route(2, 3, 0.0),
                    route(3, 4, 0.0),
                ],
                4,
            );
            let (mut rig, feed) = wide_with_capture::<P>(BitPerfect::Auto, apart, 2);
            let narrow = crate::plugin::builtin::create("narrow", RATE, 4);
            let narrow = narrow.unwrap().unwrap();
            rig.send(Command::SetChain(
                Target::master(Place::Mix),
                chain_at::<P>(vec![narrow], 4),
            ));
            let mut a = client(1);
            rig.add(&mut a);
            // The rig cycled once before the chain came, silent, so the
            // audio fades in over the first 10 ms.
            for _ in 0..9 {
                a.ring.write(&[[1 << 24, -(3 << 20)]; PERIOD]);
                feed.push(&frames(1 << 22, PERIOD));
                rig.cycle();
            }
            let taken = old_chains(rig.events()).into_iter().flatten().count();
            assert_eq!(taken, 2);
            assert_eq!(
                rig.last_periods(1),
                vec![[1 << 24, -(3 << 20)]; PERIOD],
                "{:?}",
                format_of::<P>()
            );
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_dop_stream_plays_on_channels_1_and_2_of_a_wider_card() {
        let mut rig = Rig::<F32>::wide(BitPerfect::Auto, Matrix::default_for(4), 4);
        let mut a = client(1);
        rig.add(&mut a);
        let dop = dop_frames(PERIOD);
        a.ring.write(&dop);
        rig.cycle();
        let want: Vec<Vec<i32>> = dop.iter().map(|f| vec![f[0], f[1], 0, 0]).collect();
        assert_eq!(rig.last_wide(1), want);
    }

    fn wide_cycles_without_allocating<P: Processing>() {
        let (mut rig, feed) = wide_with_capture::<P>(BitPerfect::Auto, doubled(), 4);
        let mut b = client_at(2, 44100);
        let r = Resampler::new(44100, RATE, PERIOD as u32, Quality::High, format_of::<P>());
        b.source.as_mut().unwrap().resampler = Some(Box::new(r));
        let mut a = client(1);
        rig.add(&mut a);
        rig.add(&mut b);
        rig.send(Command::SetChain(
            Target::master(Place::Mix),
            chain_at::<P>(boxed(vec![times(0.5).any().latency(2)]), 4),
        ));
        rig.cycle();
        rig.events();
        let input = frames(1 << 24, PERIOD);
        rig.send(Command::SetGain(0, db_to_gain(-6.0)));
        rig.send(Command::SetChain(
            Target::master(Place::Mix),
            Box::new(chain_at::<P>(boxed(vec![times(0.5).any()]), 4).for_rate(96000)),
        ));
        rig.send(Command::SetChain(
            Target::master(Place::Mix),
            chain_at::<P>(boxed(vec![times(0.5).any()]), 4),
        ));
        let engine = &mut rig.engine;
        assert_no_alloc(|| {
            for _ in 0..200 {
                a.ring.write(&input);
                b.ring.write(&input);
                feed.push(&input);
                engine.cycle().unwrap();
            }
        });
        let last = rig.last_wide(1).pop().unwrap();
        assert!(last.iter().all(|&v| v != 0), "{last:?}");
        assert_eq!(last[0], last[2]);
        assert_eq!(rig.events().len(), 2);
    }

    #[test]
    fn wide_cycles_do_not_allocate() {
        wide_cycles_without_allocating::<I32>();
        wide_cycles_without_allocating::<F32>();
        wide_cycles_without_allocating::<F64>();
    }

    // Bit-perfect and DoP through the matrix.

    /// The S32 values at and next to its edges, and odd ones `f32` cannot
    /// hold.
    const EDGES: [i32; 9] = [
        i32::MIN,
        i32::MIN + 1,
        -ODD,
        -1,
        0,
        1,
        ODD,
        i32::MAX - 1,
        i32::MAX,
    ];

    /// Players left to channel 2 and right to channel 1, at `gain_db`.
    fn swap_at(gain_db: f32) -> Matrix {
        matrix(&[route(0, 2, gain_db), route(1, 1, gain_db)], 2)
    }

    /// Both players' channels into channels 1 and 2, at 0 dB.
    fn mono_sum() -> Matrix {
        matrix(
            &[
                route(0, 1, 0.0),
                route(1, 1, 0.0),
                route(0, 2, 0.0),
                route(1, 2, 0.0),
            ],
            2,
        )
    }

    fn dop_events(events: &[Event]) -> usize {
        count(events, |e| {
            matches!(e, Event::DopStarted(..) | Event::DopStopped(_))
        })
    }

    #[test]
    fn a_swap_matrix_is_bit_perfect_and_swaps() {
        let mut rig = Rig::<F32>::wide(BitPerfect::Auto, swap_at(0.0), 2);
        let mut a = client(1);
        rig.add(&mut a);
        let input: Vec<[i32; 2]> = (0..PERIOD)
            .map(|i| [EDGES[i % EDGES.len()], EDGES[(i + 4) % EDGES.len()]])
            .collect();
        a.ring.write(&input);
        rig.cycle();
        let want: Vec<[i32; 2]> = input.iter().map(|f| [f[1], f[0]]).collect();
        assert_eq!(rig.last_periods(1), want);
        assert_eq!(decide(&rig.engine.situation()), Ok(()));
    }

    #[test]
    fn a_route_at_minus_6_db_is_not_bit_perfect_in_auto() {
        let lowered = matrix(&[route(0, 1, -6.0), route(1, 2, 0.0)], 2);
        let mut rig = Rig::<F64>::wide(BitPerfect::Auto, lowered, 2);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(1 << 24, PERIOD));
        rig.cycle();
        assert_eq!(decide(&rig.engine.situation()), Err(NotBitPerfect::Routing));
        let left = (f64::from(1 << 24) * f64::from(db_to_gain(-6.0))).round() as i32;
        assert_eq!(rig.last_periods(1), vec![[left, -(1 << 24)]; PERIOD]);
    }

    #[test]
    fn a_mono_sum_is_not_bit_perfect() {
        // Not in forced bit-perfect either: two channels into one cannot
        // be copied, so they are mixed, at 0 dB.
        for mode in [BitPerfect::Auto, BitPerfect::Forced] {
            let mut rig = Rig::<F32>::wide(mode, mono_sum(), 2);
            let mut a = client(1);
            rig.add(&mut a);
            a.ring.write(&[[3 << 24, 1 << 20]; PERIOD]);
            rig.cycle();
            assert_eq!(
                decide(&rig.engine.situation()),
                Err(NotBitPerfect::Routing),
                "{mode:?}"
            );
            let sum = (3 << 24) + (1 << 20);
            assert_eq!(rig.last_periods(1), vec![[sum, sum]; PERIOD], "{mode:?}");
        }
    }

    #[test]
    fn the_copy_path_writes_zeros_to_channels_without_a_route() {
        let apart = matrix(&[route(0, 2, 0.0), route(1, 4, 0.0)], 4);
        let mut rig = Rig::<F32>::wide(BitPerfect::Auto, apart, 4);
        let mut a = client(1);
        rig.add(&mut a);
        let input: Vec<[i32; 2]> = (0..PERIOD)
            .map(|i| [EDGES[i % EDGES.len()], EDGES[(i + 2) % EDGES.len()]])
            .collect();
        a.ring.write(&input);
        rig.cycle();
        assert_eq!(decide(&rig.engine.situation()), Ok(()));
        let want: Vec<Vec<i32>> = input.iter().map(|f| vec![0, f[0], 0, f[1]]).collect();
        assert_eq!(rig.last_wide(1), want);
    }

    #[test]
    fn an_impure_analog_route_does_not_stop_bit_perfect_for_a_player() {
        let analog_summed = matrix(
            &[
                route(0, 1, 0.0),
                route(1, 2, 0.0),
                route(2, 1, -6.0),
                route(3, 1, 0.0),
            ],
            2,
        );
        let mut rig = Rig::<F32>::wide(BitPerfect::Auto, analog_summed, 2);
        let (capture, feed) = MemoryCapture::new();
        rig.send(Command::SetCapture(Some(Box::new(capture))));
        // Below the threshold: the analog input is not a source.
        set_chain(&mut rig, Place::Analog, vec![times(1.0).neutral().quiet()]);
        let mut a = client(1);
        rig.add(&mut a);
        let input: Vec<[i32; 2]> = (0..PERIOD)
            .map(|i| [EDGES[i % EDGES.len()], EDGES[(i + 3) % EDGES.len()]])
            .collect();
        a.ring.write(&input);
        feed.push(&frames(1 << 20, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), input);
        assert_eq!(decide(&rig.engine.situation()), Ok(()));
        // The analog input alone, with signal: its own routes decide.
        rig.send(Command::Remove(1));
        set_chain(&mut rig, Place::Analog, vec![times(1.0).neutral()]);
        feed.push(&[[1 << 24, 1 << 20]; PERIOD]);
        rig.cycle();
        assert_eq!(decide(&rig.engine.situation()), Err(NotBitPerfect::Routing));
        let half = f64::from(1 << 24) * f64::from(db_to_gain(-6.0));
        for f in rig.last_periods(1) {
            assert!(
                (f64::from(f[0]) - half - f64::from(1 << 20)).abs() <= 2.0,
                "{f:?}"
            );
            assert_eq!(f[1], 0);
        }
    }

    #[test]
    fn forced_takes_a_route_at_minus_6_db_as_0_db() {
        let mut rig = Rig::<F32>::wide(BitPerfect::Forced, swap_at(-6.0), 2);
        let mut a = client(1);
        rig.add(&mut a);
        let input: Vec<[i32; 2]> = (0..PERIOD)
            .map(|i| [EDGES[i % EDGES.len()], EDGES[(i + 5) % EDGES.len()]])
            .collect();
        a.ring.write(&input);
        rig.cycle();
        assert_eq!(decide(&rig.engine.situation()), Ok(()));
        let want: Vec<[i32; 2]> = input.iter().map(|f| [f[1], f[0]]).collect();
        assert_eq!(rig.last_periods(1), want);
        // A second client is mixed through the matrix at 0 dB too.
        let mut b = client(2);
        rig.add(&mut b);
        a.ring.write(&[[1 << 24, 3 << 20]; PERIOD]);
        b.ring.write(&[[1 << 22, 5 << 16]; PERIOD]);
        rig.cycle();
        assert_eq!(
            decide(&rig.engine.situation()),
            Err(NotBitPerfect::SeveralSources)
        );
        let want = [(3 << 20) + (5 << 16), (1 << 24) + (1 << 22)];
        assert_eq!(rig.last_periods(1), vec![want; PERIOD]);
    }

    #[test]
    fn a_dop_stream_reaches_swapped_channels_unaltered() {
        let mut rig = Rig::<F32>::wide(BitPerfect::Auto, swap_at(0.0), 2);
        let mut a = client(1);
        rig.add(&mut a);
        let dop = dop_frames(PERIOD);
        a.ring.write(&dop);
        rig.cycle();
        let want: Vec<[i32; 2]> = dop.iter().map(|f| [f[1], f[0]]).collect();
        assert_eq!(rig.last_periods(1), want);
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, _)]));
    }

    #[test]
    fn a_dop_stream_is_silent_when_the_routing_is_not_pure() {
        // A sum to mono, and in forced bit-perfect a route at -6 dB: a DoP
        // stream is never scaled, so forced does not take that as 0 dB.
        let cases = [
            (BitPerfect::Auto, mono_sum(), [1 << 20, 1 << 20]),
            (BitPerfect::Forced, mono_sum(), [1 << 20, 1 << 20]),
            (BitPerfect::Forced, swap_at(-6.0), [0, 1 << 20]),
        ];
        for (mode, m, want) in cases {
            let mut rig = Rig::<F32>::wide(mode, m, 2);
            let (mut dop, mut pcm) = (client(1), client(2));
            rig.add(&mut dop);
            rig.add(&mut pcm);
            dop.ring.write(&dop_frames(3 * PERIOD));
            let mut out = Vec::new();
            for _ in 0..3 {
                pcm.ring.write(&[[1 << 20, 0]; PERIOD]);
                rig.cycle();
                out.extend(rig.last_periods(1));
            }
            // Discarded in real time; the other client plays alone.
            assert_eq!(dop.ring.available(), 0, "{mode:?}");
            assert_eq!(out, vec![want; 3 * PERIOD], "{mode:?}");
            assert_eq!(dop_events(&rig.events()), 0, "{mode:?}");
        }
        // Alone, it leaves the card silent.
        let mut rig = Rig::<F32>::wide(BitPerfect::Auto, mono_sum(), 2);
        let mut dop = client(1);
        rig.add(&mut dop);
        dop.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(0, PERIOD));
        assert_eq!(dop_events(&rig.events()), 0);
    }

    #[test]
    fn a_dop_stream_is_silent_behind_a_chain_that_widens() {
        // A four channel card behind a mix chain that widens the matrix's
        // two: the chains are bypassed for DoP, and they would not be.
        let mut rig = Rig::<F32>::closed_with(BitPerfect::Auto, 1024, Matrix::default_for(2), 4);
        rig.send(Command::SetChain(
            Target::master(Place::Mix),
            chain_at::<F32>(vec![widen()], 2),
        ));
        let mut rig = Rig::opened(rig);
        let mut dop = client(1);
        rig.add(&mut dop);
        dop.ring.write(&dop_frames(2 * PERIOD));
        rig.cycle();
        rig.cycle();
        assert_eq!(dop.ring.available(), 0);
        assert!(rig.last_wide(2).iter().all(|f| f == &[0; 4]));
        assert_eq!(dop_events(&rig.events()), 0);
        // On a stereo card a widening chain that arrives while DoP plays
        // ends it: the stream stops owning the output.
        let mut rig = Rig::<F32>::wide(BitPerfect::Auto, Matrix::default_for(2), 2);
        let mut dop = client(1);
        rig.add(&mut dop);
        dop.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), dop_frames(PERIOD));
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, _)]));
        rig.send(Command::SetChain(
            Target::master(Place::Mix),
            chain_at::<F32>(vec![widen()], 2),
        ));
        dop.ring.write(&dop_frames(PERIOD));
        rig.cycle();
        assert_eq!(dop.ring.available(), 0);
        assert_eq!(rig.last_periods(1), frames(0, PERIOD));
        let events = rig.events();
        assert_eq!(count(&events, |e| matches!(e, Event::DopStopped(1))), 1);
        assert_eq!(count(&events, |e| matches!(e, Event::DopStarted(..))), 0);
    }

    /// What a DoP stream played alone for one period left behind.
    struct DopRun {
        input: Vec<[i32; 2]>,
        /// What the card got, frame by frame.
        out: Vec<Vec<i32>>,
        events: Vec<Event>,
        /// Frames still in the stream's ring.
        left: usize,
    }

    /// A DoP stream of one period played alone on a rig of `channels`
    /// through `m`.
    fn dop_through(m: Matrix, channels: usize) -> DopRun {
        let mut rig = Rig::<F32>::wide(BitPerfect::Auto, m, channels);
        let mut dop = client(1);
        rig.add(&mut dop);
        let input = dop_frames(PERIOD);
        dop.ring.write(&input);
        rig.cycle();
        DopRun {
            input,
            out: rig.last_wide(1),
            events: rig.events(),
            left: dop.ring.available(),
        }
    }

    #[test]
    fn a_dop_stream_with_one_side_routed_is_silent() {
        let run = dop_through(matrix(&[route(0, 1, 0.0)], 2), 2);
        assert_eq!(run.out, vec![vec![0, 0]; PERIOD]);
        assert_eq!(run.left, 0);
        assert_eq!(dop_events(&run.events), 0);
    }

    #[test]
    fn a_dop_stream_split_across_two_pairs_is_silent() {
        let split = matrix(&[route(0, 1, 0.0), route(1, 3, 0.0)], 4);
        let run = dop_through(split, 4);
        assert_eq!(run.out, vec![vec![0; 4]; PERIOD]);
        assert_eq!(run.left, 0);
        assert_eq!(dop_events(&run.events), 0);
    }

    #[test]
    fn a_dop_stream_copied_to_both_pairs_plays_unaltered() {
        let both = matrix(
            &[
                route(0, 1, 0.0),
                route(1, 2, 0.0),
                route(0, 3, 0.0),
                route(1, 4, 0.0),
            ],
            4,
        );
        let plays = |m: Matrix, channels: usize, place: fn(&[i32; 2]) -> Vec<i32>| {
            let run = dop_through(m, channels);
            let want: Vec<Vec<i32>> = run.input.iter().map(place).collect();
            assert_eq!(run.out, want);
            assert!(matches!(run.events[..], [Event::DopStarted(1, _)]));
        };
        plays(both, 4, |f| vec![f[0], f[1], f[0], f[1]]);
        // One side on both channels of the pair carries the markers on
        // both, so it plays too.
        let left_twice = matrix(&[route(0, 1, 0.0), route(0, 2, 0.0)], 2);
        plays(left_twice, 2, |f| vec![f[0], f[0]]);
        // A card of one channel takes the left one.
        plays(Matrix::default_for(1), 1, |f| vec![f[0]]);
    }

    #[test]
    fn a_count_changing_chain_is_reported_as_the_reason_it_mixes() {
        // In forced bit-perfect, which bypasses plugins, behind a chain
        // that widens and one that narrows: the copy path cannot place the
        // source, and the situation says so. Automatic mode reports such a
        // chain as a plugin that is not neutral. The daemon refuses this
        // at startup; the engine does not rely on that.
        let mut rig = Rig::<F64>::closed_with(BitPerfect::Forced, 1024, Matrix::default_for(2), 2);
        rig.send(Command::SetChain(
            Target::master(Place::Mix),
            chain_at::<F64>(vec![widen()], 2),
        ));
        let narrow = crate::plugin::builtin::create("narrow", RATE, 4);
        rig.send(Command::SetChain(
            Target::master(Place::Output),
            chain_at::<F64>(vec![narrow.unwrap().unwrap()], 4),
        ));
        let mut rig = Rig::opened(rig);
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(1 << 20, PERIOD));
        rig.cycle();
        assert_eq!(decide(&rig.engine.situation()), Err(NotBitPerfect::Routing));
    }

    /// The copy path and a DoP owner through a swap, under
    /// `assert_no_alloc`.
    fn swapped_cycles_without_allocating<P: Processing>() {
        let mut rig = Rig::<P>::wide(BitPerfect::Auto, swap_at(0.0), 2);
        let (mut a, mut dop) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut dop);
        rig.send(Command::SetRunning(2, false));
        rig.cycle();
        rig.events();
        let pcm = frames(ODD, PERIOD);
        let marked = dop_frames(PERIOD);
        let engine = &mut rig.engine;
        let commands = &mut rig.commands;
        assert_no_alloc(|| {
            for _ in 0..50 {
                a.ring.write(&pcm);
                engine.cycle().unwrap();
            }
            assert!(commands.push(Command::SetRunning(2, true)).is_ok());
            for _ in 0..50 {
                a.ring.write(&pcm);
                dop.ring.write(&marked);
                engine.cycle().unwrap();
            }
        });
        let frames_out = rig.log.frames();
        let n = frames_out.len();
        let copied = &frames_out[n - 51 * PERIOD..n - 50 * PERIOD];
        assert_eq!(copied, vec![[-ODD, ODD]; PERIOD], "{:?}", format_of::<P>());
        let swapped: Vec<[i32; 2]> = marked.iter().map(|f| [f[1], f[0]]).collect();
        assert_eq!(rig.last_periods(1), swapped, "{:?}", format_of::<P>());
        assert!(matches!(rig.events()[..], [Event::DopStarted(2, _)]));
    }

    #[test]
    fn swapped_cycles_do_not_allocate() {
        swapped_cycles_without_allocating::<I32>();
        swapped_cycles_without_allocating::<F32>();
        swapped_cycles_without_allocating::<F64>();
    }

    // A matrix change at run time.

    /// Frames of a 10 ms ramp at `RATE`.
    const RAMP: usize = (RATE * FADE_MS / 1000) as usize;

    /// A level that a 10 ms ramp at `RATE` moves in whole steps of 4096.
    const RAMP_LEVEL: i32 = (RAMP as i32) << 12;

    /// The matrices the engine handed back, in order.
    fn old_matrices(events: &[Event]) -> Vec<Matrix> {
        let old = |e: &Event| match e {
            Event::OldMatrix(_, m) => Some((**m).clone()),
            _ => None,
        };
        events.iter().filter_map(old).collect()
    }

    /// An open rig through a swap, with a client that has played
    /// `[RAMP_LEVEL, 0]` for two periods.
    fn swapped_and_playing<P: Processing>(mode: BitPerfect) -> (Rig<P>, Client) {
        let mut rig = Rig::<P>::wide(mode, swap_at(0.0), 2);
        let mut a = client(1);
        rig.add(&mut a);
        play_level(&mut rig, &mut a, 2);
        rig.events();
        (rig, a)
    }

    /// Plays `n` periods of `[RAMP_LEVEL, 0]` from `a`, and returns what
    /// the card got.
    fn play_level<P: Processing>(rig: &mut Rig<P>, a: &mut Client, n: usize) -> Vec<[i32; 2]> {
        let mut out = Vec::new();
        for _ in 0..n {
            a.ring.write(&[[RAMP_LEVEL, 0]; PERIOD]);
            rig.cycle();
            out.extend(rig.last_periods(1));
        }
        out
    }

    /// `out` follows `want` frame by frame, give or take the rounding of
    /// a gain in `f32`, and no step on either channel, on any frame, is
    /// larger than a 10 ms ramp to `RAMP_LEVEL` allows.
    fn check_ramp(out: &[[i32; 2]], want: impl Fn(usize) -> [f64; 2], what: &str) {
        for (i, f) in out.iter().enumerate() {
            for (c, w) in want(i).into_iter().enumerate() {
                let off = (f64::from(f[c]) - w).abs();
                assert!(off <= 2.0, "{what}: frame {i}, channel {c}: {f:?}, not {w}");
            }
        }
        let bound = i64::from(RAMP_LEVEL) / RAMP as i64 + 2;
        for (i, w) in out.windows(2).enumerate() {
            for (c, (now, next)) in w[0].iter().zip(w[1]).enumerate() {
                let step = (i64::from(next) - i64::from(*now)).abs();
                assert!(
                    step <= bound,
                    "{what}: frame {i}, channel {c}: a step of {step}"
                );
            }
        }
    }

    /// The level of a ramp `RAMP` frames long that starts at frame
    /// `start` of the output, from `from` to `to`.
    fn along(i: usize, start: usize, from: f64, to: f64) -> f64 {
        let share = (i.saturating_sub(start)).min(RAMP) as f64 / RAMP as f64;
        from + (to - from) * share
    }

    #[test]
    fn a_matrix_change_ramps_over_10_ms() {
        fn check<P: Processing>() {
            let what = format!("{:?}", format_of::<P>());
            let (mut rig, mut a) = swapped_and_playing::<P>(BitPerfect::Auto);
            let mut out = rig.last_periods(1);
            assert_eq!(out, vec![[0, RAMP_LEVEL]; PERIOD], "{what}");
            rig.send(Command::SetMatrix(0, Box::new(stereo())));
            out.extend(play_level(&mut rig, &mut a, 10));
            // From the change on, channel 1 rises and channel 2 falls by a
            // 480th of the level per frame, over 480 frames: 7.5 periods.
            let level = f64::from(RAMP_LEVEL);
            let want = |i| {
                let share = along(i, PERIOD, 0.0, 1.0);
                [level * share, level * (1.0 - share)]
            };
            check_ramp(&out, want, &what);
            let after = &out[PERIOD + RAMP..];
            assert!(after.iter().all(|f| *f == [RAMP_LEVEL, 0]), "{what}");
            assert_eq!(old_matrices(&rig.events()), vec![swap_at(0.0)], "{what}");
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_matrix_change_with_nothing_audible_applies_at_once() {
        fn check<P: Processing>() {
            let what = format!("{:?}", format_of::<P>());
            let input: Vec<[i32; 2]> = (0..PERIOD)
                .map(|i| [EDGES[i % EDGES.len()], EDGES[(i + 4) % EDGES.len()]])
                .collect();
            // A client that runs but has nothing to play.
            let mut rig = Rig::<P>::wide(BitPerfect::Auto, swap_at(0.0), 2);
            let mut a = client(1);
            rig.add(&mut a);
            rig.cycle();
            rig.send(Command::SetMatrix(0, Box::new(stereo())));
            a.ring.write(&input);
            rig.cycle();
            // The new routing from the first frame, on the copy path.
            assert_eq!(rig.last_periods(1), input, "{what}");
            assert_eq!(decide(&rig.engine.situation()), Ok(()), "{what}");
            assert_eq!(old_matrices(&rig.events()), vec![swap_at(0.0)], "{what}");
            // An engine whose output is not open.
            let mut rig = Rig::<P>::closed_with(BitPerfect::Auto, 1024, swap_at(0.0), 2);
            rig.send(Command::SetMatrix(0, Box::new(stereo())));
            rig.cycle();
            assert_eq!(old_matrices(&rig.events()), vec![swap_at(0.0)], "{what}");
            let mut rig = Rig::opened(rig);
            let mut a = client(1);
            rig.add(&mut a);
            a.ring.write(&input);
            rig.cycle();
            assert_eq!(rig.last_periods(1), input, "{what}");
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_second_change_during_the_ramp_starts_from_the_gain_reached() {
        fn check<P: Processing>() {
            let what = format!("{:?}", format_of::<P>());
            let (mut rig, mut a) = swapped_and_playing::<P>(BitPerfect::Auto);
            rig.send(Command::SetMatrix(0, Box::new(stereo())));
            let mut out = play_level(&mut rig, &mut a, 2);
            rig.send(Command::SetMatrix(0, Box::new(swap_at(0.0))));
            out.extend(play_level(&mut rig, &mut a, 10));
            // Back from where the first ramp got to, over a whole 10 ms.
            let level = f64::from(RAMP_LEVEL);
            let reached = along(2 * PERIOD, 0, 0.0, 1.0);
            let want = |i| {
                let share = if i < 2 * PERIOD {
                    along(i, 0, 0.0, 1.0)
                } else {
                    along(i, 2 * PERIOD, reached, 0.0)
                };
                [level * share, level * (1.0 - share)]
            };
            check_ramp(&out, want, &what);
            let end = 2 * PERIOD + RAMP;
            assert!(out[end..].iter().all(|f| *f == [0, RAMP_LEVEL]), "{what}");
            let old = old_matrices(&rig.events());
            assert_eq!(old, vec![swap_at(0.0), stereo()], "{what}");
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_matrix_of_another_width_is_sent_back() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        let input: Vec<[i32; 2]> = (0..PERIOD)
            .map(|i| [EDGES[i % EDGES.len()], EDGES[(i + 2) % EDGES.len()]])
            .collect();
        a.ring.write(&input);
        rig.cycle();
        let wider = matrix(&[route(0, 2, 0.0), route(1, 1, 0.0), route(0, 3, 0.0)], 4);
        rig.send(Command::SetMatrix(0, Box::new(wider.clone())));
        a.ring.write(&input);
        rig.cycle();
        assert_eq!(old_matrices(&rig.events()), vec![wider]);
        assert_eq!(rig.engine.master.matrix, stereo());
        // Nothing moved: still the copy path, unswapped.
        assert_eq!(rig.last_periods(2), [input.clone(), input].concat());
        assert_eq!(decide(&rig.engine.situation()), Ok(()));
    }

    #[test]
    fn bit_perfect_resumes_after_the_ramp() {
        fn check<P: Processing>() {
            let what = format!("{:?}", format_of::<P>());
            let mut rig = Rig::<P>::new(BitPerfect::Auto);
            let mut a = client(1);
            rig.add(&mut a);
            let input: Vec<[i32; 2]> = (0..PERIOD)
                .map(|i| [EDGES[i % EDGES.len()], EDGES[(i + 3) % EDGES.len()]])
                .collect();
            a.ring.write(&input);
            rig.cycle();
            assert_eq!(rig.last_periods(1), input, "{what}");
            rig.send(Command::SetMatrix(0, Box::new(swap_at(0.0))));
            let mut out = Vec::new();
            for _ in 0..10 {
                a.ring.write(&input);
                rig.cycle();
                out.extend(rig.last_periods(1));
            }
            let swapped: Vec<[i32; 2]> = input.iter().map(|f| [f[1], f[0]]).collect();
            // Half way the two channels are mixed half and half: the copy
            // path is not used while the ramp runs.
            let mid = out[RAMP / 2];
            let half = |x: i32, y: i32| (f64::from(x) + f64::from(y)) / 2.0;
            let f = input[(RAMP / 2) % PERIOD];
            let want = half(f[0], f[1]);
            for v in mid {
                assert!((f64::from(v) - want).abs() <= 1024.0, "{what}: {mid:?}");
            }
            // From the first period after the ramp, the swapped copy again.
            let after = RAMP.div_ceil(PERIOD) * PERIOD;
            for (k, period) in out[after..].chunks(PERIOD).enumerate() {
                assert_eq!(period, swapped, "{what}: period {k} after the ramp");
            }
            assert_eq!(decide(&rig.engine.situation()), Ok(()), "{what}");
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_matrix_change_during_dop_applies_at_once() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let (mut dop, mut pcm) = (client(1), client(2));
        rig.add(&mut dop);
        rig.add(&mut pcm);
        let marked = dop_frames(PERIOD);
        dop.ring.write(&marked);
        pcm.ring.write(&[[1 << 20, 0]; PERIOD]);
        rig.cycle();
        assert_eq!(rig.last_periods(1), marked);
        assert!(matches!(rig.events()[..], [Event::DopStarted(1, _)]));
        // A swap keeps the stream unaltered: it plays on, swapped, from
        // the first frame after the change, and keeps the output.
        rig.send(Command::SetMatrix(0, Box::new(swap_at(0.0))));
        dop.ring.write(&marked);
        pcm.ring.write(&[[1 << 20, 0]; PERIOD]);
        rig.cycle();
        let swapped: Vec<[i32; 2]> = marked.iter().map(|f| [f[1], f[0]]).collect();
        assert_eq!(rig.last_periods(1), swapped);
        let events = rig.events();
        assert_eq!(dop_events(&events), 0);
        assert_eq!(old_matrices(&events), vec![stereo()]);
        dop.ring.write(&marked);
        pcm.ring.write(&[[1 << 20, 0]; PERIOD]);
        rig.cycle();
        assert_eq!(rig.last_periods(1), swapped);
        // A sum to mono would alter it: it goes silent at once and gives
        // the output up, and the other client plays through the new
        // matrix from the first frame.
        rig.send(Command::SetMatrix(0, Box::new(mono_sum())));
        dop.ring.write(&marked);
        pcm.ring.write(&[[1 << 20, 0]; PERIOD]);
        rig.cycle();
        assert_eq!(dop.ring.available(), 0);
        assert_eq!(rig.last_periods(1), vec![[1 << 20, 1 << 20]; PERIOD]);
        let events = rig.events();
        assert_eq!(count(&events, |e| matches!(e, Event::DopStopped(1))), 1);
        assert_eq!(count(&events, |e| matches!(e, Event::DopStarted(..))), 0);
        assert_eq!(old_matrices(&events), vec![swap_at(0.0)]);
    }

    #[test]
    fn a_dop_stream_waits_for_the_end_of_a_matrix_ramp() {
        // While the gains move, the matrix in effect is neither the old
        // one nor the new one: a DoP stream that starts then, while a PCM
        // client is still heard, is silent and takes no claim until the
        // ramp is over. From the first period after it, it plays unaltered
        // through the new routing.
        let (mut rig, mut a) = swapped_and_playing::<F32>(BitPerfect::Auto);
        rig.send(Command::SetMatrix(0, Box::new(stereo())));
        let mut out = play_level(&mut rig, &mut a, 1);
        let mut dop = client(2);
        rig.add(&mut dop);
        let marked = dop_frames(PERIOD);
        let ramp_periods = RAMP.div_ceil(PERIOD);
        for _ in 1..ramp_periods {
            dop.ring.write(&marked);
            out.extend(play_level(&mut rig, &mut a, 1));
            assert_eq!(dop.ring.available(), 0);
        }
        // Only the client, along the ramp.
        let level = f64::from(RAMP_LEVEL);
        let want = |i| {
            let share = along(i, 0, 0.0, 1.0);
            [level * share, level * (1.0 - share)]
        };
        check_ramp(&out, want, "f32");
        assert_eq!(dop_events(&rig.events()), 0);
        dop.ring.write(&marked);
        a.ring.write(&[[RAMP_LEVEL, 0]; PERIOD]);
        rig.cycle();
        assert_eq!(rig.last_periods(1), marked);
        assert!(matches!(rig.events()[..], [Event::DopStarted(2, _)]));
        dop.ring.write(&marked);
        rig.cycle();
        assert_eq!(rig.last_periods(1), marked);
    }

    #[test]
    fn a_matrix_ramp_ends_when_nothing_is_heard() {
        // The client stops during the ramp: with nothing to fade, the new
        // matrix applies, and a DoP stream that starts in the next cycle
        // plays from its first period.
        let (mut rig, mut a) = swapped_and_playing::<F32>(BitPerfect::Auto);
        rig.send(Command::SetMatrix(0, Box::new(stereo())));
        play_level(&mut rig, &mut a, 1);
        rig.send(Command::SetRunning(1, false));
        let mut dop = client(2);
        rig.add(&mut dop);
        let marked = dop_frames(PERIOD);
        dop.ring.write(&marked);
        rig.cycle();
        assert_eq!(rig.last_periods(1), marked);
        let events = rig.events();
        assert_eq!(dop_events(&events), 1);
        assert_eq!(count(&events, |e| matches!(e, Event::DopStarted(2, _))), 1);
        assert_eq!(rig.engine.master.ramp_left, 0);
    }

    #[test]
    fn a_matrix_ramp_moves_the_analog_bus_too() {
        // The analog input alone, swapped, then the default: its channels
        // cross over 10 ms as a client's do.
        let swapped = matrix(
            &[
                route(0, 1, 0.0),
                route(1, 2, 0.0),
                route(2, 2, 0.0),
                route(3, 1, 0.0),
            ],
            2,
        );
        let (mut rig, feed) = wide_with_capture::<F64>(BitPerfect::Auto, swapped, 2);
        let play = |rig: &mut Rig<F64>, n: usize| {
            let mut out = Vec::new();
            for _ in 0..n {
                feed.push(&[[RAMP_LEVEL, 0]; PERIOD]);
                rig.cycle();
                out.extend(rig.last_periods(1));
            }
            out
        };
        // Past the detector's fade in.
        play(&mut rig, 20);
        assert_eq!(rig.last_periods(1), vec![[0, RAMP_LEVEL]; PERIOD]);
        rig.events();
        rig.send(Command::SetMatrix(0, Box::new(stereo())));
        let out = play(&mut rig, 10);
        let level = f64::from(RAMP_LEVEL);
        let want = |i| {
            let share = along(i, 0, 0.0, 1.0);
            [level * share, level * (1.0 - share)]
        };
        check_ramp(&out, want, "analog");
        assert!(out[RAMP..].iter().all(|f| *f == [RAMP_LEVEL, 0]));
    }

    #[test]
    fn a_period_longer_than_the_ramp_holds_the_new_gains_after_it() {
        fn check<P: Processing>() {
            let what = format!("{:?}", format_of::<P>());
            let long = 1024;
            let mut rig = Rig::<P>::closed_with(BitPerfect::Auto, 1024, swap_at(0.0), 2);
            rig.send(Command::Reconfigure {
                rate: RATE,
                period: long as u32,
            });
            let mut a = client(1);
            rig.add(&mut a);
            let play = |rig: &mut Rig<P>| {
                a.ring.write(&vec![[RAMP_LEVEL, 0]; long]);
                rig.cycle();
                let frames = rig.log.frames();
                frames[frames.len() - long..].to_vec()
            };
            play(&mut rig);
            assert_eq!(play(&mut rig), vec![[0, RAMP_LEVEL]; long], "{what}");
            rig.send(Command::SetMatrix(0, Box::new(stereo())));
            let out = play(&mut rig);
            let level = f64::from(RAMP_LEVEL);
            let want = |i| {
                let share = along(i, 0, 0.0, 1.0);
                [level * share, level * (1.0 - share)]
            };
            check_ramp(&out, want, &what);
            let after = &out[RAMP..];
            assert!(after.iter().all(|f| *f == [RAMP_LEVEL, 0]), "{what}");
            assert_eq!(play(&mut rig), vec![[RAMP_LEVEL, 0]; long], "{what}");
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_reopen_during_a_matrix_ramp_finishes_it() {
        fn check<P: Processing>() {
            let what = format!("{:?}", format_of::<P>());
            let (mut rig, mut a) = swapped_and_playing::<P>(BitPerfect::Auto);
            rig.send(Command::SetMatrix(0, Box::new(stereo())));
            play_level(&mut rig, &mut a, 2);
            rig.send(Command::Reconfigure {
                rate: 44100,
                period: PERIOD as u32,
            });
            // The period that fades out, then the first one at 44.1 kHz.
            let out = play_level(&mut rig, &mut a, 2);
            assert_eq!(rig.engine.rate, 44100, "{what}");
            assert_eq!(out[PERIOD..], [[RAMP_LEVEL, 0]; PERIOD], "{what}");
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn in_forced_bit_perfect_a_matrix_change_moves_routes_only_on_or_off() {
        fn check<P: Processing>() {
            let what = format!("{:?}", format_of::<P>());
            let (mut rig, mut a) = swapped_and_playing::<P>(BitPerfect::Forced);
            // Routes at -6 dB count as 0 dB: the ramp goes all the way.
            let lowered = matrix(
                &[
                    route(0, 1, -6.0),
                    route(1, 2, -6.0),
                    route(2, 1, -6.0),
                    route(3, 2, -6.0),
                ],
                2,
            );
            rig.send(Command::SetMatrix(0, Box::new(lowered)));
            let out = play_level(&mut rig, &mut a, 10);
            let level = f64::from(RAMP_LEVEL);
            let want = |i| {
                let share = along(i, 0, 0.0, 1.0);
                [level * share, level * (1.0 - share)]
            };
            check_ramp(&out, want, &what);
            // The same routes at 0 dB move nothing, so the copy path goes
            // on without a break.
            rig.send(Command::SetMatrix(0, Box::new(stereo())));
            let input: Vec<[i32; 2]> = (0..PERIOD)
                .map(|i| [EDGES[i % EDGES.len()], EDGES[(i + 5) % EDGES.len()]])
                .collect();
            a.ring.write(&input);
            rig.cycle();
            assert_eq!(rig.last_periods(1), input, "{what}");
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    /// Matrix changes, one during the ramp of another, and one sent back,
    /// under `assert_no_alloc`.
    fn matrix_changes_without_allocating<P: Processing>() {
        let (mut rig, a) = swapped_and_playing::<P>(BitPerfect::Auto);
        let level = [[RAMP_LEVEL, 0]; PERIOD];
        let changes = [
            Box::new(stereo()),
            Box::new(swap_at(-6.0)),
            Box::new(Matrix::default_for(4)),
        ];
        let engine = &mut rig.engine;
        let commands = &mut rig.commands;
        assert_no_alloc(|| {
            for change in changes {
                assert!(commands.push(Command::SetMatrix(0, change)).is_ok());
                for _ in 0..4 {
                    a.ring.write(&level);
                    engine.cycle().unwrap();
                }
            }
            for _ in 0..10 {
                a.ring.write(&level);
                engine.cycle().unwrap();
            }
        });
        let gain = f64::from(db_to_gain(-6.0));
        let want = (f64::from(RAMP_LEVEL) * gain).round() as i32;
        let last = rig.last_periods(1);
        for f in last {
            assert!((f[0]).abs() <= 1 && (f[1] - want).abs() <= 1, "{f:?}");
        }
        assert_eq!(old_matrices(&rig.events()).len(), 3);
    }

    #[test]
    fn matrix_changes_do_not_allocate() {
        matrix_changes_without_allocating::<I32>();
        matrix_changes_without_allocating::<F32>();
        matrix_changes_without_allocating::<F64>();
    }

    /// The default matrix passes both buses through, so the analog input is
    /// mixed straight into the players' bus. That gives the same samples as
    /// adding both buses through the matrix, in every format: with two
    /// clients at gains, sums that clip, an output fade and the analog input
    /// at a gain, as captured and through a chain that alters it.
    #[test]
    fn passing_the_buses_through_gives_what_the_matrix_gives() {
        fn run<P: Processing>(through: bool, neutral: bool) -> Vec<[i32; 2]> {
            let (mut rig, feed) = with_capture::<P>(BitPerfect::Off);
            if !neutral {
                set_chain(&mut rig, Place::Analog, vec![times(0.8)]);
            }
            assert!(rig.engine.master.routes.passes);
            rig.engine.master.routes.passes = through;
            let mut a = client(1);
            a.source.as_mut().unwrap().gain = 0.3;
            rig.add(&mut a);
            let mut b = client(2);
            b.source.as_mut().unwrap().gain = db_to_gain(-1.5);
            rig.add(&mut b);
            rig.send(Command::SetAnalogGain(0.6));
            for cycle in 0..12 {
                if cycle == 4 {
                    rig.send(Command::SetGain(0, 0.5));
                }
                a.ring.write(&ramp(PERIOD));
                b.ring.write(&frames(i32::MAX - 5, PERIOD));
                feed.push(&ramp(PERIOD));
                rig.cycle();
            }
            rig.log.frames()
        }
        fn check<P: Processing>() {
            for neutral in [true, false] {
                let through = run::<P>(true, neutral);
                assert_eq!(through, run::<P>(false, neutral), "{:?}", format_of::<P>());
                assert!(through.iter().any(|f| f[0] != 0));
            }
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn a_ramp_ending_on_a_pass_through_matrix_goes_on_by_the_direct_path() {
        // The last cycle of the ramp mixes through the matrix, the next by
        // the direct path, and the samples are those the matrix gives.
        fn run<P: Processing>(direct: bool) -> Vec<[i32; 2]> {
            let (mut rig, a) = swapped_and_playing::<P>(BitPerfect::Off);
            rig.send(Command::SetMatrix(0, Box::new(stereo())));
            let mut out = Vec::new();
            let mut ramped = 0;
            for _ in 0..12 {
                a.ring.write(&ramp(PERIOD));
                rig.cycle();
                out.extend(rig.last_periods(1));
                if rig.engine.master.ramp_left > 0 {
                    ramped += 1;
                }
                if direct {
                    assert!(rig.engine.master.routes.passes);
                } else {
                    rig.engine.master.routes.passes = false;
                }
            }
            // 480 frames: 7.5 periods, then the direct path for the rest.
            assert_eq!((ramped, rig.engine.master.ramp_left), (7, 0));
            out
        }
        fn check<P: Processing>() {
            let direct = run::<P>(true);
            assert_eq!(direct, run::<P>(false), "{:?}", format_of::<P>());
            assert!(direct[PERIOD * 8..].iter().any(|f| f[0] != 0));
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn forced_bit_perfect_takes_a_lowered_stereo_matrix_by_the_direct_path() {
        // Forced bit-perfect counts every route as 0 dB, so a stereo matrix
        // at -6 dB passes through, and the direct path gives the sum as is.
        fn run<P: Processing>(direct: bool) -> Vec<[i32; 2]> {
            let routes = [
                route(0, 1, -6.0),
                route(1, 2, -6.0),
                route(2, 1, -6.0),
                route(3, 2, -6.0),
            ];
            let mut rig = Rig::<P>::wide(BitPerfect::Forced, matrix(&routes, 2), 2);
            assert!(rig.engine.master.routes.passes);
            rig.engine.master.routes.passes = direct;
            let mut a = client(1);
            rig.add(&mut a);
            let mut b = client(2);
            rig.add(&mut b);
            for _ in 0..4 {
                a.ring.write(&frames(3 << 20, PERIOD));
                b.ring.write(&frames(1 << 20, PERIOD));
                rig.cycle();
            }
            rig.log.frames()
        }
        fn check<P: Processing>() {
            let direct = run::<P>(true);
            assert_eq!(direct, run::<P>(false), "{:?}", format_of::<P>());
            // At 0 dB, not lowered: the two clients' sum as it is.
            let last = &direct[direct.len() - PERIOD..];
            assert_eq!(last, frames(4 << 20, PERIOD), "{:?}", format_of::<P>());
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    // Further outputs.

    /// Frames that differ from each other and that every format holds
    /// exactly, so that the processing path gives them as they are.
    fn exact(n: usize) -> Vec<[i32; 2]> {
        (0..n as i32)
            .map(|i| [(ODD & !0xFFF) + (i << 12), -(i << 16)])
            .collect()
    }

    /// Interleaved stereo samples as frames.
    fn stereo_frames(samples: &[i32]) -> Vec<[i32; 2]> {
        samples.chunks_exact(2).map(|f| [f[0], f[1]]).collect()
    }

    /// Adds output `index`, as wide as `matrix`, through it, with a feed
    /// of `frames` frames, and returns the feed's probe.
    fn add_output_with<P: Processing>(
        rig: &mut Rig<P>,
        index: usize,
        matrix: Matrix,
        frames: usize,
    ) -> FeedProbe {
        let channels = matrix.width();
        let (feed, probe) = FeedProbe::new(channels, frames);
        // Further outputs are never bit-perfect: their gains always apply.
        let mix = OutputMix::<P>::new(matrix, channels, false, MAX_PERIOD as usize);
        rig.send(Command::AddOutput(index, Box::new(mix), feed));
        probe
    }

    /// The same with a feed of 16 periods.
    fn add_output<P: Processing>(rig: &mut Rig<P>, index: usize, matrix: Matrix) -> FeedProbe {
        add_output_with(rig, index, matrix, 16 * PERIOD)
    }

    /// One cycle, and what each of `probes` got in it.
    fn cycle_outputs<P: Processing>(
        rig: &mut Rig<P>,
        probes: &mut [&mut FeedProbe],
    ) -> Vec<Vec<[i32; 2]>> {
        rig.cycle();
        probes
            .iter_mut()
            .map(|p| stereo_frames(&p.take()))
            .collect()
    }

    #[test]
    fn every_output_gets_the_same_mix_by_default() {
        fn check<P: Processing>() {
            let what = format_of::<P>();
            let mut rig = Rig::<P>::new(BitPerfect::Auto);
            let mut one = add_output(&mut rig, 1, stereo());
            let mut two = add_output(&mut rig, 2, stereo());
            let (mut a, mut b) = (client(1), client(2));
            rig.add(&mut a);
            // One source at 0 dB: the master copies it, the others mix it.
            for _ in 0..3 {
                a.ring.write(&exact(PERIOD));
                let got = cycle_outputs(&mut rig, &mut [&mut one, &mut two]);
                assert_eq!(rig.last_periods(1), exact(PERIOD), "{what:?}");
                assert_eq!(got, vec![exact(PERIOD); 2], "{what:?}");
            }
            // Two sources, every output turned down: each fades on its own,
            // and they all fade alike.
            rig.add(&mut b);
            for output in 0..3 {
                rig.send(Command::SetGain(output, db_to_gain(-6.0)));
            }
            for _ in 0..12 {
                a.ring.write(&exact(PERIOD));
                b.ring.write(&frames(1 << 20, PERIOD));
                let got = cycle_outputs(&mut rig, &mut [&mut one, &mut two]);
                assert_eq!(got, vec![rig.last_periods(1); 2], "{what:?}");
            }
            let last = rig.last_periods(1)[PERIOD - 1];
            let sum = f64::from(exact(PERIOD)[PERIOD - 1][0] + (1 << 20));
            let want = sum * f64::from(db_to_gain(-6.0));
            let off = (f64::from(last[0]) - want).abs() / want;
            assert!(off < 1e-6, "{what:?}: {last:?}, not {want}");
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    /// Takes a millisecond over each block, and leaves it as it is.
    struct Slow;

    impl Plugin for Slow {
        fn name(&self) -> &str {
            "slow"
        }

        fn formats(&self) -> Formats {
            ALL_FORMATS
        }

        fn params(&self) -> &[Param] {
            &[]
        }

        fn set_param(&mut self, _: usize, _: f64) {}

        fn neutral(&self) -> bool {
            false
        }

        fn latency_frames(&self) -> u32 {
            0
        }

        fn process(&mut self, _: Block<'_>) -> PluginStatus {
            std::thread::sleep(Duration::from_millis(1));
            PluginStatus::default()
        }
    }

    #[test]
    fn every_push_of_a_cycle_carries_the_time_it_woke() {
        let mut rig = Rig::<F64>::new(BitPerfect::Auto);
        let mut one = add_output(&mut rig, 1, stereo());
        let mut two = add_output(&mut rig, 2, stereo());
        // Output 1's chain takes its time, so a push stamped when it is
        // made would show it.
        let slow = Target {
            output: 1,
            place: Place::Mix,
        };
        rig.send(Command::SetChain(
            slow,
            chain_at::<F64>(vec![Box::new(Slow)], 2),
        ));
        let (mut pcm, mut dop) = (client(1), client(2));
        rig.add(&mut pcm);
        for round in 0..4 {
            if round == 2 {
                rig.add(&mut dop);
            }
            pcm.ring.write(&frames(1000, PERIOD));
            dop.ring.write(&dop_frames(PERIOD));
            let before = crate::counters::monotonic_ns();
            rig.cycle();
            let after = crate::counters::monotonic_ns();
            let (frames_one, at_one) = one.last_push();
            let (frames_two, at_two) = two.last_push();
            assert_eq!(frames_one, frames_two);
            assert_eq!(frames_one, (round as u64 + 1) * PERIOD as u64);
            assert_eq!(at_one, at_two, "round {round}");
            assert!(at_one >= before && at_one <= after, "round {round}");
            // The slow chain runs before output 2 is mixed, after the wake.
            if round < 2 {
                assert!(after - at_one >= 1_000_000, "round {round}");
            }
            one.discard();
            two.discard();
        }
    }

    #[test]
    fn a_further_gain_applies_on_its_own() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut one = add_output(&mut rig, 1, stereo());
        let mut two = add_output(&mut rig, 2, stereo());
        let mut a = client(1);
        rig.add(&mut a);
        rig.send(Command::SetGain(2, 0.5));
        a.ring.write(&frames(1000, PERIOD));
        let got = cycle_outputs(&mut rig, &mut [&mut one, &mut two]);
        // Nothing was heard before, so the change applies at once.
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
        assert_eq!(got, vec![frames(1000, PERIOD), frames(500, PERIOD)]);
    }

    #[test]
    fn a_further_matrix_routes_on_its_own() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut one = add_output(&mut rig, 1, stereo());
        let mut two = add_output(&mut rig, 2, stereo());
        rig.send(Command::SetMatrix(2, Box::new(swap_at(0.0))));
        // No output 3, and a matrix of another width for output 1: both
        // come back.
        rig.send(Command::SetMatrix(3, Box::new(swap_at(0.0))));
        rig.send(Command::SetMatrix(1, Box::new(Matrix::default_for(4))));
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&exact(PERIOD));
        let got = cycle_outputs(&mut rig, &mut [&mut one, &mut two]);
        let swapped: Vec<[i32; 2]> = exact(PERIOD).iter().map(|f| [f[1], f[0]]).collect();
        assert_eq!(rig.last_periods(1), exact(PERIOD));
        assert_eq!(got, vec![exact(PERIOD), swapped]);
        let back: Vec<(usize, usize)> = rig
            .events()
            .iter()
            .filter_map(|e| match e {
                Event::OldMatrix(output, m) => Some((*output, m.width())),
                _ => None,
            })
            .collect();
        assert_eq!(back, vec![(2, 2), (3, 2), (1, 4)]);
    }

    #[test]
    fn further_outputs_are_silent_while_dop_plays() {
        fn check<P: Processing>() {
            let what = format_of::<P>();
            let mut rig = Rig::<P>::new(BitPerfect::Auto);
            let mut one = add_output(&mut rig, 1, stereo());
            let mut two = add_output(&mut rig, 2, stereo());
            let (mut pcm, mut dop) = (client(1), client(2));
            rig.add(&mut pcm);
            pcm.ring.write(&frames(1000, PERIOD));
            let got = cycle_outputs(&mut rig, &mut [&mut one, &mut two]);
            assert_eq!(got, vec![frames(1000, PERIOD); 2], "{what:?}");
            rig.add(&mut dop);
            for _ in 0..4 {
                pcm.ring.write(&frames(1000, PERIOD));
                dop.ring.write(&dop_frames(PERIOD));
                let got = cycle_outputs(&mut rig, &mut [&mut one, &mut two]);
                assert_eq!(rig.last_periods(1), dop_frames(PERIOD), "{what:?}");
                assert_eq!(got, vec![frames(0, PERIOD); 2], "{what:?}");
            }
            // The stream stops: the others play the PCM source again.
            rig.send(Command::SetRunning(2, false));
            for _ in 0..12 {
                pcm.ring.write(&frames(1000, PERIOD));
                rig.cycle();
            }
            let mut last = Vec::new();
            for probe in [&mut one, &mut two] {
                last.push(*stereo_frames(&probe.take()).last().unwrap());
            }
            assert_eq!(last, vec![[1000, -1000]; 2], "{what:?}");
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    /// What the master's card gets over `cycles` cycles from `script`,
    /// which writes to the clients each cycle, through a delay line of
    /// `delay` frames.
    fn delayed<P: Processing>(
        mode: BitPerfect,
        delay: u32,
        cycles: usize,
        script: impl Fn(usize, &[Client]),
        sources: usize,
    ) -> Vec<[i32; 2]> {
        let mut rig = Rig::<P>::new(mode);
        let mut clients: Vec<Client> = (0..sources as u64).map(|id| client(id + 1)).collect();
        for c in clients.iter_mut() {
            rig.add(c);
        }
        rig.send(Command::SetDelay(delay));
        rig.cycle();
        let start = rig.log.frames().len();
        for cycle in 0..cycles {
            script(cycle, &clients);
            rig.cycle();
        }
        rig.log.frames()[start..].to_vec()
    }

    #[test]
    fn the_delay_line_delays_by_exact_frames() {
        fn check<P: Processing>(
            mode: BitPerfect,
            sources: usize,
            script: impl Fn(usize, &[Client]) + Copy,
        ) {
            let what = (format_of::<P>(), mode, sources);
            let straight = delayed::<P>(mode, 0, 8, script, sources);
            assert!(straight.iter().any(|f| *f != [0, 0]), "{what:?}");
            for n in [1, 63, 64, 100, 1000] {
                let late = delayed::<P>(mode, n, 8, script, sources);
                let n = (n as usize).min(late.len());
                assert_eq!(late[..n], frames(0, n)[..], "{what:?}, {n}");
                assert_eq!(late[n..], straight[..straight.len() - n], "{what:?}, {n}");
            }
        }
        // An impulse with the low bits set, copied.
        let impulse = |cycle: usize, clients: &[Client]| {
            let mut block = frames(0, PERIOD);
            if cycle == 1 {
                block[10] = [ODD, i32::MIN + 5];
            }
            clients[0].ring.write(&block);
        };
        // Two sources, mixed.
        let mixed = |_: usize, clients: &[Client]| {
            clients[0].ring.write(&ramp(PERIOD));
            clients[1].ring.write(&frames(1 << 20, PERIOD));
        };
        // A DoP stream.
        let dop = |_: usize, clients: &[Client]| {
            clients[0].ring.write(&dop_frames(PERIOD));
        };
        check::<I32>(BitPerfect::Auto, 1, impulse);
        check::<F32>(BitPerfect::Auto, 1, impulse);
        check::<F64>(BitPerfect::Off, 2, mixed);
        check::<F32>(BitPerfect::Auto, 2, mixed);
        check::<I32>(BitPerfect::Auto, 1, dop);
        check::<F32>(BitPerfect::Off, 1, dop);
    }

    #[test]
    fn the_delay_line_delays_the_master_only() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut one = add_output(&mut rig, 1, stereo());
        rig.send(Command::SetDelay(10));
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&ramp(PERIOD));
        let got = cycle_outputs(&mut rig, &mut [&mut one]);
        assert_eq!(got, vec![ramp(PERIOD)]);
        let mut want = frames(0, 10);
        want.extend_from_slice(&ramp(PERIOD)[..PERIOD - 10]);
        assert_eq!(rig.last_periods(1), want);
    }

    #[test]
    fn a_delay_change_while_playing_fades_out_and_in() {
        let mut rig = Rig::<F64>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        play_level(&mut rig, &mut a, 2);
        rig.send(Command::SetDelay(100));
        let out = play_level(&mut rig, &mut a, 40);
        // Down to silence, then back to the level, never by a step larger
        // than a 10 ms fade takes.
        let bound = i64::from(RAMP_LEVEL) / RAMP as i64 + 2;
        for (i, w) in out.windows(2).enumerate() {
            let step = (i64::from(w[1][0]) - i64::from(w[0][0])).abs();
            assert!(step <= bound, "frame {i}: a step of {step}");
        }
        let silent = out.iter().position(|f| f[0] == 0).expect("a silence");
        assert!(
            (RAMP - 1..=RAMP + 1).contains(&silent),
            "silent at {silent}"
        );
        assert_eq!(out.last(), Some(&[RAMP_LEVEL, 0]));
        // The delay holds 100 frames of the silence.
        let zeros = out.iter().filter(|f| f[0] == 0).count();
        assert!(zeros >= 100, "{zeros} frames of silence");
    }

    #[test]
    fn a_full_feed_never_waits() {
        fn run(capacity: usize) -> (Vec<[i32; 2]>, [u64; 2]) {
            let mut rig = Rig::<F32>::new(BitPerfect::Auto);
            let mut one = add_output_with(&mut rig, 1, stereo(), capacity);
            let mut two = add_output_with(&mut rig, 2, stereo(), capacity);
            let (mut a, mut b) = (client(1), client(2));
            rig.add(&mut a);
            rig.add(&mut b);
            rig.send(Command::SetGain(0, db_to_gain(-6.0)));
            let start = Instant::now();
            for _ in 0..100 {
                a.ring.write(&ramp(PERIOD));
                b.ring.write(&frames(1 << 20, PERIOD));
                rig.cycle();
                one.take();
                two.take();
            }
            assert!(start.elapsed() < Duration::from_secs(1));
            (rig.log.frames(), [one.overflows(), two.overflows()])
        }
        let (refused, overflows) = run(0);
        assert_eq!(overflows, [100, 100]);
        let (taken, overflows) = run(16 * PERIOD);
        assert_eq!(overflows, [0, 0]);
        assert_eq!(refused, taken);
    }

    fn feeding_without_allocating<P: Processing>() {
        let what = format_of::<P>();
        let mut rig = Rig::<P>::new(BitPerfect::Auto);
        let mut one = add_output(&mut rig, 1, stereo());
        let mut two = add_output(&mut rig, 2, swap_at(-3.0));
        let (mut a, mut b, mut dop) = (client(1), client(2), client(3));
        rig.add(&mut a);
        rig.add(&mut b);
        rig.add(&mut dop);
        rig.send(Command::SetRunning(2, false));
        rig.send(Command::SetRunning(3, false));
        rig.cycle();
        rig.events();
        // Everything the audio thread will be handed is made out here.
        let target = Target {
            output: 2,
            place: Place::Mix,
        };
        let forced = false;
        let spare = OutputMix::<P>::new(stereo(), 2, forced, MAX_PERIOD as usize);
        let (feed, mut three) = FeedProbe::new(2, 16 * PERIOD);
        let late: [Command; 8] = [
            Command::SetGain(1, db_to_gain(-6.0)),
            Command::SetMatrix(2, Box::new(swap_at(-6.0))),
            Command::SetChain(target, chain_of::<P>(vec![times(0.5)])),
            Command::SetParam {
                target,
                plugin: 0,
                index: 0,
                value: 0.25,
            },
            Command::AddOutput(3, Box::new(spare), feed),
            Command::SetDelay(100),
            Command::RemoveOutput(1),
            Command::SetRunning(2, true),
        ];
        let (pcm, marked) = (frames(1 << 24, PERIOD), dop_frames(PERIOD));
        let engine = &mut rig.engine;
        let commands = &mut rig.commands;
        assert_no_alloc(|| {
            for _ in 0..20 {
                a.ring.write(&pcm);
                b.ring.write(&pcm);
                engine.cycle().unwrap();
                one.discard();
                two.discard();
            }
            for command in late {
                assert!(commands.push(command).is_ok());
            }
            for round in 0..100 {
                a.ring.write(&pcm);
                b.ring.write(&pcm);
                if round == 50 {
                    assert!(commands.push(Command::SetRunning(3, true)).is_ok());
                }
                if round >= 50 {
                    dop.ring.write(&marked);
                }
                engine.cycle().unwrap();
                two.discard();
                three.discard();
            }
        });
        assert_eq!(rig.last_periods(1), marked, "{what:?}");
        assert!(
            rig.log
                .frames()
                .iter()
                .any(|f| f[0] != 0 && f[0] != marked[0][0])
        );
        assert_eq!(one.overflows() + two.overflows() + three.overflows(), 0);
        let events = rig.events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::OldOutput(1, _, _))),
            "{what:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, Event::OldChain(2, _))),
            "{what:?}"
        );
    }

    #[test]
    fn feeding_outputs_does_not_allocate() {
        feeding_without_allocating::<I32>();
        feeding_without_allocating::<F32>();
        feeding_without_allocating::<F64>();
    }

    #[test]
    fn a_chain_for_output_2_never_runs_on_output_1() {
        fn check<P: Processing>() {
            let what = format_of::<P>();
            let mut rig = Rig::<P>::new(BitPerfect::Auto);
            let mut one = add_output(&mut rig, 1, stereo());
            let mut two = add_output(&mut rig, 2, stereo());
            let mix = |output| Target {
                output,
                place: Place::Mix,
            };
            rig.send(Command::SetChain(mix(2), chain_of::<P>(vec![times(0.5)])));
            // Refused: the analog input is the master's, there is no
            // output 3, and a chain for 4 channels fits no stereo mix.
            let analog = Target {
                output: 1,
                place: Place::Analog,
            };
            rig.send(Command::SetChain(analog, chain_of::<P>(vec![times(0.0)])));
            rig.send(Command::SetChain(mix(3), chain_of::<P>(vec![times(0.0)])));
            let wide = chain_at::<P>(boxed(vec![times(0.0).any()]), 4);
            rig.send(Command::SetChain(mix(1), wide));
            let mut a = client(1);
            rig.add(&mut a);
            a.ring.write(&frames(1 << 20, PERIOD));
            let got = cycle_outputs(&mut rig, &mut [&mut one, &mut two]);
            assert_eq!(rig.last_periods(1), frames(1 << 20, PERIOD), "{what:?}");
            assert_eq!(
                got,
                vec![frames(1 << 20, PERIOD), frames(1 << 19, PERIOD)],
                "{what:?}"
            );
            let back: Vec<usize> = rig
                .events()
                .iter()
                .filter_map(|e| match e {
                    Event::OldChain(output, _) => Some(*output),
                    _ => None,
                })
                .collect();
            assert_eq!(back, vec![2, 1, 3, 1], "{what:?}");
            // A chain built for another rate silences its own output only,
            // which fades back in once one for this rate comes.
            let at = |rate| Box::new((*chain_of::<P>(vec![times(0.5)])).for_rate(rate));
            rig.send(Command::SetChain(mix(2), at(96000)));
            for _ in 0..2 {
                a.ring.write(&frames(1 << 20, PERIOD));
                let got = cycle_outputs(&mut rig, &mut [&mut one, &mut two]);
                assert_eq!(
                    got,
                    vec![frames(1 << 20, PERIOD), frames(0, PERIOD)],
                    "{what:?}"
                );
                assert_eq!(rig.last_periods(1), frames(1 << 20, PERIOD), "{what:?}");
            }
            rig.send(Command::SetChain(mix(2), at(RATE)));
            let mut faded = Vec::new();
            for _ in 0..12 {
                a.ring.write(&frames(1 << 20, PERIOD));
                let got = cycle_outputs(&mut rig, &mut [&mut one, &mut two]);
                faded.extend(got[1].iter().copied());
            }
            assert!(faded[0][0].abs() < 1 << 19, "{what:?}");
            assert_eq!(faded.last(), Some(&[1 << 19, -(1 << 19)]), "{what:?}");
        }
        check::<I32>();
        check::<F32>();
        check::<F64>();
    }

    #[test]
    fn an_output_of_another_format_comes_back_unplayed() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let (feed, mut probe) = FeedProbe::new(2, 16 * PERIOD);
        let mix = OutputMix::<F32>::new(stereo(), 2, false, MAX_PERIOD as usize);
        rig.send(Command::AddOutput(1, Box::new(mix), feed));
        // Nor is there an output 0 to add, or a feed of another width.
        let (feed, _zero) = FeedProbe::new(2, 16 * PERIOD);
        let mix = OutputMix::<I32>::new(stereo(), 2, false, MAX_PERIOD as usize);
        rig.send(Command::AddOutput(0, Box::new(mix), feed));
        let (feed, _wide) = FeedProbe::new(4, 16 * PERIOD);
        let mix = OutputMix::<I32>::new(stereo(), 2, false, MAX_PERIOD as usize);
        rig.send(Command::AddOutput(2, Box::new(mix), feed));
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
        assert!(probe.take().is_empty());
        let back: Vec<usize> = rig
            .events()
            .iter()
            .filter_map(|e| match e {
                Event::OldOutput(index, _, _) => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(back, vec![1, 0, 2]);
    }

    #[test]
    fn a_removed_output_comes_back_and_is_fed_no_more() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let mut one = add_output(&mut rig, 1, stereo());
        let mut a = client(1);
        rig.add(&mut a);
        a.ring.write(&frames(1000, PERIOD));
        let got = cycle_outputs(&mut rig, &mut [&mut one]);
        assert_eq!(got, vec![frames(1000, PERIOD)]);
        rig.send(Command::RemoveOutput(1));
        rig.send(Command::RemoveOutput(2));
        a.ring.write(&frames(1000, PERIOD));
        let got = cycle_outputs(&mut rig, &mut [&mut one]);
        assert_eq!(got, vec![Vec::<[i32; 2]>::new()]);
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
        let events = rig.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::OldOutput(1, _, _)));
    }

    /// The last period the card gets before a reconfigure closes it, with
    /// the master's delay line at `delay` frames.
    fn last_before_reopen(delay: u32) -> Vec<[i32; 2]> {
        let mut rig = Rig::<F64>::new(BitPerfect::Auto);
        rig.send(Command::SetDelay(delay));
        let mut a = client(1);
        rig.add(&mut a);
        for _ in 0..6 {
            a.ring.write(&frames(1 << 24, PERIOD));
            rig.cycle();
        }
        a.ring.write(&frames(1 << 24, PERIOD));
        rig.send(Command::Reconfigure {
            rate: 96000,
            period: PERIOD as u32,
        });
        rig.cycle();
        assert_eq!(rig.log.closes(), 0);
        let last = rig.last_periods(1);
        rig.cycle();
        assert_eq!(rig.log.closes(), 1);
        last
    }

    #[test]
    fn a_reconfigure_fades_what_leaves_the_delay_line() {
        let step = (1 << 24) / PERIOD as i32;
        for delay in [0, 32, 64, 200] {
            let period = last_before_reopen(delay);
            // Down to the fade's last step, as at delay 0, by no larger
            // steps.
            let last = period[PERIOD - 1][0];
            assert!((last - step).abs() <= 1, "delay {delay}: {last}");
            for w in period.windows(2) {
                assert!(w[0][0] - w[1][0] <= step + 1, "delay {delay}: {w:?}");
                assert!(w[1][0] <= w[0][0], "delay {delay}: {w:?}");
            }
            assert_eq!(period[0][0], 1 << 24, "delay {delay}");
        }
        // At delay 0 the fade is as it always was.
        assert_eq!(last_before_reopen(0)[PERIOD - 1], [step, -step]);
    }

    #[test]
    fn closing_fades_what_the_delay_line_holds() {
        // At delay 0 a close cuts at once, as before there was a line.
        for delay in [0, 200] {
            let mut rig = Rig::<F64>::new(BitPerfect::Auto);
            rig.send(Command::SetDelay(delay));
            let mut a = client(1);
            rig.add(&mut a);
            for _ in 0..6 {
                a.ring.write(&frames(1 << 24, PERIOD));
                rig.cycle();
            }
            let written = rig.log.frames().len();
            rig.send(Command::CloseOutput);
            a.ring.write(&frames(1 << 24, PERIOD));
            rig.cycle();
            rig.cycle();
            assert_eq!(rig.log.closes(), 1, "delay {delay}");
            let got = rig.log.frames();
            if delay == 0 {
                assert_eq!(got.len(), written);
            } else {
                // One faded period, then the close.
                assert_eq!(got.len(), written + PERIOD);
                let step = (1 << 24) / PERIOD as i32;
                assert_eq!(got.last(), Some(&[step, -step]));
            }
        }
    }

    #[test]
    fn a_delay_beyond_the_capacity_is_the_capacity() {
        let script = |_: usize, clients: &[Client]| clients[0].ring.write(&ramp(PERIOD));
        let script = move |c: usize, clients: &[Client]| {
            script(c, clients);
        };
        let beyond = delayed::<I32>(BitPerfect::Auto, 5000, 20, script, 1);
        let full = delayed::<I32>(BitPerfect::Auto, DELAY as u32, 20, script, 1);
        assert_eq!(beyond, full);
        assert_eq!(full[..DELAY], frames(0, DELAY)[..]);
        assert_eq!(full[DELAY], ramp(PERIOD)[0]);
    }

    #[test]
    fn an_engine_without_a_delay_line_takes_no_delay() {
        let sink = MemorySink::with_channels(2);
        let log = sink.log();
        let (mut commands, consumer) = RingBuffer::new(16);
        let (producer, _events) = RingBuffer::new(16);
        let counters = Arc::new(Counters::default());
        let mode = BitPerfect::Auto;
        let mut engine =
            Engine::<I32, _>::new(sink, consumer, producer, counters, mode, stereo(), 0);
        let reconfigure = Command::Reconfigure {
            rate: RATE,
            period: PERIOD as u32,
        };
        assert!(commands.push(reconfigure).is_ok());
        assert!(commands.push(Command::SetDelay(100)).is_ok());
        let mut a = client(1);
        assert!(
            commands
                .push(Command::Add(a.source.take().unwrap()))
                .is_ok()
        );
        assert!(commands.push(Command::SetRunning(1, true)).is_ok());
        a.ring.write(&ramp(PERIOD));
        engine.cycle().unwrap();
        assert_eq!(log.frames(), ramp(PERIOD));
    }

    #[test]
    fn a_reopen_empties_the_delay_line() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        rig.send(Command::SetDelay(100));
        let mut a = client(1);
        rig.add(&mut a);
        for _ in 0..4 {
            a.ring.write(&frames(1 << 24, PERIOD));
            rig.cycle();
        }
        rig.send(Command::Reconfigure {
            rate: RATE,
            period: PERIOD as u32,
        });
        rig.cycle();
        rig.cycle();
        assert_eq!(rig.log.opens().len(), 2);
        // The first 100 frames after the reopen are the empty line, not
        // what it held before.
        let after = rig.last_periods(1);
        assert_eq!(after, frames(0, PERIOD));
        a.ring.write(&frames(1 << 20, 2 * PERIOD));
        rig.cycle();
        rig.cycle();
        let got = rig.last_periods(2);
        assert_eq!(got[..100 - PERIOD], frames(0, 100 - PERIOD)[..]);
        assert!(
            got[100 - PERIOD..]
                .iter()
                .all(|f| f[0] == 0 || f[0] == 1 << 20)
        );
    }

    #[test]
    fn a_closed_feed_takes_nothing_and_never_waits() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let mut one = add_output(&mut rig, 1, stereo());
        one.close();
        let mut a = client(1);
        rig.add(&mut a);
        for _ in 0..10 {
            a.ring.write(&frames(1000, PERIOD));
            rig.cycle();
        }
        assert_eq!(rig.last_periods(10), frames(1000, 10 * PERIOD));
        assert!(one.take().is_empty());
        assert_eq!(one.overflows(), 0);
        assert_eq!(one.last_push().0, 0);
    }

    #[test]
    fn in_forced_bit_perfect_further_outputs_keep_their_gains_and_chains() {
        let mut rig = Rig::<F64>::new(BitPerfect::Forced);
        let mut one = add_output(&mut rig, 1, stereo());
        let mut two = add_output(&mut rig, 2, stereo());
        // Output 1: its own gain and a chain. Output 2: a matrix at -6 dB.
        rig.send(Command::SetGain(1, 0.5));
        let mix = Target {
            output: 1,
            place: Place::Mix,
        };
        rig.send(Command::SetChain(mix, chain_of::<F64>(vec![times(0.5)])));
        let lowered = matrix(&[route(0, 1, -6.0), route(1, 2, -6.0)], 2);
        rig.send(Command::SetMatrix(2, Box::new(lowered)));
        // The master's gain and a client's volume stay unapplied.
        rig.send(Command::SetGain(0, 0.5));
        let mut a = client(1);
        a.source.as_mut().unwrap().gain = 0.5;
        rig.add(&mut a);
        a.ring.write(&frames(1 << 20, PERIOD));
        let got = cycle_outputs(&mut rig, &mut [&mut one, &mut two]);
        assert_eq!(rig.last_periods(1), frames(1 << 20, PERIOD));
        assert_eq!(got[0], frames(1 << 18, PERIOD));
        let want = (f64::from(1 << 20) * f64::from(db_to_gain(-6.0))).round() as i32;
        assert_eq!(got[1], frames(want, PERIOD));
    }
}
