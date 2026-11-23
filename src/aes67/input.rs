//! The `aas-aes67-in` thread: one AES67 stream into the mix.
//!
//! It receives the selected stream's RTP, holds it in a [`Jitter`] buffer by
//! media timestamp, and takes it out through an [`Adaptive`] resampler
//! steered by a [`Drift`] controller, as a further output is. What comes out
//! is S32 stereo at 48 kHz, written to an `aas_client::Ring` the engine
//! reads like a player's, through its fixed resampler to the master's rate.
//!
//! The controller holds a delay at `latency_ms`, measured in one of two
//! ways, chosen when the stream starts:
//!
//! - **PTP mode**, when the stream's `ts-refclk` names the grandmaster and
//!   domain the clock layer follows: the delay from a frame's media-clock
//!   time to the local time it reaches the master. The media-clock time
//!   comes from the PTP time and the SDP `mediaclk` offset; where it reaches
//!   the master from the ring's fill, the resampler's filter and the
//!   engine's output delay.
//! - **Buffer mode**, otherwise: the jitter buffer's depth plus the ring's
//!   fill.
//!
//! Each start primes the path onto its target by writing zeros ahead of the
//! first frame, so the first frame plays at its target delay. The input
//! starts again, silent until then, on a new PTP generation (a grandmaster
//! change, loss of PTP time, a local clock step), a new selection or
//! latency, packets returning after a second without, and a delay error
//! above 50 ms or one the controller does not take for a second.
//!
//! [`Receiver`] is the state machine, pure and tested without sockets; the
//! thread around it reads the socket, the clock and the commands. Nothing
//! allocates per packet once a stream is selected.

use std::io;
use std::mem;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use aas_client::ring::Ring;

use super::clock::{PtpClock, PtpNow, realtime_ns};
use super::jitter::Jitter;
use super::listen::set_option;
use super::rtp::{self, ts_diff};
use super::sdp::Session;
use crate::adaptive::Adaptive;
use crate::config::{Format, Quality};
use crate::drift::Drift;
use crate::rt;

/// The rate on the wire and in the ring.
pub const RATE: u32 = 48_000;
/// Frames per write to the ring, and per update of the controller: 1 ms.
const FRAMES: usize = 48;
/// The most output frames the resampler is set up for.
const MAX_OUT: u32 = 480;
/// The most frames a packet carries: 4 ms.
const MAX_PACKET_FRAMES: usize = 192;
/// A delay error above this, in frames, restarts the input: 50 ms.
const RESTART_FRAMES: f64 = 2400.0;
/// Writes in a row whose reading the controller does not take before the
/// input restarts: a second.
const SKIPPED_WRITES: u32 = 1000;
/// No packet for this long: the stream has stopped.
const STOPPED_NS: i128 = 1_000_000_000;
/// The latency until the control thread sets one, in ms.
const DEFAULT_LATENCY_MS: u32 = 20;
/// Zeros written at most per call while priming: the longest latency.
const ZEROS: usize = 4800;
/// The thread publishes its status at most this often.
const STATUS_NS: i128 = 100_000_000;
/// Datagrams read per round, and the size of each buffer: 8 channels of
/// L24 for 4 ms, and the RTP header.
const BATCH: usize = 16;
const DATAGRAM: usize = 5120;
/// How long the thread waits for a packet, or for a command while no
/// stream is selected.
const WAIT_MS: libc::c_int = 1;
const IDLE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Ptp,
    Buffer,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputStatus {
    /// The selected stream's session name.
    pub stream: Option<String>,
    /// The mode of the stream playing; `None` while none plays.
    pub mode: Option<Mode>,
    pub receiving: bool,
    pub latency_ms: u32,
    /// Frames played as silence because they never arrived.
    pub lost: u64,
    /// Packets dropped because they came after their play time.
    pub late: u64,
}

#[derive(Debug, Clone)]
pub enum InputCommand {
    Select(Option<Session>),
    Latency(u32),
    /// The two stream channels played as left and right, counted from 1.
    Channels([u8; 2]),
}

/// The thread, its stop flag, its status and its commands. Dropping it
/// stops the thread.
pub struct InputHandle {
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<InputStatus>>,
    commands: mpsc::Sender<InputCommand>,
}

impl InputHandle {
    pub fn command(&self, command: InputCommand) {
        // The thread only ends when the handle is dropped.
        let _ = self.commands.send(command);
    }

