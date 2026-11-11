//! The capture PCM: the daemon's analog input, read out of a capture
//! stream's ring. The same plugin opened for capture.
//!
//! The stream comes in one format only, S32 stereo at the rate the
//! daemon's output runs at, so `plug` does every conversion. libasound
//! takes the constraints at open, before any stream is set up, so open sets
//! up a small stream to learn the rate, and hw_params sets up the one the
//! recorder asks for in its place.
//!
//! Positions are those of playback with the daemon's write position in
//! place of its read position: the frames the daemon has put into the ring
//! since prepare are ALSA's hw pointer. ALSA's application pointer, the
//! frames the recorder has taken, is libasound's to move.
//!
//! `plug` converts through mmap access, and libasound copies a recorder's
//! frames into its mmap buffer on every look at what is there, before the
//! recorder takes any. So frames leave the ring once, in order, into the
//! place in the buffer of their position, and a later copy over the same
//! positions skips those already there.
//!
//! The daemon ends the stream when its output changes rate. The PCM is
//! then disconnected, as for a daemon that has gone, and the recorder
//! opens it again at the new rate.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What a frame of the stream takes: S32 stereo.
const FRAME_BYTES: usize = 8;
const MAX_BUFFER_BYTES: c_uint = proto::MAX_BUFFER_FRAMES * FRAME_BYTES as c_uint;
/// Frames of the stream that tells open the rate. The daemon rounds it up
/// to what it takes.
const PROBE_FRAMES: u32 = 1;
/// Frames copied per step of `transfer`. Kept on the stack.
const CHUNK: usize = 512;

/// Frames the recorder has taken since prepare: ALSA's application
/// pointer, which libasound keeps in the ioplug and moves under its own
/// lock. It wraps at the boundary, which no stream reaches.
///
/// # Safety
/// As for `with`. Without libasound's lock the value may be a moment old.
unsafe fn appl(io: Io) -> u64 {
    // SAFETY: the field lives as long as the PCM and is a c_ulong, which
    // has the size and alignment of a usize. libasound writes it with
    // plain stores under its lock; a caller without that lock reads it
    // as C plugins do, and an aligned word is never seen half written.
    let field = unsafe { AtomicUsize::from_ptr((&raw mut (*io).appl_ptr).cast()) };
    field.load(Ordering::Relaxed) as u64
}

/// The daemon's rate, which the stream will run at. Learnt by setting up a
/// capture stream, which hw_params replaces. Its ring is unmapped at once;
/// the daemon keeps it, stopped, until then.
pub(super) fn rate(client: &mut Client) -> Result<u32, c_int> {
    let rate = client
        .configure_capture(PROBE_FRAMES)
        .map_err(|e| errno(&e))?;
    client.release();
    Ok(rate)
}

impl Stream {
    /// Frames the daemon has recorded since prepare, at its write
    /// position `write`.
    fn recorded(&self, write: u64) -> u64 {
        write.saturating_sub(self.base)
    }

    /// Frames the recorder may read, with the daemon at position `write`
    /// and the recorder at `appl`.
    fn ready(&self, write: u64, appl: u64) -> u64 {
        self.recorded(write).saturating_sub(appl)
    }

    /// The daemon's write position at which the recorder, at `appl`, has
    /// the frames it waits for.
    fn ready_at(&self, appl: u64) -> u64 {
        self.base + appl + self.wanted
    }
}

impl Inner {
    /// Whether the recorder has fallen behind: the daemon has dropped
    /// frames for want of room, or more are waiting than its buffer holds.
    /// Either is an overrun, as on a card.
    fn overrun(&self, stream: &Stream, write: u64, appl: u64) -> bool {
        self.client.underruns() > stream.underruns || stream.ready(write, appl) > stream.buffer
    }

