//! Is what a player sends what reaches the card, sample for sample?
//!
//! Plays a seeded pseudo-random stream at full scale through a device, such
//! as `default` through the aas daemon, and captures the card's output, such
//! as the other end of `snd-aloop`. The capture is lined up with the stream
//! once and then compared frame by frame. A frame out of place is a break,
//! so a dropout shows up even when it is filled with silence.
//!
//! The stream is stereo. With `--channels` the capture takes more channels,
//! and `--expect` says which of them carry the stream's left and right, so
//! that a mixing matrix can be checked: the first channel of each side is
//! compared with the stream, every other channel it names must be a copy of
//! it, and every channel it does not name must be zero.
//!
//! With `--tone` it plays a 997 Hz tone at -6 dBFS instead, for an output
//! that is resampled, such as a further output of the daemon, which no
//! stream reaches sample for sample. Each side the tone is on is measured
//! for what is left once a sine is taken off, in windows of 0.1 s from one
//! second after the tone starts (see `measure::purity`): once against a
//! steady sine whose frequency is fitted to each window, and once against
//! a sine whose amplitude and phase may move slowly, which also takes off
//! the drift of a resampler that follows a clock. A side without the tone
//! must be silent, and the routing is checked as for the stream.

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use clap::{Parser, ValueEnum};
use measure::pattern::{Fault, Pattern, compare, faults};
use measure::purity;
use measure::routing::Routing;
use measure::rt;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

/// The stream's channels, as the player sends them.
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
    /// Channels captured, as many as the card is opened with. The stream
    /// itself is always stereo.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(2..=8))]
    channels: u32,
    /// Which captured channels carry the stream, counted from 1, as
    /// `channel=side` with the side `L` or `R`, separated by commas: for
    /// example `1=R,2=L` for a swap, or `1=L,2=R,7=L,8=R`. Each side must
    /// appear at least once. A channel not named must be silent.
    #[arg(long, default_value = "1=L,2=R")]
    expect: String,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Plays a 997 Hz tone at -6 dBFS instead of the stream, and measures
    /// its residual instead of comparing samples.
    #[arg(long)]
    tone: bool,
    /// With `--tone`, the sides it is on: `L`, `R` or `LR`. The other side
    /// plays silence.
    #[arg(long, default_value = "LR", requires = "tone")]
    tone_on: String,
    /// With `--tone`, the most the residual against the moving sine may be,
    /// in dB below a full-scale sine.
    #[arg(long, default_value_t = -100.0, requires = "tone")]
    max_residual_db: f64,
}

/// The tone of `--tone`, as Task 2's test of the resampler plays it.
const TONE_HZ: f64 = 997.0;
const TONE_PEAK: f64 = 0.5;
/// The residual is measured from this long after the tone starts.
const TONE_SKIP_S: f64 = 1.0;
/// And up to this long before it ends.
const TONE_END_S: f64 = 0.2;
const TONE_WINDOW_S: f64 = 0.1;
/// A sample above this part of full scale is the tone, when finding its start.
const TONE_START_LEVEL: f64 = 0.05;
/// The degree of the moving sine.
const MOVING_DEGREE: usize = 3;

