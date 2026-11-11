//! The control thread's decisions: which clients play, at what rate, and
//! when the output is open. The audio thread only carries them out.
//!
//! Everything the audio thread needs is built here, and whatever it hands
//! back is freed here. Commands that belong together go into the queue in
//! one commit, so the audio thread never sees half a decision.

use crate::capture::Capture;
use crate::config::{Config, Format, Idle, PluginConfig, PluginPlace, Quality};
use crate::dop;
use crate::engine::{ANALOG, Command, Event, INITIAL_SOURCES, INITIAL_TAPS, Source, Tap};
use crate::format::db_to_gain;
use crate::latency::{PERIODS, period_frames};
use crate::plugin::build::{Chains, Libraries, build_chain, engine_place};
use crate::plugin::chain::Chain;
use crate::rate::{ClientId, MAX_RATE, MIN_RATE, RatePolicy, SETTLE, STANDARD_RATES};
use crate::resample::Resampler;
use crate::state::{Saver, State};
use crate::volume::{HardwareVolume, OutputVolume};
use aas_client::convert::ClientFormat;
use aas_client::proto::{self, Reply, Request};
use aas_client::ring::{self, Header, Ring};
use rtrb::{Consumer, Producer};
use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::thread::Thread;
use std::time::{Duration, Instant};

/// Identifies a connection to the server.
pub type ConnId = u64;

/// Opens the card's capture at a rate and period. Called on the control
/// thread, after the output has opened at them.
///
/// It must not wait: while it runs, no client request is served. A device
/// that another process holds must make it fail at once, not block until
/// the device is free. A failed open is tried again a second later.
pub type OpenCapture = Box<dyn FnMut(u32, u32) -> Result<Box<dyn Capture>, String>>;

/// Smallest period the latency rule gives.
const MIN_PERIOD: u32 = 32;
/// Half the longest resampling filter, which rubato adds to its input.
const FILTER_HALF: u32 = 64;
/// Headroom for the ratio change rubato prepares for.
const RATIO_HEADROOM: f64 = 1.1;
/// How long a drain may wait beyond what its ring holds, should the audio
/// thread never report it drained.
const DRAIN_SPARE: Duration = Duration::from_millis(500);
/// How long the control thread waits for room in the command queue before
/// it says so in the log. It goes on waiting.
const SEND_WARN: Duration = Duration::from_secs(1);
/// How long the volume stays unchanged before the state file is written,
/// so that a volume slider being dragged does not write it many times.
const SAVE_DELAY: Duration = Duration::from_secs(1);
/// How often a capture that failed, or would not open, is tried again.
const CAPTURE_RETRY: Duration = Duration::from_secs(1);
/// Periods of the output a capture stream's ring holds at least.
const RECORDER_PERIODS: u32 = 4;

/// Chains built for a rate, not yet sent to the audio thread.
struct Spare {
    rate: u32,
    /// Built for a DoP stream: the analog chain only.
    dop: bool,
    chains: Vec<(PluginPlace, Chain)>,
}

/// Where a place's chain is kept in `Control::built`.
fn slot(place: PluginPlace) -> usize {
    match place {
        PluginPlace::Analog => 0,
        PluginPlace::Mix => 1,
        PluginPlace::Output => 2,
    }
}

/// Frames a client's ring holds at least, so that one period's input fits
/// whatever rate the output runs at. One period takes at most what the
/// latency target lasts, or the smallest period at the lowest rate.
pub fn min_ring_frames(rate: u32, latency_ms: f32) -> u32 {
    let per_period = f64::from(latency_ms) / 1000.0 / f64::from(PERIODS);
    let shortest = f64::from(MIN_PERIOD) / f64::from(MIN_RATE);
    let input = f64::from(rate) * per_period.max(shortest) * RATIO_HEADROOM;
    input.ceil() as u32 + 2 + FILTER_HALF + dop::MIN_FRAMES as u32
}

/// The smallest buffer a client may ask for: what one period of the
/// output may take from its ring, and one period more to refill it in.
/// Below that every cycle finds the ring short, an underrun each time.
pub fn min_buffer_frames(rate: u32, latency_ms: f32) -> u32 {
    let per_period = f64::from(latency_ms) / 1000.0 / f64::from(PERIODS);
    let shortest = f64::from(MIN_PERIOD) / f64::from(MIN_RATE);
    let refill = (f64::from(rate) * per_period.max(shortest)).ceil() as u32;
    min_ring_frames(rate, latency_ms) + refill
}

/// A read-only view of a ring's header, for the control thread. It maps
/// the header alone, which is one page. A second mapping of the whole ring
/// would lock the ring's memory once more for each client.
struct HeaderView {
    base: NonNull<Header>,
}

// SAFETY: the mapping is shared memory that is only read, through atomics.
unsafe impl Send for HeaderView {}

impl HeaderView {
    const LEN: usize = size_of::<Header>();

    /// Maps the header of a ring this process made, so its size is known
    /// to hold one.
    fn map(fd: BorrowedFd) -> io::Result<HeaderView> {
        // SAFETY: a fresh shared mapping, the result is checked.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                Self::LEN,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = NonNull::new(p.cast()).ok_or_else(|| io::Error::other("mmap gave null"))?;
        Ok(HeaderView { base })
    }

    fn header(&self) -> &Header {
        // SAFETY: the mapping holds a header, page aligned, until drop.
        unsafe { self.base.as_ref() }
    }
}

// The view never takes more than the first page of a ring.
const _: () = assert!(HeaderView::LEN <= 4096);

impl Drop for HeaderView {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly what `map` mapped.
        unsafe { libc::munmap(self.base.as_ptr().cast(), Self::LEN) };
    }
}

/// What the server does with a request.
pub struct Answer {
    /// Nothing for a drain, whose reply comes later.
    pub reply: Option<Reply>,
    /// Descriptors sent with the reply.
    pub fds: Vec<OwnedFd>,
    /// The connection is closed after the reply.
    pub close: bool,
}

impl Answer {
    fn reply(reply: Reply) -> Answer {
        Answer {
            reply: Some(reply),
            fds: Vec::new(),
            close: false,
        }
    }

    fn error(code: i32) -> Answer {
        Answer::reply(Reply::Error { code: code as u32 })
    }

    fn fatal(code: i32) -> Answer {
        Answer {
            close: true,
            ..Answer::error(code)
        }
    }
}

/// A configured stream as the control thread keeps it.
struct Stream {
    id: ClientId,
    rate: u32,
    capacity: usize,
    /// Kept until the ring is sealed, on the first start.
    memfd: Option<OwnedFd>,
    running: bool,
    /// The output rate and period of the resampler the source has.
    resampler: Option<(u32, u32)>,
    /// The control thread's own view of the ring, to see a drain progress.
    view: HeaderView,
    /// A drain waits for its reply. If the audio thread has taken nothing
    /// from the ring by then, it is answered anyway.
    drain: Option<Instant>,
    /// The ring's read position when the drain deadline was last set.
    drain_seen: u64,
    /// The audio thread reported the stream drained since it last started,
    /// which can come before the request: the client sets the flag first.
    drained: bool,
}

/// A capture stream as the control thread keeps it. It is no client of the
/// rate rules, never counts as active and has no resampler.
struct Recorder {
    id: ClientId,
    /// The output's rate when it was configured, which it records at.
    rate: u32,
    /// Kept until the ring is sealed, on the first start.
    memfd: Option<OwnedFd>,
    running: bool,
    /// The output's rate has changed: the audio thread has been told to
    /// remove the tap, and the client has to configure again. The ring is
    /// marked ended once the tap is back, when nothing writes it any more.
    ended: bool,
}

struct Conn {
    name: String,
    hello: bool,
    /// A connection has one stream at a time, this or a recorder.
    stream: Option<Stream>,
    recorder: Option<Recorder>,
    /// Told of every change to the output volume.
    subscribed: bool,
}

/// The analog input as the control thread keeps it.
struct AnalogInput {
    open: OpenCapture,
    /// The rate the rate rules have for the input while it has signal.
    /// Always the rate the output runs at: the input follows it and never
    /// sets it.
    active: Option<u32>,
    /// `SetCapture` commands the audio thread has not answered yet. A
    /// failure reported meanwhile belongs to a capture they replace.
    unanswered: u32,
    /// The audio thread has a capture for the output as it is, which has
    /// not failed.
    reading: bool,
    /// When to try again to open the capture.
    retry_at: Option<Instant>,
    /// A failure has been logged, and the recovery is not yet.
    failing: bool,
}

pub struct Control {
    latency_ms: f32,
    quality: Quality,
    format: Format,
    idle: Idle,
    supported: Vec<u32>,
    policy: RatePolicy,
    epoch: Instant,
    commands: Producer<Command>,
    events: Consumer<Event>,
    /// Events taken out of the queue while waiting for room in the other.
    stash: Vec<Event>,
    audio: Thread,
    conns: BTreeMap<ConnId, Conn>,
    next_id: ClientId,
    table_capacity: usize,
    table_len: usize,
    /// The same for the audio thread's table of capture streams.
    taps_capacity: usize,
    taps_len: usize,
    /// The rate and period the output was last told to open at. None while
    /// it is closed.
    output: Option<(u32, u32)>,
    /// How many times the audio thread has been told to open the output
    /// and has not yet answered. Nothing plays while one is under way. A
    /// count, since an answer to an earlier open says nothing of a later
    /// one. The audio thread answers every one, also one it dropped.
    reopening: u32,
    /// A rate change that waits, so that a stop and a start close together
    /// do not change the rate twice.
    switch_at: Option<Instant>,
    /// Since when no client has been running.
    idle_since: Option<Instant>,
    /// Clients the audio thread has seen play DoP, each with the number
    /// the audio thread gave its claim, oldest claim first.
    dop_claims: Vec<(u64, ClientId)>,
    dop_owner: Option<ClientId>,
    /// Replies that were not ready when their request came.
    outbox: Vec<(ConnId, Reply)>,
    volume: OutputVolume,
    /// Writes the volume to the state file, to keep it across restarts.
    /// None keeps it nowhere.
    saver: Option<Saver>,
    /// When the state file is due to be written.
    save_at: Option<Instant>,
    /// Volume notifications for subscribers, not yet sent.
    notices: Vec<(ConnId, Reply)>,
    /// The plugins of the config, to build chains from at each rate.
    plugins: Vec<PluginConfig>,
    libraries: Libraries,
    /// Chains built ahead, sent at the first open if it is at their rate.
    ready: Option<Chains>,
    /// The rate each place's chain on the audio thread was built for, by
    /// `slot`. None before one is sent.
    built: [Option<u32>; 3],
    /// Chains built for the rate the rules want, sent when the output runs
    /// at it.
    spare: Option<Spare>,
    /// Rates at which a chain could not be built. The output never runs at
    /// one, and the rules take the nearest other rate.
    refused: Vec<u32>,
    /// Of those, the rates the analog chain refused. A DoP stream needs
    /// only that chain, so it may run at the others.
    analog_refused: Vec<u32>,
    /// No rate works for every chain, and that has been logged.
    stuck: bool,
    /// The gain of the analog input, from the config.
    analog_gain: f32,
    /// None unless the config enables the input and the card has an ADC.
    analog: Option<AnalogInput>,
}

impl Control {
    /// `supported` are the rates the sink takes, empty for any. `audio` is
    /// unparked after every command.
    pub fn new(
        config: &Config,
        latency_ms: f32,
        supported: Vec<u32>,
        commands: Producer<Command>,
        events: Consumer<Event>,
        audio: Thread,
    ) -> Control {
        Control {
            latency_ms,
            quality: config.resampling,
            format: config.format,
            idle: config.idle,
            policy: RatePolicy::new(config.default_rate, supported.clone()),
            supported,
            epoch: Instant::now(),
            commands,
            events,
            stash: Vec::new(),
            audio,
            conns: BTreeMap::new(),
            next_id: 1,
            table_capacity: INITIAL_SOURCES,
            table_len: 0,
            taps_capacity: INITIAL_TAPS,
            taps_len: 0,
            output: None,
            reopening: 0,
            switch_at: None,
            idle_since: None,
            dop_claims: Vec::new(),
            dop_owner: None,
            outbox: Vec::new(),
            volume: OutputVolume::new(None, config.bit_perfect),
            saver: None,
            save_at: None,
            notices: Vec::new(),
            plugins: config.plugins.clone(),
            libraries: Libraries::default(),
            ready: None,
            built: [None; 3],
            spare: None,
            refused: Vec::new(),
            analog_refused: Vec::new(),
            stuck: false,
            analog_gain: db_to_gain(config.analog.volume_db),
            analog: None,
        }
    }

