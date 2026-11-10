//! The control plugin: libasound's ctl_ext callbacks over a connection to
//! the daemon. It has one element, `Master Playback Volume`, which is the
//! daemon's output volume, so mixers such as amixer and mpd's hardware
//! mixer set it on every card.
//!
//! Every callback runs inside `catch_unwind`, as in `pcm`. A panic returns
//! `-EIO`, or no element from `find_elem`, and is dropped by the callbacks
//! that return nothing.
//!
//! Once a client subscribes to events, the daemon sends a notice on every
//! volume change. The poll descriptor is an epoll set of the socket and an
//! eventfd that is made readable when a request reads a notice on its way
//! to its reply. Each event read takes every notice there and reports one
//! change.
//!
//! A connection that hangs up, gives no reply in time or a reply out of
//! step is lost: every later call fails with `-ENODEV`, as for a card that
//! is unplugged, and the socket leaves the epoll set, so a client polling
//! the descriptor sleeps instead of waking again and again.

use crate::ffi::*;
use crate::pcm::{check_config, errno, guard};
use aas_client::client::socket_address;
use aas_client::proto::{self, Reply, Request};
use std::ffi::{CStr, c_char, c_int, c_long, c_uint, c_ushort, c_void};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;
use std::sync::Mutex;
use std::time::Duration;

const ELEMENT: &CStr = c"Master Playback Volume";
const KEY: snd_ctl_ext_key_t = 0;
const MAX_PERCENT: c_long = 100;
/// 0 mutes, and 1 to 100 run linearly from -59.4 dB to 0 dB, which is how
/// the daemon maps the volume.
static TLV: [c_uint; 4] = [SND_CTL_TLVT_DB_MINMAX_MUTE, 8, -6000i32 as c_uint, 0];
/// How long a request waits for the daemon's answer.
const TIMEOUT: Duration = Duration::from_secs(2);

type Ext = *mut snd_ctl_ext_t;

/// What the plugin keeps per open control. `ext.private_data` points here.
struct Shared {
    /// Owned here, freed on close.
    ext: Ext,
    daemon: Mutex<Daemon>,
}

/// The connection to the daemon.
struct Daemon {
    socket: OwnedFd,
    /// What clients poll: an epoll set of the socket and `wake`.
    poll: OwnedFd,
    /// Readable while a notice read by a request waits to be reported.
    wake: OwnedFd,
    /// Whether the daemon sends this connection volume notices.
    subscribed: bool,
    /// A notice has come that no event has reported yet.
    changed: bool,
    /// The connection is lost. Its socket has left the epoll set.
    gone: bool,
}

fn check(rc: isize) -> Result<usize, c_int> {
    if rc < 0 {
        Err(errno(&io::Error::last_os_error()))
    } else {
        Ok(rc as usize)
    }
}

