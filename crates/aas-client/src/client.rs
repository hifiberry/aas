//! A client's side of the daemon: the socket, the ring and the wake-up.
//!
//! The socket carries setup and teardown only. Audio goes through the ring.
//! Every call that talks to the daemon waits for its reply, for at most
//! `TIMEOUT`. A reply that does not come in time would answer the next
//! request instead, so the connection is lost then: every later call fails
//! as for a daemon that has gone.

use crate::convert::{ClientFormat, to_stereo_s32};
use crate::proto::{self, Reply, Request};
use crate::ring::Ring;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{Ordering, fence};
use std::time::{Duration, Instant};

/// Descriptors a reply may carry: the ring and the wake-up.
const MAX_FDS: usize = 2;
/// Frames converted per step of `write`. Kept on the stack.
const CHUNK: usize = 512;
/// How long a request waits for the daemon's answer. Long enough for a
/// daemon that is only slow: a reset waits for the audio thread, which may
/// itself wait half a second for the card, or be opening it again. A drain
/// waits as long as the audio takes to play.
const TIMEOUT: Duration = Duration::from_secs(5);
/// How long after the buffer has played the daemon answers a drain at the
/// latest. The same as the daemon's own spare.
const DRAIN_SPARE: Duration = Duration::from_millis(500);

/// The address of a Unix socket at `path`, and its length.
pub fn socket_address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: an all-zero sockaddr_un is valid.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    // One byte stays zero as the terminator.
    if bytes.is_empty() || bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    let len = size_of::<libc::sa_family_t>() + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

fn check(rc: isize) -> io::Result<usize> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc as usize)
    }
}

fn not_configured() -> io::Error {
    io::Error::from_raw_os_error(libc::EBADFD)
}

fn daemon_gone() -> io::Error {
    io::Error::from_raw_os_error(libc::ECONNRESET)
}

fn timed_out() -> io::Error {
    io::Error::from_raw_os_error(libc::ETIMEDOUT)
}

/// A call that does not fit the direction of the stream.
fn wrong_direction() -> io::Error {
    io::Error::from_raw_os_error(libc::EBADFD)
}

struct Stream {
    ring: Ring,
    wake: OwnedFd,
    format: ClientFormat,
    channels: u8,
    /// How long the ring takes to play when it is full.
    holds: Duration,
    /// A capture stream: the daemon writes the ring and this side reads it.
    capture: bool,
}

/// A connection to the daemon, and the stream it has configured.
pub struct Client {
    socket: OwnedFd,
    stream: Option<Stream>,
    /// A drain sent by `start_drain` whose reply has not been read.
    drain_pending: bool,
    /// When the daemon has answered that drain at the latest: after the
    /// ring has played, the daemon's spare and a request's wait. A daemon
    /// that is stopped but has not hung up answers nothing, and without
    /// this a player would wait in its drain for good.
    drain_deadline: Option<Instant>,
    /// How long a request waits for its answer.
    timeout: Duration,
    /// A request failed on the socket, so replies are out of step.
    lost: bool,
}

impl Client {
    /// Connects and checks that the daemon speaks this protocol version.
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Client> {
        Client::connect_with(path.as_ref(), TIMEOUT)
    }

