//! Does the audio break? Plays a steady sine and listens to it.
//!
//! The tone is played through a device, such as `default` through the aas
//! daemon, and captured from the card's input through a cable. Every break
//! in the captured tone is reported with its time on CLOCK_MONOTONIC, the
//! clock the daemon logs its wake-up gaps on, and its length. So a late
//! wake-up can be matched to what it did to the sound, whether or not ALSA
//! counted an xrun.
//!
//! The probe's own faults are counted apart from the breaks: an overrun of
//! its capture leaves a hole that is not looked at, and an underrun of its
//! playback is named, since it breaks the tone too.

use alsa::Direction;
use clap::Parser;
use measure::hw::{self, CHANNELS};
use measure::rt;
use measure::sample::{f64_to_s32, s32_to_f64};
use measure::stop;
use measure::tone::{Break, Calibration, Detector, FrameClock, calibrate, db};
use std::f64::consts::TAU;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

const FIFO_PRIORITY: i32 = 70;
/// The player's own buffer. Long, so that the probe is not what underruns.
const PLAY_PERIOD: usize = 1024;
const PLAY_PERIODS: u32 = 4;
/// The capture's buffer, 171 ms at 48 kHz.
const CAPTURE_PERIOD: usize = 256;
const CAPTURE_PERIODS: u32 = 32;
/// The capture opens this long after the tone starts. By then the daemon
/// has the card running at the tone's rate.
const CAPTURE_AFTER: Duration = Duration::from_millis(500);
/// How long the capture may take to open, for a card still changing rate.
const CAPTURE_OPEN_WAIT: Duration = Duration::from_secs(2);
/// Capture thrown away before the calibration, while the input settles.
const SETTLE_S: f64 = 0.5;
const CALIBRATION_S: f64 = 1.0;
/// With less than this coming back, the cable is missing.
const MIN_LEVEL_DBFS: f64 = -40.0;

#[derive(Parser)]
struct Args {
    /// Where the tone is played.
    #[arg(long, default_value = "default")]
    device: String,
    /// The card input the cable ends in.
    #[arg(long, default_value = "hw:0,0")]
    capture: String,
    #[arg(long, default_value_t = 48_000)]
    rate: u32,
    /// The tone. Its period should not divide the card's buffer, or a
    /// buffer played twice would leave the tone in step.
    #[arg(long, default_value_t = 1100.0)]
    frequency: f64,
    /// Level of the tone played, in dBFS.
    #[arg(long, default_value_t = -6.0, allow_negative_numbers = true)]
    level: f64,
    /// How long to listen after the calibration.
    #[arg(long, default_value_t = 60.0)]
    seconds: f64,
    /// Faults closer together than this are one break.
    #[arg(long, default_value_t = 5.0)]
    merge_ms: f64,
    /// The threshold, as a multiple of the largest residual of the
    /// calibration.
    #[arg(long, default_value_t = measure::tone::THRESHOLD_FACTOR)]
    threshold_factor: f64,
}

enum Heard {
    /// Left samples, how many frames the card had captured when they were
    /// read, counted as the reads count them, and the clock then.
    Block {
        samples: Vec<f64>,
        card_frames: u64,
        now: f64,
    },
    /// The capture lost frames here.
    Overrun {
        now: f64,
    },
    Failed(String),
}

/// Plays the tone until `stop` and counts its own underruns.
fn play(args: &Args, stop: &AtomicBool, underruns: &AtomicU64) -> Result<(), String> {
    if let Err(e) = rt::set_fifo(FIFO_PRIORITY) {
        eprintln!("playback without real-time priority: {e}");
    }
    let (pcm, period, buffer) = hw::open_near(
        &args.device,
        Direction::Playback,
        args.rate,
        PLAY_PERIOD,
        PLAY_PERIODS,
    )?;
    println!(
        "playing {} Hz at {} dBFS to {}, period {period}, buffer {buffer} frames",
        args.frequency, args.level, args.device
    );
    let io = pcm.io_i32().map_err(|e| format!("playback io: {e}"))?;
    let channels = CHANNELS as usize;
    let amplitude = 10f64.powf(args.level / 20.0);
    let step = TAU * args.frequency / f64::from(args.rate);
    let mut phase = 0.0f64;
    let mut block = vec![0i32; period * channels];
    while !stop.load(Ordering::Relaxed) {
        for frame in block.chunks_exact_mut(channels) {
            frame.fill(f64_to_s32(amplitude * phase.sin()));
            phase += step;
            if phase >= TAU {
                phase -= TAU;
            }
        }
        let mut done = 0;
        while done < block.len() {
            match io.writei(&block[done..]) {
                Ok(n) => done += n * channels,
                Err(err) => {
                    underruns.fetch_add(1, Ordering::Relaxed);
                    pcm.try_recover(err, true)
                        .map_err(|e| format!("playback: {e}"))?;
                }
            }
        }
    }
    Ok(())
}

