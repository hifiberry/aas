//! Chains: plugins run in order on one block.
//!
//! A chain is built on the control thread and handed to the audio thread.
//! It owns the buffers it converts in, so `process` never allocates.

use super::{Block, Formats, Plugin, Status};
use crate::config::Format;
use crate::format::BlockStore;

/// Plugins in the order they run, with what each needs converted.
pub struct Chain {
    plugins: Vec<Box<dyn Plugin>>,
    /// For each plugin, the format its blocks are converted to, if it does
    /// not take the chain's own.
    convert: Vec<Option<Format>>,
    format: Format,
    store: BlockStore,
}

impl Chain {
    /// A chain for blocks in `format` of up to `max_frames` frames. A longer
    /// block given to `process` panics. The engine takes only chains built
    /// for `crate::latency::MAX_PERIOD`, so that one never depends on the
    /// period.
    pub fn new(plugins: Vec<Box<dyn Plugin>>, format: Format, max_frames: usize) -> Chain {
        let convert = plugins
            .iter()
            .map(|p| conversion(p.formats(), format))
            .collect();
        Chain {
            plugins,
            convert,
            format,
            store: BlockStore::new(max_frames),
        }
    }

    /// A chain without plugins. Allocates nothing.
    pub fn empty() -> Chain {
        Chain {
            plugins: Vec::new(),
            convert: Vec::new(),
            format: Format::I32,
            store: BlockStore::new(0),
        }
    }

    /// Runs the plugins in order on `block`, which must be in the chain's
    /// format (only a debug build checks that) and at most `max_frames`
    /// frames long (a longer block panics).
    /// `no_signal` is set if any plugin reported it.
    pub fn process(&mut self, mut block: Block<'_>) -> Status {
        debug_assert!(self.plugins.is_empty() || block.format() == self.format);
        let mut status = Status::default();
        for (plugin, convert) in self.plugins.iter_mut().zip(&self.convert) {
            let plugin = plugin.as_mut();
            let s = match (*convert, &mut block) {
                (None, _) => plugin.process(block.reborrow()),
                (Some(to), Block::I32(d)) => converted(plugin, d, to, &mut self.store),
                (Some(to), Block::F32(d)) => converted(plugin, d, to, &mut self.store),
                (Some(to), Block::F64(d)) => converted(plugin, d, to, &mut self.store),
            };
            status.no_signal |= s.no_signal;
            status.changed |= s.changed;
        }
        status
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
            self.store.i32.len()
        }
    }

    /// True if every plugin is neutral, and so for an empty chain.
    pub fn neutral(&self) -> bool {
        self.plugins.iter().all(|p| p.neutral())
    }

    /// The delay of all plugins together.
    pub fn latency_frames(&self) -> u32 {
        self.plugins.iter().map(|p| p.latency_frames()).sum()
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
    fn buffer(store: &mut BlockStore) -> &mut [[Self; 2]];
    fn block(data: &mut [[Self; 2]]) -> Block<'_>;
}

impl Sample for i32 {
    fn to_unit(self) -> f64 {
        f64::from(self) / SCALE
    }

    /// Rounds to nearest and saturates, as `Processing::finish` does.
    fn from_unit(v: f64) -> i32 {
        (v * SCALE).round() as i32
    }

    fn buffer(store: &mut BlockStore) -> &mut [[i32; 2]] {
        &mut store.i32
    }

    fn block(data: &mut [[i32; 2]]) -> Block<'_> {
        Block::I32(data)
    }
}

impl Sample for f32 {
    fn to_unit(self) -> f64 {
        f64::from(self)
    }

    fn from_unit(v: f64) -> f32 {
        v as f32
    }

    fn buffer(store: &mut BlockStore) -> &mut [[f32; 2]] {
        &mut store.f32
    }

    fn block(data: &mut [[f32; 2]]) -> Block<'_> {
        Block::F32(data)
    }
}

impl Sample for f64 {
    fn to_unit(self) -> f64 {
        self
    }

    fn from_unit(v: f64) -> f64 {
        v
    }

    fn buffer(store: &mut BlockStore) -> &mut [[f64; 2]] {
        &mut store.f64
    }

    fn block(data: &mut [[f64; 2]]) -> Block<'_> {
        Block::F64(data)
    }
}

/// Runs `plugin` on `data` converted to `to`, and converts the result back.
fn converted<S: Sample>(
    plugin: &mut dyn Plugin,
    data: &mut [[S; 2]],
    to: Format,
    store: &mut BlockStore,
) -> Status {
    match to {
        Format::I32 => through::<S, i32>(plugin, data, store),
        Format::F32 => through::<S, f32>(plugin, data, store),
        Format::F64 => through::<S, f64>(plugin, data, store),
    }
}

