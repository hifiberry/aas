//! Single producer, single consumer ring of stereo `i32` frames in shared
//! memory. The writer is the client, the reader is the daemon.
//!
//! The mapped memory is written by another process, so nothing in it is
//! trusted. The capacity is read once at map time and kept in the `Ring`.
//! Positions are taken modulo that capacity and a fill level above the
//! capacity is treated as an empty ring. None of the data path methods
//! allocate or block. The mapping is populated and locked when it is made,
//! so they do not page fault either, as long as mlock is permitted and the
//! peer calls `seal_writes` before it can punch holes.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const MAGIC: u32 = 0x4141_5352;
pub const VERSION: u32 = 1;

/// Bytes before the frame data. Keeps the data 64 byte aligned.
const DATA_OFFSET: usize = 64;
const FRAME_BYTES: usize = std::mem::size_of::<[i32; 2]>();

#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub version: u32,
    pub capacity_frames: u32,
    /// Frames written since creation. Never wraps back.
    pub write_pos: AtomicU64,
    /// Frames consumed since creation. Never wraps back.
    pub read_pos: AtomicU64,
    pub underruns: AtomicU64,
    pub output_delay_frames: AtomicU32,
    pub draining: AtomicU32,
}

const _: () = assert!(std::mem::size_of::<Header>() <= DATA_OFFSET);

pub struct Ring {
    base: NonNull<u8>,
    map_len: usize,
    capacity: usize,
}

// SAFETY: the mapping is plain shared memory and all shared mutable state is
// accessed through atomics or the single producer and consumer discipline.
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

