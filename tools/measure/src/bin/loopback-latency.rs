//! The real delay through a cable from the DAC output to the ADC input.
//!
//! On one device, playback and capture are linked, so they share a sample
//! clock and the delay found is the converters alone. The output queue is
//! added separately to give the time from writing a sample to capturing it.
//!
//! With `--capture` naming another device, such as `--device default` to
//! play through the aas daemon, the two cannot be linked. The time from
//! writing the chirp to capturing it is then measured by the clock. The
//! player's own buffer is taken off, which leaves what the path behind it
//! adds, converters included.

use alsa::Direction;
use alsa::pcm::PCM;
use clap::Parser;
use measure::correlate::{chirp_level_dbfs, find_delay, test_chirp, write_to_capture_frames};
use measure::hw::{self, CHANNELS};
use measure::rt;
use measure::sample::{f64_to_s32, s32_to_f64};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const CHIRP_FRAMES: usize = 2048;
/// The chirp starts here, so the stream has settled by then.
const CHIRP_START: usize = 4096;
const TOTAL_FRAMES: usize = 24_000;
const MIN_PEAK_RATIO: f64 = 2.0;
/// Below this, the correlation has found crosstalk on the board, not a cable.
const MIN_LEVEL_DBFS: f64 = -40.0;
const FIFO_PRIORITY: i32 = 70;
/// Without a link: silence played before the capture opens, while the daemon
/// sets up the card.
const UNLINKED_PREROLL_S: f64 = 0.3;
/// How long the capture may take to open, for a card still changing rate.
const CAPTURE_OPEN_WAIT: Duration = Duration::from_secs(2);
/// Capture running before the chirp is played.
const CAPTURE_LEAD_S: f64 = 0.1;
/// Played after the chirp. Longer than any delay this can measure.
const UNLINKED_TAIL_S: f64 = 0.3;
const CAPTURE_PERIODS: u32 = 4;

