//! Makes a wake-up late on purpose, for a chosen time.
//!
//! `--mode thread` holds one CPU at a higher real-time priority than the
//! audio thread for `--ms`. An audio thread pinned to that CPU (`taskset
//! -cp`) cannot run for that long, while the card's interrupt is served on
//! time. `--mode irq` makes the kernel itself late: `--timers` timers are
//! set to expire at one instant on the CPU, and the kernel works through
//! them with interrupts off. On the CPU that serves the card's interrupt
//! this delays the interrupt, which a thread cannot do. The length follows
//! from the number of timers and is measured, not chosen: a sleep that ends
//! at the same instant returns only when the kernel is through. The timers
//! are set at normal priority, so that setting them holds nobody up. Where the card's
//! interrupt goes to any CPU, as on a Pi 3, name them all (`--cpu 0,1,2,3`):
//! each gets its own timers, due at the same instant, and the stall printed
//! is the time during which all of them were held.
//!
//! Each stall is printed with its start on CLOCK_MONOTONIC and its length,
//! to be matched against the daemon's log and `tone-probe`.

use clap::{Parser, ValueEnum};
use measure::rt;
use measure::stop;
use std::io::Error;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const FIFO_PRIORITY: i32 = 99;
/// Time allowed for setting one timer, on a slow board.
const SET_TIME_S: f64 = 8e-6;

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Thread,
    Irq,
}

#[derive(Parser)]
struct Args {
    #[arg(long, value_enum)]
    mode: Mode,
    /// The CPU to hold. Irq mode takes several, separated by commas.
    #[arg(long, value_delimiter = ',', required = true)]
    cpu: Vec<usize>,
    /// Thread mode: length of each stall.
    #[arg(long, default_value_t = 1.0)]
    ms: f64,
    /// Irq mode: timers that expire together, on each CPU.
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

/// Back to the normal scheduler, for work that must not hold anyone up.
fn set_normal() -> Result<(), String> {
    let param = libc::sched_param { sched_priority: 0 };
    // SAFETY: param is a valid sched_param for the duration of the call.
    if unsafe { libc::sched_setscheduler(0, libc::SCHED_OTHER, &param) } == 0 {
        Ok(())
    } else {
        Err(os_error("sched_setscheduler"))
    }
}

/// Sleeps until `at` on CLOCK_MONOTONIC and returns the time it woke.
fn sleep_until(at: f64) -> f64 {
    let ts = libc::timespec {
        tv_sec: at as libc::time_t,
        tv_nsec: (at.fract() * 1e9) as libc::c_long,
    };
    // SAFETY: ts is a valid timespec, and no remaining time is asked for.
    // A signal cuts the sleep short, which the time returned shows.
    unsafe {
        libc::clock_nanosleep(
            libc::CLOCK_MONOTONIC,
            libc::TIMER_ABSTIME,
            &ts,
            std::ptr::null_mut(),
        );
    }
    rt::monotonic_s()
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

/// The timers of one CPU, made by a thread pinned to it.
fn make_timers(count: usize) -> Result<Vec<libc::c_int>, String> {
    let mut timers = Vec::with_capacity(count);
    for _ in 0..count {
        // SAFETY: timerfd_create takes no pointers.
        let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC) };
        if fd < 0 {
            return Err(os_error("timerfd_create"));
        }
        timers.push(fd);
    }
    Ok(timers)
}

/// Sets the timers to expire at `due` and sleeps until then. Returns `due`
/// and the time the sleep ended, which is when the kernel was through with
/// the timers and with any interrupt that had waited behind them.
fn hold_irqs(timers: &[libc::c_int], due: f64) -> Result<(f64, f64), String> {
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
    for fd in timers {
        // SAFETY: at is a valid itimerspec, and no old value is asked for.
        let rc = unsafe {
            libc::timerfd_settime(*fd, libc::TFD_TIMER_ABSTIME, &at, std::ptr::null_mut())
        };
        if rc != 0 {
            return Err(os_error("timerfd_settime"));
        }
    }
    if rt::monotonic_s() >= due {
        return Err("setting the timers took longer than the time allowed".to_string());
    }
    rt::set_fifo(FIFO_PRIORITY)?;
    let woke = sleep_until(due);
    set_normal()?;
    Ok((due, woke))
}

/// One CPU's part: makes its timers, then holds the CPU at each time sent.
fn irq_worker(
    cpu: usize,
    count: usize,
    dues: &mpsc::Receiver<f64>,
    holes: &mpsc::Sender<Result<(f64, f64), String>>,
) {
    let ready = pin_to(cpu).and_then(|()| make_timers(count));
    let timers = match ready {
        Ok(timers) => timers,
        Err(e) => {
            let _ = holes.send(Err(format!("cpu {cpu}: {e}")));
            return;
        }
    };
    let _ = holes.send(Ok((0.0, 0.0)));
    while let Ok(due) = dues.recv() {
        if holes.send(hold_irqs(&timers, due)).is_err() {
            break;
        }
    }
}

fn stall_irq(args: &Args, jitter: &mut Jitter) -> Result<(), String> {
    raise_file_limit(args.timers * args.cpu.len())?;
    let mut workers = Vec::new();
    for &cpu in &args.cpu {
        let (send_due, dues) = mpsc::channel();
        let (send_hole, holes) = mpsc::channel();
        let count = args.timers;
        thread::spawn(move || irq_worker(cpu, count, &dues, &send_hole));
        workers.push((send_due, holes));
    }
    let answer = |holes: &mpsc::Receiver<Result<(f64, f64), String>>| {
        holes.recv().map_err(|_| "a worker ended".to_string())?
    };
    for (_, holes) in &workers {
        answer(holes)?;
    }
    for i in 0..args.repeats {
        if stop::requested() {
            break;
        }
        // Setting the timers takes time, so they are due well after.
        let pause = args.interval_ms as f64 + jitter.next_ms();
        let due = rt::monotonic_s() + pause / 1000.0 + args.timers as f64 * SET_TIME_S;
        for (send_due, _) in &workers {
            send_due.send(due).map_err(|_| "a worker ended")?;
        }
        // All CPUs are held from the last start to the first end.
        let (mut start, mut end) = (f64::NEG_INFINITY, f64::INFINITY);
        let mut each = String::new();
        for (_, holes) in &workers {
            let (s, e) = answer(holes)?;
            start = start.max(s);
            end = end.min(e);
            each.push_str(&format!(" {:.3}", (e - s) * 1000.0));
        }
        println!(
            "stall {i}: at {start:.6} s monotonic for {:.3} ms, each cpu{each}",
            (end - start).max(0.0) * 1000.0
        );
    }
    Ok(())
}

fn run(args: &Args) -> Result<(), String> {
    rt::lock_memory()?;
    stop::install();
    let mut jitter = Jitter((rt::monotonic_s() * 1e9) as u64);
    match args.mode {
        Mode::Thread => {
            let [cpu] = args.cpu[..] else {
                return Err("thread mode holds one CPU".to_string());
            };
            pin_to(cpu)?;
            rt::set_fifo(FIFO_PRIORITY)?;
            stall_thread(args, &mut jitter);
            Ok(())
        }
        Mode::Irq => stall_irq(args, &mut jitter),
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
