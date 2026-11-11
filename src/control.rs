//! The control thread's decisions: which clients play, at what rate, and
//! when the output is open. The audio thread only carries them out.
//!
//! Everything the audio thread needs is built here, and whatever it hands
//! back is freed here. Commands that belong together go into the queue in
//! one commit, so the audio thread never sees half a decision.

use crate::config::{Config, Format, Idle, Quality};
use crate::dop;
use crate::engine::{Command, Event, INITIAL_SOURCES, Source};
use crate::latency::{PERIODS, period_frames};
use crate::rate::{ClientId, MAX_RATE, MIN_RATE, RatePolicy, SETTLE};
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

struct Conn {
    name: String,
    hello: bool,
    stream: Option<Stream>,
    /// Told of every change to the output volume.
    subscribed: bool,
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
        }
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
    /// zeros, and the idle rule applies from now.
    pub fn start(&mut self, now: Instant) {
        let mut batch = vec![Command::SetGain(self.volume.gain())];
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
        match request {
            Request::Hello { .. } => Answer::reply(Reply::Hello {
                version: proto::VERSION,
            }),
            Request::Configure {
                rate,
                format,
                channels,
                buffer_frames,
            } => self.configure(conn, rate, format, channels, buffer_frames, now),
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
            reply: Some(Reply::Configured),
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

    /// The audio thread has emptied a stream's ring: its reset is answered.
    fn flushed(&mut self, id: ClientId) {
        let conn = self
            .conns
            .iter()
            .find(|(_, c)| c.stream.as_ref().is_some_and(|s| s.id == id));
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

    fn is_supported(&self, rate: u32) -> bool {
        self.supported.is_empty() || self.supported.contains(&rate)
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
        let running = self.running_count() > 0;
        if running {
            self.idle_since = None;
        } else {
            self.idle_since.get_or_insert(now);
        }
        let want = self.policy.current();
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
        }
    }

    /// Opens or reopens the output at the rate the rules want. Resamplers
    /// for the new rate go first, so that they take effect with it.
    fn open_output(&mut self, batch: &mut Vec<Command>) {
        let rate = self.policy.current();
        let period = period_frames(rate, self.latency_ms);
        eprintln!("aas: output at {rate} Hz, {period} frames per period");
        self.output = Some((rate, period));
        self.reopening += 1;
        self.switch_at = None;
        self.sync_resamplers(batch);
        batch.push(Command::Reconfigure { rate, period });
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
        self.supported = rates.clone();
        self.policy.set_supported(rates);
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
                | Event::OldCapture(_) => {}
                // The control thread does not run the analog input yet.
                Event::InputSignal(_) | Event::CaptureFailed => {}
                Event::Reconfigured => self.reopening = self.reopening.saturating_sub(1),
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
        if let (Idle::Off { delay_s }, Some(since)) = (self.idle, self.idle_since) {
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
    use crate::config::{self, Format, Quality};

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
        };
        let answer = control.request(1, Some(configure), now);
        assert_eq!(answer.reply, Some(Reply::Configured));
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
            assert_eq!(enough.reply, Some(Reply::Configured), "{rate}");
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
        assert_eq!(answer.reply, Some(Reply::Configured));
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
        assert_eq!(answer.reply, Some(Reply::Configured));
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
        assert_eq!(answer.reply, Some(Reply::Configured));
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
            assert_eq!(answer.reply, Some(Reply::Configured));
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
        assert_eq!(answer.reply, Some(Reply::Configured));
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
