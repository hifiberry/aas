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
use crate::rate::{ClientId, RatePolicy, SETTLE};
use crate::resample::Resampler;
use aas_client::convert::ClientFormat;
use aas_client::proto::{self, Reply, Request};
use aas_client::ring::{self, Ring};
use rtrb::{Consumer, Producer};
use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::thread::Thread;
use std::time::{Duration, Instant};

/// Identifies a connection to the server.
pub type ConnId = u64;

/// Client rates the daemon accepts, as the ALSA plugin offers them.
const MIN_RATE: u32 = 8000;
const MAX_RATE: u32 = 384000;
/// Smallest period the latency rule gives.
const MIN_PERIOD: u32 = 32;
/// Half the longest resampling filter, which rubato adds to its input.
const FILTER_HALF: u32 = 64;
/// Headroom for the ratio change rubato prepares for.
const RATIO_HEADROOM: f64 = 1.1;
/// How long a drain may wait beyond what its ring holds, should the audio
/// thread never report it drained.
const DRAIN_SPARE: Duration = Duration::from_millis(500);

/// Frames a client's ring holds at least, so that one period's input fits
/// whatever rate the output runs at. One period takes at most what the
/// latency target lasts, or the smallest period at the lowest rate.
pub fn min_ring_frames(rate: u32, latency_ms: f32) -> u32 {
    let per_period = f64::from(latency_ms) / 1000.0 / f64::from(PERIODS);
    let shortest = f64::from(MIN_PERIOD) / f64::from(MIN_RATE);
    let input = f64::from(rate) * per_period.max(shortest) * RATIO_HEADROOM;
    input.ceil() as u32 + 2 + FILTER_HALF + dop::MIN_FRAMES as u32
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
    /// A drain waits for its reply, at the latest until then.
    drain: Option<Instant>,
}

