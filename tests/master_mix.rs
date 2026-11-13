//! Pins what the engine gives its card, sample for sample, over scripted
//! runs that pass every part of the output's own path: the copy path, the
//! direct mixing path, a matrix that is not the default, a change of matrix
//! that ramps and another that arrives during it, the output gain's fade,
//! the analog input playing, the mix and output chains, a chain at another
//! rate and the fade in after it, DoP, and the fade before a reopen.
//!
//! Each run is 60 cycles of 64 frames in `i32`, `f32` and `f64`, with
//! bit-perfect off, auto and forced, on a stereo card and on one of four
//! channels. The SHA-256 of everything the card got, as little-endian
//! `i32` samples, is compared with the one recorded from the engine as it
//! was before its output's mix moved into a struct of its own.

use aas::capture::{CaptureFeed, MemoryCapture};
use aas::config::{BitPerfect, Format, Route};
use aas::counters::Counters;
use aas::engine::{Command, Engine, Event, Place, Source, Target};
use aas::format::{F32, F64, I32, Processing, db_to_gain};
use aas::latency::MAX_PERIOD;
use aas::matrix::Matrix;
use aas::plugin::Plugin;
use aas::plugin::balance::Balance;
use aas::plugin::chain::Chain;
use aas::plugin::detect::Detect;
use aas::sink::{MemorySink, SinkLog};
use aas_client::ring::Ring;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::Arc;

const RATE: u32 = 48000;
const PERIOD: usize = 64;
const CYCLES: usize = 60;

/// The SHA-256 of each run, by card, format and mode, as recorded.
const EXPECTED: [(&str, &str); 18] = [
    (
        "stereo i32 off",
        "c3fc691ff511f8caada78cc1786844eea192bb9a37dceab9f7feea3c0d4d006f",
    ),
    (
        "stereo i32 auto",
        "c3fc691ff511f8caada78cc1786844eea192bb9a37dceab9f7feea3c0d4d006f",
    ),
    (
        "stereo i32 forced",
        "482d4c7d97027cb6f97637ea8a470071343460053408fa7b507c0c3e8d55e99d",
    ),
    (
        "stereo f32 off",
        "d5ec975545212236d0f4f74ed488f1baeee6e48f38e645fe6086a71fe99267f7",
    ),
    (
        "stereo f32 auto",
        "ded4e20b0ee11de93c036a24aadb0270d2b4d82fa36ef06a337866aba4e146a1",
    ),
    (
        "stereo f32 forced",
        "795e624fdee8f8780396c08d5dd3922ddd0fe6a7998e269ff952a8fabd10b302",
    ),
    (
        "stereo f64 off",
        "97b660cf93746acbad2b95d5a526be6624205e710834b56bbff6ff4c4b7e2657",
    ),
    (
        "stereo f64 auto",
        "97b660cf93746acbad2b95d5a526be6624205e710834b56bbff6ff4c4b7e2657",
    ),
    (
        "stereo f64 forced",
        "63d1addaaaf5f7c940d7234e416858e80fbbcb843aa84081af70c558b29b24a5",
    ),
    (
        "wide i32 off",
        "82cfe97f2c19f5517a692312bebe0256d9d3dfa4b2cc60a38fa2124e61f42d6c",
    ),
    (
        "wide i32 auto",
        "82cfe97f2c19f5517a692312bebe0256d9d3dfa4b2cc60a38fa2124e61f42d6c",
    ),
    (
        "wide i32 forced",
        "06eaf2c718d460309856a044b2a3800706c2939aaff61d52c99ae73a954a78d6",
    ),
    (
        "wide f32 off",
        "45c1d7666aea61d85d89133ad3ed8f396876c81462304f4b88187c49aff15194",
    ),
    (
        "wide f32 auto",
        "45c1d7666aea61d85d89133ad3ed8f396876c81462304f4b88187c49aff15194",
    ),
    (
        "wide f32 forced",
        "44811e9c351d9d4c2fc9d42a94587381d33853fefb5a64ac6453cac72c90751a",
    ),
    (
        "wide f64 off",
        "3ccaceff3a06a68ca1f28922be76c3845ae7cb7f30146e6f530cdac0db5cb495",
    ),
    (
        "wide f64 auto",
        "3ccaceff3a06a68ca1f28922be76c3845ae7cb7f30146e6f530cdac0db5cb495",
    ),
    (
        "wide f64 forced",
        "2159eb783b66a54f84f71d9888acbe7a22d01c1ed9b986dceb4b5f7106970cfb",
    ),
];

