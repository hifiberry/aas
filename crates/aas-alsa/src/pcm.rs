//! The playback PCM: libasound's ioplug callbacks over an aas `Client`.
//!
//! Every callback runs inside `catch_unwind` and returns `-EIO` on a panic,
//! so a bug here fails one call instead of bringing down the player.
//!
//! Positions count from the last prepare, which sets up a fresh ring. So
//! the frames the daemon has taken from the ring are ALSA's hw pointer, and
//! the frames written to it are ALSA's application pointer.

use crate::ffi::*;
use aas_client::client::Client;
use aas_client::convert::ClientFormat;
use aas_client::proto;
use std::ffi::{CStr, c_char, c_int, c_uint, c_ushort, c_void};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{null, null_mut};
use std::sync::Mutex;

const NAME: &CStr = c"aas";
const FORMATS: [c_uint; 5] = [
    SND_PCM_FORMAT_S16_LE as c_uint,
    SND_PCM_FORMAT_S24_LE as c_uint,
    SND_PCM_FORMAT_S24_3LE as c_uint,
    SND_PCM_FORMAT_S32_LE as c_uint,
    SND_PCM_FORMAT_FLOAT_LE as c_uint,
];
const ACCESS: [c_uint; 2] = [
    SND_PCM_ACCESS_RW_INTERLEAVED as c_uint,
    SND_PCM_ACCESS_MMAP_INTERLEAVED as c_uint,
];
const MIN_RATE: c_uint = 8_000;
const MAX_RATE: c_uint = 384_000;
const MAX_CHANNELS: c_uint = 2;
/// The smallest frame is 2 bytes, S16 mono, so no buffer holds more than
/// the daemon's largest ring.
const MAX_BUFFER_BYTES: c_uint = proto::MAX_BUFFER_FRAMES * 2;
const MIN_PERIOD_BYTES: c_uint = 128;
const MAX_PERIODS: c_uint = 1024;

type Io = *mut snd_pcm_ioplug_t;

/// What the plugin keeps per open PCM. `io.private_data` points here.
struct Shared {
    /// Owned here, freed on close. libasound updates it, so it is only
    /// ever reached through the raw pointer.
    io: Io,
    inner: Mutex<Inner>,
}

struct Inner {
    client: Client,
    stream: Option<Stream>,
}

/// The stream set up by the last prepare.
struct Stream {
    frame_bytes: usize,
    buffer: u64,
    period: u64,
    /// Where ALSA wraps the hw pointer.
    boundary: u64,
    /// Frames written to the ring since prepare.
    written: u64,
    /// The daemon's underrun count when the stream last started.
    underruns: u64,
}

impl Stream {
    /// Frames the player may write, with `read` frames taken by the daemon.
    fn avail(&self, read: u64) -> u64 {
        self.buffer
            .saturating_sub(self.written.saturating_sub(read))
    }
}

fn client_format(format: snd_pcm_format_t) -> Option<ClientFormat> {
    Some(match format {
        SND_PCM_FORMAT_S16_LE => ClientFormat::S16,
        SND_PCM_FORMAT_S24_LE => ClientFormat::S24,
        SND_PCM_FORMAT_S24_3LE => ClientFormat::S24Packed,
        SND_PCM_FORMAT_S32_LE => ClientFormat::S32,
        SND_PCM_FORMAT_FLOAT_LE => ClientFormat::Float,
        _ => return None,
    })
}

/// The positive errno for a failed call. A daemon that has gone is
/// `ENODEV`, as for an unplugged card.
fn errno(e: &io::Error) -> c_int {
    match e.raw_os_error() {
        Some(libc::ECONNRESET | libc::EPIPE | libc::ENOTCONN) => libc::ENODEV,
        Some(n) if n > 0 => n,
        _ => libc::EIO,
    }
}

/// Runs a callback body. A panic becomes `-EIO`, an `Err(n)` becomes `-n`.
fn guard<T: From<c_int>>(body: impl FnOnce() -> Result<T, c_int>) -> T {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(value)) => value,
        Ok(Err(n)) => T::from(-n),
        Err(_) => T::from(-libc::EIO),
    }
}

