//! The mix of one output: the buses through its matrix, its mix and output
//! chains and its gain, into the samples its card gets.
//!
//! The engine reads the sources once a cycle into the `players` and
//! `analog` buses and hands them to each output's mix. The copy path and
//! DoP play to the master only, and stay in the engine; they read the
//! master's routes from here.

use crate::engine::{FADE_MS, Place};
use crate::format::{BlockStore, Processing, Ramp, matrix_add, mix_finish_wide};
use crate::matrix::{Bus, MAX_CHANNELS, Matrix};
use crate::plugin::chain::Chain;

/// For each output channel, which channel of a stereo bus it copies, 0
/// left and 1 right, or `None` for silence: a pure routing, as
/// `Matrix::routing` gives it.
pub(crate) type Routing = [Option<u8>; MAX_CHANNELS];

/// Whether `routing` gives a DoP stream to whole pairs of the sink's
/// `channels`: channels 1 and 2, 3 and 4 and so on, with the last one a
/// pair on its own when `channels` is odd. Each pair has both of its
/// channels named or neither, and at least one channel is named. A DAC
/// that takes DoP over a stereo pair needs the markers on both of its
/// channels: zeros on one of them drop the markers, and it may play the
/// other as PCM, which is loud noise. One side of the stream on both
/// channels of a pair keeps the markers, and a card of one channel takes
/// the left side alone.
fn keeps_pairs(routing: &Routing, channels: usize) -> bool {
    let named = |c: usize| routing.get(c).is_some_and(Option::is_some);
    let whole = (0..channels)
        .step_by(2)
        .all(|c| c + 1 >= channels || named(c) == named(c + 1));
    whole && (0..channels).any(named)
}

/// The channels `chain` gives for blocks of `count` channels: its own
/// output count, or `count` for a chain that keeps any.
fn gives(chain: &Chain, count: usize) -> usize {
    match chain.channels_out() {
        0 => count,
        n => n,
    }
}

/// The gains of `matrix` from each channel of `bus` to each output
/// channel, as constant ramps. `forced` takes every route that is not off
/// as 0 dB, as forced bit-perfect does: there a routing that sums two
/// channels into one, which cannot be copied, is mixed at unit gains, as
/// overlapping sources are.
fn bus_gains(matrix: &Matrix, bus: Bus, forced: bool) -> [[Ramp; MAX_CHANNELS]; 2] {
    let mut gains = [[Ramp::flat(0.0); MAX_CHANNELS]; 2];
    for (row, input) in gains.iter_mut().zip(bus.inputs()) {
        for (channel, gain) in row.iter_mut().enumerate().take(matrix.width()) {
            let g = matrix.gain(input, channel);
            *gain = Ramp::flat(if forced && g != 0.0 { 1.0 } else { g });
        }
    }
    gains
}

/// The gains of both buses, as `bus_gains` gives them, by bus.
type Gains = [[[Ramp; MAX_CHANNELS]; 2]; 2];

/// `bus_gains` of both buses.
fn gains_of(matrix: &Matrix, forced: bool) -> Gains {
    [
        bus_gains(matrix, Bus::Players, forced),
        bus_gains(matrix, Bus::Analog, forced),
    ]
}

/// Whether `gains`, of a matrix `width` wide, take each bus to channels 1
/// and 2 unchanged: left to 1 and right to 2 at exactly 0 dB, nothing
/// else. Adding a bus through them then adds it as it is, in every format.
fn passes_through(gains: &Gains, width: usize) -> bool {
    let (one, off) = (Ramp::flat(1.0), Ramp::flat(0.0));
    width == 2
        && gains
            .iter()
            .all(|bus| bus[0][0] == one && bus[0][1] == off && bus[1][0] == off && bus[1][1] == one)
}

