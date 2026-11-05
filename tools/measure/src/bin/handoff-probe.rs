//! What does waking another process through shared memory and an eventfd cost?
//! The parent stands in for the daemon and the child for a client.

use clap::Parser;
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
    sent_at: AtomicU64,
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

fn set_priority(priority: i32, no_rt: bool) -> Result<(), String> {
    match rt::lock_memory().and_then(|()| rt::set_fifo(priority)) {
        Err(e) if no_rt => {
            eprintln!("continuing without real-time rights: {e}");
            Ok(())
        }
        other => other,
    }
}

fn signal(fd: i32) {
    let one = 1u64;
    // SAFETY: writes 8 bytes from a valid u64 to an eventfd.
    unsafe { libc::write(fd, (&raw const one).cast(), 8) };
}

fn child(shared: &Shared, fd: i32, args: &Args) -> Result<(), String> {
    set_priority(CHILD_PRIORITY, args.no_rt)?;
    let mut latency = Stats::with_warmup(WARMUP);
    let mut gaps = 0u64;
    let mut last = 0u64;
    let started = Instant::now();
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
        let sequence = shared.sequence.load(Ordering::Acquire);
        if sequence != last + 1 {
            gaps += 1;
        }
        last = sequence;
        latency.record(woke.saturating_sub(shared.sent_at.load(Ordering::Acquire)));
    }
    let s = latency.summary();
    println!(
        "{}",
        report_header(Duration::from_secs(args.seconds), started.elapsed())
    );
    println!("period {} us rt {}", args.period_us, !args.no_rt);
    println!("sequence gaps {gaps}");
    println!(
        "wake-up latency ns: count {} min {} mean {} p99 {} p99.9 {} max {}",
        s.count, s.min, s.mean, s.p99, s.p999, s.max
    );
    Ok(())
}

fn parent(shared: &Shared, fd: i32, args: &Args) -> Result<(), String> {
    set_priority(PARENT_PRIORITY, args.no_rt)?;
    let period = args.period_us as i64 * 1000;
    let mut next = now();
    let planned = Duration::from_secs(args.seconds);
    let started = Instant::now();
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
        shared.sent_at.store(nanos(now()), Ordering::Release);
        shared.sequence.store(sequence, Ordering::Release);
        signal(fd);
    }
    shared.done.store(true, Ordering::Release);
    signal(fd);
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
    // SAFETY: this process is single-threaded here, so fork is safe.
    match unsafe { libc::fork() } {
        -1 => Err(format!("fork: {}", std::io::Error::last_os_error())),
        0 => child(shared, fd, args),
        pid => {
            let result = parent(shared, fd, args);
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
