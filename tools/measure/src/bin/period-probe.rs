//! Can this period size run without xruns? Plays silence and counts them.

use alsa::Direction;
use clap::Parser;
use measure::hw::{self, CHANNELS};
use measure::stats::{Stats, report_header, verdict};
use measure::xrun::{Action, Counter};
use measure::{rt, stop};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const FIFO_PRIORITY: i32 = 70;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "hw:0,0")]
    device: String,
    #[arg(long, default_value_t = 48_000)]
    rate: u32,
    #[arg(long, default_value_t = 64)]
    period: usize,
    #[arg(long, default_value_t = 2)]
    periods: u32,
    #[arg(long, default_value_t = 3600)]
    seconds: u64,
    /// Run without real-time rights. The figures then mean little.
    #[arg(long)]
    no_rt: bool,
}

fn run(args: &Args) -> Result<(), String> {
    stop::install();
    let rt = rt::acquire(FIFO_PRIORITY, args.no_rt)?;
    let pcm = hw::open(
        &args.device,
        Direction::Playback,
        args.rate,
        args.period,
        args.periods,
    )?;
    let io = pcm.io_i32().map_err(|e| format!("io: {e}"))?;
    let silence = vec![0i32; args.period * CHANNELS as usize];
    let warmup = args.rate as usize / args.period;
    let mut wakeups = Stats::with_warmup(warmup);
    let mut xruns = Counter::default();
    let planned = Duration::from_secs(args.seconds);
    let started = Instant::now();
    let mut last = started;
    while started.elapsed() < planned && !stop::requested() {
        let result = match io.writei(&silence) {
            Ok(frames) => frames as i32,
            Err(e) => -e.errno().abs(),
        };
        match xruns.on_write_result(result) {
            Action::Continue => {}
            Action::Recover => pcm.prepare().map_err(|e| format!("recover: {e}"))?,
            Action::Fatal => return Err(format!("write failed: errno {}", -result)),
        }
        let now = Instant::now();
        wakeups.record((now - last).as_nanos() as u64);
        last = now;
    }
    let actual = started.elapsed();
    let s = wakeups.summary();
    println!("{}", report_header(planned, actual, s.count));
    println!(
        "device {} rate {} period {} periods {} rt {}",
        args.device, args.rate, args.period, args.periods, rt
    );
    let buffer_ns =
        args.period as u64 * u64::from(args.periods) * 1_000_000_000 / u64::from(args.rate);
    println!("buffer {:.3} ms", buffer_ns as f64 / 1e6);
    println!("xruns {}", xruns.count());
    println!(
        "wake-up interval ns: count {} min {} mean {} p99 {} p99.9 {} max {}",
        s.count, s.min, s.mean, s.p99, s.p999, s.max
    );
    let complete = actual >= planned && s.count > 0;
    println!(
        "verdict {}",
        verdict(complete, xruns.count(), s.max, buffer_ns)
    );
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
