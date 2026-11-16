//! The config file: one flat TOML file, all keys optional.

use crate::matrix::{INPUT_NAMES, MAX_CHANNELS};
use crate::rate::{MAX_RATE, MIN_RATE};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Default config path.
pub const DEFAULT_PATH: &str = "/etc/aas/aas.toml";

/// Sample format used for processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    I32,
    F32,
    F64,
}

/// Who applies the volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VolumeMode {
    Hardware,
    Software,
    /// aas follows and writes the named control, and applies the volume
    /// itself.
    Follow,
}

/// Resampler quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    High,
    Medium,
}

/// Bit-perfect mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BitPerfect {
    Auto,
    Forced,
    Off,
}

/// What an output does when no source is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idle {
    /// Keep running and send zeros.
    Zeros,
    /// Stop after `delay_s` seconds of zeros.
    Off { delay_s: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum IdleKind {
    Zeros,
    Off,
}

/// The card's analog input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Analog {
    /// Whether the input is used. It needs a card with an ADC.
    pub enabled: bool,
    /// The level of the input in dB, from -60 to 0.
    pub volume_db: f32,
}

impl Default for Analog {
    fn default() -> Analog {
        Analog {
            enabled: false,
            volume_db: 0.0,
        }
    }
}

/// Where in the signal path a plugin runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginPlace {
    /// On the analog input, before it is mixed.
    Analog,
    /// On the mix of all sources.
    Mix,
    /// On the final output, after the volume.
    Output,
}

/// One `[[plugin]]` table.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginConfig {
    pub place: PluginPlace,
    /// The `type` key: a built-in name, or `library`.
    pub kind: String,
    /// The shared library of a `library` plugin.
    pub path: Option<PathBuf>,
    /// Every other key of the table. The chain builder checks the names,
    /// and each value against the type of its parameter.
    pub params: BTreeMap<String, ParamValue>,
}

/// The value of a plugin parameter in the config.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    /// For every parameter but a text one. Finite.
    Number(f64),
    /// For a text parameter, passed to the plugin as written.
    Text(String),
}

/// One `[[matrix]]` table: a gain from an input to an output channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Route {
    /// The input, as an index into `matrix::INPUT_NAMES`.
    pub from: usize,
    /// The output channel, counted from 1.
    pub to: usize,
    /// From -100 to 0.
    pub gain_db: f32,
}

/// The most outputs there can be, the master included.
pub const MAX_OUTPUTS: usize = 4;

/// One `[[output]]` table: a further card to play to.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputConfig {
    /// The card, as its ALSA id or index.
    pub card: String,
    /// The channels the card is opened with, from 1 to `MAX_CHANNELS`.
    pub channels: usize,
    /// The target for the latency of the card's buffer, as the flat key's.
    /// None takes the master's.
    pub latency_ms: Option<f32>,
    /// What the output does when nothing plays.
    pub idle: Idle,
    /// The output's offset to the `Master` volume in dB, from -60 to 0.
    pub volume_db: f32,
    /// The plugins of this output, in the order of the file.
    pub plugins: Vec<PluginConfig>,
    /// The routes of this output's matrix. None gives the default matrix.
    pub matrix: Option<Vec<Route>>,
}

/// The settings, after the defaults are filled in. The README lists the
/// keys of the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The card to play to, as its ALSA id or index. None finds one.
    pub card: Option<String>,
    /// The channels the card is opened with, from 1 to `MAX_CHANNELS`.
    pub channels: usize,
    /// The target for the latency of the card's buffer. None takes the
    /// default of the board.
    pub latency_ms: Option<f32>,
    /// The rate the output opens at before the first client plays, from
    /// `MIN_RATE` to `MAX_RATE`.
    pub default_rate: u32,
    /// The arithmetic used to mix and to apply volume.
    pub format: Format,
    /// The quality of the resampler for a client at another rate.
    pub resampling: Quality,
    /// When samples are copied to the card untouched.
    pub bit_perfect: BitPerfect,
    /// What the output does when nothing plays.
    pub idle: Idle,
    /// The card's volume control to use. None takes the first of
    /// `Digital`, `Master` and `PCM` the card has.
    pub volume_control: Option<String>,
    /// Who applies the volume. None uses the card's control if it has one.
    pub volume_mode: Option<VolumeMode>,
    /// How often, in ms, the card's volume control is checked for a change
    /// made by another program, 50 to 5000.
    pub volume_poll_ms: u32,
    /// The card's analog input.
    pub analog: Analog,
    /// The plugins, in the order of the file.
    pub plugins: Vec<PluginConfig>,
    /// The routes of the mixing matrix, in the order of the file. None without
    /// a `[[matrix]]` table, and for an empty `matrix = []`: both give the
    /// default matrix, never one with every route off.
    pub matrix: Option<Vec<Route>>,
    /// The further outputs, in the order of the file. The flat keys are the
    /// master, output 1.
    pub outputs: Vec<OutputConfig>,
    /// The port the API listens on, on 127.0.0.1. 0 turns the API off.
    pub api_port: u16,
    /// The PulseAudio socket. None is the client socket's directory plus
    /// `pulse`; an empty path turns the endpoint off.
    pub pulse_socket: Option<PathBuf>,
    /// The default and cap for a PulseAudio stream's buffer, in ms.
    pub pulse_buffer_ms: Option<u16>,
}