/// What each cycle asks of the matrix in place, worked out once when the
/// matrix is set: it changes only with `Command::SetMatrix`, and the mode
/// and the sink's channels not at all.
#[derive(Clone, Copy)]
pub(crate) struct Routes {
    /// How the copy path places each bus, by `Bus::index`, as the engine's
    /// `copy_routing` says.
    pub(crate) copy: [Option<Routing>; 2],
    /// The `players` routing for a DoP stream, if it keeps the sink's pairs
    /// whole, as `routing_for_dop` says before its checks of the ramp and
    /// the chains.
    dop: Option<Routing>,
    /// The gains pass both buses through, as `passes_through` says.
    pub(crate) passes: bool,
}

impl Routes {
    fn of(matrix: &Matrix, gains: &Gains, forced: bool, channels: usize) -> Routes {
        let dop = matrix.routing(Bus::Players, false);
        Routes {
            copy: Bus::ALL.map(|bus| matrix.routing(bus, forced)),
            dop: dop.filter(|routing| keeps_pairs(routing, channels)),
            passes: passes_through(gains, matrix.width()),
        }
    }
}

/// Where each of `gains` ends.
fn levels(gains: &Gains) -> [[[f32; MAX_CHANNELS]; 2]; 2] {
    gains.map(|bus| bus.map(|side| side.map(|ramp| ramp.to)))
}

/// Adds `bus` through the matrix into `acc`, `width` wide: its first
/// `frames` frames along `step`, the rest at `gains`.
fn matrix_add_ramped<P: Processing>(
    bus: &[[P::Acc; 2]],
    step: &[[Ramp; MAX_CHANNELS]; 2],
    frames: usize,
    gains: &[[Ramp; MAX_CHANNELS]; 2],
    width: usize,
    acc: &mut [P::Acc],
) {
    let (bus_ramp, bus_rest) = bus.split_at(frames);
    let (acc_ramp, acc_rest) = acc.split_at_mut(frames * width);
    if frames > 0 {
        matrix_add::<P>(bus_ramp, step, width, acc_ramp);
    }
    matrix_add::<P>(bus_rest, gains, width, acc_rest);
}

/// Fades `out`, interleaved frames of `channels` samples, in after
/// silence: `left` of `fade` frames of the fade are still to come, and
/// fewer after the call. Every channel of a frame gets the same gain.
/// A `left` beyond `fade` is taken as `fade`: the fade starts from zero.
fn unmute(out: &mut [i32], channels: usize, left: &mut usize, fade: usize) {
    *left = (*left).min(fade);
    for frame in out.chunks_exact_mut(channels) {
        if *left == 0 {
            break;
        }
        let gain = (fade - *left) as f64 / fade as f64;
        for v in frame.iter_mut() {
            *v = (f64::from(*v) * gain).round() as i32;
        }
        *left -= 1;
    }
}

/// One output's matrix, chains and gain, and the buffers its mix needs.
/// Everything is sized when it is made, for periods of up to the
/// `max_frames` it is made with: the audio thread allocates nothing here.
pub struct OutputMix<P: Processing> {
    /// The card's channels.
    pub(crate) channels: usize,
    /// The gains from the buses to the channels of the output.
    pub(crate) matrix: Matrix,
    /// The same, by bus: `players` and then `analog`. In forced bit-perfect
    /// every route that is not off is at 0 dB. While a change of matrix
    /// ramps, these are the gains it goes to.
    gains: Gains,
    /// While a change of matrix ramps: where each gain is at the start of
    /// the next cycle.
    ramp_from: [[[f32; MAX_CHANNELS]; 2]; 2],
    /// Frames until the gains reach `gains`. No ramp runs at 0.
    pub(crate) ramp_left: usize,
    /// What the matrix gives the copy path and DoP, and whether it passes
    /// the buses through.
    pub(crate) routes: Routes,
    /// How a DoP stream reaches the sink this cycle, or `None` while the
    /// matrix or the chains would alter it. Worked out at the start of each
    /// period from the matrix and chains in place.
    pub(crate) dop_routing: Option<Routing>,
    /// The buses through the matrix, interleaved, and the chains' output.
    wide: Vec<P::Acc>,
    /// Where the mix is handed to the plugins.
    store: BlockStore,
    pub(crate) mix: Chain,
    pub(crate) output: Chain,
    /// The output gain at the start of the next cycle.
    pub(crate) gain: f32,
    pub(crate) target_gain: f32,
    /// Frames until `gain` reaches `target_gain`.
    fade_left: usize,
    /// Frames of the fade in left after the mix or output chain was built
    /// for another rate than the engine runs at, and the output was
    /// silent for it.
    unmute_left: usize,
    /// A period passed without the mix and output chains running: after
    /// DoP, the copy path, a mute or in forced bit-perfect. They are reset
    /// before they run again.
    pub(crate) chains_idle: bool,
}