    /// The status as the thread last published it, at most 100 ms ago.
    pub fn status(&self) -> InputStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for InputHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts the `aas-aes67-in` thread at [`rt::WRITER_PRIORITY`]. It
/// receives on `interface` the stream an `InputCommand::Select` names and
/// writes it to `ring`, writing `wake` after each write. `output_delay` is
/// the engine's delay after the ring, in frames at `master_rate`.
pub fn start(
    interface: Ipv4Addr,
    clock: PtpClock,
    ring: Ring,
    wake: OwnedFd,
    output_delay: Arc<AtomicU32>,
    master_rate: Arc<AtomicU32>,
) -> io::Result<InputHandle> {
    let status = Arc::new(Mutex::new(InputStatus {
        latency_ms: DEFAULT_LATENCY_MS,
        ..InputStatus::default()
    }));
    let (commands, received) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let status = status.clone();
        let stop = stop.clone();
        let out = RingOut {
            ring,
            wake,
            wrote: false,
        };
        let shared = Shared {
            interface,
            clock,
            output_delay,
            master_rate,
            status,
            stop,
        };
        thread::Builder::new()
            .name("aas-aes67-in".into())
            .spawn(move || run(shared, out, received))?
    };
    Ok(InputHandle {
        thread: Some(thread),
        stop,
        status,
        commands,
    })
}

/// Where the frames go: the ring, or a fake one in the tests.
pub trait Output {
    /// Frames written and not yet read.
    fn fill(&self) -> usize;
    /// Frames that can be written.
    fn free(&self) -> usize;
    /// Writes what fits and returns the count.
    fn write(&mut self, frames: &[[i32; 2]]) -> usize;
}

/// What the receiver needs to know of the world at a step.
#[derive(Debug, Clone, Copy)]
pub struct Now {
    /// CLOCK_REALTIME, ns.
    pub local_ns: i128,
    pub ptp: Option<PtpNow>,
    /// `PtpClock::generation`.
    pub generation: u64,
    /// The engine's delay after the ring, in frames at 48 kHz.
    pub output_delay: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No stream selected.
    Idle,
    /// Waiting for a packet to start on.
    Waiting,
    /// Started: zeros go out until the first frame is at its target delay.
    Priming,
    Playing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Restart {
    /// The clock or the target changed: the controller starts over.
    Hard,
    /// The stream paused: the controller keeps its ratio.
    Soft,
}

/// The input's state machine, without sockets or threads.
pub struct Receiver {
    session: Option<Session>,
    pick: [u8; 2],
    latency_ms: u32,
    phase: Phase,
    /// The mode of the last start.
    mode: Option<Mode>,
    generation: Option<u64>,
    receiving: bool,
    /// Receive time of the last packet, CLOCK_REALTIME ns.
    last_rx: i128,
    /// Writes in a row whose reading the controller skipped.
    skipping: u32,
    /// Restarts of a stream that was playing or priming.
    restarts: u64,
    /// The ratio last set.
    ratio: f64,
    jitter: Jitter,
    adaptive: Adaptive,
    drift: Drift,
    decoded: Vec<[i32; 2]>,
    input: Vec<[i32; 2]>,
    output: Vec<[i32; 2]>,
    zeros: Vec<[i32; 2]>,
}

impl Receiver {
    /// Allocates everything it will use.
    pub fn new(latency_ms: u32) -> Receiver {
        let adaptive = Adaptive::new(RATE, RATE, 2, MAX_OUT, Quality::High, Format::I32);
        let input = vec![[0; 2]; adaptive.input_frames_max()];
        Receiver {
            session: None,
            pick: [1, 2],
            latency_ms,
            phase: Phase::Idle,
            mode: None,
            generation: None,
            receiving: false,
            last_rx: 0,
            skipping: 0,
            restarts: 0,
            ratio: 1.0,
            jitter: Jitter::new(),
            adaptive,
            drift: drift(latency_ms),
            decoded: vec![[0; 2]; MAX_PACKET_FRAMES],
            input,
            output: vec![[0; 2]; FRAMES],
            zeros: vec![[0; 2]; ZEROS],
        }
    }

    /// Selects a stream, or none. A different one starts over.
    pub fn select(&mut self, session: Option<Session>) {
        if session == self.session {
            return;
        }
        self.restart(Restart::Hard);
        self.phase = if session.is_some() {
            Phase::Waiting
        } else {
            Phase::Idle
        };
        self.session = session;
        self.mode = None;
        self.receiving = false;
    }

    /// A new latency starts the stream over at it.
    pub fn set_latency(&mut self, latency_ms: u32) {
        if latency_ms == self.latency_ms {
            return;
        }
        self.restart(Restart::Hard);
        self.latency_ms = latency_ms;
        self.drift = drift(latency_ms);
    }

    /// The stream channels played as left and right, counted from 1. Takes
    /// effect with the next packet.
    pub fn set_channels(&mut self, pick: [u8; 2]) {
        self.pick = pick;
    }

    /// One round: the packets received since the last, then as many writes
    /// of [`FRAMES`] frames as `out` has room for and the stream allows.
    pub fn step(&mut self, now: &Now, packets: &[(i128, &[u8])], out: &mut impl Output) {
        if self.generation != Some(now.generation) {
            if self.generation.is_some() {
                self.restart(Restart::Hard);
            }
            self.generation = Some(now.generation);
        }
        if self.session.is_none() {
            return;
        }
        for &(rx_ns, datagram) in packets {
            self.packet(rx_ns, datagram, now);
        }
        if self.receiving && now.local_ns - self.last_rx >= STOPPED_NS {
            self.receiving = false;
            self.restart(Restart::Soft);
        }
        if self.phase == Phase::Priming {
            self.prime(now, out);
        }
        if self.phase == Phase::Playing {
            self.play(now, out);
        }
    }