/// The port of the API unless the config names another.
pub const DEFAULT_API_PORT: u16 = 2718;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutput {
    card: Option<String>,
    channels: Option<usize>,
    latency_ms: Option<f32>,
    idle: Option<IdleKind>,
    idle_delay_s: Option<u32>,
    volume_db: Option<toml::Value>,
    plugin: Option<Vec<toml::Table>>,
    matrix: Option<Vec<RawRoute>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnalog {
    enabled: Option<bool>,
    volume_db: Option<f32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    card: Option<String>,
    channels: Option<usize>,
    latency_ms: Option<f32>,
    default_rate: Option<u32>,
    format: Option<Format>,
    resampling: Option<Quality>,
    bit_perfect: Option<BitPerfect>,
    idle: Option<IdleKind>,
    idle_delay_s: Option<u32>,
    volume_control: Option<String>,
    volume_mode: Option<VolumeMode>,
    volume_poll_ms: Option<u32>,
    analog: Option<RawAnalog>,
    plugin: Option<Vec<toml::Table>>,
    matrix: Option<Vec<RawRoute>>,
    output: Option<Vec<RawOutput>>,
    api_port: Option<toml::Value>,
    pulse_socket: Option<toml::Value>,
    pulse_buffer_ms: Option<toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    from: Option<toml::Value>,
    to: Option<toml::Value>,
    gain_db: Option<toml::Value>,
}

/// Checks one `[[matrix]]` table. `n` is its position in the file, from 1.
fn route_from_table(n: usize, raw: RawRoute) -> Result<Route, String> {
    let from = match raw.from {
        None => return Err(format!("matrix {n}: from is missing")),
        Some(toml::Value::String(name)) => name,
        Some(_) => return Err(format!("matrix {n}: from must be a string")),
    };
    let to = match raw.to {
        None => return Err(format!("matrix {n}: to is missing")),
        Some(toml::Value::Integer(v)) => v,
        Some(_) => return Err(format!("matrix {n}: to must be a whole number")),
    };
    let gain_db = match raw.gain_db {
        None => 0.0,
        Some(toml::Value::Integer(v)) => v as f64,
        Some(toml::Value::Float(v)) => v,
        Some(_) => return Err(format!("matrix {n}: gain_db must be a number")),
    };
    route_from_parts(n, &from, to, gain_db)
}

/// Checks a route given by its parts, as a `[[matrix]]` table or the API
/// gives it. `n` is its position, from 1. A table and the API give the
/// same messages.
pub fn route_from_parts(n: usize, from: &str, to: i64, gain_db: f64) -> Result<Route, String> {
    let from = INPUT_NAMES
        .iter()
        .position(|known| *known == from)
        .ok_or_else(|| {
            format!(
                "matrix {n}: from \"{from}\" must be one of {}",
                INPUT_NAMES.map(|known| format!("\"{known}\"")).join(", ")
            )
        })?;
    if !(1..=MAX_CHANNELS as i64).contains(&to) {
        return Err(format!(
            "matrix {n}: to must be a channel from 1 to {MAX_CHANNELS}"
        ));
    }
    // Not finite is out of range too.
    if !(-100.0..=0.0).contains(&gain_db) {
        return Err(format!("matrix {n}: gain_db must be from -100 to 0"));
    }
    Ok(Route {
        from,
        to: to as usize,
        gain_db: gain_db as f32,
    })
}

const BUILT_IN_PLUGINS: [&str; 2] = ["detect", "balance"];

/// Checks one `[[plugin]]` table. `n` is its position in the file, from 1.
fn plugin_from_table(n: usize, mut table: toml::Table) -> Result<PluginConfig, String> {
    let mut take_str = |key: &str| match table.remove(key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("plugin {n}: {key} must be a string")),
    };
    let place = match take_str("place")?.as_deref() {
        Some("analog") => PluginPlace::Analog,
        Some("mix") => PluginPlace::Mix,
        Some("output") => PluginPlace::Output,
        Some(other) => {
            return Err(format!(
                "plugin {n}: place \"{other}\" must be \"analog\", \"mix\" or \"output\""
            ));
        }
        None => return Err(format!("plugin {n}: place is missing")),
    };
    let kind = take_str("type")?.ok_or(format!("plugin {n}: type is missing"))?;
    let path = take_str("path")?.map(PathBuf::from);
    match (kind.as_str(), &path) {
        ("library", None) => return Err(format!("plugin {n}: type \"library\" needs a path")),
        ("library", Some(p)) if !p.is_absolute() => {
            return Err(format!(
                "plugin {n}: path \"{}\" must be absolute",
                p.display()
            ));
        }
        ("library", Some(_)) => {}
        (k, Some(_)) if BUILT_IN_PLUGINS.contains(&k) => {
            return Err(format!("plugin {n}: path is only for type \"library\""));
        }
        (k, _) if BUILT_IN_PLUGINS.contains(&k) => {}
        (other, _) => {
            return Err(format!(
                "plugin {n}: type \"{other}\" must be \"detect\", \"balance\" or \"library\""
            ));
        }
    }
    if kind == "detect" && place != PluginPlace::Analog {
        return Err(format!(
            "plugin {n}: detect only runs with place = \"analog\""
        ));
    }
    let mut params = BTreeMap::new();
    for (key, value) in table {
        let number = match value {
            toml::Value::Float(f) => f,
            toml::Value::Integer(i) => i as f64,
            toml::Value::String(text) => {
                params.insert(key, ParamValue::Text(text));
                continue;
            }
            _ => return Err(format!("plugin {n}: {key} must be a number or text")),
        };
        if !number.is_finite() {
            return Err(format!("plugin {n}: {key} must be a finite number"));
        }
        params.insert(key, ParamValue::Number(number));
    }
    Ok(PluginConfig {
        place,
        kind,
        path,
        params,
    })
}

/// The default of `volume_poll_ms`.
const DEFAULT_VOLUME_POLL_MS: u32 = 100;

/// The delay of `idle = "off"` without `idle_delay_s`.
pub const DEFAULT_IDLE_DELAY_S: u32 = 60;