/// SHA-256, FIPS 180-4, of `data`.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&(data.len() as u64 * 8).to_be_bytes());
    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (bytes, word) in out.chunks_exact_mut(4).zip(h) {
        bytes.copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn eventfd() -> OwnedFd {
    let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    assert!(raw >= 0);
    unsafe { OwnedFd::from_raw_fd(raw) }
}

/// Noise from a fixed seed, at about -12 dBFS, every low bit in use.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> i32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 32) as u32 as i32) >> 2
    }

    fn period(&mut self) -> Vec<[i32; 2]> {
        (0..PERIOD).map(|_| [self.next(), self.next()]).collect()
    }
}

/// A period of DoP, its markers alternating from where `count` stands.
fn dop_period(count: &mut usize) -> Vec<[i32; 2]> {
    (0..PERIOD)
        .map(|_| {
            let marker: u32 = if *count % 2 == 0 { 0x05 } else { 0xFA };
            *count += 1;
            let bits = (*count as u32).wrapping_mul(0x9E37_79B9) & 0x00FF_FF00;
            let s = ((marker << 24) | bits) as i32;
            [s, s ^ 0x0000_A500]
        })
        .collect()
}

fn route(from: usize, to: usize, gain_db: f32) -> Route {
    Route { from, to, gain_db }
}

fn matrix(routes: &[Route], width: usize) -> Box<Matrix> {
    Box::new(Matrix::from_routes(routes, width).unwrap())
}

fn format_of<P: Processing>() -> Format {
    match std::any::type_name::<P>() {
        name if name.ends_with("I32") => Format::I32,
        name if name.ends_with("F32") => Format::F32,
        _ => Format::F64,
    }
}

fn balance(format: Format, value: f64, rate: u32) -> Box<Chain> {
    let mut plugin = Balance::new();
    plugin.set_param(0, value);
    let plugins: Vec<Box<dyn Plugin>> = vec![Box::new(plugin)];
    let chain = Chain::new(plugins, format, MAX_PERIOD as usize, 2).unwrap();
    Box::new(chain.for_rate(rate))
}

/// A detector told a rate of 1280 Hz, so that its window of 50 ms is one
/// period: it opens in the second period of signal, not the 38th.
fn detector(format: Format) -> Box<Chain> {
    let plugins: Vec<Box<dyn Plugin>> = vec![Box::new(Detect::new(1280))];
    let chain = Chain::new(plugins, format, MAX_PERIOD as usize, 2).unwrap();
    Box::new(chain.for_rate(RATE))
}

struct Run<P: Processing> {
    engine: Engine<P, MemorySink>,
    commands: rtrb::Producer<Command>,
    events: rtrb::Consumer<Event>,
    log: SinkLog,
    /// A DoP stream took the output.
    dop_played: bool,
    /// The analog input had signal.
    analog_played: bool,
}

impl<P: Processing> Run<P> {
    fn new(mode: BitPerfect, matrix: Matrix, channels: usize) -> Self {
        let sink = MemorySink::with_channels(channels);
        let log = sink.log();
        log.reserve(1 << 16);
        let (commands, consumer) = rtrb::RingBuffer::new(256);
        let (producer, events) = rtrb::RingBuffer::new(256);
        let counters = Arc::new(Counters::default());
        let engine = Engine::new(sink, consumer, producer, counters, mode, matrix, 0);
        Run {
            engine,
            commands,
            events,
            log,
            dop_played: false,
            analog_played: false,
        }
    }

    fn send(&mut self, command: Command) {
        assert!(self.commands.push(command).is_ok());
    }