/// Owns a descriptor a syscall returned, or its error.
fn owned(raw: c_int) -> Result<OwnedFd, c_int> {
    check(raw as isize)?;
    // SAFETY: a fresh descriptor, owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

impl Daemon {
    /// Connects and checks that the daemon speaks this protocol version.
    fn connect() -> Result<Daemon, c_int> {
        let path = proto::default_socket_path();
        let (addr, len) = socket_address(&path).map_err(|e| errno(&e))?;
        // SAFETY: a plain syscall.
        let socket = owned(unsafe {
            libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0)
        })?;
        // SAFETY: addr is a sockaddr_un of length len.
        let rc = unsafe { libc::connect(socket.as_raw_fd(), (&raw const addr).cast(), len) };
        check(rc as isize)?;
        let mut daemon = Daemon::new(socket, TIMEOUT)?;
        daemon.call(&Request::Hello {
            version: proto::VERSION,
        })?;
        Ok(daemon)
    }

    /// Wraps a connected socket. Requests on it wait at most `timeout`.
    fn new(socket: OwnedFd, timeout: Duration) -> Result<Daemon, c_int> {
        let timeout = libc::timeval {
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
                    (&raw const timeout).cast(),
                    size_of::<libc::timeval>() as libc::socklen_t,
                )
            };
            check(rc as isize)?;
        }
        // SAFETY: plain syscalls.
        let poll = owned(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
        let wake = owned(unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) })?;
        for fd in [socket.as_raw_fd(), wake.as_raw_fd()] {
            let mut event = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: fd as u64,
            };
            // SAFETY: both descriptors are open; event lives for the call.
            let rc =
                unsafe { libc::epoll_ctl(poll.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut event) };
            check(rc as isize)?;
        }
        Ok(Daemon {
            socket,
            poll,
            wake,
            subscribed: false,
            changed: false,
            gone: false,
        })
    }

    /// Gives the connection up. Nothing it holds wakes a poll again.
    fn lose(&mut self) {
        if self.gone {
            return;
        }
        self.gone = true;
        for fd in [self.socket.as_raw_fd(), self.wake.as_raw_fd()] {
            // SAFETY: both descriptors are open and in the set.
            unsafe { libc::epoll_ctl(self.poll.as_raw_fd(), libc::EPOLL_CTL_DEL, fd, null_mut()) };
        }
    }

    /// Loses the connection on `err`, and passes it on.
    fn lost(&mut self, err: c_int) -> c_int {
        self.lose();
        err
    }

    fn alive(&self) -> Result<(), c_int> {
        if self.gone { Err(libc::ENODEV) } else { Ok(()) }
    }

    /// Whether the daemon has hung up. Loses the connection if it has.
    fn hung_up(&mut self) -> bool {
        if self.gone {
            return true;
        }
        let mut pfd = libc::pollfd {
            fd: self.socket.as_raw_fd(),
            events: 0,
            revents: 0,
        };
        // SAFETY: one pollfd, no wait.
        let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
        if rc == 1 && pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            self.lose();
        }
        self.gone
    }

    fn send(&self, request: &Request) -> Result<(), c_int> {
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
                Err(libc::EINTR) => continue,
                Err(libc::EAGAIN) => return Err(libc::ETIMEDOUT),
                Err(e) => return Err(e),
                Ok(_) => return Ok(()),
            }
        }
    }

    /// Reads one message, or `None` if none came: at once with
    /// `MSG_DONTWAIT`, else within the timeout.
    fn receive(&self, flags: c_int) -> Result<Option<Reply>, c_int> {
        let mut data = [0u8; 64];
        let n = loop {
            // SAFETY: the buffer is valid for its length.
            let rc = unsafe {
                libc::recv(
                    self.socket.as_raw_fd(),
                    data.as_mut_ptr().cast(),
                    data.len(),
                    flags,
                )
            };
            match check(rc) {
                Err(libc::EINTR) => continue,
                Err(libc::EAGAIN) => return Ok(None),
                Err(e) => return Err(e),
                Ok(0) => return Err(libc::ENODEV),
                Ok(n) => break n,
            }
        };
        proto::decode_reply(&data[..n])
            .map(Some)
            .ok_or(libc::EPROTO)
    }

    /// Waits for the reply to the request just sent. A notice that comes
    /// first is kept for the next event, and wakes the poll for it.
    fn reply(&mut self) -> Result<Reply, c_int> {
        loop {
            match self.receive(0)? {
                None => return Err(libc::ETIMEDOUT),
                Some(Reply::VolumeChanged { .. }) => {
                    self.changed = true;
                    let one = 1u64;
                    // SAFETY: 8 bytes to a non-blocking eventfd.
                    unsafe {
                        libc::write(self.wake.as_raw_fd(), (&raw const one).cast::<c_void>(), 8)
                    };
                }
                Some(reply) => return Ok(reply),
            }
        }
    }

    /// Sends a request and waits for its reply. `Reply::Error` becomes the
    /// error it names. A reply that does not come in time would answer the
    /// next request instead, so the connection is lost then, as on any
    /// other failure of the socket.
    fn call(&mut self, request: &Request) -> Result<Reply, c_int> {
        self.alive()?;
        let reply = self.send(request).and_then(|()| self.reply());
        match reply.map_err(|e| self.lost(e))? {
            Reply::Error { code } => Err(c_int::try_from(code)
                .ok()
                .filter(|&n| n > 0)
                .unwrap_or(libc::EIO)),
            reply => Ok(reply),
        }
    }

    fn volume(&mut self) -> Result<u8, c_int> {
        match self.call(&Request::GetVolume)? {
            Reply::Volume { percent } => Ok(percent),
            _ => Err(self.lost(libc::EPROTO)),
        }
    }

    fn set_volume(&mut self, percent: u8) -> Result<(), c_int> {
        match self.call(&Request::SetVolume { percent })? {
            Reply::Ok => Ok(()),
            _ => Err(self.lost(libc::EPROTO)),
        }
    }

    /// Asks for notices. The daemon has no way to stop them, so they are
    /// asked for once and read until the control closes.
    fn subscribe(&mut self) -> Result<(), c_int> {
        if self.subscribed {
            return Ok(());
        }
        match self.call(&Request::SubscribeVolume)? {
            Reply::Volume { .. } => {
                self.subscribed = true;
                Ok(())
            }
            _ => Err(self.lost(libc::EPROTO)),
        }
    }

    /// Whether the volume changed since the last call. Reads every notice
    /// waiting, so that the daemon sends the next.
    fn next_change(&mut self) -> Result<bool, c_int> {
        self.alive()?;
        let mut count = 0u64;
        // SAFETY: 8 bytes from a non-blocking eventfd.
        unsafe { libc::read(self.wake.as_raw_fd(), (&raw mut count).cast::<c_void>(), 8) };
        loop {
            match self.receive(libc::MSG_DONTWAIT) {
                Ok(None) => break,
                Ok(Some(Reply::VolumeChanged { .. })) => self.changed = true,
                Ok(Some(_)) => return Err(self.lost(libc::EPROTO)),
                Err(e) => return Err(self.lost(e)),
            }
        }
        Ok(std::mem::take(&mut self.changed))
    }
}

