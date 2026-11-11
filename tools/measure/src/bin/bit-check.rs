//! Is what a player sends what reaches the card, sample for sample?
//!
//! Plays a seeded pseudo-random stream at full scale through a device, such
//! as `default` through the aas daemon, and captures the card's output, such
//! as the other end of `snd-aloop`. The capture is lined up with the stream
//! once and then compared frame by frame. A frame out of place is a break,
//! so a dropout shows up even when it is filled with silence.

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use clap::{Parser, ValueEnum};
use measure::pattern::{Fault, Pattern, compare, faults};
use measure::rt;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

const CHANNELS: u32 = 2;
/// Silence played before the stream, while the daemon sets up the card.
const PREROLL_S: f64 = 1.0;
/// The capture opens once this much has been written. By then the card runs
/// at the stream's rate, and a capture through `snd-aloop` takes on that
/// configuration rather than imposing its own.
const CAPTURE_AFTER_S: f64 = 0.5;
/// Silence played after the stream, so its end reaches the capture.
const TAIL_S: f64 = 0.5;
const PLAYBACK_BUFFER_US: u32 = 100_000;
const PLAYBACK_PERIOD_US: u32 = 20_000;
const CAPTURE_BUFFER_US: u32 = 200_000;
const CAPTURE_PERIOD_US: u32 = 20_000;
const FIFO_PRIORITY: i32 = 70;
/// Faults listed on a failure. A run of differences counts as one.
const FAULTS_SHOWN: usize = 20;

#[derive(Clone, Copy, ValueEnum)]
#[value(rename_all = "UPPER")]
enum SampleFormat {
    S16,
    S24,
    S32,
}

impl SampleFormat {
    fn bits(self) -> u32 {
        match self {
            SampleFormat::S16 => 16,
            SampleFormat::S24 => 24,
            SampleFormat::S32 => 32,
        }
    }

    fn alsa(self) -> Format {
        match self {
            SampleFormat::S16 => Format::S16LE,
            SampleFormat::S24 => Format::S24LE,
            SampleFormat::S32 => Format::S32LE,
        }
    }
}

