//! The `aas-aes67-out` thread: what the master plays, as an AES67 stream.
//!
//! It reads the master from a monitor tap, as an AirPlay receiver does,
//! and sends it to a multicast group as L24 stereo at 48 kHz, one packet
//! of 48 frames for every millisecond of PTP time. While it sends, the
//! announce slot holds its session, which the SAP thread announces.
//!
//! - **Timeline.** At its start, and again when PTP time comes back, the
//!   grandmaster or the generation changes, or PTP time is more than 50 ms
//!   from where the timeline expects it (a step of the local clock shows
//!   there before the clock layer's generation moves), the next packet is
//!   put at the next whole millisecond of PTP time. Its RTP timestamp is
//!   the media-clock time of its first frame, `mediaclk:direct=0`; each
//!   packet after it is 1 ms of PTP time and 48 ticks later. A packet goes
//!   out once PTP time reaches the media time of its first frame, with no
//!   lead: its last frame is then a millisecond ahead of the time a
//!   capturing device could send it, which is margin for the receiver.
//!   A new master rate leaves the timeline alone.
//! - **Rate.** An [`Adaptive`] resampler from the master's rate to 48 kHz,
//!   steered by a [`Drift`] that holds the tap-to-send delay at 20 ms of
//!   48 kHz frames against the packets' PTP pace, so the master card's
//!   crystal is absorbed against the grandmaster. The delay is the frames
//!   waiting and the resampler's filter, less what is left of the last
//!   period the tap gave, counted down at the master's rate since it came:
//!   the master writes a period at a time, and without that the reading
//!   would swing by a whole period, beyond what the controller believes for
//!   a period of 1024 frames. Waiting frames never fall below the target,
//!   so a period of any size survives.
//! - **Silence.** Until the path holds its target, and when the tap runs
//!   dry, packets carry zeros: a Dante subscription wants a steady stream.
//!   Nothing goes out without PTP time, and the announcement is withdrawn.
//!
//! [`Sender`] is all of that, pure and tested without sockets; the thread
//! around it reads the tap, the clock and the socket. Nothing allocates per
//! packet.

use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use super::clock::{PtpClock, PtpNow, realtime_ns};
use super::config::{Group, OutputConfig};
use super::rtp;
use super::sap_thread::multicast_interface;
use super::sdp::{Encoding, Session};
use crate::adaptive::Adaptive;
use crate::airplay::output::TapReader;
use crate::config::{Format, Quality};
use crate::drift::Drift;
use crate::rt;

/// The rate on the wire.
const RATE: u32 = 48_000;
/// Frames per packet: 1 ms.
const FRAMES: usize = 48;
/// A packet's length in PTP time, ns.
const PACKET_NS: i128 = 1_000_000;
/// The RTP payload type of the stream.
pub const PAYLOAD_TYPE: u8 = 96;
/// RTP header and 48 frames of L24 stereo.
const PACKET_BYTES: usize = 12 + FRAMES * 6;
/// The tap-to-send delay the path holds, in 48 kHz frames: 20 ms.
const TARGET: f64 = 960.0;
/// A delay this far above the target, in 48 kHz frames, is cut back to it:
/// 8 ms, where the controller stops believing a reading.
const EXCESS: f64 = 384.0;
/// PTP time this far from where the timeline expects it re-anchors it.
const JUMP_NS: i128 = 50_000_000;
/// How often the thread looks at the tap and the clock while not sending.
const IDLE_NS: i128 = 20_000_000;
/// Frames read from the tap at a time.
const CHUNK: usize = 1024;
/// How often the thread publishes its delay, ns.
const STATUS_NS: i128 = 100_000_000;

/// The output thread as the control thread sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputStatus {
    /// The thread runs.
    pub enabled: bool,
    /// It has PTP time and sends; its session is announced.
    pub sending: bool,
    /// The group it sends to, the config's or the one made from the MAC.
    pub group: Ipv4Addr,
    pub port: u16,
    /// The tap-to-send delay as the controller last read it, in µs.
    pub delay_us: Option<i64>,
}

/// What the sender needs to know of the world at a step.
#[derive(Debug, Clone, Copy)]
pub struct Now {
    /// CLOCK_REALTIME, ns.
    pub local_ns: i128,
    pub ptp: Option<PtpNow>,
    /// `PtpClock::generation`.
    pub generation: u64,
}