/// Runs `f` with the plugin's state locked. A lock poisoned by an earlier
/// panic stays an error.
///
/// # Safety
/// `io` is a handle this plugin created and has not closed.
unsafe fn with<T>(io: Io, f: impl FnOnce(&mut Inner) -> Result<T, c_int>) -> Result<T, c_int> {
    // SAFETY: private_data is the Shared made in open, alive until close.
    let shared = unsafe { &*((*io).private_data as *const Shared) };
    let mut inner = shared.inner.lock().map_err(|_| libc::EIO)?;
    f(&mut inner)
}

/// The PCM state as libasound keeps it.
///
/// # Safety
/// As for `with`.
unsafe fn state(io: Io) -> snd_pcm_state_t {
    unsafe { (*io).state }
}

/// Makes the wake-up readable, so a poll returns at once. The daemon
/// writes it only while the stream runs.
fn arm(client: &Client) {
    let fd = client.poll_fds()[0];
    if fd >= 0 {
        let one = 1u64;
        // SAFETY: 8 bytes to a non-blocking eventfd.
        unsafe { libc::write(fd, (&raw const one).cast::<c_void>(), 8) };
    }
}

/// Clears the wake-up.
fn clear(client: &Client) {
    let fd = client.poll_fds()[0];
    if fd >= 0 {
        let mut count = 0u64;
        // SAFETY: 8 bytes from a non-blocking eventfd.
        unsafe { libc::read(fd, (&raw mut count).cast::<c_void>(), 8) };
    }
}

/// Where ALSA wraps the hw pointer. Asked before the plugin's lock is
/// taken, since libasound takes its own.
///
/// # Safety
/// `pcm` is a set-up PCM.
// c_ulong is 32 bits on some targets, so its casts to u64 are not idle.
#[allow(clippy::unnecessary_cast)]
unsafe fn boundary(pcm: *mut snd_pcm_t) -> Result<u64, c_int> {
    let mut params = null_mut();
    // SAFETY: plain libasound calls; params is freed on every path.
    unsafe {
        let rc = snd_pcm_sw_params_malloc(&mut params);
        if rc < 0 {
            return Err(-rc);
        }
        let mut value = 0;
        let mut rc = snd_pcm_sw_params_current(pcm, params);
        if rc >= 0 {
            rc = snd_pcm_sw_params_get_boundary(params, &mut value);
        }
        snd_pcm_sw_params_free(params);
        match rc {
            rc if rc < 0 => Err(-rc),
            _ if value == 0 => Err(libc::EINVAL),
            _ => Ok(value as u64),
        }
    }
}

unsafe extern "C" fn start(io: Io) -> c_int {
    guard(|| unsafe {
        with(io, |inner| {
            inner.client.start().map_err(|e| errno(&e))?;
            let underruns = inner.client.underruns();
            if let Some(stream) = &mut inner.stream {
                stream.underruns = underruns;
            }
            Ok(0)
        })
    })
}

unsafe extern "C" fn stop(io: Io) -> c_int {
    guard(|| unsafe {
        with(io, |inner| {
            let stopped = inner.client.stop();
            arm(&inner.client);
            stopped.map(|_| 0).map_err(|e| errno(&e))
        })
    })
}

unsafe extern "C" fn pointer(io: Io) -> snd_pcm_sframes_t {
    guard(|| unsafe {
        let running = state(io) == SND_PCM_STATE_RUNNING;
        with(io, |inner| {
            let Some(stream) = &inner.stream else {
                return Ok(0);
            };
            let read = inner.client.hw_position().map_err(|e| errno(&e))?;
            if running && inner.client.underruns() > stream.underruns {
                return Err(libc::EPIPE);
            }
            Ok((read % stream.boundary) as snd_pcm_sframes_t)
        })
    })
}

unsafe extern "C" fn transfer(
    io: Io,
    areas: *const snd_pcm_channel_area_t,
    offset: snd_pcm_uframes_t,
    size: snd_pcm_uframes_t,
) -> snd_pcm_sframes_t {
    guard(|| unsafe {
        with(io, |inner| {
            let stream = inner.stream.as_mut().ok_or(libc::EBADFD)?;
            // Interleaved: the first area describes every frame.
            let area = &*areas;
            if area.first % 8 != 0 || area.step as usize != stream.frame_bytes * 8 {
                return Err(libc::EINVAL);
            }
            let first = area.first as usize / 8 + offset as usize * stream.frame_bytes;
            let data = (area.addr as *const u8).add(first);
            let bytes = std::slice::from_raw_parts(data, size as usize * stream.frame_bytes);
            let frames = inner.client.write(bytes).map_err(|e| errno(&e))?;
            stream.written += frames as u64;
            Ok(frames as snd_pcm_sframes_t)
        })
    })
}

