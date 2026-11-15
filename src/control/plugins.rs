//! Plugins as the API lists and changes them, and as the state file keeps
//! them. The control thread keeps an entry for each plugin of each output's
//! config, in the order of the config: the config with the values the
//! plugin has now, whether it is enabled, and the stats every chain built
//! for it publishes to. Every chain the control thread builds takes its
//! values from there, so a rebuild at a rate change keeps them.

use super::Control;
use crate::api::{
    self, ApiError, ApiEvent, ParamView, PluginId, PluginMetrics, PluginPatch, PluginView,
};
use crate::config::{ParamValue, PluginConfig, PluginPlace};
use crate::counters::PluginStats;
use crate::engine::{Command, Target};
use crate::plugin::build::{Libraries, describe, engine_place};
use crate::plugin::chain::Chain;
use crate::plugin::{Channels, Formats, Param, ParamKind};
use crate::state::{State, Stored, StoredPlugin};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

/// One plugin of an output's config, as the control thread keeps it.
pub(super) struct PluginEntry {
    pub id: PluginId,
    /// Its table, its params holding the values it has now.
    pub config: PluginConfig,
    /// The params as the config gives them, which a stored value is kept
    /// with.
    pub config_params: BTreeMap<String, ParamValue>,
    /// The descriptor's, or the built-in's; the type for one not found.
    pub name: String,
    pub params: Vec<Param>,
    /// Its channel counts differ, so it cannot be disabled.
    pub changes_count: bool,
    /// The formats it takes, which decide whether a chain converts for it.
    pub formats: Formats,
    pub enabled: bool,
    /// Shared with every chain built for it, so a rebuild keeps them.
    pub stats: Arc<PluginStats>,
}

impl PluginEntry {
    /// Its key in the state file, `"mix/0"`.
    fn key(&self) -> String {
        format!("{}/{}", self.id.place_name(), self.id.index)
    }

    fn path(&self) -> Option<String> {
        let path = self.config.path.as_ref();
        path.map(|p| p.to_string_lossy().into_owned())
    }

    /// The config's value of parameter `param`, or its default.
    fn config_value(&self, param: &Param) -> f64 {
        match self.config_params.get(&param.name) {
            Some(ParamValue::Number(v)) => *v,
            _ => param.default,
        }
    }

    /// The value of every parameter but the text ones, by name.
    fn values(&self) -> BTreeMap<String, f64> {
        let numbers = self.params.iter().filter(|p| p.kind != ParamKind::Text);
        numbers
            .map(|p| {
                let value = match self.config.params.get(&p.name) {
                    Some(ParamValue::Number(v)) => *v,
                    _ => p.default,
                };
                (p.name.clone(), value)
            })
            .collect()
    }

    /// Why it cannot be disabled, if it cannot.
    fn fixed(&self) -> Option<String> {
        if self.config.kind == "detect" {
            Some("detect cannot be disabled: the analog input needs it".into())
        } else if self.changes_count {
            Some(format!(
                "{} cannot be disabled: it changes the channel count",
                self.name
            ))
        } else {
            None
        }
    }

    fn view(&self) -> PluginView {
        let params = self
            .params
            .iter()
            .map(|p| {
                let (kind, values) = match &p.kind {
                    ParamKind::Float => ("float", None),
                    ParamKind::Integer => ("integer", None),
                    ParamKind::Switch => ("switch", None),
                    ParamKind::Enumeration(names) => ("enumeration", Some(names.clone())),
                    ParamKind::Text => ("text", None),
                };
                let value = match (self.config.params.get(&p.name), &p.kind) {
                    (Some(ParamValue::Number(v)), _) => serde_json::json!(v),
                    (Some(ParamValue::Text(t)), _) => serde_json::json!(t),
                    (None, ParamKind::Text) => serde_json::Value::Null,
                    (None, _) => serde_json::json!(p.default),
                };
                ParamView {
                    name: p.name.clone(),
                    unit: p.unit.clone(),
                    kind: kind.to_string(),
                    min: p.min,
                    max: p.max,
                    default: p.default,
                    values,
                    value,
                }
            })
            .collect();
        PluginView {
            id: self.id.to_string(),
            output: self.id.output,
            place: self.id.place_name().to_string(),
            index: self.id.index,
            kind: self.config.kind.clone(),
            path: self.path(),
            name: self.name.clone(),
            enabled: self.enabled,
            neutral: self.stats.neutral.load(Relaxed),
            latency_frames: self.stats.latency_frames.load(Relaxed),
            params,
        }
    }
}

