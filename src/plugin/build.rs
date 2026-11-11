//! Chains built from the config, on the control thread.

use super::Plugin;
use super::builtin;
use super::chain::Chain;
use super::native::Library;
use crate::config::{Format, PluginConfig, PluginPlace};
use crate::engine::Place;
use crate::latency::MAX_PERIOD;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The plugin libraries the config names, each loaded once by its path and
/// kept for the life of the daemon.
#[derive(Default)]
pub struct Libraries {
    loaded: BTreeMap<PathBuf, Library>,
}

impl Libraries {
    /// The library at `path`, loaded the first time it is asked for.
    pub fn get(&mut self, path: &Path) -> Result<&Library, String> {
        if !self.loaded.contains_key(path) {
            let library = Library::open(path)?;
            self.loaded.insert(path.to_path_buf(), library);
        }
        Ok(&self.loaded[path])
    }
}

/// The engine's place for a place in the config.
pub fn engine_place(place: PluginPlace) -> Place {
    match place {
        PluginPlace::Analog => Place::Analog,
        PluginPlace::Mix => Place::Mix,
        PluginPlace::Output => Place::Output,
    }
}

/// Builds the chain at `place` for a stream at `rate`, from the plugins the
/// config lists there, in their order. A chain with a plugin takes blocks of
/// up to `MAX_PERIOD` frames, so that it never depends on the period. A
/// place without plugins gives an empty chain, except `Analog`: it always
/// has a detector, last if the config lists none.
///
/// Errors name the plugin by its position in the config, from 1.
pub fn build_chain(
    config: &[PluginConfig],
    place: PluginPlace,
    rate: u32,
    format: Format,
    libraries: &mut Libraries,
) -> Result<Chain, String> {
    let max_frames = MAX_PERIOD as usize;
    let mut plugins = Vec::new();
    let listed = config.iter().enumerate().filter(|(_, p)| p.place == place);
    for (i, p) in listed {
        let n = i + 1;
        let mut plugin: Box<dyn Plugin> = match (p.kind.as_str(), &p.path) {
            ("library", Some(path)) => libraries
                .get(path)
                .and_then(|library| library.create(rate, max_frames))
                .map_err(|e| format!("plugin {n}: {e}"))?,
            ("library", None) => return Err(format!("plugin {n}: type \"library\" needs a path")),
            (kind, _) => builtin::create(kind, rate)
                .ok_or_else(|| format!("plugin {n}: there is no plugin of type \"{kind}\""))?,
        };
        for (name, &value) in &p.params {
            let index = plugin.params().iter().position(|q| q.name == *name);
            let Some(index) = index else {
                return Err(format!(
                    "plugin {n}: type \"{}\" has no parameter {name}",
                    p.kind
                ));
            };
            plugin.set_param(index, value);
        }
        plugins.push(plugin);
    }
    let has_detect = config
        .iter()
        .any(|p| p.place == place && p.kind == "detect");
    if place == PluginPlace::Analog && !has_detect {
        plugins.push(builtin::create("detect", rate).expect("detect is built in"));
    }
    if plugins.is_empty() {
        return Ok(Chain::empty());
    }
    Ok(Chain::new(plugins, format, max_frames))
}

/// The three chains, built for one rate.
pub struct Chains {
    pub rate: u32,
    pub analog: Chain,
    pub mix: Chain,
    pub output: Chain,
}

impl Chains {
    /// Builds every chain at `rate`, which loads every library the config
    /// names. The first error stops it.
    pub fn build(
        config: &[PluginConfig],
        rate: u32,
        format: Format,
        libraries: &mut Libraries,
    ) -> Result<Chains, String> {
        let mut build = |place| build_chain(config, place, rate, format, libraries);
        Ok(Chains {
            rate,
            analog: build(PluginPlace::Analog)?,
            mix: build(PluginPlace::Mix)?,
            output: build(PluginPlace::Output)?,
        })
    }

