//! The control thread's decisions: which clients play, at what rate, and
//! when the output is open. The audio thread only carries them out.
//!
//! Everything the audio thread needs is built here, and whatever it hands
//! back is freed here. Commands that belong together go into the queue in
//! one commit, so the audio thread never sees half a decision.

use crate::adaptive::{Adaptive, MAX_DRIFT};
use crate::capture::{Capture, OpenError};
use crate::config::{Config, Format, Idle, OutputConfig, PluginConfig, PluginPlace, Quality};
use crate::dop;
use crate::drift::SANE;
use crate::engine::{Command, Event, INITIAL_SOURCES, INITIAL_TAPS, Source, Tap, Target};
use crate::format::{F32, F64, I32, Processing, db_to_gain};
use crate::further::{BUFFERED, FEED_PERIODS, Feed, FurtherEvent, FurtherHandle};
use crate::latency::{MAX_PERIOD, PERIODS, period_frames};
use crate::matrix::Matrix;
use crate::output_mix::OutputMix;
use crate::plugin::build::{Chains, Libraries, build_chain, engine_place, widths};
use crate::plugin::chain::Chain;
use crate::plugin::{BuildError, Failure};
use crate::rate::{ClientId, MAX_RATE, MIN_RATE, RatePolicy, SETTLE, STANDARD_RATES};
use crate::resample::{Resampler, taps};
use crate::state::{Saver, State};
use crate::volume::{HardwareVolume, OutputVolume};
use aas_client::convert::ClientFormat;
use aas_client::proto::{self, Reply, Request};
use aas_client::ring::{self, Header, Ring};
use rtrb::{Consumer, Producer};
use std::any::Any;
use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::thread::Thread;
use std::time::{Duration, Instant};

/// Identifies a connection to the server.
pub type ConnId = u64;

/// Opens the card's capture at a rate and period. Called on the control
/// thread, after the output has opened at them.
///
/// It must not wait: while it runs, no client request is served. A device
/// that another process holds must make it fail at once, not block until
/// the device is free. A failed open is tried again a second later.
///
/// An `OpenError::Never`, for a card found to have no analog input aas
/// knows, ends the analog input: it is not tried again, and the idle rule
/// of the config applies again.
pub type OpenCapture = Box<dyn FnMut(u32, u32) -> Result<Box<dyn Capture>, OpenError>>;

/// The name the analog input has in the rate rules while it is a client.
pub const ANALOG: ClientId = ClientId::MAX;

/// Smallest period the latency rule gives.
const MIN_PERIOD: u32 = 32;
/// Half the longest resampling filter, which rubato adds to its input.
const FILTER_HALF: u32 = 64;
/// Headroom for the ratio change rubato prepares for.
const RATIO_HEADROOM: f64 = 1.1;
/// How long a drain may wait beyond what its ring holds, should the audio
/// thread never report it drained.
const DRAIN_SPARE: Duration = Duration::from_millis(500);
/// How long the control thread waits for room in the command queue before
/// it says so in the log. It goes on waiting.
const SEND_WARN: Duration = Duration::from_secs(1);
/// How long the volume stays unchanged before the state file is written,
/// so that a volume slider being dragged does not write it many times.
const SAVE_DELAY: Duration = Duration::from_secs(1);
/// How often a capture that failed, or would not open, is tried again.
const CAPTURE_RETRY: Duration = Duration::from_secs(1);
/// Periods of the output a capture stream's ring holds at least.
const RECORDER_PERIODS: u32 = 4;
/// How long after the master's card opens under a parked cycle a further
/// output's starve is put down to the open, which stalls the cycle.
const RESUME_STARVE: Duration = Duration::from_secs(2);

/// Chains built for a rate, not yet sent to the audio thread.
struct Spare {
    rate: u32,
    /// Built for a DoP stream: the analog chain only.
    dop: bool,
    chains: Vec<(PluginPlace, Chain)>,
}

/// Where a place's chain is kept in `Control::built`.
fn slot(place: PluginPlace) -> usize {
    match place {
        PluginPlace::Analog => 0,
        PluginPlace::Mix => 1,
        PluginPlace::Output => 2,
    }
}

/// Frames a client's ring holds at least, so that one period's input fits
/// whatever rate the output runs at. One period takes at most what the
/// latency target lasts, or the smallest period at the lowest rate.
pub fn min_ring_frames(rate: u32, latency_ms: f32) -> u32 {
    let per_period = f64::from(latency_ms) / 1000.0 / f64::from(PERIODS);
    let shortest = f64::from(MIN_PERIOD) / f64::from(MIN_RATE);
    let input = f64::from(rate) * per_period.max(shortest) * RATIO_HEADROOM;
    input.ceil() as u32 + 2 + FILTER_HALF + dop::MIN_FRAMES as u32
}

/// The smallest buffer a client may ask for: what one period of the
/// output may take from its ring, and one period more to refill it in.
/// Below that every cycle finds the ring short, an underrun each time.
pub fn min_buffer_frames(rate: u32, latency_ms: f32) -> u32 {
    let per_period = f64::from(latency_ms) / 1000.0 / f64::from(PERIODS);
    let shortest = f64::from(MIN_PERIOD) / f64::from(MIN_RATE);
    let refill = (f64::from(rate) * per_period.max(shortest)).ceil() as u32;
    min_ring_frames(rate, latency_ms) + refill
}

/// The margin a further output's path holds beyond what its parts need:
/// half a master period, at least 2 ms, in master frames. The writer's
/// measurement needs a quarter to a half of a period, and the first lock
/// overshoots a little.
fn margin_frames(master_rate: u32, master_period: u32) -> f64 {
    (f64::from(master_period) / 2.0).max(f64::from(master_rate) / 500.0)
}

/// What a card's buffer and its resampler hold, in master frames: `BUFFERED`
/// periods and the filter, `(card rate, card period, resampler delay in
/// card frames)`.
fn card_part(master_rate: u32, (rate, period, delay): (u32, u32, f64)) -> f64 {
    let scale = f64::from(master_rate) / f64::from(rate);
    (f64::from(BUFFERED * period) + delay) * scale
}

/// The delay every output is held at, in master frames: a master period in
/// the feed, then the card's buffer and the resampler of each further output,
/// given as `(card rate, card period, resampler delay in card frames)`, and
/// the margin, rounded up and taken over the slowest path. 0 without
/// further outputs.
pub fn align_frames(master_rate: u32, master_period: u32, further: &[(u32, u32, f64)]) -> u32 {
    let margin = margin_frames(master_rate, master_period);
    further
        .iter()
        .map(|&part| f64::from(master_period) + card_part(master_rate, part) + margin)
        .fold(0.0f64, f64::max)
        .ceil() as u32
}

/// The frames the feed of a further output holds at a configuration: what
/// the target leaves to the feed; a card period, which the feed holds
/// while the card is a period short of the full buffer the target counts,
/// when the writer wakes; another, which the master pushes on while the
/// writer wakes late and resamples before it takes its period from the
/// feed at once; and two master periods. At least `FEED_PERIODS` master
/// periods, and two of the card's periods and the margin.
///
/// The target holds three card periods and the feed's share, so this is
/// at most the target and two master periods.
pub(crate) fn feed_frames(
    master_rate: u32,
    master_period: u32,
    target: u32,
    part: (u32, u32, f64),
) -> usize {
    let share = f64::from(target) - card_part(master_rate, part);
    let card = writer_frames(master_rate, part);
    let fed = (share.max(0.0) + 2.0 * card).ceil() as usize + 2 * master_period as usize;
    let least = (2.0 * card + margin_frames(master_rate, master_period)).ceil() as usize;
    fed.max(FEED_PERIODS * master_period as usize).max(least)
}

/// The delay a fresh resampler from `master` to `card` Hz brings, in card
/// frames, as `Adaptive::delay_frames` gives it at its nominal ratio.
fn resampler_delay(quality: Quality, master: u32, card: u32) -> f64 {
    taps(quality) as f64 * f64::from(card) / f64::from(master) / 2.0 - 1.0
}

/// A further card's part of the path with the master at `master` Hz and the
/// card at `card` Hz: its rate, its period by the latency rule for
/// `latency_ms`, and its resampler's delay, as `align_frames` takes it.
fn part_at(master: u32, card: u32, latency_ms: f32, quality: Quality) -> (u32, u32, f64) {
    let period = period_frames(card, latency_ms);
    (card, period, resampler_delay(quality, master, card))
}

/// The writer's period in master frames.
fn writer_frames(master_rate: u32, (rate, period, _): (u32, u32, f64)) -> f64 {
    f64::from(period) * f64::from(master_rate) / f64::from(rate)
}

/// The fullest feed the drift believes at `target`: a full feed has to read
/// as a delay it takes in, or a path that overflows starts over every
/// second.
fn believed(master_rate: u32, target: u32, part: (u32, u32, f64)) -> f64 {
    f64::from(target) + SANE * writer_frames(master_rate, part)
}

/// The feed of a further output at a configuration, as `feed_frames` says,
/// within what it was made with and what the drift believes; true if it
/// had to be cut short for the drift.
fn feed_within(
    master_rate: u32,
    master_period: u32,
    target: u32,
    part: (u32, u32, f64),
    capacity: usize,
) -> (usize, bool) {
    let feed = feed_frames(master_rate, master_period, target, part).min(capacity);
    let most = believed(master_rate, target, part).ceil() as usize - 1;
    (feed.min(most), feed > most)
}

/// The rates the start checks a further output's latency at: those of the
/// cards aas is for. Below them a feed that the drift would not believe is
/// cut short at run time, and logged.
fn checked_rates() -> impl Iterator<Item = u32> {
    STANDARD_RATES.into_iter().filter(|&r| r >= 44100)
}

/// Refuses an output latency the drift cannot follow beside the master's:
/// one so much shorter that the master hands over more at a time than the
/// feed may hold for the drift to believe it. Checked with the master and
/// the card each at every rate from 44.1 kHz. `number` names the output.
pub fn check_alignment(
    master_latency_ms: f32,
    number: usize,
    latency_ms: f32,
    quality: Quality,
) -> Result<(), String> {
    for master in checked_rates() {
        let period = period_frames(master, master_latency_ms);
        for card in checked_rates() {
            let part = part_at(master, card, latency_ms, quality);
            let target = align_frames(master, period, &[part]);
            let feed = feed_frames(master, period, target, part);
            if feed as f64 >= believed(master, target, part) {
                return Err(format!(
                    "output {number}: latency_ms = {latency_ms} is too short beside the \
                     master's {master_latency_ms}: with the master at {master} Hz and the card \
                     at {card} Hz the master hands over {period} frames at a time, more than \
                     periods of {} frames can follow",
                    part.1
                ));
            }
        }
    }
    Ok(())
}

/// Every further card's part at every rate, for the master at `master` Hz.
fn all_parts(master: u32, latencies: &[f32], quality: Quality) -> Vec<(u32, u32, f64)> {
    let cards = STANDARD_RATES.iter();
    let each = |&card: &u32| latencies.iter().map(move |&l| (card, l));
    cards
        .flat_map(each)
        .map(|(card, l)| part_at(master, card, l, quality))
        .collect()
}

/// The frames a further output's feed is made with: the most it holds at
/// any rate the master and the cards may run at, the further outputs'
/// latencies being `latencies`, with a master period to spare for a rate
/// between the standard ones. A feed holds at most the target and two
/// master periods (`feed_frames`), the target here being the longest path.
pub fn feed_capacity(master_latency_ms: f32, latencies: &[f32], quality: Quality) -> usize {
    STANDARD_RATES
        .iter()
        .map(|&master| {
            let period = period_frames(master, master_latency_ms) as usize;
            let target = align_frames(
                master,
                period as u32,
                &all_parts(master, latencies, quality),
            );
            (target as usize + 2 * period).max(FEED_PERIODS * period) + period
        })
        .max()
        .unwrap_or(0)
}

/// The longest delay line the master needs, in frames: the most the
/// alignment target can be beyond its two periods at any rate the master
/// and the cards may run at, with a master period to spare. 0 without
/// further outputs. The engine's line is made this long, so no valid config
/// needs a cap.
pub fn delay_capacity(master_latency_ms: f32, latencies: &[f32], quality: Quality) -> u32 {
    if latencies.is_empty() {
        return 0;
    }
    STANDARD_RATES
        .iter()
        .map(|&master| {
            let period = period_frames(master, master_latency_ms);
            let target = align_frames(master, period, &all_parts(master, latencies, quality));
            target.saturating_sub(PERIODS * period) + period
        })
        .max()
        .unwrap_or(0)
}

/// The rate a further card runs at: the master's if it takes it, else the
/// nearest it takes, the lower of two as near. `rates` empty takes any.
pub fn card_rate(rates: &[u32], master_rate: u32) -> u32 {
    if rates.is_empty() || rates.contains(&master_rate) {
        return master_rate;
    }
    let nearest = rates.iter().min_by_key(|&&r| (r.abs_diff(master_rate), r));
    nearest.copied().unwrap_or(master_rate)
}

/// How the master misses the time the further outputs play at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misaligned {
    /// The delay line holds `capacity` frames, not the `wanted`: the
    /// master plays ahead.
    Ahead { capacity: u32, wanted: u32 },
    /// The master's own buffer is `frames` longer than the target: the
    /// further outputs play ahead of it.
    Behind { frames: u32 },
}

impl Misaligned {
    pub fn line(self) -> String {
        match self {
            Misaligned::Ahead { capacity, wanted } => format!(
                "aas: warning: the master can be held back by {capacity} frames, \
                 not {wanted}: it plays ahead of the other outputs"
            ),
            Misaligned::Behind { frames } => format!(
                "aas: warning: the master's own buffer is {frames} frames longer than \
                 the delay the other outputs are held at: they play ahead of it"
            ),
        }
    }
}

/// What the master's delay line holds for the alignment `target` at the
/// master's `period`, with a line of `capacity` frames: the target less
/// the master's own buffer. With how the master misses the time, if the
/// line cannot hold that.
pub fn master_delay(target: u32, period: u32, capacity: u32) -> (u32, Option<Misaligned>) {
    let own = PERIODS * period;
    let Some(wanted) = target.checked_sub(own) else {
        let frames = own - target;
        return (0, Some(Misaligned::Behind { frames }));
    };
    if wanted > capacity {
        return (capacity, Some(Misaligned::Ahead { capacity, wanted }));
    }
    (wanted, None)
}

/// The line logged for an event of further output `number`, if any.
/// Starves, overflows and the clamp are logged through an `OutputLog`.
pub fn event_line(number: usize, event: FurtherEvent) -> Option<String> {
    let line = match event {
        FurtherEvent::Gone => format!(
            "aas: output {number}: its card is gone, it plays to nothing and is looked for every second"
        ),
        FurtherEvent::Back => format!("aas: output {number}: its card is back"),
        FurtherEvent::Locked { ppm: None } => format!("aas: output {number} locked"),
        FurtherEvent::Locked { ppm: Some(ppm) } => {
            format!("aas: output {number} locked, its card runs {ppm:+.1} ppm from the master")
        }
        FurtherEvent::Unlocked => format!("aas: output {number} lost its lock"),
        FurtherEvent::Granted {
            rate,
            asked_period,
            period,
            asked_buffer,
            buffer,
        } => {
            let ms = |frames: u32| f64::from(frames) * 1000.0 / f64::from(rate.max(1));
            format!(
                "aas: warning: output {number}: its card runs a period of {period} frames \
                 ({:.1} ms) and a buffer of {buffer} frames ({:.1} ms), asked for {asked_period} \
                 ({:.1} ms) and {asked_buffer} ({:.1} ms); its delay is planned from what was \
                 asked and it may starve, set this output's latency_ms to a period the card runs",
                ms(period),
                ms(buffer),
                ms(asked_period),
                ms(asked_buffer),
            )
        }
        FurtherEvent::Clamped { .. }
        | FurtherEvent::Starved { .. }
        | FurtherEvent::Overflowed { .. } => return None,
    };
    Some(line)
}

/// Starves and overflows logged one by one at first, each.
const FEED_LINES: u32 = 10;
/// After that, at most one line in this many ms, and as often for the
/// clamp.
const LINE_MS: u64 = 60_000;

/// The lines of a further output that can come again and again: its
/// starves and overflows, the first [`FEED_LINES`] one by one, then one a
/// minute with how many went unlogged; and the clamp, once a minute at
/// most. The counters at the stop have them all.
#[derive(Default)]
struct OutputLog {
    lines: u32,
    /// When the feed's last line was, ms after the writer started.
    last_ms: u64,
    /// Starves and overflows since.
    unlogged: u64,
    /// When the clamp's last line was.
    clamp_ms: Option<u64>,
    /// Clamps since.
    clamps: u64,
}

impl OutputLog {
    /// The line for `event` of output `number`, if it is a starve, an
    /// overflow or the clamp and its turn has come.
    fn line(&mut self, number: usize, event: FurtherEvent) -> Option<String> {
        let (what, at_ms, fill, limit, count) = match event {
            FurtherEvent::Starved { at_ms, fill, limit } => ("starved", at_ms, fill, limit, 1),
            FurtherEvent::Overflowed {
                at_ms,
                fill,
                limit,
                count,
            } => ("overflowed", at_ms, fill, limit, count.max(1)),
            FurtherEvent::Clamped { at_ms } => return self.clamped(number, at_ms),
            _ => return None,
        };
        if self.lines >= FEED_LINES && at_ms < self.last_ms + LINE_MS {
            self.unlogged += u64::from(count);
            return None;
        }
        let when = if count > 1 {
            format!("{count} times by")
        } else {
            String::new()
        };
        let mut line = format!(
            "aas: output {number} {what} {when}{}{}.{:03} s after the start, \
             its feed held {fill} of {limit} frames",
            if when.is_empty() { "" } else { " " },
            at_ms / 1000,
            at_ms % 1000
        );
        if self.lines + 1 == FEED_LINES {
            line.push_str("; from now on one line a minute");
        } else if self.lines >= FEED_LINES {
            line.push_str(&format!(", {} more since the last line", self.unlogged));
        }
        self.lines += 1;
        self.last_ms = at_ms;
        self.unlogged = 0;
        Some(line)
    }

    fn clamped(&mut self, number: usize, at_ms: u64) -> Option<String> {
        if self.clamp_ms.is_some_and(|last| at_ms < last + LINE_MS) {
            self.clamps += 1;
            return None;
        }
        let mut line = format!(
            "aas: output {number}: its card runs more than {:.0} ppm from the master, \
             it plays on with gaps",
            MAX_DRIFT * 1e6
        );
        if self.clamp_ms.is_some() {
            line.push_str(&format!(", {} more times since the last line", self.clamps));
        }
        self.clamp_ms = Some(at_ms);
        self.clamps = 0;
        Some(line)
    }
}

/// The control thread's side of a further output's writer: a
/// [`FurtherHandle`], or a stand-in in tests.
pub trait FurtherWriter {
    /// As [`FurtherHandle::reconfigure_holding`].
    fn reconfigure(
        &self,
        master_rate: u32,
        card_rate: u32,
        period: u32,
        resampler: Box<Adaptive>,
        target: f64,
        feed_frames: usize,
    );
    fn set_open(&self, open: bool);
    fn event(&self) -> Option<FurtherEvent>;
}

impl FurtherWriter for FurtherHandle {
    fn reconfigure(
        &self,
        master_rate: u32,
        card_rate: u32,
        period: u32,
        resampler: Box<Adaptive>,
        target: f64,
        feed_frames: usize,
    ) {
        self.reconfigure_holding(
            master_rate,
            card_rate,
            period,
            resampler,
            target,
            feed_frames,
        );
    }

    fn set_open(&self, open: bool) {
        FurtherHandle::set_open(self, open);
    }

    fn event(&self) -> Option<FurtherEvent> {
        FurtherHandle::event(self)
    }
}

/// A further output, as the server hands it to the control thread before
/// the start.
pub struct FurtherSetup {
    /// 1 to 3; the master is 0.
    pub index: usize,
    pub config: OutputConfig,
    /// The output's matrix, checked against its chains.
    pub matrix: Matrix,
    /// The rates its card takes, empty for any.
    pub rates: Vec<u32>,
    pub writer: Box<dyn FurtherWriter>,
    /// The feed the audio thread fills, which goes to it at the start.
    pub feed: Feed,
    /// The frames the feed was made with.
    pub feed_frames: usize,
}

/// A configuration for a writer, made when the master opens and handed
/// over once it is open.
struct Pending {
    card_rate: u32,
    period: u32,
    resampler: Box<Adaptive>,
    target: u32,
    feed: usize,
}

/// A further output as the control thread keeps it.
struct FurtherOutput {
    index: usize,
    channels: usize,
    /// Its own latency target, or the master's.
    latency_ms: f32,
    /// Its card's part of the path as its writer was last given it.
    current: Option<(u32, u32, f64)>,
    idle: Idle,
    volume_db: f32,
    plugins: Vec<PluginConfig>,
    rates: Vec<u32>,
    writer: Box<dyn FurtherWriter>,
    capacity: usize,
    /// The matrix and the feed, until the start sends them.
    unsent: Option<(Matrix, Feed)>,
    /// The rate the mix and output chain on the audio thread are built for.
    built: [Option<u32>; 2],
    /// Its chains do not build at the master's rate: it plays at gain 0.
    silenced: bool,
    pending: Option<Pending>,
    /// The writer has a configuration.
    configured: bool,
    /// The writer has been told to play.
    open: bool,
    /// The master's card opened under the parked cycle: a starve until
    /// then gets a line of its own, outside the log's count.
    resumed_until: Option<Instant>,
    log: OutputLog,
}

impl FurtherOutput {
    /// Its number in the log: the master is output 1.
    fn number(&self) -> usize {
        self.index + 1
    }
}

/// Whether a further output's idle rule closes it now, with no source
/// active since `quiet_since`.
fn output_idle(idle: Idle, quiet_since: Option<Instant>, now: Instant) -> bool {
    match idle {
        Idle::Off { delay_s } => {
            let delay = Duration::from_secs(u64::from(delay_s));
            quiet_since.is_some_and(|since| now >= since + delay)
        }
        Idle::Zeros => false,
    }
}

/// The mix of a further output in the engine's processing format, at
/// `gain`. Further outputs are never bit-perfect.
fn further_mix(format: Format, matrix: Matrix, channels: usize, gain: f32) -> Box<dyn Any + Send> {
    fn made<P: Processing>(matrix: Matrix, channels: usize, gain: f32) -> Box<dyn Any + Send> {
        let mut mix = OutputMix::<P>::new(matrix, channels, false, MAX_PERIOD as usize);
        mix.set_gain(gain, None);
        Box::new(mix)
    }
    match format {
        Format::I32 => made::<I32>(matrix, channels, gain),
        Format::F32 => made::<F32>(matrix, channels, gain),
        Format::F64 => made::<F64>(matrix, channels, gain),
    }
}

