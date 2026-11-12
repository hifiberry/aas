//! How much of a period do mixing and resampling use, in 32 and 64-bit float,
//! and mixing also in 32-bit integer? And the built-in plugins, `detect` and
//! `balance`, in all three formats.

use aas::plugin::{Block, Plugin, Samples, builtin};
use audioadapter_buffers::direct::InterleavedSlice;
use clap::Parser;
use measure::sample::{f32_to_s32, f64_to_s32, mix_gain, mix_i32, s32_to_f32, s32_to_f64};
use measure::stats::{Stats, Summary, report_cases};
use measure::{rt, stop};
use num_traits::Float;
use rubato::{
    Async, FixedAsync, Resampler, Sample, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::hint::black_box;
use std::process::ExitCode;
use std::time::Instant;

const CHANNELS: usize = 2;
const WARMUP: usize = 1000;
const MAX_SOURCES: usize = 8;
const MIX_RATES: [u32; 2] = [48_000, 192_000];
const MIX_FRAMES: [usize; 4] = [32, 64, 128, 256];
const MIX_SOURCES: [usize; 3] = [2, 4, 8];
const RESAMPLE_FRAMES: [usize; 3] = [64, 128, 256];
const RESAMPLE_PAIRS: [(usize, usize); 4] = [
    (44_100, 48_000),
    (48_000, 96_000),
    (48_000, 192_000),
    (96_000, 192_000),
];
/// Mixing runs in `i32`, `f32` and `f64`.
const MIX_FORMATS: usize = 3;
/// The resampler works on float only.
const FORMATS: usize = 2;
/// `high` and `medium`, each interpolating and each on exact phases.
const QUALITIES: usize = 4;
const MIX_CASES: usize = MIX_FRAMES.len() * MIX_RATES.len() * MIX_SOURCES.len() * MIX_FORMATS;
/// The built-in plugins, each timed alone on one block per cycle.
const PLUGINS: [&str; 2] = ["detect", "balance"];
const PLUGIN_RATES: [u32; 2] = [48_000, 192_000];
const PLUGIN_FRAMES: [usize; 2] = [64, 256];
/// A tone this far below full scale keeps the detector open, as on a record.
const TONE_DBFS: f64 = -20.0;
/// Balance a little to the right, so that the left channel is scaled.
const BALANCE: f64 = 0.5;
const PLUGIN_CASES: usize = PLUGINS.len() * PLUGIN_RATES.len() * PLUGIN_FRAMES.len() * MIX_FORMATS;
const RESAMPLE_CASES: usize = RESAMPLE_FRAMES.len() * RESAMPLE_PAIRS.len() * QUALITIES * FORMATS;
const FIFO_PRIORITY: i32 = 70;

#[derive(Parser)]
struct Args {
    /// Cycles timed for each combination.
    #[arg(long, default_value_t = 100_000)]
    cycles: usize,
    /// Run without real-time rights. The worst cases then mean little.
    #[arg(long)]
    no_rt: bool,
    /// Run the mixing and plugin cases only.
    #[arg(long)]
    mix_only: bool,
}

fn mix_case<T: Float>(
    to: fn(i32) -> T,
    from: fn(T) -> i32,
    sources: usize,
    frames: usize,
    cycles: usize,
) -> Summary {
    let samples = frames * CHANNELS;
    let inputs: Vec<Vec<i32>> = (0..sources)
        .map(|s| (0..samples).map(|i| (i * 7919 + s) as i32).collect())
        .collect();
    let mut floats = vec![vec![T::zero(); samples]; sources];
    let gains = vec![T::from(0.5).unwrap(); sources];
    let mut mixed = vec![T::zero(); samples];
    let mut out = vec![0i32; samples];
    let mut stats = Stats::with_warmup(WARMUP);
    for _ in 0..cycles + WARMUP {
        let start = Instant::now();
        for (input, float) in inputs.iter().zip(floats.iter_mut()) {
            for (i, f) in input.iter().zip(float.iter_mut()) {
                *f = to(*i);
            }
        }
        let mut refs: [&[T]; MAX_SOURCES] = [&[]; MAX_SOURCES];
        for (r, f) in refs.iter_mut().zip(floats.iter()) {
            *r = f;
        }
        mix_gain(&refs[..sources], &gains, &mut mixed);
        for (m, o) in mixed.iter().zip(out.iter_mut()) {
            *o = from(*m);
        }
        black_box(&mut out);
        stats.record(start.elapsed().as_nanos() as u64);
    }
    stats.summary()
}

/// Mixing in integers, the way the daemon's `i32` format does.
fn mix_case_i32(sources: usize, frames: usize, cycles: usize) -> Summary {
    let samples = frames * CHANNELS;
    let inputs: Vec<Vec<i32>> = (0..sources)
        .map(|s| (0..samples).map(|i| (i * 7919 + s) as i32).collect())
        .collect();
    let gains = vec![0.5f32; sources];
    let mut acc = vec![0i64; samples];
    let mut out = vec![0i32; samples];
    let mut stats = Stats::with_warmup(WARMUP);
    for _ in 0..cycles + WARMUP {
        let start = Instant::now();
        let mut refs: [&[i32]; MAX_SOURCES] = [&[]; MAX_SOURCES];
        for (r, input) in refs.iter_mut().zip(inputs.iter()) {
            *r = input;
        }
        mix_i32(&refs[..sources], &gains, &mut acc, &mut out);
        black_box(&mut out);
        stats.record(start.elapsed().as_nanos() as u64);
    }
    stats.summary()
}

/// The built-in plugin `kind` at `rate`, with the parameters it is timed with.
fn plugin_for(kind: &str, rate: u32) -> Option<Box<dyn Plugin>> {
    let mut plugin = builtin::create(kind, rate, 2)?.ok()?;
    if kind == "balance" {
        plugin.set_param(0, BALANCE);
    }
    Some(plugin)
}

/// A 1 kHz tone at `TONE_DBFS` peak on both channels, as `i32` samples.
fn tone_i32(rate: u32, frames: usize) -> Vec<[i32; 2]> {
    let amplitude = 10f64.powf(TONE_DBFS / 20.0);
    (0..frames)
        .map(|n| {
            let phase = 2.0 * std::f64::consts::PI * 1000.0 * n as f64 / f64::from(rate);
            let s = f64_to_s32(amplitude * phase.sin());
            [s, s]
        })
        .collect()
}

/// Times `plugin` on `source`, copied into a fresh block before each cycle
/// so that the plugin always sees the same audio. The copy is not timed.
fn plugin_timing<T: Copy>(
    plugin: &mut dyn Plugin,
    source: &[[T; 2]],
    wrap: fn(&mut [[T; 2]]) -> Block<'_>,
    cycles: usize,
) -> Summary {
    let mut block = source.to_vec();
    let mut stats = Stats::with_warmup(WARMUP);
    for _ in 0..cycles + WARMUP {
        block.copy_from_slice(source);
        let start = Instant::now();
        let status = plugin.process(wrap(&mut block));
        black_box(status);
        stats.record(start.elapsed().as_nanos() as u64);
    }
    black_box(&mut block);
    stats.summary()
}

/// Times the plugin `kind` in `i32`, `f32` and `f64`, each with an instance
/// of its own, and prints a row for each.
fn plugin_cases(kind: &str, rate: u32, frames: usize, cycles: usize) {
    let source = tone_i32(rate, frames);
    let f32s: Vec<[f32; 2]> = source.iter().map(|f| f.map(s32_to_f32)).collect();
    let f64s: Vec<[f64; 2]> = source.iter().map(|f| f.map(s32_to_f64)).collect();
    let fresh = || plugin_for(kind, rate).expect("a built-in plugin");
    let s = plugin_timing(
        fresh().as_mut(),
        &source,
        |b| Block {
            channels: 2,
            samples: Samples::I32(b.as_flattened_mut()),
        },
        cycles,
    );
    print_row(&format!("plugin {kind} i32 @{rate}"), frames, rate, &s, "");
    let s = plugin_timing(
        fresh().as_mut(),
        &f32s,
        |b| Block {
            channels: 2,
            samples: Samples::F32(b.as_flattened_mut()),
        },
        cycles,
    );
    print_row(&format!("plugin {kind} f32 @{rate}"), frames, rate, &s, "");
    let s = plugin_timing(
        fresh().as_mut(),
        &f64s,
        |b| Block {
            channels: 2,
            samples: Samples::F64(b.as_flattened_mut()),
        },
        cycles,
    );
    print_row(&format!("plugin {kind} f64 @{rate}"), frames, rate, &s, "");
}

/// Returns the timing and the delay the resampler adds, in output frames.
fn resample_case<T: Sample>(
    from_rate: usize,
    to_rate: usize,
    params: &SincInterpolationParameters,
    frames: usize,
    cycles: usize,
) -> (Summary, usize) {
    let ratio = to_rate as f64 / from_rate as f64;
    let mut resampler =
        Async::<T>::new_sinc(ratio, 1.1, params, frames, CHANNELS, FixedAsync::Input).unwrap();
    let input = vec![T::zero(); frames * CHANNELS];
    let capacity = resampler.output_frames_max();
    let mut output = vec![T::zero(); capacity * CHANNELS];
    let input_adapter = InterleavedSlice::new(&input, CHANNELS, frames).unwrap();
    let mut output_adapter = InterleavedSlice::new_mut(&mut output, CHANNELS, capacity).unwrap();
    let mut stats = Stats::with_warmup(WARMUP);
    for _ in 0..cycles + WARMUP {
        let start = Instant::now();
        let done = resampler
            .process_into_buffer(&input_adapter, &mut output_adapter, None)
            .unwrap();
        black_box(done);
        stats.record(start.elapsed().as_nanos() as u64);
    }
    (stats.summary(), resampler.output_delay())
}

/// The phases a fixed ratio takes: the output rate over the greatest common
/// divisor of the two rates.
fn exact_phases(from: usize, to: usize) -> usize {
    let (mut a, mut b) = (from, to);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    to / a
}

/// `rate` is the rate `frames` are at, which sets the time available.
fn print_row(label: &str, frames: usize, rate: u32, s: &Summary, extra: &str) {
    let budget = frames as f64 / f64::from(rate) * 1e9;
    println!(
        "{label:<34} {frames:>5} {:>9} {:>9} {:>9} {:>7.1}% {:>7.1}% {extra}",
        s.mean,
        s.p999,
        s.max,
        s.p999 as f64 / budget * 100.0,
        s.max as f64 / budget * 100.0,
    );
}

fn run_cases(cycles: usize, mix_only: bool) -> usize {
    let mut done = 0;
    for (frames, rate) in MIX_FRAMES
        .into_iter()
        .flat_map(|f| MIX_RATES.map(|r| (f, r)))
    {
        for sources in MIX_SOURCES {
            if stop::requested() {
                return done;
            }
            let s = mix_case_i32(sources, frames, cycles);
            print_row(&format!("mix i32 x{sources} @{rate}"), frames, rate, &s, "");
            let s = mix_case(s32_to_f32, f32_to_s32, sources, frames, cycles);
            print_row(&format!("mix f32 x{sources} @{rate}"), frames, rate, &s, "");
            let s = mix_case(s32_to_f64, f64_to_s32, sources, frames, cycles);
            print_row(&format!("mix f64 x{sources} @{rate}"), frames, rate, &s, "");
            done += MIX_FORMATS;
        }
    }
    for (frames, rate) in PLUGIN_FRAMES
        .into_iter()
        .flat_map(|f| PLUGIN_RATES.map(|r| (f, r)))
    {
        for kind in PLUGINS {
            if stop::requested() {
                return done;
            }
            plugin_cases(kind, rate, frames, cycles);
            done += MIX_FORMATS;
        }
    }
    if mix_only {
        return done;
    }
    let high = SincInterpolationParameters::new(128, WindowFunction::Blackman2)
        .oversampling_factor(256)
        .interpolation(SincInterpolationType::Cubic);
    let medium = SincInterpolationParameters::new(64, WindowFunction::Blackman2)
        .oversampling_factor(128)
        .interpolation(SincInterpolationType::Linear);
    for frames in RESAMPLE_FRAMES {
        for (from, to) in RESAMPLE_PAIRS {
            // A fixed ratio puts every output frame on one of `to / gcd`
            // phases, so a table with that many needs no interpolation.
            let phases = exact_phases(from, to);
            let exact = |length| {
                SincInterpolationParameters::new(length, WindowFunction::Blackman2)
                    .oversampling_factor(phases)
                    .interpolation(SincInterpolationType::Nearest)
            };
            let (high_exact, medium_exact) = (exact(128), exact(64));
            let cases = [
                ("high", &high),
                ("high exact", &high_exact),
                ("medium", &medium),
                ("medium exact", &medium_exact),
            ];
            for (quality, params) in cases {
                if stop::requested() {
                    return done;
                }
                let (s, delay) = resample_case::<f32>(from, to, params, frames, cycles);
                let extra = format!("delay {delay} frames");
                print_row(
                    &format!("resample f32 {quality} {from}>{to}"),
                    frames,
                    from as u32,
                    &s,
                    &extra,
                );
                let (s, delay) = resample_case::<f64>(from, to, params, frames, cycles);
                let extra = format!("delay {delay} frames");
                print_row(
                    &format!("resample f64 {quality} {from}>{to}"),
                    frames,
                    from as u32,
                    &s,
                    &extra,
                );
                done += FORMATS;
            }
        }
    }
    done
}

fn run(args: &Args) -> Result<(), String> {
    stop::install();
    let rt = rt::acquire(FIFO_PRIORITY, args.no_rt)?;
    let started = Instant::now();
    println!(
        "{:<34} {:>5} {:>9} {:>9} {:>9} {:>8} {:>8}",
        "case", "frames", "mean ns", "p99.9 ns", "max ns", "p99.9", "max"
    );
    let done = run_cases(args.cycles, args.mix_only);
    let planned = if args.mix_only {
        MIX_CASES + PLUGIN_CASES
    } else {
        MIX_CASES + PLUGIN_CASES + RESAMPLE_CASES
    };
    println!("{}", report_cases(done, planned));
    println!("took {:.1} s, rt {}", started.elapsed().as_secs_f64(), rt);
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
    fn every_plugin_case_is_a_built_in_plugin() {
        for kind in PLUGINS {
            assert!(plugin_for(kind, 48_000).is_some(), "{kind}");
        }
    }

    #[test]
    fn every_plugin_case_does_work() {
        // A neutral balance returns at once and would time nothing.
        for kind in PLUGINS {
            let plugin = plugin_for(kind, 48_000).unwrap();
            assert!(kind == "detect" || !plugin.neutral(), "{kind}");
        }
    }

    #[test]
    fn the_detector_is_open_on_the_timed_tone() {
        let mut plugin = plugin_for("detect", 48_000).unwrap();
        let mut block = tone_i32(48_000, 4800);
        let status = plugin.process(Block {
            channels: 2,
            samples: Samples::I32(block.as_flattened_mut()),
        });
        assert!(!status.no_signal);
    }
}