    /// Takes the chain at `place` out, leaving an empty one.
    pub fn take(&mut self, place: PluginPlace) -> Chain {
        let chain = match place {
            PluginPlace::Analog => &mut self.analog,
            PluginPlace::Mix => &mut self.mix,
            PluginPlace::Output => &mut self.output,
        };
        std::mem::replace(chain, Chain::empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn plugin(place: PluginPlace, kind: &str, params: &[(&str, f64)]) -> PluginConfig {
        PluginConfig {
            place,
            kind: kind.into(),
            path: None,
            params: params.iter().map(|&(k, v)| (k.into(), v)).collect(),
        }
    }

    fn names(chain: &Chain) -> Vec<&str> {
        chain.names().collect()
    }

    fn build(config: &[PluginConfig], place: PluginPlace) -> Result<Chain, String> {
        let mut libraries = Libraries::default();
        build_chain(config, place, 48000, Format::F32, &mut libraries)
    }

    #[test]
    fn the_analog_chain_gets_a_detector_if_none_is_listed() {
        let chain = build(&[], PluginPlace::Analog).unwrap();
        assert_eq!(names(&chain), ["detect"]);
        assert_eq!(chain.max_frames(), MAX_PERIOD as usize);
        let config = [plugin(PluginPlace::Analog, "balance", &[])];
        let chain = build(&config, PluginPlace::Analog).unwrap();
        assert_eq!(names(&chain), ["balance", "detect"]);
        // The other places get nothing they do not list.
        let chain = build(&config, PluginPlace::Mix).unwrap();
        assert!(chain.is_empty());
        let chain = build(&config, PluginPlace::Output).unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn a_listed_detector_keeps_its_place() {
        let config = [
            plugin(PluginPlace::Analog, "detect", &[("threshold", -40.0)]),
            plugin(PluginPlace::Mix, "balance", &[]),
            plugin(PluginPlace::Analog, "balance", &[("balance", 0.5)]),
        ];
        let chain = build(&config, PluginPlace::Analog).unwrap();
        assert_eq!(names(&chain), ["detect", "balance"]);
        let chain = build(&config, PluginPlace::Mix).unwrap();
        assert_eq!(names(&chain), ["balance"]);
        assert_eq!(chain.max_frames(), MAX_PERIOD as usize);
    }

    #[test]
    fn an_unknown_parameter_is_an_error() {
        let config = [
            plugin(PluginPlace::Mix, "balance", &[("balance", 0.5)]),
            plugin(PluginPlace::Output, "balance", &[("gain", 1.0)]),
        ];
        assert!(build(&config, PluginPlace::Mix).is_ok());
        let e = build(&config, PluginPlace::Output).err().unwrap();
        assert!(
            e.contains("plugin 2") && e.contains("balance") && e.contains("gain"),
            "{e}"
        );
    }

    #[test]
    fn a_parameter_is_set_by_its_name() {
        // A threshold above any level keeps the detector closed: a loud
        // block comes out as zeros.
        let config = [plugin(PluginPlace::Analog, "detect", &[("threshold", 0.0)])];
        let mut chain = build(&config, PluginPlace::Analog).unwrap();
        let mut data = vec![[0.5f32; 2]; 4800];
        let status = chain.process(crate::plugin::Block::F32(&mut data));
        assert!(status.no_signal);
        let default = build(&[], PluginPlace::Analog);
        let mut chain = default.unwrap();
        let mut data = vec![[0.5f32; 2]; 4800];
        assert!(
            !chain
                .process(crate::plugin::Block::F32(&mut data))
                .no_signal
        );
    }

    #[test]
    fn a_library_that_cannot_be_loaded_is_an_error() {
        let config = [
            plugin(PluginPlace::Output, "balance", &[]),
            PluginConfig {
                place: PluginPlace::Output,
                kind: "library".into(),
                path: Some("/nonexistent/libnothing.so".into()),
                params: BTreeMap::new(),
            },
        ];
        let mut libraries = Libraries::default();
        let e = Chains::build(&config, 48000, Format::F32, &mut libraries)
            .err()
            .unwrap();
        // Named by its place in the file, as every other plugin error.
        assert!(e.starts_with("plugin 2: "), "{e}");
        assert!(e.contains("/nonexistent/libnothing.so"), "{e}");
    }

    #[test]
    fn the_chains_for_a_rate_cover_every_place() {
        let config = [plugin(PluginPlace::Output, "balance", &[])];
        let mut libraries = Libraries::default();
        let mut chains = Chains::build(&config, 96000, Format::F64, &mut libraries).unwrap();
        assert_eq!(chains.rate, 96000);
        assert_eq!(names(&chains.take(PluginPlace::Analog)), ["detect"]);
        assert!(chains.take(PluginPlace::Mix).is_empty());
        assert_eq!(names(&chains.take(PluginPlace::Output)), ["balance"]);
        assert!(chains.take(PluginPlace::Output).is_empty());
    }
}