/// Builds the mix and output chains of `output` at `rate` that the audio
/// thread does not have for it, into `batch`. Fails with the first error;
/// the chains built before it go anyway.
fn further_chains(
    output: &mut FurtherOutput,
    rate: u32,
    format: Format,
    libraries: &mut Libraries,
    batch: &mut Vec<Command>,
) -> Result<(), BuildError> {
    let w = widths(&output.plugins, output.channels, libraries).map_err(|(_, e)| e)?;
    let places = [(PluginPlace::Mix, w.mix), (PluginPlace::Output, w.output)];
    for (slot, (place, channels)) in places.into_iter().enumerate() {
        if output.built[slot] == Some(rate) || !output.plugins.iter().any(|p| p.place == place) {
            continue;
        }
        let chain = build_chain(&output.plugins, place, rate, format, channels, libraries)?;
        output.built[slot] = Some(rate);
        let target = Target {
            output: output.index,
            place: engine_place(place),
        };
        batch.push(Command::SetChain(target, Box::new(chain)));
    }
    Ok(())
}

/// A read-only view of a ring's header, for the control thread. It maps
/// the header alone, which is one page. A second mapping of the whole ring
/// would lock the ring's memory once more for each client.
struct HeaderView {
    base: NonNull<Header>,
}

// SAFETY: the mapping is shared memory that is only read, through atomics.
unsafe impl Send for HeaderView {}

impl HeaderView {
    const LEN: usize = size_of::<Header>();

    /// Maps the header of a ring this process made, so its size is known
    /// to hold one.
    fn map(fd: BorrowedFd) -> io::Result<HeaderView> {
        // SAFETY: a fresh shared mapping, the result is checked.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                Self::LEN,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = NonNull::new(p.cast()).ok_or_else(|| io::Error::other("mmap gave null"))?;
        Ok(HeaderView { base })
    }

    fn header(&self) -> &Header {
        // SAFETY: the mapping holds a header, page aligned, until drop.
        unsafe { self.base.as_ref() }
    }
}

// The view never takes more than the first page of a ring.
const _: () = assert!(HeaderView::LEN <= 4096);

impl Drop for HeaderView {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly what `map` mapped.
        unsafe { libc::munmap(self.base.as_ptr().cast(), Self::LEN) };
    }
}

/// What the server does with a request.
pub struct Answer {
    /// Nothing for a drain, whose reply comes later.
    pub reply: Option<Reply>,
    /// Descriptors sent with the reply.
    pub fds: Vec<OwnedFd>,
    /// The connection is closed after the reply.
    pub close: bool,
}

impl Answer {
    fn reply(reply: Reply) -> Answer {
        Answer {
            reply: Some(reply),
            fds: Vec::new(),
            close: false,
        }
    }

    fn error(code: i32) -> Answer {
        Answer::reply(Reply::Error { code: code as u32 })
    }

    fn fatal(code: i32) -> Answer {
        Answer {
            close: true,
            ..Answer::error(code)
        }
    }
}

/// A configured stream as the control thread keeps it.
struct Stream {
    id: ClientId,
    rate: u32,
    capacity: usize,
    /// Kept until the ring is sealed, on the first start.
    memfd: Option<OwnedFd>,
    running: bool,
    /// The output rate and period of the resampler the source has.
    resampler: Option<(u32, u32)>,
    /// The control thread's own view of the ring, to see a drain progress.
    view: HeaderView,
    /// A drain waits for its reply. If the audio thread has taken nothing
    /// from the ring by then, it is answered anyway.
    drain: Option<Instant>,
    /// The ring's read position when the drain deadline was last set.
    drain_seen: u64,
    /// The audio thread reported the stream drained since it last started,
    /// which can come before the request: the client sets the flag first.
    drained: bool,
}

/// A capture stream as the control thread keeps it. It is no client of the
/// rate rules, never counts as active and has no resampler.
struct Recorder {
    id: ClientId,
    /// The output's rate when it was configured, which it records at.
    rate: u32,
    /// Kept until the ring is sealed, on the first start.
    memfd: Option<OwnedFd>,
    running: bool,
    /// The output's rate has changed: the audio thread has been told to
    /// remove the tap, and the client has to configure again. The ring is
    /// marked ended once the tap is back, when nothing writes it any more.
    ended: bool,
}

struct Conn {
    name: String,
    hello: bool,
    /// A connection has one stream at a time, this or a recorder.
    stream: Option<Stream>,
    recorder: Option<Recorder>,
    /// Told of every change to the output volume.
    subscribed: bool,
}

/// The analog input as the control thread keeps it.
struct AnalogInput {
    open: OpenCapture,
    /// The input has signal.
    active: bool,
    /// The input is a client of the rate rules: it has signal and no DoP
    /// stream owns the output.
    registered: bool,
    /// `SetCapture` commands the audio thread has not answered yet. A
    /// failure reported meanwhile belongs to a capture they replace.
    unanswered: u32,
    /// The audio thread has a capture for the output as it is, which has
    /// not failed.
    reading: bool,
    /// When to try again to open the capture.
    retry_at: Option<Instant>,
    /// A failure has been logged, and the recovery is not yet.
    failing: bool,
}

pub struct Control {
    latency_ms: f32,
    quality: Quality,
    format: Format,
    idle: Idle,
    supported: Vec<u32>,
    policy: RatePolicy,
    epoch: Instant,
    commands: Producer<Command>,
    events: Consumer<Event>,
    /// Events taken out of the queue while waiting for room in the other.
    stash: Vec<Event>,
    audio: Thread,
    conns: BTreeMap<ConnId, Conn>,
    next_id: ClientId,
    table_capacity: usize,
    table_len: usize,
    /// The same for the audio thread's table of capture streams.
    taps_capacity: usize,
    taps_len: usize,
    /// The rate and period the output was last told to open at. None while
    /// it is closed.
    output: Option<(u32, u32)>,
    /// The rate and period the cycle runs at on the timer while the
    /// master's card is closed by its idle rule and a further output's own
    /// rule keeps that output open. None otherwise.
    parked: Option<(u32, u32)>,
    /// How many times the audio thread has been told to open the output
    /// and has not yet answered. Nothing plays while one is under way. A
    /// count, since an answer to an earlier open says nothing of a later
    /// one. The audio thread answers every one, also one it dropped.
    reopening: u32,
    /// A rate change that waits, so that a stop and a start close together
    /// do not change the rate twice.
    switch_at: Option<Instant>,
    /// Since when no client has been running.
    idle_since: Option<Instant>,
    /// Clients the audio thread has seen play DoP, each with the number
    /// the audio thread gave its claim, oldest claim first.
    dop_claims: Vec<(u64, ClientId)>,
    dop_owner: Option<ClientId>,
    /// Replies that were not ready when their request came.
    outbox: Vec<(ConnId, Reply)>,
    volume: OutputVolume,
    /// Writes the volume to the state file, to keep it across restarts.
    /// None keeps it nowhere.
    saver: Option<Saver>,
    /// When the state file is due to be written.
    save_at: Option<Instant>,
    /// Volume notifications for subscribers, not yet sent.
    notices: Vec<(ConnId, Reply)>,
    /// The plugins of the config, to build chains from at each rate.
    plugins: Vec<PluginConfig>,
    /// The card's channels, from the config, which the chains end at.
    channels: usize,
    libraries: Libraries,
    /// Chains built ahead, sent at the first open if it is at their rate.
    ready: Option<Chains>,
    /// The rate each place's chain on the audio thread was built for, by
    /// `slot`. None before one is sent.
    built: [Option<u32>; 3],
    /// Chains built for the rate the rules want, sent when the output runs
    /// at it.
    spare: Option<Spare>,
    /// Rates at which a chain could not be built. The output never runs at
    /// one, and the rules take the nearest other rate.
    refused: Vec<u32>,
    /// Of those, the rates the analog chain refused. A DoP stream needs
    /// only that chain, so it may run at the others.
    analog_refused: Vec<u32>,
    /// No rate works for every chain, and that has been logged.
    stuck: bool,
    /// A chain failed at this rate, DoP or not, in this rate decision. It
    /// is tried again at the next one, not before.
    failed: Option<(u32, bool)>,
    /// No other rate built either, in this rate decision, while the output
    /// was closed. Not tried again before the next one.
    no_fallback: bool,
    /// The gain of the analog input, from the config.
    analog_gain: f32,
    /// None unless the config enables the input and the card has an ADC.
    analog: Option<AnalogInput>,
    /// The further outputs, in the order of the config.
    further: Vec<FurtherOutput>,
    /// Since when no player runs and the analog input has no signal.
    quiet_since: Option<Instant>,
    /// The master's delay line as last sent.
    delay: Option<u32>,
    /// The delay every output is held at, as last planned.
    target: Option<u32>,
    /// The longest delay line the engine has, from the outputs' latencies.
    delay_capacity: u32,
    /// The delay line was found too short, and that has been logged.
    delay_warned: bool,
    /// The master's own buffer was found longer than the target, and that
    /// has been logged.
    behind_warned: bool,
}

impl Control {
    /// `supported` are the rates the sink takes, empty for any. `audio` is
    /// unparked after every command.
    pub fn new(
        config: &Config,
        latency_ms: f32,
        supported: Vec<u32>,
        commands: Producer<Command>,
        events: Consumer<Event>,
        audio: Thread,
    ) -> Control {
        Control {
            latency_ms,
            quality: config.resampling,
            format: config.format,
            idle: config.idle,
            policy: RatePolicy::new(config.default_rate, supported.clone()),
            supported,
            epoch: Instant::now(),
            commands,
            events,
            stash: Vec::new(),
            audio,
            conns: BTreeMap::new(),
            next_id: 1,
            table_capacity: INITIAL_SOURCES,
            table_len: 0,
            taps_capacity: INITIAL_TAPS,
            taps_len: 0,
            output: None,
            parked: None,
            reopening: 0,
            switch_at: None,
            idle_since: None,
            dop_claims: Vec::new(),
            dop_owner: None,
            outbox: Vec::new(),
            volume: OutputVolume::new(None, config.bit_perfect),
            saver: None,
            save_at: None,
            notices: Vec::new(),
            plugins: config.plugins.clone(),
            channels: config.channels,
            libraries: Libraries::default(),
            ready: None,
            built: [None; 3],
            spare: None,
            refused: Vec::new(),
            analog_refused: Vec::new(),
            stuck: false,
            failed: None,
            no_fallback: false,
            analog_gain: db_to_gain(config.analog.volume_db),
            analog: None,
            further: Vec::new(),
            quiet_since: None,
            delay: None,
            target: None,
            delay_capacity: 0,
            delay_warned: false,
            behind_warned: false,
        }
    }

    /// Adds a further output. Call before `start`, once for each, in the
    /// order of the config.
    pub fn add_output(&mut self, setup: FurtherSetup) {
        let config = setup.config;
        let latency_ms = config.latency_ms.unwrap_or(self.latency_ms);
        self.further.push(FurtherOutput {
            index: setup.index,
            channels: config.channels,
            latency_ms,
            current: None,
            idle: config.idle,
            volume_db: config.volume_db,
            plugins: config.plugins,
            rates: setup.rates,
            writer: setup.writer,
            capacity: setup.feed_frames,
            unsent: Some((setup.matrix, setup.feed)),
            built: [None; 2],
            silenced: false,
            pending: None,
            configured: false,
            open: false,
            resumed_until: None,
            log: OutputLog::default(),
        });
        let latencies: Vec<f32> = self.further.iter().map(|o| o.latency_ms).collect();
        self.delay_capacity = delay_capacity(self.latency_ms, &latencies, self.quality);
    }

    /// The gain of a further output: the master's volume in software and
    /// its offset, or 0 while its chains do not build.
    fn further_gain(&self, output: &FurtherOutput) -> f32 {
        if output.silenced {
            0.0
        } else {
            self.volume.further_gain(output.volume_db)
        }
    }

    /// Takes over the libraries loaded at startup, and the chains built
    /// then, if any. Call before `start`. Without them the chains are built
    /// at the first open.
    pub fn set_plugins(&mut self, libraries: Libraries, chains: Option<Chains>) {
        self.libraries = libraries;
        self.ready = chains;
    }

    /// Runs the analog input, which `open` opens. Call before `start`, and
    /// only when the config enables the input and the card has an ADC.
    /// The output then stays open when idle, since the card paces the
    /// capture and the detector has to keep measuring.
    pub fn set_analog(&mut self, open: OpenCapture) {
        if let Idle::Off { .. } = self.idle {
            eprintln!("aas: the analog input keeps the output open, idle = \"off\" does not apply");
        }
        self.analog = Some(AnalogInput {
            open,
            active: false,
            registered: false,
            unanswered: 0,
            reading: false,
            retry_at: None,
            failing: false,
        });
    }

    /// Takes over the output volume, at the level the state file keeps.
    /// In hardware mode this sets the card's mixer. Call before `start`.
    pub fn restore_volume(&mut self, mut volume: OutputVolume, state: PathBuf) {
        volume.set(State::load(&state).output_percent);
        self.volume = volume;
        self.saver = Some(Saver::new(state));
        eprintln!(
            "aas: output volume {}%, in {}",
            self.volume.percent(),
            self.volume_mode()
        );
    }

    /// The card has gone or come back, and has this volume control now, or
    /// none. A card that is back may be a new instance, its mixer at its own
    /// default. The old control is closed, and the current volume is set on
    /// the new one, or carried in software without one.
    pub fn card_changed(&mut self, hardware: Option<Box<dyn HardwareVolume>>) {
        self.volume.replace_hardware(hardware);
        eprintln!(
            "aas: output volume {}%, in {}",
            self.volume.percent(),
            self.volume_mode()
        );
        self.send(vec![Command::SetGain(0, self.volume.gain())]);
    }