unsafe extern "C" fn close(io: Io) -> c_int {
    guard(|| unsafe {
        let shared = Box::from_raw((*io).private_data as *mut Shared);
        let io = shared.io;
        // Dropping the client closes the connection, which ends the stream.
        drop(shared);
        drop(Box::from_raw(io));
        Ok(0)
    })
}

unsafe extern "C" fn hw_params(io: Io, _params: *mut snd_pcm_hw_params_t) -> c_int {
    guard(|| unsafe {
        let (format, channels) = ((*io).format, (*io).channels);
        if client_format(format).is_none() || !(1..=MAX_CHANNELS).contains(&channels) {
            return Err(libc::EINVAL);
        }
        // The stream is set up in prepare, which hw_params is followed by.
        with(io, |inner| {
            inner.stream = None;
            Ok(0)
        })
    })
}

/// Sets up a fresh ring on the daemon, so the stream starts empty and its
/// positions start at zero, as ALSA's do after prepare.
#[allow(clippy::unnecessary_cast)] // c_ulong is 32 bits on some targets.
unsafe extern "C" fn prepare(io: Io) -> c_int {
    guard(|| unsafe {
        let boundary = boundary((*io).pcm)?;
        let format = client_format((*io).format).ok_or(libc::EINVAL)?;
        let channels = (*io).channels;
        let rate = (*io).rate;
        let buffer = (*io).buffer_size as u64;
        let period = (*io).period_size as u64;
        if !(1..=MAX_CHANNELS).contains(&channels)
            || buffer == 0
            || buffer > u64::from(proto::MAX_BUFFER_FRAMES)
        {
            return Err(libc::EINVAL);
        }
        with(io, |inner| {
            inner.stream = None;
            inner
                .client
                .configure(rate, format, channels as u8, buffer as u32)
                .map_err(|e| errno(&e))?;
            inner.stream = Some(Stream {
                frame_bytes: format.sample_bytes() * channels as usize,
                buffer,
                period: period.max(1),
                boundary,
                written: 0,
                underruns: inner.client.underruns(),
            });
            arm(&inner.client);
            Ok(0)
        })
    })
}

/// Blocks until the daemon has played everything written. Starts the
/// stream first if it was not running.
unsafe extern "C" fn drain(io: Io) -> c_int {
    guard(|| unsafe {
        with(io, |inner| {
            inner.client.drain().map_err(|e| errno(&e))?;
            Ok(0)
        })
    })
}

/// Pausing stops the stream on the daemon. The ring keeps what is in it,
/// so the position holds.
unsafe extern "C" fn pause(io: Io, enable: c_int) -> c_int {
    guard(|| unsafe {
        with(io, |inner| {
            if enable != 0 {
                let stopped = inner.client.stop();
                arm(&inner.client);
                stopped.map_err(|e| errno(&e))?;
            } else {
                inner.client.start().map_err(|e| errno(&e))?;
                let underruns = inner.client.underruns();
                if let Some(stream) = &mut inner.stream {
                    stream.underruns = underruns;
                }
            }
            Ok(0)
        })
    })
}

unsafe extern "C" fn poll_descriptors_count(_io: Io) -> c_int {
    guard(|| Ok(2))
}

/// The wake-up eventfd, readable after each period the daemon plays, and
/// the socket, which hangs up when the daemon goes.
unsafe extern "C" fn poll_descriptors(io: Io, pfd: *mut libc::pollfd, space: c_uint) -> c_int {
    guard(|| unsafe {
        if pfd.is_null() || space < 2 {
            return Err(libc::EINVAL);
        }
        with(io, |inner| {
            let pfds = std::slice::from_raw_parts_mut(pfd, 2);
            for (p, fd) in pfds.iter_mut().zip(inner.client.poll_fds()) {
                *p = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
            }
            Ok(2)
        })
    })
}

