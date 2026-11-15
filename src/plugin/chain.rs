//! Chains: plugins run in order on one block.
//!
//! A chain is built on the control thread and handed to the audio thread.
//! It owns the buffers it converts in and the ones a change of the channel
//! count writes to, so `process` and `run` never allocate.

use super::{Block, Channels, Formats, Plugin, Samples, Status};
use crate::config::Format;
use crate::counters::{PluginStats, monotonic_ns};
use crate::format::BlockStore;
use crate::matrix::MAX_CHANNELS;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

/// Plugins in the order they run, with what each needs converted.
pub struct Chain {
    plugins: Vec<Box<dyn Plugin>>,
    /// For each plugin, the format its blocks are converted to, if it does
    /// not take the chain's own.
    convert: Vec<Option<Format>>,
    format: Format,
    /// Where a plugin that converts reads from and writes to, for blocks of
    /// up to `max_frames` frames at the widest count in the chain.
    store: BlockStore,
    /// The same, for the output of a plugin whose counts differ.
    store_out: BlockStore,
    /// A copy of a fading plugin's input, sized as `store`.
    dry: BlockStore,
    /// For each plugin, whether it is enabled, where its fade is and what
    /// it publishes.
    states: Vec<State>,
    /// The blocks a change of the channel count leaves the audio in, one
    /// after the other. Empty without a plugin whose counts differ.
    wide: [BlockStore; 2],
    /// For each plugin, the channels it takes and the channels it gives.
    counts: Vec<(usize, usize)>,
    /// A plugin's counts differ.
    changes: bool,
    /// The channels of the blocks the chain takes, and of the ones it gives.
    /// Both 0 for a chain that takes any count, which is `Chain::empty()`.
    channels_in: usize,
    channels_out: usize,
    max_frames: usize,
    /// The rate its plugins were built for. None runs at any rate.
    rate: Option<u32>,
}

impl Chain {
    /// A chain for blocks of `channels` channels in `format` of up to
    /// `max_frames` frames. A longer block given to `process` or `run`
    /// panics. The engine takes only chains built for
    /// `crate::latency::MAX_PERIOD`, so that one never depends on the
    /// period.
    ///
    /// A chain built here with no plugins reports `channels` as both its
    /// counts, unlike `Chain::empty()`.
    ///
    /// Fails with a message that names the plugin and gives both counts when
    /// a plugin does not fit the count that reaches it, or when a count is 0
    /// or above `MAX_CHANNELS`.
    pub fn new(
        plugins: Vec<Box<dyn Plugin>>,
        format: Format,
        max_frames: usize,
        channels: usize,
    ) -> Result<Chain, String> {
        let counts = fit(&plugins, channels).map_err(|(_, message)| message)?;
        let convert = plugins
            .iter()
            .map(|p| conversion(p.formats(), format))
            .collect();
        let widest = counts
            .iter()
            .flat_map(|&(i, o)| [i, o])
            .fold(channels, usize::max);
        let changes = counts.iter().filter(|(i, o)| i != o).count();
        let wide = |n: usize| match changes >= n {
            true => BlockStore::new(max_frames, widest),
            false => BlockStore::new(0, 1),
        };
        let states = plugins.iter().map(|_| State::default()).collect();
        Ok(Chain {
            dry: match plugins.is_empty() {
                true => BlockStore::new(0, 1),
                false => BlockStore::new(max_frames, widest),
            },
            states,
            plugins,
            convert,
            format,
            store: BlockStore::new(max_frames, widest),
            store_out: match changes {
                0 => BlockStore::new(0, 1),
                _ => BlockStore::new(max_frames, widest),
            },
            wide: [wide(1), wide(2)],
            channels_in: channels,
            channels_out: counts.last().map_or(channels, |&(_, o)| o),
            counts,
            changes: changes > 0,
            max_frames,
            rate: None,
        })
    }

    /// The same chain, marked as built for `rate`. The engine never runs
    /// it at another.
    pub fn for_rate(mut self, rate: u32) -> Chain {
        self.rate = Some(rate);
        self
    }

    /// The same chain, publishing to `stats`, one for each plugin in order.
    /// Panics on another number. Done on the control thread.
    pub fn with_stats(mut self, stats: Vec<Arc<PluginStats>>) -> Chain {
        assert_eq!(stats.len(), self.plugins.len(), "one stats per plugin");
        for (state, stats) in self.states.iter_mut().zip(stats) {
            state.stats = Some(stats);
        }
        for i in 0..self.plugins.len() {
            self.publish(i);
        }
        self
    }

    /// The rate the chain was built for, None if it names none.
    pub fn rate(&self) -> Option<u32> {
        self.rate
    }

    /// Whether the chain may run at `rate`: its own, or any for a chain
    /// that names none, such as one without plugins.
    pub fn fits(&self, rate: u32) -> bool {
        self.rate.is_none_or(|r| r == rate)
    }

    /// A chain without plugins, which takes blocks of any channel count and
    /// gives them back as they are. `channels_in` and `channels_out` are 0
    /// for it. Allocates nothing.
    pub fn empty() -> Chain {
        Chain {
            plugins: Vec::new(),
            convert: Vec::new(),
            format: Format::I32,
            store: BlockStore::new(0, 1),
            store_out: BlockStore::new(0, 1),
            dry: BlockStore::new(0, 1),
            states: Vec::new(),
            wide: [BlockStore::new(0, 1), BlockStore::new(0, 1)],
            counts: Vec::new(),
            changes: false,
            channels_in: 0,
            channels_out: 0,
            max_frames: 0,
            rate: None,
        }
    }

    /// The channels of the blocks the chain takes, or 0 if it takes any
    /// count, as an empty chain made by `empty` does.
    pub fn channels_in(&self) -> usize {
        self.channels_in
    }

    /// The channels of the blocks the chain gives, or 0 if it takes any
    /// count and keeps it, as an empty chain made by `empty` does.
    pub fn channels_out(&self) -> usize {
        self.channels_out
    }

    /// True if a plugin's channel counts differ.
    pub fn changes_channels(&self) -> bool {
        self.changes
    }

