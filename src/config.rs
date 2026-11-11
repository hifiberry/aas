//! The config file: one flat TOML file, all keys optional.

use crate::rate::{MAX_RATE, MIN_RATE};
use serde::Deserialize;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

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
}