/// Runs `f` with the connection locked. A lock poisoned by an earlier
/// panic stays an error.
///
/// # Safety
/// `ext` is a handle this plugin created and has not closed.
unsafe fn with<T>(ext: Ext, f: impl FnOnce(&mut Daemon) -> Result<T, c_int>) -> Result<T, c_int> {
    // SAFETY: private_data is the Shared made in open, alive until close.
    let shared = unsafe { &*((*ext).private_data as *const Shared) };
    let mut daemon = shared.daemon.lock().map_err(|_| libc::EIO)?;
    f(&mut daemon)
}

/// Names the element in `id`. Its number is 1, as libasound numbers the
/// first element of a list.
///
/// # Safety
/// `id` is a valid element id.
unsafe fn name_element(id: *mut snd_ctl_elem_id_t) {
    unsafe {
        snd_ctl_elem_id_set_numid(id, 1);
        snd_ctl_elem_id_set_interface(id, SND_CTL_ELEM_IFACE_MIXER);
        snd_ctl_elem_id_set_name(id, ELEMENT.as_ptr());
    }
}

fn check_key(key: snd_ctl_ext_key_t) -> Result<(), c_int> {
    if key == KEY {
        Ok(())
    } else {
        Err(libc::EINVAL)
    }
}

unsafe extern "C" fn close(ext: Ext) {
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        let shared = Box::from_raw((*ext).private_data as *mut Shared);
        let ext = shared.ext;
        // Dropping the connection closes the socket.
        drop(shared);
        drop(Box::from_raw(ext));
    }));
}

unsafe extern "C" fn elem_count(_ext: Ext) -> c_int {
    guard(|| Ok(1))
}

unsafe extern "C" fn elem_list(_ext: Ext, offset: c_uint, id: *mut snd_ctl_elem_id_t) -> c_int {
    guard(|| unsafe {
        if offset != 0 || id.is_null() {
            return Err(libc::EINVAL);
        }
        name_element(id);
        Ok(0)
    })
}

unsafe extern "C" fn find_elem(_ext: Ext, id: *const snd_ctl_elem_id_t) -> snd_ctl_ext_key_t {
    let found = catch_unwind(|| unsafe {
        !id.is_null()
            && snd_ctl_elem_id_get_interface(id) == SND_CTL_ELEM_IFACE_MIXER
            && snd_ctl_elem_id_get_index(id) == 0
            && {
                let name = snd_ctl_elem_id_get_name(id);
                !name.is_null() && CStr::from_ptr(name) == ELEMENT
            }
    });
    match found {
        Ok(true) => KEY,
        _ => SND_CTL_EXT_KEY_NOT_FOUND,
    }
}