    /// Runs the plugins in order on `block`, which must be in the chain's
    /// format and have `channels_in` channels (only a debug build checks
    /// that), and be at most `max_frames` frames long (a longer block
    /// panics). The result is `block` itself, changed, if the chain kept the
    /// channel count, else a block in the chain's own memory with
    /// `channels_out` channels. `no_signal` is set if any plugin reported
    /// it.
    pub fn run<'a>(&'a mut self, mut block: Block<'a>) -> (Block<'a>, Status) {
        if !self.changes_channels() {
            let status = self.process(block.reborrow());
            return (block, status);
        }
        debug_assert_eq!(block.channels, self.channels_in);
        debug_assert_eq!(block.format(), self.format);
        let channels = block.channels;
        match block.samples {
            Samples::I32(d) => self.run_wide::<i32>(d, channels),
            Samples::F32(d) => self.run_wide::<f32>(d, channels),
            Samples::F64(d) => self.run_wide::<f64>(d, channels),
        }
    }

    /// `run` for a chain that changes the count, on samples of type `S`.
    fn run_wide<'a, S: Sample>(
        &'a mut self,
        input: &'a mut [S],
        channels: usize,
    ) -> (Block<'a>, Status) {
        let frames = input.len() / channels;
        assert!(
            frames <= self.max_frames,
            "a block longer than the chain takes"
        );
        let mut status = Status::default();
        let fade = self.fade_frames();
        // Where the audio is: the caller's block, or one of `wide`.
        let mut at = None;
        let mut width = channels;
        for (((plugin, convert), &(cin, cout)), state) in self
            .plugins
            .iter_mut()
            .zip(&self.convert)
            .zip(&self.counts)
            .zip(&mut self.states)
        {
            let plugin = plugin.as_mut();
            let (in_len, out_len) = (frames * cin, frames * cout);
            let s = if cin == cout {
                let data = match at {
                    None => &mut *input,
                    Some(i) => &mut S::buffer(&mut self.wide[i])[..in_len],
                };
                let stores = (&mut self.store, &mut self.dry);
                stage(plugin, *convert, data, cin, stores, state, fade)
            } else {
                let (first, second) = self.wide.split_at_mut(1);
                let (first, second) = (&mut first[0], &mut second[0]);
                let (src, dst): (&mut [S], &mut [S]) = match at {
                    None => (&mut *input, &mut S::buffer(first)[..out_len]),
                    Some(0) => (
                        &mut S::buffer(first)[..in_len],
                        &mut S::buffer(second)[..out_len],
                    ),
                    Some(_) => (
                        &mut S::buffer(second)[..in_len],
                        &mut S::buffer(first)[..out_len],
                    ),
                };
                let s = timed(state.stats.as_deref(), || match *convert {
                    None => plugin.process_into(S::block(src, cin), S::block(dst, cout)),
                    Some(to) => converted_into(
                        plugin,
                        (src, cin),
                        (&mut *dst, cout),
                        to,
                        (&mut self.store, &mut self.store_out),
                    ),
                });
                at = Some(usize::from(at == Some(0)));
                s
            };
            width = cout;
            status.no_signal |= s.no_signal;
            status.changed |= s.changed;
        }
        let block = match at {
            None => S::block(input, width),
            Some(i) => S::block(&mut S::buffer(&mut self.wide[i])[..frames * width], width),
        };
        (block, status)
    }

    /// Runs the plugins in order on `block`, which must be in the chain's
    /// format (only a debug build checks that) and at most `max_frames`
    /// frames long (a longer block panics). For a chain that keeps the
    /// channel count: one that changes it panics, so use `run`.
    /// `no_signal` is set if any plugin reported it.
    pub fn process(&mut self, block: Block<'_>) -> Status {
        assert!(
            !self.changes_channels(),
            "a chain that changes the channel count is run with `run`"
        );
        debug_assert!(self.plugins.is_empty() || block.format() == self.format);
        debug_assert!(self.channels_in == 0 || block.channels == self.channels_in);
        let channels = block.channels;
        match block.samples {
            Samples::I32(d) => self.process_as(d, channels),
            Samples::F32(d) => self.process_as(d, channels),
            Samples::F64(d) => self.process_as(d, channels),
        }
    }

    /// `process` on samples of type `S`.
    fn process_as<S: Sample>(&mut self, data: &mut [S], channels: usize) -> Status {
        let fade = self.fade_frames();
        let mut status = Status::default();
        for ((plugin, convert), state) in self
            .plugins
            .iter_mut()
            .zip(&self.convert)
            .zip(&mut self.states)
        {
            let stores = (&mut self.store, &mut self.dry);
            let s = stage(
                plugin.as_mut(),
                *convert,
                data,
                channels,
                stores,
                state,
                fade,
            );
            status.no_signal |= s.no_signal;
            status.changed |= s.changed;
        }
        status
    }

    /// Resets every plugin, so that none plays a tail of audio from before
    /// a gap in the blocks it was given.
    pub fn reset(&mut self) {
        for plugin in &mut self.plugins {
            plugin.reset();
        }
    }

    /// The names of the plugins, in the order they run.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.plugins.iter().map(|p| p.name())
    }

    /// True if the chain has no plugins.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// The longest block `process` takes. A chain without plugins takes any.
    pub fn max_frames(&self) -> usize {
        if self.plugins.is_empty() {
            usize::MAX
        } else {
            self.max_frames
        }
    }

    /// True if every enabled plugin is neutral and no fade runs, and so for
    /// an empty chain. Never true for a chain that changes the channel
    /// count.
    pub fn neutral(&self) -> bool {
        let fade = self.fade_frames();
        !self.changes_channels()
            && (self.plugins.iter().zip(&self.states)).all(|(p, s)| s.neutral(p.as_ref(), fade))
    }

    /// The delay of the enabled plugins together.
    pub fn latency_frames(&self) -> u32 {
        (self.plugins.iter().zip(&self.states))
            .filter(|(_, s)| s.enabled)
            .fold(0u32, |sum, (p, _)| sum.saturating_add(p.latency_frames()))
    }

    /// Sets parameter `index` of plugin `plugin`. An unknown plugin or
    /// parameter index is ignored.
    pub fn set_param(&mut self, plugin: usize, index: usize, value: f64) {
        if let Some(p) = self.plugins.get_mut(plugin)
            && index < p.params().len()
        {
            p.set_param(index, value);
            self.publish(plugin);
        }
    }

    /// `set_param` without smoothing, for a chain that has not run: the
    /// plugin starts at the value. Done on the control thread, before the
    /// chain goes to the audio thread.
    pub fn set_param_now(&mut self, plugin: usize, index: usize, value: f64) {
        self.set_param(plugin, index, value);
        if let Some(p) = self.plugins.get_mut(plugin) {
            p.settle();
            self.publish(plugin);
        }
    }

    /// Enables or disables plugin `plugin`, crossfading over 10 ms between
    /// its input and its output. A disabled plugin is passed over once its
    /// fade has ended, and one enabled from there is reset first, so it
    /// plays no old tail. An unknown plugin, or one whose counts differ, is
    /// ignored. Does not allocate.
    pub fn set_enabled(&mut self, plugin: usize, enabled: bool) {
        let fade = self.fade_frames();
        let (Some(p), Some(state), Some(&(cin, cout))) = (
            self.plugins.get_mut(plugin),
            self.states.get_mut(plugin),
            self.counts.get(plugin),
        ) else {
            return;
        };
        if cin != cout || state.enabled == enabled {
            return;
        }
        if enabled && state.dry == fade {
            p.reset();
        }
        state.enabled = enabled;
        self.publish(plugin);
    }

    /// `set_enabled` without a fade, for a chain that has not run: a
    /// plugin disabled here is passed over from the first block. Done on
    /// the control thread, before the chain goes to the audio thread.
    pub fn set_enabled_now(&mut self, plugin: usize, enabled: bool) {
        let fade = self.fade_frames();
        self.set_enabled(plugin, enabled);
        if let Some(state) = self.states.get_mut(plugin) {
            state.dry = state.target(fade);
            self.publish(plugin);
        }
    }

    /// Whether plugin `plugin` is enabled, false for an unknown one. True
    /// from `set_enabled(plugin, true)` on, also while it fades in.
    pub fn enabled(&self, plugin: usize) -> bool {
        self.states.get(plugin).is_some_and(|s| s.enabled)
    }

    /// The frames a fade lasts: 10 ms at the chain's rate, 480 for a chain
    /// that names none.
    fn fade_frames(&self) -> u32 {
        self.rate.map_or(480, |r| (r / 100).max(1))
    }

    /// Writes what plugin `plugin` publishes, if it has stats.
    fn publish(&self, plugin: usize) {
        let fade = self.fade_frames();
        publish(self.plugins[plugin].as_ref(), &self.states[plugin], fade);
    }
}

/// What a chain keeps for each plugin besides the plugin.
#[derive(Debug)]
struct State {
    enabled: bool,
    /// How far the fade toward the plugin's input has come, in frames: 0
    /// is its output alone, the fade's length its input alone.
    dry: u32,
    stats: Option<Arc<PluginStats>>,
}

impl Default for State {
    fn default() -> State {
        State {
            enabled: true,
            dry: 0,
            stats: None,
        }
    }
}

impl State {
    /// Where the fade of `fade` frames goes.
    fn target(&self, fade: u32) -> u32 {
        if self.enabled { 0 } else { fade }
    }

