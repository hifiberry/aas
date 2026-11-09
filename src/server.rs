//! The daemon's socket and threads. The control thread runs here, polling
//! the listener, the clients, the audio thread's events and a 50 ms timer.

use crate::board;
use crate::config::{Config, Format};
use crate::control::{Answer, ConnId, Control, eventfd};
use crate::counters::Counters;
use crate::engine::Engine;
use crate::format::{F32, F64, I32, Processing};
use crate::rt;
use crate::sink::{FallbackSink, RatesWatch, Sink};
use crate::volume::{HardwareVolume, OutputVolume};
use aas_client::client::socket_address;
use aas_client::proto::{self, REQUEST_LEN, Reply};
use rtrb::RingBuffer;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// How often the control thread looks at the clock with nothing else to do.
const TICK_MS: i32 = 50;
/// How long the audio thread waits for a command while the output is closed.
const PARK: Duration = Duration::from_millis(100);
const COMMAND_SLOTS: usize = 4096;
const EVENT_SLOTS: usize = 1024;

fn check(rc: isize) -> io::Result<usize> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc as usize)
    }
}

/// Runs the daemon until `shutdown` is set. The sink is wrapped in a
/// `FallbackSink`, so a missing card does not stop playback. The output
/// volume is kept in the file `state`, and set on `hardware` if given, else
/// in software.
pub fn run(
    config: Config,
    sink: Box<dyn Sink>,
    socket: &Path,
    state: &Path,
    hardware: Option<Box<dyn HardwareVolume>>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), String> {
    raise_file_limit();
    let latency_ms = config
        .latency_ms
        .unwrap_or_else(|| board::detect().default_latency_ms());
    let listener = listen(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let notify = eventfd().map_err(|e| format!("eventfd: {e}"))?;
    let supported = sink.supported_rates();
    let mut sink = FallbackSink::new(sink);
    let rates = sink.rates_watch();
    let (commands, command_queue) = RingBuffer::new(COMMAND_SLOTS);
    let (event_queue, events) = RingBuffer::new(EVENT_SLOTS);
    let counters = Arc::new(Counters::default());
    let engine_counters = counters.clone();
    let mode = config.bit_perfect;
    let wake = notify.as_raw_fd();
    let spawned = thread::Builder::new().name("aas-audio".into());
    let audio = match config.format {
        Format::I32 => {
            let engine =
                Engine::<I32, _>::new(sink, command_queue, event_queue, engine_counters, mode);
            spawned.spawn(move || audio_loop(engine, wake))
        }
        Format::F32 => {
            let engine =
                Engine::<F32, _>::new(sink, command_queue, event_queue, engine_counters, mode);
            spawned.spawn(move || audio_loop(engine, wake))
        }
        Format::F64 => {
            let engine =
                Engine::<F64, _>::new(sink, command_queue, event_queue, engine_counters, mode);
            spawned.spawn(move || audio_loop(engine, wake))
        }
    }
    .map_err(|e| format!("audio thread: {e}"))?;
    let mut control = Control::new(
        &config,
        latency_ms,
        supported,
        commands,
        events,
        audio.thread().clone(),
    );
    let volume = OutputVolume::new(hardware, config.bit_perfect);
    control.restore_volume(volume, state.to_path_buf());
    let mut server = Server {
        listener,
        notify,
        control,
        conns: BTreeMap::new(),
        notices: BTreeMap::new(),
        next_conn: 1,
        rates,
    };
    server.control.start(Instant::now());
    while !shutdown.load(Ordering::Relaxed) {
        let was_open = server.control.output_is_open();
        server.step();
        if was_open && !server.control.output_is_open() {
            eprintln!("aas: output closed: {counters}");
        }
    }
    server.control.shutdown();
    let joined = audio.join();
    eprintln!("aas: stopped: {counters}");
    // The fds of `notify` must outlive the audio thread.
    drop(server);
    let _ = fs::remove_file(socket);
    joined.map_err(|_| "the audio thread panicked".to_string())
}

/// The audio thread: one cycle after another, parked while the output is
/// closed. Wakes the control thread when it has events to read.
fn audio_loop<P: Processing, S: Sink>(mut engine: Engine<P, S>, notify: RawFd) {
    match rt::set_fifo(rt::AUDIO_PRIORITY) {
        Ok(()) => eprintln!("aas: audio thread at SCHED_FIFO {}", rt::AUDIO_PRIORITY),
        Err(e) => eprintln!("aas: warning: audio thread not real time: {e}"),
    }
    while !engine.is_stopped() {
        // An error from a FallbackSink is an xrun, already counted.
        let _ = engine.cycle();
        if engine.has_unread_events() {
            let one = 1u64;
            // SAFETY: a plain write of 8 bytes to a non-blocking eventfd.
            unsafe { libc::write(notify, (&raw const one).cast(), size_of::<u64>()) };
        }
        if engine.is_closed() && !engine.is_stopped() {
            thread::park_timeout(PARK);
        }
    }
}

struct Server {
    listener: OwnedFd,
    notify: OwnedFd,
    control: Control,
    conns: BTreeMap<ConnId, OwnedFd>,
    /// The latest volume notification each subscriber has waiting.
    notices: BTreeMap<ConnId, Reply>,
    next_conn: ConnId,
    /// The card's rates, when they change.
    rates: RatesWatch,
}

impl Server {
    /// Waits for something to do, or the timer, and does it.
    fn step(&mut self) {
        let poll_in = |fd: RawFd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut fds = vec![
            poll_in(self.listener.as_raw_fd()),
            poll_in(self.notify.as_raw_fd()),
        ];
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        fds.extend(self.conns.values().map(|fd| poll_in(fd.as_raw_fd())));
        // SAFETY: fds is a valid array of its length.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, TICK_MS) };
        if rc < 0 {
            // Only EINTR is expected; the next step polls again.
            return;
        }
        let now = Instant::now();
        if let Some(rates) = self.rates.take() {
            self.control.set_supported(rates, now);
        }
        if fds[1].revents != 0 {
            let mut count = 0u64;
            // SAFETY: reads 8 bytes into a u64; non-blocking.
            unsafe { libc::read(fds[1].fd, (&raw mut count).cast(), size_of::<u64>()) };
        }
        self.control.handle_events(now);
        if fds[0].revents != 0 {
            self.accept_all();
        }
        for (conn, pfd) in ids.into_iter().zip(&fds[2..]) {
            if pfd.revents != 0 {
                self.serve(conn, now);
            }
        }
        self.control.tick(now);
        for (conn, reply) in self.control.take_replies() {
            self.answer(conn, Answer::from(reply), now);
        }
        self.notify();
    }

    /// Sends the volume notifications. Each subscriber has at most one
    /// waiting, the latest, and it is sent only once the subscriber has
    /// read everything sent to it before. So notifications never fill its
    /// socket, its replies always fit, and a subscriber that has not read
    /// for a while still gets the current volume. What cannot be sent now
    /// is tried again at the next step, within the tick.
    fn notify(&mut self) {
        self.notices.extend(self.control.take_notices());
        self.notices.retain(|conn, notice| {
            let Some(fd) = self.conns.get(conn) else {
                return false;
            };
            if unread_bytes(fd) != Some(0) {
                return true;
            }
            // A connection that has gone is closed by the next poll.
            let _ = send_reply(fd, notice, &[]);
            false
        });
    }

    fn accept_all(&mut self) {
        loop {
            // SAFETY: plain syscall; the descriptor is owned at once.
            let raw = unsafe {
                libc::accept4(
                    self.listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                )
            };
            if raw < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::WouldBlock {
                    eprintln!("aas: accept: {e}");
                }
                return;
            }
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let conn = self.next_conn;
            self.next_conn += 1;
            let name = peer_name(&fd);
            self.conns.insert(conn, fd);
            self.control.connected(conn, name);
        }
    }

    /// Reads every message waiting on a connection.
    fn serve(&mut self, conn: ConnId, now: Instant) {
        loop {
            let Some(fd) = self.conns.get(&conn) else {
                return;
            };
            let mut buf = [0u8; 64];
            // SAFETY: the buffer is valid for its length.
            let rc = unsafe {
                libc::recv(
                    fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            let n = match check(rc) {
                Ok(0) => return self.close(conn, now),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return self.close(conn, now),
            };
            let request = (n == REQUEST_LEN)
                .then(|| proto::decode_request(&buf[..n]))
                .flatten();
            let answer = self.control.request(conn, request, now);
            self.answer(conn, answer, now);
            self.notify();
        }
    }

    fn answer(&mut self, conn: ConnId, answer: Answer, now: Instant) {
        let Some(fd) = self.conns.get(&conn) else {
            return;
        };
        let sent = match answer.reply {
            Some(reply) => send_reply(fd, &reply, &answer.fds).is_ok(),
            None => true,
        };
        if !sent || answer.close {
            self.close(conn, now);
        }
    }

    fn close(&mut self, conn: ConnId, now: Instant) {
        self.notices.remove(&conn);
        if self.conns.remove(&conn).is_some() {
            self.control.disconnected(conn, now);
        }
    }
}

impl From<Reply> for Answer {
    fn from(reply: Reply) -> Answer {
        Answer {
            reply: Some(reply),
            fds: Vec::new(),
            close: false,
        }
    }
}

/// Sends a reply, with descriptors by `SCM_RIGHTS`.
fn send_reply(fd: &OwnedFd, reply: &Reply, fds: &[OwnedFd]) -> io::Result<()> {
    let data = proto::encode_reply(reply);
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let mut control = [0u64; 8];
    // SAFETY: an all-zero msghdr is valid; the pointers set below live
    // until sendmsg returns, and the control buffer is large enough for
    // the two descriptors a reply carries at most.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let bytes = size_of_val(fds) as u32;
        let space = unsafe { libc::CMSG_SPACE(bytes) } as usize;
        assert!(space <= size_of_val(&control));
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(bytes) as _;
            let out = libc::CMSG_DATA(cmsg) as *mut RawFd;
            for (i, fd) in fds.iter().enumerate() {
                out.add(i).write_unaligned(fd.as_raw_fd());
            }
        }
    }
    loop {
        let flags = libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT;
        let rc = unsafe { libc::sendmsg(fd.as_raw_fd(), &msg, flags) };
        match check(rc) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other.map(drop),
        }
    }
}