unsafe extern "C" fn get_attribute(
    _ext: Ext,
    key: snd_ctl_ext_key_t,
    kind: *mut c_int,
    access: *mut c_uint,
    count: *mut c_uint,
) -> c_int {
    guard(|| unsafe {
        check_key(key)?;
        if kind.is_null() || access.is_null() || count.is_null() {
            return Err(libc::EINVAL);
        }
        *kind = SND_CTL_ELEM_TYPE_INTEGER;
        *access = SND_CTL_EXT_ACCESS_READWRITE | SND_CTL_EXT_ACCESS_TLV_READ;
        *count = 1;
        Ok(0)
    })
}

unsafe extern "C" fn get_integer_info(
    _ext: Ext,
    key: snd_ctl_ext_key_t,
    min: *mut c_long,
    max: *mut c_long,
    step: *mut c_long,
) -> c_int {
    guard(|| unsafe {
        check_key(key)?;
        if min.is_null() || max.is_null() || step.is_null() {
            return Err(libc::EINVAL);
        }
        (*min, *max, *step) = (0, MAX_PERCENT, 1);
        Ok(0)
    })
}

unsafe extern "C" fn read_integer(ext: Ext, key: snd_ctl_ext_key_t, value: *mut c_long) -> c_int {
    guard(|| unsafe {
        check_key(key)?;
        if value.is_null() {
            return Err(libc::EINVAL);
        }
        *value = with(ext, |daemon| daemon.volume())?.into();
        Ok(0)
    })
}

/// Returns 1 if the volume changed and 0 if it was already `value`, as a
/// card's control does.
unsafe extern "C" fn write_integer(ext: Ext, key: snd_ctl_ext_key_t, value: *mut c_long) -> c_int {
    guard(|| unsafe {
        check_key(key)?;
        let value = value.as_ref().ok_or(libc::EINVAL)?;
        let percent = u8::try_from(*value)
            .ok()
            .filter(|&p| c_long::from(p) <= MAX_PERCENT)
            .ok_or(libc::EINVAL)?;
        with(ext, |daemon| {
            if daemon.volume()? == percent {
                return Ok(0);
            }
            daemon.set_volume(percent)?;
            Ok(1)
        })
    })
}

/// libasound has set `ext.subscribed` already. A failure shows at the
/// next read of an event, since this callback returns nothing.
unsafe extern "C" fn subscribe_events(ext: Ext, subscribe: c_int) {
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        if subscribe as c_uint & SND_CTL_EVENT_MASK_VALUE != 0 {
            let _ = with(ext, |daemon| daemon.subscribe());
        }
    }));
}

/// Reports one change of the volume, however many notices came, or
/// `-EAGAIN` if there is none.
unsafe extern "C" fn read_event(ext: Ext, id: *mut snd_ctl_elem_id_t, mask: *mut c_uint) -> c_int {
    guard(|| unsafe {
        if id.is_null() || mask.is_null() {
            return Err(libc::EINVAL);
        }
        let subscribed = (*ext).subscribed != 0;
        let changed = with(ext, |daemon| {
            if subscribed && !daemon.subscribed {
                daemon.subscribe()?;
            }
            daemon.next_change()
        })?;
        if !(subscribed && changed) {
            return Err(libc::EAGAIN);
        }
        name_element(id);
        *mask = SND_CTL_EVENT_MASK_VALUE;
        Ok(1)
    })
}

