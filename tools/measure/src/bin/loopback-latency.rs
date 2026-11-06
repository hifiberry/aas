//! The real delay through a cable from the DAC output to the ADC input.
//!
//! Playback and capture are linked, so they share a sample clock and the delay
//! found is the converters alone. The output queue is added separately to give
//! the time from writing a sample to capturing it.

use alsa::Direction;
use alsa::pcm::PCM;
use clap::Parser;
use measure::correlate::{chirp_level_dbfs, find_delay, test_chirp, write_to_capture_frames};
use measure::hw::{self, CHANNELS};
use measure::rt;
use measure::sample::{f64_to_s32, s32_to_f64};
use std::process::ExitCode;

const CHIRP_FRAMES: usize = 2048;
/// The chirp starts here, so the stream has settled by then.
const CHIRP_START: usize = 4096;
const TOTAL_FRAMES: usize = 24_000;
const MIN_PEAK_RATIO: f64 = 2.0;
/// Below this, the correlation has found crosstalk on the board, not a cable.
const MIN_LEVEL_DBFS: f64 = -40.0;
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
    #[arg(long, default_value_t = 20)]
    repeats: usize,
}

/// Play the chirp and return the left channel of what came back.
fn capture_once(args: &Args, chirp: &[f64]) -> Result<Vec<f64>, String> {
    let channels = CHANNELS as usize;
    let out = hw::open(
        &args.device,
        Direction::Playback,
        args.rate,
        args.period,
        args.periods,
    )?;
    let inp = hw::open(
        &args.device,
        Direction::Capture,
        args.rate,
        args.period,
        args.periods,
    )?;
    PCM::link(&out, &inp).map_err(|e| format!("link: {e}"))?;
    let play = out.io_i32().map_err(|e| format!("io: {e}"))?;
    let record = inp.io_i32().map_err(|e| format!("io: {e}"))?;
    let mut block = vec![0i32; args.period * channels];
    let mut heard = vec![0i32; args.period * channels];
    let mut captured = Vec::with_capacity(TOTAL_FRAMES);
    let fill = |block: &mut [i32], first: usize| {
        for (i, frame) in block.chunks_exact_mut(channels).enumerate() {
            let at = first + i;
            let value = at
                .checked_sub(CHIRP_START)
                .and_then(|n| chirp.get(n))
                .map_or(0, |v| f64_to_s32(*v));
            frame.fill(value);
        }
    };
    let mut written = 0;
    for _ in 0..args.periods {
        fill(&mut block, written);
        play.writei(&block).map_err(|e| format!("prefill: {e}"))?;
        written += args.period;
    }
    while captured.len() < TOTAL_FRAMES {
        record.readi(&mut heard).map_err(|e| format!("read: {e}"))?;
        captured.extend(heard.chunks_exact(channels).map(|f| s32_to_f64(f[0])));
        fill(&mut block, written);
        play.writei(&block).map_err(|e| format!("write: {e}"))?;
        written += args.period;
    }
    Ok(captured)
}

fn run(args: &Args) -> Result<(), String> {
    rt::lock_memory()?;
    rt::set_fifo(FIFO_PRIORITY)?;
    let chirp = test_chirp(CHIRP_FRAMES);
    let mut delays = Vec::with_capacity(args.repeats);
    for repeat in 0..args.repeats {
        let captured = capture_once(args, &chirp)?;
        let found = find_delay(&chirp, &captured, MIN_PEAK_RATIO)
            .ok_or("no signal: check the cable and input level")?;
        let level = chirp_level_dbfs(&captured, found, CHIRP_FRAMES);
        if level < MIN_LEVEL_DBFS {
            return Err(format!(
                "no signal: peak {level:.1} dBFS, check the cable and input level"
            ));
        }
        let delay = found as i64 - CHIRP_START as i64;
        let ms = delay as f64 / f64::from(args.rate) * 1000.0;
        println!("repeat {repeat}: converters {delay} frames, {ms:.3} ms, peak {level:.1} dBFS");
        delays.push(delay);
    }
    let min = delays.iter().min().copied().unwrap_or(0);
    let max = delays.iter().max().copied().unwrap_or(0);
    let mean = delays.iter().sum::<i64>() as f64 / delays.len().max(1) as f64;
    let total = write_to_capture_frames(mean.round() as i64, args.period, args.periods);
    println!(
        "rate {} period {} periods {}: converters min {min} mean {mean:.1} max {max} frames, \
         mean {:.3} ms; write to capture {total} frames, {:.3} ms",
        args.rate,
        args.period,
        args.periods,
        mean / f64::from(args.rate) * 1000.0,
        total as f64 / f64::from(args.rate) * 1000.0
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
