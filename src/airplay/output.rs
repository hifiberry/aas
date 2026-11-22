//! The thread of one AirPlay receiver: it reads the master monitor through
//! its tap, connects when the gate opens (or always), streams while
//! connected, answers the receiver's timing and retransmit requests, and
//! disconnects after the gate closes. A failure is retried after 1, 2, 4, 8
//! and 16 s, then every 30 s.
//!
//! The thread runs at normal priority and wakes on its own timer: every
//! packet while it plays, every 20 ms otherwise. The audio thread never
//! waits for it; the tap drops what does not fit.

use super::ap2::session::Ap2;
use super::config::{AirplayConfig, Connect, Protocol};
use super::gate::{Gate, GateEvent};
use super::meta;
use super::raop::{self, Raop, Setup};
use super::rtp;
use super::rtsp::{Conn, RtspError};
use super::stream::{Outgoing, Stream};
use crate::metadata::NowPlaying;
use aas_client::ring::Ring;
use std::net::UdpSocket;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// For the connect, and for each answer of the handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the thread looks at the tap while it does not play.
const IDLE_WAKE: Duration = Duration::from_millis(20);
/// Seconds before each retry; after the last, every `RETRY_LAST`.
const RETRIES: [u32; 5] = [1, 2, 4, 8, 16];
const RETRY_LAST: u32 = 30;
/// How long the tap may give nothing before that counts as silence: well
/// beyond a period, so that it never cuts into a running master.
const NOTHING: Duration = Duration::from_millis(200);
/// Frames read from the tap at a time.
const CHUNK: usize = 4096;
/// How often an AirPlay 2 session is told we are still there.
const FEEDBACK: Duration = Duration::from_secs(2);

