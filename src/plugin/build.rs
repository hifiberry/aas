//! Chains built from the config, on the control thread.

use super::builtin;
use super::chain::{self, Chain};
use super::native::Library;
use super::{BuildError, Channels, Failure, Param, ParamKind, Plugin};
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
/// `channels` is the count of the blocks the chain is given. A plugin that
/// does not fit the count that reaches it is a config error.
///
/// Errors name the plugin by its position in the config, from 1.
pub fn build_chain(
    config: &[PluginConfig],
    place: PluginPlace,
    rate: u32,
    format: Format,
    channels: usize,
    libraries: &mut Libraries,
) -> Result<Chain, BuildError> {
    let config_error = |m: String| BuildError::new(Failure::Config, m);
    let max_frames = MAX_PERIOD as usize;
    let mut plugins: Vec<Box<dyn Plugin>> = Vec::new();
    let mut positions = Vec::new();
    // The count that reaches the next plugin.
    let mut width = channels;
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
                    .create(rate, max_frames, width)
                    .map_err(|e| e.context(&prefix))?
            }
            ("library", None) => {
                return Err(config_error(format!(
                    "{prefix}type \"library\" needs a path"
                )));
            }
            (kind, _) => match builtin::create(kind, rate, width) {
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
        if let Channels::Fixed { output, .. } = plugin.channels() {
            width = output;
        }
        plugins.push(plugin);
        positions.push(n);
    }
    let has_detect = config
        .iter()
        .any(|p| p.place == place && p.kind == "detect");
    if place == PluginPlace::Analog && !has_detect {
        let detect = builtin::create("detect", rate, channels).and_then(Result::ok);
        plugins.push(detect.expect("detect is built in and takes every rate"));
    }
    if plugins.is_empty() {
        return Ok(Chain::empty());
    }
    if let Err((at, message)) = chain::fit(&plugins, channels) {
        let m = match at.and_then(|i| positions.get(i)) {
            Some(n) => format!("plugin {n}: {message}"),
            None => message,
        };
        return Err(config_error(m));
    }
    let chain = Chain::new(plugins, format, max_frames, channels).map_err(config_error)?;
    Ok(chain.for_rate(rate))
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

/// The channels of the blocks the `mix` and the `output` chain take, and
/// those the `output` chain gives, as the config's plugins make them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Widths {
    /// The width of the matrix.
    pub mix: usize,
    pub output: usize,
    /// What reaches the card.
    pub end: usize,
    /// A plugin of the `mix` or `output` chain gives another count than
    /// it takes.
    pub changes: bool,
}

/// The width of the matrix: walking the `mix` and then the `output` chain,
/// the input count of the first plugin with a fixed count, else `channels`,
/// the card's.
pub fn matrix_width(
    config: &[PluginConfig],
    channels: usize,
    libraries: &mut Libraries,
) -> Result<usize, BuildError> {
    widths(config, channels, libraries)
        .map(|w| w.mix)
        .map_err(|(_, e)| e)
}

/// The counts at the `mix` and `output` chains from the plugins' declared
/// counts, which loads every library they name. The matrix is as wide as
/// `matrix_width` says, and each fixed plugin then sets the count to its
/// output. A plugin that does not fit the count that reaches it is left to
/// the chain to refuse. Fails with the place of the plugin at fault.
pub fn widths(
    config: &[PluginConfig],
    channels: usize,
    libraries: &mut Libraries,
) -> Result<Widths, (PluginPlace, BuildError)> {
    let config_error = |m: String| BuildError::new(Failure::Config, m);
    let mut declared = |place: PluginPlace| -> Result<Vec<Channels>, BuildError> {
        let mut counts = Vec::new();
        for (i, p) in config.iter().enumerate().filter(|(_, p)| p.place == place) {
            let n = i + 1;
            let counts_of = match (p.kind.as_str(), &p.path) {
                ("library", Some(path)) => libraries
                    .get(path)
                    .map_err(|e| config_error(format!("plugin {n}: {e}")))?
                    .channels(),
                ("library", None) => {
                    return Err(config_error(format!(
                        "plugin {n}: type \"library\" needs a path"
                    )));
                }
                (kind, _) => builtin::channels(kind).ok_or_else(|| {
                    config_error(format!("plugin {n}: there is no plugin of type \"{kind}\""))
                })?,
            };
            counts.push(counts_of);
        }
        Ok(counts)
    };
    let mix = declared(PluginPlace::Mix).map_err(|e| (PluginPlace::Mix, e))?;
    let output = declared(PluginPlace::Output).map_err(|e| (PluginPlace::Output, e))?;
    let width = mix
        .iter()
        .chain(&output)
        .find_map(|c| match *c {
            Channels::Fixed { input, .. } => Some(input),
            Channels::Any => None,
        })
        .unwrap_or(channels);
    let after = |start: usize, plugins: &[Channels]| {
        plugins.iter().fold(start, |count, c| match *c {
            Channels::Fixed { output, .. } => output,
            Channels::Any => count,
        })
    };
    let at_output = after(width, &mix);
    let changes = mix.iter().chain(&output).any(|c| match *c {
        Channels::Fixed { input, output } => input != output,
        Channels::Any => false,
    });
    Ok(Widths {
        mix: width,
        output: at_output,
        end: after(at_output, &output),
        changes,
    })
}