/// A daemon that has hung up is `POLLERR`, as a card's control is once
/// the card is gone. Reading an event then fails with `-ENODEV`, and the
/// descriptor stays quiet from then on.
unsafe extern "C" fn poll_revents(
    ext: Ext,
    pfd: *mut libc::pollfd,
    nfds: c_uint,
    revents: *mut c_ushort,
) -> c_int {
    guard(|| unsafe {
        if pfd.is_null() || nfds != 1 || revents.is_null() {
            return Err(libc::EINVAL);
        }
        let seen = (*pfd).revents;
        let hung_up = with(ext, |daemon| Ok(daemon.hung_up()))?;
        *revents = if hung_up || seen & (libc::POLLERR | libc::POLLNVAL) != 0 {
            libc::POLLERR
        } else {
            seen & libc::POLLIN
        } as c_ushort;
        Ok(0)
    })
}

static CALLBACKS: snd_ctl_ext_callback_t = snd_ctl_ext_callback_t {
    close: Some(close),
    elem_count: Some(elem_count),
    elem_list: Some(elem_list),
    find_elem: Some(find_elem),
    free_key: None,
    get_attribute: Some(get_attribute),
    get_integer_info: Some(get_integer_info),
    get_integer64_info: None,
    get_enumerated_info: None,
    get_enumerated_name: None,
    read_integer: Some(read_integer),
    read_integer64: None,
    read_enumerated: None,
    read_bytes: None,
    read_iec958: None,
    write_integer: Some(write_integer),
    write_integer64: None,
    write_enumerated: None,
    write_bytes: None,
    write_iec958: None,
    subscribe_events: Some(subscribe_events),
    read_event: Some(read_event),
    // libasound polls `poll_fd`, the epoll set, for POLLIN.
    poll_descriptors_count: None,
    poll_descriptors: None,
    poll_revents: Some(poll_revents),
};

/// `text` as a C string in a fixed array, cut to fit.
fn text<const N: usize>(text: &str) -> [c_char; N] {
    let mut out = [0; N];
    for (dst, src) in out.iter_mut().zip(text.bytes().take(N - 1)) {
        *dst = src as c_char;
    }
    out
}

/// # Safety
/// As `_snd_ctl_aas_open`.
unsafe fn open(
    handlep: *mut *mut snd_ctl_t,
    name: *const c_char,
    conf: *mut snd_config_t,
    mode: c_int,
) -> Result<c_int, c_int> {
    unsafe { check_config(conf)? };
    if handlep.is_null() {
        return Err(libc::EINVAL);
    }
    let daemon = Daemon::connect()?;
    let ext = Box::into_raw(Box::new(snd_ctl_ext_t {
        version: SND_CTL_EXT_VERSION,
        card_idx: 0,
        id: text("aas"),
        driver: text("aas"),
        name: text("aas"),
        longname: text("Alternative audio server"),
        mixername: text("aas"),
        poll_fd: daemon.poll.as_raw_fd(),
        callback: &CALLBACKS,
        private_data: null_mut(),
        handle: null_mut(),
        nonblock: 0,
        subscribed: 0,
        tlv: snd_ctl_ext_tlv { p: TLV.as_ptr() },
    }));
    let shared = Box::into_raw(Box::new(Shared {
        ext,
        daemon: Mutex::new(daemon),
    }));
    // SAFETY: ext and shared are fresh allocations. After create succeeds,
    // libasound owns their release through the close callback.
    unsafe {
        (*ext).private_data = shared.cast();
        let rc = snd_ctl_ext_create(ext, name, mode);
        if rc < 0 {
            drop(Box::from_raw(shared));
            drop(Box::from_raw(ext));
            return Err(-rc);
        }
        *handlep = (*ext).handle;
    }
    Ok(0)
}

/// Opens the control. libasound finds it by name for `type aas`.
///
/// # Safety
/// Called by libasound with its own valid arguments.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _snd_ctl_aas_open(
    handlep: *mut *mut snd_ctl_t,
    name: *const c_char,
    _root: *mut snd_config_t,
    conf: *mut snd_config_t,
    mode: c_int,
) -> c_int {
    guard(|| unsafe { open(handlep, name, conf, mode) })
}