/// The entries for the plugins of output `output`, numbered as in the API,
/// from its config, in the order of the config.
pub(super) fn entries(
    output: usize,
    config: &[PluginConfig],
    libraries: &mut Libraries,
) -> Vec<PluginEntry> {
    let mut seen = [0usize; 3];
    config
        .iter()
        .map(|p| {
            let place = engine_place(p.place);
            let slot = &mut seen[super::slot(p.place)];
            let index = *slot;
            *slot += 1;
            let every = Formats {
                i32: true,
                f32: true,
                f64: true,
            };
            let (name, params, channels, formats) = describe(p, libraries).unwrap_or((
                p.kind.clone(),
                Vec::new(),
                Channels::Any,
                every,
            ));
            let changes_count = match channels {
                Channels::Fixed { input, output } => input != output,
                Channels::Any => false,
            };
            let stats = PluginStats {
                neutral: true.into(),
                ..PluginStats::default()
            };
            PluginEntry {
                id: PluginId {
                    output,
                    place,
                    index,
                },
                config: p.clone(),
                config_params: p.params.clone(),
                name,
                params,
                changes_count,
                formats,
                enabled: true,
                stats: Arc::new(stats),
            }
        })
        .collect()
}

/// The tables the entries hold now, to build chains from.
pub(super) fn configs(entries: &[PluginEntry]) -> Vec<PluginConfig> {
    entries.iter().map(|e| e.config.clone()).collect()
}

/// Whether `entries` list a plugin at `place`.
pub(super) fn has_place(entries: &[PluginEntry], place: PluginPlace) -> bool {
    entries.iter().any(|e| e.config.place == place)
}

/// Gives `chain`, built for `place` and not run yet, the values, the
/// enabled state and the stats of `entries`. A chain built from the
/// entries has their values already; one built before the state file was
/// read does not. A detector the analog chain adds of its own, last, gets
/// stats of its own, which nothing reads.
pub(super) fn prepare(entries: &[PluginEntry], place: PluginPlace, mut chain: Chain) -> Chain {
    let at: Vec<&PluginEntry> = entries.iter().filter(|e| e.config.place == place).collect();
    let count = chain.names().count();
    for (i, entry) in at.iter().enumerate() {
        apply_values(&mut chain, i, entry, None);
        if !entry.enabled {
            chain.set_enabled_now(i, false);
        }
    }
    if count >= at.len() && count > 0 {
        let mut stats: Vec<Arc<PluginStats>> = at.iter().map(|e| e.stats.clone()).collect();
        stats.resize_with(count, Arc::default);
        chain = chain.with_stats(stats);
    }
    chain
}

/// Sets the values of `entry`, or only those named in `only`, on plugin
/// `plugin` of `chain`.
fn apply_values(
    chain: &mut Chain,
    plugin: usize,
    entry: &PluginEntry,
    only: Option<&BTreeMap<String, f64>>,
) {
    for (index, param) in entry.params.iter().enumerate() {
        let value = match only {
            Some(only) => only.get(&param.name).copied(),
            None => match entry.config.params.get(&param.name) {
                Some(ParamValue::Number(v)) => Some(*v),
                _ => None,
            },
        };
        if let Some(value) = value {
            chain.set_param_now(plugin, index, value);
        }
    }
}

/// Why `value` does not fit `param`, if it does not. Unlike the config's,
/// a float out of its range is refused, never clamped.
fn check(param: &Param, value: f64) -> Result<(), String> {
    let name = &param.name;
    let (min, max) = match &param.kind {
        ParamKind::Text => {
            return Err(format!(
                "{name} is a text parameter; it changes only in the config"
            ));
        }
        ParamKind::Switch => (0.0, 1.0),
        ParamKind::Enumeration(names) => (0.0, names.len() as f64 - 1.0),
        ParamKind::Float | ParamKind::Integer => (param.min, param.max),
    };
    if !(value.is_finite() && (min..=max).contains(&value)) {
        return Err(format!("{name} must be from {min} to {max}"));
    }
    if param.kind != ParamKind::Float && value.fract() != 0.0 {
        return Err(format!("{name} must be a whole number"));
    }
    Ok(())
}

impl Control {
    /// The entries of the output at engine index `index`.
    pub(super) fn entries_of(&self, index: usize) -> &[PluginEntry] {
        match index {
            0 => &self.plugins,
            _ => &self.further[index - 1].plugins,
        }
    }

    fn entries_of_mut(&mut self, index: usize) -> &mut Vec<PluginEntry> {
        match index {
            0 => &mut self.plugins,
            _ => &mut self.further[index - 1].plugins,
        }
    }