    fn volume_mode(&self) -> &'static str {
        if self.volume.is_hardware() {
            "hardware"
        } else {
            "software"
        }
    }

    /// Opens the output at the default rate. Until a client plays it sends
    /// zeros, and the idle rule applies from now, unless the analog input
    /// is on.
    pub fn start(&mut self, now: Instant) {
        let mut batch = vec![Command::SetGain(0, self.volume.gain())];
        for i in 0..self.further.len() {
            let gain = self.further_gain(&self.further[i]);
            let output = &mut self.further[i];
            if let Some((matrix, feed)) = output.unsent.take() {
                let mix = further_mix(self.format, matrix, output.channels, gain);
                batch.push(Command::AddOutput(output.index, mix, feed));
            }
        }
        if self.analog.is_some() {
            batch.push(Command::SetAnalogGain(self.analog_gain));
        }
        self.open_output(now, &mut batch);
        self.idle_since = Some(now);
        self.send(batch);
    }

    /// Stops the audio thread. It returns once it has seen this.
    /// Writes the state file if a change is still waiting for it, and
    /// returns only once the file is on disk.
    pub fn shutdown(&mut self) {
        self.send(vec![Command::Shutdown]);
        if self.save_at.is_some() {
            self.save();
        }
        if let Some(saver) = &mut self.saver {
            saver.finish();
        }
    }

    pub fn connected(&mut self, conn: ConnId, name: String) {
        eprintln!("aas: {name} connected");
        let state = Conn {
            name,
            hello: false,
            stream: None,
            recorder: None,
            subscribed: false,
        };
        self.conns.insert(conn, state);
    }

    /// A closed socket removes the client, whatever state it was in.
    pub fn disconnected(&mut self, conn: ConnId, now: Instant) {
        let Some(state) = self.conns.remove(&conn) else {
            return;
        };
        eprintln!("aas: {} disconnected", state.name);
        if let Some(stream) = state.stream {
            let mut batch = Vec::new();
            self.remove_stream(stream, now, &mut batch);
            self.decide(now, false, &mut batch);
            self.send(batch);
        }
        if let Some(recorder) = state.recorder {
            let mut batch = Vec::new();
            self.remove_recorder(recorder, &mut batch);
            self.send(batch);
        }
    }

    /// Replies that have become ready, such as finished drains.
    pub fn take_replies(&mut self) -> Vec<(ConnId, Reply)> {
        std::mem::take(&mut self.outbox)
    }

    /// The ready replies for one connection, in order. They answer requests
    /// it sent earlier, so they go out before the reply to a newer one.
    pub fn take_replies_for(&mut self, conn: ConnId) -> Vec<Reply> {
        let mut mine = Vec::new();
        self.outbox.retain(|&(c, reply)| {
            if c == conn {
                mine.push(reply);
            }
            c != conn
        });
        mine
    }

    /// A request that changes the stream ends a drain still waiting. The
    /// drain is answered with `ECANCELED`, so every request still gets one
    /// reply, in order. A non-blocking client sends one while it drains.
    fn cancel_drain(&mut self, conn: ConnId) {
        let Some(stream) = self.stream_mut(conn) else {
            return;
        };
        if stream.drain.take().is_some() {
            let code = libc::ECANCELED as u32;
            self.outbox.push((conn, Reply::Error { code }));
        }
    }

    /// Volume notifications for subscribers, in order. The server sends
    /// only the latest one each subscriber has waiting.
    pub fn take_notices(&mut self) -> Vec<(ConnId, Reply)> {
        std::mem::take(&mut self.notices)
    }

    /// Handles one message. `None` is a message that does not decode.
    pub fn request(&mut self, conn: ConnId, request: Option<Request>, now: Instant) -> Answer {
        let Some(state) = self.conns.get_mut(&conn) else {
            return Answer::fatal(libc::ENOTCONN);
        };
        let Some(request) = request else {
            return Answer::error(libc::EINVAL);
        };
        if !state.hello {
            return match request {
                Request::Hello { version } if version == proto::VERSION => {
                    state.hello = true;
                    Answer::reply(Reply::Hello {
                        version: proto::VERSION,
                    })
                }
                Request::Hello { version } => {
                    eprintln!("aas: {} speaks protocol {version}", state.name);
                    Answer::fatal(libc::EPROTONOSUPPORT)
                }
                _ => Answer::fatal(libc::EPROTO),
            };
        }
        if matches!(
            request,
            Request::Configure { .. }
                | Request::Start
                | Request::Stop
                | Request::Drain
                | Request::Reset
        ) {
            self.cancel_drain(conn);
        }
        let recording = self.recorder_mut(conn).is_some();
        match request {
            Request::Hello { .. } => Answer::reply(Reply::Hello {
                version: proto::VERSION,
            }),
            Request::Configure {
                buffer_frames,
                capture: true,
                ..
            } => self.configure_recorder(conn, buffer_frames, now),
            Request::Configure {
                rate,
                format,
                channels,
                buffer_frames,
                capture: false,
            } => self.configure(conn, rate, format, channels, buffer_frames, now),
            Request::Start if recording => self.start_recorder(conn),
            Request::Stop if recording => self.stop_recorder(conn),
            Request::Reset if recording => self.reset_recorder(conn),
            // A capture stream never ends by itself.
            Request::Drain if recording => Answer::error(libc::EINVAL),
            Request::Start => self.start_stream(conn, now),
            Request::Stop => self.stop_stream(conn, now),
            Request::Drain => self.drain(conn, now),
            Request::Reset => self.reset(conn, now),
            Request::GetVolume => self.volume_reply(),
            Request::SetVolume { percent } if percent > 100 => Answer::error(libc::EINVAL),
            Request::SetVolume { percent } => {
                self.set_volume(percent, now);
                Answer::reply(Reply::Ok)
            }
            Request::SubscribeVolume => {
                if let Some(state) = self.conns.get_mut(&conn) {
                    state.subscribed = true;
                }
                self.volume_reply()
            }
        }
    }

    fn volume_reply(&self) -> Answer {
        let percent = self.volume.percent();
        Answer::reply(Reply::Volume { percent })
    }

    /// Sets the output volume and tells every subscriber. The state file is
    /// written once the volume has been left alone for a while.
    ///
    /// The card's mixer is written even when the percent is the same, so
    /// that asking again puts back a level another tool has changed. A
    /// change made outside aas is not read back.
    fn set_volume(&mut self, percent: u8, now: Instant) {
        let changed = percent != self.volume.percent();
        self.volume.set(percent);
        if !changed {
            return;
        }
        let mut batch = vec![Command::SetGain(0, self.volume.gain())];
        for output in &self.further {
            batch.push(Command::SetGain(output.index, self.further_gain(output)));
        }
        self.send(batch);
        let subscribers = self.conns.iter().filter(|(_, c)| c.subscribed);
        let notice = Reply::VolumeChanged { percent };
        self.notices
            .extend(subscribers.map(|(&conn, _)| (conn, notice)));
        if self.saver.is_some() {
            self.save_at = Some(now + SAVE_DELAY);
        }
    }

    /// Has the state file written. The write and its sync to disk run on
    /// the saver's thread, so a slow SD card holds up no request.
    fn save(&mut self) {
        self.save_at = None;
        if let Some(saver) = &self.saver {
            saver.save(State {
                output_percent: self.volume.percent(),
            });
        }
    }

    fn configure(
        &mut self,
        conn: ConnId,
        rate: u32,
        format: ClientFormat,
        channels: u8,
        buffer_frames: u32,
        now: Instant,
    ) -> Answer {
        // The client converts its format; the ring is always S32 stereo.
        let _ = format;
        if !(MIN_RATE..=MAX_RATE).contains(&rate)
            || channels == 0
            || buffer_frames < min_buffer_frames(rate, self.latency_ms)
        {
            return Answer::error(libc::EINVAL);
        }
        let mut batch = Vec::new();
        if let Some(old) = self.conns.get_mut(&conn).and_then(|c| c.stream.take()) {
            self.remove_stream(old, now, &mut batch);
        }
        if let Some(old) = self.conns.get_mut(&conn).and_then(|c| c.recorder.take()) {
            self.remove_recorder(old, &mut batch);
        }
        let capacity = buffer_frames.max(min_ring_frames(rate, self.latency_ms));
        let made = Ring::create(capacity).and_then(|(ring, memfd)| {
            let view = HeaderView::map(memfd.as_fd())?;
            let wake = eventfd()?;
            let theirs = wake.try_clone()?;
            let memfd_theirs = memfd.try_clone()?;
            Ok((ring, view, memfd, memfd_theirs, wake, theirs))
        });
        let (ring, view, memfd, memfd_theirs, wake, theirs) = match made {
            Ok(made) => made,
            Err(e) => {
                eprintln!("aas: no ring for a client: {e}");
                self.decide(now, false, &mut batch);
                self.send(batch);
                return Answer::error(e.raw_os_error().unwrap_or(libc::ENOMEM));
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        if self.table_len == self.table_capacity {
            self.table_capacity *= 2;
            batch.push(Command::Grow(Vec::with_capacity(self.table_capacity)));
        }
        batch.push(Command::Add(Box::new(Source::new(id, ring, wake, rate))));
        self.table_len += 1;
        let stream = Stream {
            id,
            rate,
            capacity: capacity as usize,
            memfd: Some(memfd),
            running: false,
            resampler: None,
            view,
            drain: None,
            drain_seen: 0,
            drained: false,
        };
        if let Some(state) = self.conns.get_mut(&conn) {
            state.stream = Some(stream);
        }
        self.decide(now, false, &mut batch);
        self.send(batch);
        Answer {
            reply: Some(Reply::Configured { rate: 0 }),
            fds: vec![memfd_theirs, theirs],
            close: false,
        }
    }

    /// Starts the stream. A stream that cannot be sealed is not trusted, and
    /// the client is dropped.
    fn start_stream(&mut self, conn: ConnId, now: Instant) -> Answer {
        self.new_rate_decision();
        let urgent = self.running_count() == 0;
        let Some(stream) = self.stream_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        if let Some(memfd) = stream.memfd.take() {
            if let Err(e) = ring::seal_writes(memfd.as_fd()) {
                eprintln!("aas: could not seal a client's ring: {e}");
                return Answer::fatal(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
        stream.drain = None;
        if stream.running {
            return Answer::reply(Reply::Ok);
        }
        stream.running = true;
        stream.drained = false;
        let (id, rate) = (stream.id, stream.rate);
        let t = now - self.epoch;
        self.policy.client_started(id, rate, t);
        let mut batch = Vec::new();
        self.decide(now, urgent, &mut batch);
        batch.push(Command::SetRunning(id, true));
        self.send(batch);
        Answer::reply(Reply::Ok)
    }

    fn stop_stream(&mut self, conn: ConnId, now: Instant) -> Answer {
        let Some(stream) = self.stream_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        let id = stream.id;
        stream.drain = None;
        let mut batch = Vec::new();
        self.stop(id, now, &mut batch);
        self.send(batch);
        Answer::reply(Reply::Ok)
    }

    /// Stops the stream and has the audio thread empty its ring. The reply
    /// comes once it has, so nothing written after the reply is lost.
    fn reset(&mut self, conn: ConnId, now: Instant) -> Answer {
        let Some(stream) = self.stream_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        let id = stream.id;
        stream.drain = None;
        stream.drained = false;
        let mut batch = Vec::new();
        self.stop(id, now, &mut batch);
        batch.push(Command::Flush(id));
        self.send(batch);
        Answer {
            reply: None,
            fds: Vec::new(),
            close: false,
        }
    }

    /// The audio thread has emptied a stream's ring, or stopped a capture
    /// stream: its reset is answered.
    fn flushed(&mut self, id: ClientId) {
        let conn = self.conns.iter().find(|(_, c)| {
            c.stream.as_ref().is_some_and(|s| s.id == id)
                || c.recorder.as_ref().is_some_and(|r| r.id == id)
        });
        if let Some((&conn, _)) = conn {
            self.outbox.push((conn, Reply::Ok));
        }
    }

    /// Stops a running stream and settles what follows from that.
    fn stop(&mut self, id: ClientId, now: Instant, batch: &mut Vec<Command>) {
        self.new_rate_decision();
        let Some(stream) = self.stream_by_id(id) else {
            return;
        };
        if !std::mem::take(&mut stream.running) {
            return;
        }
        self.policy.client_stopped(id, now - self.epoch);
        batch.push(Command::SetRunning(id, false));
        self.dop_claims.retain(|&(_, c)| c != id);
        self.update_dop(now, batch);
        self.decide(now, false, batch);
    }

    /// Starts the stream if needed. The reply comes once the audio thread
    /// has played everything.
    fn drain(&mut self, conn: ConnId, now: Instant) -> Answer {
        let Some(stream) = self.stream_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        if !stream.running {
            let answer = self.start_stream(conn, now);
            if answer.reply != Some(Reply::Ok) {
                return answer;
            }
        }
        let Some(stream) = self.stream_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        if std::mem::take(&mut stream.drained) {
            let id = stream.id;
            let mut batch = Vec::new();
            self.stop(id, now, &mut batch);
            self.send(batch);
            return Answer::reply(Reply::Ok);
        }
        let held = Duration::from_secs_f64(stream.capacity as f64 / f64::from(stream.rate));
        stream.drain = Some(now + held + DRAIN_SPARE);
        stream.drain_seen = stream.view.header().read_pos.load(Ordering::Acquire);
        Answer {
            reply: None,
            fds: Vec::new(),
            close: false,
        }
    }

    /// Ends a drain: the stream stops and the client gets its reply.
    fn drained(&mut self, id: ClientId, now: Instant, batch: &mut Vec<Command>) {
        let Some((&conn, _)) = self
            .conns
            .iter()
            .find(|(_, c)| c.stream.as_ref().is_some_and(|s| s.id == id))
        else {
            return;
        };
        let Some(stream) = self.stream_mut(conn) else {
            return;
        };
        if stream.drain.take().is_none() {
            stream.drained = true;
            return;
        }
        stream.drained = false;
        self.stop(id, now, batch);
        self.outbox.push((conn, Reply::Ok));
    }

    fn remove_stream(&mut self, stream: Stream, now: Instant, batch: &mut Vec<Command>) {
        self.new_rate_decision();
        if stream.running {
            self.policy.client_stopped(stream.id, now - self.epoch);
        }
        batch.push(Command::Remove(stream.id));
        self.table_len -= 1;
        self.dop_claims.retain(|&(_, c)| c != stream.id);
        self.update_dop(now, batch);
    }

    /// Sets up a capture stream: S32 stereo at the rate the output runs at,
    /// or opens at next if it is closed. The ring holds what is asked for,
    /// and at least a few periods. Sends the tap to the audio thread, which
    /// writes it once it is started.
    fn configure_recorder(&mut self, conn: ConnId, buffer_frames: u32, now: Instant) -> Answer {
        let rate = match self.output {
            Some((rate, _)) => rate,
            None => self.policy.current(),
        };
        let least = RECORDER_PERIODS * period_frames(rate, self.latency_ms);
        let capacity = buffer_frames.max(least);
        let mut batch = Vec::new();
        if let Some(old) = self.conns.get_mut(&conn).and_then(|c| c.stream.take()) {
            self.remove_stream(old, now, &mut batch);
            self.decide(now, false, &mut batch);
        }
        if let Some(old) = self.conns.get_mut(&conn).and_then(|c| c.recorder.take()) {
            self.remove_recorder(old, &mut batch);
        }
        let made = Ring::create(capacity).and_then(|(ring, memfd)| {
            let wake = eventfd()?;
            let theirs = wake.try_clone()?;
            let memfd_theirs = memfd.try_clone()?;
            Ok((ring, memfd, memfd_theirs, wake, theirs))
        });
        let (ring, memfd, memfd_theirs, wake, theirs) = match made {
            Ok(made) => made,
            Err(e) => {
                eprintln!("aas: no ring for a recorder: {e}");
                self.send(batch);
                return Answer::error(e.raw_os_error().unwrap_or(libc::ENOMEM));
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        if self.taps_len == self.taps_capacity {
            self.taps_capacity *= 2;
            batch.push(Command::GrowTaps(Vec::with_capacity(self.taps_capacity)));
        }
        batch.push(Command::AddTap(Box::new(Tap::new(id, ring, wake))));
        self.taps_len += 1;
        let recorder = Recorder {
            id,
            rate,
            memfd: Some(memfd),
            running: false,
            ended: false,
        };
        if let Some(state) = self.conns.get_mut(&conn) {
            state.recorder = Some(recorder);
        }
        self.send(batch);
        Answer {
            reply: Some(Reply::Configured { rate }),
            fds: vec![memfd_theirs, theirs],
            close: false,
        }
    }

    /// Starts a capture stream, unless it has ended. As for a player, a
    /// ring that cannot be sealed is not trusted, and the client is dropped.
    fn start_recorder(&mut self, conn: ConnId) -> Answer {
        let Some(recorder) = self.recorder_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        if recorder.ended {
            return Answer::error(libc::ENODEV);
        }
        if let Some(memfd) = recorder.memfd.take() {
            if let Err(e) = ring::seal_writes(memfd.as_fd()) {
                eprintln!("aas: could not seal a recorder's ring: {e}");
                return Answer::fatal(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
        if !std::mem::replace(&mut recorder.running, true) {
            let id = recorder.id;
            self.send(vec![Command::SetRunning(id, true)]);
        }
        Answer::reply(Reply::Ok)
    }

    fn stop_recorder(&mut self, conn: ConnId) -> Answer {
        let Some(recorder) = self.recorder_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        if std::mem::take(&mut recorder.running) {
            let id = recorder.id;
            self.send(vec![Command::SetRunning(id, false)]);
        }
        Answer::reply(Reply::Ok)
    }

    /// Stops a capture stream. The reply comes once the audio thread has
    /// stopped writing, so that the client can empty the ring, which is
    /// the reader's to do.
    fn reset_recorder(&mut self, conn: ConnId) -> Answer {
        let Some(recorder) = self.recorder_mut(conn) else {
            return Answer::error(libc::EBADFD);
        };
        recorder.running = false;
        let id = recorder.id;
        self.send(vec![Command::Flush(id)]);
        Answer {
            reply: None,
            fds: Vec::new(),
            close: false,
        }
    }

    fn remove_recorder(&mut self, recorder: Recorder, batch: &mut Vec<Command>) {
        if !recorder.ended {
            batch.push(Command::RemoveTap(recorder.id));
            self.taps_len -= 1;
        }
    }

    /// Ends every capture stream that records at another rate than `rate`.
    /// The capture runs at the output's rate only, so the client has to
    /// configure again. It is told through the ring once the tap is back.
    fn end_recorders(&mut self, rate: u32, batch: &mut Vec<Command>) {
        let recorders = self.conns.values_mut().filter_map(|c| c.recorder.as_mut());
        for recorder in recorders.filter(|r| !r.ended && r.rate != rate) {
            recorder.ended = true;
            recorder.running = false;
            batch.push(Command::RemoveTap(recorder.id));
            self.taps_len -= 1;
        }
    }

    fn recorder_mut(&mut self, conn: ConnId) -> Option<&mut Recorder> {
        self.conns.get_mut(&conn)?.recorder.as_mut()
    }

    fn stream_mut(&mut self, conn: ConnId) -> Option<&mut Stream> {
        self.conns.get_mut(&conn)?.stream.as_mut()
    }

    fn stream_by_id(&mut self, id: ClientId) -> Option<&mut Stream> {
        let mut streams = self.conns.values_mut().filter_map(|c| c.stream.as_mut());
        streams.find(|s| s.id == id)
    }

    fn streams(&self) -> impl Iterator<Item = &Stream> {
        self.conns.values().filter_map(|c| c.stream.as_ref())
    }

    fn streams_mut(&mut self) -> impl Iterator<Item = &mut Stream> {
        self.conns.values_mut().filter_map(|c| c.stream.as_mut())
    }

    fn running_count(&self) -> usize {
        self.streams().filter(|s| s.running).count()
    }

    /// Whether a DoP stream at `rate` can have the output: the card takes
    /// the rate, and the analog chain, if any, can be built at it.
    fn is_supported(&self, rate: u32) -> bool {
        let card = self.supported.is_empty() || self.supported.contains(&rate);
        card && !self.analog_refused.contains(&rate)
    }

    /// The card's rates, without `refused`. Empty takes any rate, as the
    /// card's list does, only while nothing has been refused.
    fn usable(&self, refused: &[u32]) -> Vec<u32> {
        if refused.is_empty() {
            return self.supported.clone();
        }
        let all = if self.supported.is_empty() {
            &STANDARD_RATES[..]
        } else {
            &self.supported[..]
        };
        all.iter()
            .copied()
            .filter(|r| !refused.contains(r))
            .collect()
    }

    /// Gives the rules the rates the card takes and the chains can be built
    /// at. Returns false, and leaves the rules as they are, if there is none.
    fn update_rates(&mut self) -> bool {
        let pcm = self.usable(&self.refused);
        if pcm.is_empty() && !self.refused.is_empty() {
            return false;
        }
        // DoP needs only the analog chain, so only its refusals limit DoP.
        let dop = (self.refused != self.analog_refused).then(|| self.usable(&self.analog_refused));
        self.policy.set_supported(pcm);
        self.policy.set_dop_supported(dop);
        true
    }

    /// Makes the rate rules follow the audio thread's DoP owner: the oldest
    /// claim of a running stream at a rate the card takes. The audio thread
    /// ranks the streams holding the output the same way. A new owner needs its rate at
    /// once, since its audio is discarded until then.
    fn update_dop(&mut self, now: Instant, batch: &mut Vec<Command>) {
        let claimed = self.dop_claims.iter().map(|&(_, id)| id);
        let owner = claimed.into_iter().find(|&id| {
            let stream = self.streams().find(|s| s.id == id);
            stream.is_some_and(|s| s.running && self.is_supported(s.rate))
        });
        if owner == self.dop_owner {
            return;
        }
        match owner {
            Some(id) => self.policy.dop_started(id),
            None => {
                if let Some(old) = self.dop_owner {
                    self.policy.dop_stopped(old);
                }
            }
        }
        self.dop_owner = owner;
        self.new_rate_decision();
        self.sync_analog_client(now);
        if owner.is_some() {
            self.decide(now, true, batch);
        }
    }

    /// Makes the analog input a client of the rate rules while it has
    /// signal and no DoP stream owns the output, and not otherwise. It
    /// joins at the rate the card runs at, by the rules for any client:
    /// it never asks for a rate of its own, it holds the running rate as a
    /// client at it does, and its joining cancels a pending switch away
    /// from that rate. While DoP owns the output the input is not played,
    /// so it is not a client; it joins when DoP ends, at the rate the card
    /// then runs at.
    fn sync_analog_client(&mut self, now: Instant) {
        let t = now - self.epoch;
        let dop = self.dop_owner.is_some();
        let Some(analog) = &mut self.analog else {
            return;
        };
        let want = analog.active && !dop;
        if want && !analog.registered {
            // The rate the card runs at, which the rules may not have
            // settled on yet, such as just after a DoP stream.
            let rate = self.output.map_or(self.policy.current(), |(r, _)| r);
            self.policy.client_started(ANALOG, rate, t);
        } else if !want && analog.registered {
            self.policy.client_stopped(ANALOG, t);
        }
        analog.registered = want;
    }

    /// Takes the output to the rate the rules want, and keeps every running
    /// source's resampler in step with it. `urgent` applies a change at
    /// once; otherwise it waits for the settle time, so that a stop and a
    /// start in quick succession leave the rate alone.
    fn decide(&mut self, now: Instant, urgent: bool, batch: &mut Vec<Command>) {
        let running = self.running_count() > 0;
        if running {
            self.idle_since = None;
        } else {
            self.idle_since.get_or_insert(now);
        }
        let Some(want) = self.usable_rate() else {
            // The output stays as it is; the clients follow it.
            self.sync_resamplers(batch);
            return;
        };
        let reopen = match self.output {
            None => running,
            Some((rate, _)) if rate == want => {
                self.switch_at = None;
                false
            }
            Some(_) => {
                let due = self.switch_at.is_some_and(|at| now >= at);
                if !urgent && !due {
                    self.switch_at.get_or_insert(now + SETTLE);
                }
                urgent || due
            }
        };
        if reopen {
            self.open_output(now, batch);
        } else {
            self.sync_resamplers(batch);
            // Chains for the rate the output already runs at, such as when
            // it stays at the rate a DoP stream set.
            if self.output.is_some_and(|(rate, _)| rate == want) {
                self.send_spare(batch);
            }
        }
        self.sync_further(now);
    }

    /// The rate the rules want, once chains are built for it. A rate at
    /// which one cannot be built is refused: the rules then take the
    /// nearest other rate, and the chains are built for that. None if no
    /// rate works, which is logged once; the output then stays as it is.
    fn usable_rate(&mut self) -> Option<u32> {
        loop {
            let rate = self.policy.current();
            let dop = self.policy.dop_rate().is_some();
            let known = if dop {
                &self.analog_refused
            } else {
                &self.refused
            };
            if known.contains(&rate) {
                // Refused already, and nothing else works.
                return None;
            }
            if self.failed == Some((rate, dop)) {
                // Failed in this rate decision.
                return self.fallback(rate);
            }
            let spare = self.spare.as_ref();
            if spare.is_some_and(|s| s.rate == rate && s.dop == dop) {
                return Some(rate);
            }
            match self.chains_for(rate, dop) {
                Ok(chains) => {
                    self.spare = Some(Spare { rate, dop, chains });
                    return Some(rate);
                }
                Err((_, e)) if e.kind != Failure::Unsupported => {
                    // Not the rate's fault: it is not refused, and the
                    // next rate decision tries again.
                    eprintln!(
                        "aas: {e}; the output stays as it is until the next change of clients"
                    );
                    self.failed = Some((rate, dop));
                    return self.fallback(rate);
                }
                Err((place, e)) => {
                    eprintln!("aas: {e}; the output does not run at {rate} Hz");
                    self.refused.push(rate);
                    if place == PluginPlace::Analog {
                        self.analog_refused.push(rate);
                    }
                    if !self.update_rates() || self.policy.current() == rate {
                        if !std::mem::replace(&mut self.stuck, true) {
                            eprintln!("aas: no rate works for every plugin chain");
                        }
                        return None;
                    }
                }
            }
        }
    }

    /// Builds the chains `rate` needs that are not on the audio thread
    /// already: for a DoP stream the analog chain only, since DoP bypasses
    /// the others and they keep their state. Each is built for the channels
    /// that reach it: two at the analog input, the matrix width at `mix`
    /// and what `mix` gives at `output`. Fails with the place whose chain
    /// could not be built.
    ///
    /// A DoP stream that opens the output before any `mix` or `output`
    /// chain was sent gets those too, so that PCM at its rate never plays
    /// without them. One that cannot be built at the rate does not refuse
    /// DoP there: a chain built for another rate stands in, which the audio
    /// thread does not run at this one, so such PCM is silent.
    fn chains_for(
        &mut self,
        rate: u32,
        dop: bool,
    ) -> Result<Vec<(PluginPlace, Chain)>, (PluginPlace, BuildError)> {
        let mut ready = self.ready.take().filter(|c| c.rate == rate);
        let widths = widths(&self.plugins, self.channels, &mut self.libraries)?;
        let mut chains = Vec::new();
        for place in self.chain_places() {
            let built = self.built[slot(place)];
            let bypassed = dop && place != PluginPlace::Analog;
            if (bypassed && built.is_some()) || built == Some(rate) {
                continue;
            }
            let channels = match place {
                PluginPlace::Analog => 2,
                PluginPlace::Mix => widths.mix,
                PluginPlace::Output => widths.output,
            };
            let chain = match &mut ready {
                Some(ready) => ready.take(place),
                None => match self.build(place, rate, channels) {
                    Ok(chain) => chain,
                    Err(e) if bypassed => {
                        let stand_in = self.stand_in(place, rate, channels);
                        let Some(chain) = stand_in else {
                            return Err((place, e));
                        };
                        eprintln!(
                            "aas: {e}; the output carries only DoP at {rate} Hz, PCM there is silent"
                        );
                        chain
                    }
                    Err(e) => return Err((place, e)),
                },
            };
            chains.push((place, chain));
        }
        Ok(chains)
    }

    /// The chain at `place` for `channels` channels at `rate`.
    fn build(
        &mut self,
        place: PluginPlace,
        rate: u32,
        channels: usize,
    ) -> Result<Chain, BuildError> {
        let (plugins, format) = (&self.plugins, self.format);
        build_chain(plugins, place, rate, format, channels, &mut self.libraries)
    }

    /// The chain at `place` built for the standard rate nearest `rate` at
    /// which it builds, if any.
    fn stand_in(&mut self, place: PluginPlace, rate: u32, channels: usize) -> Option<Chain> {
        let mut rates: Vec<u32> = STANDARD_RATES
            .iter()
            .copied()
            .filter(|&r| r != rate)
            .collect();
        rates.sort_by_key(|&r| (r.abs_diff(rate), r));
        rates
            .into_iter()
            .find_map(|r| self.build(place, r, channels).ok())
    }

    /// A change of clients or of the card: a chain that failed is tried
    /// again, once, at the next rate the rules want.
    fn new_rate_decision(&mut self) {
        self.failed = None;
        self.no_fallback = false;
    }

    /// The rate to use in this rate decision when the chains for `wanted`
    /// failed. An open output keeps what it has: None. A closed one must
    /// not stay closed under a client: it opens at the nearest rate whose
    /// chains build, without refusing `wanted`, which the next decision
    /// tries again. None if no rate builds, logged once per decision.
    fn fallback(&mut self, wanted: u32) -> Option<u32> {
        if self.output.is_some() || self.no_fallback {
            return None;
        }
        if let Some(spare) = &self.spare
            && !spare.dop
            && spare.rate != wanted
        {
            return Some(spare.rate);
        }
        let mut rates = if self.supported.is_empty() {
            STANDARD_RATES.to_vec()
        } else {
            self.supported.clone()
        };
        rates.retain(|r| *r != wanted && !self.refused.contains(r));
        rates.sort_by_key(|&r| (r.abs_diff(wanted), r));
        for rate in rates {
            if let Ok(chains) = self.chains_for(rate, false) {
                eprintln!("aas: the output opens at {rate} Hz until {wanted} Hz works");
                self.spare = Some(Spare {
                    rate,
                    dop: false,
                    chains,
                });
                return Some(rate);
            }
        }
        eprintln!("aas: error: no rate builds every plugin chain, the output stays closed");
        self.no_fallback = true;
        None
    }

    /// Whether a mix or output chain with plugins on the audio thread is
    /// built for another rate than the output runs at.
    fn chains_behind(&self) -> bool {
        let Some((rate, _)) = self.output else {
            return false;
        };
        self.chain_places()
            .into_iter()
            .filter(|&place| place != PluginPlace::Analog)
            .any(|place| self.built[slot(place)] != Some(rate))
    }

    /// Sends the spare chains, which take effect from the next command on.
    fn send_spare(&mut self, batch: &mut Vec<Command>) {
        let Some(spare) = self.spare.take() else {
            return;
        };
        for (place, chain) in spare.chains {
            // A stand-in is built for another rate than the spare's.
            self.built[slot(place)] = Some(chain.rate().unwrap_or(spare.rate));
            batch.push(Command::SetChain(
                Target::master(engine_place(place)),
                Box::new(chain),
            ));
        }
    }

    /// Opens or reopens the output at the rate the rules want. Resamplers
    /// and chains for the new rate go first, so that they take effect with
    /// it. The audio thread closes the capture first; a new one is opened
    /// once the output is open.
    fn open_output(&mut self, now: Instant, batch: &mut Vec<Command>) {
        let Some(rate) = self.usable_rate() else {
            return;
        };
        let period = period_frames(rate, self.latency_ms);
        eprintln!("aas: output at {rate} Hz, {period} frames per period");
        // The cycle ran on the timer at this rate, and the further outputs
        // played on: the card opens under them, and they go on as they are.
        let resumed = self.parked.take() == Some((rate, period));
        self.output = Some((rate, period));
        self.reopening += 1;
        self.switch_at = None;
        self.sync_resamplers(batch);
        self.send_spare(batch);
        if let Some(analog) = &mut self.analog {
            analog.reading = false;
            analog.retry_at = None;
        }
        self.end_recorders(rate, batch);
        if resumed {
            for output in self.further.iter_mut().filter(|o| o.open) {
                output.resumed_until = Some(now + RESUME_STARVE);
            }
        } else {
            // The writers close while the master reopens, and hear of its
            // new configuration once it is open.
            for output in &mut self.further {
                if std::mem::take(&mut output.open) {
                    output.writer.set_open(false);
                }
            }
            self.plan_further(rate, period, batch, None);
        }
        batch.push(Command::Reconfigure { rate, period });
    }

    /// Prepares the further outputs for the master at `rate` and `period`:
    /// their chains, their cards' rates and periods, fresh resamplers, and
    /// the delay every output is held at, which the master's delay line
    /// makes up. A chain that does not build at the rate silences its output
    /// alone. The writers get it once the master is open.
    ///
    /// With `only`, the master stays as it is and the rates of that output's
    /// card have changed: it alone is given a new configuration, unless the
    /// delay all are held at changes, which every output then follows.
    fn plan_further(
        &mut self,
        rate: u32,
        period: u32,
        batch: &mut Vec<Command>,
        only: Option<usize>,
    ) {
        if self.further.is_empty() {
            return;
        }
        let (quality, format) = (self.quality, self.format);
        if only.is_none() {
            for i in 0..self.further.len() {
                let output = &mut self.further[i];
                let built = further_chains(output, rate, format, &mut self.libraries, batch);
                let silenced = match built {
                    Ok(()) => false,
                    Err(e) => {
                        let number = output.number();
                        eprintln!("aas: output {number}: {e}; it is silent at {rate} Hz");
                        true
                    }
                };
                if silenced != output.silenced {
                    output.silenced = silenced;
                    let gain = self.further_gain(&self.further[i]);
                    batch.push(Command::SetGain(self.further[i].index, gain));
                }
            }
        }
        let parts: Vec<(u32, u32, f64)> = self
            .further
            .iter()
            .map(|o| part_at(rate, card_rate(&o.rates, rate), o.latency_ms, quality))
            .collect();
        let target = align_frames(rate, period, &parts);
        let moved = self.target != Some(target);
        let given: Vec<bool> = self
            .further
            .iter()
            .zip(&parts)
            .map(|(o, &part)| match only {
                None => true,
                Some(index) => moved || (o.index == index && o.current != Some(part)),
            })
            .collect();
        if !given.contains(&true) {
            return;
        }
        self.target = Some(target);
        // Only a rate between the standard ones, or below 44.1 kHz, can
        // miss: `check_alignment` rules it out for the others.
        let (delay, missed) = master_delay(target, period, self.delay_capacity);
        if let Some(missed) = missed {
            let warned = match missed {
                Misaligned::Ahead { .. } => &mut self.delay_warned,
                Misaligned::Behind { .. } => &mut self.behind_warned,
            };
            if !std::mem::replace(warned, true) {
                eprintln!("{}", missed.line());
            }
        }
        if self.delay != Some(delay) {
            self.delay = Some(delay);
            batch.push(Command::SetDelay(delay));
        }
        let outputs = self.further.iter_mut().zip(parts).zip(given);
        for (output, part) in outputs.filter(|(_, given)| *given).map(|(o, _)| o) {
            let (card, card_period, _) = part;
            let number = output.number();
            // The start refuses latencies that need more; a rate between
            // the standard ones, or below 44.1 kHz, may still.
            let (feed, cut) = feed_within(rate, period, target, part, output.capacity);
            if cut {
                eprintln!(
                    "aas: warning: output {number}: its feed holds only {feed} frames, \
                     for the drift to follow at {rate} Hz"
                );
            }
            let resampler =
                Adaptive::new(rate, card, output.channels, card_period, quality, format);
            eprintln!(
                "aas: output {number} at {card} Hz, {card_period} frames per period, \
                 {target} frames behind the mix like every output"
            );
            output.current = Some(part);
            output.pending = Some(Pending {
                card_rate: card,
                period: card_period,
                resampler: Box::new(resampler),
                target,
                feed,
            });
        }
    }

    /// Hands each further output its configuration, once the master is
    /// open at it, and opens it as its idle rule says.
    fn hand_over(&mut self, now: Instant) {
        let (Some((rate, _)), 0) = (self.output.or(self.parked), self.reopening) else {
            return;
        };
        for output in &mut self.further {
            let Some(p) = output.pending.take() else {
                continue;
            };
            let target = f64::from(p.target);
            let writer = &output.writer;
            writer.reconfigure(rate, p.card_rate, p.period, p.resampler, target, p.feed);
            // An open writer opens its card again at the configuration.
            output.configured = true;
        }
        self.sync_further(now);
    }

    /// Opens each further output while the cycle runs, on the master's card
    /// or on the timer, and its writer has the configuration for it, unless
    /// its idle rule has closed it, and closes it otherwise. A parked cycle
    /// ends once no further output is open.
    fn sync_further(&mut self, now: Instant) {
        if self.further.is_empty() {
            return;
        }
        let signal = self.analog.as_ref().is_some_and(|a| a.active);
        if self.running_count() > 0 || signal {
            self.quiet_since = None;
        } else {
            self.quiet_since.get_or_insert(now);
        }
        // A reopen gives every writer a configuration to wait for, which
        // keeps it closed until the master is open at it.
        let cycle = self.output.or(self.parked).is_some();
        let quiet_since = self.quiet_since;
        for output in &mut self.further {
            let idle = output_idle(output.idle, quiet_since, now);
            let want = cycle && output.configured && output.pending.is_none() && !idle;
            if want == output.open {
                continue;
            }
            if let (Idle::Off { delay_s }, true) = (output.idle, cycle && idle) {
                eprintln!("aas: output {} off after {delay_s} s idle", output.number());
            }
            output.writer.set_open(want);
            output.open = want;
        }
        if self.parked.is_some() && !self.further.iter().any(|o| o.open) {
            self.parked = None;
            self.send(vec![Command::CloseOutput]);
        }
    }

    /// The further outputs whose own idle rules keep them open now.
    fn kept_open(&self, now: Instant) -> usize {
        let outputs = self.further.iter().filter(|o| o.configured);
        outputs
            .filter(|o| !output_idle(o.idle, self.quiet_since, now))
            .count()
    }

    /// The rates a further output's card takes have changed, such as when
    /// it is found. An open master moves it to the rate it now takes.
    pub fn set_output_rates(&mut self, index: usize, rates: Vec<u32>, now: Instant) {
        let Some(output) = self.further.iter_mut().find(|o| o.index == index) else {
            return;
        };
        eprintln!(
            "aas: output {}: the card takes the rates {rates:?}",
            output.number()
        );
        output.rates = rates;
        let Some((rate, period)) = self.output.or(self.parked) else {
            return;
        };
        let mut batch = Vec::new();
        self.plan_further(rate, period, &mut batch, Some(index));
        self.send(batch);
        self.hand_over(now);
    }

    /// Logs what the further outputs' writers report.
    fn further_events(&mut self, now: Instant) {
        for output in &mut self.further {
            let number = output.number();
            while let Some(event) = output.writer.event() {
                let resuming = output.resumed_until.is_some_and(|until| now <= until);
                if resuming && matches!(event, FurtherEvent::Starved { .. }) {
                    output.resumed_until = None;
                    eprintln!(
                        "aas: output {number} starved while the master's card opened, \
                         it primes again"
                    );
                    continue;
                }
                let line = event_line(number, event).or_else(|| output.log.line(number, event));
                if let Some(line) = line {
                    eprintln!("{line}");
                }
            }
        }
    }

    /// The places that get a chain: those the config lists plugins at, and
    /// the analog input, which always has its detector.
    fn chain_places(&self) -> Vec<PluginPlace> {
        let places = [PluginPlace::Analog, PluginPlace::Mix, PluginPlace::Output];
        places
            .into_iter()
            .filter(|&place| match place {
                PluginPlace::Analog => self.analog.is_some(),
                _ => self.plugins.iter().any(|p| p.place == place),
            })
            .collect()
    }

    /// Opens the capture for the output and hands it to the audio thread,
    /// once the output is open and if it has none that works. One that
    /// will not open is tried again a second later.
    fn open_capture(&mut self, now: Instant, batch: &mut Vec<Command>) {
        let (Some((rate, period)), 0) = (self.output, self.reopening) else {
            return;
        };
        let Some(analog) = &mut self.analog else {
            return;
        };
        if analog.reading {
            return;
        }
        match (analog.open)(rate, period) {
            Ok(capture) => {
                batch.push(Command::SetCapture(Some(capture)));
                analog.unanswered += 1;
                analog.reading = true;
                analog.retry_at = None;
                if std::mem::take(&mut analog.failing) {
                    eprintln!("aas: the analog input works again");
                }
            }
            Err(OpenError::Never(e)) => {
                eprintln!("aas: the analog input: {e}; it stays off");
                self.analog = None;
            }
            Err(OpenError::Again(e)) => {
                if !std::mem::replace(&mut analog.failing, true) {
                    eprintln!("aas: the analog input: {e}, trying again every second");
                }
                analog.retry_at = Some(now + CAPTURE_RETRY);
            }
        }
    }

    /// The audio thread reports that the capture failed, and it is closed. A
    /// report that comes before the answer to a newer capture is about the
    /// one that capture replaced, so it changes nothing.
    fn capture_failed(&mut self, now: Instant) {
        let Some(analog) = &mut self.analog else {
            return;
        };
        if analog.unanswered > 0 || !analog.reading {
            return;
        }
        analog.reading = false;
        if !std::mem::replace(&mut analog.failing, true) {
            eprintln!("aas: the analog input failed, trying again every second");
        }
        analog.retry_at = Some(now + CAPTURE_RETRY);
    }

    /// The analog input has signal or not. With signal it is a client of
    /// the rate rules, see `sync_analog_client`, and the rate is decided
    /// again as for a player that starts or stops.
    fn input_signal(&mut self, on: bool, now: Instant, batch: &mut Vec<Command>) {
        self.new_rate_decision();
        let Some(analog) = &mut self.analog else {
            return;
        };
        eprintln!("aas: analog input: {}", if on { "signal" } else { "quiet" });
        analog.active = on;
        self.sync_analog_client(now);
        self.decide(now, false, batch);
    }

    /// Gives each source at another rate than the output a resampler for
    /// it, and takes it away from the others. A stopped source keeps its
    /// own, so that a pause or a reset does not build a new one, which
    /// takes far longer than a period.
    fn sync_resamplers(&mut self, batch: &mut Vec<Command>) {
        let output = self.output;
        let (quality, format) = (self.quality, self.format);
        let streams = self.conns.values_mut().filter_map(|c| c.stream.as_mut());
        for stream in streams {
            let want = match output {
                Some((rate, period)) if stream.rate != rate => Some((rate, period)),
                _ => None,
            };
            if want == stream.resampler {
                continue;
            }
            stream.resampler = want;
            let resampler = want.map(|(rate, period)| {
                let r = Resampler::new(stream.rate, rate, period, quality, format);
                if r.input_frames_max() > stream.capacity {
                    eprintln!(
                        "aas: a ring of {} frames is too small to resample from {} Hz",
                        stream.capacity, stream.rate
                    );
                }
                Box::new(r)
            });
            batch.push(Command::SetResampler(stream.id, resampler));
        }
    }

    /// The card's rates have changed, such as when a card that was missing
    /// is found. The output moves at once to a rate the card takes.
    pub fn set_supported(&mut self, rates: Vec<u32>, now: Instant) {
        self.new_rate_decision();
        eprintln!("aas: the card takes the rates {rates:?}");
        self.supported = rates;
        // New rates are a new chance, and a new failure is worth a line.
        self.stuck = false;
        if !self.update_rates() {
            eprintln!("aas: no rate the card takes works for every plugin chain");
        }
        let mut batch = Vec::new();
        self.update_dop(now, &mut batch);
        self.decide(now, true, &mut batch);
        self.send(batch);
    }

    /// Whether the output has been told to stay open.
    pub fn output_is_open(&self) -> bool {
        self.output.is_some()
    }

    /// Takes what the audio thread has sent and acts on it.
    pub fn handle_events(&mut self, now: Instant) {
        let mut batch = Vec::new();
        let mut events = std::mem::take(&mut self.stash);
        while let Ok(event) = self.events.pop() {
            events.push(event);
        }
        for event in events {
            match event {
                // Freed here, not on the audio thread.
                Event::Dropped(_)
                | Event::Replaced(_)
                | Event::OldTable(_)
                | Event::OldChain(..)
                | Event::OldTaps(_)
                | Event::OldMatrix(..)
                | Event::OldOutput(..) => {}
                // The audio thread writes it no more, so a client that
                // reads it empty after this has all of it. A client that
                // gave the stream up meanwhile is told nothing it minds.
                // The client is woken: nothing else will wake one that
                // waits for frames.
                Event::DroppedTap(tap) => {
                    tap.ring.header().ended.store(1, Ordering::Release);
                    let one = 1u64;
                    // SAFETY: 8 bytes to a non-blocking eventfd. A full
                    // counter only means the client is woken already.
                    unsafe { libc::write(tap.wake.as_raw_fd(), (&raw const one).cast(), 8) };
                }
                Event::OldCapture(_) => {
                    if let Some(analog) = &mut self.analog {
                        analog.unanswered = analog.unanswered.saturating_sub(1);
                    }
                }
                Event::InputSignal(on) => self.input_signal(on, now, &mut batch),
                Event::CaptureFailed(mut capture) => {
                    // Off the audio thread, as the engine leaves it.
                    capture.close();
                    self.capture_failed(now);
                }
                Event::Reconfigured => {
                    self.reopening = self.reopening.saturating_sub(1);
                    self.open_capture(now, &mut batch);
                    self.hand_over(now);
                }
                Event::DopStarted(id, claim) => {
                    // The events can come in another order than the claims
                    // were made, when the queue was full meanwhile. The
                    // number says which is older, as the audio thread has it.
                    self.dop_claims.retain(|&(_, c)| c != id);
                    let at = self.dop_claims.partition_point(|&(c, _)| c < claim);
                    self.dop_claims.insert(at, (claim, id));
                    self.update_dop(now, &mut batch);
                }
                Event::DopStopped(id) => {
                    self.dop_claims.retain(|&(_, c)| c != id);
                    self.update_dop(now, &mut batch);
                    // At once if the mix or output chain on the audio
                    // thread is built for another rate than the output's:
                    // the engine keeps them silent until the output and
                    // its chains follow. Otherwise after the settle time,
                    // so that a stop and a start leave the rate alone.
                    let urgent = self.chains_behind();
                    self.decide(now, urgent, &mut batch);
                }
                Event::Drained(id) => self.drained(id, now, &mut batch),
                Event::Flushed(id) => self.flushed(id),
            }
        }
        self.send(batch);
        self.further_events(now);
    }

    /// Applies what is due by the clock: a settled rate change, the end of
    /// a drain the audio thread never reported, and the idle timer. Such a
    /// drain is not ended while the output is being opened.
    pub fn tick(&mut self, now: Instant) {
        let mut batch = Vec::new();
        let before = self.policy.current();
        self.policy.tick(now - self.epoch);
        let settled = self.policy.current() != before;
        let mut overdue = Vec::new();
        let reopening = self.reopening > 0;
        for stream in self.streams_mut() {
            if !stream.drain.is_some_and(|at| now >= at) {
                continue;
            }
            // A drain that still makes progress is slow, not stuck: the
            // audio thread can fall behind real time when its cycles run
            // long. It gets more time for as long as it takes frames. So
            // does one that waits for the card to open: its audio has not
            // been played, and the time runs from when the card is open.
            let read = stream.view.header().read_pos.load(Ordering::Acquire);
            if read != stream.drain_seen || reopening {
                stream.drain_seen = read;
                stream.drain = Some(now + DRAIN_SPARE);
            } else {
                overdue.push(stream.id);
            }
        }
        for id in overdue {
            // A last resort: the audio thread should always report it.
            eprintln!("aas: a drain was never reported, answering it now");
            self.drained(id, now, &mut batch);
        }
        self.decide(now, settled, &mut batch);
        let retry = self.analog.as_ref().and_then(|a| a.retry_at);
        if retry.is_some_and(|at| now >= at) {
            self.open_capture(now, &mut batch);
        }
        // The analog input keeps the output open.
        let idle = if self.analog.is_some() {
            Idle::Zeros
        } else {
            self.idle
        };
        if let (Idle::Off { delay_s }, Some(since)) = (idle, self.idle_since) {
            let delay = Duration::from_secs(u64::from(delay_s));
            let kept = self.kept_open(now);
            // A reopen under way is waited for: the cycle parks at the
            // format the engine runs at.
            let parks = kept > 0 && self.reopening == 0;
            if let Some(format) = self.output
                && now >= since + delay
                && (kept == 0 || parks)
            {
                eprintln!("aas: output off after {delay_s} s idle");
                self.output = None;
                self.switch_at = None;
                if parks {
                    let (rate, period) = format;
                    let others = if kept == 1 {
                        "1 other output keeps".to_string()
                    } else {
                        format!("{kept} other outputs keep")
                    };
                    eprintln!("aas: the master's card is closed, {others} running");
                    self.parked = Some(format);
                    batch.push(Command::ParkOutput { rate, period });
                } else {
                    batch.push(Command::CloseOutput);
                }
            }
        }
        self.send(batch);
        self.sync_further(now);
        if self.save_at.is_some_and(|at| now >= at) {
            self.save();
        }
    }

    /// Queues the commands in one commit and wakes the audio thread. Waits
    /// for room if needed, taking events out of the other queue meanwhile,
    /// because the audio thread stops taking commands while that is full.
    ///
    /// The wait has no limit: a command left out would leave the two
    /// threads with different ideas of the clients. The audio thread reads
    /// commands every cycle, so room comes within a period. A wait of a
    /// second means it is stuck, and is logged once.
    fn send(&mut self, batch: Vec<Command>) {
        let capacity = self.commands.buffer().capacity();
        let mut waiting_since: Option<Instant> = None;
        let mut warned = false;
        let mut batch = batch.into_iter().peekable();
        while batch.peek().is_some() {
            // Nobody is left to take them.
            if self.commands.is_abandoned() {
                return;
            }
            let want = batch.len().min(capacity);
            match self.commands.write_chunk_uninit(want) {
                Ok(chunk) => {
                    chunk.fill_from_iter(batch.by_ref().take(want));
                }
                Err(_) => {
                    while let Ok(event) = self.events.pop() {
                        self.stash.push(event);
                    }
                    self.audio.unpark();
                    let since = *waiting_since.get_or_insert_with(Instant::now);
                    if !warned && since.elapsed() >= SEND_WARN {
                        warned = true;
                        eprintln!(
                            "aas: the audio thread has taken no command for {} s, still waiting",
                            SEND_WARN.as_secs()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        self.audio.unpark();
    }
}

/// A non-blocking eventfd, as the audio thread writes them.
pub(crate) fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: a plain syscall; the descriptor is owned at once.
    let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive::Adaptive;
    use crate::capture::MemoryCapture;
    use crate::config::{self, Format, OutputConfig, Quality};
    use crate::format::F32;
    use crate::further::{FeedProbe, FurtherEvent};
    use crate::output_mix::OutputMix;
    use crate::plugin::chain::Chain;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_drain_reported_before_its_request_is_answered_at_once() {
        let (commands, _engine_commands) = rtrb::RingBuffer::new(64);
        let (mut engine_events, events) = rtrb::RingBuffer::new(64);
        let config = Config::default();
        let audio = std::thread::current();
        let mut control = Control::new(&config, 4.0, Vec::new(), commands, events, audio);
        let now = Instant::now();
        control.connected(1, "test".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        let said = Reply::Hello {
            version: proto::VERSION,
        };
        assert_eq!(control.request(1, Some(hello), now).reply, Some(said));
        let configure = Request::Configure {
            rate: 48000,
            format: ClientFormat::S32,
            channels: 2,
            buffer_frames: 4096,
            capture: false,
        };
        let answer = control.request(1, Some(configure), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        let start = control.request(1, Some(Request::Start), now);
        assert_eq!(start.reply, Some(Reply::Ok));
        // The client set the flag, and the audio thread saw the ring empty
        // before the request arrived.
        assert!(engine_events.push(Event::Drained(1)).is_ok());
        control.handle_events(now);
        let drain = control.request(1, Some(Request::Drain), now);
        assert_eq!(drain.reply, Some(Reply::Ok));
        // A restart forgets it.
        let start = control.request(1, Some(Request::Start), now);
        assert_eq!(start.reply, Some(Reply::Ok));
        assert!(
            control
                .request(1, Some(Request::Drain), now)
                .reply
                .is_none()
        );
    }

    /// A control thread with one connection that has said hello, and the
    /// audio thread's end of the command queue. One `Reconfigured` waits in
    /// the event queue, for a test to take with `handle_events` once it has
    /// made the output open.
    fn hello() -> (Control, rtrb::Consumer<Command>, Instant) {
        let (commands, engine_commands) = rtrb::RingBuffer::new(64);
        let (mut engine_events, events) = rtrb::RingBuffer::new(64);
        assert!(engine_events.push(Event::Reconfigured).is_ok());
        let config = Config::default();
        let audio = std::thread::current();
        let mut control = Control::new(&config, 12.0, Vec::new(), commands, events, audio);
        let now = Instant::now();
        control.connected(1, "test".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        let said = Reply::Hello {
            version: proto::VERSION,
        };
        assert_eq!(control.request(1, Some(hello), now).reply, Some(said));
        (control, engine_commands, now)
    }

    fn configure(rate: u32, buffer_frames: u32) -> Request {
        Request::Configure {
            rate,
            format: ClientFormat::S16,
            channels: 2,
            buffer_frames,
            capture: false,
        }
    }

    /// The source the audio thread was given.
    fn added(commands: &mut rtrb::Consumer<Command>) -> Box<Source> {
        while let Ok(command) = commands.pop() {
            if let Command::Add(source) = command {
                return source;
            }
        }
        panic!("no source added");
    }

    /// A card's mixer that records what it is set to.
    struct FakeMixer(std::sync::Arc<std::sync::Mutex<Vec<f32>>>);

    impl HardwareVolume for FakeMixer {
        fn set_db(&mut self, db: f32) {
            self.0.lock().unwrap().push(db);
        }

        fn range_db(&self) -> (f32, f32) {
            (-103.5, 0.0)
        }

        fn can_mute(&self) -> bool {
            true
        }
    }

    #[test]
    fn the_same_volume_again_is_written_to_the_card_but_not_announced() {
        let (mut control, mut commands, now) = hello();
        let set = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        control.card_changed(Some(Box::new(FakeMixer(set.clone()))));
        let volume = |percent| Some(Request::SetVolume { percent });
        assert_eq!(
            control
                .request(1, Some(Request::SubscribeVolume), now)
                .reply,
            Some(Reply::Volume { percent: 100 })
        );
        assert_eq!(control.request(1, volume(50), now).reply, Some(Reply::Ok));
        assert_eq!(*set.lock().unwrap(), [0.0, -30.0]);
        assert_eq!(control.take_notices().len(), 1);
        while commands.pop().is_ok() {}
        // Someone moved the card's mixer with another tool meanwhile. The
        // same percent again puts it back.
        assert_eq!(control.request(1, volume(50), now).reply, Some(Reply::Ok));
        assert_eq!(*set.lock().unwrap(), [0.0, -30.0, -30.0]);
        // Nothing changed for the subscribers, the engine or the state file.
        assert!(control.take_notices().is_empty());
        assert!(commands.pop().is_err());
        assert!(control.save_at.is_none());
    }

    #[test]
    fn the_header_view_follows_the_ring() {
        let (ring, memfd) = Ring::create(1 << 20).unwrap();
        let view = HeaderView::map(memfd.as_fd()).unwrap();
        ring.write(&[[1, 1]; 300]);
        ring.consume(200);
        assert_eq!(view.header().read_pos.load(Ordering::Acquire), 200);
        assert_eq!(view.header().write_pos.load(Ordering::Acquire), 300);
    }

    #[test]
    fn a_buffer_too_short_to_refill_a_period_is_refused() {
        let (mut control, _commands, now) = hello();
        for rate in [8000, 44100, 48000, 96000, 192000, 384000] {
            let least = min_buffer_frames(rate, 12.0);
            let short = control.request(1, Some(configure(rate, least - 1)), now);
            let code = libc::EINVAL as u32;
            assert_eq!(short.reply, Some(Reply::Error { code }), "{rate}");
            let enough = control.request(1, Some(configure(rate, least)), now);
            assert_eq!(enough.reply, Some(Reply::Configured { rate: 0 }), "{rate}");
        }
    }

    #[test]
    fn the_least_buffer_holds_two_periods_of_input_at_any_output_rate() {
        let rates = [
            8000, 11025, 22050, 44100, 48000, 96000, 176400, 192000, 384000,
        ];
        for latency_ms in [4.0, 12.0, 100.0] {
            for from in rates {
                let least = u64::from(min_buffer_frames(from, latency_ms));
                for to in rates {
                    let period = u64::from(period_frames(to, latency_ms));
                    let input = (period * u64::from(from)).div_ceil(u64::from(to));
                    assert!(least >= 2 * input, "{from} to {to} at {latency_ms} ms");
                }
            }
        }
    }

    #[test]
    fn a_request_during_a_drain_answers_the_drain_first() {
        let (mut control, _commands, now) = hello();
        let answer = control.request(1, Some(configure(48000, 4096)), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        assert_eq!(
            control.request(1, Some(Request::Start), now).reply,
            Some(Reply::Ok)
        );
        assert!(
            control
                .request(1, Some(Request::Drain), now)
                .reply
                .is_none()
        );
        let stop = control.request(1, Some(Request::Stop), now);
        assert_eq!(stop.reply, Some(Reply::Ok));
        let code = libc::ECANCELED as u32;
        assert_eq!(control.take_replies_for(1), vec![Reply::Error { code }]);
        // Nothing else is owed, now or later.
        control.tick(now + Duration::from_secs(60));
        assert!(control.take_replies().is_empty());
    }

    #[test]
    fn a_drain_that_makes_progress_is_not_cut_short() {
        let (mut control, mut commands, now) = hello();
        let answer = control.request(1, Some(configure(48000, 4096)), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        let source = added(&mut commands);
        source.ring.write(&[[1, 1]; 4096]);
        assert_eq!(
            control.request(1, Some(Request::Start), now).reply,
            Some(Reply::Ok)
        );
        assert!(
            control
                .request(1, Some(Request::Drain), now)
                .reply
                .is_none()
        );
        // The output has opened.
        control.handle_events(now);
        // The audio thread runs slower than real time but keeps taking
        // frames, well past the deadline set for the ring.
        let mut t = now;
        for _ in 0..20 {
            t += Duration::from_millis(400);
            source.ring.consume(100);
            control.tick(t);
            assert!(control.take_replies().is_empty(), "answered early");
        }
        // Once it stops taking frames the drain is answered after all.
        control.tick(t + Duration::from_millis(400));
        control.tick(t + Duration::from_millis(1000));
        assert_eq!(control.take_replies(), vec![(1, Reply::Ok)]);
    }

    #[test]
    fn a_drain_is_not_answered_while_the_output_is_still_opening() {
        let (mut control, mut commands, now) = hello();
        let answer = control.request(1, Some(configure(48000, 4096)), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        let source = added(&mut commands);
        source.ring.write(&[[1, 1]; 4096]);
        let start = control.request(1, Some(Request::Start), now);
        assert_eq!(start.reply, Some(Reply::Ok));
        let drain = control.request(1, Some(Request::Drain), now);
        assert!(drain.reply.is_none());
        // The card takes long to open, so nothing is played, far past the
        // time the ring lasts. The audio was not played, so no reply.
        let mut t = now;
        for _ in 0..20 {
            t += Duration::from_millis(400);
            control.tick(t);
            assert!(control.take_replies().is_empty(), "answered unplayed");
        }
        // Once it is open, the last resort applies again from then.
        control.handle_events(t);
        control.tick(t + Duration::from_millis(400));
        assert!(control.take_replies().is_empty());
        control.tick(t + Duration::from_millis(1000));
        assert_eq!(control.take_replies(), vec![(1, Reply::Ok)]);
    }

    /// Waits for the helper thread to have written this volume.
    fn saved(path: &std::path::Path, percent: u8) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if path.exists() && State::load(path).output_percent == percent {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn the_volume_is_saved_once_it_has_been_left_alone_for_a_second() {
        let (mut control, _commands, now) = hello();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let volume = OutputVolume::new(None, config::BitPerfect::Auto);
        control.restore_volume(volume, path.clone());
        let set = |percent| Some(Request::SetVolume { percent });
        control.request(1, set(40), now);
        // A slider still moving: each change starts the second again.
        control.request(1, set(30), now + Duration::from_millis(500));
        control.tick(now + Duration::from_millis(1499));
        assert!(control.save_at.is_some());
        assert!(!path.exists());
        control.tick(now + Duration::from_millis(1500));
        assert!(control.save_at.is_none());
        assert!(saved(&path, 30));
        // Nothing more is written until the next change.
        std::fs::remove_file(&path).unwrap();
        control.tick(now + Duration::from_secs(60));
        control.shutdown();
        assert!(!path.exists());
    }

    #[test]
    fn a_volume_still_waiting_is_on_disk_when_shutdown_returns() {
        let (mut control, _commands, now) = hello();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let volume = OutputVolume::new(None, config::BitPerfect::Auto);
        control.restore_volume(volume, path.clone());
        control.request(1, Some(Request::SetVolume { percent: 20 }), now);
        control.shutdown();
        assert_eq!(State::load(&path).output_percent, 20);
        // Only the state file is left, no temporary one.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_batch_larger_than_the_queue_arrives_whole_and_in_order() {
        let (commands, mut engine_commands) = rtrb::RingBuffer::new(4);
        let (_engine_events, events) = rtrb::RingBuffer::new(4);
        let audio = std::thread::current();
        let mut control = Control::new(
            &Config::default(),
            12.0,
            Vec::new(),
            commands,
            events,
            audio,
        );
        // The audio thread takes one command now and then.
        let reader = std::thread::spawn(move || {
            let mut gains = Vec::new();
            while gains.len() < 11 {
                match engine_commands.pop() {
                    Ok(Command::SetGain(0, gain)) => gains.push(gain),
                    Ok(_) => panic!("another command"),
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
            gains
        });
        control.send((0..11).map(|i| Command::SetGain(0, i as f32)).collect());
        let expect: Vec<f32> = (0..11).map(|i| i as f32).collect();
        assert_eq!(reader.join().unwrap(), expect);
    }

    #[test]
    fn the_oldest_dop_claim_owns_the_rate_whatever_order_the_events_come_in() {
        let (commands, _engine_commands) = rtrb::RingBuffer::new(64);
        let (mut engine_events, events) = rtrb::RingBuffer::new(64);
        let audio = std::thread::current();
        let mut control = Control::new(
            &Config::default(),
            12.0,
            Vec::new(),
            commands,
            events,
            audio,
        );
        let now = Instant::now();
        for (conn, rate) in [(1, 176400), (2, 352800)] {
            control.connected(conn, "test".into());
            let hello = Request::Hello {
                version: proto::VERSION,
            };
            control.request(conn, Some(hello), now);
            let buffer = min_buffer_frames(rate, 12.0);
            let answer = control.request(conn, Some(configure(rate, buffer)), now);
            assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
            let start = control.request(conn, Some(Request::Start), now);
            assert_eq!(start.reply, Some(Reply::Ok));
        }
        // The audio thread saw client 2 play DoP first, but its event could
        // not be sent until after the one for client 1.
        assert!(engine_events.push(Event::DopStarted(1, 8)).is_ok());
        assert!(engine_events.push(Event::DopStarted(2, 7)).is_ok());
        control.handle_events(now);
        assert_eq!(control.dop_owner, Some(2));
        assert_eq!(control.policy.current(), 352800);
        // Once the older claim ends, the other one holds the output.
        assert!(engine_events.push(Event::DopStopped(2)).is_ok());
        control.handle_events(now);
        assert_eq!(control.dop_owner, Some(1));
        assert_eq!(control.policy.current(), 176400);
    }

    #[test]
    fn a_drain_is_held_until_the_last_of_two_opens_is_done() {
        let (commands, mut engine_commands) = rtrb::RingBuffer::new(64);
        let (mut engine_events, events) = rtrb::RingBuffer::new(64);
        let audio = std::thread::current();
        let mut control = Control::new(
            &Config::default(),
            12.0,
            Vec::new(),
            commands,
            events,
            audio,
        );
        let now = Instant::now();
        control.connected(1, "test".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(1, Some(hello), now);
        let answer = control.request(1, Some(configure(48000, 4096)), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        let source = added(&mut engine_commands);
        source.ring.write(&[[1, 1]; 4096]);
        // The start opens the output, and new rates of the card open it
        // again at once.
        control.request(1, Some(Request::Start), now);
        control.set_supported(vec![44100], now);
        assert!(
            control
                .request(1, Some(Request::Drain), now)
                .reply
                .is_none()
        );
        // The first open is done. The second is still under way.
        assert!(engine_events.push(Event::Reconfigured).is_ok());
        control.handle_events(now);
        let mut t = now;
        for _ in 0..20 {
            t += Duration::from_millis(400);
            control.tick(t);
            assert!(control.take_replies().is_empty(), "answered unplayed");
        }
        assert!(engine_events.push(Event::Reconfigured).is_ok());
        control.handle_events(t);
        control.tick(t + Duration::from_millis(1000));
        assert_eq!(control.take_replies(), vec![(1, Reply::Ok)]);
    }

    /// Opens `MemoryCapture`s for the control thread and records the rate
    /// and period of each open. The next `fails` opens fail.
    #[derive(Clone, Default)]
    struct Opener {
        asked: Arc<Mutex<Vec<(u32, u32)>>>,
        fails: Arc<Mutex<u32>>,
    }

    impl Opener {
        fn open(&self) -> OpenCapture {
            let me = self.clone();
            Box::new(move |rate, period| {
                me.asked.lock().unwrap().push((rate, period));
                let mut fails = me.fails.lock().unwrap();
                if *fails > 0 {
                    *fails -= 1;
                    return Err("no such device".into());
                }
                let (capture, _feed) = MemoryCapture::new();
                Ok(Box::new(capture) as Box<dyn Capture>)
            })
        }

        fn asked(&self) -> Vec<(u32, u32)> {
            self.asked.lock().unwrap().clone()
        }

        fn fail(&self, times: u32) {
            *self.fails.lock().unwrap() = times;
        }
    }

    /// The audio thread as far as the control thread can tell: it takes the
    /// commands, answers each as the engine does, and names them.
    struct FakeEngine {
        commands: rtrb::Consumer<Command>,
        events: rtrb::Producer<Event>,
        capture: Option<Box<dyn Capture>>,
        /// Every `SetGain`, by output.
        gains: Vec<(usize, f32)>,
        /// Every further output put in place, with the gain of its mix.
        outputs: Vec<(usize, f32)>,
    }

    impl FakeEngine {
        fn new(commands: rtrb::Consumer<Command>, events: rtrb::Producer<Event>) -> FakeEngine {
            FakeEngine {
                commands,
                events,
                capture: None,
                gains: Vec::new(),
                outputs: Vec::new(),
            }
        }

        /// Takes every queued command and answers it. A reopen answers at
        /// once only with `opened`.
        fn take(&mut self, opened: bool) -> Vec<String> {
            let mut seen = Vec::new();
            while let Ok(command) = self.commands.pop() {
                let (name, answer) = match command {
                    Command::Add(_) => ("Add".into(), None),
                    Command::Remove(_) => ("Remove".into(), None),
                    Command::SetRunning(id, on) => (format!("SetRunning {id} {on}"), None),
                    Command::Flush(id) => (format!("Flush {id}"), Some(Event::Flushed(id))),
                    Command::SetResampler(id, r) => (
                        format!("SetResampler {id} {}", r.is_some()),
                        Some(Event::Replaced(r)),
                    ),
                    Command::Reconfigure { rate, period } => (
                        format!("Reconfigure {rate} {period}"),
                        opened.then_some(Event::Reconfigured),
                    ),
                    Command::CloseOutput => ("CloseOutput".into(), None),
                    Command::ParkOutput { rate, period } => {
                        (format!("ParkOutput {rate} {period}"), None)
                    }
                    Command::SetGain(output, gain) => {
                        self.gains.push((output, gain));
                        ("SetGain".into(), None)
                    }
                    Command::Grow(t) => ("Grow".into(), Some(Event::OldTable(t))),
                    Command::SetChain(target, chain) => {
                        let names: Vec<&str> = chain.names().collect();
                        let place = target.place;
                        let name = match target.output {
                            0 => format!("SetChain {place:?} {}", names.join(",")),
                            n => format!("SetChain {n} {place:?} {}", names.join(",")),
                        };
                        (name, Some(Event::OldChain(target.output, chain)))
                    }
                    Command::SetParam { .. } => ("SetParam".into(), None),
                    Command::SetCapture(c) => {
                        let old = std::mem::replace(&mut self.capture, c);
                        let name = format!("SetCapture {}", self.capture.is_some());
                        (name, Some(Event::OldCapture(old)))
                    }
                    Command::SetAnalogGain(g) => (format!("SetAnalogGain {g}"), None),
                    Command::AddTap(t) => (format!("AddTap {}", t.id), None),
                    Command::RemoveTap(id) => (format!("RemoveTap {id}"), None),
                    Command::GrowTaps(t) => ("GrowTaps".into(), Some(Event::OldTaps(t))),
                    Command::SetMatrix(output, m) => {
                        ("SetMatrix".into(), Some(Event::OldMatrix(output, m)))
                    }
                    Command::AddOutput(index, mix, _feed) => {
                        let mix = mix.downcast::<OutputMix<F32>>().expect("an f32 mix");
                        self.outputs.push((index, mix.target_gain));
                        (format!("AddOutput {index}"), None)
                    }
                    Command::RemoveOutput(index) => (format!("RemoveOutput {index}"), None),
                    Command::SetDelay(frames) => (format!("SetDelay {frames}"), None),
                    Command::Shutdown => ("Shutdown".into(), None),
                };
                seen.push(name);
                if let Some(event) = answer {
                    assert!(self.events.push(event).is_ok());
                }
            }
            seen
        }

        fn send(&mut self, event: Event) {
            assert!(self.events.push(event).is_ok());
        }
    }

    /// A started control thread with one client said hello on connection
    /// 1, the analog input on if `opener` is given, and the fake engine.
    fn started(config: &Config, opener: Option<&Opener>) -> (Control, FakeEngine, Instant) {
        let (commands, engine_commands) = rtrb::RingBuffer::new(256);
        let (engine_events, events) = rtrb::RingBuffer::new(256);
        let audio = std::thread::current();
        let mut control = Control::new(config, 12.0, Vec::new(), commands, events, audio);
        if let Some(opener) = opener {
            control.set_analog(opener.open());
        }
        let now = Instant::now();
        control.start(now);
        let engine = FakeEngine::new(engine_commands, engine_events);
        (control, engine, now)
    }

    /// Connects a client on `conn` and has it play at `rate`.
    fn play(control: &mut Control, conn: ConnId, rate: u32, now: Instant) {
        control.connected(conn, "test".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(conn, Some(hello), now);
        let buffer = min_buffer_frames(rate, 12.0);
        let answer = control.request(conn, Some(configure(rate, buffer)), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        let start = control.request(conn, Some(Request::Start), now);
        assert_eq!(start.reply, Some(Reply::Ok));
    }

    fn has(seen: &[String], what: &str) -> bool {
        seen.iter().any(|s| s.starts_with(what))
    }

    fn analog_config() -> Config {
        Config {
            analog: config::Analog {
                enabled: true,
                volume_db: -20.0,
            },
            ..Config::default()
        }
    }

    /// Connects a client on `conn` and configures a capture stream.
    /// Returns the client's mapping of the ring.
    fn record(control: &mut Control, conn: ConnId, now: Instant) -> (Ring, u32) {
        control.connected(conn, "recorder".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(conn, Some(hello), now);
        let capture = Request::Configure {
            rate: 0,
            format: ClientFormat::S32,
            channels: 2,
            buffer_frames: 4096,
            capture: true,
        };
        let answer = control.request(conn, Some(capture), now);
        let Some(Reply::Configured { rate }) = answer.reply else {
            panic!("not configured: {:?}", answer.reply);
        };
        (Ring::map(answer.fds[0].as_fd()).unwrap(), rate)
    }

    fn ask_start(control: &mut Control, conn: ConnId, now: Instant) -> Answer {
        control.request(conn, Some(Request::Start), now)
    }

    #[test]
    fn a_recorder_is_ended_only_once_its_tap_is_back() {
        let (mut control, mut engine, now) = started(&Config::default(), None);
        engine.take(true);
        control.handle_events(now);
        let (ring, rate) = record(&mut control, 2, now);
        assert_eq!(rate, 48000);
        let mut tap = None;
        while let Ok(command) = engine.commands.pop() {
            if let Command::AddTap(t) = command {
                tap = Some(t);
            }
        }
        let tap = tap.expect("no tap added");
        let ok = Some(Reply::Ok);
        assert_eq!(ask_start(&mut control, 2, now).reply, ok);
        // A player at another rate moves the output, and the tap goes.
        play(&mut control, 1, 44100, now);
        let seen = engine.take(false);
        let removed = format!("RemoveTap {}", tap.id);
        assert!(has(&seen, &removed), "{seen:?}");
        assert!(has(&seen, "Reconfigure 44100"), "{seen:?}");
        // The audio thread may still write it, so the client is not told
        // yet. A start is refused already.
        assert_eq!(ring.header().ended.load(Ordering::Acquire), 0);
        let code = libc::ENODEV as u32;
        let refused = Some(Reply::Error { code });
        assert_eq!(ask_start(&mut control, 2, now).reply, refused);
        engine.send(Event::DroppedTap(tap));
        control.handle_events(now);
        assert_eq!(ring.header().ended.load(Ordering::Acquire), 1);
        assert_eq!(ask_start(&mut control, 2, now).reply, refused);
    }

    /// The count an eventfd holds, which a read takes. Zero if it is not
    /// readable.
    fn wake_count(wake: &OwnedFd) -> u64 {
        let mut count = 0u64;
        // SAFETY: 8 bytes from a non-blocking eventfd.
        unsafe { libc::read(wake.as_raw_fd(), (&raw mut count).cast(), 8) };
        count
    }

    #[test]
    fn a_recorder_is_woken_when_its_stream_ends() {
        let (mut control, mut engine, now) = started(&Config::default(), None);
        engine.take(true);
        control.handle_events(now);
        let (_ring, _) = record(&mut control, 2, now);
        let mut tap = None;
        while let Ok(command) = engine.commands.pop() {
            if let Command::AddTap(t) = command {
                tap = Some(t);
            }
        }
        let tap = tap.expect("no tap added");
        // The recorder's copy of the wake-up.
        let wake = tap.wake.try_clone().unwrap();
        assert_eq!(ask_start(&mut control, 2, now).reply, Some(Reply::Ok));
        play(&mut control, 1, 44100, now);
        engine.take(false);
        assert_eq!(wake_count(&wake), 0);
        // A recorder asleep in poll, with nothing more to come, has to
        // hear that the stream is over.
        engine.send(Event::DroppedTap(tap));
        control.handle_events(now);
        assert_eq!(wake_count(&wake), 1);
    }

    #[test]
    fn with_the_analog_input_on_the_output_opens_at_startup() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        let seen = engine.take(true);
        let gain = crate::format::db_to_gain(-20.0);
        assert_eq!(
            seen,
            [
                "SetGain".to_string(),
                format!("SetAnalogGain {gain}"),
                "SetChain Analog detect".into(),
                "Reconfigure 48000 256".into(),
            ]
        );
        // The capture opens once the output is open, not before.
        assert!(opener.asked().is_empty());
        control.handle_events(now);
        assert_eq!(opener.asked(), [(48000, 256)]);
        assert_eq!(engine.take(true), ["SetCapture true"]);
        control.handle_events(now);
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn with_the_analog_input_on_the_output_stays_open_when_idle() {
        let config = Config {
            idle: Idle::Off { delay_s: 1 },
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        // A player comes and goes, and the input has signal for a while.
        play(&mut control, 1, 48000, now);
        engine.send(Event::InputSignal(true));
        control.handle_events(now);
        let stop = control.request(1, Some(Request::Stop), now);
        assert_eq!(stop.reply, Some(Reply::Ok));
        engine.send(Event::InputSignal(false));
        control.handle_events(now);
        control.tick(now + Duration::from_secs(120));
        assert!(control.output_is_open());
        assert!(!has(&engine.take(true), "CloseOutput"));
        // Without the input the same config closes it.
        let config = Config {
            idle: Idle::Off { delay_s: 1 },
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        control.tick(now + Duration::from_secs(120));
        assert!(has(&engine.take(true), "CloseOutput"));
    }

    #[test]
    fn the_capture_is_opened_after_each_reconfigure_at_its_rate() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        // The card's rates come before the first open is done: two opens
        // are under way, and the capture waits for the last.
        control.set_supported(vec![44100, 96000], now);
        let seen = engine.take(false);
        assert!(has(&seen, "Reconfigure 48000"));
        assert!(has(&seen, "Reconfigure 44100 256"));
        engine.send(Event::Reconfigured);
        control.handle_events(now);
        assert!(opener.asked().is_empty());
        engine.send(Event::Reconfigured);
        control.handle_events(now);
        assert_eq!(opener.asked(), [(44100, 256)]);
        assert_eq!(engine.take(true), ["SetCapture true"]);
        control.handle_events(now);
        // The next rate the card takes moves the output, and the capture
        // follows it. The old one comes back to be dropped here.
        control.set_supported(vec![96000], now);
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 96000 512"));
        control.handle_events(now);
        assert_eq!(opener.asked(), [(44100, 256), (96000, 512)]);
        assert_eq!(engine.take(true), ["SetCapture true"]);
    }

    #[test]
    fn an_active_analog_input_is_the_first_client() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        engine.send(Event::InputSignal(true));
        control.handle_events(now);
        // A player at 96 kHz arriving while it plays at 48 kHz is resampled.
        play(&mut control, 1, 96000, now);
        let seen = engine.take(true);
        assert!(has(&seen, "SetResampler 1 true"), "{seen:?}");
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        control.tick(now + Duration::from_secs(5));
        assert!(!has(&engine.take(true), "Reconfigure"));
        assert_eq!(opener.asked(), [(48000, 256)]);
    }

    #[test]
    fn a_silent_analog_input_does_not_hold_the_rate() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        // A player at 96 kHz arriving then sets 96 kHz.
        play(&mut control, 1, 96000, now);
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 96000 512"), "{seen:?}");
        // The resampler it was given while it was only configured goes.
        let resampler = seen.iter().rfind(|s| s.starts_with("SetResampler 1"));
        assert_eq!(resampler.map(String::as_str), Some("SetResampler 1 false"));
        // And the capture reopens at it.
        control.handle_events(now);
        assert_eq!(opener.asked(), [(48000, 256), (96000, 512)]);
        assert_eq!(engine.take(true), ["SetCapture true"]);
    }

    #[test]
    fn the_analog_input_going_quiet_lets_the_remaining_player_take_the_rate() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        engine.send(Event::InputSignal(true));
        control.handle_events(now);
        play(&mut control, 1, 96000, now);
        engine.take(true);
        engine.send(Event::InputSignal(false));
        control.handle_events(now);
        // The rate settles first, as for a player that stops.
        let ms = Duration::from_millis;
        control.tick(now + ms(400));
        assert!(!has(&engine.take(true), "Reconfigure"));
        control.tick(now + ms(600));
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 96000 512"), "{seen:?}");
        // The player needs no resampler at its own rate.
        assert!(has(&seen, "SetResampler 1 false"), "{seen:?}");
        control.handle_events(now + ms(600));
        assert_eq!(opener.asked(), [(48000, 256), (96000, 512)]);
        // Signal again: the input runs at the output's rate, 96 kHz now.
        // When the player stops, the input holds that rate.
        engine.take(true);
        engine.send(Event::InputSignal(true));
        control.handle_events(now + ms(700));
        let stop = control.request(1, Some(Request::Stop), now + ms(700));
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(now + ms(5000));
        assert!(!has(&engine.take(true), "Reconfigure"));
        assert_eq!(control.policy.current(), 96000);
    }

    /// The analog input's rate in the rules, while it has signal.
    /// The analog input's rate in the rules, while it is a client.
    fn analog_rate(control: &Control) -> Option<u32> {
        control.policy.client_rate(ANALOG)
    }

    /// The capture is silent for `SETTLE_MS` after each start and the
    /// detector needs a window, so on a board the signal is back no sooner
    /// than this after a reopen.
    const SIGNAL_BACK: Duration = Duration::from_millis(1050);

    #[test]
    fn after_dop_a_lone_analog_input_leaves_the_card_at_the_dop_rate() {
        let ms = Duration::from_millis;
        // DoP stops before the signal is back, and well after.
        for held in [ms(300), ms(5000)] {
            let opener = Opener::default();
            let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
            engine.take(true);
            control.handle_events(now);
            engine.take(true);
            engine.send(Event::InputSignal(true));
            control.handle_events(now);
            assert_eq!(analog_rate(&control), Some(48000));
            // A player at 176.4 kHz is resampled to the input's 48 kHz,
            // until it plays DoP, which takes the output at its rate.
            play(&mut control, 1, 176400, now);
            assert!(!has(&engine.take(true), "Reconfigure"));
            engine.send(Event::DopStarted(1, 1));
            control.handle_events(now);
            let seen = engine.take(false);
            assert!(has(&seen, "Reconfigure 176400 1024"), "{seen:?}");
            // While DoP owns the output the input is not a client.
            assert_eq!(analog_rate(&control), None);
            // The reopen closes the capture and the new detector has no
            // hold, so the input goes quiet. The capture opens again at the
            // rate the output runs at.
            engine.send(Event::InputSignal(false));
            engine.send(Event::Reconfigured);
            control.handle_events(now + ms(10));
            assert_eq!(opener.asked(), [(48000, 256), (176400, 1024)]);
            assert_eq!(engine.take(true), ["SetCapture true"]);
            // DoP ends: the player stops.
            let stop = control.request(1, Some(Request::Stop), now + held);
            assert_eq!(stop.reply, Some(Reply::Ok));
            control.tick(now + held);
            // The signal is back once the capture's mute is over.
            let back = now + ms(10) + SIGNAL_BACK;
            control.tick(back);
            engine.send(Event::InputSignal(true));
            control.handle_events(back.max(now + held));
            assert_eq!(analog_rate(&control), Some(176400), "{held:?}");
            // Nothing returns the card to 48 kHz: it keeps its rate, as it
            // does when idle, and the input runs at it.
            control.tick(now + held + ms(10000));
            let seen = engine.take(true);
            assert!(!has(&seen, "Reconfigure"), "{held:?}: {seen:?}");
            assert_eq!(control.policy.current(), 176400, "{held:?}");
            assert_eq!(opener.asked().len(), 2, "{held:?}");
        }
    }

    /// A 48 kHz player and the analog input with signal; DoP at 176.4 kHz
    /// takes the output and the input's signal is back after its mute. The
    /// player leaves during DoP, and DoP ends `gap_ms` later. Returns the
    /// rate the output is left at.
    fn player_leaves_during_dop(gap_ms: u64) -> u32 {
        let (control, _) = player_leaves_during_dop_with(gap_ms);
        control.output.unwrap().0
    }

    fn player_leaves_during_dop_with(gap_ms: u64) -> (Control, FakeEngine) {
        let ms = Duration::from_millis;
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 48000, now);
        engine.send(Event::InputSignal(true));
        control.handle_events(now + ms(10));
        engine.take(true);
        play(&mut control, 2, 176400, now + ms(20));
        engine.send(Event::DopStarted(2, 1));
        control.handle_events(now + ms(20));
        let seen = engine.take(false);
        assert!(has(&seen, "Reconfigure 176400"), "{seen:?}");
        engine.send(Event::InputSignal(false));
        engine.send(Event::Reconfigured);
        control.handle_events(now + ms(30));
        engine.take(true);
        let back = now + ms(30) + SIGNAL_BACK;
        control.tick(back);
        engine.send(Event::InputSignal(true));
        control.handle_events(back);
        engine.take(true);
        let t1 = back + ms(1000);
        let stop = control.request(1, Some(Request::Stop), t1);
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(t1);
        let t2 = t1 + ms(gap_ms);
        control.tick(t2);
        let stop = control.request(2, Some(Request::Stop), t2);
        assert_eq!(stop.reply, Some(Reply::Ok));
        for k in 0..40 {
            control.tick(t2 + ms(100 * k));
            engine.take(true);
            control.handle_events(t2 + ms(100 * k));
        }
        (control, engine)
    }

    #[test]
    fn after_dop_the_card_keeps_its_rate_for_the_input_whatever_the_timing() {
        assert_eq!(player_leaves_during_dop(200), 176400);
        assert_eq!(player_leaves_during_dop(800), 176400);
    }

    /// A control thread whose analog input has signal before any player.
    fn input_first() -> (Control, FakeEngine, Instant) {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        engine.send(Event::InputSignal(true));
        control.handle_events(now + Duration::from_millis(10));
        engine.take(true);
        (control, engine, now)
    }

    /// Ticks for `secs` seconds from `from`, and returns what was sent.
    fn run_for(
        control: &mut Control,
        engine: &mut FakeEngine,
        from: Instant,
        secs: u64,
    ) -> Vec<String> {
        let mut seen = engine.take(true);
        for k in 0..=10 * secs {
            let at = from + Duration::from_millis(100 * k);
            control.tick(at);
            control.handle_events(at);
            seen.extend(engine.take(true));
        }
        seen
    }

    #[test]
    fn s2_two_players_after_the_input_never_reopen_the_output_under_it() {
        let ms = Duration::from_millis;
        let (mut control, mut engine, now) = input_first();
        play(&mut control, 1, 44100, now + ms(20));
        play(&mut control, 2, 44100, now + ms(30));
        let stop = control.request(1, Some(Request::Stop), now + ms(2000));
        assert_eq!(stop.reply, Some(Reply::Ok));
        let seen = run_for(&mut control, &mut engine, now + ms(2000), 3);
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        assert!(control.policy.needs_resampling(2));
    }

    #[test]
    fn s3_signal_back_within_the_settle_time_cancels_the_switch() {
        let ms = Duration::from_millis;
        let (mut control, mut engine, now) = input_first();
        play(&mut control, 1, 44100, now + ms(20));
        assert!(!has(&engine.take(true), "Reconfigure"));
        // A quiet passage closes the detector; signal is back 300 ms later.
        engine.send(Event::InputSignal(false));
        control.handle_events(now + ms(5000));
        engine.send(Event::InputSignal(true));
        control.handle_events(now + ms(5300));
        let seen = run_for(&mut control, &mut engine, now + ms(5300), 3);
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        assert_eq!(analog_rate(&control), Some(48000));
    }

    #[test]
    fn s4_a_quiet_input_lets_the_player_take_the_rate_after_the_settle() {
        let ms = Duration::from_millis;
        let (mut control, mut engine, now) = input_first();
        play(&mut control, 1, 44100, now + ms(20));
        engine.take(true);
        engine.send(Event::InputSignal(false));
        control.handle_events(now + ms(5000));
        control.tick(now + ms(5400));
        assert!(!has(&engine.take(true), "Reconfigure"));
        control.tick(now + ms(5500));
        assert!(has(&engine.take(true), "Reconfigure 44100"));
        assert!(!control.policy.needs_resampling(1));
    }

    #[test]
    fn s5_an_input_after_a_player_holds_the_rate_it_joined_at() {
        let ms = Duration::from_millis;
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 48000, now);
        engine.send(Event::InputSignal(true));
        control.handle_events(now + ms(10));
        play(&mut control, 2, 44100, now + ms(20));
        let stop = control.request(1, Some(Request::Stop), now + ms(1000));
        assert_eq!(stop.reply, Some(Reply::Ok));
        let seen = run_for(&mut control, &mut engine, now + ms(1000), 3);
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        assert_eq!(analog_rate(&control), Some(48000));
    }

    #[test]
    fn s1_after_dop_the_input_joins_at_the_rate_the_card_keeps() {
        // The output part is `after_dop_the_card_keeps_its_rate_...`.
        for gap in [200, 800] {
            let (control, _) = player_leaves_during_dop_with(gap);
            assert_eq!(analog_rate(&control), Some(176400), "{gap}");
        }
    }

    #[test]
    fn s6_after_dop_as_the_first_client_the_input_joins_at_the_rate_kept() {
        let ms = Duration::from_millis;
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 176400, now);
        engine.send(Event::DopStarted(1, 1));
        control.handle_events(now);
        assert!(has(&engine.take(true), "Reconfigure 176400"));
        control.handle_events(now);
        engine.take(true);
        // The input gets signal while DoP owns the output: not a client yet.
        engine.send(Event::InputSignal(true));
        control.handle_events(now + SIGNAL_BACK);
        assert_eq!(analog_rate(&control), None);
        let stop = control.request(1, Some(Request::Stop), now + ms(3000));
        assert_eq!(stop.reply, Some(Reply::Ok));
        let seen = run_for(&mut control, &mut engine, now + ms(3000), 3);
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        assert_eq!(analog_rate(&control), Some(176400));
    }

    /// A 48 kHz player, a DoP stream at 176.4 kHz owning the output, and
    /// the input with signal. The player stops, and the DoP stream turns to
    /// PCM `x` ms later and plays on. Returns what was sent after that, the
    /// output's rate, and whether the stream is resampled.
    fn dop_turns_to_pcm_after_the_player_left(x: u64) -> (Vec<String>, u32, bool) {
        let ms = Duration::from_millis;
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 48000, now);
        play(&mut control, 2, 176400, now);
        engine.take(true);
        engine.send(Event::DopStarted(2, 1));
        control.handle_events(now);
        engine.take(false);
        engine.send(Event::InputSignal(false));
        engine.send(Event::Reconfigured);
        control.handle_events(now + ms(10));
        engine.take(true);
        engine.send(Event::InputSignal(true));
        control.handle_events(now + ms(10) + SIGNAL_BACK);
        engine.take(true);
        let t = now + ms(3000);
        let stop = control.request(1, Some(Request::Stop), t);
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(t);
        engine.take(true);
        control.tick(t + ms(x));
        engine.send(Event::DopStopped(2));
        control.handle_events(t + ms(x));
        let seen = run_for(&mut control, &mut engine, t + ms(x), 3);
        let rate = control.output.unwrap().0;
        (seen, rate, control.policy.needs_resampling(2))
    }

    #[test]
    fn the_input_joins_at_the_rate_the_card_runs_at_whatever_the_timing() {
        for x in [200, 800] {
            let (seen, rate, resampled) = dop_turns_to_pcm_after_the_player_left(x);
            assert!(!has(&seen, "Reconfigure"), "{x}: {seen:?}");
            assert_eq!(rate, 176400, "{x}");
            assert!(!resampled, "{x}");
        }
    }

    #[test]
    fn a_dop_stream_that_is_the_first_client_leaves_the_analog_input_at_its_rate() {
        let ms = Duration::from_millis;
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 176400, now);
        engine.send(Event::DopStarted(1, 1));
        control.handle_events(now);
        assert!(has(&engine.take(true), "Reconfigure 176400 1024"));
        control.handle_events(now);
        engine.take(true);
        let back = now + SIGNAL_BACK;
        engine.send(Event::InputSignal(true));
        control.handle_events(back);
        assert_eq!(analog_rate(&control), None);
        let stop = control.request(1, Some(Request::Stop), back + ms(1000));
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(back + ms(10000));
        assert!(!has(&engine.take(true), "Reconfigure"));
        assert_eq!(control.policy.current(), 176400);
    }

    #[test]
    fn an_analog_input_with_signal_during_dop_follows_the_output_back() {
        let ms = Duration::from_millis;
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        // A player at 48 kHz holds the rate, another at 176.4 kHz plays DoP.
        play(&mut control, 1, 48000, now);
        play(&mut control, 2, 176400, now);
        engine.send(Event::DopStarted(2, 1));
        control.handle_events(now);
        assert!(has(&engine.take(true), "Reconfigure 176400 1024"));
        control.handle_events(now);
        engine.take(true);
        // The input gets signal while DoP holds the output: it is not a
        // client until DoP ends.
        let back = now + SIGNAL_BACK;
        engine.send(Event::InputSignal(true));
        control.handle_events(back);
        assert_eq!(analog_rate(&control), None);
        // DoP stops: the input joins at the rate the card runs at, and the
        // 48 kHz player, the first client, has the output back after the
        // settle time, as when DoP ends without the input.
        let stop = control.request(2, Some(Request::Stop), back + ms(100));
        assert_eq!(stop.reply, Some(Reply::Ok));
        assert_eq!(analog_rate(&control), Some(176400));
        control.tick(back + ms(700));
        assert!(has(&engine.take(true), "Reconfigure 48000 256"));
        // That reopen mutes the capture: the input goes quiet and leaves,
        // and joins again at 48 kHz once its signal is back.
        engine.send(Event::InputSignal(false));
        engine.send(Event::Reconfigured);
        control.handle_events(back + ms(710));
        assert_eq!(analog_rate(&control), None);
        let again = back + ms(710) + SIGNAL_BACK;
        engine.send(Event::InputSignal(true));
        control.handle_events(again);
        assert_eq!(analog_rate(&control), Some(48000));
        engine.take(true);
        // The player leaves. The input holds the rate it plays at.
        let stop = control.request(1, Some(Request::Stop), again + ms(100));
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(again + ms(10000));
        assert!(!has(&engine.take(true), "Reconfigure"));
        assert_eq!(control.policy.current(), 48000);
    }

    #[test]
    fn a_failed_capture_is_closed_here() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        let (capture, feed) = MemoryCapture::new();
        engine.send(Event::CaptureFailed(Box::new(capture)));
        control.handle_events(now);
        assert!(feed.closed());
    }

    #[test]
    fn a_late_card_without_an_adc_ends_the_analog_input_and_idle_applies_again() {
        let config = Config {
            idle: Idle::Off { delay_s: 1 },
            ..analog_config()
        };
        let (commands, engine_commands) = rtrb::RingBuffer::new(256);
        let (engine_events, events) = rtrb::RingBuffer::new(256);
        let audio = std::thread::current();
        let mut control = Control::new(&config, 12.0, Vec::new(), commands, events, audio);
        let asked = Arc::new(Mutex::new(0u32));
        let counted = asked.clone();
        control.set_analog(Box::new(move |_, _| {
            *counted.lock().unwrap() += 1;
            Err(crate::capture::OpenError::Never(
                "the card has no ADC".into(),
            ))
        }));
        let now = Instant::now();
        control.start(now);
        let mut engine = FakeEngine::new(engine_commands, engine_events);
        engine.take(true);
        control.handle_events(now);
        assert_eq!(*asked.lock().unwrap(), 1);
        // Never tried again, and the output closes when idle once more.
        control.tick(now + Duration::from_secs(5));
        assert_eq!(*asked.lock().unwrap(), 1);
        assert!(has(&engine.take(true), "CloseOutput"));
    }

    #[test]
    fn a_failed_capture_is_retried_every_second() {
        let opener = Opener::default();
        opener.fail(2);
        let (mut control, mut engine, now) = started(&analog_config(), Some(&opener));
        engine.take(true);
        let ms = Duration::from_millis;
        // The open after the output opens fails, and so does the next.
        control.handle_events(now);
        assert_eq!(opener.asked().len(), 1);
        assert!(engine.take(true).is_empty());
        control.tick(now + ms(900));
        assert_eq!(opener.asked().len(), 1);
        control.tick(now + ms(1000));
        assert_eq!(opener.asked().len(), 2);
        assert!(engine.take(true).is_empty());
        control.tick(now + ms(1500));
        assert_eq!(opener.asked().len(), 2);
        control.tick(now + ms(2000));
        assert_eq!(opener.asked().len(), 3);
        assert_eq!(engine.take(true), ["SetCapture true"]);
        control.handle_events(now + ms(2000));
        // It fails while it runs. The engine has closed it.
        engine.send(Event::CaptureFailed(Box::new(MemoryCapture::new().0)));
        control.handle_events(now + ms(3000));
        assert_eq!(opener.asked().len(), 3);
        control.tick(now + ms(3900));
        assert_eq!(opener.asked().len(), 3);
        control.tick(now + ms(4000));
        assert_eq!(opener.asked().len(), 4);
        // A failure that comes before the answer to the new capture is the
        // old one's. The new one is kept, and not opened again.
        engine.send(Event::CaptureFailed(Box::new(MemoryCapture::new().0)));
        control.handle_events(now + ms(4000));
        assert_eq!(engine.take(true), ["SetCapture true"]);
        control.handle_events(now + ms(4000));
        control.tick(now + ms(10000));
        assert_eq!(opener.asked().len(), 4);
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn chains_are_rebuilt_at_the_new_rate() {
        let mix = config::PluginConfig {
            place: config::PluginPlace::Mix,
            kind: "balance".into(),
            path: None,
            params: Default::default(),
        };
        let config = Config {
            idle: Idle::Off { delay_s: 1 },
            plugins: vec![mix],
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        // Without the analog input only the places with plugins get one.
        assert_eq!(
            engine.take(true),
            ["SetGain", "SetChain Mix balance", "Reconfigure 48000 256"]
        );
        control.handle_events(now);
        play(&mut control, 1, 96000, now);
        let seen = engine.take(true);
        let at = |what: &str| seen.iter().position(|s| s.starts_with(what));
        let (chain, reopen) = (at("SetChain Mix"), at("Reconfigure 96000"));
        assert!(chain.is_some() && chain < reopen, "{seen:?}");
        control.handle_events(now);
        // An idle close and a reopen at the same rate keep the chains, so
        // their filters keep their state.
        let stop = control.request(1, Some(Request::Stop), now);
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(now + Duration::from_secs(2));
        assert!(has(&engine.take(true), "CloseOutput"));
        let start = control.request(1, Some(Request::Start), now + Duration::from_secs(3));
        assert_eq!(start.reply, Some(Reply::Ok));
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 96000"), "{seen:?}");
        assert!(!has(&seen, "SetChain"), "{seen:?}");
        // Without plugins no chain is ever sent.
        let (mut control, mut engine, now) = started(&Config::default(), None);
        control.handle_events(now);
        play(&mut control, 1, 96000, now);
        assert!(!has(&engine.take(true), "SetChain"));
    }

    fn entry(place: config::PluginPlace, kind: &str) -> config::PluginConfig {
        config::PluginConfig {
            place,
            kind: kind.into(),
            path: None,
            params: Default::default(),
        }
    }

    /// What the engine was sent, without the volume.
    fn sent(engine: &mut FakeEngine) -> Vec<String> {
        let seen = engine.take(true);
        seen.into_iter().filter(|s| s != "SetGain").collect()
    }

    #[test]
    fn a_plugin_that_fails_at_a_rate_does_not_ban_it_and_is_tried_once_per_decision() {
        use crate::plugin::builtin::FAILS_TRIED;
        let tried = || FAILS_TRIED.with(|n| n.get());
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Mix, "fails-above-48k")],
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        control.set_supported(vec![48000, 96000], now);
        engine.take(true);
        control.handle_events(now);
        let before = tried();
        play(&mut control, 1, 96000, now);
        // The output keeps what it has, and the player follows it.
        let seen = sent(&mut engine);
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        assert!(has(&seen, "SetResampler 1 true"), "{seen:?}");
        assert!(!control.refused.contains(&96000));
        assert_eq!(tried() - before, 1);
        // Ticks are no rate decision: no new attempt.
        for k in 1..20 {
            control.tick(now + Duration::from_millis(100 * k));
        }
        assert_eq!(tried() - before, 1);
        // A change of clients is: one more attempt.
        play(&mut control, 2, 96000, now + Duration::from_secs(3));
        assert_eq!(tried() - before, 2);
        assert!(!has(&sent(&mut engine), "Reconfigure"));
    }

    #[test]
    fn a_failing_plugin_never_keeps_the_output_closed_under_a_player() {
        use crate::plugin::builtin::FAILS_TRIED;
        let tried = || FAILS_TRIED.with(|n| n.get());
        let s = Duration::from_secs;
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Mix, "fails-above-48k")],
            idle: Idle::Off { delay_s: 1 },
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        control.set_supported(vec![48000, 96000], now);
        engine.take(true);
        control.handle_events(now);
        control.tick(now + s(5));
        assert!(has(&sent(&mut engine), "CloseOutput"));
        // A 96 kHz player while the output is closed: the build at 96 kHz
        // fails, and for this decision the output opens at the nearest
        // rate that builds, the player resampled to it.
        play(&mut control, 1, 96000, now + s(6));
        let seen = sent(&mut engine);
        assert!(has(&seen, "Reconfigure 48000"), "{seen:?}");
        assert!(has(&seen, "SetResampler 1 true"), "{seen:?}");
        // 96 kHz is not refused, and is not tried again on ticks.
        assert!(!control.refused.contains(&96000));
        let before = tried();
        for k in 1..50 {
            control.tick(now + s(6) + Duration::from_millis(100 * k));
        }
        assert_eq!(tried(), before);
        assert!(!has(&sent(&mut engine), "Reconfigure"));
        // At the next decision 96 kHz is tried again, and the output stays.
        play(&mut control, 2, 48000, now + s(12));
        assert!(tried() > before);
        assert!(!has(&sent(&mut engine), "Reconfigure"));
        assert_eq!(control.output.unwrap().0, 48000);
    }

    #[test]
    fn a_rate_a_chain_cannot_be_built_at_is_not_used() {
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Mix, "only-up-to-48k")],
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        control.set_supported(vec![44100, 48000, 96000], now);
        engine.take(true);
        control.handle_events(now);
        // A player at 96 kHz: the nearest rate the chain takes is 48 kHz,
        // and the player is resampled to it.
        play(&mut control, 1, 96000, now);
        let seen = sent(&mut engine);
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        assert!(has(&seen, "SetResampler 1 true"), "{seen:?}");
        control.tick(now + Duration::from_secs(5));
        assert!(!has(&engine.take(true), "Reconfigure"));
        assert_eq!(control.policy.current(), 48000);
    }

    #[test]
    fn with_any_rate_the_card_takes_a_refused_one_moves_to_the_nearest_that_works() {
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Output, "only-up-to-48k")],
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        engine.take(true);
        control.handle_events(now);
        play(&mut control, 1, 96000, now);
        let seen = sent(&mut engine);
        // 96, 88.2 and 64 kHz are refused in turn, and nothing is sent for
        // them: the output stays at 48 kHz, with its chain.
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        assert!(!has(&seen, "SetChain"), "{seen:?}");
        assert!(has(&seen, "SetResampler 1 true"), "{seen:?}");
        assert_eq!(control.policy.current(), 48000);
    }

    #[test]
    fn a_chain_is_never_sent_short_of_its_plugins() {
        // A parameter the plugin does not have fails at every rate.
        let mut nope = entry(config::PluginPlace::Mix, "balance");
        nope.params
            .insert("nope".into(), config::ParamValue::Number(1.0));
        let config = Config {
            plugins: vec![nope, entry(config::PluginPlace::Output, "balance")],
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        // No rate works, so the output is not opened at all.
        let seen = sent(&mut engine);
        assert!(!has(&seen, "SetChain"), "{seen:?}");
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        play(&mut control, 1, 44100, now);
        let seen = sent(&mut engine);
        assert!(
            !has(&seen, "SetChain") && !has(&seen, "Reconfigure"),
            "{seen:?}"
        );
    }

    #[test]
    fn the_analog_chain_has_no_fallback_either() {
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Analog, "only-up-to-48k")],
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        control.set_supported(vec![48000, 96000], now);
        let seen = sent(&mut engine);
        assert!(has(&seen, "SetChain Analog balance,detect"), "{seen:?}");
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 96000, now);
        let seen = sent(&mut engine);
        assert!(
            !has(&seen, "Reconfigure") && !has(&seen, "SetChain"),
            "{seen:?}"
        );
    }

    #[test]
    fn dop_rebuilds_only_the_analog_chain_and_the_others_keep_their_state() {
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Mix, "balance")],
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 48000, now);
        play(&mut control, 2, 176400, now);
        engine.take(true);
        engine.send(Event::DopStarted(2, 1));
        control.handle_events(now);
        let seen = sent(&mut engine);
        assert!(has(&seen, "Reconfigure 176400"), "{seen:?}");
        assert!(has(&seen, "SetChain Analog detect"), "{seen:?}");
        assert!(!has(&seen, "SetChain Mix"), "{seen:?}");
        control.handle_events(now);
        engine.take(true);
        // DoP stops, and the 48 kHz player has the output back: the mix
        // chain is the one it had, and the analog chain is built again.
        let stop = control.request(2, Some(Request::Stop), now);
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(now + Duration::from_secs(1));
        let seen = sent(&mut engine);
        assert!(has(&seen, "Reconfigure 48000"), "{seen:?}");
        assert!(has(&seen, "SetChain Analog detect"), "{seen:?}");
        assert!(!has(&seen, "SetChain Mix"), "{seen:?}");
    }

    #[test]
    fn when_dop_stops_the_output_and_its_chains_follow_at_once() {
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Mix, "balance")],
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 48000, now);
        play(&mut control, 2, 176400, now);
        engine.take(true);
        engine.send(Event::DopStarted(2, 1));
        control.handle_events(now);
        assert!(has(&sent(&mut engine), "Reconfigure 176400"));
        control.handle_events(now);
        engine.take(true);
        // The DoP stream turns to PCM: the 48 kHz player has the output
        // back at once, not after the settle time.
        engine.send(Event::DopStopped(2));
        control.handle_events(now + Duration::from_secs(1));
        let seen = sent(&mut engine);
        assert!(has(&seen, "Reconfigure 48000"), "{seen:?}");
        // And with only the DoP stream's player left, the card keeps the
        // DoP rate and gets the mix chain for it at once.
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Mix, "balance")],
            ..analog_config()
        };
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 48000, now);
        play(&mut control, 2, 176400, now);
        engine.send(Event::DopStarted(2, 1));
        control.handle_events(now);
        control.handle_events(now);
        let stop = control.request(1, Some(Request::Stop), now);
        assert_eq!(stop.reply, Some(Reply::Ok));
        control.tick(now + Duration::from_secs(1));
        engine.take(true);
        engine.send(Event::DopStopped(2));
        control.handle_events(now + Duration::from_secs(2));
        let seen = sent(&mut engine);
        assert!(has(&seen, "SetChain Mix balance"), "{seen:?}");
    }

    #[test]
    fn dop_at_a_rate_the_analog_chain_refuses_does_not_take_the_output() {
        let config = Config {
            plugins: vec![
                entry(config::PluginPlace::Analog, "only-up-to-48k"),
                entry(config::PluginPlace::Mix, "balance"),
            ],
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        control.set_supported(vec![48000, 176400], now);
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        play(&mut control, 1, 176400, now);
        engine.send(Event::DopStarted(1, 1));
        control.handle_events(now);
        let seen = sent(&mut engine);
        assert!(!has(&seen, "Reconfigure 176400"), "{seen:?}");
        assert_eq!(control.policy.current(), 48000);
    }

    #[test]
    fn a_rate_only_the_mix_chain_refuses_still_carries_dop() {
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Mix, "only-up-to-48k")],
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        control.set_supported(vec![48000, 176400], now);
        engine.take(true);
        control.handle_events(now);
        // A PCM player at 176.4 kHz: the mix chain refuses the rate.
        play(&mut control, 1, 176400, now);
        engine.take(true);
        assert_eq!(control.policy.current(), 48000);
        let stop = control.request(1, Some(Request::Stop), now);
        assert_eq!(stop.reply, Some(Reply::Ok));
        engine.take(true);
        // A DoP stream at that rate bypasses the mix chain, so it has it.
        play(&mut control, 2, 176400, now);
        engine.send(Event::DopStarted(2, 1));
        control.handle_events(now);
        let seen = sent(&mut engine);
        assert!(has(&seen, "Reconfigure 176400"), "{seen:?}");
        assert!(!has(&seen, "SetChain Mix"), "{seen:?}");
    }

    #[test]
    fn a_first_open_for_dop_sends_the_mix_and_output_chains_too() {
        // The card takes only 176.4 kHz, where the output chain refuses PCM,
        // so the output has never opened when the DoP stream claims it.
        let config = Config {
            plugins: vec![
                entry(config::PluginPlace::Mix, "balance"),
                entry(config::PluginPlace::Output, "only-up-to-48k"),
            ],
            ..Config::default()
        };
        let (commands, engine_commands) = rtrb::RingBuffer::new(256);
        let (engine_events, events) = rtrb::RingBuffer::new(256);
        let audio = std::thread::current();
        let mut control = Control::new(&config, 12.0, Vec::new(), commands, events, audio);
        let now = Instant::now();
        control.set_supported(vec![176400], now);
        control.start(now);
        let mut engine = FakeEngine::new(engine_commands, engine_events);
        play(&mut control, 1, 176400, now);
        let seen = sent(&mut engine);
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        engine.send(Event::DopStarted(1, 1));
        control.handle_events(now);
        let seen = sent(&mut engine);
        // The chains go before the open, so that PCM at that rate never
        // plays without them: the mix chain built for it, and in place of
        // the output chain, which refuses it, one built for another rate,
        // which the audio thread does not run at this one. It plays
        // silence there instead of a bypass.
        let open = seen
            .iter()
            .position(|s| s.starts_with("Reconfigure 176400"));
        let mix = seen.iter().position(|s| s == "SetChain Mix balance");
        let output = seen.iter().position(|s| s == "SetChain Output balance");
        assert!(open.is_some() && mix < open && output < open, "{seen:?}");
        let built = |place| control.built[slot(place)];
        assert_eq!(built(config::PluginPlace::Mix), Some(176400));
        assert_eq!(built(config::PluginPlace::Output), Some(48000));
        // When the stream turns to PCM, the mix chain is not built again.
        control.handle_events(now);
        engine.take(true);
        engine.send(Event::DopStopped(1));
        control.handle_events(now);
        let seen = sent(&mut engine);
        assert!(!has(&seen, "SetChain Mix"), "{seen:?}");
    }

    #[test]
    fn the_card_kept_at_a_dop_rate_gets_chains_for_it_when_pcm_plays_there() {
        let ms = Duration::from_millis;
        let config = Config {
            plugins: vec![entry(config::PluginPlace::Mix, "balance")],
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, mut engine, now) = started(&config, Some(&opener));
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        // The mix chain is built for a player at 48 kHz, then DoP takes
        // the output to its rate.
        play(&mut control, 1, 48000, now);
        play(&mut control, 2, 176400, now);
        engine.send(Event::DopStarted(2, 1));
        control.handle_events(now);
        let seen = sent(&mut engine);
        assert!(has(&seen, "Reconfigure 176400"), "{seen:?}");
        assert!(!has(&seen, "SetChain Mix"), "{seen:?}");
        control.handle_events(now);
        engine.take(true);
        // DoP ends with no client left: the card keeps its rate, and the
        // analog input, back after its mute, plays there through the mix.
        let stop = control.request(1, Some(Request::Stop), now + ms(50));
        assert_eq!(stop.reply, Some(Reply::Ok));
        let stop = control.request(2, Some(Request::Stop), now + ms(100));
        assert_eq!(stop.reply, Some(Reply::Ok));
        engine.send(Event::InputSignal(true));
        control.handle_events(now + SIGNAL_BACK);
        control.tick(now + SIGNAL_BACK);
        let seen = sent(&mut engine);
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        assert!(has(&seen, "SetChain Mix balance"), "{seen:?}");
    }

    #[test]
    fn the_chains_built_at_startup_are_sent_at_the_first_open() {
        let mix = config::PluginConfig {
            place: config::PluginPlace::Mix,
            kind: "balance".into(),
            path: None,
            params: Default::default(),
        };
        let config = Config {
            plugins: vec![mix],
            ..Config::default()
        };
        let (commands, engine_commands) = rtrb::RingBuffer::new(256);
        let (engine_events, events) = rtrb::RingBuffer::new(256);
        let audio = std::thread::current();
        let mut control = Control::new(&config, 12.0, Vec::new(), commands, events, audio);
        let mut libraries = Libraries::default();
        let mut chains =
            Chains::build(&config.plugins, 48000, Format::F32, 2, &mut libraries).unwrap();
        // A chain built ahead stands out by what it holds.
        chains.mix = Chain::empty();
        control.set_plugins(libraries, Some(chains));
        control.start(Instant::now());
        let mut engine = FakeEngine::new(engine_commands, engine_events);
        assert_eq!(
            engine.take(true),
            ["SetGain", "SetChain Mix ", "Reconfigure 48000 256"]
        );
    }

    #[test]
    fn a_ring_holds_one_period_of_input_at_any_output_rate() {
        // Every rate a card is asked about. A ring of the least size never
        // makes `sync_resamplers` log that it is too small to resample from.
        let rates = [
            8000, 11025, 16000, 22050, 32000, 44100, 48000, 64000, 88200, 96000, 176400, 192000,
            352800, 384000,
        ];
        for latency_ms in [4.0, 12.0, 100.0] {
            for from in rates {
                let ring = min_ring_frames(from, latency_ms) as usize;
                for to in rates.into_iter().filter(|&to| to != from) {
                    let period = period_frames(to, latency_ms);
                    let r = Resampler::new(from, to, period, Quality::High, Format::F32);
                    let need = r.input_frames_max();
                    assert!(
                        need <= ring,
                        "{from} to {to} at {latency_ms} ms: {need} > {ring}"
                    );
                }
            }
        }
    }

    /// What a further output's writer was told, as the control thread
    /// tells it, and the events it has for the control thread.
    #[derive(Clone, Default)]
    struct WriterLog {
        calls: Arc<Mutex<Vec<String>>>,
        /// The target and the feed of each `reconfigure`, and the delay of
        /// the resampler it brought.
        paths: Arc<Mutex<Vec<(f64, usize, f64)>>>,
        events: Arc<Mutex<VecDeque<FurtherEvent>>>,
    }

    impl WriterLog {
        /// The calls since the last time.
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }

        fn last_path(&self) -> (f64, usize, f64) {
            *self.paths.lock().unwrap().last().expect("no reconfigure")
        }
    }

    struct FakeWriter(WriterLog);

    impl FurtherWriter for FakeWriter {
        fn reconfigure(
            &self,
            master_rate: u32,
            card_rate: u32,
            period: u32,
            resampler: Box<Adaptive>,
            target: f64,
            feed_frames: usize,
        ) {
            let call = format!("reconfigure {master_rate} {card_rate} {period}");
            self.0.calls.lock().unwrap().push(call);
            let path = (target, feed_frames, resampler.delay_frames());
            self.0.paths.lock().unwrap().push(path);
        }

        fn set_open(&self, open: bool) {
            self.0.calls.lock().unwrap().push(format!("open {open}"));
        }

        fn event(&self) -> Option<FurtherEvent> {
            self.0.events.lock().unwrap().pop_front()
        }
    }

    /// A stereo `[[output]]` on a card that takes `rates`, any for none.
    fn further(rates: &[u32]) -> (OutputConfig, Vec<u32>) {
        let config = OutputConfig {
            card: "Loopback".into(),
            channels: 2,
            latency_ms: None,
            idle: Idle::Zeros,
            volume_db: 0.0,
            plugins: Vec::new(),
            matrix: None,
        };
        (config, rates.to_vec())
    }

    /// A started control thread with the further outputs given, its fake
    /// engine and each writer's log.
    fn started_with(
        config: &Config,
        outputs: Vec<(OutputConfig, Vec<u32>)>,
    ) -> (Control, FakeEngine, Vec<WriterLog>, Instant) {
        let (commands, engine_commands) = rtrb::RingBuffer::new(256);
        let (engine_events, events) = rtrb::RingBuffer::new(256);
        let audio = std::thread::current();
        let mut control = Control::new(config, 12.0, Vec::new(), commands, events, audio);
        let latencies: Vec<f32> = outputs
            .iter()
            .map(|(o, _)| o.latency_ms.unwrap_or(12.0))
            .collect();
        let capacity = feed_capacity(12.0, &latencies, config.resampling);
        let mut logs = Vec::new();
        for (i, (output, rates)) in outputs.into_iter().enumerate() {
            let log = WriterLog::default();
            let (feed, _probe) = FeedProbe::new(output.channels, capacity);
            control.add_output(FurtherSetup {
                index: i + 1,
                matrix: Matrix::default_for(output.channels),
                config: output,
                rates,
                writer: Box::new(FakeWriter(log.clone())),
                feed,
                feed_frames: capacity,
            });
            logs.push(log);
        }
        let now = Instant::now();
        control.start(now);
        let engine = FakeEngine::new(engine_commands, engine_events);
        (control, engine, logs, now)
    }

    /// The delay a resampler from `from` to `to` brings, in frames at `to`.
    fn resampler_delay(from: u32, to: u32, period: u32) -> f64 {
        Adaptive::new(from, to, 2, period, Quality::High, Format::F32).delay_frames()
    }

    #[test]
    fn the_target_is_the_longest_path_with_its_margin() {
        // A master period, three card periods, the resampler and a margin
        // of half a master period.
        assert_eq!(align_frames(48000, 256, &[(48000, 256, 63.0)]), 1215);
        // At short periods the margin is 2 ms.
        assert_eq!(align_frames(48000, 64, &[(48000, 64, 63.0)]), 415);
        // A card at another rate counts at the master's.
        let target = 64.0 + (3.0 * 32.0 + 31.0) * 2.0 + 96.0;
        assert_eq!(align_frames(48000, 64, &[(24000, 32, 31.0)]), target as u32);
        // The slowest path sets it for all.
        let two = [(48000, 64, 31.0), (24000, 32, 31.0)];
        assert_eq!(align_frames(48000, 64, &two), target as u32);
        assert_eq!(align_frames(48000, 64, &[]), 0);
    }

    /// The latencies the sweeps cover, in ms.
    const LATENCIES: [f32; 9] = [1.0, 2.0, 4.0, 6.5, 12.0, 20.0, 48.0, 64.0, 100.0];

    #[test]
    fn a_feed_holds_what_its_card_and_the_drift_need_at_any_rate() {
        let quality = Quality::High;
        let rates: Vec<u32> = checked_rates().collect();
        let mut refused = 0;
        for master_latency in LATENCIES {
            for latency in LATENCIES {
                let ok = check_alignment(master_latency, 2, latency, quality).is_ok();
                // An output as long as the master's, or longer, always works.
                if latency >= master_latency {
                    assert!(ok, "{latency} ms beside {master_latency} ms");
                }
                if !ok {
                    refused += 1;
                    continue;
                }
                let capacity = feed_capacity(master_latency, &[latency], quality);
                let line = delay_capacity(master_latency, &[latency], quality);
                for &master in &rates {
                    let period = period_frames(master, master_latency);
                    let margin = margin_frames(master, period);
                    for &card in &rates {
                        let part = part_at(master, card, latency, quality);
                        let target = align_frames(master, period, &[part]);
                        let what = format!("{master} to {card}, {master_latency} and {latency} ms");
                        let wanted = feed_frames(master, period, target, part);
                        assert!(wanted <= capacity, "{what}: {wanted} > {capacity}");
                        let (feed, cut) = feed_within(master, period, target, part, capacity);
                        assert!(!cut, "{what}: cut to {feed}");
                        let writer = writer_frames(master, part);
                        assert!((feed as f64) < believed(master, target, part), "{what}");
                        assert!(feed as f64 >= 2.0 * writer + margin, "{what}: {feed}");
                        let share = f64::from(target) - card_part(master, part);
                        let held = share + 2.0 * writer + 2.0 * f64::from(period);
                        assert!(feed as f64 >= held, "{what}: {feed} < {held}");
                        let delay = target.saturating_sub(PERIODS * period);
                        assert!(delay <= line, "{what}: delay {delay} > {line}");
                    }
                }
            }
        }
        // A latency far below the master's is refused, not cut short.
        assert!(refused > 0);
        let e = check_alignment(100.0, 3, 1.0, quality).unwrap_err();
        assert!(
            e.contains("output 3") && e.contains("latency_ms = 1"),
            "{e}"
        );
    }

    #[test]
    fn a_further_output_on_its_own_latency_plans_without_panicking() {
        // An output on its own latency, the master moved through rates
        // its card takes and does not.
        for latency in LATENCIES {
            let (mut output, rates) = further(&[44100, 48000, 96000, 192000]);
            output.latency_ms = Some(latency);
            let config = Config {
                latency_ms: Some(4.0),
                ..Config::default()
            };
            if check_alignment(12.0, 2, latency, config.resampling).is_err() {
                continue;
            }
            let (mut control, mut engine, logs, now) = started_with(&config, vec![(output, rates)]);
            engine.take(true);
            control.handle_events(now);
            for (conn, rate) in [44100, 88200, 176400, 352800, 384000]
                .into_iter()
                .enumerate()
            {
                let conn = conn as u64 + 1;
                play(&mut control, conn, rate, now);
                engine.take(true);
                control.handle_events(now);
                let stop = control.request(conn, Some(Request::Stop), now);
                assert_eq!(stop.reply, Some(Reply::Ok));
            }
            let (target, feed, _) = logs[0].last_path();
            assert!(target > 0.0 && feed > 0, "{latency}");
        }
    }

    #[test]
    fn a_card_runs_at_the_masters_rate_or_the_nearest_lower_in_a_tie() {
        assert_eq!(card_rate(&[], 96000), 96000);
        assert_eq!(card_rate(&[44100, 48000, 96000], 96000), 96000);
        assert_eq!(card_rate(&[44100, 48000], 96000), 48000);
        assert_eq!(card_rate(&[32000, 64000], 48000), 32000);
        // Its period follows from its latency by the master's rule.
        let period = |card, latency_ms| part_at(48000, card, latency_ms, Quality::High).1;
        assert_eq!(period(44100, 12.0), 256);
        assert_eq!(period(48000, 4.0), 64);
        assert_eq!(period(48000, 48.0), 1024);
        assert_eq!(period(8000, 4.0), 32);
    }

    #[test]
    fn a_master_rate_change_reopens_every_further_output() {
        let outputs = vec![further(&[44100, 48000, 96000]), further(&[44100, 48000])];
        let (mut control, mut engine, logs, now) = started_with(&Config::default(), outputs);
        let seen = engine.take(false);
        assert!(
            has(&seen, "AddOutput 1") && has(&seen, "AddOutput 2"),
            "{seen:?}"
        );
        // Nothing reaches a writer before the master is open.
        assert!(logs.iter().all(|log| log.take().is_empty()));
        engine.send(Event::Reconfigured);
        control.handle_events(now);
        for log in &logs {
            assert_eq!(log.take(), ["reconfigure 48000 48000 256", "open true"]);
        }
        // A player at 96 kHz takes the master there. The writers close
        // while it reopens, and hear of the new rate only once it is open.
        play(&mut control, 1, 96000, now);
        assert!(has(&engine.take(false), "Reconfigure 96000 512"));
        for log in &logs {
            assert_eq!(log.take(), ["open false"]);
        }
        engine.send(Event::Reconfigured);
        control.handle_events(now);
        assert_eq!(logs[0].take(), ["reconfigure 96000 96000 512", "open true"]);
        // The second card does not take 96 kHz: its nearest rate, through
        // a resampler made for it.
        assert_eq!(logs[1].take(), ["reconfigure 96000 48000 256", "open true"]);
        let (_, _, delay) = logs[1].last_path();
        assert_eq!(delay, resampler_delay(96000, 48000, 256));
        let (_, _, delay) = logs[0].last_path();
        assert_eq!(delay, resampler_delay(96000, 96000, 512));
    }

    #[test]
    fn a_further_card_found_with_other_rates_moves_to_one_it_takes() {
        let (mut control, mut engine, logs, now) =
            started_with(&Config::default(), vec![further(&[])]);
        engine.take(true);
        control.handle_events(now);
        assert_eq!(logs[0].take(), ["reconfigure 48000 48000 256", "open true"]);
        // The card comes back taking 44.1 kHz only: the open writer is
        // reconfigured, which reopens its card, and the master stays.
        control.set_output_rates(1, vec![44100], now);
        assert_eq!(logs[0].take(), ["reconfigure 48000 44100 256"]);
        let seen = engine.take(true);
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
    }

    #[test]
    fn a_change_of_one_cards_rates_reconfigures_that_output_alone() {
        // The first output's 48 ms sets the target for all.
        let (mut slow, rates) = further(&[]);
        slow.latency_ms = Some(48.0);
        let outputs = vec![(slow, rates), further(&[])];
        let (mut control, mut engine, logs, now) = started_with(&Config::default(), outputs);
        engine.take(true);
        control.handle_events(now);
        for log in &logs {
            log.take();
        }
        // Rates that leave the card where it is change nothing.
        control.set_output_rates(2, vec![48000, 96000], now);
        assert!(logs.iter().all(|log| log.take().is_empty()));
        assert!(engine.take(true).is_empty());
        // The second card now takes 44.1 kHz only: it alone moves, and
        // the target, the first output's, stays, so the master's delay too.
        let (target, _, _) = logs[1].last_path();
        control.set_output_rates(2, vec![44100], now);
        assert!(logs[0].take().is_empty());
        assert_eq!(logs[1].take(), ["reconfigure 48000 44100 256"]);
        assert_eq!(logs[1].last_path().0, target);
        let seen = engine.take(true);
        assert!(!has(&seen, "SetDelay"), "{seen:?}");
        // The first card moving changes the target: every output follows,
        // and the master's delay with them.
        control.set_output_rates(1, vec![44100], now);
        assert_eq!(logs[0].take(), ["reconfigure 48000 44100 1024"]);
        assert_eq!(logs[1].take(), ["reconfigure 48000 44100 256"]);
        assert!(has(&engine.take(true), "SetDelay"));
    }

    #[test]
    fn master_volume_moves_every_output_with_its_offset() {
        let (mut output, rates) = further(&[]);
        output.volume_db = -6.0;
        let (mut control, mut engine, _logs, now) =
            started_with(&Config::default(), vec![(output, rates)]);
        engine.take(true);
        // The mix comes with its gain, at 100 %.
        let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
        assert_eq!(engine.outputs.len(), 1);
        assert!(close(engine.outputs[0].1, db_to_gain(-6.0)));
        let volume = |percent| Some(Request::SetVolume { percent });
        control.connected(1, "test".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(1, Some(hello), now);
        engine.gains.clear();
        assert_eq!(control.request(1, volume(50), now).reply, Some(Reply::Ok));
        engine.take(true);
        let gains = std::mem::take(&mut engine.gains);
        assert_eq!(gains.len(), 2, "{gains:?}");
        assert_eq!(gains[0], (0, db_to_gain(-30.0)));
        assert_eq!(gains[1].0, 1);
        assert!(close(gains[1].1, db_to_gain(-36.0)), "{gains:?}");
        // With the volume on the master's card, the master's samples are
        // left alone; the further output still follows, in software.
        let set = Arc::new(Mutex::new(Vec::new()));
        control.card_changed(Some(Box::new(FakeMixer(set))));
        engine.take(true);
        engine.gains.clear();
        assert_eq!(control.request(1, volume(40), now).reply, Some(Reply::Ok));
        engine.take(true);
        let gains = std::mem::take(&mut engine.gains);
        assert_eq!(gains[0], (0, 1.0));
        assert!(close(gains[1].1, db_to_gain(-42.0)), "{gains:?}");
    }

    #[test]
    fn the_master_is_delayed_to_the_further_outputs_target() {
        let (mut control, mut engine, logs, now) =
            started_with(&Config::default(), vec![further(&[44100])]);
        // The card takes only 44.1 kHz: a period of 256 frames there lasts
        // nearest to the master's 256 at 48 kHz.
        let delay = resampler_delay(48000, 44100, 256);
        let target = align_frames(48000, 256, &[(44100, 256, delay)]);
        let seen = engine.take(true);
        let line = seen.iter().position(|s| s.starts_with("SetDelay"));
        let open = seen.iter().position(|s| s.starts_with("Reconfigure"));
        // The master's two periods are part of its path already.
        let expected = format!("SetDelay {}", target - 2 * 256);
        assert_eq!(line.map(|i| seen[i].clone()), Some(expected), "{seen:?}");
        assert!(line < open, "{seen:?}");
        control.handle_events(now);
        let (held, feed, _) = logs[0].last_path();
        assert_eq!(held, f64::from(target));
        // The feed holds what a further output needs and the drift believes.
        let scale = 48000.0 / 44100.0;
        let writer_period = 256.0 * scale;
        assert!((feed as f64) < held + crate::drift::SANE * writer_period);
        assert!(feed as f64 >= 2.0 * writer_period + 128.0);
        assert!(feed >= crate::further::FEED_PERIODS * 256);
    }

    #[test]
    fn no_further_output_means_no_delay() {
        let (mut control, mut engine, now) = started(&Config::default(), None);
        assert_eq!(engine.take(true), ["SetGain", "Reconfigure 48000 256"]);
        control.handle_events(now);
        play(&mut control, 1, 96000, now);
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 96000"), "{seen:?}");
        assert!(
            !has(&seen, "SetDelay") && !has(&seen, "AddOutput"),
            "{seen:?}"
        );
    }

    #[test]
    fn a_further_output_goes_off_by_its_own_idle_rule() {
        let config = Config {
            idle: Idle::Zeros,
            ..Config::default()
        };
        let (mut output, rates) = further(&[]);
        output.idle = Idle::Off { delay_s: 2 };
        let (mut control, mut engine, logs, now) = started_with(&config, vec![(output, rates)]);
        engine.take(true);
        control.handle_events(now);
        assert_eq!(logs[0].take(), ["reconfigure 48000 48000 256", "open true"]);
        let s = Duration::from_secs;
        control.tick(now + s(1));
        assert!(logs[0].take().is_empty());
        control.tick(now + s(2));
        assert_eq!(logs[0].take(), ["open false"]);
        // The master keeps playing zeros.
        control.tick(now + s(60));
        assert!(control.output_is_open());
        assert!(!has(&engine.take(true), "CloseOutput"));
        // A player opens it again, at the rate it had: the writer relocks.
        play(&mut control, 1, 48000, now + s(61));
        assert_eq!(logs[0].take(), ["open true"]);
        // Playing, it stays open.
        control.tick(now + s(70));
        assert!(logs[0].take().is_empty());
    }

    /// A started control thread whose master goes off after 1 s, with
    /// further outputs on the idle rules given, open and handed over.
    fn master_off_with(rules: &[Idle]) -> (Control, FakeEngine, Vec<WriterLog>, Instant) {
        let config = Config {
            idle: Idle::Off { delay_s: 1 },
            ..Config::default()
        };
        let outputs = rules.iter().map(|&idle| {
            let (mut output, rates) = further(&[]);
            output.idle = idle;
            (output, rates)
        });
        let (mut control, mut engine, logs, now) = started_with(&config, outputs.collect());
        engine.take(true);
        control.handle_events(now);
        for log in &logs {
            assert_eq!(log.take(), ["reconfigure 48000 48000 256", "open true"]);
        }
        (control, engine, logs, now)
    }

    #[test]
    fn a_further_output_on_zeros_keeps_the_cycle_on_the_timer_when_the_master_goes_off() {
        let (mut control, mut engine, logs, now) = master_off_with(&[Idle::Zeros]);
        let s = Duration::from_secs;
        let period = period_frames(48000, 12.0);
        control.tick(now + s(2));
        let seen = engine.take(true);
        assert!(
            has(&seen, &format!("ParkOutput 48000 {period}")),
            "{seen:?}"
        );
        assert!(!has(&seen, "CloseOutput"));
        assert!(!control.output_is_open());
        // The output plays on, for as long as nothing plays.
        control.tick(now + s(600));
        assert!(engine.take(true).is_empty());
        assert!(logs[0].take().is_empty());
        // A player at the rate the cycle runs at opens the card under it:
        // the output goes on as it is, neither closed nor reconfigured.
        play(&mut control, 1, 48000, now + s(601));
        let seen = engine.take(true);
        assert!(
            has(&seen, &format!("Reconfigure 48000 {period}")),
            "{seen:?}"
        );
        assert!(!has(&seen, "SetDelay"));
        control.handle_events(now + s(601));
        assert!(control.output_is_open());
        assert!(logs[0].take().is_empty());
        // Off again once the player has gone, and parked again.
        control.request(1, Some(Request::Stop), now + s(602));
        control.tick(now + s(604));
        assert!(has(&engine.take(true), "ParkOutput"));
        assert!(logs[0].take().is_empty());
    }

    #[test]
    fn a_player_at_another_rate_reopens_a_parked_master_as_any_rate_change_does() {
        let (mut control, mut engine, logs, now) = master_off_with(&[Idle::Zeros]);
        let s = Duration::from_secs;
        control.tick(now + s(2));
        engine.take(true);
        play(&mut control, 1, 44100, now + s(3));
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 44100"), "{seen:?}");
        assert_eq!(logs[0].take(), ["open false"]);
        control.handle_events(now + s(3));
        assert_eq!(logs[0].take(), ["reconfigure 44100 44100 256", "open true"]);
    }

    #[test]
    fn a_further_output_on_off_closes_after_its_own_delay_while_the_master_is_parked() {
        let rules = [Idle::Off { delay_s: 5 }, Idle::Zeros];
        let (mut control, mut engine, logs, now) = master_off_with(&rules);
        let s = Duration::from_secs;
        control.tick(now + s(2));
        assert!(has(&engine.take(true), "ParkOutput"));
        control.tick(now + s(4));
        assert!(logs[0].take().is_empty());
        // Its delay counts from when the last source stopped, here the start.
        control.tick(now + s(5));
        assert_eq!(logs[0].take(), ["open false"]);
        assert!(logs[1].take().is_empty());
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn the_parked_cycle_ends_with_the_last_further_output() {
        let (mut control, mut engine, logs, now) = master_off_with(&[Idle::Off { delay_s: 5 }]);
        let s = Duration::from_secs;
        control.tick(now + s(2));
        assert!(has(&engine.take(true), "ParkOutput"));
        control.tick(now + s(5));
        assert_eq!(logs[0].take(), ["open false"]);
        assert_eq!(engine.take(true), ["CloseOutput"]);
        // A player opens the master and the output as from closed.
        play(&mut control, 1, 48000, now + s(6));
        assert!(has(&engine.take(true), "Reconfigure 48000"));
        control.handle_events(now + s(6));
        assert_eq!(logs[0].take(), ["reconfigure 48000 48000 256", "open true"]);
    }

    #[test]
    fn a_further_card_found_while_parked_is_handed_its_path_at_the_parked_rate() {
        let (mut control, mut engine, logs, now) = master_off_with(&[Idle::Zeros]);
        let s = Duration::from_secs;
        control.tick(now + s(2));
        assert!(has(&engine.take(true), "ParkOutput"));
        // Found again, it takes only 44.1 kHz: it is given a path from the
        // parked master's 48 kHz to that, and plays on.
        control.set_output_rates(1, vec![44100], now + s(3));
        assert_eq!(logs[0].take(), ["reconfigure 48000 44100 256"]);
        let seen = engine.take(true);
        assert!(!has(&seen, "CloseOutput"), "{seen:?}");
        assert!(!has(&seen, "Reconfigure"), "{seen:?}");
        control.tick(now + s(4));
        assert!(engine.take(true).is_empty());
        assert!(logs[0].take().is_empty());
    }

    #[test]
    fn the_master_parks_only_once_a_reopen_under_way_is_done() {
        let (mut control, mut engine, logs, now) = master_off_with(&[Idle::Zeros]);
        let ms = Duration::from_millis;
        play(&mut control, 1, 44100, now);
        engine.take(true);
        control.handle_events(now);
        logs[0].take();
        control.request(1, Some(Request::Stop), now + ms(100));
        // The card now takes only 48 kHz: the master reopens at once, and
        // the reopen is not answered yet when the idle delay runs out.
        control.set_supported(vec![48000], now + ms(1500));
        control.tick(now + ms(1500));
        let seen = engine.take(false);
        assert!(has(&seen, "Reconfigure 48000"), "{seen:?}");
        assert!(!has(&seen, "ParkOutput") && !has(&seen, "CloseOutput"));
        assert!(control.output_is_open());
        assert_eq!(logs[0].take(), ["open false"]);
        control.tick(now + ms(1600));
        assert!(engine.take(false).is_empty());
        engine.send(Event::Reconfigured);
        control.handle_events(now + ms(1700));
        assert_eq!(logs[0].take(), ["reconfigure 48000 48000 256", "open true"]);
        control.tick(now + ms(1800));
        assert!(has(&engine.take(true), "ParkOutput 48000"));
        assert!(logs[0].take().is_empty());
    }

    #[test]
    fn a_starve_as_the_masters_card_opens_under_the_parked_cycle_is_one_line() {
        let (mut control, mut engine, logs, now) = master_off_with(&[Idle::Zeros]);
        let s = Duration::from_secs;
        control.tick(now + s(2));
        engine.take(true);
        play(&mut control, 1, 48000, now + s(3));
        engine.take(true);
        let starve = |at_ms| FurtherEvent::Starved {
            at_ms,
            fill: 0,
            limit: 1024,
        };
        logs[0].events.lock().unwrap().push_back(starve(3000));
        control.handle_events(now + s(3));
        // Taken as the open's, outside the count: the log still has all ten
        // lines before it goes to one a minute.
        assert_eq!(control.further[0].log.lines, 0);
        assert!(control.further[0].resumed_until.is_none());
        // A second, later, is an ordinary starve.
        logs[0].events.lock().unwrap().push_back(starve(5000));
        control.handle_events(now + s(5));
        assert_eq!(control.further[0].log.lines, 1);
    }

    #[test]
    fn further_outputs_off_by_the_masters_delay_close_with_it() {
        let (mut control, mut engine, logs, now) = master_off_with(&[Idle::Off { delay_s: 1 }]);
        control.tick(now + Duration::from_secs(2));
        let seen = engine.take(true);
        assert!(has(&seen, "CloseOutput"), "{seen:?}");
        assert!(!has(&seen, "ParkOutput"));
        assert_eq!(logs[0].take(), ["open false"]);
    }

    #[test]
    fn a_further_chain_that_cannot_build_at_the_rate_silences_that_output_only() {
        let (mut output, rates) = further(&[]);
        output.plugins = vec![entry(config::PluginPlace::Mix, "only-up-to-48k")];
        let (mut control, mut engine, logs, now) =
            started_with(&Config::default(), vec![(output, rates)]);
        let seen = engine.take(true);
        assert!(has(&seen, "SetChain 1 Mix balance"), "{seen:?}");
        control.handle_events(now);
        engine.gains.clear();
        // A player at 96 kHz: the master goes there, as without the output,
        // and the output, whose chain does not build there, is silent.
        play(&mut control, 1, 96000, now);
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 96000"), "{seen:?}");
        assert!(!has(&seen, "SetChain"), "{seen:?}");
        assert_eq!(engine.gains, [(1, 0.0)]);
        assert_eq!(control.output.unwrap().0, 96000);
        control.handle_events(now);
        // It still follows the master, in silence.
        assert_eq!(
            logs[0].take(),
            [
                "reconfigure 48000 48000 256",
                "open true",
                "open false",
                "reconfigure 96000 96000 512",
                "open true"
            ]
        );
        // Back at 48 kHz it plays again, with the chain it had.
        engine.gains.clear();
        let stop = control.request(1, Some(Request::Stop), now);
        assert_eq!(stop.reply, Some(Reply::Ok));
        play(&mut control, 2, 48000, now + Duration::from_secs(1));
        control.tick(now + Duration::from_secs(2));
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 48000"), "{seen:?}");
        assert!(!has(&seen, "SetChain"), "{seen:?}");
        assert_eq!(engine.gains, [(1, 1.0)]);
    }

    #[test]
    fn a_further_outputs_lock_is_logged_with_its_ppm_once_known() {
        let line = |event| event_line(2, event);
        let locked = line(FurtherEvent::Locked { ppm: None });
        assert_eq!(locked.as_deref(), Some("aas: output 2 locked"));
        let known = line(FurtherEvent::Locked { ppm: Some(12.5) });
        assert_eq!(
            known.as_deref(),
            Some("aas: output 2 locked, its card runs +12.5 ppm from the master")
        );
        let lost = line(FurtherEvent::Unlocked);
        assert_eq!(lost.as_deref(), Some("aas: output 2 lost its lock"));
        // Starves, overflows and the clamp go through an output's log.
        let starve = FurtherEvent::Starved {
            at_ms: 1,
            fill: 0,
            limit: 1,
        };
        assert_eq!(line(starve), None);
        assert_eq!(line(FurtherEvent::Clamped { at_ms: 1 }), None);
    }

    #[test]
    fn the_masters_delay_says_when_the_master_cannot_be_in_time() {
        // Target less the master's two periods, within the line.
        assert_eq!(master_delay(1000, 256, 2000), (488, None));
        assert_eq!(master_delay(512, 256, 2000), (0, None));
        // A further path shorter than the master's own buffer.
        let (delay, missed) = master_delay(400, 256, 2000);
        assert_eq!(
            (delay, missed),
            (0, Some(Misaligned::Behind { frames: 112 }))
        );
        assert_eq!(
            missed.unwrap().line(),
            "aas: warning: the master's own buffer is 112 frames longer than the delay \
             the other outputs are held at: they play ahead of it"
        );
        // A line too short.
        let (delay, missed) = master_delay(3000, 256, 2000);
        let ahead = Misaligned::Ahead {
            capacity: 2000,
            wanted: 2488,
        };
        assert_eq!((delay, missed), (2000, Some(ahead)));
        assert!(ahead.line().contains("held back by 2000 frames, not 2488"));
    }

    #[test]
    fn a_card_that_runs_another_buffer_is_logged_with_what_to_set() {
        let granted = FurtherEvent::Granted {
            rate: 48000,
            asked_period: 1024,
            period: 1024,
            asked_buffer: 3072,
            buffer: 4096,
        };
        assert_eq!(
            event_line(3, granted).as_deref(),
            Some(
                "aas: warning: output 3: its card runs a period of 1024 frames (21.3 ms) \
                 and a buffer of 4096 frames (85.3 ms), asked for 1024 (21.3 ms) and 3072 \
                 (64.0 ms); its delay is planned from what was asked and it may starve, \
                 set this output's latency_ms to a period the card runs"
            )
        );
    }

    #[test]
    fn the_first_starves_and_overflows_are_logged_then_one_a_minute() {
        let mut log = OutputLog::default();
        let starve = |at_ms| FurtherEvent::Starved {
            at_ms,
            fill: 300,
            limit: 2944,
        };
        let overflow = |at_ms, count| FurtherEvent::Overflowed {
            at_ms,
            fill: 2560,
            limit: 2944,
            count,
        };
        assert_eq!(
            log.line(2, starve(12_345)).as_deref(),
            Some(
                "aas: output 2 starved 12.345 s after the start, its feed held 300 of 2944 frames"
            )
        );
        assert_eq!(
            log.line(2, overflow(15_000, 1)).as_deref(),
            Some(
                "aas: output 2 overflowed 15.000 s after the start, its feed held 2560 of 2944 frames"
            )
        );
        // Several overflows seen at once are told as such.
        let three = log.line(2, overflow(16_000, 3)).unwrap();
        assert!(
            three.starts_with("aas: output 2 overflowed 3 times by 16.000 s"),
            "{three}"
        );
        for n in 3..9 {
            assert!(log.line(2, starve(20_000 + n)).is_some());
        }
        let tenth = log.line(2, starve(30_000)).unwrap();
        assert!(
            tenth.ends_with("frames; from now on one line a minute"),
            "{tenth}"
        );
        // Then one a minute, with how many starves and overflows went
        // unlogged.
        for n in 0..5 {
            assert_eq!(log.line(2, overflow(31_000 + n * 10_000, 2)), None);
        }
        assert_eq!(log.line(2, starve(89_999)), None);
        let next = log.line(2, overflow(90_000, 1)).unwrap();
        assert!(
            next.starts_with("aas: output 2 overflowed 90.000 s"),
            "{next}"
        );
        assert!(
            next.ends_with("frames, 11 more since the last line"),
            "{next}"
        );
        assert_eq!(log.line(2, starve(90_001)), None);
        // Lines it does not keep are not its business.
        assert_eq!(log.line(2, FurtherEvent::Unlocked), None);
    }

    #[test]
    fn the_clamp_is_logged_at_most_once_a_minute() {
        let mut log = OutputLog::default();
        let clamped = |at_ms| FurtherEvent::Clamped { at_ms };
        let first = log.line(2, clamped(31_000)).unwrap();
        assert_eq!(
            first,
            "aas: output 2: its card runs more than 2000 ppm from the master, \
             it plays on with gaps"
        );
        assert_eq!(log.line(2, clamped(40_000)), None);
        assert_eq!(log.line(2, clamped(90_999)), None);
        let again = log.line(2, clamped(91_000)).unwrap();
        assert!(
            again.ends_with(", 2 more times since the last line"),
            "{again}"
        );
        // The feed's lines are counted apart.
        let starve = FurtherEvent::Starved {
            at_ms: 91_001,
            fill: 0,
            limit: 1,
        };
        assert!(log.line(2, starve).is_some());
    }

    #[test]
    fn the_feed_holds_a_card_period_and_two_master_periods() {
        // A card period of 2048 frames on a 48 ms output beside a master
        // of 512 at 96 kHz. When the writer wakes its card holds a period
        // less than the target counts, which the feed holds meanwhile; then
        // it takes up to a whole card period, late wake and resampling
        // together, while the master pushes on; and two master periods.
        for quality in [Quality::Medium, Quality::High] {
            let part = part_at(96000, 96000, 48.0, quality);
            assert_eq!(part.1, 2048);
            let target = align_frames(96000, 512, &[part]);
            let share = f64::from(target) - card_part(96000, part);
            let feed = feed_frames(96000, 512, target, part);
            let least = share + 2.0 * 2048.0 + 2.0 * 512.0;
            assert!(feed as f64 >= least, "{feed} < {least}");
            assert!((feed as f64) < believed(96000, target, part));
            check_alignment(12.0, 2, 48.0, quality).unwrap();
            assert!(feed <= feed_capacity(12.0, &[48.0], quality));
        }
    }
}
