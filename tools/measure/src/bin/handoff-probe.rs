//! What does waking another process through shared memory and an eventfd cost?
//! The parent stands in for the daemon and the child for a client.

use clap::Parser;
use measure::handoff::{RING, Tracker};
use measure::stats::{Stats, report_header};
use measure::{rt, stop};
use std::process::ExitCode;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PARENT_PRIORITY: i32 = 70;
const CHILD_PRIORITY: i32 = 60;
const NANOS: i64 = 1_000_000_000;
/// A second of wake-ups at the default period.
const WARMUP: usize = 750;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 1333)]
    period_us: u64,
    #[arg(long, default_value_t = 60)]
    seconds: u64,
    /// Run without real-time rights. The figures then mean little.
    #[arg(long)]
    no_rt: bool,
}

#[repr(C)]
struct Shared {
    sent_at: [AtomicU64; RING as usize],
    sequence: AtomicU64,
    done: AtomicBool,
}

fn now() -> libc::timespec {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: t is a valid timespec for the duration of the call.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    t
}

fn nanos(t: libc::timespec) -> u64 {
    t.tv_sec as u64 * NANOS as u64 + t.tv_nsec as u64
}

fn map_shared() -> Result<&'static Shared, String> {
    let size = size_of::<Shared>();
    // SAFETY: the name is a valid C string and the fd is checked below.
    let fd = unsafe { libc::memfd_create(c"handoff".as_ptr(), 0) };
    // SAFETY: fd is checked by the result of ftruncate.
    if fd < 0 || unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 {
        return Err(format!("memfd: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: mapping a fresh memfd of `size` bytes, which reads as zeros, and
    // all-zero bytes are valid for every field of Shared.
    unsafe {
        let p = libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if p == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        Ok(&*p.cast::<Shared>())
    }
}

fn signal(fd: i32) {
    let one = 1u64;
    // SAFETY: writes 8 bytes from a valid u64 to an eventfd.
    unsafe { libc::write(fd, (&raw const one).cast(), 8) };
}

/// `started` is taken before the fork, so the child's report covers the
/// sender's whole run and does not read as cut short.
fn child(shared: &Shared, fd: i32, args: &Args, started: Instant) -> Result<(), String> {
    let rt = rt::acquire(CHILD_PRIORITY, args.no_rt)?;
    let mut latency = Stats::with_warmup(WARMUP);
    let mut tracker = Tracker::default();
    loop {
        let mut count = 0u64;
        // SAFETY: reads 8 bytes into a valid u64 from an eventfd.
        let n = unsafe { libc::read(fd, (&raw mut count).cast(), 8) };
        let woke = nanos(now());
        if shared.done.load(Ordering::Acquire) {
            break;
        }
        if n != 8 {
            continue;
        }
        for sequence in tracker.on_wake(shared.sequence.load(Ordering::Acquire)) {
            let sent = shared.sent_at[(sequence % RING) as usize].load(Ordering::Acquire);
            latency.record(woke.saturating_sub(sent));
        }
    }
    let s = latency.summary();
    println!(
        "{}",
        report_header(
            Duration::from_secs(args.seconds),
            started.elapsed(),
            s.count
        )
    );
    println!("period {} us rt {}", args.period_us, rt);
    println!("missed wake-ups {}", tracker.missed());
    println!(
        "wake-up latency ns: count {} min {} mean {} p99 {} p99.9 {} max {}",
        s.count, s.min, s.mean, s.p99, s.p999, s.max
    );
    Ok(())
}

fn parent(shared: &Shared, fd: i32, args: &Args, started: Instant) -> Result<(), String> {
    if !rt::acquire(PARENT_PRIORITY, args.no_rt)? {
        println!("sender runs without real-time rights");
    }
    let period = args.period_us as i64 * 1000;
    let mut next = now();
    let planned = Duration::from_secs(args.seconds);
    let mut sequence = 0u64;
    while started.elapsed() < planned && !stop::requested() {
        next.tv_nsec += period;
        if next.tv_nsec >= NANOS {
            next.tv_nsec -= NANOS;
            next.tv_sec += 1;
        }
        // SAFETY: next is a valid timespec and the remainder is unused.
        unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_MONOTONIC,
                libc::TIMER_ABSTIME,
                &next,
                ptr::null_mut(),
            )
        };
        sequence += 1;
        shared.sent_at[(sequence % RING) as usize].store(nanos(now()), Ordering::Release);
        shared.sequence.store(sequence, Ordering::Release);
        signal(fd);
    }
    Ok(())
}

fn run(args: &Args) -> Result<(), String> {
    stop::install();
    let shared = map_shared()?;
    // SAFETY: eventfd takes no pointers.
    let fd = unsafe { libc::eventfd(0, 0) };
    if fd < 0 {
        return Err(format!("eventfd: {}", std::io::Error::last_os_error()));
    }
    let started = Instant::now();
    // SAFETY: this process is single-threaded here, so fork is safe.
    match unsafe { libc::fork() } {
        -1 => Err(format!("fork: {}", std::io::Error::last_os_error())),
        0 => child(shared, fd, args, started),
        pid => {
            let result = parent(shared, fd, args, started);
            // Also when the sender failed, or the child would wait for ever.
            shared.done.store(true, Ordering::Release);
            signal(fd);
            let mut status = 0;
            // SAFETY: status is a valid int and pid is our child.
            unsafe { libc::waitpid(pid, &mut status, 0) };
            if result.is_ok() && status != 0 {
                return Err(format!("child exited with status {status}"));
            }
            result
        }
    }
}

fn main() -> ExitCode {
    match run(&Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