    /// Every entry, in chain order: by output, then place, then index.
    fn all_entries(&self) -> Vec<&PluginEntry> {
        let mut all: Vec<&PluginEntry> = (0..=self.further.len())
            .flat_map(|index| self.entries_of(index))
            .collect();
        let order = |e: &PluginEntry| {
            let place = super::slot(e.config.place);
            (e.id.output, place, e.id.index)
        };
        all.sort_by_key(|e| order(e));
        all
    }

    /// The engine index of the output and the position of the entry `id`
    /// names.
    fn find_entry(&self, id: &PluginId) -> Result<(usize, usize), ApiError> {
        let none = || ApiError::not_found(format!("no plugin {id}"));
        let index = id
            .output
            .checked_sub(1)
            .filter(|&i| i <= self.further.len());
        let index = index.ok_or_else(none)?;
        let at = self.entries_of(index).iter().position(|e| e.id == *id);
        Ok((index, at.ok_or_else(none)?))
    }

    pub(super) fn plugin_views(&self) -> Vec<PluginView> {
        self.all_entries().into_iter().map(|e| e.view()).collect()
    }

    pub(super) fn plugin_answer(&self, id: &PluginId) -> api::Answer {
        let (index, at) = self.find_entry(id)?;
        api::answer(&self.entries_of(index)[at].view())
    }

    /// Each plugin's time per cycle, by id.
    pub(super) fn plugin_metrics(&self) -> Vec<PluginMetrics> {
        self.all_entries()
            .into_iter()
            .map(|e| {
                let runs = e.stats.runs.load(Relaxed);
                PluginMetrics {
                    id: e.id.to_string(),
                    runs,
                    ns_avg: e
                        .stats
                        .total_ns
                        .load(Relaxed)
                        .checked_div(runs)
                        .unwrap_or(0),
                    ns_worst: e.stats.worst_ns.load(Relaxed),
                }
            })
            .collect()
    }

    /// Checks every value first, so that a refused patch changes nothing.
    /// Then sends each value and the enabled state to the output's chain
    /// on the audio thread, sets them in the chains held ready or spare,
    /// and keeps and stores them.
    pub(super) fn patch_plugin(
        &mut self,
        id: PluginId,
        patch: PluginPatch,
        now: Instant,
    ) -> api::Answer {
        let (index, at) = self.find_entry(&id)?;
        let entry = &self.entries_of(index)[at];
        let values = patch.params.unwrap_or_default();
        let mut changes = Vec::new();
        for (name, &value) in &values {
            let Some(param) = entry.params.iter().position(|p| p.name == *name) else {
                return Err(ApiError::unprocessable(format!(
                    "{} has no parameter {name}",
                    entry.name
                )));
            };
            check(&entry.params[param], value).map_err(ApiError::unprocessable)?;
            changes.push((param, value));
        }
        let enabled = patch.enabled.filter(|&on| on != entry.enabled);
        if enabled == Some(false)
            && let Some(why) = entry.fixed()
        {
            return Err(ApiError::unprocessable(why));
        }
        let plugin = u8::try_from(id.index)
            .map_err(|_| ApiError::unprocessable(format!("plugin {id} is beyond the 256th")))?;
        let target = Target {
            output: index,
            place: id.place,
        };
        let mut batch: Vec<Command> = changes
            .iter()
            .map(|&(param, value)| Command::SetParam {
                target,
                plugin,
                index: param as u16,
                value,
            })
            .collect();
        if let Some(enabled) = enabled {
            batch.push(Command::SetEnabled {
                target,
                plugin,
                enabled,
            });
        }
        let place = entry.config.place;
        if index == 0 {
            self.patch_held(place, id.index, &values, enabled);
        }
        let card = self.card_of(index).to_string();
        let entry = &mut self.entries_of_mut(index)[at];
        for (name, &value) in &values {
            entry
                .config
                .params
                .insert(name.clone(), ParamValue::Number(value));
        }
        if let Some(enabled) = enabled {
            entry.enabled = enabled;
        }
        let entry = &self.entries_of(index)[at];
        let event = ApiEvent::Plugin {
            id: id.to_string(),
            enabled: entry.enabled,
            params: entry.values(),
        };
        let view = entry.view();
        if !values.is_empty() || patch.enabled.is_some() {
            self.store_plugin(index, at, &card, &values, patch.enabled.is_some());
            self.send(batch);
            self.publish(event);
            self.save_later(now);
        }
        api::answer(&view)
    }