impl Default for Config {
    fn default() -> Config {
        Config {
            card: None,
            channels: 2,
            latency_ms: None,
            default_rate: 48000,
            format: Format::F32,
            resampling: Quality::High,
            bit_perfect: BitPerfect::Auto,
            idle: Idle::Off {
                delay_s: DEFAULT_IDLE_DELAY_S,
            },
            volume_control: None,
            volume_mode: None,
            volume_poll_ms: DEFAULT_VOLUME_POLL_MS,
            analog: Analog::default(),
            plugins: Vec::new(),
            matrix: None,
            outputs: Vec::new(),
            api_port: DEFAULT_API_PORT,
            pulse_socket: None,
            pulse_buffer_ms: None,
        }
    }
}

fn idle_from(kind: Option<IdleKind>, delay: Option<u32>) -> Result<Idle, String> {
    match (kind, delay) {
        (Some(IdleKind::Zeros), Some(_)) => Err("idle_delay_s needs idle = \"off\"".to_string()),
        (Some(IdleKind::Zeros), None) => Ok(Idle::Zeros),
        (_, delay) => Ok(Idle::Off {
            delay_s: delay.unwrap_or(DEFAULT_IDLE_DELAY_S),
        }),
    }
}

/// Checks the `[[plugin]]` tables of one output. The master takes the analog
/// place and `detect`; a further output does not.
fn plugins_from(tables: Vec<toml::Table>, master: bool) -> Result<Vec<PluginConfig>, String> {
    let mut plugins = Vec::new();
    for (i, table) in tables.into_iter().enumerate() {
        let plugin = plugin_from_table(i + 1, table)?;
        if !master && plugin.place == PluginPlace::Analog {
            return Err(format!(
                "plugin {}: place \"analog\" is only for the master output",
                i + 1
            ));
        }
        if !master && plugin.kind == "detect" {
            return Err(format!(
                "plugin {}: detect is only for the master output",
                i + 1
            ));
        }
        if plugin.kind == "detect" && plugins.iter().any(|p: &PluginConfig| p.kind == "detect") {
            return Err(format!(
                "plugin {}: only one detect plugin is allowed",
                i + 1
            ));
        }
        plugins.push(plugin);
    }
    Ok(plugins)
}

fn matrix_from(tables: Option<Vec<RawRoute>>) -> Result<Option<Vec<Route>>, String> {
    match tables {
        None => Ok(None),
        Some(tables) if tables.is_empty() => Ok(None),
        Some(tables) => tables
            .into_iter()
            .enumerate()
            .map(|(i, table)| route_from_table(i + 1, table))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
    }
}

/// Checks one `[[output]]` table. `n` is its output number, from 2.
fn output_from_table(n: usize, raw: RawOutput) -> Result<OutputConfig, String> {
    let at = |e: String| format!("output {n}: {e}");
    let card = raw.card.ok_or_else(|| at("card is missing".to_string()))?;
    if raw
        .channels
        .is_some_and(|c| !(1..=MAX_CHANNELS).contains(&c))
    {
        return Err(at(format!("channels must be from 1 to {MAX_CHANNELS}")));
    }
    if raw
        .latency_ms
        .is_some_and(|ms| !(ms.is_finite() && ms > 0.0))
    {
        return Err(at("latency_ms must be a positive number".to_string()));
    }
    let volume_db = match raw.volume_db {
        None => 0.0,
        Some(toml::Value::Integer(v)) => v as f64,
        Some(toml::Value::Float(v)) => v,
        Some(_) => return Err(at("volume_db must be a number".to_string())),
    };
    if !(-60.0..=0.0).contains(&volume_db) {
        return Err(at("volume_db must be from -60 to 0".to_string()));
    }
    Ok(OutputConfig {
        card,
        channels: raw.channels.unwrap_or(2),
        latency_ms: raw.latency_ms,
        idle: idle_from(raw.idle, raw.idle_delay_s).map_err(at)?,
        volume_db: volume_db as f32,
        plugins: plugins_from(raw.plugin.unwrap_or_default(), false).map_err(at)?,
        matrix: matrix_from(raw.matrix).map_err(at)?,
    })
}