struct Conn {
    name: String,
    hello: bool,
    stream: Option<Stream>,
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
    /// A rate change that waits, so that a stop and a start close together
    /// do not change the rate twice.
    switch_at: Option<Instant>,
    /// Since when no client has been running.
    idle_since: Option<Instant>,
    /// Clients the audio thread has seen play DoP, in the order seen.
    dop_claims: Vec<ClientId>,
    dop_owner: Option<ClientId>,
    /// Replies that were not ready when their request came.
    outbox: Vec<(ConnId, Reply)>,
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
            switch_at: None,
            idle_since: None,
            dop_claims: Vec::new(),
            dop_owner: None,
            outbox: Vec::new(),
        }
    }

    /// Opens the output at the default rate. Until a client plays it sends
    /// zeros, and the idle rule applies from now.
    pub fn start(&mut self, now: Instant) {
        let mut batch = Vec::new();
        self.open_output(&mut batch);
        self.idle_since = Some(now);
        self.send(batch);
    }

    /// Stops the audio thread. It returns once it has seen this.
    pub fn shutdown(&mut self) {
        self.send(vec![Command::Shutdown]);
    }

    pub fn connected(&mut self, conn: ConnId, name: String) {
        eprintln!("aas: {name} connected");
        let state = Conn {
            name,
            hello: false,
            stream: None,
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
                    Answer::reply(Reply::Ok)
                }
                Request::Hello { version } => {
                    eprintln!("aas: {} speaks protocol {version}", state.name);
                    Answer::fatal(libc::EPROTONOSUPPORT)
                }
                _ => Answer::fatal(libc::EPROTO),
            };
        }
        match request {
            Request::Hello { .. } => Answer::reply(Reply::Ok),
            Request::Configure {
                rate,
                format,
                channels,
                buffer_frames,
            } => self.configure(conn, rate, format, channels, buffer_frames, now),
            Request::Start => self.start_stream(conn, now),
            Request::Stop => self.stop_stream(conn, now),
            Request::Drain => self.drain(conn, now),
            // Volume comes with the volume work, not here.
            Request::GetVolume | Request::SetVolume { .. } | Request::SubscribeVolume => {
                Answer::error(libc::EOPNOTSUPP)
            }
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
        if !(MIN_RATE..=MAX_RATE).contains(&rate) || channels == 0 {
            return Answer::error(libc::EINVAL);
        }
        let mut batch = Vec::new();
        if let Some(old) = self.conns.get_mut(&conn).and_then(|c| c.stream.take()) {
            self.remove_stream(old, now, &mut batch);
        }
        let capacity = buffer_frames.max(min_ring_frames(rate, self.latency_ms));
        let made = Ring::create(capacity).and_then(|(ring, memfd)| {
            let wake = eventfd()?;
            let theirs = wake.try_clone()?;
            let memfd_theirs = memfd.try_clone()?;
            Ok((ring, memfd, memfd_theirs, wake, theirs))
        });
        let (ring, memfd, memfd_theirs, wake, theirs) = match made {
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
            drain: None,
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
        self.dop_claims.retain(|&c| c != id);
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
        let held = Duration::from_secs_f64(stream.capacity as f64 / f64::from(stream.rate));
        stream.drain = Some(now + held + DRAIN_SPARE);
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
            return;
        }
        self.stop(id, now, batch);
        self.outbox.push((conn, Reply::Ok));
    }

    fn remove_stream(&mut self, stream: Stream, now: Instant, batch: &mut Vec<Command>) {
        if stream.running {
            self.policy.client_stopped(stream.id, now - self.epoch);
        }
        batch.push(Command::Remove(stream.id));
        self.table_len -= 1;
        self.dop_claims.retain(|&c| c != stream.id);
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

    fn running_count(&self) -> usize {
        self.streams().filter(|s| s.running).count()
    }

    fn is_supported(&self, rate: u32) -> bool {
        self.supported.is_empty() || self.supported.contains(&rate)
    }

    /// Makes the rate rules follow the audio thread's DoP owner: the claim
    /// added first, at a rate the card takes. A new owner needs its rate at
    /// once, since its audio is discarded until then.
    fn update_dop(&mut self, now: Instant, batch: &mut Vec<Command>) {
        let owner = self
            .dop_claims
            .iter()
            .copied()
            .filter(|&id| {
                let stream = self.streams().find(|s| s.id == id);
                stream.is_some_and(|s| s.running && self.is_supported(s.rate))
            })
            .min();
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
        self.switch_at = None;
        self.sync_resamplers(batch);
        batch.push(Command::Reconfigure { rate, period });
    }

    /// Gives each running source at another rate than the output a
    /// resampler for it, and takes it away from the others.
    fn sync_resamplers(&mut self, batch: &mut Vec<Command>) {
        let output = self.output;
        let (quality, format) = (self.quality, self.format);
        let streams = self.conns.values_mut().filter_map(|c| c.stream.as_mut());
        for stream in streams {
            let want = match output {
                Some((rate, period)) if stream.running && stream.rate != rate => {
                    Some((rate, period))
                }
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
                Event::Dropped(_) | Event::Replaced(_) | Event::OldTable(_) => {}
                Event::Reconfigured => {}
                Event::DopStarted(id) => {
                    if !self.dop_claims.contains(&id) {
                        self.dop_claims.push(id);
                    }
                    self.update_dop(now, &mut batch);
                }
                Event::DopStopped(id) => {
                    self.dop_claims.retain(|&c| c != id);
                    self.update_dop(now, &mut batch);
                    self.decide(now, false, &mut batch);
                }
                Event::Drained(id) => self.drained(id, now, &mut batch),
            }
        }
        self.send(batch);
    }

    /// Applies what is due by the clock: a settled rate change, the end of
    /// a drain the audio thread never reported, and the idle timer.
    pub fn tick(&mut self, now: Instant) {
        let mut batch = Vec::new();
        let before = self.policy.current();
        self.policy.tick(now - self.epoch);
        let settled = self.policy.current() != before;
        let overdue: Vec<ClientId> = self
            .streams()
            .filter(|s| s.drain.is_some_and(|at| now >= at))
            .map(|s| s.id)
            .collect();
        for id in overdue {
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
    }

    /// Queues the commands in one commit and wakes the audio thread. Waits
    /// for room if needed, taking events out of the other queue meanwhile,
    /// because the audio thread stops taking commands while that is full.
    fn send(&mut self, batch: Vec<Command>) {
        let capacity = self.commands.buffer().capacity();
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
    use crate::config::{Format, Quality};

    #[test]
    fn a_ring_holds_one_period_of_input_at_any_output_rate() {
        let rates = [
            8000, 11025, 22050, 44100, 48000, 96000, 176400, 192000, 384000,
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