    /// Takes `frames` out of the ring into `out`, or drops them where
    /// `out` is null. Returns the frames taken, fewer if the ring holds
    /// fewer.
    ///
    /// # Safety
    /// A non-null `out` has room for `frames` frames.
    unsafe fn take(&mut self, out: *mut u8, frames: usize) -> Result<usize, c_int> {
        let mut chunk = [[0i32; 2]; CHUNK];
        let mut done = 0;
        while done < frames {
            let want = (frames - done).min(CHUNK);
            let n = self
                .client
                .read(&mut chunk[..want])
                .map_err(|e| errno(&e))?;
            if !out.is_null() {
                // SAFETY: the caller's room, n frames at done. The ring
                // holds S32 in the machine's order, which is little-endian
                // on every target of this plugin.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        chunk.as_ptr().cast::<u8>(),
                        out.add(done * FRAME_BYTES),
                        n * FRAME_BYTES,
                    );
                }
            }
            done += n;
            if n < want {
                break;
            }
        }
        if let Some(stream) = &mut self.stream {
            stream.written += done as u64;
        }
        Ok(done)
    }

    /// Sets up a capture stream with the buffer libasound has chosen. Its
    /// wake-up takes the place of the last one. Fails with `ENODEV` if the
    /// daemon's rate is no longer the one the PCM was opened at: the
    /// recorder has to open it again.
    ///
    /// # Safety
    /// `io` is this PCM's handle, after hw_params.
    #[allow(clippy::unnecessary_cast)] // c_ulong is 32 bits on some targets.
    unsafe fn set_up_capture(&mut self, io: Io) -> Result<(), c_int> {
        let (format, channels, rate, buffer, period) = unsafe {
            let io = &*io;
            (
                io.format,
                io.channels,
                io.rate,
                io.buffer_size as u64,
                io.period_size as u64,
            )
        };
        if format != SND_PCM_FORMAT_S32_LE
            || channels != 2
            || buffer == 0
            || buffer > u64::from(proto::MAX_BUFFER_FRAMES)
        {
            return Err(libc::EINVAL);
        }
        let boundary = self.stream.as_ref().map_or(u64::MAX, |s| s.boundary);
        self.stream = None;
        let told = self
            .client
            .configure_capture(buffer as u32)
            .map_err(|e| errno(&e))?;
        if told != rate {
            self.client.release();
            return Err(libc::ENODEV);
        }
        let wake = self.client.poll_fds()[0];
        // SAFETY: both descriptors are open; dup3 replaces ours in one step.
        let rc = unsafe { libc::dup3(wake, self.wake.as_raw_fd(), libc::O_CLOEXEC) };
        if rc < 0 {
            return Err(errno(&io::Error::last_os_error()));
        }
        self.stream = Some(Stream {
            frame_bytes: FRAME_BYTES,
            buffer,
            wanted: self.avail_min.unwrap_or(period).clamp(1, buffer),
            boundary,
            base: 0,
            written: 0,
            underruns: self.client.underruns(),
            fresh: true,
        });
        self.arm();
        Ok(())
    }

    /// Empties the stream for a prepare and keeps its ring: the daemon
    /// stops writing it, and what it holds is dropped. Sets up a new
    /// stream if the daemon cannot. A stream the daemon has ended stays
    /// ended.
    ///
    /// # Safety
    /// As for `set_up_capture`.
    unsafe fn renew_capture(&mut self, io: Io) -> Result<(), c_int> {
        if self.client.ended() {
            return Err(libc::ENODEV);
        }
        if self.stream.is_none() || self.client.reset().is_err() {
            return unsafe { self.set_up_capture(io) };
        }
        let base = self.client.hw_position().map_err(|e| errno(&e))?;
        let underruns = self.client.underruns();
        if let Some(stream) = &mut self.stream {
            stream.base = base;
            stream.written = 0;
            stream.underruns = underruns;
        }
        Ok(())
    }
}

/// The frames recorded since prepare. A running stream that has overrun
/// is `-EPIPE`, which libasound turns into XRUN.
unsafe extern "C" fn pointer(io: Io) -> snd_pcm_sframes_t {
    guard(|| unsafe {
        let running = state_locked(io) == SND_PCM_STATE_RUNNING;
        let appl = appl(io);
        with(io, |inner| {
            let Some(stream) = &inner.stream else {
                return Ok(0);
            };
            let write = inner.client.hw_position().map_err(|e| errno(&e))?;
            if running && inner.overrun(stream, write, appl) {
                return Err(libc::EPIPE);
            }
            Ok((stream.recorded(write) % stream.boundary) as snd_pcm_sframes_t)
        })
    })
}

