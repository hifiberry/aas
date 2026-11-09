//! A client's side of the daemon: the socket, the ring and the wake-up.
//!
//! The socket carries setup and teardown only. Audio goes through the ring.
//! Every call that talks to the daemon waits for its reply.

use crate::convert::{ClientFormat, to_stereo_s32};
use crate::proto::{self, Reply, Request};
use crate::ring::Ring;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::Ordering;

/// Descriptors a reply may carry: the ring and the wake-up.
const MAX_FDS: usize = 2;
/// Frames converted per step of `write`. Kept on the stack.
const CHUNK: usize = 512;

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

struct Stream {
    ring: Ring,
    wake: OwnedFd,
    format: ClientFormat,
    channels: u8,
}

/// A connection to the daemon, and the stream it has configured.
pub struct Client {
    socket: OwnedFd,
    stream: Option<Stream>,
}

impl Client {
    /// Connects and checks that the daemon speaks this protocol version.
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Client> {
        let (addr, len) = socket_address(path.as_ref())?;
        // SAFETY: plain syscalls; the descriptor is owned at once.
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        check(raw as isize)?;
        let socket = unsafe { OwnedFd::from_raw_fd(raw) };
        let rc = unsafe { libc::connect(socket.as_raw_fd(), (&raw const addr).cast(), len) };
        check(rc as isize)?;
        let client = Client {
            socket,
            stream: None,
        };
        client.call(&Request::Hello {
            version: proto::VERSION,
        })?;
        Ok(client)
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
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => return Err(daemon_gone()),
                Err(e) => return Err(e),
                Ok(_) => return Ok(()),
            }
        }
    }

    /// Waits for one reply and the descriptors that came with it.
    fn receive(&self) -> io::Result<(Reply, Vec<OwnedFd>)> {
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
            let rc =
                unsafe { libc::recvmsg(self.socket.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
            match check(rc) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
                Ok(0) => return Err(daemon_gone()),
                Ok(n) => break n,
            }
        };
        let fds = Self::descriptors(&msg);
        let reply = proto::decode_reply(&data[..n])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad reply"))?;
        Ok((reply, fds))
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
    /// error it names.
    fn call(&self, request: &Request) -> io::Result<(Reply, Vec<OwnedFd>)> {
        self.send(request)?;
        match self.receive()? {
            (Reply::Error { code }, _) => Err(io::Error::from_raw_os_error(code as i32)),
            answer => Ok(answer),
        }
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
        self.stream = None;
        let request = Request::Configure {
            rate,
            format,
            channels,
            buffer_frames,
        };
        let (reply, fds) = self.call(&request)?;
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
        if reply != Reply::Configured {
            return Err(bad("unexpected reply to configure"));
        }
        let [memfd, wake]: [OwnedFd; MAX_FDS] = fds
            .try_into()
            .map_err(|_| bad("configure came without the ring"))?;
        let ring = Ring::map(memfd.as_fd())?;
        self.stream = Some(Stream {
            ring,
            wake,
            format,
            channels,
        });
        Ok(())
    }

    /// Starts playing what is in the ring and what is written after.
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
        let stream = self.stream()?;
        // After the last frames, which the daemon relies on.
        stream.ring.header().draining.store(1, Ordering::Release);
        self.call(&Request::Drain).map(drop)
    }

    /// Converts and writes as many whole frames as the ring has room for.
    /// Returns the number of frames written.
    pub fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let stream = self.stream()?;
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

    /// Frames the daemon has taken from the ring since it was configured.
    /// Fails once the daemon has gone.
    pub fn hw_position(&self) -> io::Result<u64> {
        self.check_alive()?;
        let header = self.stream()?.ring.header();
        Ok(header.read_pos.load(Ordering::Acquire))
    }

    /// Frames between what is written next and the listener: what the ring
    /// holds and what plays after it.
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
    pub fn underruns(&self) -> u64 {
        self.stream
            .as_ref()
            .map_or(0, |s| s.ring.header().underruns.load(Ordering::Relaxed))
    }

    /// The wake-up eventfd, readable after each period played, and the
    /// socket, which hangs up when the daemon goes. The first is -1 until
    /// the stream is configured.
    pub fn poll_fds(&self) -> [RawFd; 2] {
        let wake = self.stream.as_ref().map_or(-1, |s| s.wake.as_raw_fd());
        [wake, self.socket.as_raw_fd()]
    }

    /// Fails if the daemon has closed the connection.
    pub fn check_alive(&self) -> io::Result<()> {
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