    /// Stores one datagram; the first after a (re)start starts the stream.
    fn packet(&mut self, rx_ns: i128, datagram: &[u8], now: &Now) {
        let Some(s) = &self.session else { return };
        let (payload_type, encoding, channels) = (s.payload_type, s.encoding, s.channels);
        let Some(p) = rtp::parse(datagram) else {
            return;
        };
        if p.payload_type != payload_type {
            return;
        }
        // A mono flow feeds both sides.
        let pick = if channels == 1 { [1, 1] } else { self.pick };
        let n = rtp::decode(encoding, channels, pick, p.payload, &mut self.decoded);
        if n == 0 {
            return;
        }
        if self.receiving && rx_ns - self.last_rx >= STOPPED_NS {
            self.restart(Restart::Soft);
        }
        self.receiving = true;
        self.last_rx = rx_ns;
        if self.phase == Phase::Waiting {
            self.start(p.ts, now);
        }
        self.jitter.put(p.ts, &self.decoded[..n]);
    }

    /// Starts on a packet at `ts`, in the mode the clock allows now.
    fn start(&mut self, ts: u32, now: &Now) {
        let refclk = self.session.as_ref().and_then(|s| s.refclk);
        let mode = match (now.ptp, refclk) {
            (Some(p), Some((gm, domain))) if gm == p.gm && domain == p.domain => Mode::Ptp,
            _ => Mode::Buffer,
        };
        if self.mode != Some(mode) {
            self.drift.reset();
        }
        self.mode = Some(mode);
        self.jitter.anchor(ts);
        self.phase = Phase::Priming;
    }

    /// The delay the controller holds, in frames, with `fill` frames in the
    /// ring; `None` in PTP mode without PTP time.
    fn measure(&self, now: &Now, fill: usize) -> Option<f64> {
        match self.mode? {
            Mode::Ptp => {
                let ptp = now.ptp.as_ref()?;
                let offset = self.session.as_ref()?.mediaclk_offset;
                let (ts, fraction) = media_now(ptp, now.local_ns, offset);
                // The next frame out of the resampler is its filter's delay
                // behind the jitter buffer's read position, and reaches the
                // master after the ring and the engine.
                let behind = f64::from(ts_diff(ts, self.jitter.read_ts())) + fraction;
                Some(behind + self.adaptive.delay_frames() + fill as f64 + now.output_delay)
            }
            Mode::Buffer => Some(f64::from(self.jitter.depth()) + fill as f64),
        }
    }

    /// Zeros ahead of the first frame until it is at its target delay, or
    /// frames dropped when it is past it already.
    fn prime(&mut self, now: &Now, out: &mut impl Output) {
        let target = self.target();
        let Some(delay) = self.measure(now, out.fill()) else {
            return;
        };
        if delay > target {
            // Due before it arrived: drop what is late.
            let mut late = (delay - target).round() as usize;
            while late > 0 {
                let n = late.min(self.input.len());
                self.jitter.take(&mut self.input[..n]);
                late -= n;
            }
        } else {
            let mut missing = (target - delay).round() as usize;
            while missing > 0 {
                let n = out.write(&self.zeros[..missing.min(ZEROS)]);
                if n == 0 {
                    // The ring is full: go on at the next step.
                    return;
                }
                missing -= n;
            }
        }
        self.phase = Phase::Playing;
    }

    /// Writes while the ring has room and the stream has the frames. In PTP
    /// mode a ring about to run dry is written anyway, with a lost frame as
    /// silence, so the timing holds.
    fn play(&mut self, now: &Now, out: &mut impl Output) {
        let target = self.target();
        while out.free() >= FRAMES {
            let fill = out.fill();
            // Without PTP time the generation moves on and restarts.
            let Some(delay) = self.measure(now, fill) else {
                return;
            };
            if (delay - target).abs() > RESTART_FRAMES {
                self.restart(Restart::Hard);
                return;
            }
            let need = self.adaptive.input_needed(FRAMES as u32) as usize;
            let starving = self.mode == Some(Mode::Ptp) && fill < FRAMES;
            if (self.jitter.depth() as usize) <= need && !starving {
                return;
            }
            let skipped = self.drift.skipped();
            self.ratio = self.drift.update(delay);
            if self.drift.skipped() > skipped {
                self.skipping += 1;
                if self.skipping >= SKIPPED_WRITES {
                    self.restart(Restart::Hard);
                    return;
                }
            } else {
                self.skipping = 0;
            }
            self.adaptive.set_ratio(self.ratio);
            let need = self.adaptive.input_needed(FRAMES as u32) as usize;
            self.jitter.take(&mut self.input[..need]);
            let (_, made) = self.adaptive.process(
                self.input[..need].as_flattened(),
                self.output.as_flattened_mut(),
            );
            out.write(&self.output[..made]);
        }
    }

