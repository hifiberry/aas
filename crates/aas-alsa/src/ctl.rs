//! The control plugin: libasound's ctl_ext callbacks over a connection to
//! the daemon. It has one element, `Master Playback Volume`, which is the
//! daemon's output volume, so mixers such as amixer and mpd's hardware
//! mixer set it on every card.
//!
//! Every callback runs inside `catch_unwind`, as in `pcm`. A panic returns
//! `-EIO`, or no element from `find_elem`, and is dropped by the callbacks
//! that return nothing. A daemon that is gone fails each call with `-ENODEV`, and
//! one that does not answer fails it with `-ETIMEDOUT`.
//!
//! The socket is the poll descriptor. Once a client subscribes to events,
//! the daemon sends a notice on every volume change, and each poll that
//! sees the socket readable reads every notice there and reports one event.

use crate::ffi::*;
use crate::pcm::{check_config, errno, guard};
use aas_client::client::socket_address;
use aas_client::proto::{self, Reply, Request};
use std::ffi::{CStr, c_char, c_int, c_long, c_uint, c_ushort};
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
    /// Whether the daemon sends this connection volume notices.
    subscribed: bool,
    /// A notice has come that no event has reported yet.
    changed: bool,
}

fn check(rc: isize) -> Result<usize, c_int> {
    if rc < 0 {
        Err(errno(&io::Error::last_os_error()))
    } else {
        Ok(rc as usize)
    }
}

impl Daemon {
    /// Connects and checks that the daemon speaks this protocol version.
    fn connect() -> Result<Daemon, c_int> {
        let path = proto::default_socket_path();
        let (addr, len) = socket_address(&path).map_err(|e| errno(&e))?;
        // SAFETY: plain syscalls; the descriptor is owned at once.
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        check(raw as isize)?;
        let socket = unsafe { OwnedFd::from_raw_fd(raw) };
        let timeout = libc::timeval {
            tv_sec: TIMEOUT.as_secs() as libc::time_t,
            tv_usec: 0,
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
        let rc = unsafe { libc::connect(socket.as_raw_fd(), (&raw const addr).cast(), len) };
        check(rc as isize)?;
        let mut daemon = Daemon {
            socket,
            subscribed: false,
            changed: false,
        };
        daemon.call(&Request::Hello {
            version: proto::VERSION,
        })?;
        Ok(daemon)
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

    /// Sends a request and waits for its reply. Notices that come first
    /// are noted. `Reply::Error` becomes the error it names.
    fn call(&mut self, request: &Request) -> Result<Reply, c_int> {
        self.send(request)?;
        loop {
            match self.receive(0)? {
                None => return Err(libc::ETIMEDOUT),
                Some(Reply::VolumeChanged { .. }) => self.changed = true,
                Some(Reply::Error { code }) => {
                    return Err(c_int::try_from(code)
                        .ok()
                        .filter(|&n| n > 0)
                        .unwrap_or(libc::EIO));
                }
                Some(reply) => return Ok(reply),
            }
        }
    }

    fn volume(&mut self) -> Result<u8, c_int> {
        match self.call(&Request::GetVolume)? {
            Reply::Volume { percent } => Ok(percent),
            _ => Err(libc::EPROTO),
        }
    }

    fn set_volume(&mut self, percent: u8) -> Result<(), c_int> {
        match self.call(&Request::SetVolume { percent })? {
            Reply::Ok => Ok(()),
            _ => Err(libc::EPROTO),
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
            _ => Err(libc::EPROTO),
        }
    }

    /// Reads every notice waiting, so that the daemon sends the next.
    fn read_notices(&mut self) -> Result<(), c_int> {
        while let Some(reply) = self.receive(libc::MSG_DONTWAIT)? {
            match reply {
                Reply::VolumeChanged { .. } => self.changed = true,
                _ => return Err(libc::EPROTO),
            }
        }
        Ok(())
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
            daemon.read_notices()?;
            Ok(std::mem::take(&mut daemon.changed))
        })?;
        if !(subscribed && changed) {
            return Err(libc::EAGAIN);
        }
        name_element(id);
        *mask = SND_CTL_EVENT_MASK_VALUE;
        Ok(1)
    })
}

/// A socket the daemon has hung up is `POLLERR`, as a card's control is
/// once the card is gone. Reading an event then fails with `-ENODEV`.
/// Clients that watch the descriptor without asking here see `POLLHUP`.
unsafe extern "C" fn poll_revents(
    _ext: Ext,
    pfd: *mut libc::pollfd,
    nfds: c_uint,
    revents: *mut c_ushort,
) -> c_int {
    guard(|| unsafe {
        if pfd.is_null() || nfds != 1 || revents.is_null() {
            return Err(libc::EINVAL);
        }
        let seen = (*pfd).revents;
        let hangup = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
        *revents = if seen & hangup != 0 {
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
    // libasound polls `poll_fd` for POLLIN.
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
        poll_fd: daemon.socket.as_raw_fd(),
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