/// The three chains, built for one rate.
pub struct Chains {
    pub rate: u32,
    pub analog: Chain,
    pub mix: Chain,
    pub output: Chain,
}

impl Chains {
    /// Builds every chain at `rate` for a card of `channels` channels:
    /// the analog chain for two, `mix` as wide as the matrix and `output`
    /// at what `mix` gives. This loads every library the config names. The
    /// first error stops it.
    pub fn build(
        config: &[PluginConfig],
        rate: u32,
        format: Format,
        channels: usize,
        libraries: &mut Libraries,
    ) -> Result<Chains, BuildError> {
        let w = widths(config, channels, libraries).map_err(|(_, e)| e)?;
        Chains::build_at(config, rate, format, w, libraries)
    }

    /// `build` with the counts already worked out by `widths`.
    fn build_at(
        config: &[PluginConfig],
        rate: u32,
        format: Format,
        w: Widths,
        libraries: &mut Libraries,
    ) -> Result<Chains, BuildError> {
        let mut build =
            |place, channels| build_chain(config, place, rate, format, channels, libraries);
        Ok(Chains {
            rate,
            analog: build(PluginPlace::Analog, 2)?,
            mix: build(PluginPlace::Mix, w.mix)?,
            output: build(PluginPlace::Output, w.output)?,
        })
    }