    /// Neutral as the chain counts it: no fade runs, and the plugin is
    /// disabled or neutral itself.
    fn neutral(&self, plugin: &dyn Plugin, fade: u32) -> bool {
        self.dry == self.target(fade) && (!self.enabled || plugin.neutral())
    }
}

/// Writes whether `plugin` is neutral and its latency to its stats, if it
/// has them.
fn publish(plugin: &dyn Plugin, state: &State, fade: u32) {
    if let Some(stats) = &state.stats {
        stats.neutral.store(state.neutral(plugin, fade), Relaxed);
        let latency = if state.enabled {
            plugin.latency_frames()
        } else {
            0
        };
        stats.latency_frames.store(latency, Relaxed);
    }
}

/// Runs `run`, and if there are `stats`, counts it there with the time it
/// took.
fn timed(stats: Option<&PluginStats>, run: impl FnOnce() -> Status) -> Status {
    let Some(stats) = stats else {
        return run();
    };
    let start = monotonic_ns();
    let status = run();
    let ns = monotonic_ns().saturating_sub(start);
    stats.runs.fetch_add(1, Relaxed);
    stats.total_ns.fetch_add(ns, Relaxed);
    stats.worst_ns.fetch_max(ns, Relaxed);
    status
}

/// Runs a plugin whose counts are the same on `data` in place, converted
/// if `convert` names a format, timed, and crossfaded with its input while
/// a fade of `fade` frames runs. A disabled plugin whose fade has ended is
/// passed over.
fn stage<S: Sample>(
    plugin: &mut dyn Plugin,
    convert: Option<Format>,
    data: &mut [S],
    channels: usize,
    (store, dry): (&mut BlockStore, &mut BlockStore),
    state: &mut State,
    fade: u32,
) -> Status {
    let target = state.target(fade);
    if !state.enabled && state.dry == target {
        return Status::default();
    }
    let fading = state.dry != target;
    if fading {
        S::buffer(dry)[..data.len()].copy_from_slice(data);
    }
    let mut status = timed(state.stats.as_deref(), || match convert {
        None => plugin.process(S::block(data, channels)),
        Some(to) => converted(plugin, data, channels, to, store),
    });
    if status.changed && !fading {
        // A plugin that changed a block may be neutral after it, as a
        // balance whose ramp ends at the centre is.
        publish(plugin, state, fade);
    }
    if fading {
        // The chain may be neutral again by the end of the block, but this
        // block was crossfaded, so it must be played as processed.
        status.changed = true;
        let input = &S::buffer(dry)[..data.len()];
        for (out, input) in data
            .chunks_exact_mut(channels)
            .zip(input.chunks_exact(channels))
        {
            // The share of the plugin's output.
            let g = 1.0 - f64::from(state.dry) / f64::from(fade);
            for (o, i) in out.iter_mut().zip(input) {
                *o = S::from_unit(i.to_unit() * (1.0 - g) + o.to_unit() * g);
            }
            match state.dry.cmp(&target) {
                std::cmp::Ordering::Less => state.dry += 1,
                std::cmp::Ordering::Greater => state.dry -= 1,
                std::cmp::Ordering::Equal => {}
            }
        }
        if state.dry == target {
            publish(plugin, state, fade);
        }
    }
    status
}

/// Why plugins do not fit a channel count: the index of the plugin, if one
/// is at fault, and a message that names it and gives both counts.
pub(super) type Misfit = (Option<usize>, String);

/// The channels each plugin takes and gives, in a chain given blocks of
/// `channels` channels, or why they do not fit.
pub(super) fn fit(
    plugins: &[Box<dyn Plugin>],
    channels: usize,
) -> Result<Vec<(usize, usize)>, Misfit> {
    let valid = |n: usize| (1..=MAX_CHANNELS).contains(&n);
    if !valid(channels) {
        return Err((
            None,
            format!("{channels} channels is not from 1 to {MAX_CHANNELS}"),
        ));
    }
    let mut counts = Vec::with_capacity(plugins.len());
    let mut width = channels;
    for (i, plugin) in plugins.iter().enumerate() {
        let (input, output) = match plugin.channels() {
            Channels::Any => (width, width),
            Channels::Fixed { input, output } => (input, output),
        };
        if input != width {
            let name = plugin.name();
            let m = format!("{name} takes {input} channels, but {width} reach it");
            return Err((Some(i), m));
        }
        if !valid(output) {
            let name = plugin.name();
            let m = format!("{name} gives {output} channels, not from 1 to {MAX_CHANNELS}");
            return Err((Some(i), m));
        }
        counts.push((input, output));
        width = output;
    }
    Ok(counts)
}

/// The format a plugin that takes `formats` is given in a chain of
/// `format`: its own if it takes it, else `f64`, `f32` or `i32` in that
/// order. `None` means no conversion. A plugin that names no format at all
/// is given the chain's.
pub fn conversion(formats: Formats, format: Format) -> Option<Format> {
    if formats.has(format) {
        None
    } else {
        [Format::F64, Format::F32, Format::I32]
            .into_iter()
            .find(|&f| formats.has(f))
    }
}

/// Full scale of S32 as a float, 2^31.
const SCALE: f64 = 2_147_483_648.0;

/// A sample type a block can hold, converted through `f64` at full scale
/// 1.0.
trait Sample: Copy {
    fn to_unit(self) -> f64;
    fn from_unit(v: f64) -> Self;
    fn buffer(store: &mut BlockStore) -> &mut [Self];
    fn block(data: &mut [Self], channels: usize) -> Block<'_>;
}

impl Sample for i32 {
    fn to_unit(self) -> f64 {
        f64::from(self) / SCALE
    }

    /// Rounds to nearest and saturates, as `Processing::finish` does.
    fn from_unit(v: f64) -> i32 {
        (v * SCALE).round() as i32
    }

    fn buffer(store: &mut BlockStore) -> &mut [i32] {
        &mut store.i32
    }

    fn block(data: &mut [i32], channels: usize) -> Block<'_> {
        Block {
            channels,
            samples: Samples::I32(data),
        }
    }
}

impl Sample for f32 {
    fn to_unit(self) -> f64 {
        f64::from(self)
    }

    fn from_unit(v: f64) -> f32 {
        v as f32
    }

    fn buffer(store: &mut BlockStore) -> &mut [f32] {
        &mut store.f32
    }

    fn block(data: &mut [f32], channels: usize) -> Block<'_> {
        Block {
            channels,
            samples: Samples::F32(data),
        }
    }
}

impl Sample for f64 {
    fn to_unit(self) -> f64 {
        self
    }

    fn from_unit(v: f64) -> f64 {
        v
    }

    fn buffer(store: &mut BlockStore) -> &mut [f64] {
        &mut store.f64
    }

    fn block(data: &mut [f64], channels: usize) -> Block<'_> {
        Block {
            channels,
            samples: Samples::F64(data),
        }
    }
}

/// Runs `plugin` on `data` converted to `to`, and converts the result back.
fn converted<S: Sample>(
    plugin: &mut dyn Plugin,
    data: &mut [S],
    channels: usize,
    to: Format,
    store: &mut BlockStore,
) -> Status {
    match to {
        Format::I32 => through::<S, i32>(plugin, data, channels, store),
        Format::F32 => through::<S, f32>(plugin, data, channels, store),
        Format::F64 => through::<S, f64>(plugin, data, channels, store),
    }
}

/// Runs a plugin whose counts differ on `src`, converted to `to`, and
/// converts what it wrote back into `dst`. Each is a block's samples and
/// its channels.
fn converted_into<S: Sample>(
    plugin: &mut dyn Plugin,
    src: (&mut [S], usize),
    dst: (&mut [S], usize),
    to: Format,
    stores: (&mut BlockStore, &mut BlockStore),
) -> Status {
    match to {
        Format::I32 => through_into::<S, i32>(plugin, src, dst, stores),
        Format::F32 => through_into::<S, f32>(plugin, src, dst, stores),
        Format::F64 => through_into::<S, f64>(plugin, src, dst, stores),
    }
}