    fn cycle(&mut self) {
        self.engine.cycle().unwrap();
        while let Ok(event) = self.events.pop() {
            match event {
                Event::DopStarted(..) => self.dop_played = true,
                Event::InputSignal(true) => self.analog_played = true,
                _ => {}
            }
        }
    }

    fn add(&mut self, id: u64, gain: f32) -> Ring {
        let (ring, fd) = Ring::create(1 << 16).unwrap();
        let client = Ring::map(fd.as_fd()).unwrap();
        let mut source = Source::new(id, ring, eventfd(), RATE);
        source.gain = gain;
        self.send(Command::Add(Box::new(source)));
        self.send(Command::SetRunning(id, true));
        client
    }

    fn capture(&mut self, format: Format) -> CaptureFeed {
        let (capture, feed) = MemoryCapture::new();
        self.send(Command::SetCapture(Some(Box::new(capture))));
        self.send(Command::SetChain(
            Target::master(Place::Analog),
            detector(format),
        ));
        feed
    }

    fn hash(&self) -> String {
        let bytes: Vec<u8> = self
            .log
            .samples()
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        hex(&sha256(&bytes))
    }
}

/// A stereo card: the copy path, the direct path, two ramps one into the
/// other, a swap, the mix and output chains, a chain at another rate, DoP
/// and a reopen.
fn stereo<P: Processing>(mode: BitPerfect) -> String {
    let format = format_of::<P>();
    let mut run = Run::<P>::new(mode, Matrix::default_for(2), 2);
    run.send(Command::Reconfigure {
        rate: RATE,
        period: PERIOD as u32,
    });
    run.cycle();
    let player = run.add(1, 1.0);
    let dop = run.add(2, 1.0);
    let mut noise = Noise(1);
    let mut analog_noise = Noise(2);
    let mut dop_count = 0;
    let mut feed = None;
    for cycle in 0..CYCLES {
        match cycle {
            3 => run.send(Command::SetGain(0, db_to_gain(-6.0))),
            6 => feed = Some(run.capture(format)),
            12 => {
                // A swap of the players, the analog input on both sides.
                let routes = [
                    route(0, 2, 0.0),
                    route(1, 1, 0.0),
                    route(2, 1, -3.0),
                    route(3, 2, -3.0),
                ];
                run.send(Command::SetMatrix(0, matrix(&routes, 2)));
            }
            // During that ramp.
            15 => {
                let routes = [
                    route(0, 2, 0.0),
                    route(1, 1, 0.0),
                    route(2, 2, -6.0),
                    route(3, 1, -6.0),
                ];
                run.send(Command::SetMatrix(0, matrix(&routes, 2)));
            }
            22 => run.send(Command::SetChain(
                Target::master(Place::Mix),
                balance(format, 0.4, RATE),
            )),
            // Built for another rate: silence, then a fade in.
            26 => run.send(Command::SetChain(
                Target::master(Place::Mix),
                balance(format, 0.4, 44100),
            )),
            28 => run.send(Command::SetChain(
                Target::master(Place::Mix),
                balance(format, -0.3, RATE),
            )),
            30 => run.send(Command::SetGain(0, 1.0)),
            33 => {
                run.send(Command::SetCapture(None));
                feed = None;
                run.send(Command::SetChain(
                    Target::master(Place::Mix),
                    Box::new(Chain::empty()),
                ));
                run.send(Command::SetMatrix(0, Box::new(Matrix::default_for(2))));
            }
            36 => {
                let routes = [route(0, 2, 0.0), route(1, 1, 0.0)];
                run.send(Command::SetMatrix(0, matrix(&routes, 2)));
            }
            53 => run.send(Command::SetRunning(2, false)),
            54 => run.send(Command::SetChain(
                Target::master(Place::Output),
                balance(format, -0.5, RATE),
            )),
            56 => run.send(Command::SetMatrix(0, Box::new(Matrix::default_for(2)))),
            58 => run.send(Command::Reconfigure {
                rate: RATE,
                period: PERIOD as u32,
            }),
            _ => {}
        }
        player.write(&noise.period());
        if (45..53).contains(&cycle) {
            dop.write(&dop_period(&mut dop_count));
        }
        if let Some(feed) = &feed {
            feed.push(&analog_noise.period());
        }
        run.cycle();
    }
    assert!(run.dop_played, "DoP played");
    assert!(run.analog_played, "the analog input played");
    run.hash()
}