    /// Connects. Requests wait at most `timeout` for their reply.
    fn connect_with(path: &Path, timeout: Duration) -> io::Result<Client> {
        let (addr, len) = socket_address(path)?;
        // SAFETY: plain syscalls; the descriptor is owned at once.
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        check(raw as isize)?;
        let socket = unsafe { OwnedFd::from_raw_fd(raw) };
        let wait = libc::timeval {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_usec: timeout.subsec_micros() as libc::suseconds_t,
        };
        for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
            // SAFETY: the option value is a timeval of the size given.
            let rc = unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&raw const wait).cast(),
                    size_of::<libc::timeval>() as libc::socklen_t,
                )
            };
            check(rc as isize)?;
        }
        let rc = unsafe { libc::connect(socket.as_raw_fd(), (&raw const addr).cast(), len) };
        check(rc as isize)?;
        let mut client = Client {
            socket,
            stream: None,
            drain_pending: false,
            drain_deadline: None,
            timeout,
            lost: false,
        };
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        match client.call(&hello)?.0 {
            Reply::Hello { version } if version == proto::VERSION => Ok(client),
            Reply::Hello { version } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "the daemon speaks protocol {version}, this client {}",
                    proto::VERSION
                ),
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected reply to hello",
            )),
        }
    }

    fn send(&self, request: &Request) -> io::Result<()> {
        let message = proto::encode_request(request);
        loop {
            // SAFETY: the buffer is valid for its length.
            let rc = unsafe {
                libc::send(
                    self.socket.as_raw_fd(),
                    message.as_ptr().cast(),
                    message.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            match check(rc) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Err(timed_out()),
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => return Err(daemon_gone()),
                Err(e) => return Err(e),
                Ok(_) => return Ok(()),
            }
        }
    }

    /// Waits for one reply and the descriptors that came with it, for as
    /// long as the socket's timeout.
    fn receive(&self) -> io::Result<(Reply, Vec<OwnedFd>)> {
        self.receive_flags(0)?.ok_or_else(timed_out)
    }

    /// Reads one reply. It is `None` if none is there: at once with
    /// `MSG_DONTWAIT`, else after the socket's timeout.
    fn receive_flags(&self, flags: libc::c_int) -> io::Result<Option<(Reply, Vec<OwnedFd>)>> {
        let mut data = [0u8; 64];
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr().cast(),
            iov_len: data.len(),
        };
        // Room for the descriptors, aligned as cmsghdr wants.
        let mut control = [0u64; 8];
        // SAFETY: an all-zero msghdr is valid; the pointers set below live
        // until recvmsg returns.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = size_of_val(&control) as _;
        let n = loop {
            let flags = libc::MSG_CMSG_CLOEXEC | flags;
            let rc = unsafe { libc::recvmsg(self.socket.as_raw_fd(), &mut msg, flags) };
            match check(rc) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) => return Err(e),
                Ok(0) => return Err(daemon_gone()),
                Ok(n) => break n,
            }
        };
        let fds = Self::descriptors(&msg);
        let reply = proto::decode_reply(&data[..n])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad reply"))?;
        Ok(Some((reply, fds)))
    }

    fn descriptors(msg: &libc::msghdr) -> Vec<OwnedFd> {
        let mut fds = Vec::new();
        // SAFETY: walks the control buffer recvmsg filled in, as CMSG_*
        // intends. Each descriptor received is owned from here on.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(cmsg) as *const RawFd;
                    let bytes = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    for i in 0..bytes / size_of::<RawFd>() {
                        fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(msg, cmsg);
            }
        }
        fds
    }

    /// Sends a request and waits for its reply. `Reply::Error` becomes the
    /// error it names. A drain still waiting is answered first, finished or
    /// cancelled by this request, and its reply is set aside. A failure of
    /// the socket, a reply that does not come in time among them, loses the
    /// connection.
    fn call(&mut self, request: &Request) -> io::Result<(Reply, Vec<OwnedFd>)> {
        if self.lost {
            return Err(daemon_gone());
        }
        let answer = self.exchange(request);
        self.lost = answer.is_err();
        match answer? {
            (Reply::Error { code }, _) => Err(io::Error::from_raw_os_error(code as i32)),
            answer => Ok(answer),
        }
    }

    fn exchange(&mut self, request: &Request) -> io::Result<(Reply, Vec<OwnedFd>)> {
        self.send(request)?;
        if self.drain_pending {
            self.receive()?;
            self.drain_done();
        }
        self.receive()
    }

    fn stream(&self) -> io::Result<&Stream> {
        self.stream.as_ref().ok_or_else(not_configured)
    }

    /// Sets up a stream. Replaces any stream set up before, which must be
    /// started again. `buffer_frames` is what the ring holds at least.
    pub fn configure(
        &mut self,
        rate: u32,
        format: ClientFormat,
        channels: u8,
        buffer_frames: u32,
    ) -> io::Result<()> {
        self.set_up(rate, format, channels, buffer_frames, false)
            .map(drop)
    }

    /// Sets up a capture stream, which records the daemon's analog input:
    /// S32 stereo at the rate this returns, the rate the daemon's output
    /// runs at. Replaces any stream set up before. `buffer_frames` is what
    /// the ring holds at least; the daemon rounds it up as it needs.
    ///
    /// The stream carries the analog input after its plugins and before
    /// the input volume, which belongs to the mix. While the daemon's
    /// output is closed a running stream gets neither frames nor wake-ups;
    /// with the analog input enabled the output never closes. A ring that
    /// is full drops what does not fit, and `underruns` counts the frames
    /// dropped.
    ///
    /// The stream ends when the daemon's rate changes: `ended` turns true
    /// once nothing more is written, and the wake-up is written then. What
    /// the ring holds can still be read, and `start` fails with `ENODEV`. A new `configure_capture`
    /// gets the new rate.
    pub fn configure_capture(&mut self, buffer_frames: u32) -> io::Result<u32> {
        self.set_up(0, ClientFormat::S32, 2, buffer_frames, true)
    }

    /// Sends a configure and maps the ring it is answered with. Returns the
    /// stream's rate: the one asked for, or for a capture stream the one
    /// the reply names.
    fn set_up(
        &mut self,
        rate: u32,
        format: ClientFormat,
        channels: u8,
        buffer_frames: u32,
        capture: bool,
    ) -> io::Result<u32> {
        self.stream = None;
        let request = Request::Configure {
            rate,
            format,
            channels,
            buffer_frames,
            capture,
        };
        let (reply, fds) = self.call(&request)?;
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
        let Reply::Configured { rate: told } = reply else {
            return Err(bad("unexpected reply to configure"));
        };
        let rate = if capture { told } else { rate };
        if capture && rate == 0 {
            return Err(bad("configure named no rate"));
        }
        let [memfd, wake]: [OwnedFd; MAX_FDS] = fds
            .try_into()
            .map_err(|_| bad("configure came without the ring"))?;
        let ring = Ring::map(memfd.as_fd())?;
        let holds = Duration::from_secs_f64(ring.capacity() as f64 / f64::from(rate.max(1)));
        self.stream = Some(Stream {
            ring,
            wake,
            format,
            channels,
            holds,
            capture,
        });
        Ok(rate)
    }

    /// Gives the stream up on this side: the ring is unmapped and the
    /// wake-up closed. The daemon keeps its side, stopped or not, until
    /// the next `configure` replaces it or the connection closes.
    pub fn release(&mut self) {
        self.stream = None;
    }

    /// Starts playing what is in the ring and what is written after. A
    /// capture stream starts recording.
    pub fn start(&mut self) -> io::Result<()> {
        let stream = self.stream()?;
        stream.ring.header().draining.store(0, Ordering::Release);
        self.call(&Request::Start).map(drop)
    }

    /// Stops playing. What is in the ring stays there.
    pub fn stop(&mut self) -> io::Result<()> {
        self.stream()?;
        self.call(&Request::Stop)?;
        let stream = self.stream()?;
        stream.ring.header().draining.store(0, Ordering::Release);
        Ok(())
    }

    /// Plays everything written so far and returns once it has reached the
    /// output. Starts the stream if it was not running. The stream is
    /// stopped afterwards.
    pub fn drain(&mut self) -> io::Result<()> {
        self.start_drain()?;
        // The audio may take longer to play than a request may take, so
        // this waits again after each timeout, up to the drain's deadline.
        let reply = loop {
            match self.receive_flags(0) {
                Ok(None) if self.drain_overdue() => break Err(daemon_gone()),
                Ok(None) => continue,
                Ok(Some(reply)) => break Ok(reply),
                Err(e) => break Err(e),
            }
        };
        self.drain_done();
        self.lost |= reply.is_err();
        match reply? {
            (Reply::Error { code }, _) => Err(io::Error::from_raw_os_error(code as i32)),
            _ => Ok(()),
        }
    }

    /// Whether the drain under way should have been answered by now.
    fn drain_overdue(&self) -> bool {
        self.drain_deadline.is_some_and(|at| Instant::now() >= at)
    }

    /// Starts a drain without waiting for it. `poll_drain` tells when it is
    /// done. Does nothing if one is under way already.
    pub fn start_drain(&mut self) -> io::Result<()> {
        // First: on a lost connection a drain still counts as under way,
        // and whatever comes on the socket is not its reply.
        if self.lost {
            return Err(daemon_gone());
        }
        if self.drain_pending {
            return Ok(());
        }
        let stream = self.stream()?;
        let deadline = Instant::now() + stream.holds + DRAIN_SPARE + self.timeout;
        // After the last frames, which the daemon relies on.
        stream.ring.header().draining.store(1, Ordering::Release);
        if let Err(e) = self.send(&Request::Drain) {
            self.drain_done();
            self.lost = true;
            return Err(e);
        }
        self.drain_pending = true;
        self.drain_deadline = Some(deadline);
        Ok(())
    }

    /// Whether the drain `start_drain` began has finished. Never waits.
    /// True also when no drain is under way. A drain the daemon has not
    /// answered by its deadline loses the connection.
    pub fn poll_drain(&mut self) -> io::Result<bool> {
        if self.lost {
            return Err(daemon_gone());
        }
        if !self.drain_pending {
            return Ok(true);
        }
        let reply = match self.receive_flags(libc::MSG_DONTWAIT) {
            Ok(None) if self.drain_overdue() => Err(daemon_gone()),
            Ok(None) => return Ok(false),
            other => other,
        };
        self.drain_done();
        self.lost |= reply.is_err();
        match reply?.map(|(reply, _)| reply) {
            Some(Reply::Error { code }) => Err(io::Error::from_raw_os_error(code as i32)),
            _ => Ok(true),
        }
    }

    fn drain_done(&mut self) {
        self.drain_pending = false;
        self.drain_deadline = None;
        if let Some(stream) = &self.stream {
            stream.ring.header().draining.store(0, Ordering::Release);
        }
    }

    /// Converts and writes as many whole frames as the ring has room for.
    /// Returns the number of frames written. Fails on a capture stream.
    pub fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let stream = self.stream()?;
        if stream.capture {
            return Err(wrong_direction());
        }
        let frame_bytes = stream.format.sample_bytes() * usize::from(stream.channels);
        if frame_bytes == 0 {
            return Ok(0);
        }
        let mut frames = [[0i32; 2]; CHUNK];
        let mut written = 0;
        for chunk in input.chunks(frame_bytes * CHUNK) {
            let room = stream.ring.free().min(CHUNK);
            let n = to_stereo_s32(stream.format, stream.channels, chunk, &mut frames[..room]);
            let done = stream.ring.write(&frames[..n]);
            written += done;
            if done < chunk.len() / frame_bytes {
                break;
            }
        }
        Ok(written)
    }

    /// Reads as many frames of a capture stream as the ring holds, up to
    /// `out.len()`. Returns the number read. Fails on a playback stream.
    pub fn read(&mut self, out: &mut [[i32; 2]]) -> io::Result<usize> {
        let stream = self.stream()?;
        if !stream.capture {
            return Err(wrong_direction());
        }
        Ok(stream.ring.read(out))
    }

    /// Whether the daemon has ended the stream. It serves it no more, and
    /// it has to be configured again. False without a stream.
    pub fn ended(&self) -> bool {
        self.stream
            .as_ref()
            .is_some_and(|s| s.ring.header().ended.load(Ordering::Acquire) != 0)
    }

    /// Stops the stream and empties its ring, keeping the ring and the
    /// stream. Positions go on counting. Returns once the ring is empty.
    ///
    /// A capture stream is emptied from this side, the reader's, once the
    /// daemon has stopped writing it.
    pub fn reset(&mut self) -> io::Result<()> {
        self.stream()?;
        self.call(&Request::Reset)?;
        let stream = self.stream()?;
        if stream.capture {
            stream.ring.consume(usize::MAX);
        }
        stream.ring.header().draining.store(0, Ordering::Release);
        Ok(())
    }

    /// The daemon's position in the ring, the one `wake_at` waits for:
    /// frames it has taken from a playback ring, or put into a capture
    /// ring, since the stream was configured. It reads shared memory only,
    /// so it does not notice a daemon that has gone: `check_alive` and the
    /// socket in `poll_fds` do.
    pub fn hw_position(&self) -> io::Result<u64> {
        let stream = self.stream()?;
        let header = stream.ring.header();
        let position = if stream.capture {
            &header.write_pos
        } else {
            &header.read_pos
        };
        Ok(position.load(Ordering::Acquire))
    }

    /// Asks for one wake-up once the daemon's position, as `hw_position`
    /// gives it, has reached `position`, in place of one after every
    /// period. For a playback stream that is frames taken from the ring,
    /// for a capture stream frames put into it. Returns true if the
    /// position is reached already, in which case no wake-up may come. A
    /// position of zero brings back the wake-up after every period.
    pub fn wake_at(&self, position: u64) -> io::Result<bool> {
        let stream = self.stream()?;
        let header = stream.ring.header();
        header.wake_at.store(position, Ordering::SeqCst);
        // The daemon stores its position and then loads this one. With a
        // full barrier on both sides, one of the two sees the other.
        fence(Ordering::SeqCst);
        let daemon = if stream.capture {
            &header.write_pos
        } else {
            &header.read_pos
        };
        Ok(daemon.load(Ordering::SeqCst) >= position)
    }

    /// Frames between what is written next and the listener: what the ring
    /// holds and what plays after it. For a capture stream, what the ring
    /// holds to be read and what the analog input's plugins hold back.
    pub fn delay_frames(&self) -> u64 {
        let Some(stream) = &self.stream else {
            return 0;
        };
        let after = stream
            .ring
            .header()
            .output_delay_frames
            .load(Ordering::Relaxed);
        stream.ring.available() as u64 + u64::from(after)
    }

    /// Times the daemon found the ring short while the stream was running.
    /// For a capture stream: frames dropped because the ring was full.
    pub fn underruns(&self) -> u64 {
        self.stream
            .as_ref()
            .map_or(0, |s| s.ring.header().underruns.load(Ordering::Relaxed))
    }

    /// The wake-up eventfd, readable after each period played or as
    /// `wake_at` asks, and after each underrun, and the
    /// socket, which hangs up when the daemon goes. The first is -1 until
    /// the stream is configured.
    pub fn poll_fds(&self) -> [RawFd; 2] {
        let wake = self.stream.as_ref().map_or(-1, |s| s.wake.as_raw_fd());
        [wake, self.socket.as_raw_fd()]
    }

    /// Fails if the daemon has closed the connection, or the connection
    /// is lost.
    pub fn check_alive(&self) -> io::Result<()> {
        if self.lost {
            return Err(daemon_gone());
        }
        let mut pfd = libc::pollfd {
            fd: self.socket.as_raw_fd(),
            events: libc::POLLIN | libc::POLLRDHUP,
            revents: 0,
        };
        // SAFETY: one valid pollfd, no waiting.
        let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
        check(rc as isize)?;
        if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLRDHUP) != 0 {
            return Err(daemon_gone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU32;
    use std::thread::JoinHandle;

    const SHORT: Duration = Duration::from_millis(100);

    /// A daemon played by `script`, which gets the one connection it
    /// accepts. Returns its socket and the thread to join.
    fn fake_daemon(script: impl FnOnce(OwnedFd) + Send + 'static) -> (PathBuf, JoinHandle<()>) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("aas-client-test-{}-{n}.sock", std::process::id());
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        let (addr, len) = socket_address(&path).unwrap();
        // SAFETY: plain syscalls; the descriptor is owned at once.
        let listener = unsafe {
            let raw = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
            assert!(raw >= 0);
            let listener = OwnedFd::from_raw_fd(raw);
            assert_eq!(libc::bind(raw, (&raw const addr).cast(), len), 0);
            assert_eq!(libc::listen(raw, 1), 0);
            listener
        };
        let unlink = path.clone();
        let thread = std::thread::spawn(move || {
            // SAFETY: accept on the listener; the descriptor is owned at once.
            let conn = unsafe {
                let raw = libc::accept(
                    listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                assert!(raw >= 0);
                OwnedFd::from_raw_fd(raw)
            };
            let _ = std::fs::remove_file(unlink);
            script(conn);
        });
        (path, thread)
    }

    /// The next request, or `None` once the client has hung up.
    fn request(conn: &OwnedFd) -> Option<Request> {
        let mut data = [0u8; 64];
        // SAFETY: the buffer is valid for its length.
        let n = unsafe { libc::recv(conn.as_raw_fd(), data.as_mut_ptr().cast(), data.len(), 0) };
        (n > 0).then(|| proto::decode_request(&data[..n as usize]).expect("a request"))
    }

    fn answer(conn: &OwnedFd, reply: Reply, fds: &[RawFd]) {
        let mut data = proto::encode_reply(&reply);
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr().cast(),
            iov_len: data.len(),
        };
        let mut control = [0u64; 8];
        // SAFETY: an all-zero msghdr is valid; the pointers set below live
        // until sendmsg returns, and the control buffer holds the header
        // and the descriptors.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            if !fds.is_empty() {
                let bytes = size_of_val(fds);
                msg.msg_control = control.as_mut_ptr().cast();
                msg.msg_controllen = libc::CMSG_SPACE(bytes as u32) as _;
                let cmsg = libc::CMSG_FIRSTHDR(&msg);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(bytes as u32) as _;
                std::ptr::copy_nonoverlapping(
                    fds.as_ptr().cast::<u8>(),
                    libc::CMSG_DATA(cmsg),
                    bytes,
                );
            }
            assert_eq!(
                libc::sendmsg(conn.as_raw_fd(), &msg, 0),
                data.len() as isize
            );
        }
    }

    fn greet(conn: &OwnedFd) {
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        assert_eq!(request(conn), Some(hello));
        let said = Reply::Hello {
            version: proto::VERSION,
        };
        answer(conn, said, &[]);
    }

    /// Answers a configure with a ring and a wake-up, which it returns.
    fn configure(conn: &OwnedFd) -> (Ring, OwnedFd) {
        assert!(matches!(request(conn), Some(Request::Configure { .. })));
        let (ring, memfd) = Ring::create(1024).unwrap();
        // SAFETY: a plain syscall; the descriptor is owned at once.
        let wake = unsafe { OwnedFd::from_raw_fd(libc::eventfd(0, libc::EFD_CLOEXEC)) };
        let fds = [memfd.as_raw_fd(), wake.as_raw_fd()];
        answer(conn, Reply::Configured { rate: 0 }, &fds);
        (ring, wake)
    }

    #[test]
    fn connect_takes_the_version_from_the_reply_to_hello() {
        let (path, daemon) = fake_daemon(|conn| {
            assert!(request(&conn).is_some());
            let other = Reply::Hello {
                version: proto::VERSION + 1,
            };
            answer(&conn, other, &[]);
        });
        let err = Client::connect(&path).err().expect("another version");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(err.to_string().contains(&(proto::VERSION + 1).to_string()));
        daemon.join().unwrap();

        // A reply that names no version is not taken for a match.
        let (path, daemon) = fake_daemon(|conn| {
            assert!(request(&conn).is_some());
            answer(&conn, Reply::Ok, &[]);
        });
        let err = Client::connect(&path).err().expect("no version");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        daemon.join().unwrap();

        let (path, daemon) = fake_daemon(|conn| greet(&conn));
        assert!(Client::connect(&path).is_ok());
        daemon.join().unwrap();
    }

    #[test]
    fn a_request_without_a_reply_times_out_and_loses_the_connection() {
        let (path, daemon) = fake_daemon(|conn| {
            greet(&conn);
            // Reads the requests and answers none, until the client goes.
            while request(&conn).is_some() {}
        });
        let mut client = Client::connect_with(&path, SHORT).unwrap();
        assert!(client.check_alive().is_ok());
        let start = Instant::now();
        let err = client.configure(48000, ClientFormat::S16, 2, 1024);
        let took = start.elapsed();
        assert_eq!(
            err.err().and_then(|e| e.raw_os_error()),
            Some(libc::ETIMEDOUT)
        );
        assert!(took >= SHORT && took < SHORT * 10, "took {took:?}");
        // A late reply would answer the next request, so there is none.
        let start = Instant::now();
        let err = client.configure(48000, ClientFormat::S16, 2, 1024);
        assert_eq!(
            err.err().and_then(|e| e.raw_os_error()),
            Some(libc::ECONNRESET)
        );
        assert!(start.elapsed() < SHORT);
        assert!(client.check_alive().is_err());
        drop(client);
        daemon.join().unwrap();
    }

    #[test]
    fn a_lost_connection_does_not_take_a_late_reply_for_a_drain() {
        let (path, daemon) = fake_daemon(|conn| {
            greet(&conn);
            let _stream = configure(&conn);
            assert_eq!(request(&conn), Some(Request::Drain));
            assert_eq!(request(&conn), Some(Request::Stop));
            // Late: the client has given the stop up by now.
            std::thread::sleep(SHORT * 2);
            answer(&conn, Reply::Ok, &[]);
            assert_eq!(request(&conn), None);
        });
        let mut client = Client::connect_with(&path, SHORT).unwrap();
        client.configure(48000, ClientFormat::S16, 2, 1024).unwrap();
        client.start_drain().unwrap();
        let err = client.stop().err().and_then(|e| e.raw_os_error());
        assert_eq!(err, Some(libc::ETIMEDOUT));
        std::thread::sleep(SHORT * 2);
        // The reply waiting on the socket is not the drain's.
        let err = client.poll_drain().err().and_then(|e| e.raw_os_error());
        assert_eq!(err, Some(libc::ECONNRESET));
        let err = client.start_drain().err().and_then(|e| e.raw_os_error());
        assert_eq!(err, Some(libc::ECONNRESET));
        drop(client);
        daemon.join().unwrap();
    }

    #[test]
    fn a_drain_the_daemon_never_answers_is_given_up() {
        let (path, daemon) = fake_daemon(|conn| {
            greet(&conn);
            let _stream = configure(&conn);
            while request(&conn).is_some() {}
        });
        // 1024 frames at 48 kHz are 21 ms. The daemon answers a drain no
        // later than that plus its spare.
        let limit = Duration::from_millis(22) + DRAIN_SPARE + SHORT;
        let mut client = Client::connect_with(&path, SHORT).unwrap();
        client.configure(48000, ClientFormat::S16, 2, 1024).unwrap();
        let start = Instant::now();
        let err = client.drain().err().and_then(|e| e.raw_os_error());
        let took = start.elapsed();
        assert_eq!(err, Some(libc::ECONNRESET));
        assert!(
            took >= limit - SHORT && took < limit + SHORT * 3,
            "took {took:?}"
        );
        assert!(client.check_alive().is_err());

        // The same through start_drain and poll_drain, as the plugin waits.
        drop(client);
        daemon.join().unwrap();
        let (path, daemon) = fake_daemon(|conn| {
            greet(&conn);
            let _stream = configure(&conn);
            while request(&conn).is_some() {}
        });
        let mut client = Client::connect_with(&path, SHORT).unwrap();
        client.configure(48000, ClientFormat::S16, 2, 1024).unwrap();
        client.start_drain().unwrap();
        assert!(!client.poll_drain().unwrap());
        std::thread::sleep(limit + SHORT);
        let err = client.poll_drain().err().and_then(|e| e.raw_os_error());
        assert_eq!(err, Some(libc::ECONNRESET));
        drop(client);
        daemon.join().unwrap();
    }

    #[test]
    fn a_released_stream_is_gone_until_the_next_configure() {
        let (path, daemon) = fake_daemon(|conn| {
            greet(&conn);
            let _first = configure(&conn);
            let _second = configure(&conn);
            assert_eq!(request(&conn), None);
        });
        let mut client = Client::connect(&path).unwrap();
        client.configure(48000, ClientFormat::S16, 2, 1024).unwrap();
        assert!(client.poll_fds()[0] >= 0);
        assert_eq!(client.write(&[0u8; 400]).unwrap(), 100);
        client.release();
        assert_eq!(client.poll_fds()[0], -1);
        let err = client.hw_position().err().and_then(|e| e.raw_os_error());
        assert_eq!(err, Some(libc::EBADFD));
        // Nothing is sent for a stream that is not there.
        assert!(client.start().is_err());
        assert!(client.write(&[0u8; 400]).is_err());
        client.configure(48000, ClientFormat::S16, 2, 1024).unwrap();
        assert_eq!(client.hw_position().unwrap(), 0);
        drop(client);
        daemon.join().unwrap();
    }

    #[test]
    fn a_drain_waits_longer_than_a_request_may_take() {
        let (path, daemon) = fake_daemon(|conn| {
            greet(&conn);
            let _stream = configure(&conn);
            assert_eq!(request(&conn), Some(Request::Drain));
            std::thread::sleep(SHORT * 3);
            answer(&conn, Reply::Ok, &[]);
            assert_eq!(request(&conn), Some(Request::Stop));
            answer(&conn, Reply::Ok, &[]);
        });
        let mut client = Client::connect_with(&path, SHORT).unwrap();
        client.configure(48000, ClientFormat::S16, 2, 1024).unwrap();
        let start = Instant::now();
        client.drain().unwrap();
        assert!(start.elapsed() >= SHORT * 3);
        // The connection is in step: the next request gets its own reply.
        client.stop().unwrap();
        daemon.join().unwrap();
    }
}