/// The reading end of the monitor tap made for this receiver, at the
/// master's rate.
pub struct TapReader {
    pub ring: Ring,
    pub rate: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AirplayState {
    Idle,
    Connecting,
    Playing,
    /// After a failure; the next try is `in_s` seconds after it.
    Retrying {
        in_s: u32,
    },
}

impl AirplayState {
    pub fn name(self) -> &'static str {
        match self {
            AirplayState::Idle => "idle",
            AirplayState::Connecting => "connecting",
            AirplayState::Playing => "playing",
            AirplayState::Retrying { .. } => "retrying",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub state: AirplayState,
    /// The last failure, kept until the next session plays.
    pub error: Option<String>,
}

enum Msg {
    Tap(TapReader),
    Stop,
}

/// The control thread's side of a receiver's thread.
pub struct AirplayHandle {
    tx: Sender<Msg>,
    status: Arc<Mutex<Status>>,
    thread: Option<JoinHandle<()>>,
}

impl AirplayHandle {
    /// Gives the thread a tap, the first or one at a new rate.
    pub fn tap(&self, reader: TapReader) {
        let _ = self.tx.send(Msg::Tap(reader));
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Ends a session with TEARDOWN and joins the thread. A handshake under
    /// way finishes first, within `HANDSHAKE_TIMEOUT` per step.
    pub fn stop(mut self) {
        self.end();
    }

    /// Asks the thread to stop, without waiting: several receivers then
    /// end their sessions at once.
    pub fn ask_stop(&self) {
        let _ = self.tx.send(Msg::Stop);
    }

    fn end(&mut self) {
        self.ask_stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for AirplayHandle {
    fn drop(&mut self) {
        self.end();
    }
}

/// Starts the thread of a receiver. It does nothing until it has a tap.
pub fn spawn(config: AirplayConfig, now_playing: Arc<NowPlaying>) -> AirplayHandle {
    let status = Arc::new(Mutex::new(Status {
        state: AirplayState::Idle,
        error: None,
    }));
    let (tx, rx) = mpsc::channel();
    let shared = status.clone();
    let thread = std::thread::Builder::new()
        .name("aas-airplay".into())
        .spawn(move || Output::new(config, shared, now_playing).run(rx))
        .expect("the AirPlay thread starts");
    AirplayHandle {
        tx,
        status,
        thread: Some(thread),
    }
}

/// The session's RTSP side, by protocol.
enum Control {
    One(Raop),
    Two(Ap2),
}

impl Control {
    fn closed(&mut self) -> bool {
        match self {
            Control::One(r) => r.closed(),
            Control::Two(a) => a.closed(),
        }
    }

    fn metadata(&mut self, m: &meta::Meta, rtptime: u32) -> Result<(), RtspError> {
        match self {
            Control::One(r) => r.metadata(m, rtptime),
            Control::Two(a) => a.metadata(m, rtptime),
        }
    }

    fn flush(&mut self) -> Result<(), RtspError> {
        match self {
            Control::One(r) => r.flush(),
            Control::Two(a) => a.flush(),
        }
    }

    fn teardown(self) {
        match self {
            Control::One(r) => r.teardown(),
            Control::Two(a) => a.teardown(),
        }
    }
}

struct Session {
    rtsp: Control,
    /// The metadata sent so far.
    meta: meta::Sent,
    /// When AirPlay 2's next `/feedback` is due.
    feedback_at: Option<Instant>,
    data: UdpSocket,
    control: UdpSocket,
    stream: Stream,
    /// When it started, and the packets the receiver asked for again.
    started: Instant,
    resent: u64,
    /// Answers the receiver's timing requests for as long as the session
    /// lasts.
    _timing: Timing,
}

/// A thread that answers timing requests on its own socket, from before
/// SETUP on: a receiver may ask the time before it answers SETUP, while
/// the output's thread waits for that answer.
struct Timing {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Timing {
    /// How long a wait for a request lasts before the thread looks
    /// whether it should stop.
    const POLL: Duration = Duration::from_millis(100);

    fn start(socket: UdpSocket, line: Line) -> Timing {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::Builder::new()
            .name("aas-airplay-ntp".into())
            .spawn(move || {
                let _ = socket.set_read_timeout(Some(Timing::POLL));
                let mut buf = [0u8; 64];
                while !stopped.load(Ordering::Relaxed) {
                    let Ok((n, from)) = socket.recv_from(&mut buf) else {
                        continue;
                    };
                    let received = line.ntp(Instant::now());
                    let reply = rtp::timing_reply(&buf[..n], received, line.ntp(Instant::now()));
                    if let Some(reply) = reply {
                        let _ = socket.send_to(&reply, from);
                    }
                }
            })
            .ok();
        Timing { stop, thread }
    }
}

impl Drop for Timing {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Output {
    config: AirplayConfig,
    status: Arc<Mutex<Status>>,
    base: Instant,
    tap: Option<TapReader>,
    gate: Option<Gate>,
    session: Option<Session>,
    retry_at: Option<Instant>,
    failures: usize,
    /// When the tap last gave frames, or the time without them was last
    /// counted.
    heard: Instant,
    auth_setup_told: bool,
    frames: Vec<[i32; 2]>,
    interleaved: Vec<i32>,
    sent: Vec<Outgoing>,
    now_playing: Arc<NowPlaying>,
}

/// A line from the monotonic clock to NTP time, taken from the wall clock
/// once.
#[derive(Clone, Copy)]
struct Line {
    at: Instant,
    ntp: u64,
}

impl Line {
    fn now() -> Line {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Line {
            at: Instant::now(),
            ntp: rtp::ntp_from(wall),
        }
    }

    fn ntp(self, at: Instant) -> u64 {
        let d = at.saturating_duration_since(self.at);
        let fraction = (u64::from(d.subsec_nanos()) << 32) / 1_000_000_000;
        self.ntp
            .wrapping_add(d.as_secs() << 32)
            .wrapping_add(fraction)
    }
}

impl Output {
    fn new(
        config: AirplayConfig,
        status: Arc<Mutex<Status>>,
        now_playing: Arc<NowPlaying>,
    ) -> Output {
        Output {
            now_playing,
            config,
            status,
            base: Instant::now(),
            tap: None,
            gate: None,
            session: None,
            retry_at: None,
            failures: 0,
            heard: Instant::now(),
            auth_setup_told: false,
            frames: vec![[0; 2]; CHUNK],
            interleaved: Vec::with_capacity(CHUNK * 2),
            sent: Vec::new(),
        }
    }

    fn state(&self) -> AirplayState {
        self.status.lock().unwrap_or_else(|p| p.into_inner()).state
    }

    fn set(&self, state: AirplayState, error: Option<String>) {
        let mut s = self.status.lock().unwrap_or_else(|p| p.into_inner());
        s.state = state;
        if state == AirplayState::Playing || error.is_some() {
            s.error = error;
        }
    }

    fn mono(&self, at: Instant) -> Duration {
        at.saturating_duration_since(self.base)
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            let now = Instant::now();
            self.read_tap(now);
            self.retry(now);
            if self.config.connect == Connect::Always
                && self.state() == AirplayState::Idle
                && self.tap.is_some()
            {
                self.connect();
            }
            self.pump(Instant::now());
            match rx.recv_timeout(self.wait(Instant::now())) {
                Ok(Msg::Tap(tap)) => self.new_tap(tap),
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => {
                    self.disconnect();
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Until the next packet, the next retry, or `IDLE_WAKE`.
    fn wait(&self, now: Instant) -> Duration {
        let mut wait = IDLE_WAKE;
        if let Some(s) = &self.session {
            wait = wait.min(s.stream.next_due().saturating_sub(self.mono(now)));
        }
        if let Some(at) = self.retry_at {
            wait = wait.min(at.saturating_duration_since(now));
        }
        wait
    }

    fn new_tap(&mut self, tap: TapReader) {
        // The thread reads on its own timer and is never woken.
        tap.ring.header().wake_at.store(u64::MAX, Ordering::SeqCst);
        match &mut self.gate {
            Some(gate) => gate.set_rate(tap.rate),
            None => {
                let c = &self.config;
                self.gate = Some(Gate::new(tap.rate, c.threshold_dbfs, c.idle_s));
            }
        }
        if let Some(s) = &mut self.session {
            s.stream.set_rate(tap.rate);
        }
        self.tap = Some(tap);
    }

    /// Reads what the tap holds into the gate, and into the stream while
    /// connected, and acts on what the gate says. Time in which the tap
    /// gives nothing, as when the master has closed, counts as silence.
    fn read_tap(&mut self, now: Instant) {
        let mut any = false;
        loop {
            let n = match &self.tap {
                Some(tap) => tap.ring.read(&mut self.frames),
                None => return,
            };
            if n == 0 {
                break;
            }
            any = true;
            self.interleaved.clear();
            self.interleaved.extend(self.frames[..n].iter().flatten());
            let event = self.gate.as_mut().and_then(|g| g.feed(&self.interleaved));
            if let Some(s) = &mut self.session {
                s.stream.take(&self.interleaved);
            }
            self.on_gate(event);
            if n < self.frames.len() {
                break;
            }
        }
        if any {
            self.heard = now;
        } else if now.duration_since(self.heard) >= NOTHING {
            let elapsed = now.duration_since(self.heard);
            self.heard = now;
            let event = self.gate.as_mut().and_then(|g| g.nothing(elapsed));
            self.on_gate(event);
        }
    }

    fn on_gate(&mut self, event: Option<GateEvent>) {
        match (event, self.config.connect, self.state()) {
            (Some(GateEvent::Signal), Connect::Signal, AirplayState::Idle) => self.connect(),
            (Some(GateEvent::Silence), Connect::Signal, AirplayState::Playing) => {
                self.disconnect();
            }
            _ => {}
        }
    }

    /// Once the time has come, tries again: always, or while the gate is
    /// open. Otherwise the episode is over and the receiver idle.
    fn retry(&mut self, now: Instant) {
        let Some(at) = self.retry_at else { return };
        if now < at {
            return;
        }
        self.retry_at = None;
        let signal = self.gate.as_ref().is_some_and(Gate::signal);
        if self.config.connect == Connect::Always || signal {
            self.connect();
        } else {
            // The episode is over: the next signal tries from 1 s again.
            self.failures = 0;
            self.set(AirplayState::Idle, None);
        }
    }

    fn connect(&mut self) {
        self.set(AirplayState::Connecting, None);
        match self.open() {
            Ok(session) => {
                eprintln!("aas: airplay \"{}\": playing", self.config.name);
                self.session = Some(session);
                self.failures = 0;
                self.set(AirplayState::Playing, None);
            }
            Err(why) => self.fail(why),
        }
    }

    fn fail(&mut self, why: String) {
        eprintln!("aas: airplay \"{}\": {why}", self.config.name);
        let delay = RETRIES.get(self.failures).copied().unwrap_or(RETRY_LAST);
        self.failures += 1;
        self.retry_at = Some(Instant::now() + Duration::from_secs(u64::from(delay)));
        self.set(AirplayState::Retrying { in_s: delay }, Some(why));
    }

    /// What a session did, for the log when it ends.
    fn summary(&self, s: &Session) -> String {
        format!(
            "{} s, {} packets, {} resent, {} short of frames",
            s.started.elapsed().as_secs(),
            s.stream.packets(),
            s.resent,
            s.stream.padded()
        )
    }

    fn disconnect(&mut self) {
        if let Some(s) = self.session.take() {
            eprintln!(
                "aas: airplay \"{}\": session ended: {}",
                self.config.name,
                self.summary(&s)
            );
            s.rtsp.teardown();
        }
        if self.state() == AirplayState::Playing {
            self.set(AirplayState::Idle, None);
        }
    }

    fn open(&mut self) -> Result<Session, String> {
        let rate = self.tap.as_ref().map(|t| t.rate).ok_or("no audio yet")?;
        let c = &self.config;
        let conn = Conn::connect(&c.host, c.port, HANDSHAKE_TIMEOUT).map_err(|e| e.to_string())?;
        let (local, peer) = (conn.local_ip(), conn.peer_ip());
        let any = if local.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let bind = || UdpSocket::bind(any).map_err(|e| format!("a UDP socket: {e}"));
        let (control, timing, data) = (bind()?, bind()?, bind()?);
        let port = |s: &UdpSocket| s.local_addr().map(|a| a.port()).unwrap_or(0);
        let first_timing = timing
            .try_clone()
            .map_err(|e| format!("a UDP socket: {e}"))?;
        // One clock line for the session: the timing answers and the
        // stream's sync packets both read it, and a step of the wall
        // clock does not move it.
        let line = Line::now();
        let responder = Timing::start(timing, line);
        let wanted = u64::from(c.latency_ms) * u64::from(rtp::RATE) / 1000;
        let setup = Setup {
            control_port: port(&control),
            timing_port: port(&first_timing),
            first_seq: u16::from_be_bytes(raop::random()),
            first_rtptime: u32::from_be_bytes(raop::random()),
            volume_db: c.volume_db,
            min_latency: raop::offered_minimum(wanted as u32),
        };
        let (rtsp, ports, audio_latency, shk) = match c.protocol {
            Protocol::AirPlay1 => {
                let (raop, ports) = Raop::open(conn, &setup).map_err(|e| e.to_string())?;
                if let Some(status) = raop.auth_setup_refused
                    && !self.auth_setup_told
                {
                    eprintln!(
                        "aas: airplay \"{}\": /auth-setup answered {status}, playing without it",
                        c.name
                    );
                    self.auth_setup_told = true;
                }
                let latency = raop.audio_latency;
                (Control::One(raop), ports, latency, None)
            }
            Protocol::AirPlay2 => {
                let (ap2, ports) = Ap2::open(conn, &setup).map_err(|e| e.to_string())?;
                let (latency, shk) = (ap2.audio_latency, ap2.shk);
                (Control::Two(ap2), ports, latency, Some(shk))
            }
        };
        let udp = |e: std::io::Error| format!("UDP to {peer}: {e}");
        data.connect((peer, ports.server)).map_err(udp)?;
        control.connect((peer, ports.control)).map_err(udp)?;
        for s in [&data, &control] {
            s.set_nonblocking(true).map_err(udp)?;
        }
        if let Some(t) = ports.timing {
            let ntp = line.ntp(Instant::now());
            let _ = first_timing.send_to(&rtp::timing_packet(ntp, ntp, ntp), (peer, t));
        }
        // The stream starts now, after the handshake, so that its time is
        // not made up for with a burst.
        let begun = Instant::now();
        let latency = (wanted as u32).max(audio_latency.unwrap_or(0));
        eprintln!(
            "aas: airplay \"{}\": {} ms late, the receiver asks for {}",
            c.name,
            u64::from(latency) * 1000 / u64::from(rtp::RATE),
            audio_latency.map_or("nothing".into(), |f| format!("{f} frames")),
        );
        let mut stream = Stream::new(
            rate,
            latency,
            setup.first_seq,
            setup.first_rtptime,
            line.ntp(begun),
            self.mono(begun),
        );
        if let Some(shk) = &shk {
            stream = stream.sealed(shk);
        }
        let feedback_at = shk.map(|_| begun + FEEDBACK);
        Ok(Session {
            rtsp,
            feedback_at,
            data,
            control,
            stream,
            started: begun,
            meta: meta::Sent::default(),
            resent: 0,
            _timing: responder,
        })
    }

    /// Sends what is due and answers the receiver. A closed connection
    /// ends the session as a failure.
    fn pump(&mut self, now: Instant) {
        let mono = self.mono(now);
        let Some(s) = &mut self.session else { return };
        let closed = if s.rtsp.closed() {
            Some("the connection")
        } else if let Control::Two(ap2) = &mut s.rtsp
            && !ap2.poll_events()
        {
            Some("the event channel")
        } else {
            None
        };
        if let Some(what) = closed {
            let summary = self
                .session
                .take()
                .map(|s| self.summary(&s))
                .unwrap_or_default();
            self.fail(format!("the receiver closed {what}, after {summary}"));
            return;
        }
        if let (Some(at), Control::Two(ap2)) = (s.feedback_at, &mut s.rtsp)
            && now >= at
        {
            // A failure shows as a closed connection at the next look.
            let _ = ap2.feedback();
            s.feedback_at = Some(at + FEEDBACK);
        }
        // Metadata goes out queued, as the socket takes it, so that a cover
        // never holds the packets up; a failure shows as a closed
        // connection at the next look.
        let (text, cover, progress) = self.now_playing.generations();
        let sent = s.meta;
        if (text, cover, progress) != (sent.text_gen, sent.cover_gen, sent.progress_gen) {
            let snap = self.now_playing.snapshot(now);
            for m in meta::due(&mut s.meta, &snap, s.stream.rtptime()) {
                let _ = s.rtsp.metadata(&m, s.stream.rtptime());
            }
        }
        let _ = s.rtsp.flush();
        self.sent.clear();
        s.stream.due(mono, &mut self.sent);
        let mut refused = None;
        for out in &self.sent {
            // A lost datagram is the receiver's to ask for again; a refused
            // one means its port is gone, as after a reboot.
            let sent = match out {
                Outgoing::Audio(p) => s.data.send(p),
                Outgoing::Sync(p) => s.control.send(p),
            };
            if let Err(e) = sent
                && e.kind() == std::io::ErrorKind::ConnectionRefused
            {
                refused = Some(e);
            }
        }
        if let Some(e) = refused {
            let summary = self
                .session
                .take()
                .map(|s| self.summary(&s))
                .unwrap_or_default();
            self.fail(format!(
                "the receiver refused the audio ({e}), after {summary}"
            ));
            return;
        }
        let Some(s) = &mut self.session else { return };
        let mut buf = [0u8; 64];
        let mut again = Vec::new();
        while let Ok(n) = s.control.recv(&mut buf) {
            if let Some((first, count)) = rtp::retransmit_request(&buf[..n]) {
                s.stream.resend(first, count, &mut again);
            }
        }
        s.resent += again.len() as u64;
        for p in &again {
            let _ = s.data.send(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::Protocol;
    use super::super::fake::{Answer, FakeReceiver, Seen, accepting_on};
    use super::*;
    use crate::metadata::{Cover, Track};
    use std::os::fd::AsFd;
    use std::sync::atomic::AtomicUsize;

    const RATE: u32 = 48000;

    fn config(port: u16, connect: Connect) -> AirplayConfig {
        AirplayConfig {
            name: "test".into(),
            host: "127.0.0.1".into(),
            port,
            protocol: Protocol::AirPlay1,
            connect,
            threshold_dbfs: -50.0,
            idle_s: 1,
            latency_ms: 1500,
            volume_db: Some(-25.0),
        }
    }

    /// A tap ring: the engine's end, to write, and the thread's.
    fn tap() -> (Ring, TapReader) {
        let (ring, fd) = Ring::create(RATE).unwrap();
        let reader = Ring::map(fd.as_fd()).unwrap();
        (
            ring,
            TapReader {
                ring: reader,
                rate: RATE,
            },
        )
    }

    /// Writes `seconds` of a -20 dBFS tone, or of zeros, in real time.
    fn play(ring: &Ring, seconds: f64, loud: bool) {
        let chunk = 480;
        let amplitude = if loud { 0.1 * f64::from(i32::MAX) } else { 0.0 };
        for i in 0..(seconds * f64::from(RATE) / chunk as f64) as usize {
            let frames: Vec<[i32; 2]> = (0..chunk)
                .map(|n| {
                    let t = (i * chunk + n) as f64 / f64::from(RATE);
                    let v = (amplitude * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as i32;
                    [v, v]
                })
                .collect();
            ring.write(&frames);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits up to `seconds` for `state`.
    fn until(handle: &AirplayHandle, seconds: f64, state: fn(AirplayState) -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs_f64(seconds);
        while Instant::now() < end {
            if state(handle.status().state) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn playing(s: AirplayState) -> bool {
        s == AirplayState::Playing
    }

    fn spawn_quiet(c: AirplayConfig) -> AirplayHandle {
        spawn(c, NowPlaying::new())
    }

    fn song(title: &str) -> Track {
        Track {
            title: Some(title.into()),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            duration_s: Some(200.0),
            playing: true,
        }
    }

    /// The bodies of the metadata requests of `kind` the receiver saw.
    fn bodies(seen: &[Seen], kind: &str) -> Vec<Vec<u8>> {
        seen.iter()
            .filter(|s| s.method == "SET_PARAMETER" && s.header("Content-Type") == Some(kind))
            .map(|s| s.body.clone())
            .collect()
    }

    /// Plays to `receiver` for `ms` with `np` as the current track.
    fn play_with(receiver: &FakeReceiver, np: Arc<NowPlaying>, ms: u64, during: impl FnOnce()) {
        let handle = spawn(config(receiver.port, Connect::Always), np);
        let (_ring, reader) = tap();
        handle.tap(reader);
        assert!(until(&handle, 3.0, playing), "{:?}", handle.status());
        std::thread::sleep(Duration::from_millis(ms / 2));
        during();
        std::thread::sleep(Duration::from_millis(ms / 2));
        assert_eq!(handle.status().state, AirplayState::Playing);
        handle.stop();
    }

    #[test]
    fn a_session_sends_the_current_track_after_record() {
        let receiver = streaming();
        let np = NowPlaying::new();
        np.set_track(song("One"));
        np.set_cover(Some(Cover {
            mime: "image/jpeg",
            bytes: vec![0xFF, 0xD8, 0xFF, 1],
        }));
        np.set_position(10.0, Instant::now());
        play_with(&receiver, np, 400, || {});
        let seen = receiver.finish();
        let record = seen.iter().position(|s| s.method == "RECORD").unwrap();
        let text = seen
            .iter()
            .position(|s| s.header("Content-Type") == Some("application/x-dmap-tagged"))
            .expect("no text");
        assert!(text > record);
        assert!(
            seen[text]
                .header("RTP-Info")
                .is_some_and(|r| r.starts_with("rtptime="))
        );
        assert_eq!(
            bodies(&seen, "application/x-dmap-tagged"),
            [meta::dmap(&song("One"))]
        );
        assert_eq!(bodies(&seen, "image/jpeg"), [vec![0xFF, 0xD8, 0xFF, 1]]);
        let progress = bodies(&seen, "text/parameters");
        assert!(
            progress
                .iter()
                .any(|b| b.starts_with(b"progress: ") && b != b"progress: 0/0/0\r\n"),
            "{progress:?}"
        );
    }

    #[test]
    fn a_new_track_during_the_session_is_sent() {
        let receiver = streaming();
        let np = NowPlaying::new();
        np.set_track(song("One"));
        let during = np.clone();
        play_with(&receiver, np, 600, move || during.set_track(song("Two")));
        let seen = receiver.finish();
        assert_eq!(
            bodies(&seen, "application/x-dmap-tagged"),
            [meta::dmap(&song("One")), meta::dmap(&song("Two"))]
        );
    }

    #[test]
    fn a_receiver_that_refuses_metadata_keeps_playing() {
        let receiver = FakeReceiver::serving(1, |port| {
            let accept = accepting_on(port);
            move |seen: &Seen| {
                if seen.header("Content-Type") == Some("application/x-dmap-tagged") {
                    Answer::Status(400, Vec::new())
                } else {
                    accept(seen)
                }
            }
        });
        let np = NowPlaying::new();
        np.set_track(song("One"));
        play_with(&receiver, np, 2000, || {});
        let mut packet = [0u8; 2048];
        assert!(receiver.data.recv(&mut packet).is_ok());
        let seen = receiver.finish();
        assert_eq!(bodies(&seen, "application/x-dmap-tagged").len(), 1);
    }

    #[test]
    fn a_skip_storm_sends_only_the_newest() {
        let receiver = streaming();
        let np = NowPlaying::new();
        for i in 0..50 {
            np.set_track(song(&format!("T{i}")));
        }
        play_with(&receiver, np, 400, || {});
        let seen = receiver.finish();
        assert_eq!(
            bodies(&seen, "application/x-dmap-tagged"),
            [meta::dmap(&song("T49"))]
        );
    }

    #[test]
    fn no_track_sends_no_metadata() {
        let receiver = streaming();
        play_with(&receiver, NowPlaying::new(), 400, || {});
        let seen = receiver.finish();
        assert!(bodies(&seen, "application/x-dmap-tagged").is_empty());
        assert!(bodies(&seen, "image/none").is_empty());
    }

    fn streaming() -> FakeReceiver {
        FakeReceiver::serving(1, accepting_on)
    }

    #[test]
    fn the_receiver_is_offered_the_latency_up_to_250_ms() {
        for (latency_ms, offered) in [(100, 4410), (1500, 11025)] {
            let receiver = streaming();
            let handle = spawn_quiet(AirplayConfig {
                latency_ms,
                ..config(receiver.port, Connect::Always)
            });
            let (ring, reader) = tap();
            handle.tap(reader);
            play(&ring, 0.2, false);
            assert!(until(&handle, 3.0, playing), "{:?}", handle.status());
            handle.stop();
            let seen = receiver.finish();
            let announce = seen.iter().find(|s| s.method == "ANNOUNCE").unwrap();
            let sdp = String::from_utf8_lossy(&announce.body).to_string();
            assert!(
                sdp.ends_with(&format!("a=min-latency:{offered}\r\n")),
                "{latency_ms} ms: {sdp}"
            );
        }
    }

    #[test]
    fn a_signal_connects_and_silence_tears_down() {
        let receiver = streaming();
        let handle = spawn_quiet(config(receiver.port, Connect::Signal));
        let (ring, reader) = tap();
        handle.tap(reader);
        play(&ring, 0.3, false);
        assert_eq!(handle.status().state, AirplayState::Idle);
        play(&ring, 0.3, true);
        assert!(until(&handle, 3.0, playing), "{:?}", handle.status());
        // Audio arrives: an RTP packet of 352 frames.
        let mut packet = [0u8; 2048];
        let n = receiver.data.recv(&mut packet).unwrap();
        assert_eq!(n, 12 + 1416);
        assert_eq!(packet[0], 0x80);
        play(&ring, 1.5, false);
        assert!(
            until(&handle, 2.0, |s| s == AirplayState::Idle),
            "{:?}",
            handle.status()
        );
        handle.stop();
        let seen = receiver.finish();
        assert_eq!(seen.last().unwrap().method, "TEARDOWN");
        assert!(seen.iter().any(|s| s.body == b"volume: -25\r\n"));
    }

    #[test]
    fn airplay2_pairs_seals_the_audio_and_keeps_the_session_alive() {
        let receiver = FakeReceiver::serving(1, |port| {
            super::super::ap2::session::tests::ap2_receiver(port, 0x4)
        });
        let mut c = config(receiver.port, Connect::Always);
        c.protocol = Protocol::AirPlay2;
        let handle = spawn_quiet(c);
        let (_ring, reader) = tap();
        handle.tap(reader);
        assert!(until(&handle, 3.0, playing), "{:?}", handle.status());
        // A packet of 352 frames, its payload sealed: the tag and the
        // count after it.
        let mut packet = [0u8; 2048];
        let n = receiver.data.recv(&mut packet).unwrap();
        assert_eq!(n, 12 + 1416 + 16 + 8);
        std::thread::sleep(Duration::from_millis(2300));
        handle.stop();
        let seen = receiver.finish();
        let feedback = seen.iter().filter(|s| s.url == "/feedback").count();
        assert_eq!(
            feedback,
            1,
            "{:?}",
            seen.iter().map(|s| &s.url).collect::<Vec<_>>()
        );
        assert_eq!(seen.last().unwrap().method, "TEARDOWN");
    }

    #[test]
    fn an_airplay2_receiver_that_closes_its_event_channel_is_retried() {
        use super::super::ap2::session::tests::{EventPort, ap2_receiver_with};
        let receiver =
            FakeReceiver::serving(1, |port| ap2_receiver_with(port, 0x4, EventPort::Closing).0);
        let mut c = config(receiver.port, Connect::Always);
        c.protocol = Protocol::AirPlay2;
        let handle = spawn_quiet(c);
        let (_ring, reader) = tap();
        handle.tap(reader);
        assert!(until(&handle, 3.0, playing), "{:?}", handle.status());
        assert!(
            until(&handle, 3.0, |s| matches!(s, AirplayState::Retrying { .. })),
            "{:?}",
            handle.status()
        );
        let error = handle.status().error.unwrap();
        assert!(error.contains("event channel"), "{error}");
    }

    #[test]
    fn a_tap_that_stops_delivering_counts_as_silence() {
        // The master closes when idle: the tap then gets nothing at all.
        let receiver = streaming();
        let handle = spawn_quiet(config(receiver.port, Connect::Signal));
        let (ring, reader) = tap();
        handle.tap(reader);
        play(&ring, 0.3, true);
        assert!(until(&handle, 3.0, playing), "{:?}", handle.status());
        assert!(
            until(&handle, 2.5, |s| s == AirplayState::Idle),
            "{:?}",
            handle.status()
        );
    }

    #[test]
    fn a_receiver_whose_audio_port_is_gone_is_retried() {
        // Its port answers ICMP unreachable: a rebooted receiver.
        let gone = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = gone.local_addr().unwrap().port();
        drop(gone);
        let receiver = FakeReceiver::serving(1, move |_| accepting_on(port));
        let handle = spawn_quiet(config(receiver.port, Connect::Always));
        let (_ring, reader) = tap();
        handle.tap(reader);
        assert!(
            until(&handle, 3.0, |s| matches!(s, AirplayState::Retrying { .. })),
            "{:?}",
            handle.status()
        );
        let error = handle.status().error.unwrap();
        assert!(error.contains("refused"), "{error}");
    }

    #[test]
    fn a_slow_handshake_is_not_made_up_for_with_a_burst() {
        let receiver = FakeReceiver::serving(1, |port| {
            let accept = accepting_on(port);
            move |s: &Seen| {
                if s.method == "SETUP" {
                    std::thread::sleep(Duration::from_millis(600));
                }
                accept(s)
            }
        });
        let handle = spawn_quiet(config(receiver.port, Connect::Always));
        let (_ring, reader) = tap();
        handle.tap(reader);
        let mut packet = [0u8; 2048];
        receiver.data.recv(&mut packet).unwrap();
        let first = Instant::now();
        let mut count = 0;
        receiver
            .data
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        while first.elapsed() < Duration::from_millis(100) {
            if receiver.data.recv(&mut packet).is_ok() {
                count += 1;
            }
        }
        // 100 ms is 12.5 packets; 0.6 s made up at once would be 75.
        assert!(count < 30, "{count} packets in 100 ms");
    }

    #[test]
    fn a_new_signal_after_a_failed_one_retries_from_1_s() {
        // The discard port: every try is refused.
        let handle = spawn_quiet(config(9, Connect::Signal));
        let (ring, reader) = tap();
        handle.tap(reader);
        play(&ring, 0.1, true);
        assert!(until(&handle, 2.0, |s| s == AirplayState::Retrying { in_s: 1 }));
        play(&ring, 1.2, true);
        assert!(until(&handle, 2.0, |s| s == AirplayState::Retrying { in_s: 2 }));
        // The signal ends; at the next try the receiver goes idle.
        assert!(
            until(&handle, 4.0, |s| s == AirplayState::Idle),
            "{:?}",
            handle.status()
        );
        play(&ring, 0.1, true);
        assert!(
            until(&handle, 2.0, |s| matches!(s, AirplayState::Retrying { .. })),
            "{:?}",
            handle.status()
        );
        assert_eq!(handle.status().state, AirplayState::Retrying { in_s: 1 });
    }

    #[test]
    fn always_connects_without_a_signal() {
        let receiver = streaming();
        let handle = spawn_quiet(config(receiver.port, Connect::Always));
        let (ring, reader) = tap();
        handle.tap(reader);
        play(&ring, 0.2, false);
        assert!(until(&handle, 3.0, playing), "{:?}", handle.status());
        // Silence does not disconnect it.
        play(&ring, 1.3, false);
        assert_eq!(handle.status().state, AirplayState::Playing);
    }

    #[test]
    fn a_receiver_that_closes_is_retried_and_plays_again() {
        let closes = Arc::new(AtomicUsize::new(0));
        let count = closes.clone();
        let receiver = FakeReceiver::serving(3, move |port| {
            let accept = accepting_on(port);
            move |s: &Seen| {
                // The first two sessions end at their last SET_PARAMETER.
                if s.body.starts_with(b"progress") && count.fetch_add(1, Ordering::SeqCst) < 2 {
                    Answer::Close
                } else {
                    accept(s)
                }
            }
        });
        let handle = spawn_quiet(config(receiver.port, Connect::Always));
        let (ring, reader) = tap();
        handle.tap(reader);
        assert!(
            until(&handle, 3.0, |s| matches!(
                s,
                AirplayState::Retrying { in_s: 1 }
            )),
            "{:?}",
            handle.status()
        );
        let error = handle.status().error.unwrap();
        assert!(error.contains("closed"), "{error}");
        // The second session played before it closed: the backoff starts
        // over at 1 s.
        assert!(until(&handle, 3.0, |s| s == AirplayState::Connecting
            || s == AirplayState::Playing));
        assert!(
            until(&handle, 3.0, |s| matches!(
                s,
                AirplayState::Retrying { in_s: 1 }
            )),
            "{:?}",
            handle.status()
        );
        play(&ring, 0.1, false);
        assert!(until(&handle, 3.0, playing), "{:?}", handle.status());
        assert_eq!(receiver.accepted.load(Ordering::SeqCst), 3);
        assert_eq!(handle.status().error, None);
    }

    #[test]
    fn an_unresolvable_host_retries_and_the_thread_goes_on() {
        let mut c = config(7000, Connect::Always);
        c.host = "nonexistent.invalid".into();
        let handle = spawn_quiet(c);
        let (_ring, reader) = tap();
        handle.tap(reader);
        assert!(until(&handle, 6.0, |s| matches!(
            s,
            AirplayState::Retrying { in_s: 1 }
        )));
        assert!(handle.status().error.is_some());
        assert!(until(&handle, 6.0, |s| matches!(
            s,
            AirplayState::Retrying { in_s: 2 }
        )));
    }

    #[test]
    fn stop_tears_down_and_joins_within_a_second() {
        let receiver = streaming();
        let handle = spawn_quiet(config(receiver.port, Connect::Always));
        let (_ring, reader) = tap();
        handle.tap(reader);
        assert!(until(&handle, 3.0, playing), "{:?}", handle.status());
        let started = Instant::now();
        handle.stop();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(receiver.finish().last().unwrap().method, "TEARDOWN");
    }

    #[test]
    fn a_receiver_that_asks_the_time_during_setup_is_answered_then() {
        // As a sound bar does: SETUP is answered only once a timing
        // request to the sender's timing port has had its reply.
        let receiver = FakeReceiver::serving(1, |port| {
            let accept = accepting_on(port);
            move |s: &Seen| {
                if s.method != "SETUP" {
                    return accept(s);
                }
                let ours = s.header("Transport").unwrap_or_default();
                let timing: u16 = ours
                    .split(';')
                    .find_map(|p| p.strip_prefix("timing_port="))
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(0);
                let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = [0u8; 32];
                request[..4].copy_from_slice(&[0x80, 0xd2, 0x00, 0x07]);
                let _ = socket.send_to(&request, ("127.0.0.1", timing));
                let mut reply = [0u8; 64];
                match socket.recv(&mut reply) {
                    Ok(32) => accept(s),
                    _ => Answer::Status(453, Vec::new()),
                }
            }
        });
        let handle = spawn_quiet(config(receiver.port, Connect::Always));
        let (_ring, reader) = tap();
        handle.tap(reader);
        assert!(until(&handle, 3.0, playing), "{:?}", handle.status());
    }

    #[test]
    fn a_timing_request_is_answered() {
        let receiver = streaming();
        let handle = spawn_quiet(config(receiver.port, Connect::Always));
        let (_ring, reader) = tap();
        handle.tap(reader);
        assert!(until(&handle, 3.0, playing), "{:?}", handle.status());
        let seen = receiver.seen();
        let transport = seen.iter().find(|s| s.method == "SETUP").unwrap();
        let ours = transport.header("Transport").unwrap();
        let timing: u16 = ours
            .split(';')
            .find_map(|p| p.strip_prefix("timing_port="))
            .unwrap()
            .parse()
            .unwrap();
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0u8; 32];
        request[..4].copy_from_slice(&[0x80, 0xd2, 0x00, 0x07]);
        request[24..].copy_from_slice(&42u64.to_be_bytes());
        socket.send_to(&request, ("127.0.0.1", timing)).unwrap();
        let mut reply = [0u8; 64];
        let n = socket.recv(&mut reply).unwrap();
        assert_eq!(n, 32);
        assert_eq!(reply[..2], [0x80, 0xd3]);
        assert_eq!(reply[8..16], 42u64.to_be_bytes());
    }
}