/// Reads the capture until `stop` and hands every block on.
fn capture(args: &Args, stop: &AtomicBool, heard: &Sender<Heard>) -> Result<(), String> {
    if let Err(e) = rt::set_fifo(FIFO_PRIORITY) {
        eprintln!("capture without real-time priority: {e}");
    }
    let opened = rt::monotonic_s();
    let pcm = loop {
        match hw::open(
            &args.capture,
            Direction::Capture,
            args.rate,
            CAPTURE_PERIOD,
            CAPTURE_PERIODS,
        ) {
            Ok(pcm) => break pcm,
            Err(e) if rt::monotonic_s() - opened > CAPTURE_OPEN_WAIT.as_secs_f64() => {
                return Err(format!("{}: {e}", args.capture));
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    };
    let io = pcm.io_i32().map_err(|e| format!("capture io: {e}"))?;
    pcm.start().map_err(|e| format!("capture start: {e}"))?;
    let channels = CHANNELS as usize;
    let mut block = vec![0i32; CAPTURE_PERIOD * channels];
    let mut read = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let message = match io.readi(&mut block) {
            Ok(n) => {
                read += n as u64;
                // An overrun seen here is reported by the next read.
                let waiting = pcm.avail().unwrap_or(0).max(0) as u64;
                Heard::Block {
                    samples: block[..n * channels]
                        .chunks_exact(channels)
                        .map(|f| s32_to_f64(f[0]))
                        .collect(),
                    card_frames: read + waiting,
                    now: rt::monotonic_s(),
                }
            }
            Err(err) => {
                let now = rt::monotonic_s();
                pcm.try_recover(err, true)
                    .map_err(|e| format!("capture: {e}"))?;
                pcm.start().map_err(|e| format!("capture restart: {e}"))?;
                Heard::Overrun { now }
            }
        };
        if heard.send(message).is_err() {
            break;
        }
    }
    Ok(())
}

fn print_calibration(cal: &Calibration, rate: f64) {
    println!(
        "calibrated: tone {:.2} Hz at {:.1} dBFS, residual rms {:.1} dBFS, peak {:.1} dBFS, \
         threshold {:.1} dBFS, {:.1} dB under the tone",
        cal.frequency(rate),
        db(cal.amplitude, 1.0),
        db(cal.noise_rms, 1.0),
        db(cal.noise_peak, 1.0),
        db(cal.threshold, 1.0),
        db(cal.amplitude, cal.threshold),
    );
}

fn print_break(b: &Break, at: Option<f64>, rate: f64) {
    let at = at.map_or("unknown".to_string(), |t| format!("{t:.6} s monotonic"));
    let shift = if b.shift.is_nan() {
        "shift unknown".to_string()
    } else {
        format!("tone {:+.1} frames after it", b.shift)
    };
    println!(
        "break at {at}: {:.3} ms, {} frames, residual peak {:.1} dBFS, lowest level {:.1} dB, \
         {shift}",
        b.frames as f64 / rate * 1000.0,
        b.frames,
        db(b.peak_residual, 1.0),
        db(b.min_level.max(1e-6), 1.0),
    );
}

struct Counts {
    breaks: u64,
    overruns: u64,
    listened_s: f64,
}

/// Calibrates on the first clean second and then looks at every sample.
fn listen(args: &Args, heard: &Receiver<Heard>) -> Result<Counts, String> {
    let rate = f64::from(args.rate);
    let settle = (SETTLE_S * rate) as usize;
    let calibration = (CALIBRATION_S * rate) as usize;
    let wanted = (args.seconds * rate) as u64;
    let merge = (args.merge_ms / 1000.0 * rate) as usize;
    let mut skipped = 0;
    let mut first = Vec::with_capacity(calibration);
    let mut detector: Option<Detector> = None;
    let mut clock = FrameClock::new(rate);
    // Frames handed over so far, and how many of them came before the
    // detector's first.
    let mut handed = 0u64;
    let mut before_detector = 0u64;
    let mut counts = Counts {
        breaks: 0,
        overruns: 0,
        listened_s: 0.0,
    };
    let report = |b: Break, clock: &FrameClock, before: u64, counts: &mut Counts| {
        counts.breaks += 1;
        print_break(&b, clock.time_of(b.start + before), rate);
    };
    while !stop::requested() {
        let message = match heard.recv_timeout(Duration::from_secs(2)) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => return Err("the capture stalled".to_string()),
            Err(RecvTimeoutError::Disconnected) => return Err("the capture ended".to_string()),
        };
        let (samples, card_frames, now) = match message {
            Heard::Failed(e) => return Err(e),
            Heard::Overrun { now } => {
                counts.overruns += 1;
                println!("capture overrun at {now:.6} s monotonic, not a break in the tone");
                if let Some(d) = &mut detector {
                    if let Some(b) = d.reset() {
                        report(b, &clock, before_detector, &mut counts);
                    }
                } else {
                    // The calibration needs one stretch without a hole.
                    first.clear();
                }
                clock.reset();
                continue;
            }
            Heard::Block {
                samples,
                card_frames,
                now,
            } => (samples, card_frames, now),
        };
        clock.note(card_frames, now);
        for sample in samples {
            handed += 1;
            if skipped < settle {
                skipped += 1;
                continue;
            }
            let Some(d) = &mut detector else {
                first.push(sample);
                if first.len() == calibration {
                    let cal = calibrate(&first, args.threshold_factor)?;
                    if db(cal.amplitude, 1.0) < MIN_LEVEL_DBFS {
                        return Err(format!(
                            "no signal: {:.1} dBFS, check the cable and input level",
                            db(cal.amplitude, 1.0)
                        ));
                    }
                    print_calibration(&cal, rate);
                    println!("listening from {now:.3} s monotonic");
                    detector = Some(Detector::new(cal, merge));
                    before_detector = handed;
                }
                continue;
            };
            if let Some(b) = d.push(sample) {
                report(b, &clock, before_detector, &mut counts);
            }
            if d.position() >= wanted {
                break;
            }
        }
        if detector.as_ref().is_some_and(|d| d.position() >= wanted) {
            break;
        }
    }
    let Some(mut d) = detector else {
        return Err("stopped before the calibration".to_string());
    };
    if let Some(b) = d.finish() {
        report(b, &clock, before_detector, &mut counts);
    }
    counts.listened_s = d.position() as f64 / rate;
    Ok(counts)
}

