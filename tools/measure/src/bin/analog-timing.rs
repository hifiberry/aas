//! When does a tone leave the player's buffer? For timing the analog input's
//! detector against the daemon's log.
//!
//! Plays silence, then a 1 kHz tone, then silence again, `--repeats` times,
//! through `--device`. The silence before each tone is a few blocks longer
//! or shorter from tone to tone, so that the tones do not all start at the
//! same point of the detector's windows. The tone starts on a block, and
//! each block is written into a full buffer, so when the write of the tone's
//! first block returns, that block is the last thing queued. The tone's
//! first frame then leaves the player's buffer one buffer less one period
//! later. The same holds for the tone's end. Both times are printed on
//! CLOCK_MONOTONIC, the clock the daemon's log lines are stamped with. What
//! the path behind the player adds is not measured here: add it from the
//! latency measurements.

use alsa::Direction;
use clap::Parser;
use measure::hw::{self, CHANNELS};
use measure::rt;
use measure::sample::f64_to_s32;
use std::process::ExitCode;

const FIFO_PRIORITY: i32 = 70;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "default")]
    device: String,
    #[arg(long, default_value_t = 48_000)]
    rate: u32,
    #[arg(long, default_value_t = 256)]
    period: usize,
    #[arg(long, default_value_t = 4)]
    periods: u32,
    #[arg(long, default_value_t = 20)]
    repeats: usize,
    /// Peak level of the tone.
    #[arg(long, default_value_t = -20.0, allow_negative_numbers = true)]
    dbfs: f64,
    /// Seconds of silence before each tone.
    #[arg(long, default_value_t = 1.0)]
    before: f64,
    /// Seconds of tone.
    #[arg(long, default_value_t = 2.0)]
    tone: f64,
    /// Seconds of silence after each tone. Longer than the detector's hold.
    #[arg(long, default_value_t = 5.0)]
    after: f64,
}

/// Now on CLOCK_MONOTONIC, in seconds.
fn monotonic() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid timespec for the call.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// Extra blocks of silence before tone `repeat`: 0 to 40, in steps of 7
/// modulo 41, so that consecutive tones fall at unrelated window phases.
fn stagger(repeat: usize) -> usize {
    repeat * 7 % 41
}

/// Rounds `seconds` at `rate` to whole blocks of `period` frames.
fn blocks(seconds: f64, rate: u32, period: usize) -> usize {
    ((seconds * f64::from(rate)) / period as f64)
        .round()
        .max(1.0) as usize
}

fn run(args: &Args) -> Result<(), String> {
    rt::lock_memory()?;
    rt::set_fifo(FIFO_PRIORITY)?;
    let channels = CHANNELS as usize;
    let (pcm, period, buffer) = hw::open_near(
        &args.device,
        Direction::Playback,
        args.rate,
        args.period,
        args.periods,
    )?;
    let play = pcm.io_i32().map_err(|e| format!("io: {e}"))?;
    let rate = f64::from(args.rate);
    let amplitude = 10f64.powf(args.dbfs / 20.0);
    let (quiet, tone, after) = (
        blocks(args.before, args.rate, period),
        blocks(args.tone, args.rate, period),
        blocks(args.after, args.rate, period),
    );
    println!(
        "device {} rate {} period {period} buffer {buffer}: tone {} frames at {:.1} dBFS peak",
        args.device,
        args.rate,
        tone * period,
        args.dbfs
    );
    // The tone's first frame leaves the buffer this long after the write of
    // its block returns.
    let lead = (buffer - period) as f64 / rate;
    let silence = vec![0i32; period * channels];
    let mut block = vec![0i32; period * channels];
    let write = |data: &[i32]| -> Result<f64, String> {
        let mut done = 0;
        while done < data.len() {
            let n = play
                .writei(&data[done..])
                .map_err(|e| format!("write: {e}"))?;
            done += n * channels;
        }
        Ok(monotonic())
    };
    for repeat in 0..args.repeats {
        // A different number of blocks before each tone, so that the tones
        // start at different points of the detector's 50 ms windows.
        for _ in 0..quiet + stagger(repeat) {
            write(&silence)?;
        }
        let mut started = 0.0;
        for b in 0..tone {
            for (i, frame) in block.chunks_exact_mut(channels).enumerate() {
                let n = (b * period + i) as f64;
                let phase = 2.0 * std::f64::consts::PI * 1000.0 * n / rate;
                frame.fill(f64_to_s32(amplitude * phase.sin()));
            }
            let at = write(&block)?;
            if b == 0 {
                started = at + lead;
            }
        }
        // The first block of silence after the tone: its first frame is
        // the first one without the tone.
        let ended = write(&silence)? + lead;
        println!(
            "repeat {repeat}: tone leaves the player's buffer from {started:.6} to {ended:.6} s monotonic"
        );
        for _ in 1..after {
            write(&silence)?;
        }
    }
    pcm.drain().map_err(|e| format!("drain: {e}"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_is_rounded_to_whole_blocks() {
        assert_eq!(blocks(2.0, 48_000, 256), 375);
        assert_eq!(blocks(1.0, 48_000, 256), 188);
    }

    #[test]
    fn the_stagger_differs_from_tone_to_tone() {
        let offsets: Vec<usize> = (0..20).map(stagger).collect();
        let mut unique = offsets.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 20);
        assert!(offsets.iter().all(|&o| o <= 40));
    }

    #[test]
    fn a_time_is_at_least_one_block() {
        assert_eq!(blocks(0.0, 48_000, 256), 1);
    }
}