    /// Sets `values` and `enabled` on plugin `plugin` at `place` of the
    /// master's chains that are held ready or spare and have not run.
    fn patch_held(
        &mut self,
        place: PluginPlace,
        plugin: usize,
        values: &BTreeMap<String, f64>,
        enabled: Option<bool>,
    ) {
        let Some(entry) = self
            .plugins
            .iter()
            .find(|e| e.config.place == place && e.id.index == plugin)
        else {
            return;
        };
        let mut held: Vec<&mut Chain> = Vec::new();
        if let Some(ready) = &mut self.ready {
            held.push(match place {
                PluginPlace::Analog => &mut ready.analog,
                PluginPlace::Mix => &mut ready.mix,
                PluginPlace::Output => &mut ready.output,
            });
        }
        if let Some(spare) = &mut self.spare {
            let chains = spare.chains.iter_mut().filter(|(p, _)| *p == place);
            held.extend(chains.map(|(_, chain)| chain));
        }
        for chain in held {
            apply_values(chain, plugin, entry, Some(values));
            if let Some(enabled) = enabled {
                chain.set_enabled_now(plugin, enabled);
            }
        }
    }

    /// Keeps the entry at `at` of output `index` in the state, under
    /// `card`: the values just set, each with the config's value, and its
    /// enabled state if it was given. What was stored for another plugin
    /// at its place is replaced.
    fn store_plugin(
        &mut self,
        index: usize,
        at: usize,
        card: &str,
        values: &BTreeMap<String, f64>,
        enabled: bool,
    ) {
        let entry = &self.entries_of(index)[at];
        let (key, kind, path) = (entry.key(), entry.config.kind.clone(), entry.path());
        let now_enabled = entry.enabled;
        let stored_values: Vec<(String, Stored<f64>)> = values
            .iter()
            .filter_map(|(name, &value)| {
                let param = entry.params.iter().find(|p| p.name == *name)?;
                let config = entry.config_value(param);
                Some((name.clone(), Stored { value, config }))
            })
            .collect();
        let output = self.state.outputs.entry(card.to_string()).or_default();
        let fresh = || StoredPlugin {
            kind: kind.clone(),
            path: path.clone(),
            params: BTreeMap::new(),
            enabled: None,
        };
        let stored = output.plugins.entry(key).or_insert_with(fresh);
        if stored.kind != kind || stored.path != path {
            *stored = fresh();
        }
        stored.params.extend(stored_values);
        if enabled {
            stored.enabled = Some(now_enabled);
        }
    }