fn open(
    device: &str,
    direction: Direction,
    channels: u32,
    format: Format,
    rate: u32,
    buffer_us: u32,
    period_us: u32,
) -> Result<PCM, String> {
    let e = |what: &str, err: alsa::Error| format!("{device}: {what}: {err}");
    let pcm = PCM::new(device, direction, false).map_err(|x| e("open", x))?;
    {
        let hw = HwParams::any(&pcm).map_err(|x| e("hw params", x))?;
        hw.set_channels(channels).map_err(|x| e("channels", x))?;
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
    /// The stream's two channels, as the routing places them.
    frames: Vec<[i32; 2]>,
    overruns: u64,
    /// Frames in which a channel did not follow the routing: a copy that
    /// differs from its side, or a channel that should be silent and is not.
    off_routing: u64,
    /// The first such frame, by its place in the capture, all channels.
    first_off_routing: Option<(usize, Vec<i32>)>,
}

/// Capture until `stop` is set.
fn capture(
    args: &Args,
    routing: &Routing,
    stop: &AtomicBool,
    capacity: usize,
) -> Result<Captured, String> {
    if let Err(e) = rt::set_fifo(FIFO_PRIORITY) {
        eprintln!("capture without real-time priority: {e}");
    }
    let pcm = open(
        &args.capture,
        Direction::Capture,
        args.channels,
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
    let channels = args.channels as usize;
    let mut block = vec![0i32; period * channels];
    let mut result = Captured {
        frames: Vec::with_capacity(capacity),
        overruns: 0,
        off_routing: 0,
        first_off_routing: None,
    };
    while !stop.load(Ordering::Relaxed) {
        match io.readi(&mut block) {
            Ok(n) => {
                for frame in block[..n * channels].chunks_exact(channels) {
                    let (stereo, fits) = routing.split(frame);
                    if !fits {
                        result.off_routing += 1;
                        result
                            .first_off_routing
                            .get_or_insert_with(|| (result.frames.len(), frame.to_vec()));
                    }
                    result.frames.push(stereo);
                }
            }
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

/// The client's frame `k` of the tone, right-aligned as `pattern` gives
/// its frames.
fn tone_frame(k: u64, rate: f64, bits: u32, on: [bool; 2]) -> [i32; 2] {
    let full = f64::from(1u32 << (bits - 1));
    let v = (TONE_PEAK * (std::f64::consts::TAU * TONE_HZ * k as f64 / rate).sin() * full).round()
        as i32;
    [if on[0] { v } else { 0 }, if on[1] { v } else { 0 }]
}

/// Measures the tone in the capture. Returns whether it passes.
fn check_tone(args: &Args, on: [bool; 2], captured: &[[i32; 2]], sent: u64) -> bool {
    let rate = f64::from(args.rate);
    let side = |c: usize| -> Vec<f64> {
        captured
            .iter()
            .map(|f| f64::from(f[c]) / 2_147_483_648.0)
            .collect()
    };
    let sides = [side(0), side(1)];
    let lit = if on[0] { 0 } else { 1 };
    let Some(start) = sides[lit].iter().position(|s| s.abs() > TONE_START_LEVEL) else {
        println!("tone: not found in the capture");
        return false;
    };
    let from = start + (TONE_SKIP_S * rate) as usize;
    let to = (start + sent as usize)
        .saturating_sub((TONE_END_S * rate) as usize)
        .min(captured.len());
    if to < from + (TONE_WINDOW_S * rate) as usize {
        println!("tone: the capture is too short, from {from} to {to}");
        return false;
    }
    let window = (TONE_WINDOW_S * rate) as usize;
    let mut pass = true;
    for (c, name) in [(0, "left"), (1, "right")] {
        let samples = &sides[c][from..to];
        if !on[c] {
            let peak = samples.iter().fold(0.0f64, |m, s| m.max(s.abs()));
            let level = if peak == 0.0 {
                "exactly zero".to_string()
            } else {
                format!("peak {:.1} dBFS", 20.0 * peak.log10())
            };
            println!("tone {name}: silent side, {level}");
            pass &= peak < 1e-7;
            continue;
        }
        let guess = purity::omega(TONE_HZ, rate);
        let span = purity::omega(1.0, rate);
        let global = purity::fit_omega(&samples[..samples.len().min(rate as usize)], guess, span);
        let (mut steady, mut moving) = (Vec::new(), Vec::new());
        for w in samples.chunks_exact(window) {
            let own = purity::fit_omega(w, global, purity::omega(0.05, rate));
            steady.push(purity::residual_dbfs(w, own, 0));
            moving.push(purity::residual_dbfs(w, global, MOVING_DEGREE));
        }
        // The level of the whole: the mean of the windows' powers.
        let overall = |v: &[f64]| {
            10.0 * (v.iter().map(|d| 10f64.powf(d / 10.0)).sum::<f64>() / v.len() as f64).log10()
        };
        let worst = |v: &[f64]| v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        println!(
            "tone {name}: {:.4} Hz, {} windows of {TONE_WINDOW_S} s; residual against a steady \
             sine {:.1} dBFS, worst window {:.1}; against a moving sine {:.1} dBFS, worst window {:.1}",
            global / std::f64::consts::TAU * rate,
            steady.len(),
            overall(&steady),
            worst(&steady),
            overall(&moving),
            worst(&moving),
        );
        pass &= overall(&moving) < args.max_residual_db;
    }
    pass
}

fn run(args: Args) -> Result<bool, String> {
    let routing = Arc::new(Routing::parse(args.channels as usize, &args.expect)?);
    rt::lock_memory()?;
    let args = Arc::new(args);
    let rate = f64::from(args.rate);
    let sent = u64::from(args.seconds) * u64::from(args.rate);
    let pattern = Pattern::new(args.format.bits(), args.seed);
    let tone_on = [
        args.tone_on.contains(['L', 'l']),
        args.tone_on.contains(['R', 'r']),
    ];
    if args.tone && !tone_on.contains(&true) {
        return Err(format!("--tone-on {}: neither L nor R", args.tone_on));
    }
    let pcm = open(
        &args.device,
        Direction::Playback,
        CHANNELS,
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
            let (args, routing, stop) = (args.clone(), routing.clone(), stop.clone());
            capturing = Some(thread::spawn(move || {
                capture(&args, &routing, &stop, capacity)
            }));
        }
        let end = (written + period as u64).min(total);
        block.clear();
        for at in written..end {
            let frame = match at.checked_sub(preroll) {
                Some(k) if k < sent && args.tone => {
                    tone_frame(k, rate, args.format.bits(), tone_on)
                }
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
    if args.tone {
        println!(
            "sent {sent}, captured {}, capture overruns {}",
            captured.frames.len(),
            captured.overruns
        );
        if *routing != Routing::stereo() {
            println!(
                "channels {}, expected {}, frames off the routing {}",
                args.channels,
                routing.describe(),
                captured.off_routing
            );
            if let Some((at, frame)) = &captured.first_off_routing {
                println!("  first off the routing at capture {at}: {frame:?}");
            }
        }
        let pass = check_tone(&args, tone_on, &captured.frames, sent);
        return Ok(pass && captured.overruns == 0 && captured.off_routing == 0);
    }
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
    // Only for a capture that is not plain stereo, so that the output of a
    // stereo check stays as it was.
    if *routing != Routing::stereo() {
        println!(
            "channels {}, expected {}, frames off the routing {}",
            args.channels,
            routing.describe(),
            captured.off_routing
        );
        if let Some((at, frame)) = &captured.first_off_routing {
            println!("  first off the routing at capture {at}: {frame:?}");
        }
    }
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
        && captured.overruns == 0
        && captured.off_routing == 0)
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
