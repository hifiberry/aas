//! What one engine cycle costs at the default configuration: one player at
//! 44.1 kHz in periods of 256 frames, the default stereo matrix, bit-perfect
//! off so that the stream goes through the mix, and the analog input off,
//! quiet or playing. Not run by default: it measures, it does not check.
//!
//! `cargo test --release --test cycle_cost -- --ignored --nocapture`
//!
//! Each line gives the fastest, middle and slowest of seven runs of 20,000
//! cycles, in nanoseconds per cycle. The sink takes the period and does
//! nothing with it, so the figure is the cycle's own work. Pin the process
//! to one CPU of your choice, the same for every build, with `taskset -c`
//! for figures that compare between builds.

use aas::capture::MemoryCapture;
use aas::config::{BitPerfect, Format};
use aas::counters::Counters;
use aas::engine::{Command, Engine, Place, Source};
use aas::format::{F32, F64, I32, Processing};
use aas::latency::MAX_PERIOD;
use aas::matrix::Matrix;
use aas::plugin::Plugin;
use aas::plugin::chain::Chain;
use aas::plugin::detect::Detect;
use aas::sink::{Sink, SinkError};
use aas_client::ring::Ring;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RATE: u32 = 44100;
const PERIOD: usize = 256;

/// Takes every period and drops it.
struct NullSink;

impl Sink for NullSink {
    fn open(&mut self, _rate: u32, _period: u32) -> Result<(), SinkError> {
        Ok(())
    }

    fn write(&mut self, samples: &[i32]) -> Result<(), SinkError> {
        std::hint::black_box(samples);
        Ok(())
    }

    fn channels(&self) -> usize {
        2
    }

    fn close(&mut self) {}

    fn delay_frames(&self) -> u32 {
        0
    }

    fn supported_rates(&self) -> Vec<u32> {
        Vec::new()
    }
}

/// A tone with the low bits set.
fn tone(amp: f64, hz: f64) -> Vec<[i32; 2]> {
    (0..PERIOD)
        .map(|i| {
            let x = (i as f64 * hz / f64::from(RATE) * std::f64::consts::TAU).sin();
            let v = (x * amp * 2_147_483_647.0) as i32;
            [v | 1, (-v) ^ 0x55]
        })
        .collect()
}

fn eventfd() -> OwnedFd {
    let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    assert!(raw >= 0);
    unsafe { OwnedFd::from_raw_fd(raw) }
}

#[derive(Clone, Copy)]
enum Analog {
    Off,
    /// A capture of silence: read and measured each cycle, never mixed.
    Quiet,
    /// A capture of a tone, mixed with the player.
    Playing,
}

/// Nanoseconds per cycle over `cycles` cycles.
fn run<P: Processing>(format: Format, analog: Analog, cycles: usize) -> f64 {
    let (mut commands, consumer) = rtrb::RingBuffer::new(64);
    let (producer, mut events) = rtrb::RingBuffer::new(64);
    let counters = Arc::new(Counters::default());
    let matrix = Matrix::default_for(2);
    let mode = BitPerfect::Off;
    let mut engine = Engine::<P, _>::new(NullSink, consumer, producer, counters, mode, matrix);
    let mut send = |command| assert!(commands.push(command).is_ok());
    send(Command::Reconfigure {
        rate: RATE,
        period: PERIOD as u32,
    });
    // A reopen closes the capture, so it comes once the output is open.
    engine.cycle().unwrap();
    let feed = match analog {
        Analog::Off => None,
        Analog::Quiet | Analog::Playing => {
            let (capture, feed) = MemoryCapture::new();
            let plugins: Vec<Box<dyn Plugin>> = vec![Box::new(Detect::new(RATE))];
            let chain = Chain::new(plugins, format, MAX_PERIOD as usize, 2).unwrap();
            send(Command::SetCapture(Some(Box::new(capture))));
            send(Command::SetChain(Place::Analog, Box::new(chain)));
            Some(feed)
        }
    };
    let (ring, fd) = Ring::create(1 << 16).unwrap();
    let client = Ring::map(fd.as_fd()).unwrap();
    let source = Source::new(1, ring, eventfd(), RATE);
    send(Command::Add(Box::new(source)));
    send(Command::SetRunning(1, true));
    let block = tone(0.5, 997.0);
    let captured = match analog {
        Analog::Playing => tone(0.3, 440.0),
        _ => vec![[0; 2]; PERIOD],
    };
    for _ in 0..4 {
        client.write(&block);
    }
    let warm = 500;
    let mut total = Duration::ZERO;
    for cycle in 0..warm + cycles {
        client.write(&block);
        if let Some(feed) = &feed {
            feed.push(&captured);
        }
        let start = Instant::now();
        engine.cycle().unwrap();
        let took = start.elapsed();
        if cycle >= warm {
            total += took;
        }
        while events.pop().is_ok() {}
    }
    total.as_nanos() as f64 / cycles as f64
}

fn series<P: Processing>(format: Format, name: &str) {
    for (analog, label) in [
        (Analog::Off, "off"),
        (Analog::Quiet, "quiet"),
        (Analog::Playing, "playing"),
    ] {
        let mut runs: Vec<f64> = (0..7).map(|_| run::<P>(format, analog, 20_000)).collect();
        runs.sort_by(f64::total_cmp);
        eprintln!(
            "{name} analog {label:7} {:6.0} {:6.0} {:6.0} ns per cycle",
            runs[0], runs[3], runs[6]
        );
    }
}

#[test]
#[ignore]
fn cycle_cost_at_the_default_config() {
    series::<F32>(Format::F32, "f32");
    series::<F64>(Format::F64, "f64");
    series::<I32>(Format::I32, "i32");
}
