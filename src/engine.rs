//! The audio thread's work: one period per `cycle`.
//!
//! The control thread owns every decision and sends them as commands. This
//! code only carries them out. It does not allocate, free or lock. Whatever
//! it would free goes back to the control thread in an `Event`.

use crate::bitperfect::{Situation, decide, volumes_apply};
use crate::config::BitPerfect;
use crate::counters::Counters;
use crate::dop;
use crate::format::{Processing, Ramp, mix_add, mix_clear, mix_finish};
use crate::latency::{MAX_PERIOD, PERIODS};
use crate::rate::ClientId;
use crate::resample::Resampler;
use crate::sink::{Sink, SinkError};
use aas_client::ring::Ring;
use rtrb::{Consumer, Producer, PushError};
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Sources the table of a new engine has room for. The control thread sends
/// `Grow` before it adds more.
pub const INITIAL_SOURCES: usize = 16;

/// Length of a gain change.
const FADE_MS: u32 = 10;

/// Events held back while the queue is full.
const BACKLOG: usize = 8;
/// The most events one command can cause, with room for `Reconfigured`.
const EVENTS_PER_COMMAND: usize = 3;

/// What a client's stream has been seen to be.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// Too few frames so far to tell.
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
    /// Frames to read from the ring this cycle, at most a period.
    ready: usize,
    dop_reported: bool,
    drained_reported: bool,
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
            dop_reported: false,
            drained_reported: false,
        }
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

    /// Whether this source can contribute audio this cycle.
    fn has_input(&self) -> bool {
        match &self.resampler {
            Some(r) => (self.ready > 0 || self.draining()) && r.ready(&self.ring),
            None => self.ready > 0,
        }
    }

    /// Counts an underrun if `got` falls short of `want`. Running out at the
    /// end of a drain is not one.
    fn note_shortfall(&self, got: usize, want: usize) {
        if got < want && !self.draining() {
            let underruns = &self.ring.header().underruns;
            underruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Fills `into` from the ring, padding with zeros.
    fn read_raw(&self, into: &mut [[i32; 2]]) {
        let n = self.ring.read(&mut into[..self.ready]);
        into[n..].fill([0; 2]);
        self.note_shortfall(n, into.len());
    }

    /// Fills `into` with a period at the output rate.
    fn read_resampled(&mut self, into: &mut [[i32; 2]]) {
        let Some(resampler) = self.resampler.as_mut() else {
            return self.read_raw(into);
        };
        let n = resampler.process(&self.ring, into);
        into[n..].fill([0; 2]);
        self.note_shortfall(n, into.len());
    }

    /// Drops a period's worth of input, keeping the client in real time.
    fn discard(&mut self, period: usize) {
        let want = match self.resampler.as_mut() {
            Some(r) => {
                r.skip();
                r.input_frames_next()
            }
            None => period,
        };
        let n = self.ring.available().min(want);
        self.ring.consume(n);
        self.note_shortfall(n, want);
    }

    /// Tells the client how much plays after its ring, and that it has moved.
    fn notify(&self, sink_delay: u32, output_rate: u32) {
        let filter = self.resampler.as_ref().map_or(0, |r| r.delay_frames());
        let delay = u64::from(sink_delay + filter) * u64::from(self.rate) / u64::from(output_rate);
        let header = self.ring.header();
        header
            .output_delay_frames
            .store(delay as u32, Ordering::Relaxed);
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

/// Decisions from the control thread, carried out in order.
pub enum Command {
    /// Refused with `Event::Dropped` if the table is full.
    Add(Box<Source>),
    Remove(ClientId),
    SetRunning(ClientId, bool),
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
    Shutdown,
}

/// What the audio thread hands back or has noticed.
pub enum Event {
    Dropped(Box<Source>),
    Replaced(Option<Box<Resampler>>),
    OldTable(Vec<Box<Source>>),
    DopStarted(ClientId),
    DopStopped(ClientId),
    Drained(ClientId),
    Reconfigured,
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
    last_start: Option<Instant>,
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
            scratch: vec![[0; 2]; frames],
            acc: vec![[P::zero(); 2]; frames],
            rate: 0,
            period: 0,
            open: false,
            reopen: None,
            faded: false,
            stopped: false,
            gain: 1.0,
            target_gain: 1.0,
            fade_left: 0,
            last_start: None,
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

    /// Carries out queued commands and, with an open sink, plays one period.
    ///
    /// An error from the sink comes after the rest of the cycle is done. A
    /// failed open is retried on the next call.
    pub fn cycle(&mut self) -> Result<(), SinkError> {
        if self.stopped {
            return Ok(());
        }
        let start = Instant::now();
        self.flush_backlog();
        let result = if self.open && !self.faded && self.reconfigure_queued() {
            self.faded = true;
            self.render(start, true)
        } else {
            self.drain_commands();
            self.apply_reopen()?;
            if self.open {
                self.render(start, false)
            } else {
                Ok(())
            }
        };
        self.report();
        result
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
            Command::SetRunning(id, running) => {
                if let Some(source) = self.find(id) {
                    source.running = running;
                    if !running {
                        source.stream = Stream::Unknown;
                    }
                }
            }
            Command::SetResampler(id, resampler) => {
                let replaced = match self.find(id) {
                    Some(source) => std::mem::replace(&mut source.resampler, resampler),
                    None => resampler,
                };
                self.emit(Event::Replaced(replaced));
            }
            Command::Reconfigure { rate, period } => {
                self.reopen = Some((rate, period.min(MAX_PERIOD)));
                self.faded = false;
            }
            Command::CloseOutput => {
                self.reopen = None;
                self.close();
            }
            Command::SetGain(gain) => self.set_gain(gain),
            Command::Grow(table) => self.grow(table),
            Command::Shutdown => {
                self.reopen = None;
                self.close();
                self.stopped = true;
            }
        }
    }

    fn find(&mut self, id: ClientId) -> Option<&mut Source> {
        let found = self.table.iter_mut().find(|s| s.id == id);
        found.map(|s| s.as_mut())
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

    fn close(&mut self) {
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
        self.period = period as usize;
        self.last_start = None;
        self.emit(Event::Reconfigured);
        Ok(())
    }

    fn set_gain(&mut self, gain: f32) {
        if gain == self.target_gain {
            return;
        }
        self.target_gain = gain;
        if self.open {
            self.fade_left = (self.rate * FADE_MS / 1000) as usize;
        } else {
            self.finish_fade();
        }
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
    fn render(&mut self, start: Instant, fade_out: bool) -> Result<(), SinkError> {
        self.scan();
        let rate = self.rate;
        let owner = self
            .table
            .iter()
            .position(|s| s.running && s.stream == Stream::Dop && s.ready > 0 && s.rate == rate);
        match owner {
            Some(owner) => self.play_dop(owner),
            None => self.play_pcm(fade_out),
        }
        let before_write = Instant::now();
        let result = self.sink.write(&self.out[..self.period]);
        let after_write = Instant::now();
        if result == Err(SinkError::Xrun) {
            self.counters.xruns.fetch_add(1, Ordering::Relaxed);
        }
        let sink_delay = self.sink.delay_frames();
        for source in self.table.iter().filter(|s| s.running) {
            source.notify(sink_delay, rate);
        }
        let work = before_write - start + after_write.elapsed();
        self.record_timing(start, work.as_nanos() as u64);
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

    /// Looks at what each running source has, without consuming it.
    fn scan(&mut self) {
        let window = &mut self.scratch[..self.period];
        for source in self.table.iter_mut().filter(|s| s.running) {
            let draining = source.draining();
            let n = source.ring.peek(window);
            if n >= dop::MIN_FRAMES {
                source.stream = if dop::is_dop(&window[..n]) {
                    Stream::Dop
                } else {
                    Stream::Pcm
                };
            }
            // Until the stream is known, its frames must not reach the mix.
            let wait = source.stream == Stream::Unknown && !draining;
            source.ready = if wait { 0 } else { n };
        }
    }

    /// DoP has the output to itself and nothing touches its samples.
    fn play_dop(&mut self, owner: usize) {
        self.finish_fade();
        let period = self.period;
        for (index, source) in self.table.iter_mut().enumerate() {
            if index == owner {
                source.read_raw(&mut self.out[..period]);
            } else if source.running {
                source.discard(period);
            }
        }
    }

    /// What the bit-perfect rules need to know about this cycle.
    fn situation(&self) -> Situation {
        let rate = self.rate;
        let audible = || {
            let sources = self.table.iter();
            sources.filter(move |s| s.running && !s.muted(rate) && s.has_input())
        };
        Situation {
            mode: self.mode,
            active: audible().count(),
            software_volume_at_0db: self.gain == 1.0
                && self.target_gain == 1.0
                && audible().all(|s| s.gain == 1.0),
            resampled: audible().any(|s| s.resampler.is_some()),
        }
    }

    fn play_pcm(&mut self, fade_out: bool) {
        let (rate, period) = (self.rate, self.period);
        let volumes = volumes_apply(self.mode);
        let copy = !fade_out && decide(&self.situation()).is_ok();
        let (next_gain, fade_frames) = self.next_fade_step();
        let (from, to) = if volumes {
            (self.gain, next_gain)
        } else {
            (1.0, 1.0)
        };
        let out = &mut self.out[..period];
        let scratch = &mut self.scratch[..period];
        let acc = &mut self.acc[..period];
        if !copy {
            mix_clear::<P>(acc);
        }
        for source in self.table.iter_mut().filter(|s| s.running) {
            if source.muted(rate) {
                source.discard(period);
            } else if !source.has_input() {
                source.note_shortfall(0, 1);
            } else if copy {
                source.read_raw(out);
            } else {
                source.read_resampled(scratch);
                let level = if volumes { source.gain } else { 1.0 };
                if fade_out {
                    let ramp = Ramp {
                        from: from * level,
                        to: 0.0,
                    };
                    mix_add::<P>(scratch, ramp, acc);
                } else {
                    let (fading, steady) = scratch.split_at(fade_frames);
                    let (acc_fading, acc_steady) = acc.split_at_mut(fade_frames);
                    let ramp = Ramp {
                        from: from * level,
                        to: to * level,
                    };
                    mix_add::<P>(fading, ramp, acc_fading);
                    mix_add::<P>(steady, Ramp::flat(to * level), acc_steady);
                }
            }
        }
        if !copy {
            mix_finish::<P>(acc, out);
        }
        self.gain = next_gain;
        self.fade_left -= fade_frames;
    }

    /// Sends the events that follow from the state of the sources. These need
    /// no storage: what the queue will not take now is sent on a later cycle.
    fn report(&mut self) {
        if !self.backlog.is_empty() {
            return;
        }
        for source in self.table.iter_mut() {
            let dop = source.stream == Stream::Dop;
            if dop != source.dop_reported {
                let event = if dop {
                    Event::DopStarted(source.id)
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
    use crate::config::{Format, Quality};
    use crate::format::{F32, F64, I32, db_to_gain};
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

    #[test]
    fn dop_and_reconfiguring_do_not_allocate() {
        let mut rig = Rig::<F32>::new(BitPerfect::Auto);
        let (mut a, mut b) = (client(1), client(2));
        rig.add(&mut a);
        rig.add(&mut b);
        rig.cycle();
        let (dop, pcm) = (dop_frames(PERIOD), frames(1000, PERIOD));
        rig.send(Command::Reconfigure {
            rate: 96000,
            period: PERIOD as u32,
        });
        let engine = &mut rig.engine;
        assert_no_alloc(|| {
            for _ in 0..20 {
                a.ring.write(&dop);
                b.ring.write(&pcm);
                engine.cycle().unwrap();
            }
        });
    }

    #[test]
    fn a_gain_change_ramps_over_10_ms() {
        let mut rig = Rig::<F64>::new(BitPerfect::Auto);
        let mut a = client(1);
        rig.add(&mut a);
        const LEVEL: f64 = (1 << 24) as f64;
        let total = 10 * PERIOD;
        a.ring.write(&frames(1 << 24, total));
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
        let mut a = client_at(1, 44100);
        let resampler = Resampler::new(44100, RATE, PERIOD as u32, Quality::High, Format::F32);
        a.source.as_mut().unwrap().resampler = Some(Box::new(resampler));
        rig.add(&mut a);
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
        let mut want = input[PERIOD..].to_vec();
        want.extend(frames(0, PERIOD - 10));
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
        assert!(matches!(events[0], Event::DopStarted(2)));
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
    fn a_dop_stream_with_nothing_to_play_does_not_silence_the_others() {
        let mut rig = Rig::<I32>::new(BitPerfect::Auto);
        let (mut pcm, mut dop) = (client(1), client(2));
        rig.add(&mut pcm);
        rig.add(&mut dop);
        dop.ring.write(&dop_frames(PERIOD));
        pcm.ring.write(&frames(1000, 2 * PERIOD));
        rig.cycle();
        rig.cycle();
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
        // An underrun is not the end of the stream.
        assert_eq!(
            count(&rig.events(), |e| matches!(e, Event::DopStopped(_))),
            0
        );
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
            [Event::DopStarted(1), Event::DopStopped(1)]
        ));
        rig.send(Command::SetRunning(1, true));
        rig.cycle();
        rig.send(Command::Remove(1));
        rig.cycle();
        let events = rig.events();
        assert!(matches!(
            events[..],
            [
                Event::DopStarted(1),
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
        dop.ring.write(&dop_frames(2 * PERIOD));
        pcm.ring.write(&frames(1000, PERIOD));
        rig.cycle();
        // Muted, but discarded in real time. Other clients keep playing.
        assert_eq!(rig.last_periods(1), frames(1000, PERIOD));
        assert_eq!(dop.ring.available(), PERIOD);
        assert!(matches!(rig.events()[..], [Event::DopStarted(1)]));
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
        assert_eq!(rig.counters.wake_gaps.load(Relaxed), 1);
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
        assert!(matches!(rig.events.pop(), Ok(Event::DopStarted(1))));
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
}
