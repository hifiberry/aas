//! How much of a period do mixing and resampling use, in 32 and 64-bit float?

use audioadapter_buffers::direct::InterleavedSlice;
use clap::Parser;
use measure::sample::{f32_to_s32, f64_to_s32, mix_gain, s32_to_f32, s32_to_f64};
use measure::stats::{Stats, Summary, report_header};
use num_traits::Float;
use rubato::{
    Async, FixedAsync, Resampler, Sample, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::hint::black_box;
use std::time::{Duration, Instant};

const CHANNELS: usize = 2;
const WARMUP: usize = 1000;
const MAX_SOURCES: usize = 8;
const MIX_RATES: [u32; 2] = [48_000, 192_000];

#[derive(Parser)]
struct Args {
    /// Cycles timed for each combination.
    #[arg(long, default_value_t = 100_000)]
    cycles: usize,
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

fn main() {
    let args = Args::parse();
    let started = Instant::now();
    println!(
        "{:<34} {:>5} {:>9} {:>9} {:>9} {:>8} {:>8}",
        "case", "frames", "mean ns", "p99.9 ns", "max ns", "p99.9", "max"
    );
    for (frames, rate) in [32, 64, 128, 256]
        .into_iter()
        .flat_map(|f| MIX_RATES.map(|r| (f, r)))
    {
        for sources in [2, 4, 8] {
            let s = mix_case(s32_to_f32, f32_to_s32, sources, frames, args.cycles);
            print_row(&format!("mix f32 x{sources} @{rate}"), frames, rate, &s, "");
            let s = mix_case(s32_to_f64, f64_to_s32, sources, frames, args.cycles);
            print_row(&format!("mix f64 x{sources} @{rate}"), frames, rate, &s, "");
        }
    }
    let high = SincInterpolationParameters::new(128, WindowFunction::Blackman2)
        .oversampling_factor(256)
        .interpolation(SincInterpolationType::Cubic);
    let medium = SincInterpolationParameters::new(64, WindowFunction::Blackman2)
        .oversampling_factor(128)
        .interpolation(SincInterpolationType::Linear);
    for frames in [64, 128, 256] {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 96_000),
            (48_000, 192_000),
            (96_000, 192_000),
        ] {
            for (quality, params) in [("high", &high), ("medium", &medium)] {
                let (s, delay) = resample_case::<f32>(from, to, params, frames, args.cycles);
                let extra = format!("delay {delay} frames");
                print_row(
                    &format!("resample f32 {quality} {from}>{to}"),
                    frames,
                    from as u32,
                    &s,
                    &extra,
                );
                let (s, delay) = resample_case::<f64>(from, to, params, frames, args.cycles);
                let extra = format!("delay {delay} frames");
                print_row(
                    &format!("resample f64 {quality} {from}>{to}"),
                    frames,
                    from as u32,
                    &s,
                    &extra,
                );
            }
        }
    }
    let actual = started.elapsed();
    println!("{}", report_header(Duration::ZERO, actual));
}
