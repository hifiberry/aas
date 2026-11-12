//! Chains built from the config, on the control thread.

use super::builtin;
use super::chain::Chain;
use super::native::Library;
use super::{BuildError, Failure, Param, ParamKind, Plugin};
use crate::config::{Format, ParamValue, PluginConfig, PluginPlace};
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
) -> Result<Chain, BuildError> {
    let config_error = |m: String| BuildError::new(Failure::Config, m);
    let max_frames = MAX_PERIOD as usize;
    let mut plugins = Vec::new();
    let listed = config.iter().enumerate().filter(|(_, p)| p.place == place);
    for (i, p) in listed {
        let n = i + 1;
        let prefix = format!("plugin {n}: ");
        let mut plugin: Box<dyn Plugin> = match (p.kind.as_str(), &p.path) {
            ("library", Some(path)) => {
                let library = libraries
                    .get(path)
                    .map_err(|e| config_error(format!("{prefix}{e}")))?;
                library
                    .create(rate, max_frames)
                    .map_err(|e| e.context(&prefix))?
            }
            ("library", None) => {
                return Err(config_error(format!(
                    "{prefix}type \"library\" needs a path"
                )));
            }
            (kind, _) => match builtin::create(kind, rate) {
                Some(Ok(plugin)) => plugin,
                Some(Err(failure)) => {
                    let what = match failure {
                        Failure::Unsupported => "does not support",
                        _ => "failed at",
                    };
                    let m = format!("{prefix}type \"{kind}\" {what} {rate} Hz");
                    return Err(BuildError::new(failure, m));
                }
                None => {
                    return Err(config_error(format!(
                        "{prefix}there is no plugin of type \"{kind}\""
                    )));
                }
            },
        };
        for (name, value) in &p.params {
            let index = plugin.params().iter().position(|q| q.name == *name);
            let Some(index) = index else {
                return Err(config_error(format!(
                    "plugin {n}: type \"{}\" has no parameter {name}",
                    p.kind
                )));
            };
            let param = &plugin.params()[index];
            check_value(param, value).map_err(|e| config_error(format!("{prefix}{e}")))?;
            match value {
                ParamValue::Number(v) => plugin.set_param(index, *v),
                ParamValue::Text(text) => {
                    plugin.set_text(index, text).map_err(|(kind, e)| {
                        let what = match kind {
                            Failure::Unsupported => format!("cannot use at {rate} Hz"),
                            _ => "refused".to_string(),
                        };
                        BuildError::new(
                            kind,
                            format!("{prefix}{} {what} {name} = \"{text}\": {e}", plugin.name()),
                        )
                    })?;
                }
            }
        }
        plugins.push(plugin);
    }
    let has_detect = config
        .iter()
        .any(|p| p.place == place && p.kind == "detect");
    if place == PluginPlace::Analog && !has_detect {
        let detect = builtin::create("detect", rate).and_then(Result::ok);
        plugins.push(detect.expect("detect is built in and takes every rate"));
    }
    if plugins.is_empty() {
        return Ok(Chain::empty());
    }
    Ok(Chain::new(plugins, format, max_frames).for_rate(rate))
}

