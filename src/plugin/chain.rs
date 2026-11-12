//! Chains: plugins run in order on one block.
//!
//! A chain is built on the control thread and handed to the audio thread.
//! It owns the buffers it converts in and the ones a change of the channel
//! count writes to, so `process` and `run` never allocate.

use super::{Block, Channels, Formats, Plugin, Samples, Status};
use crate::config::Format;
use crate::format::BlockStore;
use crate::matrix::MAX_CHANNELS;

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
        Ok(Chain {
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
        // Where the audio is: the caller's block, or one of `wide`.
        let mut at = None;
        let mut width = channels;
        for ((plugin, convert), &(cin, cout)) in
            self.plugins.iter_mut().zip(&self.convert).zip(&self.counts)
        {
            let plugin = plugin.as_mut();
            let (in_len, out_len) = (frames * cin, frames * cout);
            let s = if cin == cout {
                let data = match at {
                    None => &mut *input,
                    Some(i) => &mut S::buffer(&mut self.wide[i])[..in_len],
                };
                match *convert {
                    None => plugin.process(S::block(data, cin)),
                    Some(to) => converted(plugin, data, cin, to, &mut self.store),
                }
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
                let s = match *convert {
                    None => plugin.process_into(S::block(src, cin), S::block(dst, cout)),
                    Some(to) => converted_into(
                        plugin,
                        (src, cin),
                        (&mut *dst, cout),
                        to,
                        (&mut self.store, &mut self.store_out),
                    ),
                };
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
    pub fn process(&mut self, mut block: Block<'_>) -> Status {
        assert!(
            !self.changes_channels(),
            "a chain that changes the channel count is run with `run`"
        );
        debug_assert!(self.plugins.is_empty() || block.format() == self.format);
        debug_assert!(self.channels_in == 0 || block.channels == self.channels_in);
        let mut status = Status::default();
        for (plugin, convert) in self.plugins.iter_mut().zip(&self.convert) {
            let plugin = plugin.as_mut();
            let channels = block.channels;
            let s = match (*convert, &mut block.samples) {
                (None, _) => plugin.process(block.reborrow()),
                (Some(to), Samples::I32(d)) => converted(plugin, d, channels, to, &mut self.store),
                (Some(to), Samples::F32(d)) => converted(plugin, d, channels, to, &mut self.store),
                (Some(to), Samples::F64(d)) => converted(plugin, d, channels, to, &mut self.store),
            };
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

    /// True if every plugin is neutral, and so for an empty chain. Never
    /// true for a chain that changes the channel count.
    pub fn neutral(&self) -> bool {
        !self.changes_channels() && self.plugins.iter().all(|p| p.neutral())
    }

    /// The delay of all plugins together.
    pub fn latency_frames(&self) -> u32 {
        self.plugins
            .iter()
            .fold(0u32, |sum, p| sum.saturating_add(p.latency_frames()))
    }

    /// Sets parameter `index` of plugin `plugin`. An unknown plugin or
    /// parameter index is ignored.
    pub fn set_param(&mut self, plugin: usize, index: usize, value: f64) {
        if let Some(p) = self.plugins.get_mut(plugin)
            && index < p.params().len()
        {
            p.set_param(index, value);
        }
    }
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
fn conversion(formats: Formats, format: Format) -> Option<Format> {
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
            self.neutral
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