fn through<S: Sample, T: Sample>(
    plugin: &mut dyn Plugin,
    data: &mut [[S; 2]],
    store: &mut BlockStore,
) -> Status {
    let buf = &mut T::buffer(store)[..data.len()];
    for (b, d) in buf.iter_mut().zip(data.iter()) {
        *b = [T::from_unit(d[0].to_unit()), T::from_unit(d[1].to_unit())];
    }
    let status = plugin.process(T::block(buf));
    for (d, b) in data.iter_mut().zip(buf.iter()) {
        *d = [S::from_unit(b[0].to_unit()), S::from_unit(b[1].to_unit())];
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
            let first = match block {
                Block::I32(d) => {
                    let first = f64::from(d[0][0]) / FULL;
                    for s in d.iter_mut().flatten() {
                        *s = (op(f64::from(*s) / FULL, v) * FULL).round() as i32;
                    }
                    (Format::I32, first)
                }
                Block::F32(d) => {
                    let first = f64::from(d[0][0]);
                    for s in d.iter_mut().flatten() {
                        *s = op(f64::from(*s), v) as f32;
                    }
                    (Format::F32, first)
                }
                Block::F64(d) => {
                    let first = d[0][0];
                    for s in d.iter_mut().flatten() {
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
        Chain::new(plugins, format, 16)
    }

    /// Runs `chain` on one f64 frame.
    fn run_f64(chain: &mut Chain, v: f64) -> f64 {
        let mut d = [[v, v]];
        chain.process(Block::F64(&mut d));
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
            assert_eq!(c.process(Block::I32(&mut i)), Status::default());
            c.process(Block::F32(&mut f));
            c.process(Block::F64(&mut d));
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
        c.process(Block::I32(&mut d));
        assert_eq!(*seen.lock().unwrap(), Some((Format::F64, 0.5)));
        assert_eq!(d, [[1 << 30, -(1 << 30)]]);

        // On the way back the chain rounds to nearest and saturates.
        let mut c = chain(
            vec![mul(2.6).only(formats(false, false, true))],
            Format::I32,
        );
        let mut d = [[1, -(1 << 30)], [1 << 30, -1]];
        c.process(Block::I32(&mut d));
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
                Format::I32 => Block::I32(&mut i),
                Format::F32 => Block::F32(&mut f),
                Format::F64 => Block::F64(&mut d),
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
        assert!(c.process(Block::F64(&mut d)).no_signal);
        let mut c = chain(vec![add(0.0), add(0.0)], Format::F64);
        assert!(!c.process(Block::F64(&mut d)).no_signal);
    }

    #[test]
    fn a_float_block_is_not_clipped_between_plugins() {
        let mut c = chain(vec![mul(4.0), mul(0.25)], Format::F64);
        assert_eq!(run_f64(&mut c, 0.9), 0.9);
        let mut c = chain(vec![mul(4.0), mul(0.25)], Format::F32);
        let mut f = [[0.9f32, -0.9]];
        c.process(Block::F32(&mut f));
        assert_eq!(f, [[0.9, -0.9]]);
        // Nor where an f32 chain converts for an f64 plugin.
        let mut c = chain(
            vec![mul(4.0).only(formats(false, false, true)), mul(0.25)],
            Format::F32,
        );
        c.process(Block::F32(&mut f));
        assert_eq!(f, [[0.9, -0.9]]);
    }

    #[test]
    fn an_i32_block_is_clipped_before_the_first_plugin() {
        let mut acc = [[i64::from(i32::MAX) + 1000, i64::from(i32::MIN) - 1000]];
        let mut store = BlockStore::new(1);
        let mut c = chain(
            vec![mul(0.5).only(formats(true, false, false))],
            Format::I32,
        );
        let mut block = I32::to_block(&mut acc, &mut store);
        c.process(block.reborrow());
        I32::from_block(block, &mut acc);
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
        let mut store = BlockStore::new(frames);
        let plugins = vec![add(0.0).boxed(), mul(2.0).only(other).boxed()];
        let mut c = Chain::new(plugins, format, frames);
        assert_no_alloc(|| {
            crate::format::mix_clear::<P>(&mut acc);
            crate::format::mix_add::<P>(&input, crate::format::Ramp::flat(1.0), &mut acc);
            let mut block = P::to_block(&mut acc, &mut store);
            c.process(block.reborrow());
            P::from_block(block, &mut acc);
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
}