/// Turns what poll saw into what the player may do. A hang-up is
/// `POLLERR` and the PCM is disconnected. A stream that is not running
/// gets no wake-up from the daemon, so it keeps the wake-up armed itself
/// and polls as level triggered, as a card does.
unsafe extern "C" fn poll_revents(
    io: Io,
    pfd: *mut libc::pollfd,
    nfds: c_uint,
    revents: *mut c_ushort,
) -> c_int {
    guard(|| unsafe {
        if revents.is_null() || (pfd.is_null() && nfds > 0) {
            return Err(libc::EINVAL);
        }
        let pfds: &[libc::pollfd] = if nfds == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(pfd, nfds as usize)
        };
        let state = state(io);
        let (events, disconnected) = with(io, |inner| {
            let socket = inner.client.poll_fds()[1];
            let hangup = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
            let hung_up = pfds
                .iter()
                .any(|p| p.fd == socket && p.revents & hangup != 0);
            if hung_up || inner.client.check_alive().is_err() {
                return Ok((libc::POLLERR, true));
            }
            clear(&inner.client);
            let Some(stream) = &inner.stream else {
                arm(&inner.client);
                return Ok((libc::POLLERR, false));
            };
            let read = inner.client.hw_position().map_err(|e| errno(&e))?;
            let avail = stream.avail(read);
            let events = match state {
                SND_PCM_STATE_RUNNING if inner.client.underruns() > stream.underruns => {
                    libc::POLLERR
                }
                SND_PCM_STATE_RUNNING if avail >= stream.period => libc::POLLOUT,
                SND_PCM_STATE_RUNNING => 0,
                SND_PCM_STATE_PREPARED | SND_PCM_STATE_PAUSED if avail > 0 => {
                    arm(&inner.client);
                    libc::POLLOUT
                }
                SND_PCM_STATE_PREPARED | SND_PCM_STATE_PAUSED => 0,
                _ => {
                    arm(&inner.client);
                    libc::POLLERR
                }
            };
            Ok((events, false))
        })?;
        if disconnected {
            snd_pcm_ioplug_set_state(io, SND_PCM_STATE_DISCONNECTED);
        } else if state == SND_PCM_STATE_RUNNING && events == libc::POLLERR {
            snd_pcm_ioplug_set_state(io, SND_PCM_STATE_XRUN);
        }
        *revents = events as c_ushort;
        Ok(0)
    })
}

/// Frames between what is written next and the listener.
unsafe extern "C" fn delay(io: Io, delayp: *mut snd_pcm_sframes_t) -> c_int {
    guard(|| unsafe {
        if delayp.is_null() {
            return Err(libc::EINVAL);
        }
        with(io, |inner| {
            inner.client.check_alive().map_err(|e| errno(&e))?;
            *delayp = inner.client.delay_frames() as snd_pcm_sframes_t;
            Ok(0)
        })
    })
}

static CALLBACKS: snd_pcm_ioplug_callback_t = snd_pcm_ioplug_callback_t {
    start: Some(start),
    stop: Some(stop),
    pointer: Some(pointer),
    transfer: Some(transfer),
    close: Some(close),
    hw_params: Some(hw_params),
    hw_free: None,
    sw_params: None,
    prepare: Some(prepare),
    drain: Some(drain),
    pause: Some(pause),
    resume: None,
    poll_descriptors_count: Some(poll_descriptors_count),
    poll_descriptors: Some(poll_descriptors),
    poll_revents: Some(poll_revents),
    dump: None,
    delay: Some(delay),
    query_chmaps: None,
    get_chmap: None,
    set_chmap: None,
};

/// Accepts the fields every PCM definition may have, and no others.
///
/// # Safety
/// `conf` is the PCM's configuration node.
unsafe fn check_config(conf: *mut snd_config_t) -> Result<(), c_int> {
    // SAFETY: walks the node's children as snd_config_for_each does.
    unsafe {
        let end = snd_config_iterator_end(conf);
        let mut i = snd_config_iterator_first(conf);
        while i != end {
            let node = snd_config_iterator_entry(i);
            i = snd_config_iterator_next(i);
            let mut id: *const c_char = null();
            if snd_config_get_id(node, &mut id) < 0 || id.is_null() {
                continue;
            }
            let id = CStr::from_ptr(id);
            if !matches!(id.to_bytes(), b"comment" | b"type" | b"hint") {
                eprintln!("aas: unknown field {}", id.to_string_lossy());
                return Err(libc::EINVAL);
            }
        }
    }
    Ok(())
}