impl<P: Processing> OutputMix<P> {
    /// The mix of a card of `channels` channels through `matrix`, for
    /// periods of up to `max_frames`, at unity gain with empty chains.
    /// `forced` is forced bit-perfect. Its buffers are sized for the wider
    /// of the matrix and the card.
    pub fn new(matrix: Matrix, channels: usize, forced: bool, max_frames: usize) -> Self {
        let widest = matrix.width().max(channels);
        let gains = gains_of(&matrix, forced);
        OutputMix {
            channels,
            ramp_from: levels(&gains),
            ramp_left: 0,
            routes: Routes::of(&matrix, &gains, forced, channels),
            gains,
            dop_routing: None,
            matrix,
            wide: vec![P::zero(); max_frames * widest],
            store: BlockStore::new(max_frames, widest),
            mix: Chain::empty(),
            output: Chain::empty(),
            gain: 1.0,
            target_gain: 1.0,
            fade_left: 0,
            unmute_left: 0,
            chains_idle: false,
        }
    }

    /// Whether `chain` fits the channel counts at `place`, the mix or the
    /// output, as `Command::SetChain` says. A count of 0 is any count.
    pub(crate) fn chain_fits(&self, place: Place, chain: &Chain) -> bool {
        let takes = |n: usize, count: usize| n == 0 || n == count;
        let width = self.matrix.width();
        match place {
            Place::Mix => {
                let between = gives(chain, width);
                let output = &self.output;
                takes(chain.channels_in(), width)
                    && (output.is_empty()
                        || takes(output.channels_in(), between)
                            && gives(output, between) == self.channels)
            }
            Place::Output => {
                let between = gives(&self.mix, width);
                takes(chain.channels_in(), between) && gives(chain, between) == self.channels
            }
            Place::Analog => unreachable!("the analog input's chain is the engine's"),
        }
    }

    /// The output's chain at `place`, the mix or the output.
    pub(crate) fn chain(&mut self, place: Place) -> &mut Chain {
        match place {
            Place::Mix => &mut self.mix,
            Place::Output => &mut self.output,
            Place::Analog => unreachable!("the analog input's chain is the engine's"),
        }
    }

    /// Sets the output gain: over `fade` frames from where it is, or at
    /// once for `None`.
    pub(crate) fn set_gain(&mut self, gain: f32, fade: Option<usize>) {
        self.target_gain = gain;
        match fade {
            Some(frames) => self.fade_left = frames,
            None => self.finish_fade(),
        }
    }

    pub(crate) fn finish_fade(&mut self) {
        self.gain = self.target_gain;
        self.fade_left = 0;
    }

    /// The gain after this cycle's part of the fade, over a period of
    /// `period` frames, and how many frames of it that part covers.
    pub(crate) fn next_fade_step(&self, period: usize) -> (f32, usize) {
        let frames = self.fade_left.min(period);
        if frames == self.fade_left {
            return (self.target_gain, frames);
        }
        let share = frames as f32 / self.fade_left as f32;
        (self.gain + (self.target_gain - self.gain) * share, frames)
    }

    /// Moves the fade on by a cycle's part of it, as `next_fade_step` gave
    /// it.
    pub(crate) fn step_fade(&mut self, (gain, frames): (f32, usize)) {
        self.gain = gain;
        self.fade_left -= frames;
    }