/// Copies frames out of the ring. With read access libasound asks for the
/// next `size` frames, into the recorder's buffer, and gets the frames
/// copied, fewer if the ring holds fewer. With mmap access it asks for the
/// frames at `offset` in its buffer, which are those from the recorder's
/// position on: the copy goes on from where the last one ended and skips
/// what is there already.
#[allow(clippy::unnecessary_cast)] // c_ulong is 32 bits on some targets.
unsafe extern "C" fn transfer(
    io: Io,
    areas: *const snd_pcm_channel_area_t,
    offset: snd_pcm_uframes_t,
    size: snd_pcm_uframes_t,
) -> snd_pcm_sframes_t {
    guard(|| unsafe {
        let mmap = (*io).access != SND_PCM_ACCESS_RW_INTERLEAVED;
        // libasound holds its lock for a transfer, so the pointer is
        // current.
        let appl = appl(io);
        with(io, |inner| {
            let stream = inner.stream.as_mut().ok_or(libc::EBADFD)?;
            stream.fresh = false;
            // Interleaved: the first area describes every frame.
            let area = areas.as_ref().ok_or(libc::EINVAL)?;
            if area.first % 8 != 0 || area.step as usize != FRAME_BYTES * 8 {
                return Err(libc::EINVAL);
            }
            let first = area.first as usize / 8 + offset as usize * FRAME_BYTES;
            let out = (area.addr as *mut u8).add(first);
            let size = size as u64;
            if !mmap {
                return Ok(inner.take(out, size as usize)? as snd_pcm_sframes_t);
            }
            // The position of the frame at `offset`, within a buffer from
            // the recorder's position on.
            let buffer = stream.buffer;
            let start = appl + (offset as u64 + buffer - appl % buffer) % buffer;
            let copied = stream.written;
            if copied >= start + size {
                return Ok(size as snd_pcm_sframes_t);
            }
            // Never so, as the copies follow each other: frames up to the
            // start are not wanted any more.
            if copied < start {
                inner.take(null_mut(), (start - copied) as usize)?;
            }
            let skip = copied.saturating_sub(start);
            let out = out.add(skip as usize * FRAME_BYTES);
            let done = inner.take(out, (size - skip) as usize)?;
            Ok((skip + done as u64) as snd_pcm_sframes_t)
        })
    })
}

/// Sets the stream up on the daemon at once, so that a buffer it refuses,
/// or a rate it no longer runs at, fails hw_params.
unsafe extern "C" fn hw_params(io: Io, _params: *mut snd_pcm_hw_params_t) -> c_int {
    guard(|| unsafe { with(io, |inner| inner.set_up_capture(io).map(|_| 0)) })
}

/// Starts the stream empty, its positions at zero, as ALSA's are after
/// prepare. A stream not used since it was set up is kept; any other is
/// emptied in the ring it has, so that a recorder goes on after a stop or
/// an overrun with the same ring and descriptors.
unsafe extern "C" fn prepare(io: Io) -> c_int {
    guard(|| unsafe {
        let boundary = boundary((*io).pcm)?;
        with(io, |inner| {
            if inner.client.ended() {
                return Err(libc::ENODEV);
            }
            if !inner.stream.as_ref().is_some_and(|s| s.fresh) {
                inner.renew_capture(io)?;
            }
            let stream = inner.stream.as_mut().ok_or(libc::EBADFD)?;
            stream.boundary = boundary;
            stream.fresh = false;
            inner.arm();
            Ok(0)
        })
    })
}

/// libasound drains a recorder at its end, as arecord does. A capture
/// stream has nothing to play out, so this returns at once, and libasound
/// then drops the PCM, which stops the stream on the daemon.
unsafe extern "C" fn drain(_io: Io) -> c_int {
    guard(|| Ok(0))
}

/// Turns what poll saw into what the recorder may do, as playback does
/// with `POLLIN` for `POLLOUT`: a running stream with the frames its
/// recorder waits for keeps the wake-up armed, and one without asks the
/// daemon for a wake-up at the write position where it has them. An
/// overrun is `POLLERR` and XRUN.
///
/// A daemon that has gone, or has ended the stream because its rate
/// changed, is `POLLERR` and the PCM is disconnected: the recorder opens
/// it again. In PREPARED and PAUSED a recorder is told only of frames it
/// may read. A drain reports what is left, and `POLLERR` once nothing is.
/// Any other state is `POLLERR`, as on a card.
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
        // Read with the plugin's lock released, see `state`.
        let state = state(io);
        let appl = appl(io);
        let (events, next) = with(io, |inner| {
            let socket = inner.client.poll_fds()[1];
            let hangup = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
            let hung_up = pfds
                .iter()
                .any(|p| p.fd == socket && p.revents & hangup != 0);
            if hung_up || inner.client.check_alive().is_err() || inner.client.ended() {
                inner.arm();
                return Ok((libc::POLLERR, Some(SND_PCM_STATE_DISCONNECTED)));
            }
            inner.clear();
            let Some(stream) = &inner.stream else {
                inner.arm();
                return Ok((libc::POLLERR, None));
            };
            let write = inner.client.hw_position().map_err(|e| errno(&e))?;
            let ready = stream.ready(write, appl);
            let events = match state {
                SND_PCM_STATE_RUNNING if inner.overrun(stream, write, appl) => {
                    inner.arm();
                    libc::POLLERR
                }
                SND_PCM_STATE_RUNNING
                    if ready >= stream.wanted
                        || inner
                            .client
                            .wake_at(stream.ready_at(appl))
                            .map_err(|e| errno(&e))? =>
                {
                    inner.arm();
                    libc::POLLIN
                }
                SND_PCM_STATE_RUNNING => 0,
                SND_PCM_STATE_PREPARED | SND_PCM_STATE_PAUSED if ready >= stream.wanted => {
                    inner.arm();
                    libc::POLLIN
                }
                SND_PCM_STATE_PREPARED | SND_PCM_STATE_PAUSED => 0,
                SND_PCM_STATE_DRAINING if ready > 0 => {
                    inner.arm();
                    libc::POLLIN
                }
                _ => {
                    inner.arm();
                    libc::POLLERR
                }
            };
            let xrun = state == SND_PCM_STATE_RUNNING && events == libc::POLLERR;
            Ok((events, xrun.then_some(SND_PCM_STATE_XRUN)))
        })?;
        // As in playback's poll_revents: a state another thread has set
        // since it was read is the newer one and stays, and a daemon that
        // has gone is gone in any state.
        match next {
            Some(SND_PCM_STATE_DISCONNECTED) => {
                snd_pcm_ioplug_set_state(io, SND_PCM_STATE_DISCONNECTED);
            }
            Some(next) if self::state(io) == state => {
                snd_pcm_ioplug_set_state(io, next);
            }
            _ => {}
        }
        *revents = events as c_ushort;
        Ok(0)
    })
}