#[derive(Parser)]
struct Args {
    /// The playback device.
    #[arg(long, default_value = "hw:0,0")]
    device: String,
    /// The capture device, when it is not the playback device.
    #[arg(long)]
    capture: Option<String>,
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

/// One unlinked run: the chirp's write time, the capture, and when the
/// capture's first frame was taken. Times are seconds from `t0`.
struct Unlinked {
    written_at: f64,
    captured: Vec<f64>,
    first_frame_at: f64,
    period: usize,
    buffer: usize,
}

/// Play the chirp through `args.device` and capture `capture`, timing both.
///
/// The chirp's first block is written into a full buffer, so when the write
/// returns that block is the last thing queued. Each capture read notes the
/// clock and how far the card has captured; the earliest of these gives the
/// time of the first captured frame, since the card's position can only lag.
fn capture_unlinked(args: &Args, capture: &str, chirp: &[f64]) -> Result<Unlinked, String> {
    let channels = CHANNELS as usize;
    let rate = f64::from(args.rate);
    let (out, period, buffer) = hw::open_near(
        &args.device,
        Direction::Playback,
        args.rate,
        args.period,
        args.periods,
    )?;
    let play = out.io_i32().map_err(|e| format!("io: {e}"))?;
    let t0 = Instant::now();
    let since = |t: Instant| t.duration_since(t0).as_secs_f64();
    let mut block = vec![0i32; period * channels];
    let mut heard = vec![0i32; args.period * CAPTURE_PERIODS as usize * channels];
    let mut captured = Vec::new();
    let mut first_frame_at = f64::INFINITY;
    let mut input: Option<PCM> = None;
    let mut open_error = String::new();
    let mut written = 0usize;
    let mut capture_from = None;
    let mut chirp_at = None;
    let mut written_at = 0.0;
    let mut end = usize::MAX;
    let preroll = (UNLINKED_PREROLL_S * rate) as usize;
    let wait_until = (UNLINKED_PREROLL_S + CAPTURE_OPEN_WAIT.as_secs_f64()) * rate;
    while written < end {
        if input.is_none() && written >= preroll {
            match hw::open(
                capture,
                Direction::Capture,
                args.rate,
                args.period,
                CAPTURE_PERIODS,
            ) {
                Ok(pcm) => {
                    pcm.start().map_err(|e| format!("capture start: {e}"))?;
                    input = Some(pcm);
                    capture_from = Some(written + (CAPTURE_LEAD_S * rate) as usize);
                }
                Err(e) if (written as f64) < wait_until => open_error = e,
                Err(e) => return Err(format!("{e} (before that: {open_error})")),
            }
        }
        let starts_chirp = chirp_at.is_none() && capture_from.is_some_and(|at| written >= at);
        if starts_chirp {
            chirp_at = Some(written);
            end = written + chirp.len() + (UNLINKED_TAIL_S * rate) as usize;
        }
        for (i, frame) in block.chunks_exact_mut(channels).enumerate() {
            let value = chirp_at
                .and_then(|c| (written + i).checked_sub(c))
                .and_then(|n| chirp.get(n))
                .map_or(0, |v| f64_to_s32(*v));
            frame.fill(value);
        }
        let mut done = 0;
        while done < block.len() {
            let n = play.writei(&block[done..]).map_err(|e| {
                let capture = if input.is_some() { "open" } else { "not open" };
                format!("write at frame {written}, capture {capture}: {e}")
            })?;
            done += n * channels;
        }
        if starts_chirp {
            written_at = since(Instant::now());
        }
        written += period;
        if let Some(pcm) = &input {
            let record = pcm.io_i32().map_err(|e| format!("io: {e}"))?;
            let mut avail = pcm.avail().map_err(|e| format!("capture: {e}"))? as usize;
            while avail > 0 {
                let want = avail.min(heard.len() / channels);
                let n = record
                    .readi(&mut heard[..want * channels])
                    .map_err(|e| format!("read: {e}"))?;
                captured.extend(
                    heard[..n * channels]
                        .chunks_exact(channels)
                        .map(|f| s32_to_f64(f[0])),
                );
                avail = pcm.avail().map_err(|e| format!("capture: {e}"))? as usize;
                let now = since(Instant::now());
                let position = (captured.len() + avail) as f64;
                first_frame_at = first_frame_at.min(now - position / rate);
            }
        }
    }
    Ok(Unlinked {
        written_at,
        captured,
        first_frame_at,
        period,
        buffer,
    })
}

fn run_unlinked(args: &Args, capture: &str) -> Result<(), String> {
    let chirp = test_chirp(CHIRP_FRAMES);
    let rate = f64::from(args.rate);
    let mut after = Vec::with_capacity(args.repeats);
    let mut geometry = (0, 0);
    for repeat in 0..args.repeats {
        let run = capture_unlinked(args, capture, &chirp)?;
        geometry = (run.period, run.buffer);
        let found = find_delay(&chirp, &run.captured, MIN_PEAK_RATIO)
            .ok_or("no signal: check the cable and input level")?;
        let level = chirp_level_dbfs(&run.captured, found, CHIRP_FRAMES);
        if level < MIN_LEVEL_DBFS {
            return Err(format!(
                "no signal: peak {level:.1} dBFS, check the cable and input level"
            ));
        }
        let heard_at = run.first_frame_at + found as f64 / rate;
        // The block's last frame was the last one queued when the write
        // returned, the way the linked figure counts it.
        let total = heard_at + run.period as f64 / rate - run.written_at;
        let behind = total - run.buffer as f64 / rate;
        println!(
            "repeat {repeat}: write to capture {:.3} ms, after the player's buffer {:.3} ms, \
             peak {level:.1} dBFS",
            total * 1000.0,
            behind * 1000.0
        );
        after.push(behind * 1000.0);
    }
    let min = after.iter().copied().fold(f64::INFINITY, f64::min);
    let max = after.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mean = after.iter().sum::<f64>() / after.len().max(1) as f64;
    println!(
        "rate {} player period {} buffer {}: after the player's buffer \
         min {min:.3} mean {mean:.3} max {max:.3} ms",
        args.rate, geometry.0, geometry.1
    );
    Ok(())
}

fn run(args: &Args) -> Result<(), String> {
    rt::lock_memory()?;
    rt::set_fifo(FIFO_PRIORITY)?;
    if let Some(capture) = &args.capture {
        return run_unlinked(args, capture);
    }
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