/// The symbol libasound checks for the plugin's API version. Its suffix is
/// `SND_CONTROL_DLSYM_VERSION` from `alsa/control.h`.
#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
pub static __snd_ctl_aas_open_dlsym_control_001: c_char = 0;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::RawFd;

    /// A connection to a fake daemon: the plugin's side and the daemon's.
    fn pair() -> (Daemon, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: fds has room for two descriptors.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0);
        let [ours, theirs] = fds.map(|fd| owned(fd).unwrap());
        let daemon = Daemon::new(ours, Duration::from_millis(100)).unwrap();
        (daemon, theirs)
    }

    fn answer(peer: &OwnedFd, reply: Reply) {
        let message = proto::encode_reply(&reply);
        // SAFETY: the buffer is valid for its length.
        let rc = unsafe { libc::send(peer.as_raw_fd(), message.as_ptr().cast(), message.len(), 0) };
        assert_eq!(rc, message.len() as isize);
    }

    fn readable(fd: RawFd) -> bool {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd, no wait.
        unsafe { libc::poll(&mut pfd, 1, 0) == 1 }
    }

    #[test]
    fn a_reply_that_comes_late_does_not_answer_the_next_request() {
        let (mut daemon, peer) = pair();
        assert_eq!(daemon.volume(), Err(libc::ETIMEDOUT));
        answer(&peer, Reply::Volume { percent: 50 });
        assert_eq!(daemon.volume(), Err(libc::ENODEV));
        assert_eq!(daemon.set_volume(20), Err(libc::ENODEV));
    }

    #[test]
    fn a_reply_out_of_step_loses_the_connection() {
        let (mut daemon, peer) = pair();
        answer(&peer, Reply::Ok);
        assert_eq!(daemon.volume(), Err(libc::EPROTO));
        answer(&peer, Reply::Volume { percent: 50 });
        assert_eq!(daemon.volume(), Err(libc::ENODEV));
    }

    #[test]
    fn a_daemon_error_keeps_the_connection() {
        let (mut daemon, peer) = pair();
        answer(
            &peer,
            Reply::Error {
                code: libc::EINVAL as u32,
            },
        );
        assert_eq!(daemon.set_volume(100), Err(libc::EINVAL));
        answer(&peer, Reply::Volume { percent: 50 });
        assert_eq!(daemon.volume(), Ok(50));
    }

    #[test]
    fn a_notice_read_by_a_request_wakes_the_poll() {
        let (mut daemon, peer) = pair();
        daemon.subscribed = true;
        answer(&peer, Reply::VolumeChanged { percent: 30 });
        answer(&peer, Reply::Volume { percent: 30 });
        assert_eq!(daemon.volume(), Ok(30));
        assert!(
            readable(daemon.poll.as_raw_fd()),
            "no wake-up for the change"
        );
        assert_eq!(daemon.next_change(), Ok(true));
        assert!(
            !readable(daemon.poll.as_raw_fd()),
            "woken with nothing to report"
        );
        assert_eq!(daemon.next_change(), Ok(false));
    }

    #[test]
    fn notices_on_the_socket_make_one_change() {
        let (mut daemon, peer) = pair();
        assert!(!readable(daemon.poll.as_raw_fd()));
        answer(&peer, Reply::VolumeChanged { percent: 30 });
        answer(&peer, Reply::VolumeChanged { percent: 40 });
        assert!(readable(daemon.poll.as_raw_fd()));
        assert_eq!(daemon.next_change(), Ok(true));
        assert!(!readable(daemon.poll.as_raw_fd()));
        assert_eq!(daemon.next_change(), Ok(false));
    }

    #[test]
    fn a_daemon_that_hangs_up_leaves_the_poll_quiet() {
        let (mut daemon, peer) = pair();
        drop(peer);
        assert!(readable(daemon.poll.as_raw_fd()));
        assert_eq!(daemon.next_change(), Err(libc::ENODEV));
        assert!(!readable(daemon.poll.as_raw_fd()), "the poll still wakes");
        assert_eq!(daemon.volume(), Err(libc::ENODEV));
    }

    #[test]
    fn a_hang_up_seen_by_poll_leaves_the_poll_quiet() {
        let (mut daemon, peer) = pair();
        assert!(!daemon.hung_up());
        drop(peer);
        assert!(daemon.hung_up());
        assert!(!readable(daemon.poll.as_raw_fd()), "the poll still wakes");
        assert_eq!(daemon.next_change(), Err(libc::ENODEV));
    }
}
