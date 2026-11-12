//! Do two outputs play in time?
//!
//! Plays a chirp through a device, such as `default` through the aas
//! daemon, once every interval, and captures two devices at once: the
//! first output through a cable into the ADC, and a second output at the
//! other end of `snd-aloop`. Both captures run with the cards' timestamps
//! on `CLOCK_MONOTONIC`, so each chirp found in either gets a time on the
//! system clock (see `measure::clockfit`). The offset of a chirp is its
//! time in the first capture less its time in the second.
//!
//! What the offset holds depends on the paths. Through the cable it holds
//! the DAC's and the ADC's own delays, which a capture of `snd-aloop` does
//! not have, so two outputs in time differ by those. The capture side's
//! wait for its reader is not in it: a timestamp dates the frame when the
//! card took it, not when it was read.
//!
//! A frame is dated by the card's readings within a second either side of
//! it, not by one line through the whole run: `snd-aloop` with a rate shift
//! set keeps its rate only on average, and wanders from a straight line by
//! hundreds of µs. Each time is given two ways: by the line through those
//! readings, and by their lower edge, which dates a frame by the least
//! delay with which the card's pointer was read. A card whose pointer moves
//! in coarse steps is read late by up to a step, so the lower edge is the
//! better date. The offset by one line through all readings is printed too.

use alsa::Direction;
use alsa::pcm::{PCM, State, TstampType};
use clap::Parser;
use measure::clockfit::ClockFit;
use measure::correlate::{chirp_level_dbfs, test_chirp};
use measure::hw;
use measure::rt;
use measure::sample::f64_to_s32;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

const CHIRP_FRAMES: usize = 2048;
/// The captures open once this much has been played, so that the daemon
/// has set its cards to the stream's rate and a capture of `snd-aloop`
/// takes on that rate.
const CAPTURE_AFTER_S: f64 = 0.5;
/// Played after the last chirp, so that it reaches both captures.
const TAIL_S: f64 = 0.5;
const PLAYBACK_PERIOD_S: f64 = 0.02;
const PLAYBACK_PERIODS: u32 = 5;
const CAPTURE_PERIOD_S: f64 = 0.01;
const CAPTURE_PERIODS: u32 = 8;
const FIFO_PRIORITY: i32 = 70;
/// The correlation's peak must stand this far above the next one.
const MIN_PEAK_RATIO: f64 = 2.0;
/// After the first chirp, the next is looked for this many frames either
/// side of one interval later.
const SEARCH_FRAMES: usize = 1024;
/// A chirp is dated by the readings this long either side of it.
const LOCAL_S: f64 = 1.0;

#[derive(Parser)]
struct Args {
    /// Where the chirps are played.
    #[arg(long, default_value = "default")]
    device: String,
    /// The first capture, such as the ADC behind a cable from the DAC.
    #[arg(long, default_value = "hw:sndrpihifiberry")]
    first: String,
    /// The second capture, such as the other end of `snd-aloop`.
    #[arg(long, default_value = "hw:Loopback,1,0")]
    second: String,
    #[arg(long, default_value_t = 48_000)]
    rate: u32,
    /// Chirps played.
    #[arg(long, default_value_t = 20)]
    repeats: usize,
    /// From one chirp to the next.
    #[arg(long, default_value_t = 1000)]
    interval_ms: u32,
    /// Silence before the first chirp, in seconds.
    #[arg(long, default_value_t = 1.5)]
    lead_s: f64,
    /// The chirp's peak, 1.0 being full scale.
    #[arg(long, default_value_t = 0.5)]
    level: f64,
    /// The offset expected, in µs, to report the distance from.
    #[arg(long)]
    expected_us: Option<f64>,
    /// Writes both captures' timestamp readings as text, to files starting
    /// with this.
    #[arg(long)]
    save: Option<String>,
    /// With `--save`, also both captures' left channel as raw f32.
    #[arg(long, requires = "save")]
    save_audio: bool,
}

/// One capture, and the card's readings of its position against time.
struct Captured {
    left: Vec<f32>,
    /// Frames the card had taken, and when, in seconds on CLOCK_MONOTONIC.
    readings: Vec<(f64, f64)>,
    overruns: u64,
    period: usize,
}