/// Offers what the daemon takes.
///
/// # Safety
/// `io` is created and not yet handed to the player.
unsafe fn constrain(io: Io) -> Result<(), c_int> {
    let check = |rc: c_int| if rc < 0 { Err(-rc) } else { Ok(()) };
    // SAFETY: plain libasound calls on a created ioplug.
    unsafe {
        let list = |kind, values: &[c_uint]| {
            snd_pcm_ioplug_set_param_list(io, kind, values.len() as c_uint, values.as_ptr())
        };
        let minmax = |kind, min, max| snd_pcm_ioplug_set_param_minmax(io, kind, min, max);
        check(list(SND_PCM_IOPLUG_HW_ACCESS, &ACCESS))?;
        check(list(SND_PCM_IOPLUG_HW_FORMAT, &FORMATS))?;
        check(minmax(SND_PCM_IOPLUG_HW_CHANNELS, 1, MAX_CHANNELS))?;
        check(minmax(SND_PCM_IOPLUG_HW_RATE, MIN_RATE, MAX_RATE))?;
        check(minmax(
            SND_PCM_IOPLUG_HW_PERIOD_BYTES,
            MIN_PERIOD_BYTES,
            MAX_BUFFER_BYTES / 2,
        ))?;
        check(minmax(
            SND_PCM_IOPLUG_HW_BUFFER_BYTES,
            2 * MIN_PERIOD_BYTES,
            MAX_BUFFER_BYTES,
        ))?;
        check(minmax(SND_PCM_IOPLUG_HW_PERIODS, 2, MAX_PERIODS))?;
    }
    Ok(())
}

/// # Safety
/// As `_snd_pcm_aas_open`.
unsafe fn open(
    pcmp: *mut *mut snd_pcm_t,
    name: *const c_char,
    conf: *mut snd_config_t,
    stream: snd_pcm_stream_t,
    mode: c_int,
) -> Result<c_int, c_int> {
    unsafe { check_config(conf)? };
    if stream != SND_PCM_STREAM_PLAYBACK {
        return Err(libc::EINVAL);
    }
    let client = Client::connect(proto::default_socket_path()).map_err(|e| errno(&e))?;
    let socket = client.poll_fds()[1];
    let io = Box::into_raw(Box::new(snd_pcm_ioplug_t {
        version: SND_PCM_IOPLUG_VERSION,
        name: NAME.as_ptr(),
        flags: SND_PCM_IOPLUG_FLAG_BOUNDARY_WA,
        poll_fd: socket,
        poll_events: libc::POLLIN as c_uint,
        mmap_rw: 0,
        callback: &CALLBACKS,
        private_data: null_mut(),
        pcm: null_mut(),
        stream: 0,
        state: 0,
        appl_ptr: 0,
        hw_ptr: 0,
        nonblock: 0,
        access: 0,
        format: 0,
        channels: 0,
        rate: 0,
        period_size: 0,
        buffer_size: 0,
    }));
    let shared = Box::into_raw(Box::new(Shared {
        io,
        inner: Mutex::new(Inner {
            client,
            stream: None,
        }),
    }));
    // SAFETY: io and shared are fresh allocations. After create succeeds,
    // libasound owns their release through the close callback.
    unsafe {
        (*io).private_data = shared.cast();
        let rc = snd_pcm_ioplug_create(io, name, stream, mode);
        if rc < 0 {
            drop(Box::from_raw(shared));
            drop(Box::from_raw(io));
            return Err(-rc);
        }
        if let Err(e) = constrain(io) {
            snd_pcm_ioplug_delete(io);
            return Err(e);
        }
        *pcmp = (*io).pcm;
    }
    Ok(0)
}

/// Opens the PCM. libasound finds it by name for `type aas`.
///
/// # Safety
/// Called by libasound with its own valid arguments.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _snd_pcm_aas_open(
    pcmp: *mut *mut snd_pcm_t,
    name: *const c_char,
    _root: *mut snd_config_t,
    conf: *mut snd_config_t,
    stream: snd_pcm_stream_t,
    mode: c_int,
) -> c_int {
    guard(|| unsafe { open(pcmp, name, conf, stream, mode) })
}

/// The symbol libasound checks for the plugin's API version.
#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
pub static __snd_pcm_aas_open_dlsym_pcm_001: c_char = 0;