    /// Takes over the libraries loaded at startup, and the chains built
    /// then, if any. Call before `start`. Without them the chains are built
    /// at the first open.
    pub fn set_plugins(&mut self, libraries: Libraries, chains: Option<Chains>) {
        self.libraries = libraries;
        self.ready = chains;
    }

    /// Runs the analog input, which `open` opens. Call before `start`, and
    /// only when the config enables the input and the card has an ADC.
    /// The output then stays open when idle, since the card paces the
    /// capture and the detector has to keep measuring.
    pub fn set_analog(&mut self, open: OpenCapture) {
        if let Idle::Off { .. } = self.idle {
            eprintln!("aas: the analog input keeps the output open, idle = \"off\" does not apply");
        }
        self.analog = Some(AnalogInput {
            open,
            active: None,
            unanswered: 0,
            reading: false,
            retry_at: None,
            failing: false,
        });
    }

    /// Takes over the output volume, at the level the state file keeps.
    /// In hardware mode this sets the card's mixer. Call before `start`.
    pub fn restore_volume(&mut self, mut volume: OutputVolume, state: PathBuf) {
        volume.set(State::load(&state).output_percent);
        self.volume = volume;
        self.saver = Some(Saver::new(state));
        eprintln!(
            "aas: output volume {}%, in {}",
            self.volume.percent(),
            self.volume_mode()
        );
    }

    /// The card has gone or come back, and has this volume control now, or
    /// none. A card that is back may be a new instance, its mixer at its own
    /// default. The old control is closed, and the current volume is set on
    /// the new one, or carried in software without one.
    pub fn card_changed(&mut self, hardware: Option<Box<dyn HardwareVolume>>) {
        self.volume.replace_hardware(hardware);
        eprintln!(
            "aas: output volume {}%, in {}",
            self.volume.percent(),
            self.volume_mode()
        );
        self.send(vec![Command::SetGain(self.volume.gain())]);
    }