fn open_capture(device: &str, rate: u32) -> Result<(PCM, usize), String> {
    let period = (f64::from(rate) * CAPTURE_PERIOD_S) as usize;
    let (pcm, period, _) =
        hw::open_near(device, Direction::Capture, rate, period, CAPTURE_PERIODS)?;
    let e = |what: &str, err: alsa::Error| format!("{device}: {what}: {err}");
    {
        let sw = pcm.sw_params_current().map_err(|x| e("sw params", x))?;
        sw.set_tstamp_mode(true).map_err(|x| e("tstamp mode", x))?;
        sw.set_tstamp_type(TstampType::Monotonic)
            .map_err(|x| e("tstamp type", x))?;
        pcm.sw_params(&sw).map_err(|x| e("apply sw", x))?;
    }
    Ok((pcm, period))
}

fn seconds(ts: libc::timespec) -> f64 {
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// Captures `device` until `stop` is set.
fn capture(
    device: &str,
    rate: u32,
    stop: &AtomicBool,
    capacity: usize,
) -> Result<Captured, String> {
    if let Err(e) = rt::set_fifo(FIFO_PRIORITY) {
        eprintln!("{device}: capture without real-time priority: {e}");
    }
    let (pcm, period) = open_capture(device, rate)?;
    let channels = hw::CHANNELS as usize;
    let io = pcm.io_i32().map_err(|x| format!("{device}: io: {x}"))?;
    pcm.start().map_err(|x| format!("{device}: start: {x}"))?;
    let mut block = vec![0i32; period * channels];
    let mut result = Captured {
        left: Vec::with_capacity(capacity),
        readings: Vec::with_capacity(capacity / period + 16),
        overruns: 0,
        period,
    };
    // Frames read before the last overrun's restart, which starts the
    // card's count again.
    let mut base = 0u64;
    let mut read_since = 0u64;
    while !stop.load(Ordering::Relaxed) {
        match io.readi(&mut block) {
            Ok(n) => {
                result.left.extend(
                    block[..n * channels]
                        .iter()
                        .step_by(channels)
                        .map(|&s| s as f32 / 2_147_483_648.0),
                );
                read_since += n as u64;
                if let Ok(status) = pcm.status() {
                    if status.get_state() == State::Running {
                        let taken = base + read_since + status.get_avail().max(0) as u64;
                        result
                            .readings
                            .push((taken as f64, seconds(status.get_htstamp())));
                    }
                }
            }
            Err(err) => {
                // A gap in the frames: the positions after it no longer
                // count from the same start, so the run is spoilt. Counted
                // and reported; the readings are kept only up to here.
                result.overruns += 1;
                base += read_since;
                read_since = 0;
                pcm.try_recover(err, true)
                    .map_err(|x| format!("{device}: {x}"))?;
                pcm.start().map_err(|x| format!("{device}: restart: {x}"))?;
            }
        }
    }
    Ok(result)
}

/// A chirp found in a capture, at a fractional frame.
#[derive(Clone, Copy)]
struct Found {
    at: f64,
    level_dbfs: f64,
}

/// The chirp in `captured[from..to]`, to a fraction of a frame.
fn find(captured: &[f32], from: usize, to: usize, chirp: &[f64]) -> Option<Found> {
    let to = to.min(captured.len());
    if to < from + chirp.len() {
        return None;
    }
    let window: Vec<f64> = captured[from..to].iter().map(|&s| f64::from(s)).collect();
    let scores: Vec<f64> = window
        .windows(chirp.len())
        .map(|w| w.iter().zip(chirp).map(|(a, b)| a * b).sum::<f64>().abs())
        .collect();
    let (best, peak) = scores
        .iter()
        .copied()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    let next = scores
        .iter()
        .enumerate()
        .filter(|(i, _)| i.abs_diff(best) > 16)
        .map(|(_, s)| *s)
        .fold(0.0, f64::max);
    if peak == 0.0 || peak < next * MIN_PEAK_RATIO {
        return None;
    }
    // A parabola through the peak and its neighbours.
    let fraction = match (best.checked_sub(1).map(|i| scores[i]), scores.get(best + 1)) {
        (Some(l), Some(&r)) => {
            let d = l - 2.0 * peak + r;
            if d < 0.0 { 0.5 * (l - r) / d } else { 0.0 }
        }
        _ => 0.0,
    };
    Some(Found {
        at: (from + best) as f64 + fraction,
        level_dbfs: chirp_level_dbfs(&window, best, chirp.len()),
    })
}

/// Every chirp in a capture, in order, `None` where one was not found.
fn find_all(captured: &[f32], args: &Args, chirp: &[f64]) -> Vec<Option<Found>> {
    let rate = f64::from(args.rate);
    let interval = (f64::from(args.interval_ms) / 1000.0 * rate) as usize;
    // The first chirp comes within the lead after the capture opened, and
    // well before the second.
    let first_end =
        ((args.lead_s - CAPTURE_AFTER_S).max(0.0) * rate) as usize + interval / 2 + chirp.len();
    let mut found = Vec::with_capacity(args.repeats);
    let mut last: Option<(usize, f64)> = None;
    for i in 0..args.repeats {
        let hit = match last {
            None => find(captured, 0, first_end, chirp),
            Some((k, at)) => {
                let expect = at + ((i - k) * interval) as f64;
                let from = (expect as usize).saturating_sub(SEARCH_FRAMES);
                find(
                    captured,
                    from,
                    expect as usize + SEARCH_FRAMES + chirp.len(),
                    chirp,
                )
            }
        };
        if let Some(f) = hit {
            last = Some((i, f.at));
        }
        found.push(hit);
    }
    found
}

fn save(prefix: &str, name: &str, captured: &Captured, audio: bool) -> Result<(), String> {
    let e = |x: std::io::Error| format!("{prefix}: {x}");
    if audio {
        let mut raw = BufWriter::new(File::create(format!("{prefix}-{name}.f32")).map_err(e)?);
        for s in &captured.left {
            raw.write_all(&s.to_le_bytes()).map_err(e)?;
        }
    }
    let mut text = BufWriter::new(File::create(format!("{prefix}-{name}.readings")).map_err(e)?);
    for (frame, t) in &captured.readings {
        writeln!(text, "{frame:.0} {t:.9}").map_err(e)?;
    }
    Ok(())
}

/// The card's clock around `frame`, from its readings within `LOCAL_S`.
fn local(readings: &[(f64, f64)], frame: f64, rate: f64) -> Option<ClockFit> {
    let span = LOCAL_S * rate;
    let from = readings.partition_point(|r| r.0 < frame - span);
    let to = readings.partition_point(|r| r.0 <= frame + span);
    ClockFit::new(&readings[from..to])
}

fn describe(name: &str, device: &str, rate: f64, captured: &Captured, fit: &ClockFit) {
    println!(
        "{name} {device}: period {}, {} frames, {} readings, overruns {}, \
         rate {:.3} Hz ({:+.2} ppm on the system clock), line rms {:.1} µs, \
         lower edge {:.1} µs below the line at the middle",
        captured.period,
        captured.left.len(),
        fit.readings,
        captured.overruns,
        fit.rate(),
        (fit.rate() / rate - 1.0) * 1e6,
        fit.rms * 1e6,
        {
            let mid = captured.left.len() as f64 / 2.0;
            (fit.mean_time(mid) - fit.lower_time(mid)) * 1e6
        },
    );
}

fn run(args: Args) -> Result<bool, String> {
    rt::lock_memory()?;
    let args = Arc::new(args);
    let rate = f64::from(args.rate);
    let chirp: Vec<f64> = test_chirp(CHIRP_FRAMES)
        .iter()
        .map(|s| s * 2.0 * args.level)
        .collect();
    let interval = (f64::from(args.interval_ms) / 1000.0 * rate) as u64;
    let lead = (args.lead_s * rate) as u64;
    let total = lead + args.repeats as u64 * interval + (TAIL_S * rate) as u64;
    let capture_after = (CAPTURE_AFTER_S * rate) as u64;
    if lead < capture_after + CHIRP_FRAMES as u64 {
        return Err(format!("the lead must be over {CAPTURE_AFTER_S} s"));
    }
    if interval < 4 * (SEARCH_FRAMES + CHIRP_FRAMES) as u64 {
        return Err("the interval is too short for the search".into());
    }
    let (pcm, period, _) = hw::open_near(
        &args.device,
        Direction::Playback,
        args.rate,
        (rate * PLAYBACK_PERIOD_S) as usize,
        PLAYBACK_PERIODS,
    )?;
    let io = pcm.io_i32().map_err(|x| format!("io: {x}"))?;
    let channels = hw::CHANNELS as usize;
    let capacity = total as usize;
    let stop = Arc::new(AtomicBool::new(false));
    let mut captures = None;
    let mut block = Vec::with_capacity(period * channels);
    let mut written = 0u64;
    let mut chirp_times = Vec::with_capacity(args.repeats);
    while written < total {
        if captures.is_none() && written >= capture_after {
            let spawn = |device: String| {
                let (args, stop) = (args.clone(), stop.clone());
                thread::spawn(move || capture(&device, args.rate, &stop, capacity))
            };
            captures = Some((spawn(args.first.clone()), spawn(args.second.clone())));
        }
        let end = (written + period as u64).min(total);
        block.clear();
        for at in written..end {
            let value = at
                .checked_sub(lead)
                .filter(|k| (k / interval) < args.repeats as u64)
                .and_then(|k| chirp.get((k % interval) as usize))
                .map_or(0, |v| f64_to_s32(*v));
            if at >= lead
                && (at - lead) % interval == 0
                && (at - lead) / interval < args.repeats as u64
            {
                chirp_times.push(rt::monotonic_s());
            }
            block.extend([value, value]);
        }
        let mut done = 0;
        while done < block.len() {
            done += io
                .writei(&block[done..])
                .map_err(|x| format!("write: {x}"))?
                * channels;
        }
        written = end;
    }
    pcm.drain().map_err(|x| format!("drain: {x}"))?;
    stop.store(true, Ordering::Relaxed);
    let (a, b) = captures.ok_or("the captures never started")?;
    let first = a.join().map_err(|_| "the first capture panicked")??;
    let second = b.join().map_err(|_| "the second capture panicked")??;
    if let Some(prefix) = &args.save {
        save(prefix, "first", &first, args.save_audio)?;
        save(prefix, "second", &second, args.save_audio)?;
    }
    let fit_first = ClockFit::new(&first.readings).ok_or("the first capture has no timestamps")?;
    let fit_second =
        ClockFit::new(&second.readings).ok_or("the second capture has no timestamps")?;
    describe("first", &args.first, rate, &first, &fit_first);
    describe("second", &args.second, rate, &second, &fit_second);
    println!(
        "second against first: {:+.2} ppm",
        (fit_second.rate() / fit_first.rate() - 1.0) * 1e6
    );
    let in_first = find_all(&first.left, &args, &chirp);
    let in_second = find_all(&second.left, &args, &chirp);
    println!(
        "chirp  time_s  first_frame  second_frame  first_dbfs  second_dbfs  \
         offset_lower_us  offset_line_us  offset_global_us  second_local_rms_us"
    );
    let mut lower = Vec::new();
    let mut line = Vec::new();
    let mut global = Vec::new();
    for i in 0..args.repeats {
        let dated = |f: Found, s: Found| {
            Some((
                local(&first.readings, f.at, rate)?,
                local(&second.readings, s.at, rate)?,
            ))
        };
        match (in_first[i], in_second[i]) {
            (Some(f), Some(s)) if dated(f, s).is_some() => {
                let (lf, ls) = dated(f, s).expect("just dated");
                let by_lower = lf.lower_time(f.at) - ls.lower_time(s.at);
                let by_line = lf.mean_time(f.at) - ls.mean_time(s.at);
                let by_global = fit_first.mean_time(f.at) - fit_second.mean_time(s.at);
                lower.push(by_lower * 1e6);
                line.push(by_line * 1e6);
                global.push(by_global * 1e6);
                println!(
                    "{i:5}  {:.3}  {:.2}  {:.2}  {:.1}  {:.1}  {:.1}  {:.1}  {:.1}  {:.1}",
                    ls.lower_time(s.at),
                    f.at,
                    s.at,
                    f.level_dbfs,
                    s.level_dbfs,
                    by_lower * 1e6,
                    by_line * 1e6,
                    by_global * 1e6,
                    ls.rms * 1e6,
                );
            }
            (f, s) => println!(
                "{i:5}  played at {:.3}: not found in the{}{} capture",
                chirp_times.get(i).copied().unwrap_or(f64::NAN),
                if f.is_none() { " first" } else { "" },
                if s.is_none() { " second" } else { "" },
            ),
        }
    }
    let summary = |name: &str, v: &[f64]| {
        if v.is_empty() {
            return;
        }
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        let min = v.iter().copied().fold(f64::INFINITY, f64::min);
        let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        print!(
            "offset by the {name}: mean {mean:.1} µs ({:.2} frames), min {min:.1}, max {max:.1}, spread {:.1} µs",
            mean * rate / 1e6,
            max - min
        );
        if let Some(expected) = args.expected_us {
            print!(
                ", {:+.1} µs from the expected {expected:.1}",
                mean - expected
            );
        }
        println!();
    };
    println!("chirps found in both: {} of {}", lower.len(), args.repeats);
    summary("lower edge", &lower);
    summary("line", &line);
    summary("global line", &global);
    Ok(lower.len() == args.repeats && first.overruns == 0 && second.overruns == 0)
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("align-check: incomplete, see above");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