/// The output's state machine, without sockets or threads.
pub struct Sender {
    ssrc: u32,
    seq: u16,
    /// The session announced while sending; its `refclk` is the
    /// grandmaster the timeline was last anchored on.
    session: Session,
    /// The generation the timeline is anchored in; `None` while not
    /// sending.
    generation: Option<u64>,
    /// PTP time of the next packet's first frame, ns.
    next_ptp: i128,
    /// Its RTP timestamp.
    next_ts: u32,
    master_rate: u32,
    /// Master-rate frames waiting.
    input: Vec<[i32; 2]>,
    /// The most frames `input` holds.
    capacity: usize,
    /// Master-rate frames of the last period from the tap, as they were
    /// when it came, and when, local ns.
    pending: f64,
    pending_at: i128,
    adaptive: Adaptive,
    drift: Drift,
    /// Until the path first holds its target, packets go out as zeros.
    primed: bool,
    /// The delay as the drift last read it.
    measured: f64,
    /// Packets that had to be zeros after priming.
    padded: u64,
    out: Vec<[i32; 2]>,
    packet: Vec<u8>,
}

impl Sender {
    /// A sender of `session`, not sending until it has PTP time, with the
    /// master at `master_rate`. Allocates everything it will use.
    pub fn new(session: Session, ssrc: u32, first_seq: u16, master_rate: u32) -> Sender {
        let (adaptive, drift, capacity) = path(master_rate);
        Sender {
            ssrc,
            seq: first_seq,
            session,
            generation: None,
            next_ptp: 0,
            next_ts: 0,
            master_rate,
            input: Vec::with_capacity(capacity),
            capacity,
            pending: 0.0,
            pending_at: 0,
            adaptive,
            drift,
            primed: false,
            measured: 0.0,
            padded: 0,
            out: vec![[0; 2]; FRAMES],
            packet: Vec::with_capacity(PACKET_BYTES),
        }
    }

    /// A new master rate, from a new tap: the frames at the old one are
    /// dropped and the path primes again. The RTP timeline goes on.
    pub fn set_rate(&mut self, master_rate: u32) {
        let (adaptive, drift, capacity) = path(master_rate);
        self.master_rate = master_rate;
        self.adaptive = adaptive;
        self.drift = drift;
        self.capacity = capacity;
        self.input = Vec::with_capacity(capacity);
        self.pending = 0.0;
        self.primed = false;
    }

    /// Takes master-rate frames from the tap, read at `local_ns`. A delay
    /// beyond what the controller believes is cut back to the target.
    pub fn take(&mut self, frames: &[[i32; 2]], local_ns: i128) {
        let frames = &frames[frames.len().saturating_sub(self.capacity)..];
        let over = (self.input.len() + frames.len()).saturating_sub(self.capacity);
        if over > 0 {
            self.drop_frames(over);
        }
        // Only the newest block is ahead of real time; what came before it
        // is due. Summing the blocks would keep a backlog the tap gave at its
        // start in the reading for good, and the path that long too.
        if !frames.is_empty() {
            self.pending = frames.len() as f64;
            self.pending_at = local_ns;
        }
        self.input.extend_from_slice(frames);
        let delay = self.delay(local_ns);
        if delay > TARGET + EXCESS {
            self.drop_frames(self.master_frames(delay - TARGET));
            self.drift.reprime();
        }
    }

    /// Sends every packet whose time has come, through `send`.
    pub fn step(&mut self, now: &Now, send: &mut impl FnMut(&[u8])) {
        let Some(ptp) = now.ptp else {
            self.generation = None;
            return;
        };
        let jumped = (ptp.ptp_ns - self.next_ptp).abs() > JUMP_NS;
        if self.generation != Some(now.generation) || jumped {
            self.anchor(&ptp, now.generation);
        }
        while self.next_ptp <= ptp.ptp_ns {
            self.send(now.local_ns, send);
        }
    }

    /// Puts the next packet at the next whole millisecond of PTP time, and
    /// the grandmaster in the session, with a new version when it changed.
    /// Coming back from no PTP time, the path primes again.
    fn anchor(&mut self, ptp: &PtpNow, generation: u64) {
        if self.generation.is_none() {
            self.primed = false;
        }
        self.next_ptp = -(-ptp.ptp_ns).div_euclid(PACKET_NS) * PACKET_NS;
        self.next_ts = rtp::media_ts(self.next_ptp, 0);
        let refclk = Some((ptp.gm, ptp.domain));
        if self.session.refclk != refclk {
            if self.session.refclk.is_some() {
                self.session.version += 1;
            }
            self.session.refclk = refclk;
        }
        self.generation = Some(generation);
    }

    fn send(&mut self, local_ns: i128, send: &mut impl FnMut(&[u8])) {
        self.frames(local_ns);
        self.packet.clear();
        let header = rtp::header(self.seq, self.next_ts, self.ssrc, PAYLOAD_TYPE);
        self.packet.extend_from_slice(&header);
        rtp::encode_l24(&self.out, &mut self.packet);
        send(&self.packet);
        self.seq = self.seq.wrapping_add(1);
        self.next_ts = self.next_ts.wrapping_add(FRAMES as u32);
        self.next_ptp += PACKET_NS;
    }

