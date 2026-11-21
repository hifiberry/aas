//! The thread of one AirPlay receiver: it reads the master monitor through
//! its tap, connects when the gate opens (or always), streams while
//! connected, answers the receiver's timing and retransmit requests, and
//! disconnects after the gate closes. A failure is retried after 1, 2, 4, 8
//! and 16 s, then every 30 s.
//!
//! The thread runs at normal priority and wakes on its own timer: every
//! packet while it plays, every 20 ms otherwise. The audio thread never
//! waits for it; the tap drops what does not fit.

use super::config::{AirplayConfig, Connect};
use super::gate::{Gate, GateEvent};
use super::raop::{self, Raop, Setup};
use super::rtp;
use super::rtsp::Conn;
use super::stream::{Outgoing, Stream};
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
/// Frames read from the tap at a time.
const CHUNK: usize = 4096;

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

    fn end(&mut self) {
        let _ = self.tx.send(Msg::Stop);
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
pub fn spawn(config: AirplayConfig) -> AirplayHandle {
    let status = Arc::new(Mutex::new(Status {
        state: AirplayState::Idle,
        error: None,
    }));
    let (tx, rx) = mpsc::channel();
    let shared = status.clone();
    let thread = std::thread::Builder::new()
        .name("aas-airplay".into())
        .spawn(move || Output::new(config, shared).run(rx))
        .expect("the AirPlay thread starts");
    AirplayHandle {
        tx,
        status,
        thread: Some(thread),
    }
}

struct Session {
    raop: Raop,
    data: UdpSocket,
    control: UdpSocket,
    timing: UdpSocket,
    stream: Stream,
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
    auth_setup_told: bool,
    frames: Vec<[i32; 2]>,
    interleaved: Vec<i32>,
    sent: Vec<Outgoing>,
}

fn ntp_now() -> u64 {
    rtp::ntp_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default(),
    )
}

impl Output {
    fn new(config: AirplayConfig, status: Arc<Mutex<Status>>) -> Output {
        Output {
            config,
            status,
            base: Instant::now(),
            tap: None,
            gate: None,
            session: None,
            retry_at: None,
            failures: 0,
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
                self.connect(Instant::now());
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
    /// connected, and acts on what the gate says.
    fn read_tap(&mut self, now: Instant) {
        loop {
            let n = match &self.tap {
                Some(tap) => tap.ring.read(&mut self.frames),
                None => return,
            };
            if n == 0 {
                break;
            }
            self.interleaved.clear();
            self.interleaved.extend(self.frames[..n].iter().flatten());
            let event = self.gate.as_mut().and_then(|g| g.feed(&self.interleaved));
            if let Some(s) = &mut self.session {
                s.stream.take(&self.interleaved);
            }
            match (event, self.config.connect, self.state()) {
                (Some(GateEvent::Signal), Connect::Signal, AirplayState::Idle) => {
                    self.connect(now);
                }
                (Some(GateEvent::Silence), Connect::Signal, AirplayState::Playing) => {
                    self.disconnect();
                }
                _ => {}
            }
            if n < self.frames.len() {
                break;
            }
        }
    }

    /// Once the time has come, tries again while the gate is open; else
    /// the receiver is idle, which under `Connect::Always` connects again.
    fn retry(&mut self, now: Instant) {
        let Some(at) = self.retry_at else { return };
        if now < at {
            return;
        }
        self.retry_at = None;
        if self.gate.as_ref().is_some_and(Gate::signal) {
            self.connect(now);
        } else {
            self.set(AirplayState::Idle, None);
        }
    }

    fn connect(&mut self, now: Instant) {
        self.set(AirplayState::Connecting, None);
        match self.open(now) {
            Ok(session) => {
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

    fn disconnect(&mut self) {
        if let Some(s) = self.session.take() {
            s.raop.teardown();
        }
        if self.state() == AirplayState::Playing {
            self.set(AirplayState::Idle, None);
        }
    }

    fn open(&mut self, now: Instant) -> Result<Session, String> {
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
        let setup = Setup {
            control_port: port(&control),
            timing_port: port(&timing),
            first_seq: u16::from_be_bytes(raop::random()),
            first_rtptime: u32::from_be_bytes(raop::random()),
            volume_db: c.volume_db,
        };
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
        let udp = |e: std::io::Error| format!("UDP to {peer}: {e}");
        data.connect((peer, ports.server)).map_err(udp)?;
        control.connect((peer, ports.control)).map_err(udp)?;
        for s in [&data, &control, &timing] {
            s.set_nonblocking(true).map_err(udp)?;
        }
        if let Some(t) = ports.timing {
            let ntp = ntp_now();
            let _ = timing.send_to(&rtp::timing_packet(ntp, ntp, ntp), (peer, t));
        }
        let wanted = u64::from(c.latency_ms) * u64::from(rtp::RATE) / 1000;
        let latency = (wanted as u32).max(raop.audio_latency.unwrap_or(0));
        let stream = Stream::new(
            rate,
            latency,
            setup.first_seq,
            setup.first_rtptime,
            ntp_now(),
            self.mono(now),
        );
        Ok(Session {
            raop,
            data,
            control,
            timing,
            stream,
        })
    }

    /// Sends what is due and answers the receiver. A closed connection
    /// ends the session as a failure.
    fn pump(&mut self, now: Instant) {
        let mono = self.mono(now);
        let Some(s) = &mut self.session else { return };
        if s.raop.closed() {
            self.session = None;
            self.fail("the receiver closed the connection".into());
            return;
        }
        self.sent.clear();
        s.stream.due(mono, &mut self.sent);
        for out in &self.sent {
            // A lost datagram is the receiver's to ask for again.
            let _ = match out {
                Outgoing::Audio(p) => s.data.send(p),
                Outgoing::Sync(p) => s.control.send(p),
            };
        }
        let mut buf = [0u8; 64];
        while let Ok((n, from)) = s.timing.recv_from(&mut buf) {
            let received = ntp_now();
            if let Some(reply) = rtp::timing_reply(&buf[..n], received, ntp_now()) {
                let _ = s.timing.send_to(&reply, from);
            }
        }
        let mut again = Vec::new();
        while let Ok(n) = s.control.recv(&mut buf) {
            if let Some((first, count)) = rtp::retransmit_request(&buf[..n]) {
                s.stream.resend(first, count, &mut again);
            }
        }
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

    fn streaming() -> FakeReceiver {
        FakeReceiver::serving(1, accepting_on)
    }

    #[test]
    fn a_signal_connects_and_silence_tears_down() {
        let receiver = streaming();
        let handle = spawn(config(receiver.port, Connect::Signal));
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
    fn always_connects_without_a_signal() {
        let receiver = streaming();
        let handle = spawn(config(receiver.port, Connect::Always));
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
        let handle = spawn(config(receiver.port, Connect::Always));
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
        let handle = spawn(c);
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
        let handle = spawn(config(receiver.port, Connect::Always));
        let (_ring, reader) = tap();
        handle.tap(reader);
        assert!(until(&handle, 3.0, playing), "{:?}", handle.status());
        let started = Instant::now();
        handle.stop();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(receiver.finish().last().unwrap().method, "TEARDOWN");
    }

    #[test]
    fn a_timing_request_is_answered() {
        let receiver = streaming();
        let handle = spawn(config(receiver.port, Connect::Always));
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