/// A card of four channels: the players on 1 and 2 and some of them on 3,
/// the analog input on 3 and 4, a gain fade, a change of matrix, a chain
/// at another rate and the copy path.
fn wide<P: Processing>(mode: BitPerfect) -> String {
    let format = format_of::<P>();
    let first = [
        route(0, 1, 0.0),
        route(1, 2, 0.0),
        route(0, 3, -12.0),
        route(2, 3, -6.0),
        route(3, 4, -6.0),
    ];
    let mut run = Run::<P>::new(mode, *matrix(&first, 4), 4);
    run.send(Command::Reconfigure {
        rate: RATE,
        period: PERIOD as u32,
    });
    run.cycle();
    let player = run.add(1, db_to_gain(-1.5));
    let mut noise = Noise(3);
    let mut analog_noise = Noise(4);
    let mut feed = None;
    for cycle in 0..CYCLES {
        match cycle {
            4 => feed = Some(run.capture(format)),
            10 => run.send(Command::SetGain(0, db_to_gain(-10.0))),
            20 => {
                let routes = [
                    route(1, 1, 0.0),
                    route(0, 2, 0.0),
                    route(2, 4, -1.0),
                    route(3, 3, 0.0),
                ];
                run.send(Command::SetMatrix(0, matrix(&routes, 4)));
            }
            34 => {
                let plugins: Vec<Box<dyn Plugin>> = Vec::new();
                let chain = Chain::new(plugins, format, MAX_PERIOD as usize, 4).unwrap();
                run.send(Command::SetChain(
                    Target::master(Place::Mix),
                    Box::new(chain.for_rate(44100)),
                ));
            }
            38 => run.send(Command::SetChain(
                Target::master(Place::Mix),
                Box::new(Chain::empty()),
            )),
            44 => {
                run.send(Command::SetCapture(None));
                feed = None;
                run.send(Command::SetGain(0, 1.0));
                let routes = [route(0, 1, 0.0), route(1, 2, 0.0)];
                run.send(Command::SetMatrix(0, matrix(&routes, 4)));
            }
            _ => {}
        }
        player.write(&noise.period());
        if let Some(feed) = &feed {
            feed.push(&analog_noise.period());
        }
        run.cycle();
    }
    assert!(run.analog_played);
    run.hash()
}

fn every_run() -> Vec<(String, String)> {
    let modes = [
        (BitPerfect::Off, "off"),
        (BitPerfect::Auto, "auto"),
        (BitPerfect::Forced, "forced"),
    ];
    let mut all = Vec::new();
    for (card, wide_card) in [("stereo", false), ("wide", true)] {
        for (format, name) in [("i32", 0), ("f32", 1), ("f64", 2)].map(|(n, i)| (i, n)) {
            for (mode, mode_name) in modes {
                let hash = match (wide_card, format) {
                    (false, 0) => stereo::<I32>(mode),
                    (false, 1) => stereo::<F32>(mode),
                    (false, _) => stereo::<F64>(mode),
                    (true, 0) => wide::<I32>(mode),
                    (true, 1) => wide::<F32>(mode),
                    (true, _) => wide::<F64>(mode),
                };
                all.push((format!("{card} {name} {mode_name}"), hash));
            }
        }
    }
    all
}

#[test]
fn sha256_is_sha256() {
    assert_eq!(
        hex(&sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&sha256(&[0x61; 1000])),
        "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
    );
}

#[test]
fn the_master_mix_is_unchanged_by_the_split() {
    let got = every_run();
    for (name, hash) in &got {
        println!("    (\"{name}\", \"{hash}\"),");
    }
    let expected: Vec<(String, String)> = EXPECTED
        .iter()
        .map(|(n, h)| (n.to_string(), h.to_string()))
        .collect();
    assert_eq!(got, expected);
}
