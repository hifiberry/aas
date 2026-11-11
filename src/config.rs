//! The config file: one flat TOML file, all keys optional.

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
    /// Every other key of the table. The chain builder checks the names.
    pub params: BTreeMap<String, f64>,
}

/// The settings, after the defaults are filled in. The README lists the
/// keys of the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The card to play to, as its ALSA id or index. None finds one.
    pub card: Option<String>,
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
    /// The card's analog input.
    pub analog: Analog,
    /// The plugins, in the order of the file.
    pub plugins: Vec<PluginConfig>,
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
    latency_ms: Option<f32>,
    default_rate: Option<u32>,
    format: Option<Format>,
    resampling: Option<Quality>,
    bit_perfect: Option<BitPerfect>,
    idle: Option<IdleKind>,
    idle_delay_s: Option<u32>,
    volume_control: Option<String>,
    volume_mode: Option<VolumeMode>,
    analog: Option<RawAnalog>,
    plugin: Option<Vec<toml::Table>>,
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
            _ => return Err(format!("plugin {n}: {key} must be a number")),
        };
        if !number.is_finite() {
            return Err(format!("plugin {n}: {key} must be a finite number"));
        }
        params.insert(key, number);
    }
    Ok(PluginConfig {
        place,
        kind,
        path,
        params,
    })
}

const DEFAULT_IDLE_DELAY_S: u32 = 60;

impl Default for Config {
    fn default() -> Config {
        Config {
            card: None,
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
            analog: Analog::default(),
            plugins: Vec::new(),
        }
    }
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
        let d = Config::default();
        let idle = match (raw.idle, raw.idle_delay_s) {
            (Some(IdleKind::Zeros), Some(_)) => {
                return Err(format!(
                    "{}: idle_delay_s needs idle = \"off\"",
                    path.display()
                ));
            }
            (Some(IdleKind::Zeros), None) => Idle::Zeros,
            (_, delay) => Idle::Off {
                delay_s: delay.unwrap_or(DEFAULT_IDLE_DELAY_S),
            },
        };
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
        let mut plugins = Vec::new();
        for (i, table) in raw.plugin.unwrap_or_default().into_iter().enumerate() {
            let plugin =
                plugin_from_table(i + 1, table).map_err(|e| format!("{}: {e}", path.display()))?;
            if plugin.kind == "detect" && plugins.iter().any(|p: &PluginConfig| p.kind == "detect")
            {
                return Err(format!(
                    "{}: plugin {}: only one detect plugin is allowed",
                    path.display(),
                    i + 1
                ));
            }
            plugins.push(plugin);
        }
        Ok(Config {
            card: raw.card,
            latency_ms: raw.latency_ms,
            default_rate: raw.default_rate.unwrap_or(d.default_rate),
            format: raw.format.unwrap_or(d.format),
            resampling: raw.resampling.unwrap_or(d.resampling),
            bit_perfect: raw.bit_perfect.unwrap_or(d.bit_perfect),
            idle,
            volume_control: raw.volume_control,
            volume_mode: raw.volume_mode,
            analog,
            plugins,
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
        assert_eq!(detect.params["threshold"], -50.0);
        assert_eq!(detect.params["hold"], 3.5);
        assert_eq!(detect.params.len(), 2);
        let lib = &c.plugins[1];
        assert_eq!(lib.place, PluginPlace::Output);
        assert_eq!(lib.kind, "library");
        assert_eq!(
            lib.path.as_deref(),
            Some(Path::new("/usr/lib/aas/libaas_gain.so"))
        );
        assert_eq!(lib.params.len(), 1);
        assert_eq!(lib.params["gain"], -3.0);
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
    fn a_parameter_that_is_not_a_number_is_an_error() {
        for v in ["\"loud\"", "true", "{ a = 1 }", "[1, 2]"] {
            let e = err(&format!(
                "[[plugin]]\nplace = \"mix\"\ntype = \"balance\"\ngain = {v}\n"
            ));
            assert!(e.contains("plugin 1") && e.contains("gain"), "{v}: {e}");
        }
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
}