impl Ring {
    /// Creates an anonymous shared ring and returns it with the memfd to hand
    /// to the peer.
    pub fn create(capacity_frames: u32) -> io::Result<(Ring, OwnedFd)> {
        if capacity_frames == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let len = DATA_OFFSET + capacity_frames as usize * FRAME_BYTES;
        // SAFETY: plain syscalls with a valid NUL terminated name.
        let raw = unsafe {
            libc::memfd_create(
                c"aas-ring".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a fresh descriptor that we own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // A peer that shrinks the file would turn reads of the mapping into SIGBUS.
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let base = Self::mmap(fd.as_raw_fd(), len)?;
        // SAFETY: the mapping is at least DATA_OFFSET bytes, page aligned and
        // zero filled. We are the only holder until the fd is passed on.
        unsafe {
            let h = base.as_ptr() as *mut Header;
            (&raw mut (*h).magic).write(MAGIC);
            (&raw mut (*h).version).write(VERSION);
            (&raw mut (*h).capacity_frames).write(capacity_frames);
        }
        let ring = Ring {
            base,
            map_len: len,
            capacity: capacity_frames as usize,
        };
        Ok((ring, fd))
    }

    /// Maps a ring created by the peer. Rejects a wrong magic or version, a
    /// zero capacity and a file too small for the capacity it claims.
    pub fn map(fd: BorrowedFd) -> io::Result<Ring> {
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
        // SAFETY: st is only read after fstat filled it in.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let len = usize::try_from(st.st_size).map_err(|_| bad("bad size"))?;
        if len < DATA_OFFSET {
            return Err(bad("ring file too small"));
        }
        let base = Self::mmap(fd.as_raw_fd(), len)?;
        // Own the mapping now so that it is released on every error below.
        let mut ring = Ring {
            base,
            map_len: len,
            capacity: 0,
        };
        let h = ring.header();
        if h.magic != MAGIC {
            return Err(bad("bad ring magic"));
        }
        if h.version != VERSION {
            return Err(bad("unsupported ring version"));
        }
        let capacity = h.capacity_frames as usize;
        if capacity == 0 || DATA_OFFSET + capacity * FRAME_BYTES > len {
            return Err(bad("ring capacity does not fit the file"));
        }
        ring.capacity = capacity;
        Ok(ring)
    }

    fn mmap(fd: i32, len: usize) -> io::Result<NonNull<u8>> {
        // SAFETY: a fresh shared mapping, the result is checked.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // Map every page now, for writing, so the audio path never faults.
        // MAP_POPULATE would map a shared mapping's pages clean. A CPU that
        // cannot set the dirty bit in hardware, such as the Pi 3's
        // Cortex-A53, then faults on the first write to each page, and a
        // later MADV_POPULATE_WRITE does not help: the pages count as
        // writable already. Populating for writing an empty mapping marks
        // them dirty, and writes nothing.
        // SAFETY: the range is the mapping just made.
        if unsafe { libc::madvise(p, len, libc::MADV_POPULATE_WRITE) } < 0 {
            // Before Linux 5.14: map them for reading, as MAP_POPULATE does.
            for offset in (0..len).step_by(4096) {
                // SAFETY: inside the mapping.
                unsafe { std::ptr::read_volatile((p as *const u8).add(offset)) };
            }
        }
        // Keep the pages resident. A refused mlock only costs latency.
        unsafe { libc::mlock(p, len) };
        NonNull::new(p as *mut u8).ok_or_else(|| io::Error::from(io::ErrorKind::OutOfMemory))
    }

    pub fn header(&self) -> &Header {
        // SAFETY: the mapping is at least DATA_OFFSET bytes, aligned, and
        // lives as long as self. Atomic fields tolerate concurrent access.
        unsafe { &*(self.base.as_ptr() as *const Header) }
    }

    /// Frame capacity as fixed at creation or map time.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn data(&self) -> *mut [i32; 2] {
        // SAFETY: DATA_OFFSET is inside the mapping.
        unsafe { self.base.as_ptr().add(DATA_OFFSET) as *mut [i32; 2] }
    }

    /// Fill level for positions `w` and `r`. A value above the capacity can
    /// only come from a corrupt peer and counts as empty.
    fn fill(&self, w: u64, r: u64) -> usize {
        let n = w.wrapping_sub(r);
        if n > self.capacity as u64 {
            0
        } else {
            n as usize
        }
    }

    /// Frames ready to read.
    pub fn available(&self) -> usize {
        let h = self.header();
        let w = h.write_pos.load(Ordering::Acquire);
        let r = h.read_pos.load(Ordering::Relaxed);
        self.fill(w, r)
    }

    /// Frames that can be written.
    pub fn free(&self) -> usize {
        let h = self.header();
        let w = h.write_pos.load(Ordering::Relaxed);
        let r = h.read_pos.load(Ordering::Acquire);
        self.capacity - self.fill(w, r)
    }

    /// Writes as many frames as fit and returns the count.
    pub fn write(&self, frames: &[[i32; 2]]) -> usize {
        let h = self.header();
        let w = h.write_pos.load(Ordering::Relaxed);
        let r = h.read_pos.load(Ordering::Acquire);
        let n = frames.len().min(self.capacity - self.fill(w, r));
        let start = (w % self.capacity as u64) as usize;
        let first = n.min(self.capacity - start);
        // SAFETY: start + first <= capacity and n - first <= start, and the
        // region lies inside the mapping. The reader does not touch frames
        // beyond the published write position.
        unsafe {
            std::ptr::copy_nonoverlapping(frames.as_ptr(), self.data().add(start), first);
            std::ptr::copy_nonoverlapping(frames.as_ptr().add(first), self.data(), n - first);
        }
        h.write_pos
            .store(w.wrapping_add(n as u64), Ordering::Release);
        n
    }

    /// Copies up to `out.len()` frames without consuming them.
    pub fn peek(&self, out: &mut [[i32; 2]]) -> usize {
        let h = self.header();
        let r = h.read_pos.load(Ordering::Relaxed);
        let w = h.write_pos.load(Ordering::Acquire);
        let n = out.len().min(self.fill(w, r));
        let start = (r % self.capacity as u64) as usize;
        let first = n.min(self.capacity - start);
        // SAFETY: as in write, with the roles reversed.
        unsafe {
            std::ptr::copy_nonoverlapping(self.data().add(start), out.as_mut_ptr(), first);
            std::ptr::copy_nonoverlapping(self.data(), out.as_mut_ptr().add(first), n - first);
        }
        n
    }

    /// Drops up to `frames` frames from the read side.
    pub fn consume(&self, frames: usize) {
        let h = self.header();
        let r = h.read_pos.load(Ordering::Relaxed);
        let w = h.write_pos.load(Ordering::Acquire);
        let n = frames.min(self.fill(w, r));
        h.read_pos
            .store(r.wrapping_add(n as u64), Ordering::Release);
    }

    /// Reads and consumes up to `out.len()` frames.
    pub fn read(&self, out: &mut [[i32; 2]]) -> usize {
        let n = self.peek(out);
        self.consume(n);
        n
    }
}

/// Stops any further writable mapping of the ring and any hole punching
/// under existing mappings. Mappings made before the call stay writable.
///
/// Call this after the client has mapped the ring, and not earlier, because
/// a later writable `Ring::map` is refused. `Ring::map` is therefore only for
/// clients mapping rings the daemon created. Without this seal a client could
/// punch holes under the daemon's mapping and make the audio thread fault.
pub fn seal_writes(fd: BorrowedFd) -> io::Result<()> {
    // SAFETY: plain fcntl on a descriptor we borrow.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_FUTURE_WRITE) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: base and map_len describe the mapping made in create or map.
        unsafe { libc::munmap(self.base.as_ptr() as *mut libc::c_void, self.map_len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(range: std::ops::Range<i32>) -> Vec<[i32; 2]> {
        range.map(|i| [i, -i]).collect()
    }

    #[test]
    fn written_frames_read_back_in_order() {
        let (ring, _fd) = Ring::create(8).unwrap();
        // Move the positions near the end so the next write wraps.
        assert_eq!(ring.write(&frames(0..6)), 6);
        let mut sink = [[0; 2]; 6];
        assert_eq!(ring.read(&mut sink), 6);
        let input = frames(100..105);
        assert_eq!(ring.write(&input), 5);
        assert_eq!(ring.available(), 5);
        let mut peeked = [[0; 2]; 5];
        assert_eq!(ring.peek(&mut peeked), 5);
        assert_eq!(peeked.to_vec(), input);
        assert_eq!(ring.available(), 5);
        let mut out = [[0; 2]; 8];
        assert_eq!(ring.read(&mut out), 5);
        assert_eq!(out[..5].to_vec(), input);
        assert_eq!(ring.available(), 0);
        assert_eq!(ring.free(), 8);
    }

    #[test]
    fn write_stops_at_capacity() {
        let (ring, _fd) = Ring::create(4).unwrap();
        assert_eq!(ring.write(&frames(0..10)), 4);
        assert_eq!(ring.free(), 0);
        assert_eq!(ring.write(&frames(0..1)), 0);
        ring.consume(100);
        assert_eq!(ring.available(), 0);
    }

    #[test]
    fn peer_mapping_sees_the_same_frames() {
        let (writer, fd) = Ring::create(16).unwrap();
        let reader = Ring::map(std::os::fd::AsFd::as_fd(&fd)).unwrap();
        assert_eq!(reader.capacity(), 16);
        writer.write(&frames(0..3));
        let mut out = [[0; 2]; 4];
        assert_eq!(reader.read(&mut out), 3);
        assert_eq!(out[2], [2, -2]);
        assert_eq!(writer.free(), 16);
    }

    #[test]
    fn corrupt_positions_are_clamped() {
        let (ring, _fd) = Ring::create(8).unwrap();
        ring.write(&frames(0..4));
        let h = ring.header();
        let r = h.read_pos.load(Ordering::Relaxed);
        for bad in [r + 80, r.wrapping_sub(1), u64::MAX] {
            h.write_pos.store(bad, Ordering::Relaxed);
            assert!(ring.available() <= 8);
            assert!(ring.free() <= 8);
            let mut out = [[0; 2]; 32];
            assert!(ring.peek(&mut out) <= 8);
            assert!(ring.read(&mut out) <= 8);
            ring.consume(usize::MAX);
            assert!(ring.write(&out) <= 8);
        }
        // A corrupt read position is handled the same way.
        h.write_pos.store(5, Ordering::Relaxed);
        h.read_pos.store(1000, Ordering::Relaxed);
        let mut out = [[0; 2]; 32];
        assert_eq!(ring.read(&mut out), 0);
        assert!(ring.write(&out) <= 8);
    }

    fn memfd(len: usize) -> OwnedFd {
        let raw = unsafe { libc::memfd_create(c"t".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw >= 0);
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        assert_eq!(
            unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) },
            0
        );
        fd
    }

    #[test]
    fn map_rejects_a_wrong_magic_or_a_short_file() {
        use std::os::fd::AsFd;
        let (_ring, fd) = Ring::create(8).unwrap();
        let short = memfd(10);
        assert!(Ring::map(short.as_fd()).is_err());
        // Zero filled, so the magic is wrong.
        let blank = memfd(4096);
        assert!(Ring::map(blank.as_fd()).is_err());
        // Right header, but the file is smaller than the capacity it claims.
        let small = memfd(128);
        let base = Ring::mmap(small.as_raw_fd(), 128).unwrap();
        let ring = Ring {
            base,
            map_len: 128,
            capacity: 0,
        };
        unsafe {
            let h = ring.base.as_ptr() as *mut Header;
            (&raw mut (*h).magic).write(MAGIC);
            (&raw mut (*h).version).write(VERSION);
            (&raw mut (*h).capacity_frames).write(1000);
        }
        assert!(Ring::map(small.as_fd()).is_err());
        assert!(Ring::map(fd.as_fd()).is_ok());
    }

    fn fake(len: usize, magic: u32, version: u32, capacity: u32) -> OwnedFd {
        let fd = memfd(len);
        let mut head = Vec::new();
        for w in [magic, version, capacity] {
            head.extend_from_slice(&w.to_ne_bytes());
        }
        let n = unsafe { libc::pwrite(fd.as_raw_fd(), head.as_ptr().cast(), head.len(), 0) };
        assert_eq!(n, head.len() as isize);
        fd
    }

    fn errno() -> i32 {
        io::Error::last_os_error().raw_os_error().unwrap()
    }

    #[test]
    fn map_checks_version_and_capacity_one_by_one() {
        use std::os::fd::AsFd;
        let ok = fake(4096, MAGIC, VERSION, 8);
        assert!(Ring::map(ok.as_fd()).is_ok());
        let version = fake(4096, MAGIC, VERSION + 1, 8);
        assert!(Ring::map(version.as_fd()).is_err());
        let zero = fake(4096, MAGIC, VERSION, 0);
        assert!(Ring::map(zero.as_fd()).is_err());
    }

    #[test]
    fn the_memfd_cannot_be_resized() {
        let (_ring, fd) = Ring::create(1024).unwrap();
        assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), 64) }, -1);
        assert_eq!(errno(), libc::EPERM);
        assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), 1 << 24) }, -1);
        assert_eq!(errno(), libc::EPERM);
    }

    #[test]
    fn consume_is_clamped_to_what_is_available() {
        let (ring, _fd) = Ring::create(8).unwrap();
        ring.write(&frames(0..4));
        ring.consume(100);
        assert_eq!(ring.header().read_pos.load(Ordering::Relaxed), 4);
        assert_eq!(ring.write(&frames(10..18)), 8);
        let mut out = [[0; 2]; 8];
        assert_eq!(ring.read(&mut out), 8);
        assert_eq!(out.to_vec(), frames(10..18));
    }

    #[test]
    fn seal_writes_stops_hole_punching_and_new_writable_maps() {
        use std::os::fd::AsFd;
        let (ring, fd) = Ring::create(4096).unwrap();
        let punch = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
        // Before the seal a peer can punch a hole under a live mapping.
        assert_eq!(
            unsafe { libc::fallocate(fd.as_raw_fd(), punch, 4096, 4096) },
            0
        );
        seal_writes(fd.as_fd()).unwrap();
        assert_eq!(
            unsafe { libc::fallocate(fd.as_raw_fd(), punch, 4096, 4096) },
            -1
        );
        assert_eq!(errno(), libc::EPERM);
        assert!(Ring::map(fd.as_fd()).is_err());
        // The mapping made before the seal keeps working.
        assert_eq!(ring.write(&frames(0..100)), 100);
        let mut out = [[0; 2]; 100];
        assert_eq!(ring.read(&mut out), 100);
        assert_eq!(out.to_vec(), frames(0..100));
    }

    fn thread_minflt() -> i64 {
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) }, 0);
        ru.ru_minflt
    }

    #[test]
    fn a_fresh_ring_does_not_fault_on_a_first_full_pass() {
        // Run on its own thread so other tests do not count.
        std::thread::spawn(|| {
            let (ring, _fd) = Ring::create(16384).unwrap();
            let input = frames(0..16384);
            // Non-zero so the allocator cannot hand out lazily zeroed pages.
            let mut out = vec![[1; 2]; 16384];
            let before = thread_minflt();
            assert_eq!(ring.write(&input), 16384);
            assert_eq!(ring.read(&mut out), 16384);
            assert_eq!(thread_minflt() - before, 0);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn two_threads_see_every_frame_in_order() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        const TOTAL: i32 = 1_000_000;
        let (ring, _fd) = Ring::create(512).unwrap();
        let ring = Arc::new(ring);
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (ring, stop) = (ring.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut next = 0;
                while next < TOTAL && !stop.load(Ordering::Relaxed) {
                    let end = (next + 100).min(TOTAL);
                    let n = ring.write(&frames(next..end));
                    next += n as i32;
                    if n == 0 {
                        std::hint::spin_loop();
                    }
                }
            })
        };
        let mut expect = 0;
        let mut out = [[0; 2]; 64];
        let mut ok = true;
        while expect < TOTAL && ok {
            let n = ring.read(&mut out);
            if n == 0 {
                std::hint::spin_loop();
            }
            for f in &out[..n] {
                ok &= *f == [expect, -expect];
                expect += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        assert!(ok, "frame out of order near {expect}");
    }
}