/// Whether `value` fits the type of `param`: text for a text parameter, and
/// for the others a number, a whole one in the range for an integer, 0 or 1
/// for a switch, and one of the values for an enumeration. A float outside
/// its range is left to the plugin's clamping.
fn check_value(param: &Param, value: &ParamValue) -> Result<(), String> {
    let name = &param.name;
    let number = match (&param.kind, value) {
        (ParamKind::Text, ParamValue::Text(_)) => return Ok(()),
        (ParamKind::Text, ParamValue::Number(v)) => {
            return Err(format!("{name} takes text, in quotes, not {v}"));
        }
        (_, ParamValue::Text(t)) => {
            return Err(format!("{name} takes a number, not the text \"{t}\""));
        }
        (_, ParamValue::Number(v)) => *v,
    };
    let whole = number.fract() == 0.0;
    let within = |min: f64, max: f64| whole && (min..=max).contains(&number);
    let (fits, wants) = match &param.kind {
        // A float outside its range is clamped by the plugin.
        ParamKind::Float | ParamKind::Text => (true, String::new()),
        ParamKind::Integer => (
            within(param.min, param.max),
            format!("a whole number from {} to {}", param.min, param.max),
        ),
        ParamKind::Switch => (within(0.0, 1.0), "0 or 1".to_string()),
        ParamKind::Enumeration(names) => (
            within(0.0, names.len() as f64 - 1.0),
            format!("0 to {}", names.len() - 1),
        ),
    };
    if fits {
        Ok(())
    } else {
        Err(format!("{name} takes {wants}, not {number}"))
    }
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
    ) -> Result<Chains, BuildError> {
        let mut build = |place| build_chain(config, place, rate, format, libraries);
        Ok(Chains {
            rate,
            analog: build(PluginPlace::Analog)?,
            mix: build(PluginPlace::Mix)?,
            output: build(PluginPlace::Output)?,
        })
    }

    /// Builds every chain at `rate` for the start of the daemon. A plugin
    /// that does not support `rate` but whose chains build at one of
    /// `others` is handled by the control thread as for a rate the card
    /// refuses: that gives None. Any other failure, and chains that build
    /// at none of the rates, is an error, which stops the start.
    pub fn build_for_start(
        config: &[PluginConfig],
        rate: u32,
        others: &[u32],
        format: Format,
        libraries: &mut Libraries,
    ) -> Result<Option<Chains>, String> {
        let e = match Chains::build(config, rate, format, libraries) {
            Ok(chains) => return Ok(Some(chains)),
            Err(e) if e.kind == Failure::Unsupported => e,
            Err(e) => return Err(e.message),
        };
        for &other in others.iter().filter(|&&r| r != rate) {
            match Chains::build(config, other, format, libraries) {
                Ok(_) => {
                    eprintln!("aas: {e}; the output does not run at {rate} Hz");
                    return Ok(None);
                }
                Err(e) if e.kind == Failure::Unsupported => {}
                Err(e) => return Err(e.message),
            }
        }
        Err(e.message)
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
            params: params
                .iter()
                .map(|&(k, v)| (k.into(), ParamValue::Number(v)))
                .collect(),
        }
    }

    fn names(chain: &Chain) -> Vec<&str> {
        chain.names().collect()
    }

    fn build(config: &[PluginConfig], place: PluginPlace) -> Result<Chain, String> {
        let mut libraries = Libraries::default();
        build_chain(config, place, 48000, Format::F32, &mut libraries).map_err(|e| e.message)
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
        let status = chain.process(crate::plugin::Block {
            channels: 2,
            samples: crate::plugin::Samples::F32(data.as_flattened_mut()),
        });
        assert!(status.no_signal);
        let default = build(&[], PluginPlace::Analog);
        let mut chain = default.unwrap();
        let mut data = vec![[0.5f32; 2]; 4800];
        assert!(
            !chain
                .process(crate::plugin::Block {
                    channels: 2,
                    samples: crate::plugin::Samples::F32(data.as_flattened_mut())
                })
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
            .unwrap()
            .message;
        // Named by its place in the file, as every other plugin error.
        assert!(e.starts_with("plugin 2: "), "{e}");
        assert!(e.contains("/nonexistent/libnothing.so"), "{e}");
    }

    #[test]
    fn at_the_start_a_plugin_that_refuses_the_default_rate_is_no_error_if_another_works() {
        let mut libraries = Libraries::default();
        let refuses = [plugin(PluginPlace::Mix, "only-up-to-48k", &[])];
        let start = |config: &[PluginConfig], libraries: &mut Libraries| {
            Chains::build_for_start(config, 96000, &[44100, 96000], Format::F32, libraries)
        };
        assert!(matches!(start(&refuses, &mut libraries), Ok(None)));
        // A plugin that fails, rather than not support the rate, stops the
        // start, whatever other rates would do.
        let fails = [plugin(PluginPlace::Mix, "fails-above-48k", &[])];
        let e = start(&fails, &mut libraries).err().unwrap();
        assert!(
            e.contains("plugin 1") && e.contains("failed at 96000 Hz"),
            "{e}"
        );
        // A plugin that fails at every rate stops the start.
        let broken = [plugin(PluginPlace::Mix, "balance", &[("nope", 1.0)])];
        let e = start(&broken, &mut libraries).err().unwrap();
        assert!(e.starts_with("plugin 1: "), "{e}");
        // And one that takes the default rate gives the chains for it.
        let fine = [plugin(PluginPlace::Mix, "balance", &[])];
        let chains = start(&fine, &mut libraries).unwrap().unwrap();
        assert_eq!(chains.rate, 96000);
    }

    #[test]
    fn text_for_a_numeric_parameter_is_a_config_error() {
        let mut config = [plugin(PluginPlace::Analog, "detect", &[])];
        let text = ParamValue::Text("-40".into());
        config[0].params.insert("threshold".into(), text);
        let mut libraries = Libraries::default();
        let e = build_chain(
            &config,
            PluginPlace::Analog,
            48000,
            Format::F32,
            &mut libraries,
        );
        let e = e.err().unwrap();
        assert_eq!(e.kind, Failure::Config);
        assert!(e.message.contains("threshold takes a number"), "{e}");
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