    /// Replaces the matrix with one of the same width and returns the one
    /// it replaced, or returns `matrix` itself, refused, if its width is
    /// another. `forced` takes the gains as `bus_gains` does. Each gain
    /// moves to its new value over `ramp` frames, from where it is, also
    /// during another ramp; with `None`, or when no gain moves, the change
    /// applies at once. The engine's `set_matrix` says when it ramps.
    pub(crate) fn set_matrix(
        &mut self,
        mut matrix: Box<Matrix>,
        forced: bool,
        ramp: Option<usize>,
    ) -> Box<Matrix> {
        if matrix.width() == self.matrix.width() {
            let gains = gains_of(&matrix, forced);
            let from = if self.ramp_left > 0 {
                self.ramp_from
            } else {
                levels(&self.gains)
            };
            std::mem::swap(&mut self.matrix, &mut matrix);
            self.routes = Routes::of(&self.matrix, &gains, forced, self.channels);
            self.gains = gains;
            let moves = from != levels(&self.gains);
            match ramp {
                Some(frames) if moves => {
                    self.ramp_from = from;
                    self.ramp_left = frames;
                }
                _ => self.finish_ramp(),
            }
        }
        matrix
    }

    pub(crate) fn finish_ramp(&mut self) {
        self.ramp_from = levels(&self.gains);
        self.ramp_left = 0;
    }

    /// This cycle's part of a matrix ramp over a period of `period` frames:
    /// the gains over its first frames, by bus, and how many frames of the
    /// period that part covers.
    fn next_matrix_step(&self, period: usize) -> (Gains, usize) {
        let frames = self.ramp_left.min(period);
        let mut step = self.gains;
        if frames == 0 {
            return (step, 0);
        }
        let share = frames as f32 / self.ramp_left as f32;
        let last = frames == self.ramp_left;
        for (bus, from) in step.iter_mut().zip(&self.ramp_from) {
            for (side, from) in bus.iter_mut().zip(from) {
                for (ramp, &from) in side.iter_mut().zip(from) {
                    let to = ramp.to;
                    let to = if last { to } else { from + (to - from) * share };
                    *ramp = Ramp { from, to };
                }
            }
        }
        (step, frames)
    }

    /// Whether this cycle's mix takes the direct path: a matrix that passes
    /// both buses to channels 1 and 2 as they are, and no ramp. The analog
    /// input is then mixed straight into the players' bus, which is the mix
    /// itself.
    pub(crate) fn direct(&self, period: usize) -> bool {
        self.ramp_left.min(period) == 0 && self.routes.passes
    }

    /// Whether the copy path, or a DoP stream, may play, when the routing
    /// and the bit-perfect rules allow it: with a matrix as wide as the
    /// sink and chains that keep the channel count.
    pub(crate) fn copy_allowed(&self) -> bool {
        self.matrix.width() == self.channels
            && !self.mix.changes_channels()
            && !self.output.changes_channels()
    }

    /// How a DoP stream reaches the sink, if it can unaltered: through the
    /// `players` routing, pure with its gains as they are, also in forced
    /// bit-perfect, since a DoP stream is never scaled, and only where the
    /// copy path could play. The routing must also keep the sink's pairs of
    /// channels whole, as `keeps_pairs` says. There is none while a change
    /// of matrix ramps: the matrix in effect is then neither the old one
    /// nor the new one.
    pub(crate) fn routing_for_dop(&self) -> Option<Routing> {
        if self.ramp_left > 0 || !self.copy_allowed() {
            return None;
        }
        self.routes.dop
    }

    /// Both chains leave the audio as it is.
    pub(crate) fn neutral(&self) -> bool {
        self.mix.neutral() && self.output.neutral()
    }

    /// What the mix and output chains hold back, in frames.
    pub(crate) fn latency_frames(&self) -> u32 {
        self.mix
            .latency_frames()
            .saturating_add(self.output.latency_frames())
    }