    /// Builds every chain at `rate` for the start of the daemon, for a card
    /// of `channels` channels. A plugin that does not support `rate` but
    /// whose chains build at one of `others` is handled by the control
    /// thread as for a rate the card refuses: that gives None. Any other
    /// failure, chains that build at none of the rates, and chains that do
    /// not end at `channels`, is an error, which stops the start.
    pub fn build_for_start(
        config: &[PluginConfig],
        rate: u32,
        others: &[u32],
        format: Format,
        channels: usize,
        libraries: &mut Libraries,
    ) -> Result<Option<Chains>, String> {
        let w = widths(config, channels, libraries).map_err(|(_, e)| e.message)?;
        if w.end != channels {
            return Err(format!(
                "the mix and output chains give {} channels, but the card has {channels}",
                w.end
            ));
        }
        let e = match Chains::build_at(config, rate, format, w, libraries) {
            Ok(chains) => return Ok(Some(chains)),
            Err(e) if e.kind == Failure::Unsupported => e,
            Err(e) => return Err(e.message),
        };
        for &other in others.iter().filter(|&&r| r != rate) {
            match Chains::build_at(config, other, format, w, libraries) {
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
        build_chain(config, place, 48000, Format::F32, 2, &mut libraries).map_err(|e| e.message)
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
        let e = Chains::build(&config, 48000, Format::F32, 2, &mut libraries)
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
            Chains::build_for_start(config, 96000, &[44100, 96000], Format::F32, 2, libraries)
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
            2,
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
        let mut chains = Chains::build(&config, 96000, Format::F64, 2, &mut libraries).unwrap();
        assert_eq!(chains.rate, 96000);
        assert_eq!(names(&chains.take(PluginPlace::Analog)), ["detect"]);
        assert!(chains.take(PluginPlace::Mix).is_empty());
        assert_eq!(names(&chains.take(PluginPlace::Output)), ["balance"]);
        assert!(chains.take(PluginPlace::Output).is_empty());
    }

    #[test]
    fn a_plugin_that_does_not_fit_the_channel_count_names_its_position() {
        let config = [
            plugin(PluginPlace::Mix, "balance", &[]),
            plugin(PluginPlace::Output, "balance", &[]),
        ];
        let mut libraries = Libraries::default();
        let e = build_chain(
            &config,
            PluginPlace::Output,
            48000,
            Format::F32,
            4,
            &mut libraries,
        );
        let e = e.err().unwrap();
        assert_eq!(e.kind, Failure::Config);
        assert!(e.message.starts_with("plugin 2: balance"), "{e}");
        assert!(e.message.contains('2') && e.message.contains('4'), "{e}");
    }

    #[test]
    fn the_matrix_is_as_wide_as_the_first_fixed_plugin_takes() {
        let mut libraries = Libraries::default();
        // The mix chain comes first, wherever the file lists it.
        let config = [
            plugin(PluginPlace::Output, "balance", &[]),
            plugin(PluginPlace::Mix, "narrow", &[]),
        ];
        assert_eq!(matrix_width(&config, 2, &mut libraries).unwrap(), 4);
        let w = widths(&config, 2, &mut libraries).unwrap();
        assert_eq!((w.mix, w.output, w.end), (4, 2, 2));
        assert!(w.changes);
        let kept = [plugin(PluginPlace::Mix, "balance", &[])];
        assert!(!widths(&kept, 2, &mut libraries).unwrap().changes);
        let start = Chains::build_for_start(&config, 48000, &[], Format::F32, 2, &mut libraries);
        let chains = start.unwrap().unwrap();
        assert_eq!(chains.mix.channels_in(), 4);
        assert_eq!(chains.output.channels_in(), 2);
        assert_eq!(chains.analog.channels_in(), 2);
        // Without a fixed plugin on the mix, the output's first one counts.
        let config = [plugin(PluginPlace::Output, "widen", &[])];
        assert_eq!(matrix_width(&config, 4, &mut libraries).unwrap(), 2);
        let chains = Chains::build(&config, 48000, Format::F64, 4, &mut libraries).unwrap();
        assert_eq!(chains.output.channels_out(), 4);
        assert!(chains.mix.is_empty());
    }

    #[test]
    fn without_a_fixed_plugin_the_matrix_is_as_wide_as_the_card() {
        let mut libraries = Libraries::default();
        assert_eq!(matrix_width(&[], 6, &mut libraries).unwrap(), 6);
        // The analog chain does not count.
        let config = [plugin(PluginPlace::Analog, "balance", &[])];
        assert_eq!(matrix_width(&config, 8, &mut libraries).unwrap(), 8);
        let e = matrix_width(
            &[plugin(PluginPlace::Mix, "reverb", &[])],
            2,
            &mut libraries,
        )
        .unwrap_err();
        assert!(e.message.starts_with("plugin 1: there is no plugin"), "{e}");
        // The error names the place of the plugin at fault.
        let config = [
            plugin(PluginPlace::Mix, "balance", &[]),
            plugin(PluginPlace::Output, "reverb", &[]),
        ];
        let (place, e) = widths(&config, 2, &mut libraries).unwrap_err();
        assert_eq!(place, PluginPlace::Output);
        assert!(e.message.starts_with("plugin 2: "), "{e}");
    }

    #[test]
    fn chains_that_do_not_end_at_the_cards_channels_are_refused_with_both_counts() {
        let mut libraries = Libraries::default();
        let config = [plugin(PluginPlace::Mix, "balance", &[])];
        let e = Chains::build_for_start(&config, 48000, &[], Format::F32, 4, &mut libraries)
            .err()
            .unwrap();
        assert!(
            e.contains("give 2 channels") && e.contains("card has 4"),
            "{e}"
        );
        let config = [plugin(PluginPlace::Output, "widen", &[])];
        let e = Chains::build_for_start(&config, 48000, &[], Format::F32, 2, &mut libraries)
            .err()
            .unwrap();
        assert!(
            e.contains("give 4 channels") && e.contains("card has 2"),
            "{e}"
        );
        assert!(
            Chains::build_for_start(&config, 48000, &[], Format::F32, 4, &mut libraries)
                .unwrap()
                .is_some()
        );
    }
}