fn through_into<S: Sample, T: Sample>(
    plugin: &mut dyn Plugin,
    (src, cin): (&mut [S], usize),
    (dst, cout): (&mut [S], usize),
    (store, store_out): (&mut BlockStore, &mut BlockStore),
) -> Status {
    let input = &mut T::buffer(store)[..src.len()];
    for (b, d) in input.iter_mut().zip(src.iter()) {
        *b = T::from_unit(d.to_unit());
    }
    let output = &mut T::buffer(store_out)[..dst.len()];
    let status = plugin.process_into(T::block(input, cin), T::block(output, cout));
    for (d, b) in dst.iter_mut().zip(output.iter()) {
        *d = S::from_unit(b.to_unit());
    }
    status
}

fn through<S: Sample, T: Sample>(
    plugin: &mut dyn Plugin,
    data: &mut [S],
    channels: usize,
    store: &mut BlockStore,
) -> Status {
    let buf = &mut T::buffer(store)[..data.len()];
    for (b, d) in buf.iter_mut().zip(data.iter()) {
        *b = T::from_unit(d.to_unit());
    }
    let status = plugin.process(T::block(buf, channels));
    for (d, b) in data.iter_mut().zip(buf.iter()) {
        *d = S::from_unit(b.to_unit());
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{F32, F64, I32, Processing};
    use crate::plugin::{Param, ParamKind};
    use assert_no_alloc::assert_no_alloc;
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::{Arc, Mutex};

    const FULL: f64 = 2_147_483_648.0;
    const ALL: Formats = Formats {
        i32: true,
        f32: true,
        f64: true,
    };

    /// What a plugin saw: the format and the first sample at full scale 1.0.
    type Seen = Arc<Mutex<Option<(Format, f64)>>>;

    /// A plugin for the tests. It applies `op` with its one parameter to
    /// every sample, seen at full scale 1.0.
    struct Test {
        formats: Formats,
        op: fn(f64, f64) -> f64,
        params: Vec<Param>,
        value: f64,
        neutral: bool,
        /// Neutral only at this value, if given.
        neutral_at: Option<f64>,
        latency: u32,
        no_signal: bool,
        seen: Option<Seen>,
    }

    impl Test {
        fn new(op: fn(f64, f64) -> f64, value: f64) -> Test {
            Test {
                formats: ALL,
                op,
                params: vec![Param {
                    name: "value".into(),
                    unit: String::new(),
                    kind: ParamKind::Float,
                    min: -100.0,
                    max: 100.0,
                    default: value,
                }],
                value,
                neutral: true,
                neutral_at: None,
                latency: 0,
                no_signal: false,
                seen: None,
            }
        }

        fn only(mut self, formats: Formats) -> Test {
            self.formats = formats;
            self
        }

        fn boxed(self) -> Box<dyn Plugin> {
            Box::new(self)
        }
    }

    fn add(value: f64) -> Test {
        Test::new(|s, v| s + v, value)
    }

    fn mul(value: f64) -> Test {
        Test::new(|s, v| s * v, value)
    }

    /// A plugin that leaves the audio as it is and records what it saw.
    fn recorder(formats: Formats) -> (Test, Seen) {
        let seen: Seen = Arc::default();
        let mut t = mul(1.0).only(formats);
        t.seen = Some(seen.clone());
        (t, seen)
    }

    fn formats(i32: bool, f32: bool, f64: bool) -> Formats {
        Formats { i32, f32, f64 }
    }

    impl Plugin for Test {
        fn name(&self) -> &str {
            "test"
        }

        fn formats(&self) -> Formats {
            self.formats
        }

        fn params(&self) -> &[Param] {
            &self.params
        }

        fn set_param(&mut self, index: usize, value: f64) {
            assert_eq!(index, 0);
            self.value = value;
        }

        fn neutral(&self) -> bool {
            self.neutral && self.neutral_at.is_none_or(|v| v == self.value)
        }

        fn latency_frames(&self) -> u32 {
            self.latency
        }

        fn process(&mut self, block: Block<'_>) -> Status {
            assert!(self.formats.has(block.format()), "{:?}", block.format());
            let (op, v) = (self.op, self.value);
            let first = match block.samples {
                Samples::I32(d) => {
                    let first = f64::from(d[0]) / FULL;
                    for s in d.iter_mut() {
                        *s = (op(f64::from(*s) / FULL, v) * FULL).round() as i32;
                    }
                    (Format::I32, first)
                }
                Samples::F32(d) => {
                    let first = f64::from(d[0]);
                    for s in d.iter_mut() {
                        *s = op(f64::from(*s), v) as f32;
                    }
                    (Format::F32, first)
                }
                Samples::F64(d) => {
                    let first = d[0];
                    for s in d.iter_mut() {
                        *s = op(*s, v);
                    }
                    (Format::F64, first)
                }
            };
            if let Some(seen) = &self.seen {
                *seen.lock().unwrap() = Some(first);
            }
            Status {
                no_signal: self.no_signal,
                changed: false,
            }
        }
    }

    fn chain(plugins: Vec<Test>, format: Format) -> Chain {
        let plugins = plugins.into_iter().map(Test::boxed).collect();
        Chain::new(plugins, format, 16, 2).unwrap()
    }

    /// Runs `chain` on one f64 frame.
    fn run_f64(chain: &mut Chain, v: f64) -> f64 {
        let mut d = [[v, v]];
        chain.process(Block {
            channels: 2,
            samples: Samples::F64(d.as_flattened_mut()),
        });
        d[0][0]
    }

    #[test]
    fn plugins_run_in_the_order_given() {
        let mut c = chain(vec![add(0.125), mul(2.0)], Format::F64);
        assert_eq!(run_f64(&mut c, 0.25), 0.75);
        let mut c = chain(vec![mul(2.0), add(0.125)], Format::F64);
        assert_eq!(run_f64(&mut c, 0.25), 0.625);
    }

    #[test]
    fn an_empty_chain_changes_nothing_and_is_neutral() {
        let mut i = [[1, -7], [i32::MAX, i32::MIN]];
        let mut f = [[0.5f32, -2.0]];
        let mut d = [[0.25, 9.0]];
        assert_no_alloc(|| {
            let mut c = Chain::empty();
            assert!(c.neutral());
            assert_eq!(c.latency_frames(), 0);
            assert_eq!(
                c.process(Block {
                    channels: 2,
                    samples: Samples::I32(i.as_flattened_mut())
                }),
                Status::default()
            );
            c.process(Block {
                channels: 2,
                samples: Samples::F32(f.as_flattened_mut()),
            });
            c.process(Block {
                channels: 2,
                samples: Samples::F64(d.as_flattened_mut()),
            });
            c.set_param(0, 0, 1.0);
        });
        assert_eq!(i, [[1, -7], [i32::MAX, i32::MIN]]);
        assert_eq!(f, [[0.5, -2.0]]);
        assert_eq!(d, [[0.25, 9.0]]);
    }

    #[test]
    fn a_plugin_without_the_format_is_converted_around() {
        let (r, seen) = recorder(formats(false, false, true));
        let mut c = chain(vec![r], Format::I32);
        let mut d = [[1 << 30, -(1 << 30)]];
        c.process(Block {
            channels: 2,
            samples: Samples::I32(d.as_flattened_mut()),
        });
        assert_eq!(*seen.lock().unwrap(), Some((Format::F64, 0.5)));
        assert_eq!(d, [[1 << 30, -(1 << 30)]]);

        // On the way back the chain rounds to nearest and saturates.
        let mut c = chain(
            vec![mul(2.6).only(formats(false, false, true))],
            Format::I32,
        );
        let mut d = [[1, -(1 << 30)], [1 << 30, -1]];
        c.process(Block {
            channels: 2,
            samples: Samples::I32(d.as_flattened_mut()),
        });
        assert_eq!(d, [[3, i32::MIN], [i32::MAX, -3]]);
    }

    #[test]
    fn conversion_prefers_f64_then_f32_then_i32() {
        let cases = [
            (formats(true, true, true), Format::F32, Format::F32),
            (formats(false, true, true), Format::I32, Format::F64),
            (formats(true, true, false), Format::F64, Format::F32),
            (formats(true, true, false), Format::I32, Format::I32),
            (formats(true, false, false), Format::F32, Format::I32),
            (formats(false, true, false), Format::F64, Format::F32),
        ];
        for (takes, format, expect) in cases {
            let (r, seen) = recorder(takes);
            let mut c = chain(vec![r], format);
            let mut i = [[1 << 29; 2]];
            let mut f = [[0.25f32; 2]];
            let mut d = [[0.25; 2]];
            let block = match format {
                Format::I32 => Block {
                    channels: 2,
                    samples: Samples::I32(i.as_flattened_mut()),
                },
                Format::F32 => Block {
                    channels: 2,
                    samples: Samples::F32(f.as_flattened_mut()),
                },
                Format::F64 => Block {
                    channels: 2,
                    samples: Samples::F64(d.as_flattened_mut()),
                },
            };
            c.process(block);
            assert_eq!(
                *seen.lock().unwrap(),
                Some((expect, 0.25)),
                "{takes:?} in {format:?}"
            );
        }
    }

    #[test]
    fn a_chain_tells_whether_it_has_plugins_and_how_long_a_block_it_takes() {
        assert!(Chain::empty().is_empty());
        assert_eq!(Chain::empty().max_frames(), usize::MAX);
        let c = chain(vec![add(0.0)], Format::F32);
        assert!(!c.is_empty());
        assert_eq!(c.max_frames(), 16);
    }

    #[test]
    fn a_chain_is_neutral_only_if_every_plugin_is() {
        let mut busy = mul(2.0);
        busy.neutral = false;
        assert!(chain(vec![add(0.0), mul(1.0)], Format::F64).neutral());
        assert!(!chain(vec![add(0.0), busy, mul(1.0)], Format::F64).neutral());
    }

    #[test]
    fn latency_is_the_sum_of_the_plugins() {
        let mut a = add(0.0);
        a.latency = 3;
        let mut b = add(0.0);
        b.latency = 64;
        let c = chain(vec![a, add(0.0), b], Format::F32);
        assert_eq!(c.latency_frames(), 67);
    }

    #[test]
    fn no_signal_from_any_plugin_is_reported() {
        let mut quiet = add(0.0);
        quiet.no_signal = true;
        let mut c = chain(vec![add(0.0), quiet, add(0.0)], Format::F64);
        let mut d = [[0.0; 2]];
        assert!(
            c.process(Block {
                channels: 2,
                samples: Samples::F64(d.as_flattened_mut())
            })
            .no_signal
        );
        let mut c = chain(vec![add(0.0), add(0.0)], Format::F64);
        assert!(
            !c.process(Block {
                channels: 2,
                samples: Samples::F64(d.as_flattened_mut())
            })
            .no_signal
        );
    }

    #[test]
    fn a_float_block_is_not_clipped_between_plugins() {
        let mut c = chain(vec![mul(4.0), mul(0.25)], Format::F64);
        assert_eq!(run_f64(&mut c, 0.9), 0.9);
        let mut c = chain(vec![mul(4.0), mul(0.25)], Format::F32);
        let mut f = [[0.9f32, -0.9]];
        c.process(Block {
            channels: 2,
            samples: Samples::F32(f.as_flattened_mut()),
        });
        assert_eq!(f, [[0.9, -0.9]]);
        // Nor where an f32 chain converts for an f64 plugin.
        let mut c = chain(
            vec![mul(4.0).only(formats(false, false, true)), mul(0.25)],
            Format::F32,
        );
        c.process(Block {
            channels: 2,
            samples: Samples::F32(f.as_flattened_mut()),
        });
        assert_eq!(f, [[0.9, -0.9]]);
    }

    #[test]
    fn an_i32_block_is_clipped_before_the_first_plugin() {
        let mut acc = [[i64::from(i32::MAX) + 1000, i64::from(i32::MIN) - 1000]];
        let mut store = BlockStore::new(1, 2);
        let mut c = chain(
            vec![mul(0.5).only(formats(true, false, false))],
            Format::I32,
        );
        let mut block = I32::to_block(acc.as_flattened_mut(), 2, &mut store);
        c.process(block.reborrow());
        I32::from_block(block, acc.as_flattened_mut());
        // Half of the clipped values, not of the sums.
        assert_eq!(acc, [[1 << 30, -(1 << 30)]]);
    }

    #[test]
    fn set_param_reaches_its_plugin_and_ignores_what_is_not_there() {
        let mut c = chain(vec![add(0.0), mul(1.0)], Format::F64);
        c.set_param(1, 0, 2.0);
        c.set_param(1, 1, 5.0);
        c.set_param(2, 0, 5.0);
        assert_eq!(run_f64(&mut c, 0.25), 0.5);
    }

    /// Mixes, runs a chain that converts for one plugin, and finishes, with
    /// allocation checked.
    fn cycle_without_allocating<P: Processing>(format: Format, other: Formats) {
        let frames = 64;
        let input = vec![[1 << 28, -(1 << 28)]; frames];
        let mut acc = vec![[P::zero(); 2]; frames];
        let mut out = vec![[0; 2]; frames];
        let mut store = BlockStore::new(frames, 2);
        let plugins = vec![add(0.0).boxed(), mul(2.0).only(other).boxed()];
        let mut c = Chain::new(plugins, format, frames, 2).unwrap();
        assert_no_alloc(|| {
            crate::format::mix_clear::<P>(&mut acc);
            crate::format::mix_add::<P>(&input, crate::format::Ramp::flat(1.0), &mut acc);
            let mut block = P::to_block(acc.as_flattened_mut(), 2, &mut store);
            c.process(block.reborrow());
            P::from_block(block, acc.as_flattened_mut());
            crate::format::mix_finish::<P>(&acc, &mut out);
        });
        assert_eq!(out[0], [1 << 29, -(1 << 29)], "{format:?}");
    }

    #[test]
    fn processing_does_not_allocate() {
        cycle_without_allocating::<I32>(Format::I32, formats(false, false, true));
        cycle_without_allocating::<F32>(Format::F32, formats(true, false, false));
        cycle_without_allocating::<F64>(Format::F64, formats(false, true, false));
    }

    /// Fixed 2 to 4: copies left to channels 1 and 3, right to 2 and 4.
    struct Wide {
        formats: Formats,
    }

    impl Plugin for Wide {
        fn name(&self) -> &str {
            "wide"
        }

        fn formats(&self) -> Formats {
            self.formats
        }

        fn channels(&self) -> Channels {
            Channels::Fixed {
                input: 2,
                output: 4,
            }
        }

        fn params(&self) -> &[Param] {
            &[]
        }

        fn set_param(&mut self, _: usize, _: f64) {}

        fn neutral(&self) -> bool {
            true
        }

        fn latency_frames(&self) -> u32 {
            0
        }

        fn process(&mut self, _: Block<'_>) -> Status {
            unreachable!()
        }

        fn process_into(&mut self, input: Block<'_>, output: Block<'_>) -> Status {
            assert!(self.formats.has(input.format()));
            assert_eq!((input.channels, output.channels), (2, 4));
            match (input.samples, output.samples) {
                (Samples::F32(i), Samples::F32(o)) => widen(i, o),
                (Samples::F64(i), Samples::F64(o)) => widen(i, o),
                (Samples::I32(i), Samples::I32(o)) => widen(i, o),
                _ => panic!("formats differ"),
            }
            Status::default()
        }
    }

    fn widen<T: Copy>(input: &[T], output: &mut [T]) {
        assert_eq!(input.len() * 2, output.len());
        for (i, o) in input.chunks_exact(2).zip(output.chunks_exact_mut(4)) {
            o.copy_from_slice(&[i[0], i[1], i[0], i[1]]);
        }
    }

    /// Any count: halves every sample.
    struct Scale;

    impl Plugin for Scale {
        fn name(&self) -> &str {
            "scale"
        }

        fn formats(&self) -> Formats {
            Formats {
                i32: false,
                f32: true,
                f64: true,
            }
        }

        fn channels(&self) -> Channels {
            Channels::Any
        }

        fn params(&self) -> &[Param] {
            &[]
        }

        fn set_param(&mut self, _: usize, _: f64) {}

        fn neutral(&self) -> bool {
            true
        }

        fn latency_frames(&self) -> u32 {
            0
        }

        fn process(&mut self, block: Block<'_>) -> Status {
            match block.samples {
                Samples::F32(d) => d.iter_mut().for_each(|s| *s *= 0.5),
                Samples::F64(d) => d.iter_mut().for_each(|s| *s *= 0.5),
                Samples::I32(_) => unreachable!(),
            }
            Status::default()
        }
    }

    fn wide() -> Box<dyn Plugin> {
        Box::new(Wide { formats: ALL })
    }

    #[test]
    fn a_chain_that_widens_gives_its_own_block_with_four_channels() {
        let mut c = Chain::new(vec![wide()], Format::F32, 16, 2).unwrap();
        assert_eq!((c.channels_in(), c.channels_out()), (2, 4));
        let mut d = [1.0f32, 2.0, 3.0, 4.0];
        let (block, _) = c.run(Block {
            channels: 2,
            samples: Samples::F32(&mut d),
        });
        assert_eq!(block.channels, 4);
        let Samples::F32(out) = block.samples else {
            panic!("not f32")
        };
        assert_eq!(out, [1.0, 2.0, 1.0, 2.0, 3.0, 4.0, 3.0, 4.0]);
        assert_eq!(d, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn a_plugin_for_any_count_runs_after_one_that_widens() {
        let plugins = vec![wide(), Box::new(Scale) as Box<dyn Plugin>];
        let mut c = Chain::new(plugins, Format::F64, 16, 2).unwrap();
        assert_eq!(c.channels_out(), 4);
        let mut d = [1.0f64, 2.0];
        let (block, _) = c.run(Block {
            channels: 2,
            samples: Samples::F64(&mut d),
        });
        assert_eq!(block.channels, 4);
        let Samples::F64(out) = block.samples else {
            panic!("not f64")
        };
        assert_eq!(out, [0.5, 1.0, 0.5, 1.0]);
    }

    #[test]
    fn a_chain_that_widens_twice_alternates_its_buffers() {
        // Widens, narrows and widens again, so both buffers are used.
        struct Narrow;
        impl Plugin for Narrow {
            fn name(&self) -> &str {
                "narrow"
            }
            fn formats(&self) -> Formats {
                ALL
            }
            fn channels(&self) -> Channels {
                Channels::Fixed {
                    input: 4,
                    output: 2,
                }
            }
            fn params(&self) -> &[Param] {
                &[]
            }
            fn set_param(&mut self, _: usize, _: f64) {}
            fn neutral(&self) -> bool {
                true
            }
            fn latency_frames(&self) -> u32 {
                0
            }
            fn process(&mut self, _: Block<'_>) -> Status {
                unreachable!()
            }
            fn process_into(&mut self, input: Block<'_>, output: Block<'_>) -> Status {
                let (Samples::F32(i), Samples::F32(o)) = (input.samples, output.samples) else {
                    panic!("not f32")
                };
                for (i, o) in i.chunks_exact(4).zip(o.chunks_exact_mut(2)) {
                    o[0] = i[0] + i[2];
                    o[1] = i[1] + i[3];
                }
                Status::default()
            }
        }
        let plugins = vec![wide(), Box::new(Narrow) as Box<dyn Plugin>, wide()];
        let mut c = Chain::new(plugins, Format::F32, 16, 2).unwrap();
        assert_eq!((c.channels_in(), c.channels_out()), (2, 4));
        let mut d = [1.0f32, 2.0, 3.0, 4.0];
        let (block, _) = assert_no_alloc(|| {
            c.run(Block {
                channels: 2,
                samples: Samples::F32(&mut d),
            })
        });
        let Samples::F32(out) = block.samples else {
            panic!("not f32")
        };
        assert_eq!(out, [2.0, 4.0, 2.0, 4.0, 6.0, 8.0, 6.0, 8.0]);
    }

    #[test]
    fn a_plugin_at_the_wrong_channel_count_is_refused_with_both_counts() {
        let balance = crate::plugin::builtin::create("balance", 48_000, 2)
            .unwrap()
            .unwrap();
        let e = Chain::new(vec![wide(), balance], Format::F32, 16, 2)
            .err()
            .unwrap();
        assert!(
            e.contains("balance") && e.contains('2') && e.contains('4'),
            "{e}"
        );
    }

    #[test]
    fn a_chain_that_changes_the_count_is_never_neutral() {
        let c = Chain::new(vec![wide()], Format::F32, 16, 2).unwrap();
        assert!(c.changes_channels());
        assert!(!c.neutral());
        let c = chain(vec![add(0.0)], Format::F32);
        assert!(!c.changes_channels());
        assert!(c.neutral());
    }

    #[test]
    fn a_chain_keeps_the_count_it_is_given_without_fixed_plugins() {
        let mut c = Chain::new(vec![Box::new(Scale)], Format::F32, 16, 6).unwrap();
        assert_eq!((c.channels_in(), c.channels_out()), (6, 6));
        assert!(!c.changes_channels());
        let mut d = [1.0f32; 12];
        let (block, _) = c.run(Block {
            channels: 6,
            samples: Samples::F32(&mut d),
        });
        assert_eq!(block.channels, 6);
        let Samples::F32(out) = block.samples else {
            panic!("not f32")
        };
        assert_eq!(out, [0.5; 12]);
    }

    #[test]
    fn a_widening_plugin_that_wants_a_conversion_gets_it() {
        let only64 = formats(false, false, true);
        let plugin = Box::new(Wide { formats: only64 });
        let mut c = Chain::new(vec![plugin], Format::F32, 16, 2).unwrap();
        let mut d = [0.25f32, -0.5];
        let (block, _) = assert_no_alloc(|| {
            c.run(Block {
                channels: 2,
                samples: Samples::F32(&mut d),
            })
        });
        assert_eq!(block.channels, 4);
        let Samples::F32(out) = block.samples else {
            panic!("not f32")
        };
        assert_eq!(out, [0.25, -0.5, 0.25, -0.5]);
    }

    /// Fixed 2 to 4 as `Wide`, with a latency, a status of its own and a
    /// count of its resets.
    struct Reporting {
        resets: Arc<Mutex<u32>>,
        status: Status,
    }

    impl Plugin for Reporting {
        fn name(&self) -> &str {
            "reporting"
        }

        fn formats(&self) -> Formats {
            ALL
        }

        fn channels(&self) -> Channels {
            Channels::Fixed {
                input: 2,
                output: 4,
            }
        }

        fn params(&self) -> &[Param] {
            &[]
        }

        fn set_param(&mut self, _: usize, _: f64) {}

        fn neutral(&self) -> bool {
            true
        }

        fn latency_frames(&self) -> u32 {
            5
        }

        fn process(&mut self, _: Block<'_>) -> Status {
            unreachable!()
        }

        fn process_into(&mut self, input: Block<'_>, output: Block<'_>) -> Status {
            Wide { formats: ALL }.process_into(input, output);
            self.status
        }

        fn reset(&mut self) {
            *self.resets.lock().unwrap() += 1;
        }
    }

    #[test]
    fn a_chain_that_changes_the_count_reports_status_latency_and_resets() {
        let resets = Arc::new(Mutex::new(0));
        let reporting = Reporting {
            resets: resets.clone(),
            status: Status {
                no_signal: true,
                changed: true,
            },
        };
        let mut late = add(0.0);
        late.latency = 3;
        let plugins: Vec<Box<dyn Plugin>> = vec![late.boxed(), Box::new(reporting)];
        let mut c = Chain::new(plugins, Format::F64, 16, 2).unwrap();
        assert_eq!(c.latency_frames(), 8);
        let mut d = [0.5f64, 0.25];
        let (block, status) = c.run(Block {
            channels: 2,
            samples: Samples::F64(&mut d),
        });
        assert_eq!(block.channels, 4);
        assert_eq!(
            status,
            Status {
                no_signal: true,
                changed: true
            }
        );
        c.reset();
        assert_eq!(*resets.lock().unwrap(), 1);
        // A plugin before it with its own status counts too.
        let reporting = Reporting {
            resets,
            status: Status::default(),
        };
        let mut quiet = add(0.0);
        quiet.no_signal = true;
        let plugins: Vec<Box<dyn Plugin>> = vec![quiet.boxed(), Box::new(reporting)];
        let mut c = Chain::new(plugins, Format::F64, 16, 2).unwrap();
        let (_, status) = c.run(Block {
            channels: 2,
            samples: Samples::F64(&mut d),
        });
        assert!(status.no_signal && !status.changed);
    }

    #[test]
    fn an_i32_chain_converts_for_a_plugin_that_widens() {
        let plugin = Box::new(Wide {
            formats: formats(false, false, true),
        });
        let mut c = Chain::new(vec![plugin], Format::I32, 16, 2).unwrap();
        let mut d = [i32::MAX, i32::MIN, 1 << 30, -(1 << 30)];
        let (block, _) = assert_no_alloc(|| {
            c.run(Block {
                channels: 2,
                samples: Samples::I32(&mut d),
            })
        });
        assert_eq!(block.channels, 4);
        let Samples::I32(out) = block.samples else {
            panic!("not i32")
        };
        assert_eq!(
            out,
            [
                i32::MAX,
                i32::MIN,
                i32::MAX,
                i32::MIN,
                1 << 30,
                -(1 << 30),
                1 << 30,
                -(1 << 30)
            ]
        );
    }

    fn stats(n: usize) -> Vec<Arc<PluginStats>> {
        (0..n).map(|_| Arc::default()).collect()
    }

    /// Runs `chain` on `frames` frames of two channels, every sample `v`,
    /// and gives the left channel back.
    fn run_frames(chain: &mut Chain, v: f64, frames: usize) -> Vec<f64> {
        let mut d = vec![[v, v]; frames];
        chain.process(Block {
            channels: 2,
            samples: Samples::F64(d.as_flattened_mut()),
        });
        d.iter().map(|f| f[0]).collect()
    }

    /// A chain at 48 kHz for blocks of up to 1024 frames.
    fn chain_at_48k(plugins: Vec<Box<dyn Plugin>>) -> Chain {
        Chain::new(plugins, Format::F64, 1024, 2)
            .unwrap()
            .for_rate(48_000)
    }

    #[test]
    fn stats_count_each_block() {
        let s = stats(2);
        let mut c = chain(vec![mul(2.0), mul(0.5)], Format::F64).with_stats(s.clone());
        for _ in 0..10 {
            run_f64(&mut c, 0.25);
        }
        for s in &s {
            let (runs, total, worst) = (
                s.runs.load(Relaxed),
                s.total_ns.load(Relaxed),
                s.worst_ns.load(Relaxed),
            );
            assert_eq!(runs, 10);
            assert!(total > 0);
            assert!(worst >= total / 10, "{worst} {total}");
        }
    }

    #[test]
    #[should_panic]
    fn stats_for_another_number_of_plugins_panic() {
        let _ = chain(vec![mul(2.0)], Format::F64).with_stats(stats(2));
    }

    #[test]
    fn a_disabled_plugin_fades_out_over_10_ms() {
        let mut c = chain_at_48k(vec![mul(0.5).boxed()]);
        assert_eq!(run_frames(&mut c, 1.0, 100), vec![0.5; 100]);
        c.set_enabled(0, false);
        assert!(!c.enabled(0));
        let mut out = Vec::new();
        for _ in 0..3 {
            out.extend(run_frames(&mut c, 1.0, 200));
        }
        assert_eq!(out[0], 0.5);
        assert!((out[480] - 1.0).abs() < 1e-6, "{}", out[480]);
        for w in out.windows(2) {
            assert!((w[1] - w[0]).abs() <= 0.5 / 480.0 + 1e-6, "{w:?}");
        }
        assert_eq!(&out[481..], vec![1.0; 600 - 481]);
        assert_eq!(run_frames(&mut c, 0.75, 100), vec![0.75; 100]);
        // And back in.
        c.set_enabled(0, true);
        let out = run_frames(&mut c, 1.0, 600);
        assert_eq!(out[0], 1.0);
        assert!((out[480] - 0.5).abs() < 1e-6, "{}", out[480]);
        assert_eq!(&out[481..], vec![0.5; 600 - 481]);
    }

    #[test]
    fn a_plugin_disabled_before_the_chain_runs_is_passed_over_from_the_first_block() {
        let mut c = chain_at_48k(vec![mul(0.5).boxed()]);
        c.set_enabled_now(0, false);
        assert!(!c.enabled(0));
        assert!(c.neutral());
        assert_eq!(run_frames(&mut c, 1.0, 100), vec![1.0; 100]);
        // Enabled again, it fades in as any.
        c.set_enabled(0, true);
        let out = run_frames(&mut c, 1.0, 600);
        assert_eq!(out[0], 1.0);
        assert_eq!(&out[481..], vec![0.5; 600 - 481]);
    }

    #[test]
    fn a_block_with_a_fade_in_it_is_changed() {
        let mut c = chain_at_48k(vec![mul(1.0).boxed()]);
        let run = |c: &mut Chain, frames: usize| {
            let mut d = vec![[1.0f64; 2]; frames];
            c.process(Block {
                channels: 2,
                samples: Samples::F64(d.as_flattened_mut()),
            })
            .changed
        };
        assert!(!run(&mut c, 100));
        c.set_enabled(0, false);
        // One block holds the whole fade, and the chain is neutral after it.
        assert!(run(&mut c, 600));
        assert!(c.neutral());
        assert!(!run(&mut c, 100));
        c.set_enabled(0, true);
        assert!(run(&mut c, 200));
        assert!(run(&mut c, 400));
        assert!(!run(&mut c, 100));
    }

    #[test]
    fn a_plugin_whose_counts_differ_is_not_disabled_nor_an_unknown_one() {
        let mut c = Chain::new(vec![wide()], Format::F32, 16, 2).unwrap();
        c.set_enabled(0, false);
        c.set_enabled(1, false);
        assert!(c.enabled(0));
        assert!(!c.enabled(1));
        let mut d = [1.0f32, 2.0];
        let (block, _) = c.run(Block {
            channels: 2,
            samples: Samples::F32(&mut d),
        });
        assert_eq!(block.channels, 4);
    }

    #[test]
    fn a_plugin_beside_one_that_widens_fades_too() {
        let plugins: Vec<Box<dyn Plugin>> = vec![mul(0.5).boxed(), wide()];
        let mut c = Chain::new(plugins, Format::F64, 1024, 2)
            .unwrap()
            .for_rate(48_000);
        c.set_enabled(0, false);
        let mut d = vec![1.0f64; 2 * 600];
        let (block, _) = c.run(Block {
            channels: 2,
            samples: Samples::F64(&mut d),
        });
        let Samples::F64(out) = block.samples else {
            panic!("not f64")
        };
        assert_eq!(out[0], 0.5);
        assert!((out[4 * 480] - 1.0).abs() < 1e-6);
        assert_eq!(out[4 * 599], 1.0);
    }

    /// Two channels, each delayed by `DELAY` frames.
    struct Delay {
        line: Vec<f64>,
        at: usize,
    }

    const DELAY: usize = 64;

    impl Plugin for Delay {
        fn name(&self) -> &str {
            "delay"
        }

        fn formats(&self) -> Formats {
            formats(false, false, true)
        }

        fn params(&self) -> &[Param] {
            &[]
        }

        fn set_param(&mut self, _: usize, _: f64) {}

        fn neutral(&self) -> bool {
            false
        }

        fn latency_frames(&self) -> u32 {
            DELAY as u32
        }

        fn process(&mut self, block: Block<'_>) -> Status {
            let Samples::F64(d) = block.samples else {
                panic!("not f64")
            };
            for s in d.iter_mut() {
                std::mem::swap(s, &mut self.line[self.at]);
                self.at = (self.at + 1) % self.line.len();
            }
            Status::default()
        }

        fn reset(&mut self) {
            self.line.fill(0.0);
        }
    }

    fn delay() -> Box<dyn Plugin> {
        Box::new(Delay {
            line: vec![0.0; 2 * DELAY],
            at: 0,
        })
    }

    #[test]
    fn enabling_resets_first() {
        let mut c = chain_at_48k(vec![delay()]);
        // The line fills with 1.0 and keeps it through the fade.
        run_frames(&mut c, 1.0, 600);
        c.set_enabled(0, false);
        assert_eq!(c.latency_frames(), 0);
        run_frames(&mut c, 1.0, 600);
        assert_eq!(run_frames(&mut c, 0.0, 600), vec![0.0; 600]);
        c.set_enabled(0, true);
        assert_eq!(c.latency_frames(), DELAY as u32);
        assert_eq!(run_frames(&mut c, 0.0, 1000), vec![0.0; 1000]);
    }

    #[test]
    fn a_disabled_plugin_is_neutral() {
        let mut busy = mul(2.0);
        busy.neutral = false;
        let mut c = chain_at_48k(vec![busy.boxed()]);
        assert!(!c.neutral());
        c.set_enabled(0, false);
        assert!(!c.neutral());
        run_frames(&mut c, 1.0, 400);
        assert!(!c.neutral());
        run_frames(&mut c, 1.0, 400);
        assert!(c.neutral());
        // And a neutral one mid-fade is not.
        c.set_enabled(0, true);
        assert!(!c.neutral());
        let mut c = chain_at_48k(vec![mul(1.0).boxed()]);
        c.set_enabled(0, false);
        assert!(!c.neutral());
        run_frames(&mut c, 1.0, 500);
        assert!(c.neutral());
    }

    #[test]
    fn stats_publish_neutral_and_latency() {
        let mut t = mul(1.0);
        t.neutral_at = Some(1.0);
        t.latency = 7;
        let s = stats(1);
        let mut c = chain_at_48k(vec![t.boxed()]).with_stats(s.clone());
        assert!(s[0].neutral.load(Relaxed));
        assert_eq!(s[0].latency_frames.load(Relaxed), 7);
        c.set_param(0, 0, 2.0);
        assert!(!s[0].neutral.load(Relaxed));
        c.set_param(0, 0, 1.0);
        assert!(s[0].neutral.load(Relaxed));
        c.set_param(0, 0, 2.0);
        c.set_enabled(0, false);
        assert_eq!(s[0].latency_frames.load(Relaxed), 0);
        assert!(!s[0].neutral.load(Relaxed));
        run_frames(&mut c, 1.0, 500);
        assert!(s[0].neutral.load(Relaxed));
        c.set_enabled(0, true);
        assert_eq!(s[0].latency_frames.load(Relaxed), 7);
        assert!(!s[0].neutral.load(Relaxed));
    }

    #[test]
    fn stats_publish_neutral_when_a_ramp_ends_at_the_centre() {
        let s = stats(1);
        let balance = Box::new(crate::plugin::balance::Balance::new(48_000));
        let mut c = chain_at_48k(vec![balance]).with_stats(s.clone());
        c.set_param(0, 0, 0.5);
        run_frames(&mut c, 1.0, 500);
        assert!(!s[0].neutral.load(Relaxed));
        c.set_param(0, 0, 0.0);
        assert!(!s[0].neutral.load(Relaxed), "neutral while it ramps");
        run_frames(&mut c, 1.0, 500);
        assert!(c.neutral());
        assert!(
            s[0].neutral.load(Relaxed),
            "the end of the ramp not published"
        );
    }

    #[test]
    fn set_param_now_starts_a_balance_at_the_value() {
        let balance = Box::new(crate::plugin::balance::Balance::new(48_000));
        let mut c = chain_at_48k(vec![balance]);
        c.set_param_now(0, 0, 0.5);
        assert_eq!(run_frames(&mut c, 1.0, 1), [0.5]);
        // On the audio thread a value ramps.
        c.set_param(0, 0, 0.0);
        assert!(run_frames(&mut c, 1.0, 1)[0] < 1.0);
    }

    #[test]
    fn set_enabled_does_not_allocate() {
        let mut c = chain_at_48k(vec![delay(), mul(0.5).boxed()]).with_stats(stats(2));
        assert_no_alloc(|| {
            c.set_enabled(0, false);
            c.set_enabled(1, false);
            c.set_enabled(0, true);
            c.set_enabled(5, true);
            assert!(c.enabled(0) && !c.enabled(1));
        });
    }

    #[test]
    fn timed_processing_does_not_allocate() {
        let mut c = chain_at_48k(vec![
            delay(),
            mul(0.5).only(formats(false, true, false)).boxed(),
        ])
        .with_stats(stats(2));
        let mut d = vec![0.25f64; 2 * 256];
        c.set_enabled(1, false);
        assert_no_alloc(|| {
            for _ in 0..4 {
                c.process(Block {
                    channels: 2,
                    samples: Samples::F64(&mut d),
                });
            }
        });
    }

    #[test]
    fn more_than_eight_channels_are_refused() {
        assert!(Chain::new(vec![Box::new(Scale)], Format::F32, 16, 9).is_err());
        assert!(Chain::new(vec![Box::new(Scale)], Format::F32, 16, 0).is_err());
        assert!(Chain::new(vec![Box::new(Scale)], Format::F32, 16, 8).is_ok());
    }

    #[test]
    fn an_empty_chain_takes_any_count() {
        let mut c = Chain::empty();
        assert_eq!((c.channels_in(), c.channels_out()), (0, 0));
        let mut d = [1.0f32; 6];
        let (block, _) = c.run(Block {
            channels: 3,
            samples: Samples::F32(&mut d),
        });
        assert_eq!(block.channels, 3);
    }

    #[test]
    #[should_panic]
    fn process_refuses_a_chain_that_changes_the_count() {
        let mut c = Chain::new(vec![wide()], Format::F32, 16, 2).unwrap();
        let mut d = [0.0f32; 2];
        c.process(Block {
            channels: 2,
            samples: Samples::F32(&mut d),
        });
    }
}