/// Bytes sent on a connection that the peer has not read yet.
fn unread_bytes(fd: &OwnedFd) -> Option<usize> {
    let mut queued: libc::c_int = 0;
    // SAFETY: TIOCOUTQ, which is SIOCOUTQ on a socket, writes one int.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCOUTQ, &mut queued) };
    (rc == 0).then_some(queued as usize)
}

/// The client's process name, from its credentials, for the log.
fn peer_name(fd: &OwnedFd) -> String {
    // SAFETY: an all-zero ucred is valid, and getsockopt fills at most len.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if rc < 0 {
        return "a client".into();
    }
    let comm = fs::read_to_string(format!("/proc/{}/comm", cred.pid));
    let name = comm.map(|c| c.trim_end().to_string()).unwrap_or_default();
    format!("{name} ({})", cred.pid)
}

/// A listening seqpacket socket at `path`, readable and writable by its
/// owner and group. The group is `audio` where the process may set it.
///
/// The socket is made under another name and renamed into place once it
/// listens, so that no client finds the path before it can connect.
fn listen(path: &Path) -> io::Result<OwnedFd> {
    let mut building = path.as_os_str().to_owned();
    building.push(format!(".{}", std::process::id()));
    let building = Path::new(&building);
    let (addr, len) = socket_address(building)?;
    let _ = fs::remove_file(building);
    let flags = libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    // SAFETY: plain syscalls; the descriptor is owned at once.
    let raw = unsafe { libc::socket(libc::AF_UNIX, flags, 0) };
    check(raw as isize)?;
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    check(unsafe { libc::bind(fd.as_raw_fd(), (&raw const addr).cast(), len) } as isize)?;
    let ready = (|| {
        fs::set_permissions(building, fs::Permissions::from_mode(0o660))?;
        set_audio_group(building);
        check(unsafe { libc::listen(fd.as_raw_fd(), libc::SOMAXCONN) } as isize)?;
        // Replaces a socket left by a daemon that died.
        fs::rename(building, path)
    })();
    if let Err(e) = ready {
        let _ = fs::remove_file(building);
        return Err(e);
    }
    Ok(fd)
}

fn set_audio_group(path: &Path) {
    let Some(gid) = group_id("audio") else {
        eprintln!("aas: no group audio, the socket keeps its group");
        return;
    };
    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: a valid C string; uid -1 leaves the owner alone.
    if unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) } < 0 {
        let e = io::Error::last_os_error();
        eprintln!("aas: could not give the socket to group audio: {e}");
    }
}

fn group_id(name: &str) -> Option<libc::gid_t> {
    let c_name = CString::new(name).ok()?;
    // SAFETY: getgrnam_r writes into the buffers given, with their sizes.
    let mut group: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16384];
    let mut found: *mut libc::group = std::ptr::null_mut();
    let rc = unsafe {
        libc::getgrnam_r(
            c_name.as_ptr(),
            &mut group,
            buf.as_mut_ptr(),
            buf.len(),
            &mut found,
        )
    };
    (rc == 0 && !found.is_null()).then_some(group.gr_gid)
}

/// Lets the daemon hold as many descriptors as it is allowed, so that no
/// client is refused for want of one.
fn raise_file_limit() {
    // SAFETY: plain syscalls on a local struct.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0
        && limit.rlim_cur < limit.rlim_max
    {
        limit.rlim_cur = limit.rlim_max;
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
    }
}