#[derive(Parser)]
struct Args {
    /// The client's sample format.
    #[arg(long, value_enum)]
    format: SampleFormat,
    #[arg(long, default_value_t = 48_000)]
    rate: u32,
    /// Length of the stream.
    #[arg(long, default_value_t = 10)]
    seconds: u32,
    /// Where the stream is played.
    #[arg(long, default_value = "default")]
    device: String,
    /// Where the card's output is captured, as S32.
    #[arg(long, default_value = "hw:Loopback,1,0")]
    capture: String,
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

fn open(
    device: &str,
    direction: Direction,
    format: Format,
    rate: u32,
    buffer_us: u32,
    period_us: u32,
) -> Result<PCM, String> {
    let e = |what: &str, err: alsa::Error| format!("{device}: {what}: {err}");
    let pcm = PCM::new(device, direction, false).map_err(|x| e("open", x))?;
    {
        let hw = HwParams::any(&pcm).map_err(|x| e("hw params", x))?;
        hw.set_channels(CHANNELS).map_err(|x| e("channels", x))?;
        hw.set_rate(rate, ValueOr::Nearest)
            .map_err(|x| e("rate", x))?;
        hw.set_format(format).map_err(|x| e("format", x))?;
        hw.set_access(Access::RWInterleaved)
            .map_err(|x| e("access", x))?;
        hw.set_buffer_time_near(buffer_us, ValueOr::Nearest)
            .map_err(|x| e("buffer", x))?;
        hw.set_period_time_near(period_us, ValueOr::Nearest)
            .map_err(|x| e("period", x))?;
        pcm.hw_params(&hw).map_err(|x| e("apply", x))?;
        let got = hw.get_rate().map_err(|x| e("get rate", x))?;
        if got != rate {
            return Err(format!("{device}: runs at {got} Hz, not {rate}"));
        }
    }
    Ok(pcm)
}

struct Captured {
    frames: Vec<[i32; 2]>,
    overruns: u64,
}

/// Capture until `stop` is set.
fn capture(args: &Args, stop: &AtomicBool, capacity: usize) -> Result<Captured, String> {
    if let Err(e) = rt::set_fifo(FIFO_PRIORITY) {
        eprintln!("capture without real-time priority: {e}");
    }
    let pcm = open(
        &args.capture,
        Direction::Capture,
        Format::S32LE,
        args.rate,
        CAPTURE_BUFFER_US,
        CAPTURE_PERIOD_US,
    )?;
    let period = pcm
        .hw_params_current()
        .and_then(|hw| hw.get_period_size())
        .map_err(|x| format!("capture period: {x}"))? as usize;
    let io = pcm.io_i32().map_err(|x| format!("capture io: {x}"))?;
    pcm.start().map_err(|x| format!("capture start: {x}"))?;
    let mut block = vec![0i32; period * CHANNELS as usize];
    let mut result = Captured {
        frames: Vec::with_capacity(capacity),
        overruns: 0,
    };
    while !stop.load(Ordering::Relaxed) {
        match io.readi(&mut block) {
            Ok(n) => result.frames.extend(
                block[..n * CHANNELS as usize]
                    .chunks_exact(2)
                    .map(|f| [f[0], f[1]]),
            ),
            Err(err) => {
                result.overruns += 1;
                pcm.try_recover(err, true)
                    .map_err(|x| format!("capture: {x}"))?;
                pcm.start().map_err(|x| format!("capture restart: {x}"))?;
            }
        }
    }
    Ok(result)
}

/// Writes interleaved stereo samples, right-aligned in `i32`, in full.
fn write(pcm: &PCM, format: SampleFormat, samples: &[i32]) -> Result<(), String> {
    let e = |x: alsa::Error| format!("write: {x}");
    let channels = CHANNELS as usize;
    // Converted once, not on every pass of the loop.
    let narrow: Vec<i16> = match format {
        SampleFormat::S16 => samples.iter().map(|s| *s as i16).collect(),
        _ => Vec::new(),
    };
    let mut done = 0;
    while done < samples.len() {
        let rest = &samples[done..];
        let frames = match format {
            SampleFormat::S16 => pcm
                .io_i16()
                .map_err(e)?
                .writei(&narrow[done..])
                .map_err(e)?,
            SampleFormat::S24 => pcm.io_i32_s24().map_err(e)?.writei(rest).map_err(e)?,
            SampleFormat::S32 => pcm.io_i32().map_err(e)?.writei(rest).map_err(e)?,
        };
        done += frames * channels;
    }
    Ok(())
}

fn run(args: Args) -> Result<bool, String> {
    rt::lock_memory()?;
    let args = Arc::new(args);
    let rate = f64::from(args.rate);
    let sent = u64::from(args.seconds) * u64::from(args.rate);
    let pattern = Pattern::new(args.format.bits(), args.seed);
    let pcm = open(
        &args.device,
        Direction::Playback,
        args.format.alsa(),
        args.rate,
        PLAYBACK_BUFFER_US,
        PLAYBACK_PERIOD_US,
    )?;
    let period = pcm
        .hw_params_current()
        .and_then(|hw| hw.get_period_size())
        .map_err(|x| format!("period: {x}"))? as usize;
    let preroll = (PREROLL_S * rate) as u64;
    let capture_after = (CAPTURE_AFTER_S * rate) as u64;
    let total = preroll + sent + (TAIL_S * rate) as u64;
    let capacity = (total + preroll) as usize;
    let stop = Arc::new(AtomicBool::new(false));
    let mut capturing = None;
    let mut block = Vec::with_capacity(period * CHANNELS as usize);
    let mut written = 0u64;
    while written < total {
        if capturing.is_none() && written >= capture_after {
            let (args, stop) = (args.clone(), stop.clone());
            capturing = Some(thread::spawn(move || capture(&args, &stop, capacity)));
        }
        let end = (written + period as u64).min(total);
        block.clear();
        for at in written..end {
            let frame = match at.checked_sub(preroll) {
                Some(k) if k < sent => pattern.client_frame(k),
                _ => [0, 0],
            };
            block.extend(frame);
        }
        write(&pcm, args.format, &block)?;
        written = end;
    }
    pcm.drain().map_err(|x| format!("drain: {x}"))?;
    stop.store(true, Ordering::Relaxed);
    let captured = capturing
        .ok_or("the capture never started")?
        .join()
        .map_err(|_| "the capture thread panicked")??;
    let found = compare(&pattern, sent, &captured.frames);
    println!(
        "frames compared {}, differing {}, breaks {}",
        found.compared, found.differing, found.breaks
    );
    println!(
        "sent {sent}, captured {}, capture overruns {}",
        captured.frames.len(),
        captured.overruns
    );
    if let Some(at) = found.first_fault {
        println!(
            "first fault at frame {at} of the stream, {:.3} s",
            at as f64 / rate
        );
        let list = faults(&pattern, sent, &captured.frames, usize::MAX);
        print_faults(&pattern, &list);
    }
    Ok(found.compared == sent
        && found.differing == 0
        && found.breaks == 0
        && captured.overruns == 0)
}

/// Lists the faults, with each run of consecutive differences on one line.
fn print_faults(pattern: &Pattern, list: &[Fault]) {
    let mut shown = 0;
    let mut i = 0;
    while i < list.len() && shown < FAULTS_SHOWN {
        match list[i] {
            Fault::Break { at, due, to } => {
                println!("  capture {at}: break, frame {due} due, stream goes on from {to}");
                i += 1;
            }
            Fault::Difference { at, due, got } => {
                let mut run = 1;
                let mut silent = got == [0, 0];
                while let Some(Fault::Difference { at: next, got, .. }) = list.get(i + run) {
                    if *next != at + run {
                        break;
                    }
                    silent &= *got == [0, 0];
                    run += 1;
                }
                let what = if silent {
                    "all silence".to_string()
                } else {
                    format!("first {got:?}, wanted {:?}", pattern.frame(due))
                };
                println!("  capture {at}: {run} frames differ from frame {due} on, {what}");
                i += run;
            }
        }
        shown += 1;
    }
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("bit-check: FAIL");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