    /// The next packet's frames into `out`: resampled from the input once
    /// the path holds its target, zeros before, and zeros when it runs
    /// dry, after which it primes again.
    fn frames(&mut self, local_ns: i128) {
        let mut delay = self.delay(local_ns);
        if !self.primed {
            if delay < TARGET {
                self.out.fill([0; 2]);
                return;
            }
            // Frames that waited for PTP time are late: they go.
            self.drop_frames(self.master_frames(delay - TARGET));
            self.primed = true;
            delay = self.delay(local_ns);
        }
        self.measured = delay;
        let ratio = self.drift.update(delay);
        self.adaptive.set_ratio(ratio);
        let need = self.adaptive.input_needed(FRAMES as u32) as usize;
        if self.input.len() < need {
            self.out.fill([0; 2]);
            self.padded += 1;
            self.primed = false;
            self.drift.reprime();
            return;
        }
        let (used, _) = self
            .adaptive
            .process(self.input.as_flattened(), self.out.as_flattened_mut());
        self.input.drain(..used);
    }

    /// The tap-to-send delay at `local_ns` in 48 kHz frames: the frames
    /// waiting, less what is left of the last period, and the filter.
    fn delay(&self, local_ns: i128) -> f64 {
        let waiting = self.input.len() as f64 - self.pending_now(local_ns);
        waiting * f64::from(RATE) / f64::from(self.master_rate) + self.adaptive.delay_frames()
    }

    /// What is left of the last period at `local_ns`, master frames.
    fn pending_now(&self, local_ns: i128) -> f64 {
        let gone = (local_ns - self.pending_at) as f64 * 1e-9 * f64::from(self.master_rate);
        (self.pending - gone.max(0.0)).max(0.0)
    }

    /// `frames` at 48 kHz in master-rate frames, rounded down.
    fn master_frames(&self, frames: f64) -> usize {
        (frames * f64::from(self.master_rate) / f64::from(RATE)).max(0.0) as usize
    }

    /// Drops the oldest `n` frames waiting.
    fn drop_frames(&mut self, n: usize) {
        let n = n.min(self.input.len());
        self.input.drain(..n);
    }

    /// The session to announce: `Some` while sending.
    pub fn announcement(&self) -> Option<&Session> {
        self.generation.map(|_| &self.session)
    }

    /// PTP time of the next packet's first frame while sending.
    pub fn next_ptp(&self) -> Option<i128> {
        self.generation.map(|_| self.next_ptp)
    }

    /// The tap-to-send delay as the drift last read it, 48 kHz frames.
    pub fn measured(&self) -> f64 {
        self.measured
    }

    /// Packets that had to be zeros for want of frames, after priming.
    pub fn padded(&self) -> u64 {
        self.padded
    }
}

/// A resampler from the master's rate to 48 kHz, its drift controller,
/// and the most master-rate frames waiting: a second.
fn path(master_rate: u32) -> (Adaptive, Drift, usize) {
    let adaptive = Adaptive::new(
        master_rate,
        RATE,
        2,
        FRAMES as u32,
        Quality::High,
        Format::I32,
    );
    let drift = Drift::new(TARGET, RATE, FRAMES as u32);
    (adaptive, drift, master_rate.max(RATE) as usize)
}

/// The default group: 239.69.x.y, x and y the last two bytes of the MAC;
/// a y of 0 or 255 becomes 1.
pub fn auto_group(mac: [u8; 6]) -> Ipv4Addr {
    let y = match mac[5] {
        0 | 255 => 1,
        y => y,
    };
    Ipv4Addr::new(239, 69, mac[4], y)
}

/// The MAC address of `interface`, from sysfs.
pub fn interface_mac(interface: &str) -> io::Result<[u8; 6]> {
    let text = std::fs::read_to_string(format!("/sys/class/net/{interface}/address"))?;
    parse_mac(&text).ok_or_else(|| {
        let msg = format!("{interface}: no MAC address in {:?}", text.trim());
        io::Error::new(io::ErrorKind::InvalidData, msg)
    })
}

fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.trim().split(':');
    for b in &mut mac {
        *b = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// The thread, its stop flag, its status and its taps. Dropping it stops
/// the thread, which withdraws the announcement.
pub struct OutputHandle {
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<OutputStatus>>,
    taps: mpsc::Sender<TapReader>,
}

impl OutputHandle {
    /// Gives the thread a tap, the first or one at a new rate.
    pub fn give_tap(&self, tap: TapReader) {
        // The thread only ends when the handle is dropped.
        let _ = self.taps.send(tap);
    }

    pub fn status(&self) -> OutputStatus {
        *lock(&self.status)
    }
}

impl Drop for OutputHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts the `aas-aes67-out` thread at [`rt::WRITER_PRIORITY`]. It sends
/// from `interface` to the configured group, or the one [`auto_group`]
/// makes of `mac`, while `clock` has PTP time, and keeps its session in
/// `announce` meanwhile. It sends silence until it has a tap.
pub fn start(
    config: OutputConfig,
    interface: Ipv4Addr,
    mac: [u8; 6],
    clock: PtpClock,
    announce: Arc<Mutex<Option<Session>>>,
) -> io::Result<OutputHandle> {
    let group = match config.group {
        Group::Auto => auto_group(mac),
        Group::Addr(addr) => addr,
    };
    let socket = UdpSocket::bind((interface, 0))?;
    socket.set_multicast_loop_v4(false)?;
    socket.set_multicast_ttl_v4(u32::from(config.ttl))?;
    multicast_interface(&socket, interface)?;
    socket.connect((group, config.port))?;
    let random = crate::airplay::raop::random::<10>();
    let session = Session {
        name: config.name,
        origin: interface,
        session_id: u64::from(u32::from_be_bytes([
            random[0], random[1], random[2], random[3],
        ])),
        version: 1,
        group,
        port: config.port,
        ttl: config.ttl,
        payload_type: PAYLOAD_TYPE,
        encoding: Encoding::L24,
        rate: RATE,
        channels: 2,
        ptime_us: 1000,
        refclk: None,
        mediaclk_offset: 0,
    };
    let ssrc = u32::from_be_bytes([random[4], random[5], random[6], random[7]]);
    let first_seq = u16::from_be_bytes([random[8], random[9]]);
    let sender = Sender::new(session, ssrc, first_seq, RATE);
    let status = Arc::new(Mutex::new(OutputStatus {
        enabled: true,
        sending: false,
        group,
        port: config.port,
        delay_us: None,
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let (taps, received) = mpsc::channel();
    let thread = {
        let status = status.clone();
        let stop = stop.clone();
        thread::Builder::new()
            .name("aas-aes67-out".into())
            .spawn(move || {
                run(
                    sender, &socket, &clock, &announce, &status, &stop, &received,
                );
                *lock(&announce) = None;
            })?
    };
    Ok(OutputHandle {
        thread: Some(thread),
        stop,
        status,
        taps,
    })
}

fn run(
    mut sender: Sender,
    socket: &UdpSocket,
    clock: &PtpClock,
    announce: &Mutex<Option<Session>>,
    status: &Mutex<OutputStatus>,
    stop: &AtomicBool,
    taps: &mpsc::Receiver<TapReader>,
) {
    match rt::set_fifo(rt::WRITER_PRIORITY) {
        Ok(()) => eprintln!("aas: aes67: output at SCHED_FIFO {}", rt::WRITER_PRIORITY),
        Err(e) => eprintln!("aas: warning: aes67: output not real time: {e}"),
    }
    let mut tap: Option<TapReader> = None;
    let mut frames = vec![[0i32; 2]; CHUNK];
    // The session version announced, `None` for nothing.
    let mut told: Option<u64> = None;
    let mut failing = false;
    let mut sleep_failed = false;
    let mut published = 0i128;
    while !stop.load(Ordering::Relaxed) {
        loop {
            match taps.try_recv() {
                Ok(new) => {
                    // With the eventfd, the thread is woken after every
                    // period and takes the frames as they come, so it knows
                    // when they came; without, it reads on its own timer.
                    let wake_at = if new.wake.is_some() { 0 } else { u64::MAX };
                    new.ring.header().wake_at.store(wake_at, Ordering::SeqCst);
                    sender.set_rate(new.rate);
                    tap = Some(new);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let local_ns = realtime_ns();
        if let Some(tap) = &tap {
            loop {
                let n = tap.ring.read(&mut frames);
                sender.take(&frames[..n], local_ns);
                if n < CHUNK {
                    break;
                }
            }
        }
        let (ptp, generation) = clock.snapshot();
        let now = Now {
            local_ns: ptp.map_or(local_ns, |p| p.local_ns),
            ptp,
            generation,
        };
        sender.step(&now, &mut |packet| match socket.send(packet) {
            Ok(_) => failing = false,
            Err(e) => {
                if !std::mem::replace(&mut failing, true) {
                    eprintln!("aas: aes67: output: could not send: {e}");
                }
            }
        });
        let version = sender.announcement().map(|s| s.version);
        if version != told {
            told = version;
            *lock(announce) = sender.announcement().cloned();
            lock(status).sending = version.is_some();
        }
        if (now.local_ns - published).abs() >= STATUS_NS {
            published = now.local_ns;
            let mut st = lock(status);
            st.delay_us =
                version.map(|_| (sender.measured() * 1e6 / f64::from(RATE)).round() as i64);
        }
        let idle = now.local_ns + IDLE_NS;
        let wake = match (sender.next_ptp(), ptp) {
            (Some(next), Some(p)) => {
                let local = p.local_ns + ((next - p.ptp_ns) as f64 / p.rate) as i128;
                local.min(idle)
            }
            _ => idle,
        };
        let woken = tap.as_ref().and_then(|t| t.wake.as_ref());
        if let Err(e) = wait_until(wake, woken) {
            // Never spin at real-time priority.
            if !std::mem::replace(&mut sleep_failed, true) {
                eprintln!("aas: aes67: output: cannot sleep until the next packet: {e}");
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// Sleeps until CLOCK_REALTIME reaches `ns`.
/// Waits until `ns` of the real-time clock, or until `woken` is written
/// first, and clears it.
fn wait_until(ns: i128, woken: Option<&OwnedFd>) -> io::Result<()> {
    let Some(fd) = woken else {
        return sleep_until(ns);
    };
    let left = (ns - realtime_ns()).max(0);
    let timeout = libc::timespec {
        tv_sec: left.div_euclid(1_000_000_000) as libc::time_t,
        tv_nsec: left.rem_euclid(1_000_000_000) as libc::c_long,
    };
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd and a valid timeout; no signal mask.
    let r = unsafe { libc::ppoll(&mut poll, 1, &timeout, std::ptr::null()) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(e)
        };
    }
    if r > 0 {
        let mut count = 0u64;
        // SAFETY: a read of 8 bytes into a u64; the fd is non-blocking.
        unsafe { libc::read(fd.as_raw_fd(), (&raw mut count).cast(), size_of::<u64>()) };
    }
    Ok(())
}

fn sleep_until(ns: i128) -> io::Result<()> {
    let ts = libc::timespec {
        tv_sec: ns.div_euclid(1_000_000_000) as libc::time_t,
        tv_nsec: ns.rem_euclid(1_000_000_000) as libc::c_long,
    };
    loop {
        // SAFETY: ts is a valid timespec; no remainder is asked for.
        let r = unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_REALTIME,
                libc::TIMER_ABSTIME,
                &ts,
                std::ptr::null_mut(),
            )
        };
        match r {
            0 => return Ok(()),
            libc::EINTR => {}
            e => return Err(io::Error::from_raw_os_error(e)),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aes67::ptp::ClockId;
    use crate::aes67::rtp::{self, ts_diff};
    use crate::aes67::sdp::Encoding;
    use assert_no_alloc::assert_no_alloc;

    const GM: ClockId = [0x00, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55];
    const OTHER: ClockId = [0x00, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x66];
    /// PTP time at the start, ns: an arbitrary timescale, off the
    /// millisecond grid.
    const P0: i128 = 123_456_789_000_300_000;
    /// Local time at the start, ns.
    const L0: i128 = 1_700_000_000_000_000_000;
    const MS: i128 = 1_000_000;
    /// The first whole millisecond of PTP time from the start.
    const G0: i128 = P0 + 700_000;
    /// The tap-to-send delay the path holds, 48 kHz frames.
    const HOLD: f64 = 960.0;

    fn session() -> Session {
        Session {
            name: "aas".into(),
            origin: Ipv4Addr::new(192, 0, 2, 20),
            session_id: 42,
            version: 1,
            group: Ipv4Addr::new(239, 69, 0, 1),
            port: 5004,
            ttl: 16,
            payload_type: 96,
            encoding: Encoding::L24,
            rate: 48000,
            channels: 2,
            ptime_us: 1000,
            refclk: None,
            mediaclk_offset: 0,
        }
    }

    fn sender(first_seq: u16) -> Sender {
        Sender::new(session(), 0x1234_5678, first_seq, 48000)
    }

    /// The world at `ptp_ns` of PTP time, `gm` the grandmaster.
    fn at(ptp_ns: i128, gm: ClockId, generation: u64) -> Now {
        let local_ns = ptp_ns - P0 + L0;
        Now {
            local_ns,
            ptp: Some(PtpNow {
                local_ns,
                ptp_ns,
                gm,
                domain: 0,
                rate: 1.0,
            }),
            generation,
        }
    }

    fn no_ptp(ptp_ns: i128) -> Now {
        Now {
            local_ns: ptp_ns - P0 + L0,
            ptp: None,
            generation: 0,
        }
    }

    /// The master's frames: `rate` frames per PTP second, `ppm` fast, in
    /// periods of `period` frames, a sine or zeros.
    struct Master {
        rate: u32,
        ppm: f64,
        period: usize,
        since: i128,
        given: u64,
        sine: bool,
        buf: Vec<[i32; 2]>,
    }

    impl Master {
        fn new(rate: u32, ppm: f64, period: usize, since: i128, sine: bool) -> Master {
            Master {
                rate,
                ppm,
                period,
                since,
                given: 0,
                sine,
                buf: vec![[0; 2]; period],
            }
        }

        /// Hands the sender every period made by `ptp_ns`.
        fn feed(&mut self, s: &mut Sender, ptp_ns: i128) {
            self.feed_with(s, ptp_ns, false);
        }

        /// Hands the sender every period made by `ptp_ns`, stamped with
        /// when it was made if `when_made`, else with `ptp_ns`.
        fn feed_with(&mut self, s: &mut Sender, ptp_ns: i128, when_made: bool) {
            let made = (ptp_ns - self.since) as f64
                * 1e-9
                * f64::from(self.rate)
                * (1.0 + self.ppm * 1e-6);
            while ((self.given + self.period as u64) as f64) <= made {
                for (i, f) in self.buf.iter_mut().enumerate() {
                    let n = (self.given + i as u64) as f64;
                    let t = n / f64::from(self.rate);
                    let v = if self.sine {
                        (0.25
                            * f64::from(i32::MAX)
                            * (2.0 * std::f64::consts::PI * 1000.0 * t).sin())
                            as i32
                    } else {
                        0
                    };
                    *f = [v, v];
                }
                let made_ns = self.since
                    + ((self.given + self.period as u64) as f64
                        / (f64::from(self.rate) * (1.0 + self.ppm * 1e-6))
                        * 1e9) as i128;
                let at = if when_made { made_ns } else { ptp_ns };
                s.take(&self.buf, at - P0 + L0);
                self.given += self.period as u64;
            }
        }
    }

    /// Steps from `from` to `to` every `every` ns of PTP time, feeding the
    /// master first, and collects the packets.
    fn run(
        s: &mut Sender,
        master: Option<&mut Master>,
        (from, to, every): (i128, i128, i128),
        gm: ClockId,
        generation: u64,
        out: &mut Vec<Vec<u8>>,
    ) {
        let mut master = master;
        let mut t = from;
        while t < to {
            if let Some(m) = master.as_deref_mut() {
                m.feed(s, t);
            }
            s.step(&at(t, gm, generation), &mut |p| out.push(p.to_vec()));
            t += every;
        }
    }

    fn header(p: &[u8]) -> (u16, u32, u32, u8) {
        let p = rtp::parse(p).unwrap();
        (p.seq, p.ts, p.ssrc, p.payload_type)
    }

    /// PTP time at the first whole millisecond from `t`.
    fn grid(t: i128) -> i128 {
        (t + MS - 1).div_euclid(MS) * MS
    }

    /// A tap that opens with a backlog hands it over at once, then a period
    /// at a time in real time. The backlog plays out: the frames that wait
    /// come back to the target, not the target plus the backlog.
    #[test]
    fn a_backlog_at_the_start_does_not_stay_in_the_delay() {
        let mut s = sender(0);
        // 50 ms of master frames made before the first step.
        let mut master = Master::new(48000, 0.0, 256, G0 - 50 * MS, true);
        let mut out = Vec::new();
        run(
            &mut s,
            Some(&mut master),
            (G0, G0 + 10_000 * MS, MS),
            GM,
            0,
            &mut out,
        );
        let waiting = s.input.len() as f64 + s.adaptive.delay_frames();
        assert!(
            (waiting - HOLD).abs() < 256.0 + 48.0,
            "frames waiting {waiting:.0}, want {HOLD} within a period"
        );
        assert!((s.measured() - HOLD).abs() < 48.0, "{}", s.measured());
    }

    /// A card 16 ppm off PTP slides its periods of 64 frames by 1/3 ms
    /// against the 1 ms packets every 21 s. Taken when they were made, the
    /// delay the controller reads does not step as they slide; taken on
    /// the packets' grid, it stepped by 16 frames each time, and the
    /// controller answered with a burst of -120 ppm.
    #[test]
    fn periods_taken_when_made_keep_the_delay_steady() {
        let mut s = sender(0);
        let mut master = Master::new(48000, 16.0, 64, P0, true);
        let mut t = P0 + 370_000;
        let (mut low, mut high) = (f64::MAX, f64::MIN);
        while t < P0 + 90_000 * MS {
            master.feed_with(&mut s, t, true);
            s.step(&at(t, GM, 1), &mut |_| {});
            if t >= P0 + 30_000 * MS {
                low = low.min(s.measured());
                high = high.max(s.measured());
            }
            t += MS;
        }
        assert!(high - low < 4.0, "the delay swung {low:.1}..{high:.1}");
        assert!((low - HOLD).abs() < 8.0, "{low:.1}");
        assert_eq!(s.padded(), 0);
    }

    #[test]
    fn a_write_to_the_tap_eventfd_ends_the_wait_and_is_cleared() {
        let fd = crate::control::eventfd().unwrap();
        let one = 1u64;
        // SAFETY: a write of 8 bytes from a u64.
        unsafe { libc::write(fd.as_raw_fd(), (&raw const one).cast(), 8) };
        let start = realtime_ns();
        wait_until(start + 1_000 * MS, Some(&fd)).unwrap();
        assert!(realtime_ns() - start < 100 * MS, "woken at once");
        let mut count = 0u64;
        // SAFETY: a read of 8 bytes into a u64; the fd is non-blocking.
        let r = unsafe { libc::read(fd.as_raw_fd(), (&raw mut count).cast(), 8) };
        assert_eq!(r, -1, "the wake-up was read");
        // Not written: the wait lasts until the time.
        let start = realtime_ns();
        wait_until(start + 5 * MS, Some(&fd)).unwrap();
        assert!(realtime_ns() - start >= 5 * MS);
    }

    #[test]
    fn auto_group_from_mac() {
        let mac = |y| [0x02, 0, 0, 0, 0x12, y];
        assert_eq!(auto_group(mac(0x34)), Ipv4Addr::new(239, 69, 18, 52));
        assert_eq!(auto_group(mac(0x00)), Ipv4Addr::new(239, 69, 18, 1));
        assert_eq!(auto_group(mac(0xff)), Ipv4Addr::new(239, 69, 18, 1));
    }

    #[test]
    fn a_mac_parses_from_sysfs_text() {
        let mac = parse_mac("02:00:00:00:00:01\n");
        assert_eq!(mac, Some([0x02, 0, 0, 0, 0, 0x01]));
        assert_eq!(parse_mac("02:00:00:00:00"), None);
        assert_eq!(parse_mac("02:00:00:00:00:01:02"), None);
        assert_eq!(parse_mac(""), None);
    }

    #[test]
    fn packets_every_ms_of_ptp_time_with_media_timestamps() {
        let mut s = sender(0xfffe);
        let mut out = Vec::new();
        let mut sent = 0;
        let first = grid(P0);
        assert_eq!(first, G0);
        // Steps a quarter of a millisecond apart: every packet goes out at
        // the step at or after its time, none before.
        let mut t = P0;
        while t < P0 + 100 * MS {
            s.step(&at(t, GM, 1), &mut |p| out.push(p.to_vec()));
            let due = if t < first { 0 } else { (t - first) / MS + 1 };
            assert_eq!(out.len() as i128, due, "at {}", t - P0);
            sent = out.len();
            t += MS / 4;
        }
        assert_eq!(sent, 100);
        for (k, p) in out.iter().enumerate() {
            let (seq, ts, ssrc, pt) = header(p);
            assert_eq!(ts, rtp::media_ts(first + k as i128 * MS, 0));
            assert_eq!(seq, 0xfffe_u16.wrapping_add(k as u16));
            assert_eq!((ssrc, pt), (0x1234_5678, 96));
            assert_eq!(p.len(), 12 + 48 * 6);
        }
        for w in out.windows(2) {
            assert_eq!(ts_diff(header(&w[1]).1, header(&w[0]).1), 48);
        }
        // Wrapped from 0xffff to 0.
        assert_eq!(header(&out[2]).0, 0);
    }

    #[test]
    fn no_ptp_no_packets_and_no_announcement() {
        let mut s = sender(0);
        let mut out = Vec::new();
        for i in 0..100 {
            s.step(&no_ptp(P0 + i * MS), &mut |p| out.push(p.to_vec()));
        }
        assert!(out.is_empty());
        assert!(s.announcement().is_none());
        run(&mut s, None, (G0, G0 + 10 * MS, MS), GM, 1, &mut out);
        assert_eq!(out.len(), 10);
        let announced = s.announcement().unwrap();
        assert_eq!(announced.refclk, Some((GM, 0)));
        assert_eq!(announced.payload_type, 96);
        // PTP time lost: nothing more, and the announcement goes.
        out.clear();
        for i in 10..100 {
            s.step(&no_ptp(P0 + i * MS), &mut |p| out.push(p.to_vec()));
        }
        assert!(out.is_empty());
        assert!(s.announcement().is_none());
    }

    #[test]
    fn a_new_grandmaster_reannounces() {
        let mut s = sender(0);
        let mut out = Vec::new();
        run(&mut s, None, (P0, P0 + 10 * MS, MS), GM, 1, &mut out);
        let before = s.announcement().unwrap().clone();
        assert_eq!(before.refclk, Some((GM, 0)));
        // Another grandmaster, with a timescale of its own.
        let p1 = 987_654_321_000_000_000;
        run(&mut s, None, (p1, p1 + 10 * MS, MS), OTHER, 2, &mut out);
        let after = s.announcement().unwrap().clone();
        assert_eq!(after.refclk, Some((OTHER, 0)));
        assert_eq!(after.version, before.version + 1);
        assert_eq!(after.session_id, before.session_id);
        // The timeline follows the new grandmaster's time.
        let (_, ts, _, _) = header(out.last().unwrap());
        assert_eq!(ts, rtp::media_ts(p1 + 9 * MS, 0));
        // The same grandmaster again in a new generation: no new version.
        run(
            &mut s,
            None,
            (p1 + 10 * MS, p1 + 20 * MS, MS),
            OTHER,
            3,
            &mut out,
        );
        assert_eq!(s.announcement().unwrap().version, after.version);
    }

    #[test]
    fn a_step_of_ptp_time_reanchors() {
        let mut s = sender(0);
        let mut out = Vec::new();
        run(&mut s, None, (P0, P0 + 10 * MS, MS), GM, 1, &mut out);
        // PTP time moves 2 s on, before the generation does.
        let p1 = G0 + 2_000_000_000 + 10 * MS;
        out.clear();
        run(&mut s, None, (p1, p1 + 3 * MS, MS), GM, 1, &mut out);
        assert_eq!(out.len(), 3, "not two seconds of packets at once");
        let (_, ts, _, _) = header(&out[0]);
        assert_eq!(ts, rtp::media_ts(grid(p1), 0));
    }

    #[test]
    fn silence_is_sent() {
        let mut s = sender(0);
        let mut out = Vec::new();
        let mut master = Master::new(48000, 0.0, 256, G0, false);
        run(
            &mut s,
            Some(&mut master),
            (G0, G0 + 2000 * MS, MS),
            GM,
            1,
            &mut out,
        );
        assert_eq!(out.len(), 2000);
        assert!(out.iter().all(|p| p[12..].iter().all(|&b| b == 0)));
        assert_eq!(s.padded(), 0);
        // And without any tap at all.
        let mut s = sender(0);
        out.clear();
        run(&mut s, None, (G0, G0 + 100 * MS, MS), GM, 1, &mut out);
        assert_eq!(out.len(), 100);
    }

    #[test]
    fn a_new_master_rate_keeps_the_timeline() {
        let mut s = sender(0);
        let mut out = Vec::new();
        let mut master = Master::new(48000, 0.0, 256, G0, true);
        let mid = G0 + 2000 * MS;
        run(&mut s, Some(&mut master), (G0, mid, MS), GM, 1, &mut out);
        // The new tap comes while the packet at `mid` waits to be sent, and
        // the steps go on off the grid: a timeline anchored again would
        // start at the next whole millisecond and skip that packet.
        s.set_rate(44100);
        let mut master = Master::new(44100, 0.0, 256, mid, true);
        let end = mid + 2000 * MS;
        run(
            &mut s,
            Some(&mut master),
            (mid + 370_000, end, MS),
            GM,
            1,
            &mut out,
        );
        assert_eq!(out.len(), 4000);
        for w in out.windows(2) {
            let (a, b) = (header(&w[0]), header(&w[1]));
            assert_eq!(ts_diff(b.1, a.1), 48);
            assert_eq!(b.0, a.0.wrapping_add(1));
        }
        // The new rate plays.
        assert!(out.last().unwrap()[12..].iter().any(|&b| b != 0));
        assert!((s.measured() - HOLD).abs() < 48.0, "{}", s.measured());
    }

    #[test]
    fn the_drift_follows_the_card_against_ptp() {
        // The master's period swings what waits by as much: up to 21 ms.
        for period in [256, 1024] {
            let mut s = sender(0);
            let mut master = Master::new(48000, 20.0, period, P0, true);
            // Steps off the packets' grid, as the thread wakes.
            let mut t = P0 + 370_000;
            let mut second = Vec::new();
            let mut means = Vec::new();
            while t < P0 + 40_000 * MS {
                master.feed(&mut s, t);
                s.step(&at(t, GM, 1), &mut |_| {});
                if t >= P0 + 30_000 * MS {
                    second.push(s.measured());
                    if second.len() == 1000 {
                        means.push(second.iter().sum::<f64>() / 1000.0);
                        second.clear();
                    }
                }
                t += MS;
            }
            assert_eq!(means.len(), 10);
            for mean in &means {
                assert!((mean - HOLD).abs() <= 12.0, "{period}: {means:?}");
            }
            assert_eq!(s.padded(), 0, "{period}");
        }
    }

    #[test]
    fn steady_state_does_not_allocate() {
        let mut s = sender(0);
        let mut out = Vec::new();
        let mut master = Master::new(48000, 0.0, 256, P0, true);
        run(
            &mut s,
            Some(&mut master),
            (P0, P0 + 1000 * MS, MS),
            GM,
            1,
            &mut out,
        );
        let frames = [[1; 2]; 48];
        let mut bytes = 0;
        assert_no_alloc(|| {
            for i in 0..1000 {
                let t = P0 + (1000 + i) * MS;
                s.take(&frames, t - P0 + L0);
                s.step(&at(t, GM, 1), &mut |p| bytes += p.len());
            }
        });
        assert_eq!(bytes, 1000 * 300);
    }
}