impl Config {
    /// Loads the file. A missing file gives the defaults; an unknown key or a
    /// bad value is an error.
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let raw: Raw = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if raw
            .latency_ms
            .is_some_and(|ms| !(ms.is_finite() && ms > 0.0))
        {
            return Err(format!(
                "{}: latency_ms must be a positive number",
                path.display()
            ));
        }
        if raw
            .default_rate
            .is_some_and(|rate| !(MIN_RATE..=MAX_RATE).contains(&rate))
        {
            return Err(format!(
                "{}: default_rate must be from {MIN_RATE} to {MAX_RATE}",
                path.display()
            ));
        }
        if raw
            .channels
            .is_some_and(|n| !(1..=MAX_CHANNELS).contains(&n))
        {
            return Err(format!(
                "{}: channels must be from 1 to {MAX_CHANNELS}",
                path.display()
            ));
        }
        let d = Config::default();
        let idle = idle_from(raw.idle, raw.idle_delay_s)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let volume_poll_ms = raw.volume_poll_ms.unwrap_or(d.volume_poll_ms);
        if !(50..=5000).contains(&volume_poll_ms) {
            return Err(format!(
                "{}: volume_poll_ms must be from 50 to 5000",
                path.display()
            ));
        }
        if raw.volume_mode == Some(VolumeMode::Follow) && raw.volume_control.is_none() {
            return Err(format!(
                "{}: volume_mode = \"follow\" needs volume_control",
                path.display()
            ));
        }
        let mut analog = d.analog;
        if let Some(a) = raw.analog {
            analog.enabled = a.enabled.unwrap_or(analog.enabled);
            analog.volume_db = a.volume_db.unwrap_or(analog.volume_db);
        }
        if !(-60.0..=0.0).contains(&analog.volume_db) {
            return Err(format!(
                "{}: analog volume_db must be from -60 to 0",
                path.display()
            ));
        }
        let plugins = plugins_from(raw.plugin.unwrap_or_default(), true)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let matrix = matrix_from(raw.matrix).map_err(|e| format!("{}: {e}", path.display()))?;
        let raw_outputs = raw.output.unwrap_or_default();
        if raw_outputs.len() >= MAX_OUTPUTS {
            return Err(format!(
                "{}: at most {} [[output]] tables, so {MAX_OUTPUTS} outputs with the master",
                path.display(),
                MAX_OUTPUTS - 1
            ));
        }
        if !raw_outputs.is_empty() && raw.card.is_none() {
            return Err(format!(
                "{}: card is needed to name the master's card when there are [[output]] tables",
                path.display()
            ));
        }
        let api_port = match raw.api_port {
            None => d.api_port,
            Some(toml::Value::Integer(port)) if (0..=i64::from(u16::MAX)).contains(&port) => {
                port as u16
            }
            Some(_) => {
                return Err(format!(
                    "{}: api_port must be from 0 to 65535",
                    path.display()
                ));
            }
        };
        let pulse_socket = match raw.pulse_socket {
            None => None,
            Some(toml::Value::String(path)) => Some(PathBuf::from(path)),
            Some(_) => {
                return Err(format!("{}: pulse_socket must be a path", path.display()));
            }
        };
        let pulse_buffer_ms = match raw.pulse_buffer_ms {
            None => None,
            Some(toml::Value::Integer(ms)) if (20..=2000).contains(&ms) => Some(ms as u16),
            Some(_) => {
                return Err(format!(
                    "{}: pulse_buffer_ms must be from 20 to 2000",
                    path.display()
                ));
            }
        };
        if pulse_socket
            .as_ref()
            .is_some_and(|p| p.as_os_str().is_empty())
            && pulse_buffer_ms.is_some()
        {
            return Err(format!(
                "{}: pulse_buffer_ms must not be set while the PulseAudio endpoint is off",
                path.display()
            ));
        }
        let mut outputs: Vec<OutputConfig> = Vec::new();
        for (i, table) in raw_outputs.into_iter().enumerate() {
            let n = i + 2;
            let out =
                output_from_table(n, table).map_err(|e| format!("{}: {e}", path.display()))?;
            let earlier = raw
                .card
                .iter()
                .map(|c| (1, c))
                .chain(outputs.iter().enumerate().map(|(j, o)| (j + 2, &o.card)))
                .find(|(_, c)| **c == out.card);
            if let Some((first, _)) = earlier {
                return Err(format!(
                    "{}: card \"{}\" is used by output {first} and output {n}",
                    path.display(),
                    out.card
                ));
            }
            outputs.push(out);
        }
        Ok(Config {
            card: raw.card,
            channels: raw.channels.unwrap_or(d.channels),
            latency_ms: raw.latency_ms,
            default_rate: raw.default_rate.unwrap_or(d.default_rate),
            format: raw.format.unwrap_or(d.format),
            resampling: raw.resampling.unwrap_or(d.resampling),
            bit_perfect: raw.bit_perfect.unwrap_or(d.bit_perfect),
            idle,
            volume_control: raw.volume_control,
            volume_mode: raw.volume_mode,
            volume_poll_ms,
            analog,
            plugins,
            matrix,
            outputs,
            api_port,
            pulse_socket,
            pulse_buffer_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn load_str(text: &str) -> Result<Config, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aas.toml");
        std::fs::write(&path, text).unwrap();
        Config::load(&path)
    }

    #[test]
    fn a_missing_config_file_gives_the_defaults() {
        let c = Config::load(Path::new("/nonexistent/aas.toml")).unwrap();
        assert_eq!(c.format, Format::F32);
        assert_eq!(c.resampling, Quality::High);
        assert_eq!(c.bit_perfect, BitPerfect::Auto);
        assert_eq!(c.idle, Idle::Off { delay_s: 60 });
        assert_eq!(c.default_rate, 48000);
        assert_eq!(c.card, None);
        assert_eq!(c.latency_ms, None);
        assert_eq!(c.volume_control, None);
        assert_eq!(c.volume_mode, None);
        assert_eq!(c.volume_poll_ms, 100);
    }

    #[test]
    fn volume_poll_ms_is_loaded_and_checked() {
        assert_eq!(
            load_str("volume_poll_ms = 250\n").unwrap().volume_poll_ms,
            250
        );
        for bad in [49, 5001] {
            let e = load_str(&format!("volume_poll_ms = {bad}\n")).unwrap_err();
            assert!(e.contains("volume_poll_ms"), "{e}");
        }
    }

    #[test]
    fn volume_mode_follow_loads_with_a_control() {
        let c = load_str("volume_mode = \"follow\"\nvolume_control = \"Softvol\"\n").unwrap();
        assert_eq!(c.volume_mode, Some(VolumeMode::Follow));
        assert_eq!(c.volume_control.as_deref(), Some("Softvol"));
    }

    #[test]
    fn volume_mode_follow_needs_a_control() {
        let e = load_str("volume_mode = \"follow\"\n").unwrap_err();
        assert!(e.contains("volume_control"), "{e}");
    }

    #[test]
    fn api_port_defaults_to_2718() {
        assert_eq!(load_str("").unwrap().api_port, 2718);
        assert_eq!(Config::default().api_port, 2718);
        assert_eq!(load_str("api_port = 8080\n").unwrap().api_port, 8080);
    }

    #[test]
    fn api_port_0_turns_it_off() {
        assert_eq!(load_str("api_port = 0\n").unwrap().api_port, 0);
    }

    #[test]
    fn api_port_70000_is_an_error() {
        let e = err("api_port = 70000\n");
        assert!(e.contains("api_port must be from 0 to 65535"), "{e}");
        assert!(err("api_port = -1\n").contains("api_port"));
        assert!(err("api_port = \"2718\"\n").contains("api_port"));
    }

    #[test]
    fn a_route_from_parts_gives_the_messages_of_a_table() {
        let route = route_from_parts(1, "players.right", 2, -6.0).unwrap();
        assert_eq!(
            route,
            Route {
                from: 1,
                to: 2,
                gain_db: -6.0
            }
        );
        let e = route_from_parts(3, "players.middle", 1, 0.0).unwrap_err();
        assert!(
            e.starts_with("matrix 3: from \"players.middle\" must be one of"),
            "{e}"
        );
        let e = route_from_parts(2, "players.left", 0, 0.0).unwrap_err();
        assert_eq!(e, "matrix 2: to must be a channel from 1 to 8");
        let e = route_from_parts(2, "players.left", 1, 0.5).unwrap_err();
        assert_eq!(e, "matrix 2: gain_db must be from -100 to 0");
        assert!(route_from_parts(2, "players.left", 1, f64::NAN).is_err());
    }

    #[test]
    fn channels_defaults_to_two_and_takes_one_to_eight() {
        assert_eq!(load_str("").unwrap().channels, 2);
        for n in 1..=8 {
            assert_eq!(load_str(&format!("channels = {n}\n")).unwrap().channels, n);
        }
    }

    #[test]
    fn nine_channels_are_an_error() {
        for v in ["0", "9", "-1", "\"2\""] {
            let e = load_str(&format!("channels = {v}\n")).unwrap_err();
            assert!(e.contains("channels"), "{v}: {e}");
        }
    }

    #[test]
    fn an_unknown_key_is_an_error() {
        assert!(load_str("lateny_ms = 4.0\n").is_err());
    }

    #[test]
    fn a_latency_that_is_not_positive_and_finite_is_an_error() {
        for v in ["nan", "inf", "-inf", "0.0", "-1.0"] {
            assert!(load_str(&format!("latency_ms = {v}\n")).is_err(), "{v}");
        }
    }

    #[test]
    fn a_default_rate_no_client_could_use_is_an_error() {
        for v in [0, 1, 7999, 384001, 4_000_000_000u32] {
            assert!(load_str(&format!("default_rate = {v}\n")).is_err(), "{v}");
        }
        for v in [8000, 44100, 384000] {
            let c = load_str(&format!("default_rate = {v}\n")).unwrap();
            assert_eq!(c.default_rate, v);
        }
    }

    #[test]
    fn a_bad_value_is_an_error() {
        assert!(load_str("format = \"i16\"\n").is_err());
        assert!(load_str("idle = \"sleep\"\n").is_err());
    }

    #[test]
    fn every_format_name_parses() {
        for (name, f) in [
            ("i32", Format::I32),
            ("f32", Format::F32),
            ("f64", Format::F64),
        ] {
            assert_eq!(
                load_str(&format!("format = \"{name}\"\n")).unwrap().format,
                f
            );
        }
    }

    #[test]
    fn a_full_file_parses() {
        let c = load_str(
            "card = \"sndrpihifiberry\"\nlatency_ms = 6.5\ndefault_rate = 44100\n\
             format = \"f64\"\nresampling = \"medium\"\nbit_perfect = \"forced\"\n\
             idle = \"off\"\nidle_delay_s = 10\nvolume_control = \"Digital\"\n\
             volume_mode = \"software\"\n",
        )
        .unwrap();
        assert_eq!(c.card.as_deref(), Some("sndrpihifiberry"));
        assert_eq!(c.latency_ms, Some(6.5));
        assert_eq!(c.default_rate, 44100);
        assert_eq!(c.resampling, Quality::Medium);
        assert_eq!(c.bit_perfect, BitPerfect::Forced);
        assert_eq!(c.idle, Idle::Off { delay_s: 10 });
        assert_eq!(c.volume_control.as_deref(), Some("Digital"));
        assert_eq!(c.volume_mode, Some(VolumeMode::Software));
    }

    #[test]
    fn idle_zeros_parses() {
        assert_eq!(load_str("idle = \"zeros\"\n").unwrap().idle, Idle::Zeros);
    }

    fn err(text: &str) -> String {
        load_str(text).unwrap_err()
    }

    #[test]
    fn matrix_routes_are_read_in_file_order() {
        let c = load_str(
            "[[matrix]]\nfrom = \"players.right\"\nto = 1\n\
             [[matrix]]\nfrom = \"analog.left\"\nto = 2\ngain_db = -6\n\
             [[matrix]]\nfrom = \"players.left\"\nto = 8\ngain_db = -0.5\n",
        )
        .unwrap();
        assert_eq!(
            c.matrix.unwrap(),
            [
                Route {
                    from: 1,
                    to: 1,
                    gain_db: 0.0
                },
                Route {
                    from: 2,
                    to: 2,
                    gain_db: -6.0
                },
                Route {
                    from: 0,
                    to: 8,
                    gain_db: -0.5
                },
            ]
        );
    }

    #[test]
    fn a_matrix_route_with_an_unknown_source_is_an_error() {
        let e = err("[[matrix]]\nfrom = \"players.left\"\nto = 1\n\
             [[matrix]]\nfrom = \"players\"\nto = 1\n");
        assert!(
            e.contains("matrix 2") && e.contains("from \"players\""),
            "{e}"
        );
        assert!(e.contains("aas.toml"), "{e}");
        let e = err("[[matrix]]\nfrom = \"players.left\"\nto = 1\nvia = 2\n");
        assert!(e.contains("via"), "{e}");
    }

    #[test]
    fn a_matrix_gain_above_0_db_is_an_error() {
        for v in ["0.5", "1", "-101", "nan", "\"-3\""] {
            let e = err(&format!(
                "[[matrix]]\nfrom = \"players.left\"\nto = 1\ngain_db = {v}\n"
            ));
            assert!(e.contains("matrix 1") && e.contains("gain_db"), "{v}: {e}");
        }
        assert!(load_str("[[matrix]]\nfrom = \"players.left\"\nto = 1\ngain_db = -100\n").is_ok());
    }

    #[test]
    fn a_matrix_route_without_to_is_an_error() {
        let e = err("[[matrix]]\nfrom = \"players.left\"\n");
        assert!(e.contains("matrix 1") && e.contains("to"), "{e}");
        let e = err("[[matrix]]\nto = 1\n");
        assert!(e.contains("matrix 1") && e.contains("from"), "{e}");
        for v in ["0", "9", "1.5", "\"1\""] {
            let e = err(&format!("[[matrix]]\nfrom = \"players.left\"\nto = {v}\n"));
            assert!(e.contains("matrix 1") && e.contains("to"), "{v}: {e}");
        }
    }

    #[test]
    fn no_matrix_table_gives_none() {
        assert_eq!(load_str("").unwrap().matrix, None);
        assert_eq!(Config::default().matrix, None);
        assert_eq!(load_str("matrix = []\n").unwrap().matrix, None);
    }

    #[test]
    fn the_analog_input_is_off_by_default() {
        for c in [Config::default(), load_str("").unwrap()] {
            assert_eq!(
                c.analog,
                Analog {
                    enabled: false,
                    volume_db: 0.0
                }
            );
            assert!(c.plugins.is_empty());
        }
        let c = load_str("[analog]\nenabled = true\nvolume_db = -6.5\n").unwrap();
        assert_eq!(
            c.analog,
            Analog {
                enabled: true,
                volume_db: -6.5
            }
        );
    }

    #[test]
    fn plugins_keep_the_order_of_the_file() {
        let c = load_str(
            "[[plugin]]\nplace = \"output\"\ntype = \"balance\"\n\
             [[plugin]]\nplace = \"analog\"\ntype = \"detect\"\n\
             [[plugin]]\nplace = \"mix\"\ntype = \"balance\"\n",
        )
        .unwrap();
        let places: Vec<_> = c.plugins.iter().map(|p| p.place).collect();
        assert_eq!(
            places,
            [PluginPlace::Output, PluginPlace::Analog, PluginPlace::Mix]
        );
    }

    #[test]
    fn a_plugin_table_carries_its_parameters() {
        let c = load_str(
            "[analog]\nenabled = true\nvolume_db = 0\n\
             [[plugin]]\nplace = \"analog\"\ntype = \"detect\"\nthreshold = -50\nhold = 3.5\n\
             [[plugin]]\nplace = \"output\"\ntype = \"library\"\n\
             path = \"/usr/lib/aas/libaas_gain.so\"\ngain = -3\n",
        )
        .unwrap();
        let detect = &c.plugins[0];
        assert_eq!(detect.kind, "detect");
        assert_eq!(detect.path, None);
        assert_eq!(detect.params["threshold"], ParamValue::Number(-50.0));
        assert_eq!(detect.params["hold"], ParamValue::Number(3.5));
        assert_eq!(detect.params.len(), 2);
        let lib = &c.plugins[1];
        assert_eq!(lib.place, PluginPlace::Output);
        assert_eq!(lib.kind, "library");
        assert_eq!(
            lib.path.as_deref(),
            Some(Path::new("/usr/lib/aas/libaas_gain.so"))
        );
        assert_eq!(lib.params.len(), 1);
        assert_eq!(lib.params["gain"], ParamValue::Number(-3.0));
    }

    #[test]
    fn an_unknown_place_is_an_error() {
        let e = err("[[plugin]]\nplace = \"input\"\ntype = \"balance\"\n");
        assert!(e.contains("plugin 1") && e.contains("place"), "{e}");
        let e = err("[[plugin]]\ntype = \"balance\"\n");
        assert!(e.contains("plugin 1") && e.contains("place"), "{e}");
        let e = err("[[plugin]]\nplace = \"mix\"\ntype = \"reverb\"\n");
        assert!(e.contains("plugin 1") && e.contains("reverb"), "{e}");
        let e = err("[[plugin]]\nplace = \"mix\"\n");
        assert!(e.contains("plugin 1") && e.contains("type"), "{e}");
    }

    #[test]
    fn a_library_without_a_path_is_an_error() {
        let e = err("[[plugin]]\nplace = \"mix\"\ntype = \"library\"\n");
        assert!(e.contains("plugin 1") && e.contains("path"), "{e}");
    }

    #[test]
    fn a_library_path_that_is_not_absolute_is_an_error() {
        for path in [
            "libaas_gain.so",
            "lib/libaas_gain.so",
            "./libaas_gain.so",
            "",
        ] {
            let e = err(&format!(
                "[[plugin]]\nplace = \"mix\"\ntype = \"library\"\npath = \"{path}\"\n"
            ));
            assert!(
                e.contains("plugin 1: path") && e.contains("absolute"),
                "{e}"
            );
        }
    }

    #[test]
    fn a_path_on_a_built_in_is_an_error() {
        let e = err("[[plugin]]\nplace = \"mix\"\ntype = \"balance\"\npath = \"/x.so\"\n");
        assert!(e.contains("plugin 1") && e.contains("path"), "{e}");
    }

    #[test]
    fn detect_outside_the_analog_chain_is_an_error() {
        for place in ["mix", "output"] {
            let e = err(&format!(
                "[[plugin]]\nplace = \"{place}\"\ntype = \"detect\"\n"
            ));
            assert!(e.contains("plugin 1") && e.contains("detect"), "{e}");
        }
    }

    #[test]
    fn two_detect_entries_are_an_error() {
        let e = err("[[plugin]]\nplace = \"analog\"\ntype = \"detect\"\n\
             [[plugin]]\nplace = \"analog\"\ntype = \"detect\"\n");
        assert!(e.contains("plugin 2") && e.contains("detect"), "{e}");
    }

    #[test]
    fn a_volume_outside_the_range_is_an_error() {
        for v in ["-60.5", "0.5", "nan", "inf"] {
            let e = err(&format!("[analog]\nvolume_db = {v}\n"));
            assert!(e.contains("volume_db"), "{v}: {e}");
        }
        for v in ["-60", "0", "-12.5"] {
            assert!(
                load_str(&format!("[analog]\nvolume_db = {v}\n")).is_ok(),
                "{v}"
            );
        }
        assert!(load_str("[analog]\nvolum_db = 0\n").is_err());
    }

    #[test]
    fn a_parameter_that_is_not_a_number_or_text_is_an_error() {
        for v in ["true", "{ a = 1 }", "[1, 2]"] {
            let e = err(&format!(
                "[[plugin]]\nplace = \"mix\"\ntype = \"balance\"\ngain = {v}\n"
            ));
            assert!(e.contains("plugin 1") && e.contains("gain"), "{v}: {e}");
        }
    }

    #[test]
    fn a_parameter_in_quotes_is_text_as_written() {
        let c = load_str(
            "[[plugin]]\nplace = \"mix\"\ntype = \"library\"\n\
             path = \"/usr/lib/aas/libconv.so\"\nfilter = \"/etc/aas/room 1.wav\"\n",
        )
        .unwrap();
        let text = ParamValue::Text("/etc/aas/room 1.wav".into());
        assert_eq!(c.plugins[0].params["filter"], text);
    }

    #[test]
    fn a_parameter_that_is_not_finite_is_an_error() {
        for v in ["nan", "inf", "-inf", "+nan"] {
            let e = err(&format!(
                "[[plugin]]\nplace = \"mix\"\ntype = \"balance\"\ngain = {v}\n"
            ));
            assert!(e.contains("plugin 1") && e.contains("gain"), "{v}: {e}");
        }
    }

    #[test]
    fn no_output_table_leaves_outputs_empty() {
        assert!(load_str("card = \"a\"\n").unwrap().outputs.is_empty());
        assert!(load_str("").unwrap().outputs.is_empty());
    }

    #[test]
    fn an_output_table_takes_its_own_matrix_and_plugins() {
        let c = load_str(
            r#"
card = "a"
[[output]]
card = "b"
[[output.matrix]]
from = "players.left"
to = 2
gain_db = -6
[[output.plugin]]
place = "output"
type = "balance"
[[output]]
card = "c"
channels = 4
[[output.matrix]]
from = "players.right"
to = 3
[[output.plugin]]
place = "mix"
type = "balance"
[[output.plugin]]
place = "output"
type = "balance"
"#,
        )
        .unwrap();
        assert_eq!(c.outputs.len(), 2);
        assert_eq!(c.outputs[0].card, "b");
        assert_eq!(c.outputs[0].plugins.len(), 1);
        assert_eq!(c.outputs[0].plugins[0].place, PluginPlace::Output);
        let m = c.outputs[0].matrix.as_ref().unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].to, m[0].gain_db), (2, -6.0));
        assert_eq!(c.outputs[1].card, "c");
        assert_eq!(c.outputs[1].channels, 4);
        assert_eq!(c.outputs[1].plugins.len(), 2);
        assert_eq!(c.outputs[1].matrix.as_ref().unwrap()[0].to, 3);
        assert!(c.matrix.is_none() && c.plugins.is_empty());
    }

    #[test]
    fn an_output_without_a_card_is_refused() {
        let e = load_str("card = \"a\"\n[[output]]\nchannels = 2\n").unwrap_err();
        assert!(e.contains("output 2") && e.contains("card"), "{e}");
    }

    #[test]
    fn two_outputs_on_one_card_are_refused() {
        let e = load_str("card = \"a\"\n[[output]]\ncard = \"b\"\n[[output]]\ncard = \"b\"\n")
            .unwrap_err();
        assert!(
            e.contains("\"b\"") && e.contains("output 2") && e.contains("output 3"),
            "{e}"
        );
        let e = load_str("card = \"a\"\n[[output]]\ncard = \"a\"\n").unwrap_err();
        assert!(
            e.contains("\"a\"") && e.contains("output 1") && e.contains("output 2"),
            "{e}"
        );
    }

    #[test]
    fn outputs_need_the_master_card_named() {
        let e = load_str("[[output]]\ncard = \"b\"\n").unwrap_err();
        assert!(e.contains("card") && e.contains("output"), "{e}");
    }

    #[test]
    fn a_fifth_output_is_refused() {
        let tables = |n: usize| {
            let mut t = String::from("card = \"m\"\n");
            for i in 0..n {
                t += &format!("[[output]]\ncard = \"c{i}\"\n");
            }
            t
        };
        assert_eq!(load_str(&tables(MAX_OUTPUTS - 1)).unwrap().outputs.len(), 3);
        let e = load_str(&tables(MAX_OUTPUTS)).unwrap_err();
        assert!(e.contains("at most") && e.contains('4'), "{e}");
    }

    #[test]
    fn analog_plugins_and_detect_are_refused_in_an_output() {
        for plugin in [
            "place = \"analog\"\ntype = \"balance\"",
            "place = \"analog\"\ntype = \"detect\"",
            "place = \"mix\"\ntype = \"detect\"",
        ] {
            let e = load_str(&format!(
                "card = \"a\"\n[[output]]\ncard = \"b\"\n[[output.plugin]]\n{plugin}\n"
            ))
            .unwrap_err();
            assert!(e.contains("output 2") && e.contains("plugin 1"), "{e}");
        }
    }

    #[test]
    fn an_output_takes_its_own_latency_by_the_flat_rule() {
        let one = |v: &str| load_str(&format!("card = \"a\"\n[[output]]\ncard = \"b\"\n{v}\n"));
        // None follows the master's target.
        assert_eq!(one("").unwrap().outputs[0].latency_ms, None);
        assert_eq!(
            one("latency_ms = 48").unwrap().outputs[0].latency_ms,
            Some(48.0)
        );
        assert_eq!(
            one("latency_ms = 6.5").unwrap().outputs[0].latency_ms,
            Some(6.5)
        );
        for v in ["0", "-1", "nan", "inf", "\"x\""] {
            let e = one(&format!("latency_ms = {v}")).unwrap_err();
            assert!(
                e.contains("output 2") || e.contains("latency_ms"),
                "{v}: {e}"
            );
        }
        let e = one("latency_ms = 0").unwrap_err();
        assert!(e.contains("output 2") && e.contains("latency_ms"), "{e}");
    }

    #[test]
    fn output_volume_db_is_minus_60_to_0_and_0_by_default() {
        let one = |v: &str| load_str(&format!("card = \"a\"\n[[output]]\ncard = \"b\"\n{v}\n"));
        assert_eq!(one("").unwrap().outputs[0].volume_db, 0.0);
        assert_eq!(one("volume_db = -60").unwrap().outputs[0].volume_db, -60.0);
        assert_eq!(
            one("volume_db = -12.5").unwrap().outputs[0].volume_db,
            -12.5
        );
        for v in ["-60.5", "0.5", "\"x\"", "nan"] {
            let e = one(&format!("volume_db = {v}")).unwrap_err();
            assert!(
                e.contains("output 2") && e.contains("volume_db"),
                "{v}: {e}"
            );
        }
    }

    #[test]
    fn an_output_takes_idle_and_channels_by_the_flat_rules() {
        let c = load_str(
            "card = \"a\"\n[[output]]\ncard = \"b\"\n[[output]]\ncard = \"c\"\nidle = \"zeros\"\n\
             [[output]]\ncard = \"d\"\nidle_delay_s = 5\nchannels = 8\n",
        )
        .unwrap();
        assert_eq!(c.outputs[0].idle, Idle::Off { delay_s: 60 });
        assert_eq!(c.outputs[0].channels, 2);
        assert_eq!(c.outputs[1].idle, Idle::Zeros);
        assert_eq!(c.outputs[2].idle, Idle::Off { delay_s: 5 });
        assert_eq!(c.outputs[2].channels, 8);
        let e = load_str(
            "card = \"a\"\n[[output]]\ncard = \"b\"\nidle = \"zeros\"\nidle_delay_s = 5\n",
        )
        .unwrap_err();
        assert!(e.contains("output 2") && e.contains("idle_delay_s"), "{e}");
        for v in ["0", "9"] {
            let e = load_str(&format!(
                "card = \"a\"\n[[output]]\ncard = \"b\"\nchannels = {v}\n"
            ))
            .unwrap_err();
            assert!(e.contains("output 2") && e.contains("channels"), "{v}: {e}");
        }
        let e = load_str(
            "card = \"a\"\n[[output]]\ncard = \"b\"\n[[output.matrix]]\nfrom = \"players.left\"\nto = 9\n",
        )
        .unwrap_err();
        assert!(e.contains("output 2") && e.contains("matrix 1"), "{e}");
        assert!(load_str("card = \"a\"\n[[output]]\ncard = \"b\"\nbogus = 1\n").is_err());
    }

    #[test]
    fn pulse_keys_have_their_defaults() {
        let c = load_str("").unwrap();
        assert_eq!(c.pulse_socket, None);
        assert_eq!(c.pulse_buffer_ms, None);
    }

    #[test]
    fn an_empty_pulse_socket_turns_the_endpoint_off() {
        assert_eq!(
            load_str("pulse_socket = \"\"\n").unwrap().pulse_socket,
            Some(PathBuf::new())
        );
        assert_eq!(
            load_str("pulse_socket = \"/tmp/pulse\"\n")
                .unwrap()
                .pulse_socket,
            Some(PathBuf::from("/tmp/pulse"))
        );
    }

    #[test]
    fn pulse_buffer_ms_is_20_to_2000() {
        assert!(load_str("pulse_buffer_ms = 20\n").is_ok());
        assert!(load_str("pulse_buffer_ms = 2000\n").is_ok());
        assert!(load_str("pulse_buffer_ms = 19\n").is_err());
        assert!(load_str("pulse_buffer_ms = 2001\n").is_err());
        assert!(load_str("pulse_buffer_ms = \"200\"\n").is_err());
    }

    #[test]
    fn a_pulse_buffer_with_the_endpoint_off_is_an_error() {
        let e = err("pulse_socket = \"\"\npulse_buffer_ms = 200\n");
        assert!(e.contains("pulse_buffer_ms") && e.contains("off"), "{e}");
    }
}