    /// Copies the state into `status`, the name only when it changed.
    pub fn publish(&self, status: &mut InputStatus) {
        let name = self.session.as_ref().map(|s| s.name.as_str());
        if status.stream.as_deref() != name {
            status.stream = name.map(str::to_owned);
        }
        status.mode = match self.phase {
            Phase::Priming | Phase::Playing => self.mode,
            Phase::Idle | Phase::Waiting => None,
        };
        status.receiving = self.receiving;
        status.latency_ms = self.latency_ms;
        (status.lost, status.late) = self.jitter.counts();
    }

    fn restart(&mut self, kind: Restart) {
        if self.phase == Phase::Idle {
            return;
        }
        if matches!(self.phase, Phase::Priming | Phase::Playing) {
            self.restarts += 1;
        }
        self.phase = Phase::Waiting;
        self.adaptive.reset();
        self.ratio = 1.0;
        self.skipping = 0;
        match kind {
            Restart::Hard => self.drift.reset(),
            Restart::Soft => self.drift.reprime(),
        }
    }

    fn target(&self) -> f64 {
        f64::from(self.latency_ms * RATE / 1000)
    }
}

fn drift(latency_ms: u32) -> Drift {
    Drift::new(f64::from(latency_ms * RATE / 1000), RATE, FRAMES as u32)
}

/// The media-clock time at `local_ns`: the RTP timestamp, and the fraction
/// of a frame past it.
fn media_now(ptp: &PtpNow, local_ns: i128, offset: u32) -> (u32, f64) {
    let ptp_ns = ptp.ptp_ns + ((local_ns - ptp.local_ns) as f64 * ptp.rate).round() as i128;
    let ticks = ptp_ns * i128::from(RATE);
    let fraction = ticks.rem_euclid(1_000_000_000) as f64 / 1e9;
    (rtp::media_ts(ptp_ns, offset), fraction)
}

/// The ring and its wake-up.
struct RingOut {
    ring: Ring,
    wake: OwnedFd,
    wrote: bool,
}

impl Output for RingOut {
    fn fill(&self) -> usize {
        self.ring.available()
    }

    fn free(&self) -> usize {
        self.ring.free()
    }

    fn write(&mut self, frames: &[[i32; 2]]) -> usize {
        let n = self.ring.write(frames);
        self.wrote |= n > 0;
        n
    }
}

impl RingOut {
    /// Writes the wake-up once if anything was written since the last.
    fn signal(&mut self) {
        if !self.wrote {
            return;
        }
        self.wrote = false;
        let one = 1u64;
        // SAFETY: 8 bytes to an eventfd. A full counter only means the
        // reader has plenty of wake-ups waiting.
        unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                (&raw const one).cast(),
                mem::size_of::<u64>(),
            )
        };
    }
}

/// What the thread shares with the control thread.
struct Shared {
    interface: Ipv4Addr,
    clock: PtpClock,
    output_delay: Arc<AtomicU32>,
    master_rate: Arc<AtomicU32>,
    status: Arc<Mutex<InputStatus>>,
    stop: Arc<AtomicBool>,
}

/// A socket joined to a stream's group, and the group and port.
struct Joined {
    group: Ipv4Addr,
    port: u16,
    socket: UdpSocket,
}

