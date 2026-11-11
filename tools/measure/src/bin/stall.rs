//! Makes a wake-up late on purpose, for a chosen time.
//!
//! `--mode thread` holds one CPU at a higher real-time priority than the
//! audio thread for `--ms`. An audio thread pinned to that CPU (`taskset
//! -cp`) cannot run for that long, while the card's interrupt is served on
//! time. `--mode irq` makes the kernel itself late: `--timers` timers are
//! set to expire at one instant on the CPU, and the kernel works through
//! them with interrupts off. On the CPU that serves the card's interrupt
//! this delays the interrupt, which a thread cannot do. The length follows
//! from the number of timers and is measured, not chosen.
//!
//! Each stall is printed with its start on CLOCK_MONOTONIC and its length,
//! to be matched against the daemon's log and `tone-probe`.

use clap::{Parser, ValueEnum};
use measure::rt;
use measure::stop;
use std::io::Error;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

const FIFO_PRIORITY: i32 = 99;
/// In irq mode the clock is watched from this long before the timers are
/// due until this long after.
const WATCH_BEFORE_S: f64 = 0.0005;
const WATCH_AFTER_S: f64 = 0.06;

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Thread,
    Irq,
}

#[derive(Parser)]
struct Args {
    #[arg(long, value_enum)]
    mode: Mode,
    /// The CPU to hold.
    #[arg(long)]
    cpu: usize,
    /// Thread mode: length of each stall.
    #[arg(long, default_value_t = 1.0)]
    ms: f64,
    /// Irq mode: timers that expire together.
    #[arg(long, default_value_t = 10_000)]
    timers: usize,
    #[arg(long, default_value_t = 10)]
    repeats: usize,
    /// Time between stalls. Up to 10 ms more is added at random, so that
    /// the stalls do not keep step with the card's periods.
    #[arg(long, default_value_t = 1000)]
    interval_ms: u64,
}

fn os_error(what: &str) -> String {
    format!("{what}: {}", Error::last_os_error())
}

fn pin_to(cpu: usize) -> Result<(), String> {
    // SAFETY: the set is plain data, zeroed and then filled by the libc
    // helpers, and valid for the call.
    let rc = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set)
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(os_error("sched_setaffinity"))
    }
}

/// A small generator for the pause between stalls. Not for anything else.
struct Jitter(u64);

impl Jitter {
    fn next_ms(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 40) as f64 / (1u64 << 24) as f64 * 10.0
    }
}

/// Spins on the clock until `until` and returns the longest time the
/// clock was not read, with its start. That is how long something else
/// held the CPU.
fn longest_hole(until: f64) -> (f64, f64) {
    let mut last = rt::monotonic_s();
    let (mut start, mut longest) = (last, 0.0);
    while last < until {
        let now = rt::monotonic_s();
        if now - last > longest {
            longest = now - last;
            start = last;
        }
        last = now;
    }
    (start, longest)
}

fn stall_thread(args: &Args, jitter: &mut Jitter) {
    for i in 0..args.repeats {
        if stop::requested() {
            break;
        }
        let pause = args.interval_ms as f64 + jitter.next_ms();
        thread::sleep(Duration::from_secs_f64(pause / 1000.0));
        let start = rt::monotonic_s();
        let until = start + args.ms / 1000.0;
        while rt::monotonic_s() < until {
            std::hint::spin_loop();
        }
        let end = rt::monotonic_s();
        println!(
            "stall {i}: at {start:.6} s monotonic for {:.3} ms",
            (end - start) * 1000.0
        );
    }
}

/// Allows this process `wanted` more open files.
fn raise_file_limit(wanted: usize) -> Result<(), String> {
    let limit = libc::rlimit {
        rlim_cur: wanted as libc::rlim_t + 64,
        rlim_max: wanted as libc::rlim_t + 64,
    };
    // SAFETY: limit is a valid rlimit for the call.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } == 0 {
        Ok(())
    } else {
        Err(os_error("setrlimit"))
    }
}

fn stall_irq(args: &Args, jitter: &mut Jitter) -> Result<(), String> {
    raise_file_limit(args.timers)?;
    let mut timers = Vec::with_capacity(args.timers);
    for _ in 0..args.timers {
        // SAFETY: timerfd_create takes no pointers.
        let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC) };
        if fd < 0 {
            return Err(os_error("timerfd_create"));
        }
        timers.push(fd);
    }
    for i in 0..args.repeats {
        if stop::requested() {
            break;
        }
        // Setting the timers takes time, so they are due well after.
        let pause = args.interval_ms as f64 + jitter.next_ms();
        let due = rt::monotonic_s() + pause / 1000.0 + args.timers as f64 * 2e-6;
        let at = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: due as libc::time_t,
                tv_nsec: (due.fract() * 1e9) as libc::c_long,
            },
        };
        for fd in &timers {
            // SAFETY: at is a valid itimerspec, and no old value is asked for.
            let rc = unsafe {
                libc::timerfd_settime(*fd, libc::TFD_TIMER_ABSTIME, &at, std::ptr::null_mut())
            };
            if rc != 0 {
                return Err(os_error("timerfd_settime"));
            }
        }
        let wait = due - WATCH_BEFORE_S - rt::monotonic_s();
        if wait <= 0.0 {
            return Err("setting the timers took longer than the interval".to_string());
        }
        thread::sleep(Duration::from_secs_f64(wait));
        let (start, hole) = longest_hole(due + WATCH_AFTER_S);
        println!(
            "stall {i}: at {start:.6} s monotonic for {:.3} ms",
            hole * 1000.0
        );
    }
    Ok(())
}

fn run(args: &Args) -> Result<(), String> {
    rt::lock_memory()?;
    stop::install();
    pin_to(args.cpu)?;
    rt::set_fifo(FIFO_PRIORITY)?;
    let mut jitter = Jitter((rt::monotonic_s() * 1e9) as u64);
    match args.mode {
        Mode::Thread => stall_thread(args, &mut jitter),
        Mode::Irq => stall_irq(args, &mut jitter)?,
    }
    Ok(())
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