    /// A fade in after the output was silent for a chain at another rate
    /// is not over.
    pub(crate) fn unmuting(&self) -> bool {
        self.unmute_left > 0
    }

    /// The output was opened at `rate`. A fade in still due is counted in
    /// frames of the old rate: it starts again, a whole 10 ms at this one.
    pub(crate) fn reopened(&mut self, rate: u32) {
        if self.unmute_left > 0 {
            self.unmute_left = (rate * FADE_MS / 1000) as usize;
        }
    }

    /// Mixes one period of the buses into `out`, interleaved frames of the
    /// card's channels: `players`, and `analog` while the analog input is
    /// audible, through the matrix, then the mix chain and the output
    /// chain, in that order, when `plugins` apply. On the direct path, as
    /// `direct` says, the analog input is already in `players` and
    /// `analog` is not read. The buses come at the output's gain; the
    /// caller moves the fade on. Returns whether a chain with plugins ran.
    pub(crate) fn mix(
        &mut self,
        players: &mut [[P::Acc; 2]],
        analog: Option<&[[P::Acc; 2]]>,
        out: &mut [i32],
        rate: u32,
        plugins: bool,
    ) -> bool {
        let period = players.len();
        let channels = self.channels;
        let ramp_frames = self.ramp_left.min(period);
        let direct = ramp_frames == 0 && self.routes.passes;
        let width = self.matrix.width();
        let mixed: &mut [P::Acc] = if direct {
            players.as_flattened_mut()
        } else {
            let (step, _) = self.next_matrix_step(period);
            let wide = &mut self.wide[..period * width];
            wide.fill(P::zero());
            let gains = &self.gains;
            matrix_add_ramped::<P>(players, &step[0], ramp_frames, &gains[0], width, wide);
            if let Some(analog) = analog {
                matrix_add_ramped::<P>(analog, &step[1], ramp_frames, &gains[1], width, wide);
            }
            // The ramp moves on, whether this period is heard or not.
            if ramp_frames > 0 {
                self.ramp_left -= ramp_frames;
                self.ramp_from = levels(&step);
            }
            wide
        };
        let samples = &mut out[..period * channels];
        let has_plugins = !self.mix.is_empty() || !self.output.is_empty();
        let chains_ran = plugins && has_plugins;
        // What reaches the sink: what the chains give, or the matrix when
        // they do not run.
        let end = if chains_ran {
            gives(&self.output, gives(&self.mix, width))
        } else {
            width
        };
        // A chain never runs at a rate it was not built for, such as after
        // a DoP stream, until the one for this rate comes. Silence, not a
        // bypass: a crossover or a protection filter must not be skipped.
        // Nor do chains run that do not end at the sink's count: a `mix`
        // chain waiting for its `output` chain, or chains that change the
        // count bypassed in forced bit-perfect. Nothing is routed to the
        // sink's channels then. The audio fades in when it plays again.
        let fits = self.mix.fits(rate) && self.output.fits(rate);
        if (chains_ran && !fits) || end != channels {
            samples.fill(0);
            self.unmute_left = (rate * FADE_MS / 1000) as usize;
            return false;
        }
        // Without plugins the mix is not copied for them at all.
        let finished: &[P::Acc] = if chains_ran {
            // After periods that did not pass them, so that a filter or a
            // delay does not play an old tail.
            if std::mem::take(&mut self.chains_idle) {
                self.mix.reset();
                self.output.reset();
            }
            let block = P::to_block(mixed, width, &mut self.store);
            let (block, _) = self.mix.run(block);
            let (block, _) = self.output.run(block);
            debug_assert_eq!(block.channels, channels);
            let wide = &mut self.wide[..period * channels];
            P::from_block(block, wide);
            wide
        } else {
            mixed
        };
        mix_finish_wide::<P>(finished, samples);
        if self.unmute_left > 0 {
            let fade = (rate * FADE_MS / 1000) as usize;
            unmute(samples, channels, &mut self.unmute_left, fade);
        }
        chains_ran
    }
}