fn run(shared: Shared, mut out: RingOut, commands: mpsc::Receiver<InputCommand>) {
    match rt::set_fifo(rt::WRITER_PRIORITY) {
        Ok(()) => eprintln!("aas: aes67: input at SCHED_FIFO {}", rt::WRITER_PRIORITY),
        Err(e) => eprintln!("aas: warning: aes67: input not real time: {e}"),
    }
    let mut rx = Receiver::new(DEFAULT_LATENCY_MS);
    let mut joined: Option<Joined> = None;
    let mut bufs = vec![[0u8; DATAGRAM]; BATCH];
    let mut received = [(0i128, 0usize); BATCH];
    let mut published = 0i128;
    while !shared.stop.load(Ordering::Relaxed) {
        loop {
            match commands.try_recv() {
                Ok(command) => apply(command, &mut rx, &mut joined, shared.interface),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let mut count = 0;
        match &joined {
            Some(j) => {
                if wait_readable(&j.socket) {
                    while count < BATCH {
                        match receive(&j.socket, &mut bufs[count]) {
                            Some(r) => {
                                received[count] = r;
                                count += 1;
                            }
                            None => break,
                        }
                    }
                }
            }
            None => match commands.recv_timeout(IDLE) {
                Ok(command) => apply(command, &mut rx, &mut joined, shared.interface),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            },
        }
        let mut packets: [(i128, &[u8]); BATCH] = [(0, &[]); BATCH];
        for (p, (&(rx_ns, len), buf)) in packets.iter_mut().zip(received.iter().zip(&bufs)) {
            *p = (rx_ns, &buf[..len]);
        }
        let ptp = shared.clock.now();
        let rate = shared.master_rate.load(Ordering::Relaxed).max(1);
        let delay = shared.output_delay.load(Ordering::Relaxed);
        let now = Now {
            local_ns: ptp.map_or_else(realtime_ns, |p| p.local_ns),
            ptp,
            generation: shared.clock.generation(),
            output_delay: f64::from(delay) * f64::from(RATE) / f64::from(rate),
        };
        rx.step(&now, &packets[..count], &mut out);
        out.signal();
        if (now.local_ns - published).abs() >= STATUS_NS {
            rx.publish(&mut shared.status.lock().unwrap_or_else(|e| e.into_inner()));
            published = now.local_ns;
        }
    }
}

fn apply(
    command: InputCommand,
    rx: &mut Receiver,
    joined: &mut Option<Joined>,
    interface: Ipv4Addr,
) {
    match command {
        InputCommand::Select(session) => {
            let wanted = session.as_ref().map(|s| (s.group, s.port));
            let have = joined.as_ref().map(|j| (j.group, j.port));
            if wanted != have {
                *joined = None;
                if let Some((group, port)) = wanted {
                    match join(group, port, interface) {
                        Ok(socket) => {
                            *joined = Some(Joined {
                                group,
                                port,
                                socket,
                            })
                        }
                        Err(e) => eprintln!("aas: aes67: input: {group}:{port}: {e}"),
                    }
                }
            }
            rx.select(session);
        }
        InputCommand::Latency(ms) => rx.set_latency(ms),
        InputCommand::Channels(pick) => rx.set_channels(pick),
    }
}

/// A UDP socket bound to `group:port`, joined to the group on `interface`,
/// with kernel receive timestamps.
fn join(group: Ipv4Addr, port: u16, interface: Ipv4Addr) -> io::Result<UdpSocket> {
    // SAFETY: plain socket call; the fd is owned from here on.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a new, valid descriptor nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // The output may send on the same port; other receivers may listen.
    set_option(&fd, libc::SO_REUSEADDR, &1 as &libc::c_int)?;
    set_option(&fd, libc::SO_TIMESTAMPNS, &1 as &libc::c_int)?;
    // SAFETY: an all-zero sockaddr_in is valid.
    let mut sin: libc::sockaddr_in = unsafe { mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = port.to_be();
    sin.sin_addr.s_addr = u32::from(group).to_be();
    // SAFETY: sin is a sockaddr_in of the size passed.
    let r = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &sin as *const libc::sockaddr_in as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = UdpSocket::from(fd);
    socket.join_multicast_v4(&group, &interface)?;
    Ok(socket)
}

/// Waits up to [`WAIT_MS`] for a packet; true if one is there.
fn wait_readable(socket: &UdpSocket) -> bool {
    let mut fd = libc::pollfd {
        fd: socket.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut fd, 1, WAIT_MS) > 0 }
}

/// One datagram, without waiting, and its kernel receive time
/// (CLOCK_REALTIME ns). `None` when there is none; a truncated one reads as
/// empty.
fn receive(socket: &UdpSocket, buf: &mut [u8]) -> Option<(i128, usize)> {
    // u64 keeps the control buffer aligned for cmsghdr.
    let mut control = [0u64; 8];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    // SAFETY: an all-zero msghdr is valid.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = mem::size_of_val(&control) as _;
    // SAFETY: msg points to live buffers of the sizes it states.
    let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, libc::MSG_DONTWAIT) };
    if n < 0 {
        return None;
    }
    let mut rx_ns = None;
    // SAFETY: the CMSG macros walk the control buffer recvmsg filled in.
    let mut c = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !c.is_null() {
        let h = unsafe { &*c };
        if h.cmsg_level == libc::SOL_SOCKET && h.cmsg_type == libc::SCM_TIMESTAMPNS {
            // SAFETY: an SCM_TIMESTAMPNS message carries a timespec.
            let ts: libc::timespec =
                unsafe { ptr::read_unaligned(libc::CMSG_DATA(c) as *const libc::timespec) };
            rx_ns = Some(ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128);
        }
        c = unsafe { libc::CMSG_NXTHDR(&msg, c) };
    }
    let len = if msg.msg_flags & libc::MSG_TRUNC != 0 {
        0
    } else {
        n as usize
    };
    Some((rx_ns.unwrap_or_else(realtime_ns), len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aes67::ptp::ClockId;
    use crate::aes67::sdp::Encoding;
    use assert_no_alloc::assert_no_alloc;
    use std::collections::VecDeque;

    const MASTER: ClockId = [0x00, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55];
    const OTHER: ClockId = [0x00, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x66];
    const LATENCY_MS: u32 = 20;
    const TARGET: f64 = (LATENCY_MS * 48) as f64;
    /// How far the sender's clock runs ahead of the local one.
    const PPM: f64 = 16.3;
    /// Local time at the start, ns.
    const T0: i128 = 1_700_000_000_000_000_000;
    /// PTP time at the start, ns: an arbitrary timescale, whole seconds.
    const P0: i128 = 123_456_789_000_000_000;
    const OFFSET: u32 = 0x9abc_def0;
    /// The simulation's step, ns.
    const TICK_NS: i128 = 250_000;
    /// The engine's delay after the ring, frames at 48 kHz.
    const OUTPUT_DELAY: f64 = 256.0;
    const AMPLITUDE: i32 = 0x40_0000 << 8;
    /// Half a period of the square wave, frames.
    const HALF: i128 = 2400;

    fn session(channels: u8, refclk: Option<(ClockId, u8)>) -> Session {
        Session {
            name: "test".into(),
            origin: Ipv4Addr::new(192, 0, 2, 10),
            session_id: 1,
            version: 1,
            group: Ipv4Addr::new(239, 69, 0, 1),
            port: 5004,
            ttl: 16,
            payload_type: 98,
            encoding: Encoding::L24,
            rate: 48000,
            channels,
            ptime_us: 1000,
            refclk,
            mediaclk_offset: OFFSET,
        }
    }

    /// The engine's side of the ring: drains 48 frames per ms of local time,
    /// zeros when it finds the ring empty. Frame `n` of `played` is drained
    /// at `T0 + n / 48 kHz`.
    struct FakeRing {
        capacity: usize,
        queue: VecDeque<[i32; 2]>,
        played: Vec<[i32; 2]>,
    }

    impl Output for FakeRing {
        fn fill(&self) -> usize {
            self.queue.len()
        }
        fn free(&self) -> usize {
            self.capacity - self.queue.len()
        }
        fn write(&mut self, frames: &[[i32; 2]]) -> usize {
            let n = frames.len().min(self.free());
            self.queue.extend(&frames[..n]);
            n
        }
    }

    impl FakeRing {
        fn drain(&mut self, now: i128) {
            let due = ((now - T0) * 48_000 / 1_000_000_000) as usize;
            while self.played.len() < due {
                let f = self.queue.pop_front().unwrap_or([0; 2]);
                self.played.push(f);
            }
        }
    }

    fn square(media: i128, _channel: u8) -> i32 {
        if media.div_euclid(HALF) % 2 == 0 {
            AMPLITUDE
        } else {
            -AMPLITUDE
        }
    }

    /// Channel `c` carries `c << 28`, steady.
    fn steady(_media: i128, channel: u8) -> i32 {
        i32::from(channel) << 28
    }

    /// A sender whose media clock follows the PTP time, which runs `PPM`
    /// ahead of the local clock, a network, a fake engine and the receiver.
    struct Sim {
        rx: Receiver,
        ring: FakeRing,
        now: i128,
        channels: u8,
        sample: fn(i128, u8) -> i32,
        /// Unwrapped media time of the next packet's first frame.
        next_media: i128,
        in_flight: Vec<(i128, Vec<u8>)>,
        sending: bool,
        ptp_known: bool,
        gm: ClockId,
        generation: u64,
        seq: u16,
        noise: u64,
    }

    fn m0() -> i128 {
        P0 / 1_000_000_000 * 48_000 + i128::from(OFFSET)
    }

    fn rate() -> f64 {
        1.0 + PPM * 1e-6
    }

    fn ptp_at(local: i128) -> i128 {
        P0 + ((local - T0) as f64 * rate()).round() as i128
    }

    /// Unwrapped media time `since` ns of local time after `T0`, frames
    /// since `m0`.
    fn media_since_start(since: f64) -> f64 {
        since * rate() * 48_000.0 / 1e9
    }

    impl Sim {
        fn new(s: Session, sample: fn(i128, u8) -> i32, ptp_known: bool) -> Sim {
            let mut rx = Receiver::new(LATENCY_MS);
            let channels = s.channels;
            rx.select(Some(s));
            Sim {
                rx,
                ring: FakeRing {
                    capacity: 4096,
                    queue: VecDeque::new(),
                    played: Vec::new(),
                },
                now: T0,
                channels,
                sample,
                next_media: m0(),
                in_flight: Vec::new(),
                sending: true,
                ptp_known,
                gm: MASTER,
                generation: 1,
                seq: 0,
                noise: 1,
            }
        }

        /// Local time at which the sender has the frame at unwrapped media
        /// time `media`.
        fn local_of(media: i128) -> i128 {
            let frames = (media - m0()) as f64;
            T0 + (frames * 1e9 / (48_000.0 * rate())).round() as i128
        }

        fn datagram(&mut self, media: i128) -> Vec<u8> {
            let ts = media.rem_euclid(1 << 32) as u32;
            let mut d = rtp::header(self.seq, ts, 0x1234_5678, 98).to_vec();
            self.seq = self.seq.wrapping_add(1);
            for i in 0..48 {
                for c in 1..=self.channels {
                    let v = (self.sample)(media + i, c);
                    d.extend_from_slice(&v.to_be_bytes()[..3]);
                }
            }
            d
        }

        /// 0.3 ms of network delay and up to 0.2 ms of jitter.
        fn delay(&mut self) -> i128 {
            self.noise = self
                .noise
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            300_000 + ((self.noise >> 33) % 200_000) as i128
        }

        fn now(&self) -> Now {
            Now {
                local_ns: self.now,
                ptp: self.ptp_known.then(|| PtpNow {
                    local_ns: self.now,
                    ptp_ns: ptp_at(self.now),
                    gm: self.gm,
                    domain: 0,
                    rate: rate(),
                }),
                generation: self.generation,
                output_delay: OUTPUT_DELAY,
            }
        }

        fn run(&mut self, seconds: f64) {
            let end = self.now + (seconds * 1e9) as i128;
            while self.now < end {
                self.now += TICK_NS;
                // A packet leaves once its last frame is in.
                while Sim::local_of(self.next_media + 48) <= self.now {
                    let media = self.next_media;
                    self.next_media += 48;
                    if self.sending {
                        let at = Sim::local_of(media + 48) + self.delay();
                        let d = self.datagram(media);
                        self.in_flight.push((at, d));
                    }
                }
                self.in_flight.sort_by_key(|p| p.0);
                let due = self.in_flight.partition_point(|p| p.0 <= self.now);
                let arrived: Vec<_> = self.in_flight.drain(..due).collect();
                let packets: Vec<(i128, &[u8])> =
                    arrived.iter().map(|(t, d)| (*t, d.as_slice())).collect();
                self.ring.drain(self.now);
                let now = self.now();
                // The ring is pre-sized; the receiver allocates nothing.
                self.ring.queue.reserve(self.ring.capacity);
                self.ring.played.reserve(48_000);
                let (rx, ring) = (&mut self.rx, &mut self.ring);
                assert_no_alloc(|| rx.step(&now, &packets, ring));
            }
        }

        fn status(&self) -> InputStatus {
            let mut s = InputStatus::default();
            self.rx.publish(&mut s);
            s
        }

        /// Index of the frame the engine drains at `local`.
        fn index(local: i128) -> usize {
            ((local - T0) * 48_000 / 1_000_000_000) as usize
        }
    }

    /// The delay from each edge of the square wave's media time to the
    /// local time it reaches the master, in frames, for the edges drained
    /// from `from` on.
    fn edge_delays(sim: &Sim, from: usize) -> Vec<f64> {
        let mut delays = Vec::new();
        for n in from.max(1)..sim.ring.played.len() {
            let (a, b) = (sim.ring.played[n - 1][0], sim.ring.played[n][0]);
            if !((a > 0 && b < 0) || (a < 0 && b > 0)) {
                continue;
            }
            // The edge lies between the two frames; it reaches the master
            // the engine's output delay after it is drained.
            let reached = (n as f64 - 0.5 + OUTPUT_DELAY) * 1e9 / 48_000.0;
            let media = media_since_start(reached) + (m0() % HALF) as f64;
            let edge = ((media - TARGET) / HALF as f64).round() * HALF as f64 - 0.5;
            delays.push(media - edge);
        }
        delays
    }

    #[test]
    fn ptp_mode_holds_the_latency() {
        let mut sim = Sim::new(session(2, Some((MASTER, 0))), square, true);
        sim.run(30.0);
        assert_eq!(sim.status().mode, Some(Mode::Ptp));
        let from = Sim::index(sim.now - 10_000_000_000);
        let delays = edge_delays(&sim, from);
        assert!(delays.len() >= 150, "{} edges", delays.len());
        for d in &delays {
            assert!(
                (d - TARGET).abs() <= 12.0,
                "delay {:.2} ms, want {LATENCY_MS} ms",
                d / 48.0
            );
        }
        let want = 1.0 / rate();
        assert!(
            (sim.rx.ratio - want).abs() < 1e-6,
            "ratio {} ppm off, want {} ppm off",
            (sim.rx.ratio - 1.0) * 1e6,
            (want - 1.0) * 1e6
        );
        assert_eq!(sim.rx.restarts, 0);
    }

    #[test]
    fn buffer_mode_holds_the_depth() {
        let mut sim = Sim::new(session(2, Some((MASTER, 0))), square, false);
        sim.run(29.0);
        assert_eq!(sim.status().mode, Some(Mode::Buffer));
        // The depth moves by a packet as each arrives: average over a second.
        let mut sum = 0.0;
        let mut n = 0.0;
        let end = sim.now + 1_000_000_000;
        while sim.now < end {
            sim.run(0.000_25);
            sum += f64::from(sim.rx.jitter.depth()) + sim.ring.fill() as f64;
            n += 1.0;
        }
        let depth = sum / n;
        assert!(
            (depth - TARGET).abs() <= 24.0,
            "depth {:.2} ms, want {LATENCY_MS} ms",
            depth / 48.0
        );
        assert_eq!(sim.rx.restarts, 0);
        assert!(sim.ring.played.last().unwrap()[0] != 0);
    }

    #[test]
    fn a_foreign_grandmaster_plays_in_buffer_mode() {
        let mut sim = Sim::new(session(2, Some((OTHER, 0))), square, true);
        sim.run(2.0);
        assert_eq!(sim.status().mode, Some(Mode::Buffer));
        let tail = &sim.ring.played[sim.ring.played.len() - 480..];
        assert!(tail.iter().all(|f| f[0] != 0), "plays");
    }

    #[test]
    fn a_new_grandmaster_restarts_the_input() {
        let mut sim = Sim::new(session(2, Some((MASTER, 0))), square, true);
        sim.run(5.0);
        assert_eq!(sim.status().mode, Some(Mode::Ptp));
        let change = Sim::index(sim.now);
        sim.gm = OTHER;
        sim.generation += 1;
        sim.run(1.0);
        assert_eq!(sim.rx.restarts, 1);
        // The stream names the old grandmaster: it plays on in buffer mode.
        assert_eq!(sim.status().mode, Some(Mode::Buffer));
        let after = &sim.ring.played[change..];
        let silent = after
            .windows(48)
            .position(|w| w.iter().all(|f| *f == [0; 2]))
            .expect("the output goes silent");
        let audible = silent
            + after[silent..]
                .iter()
                .position(|f| f[0] != 0)
                .expect("the output plays again");
        assert!(
            audible <= ((LATENCY_MS + 50) * 48) as usize,
            "audible again after {:.1} ms",
            audible as f64 / 48.0
        );
    }

    #[test]
    fn a_stream_that_stops_plays_silence_and_resumes() {
        let mut sim = Sim::new(session(2, Some((MASTER, 0))), square, true);
        sim.run(3.0);
        assert!(sim.status().receiving);
        sim.sending = false;
        sim.run(0.9);
        assert!(sim.status().receiving, "not stopped before a second");
        sim.run(0.2);
        let status = sim.status();
        assert!(!status.receiving);
        assert_eq!(status.stream.as_deref(), Some("test"));
        let n = sim.ring.played.len();
        assert!(sim.ring.played[n - 480..].iter().all(|f| *f == [0; 2]));
        sim.sending = true;
        let resumed = Sim::index(sim.now);
        sim.run(0.1);
        assert!(sim.status().receiving);
        let audible = sim.ring.played[resumed..]
            .iter()
            .position(|f| f[0] != 0)
            .expect("plays again");
        assert!(audible <= ((LATENCY_MS + 50) * 48) as usize);
        // And at its latency again.
        sim.run(3.0);
        let delays = edge_delays(&sim, Sim::index(sim.now - 1_000_000_000));
        assert!(!delays.is_empty());
        for d in &delays {
            assert!((d - TARGET).abs() <= 12.0, "delay {:.2} ms", d / 48.0);
        }
    }

    #[test]
    fn a_lost_packet_plays_as_silence_in_its_place() {
        let mut sim = Sim::new(session(2, Some((MASTER, 0))), square, true);
        sim.run(5.0);
        sim.sending = false;
        sim.run(0.03);
        sim.sending = true;
        sim.run(0.5);
        assert!(sim.status().lost >= 30 * 48, "{:?}", sim.rx.jitter.counts());
        assert_eq!(sim.rx.restarts, 0);
        let delays = edge_delays(&sim, Sim::index(sim.now - 400_000_000));
        assert!(!delays.is_empty());
        for d in &delays {
            assert!((d - TARGET).abs() <= 12.0, "delay {:.2} ms", d / 48.0);
        }
    }

    #[test]
    fn the_thread_takes_commands_and_stops_with_its_handle() {
        let (ring, _memfd) = Ring::create(1024).unwrap();
        let wake = crate::control::eventfd().unwrap();
        let handle = start(
            Ipv4Addr::LOCALHOST,
            PtpClock::off(),
            ring,
            wake,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU32::new(48_000)),
        )
        .unwrap();
        handle.command(InputCommand::Latency(7));
        let mut status = handle.status();
        for _ in 0..50 {
            if status.latency_ms == 7 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
            status = handle.status();
        }
        assert_eq!(status.latency_ms, 7);
        assert_eq!(status.stream, None);
        drop(handle);
    }

    #[test]
    fn channels_are_picked() {
        let mut sim = Sim::new(session(4, None), steady, false);
        sim.rx.set_channels([3, 4]);
        sim.run(1.0);
        let last = *sim.ring.played.last().unwrap();
        for (got, channel) in last.iter().zip([3, 4]) {
            let want = f64::from(channel << 28);
            assert!(
                (f64::from(*got) - want).abs() < want * 0.001,
                "{got:#x}, want channel {channel}"
            );
        }
    }

    #[test]
    fn a_mono_flow_feeds_both_channels() {
        let mut sim = Sim::new(session(1, None), steady, false);
        sim.run(1.0);
        let last = *sim.ring.played.last().unwrap();
        let want = f64::from(1 << 28);
        for got in last {
            assert!((f64::from(got) - want).abs() < want * 0.001, "{got:#x}");
        }
    }
}