    /// Takes each plugin's values and enabled state from the state file,
    /// where they were stored for the same plugin at the same place.
    pub(super) fn restore_plugins(&mut self, stored: &State) {
        for index in 0..=self.further.len() {
            let card = self.card_of(index).to_string();
            let output = stored.outputs.get(&card);
            for entry in self.entries_of_mut(index) {
                let (key, path) = (entry.key(), entry.path());
                let kind = &entry.config.kind;
                let Some(plugin) = output.and_then(|o| o.plugin(&key, kind, path.as_deref()))
                else {
                    continue;
                };
                for param in &entry.params {
                    if param.kind == ParamKind::Text {
                        continue;
                    }
                    let config = entry.config_value(param);
                    let value = plugin.value(&param.name, config);
                    if value != config && check(param, value).is_ok() {
                        let value = ParamValue::Number(value);
                        entry.config.params.insert(param.name.clone(), value);
                    }
                }
                if plugin.enabled == Some(false) && entry.fixed().is_none() {
                    entry.enabled = false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{FakeEngine, further, has, play, started, started_with};
    use super::super::*;
    use crate::api::{self, ApiError, ApiEvent, Call, PluginId, PluginPatch};
    use crate::config::{self, ParamValue, PluginConfig};
    use crate::engine::Place;
    use crate::plugin::builtin::test::GAIN_SET;
    use crate::state::{StoredOutput, StoredPlugin};
    use serde_json::json;
    use tokio::sync::broadcast;

    fn plugin(place: PluginPlace, kind: &str) -> PluginConfig {
        PluginConfig {
            place,
            kind: kind.into(),
            path: None,
            params: Default::default(),
        }
    }

    /// `balance` at `output`, listed first, and `gain` at `mix`.
    fn config() -> Config {
        Config {
            plugins: vec![
                plugin(PluginPlace::Output, "balance"),
                plugin(PluginPlace::Mix, "gain"),
            ],
            ..Config::default()
        }
    }

    /// A started control thread whose master is open, with nothing queued
    /// and no `set_param` recorded.
    fn open(config: &Config) -> (Control, FakeEngine, Instant) {
        let (mut control, mut engine, now) = started(config, None);
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        GAIN_SET.with(|s| s.borrow_mut().clear());
        (control, engine, now)
    }

    fn id(text: &str) -> PluginId {
        let parts: Vec<&str> = text.split('/').collect();
        PluginId::parse(parts[0], parts[1], parts[2]).unwrap()
    }

    fn values(pairs: &[(&str, f64)]) -> PluginPatch {
        PluginPatch {
            params: Some(pairs.iter().map(|&(k, v)| (k.to_string(), v)).collect()),
            enabled: None,
        }
    }

    fn enabled(on: bool) -> PluginPatch {
        PluginPatch {
            params: None,
            enabled: Some(on),
        }
    }

    fn patch(control: &mut Control, at: &str, patch: PluginPatch, now: Instant) -> api::Answer {
        control.call(Call::PatchPlugin(id(at), patch), now)
    }

    fn status(answer: api::Answer) -> u16 {
        match answer {
            Ok(_) => 200,
            Err(ApiError { status, .. }) => status,
        }
    }

    fn get(control: &mut Control, at: &str, now: Instant) -> serde_json::Value {
        control.call(Call::Plugin(id(at)), now).unwrap()
    }

    fn master(place: Place) -> Target {
        Target::master(place)
    }

    fn recorded() -> Vec<(u32, usize, f64)> {
        GAIN_SET.with(|s| s.borrow().clone())
    }

    #[test]
    fn list_gives_every_plugin_in_order() {
        let (mut output, rates) = further(&[]);
        output.plugins = vec![plugin(PluginPlace::Mix, "balance")];
        let (mut control, _engine, _logs, now) = started_with(&config(), vec![(output, rates)]);
        let list = control.call(Call::Plugins, now).unwrap();
        let ids: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        // The chain order, not the config's.
        assert_eq!(ids, ["1/mix/0", "1/output/0", "2/mix/0"]);
        let gain = &list[0];
        assert_eq!(gain["output"], json!(1));
        assert_eq!(gain["place"], json!("mix"));
        assert_eq!(gain["index"], json!(0));
        assert_eq!(gain["type"], json!("gain"));
        assert_eq!(gain["path"], json!(null));
        assert_eq!(gain["name"], json!("gain"));
        assert_eq!(gain["enabled"], json!(true));
        assert_eq!(gain["latency_frames"], json!(0));
        assert_eq!(
            gain["params"][0],
            json!({"name": "gain", "unit": "dB", "type": "float",
                   "min": -60.0, "max": 20.0, "default": 0.0, "value": 0.0})
        );
        assert_eq!(gain["params"][1]["type"], json!("integer"));
        assert_eq!(gain["params"][2]["type"], json!("text"));
        assert_eq!(gain["params"][2]["value"], json!(null));
        let balance = &list[1];
        assert_eq!(balance["type"], json!("balance"));
        assert_eq!(balance["neutral"], json!(true));
        assert_eq!(
            balance["params"],
            json!([{"name": "balance", "unit": "", "type": "float",
                    "min": -1.0, "max": 1.0, "default": 0.0, "value": 0.0}])
        );
        assert_eq!(list[2]["output"], json!(2));
        // One alone, and ones that are not there.
        assert_eq!(get(&mut control, "1/output/0", now), list[1]);
        for missing in ["1/output/1", "1/analog/0", "3/mix/0"] {
            let answer = control.call(Call::Plugin(id(missing)), now);
            assert_eq!(status(answer), 404, "{missing}");
        }
    }

    #[test]
    fn a_text_parameter_shows_its_config_text() {
        let mut config = config();
        let text = ParamValue::Text("room.wav".into());
        config.plugins[1].params.insert("file".into(), text);
        let (mut control, _engine, now) = open(&config);
        let view = get(&mut control, "1/mix/0", now);
        assert_eq!(view["params"][2]["value"], json!("room.wav"));
    }

    #[test]
    fn a_value_goes_to_the_engine_and_the_entry() {
        let (mut control, mut engine, now) = open(&config());
        let answer = patch(&mut control, "1/output/0", values(&[("balance", 0.5)]), now);
        assert_eq!(answer.unwrap()["params"][0]["value"], json!(0.5));
        assert_eq!(engine.take(true), ["SetParam"]);
        assert_eq!(engine.params, [(master(Place::Output), 0, 0, 0.5)]);
        let view = get(&mut control, "1/output/0", now);
        assert_eq!(view["params"][0]["value"], json!(0.5));
        // On the mix, the second parameter.
        patch(&mut control, "1/mix/0", values(&[("steps", 3.0)]), now).unwrap();
        engine.take(true);
        assert_eq!(engine.params[1], (master(Place::Mix), 0, 1, 3.0));
    }

    #[test]
    fn a_value_out_of_range_is_422_and_not_sent() {
        let (mut control, mut engine, now) = open(&config());
        for bad in [1.5, -1.01, f64::NAN, f64::INFINITY] {
            let answer = patch(&mut control, "1/output/0", values(&[("balance", bad)]), now);
            let e = answer.unwrap_err();
            assert_eq!(e.status, 422, "{bad}");
            assert_eq!(e.message, "balance must be from -1 to 1", "{bad}");
        }
        // One bad value refuses the good ones with it.
        let both = values(&[("gain", -6.0), ("steps", 9.0)]);
        let e = patch(&mut control, "1/mix/0", both, now).unwrap_err();
        assert_eq!(
            (e.status, e.message.as_str()),
            (422, "steps must be from 0 to 8")
        );
        assert!(engine.take(true).is_empty());
        let view = get(&mut control, "1/mix/0", now);
        assert_eq!(view["params"][0]["value"], json!(0.0));
        assert_eq!(control.save_at, None);
    }

    #[test]
    fn an_unknown_parameter_is_422() {
        let (mut control, mut engine, now) = open(&config());
        let answer = patch(&mut control, "1/output/0", values(&[("gain", 0.0)]), now);
        assert_eq!(status(answer.clone()), 422);
        assert!(answer.unwrap_err().message.contains("gain"));
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn a_fraction_for_an_integer_is_422() {
        let (mut control, mut engine, now) = open(&config());
        let answer = patch(&mut control, "1/mix/0", values(&[("steps", 1.5)]), now);
        let e = answer.unwrap_err();
        assert_eq!(
            (e.status, e.message.as_str()),
            (422, "steps must be a whole number")
        );
        assert!(engine.take(true).is_empty());
        assert!(patch(&mut control, "1/mix/0", values(&[("steps", 2.0)]), now).is_ok());
    }

    #[test]
    fn a_text_parameter_is_422() {
        let (mut control, mut engine, now) = open(&config());
        let answer = patch(&mut control, "1/mix/0", values(&[("file", 1.0)]), now);
        let e = answer.unwrap_err();
        assert_eq!(e.status, 422);
        assert_eq!(
            e.message,
            "file is a text parameter; it changes only in the config"
        );
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn disabling_detect_is_422() {
        let config = Config {
            plugins: vec![plugin(PluginPlace::Analog, "detect")],
            ..Config::default()
        };
        let (mut control, mut engine, now) = open(&config);
        let answer = patch(&mut control, "1/analog/0", enabled(false), now);
        assert_eq!(status(answer), 422);
        assert!(engine.take(true).is_empty());
        // Its values change as any.
        let threshold = values(&[("threshold", -40.0)]);
        assert!(patch(&mut control, "1/analog/0", threshold, now).is_ok());
    }

    #[test]
    fn disabling_a_plugin_that_changes_the_count_is_422() {
        let config = Config {
            channels: 4,
            plugins: vec![plugin(PluginPlace::Output, "widen")],
            ..Config::default()
        };
        let (mut control, mut engine, now) = open(&config);
        let answer = patch(&mut control, "1/output/0", enabled(false), now);
        assert_eq!(status(answer), 422);
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn disabling_sends_set_enabled() {
        let (mut control, mut engine, now) = open(&config());
        let view = patch(&mut control, "1/output/0", enabled(false), now).unwrap();
        assert_eq!(view["enabled"], json!(false));
        assert_eq!(engine.take(true), ["SetEnabled"]);
        assert_eq!(engine.enables, [(master(Place::Output), 0, false)]);
        // The same again sends nothing.
        patch(&mut control, "1/output/0", enabled(false), now).unwrap();
        assert!(engine.take(true).is_empty());
        patch(&mut control, "1/output/0", enabled(true), now).unwrap();
        assert_eq!(engine.take(true), ["SetEnabled"]);
        assert_eq!(engine.enables[1], (master(Place::Output), 0, true));
    }

    #[test]
    fn a_value_survives_a_rate_change() {
        let (mut control, mut engine, now) = open(&config());
        patch(&mut control, "1/mix/0", values(&[("gain", -6.0)]), now).unwrap();
        patch(&mut control, "1/output/0", enabled(false), now).unwrap();
        engine.take(true);
        GAIN_SET.with(|s| s.borrow_mut().clear());
        play(&mut control, 1, 96000, now);
        let seen = engine.take(true);
        assert!(has(&seen, "SetChain Mix gain"), "{seen:?}");
        assert!(has(&seen, "Reconfigure 96000"), "{seen:?}");
        assert!(recorded().contains(&(96000, 0, -6.0)), "{:?}", recorded());
        // The chain built for the new rate has the disabled plugin
        // disabled, and the enabled one enabled.
        let chain = |place| {
            let sent = engine
                .chains
                .iter()
                .rev()
                .find(|(t, _)| *t == master(place));
            sent.unwrap().1.clone()
        };
        assert_eq!(chain(Place::Output), [false]);
        assert_eq!(chain(Place::Mix), [true]);
    }

    #[test]
    fn a_further_outputs_chain_keeps_its_values_too() {
        let (mut output, rates) = further(&[]);
        output.plugins = vec![plugin(PluginPlace::Mix, "gain")];
        let (mut control, mut engine, _logs, now) =
            started_with(&Config::default(), vec![(output, rates)]);
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        patch(&mut control, "2/mix/0", values(&[("gain", -3.0)]), now).unwrap();
        let target = Target {
            output: 1,
            place: Place::Mix,
        };
        engine.take(true);
        assert_eq!(engine.params, [(target, 0, 0, -3.0)]);
        patch(&mut control, "2/mix/0", enabled(false), now).unwrap();
        GAIN_SET.with(|s| s.borrow_mut().clear());
        play(&mut control, 1, 96000, now);
        let seen = engine.take(true);
        assert!(has(&seen, "SetChain 1 Mix gain"), "{seen:?}");
        assert!(recorded().contains(&(96000, 0, -3.0)), "{:?}", recorded());
        let sent = engine.chains.iter().rev().find(|(t, _)| *t == target);
        assert_eq!(sent.unwrap().1, [false]);
    }

    #[test]
    fn the_value_is_set_in_ready_chains() {
        let config = config();
        let (commands, engine_commands) = rtrb::RingBuffer::new(256);
        let (engine_events, events) = rtrb::RingBuffer::new(256);
        let audio = std::thread::current();
        let mut control = Control::new(&config, 12.0, Vec::new(), commands, events, audio);
        let mut libraries = Libraries::default();
        let chains = Chains::build(&config.plugins, 48000, Format::F32, 2, &mut libraries);
        control.set_plugins(libraries, Some(chains.unwrap()));
        let now = Instant::now();
        GAIN_SET.with(|s| s.borrow_mut().clear());
        patch(&mut control, "1/mix/0", values(&[("gain", -9.0)]), now).unwrap();
        patch(&mut control, "1/output/0", enabled(false), now).unwrap();
        // Nothing runs yet: only the chains held ready have it.
        assert_eq!(recorded(), [(48000, 0, -9.0)]);
        control.start(now);
        let mut engine = FakeEngine::new(engine_commands, engine_events);
        let seen = engine.take(true);
        assert!(has(&seen, "SetChain Output balance"), "{seen:?}");
        let sent = engine
            .chains
            .iter()
            .find(|(t, _)| *t == master(Place::Output));
        assert_eq!(sent.unwrap().1, [false]);
    }

    #[test]
    fn the_value_is_set_in_spare_chains() {
        let (mut control, _engine, now) = open(&config());
        // Chains built for a rate the output is not at yet, as a rate
        // change that waits holds them.
        let chains = control.chains_for(96000, false).ok().unwrap();
        control.spare = Some(Spare {
            rate: 96000,
            dop: false,
            chains,
        });
        GAIN_SET.with(|s| s.borrow_mut().clear());
        patch(&mut control, "1/mix/0", values(&[("gain", -12.0)]), now).unwrap();
        patch(&mut control, "1/output/0", enabled(false), now).unwrap();
        assert!(recorded().contains(&(96000, 0, -12.0)), "{:?}", recorded());
        let spare = control.spare.as_ref().unwrap();
        let (_, output) = spare
            .chains
            .iter()
            .find(|(p, _)| *p == PluginPlace::Output)
            .unwrap();
        assert!(!output.enabled(0));
        // Disabled before it runs: passed over from its first block.
        assert!(output.neutral());
    }

    #[test]
    fn a_patch_publishes_a_plugin_event() {
        let (mut control, _engine, now) = open(&config());
        let (events, mut published) = broadcast::channel(16);
        control.set_api(events);
        patch(&mut control, "1/mix/0", values(&[("gain", -6.0)]), now).unwrap();
        let event = published.try_recv().unwrap();
        assert_eq!(
            event,
            ApiEvent::Plugin {
                id: "1/mix/0".into(),
                enabled: true,
                params: [("gain".to_string(), -6.0), ("steps".to_string(), 0.0)].into(),
            }
        );
        // A refused patch publishes nothing.
        let bad = values(&[("gain", 100.0)]);
        assert!(patch(&mut control, "1/mix/0", bad, now).is_err());
        assert!(published.try_recv().is_err());
    }

    fn restored(config: &Config, path: &std::path::Path) -> (Control, FakeEngine, Instant) {
        let (commands, engine_commands) = rtrb::RingBuffer::new(256);
        let (engine_events, events) = rtrb::RingBuffer::new(256);
        let audio = std::thread::current();
        let mut control = Control::new(config, 12.0, Vec::new(), commands, events, audio);
        let mut libraries = Libraries::default();
        let chains = Chains::build(&config.plugins, 48000, Format::F32, 2, &mut libraries);
        control.set_plugins(libraries, Some(chains.unwrap()));
        let volume = OutputVolume::new(None, config::BitPerfect::Auto);
        GAIN_SET.with(|s| s.borrow_mut().clear());
        control.restore(volume, path.to_path_buf(), &["card".to_string()]);
        let now = Instant::now();
        control.start(now);
        let engine = FakeEngine::new(engine_commands, engine_events);
        (control, engine, now)
    }

    #[test]
    fn a_change_is_stored_and_in_the_first_chain_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let mut config = config();
        config.card = Some("card".into());
        config.plugins[1]
            .params
            .insert("steps".into(), ParamValue::Number(2.0));
        let (mut control, _engine, now) = restored(&config, &path);
        patch(
            &mut control,
            "1/mix/0",
            values(&[("gain", -6.0), ("steps", 4.0)]),
            now,
        )
        .unwrap();
        assert!(control.save_at.is_some());
        patch(&mut control, "1/output/0", enabled(false), now).unwrap();
        control.shutdown();
        let state = State::load(&path);
        let stored = &state.outputs["card"].plugins;
        let gain = &stored["mix/0"];
        assert_eq!((gain.kind.as_str(), gain.path.as_deref()), ("gain", None));
        // Each value with the config's, or the default without one.
        assert_eq!(
            gain.params["gain"],
            Stored {
                value: -6.0,
                config: 0.0
            }
        );
        assert_eq!(
            gain.params["steps"],
            Stored {
                value: 4.0,
                config: 2.0
            }
        );
        assert_eq!(stored["output/0"].enabled, Some(false));
        // After a restart the first chains have them.
        let (mut control, mut engine, now) = restored(&config, &path);
        let seen = engine.take(true);
        assert!(has(&seen, "SetChain Mix gain"), "{seen:?}");
        assert!(recorded().contains(&(48000, 0, -6.0)), "{:?}", recorded());
        assert!(recorded().contains(&(48000, 1, 4.0)), "{:?}", recorded());
        let sent = engine
            .chains
            .iter()
            .find(|(t, _)| *t == master(Place::Output));
        assert_eq!(sent.unwrap().1, [false]);
        let view = get(&mut control, "1/output/0", now);
        assert_eq!(view["enabled"], json!(false));
        // An edit of the config's value wins, and a plugin of another type
        // takes nothing.
        let mut edited = config.clone();
        edited.plugins[1]
            .params
            .insert("steps".into(), ParamValue::Number(3.0));
        edited.plugins[0].kind = "only-up-to-48k".into();
        let (mut control, _engine, now) = restored(&edited, &path);
        let view = get(&mut control, "1/mix/0", now);
        assert_eq!(view["params"][0]["value"], json!(-6.0));
        assert_eq!(view["params"][1]["value"], json!(3.0));
        let view = get(&mut control, "1/output/0", now);
        assert_eq!(view["enabled"], json!(true));
    }

    #[test]
    fn a_stored_value_for_a_plugin_now_elsewhere_stays_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let mut state = State::default();
        let mut output = StoredOutput::default();
        let other = StoredPlugin {
            kind: "library".into(),
            path: Some("/usr/lib/a.so".into()),
            params: [(
                "gain".to_string(),
                Stored {
                    value: -6.0,
                    config: 0.0,
                },
            )]
            .into(),
            enabled: None,
        };
        output.plugins.insert("mix/0".into(), other.clone());
        state.outputs.insert("card".into(), output);
        state.save(&path).unwrap();
        let mut config = config();
        config.card = Some("card".into());
        let (mut control, _engine, now) = restored(&config, &path);
        let view = get(&mut control, "1/mix/0", now);
        assert_eq!(view["params"][0]["value"], json!(0.0));
        // A change to another plugin saves the file with it still there.
        patch(&mut control, "1/output/0", values(&[("balance", 0.1)]), now).unwrap();
        control.shutdown();
        assert_eq!(State::load(&path).outputs["card"].plugins["mix/0"], other);
    }

    #[test]
    fn metrics_give_each_plugin_by_its_id() {
        let (mut control, _engine, now) = open(&config());
        let view = control.call(Call::Metrics, now).unwrap();
        let plugins = view["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 2);
        assert_eq!(
            plugins[0],
            json!({"id": "1/mix/0", "runs": 0, "ns_avg": 0, "ns_worst": 0})
        );
        assert_eq!(plugins[1]["id"], json!("1/output/0"));
    }
}