fn run(args: Args) -> Result<bool, String> {
    rt::lock_memory()?;
    stop::install();
    let args = Arc::new(args);
    let stop = Arc::new(AtomicBool::new(false));
    let underruns = Arc::new(AtomicU64::new(0));
    let player = {
        let (args, stop, underruns) = (args.clone(), stop.clone(), underruns.clone());
        thread::spawn(move || play(&args, &stop, &underruns))
    };
    thread::sleep(CAPTURE_AFTER);
    let (send, heard) = mpsc::channel();
    let capturing = {
        let (args, stop) = (args.clone(), stop.clone());
        thread::spawn(move || {
            if let Err(e) = capture(&args, &stop, &send) {
                let _ = send.send(Heard::Failed(e));
            }
        })
    };
    let result = listen(&args, &heard);
    stop.store(true, Ordering::Relaxed);
    drop(heard);
    let played = player.join().map_err(|_| "the player panicked")?;
    capturing.join().map_err(|_| "the capture panicked")?;
    // A player that failed explains a capture that heard nothing.
    played?;
    let counts = result?;
    let underruns = underruns.load(Ordering::Relaxed);
    println!(
        "listened {:.1} s: breaks {}, capture overruns {}, player underruns {underruns}",
        counts.listened_s, counts.breaks, counts.overruns
    );
    Ok(counts.breaks == 0 && counts.overruns == 0 && underruns == 0)
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