    fn volume_mode(&self) -> &'static str {
        if self.volume.is_hardware() {
            "hardware"
        } else {
            "software"
        }
    }

    /// Opens the output at the default rate. Until a client plays it sends
    /// zeros, and the idle rule applies from now, unless the analog input
    /// is on.
    pub fn start(&mut self, now: Instant) {
        let mut batch = vec![Command::SetGain(self.volume.gain())];
        if self.analog.is_some() {
            batch.push(Command::SetAnalogGain(self.analog_gain));
        }
        self.open_output(&mut batch);
        self.idle_since = Some(now);
        self.send(batch);
    }

    /// Stops the audio thread. It returns once it has seen this.
    /// Writes the state file if a change is still waiting for it, and
    /// returns only once the file is on disk.
    pub fn shutdown(&mut self) {
        self.send(vec![Command::Shutdown]);
        if self.save_at.is_some() {
            self.save();
        }
        if let Some(saver) = &mut self.saver {
            saver.finish();
        }
    }

    pub fn connected(&mut self, conn: ConnId, name: String) {
        eprintln!("aas: {name} connected");
        let state = Conn {
            name,
            hello: false,
            stream: None,
            recorder: None,
            subscribed: false,
        };
        self.conns.insert(conn, state);
    }

    /// A closed socket removes the client, whatever state it was in.
    pub fn disconnected(&mut self, conn: ConnId, now: Instant) {
        let Some(state) = self.conns.remove(&conn) else {
            return;
        };
        eprintln!("aas: {} disconnected", state.name);
        if let Some(stream) = state.stream {
            let mut batch = Vec::new();
            self.remove_stream(stream, now, &mut batch);
            self.decide(now, false, &mut batch);
            self.send(batch);
        }
        if let Some(recorder) = state.recorder {
            let mut batch = Vec::new();
            self.remove_recorder(recorder, &mut batch);
            self.send(batch);
        }
    }

    /// Replies that have become ready, such as finished drains.
    pub fn take_replies(&mut self) -> Vec<(ConnId, Reply)> {
        std::mem::take(&mut self.outbox)
    }

    /// The ready replies for one connection, in order. They answer requests
    /// it sent earlier, so they go out before the reply to a newer one.
    pub fn take_replies_for(&mut self, conn: ConnId) -> Vec<Reply> {
        let mut mine = Vec::new();
        self.outbox.retain(|&(c, reply)| {
            if c == conn {
                mine.push(reply);
            }
            c != conn
        });
        mine
    }

    /// A request that changes the stream ends a drain still waiting. The
    /// drain is answered with `ECANCELED`, so every request still gets one
    /// reply, in order. A non-blocking client sends one while it drains.
    fn cancel_drain(&mut self, conn: ConnId) {
        let Some(stream) = self.stream_mut(conn) else {
            return;
        };
        if stream.drain.take().is_some() {
            let code = libc::ECANCELED as u32;
            self.outbox.push((conn, Reply::Error { code }));
        }
    }

    /// Volume notifications for subscribers, in order. The server sends
    /// only the latest one each subscriber has waiting.
    pub fn take_notices(&mut self) -> Vec<(ConnId, Reply)> {
        std::mem::take(&mut self.notices)
    }

    /// Handles one message. `None` is a message that does not decode.
    pub fn request(&mut self, conn: ConnId, request: Option<Request>, now: Instant) -> Answer {
        let Some(state) = self.conns.get_mut(&conn) else {
            return Answer::fatal(libc::ENOTCONN);
        };
        let Some(request) = request else {
            return Answer::error(libc::EINVAL);
        };
        if !state.hello {
            return match request {
                Request::Hello { version } if version == proto::VERSION => {
                    state.hello = true;
                    Answer::reply(Reply::Hello {
                        version: proto::VERSION,
                    })
                }
                Request::Hello { version } => {
                    eprintln!("aas: {} speaks protocol {version}", state.name);
                    Answer::fatal(libc::EPROTONOSUPPORT)
                }
                _ => Answer::fatal(libc::EPROTO),
            };
        }
        if matches!(
            request,
            Request::Configure { .. }
                | Request::Start
                | Request::Stop
                | Request::Drain
                | Request::Reset
        ) {
            self.cancel_drain(conn);
        }
        let recording = self.recorder_mut(conn).is_some();
        match request {
            Request::Hello { .. } => Answer::reply(Reply::Hello {
                version: proto::VERSION,
            }),
            Request::Configure {
                buffer_frames,
                capture: true,
                ..
            } => self.configure_recorder(conn, buffer_frames, now),
            Request::Configure {
                rate,
                format,
                channels,
                buffer_frames,
                capture: false,
            } => self.configure(conn, rate, format, channels, buffer_frames, now),
            Request::Start if recording => self.start_recorder(conn),
            Request::Stop if recording => self.stop_recorder(conn),
            Request::Reset if recording => self.reset_recorder(conn),
            // A capture stream never ends by itself.
            Request::Drain if recording => Answer::error(libc::EINVAL),
            Request::Start => self.start_stream(conn, now),
            Request::Stop => self.stop_stream(conn, now),
            Request::Drain => self.drain(conn, now),
            Request::Reset => self.reset(conn, now),
            Request::GetVolume => self.volume_reply(),
            Request::SetVolume { percent } if percent > 100 => Answer::error(libc::EINVAL),
            Request::SetVolume { percent } => {
                self.set_volume(percent, now);
                Answer::reply(Reply::Ok)
            }
            Request::SubscribeVolume => {
                if let Some(state) = self.conns.get_mut(&conn) {
                    state.subscribed = true;
                }
                self.volume_reply()
            }
        }
    }

    fn volume_reply(&self) -> Answer {
        let percent = self.volume.percent();
        Answer::reply(Reply::Volume { percent })
    }

    /// Sets the output volume and tells every subscriber. The state file is
    /// written once the volume has been left alone for a while.
    ///
    /// The card's mixer is written even when the percent is the same, so
    /// that asking again puts back a level another tool has changed. A
    /// change made outside aas is not read back.
    fn set_volume(&mut self, percent: u8, now: Instant) {
        let changed = percent != self.volume.percent();
        self.volume.set(percent);
        if !changed {
            return;
        }
        self.send(vec![Command::SetGain(self.volume.gain())]);
        let subscribers = self.conns.iter().filter(|(_, c)| c.subscribed);
        let notice = Reply::VolumeChanged { percent };
        self.notices
            .extend(subscribers.map(|(&conn, _)| (conn, notice)));
        if self.saver.is_some() {
            self.save_at = Some(now + SAVE_DELAY);
        }
    }

    /// Has the state file written. The write and its sync to disk run on
    /// the saver's thread, so a slow SD card holds up no request.
    fn save(&mut self) {
        self.save_at = None;
        if let Some(saver) = &self.saver {
            saver.save(State {
                output_percent: self.volume.percent(),
            });
        }
    }

    fn configure(
        &mut self,
        conn: ConnId,
        rate: u32,
        format: ClientFormat,
        channels: u8,
        buffer_frames: u32,
        now: Instant,
    ) -> Answer {
        // The client converts its format; the ring is always S32 stereo.
        let _ = format;
        if !(MIN_RATE..=MAX_RATE).contains(&rate)
            || channels == 0
            || buffer_frames < min_buffer_frames(rate, self.latency_ms)
        {
            return Answer::error(libc::EINVAL);
        }
        let mut batch = Vec::new();
        if let Some(old) = self.conns.get_mut(&conn).and_then(|c| c.stream.take()) {
            self.remove_stream(old, now, &mut batch);
        }
        if let Some(old) = self.conns.get_mut(&conn).and_then(|c| c.recorder.take()) {
            self.remove_recorder(old, &mut batch);
        }
        let capacity = buffer_frames.max(min_ring_frames(rate, self.latency_ms));
        let made = Ring::create(capacity).and_then(|(ring, memfd)| {
            let view = HeaderView::map(memfd.as_fd())?;
            let wake = eventfd()?;
            let theirs = wake.try_clone()?;
            let memfd_theirs = memfd.try_clone()?;
            Ok((ring, view, memfd, memfd_theirs, wake, theirs))
        });
        let (ring, view, memfd, memfd_theirs, wake, theirs) = match made {
            Ok(made) => made,
            Err(e) => {
                eprintln!("aas: no ring for a client: {e}");
                self.decide(now, false, &mut batch);
                self.send(batch);
                return Answer::error(e.raw_os_error().unwrap_or(libc::ENOMEM));
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        if self.table_len == self.table_capacity {
            self.table_capacity *= 2;
            batch.push(Command::Grow(Vec::with_capacity(self.table_capacity)));
        }
        batch.push(Command::Add(Box::new(Source::new(id, ring, wake, rate))));
        self.table_len += 1;
        let stream = Stream {
            id,
            rate,
            capacity: capacity as usize,
            memfd: Some(memfd),
            running: false,
            resampler: None,
            view,
            drain: None,
            drain_seen: 0,
            drained: false,
        };
        if let Some(state) = self.conns.get_mut(&conn) {
            state.stream = Some(stream);
        }
        self.decide(now, false, &mut batch);
        self.send(batch);
        Answer {
            reply: Some(Reply::Configured { rate: 0 }),
            fds: vec![memfd_theirs, theirs],
            close: false,
        }
    }

    /// Starts the stream. A stream that cannot be sealed is not trusted, and
    /// the client is dropped.
    fn start_stream(&mut self, conn: ConnId, now: Instant) -> Answer {
        let urgent = self.running_count() == 0;
        let Some(stream) = self.stream_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        if let Some(memfd) = stream.memfd.take() {
            if let Err(e) = ring::seal_writes(memfd.as_fd()) {
                eprintln!("aas: could not seal a client's ring: {e}");
                return Answer::fatal(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
        stream.drain = None;
        if stream.running {
            return Answer::reply(Reply::Ok);
        }
        stream.running = true;
        stream.drained = false;
        let (id, rate) = (stream.id, stream.rate);
        let t = now - self.epoch;
        self.policy.client_started(id, rate, t);
        let mut batch = Vec::new();
        self.decide(now, urgent, &mut batch);
        batch.push(Command::SetRunning(id, true));
        self.send(batch);
        Answer::reply(Reply::Ok)
    }

    fn stop_stream(&mut self, conn: ConnId, now: Instant) -> Answer {
        let Some(stream) = self.stream_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        let id = stream.id;
        stream.drain = None;
        let mut batch = Vec::new();
        self.stop(id, now, &mut batch);
        self.send(batch);
        Answer::reply(Reply::Ok)
    }

    /// Stops the stream and has the audio thread empty its ring. The reply
    /// comes once it has, so nothing written after the reply is lost.
    fn reset(&mut self, conn: ConnId, now: Instant) -> Answer {
        let Some(stream) = self.stream_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        let id = stream.id;
        stream.drain = None;
        stream.drained = false;
        let mut batch = Vec::new();
        self.stop(id, now, &mut batch);
        batch.push(Command::Flush(id));
        self.send(batch);
        Answer {
            reply: None,
            fds: Vec::new(),
            close: false,
        }
    }

    /// The audio thread has emptied a stream's ring, or stopped a capture
    /// stream: its reset is answered.
    fn flushed(&mut self, id: ClientId) {
        let conn = self.conns.iter().find(|(_, c)| {
            c.stream.as_ref().is_some_and(|s| s.id == id)
                || c.recorder.as_ref().is_some_and(|r| r.id == id)
        });
        if let Some((&conn, _)) = conn {
            self.outbox.push((conn, Reply::Ok));
        }
    }

    /// Stops a running stream and settles what follows from that.
    fn stop(&mut self, id: ClientId, now: Instant, batch: &mut Vec<Command>) {
        let Some(stream) = self.stream_by_id(id) else {
            return;
        };
        if !std::mem::take(&mut stream.running) {
            return;
        }
        self.policy.client_stopped(id, now - self.epoch);
        batch.push(Command::SetRunning(id, false));
        self.dop_claims.retain(|&(_, c)| c != id);
        self.update_dop(now, batch);
        self.decide(now, false, batch);
    }

    /// Starts the stream if needed. The reply comes once the audio thread
    /// has played everything.
    fn drain(&mut self, conn: ConnId, now: Instant) -> Answer {
        let Some(stream) = self.stream_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        if !stream.running {
            let answer = self.start_stream(conn, now);
            if answer.reply != Some(Reply::Ok) {
                return answer;
            }
        }
        let Some(stream) = self.stream_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        if std::mem::take(&mut stream.drained) {
            let id = stream.id;
            let mut batch = Vec::new();
            self.stop(id, now, &mut batch);
            self.send(batch);
            return Answer::reply(Reply::Ok);
        }
        let held = Duration::from_secs_f64(stream.capacity as f64 / f64::from(stream.rate));
        stream.drain = Some(now + held + DRAIN_SPARE);
        stream.drain_seen = stream.view.header().read_pos.load(Ordering::Acquire);
        Answer {
            reply: None,
            fds: Vec::new(),
            close: false,
        }
    }

    /// Ends a drain: the stream stops and the client gets its reply.
    fn drained(&mut self, id: ClientId, now: Instant, batch: &mut Vec<Command>) {
        let Some((&conn, _)) = self
            .conns
            .iter()
            .find(|(_, c)| c.stream.as_ref().is_some_and(|s| s.id == id))
        else {
            return;
        };
        let Some(stream) = self.stream_mut(conn) else {
            return;
        };
        if stream.drain.take().is_none() {
            stream.drained = true;
            return;
        }
        stream.drained = false;
        self.stop(id, now, batch);
        self.outbox.push((conn, Reply::Ok));
    }

    fn remove_stream(&mut self, stream: Stream, now: Instant, batch: &mut Vec<Command>) {
        if stream.running {
            self.policy.client_stopped(stream.id, now - self.epoch);
        }
        batch.push(Command::Remove(stream.id));
        self.table_len -= 1;
        self.dop_claims.retain(|&(_, c)| c != stream.id);
        self.update_dop(now, batch);
    }

    /// Sets up a capture stream: S32 stereo at the rate the output runs at,
    /// or opens at next if it is closed. The ring holds what is asked for,
    /// and at least a few periods. Sends the tap to the audio thread, which
    /// writes it once it is started.
    fn configure_recorder(&mut self, conn: ConnId, buffer_frames: u32, now: Instant) -> Answer {
        let rate = match self.output {
            Some((rate, _)) => rate,
            None => self.policy.current(),
        };
        let least = RECORDER_PERIODS * period_frames(rate, self.latency_ms);
        let capacity = buffer_frames.max(least);
        let mut batch = Vec::new();
        if let Some(old) = self.conns.get_mut(&conn).and_then(|c| c.stream.take()) {
            self.remove_stream(old, now, &mut batch);
            self.decide(now, false, &mut batch);
        }
        if let Some(old) = self.conns.get_mut(&conn).and_then(|c| c.recorder.take()) {
            self.remove_recorder(old, &mut batch);
        }
        let made = Ring::create(capacity).and_then(|(ring, memfd)| {
            let wake = eventfd()?;
            let theirs = wake.try_clone()?;
            let memfd_theirs = memfd.try_clone()?;
            Ok((ring, memfd, memfd_theirs, wake, theirs))
        });
        let (ring, memfd, memfd_theirs, wake, theirs) = match made {
            Ok(made) => made,
            Err(e) => {
                eprintln!("aas: no ring for a recorder: {e}");
                self.send(batch);
                return Answer::error(e.raw_os_error().unwrap_or(libc::ENOMEM));
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        if self.taps_len == self.taps_capacity {
            self.taps_capacity *= 2;
            batch.push(Command::GrowTaps(Vec::with_capacity(self.taps_capacity)));
        }
        batch.push(Command::AddTap(Box::new(Tap::new(id, ring, wake))));
        self.taps_len += 1;
        let recorder = Recorder {
            id,
            rate,
            memfd: Some(memfd),
            running: false,
            ended: false,
        };
        if let Some(state) = self.conns.get_mut(&conn) {
            state.recorder = Some(recorder);
        }
        self.send(batch);
        Answer {
            reply: Some(Reply::Configured { rate }),
            fds: vec![memfd_theirs, theirs],
            close: false,
        }
    }

    /// Starts a capture stream, unless it has ended. As for a player, a
    /// ring that cannot be sealed is not trusted, and the client is dropped.
    fn start_recorder(&mut self, conn: ConnId) -> Answer {
        let Some(recorder) = self.recorder_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        if recorder.ended {
            return Answer::error(libc::ENODEV);
        }
        if let Some(memfd) = recorder.memfd.take() {
            if let Err(e) = ring::seal_writes(memfd.as_fd()) {
                eprintln!("aas: could not seal a recorder's ring: {e}");
                return Answer::fatal(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
        if !std::mem::replace(&mut recorder.running, true) {
            let id = recorder.id;
            self.send(vec![Command::SetRunning(id, true)]);
        }
        Answer::reply(Reply::Ok)
    }

    fn stop_recorder(&mut self, conn: ConnId) -> Answer {
        let Some(recorder) = self.recorder_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        if std::mem::take(&mut recorder.running) {
            let id = recorder.id;
            self.send(vec![Command::SetRunning(id, false)]);
        }
        Answer::reply(Reply::Ok)
    }

    /// Stops a capture stream. The reply comes once the audio thread has
    /// stopped writing, so that the client can empty the ring, which is
    /// the reader's to do.
    fn reset_recorder(&mut self, conn: ConnId) -> Answer {
        let Some(recorder) = self.recorder_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        recorder.running = false;
        let id = recorder.id;
        self.send(vec![Command::Flush(id)]);
        Answer {
            reply: None,
            fds: Vec::new(),
            close: false,
        }
    }

    fn remove_recorder(&mut self, recorder: Recorder, batch: &mut Vec<Command>) {
        if !recorder.ended {
            batch.push(Command::RemoveTap(recorder.id));
            self.taps_len -= 1;
        }
    }

    /// Ends every capture stream that records at another rate than `rate`.
    /// The capture runs at the output's rate only, so the client has to
    /// configure again. It is told through the ring once the tap is back.
    fn end_recorders(&mut self, rate: u32, batch: &mut Vec<Command>) {
        let recorders = self.conns.values_mut().filter_map(|c| c.recorder.as_mut());
        for recorder in recorders.filter(|r| !r.ended && r.rate != rate) {
            recorder.ended = true;
            recorder.running = false;
            batch.push(Command::RemoveTap(recorder.id));
            self.taps_len -= 1;
        }
    }

    fn recorder_mut(&mut self, conn: ConnId) -> Option<&mut Recorder> {
        self.conns.get_mut(&conn)?.recorder.as_mut()
    }

    fn stream_mut(&mut self, conn: ConnId) -> Option<&mut Stream> {
        self.conns.get_mut(&conn)?.stream.as_mut()
    }

    fn stream_by_id(&mut self, id: ClientId) -> Option<&mut Stream> {
        let mut streams = self.conns.values_mut().filter_map(|c| c.stream.as_mut());
        streams.find(|s| s.id == id)
    }

    fn streams(&self) -> impl Iterator<Item = &Stream> {
        self.conns.values().filter_map(|c| c.stream.as_ref())
    }

    fn streams_mut(&mut self) -> impl Iterator<Item = &mut Stream> {
        self.conns.values_mut().filter_map(|c| c.stream.as_mut())
    }

    fn running_count(&self) -> usize {
        self.streams().filter(|s| s.running).count()
    }

    /// Whether a DoP stream at `rate` can have the output: the card takes
    /// the rate, and the analog chain, if any, can be built at it.
    fn is_supported(&self, rate: u32) -> bool {
        let card = self.supported.is_empty() || self.supported.contains(&rate);
        card && !self.analog_refused.contains(&rate)
    }

    /// The card's rates, without `refused`. Empty takes any rate, as the
    /// card's list does, only while nothing has been refused.
    fn usable(&self, refused: &[u32]) -> Vec<u32> {
        if refused.is_empty() {
            return self.supported.clone();
        }
        let all = if self.supported.is_empty() {
            &STANDARD_RATES[..]
        } else {
            &self.supported[..]
        };
        all.iter()
            .copied()
            .filter(|r| !refused.contains(r))
            .collect()
    }

    /// Gives the rules the rates the card takes and the chains can be built
    /// at. Returns false, and leaves the rules as they are, if there is none.
    fn update_rates(&mut self) -> bool {
        let pcm = self.usable(&self.refused);
        if pcm.is_empty() && !self.refused.is_empty() {
            return false;
        }
        // DoP needs only the analog chain, so only its refusals limit DoP.
        let dop = (self.refused != self.analog_refused).then(|| self.usable(&self.analog_refused));
        self.policy.set_supported(pcm);
        self.policy.set_dop_supported(dop);
        true
    }

    /// Makes the rate rules follow the audio thread's DoP owner: the oldest
    /// claim of a running stream at a rate the card takes. The audio thread
    /// ranks the streams holding the output the same way. A new owner needs its rate at
    /// once, since its audio is discarded until then.
    fn update_dop(&mut self, now: Instant, batch: &mut Vec<Command>) {
        let claimed = self.dop_claims.iter().map(|&(_, id)| id);
        let owner = claimed.into_iter().find(|&id| {
            let stream = self.streams().find(|s| s.id == id);
            stream.is_some_and(|s| s.running && self.is_supported(s.rate))
        });
        if owner == self.dop_owner {
            return;
        }
        match owner {
            Some(id) => self.policy.dop_started(id),
            None => {
                if let Some(old) = self.dop_owner {
                    self.policy.dop_stopped(old);
                }
            }
        }
        self.dop_owner = owner;
        if owner.is_some() {
            self.decide(now, true, batch);
        }
    }

    /// Takes the output to the rate the rules want, and keeps every running
    /// source's resampler in step with it. `urgent` applies a change at
    /// once; otherwise it waits for the settle time, so that a stop and a
    /// start in quick succession leave the rate alone.
    fn decide(&mut self, now: Instant, urgent: bool, batch: &mut Vec<Command>) {
        // An active analog input follows the output rate, whatever moved
        // it. Updating a client's rate never moves the output itself.
        if let Some(analog) = &mut self.analog {
            let current = self.policy.current();
            if analog.active.is_some_and(|r| r != current) {
                analog.active = Some(current);
                self.policy
                    .client_started(ANALOG, current, now - self.epoch);
            }
        }
        let running = self.running_count() > 0;
        if running {
            self.idle_since = None;
        } else {
            self.idle_since.get_or_insert(now);
        }
        let Some(want) = self.usable_rate() else {
            return;
        };
        let reopen = match self.output {
            None => running,
            Some((rate, _)) if rate == want => {
                self.switch_at = None;
                false
            }
            Some(_) => {
                let due = self.switch_at.is_some_and(|at| now >= at);
                if !urgent && !due {
                    self.switch_at.get_or_insert(now + SETTLE);
                }
                urgent || due
            }
        };
        if reopen {
            self.open_output(batch);
        } else {
            self.sync_resamplers(batch);
            // Chains for the rate the output already runs at, such as when
            // it stays at the rate a DoP stream set.
            if self.output.is_some_and(|(rate, _)| rate == want) {
                self.send_spare(batch);
            }
        }
    }

    /// The rate the rules want, once chains are built for it. A rate at
    /// which one cannot be built is refused: the rules then take the
    /// nearest other rate, and the chains are built for that. None if no
    /// rate works, which is logged once; the output then stays as it is.
    fn usable_rate(&mut self) -> Option<u32> {
        loop {
            let rate = self.policy.current();
            let dop = self.policy.dop_rate().is_some();
            let known = if dop {
                &self.analog_refused
            } else {
                &self.refused
            };
            if known.contains(&rate) {
                // Refused already, and nothing else works.
                return None;
            }
            let spare = self.spare.as_ref();
            if spare.is_some_and(|s| s.rate == rate && s.dop == dop) {
                return Some(rate);
            }
            match self.chains_for(rate, dop) {
                Ok(chains) => {
                    self.spare = Some(Spare { rate, dop, chains });
                    return Some(rate);
                }
                Err((place, e)) => {
                    eprintln!("aas: {e}; the output does not run at {rate} Hz");
                    self.refused.push(rate);
                    if place == PluginPlace::Analog {
                        self.analog_refused.push(rate);
                    }
                    if !self.update_rates() || self.policy.current() == rate {
                        if !std::mem::replace(&mut self.stuck, true) {
                            eprintln!("aas: no rate works for every plugin chain");
                        }
                        return None;
                    }
                }
            }
        }
    }

    /// Builds the chains `rate` needs that are not on the audio thread
    /// already: for a DoP stream the analog chain only, since DoP bypasses
    /// the others and they keep their state. Fails with the place whose
    /// chain could not be built.
    fn chains_for(
        &mut self,
        rate: u32,
        dop: bool,
    ) -> Result<Vec<(PluginPlace, Chain)>, (PluginPlace, String)> {
        let mut ready = self.ready.take().filter(|c| c.rate == rate);
        let mut chains = Vec::new();
        for place in self.chain_places() {
            if (dop && place != PluginPlace::Analog) || self.built[slot(place)] == Some(rate) {
                continue;
            }
            let chain = match &mut ready {
                Some(ready) => ready.take(place),
                None => build_chain(&self.plugins, place, rate, self.format, &mut self.libraries)
                    .map_err(|e| (place, e))?,
            };
            chains.push((place, chain));
        }
        Ok(chains)
    }

    /// Sends the spare chains, which take effect from the next command on.
    fn send_spare(&mut self, batch: &mut Vec<Command>) {
        let Some(spare) = self.spare.take() else {
            return;
        };
        for (place, chain) in spare.chains {
            self.built[slot(place)] = Some(spare.rate);
            batch.push(Command::SetChain(engine_place(place), Box::new(chain)));
        }
    }

    /// Opens or reopens the output at the rate the rules want. Resamplers
    /// and chains for the new rate go first, so that they take effect with
    /// it. The audio thread closes the capture first; a new one is opened
    /// once the output is open.
    fn open_output(&mut self, batch: &mut Vec<Command>) {
        let Some(rate) = self.usable_rate() else {
            return;
        };
        let period = period_frames(rate, self.latency_ms);
        eprintln!("aas: output at {rate} Hz, {period} frames per period");
        self.output = Some((rate, period));
        self.reopening += 1;
        self.switch_at = None;
        self.sync_resamplers(batch);
        self.send_spare(batch);
        if let Some(analog) = &mut self.analog {
            analog.reading = false;
            analog.retry_at = None;
        }
        self.end_recorders(rate, batch);
        batch.push(Command::Reconfigure { rate, period });
    }

    /// The places that get a chain: those the config lists plugins at, and
    /// the analog input, which always has its detector.
    fn chain_places(&self) -> Vec<PluginPlace> {
        let places = [PluginPlace::Analog, PluginPlace::Mix, PluginPlace::Output];
        places
            .into_iter()
            .filter(|&place| match place {
                PluginPlace::Analog => self.analog.is_some(),
                _ => self.plugins.iter().any(|p| p.place == place),
            })
            .collect()
    }

    /// Opens the capture for the output and hands it to the audio thread,
    /// once the output is open and if it has none that works. One that
    /// will not open is tried again a second later.
    fn open_capture(&mut self, now: Instant, batch: &mut Vec<Command>) {
        let (Some((rate, period)), 0) = (self.output, self.reopening) else {
            return;
        };
        let Some(analog) = &mut self.analog else {
            return;
        };
        if analog.reading {
            return;
        }
        match (analog.open)(rate, period) {
            Ok(capture) => {
                batch.push(Command::SetCapture(Some(capture)));
                analog.unanswered += 1;
                analog.reading = true;
                analog.retry_at = None;
                if std::mem::take(&mut analog.failing) {
                    eprintln!("aas: the analog input works again");
                }
            }
            Err(e) => {
                if !std::mem::replace(&mut analog.failing, true) {
                    eprintln!("aas: the analog input: {e}, trying again every second");
                }
                analog.retry_at = Some(now + CAPTURE_RETRY);
            }
        }
    }

    /// The audio thread reports that the capture failed, and it is closed. A
    /// report that comes before the answer to a newer capture is about the
    /// one that capture replaced, so it changes nothing.
    fn capture_failed(&mut self, now: Instant) {
        let Some(analog) = &mut self.analog else {
            return;
        };
        if analog.unanswered > 0 || !analog.reading {
            return;
        }
        analog.reading = false;
        if !std::mem::replace(&mut analog.failing, true) {
            eprintln!("aas: the analog input failed, trying again every second");
        }
        analog.retry_at = Some(now + CAPTURE_RETRY);
    }

    /// The analog input has signal or not. With signal it is a client of
    /// the rate rules at the rate the output runs at, a DoP rate included,
    /// so it never sets a rate. As a client it counts as the first one if
    /// it came first, and the rate is decided again as for a player that
    /// starts or stops.
    fn input_signal(&mut self, on: bool, now: Instant, batch: &mut Vec<Command>) {
        let Some(analog) = &mut self.analog else {
            return;
        };
        let t = now - self.epoch;
        if on {
            eprintln!("aas: analog input: signal");
            let current = self.policy.current();
            analog.active = Some(current);
            self.policy.client_started(ANALOG, current, t);
        } else {
            eprintln!("aas: analog input: quiet");
            analog.active = None;
            self.policy.client_stopped(ANALOG, t);
        }
        self.decide(now, false, batch);
    }

    /// Gives each source at another rate than the output a resampler for
    /// it, and takes it away from the others. A stopped source keeps its
    /// own, so that a pause or a reset does not build a new one, which
    /// takes far longer than a period.
    fn sync_resamplers(&mut self, batch: &mut Vec<Command>) {
        let output = self.output;
        let (quality, format) = (self.quality, self.format);
        let streams = self.conns.values_mut().filter_map(|c| c.stream.as_mut());
        for stream in streams {
            let want = match output {
                Some((rate, period)) if stream.rate != rate => Some((rate, period)),
                _ => None,
            };
            if want == stream.resampler {
                continue;
            }
            stream.resampler = want;
            let resampler = want.map(|(rate, period)| {
                let r = Resampler::new(stream.rate, rate, period, quality, format);
                if r.input_frames_max() > stream.capacity {
                    eprintln!(
                        "aas: a ring of {} frames is too small to resample from {} Hz",
                        stream.capacity, stream.rate
                    );
                }
                Box::new(r)
            });
            batch.push(Command::SetResampler(stream.id, resampler));
        }
    }

    /// The card's rates have changed, such as when a card that was missing
    /// is found. The output moves at once to a rate the card takes.
    pub fn set_supported(&mut self, rates: Vec<u32>, now: Instant) {
        eprintln!("aas: the card takes the rates {rates:?}");
        self.supported = rates;
        if !self.update_rates() {
            eprintln!("aas: no rate the card takes works for every plugin chain");
        }
        let mut batch = Vec::new();
        self.update_dop(now, &mut batch);
        self.decide(now, true, &mut batch);
        self.send(batch);
    }

    /// Whether the output has been told to stay open.
    pub fn output_is_open(&self) -> bool {
        self.output.is_some()
    }

    /// Takes what the audio thread has sent and acts on it.
    pub fn handle_events(&mut self, now: Instant) {
        let mut batch = Vec::new();
        let mut events = std::mem::take(&mut self.stash);
        while let Ok(event) = self.events.pop() {
            events.push(event);
        }
        for event in events {
            match event {
                // Freed here, not on the audio thread.
                Event::Dropped(_)
                | Event::Replaced(_)
                | Event::OldTable(_)
                | Event::OldChain(_)
                | Event::OldTaps(_) => {}
                // The audio thread writes it no more, so a client that
                // reads it empty after this has all of it. A client that
                // gave the stream up meanwhile is told nothing it minds.
                // The client is woken: nothing else will wake one that
                // waits for frames.
                Event::DroppedTap(tap) => {
                    tap.ring.header().ended.store(1, Ordering::Release);
                    let one = 1u64;
                    // SAFETY: 8 bytes to a non-blocking eventfd. A full
                    // counter only means the client is woken already.
                    unsafe { libc::write(tap.wake.as_raw_fd(), (&raw const one).cast(), 8) };
                }
                Event::OldCapture(_) => {
                    if let Some(analog) = &mut self.analog {
                        analog.unanswered = analog.unanswered.saturating_sub(1);
                    }
                }
                Event::InputSignal(on) => self.input_signal(on, now, &mut batch),
                Event::CaptureFailed(mut capture) => {
                    // Off the audio thread, as the engine leaves it.
                    capture.close();
                    self.capture_failed(now);
                }
                Event::Reconfigured => {
                    self.reopening = self.reopening.saturating_sub(1);
                    self.open_capture(now, &mut batch);
                }
                Event::DopStarted(id, claim) => {
                    // The events can come in another order than the claims
                    // were made, when the queue was full meanwhile. The
                    // number says which is older, as the audio thread has it.
                    self.dop_claims.retain(|&(_, c)| c != id);
                    let at = self.dop_claims.partition_point(|&(c, _)| c < claim);
                    self.dop_claims.insert(at, (claim, id));
                    self.update_dop(now, &mut batch);
                }
                Event::DopStopped(id) => {
                    self.dop_claims.retain(|&(_, c)| c != id);
                    self.update_dop(now, &mut batch);
                    self.decide(now, false, &mut batch);
                }
                Event::Drained(id) => self.drained(id, now, &mut batch),
                Event::Flushed(id) => self.flushed(id),
            }
        }
        self.send(batch);
    }

    /// Applies what is due by the clock: a settled rate change, the end of
    /// a drain the audio thread never reported, and the idle timer. Such a
    /// drain is not ended while the output is being opened.
    pub fn tick(&mut self, now: Instant) {
        let mut batch = Vec::new();
        let before = self.policy.current();
        self.policy.tick(now - self.epoch);
        let settled = self.policy.current() != before;
        let mut overdue = Vec::new();
        let reopening = self.reopening > 0;
        for stream in self.streams_mut() {
            if !stream.drain.is_some_and(|at| now >= at) {
                continue;
            }
            // A drain that still makes progress is slow, not stuck: the
            // audio thread can fall behind real time when its cycles run
            // long. It gets more time for as long as it takes frames. So
            // does one that waits for the card to open: its audio has not
            // been played, and the time runs from when the card is open.
            let read = stream.view.header().read_pos.load(Ordering::Acquire);
            if read != stream.drain_seen || reopening {
                stream.drain_seen = read;
                stream.drain = Some(now + DRAIN_SPARE);
            } else {
                overdue.push(stream.id);
            }
        }
        for id in overdue {
            // A last resort: the audio thread should always report it.
            eprintln!("aas: a drain was never reported, answering it now");
            self.drained(id, now, &mut batch);
        }
        self.decide(now, settled, &mut batch);
        let retry = self.analog.as_ref().and_then(|a| a.retry_at);
        if retry.is_some_and(|at| now >= at) {
            self.open_capture(now, &mut batch);
        }
        // The analog input keeps the output open.
        let idle = if self.analog.is_some() {
            Idle::Zeros
        } else {
            self.idle
        };
        if let (Idle::Off { delay_s }, Some(since)) = (idle, self.idle_since) {
            let delay = Duration::from_secs(u64::from(delay_s));
            if self.output.is_some() && now >= since + delay {
                eprintln!("aas: output off after {delay_s} s idle");
                self.output = None;
                self.switch_at = None;
                batch.push(Command::CloseOutput);
            }
        }
        self.send(batch);
        if self.save_at.is_some_and(|at| now >= at) {
            self.save();
        }
    }

    /// Queues the commands in one commit and wakes the audio thread. Waits
    /// for room if needed, taking events out of the other queue meanwhile,
    /// because the audio thread stops taking commands while that is full.
    ///
    /// The wait has no limit: a command left out would leave the two
    /// threads with different ideas of the clients. The audio thread reads
    /// commands every cycle, so room comes within a period. A wait of a
    /// second means it is stuck, and is logged once.
    fn send(&mut self, batch: Vec<Command>) {
        let capacity = self.commands.buffer().capacity();
        let mut waiting_since: Option<Instant> = None;
        let mut warned = false;
        let mut batch = batch.into_iter().peekable();
        while batch.peek().is_some() {
            // Nobody is left to take them.
            if self.commands.is_abandoned() {
                return;
            }
            let want = batch.len().min(capacity);
            match self.commands.write_chunk_uninit(want) {
                Ok(chunk) => {
                    chunk.fill_from_iter(batch.by_ref().take(want));
                }
                Err(_) => {
                    while let Ok(event) = self.events.pop() {
                        self.stash.push(event);
                    }
                    self.audio.unpark();
                    let since = *waiting_since.get_or_insert_with(Instant::now);
                    if !warned && since.elapsed() >= SEND_WARN {
                        warned = true;
                        eprintln!(
                            "aas: the audio thread has taken no command for {} s, still waiting",
                            SEND_WARN.as_secs()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        self.audio.unpark();
    }
}

/// A non-blocking eventfd, as the audio thread writes them.
pub(crate) fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: a plain syscall; the descriptor is owned at once.
    let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::MemoryCapture;
    use crate::config::{self, Format, Quality};
    use crate::plugin::chain::Chain;
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_drain_reported_before_its_request_is_answered_at_once() {
        let (commands, _engine_commands) = rtrb::RingBuffer::new(64);
        let (mut engine_events, events) = rtrb::RingBuffer::new(64);
        let config = Config::default();
        let audio = std::thread::current();
        let mut control = Control::new(&config, 4.0, Vec::new(), commands, events, audio);
        let now = Instant::now();
        control.connected(1, "test".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        let said = Reply::Hello {
            version: proto::VERSION,
        };
        assert_eq!(control.request(1, Some(hello), now).reply, Some(said));
        let configure = Request::Configure {
            rate: 48000,
            format: ClientFormat::S32,
            channels: 2,
            buffer_frames: 4096,
            capture: false,
        };
        let answer = control.request(1, Some(configure), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        let start = control.request(1, Some(Request::Start), now);
        assert_eq!(start.reply, Some(Reply::Ok));
        // The client set the flag, and the audio thread saw the ring empty
        // before the request arrived.
        assert!(engine_events.push(Event::Drained(1)).is_ok());
        control.handle_events(now);
        let drain = control.request(1, Some(Request::Drain), now);
        assert_eq!(drain.reply, Some(Reply::Ok));
        // A restart forgets it.
        let start = control.request(1, Some(Request::Start), now);
        assert_eq!(start.reply, Some(Reply::Ok));
        assert!(
            control
                .request(1, Some(Request::Drain), now)
                .reply
                .is_none()
        );
    }

    /// A control thread with one connection that has said hello, and the
    /// audio thread's end of the command queue. One `Reconfigured` waits in
    /// the event queue, for a test to take with `handle_events` once it has
    /// made the output open.
    fn hello() -> (Control, rtrb::Consumer<Command>, Instant) {
        let (commands, engine_commands) = rtrb::RingBuffer::new(64);
        let (mut engine_events, events) = rtrb::RingBuffer::new(64);
        assert!(engine_events.push(Event::Reconfigured).is_ok());
        let config = Config::default();
        let audio = std::thread::current();
        let mut control = Control::new(&config, 12.0, Vec::new(), commands, events, audio);
        let now = Instant::now();
        control.connected(1, "test".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        let said = Reply::Hello {
            version: proto::VERSION,
        };
        assert_eq!(control.request(1, Some(hello), now).reply, Some(said));
        (control, engine_commands, now)
    }

    fn configure(rate: u32, buffer_frames: u32) -> Request {
        Request::Configure {
            rate,
            format: ClientFormat::S16,
            channels: 2,
            buffer_frames,
            capture: false,
        }
    }

    /// The source the audio thread was given.
    fn added(commands: &mut rtrb::Consumer<Command>) -> Box<Source> {
        while let Ok(command) = commands.pop() {
            if let Command::Add(source) = command {
                return source;
            }
        }
        panic!("no source added");
    }

    /// A card's mixer that records what it is set to.
    struct FakeMixer(std::sync::Arc<std::sync::Mutex<Vec<f32>>>);

    impl HardwareVolume for FakeMixer {
        fn set_db(&mut self, db: f32) {
            self.0.lock().unwrap().push(db);
        }

        fn range_db(&self) -> (f32, f32) {
            (-103.5, 0.0)
        }

        fn can_mute(&self) -> bool {
            true
        }
    }

    #[test]
    fn the_same_volume_again_is_written_to_the_card_but_not_announced() {
        let (mut control, mut commands, now) = hello();
        let set = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        control.card_changed(Some(Box::new(FakeMixer(set.clone()))));
        let volume = |percent| Some(Request::SetVolume { percent });
        assert_eq!(
            control
                .request(1, Some(Request::SubscribeVolume), now)
                .reply,
            Some(Reply::Volume { percent: 100 })
        );
        assert_eq!(control.request(1, volume(50), now).reply, Some(Reply::Ok));
        assert_eq!(*set.lock().unwrap(), [0.0, -30.0]);
        assert_eq!(control.take_notices().len(), 1);
        while commands.pop().is_ok() {}
        // Someone moved the card's mixer with another tool meanwhile. The
        // same percent again puts it back.
        assert_eq!(control.request(1, volume(50), now).reply, Some(Reply::Ok));
        assert_eq!(*set.lock().unwrap(), [0.0, -30.0, -30.0]);
        // Nothing changed for the subscribers, the engine or the state file.
        assert!(control.take_notices().is_empty());
        assert!(commands.pop().is_err());
        assert!(control.save_at.is_none());
    }

    #[test]
    fn the_header_view_follows_the_ring() {
        let (ring, memfd) = Ring::create(1 << 20).unwrap();
        let view = HeaderView::map(memfd.as_fd()).unwrap();
        ring.write(&[[1, 1]; 300]);
        ring.consume(200);
        assert_eq!(view.header().read_pos.load(Ordering::Acquire), 200);
        assert_eq!(view.header().write_pos.load(Ordering::Acquire), 300);
    }

    #[test]
    fn a_buffer_too_short_to_refill_a_period_is_refused() {
        let (mut control, _commands, now) = hello();
        for rate in [8000, 44100, 48000, 96000, 192000, 384000] {
            let least = min_buffer_frames(rate, 12.0);
            let short = control.request(1, Some(configure(rate, least - 1)), now);
            let code = libc::EINVAL as u32;
            assert_eq!(short.reply, Some(Reply::Error { code }), "{rate}");
            let enough = control.request(1, Some(configure(rate, least)), now);
            assert_eq!(enough.reply, Some(Reply::Configured { rate: 0 }), "{rate}");
        }
    }

    #[test]
    fn the_least_buffer_holds_two_periods_of_input_at_any_output_rate() {
        let rates = [
            8000, 11025, 22050, 44100, 48000, 96000, 176400, 192000, 384000,
        ];
        for latency_ms in [4.0, 12.0, 100.0] {
            for from in rates {
                let least = u64::from(min_buffer_frames(from, latency_ms));
                for to in rates {
                    let period = u64::from(period_frames(to, latency_ms));
                    let input = (period * u64::from(from)).div_ceil(u64::from(to));
                    assert!(least >= 2 * input, "{from} to {to} at {latency_ms} ms");
                }
            }
        }
    }

    #[test]
    fn a_request_during_a_drain_answers_the_drain_first() {
        let (mut control, _commands, now) = hello();
        let answer = control.request(1, Some(configure(48000, 4096)), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        assert_eq!(
            control.request(1, Some(Request::Start), now).reply,
            Some(Reply::Ok)
        );
        assert!(
            control
                .request(1, Some(Request::Drain), now)
                .reply
                .is_none()
        );
        let stop = control.request(1, Some(Request::Stop), now);
        assert_eq!(stop.reply, Some(Reply::Ok));
        let code = libc::ECANCELED as u32;
        assert_eq!(control.take_replies_for(1), vec![Reply::Error { code }]);
        // Nothing else is owed, now or later.
        control.tick(now + Duration::from_secs(60));
        assert!(control.take_replies().is_empty());
    }

    #[test]
    fn a_drain_that_makes_progress_is_not_cut_short() {
        let (mut control, mut commands, now) = hello();
        let answer = control.request(1, Some(configure(48000, 4096)), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        let source = added(&mut commands);
        source.ring.write(&[[1, 1]; 4096]);
        assert_eq!(
            control.request(1, Some(Request::Start), now).reply,
            Some(Reply::Ok)
        );
        assert!(
            control
                .request(1, Some(Request::Drain), now)
                .reply
                .is_none()
        );
        // The output has opened.
        control.handle_events(now);
        // The audio thread runs slower than real time but keeps taking
        // frames, well past the deadline set for the ring.
        let mut t = now;
        for _ in 0..20 {
            t += Duration::from_millis(400);
            source.ring.consume(100);
            control.tick(t);
            assert!(control.take_replies().is_empty(), "answered early");
        }
        // Once it stops taking frames the drain is answered after all.
        control.tick(t + Duration::from_millis(400));
        control.tick(t + Duration::from_millis(1000));
        assert_eq!(control.take_replies(), vec![(1, Reply::Ok)]);
    }

    #[test]
    fn a_drain_is_not_answered_while_the_output_is_still_opening() {
        let (mut control, mut commands, now) = hello();
        let answer = control.request(1, Some(configure(48000, 4096)), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        let source = added(&mut commands);
        source.ring.write(&[[1, 1]; 4096]);
        let start = control.request(1, Some(Request::Start), now);
        assert_eq!(start.reply, Some(Reply::Ok));
        let drain = control.request(1, Some(Request::Drain), now);
        assert!(drain.reply.is_none());
        // The card takes long to open, so nothing is played, far past the
        // time the ring lasts. The audio was not played, so no reply.
        let mut t = now;
        for _ in 0..20 {
            t += Duration::from_millis(400);
            control.tick(t);
            assert!(control.take_replies().is_empty(), "answered unplayed");
        }
        // Once it is open, the last resort applies again from then.
        control.handle_events(t);
        control.tick(t + Duration::from_millis(400));
        assert!(control.take_replies().is_empty());
        control.tick(t + Duration::from_millis(1000));
        assert_eq!(control.take_replies(), vec![(1, Reply::Ok)]);
    }

    /// Waits for the helper thread to have written this volume.
    fn saved(path: &std::path::Path, percent: u8) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if path.exists() && State::load(path).output_percent == percent {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn the_volume_is_saved_once_it_has_been_left_alone_for_a_second() {
        let (mut control, _commands, now) = hello();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let volume = OutputVolume::new(None, config::BitPerfect::Auto);
        control.restore_volume(volume, path.clone());
        let set = |percent| Some(Request::SetVolume { percent });
        control.request(1, set(40), now);
        // A slider still moving: each change starts the second again.
        control.request(1, set(30), now + Duration::from_millis(500));
        control.tick(now + Duration::from_millis(1499));
        assert!(control.save_at.is_some());
        assert!(!path.exists());
        control.tick(now + Duration::from_millis(1500));
        assert!(control.save_at.is_none());
        assert!(saved(&path, 30));
        // Nothing more is written until the next change.
        std::fs::remove_file(&path).unwrap();
        control.tick(now + Duration::from_secs(60));
        control.shutdown();
        assert!(!path.exists());
    }

    #[test]
    fn a_volume_still_waiting_is_on_disk_when_shutdown_returns() {
        let (mut control, _commands, now) = hello();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let volume = OutputVolume::new(None, config::BitPerfect::Auto);
        control.restore_volume(volume, path.clone());
        control.request(1, Some(Request::SetVolume { percent: 20 }), now);
        control.shutdown();
        assert_eq!(State::load(&path).output_percent, 20);
        // Only the state file is left, no temporary one.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_batch_larger_than_the_queue_arrives_whole_and_in_order() {
        let (commands, mut engine_commands) = rtrb::RingBuffer::new(4);
        let (_engine_events, events) = rtrb::RingBuffer::new(4);
        let audio = std::thread::current();
        let mut control = Control::new(
            &Config::default(),
            12.0,
            Vec::new(),
            commands,
            events,
            audio,
        );
        // The audio thread takes one command now and then.
        let reader = std::thread::spawn(move || {
            let mut gains = Vec::new();
            while gains.len() < 11 {
                match engine_commands.pop() {
                    Ok(Command::SetGain(gain)) => gains.push(gain),
                    Ok(_) => panic!("another command"),
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
            gains
        });
        control.send((0..11).map(|i| Command::SetGain(i as f32)).collect());
        let expect: Vec<f32> = (0..11).map(|i| i as f32).collect();
        assert_eq!(reader.join().unwrap(), expect);
    }

    #[test]
    fn the_oldest_dop_claim_owns_the_rate_whatever_order_the_events_come_in() {
        let (commands, _engine_commands) = rtrb::RingBuffer::new(64);
        let (mut engine_events, events) = rtrb::RingBuffer::new(64);
        let audio = std::thread::current();
        let mut control = Control::new(
            &Config::default(),
            12.0,
            Vec::new(),
            commands,
            events,
            audio,
        );
        let now = Instant::now();
        for (conn, rate) in [(1, 176400), (2, 352800)] {
            control.connected(conn, "test".into());
            let hello = Request::Hello {
                version: proto::VERSION,
            };
            control.request(conn, Some(hello), now);
            let buffer = min_buffer_frames(rate, 12.0);
            let answer = control.request(conn, Some(configure(rate, buffer)), now);
            assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
            let start = control.request(conn, Some(Request::Start), now);
            assert_eq!(start.reply, Some(Reply::Ok));
        }
        // The audio thread saw client 2 play DoP first, but its event could
        // not be sent until after the one for client 1.
        assert!(engine_events.push(Event::DopStarted(1, 8)).is_ok());
        assert!(engine_events.push(Event::DopStarted(2, 7)).is_ok());
        control.handle_events(now);
        assert_eq!(control.dop_owner, Some(2));
        assert_eq!(control.policy.current(), 352800);
        // Once the older claim ends, the other one holds the output.
        assert!(engine_events.push(Event::DopStopped(2)).is_ok());
        control.handle_events(now);
        assert_eq!(control.dop_owner, Some(1));
        assert_eq!(control.policy.current(), 176400);
    }

    #[test]
    fn a_drain_is_held_until_the_last_of_two_opens_is_done() {
        let (commands, mut engine_commands) = rtrb::RingBuffer::new(64);
        let (mut engine_events, events) = rtrb::RingBuffer::new(64);
        let audio = std::thread::current();
        let mut control = Control::new(
            &Config::default(),
            12.0,
            Vec::new(),
            commands,
            events,
            audio,
        );
        let now = Instant::now();
        control.connected(1, "test".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(1, Some(hello), now);
        let answer = control.request(1, Some(configure(48000, 4096)), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        let source = added(&mut engine_commands);
        source.ring.write(&[[1, 1]; 4096]);
        // The start opens the output, and new rates of the card open it
        // again at once.
        control.request(1, Some(Request::Start), now);
        control.set_supported(vec![44100], now);
        assert!(
            control
                .request(1, Some(Request::Drain), now)
                .reply
                .is_none()
        );
        // The first open is done. The second is still under way.
        assert!(engine_events.push(Event::Reconfigured).is_ok());
        control.handle_events(now);
        let mut t = now;
        for _ in 0..20 {
            t += Duration::from_millis(400);
            control.tick(t);
            assert!(control.take_replies().is_empty(), "answered unplayed");
        }
        assert!(engine_events.push(Event::Reconfigured).is_ok());
        control.handle_events(t);
        control.tick(t + Duration::from_millis(1000));
        assert_eq!(control.take_replies(), vec![(1, Reply::Ok)]);
    }

    /// Opens `MemoryCapture`s for the control thread and records the rate
    /// and period of each open. The next `fails` opens fail.
    #[derive(Clone, Default)]
    struct Opener {
        asked: Arc<Mutex<Vec<(u32, u32)>>>,
        fails: Arc<Mutex<u32>>,
    }

    impl Opener {
        fn open(&self) -> OpenCapture {
            let me = self.clone();
            Box::new(move |rate, period| {
                me.asked.lock().unwrap().push((rate, period));
                let mut fails = me.fails.lock().unwrap();
                if *fails > 0 {
                    *fails -= 1;
                    return Err("no such device".into());
                }
                let (capture, _feed) = MemoryCapture::new();
                Ok(Box::new(capture) as Box<dyn Capture>)
            })
        }

        fn asked(&self) -> Vec<(u32, u32)> {
            self.asked.lock().unwrap().clone()
        }

        fn fail(&self, times: u32) {
            *self.fails.lock().unwrap() = times;
        }
    }

    /// The audio thread as far as the control thread can tell: it takes the
    /// commands, answers each as the engine does, and names them.
    struct FakeEngine {
        commands: rtrb::Consumer<Command>,
        events: rtrb::Producer<Event>,
        capture: Option<Box<dyn Capture>>,
    }

    impl FakeEngine {
        /// Takes every queued command and answers it. A reopen answers at
        /// once only with `opened`.
        fn take(&mut self, opened: bool) -> Vec<String> {
            let mut seen = Vec::new();
            while let Ok(command) = self.commands.pop() {
                let (name, answer) = match command {
                    Command::Add(_) => ("Add".into(), None),
                    Command::Remove(_) => ("Remove".into(), None),
                    Command::SetRunning(id, on) => (format!("SetRunning {id} {on}"), None),
                    Command::Flush(id) => (format!("Flush {id}"), Some(Event::Flushed(id))),
                    Command::SetResampler(id, r) => (
                        format!("SetResampler {id} {}", r.is_some()),
                        Some(Event::Replaced(r)),
                    ),
                    Command::Reconfigure { rate, period } => (
                        format!("Reconfigure {rate} {period}"),
                        opened.then_some(Event::Reconfigured),
                    ),
                    Command::CloseOutput => ("CloseOutput".into(), None),
                    Command::SetGain(_) => ("SetGain".into(), None),
                    Command::Grow(t) => ("Grow".into(), Some(Event::OldTable(t))),
                    Command::SetChain(place, chain) => {
                        let names: Vec<&str> = chain.names().collect();
                        let name = format!("SetChain {place:?} {}", names.join(","));
                        (name, Some(Event::OldChain(chain)))
                    }
                    Command::SetParam { .. } => ("SetParam".into(), None),
                    Command::SetCapture(c) => {
                        let old = std::mem::replace(&mut self.capture, c);
                        let name = format!("SetCapture {}", self.capture.is_some());
                        (name, Some(Event::OldCapture(old)))
                    }
                    Command::SetAnalogGain(g) => (format!("SetAnalogGain {g}"), None),
                    Command::AddTap(t) => (format!("AddTap {}", t.id), None),
                    Command::RemoveTap(id) => (format!("RemoveTap {id}"), None),
                    Command::GrowTaps(t) => ("GrowTaps".into(), Some(Event::OldTaps(t))),
                    Command::Shutdown => ("Shutdown".into(), None),
                };
                seen.push(name);
                if let Some(event) = answer {
                    assert!(self.events.push(event).is_ok());
                }
            }
            seen
        }

        fn send(&mut self, event: Event) {
            assert!(self.events.push(event).is_ok());
        }
    }

    /// A started control thread with one client said hello on connection
    /// 1, the analog input on if `opener` is given, and the fake engine.
    fn started(config: &Config, opener: Option<&Opener>) -> (Control, FakeEngine, Instant) {
        let (commands, engine_commands) = rtrb::RingBuffer::new(256);
        let (engine_events, events) = rtrb::RingBuffer::new(256);
        let audio = std::thread::current();
        let mut control = Control::new(config, 12.0, Vec::new(), commands, events, audio);
        if let Some(opener) = opener {
            control.set_analog(opener.open());
        }
        let now = Instant::now();
        control.start(now);
        let engine = FakeEngine {
            commands: engine_commands,
            events: engine_events,
            capture: None,
        };
        (control, engine, now)
    }

    /// Connects a client on `conn` and has it play at `rate`.
    fn play(control: &mut Control, conn: ConnId, rate: u32, now: Instant) {
        control.connected(conn, "test".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(conn, Some(hello), now);
        let buffer = min_buffer_frames(rate, 12.0);
        let answer = control.request(conn, Some(configure(rate, buffer)), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        let start = control.request(conn, Some(Request::Start), now);
        assert_eq!(start.reply, Some(Reply::Ok));
    }

    fn has(seen: &[String], what: &str) -> bool {
        seen.iter().any(|s| s.starts_with(what))
    }

    fn analog_config() -> Config {
        Config {
            analog: config::Analog {
                enabled: true,
                volume_db: -20.0,
            },
            ..Config::default()
        }
    }

    /// Connects a client on `conn` and configures a capture stream.
    /// Returns the client's mapping of the ring.
    fn record(control: &mut Control, conn: ConnId, now: Instant) -> (Ring, u32) {
        control.connected(conn, "recorder".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(conn, Some(hello), now);
        let capture = Request::Configure {
            rate: 0,
            format: ClientFormat::S32,
            channels: 2,
            buffer_frames: 4096,
            capture: true,
        };
        let answer = control.request(conn, Some(capture), now);
        let Some(Reply::Configured { rate }) = answer.reply else {
            panic!("not configured: {:?}", answer.reply);
        };
        (Ring::map(answer.fds[0].as_fd()).unwrap(), rate)
    }

    fn ask_start(control: &mut Control, conn: ConnId, now: Instant) -> Answer {
        control.request(conn, Some(Request::Start), now)
    }

    #[test]
    fn a_recorder_is_ended_only_once_its_tap_is_back() {
        let (mut control, mut engine, now) = started(&Config::default(), None);
        engine.take(true);
        control.handle_events(now);
        let (ring, rate) = record(&mut control, 2, now);
        assert_eq!(rate, 48000);
        let mut tap = None;
        while let Ok(command) = engine.commands.pop() {
            if let Command::AddTap(t) = command {
                tap = Some(t);
            }
        }
        let tap = tap.expect("no tap added");
        let ok = Some(Reply::Ok);
        assert_eq!(ask_start(&mut control, 2, now).reply, ok);
        // A player at another rate moves the output, and the tap goes.
        play(&mut control, 1, 44100, now);
        let seen = engine.take(false);
        let removed = format!("RemoveTap {}", tap.id);
        assert!(has(&seen, &removed), "{seen:?}");
        assert!(has(&seen, "Reconfigure 44100"), "{seen:?}");
        // The audio thread may still write it, so the client is not told
        // yet. A start is refused already.
        assert_eq!(ring.header().ended.load(Ordering::Acquire), 0);
        let code = libc::ENODEV as u32;
        let refused = Some(Reply::Error { code });
        assert_eq!(ask_start(&mut control, 2, now).reply, refused);
        engine.send(Event::DroppedTap(tap));
        control.handle_events(now);
        assert_eq!(ring.header().ended.load(Ordering::Acquire), 1);
        assert_eq!(ask_start(&mut control, 2, now).reply, refused);
    }

    /// The count an eventfd holds, which a read takes. Zero if it is not
    /// readable.
    fn wake_count(wake: &OwnedFd) -> u64 {
        let mut count = 0u64;
        // SAFETY: 8 bytes from a non-blocking eventfd.
        unsafe { libc::read(wake.as_raw_fd(), (&raw mut count).cast(), 8) };
        count
    }

    #[test]
    fn a_recorder_is_woken_when_its_stream_ends() {
        let (mut control, mut engine, now) = started(&Config::default(), None);
        engine.take(true);
        control.handle_events(now);
        let (_ring, _) = record(&mut control, 2, now);
        let mut tap = None;
        while let Ok(command) = engine.commands.pop() {
            if let Command::AddTap(t) = command {
                tap = Some(t);
            }
        }
        let tap = tap.expect("no tap added");
        // The recorder's copy of the wake-up.
        let wake = tap.wake.try_clone().unwrap();
        assert_eq!(ask_start(&mut control, 2, now).reply, Some(Reply::Ok));
        play(&mut control, 1, 44100, now);
        engine.take(false);
        assert_eq!(wake_count(&wake), 0);
        // A recorder asleep in poll, with nothing more to come, has to
        // hear that the stream is over.
        engine.send(Event::DroppedTap(tap));
        control.handle_events(now);
        assert_eq!(wake_count(&wake), 1);
    }

    #[test]
    fn with_the_analog_input_on_the_output_opens_at_startup() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        let seen = engine.take(true);
        let gain = crate::format::db_to_gain(-20.0);
        assert_eq!(
            seen,
            [
                "SetGain".to_string(),
                format!("SetAnalogGain {gain}"),
                "SetChain Analog detect".into(),
                "Reconfigure 48000 256".into(),
            ]
        );
        // The capture opens once the output is open, not before.
        assert!(opener.asked().is_empty());
        control.handle_events(now);
        assert_eq!(opener.asked(), [(48000, 256)]);
        assert_eq!(engine.take(true), ["SetCapture true"]);
        control.handle_events(now);
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn with_the_analog_input_on_the_output_stays_open_when_idle() {
        let config = Config {
            idle: Idle::Off { delay_s: 1 },
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        // A player comes and goes, and the input has signal for a while.
        play(&mut control, 1, 48000, now);
        engine.send(Event::InputSignal(true));
        control.handle_events(now);
        let stop = control.request(1, Some(Request::Stop), now);
        assert_eq!(stop.reply, Some(Reply::Ok));
        engine.send(Event::InputSignal(false));
        control.handle_events(now);
        control.tick(now + Duration::from_secs(120));
        assert!(control.output_is_open());
        assert!(!has(&engine.take(true), "CloseOutput"));
        // Without the input the same config closes it.
        let config = Config {
            idle: Idle::Off { delay_s: 1 },
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        control.tick(now + Duration::from_secs(120));
        assert!(has(&engine.take(true), "CloseOutput"));
    }

    #[test]
    fn the_capture_is_opened_after_each_reconfigure_at_its_rate() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        // The card's rates come before the first open is done: two opens
        // are under way, and the capture waits for the last.
        control.set_supported(vec![44100, 96000], now);
        let seen = engine.take(false);
        assert!(has(&seen, "Reconfigure 48000"));
        assert!(has(&seen, "Reconfigure 44100 256"));
        engine.send(Event::Reconfigured);
        control.handle_events(now);
        assert!(opener.asked().is_empty());
        engine.send(Event::Reconfigured);
        control.handle_events(now);
        assert_eq!(opener.asked(), [(44100, 256)]);
        assert_eq!(engine.take(true), ["SetCapture true"]);
        control.handle_events(now);
        // The next rate the card takes moves the output, and the capture
        // follows it. The old one comes back to be dropped here.
        control.set_supported(vec![96000], now);
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 96000 512"));
        control.handle_events(now);
        assert_eq!(opener.asked(), [(44100, 256), (96000, 512)]);
        assert_eq!(engine.take(true), ["SetCapture true"]);
    }

    #[test]
    fn an_active_analog_input_is_the_first_client() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        engine.send(Event::InputSignal(true));
        control.handle_events(now);
        // A player at 96 kHz arriving while it plays at 48 kHz is resampled.
        play(&mut control, 1, 96000, now);
        let seen = engine.take(true);
        assert!(has(&seen, "SetResampler 1 true"), "{seen:?}");
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        control.tick(now + Duration::from_secs(5));
        assert!(!has(&engine.take(true), "Reconfigure"));
        assert_eq!(opener.asked(), [(48000, 256)]);
    }

    #[test]
    fn a_silent_analog_input_does_not_hold_the_rate() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        // A player at 96 kHz arriving then sets 96 kHz.
        play(&mut control, 1, 96000, now);
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 96000 512"), "{seen:?}");
        // The resampler it was given while it was only configured goes.
        let resampler = seen.iter().rfind(|s| s.starts_with("SetResampler 1"));
        assert_eq!(resampler.map(String::as_str), Some("SetResampler 1 false"));
        // And the capture reopens at it.
        control.handle_events(now);
        assert_eq!(opener.asked(), [(48000, 256), (96000, 512)]);
        assert_eq!(engine.take(true), ["SetCapture true"]);
    }

    #[test]
    fn the_analog_input_going_quiet_lets_the_remaining_player_take_the_rate() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        engine.send(Event::InputSignal(true));
        control.handle_events(now);
        play(&mut control, 1, 96000, now);
        engine.take(true);
        engine.send(Event::InputSignal(false));
        control.handle_events(now);
        // The rate settles first, as for a player that stops.
        let ms = Duration::from_millis;
        control.tick(now + ms(400));
        assert!(!has(&engine.take(true), "Reconfigure"));
        control.tick(now + ms(600));
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 96000 512"), "{seen:?}");
        // The player needs no resampler at its own rate.
        assert!(has(&seen, "SetResampler 1 false"), "{seen:?}");
        control.handle_events(now + ms(600));
        assert_eq!(opener.asked(), [(48000, 256), (96000, 512)]);
        // Signal again: the input runs at the output's rate, 96 kHz now.
        // When the player stops, the input holds that rate.
        engine.take(true);
        engine.send(Event::InputSignal(true));
        control.handle_events(now + ms(700));
        let stop = control.request(1, Some(Request::Stop), now + ms(700));
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(now + ms(5000));
        assert!(!has(&engine.take(true), "Reconfigure"));
        assert_eq!(control.policy.current(), 96000);
    }

    /// The analog input's rate in the rules, while it has signal.
    fn analog_rate(control: &Control) -> Option<u32> {
        control.analog.as_ref().unwrap().active
    }

    /// The capture is silent for `SETTLE_MS` after each start and the
    /// detector needs a window, so on a board the signal is back no sooner
    /// than this after a reopen.
    const SIGNAL_BACK: Duration = Duration::from_millis(1050);

    #[test]
    fn after_dop_a_lone_analog_input_leaves_the_card_at_the_dop_rate() {
        let ms = Duration::from_millis;
        // DoP stops before the signal is back, and well after.
        for held in [ms(300), ms(5000)] {
            let opener = Opener::default();
            let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
            engine.take(true);
            control.handle_events(now);
            engine.take(true);
            engine.send(Event::InputSignal(true));
            control.handle_events(now);
            assert_eq!(analog_rate(&control), Some(48000));
            // A player at 176.4 kHz is resampled to the input's 48 kHz,
            // until it plays DoP, which takes the output at its rate.
            play(&mut control, 1, 176400, now);
            assert!(!has(&engine.take(true), "Reconfigure"));
            engine.send(Event::DopStarted(1, 1));
            control.handle_events(now);
            let seen = engine.take(false);
            assert!(has(&seen, "Reconfigure 176400 1024"), "{seen:?}");
            // The input follows the output rate in the rules.
            assert_eq!(analog_rate(&control), Some(176400));
            // The reopen closes the capture and the new detector has no
            // hold, so the input goes quiet. The capture opens again at the
            // rate the output runs at.
            engine.send(Event::InputSignal(false));
            engine.send(Event::Reconfigured);
            control.handle_events(now + ms(10));
            assert_eq!(opener.asked(), [(48000, 256), (176400, 1024)]);
            assert_eq!(engine.take(true), ["SetCapture true"]);
            // DoP ends: the player stops.
            let stop = control.request(1, Some(Request::Stop), now + held);
            assert_eq!(stop.reply, Some(Reply::Ok));
            control.tick(now + held);
            // The signal is back once the capture's mute is over.
            let back = now + ms(10) + SIGNAL_BACK;
            control.tick(back);
            engine.send(Event::InputSignal(true));
            control.handle_events(back.max(now + held));
            assert_eq!(analog_rate(&control), Some(176400), "{held:?}");
            // Nothing returns the card to 48 kHz: it keeps its rate, as it
            // does when idle, and the input runs at it.
            control.tick(now + held + ms(10000));
            let seen = engine.take(true);
            assert!(!has(&seen, "Reconfigure"), "{held:?}: {seen:?}");
            assert_eq!(control.policy.current(), 176400, "{held:?}");
            assert_eq!(opener.asked().len(), 2, "{held:?}");
        }
    }

    #[test]
    fn a_dop_stream_that_is_the_first_client_leaves_the_analog_input_at_its_rate() {
        let ms = Duration::from_millis;
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 176400, now);
        engine.send(Event::DopStarted(1, 1));
        control.handle_events(now);
        assert!(has(&engine.take(true), "Reconfigure 176400 1024"));
        control.handle_events(now);
        engine.take(true);
        let back = now + SIGNAL_BACK;
        engine.send(Event::InputSignal(true));
        control.handle_events(back);
        assert_eq!(analog_rate(&control), Some(176400));
        let stop = control.request(1, Some(Request::Stop), back + ms(1000));
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(back + ms(10000));
        assert!(!has(&engine.take(true), "Reconfigure"));
        assert_eq!(control.policy.current(), 176400);
    }

    #[test]
    fn an_analog_input_with_signal_during_dop_follows_the_output_back() {
        let ms = Duration::from_millis;
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        // A player at 48 kHz holds the rate, another at 176.4 kHz plays DoP.
        play(&mut control, 1, 48000, now);
        play(&mut control, 2, 176400, now);
        engine.send(Event::DopStarted(2, 1));
        control.handle_events(now);
        assert!(has(&engine.take(true), "Reconfigure 176400 1024"));
        control.handle_events(now);
        engine.take(true);
        // The input gets signal while DoP holds the output, and is in the
        // rules at the rate the output runs at.
        let back = now + SIGNAL_BACK;
        engine.send(Event::InputSignal(true));
        control.handle_events(back);
        assert_eq!(analog_rate(&control), Some(176400));
        // DoP stops: the 48 kHz player takes the output back, and the
        // input follows it.
        let stop = control.request(2, Some(Request::Stop), back + ms(100));
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(back + ms(700));
        assert!(has(&engine.take(true), "Reconfigure 48000 256"));
        assert_eq!(analog_rate(&control), Some(48000));
        // The player leaves. The input does not move the output.
        let stop = control.request(1, Some(Request::Stop), back + ms(800));
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(back + ms(10000));
        assert!(!has(&engine.take(true), "Reconfigure"));
        assert_eq!(control.policy.current(), 48000);
    }

    #[test]
    fn a_failed_capture_is_closed_here() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        let (capture, feed) = MemoryCapture::new();
        engine.send(Event::CaptureFailed(Box::new(capture)));
        control.handle_events(now);
        assert!(feed.closed());
    }

    #[test]
    fn a_failed_capture_is_retried_every_second() {
        let opener = Opener::default();
        opener.fail(2);
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        let ms = Duration::from_millis;
        // The open after the output opens fails, and so does the next.
        control.handle_events(now);
        assert_eq!(opener.asked().len(), 1);
        assert!(engine.take(true).is_empty());
        control.tick(now + ms(900));
        assert_eq!(opener.asked().len(), 1);
        control.tick(now + ms(1000));
        assert_eq!(opener.asked().len(), 2);
        assert!(engine.take(true).is_empty());
        control.tick(now + ms(1500));
        assert_eq!(opener.asked().len(), 2);
        control.tick(now + ms(2000));
        assert_eq!(opener.asked().len(), 3);
        assert_eq!(engine.take(true), ["SetCapture true"]);
        control.handle_events(now + ms(2000));
        // It fails while it runs. The engine has closed it.
        engine.send(Event::CaptureFailed(Box::new(MemoryCapture::new().0)));
        control.handle_events(now + ms(3000));
        assert_eq!(opener.asked().len(), 3);
        control.tick(now + ms(3900));
        assert_eq!(opener.asked().len(), 3);
        control.tick(now + ms(4000));
        assert_eq!(opener.asked().len(), 4);
        // A failure that comes before the answer to the new capture is the
        // old one's. The new one is kept, and not opened again.
        engine.send(Event::CaptureFailed(Box::new(MemoryCapture::new().0)));
        control.handle_events(now + ms(4000));
        assert_eq!(engine.take(true), ["SetCapture true"]);
        control.handle_events(now + ms(4000));
        control.tick(now + ms(10000));
        assert_eq!(opener.asked().len(), 4);
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn chains_are_rebuilt_at_the_new_rate() {
        let mix = config::PluginConfig {
            place: config::PluginPlace::Mix,
            kind: "balance".into(),
            path: None,
            params: Default::default(),
        };
        let config = Config {
            idle: Idle::Off { delay_s: 1 },
            plugins: vec![mix],
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        // Without the analog input only the places with plugins get one.
        assert_eq!(
            engine.take(true),
            ["SetGain", "SetChain Mix balance", "Reconfigure 48000 256"]
        );
        control.handle_events(now);
        play(&mut control, 1, 96000, now);
        let seen = engine.take(true);
        let at = |what: &str| seen.iter().position(|s| s.starts_with(what));
        let (chain, reopen) = (at("SetChain Mix"), at("Reconfigure 96000"));
        assert!(chain.is_some() && chain < reopen, "{seen:?}");
        control.handle_events(now);
        // An idle close and a reopen at the same rate keep the chains, so
        // their filters keep their state.
        let stop = control.request(1, Some(Request::Stop), now);
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(now + Duration::from_secs(2));
        assert!(has(&engine.take(true), "CloseOutput"));
        let start = control.request(1, Some(Request::Start), now + Duration::from_secs(3));
        assert_eq!(start.reply, Some(Reply::Ok));
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 96000"), "{seen:?}");
        assert!(!has(&seen, "SetChain"), "{seen:?}");
        // Without plugins no chain is ever sent.
        let (mut control, mut engine, now) = started(&Config::default(), None);
        control.handle_events(now);
        play(&mut control, 1, 96000, now);
        assert!(!has(&engine.take(true), "SetChain"));
    }

    fn entry(place: config::PluginPlace, kind: &str) -> config::PluginConfig {
        config::PluginConfig {
            place,
            kind: kind.into(),
            path: None,
            params: Default::default(),
        }
    }

    /// What the engine was sent, without the volume.
    fn sent(engine: &mut FakeEngine) -> Vec<String> {
        let seen = engine.take(true);
        seen.into_iter().filter(|s| s != "SetGain").collect()
    }

    #[test]
    fn a_rate_a_chain_cannot_be_built_at_is_not_used() {
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Mix, "only-up-to-48k")],
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        control.set_supported(vec![44100, 48000, 96000], now);
        engine.take(true);
        control.handle_events(now);
        // A player at 96 kHz: the nearest rate the chain takes is 48 kHz,
        // and the player is resampled to it.
        play(&mut control, 1, 96000, now);
        let seen = sent(&mut engine);
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        assert!(has(&seen, "SetResampler 1 true"), "{seen:?}");
        control.tick(now + Duration::from_secs(5));
        assert!(!has(&engine.take(true), "Reconfigure"));
        assert_eq!(control.policy.current(), 48000);
    }

    #[test]
    fn with_any_rate_the_card_takes_a_refused_one_moves_to_the_nearest_that_works() {
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Output, "only-up-to-48k")],
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        engine.take(true);
        control.handle_events(now);
        play(&mut control, 1, 96000, now);
        let seen = sent(&mut engine);
        // 96, 88.2 and 64 kHz are refused in turn, and nothing is sent for
        // them: the output stays at 48 kHz, with its chain.
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        assert!(!has(&seen, "SetChain"), "{seen:?}");
        assert!(has(&seen, "SetResampler 1 true"), "{seen:?}");
        assert_eq!(control.policy.current(), 48000);
    }

    #[test]
    fn a_chain_is_never_sent_short_of_its_plugins() {
        // A parameter the plugin does not have fails at every rate.
        let mut nope = entry(config::PluginPlace::Mix, "balance");
        nope.params.insert("nope".into(), 1.0);
        let config = Config {
            plugins: vec![nope, entry(config::PluginPlace::Output, "balance")],
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        // No rate works, so the output is not opened at all.
        let seen = sent(&mut engine);
        assert!(!has(&seen, "SetChain"), "{seen:?}");
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        play(&mut control, 1, 44100, now);
        let seen = sent(&mut engine);
        assert!(
            !has(&seen, "SetChain") && !has(&seen, "Reconfigure"),
            "{seen:?}"
        );
    }

    #[test]
    fn the_analog_chain_has_no_fallback_either() {
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Analog, "only-up-to-48k")],
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        control.set_supported(vec![48000, 96000], now);
        let seen = sent(&mut engine);
        assert!(has(&seen, "SetChain Analog balance,detect"), "{seen:?}");
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 96000, now);
        let seen = sent(&mut engine);
        assert!(
            !has(&seen, "Reconfigure") && !has(&seen, "SetChain"),
            "{seen:?}"
        );
    }

    #[test]
    fn dop_rebuilds_only_the_analog_chain_and_the_others_keep_their_state() {
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Mix, "balance")],
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 48000, now);
        play(&mut control, 2, 176400, now);
        engine.take(true);
        engine.send(Event::DopStarted(2, 1));
        control.handle_events(now);
        let seen = sent(&mut engine);
        assert!(has(&seen, "Reconfigure 176400"), "{seen:?}");
        assert!(has(&seen, "SetChain Analog detect"), "{seen:?}");
        assert!(!has(&seen, "SetChain Mix"), "{seen:?}");
        control.handle_events(now);
        engine.take(true);
        // DoP stops, and the 48 kHz player has the output back: the mix
        // chain is the one it had, and the analog chain is built again.
        let stop = control.request(2, Some(Request::Stop), now);
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(now + Duration::from_secs(1));
        let seen = sent(&mut engine);
        assert!(has(&seen, "Reconfigure 48000"), "{seen:?}");
        assert!(has(&seen, "SetChain Analog detect"), "{seen:?}");
        assert!(!has(&seen, "SetChain Mix"), "{seen:?}");
    }

    #[test]
    fn dop_at_a_rate_the_analog_chain_refuses_does_not_take_the_output() {
        let config = Config {
            plugins: vec![
                entry(config::PluginPlace::Analog, "only-up-to-48k"),
                entry(config::PluginPlace::Mix, "balance"),
            ],
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        control.set_supported(vec![48000, 176400], now);
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 176400, now);
        engine.send(Event::DopStarted(1, 1));
        control.handle_events(now);
        let seen = sent(&mut engine);
        assert!(!has(&seen, "Reconfigure 176400"), "{seen:?}");
        assert_eq!(control.policy.current(), 48000);
    }

    #[test]
    fn a_rate_only_the_mix_chain_refuses_still_carries_dop() {
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Mix, "only-up-to-48k")],
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        control.set_supported(vec![48000, 176400], now);
        engine.take(true);
        control.handle_events(now);
        // A PCM player at 176.4 kHz: the mix chain refuses the rate.
        play(&mut control, 1, 176400, now);
        engine.take(true);
        assert_eq!(control.policy.current(), 48000);
        let stop = control.request(1, Some(Request::Stop), now);
        assert_eq!(stop.reply, Some(Reply::Ok));
        engine.take(true);
        // A DoP stream at that rate bypasses the mix chain, so it has it.
        play(&mut control, 2, 176400, now);
        engine.send(Event::DopStarted(2, 1));
        control.handle_events(now);
        let seen = sent(&mut engine);
        assert!(has(&seen, "Reconfigure 176400"), "{seen:?}");
        assert!(!has(&seen, "SetChain Mix"), "{seen:?}");
    }

    #[test]
    fn the_card_kept_at_a_dop_rate_gets_chains_for_it_when_pcm_plays_there() {
        let ms = Duration::from_millis;
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Mix, "balance")],
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        // The mix chain is built for a player at 48 kHz, then DoP takes
        // the output to its rate.
        play(&mut control, 1, 48000, now);
        play(&mut control, 2, 176400, now);
        engine.send(Event::DopStarted(2, 1));
        control.handle_events(now);
        let seen = sent(&mut engine);
        assert!(has(&seen, "Reconfigure 176400"), "{seen:?}");
        assert!(!has(&seen, "SetChain Mix"), "{seen:?}");
        control.handle_events(now);
        engine.take(true);
        // DoP ends with no client left: the card keeps its rate, and the
        // analog input, back after its mute, plays there through the mix.
        let stop = control.request(1, Some(Request::Stop), now + ms(50));
        assert_eq!(stop.reply, Some(Reply::Ok));
        let stop = control.request(2, Some(Request::Stop), now + ms(100));
        assert_eq!(stop.reply, Some(Reply::Ok));
        engine.send(Event::InputSignal(true));
        control.handle_events(now + SIGNAL_BACK);
        control.tick(now + SIGNAL_BACK);
        let seen = sent(&mut engine);
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        assert!(has(&seen, "SetChain Mix balance"), "{seen:?}");
    }

    #[test]
    fn the_chains_built_at_startup_are_sent_at_the_first_open() {
        let mix = config::PluginConfig {
            place: config::PluginPlace::Mix,
            kind: "balance".into(),
            path: None,
            params: Default::default(),
        };
        let config = Config {
            plugins: vec![mix],
            ..Config::default()
        };
        let (commands, engine_commands) = rtrb::RingBuffer::new(256);
        let (engine_events, events) = rtrb::RingBuffer::new(256);
        let audio = std::thread::current();
        let mut control = Control::new(&config, 12.0, Vec::new(), commands, events, audio);
        let mut libraries = Libraries::default();
        let mut chains =
            Chains::build(&config.plugins, 48000, Format::F32, &mut libraries).unwrap();
        // A chain built ahead stands out by what it holds.
        chains.mix = Chain::empty();
        control.set_plugins(libraries, Some(chains));
        control.start(Instant::now());
        let mut engine = FakeEngine {
            commands: engine_commands,
            events: engine_events,
            capture: None,
        };
        assert_eq!(
            engine.take(true),
            ["SetGain", "SetChain Mix ", "Reconfigure 48000 256"]
        );
    }

    #[test]
    fn a_ring_holds_one_period_of_input_at_any_output_rate() {
        // Every rate a card is asked about. A ring of the least size never
        // makes `sync_resamplers` log that it is too small to resample from.
        let rates = [
            8000, 11025, 16000, 22050, 32000, 44100, 48000, 64000, 88200, 96000, 176400, 192000,
            352800, 384000,
        ];
        for latency_ms in [4.0, 12.0, 100.0] {
            for from in rates {
                let ring = min_ring_frames(from, latency_ms) as usize;
                for to in rates.into_iter().filter(|&to| to != from) {
                    let period = period_frames(to, latency_ms);
                    let r = Resampler::new(from, to, period, Quality::High, Format::F32);
                    let need = r.input_frames_max();
                    assert!(
                        need <= ring,
                        "{from} to {to} at {latency_ms} ms: {need} > {ring}"
                    );
                }
            }
        }
    }
}