/// Frames recorded and not yet taken by the recorder.
unsafe extern "C" fn delay(io: Io, delayp: *mut snd_pcm_sframes_t) -> c_int {
    guard(|| unsafe {
        if delayp.is_null() {
            return Err(libc::EINVAL);
        }
        let appl = appl(io);
        with(io, |inner| {
            inner.client.check_alive().map_err(|e| errno(&e))?;
            let ready = match &inner.stream {
                Some(stream) => {
                    let write = inner.client.hw_position().map_err(|e| errno(&e))?;
                    stream.ready(write, appl)
                }
                None => 0,
            };
            *delayp = ready as snd_pcm_sframes_t;
            Ok(0)
        })
    })
}

/// The callbacks of a capture PCM. Those that do the same in both
/// directions are playback's.
pub(super) static CALLBACKS: snd_pcm_ioplug_callback_t = snd_pcm_ioplug_callback_t {
    start: Some(super::start),
    stop: Some(super::stop),
    pointer: Some(pointer),
    transfer: Some(transfer),
    close: Some(super::close),
    hw_params: Some(hw_params),
    hw_free: Some(super::hw_free),
    sw_params: Some(super::sw_params),
    prepare: Some(prepare),
    drain: Some(drain),
    pause: Some(super::pause),
    resume: None,
    poll_descriptors_count: Some(super::poll_descriptors_count),
    poll_descriptors: Some(super::poll_descriptors),
    poll_revents: Some(poll_revents),
    dump: None,
    delay: Some(delay),
    query_chmaps: None,
    get_chmap: None,
    set_chmap: None,
};

/// Offers the one format, channel count and rate the stream has. Access,
/// buffers and periods as for playback: `plug` needs mmap access for any
/// conversion.
///
/// # Safety
/// `io` is created and not yet handed to the recorder.
pub(super) unsafe fn constrain(io: Io, rate: c_uint) -> Result<(), c_int> {
    let check = |rc: c_int| if rc < 0 { Err(-rc) } else { Ok(()) };
    // SAFETY: plain libasound calls on a created ioplug.
    unsafe {
        let list = |kind, values: &[c_uint]| {
            snd_pcm_ioplug_set_param_list(io, kind, values.len() as c_uint, values.as_ptr())
        };
        let minmax = |kind, min, max| snd_pcm_ioplug_set_param_minmax(io, kind, min, max);
        check(list(SND_PCM_IOPLUG_HW_ACCESS, &ACCESS))?;
        check(list(
            SND_PCM_IOPLUG_HW_FORMAT,
            &[SND_PCM_FORMAT_S32_LE as c_uint],
        ))?;
        check(minmax(SND_PCM_IOPLUG_HW_CHANNELS, 2, 2))?;
        check(minmax(SND_PCM_IOPLUG_HW_RATE, rate, rate))?;
        check(minmax(
            SND_PCM_IOPLUG_HW_PERIOD_BYTES,
            MIN_PERIOD_BYTES,
            MAX_BUFFER_BYTES / 2,
        ))?;
        check(minmax(
            SND_PCM_IOPLUG_HW_BUFFER_BYTES,
            MIN_BUFFER_BYTES,
            MAX_BUFFER_BYTES,
        ))?;
        check(minmax(SND_PCM_IOPLUG_HW_PERIODS, 2, MAX_PERIODS))?;
    }
    Ok(())
}
