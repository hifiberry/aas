//! The `[aes67]` table of the config file.

use std::net::Ipv4Addr;

/// Where the PTP clock comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtpSource {
    /// Follow a grandmaster on the network; aas sends no PTP packet.
    Listen,
    /// No PTP clock.
    Off,
}

/// The input's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Latency {
    /// By board, see `latency_ms`.
    Auto,
    Ms(u32),
}

/// The output's multicast group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// 239.69.x.y from the interface's MAC.
    Auto,
    Addr(Ipv4Addr),
}

/// `[aes67.input]`.
#[derive(Debug, Clone, PartialEq)]
pub struct InputConfig {
    pub enabled: bool,
    /// The SDP session name of the stream to receive; empty selects none.
    pub stream: String,
    pub latency: Latency,
    /// The two stream channels played as left and right, counted from 1.
    pub channels: [u8; 2],
    pub volume_db: f32,
}

/// `[aes67.output]`.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputConfig {
    pub enabled: bool,
    /// The SDP session name.
    pub name: String,
    /// `address` in the file.
    pub group: Group,
    pub port: u16,
    pub ttl: u8,
}

/// The `[aes67]` table, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Aes67Config {
    pub interface: String,
    pub ptp: PtpSource,
    /// The PTP domain.
    pub domain: u8,
    pub input: InputConfig,
    pub output: OutputConfig,
}

const KEYS: [&str; 5] = ["interface", "ptp", "domain", "input", "output"];
const INPUT_KEYS: [&str; 5] = ["enabled", "stream", "latency_ms", "channels", "volume_db"];
const OUTPUT_KEYS: [&str; 5] = ["enabled", "name", "address", "port", "ttl"];

/// The lowest and highest latency, in ms.
const LATENCY_RANGE: (i64, i64) = (1, 100);

/// The latency to run with, in ms. `model` is the board's name, from
/// `/proc/device-tree/model`.
pub fn latency_ms(latency: Latency, model: Option<&str>) -> u32 {
    match latency {
        Latency::Ms(ms) => ms,
        Latency::Auto => {
            let fast = model.is_some_and(|m| {
                let m = m.to_ascii_lowercase();
                m.contains("compute module 5") || m.contains("pi 5")
            });
            if fast { 3 } else { 20 }
        }
    }
}

/// The `[aes67]` table, checked; no table is no AES67.
pub fn from_table(table: Option<toml::Table>) -> Result<Option<Aes67Config>, String> {
    let Some(table) = table else {
        return Ok(None);
    };
    from_checked(table)
        .map(Some)
        .map_err(|e| format!("aes67: {e}"))
}

fn unknown(table: &toml::Table, keys: &[&str], prefix: &str) -> Result<(), String> {
    match table.keys().find(|k| !keys.contains(&k.as_str())) {
        Some(key) => Err(format!("unknown key {prefix}{key}")),
        None => Ok(()),
    }
}

/// A whole number in `lo..=hi`, or `None` for a key that is not there.
fn whole(
    table: &toml::Table,
    key: &str,
    prefix: &str,
    lo: i64,
    hi: i64,
) -> Result<Option<i64>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Integer(v)) if (lo..=hi).contains(v) => Ok(Some(*v)),
        Some(_) => Err(format!("{prefix}{key} must be from {lo} to {hi}")),
    }
}

fn boolean(table: &toml::Table, prefix: &str, default: bool) -> Result<bool, String> {
    let key = "enabled";
    match table.get(key) {
        None => Ok(default),
        Some(toml::Value::Boolean(b)) => Ok(*b),
        Some(_) => Err(format!("{prefix}{key} must be true or false")),
    }
}

fn string(table: &toml::Table, key: &str, prefix: &str, default: &str) -> Result<String, String> {
    match table.get(key) {
        None => Ok(default.into()),
        Some(toml::Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("{prefix}{key} must be a string")),
    }
}

/// A sub-table, or an empty one when it is not there.
fn sub(table: &toml::Table, key: &str) -> Result<toml::Table, String> {
    match table.get(key) {
        None => Ok(toml::Table::new()),
        Some(toml::Value::Table(t)) => Ok(t.clone()),
        Some(_) => Err(format!("{key} must be a table")),
    }
}

fn from_checked(table: toml::Table) -> Result<Aes67Config, String> {
    unknown(&table, &KEYS, "")?;
    let interface = match table.get("interface") {
        None => "eth0".to_string(),
        Some(toml::Value::String(s)) if !s.is_empty() => s.clone(),
        Some(_) => return Err("interface must be a name".into()),
    };
    let ptp = match table.get("ptp") {
        None => PtpSource::Listen,
        Some(toml::Value::String(s)) if s == "listen" => PtpSource::Listen,
        Some(toml::Value::String(s)) if s == "off" => PtpSource::Off,
        Some(_) => return Err("ptp must be \"listen\" or \"off\"".into()),
    };
    let domain = whole(&table, "domain", "", 0, 255)?.unwrap_or(0) as u8;
    let input = input_from(&sub(&table, "input")?)?;
    let output = output_from(&sub(&table, "output")?)?;
    Ok(Aes67Config {
        interface,
        ptp,
        domain,
        input,
        output,
    })
}

fn input_from(table: &toml::Table) -> Result<InputConfig, String> {
    const P: &str = "input.";
    unknown(table, &INPUT_KEYS, P)?;
    let (lo, hi) = LATENCY_RANGE;
    let latency = match table.get("latency_ms") {
        None => Latency::Auto,
        Some(toml::Value::String(s)) if s == "auto" => Latency::Auto,
        Some(toml::Value::Integer(v)) if (lo..=hi).contains(v) => Latency::Ms(*v as u32),
        Some(_) => {
            return Err(format!(
                "{P}latency_ms must be from {lo} to {hi}, or \"auto\""
            ));
        }
    };
    let channels = match table.get("channels") {
        None => [1, 2],
        Some(toml::Value::Array(a)) => match a.as_slice() {
            [toml::Value::Integer(l), toml::Value::Integer(r)]
                if (1..=8).contains(l) && (1..=8).contains(r) =>
            {
                [*l as u8, *r as u8]
            }
            _ => return Err(format!("{P}channels must be two channels from 1 to 8")),
        },
        Some(_) => return Err(format!("{P}channels must be two channels from 1 to 8")),
    };
    let volume_db = match table.get("volume_db") {
        None => 0.0,
        Some(v) => {
            let n = match v {
                toml::Value::Integer(v) => Some(*v as f64),
                toml::Value::Float(v) => Some(*v),
                _ => None,
            };
            match n {
                Some(n) if (-60.0..=0.0).contains(&n) => n as f32,
                _ => return Err(format!("{P}volume_db must be from -60 to 0")),
            }
        }
    };
    Ok(InputConfig {
        enabled: boolean(table, P, true)?,
        stream: string(table, "stream", P, "")?,
        latency,
        channels,
        volume_db,
    })
}

fn output_from(table: &toml::Table) -> Result<OutputConfig, String> {
    const P: &str = "output.";
    unknown(table, &OUTPUT_KEYS, P)?;
    let group = match table.get("address") {
        None => Group::Auto,
        Some(toml::Value::String(s)) if s == "auto" => Group::Auto,
        Some(toml::Value::String(s)) => match s.parse::<Ipv4Addr>() {
            Ok(a) if a.is_multicast() => Group::Addr(a),
            _ => {
                return Err(format!(
                    "{P}address must be \"auto\" or a multicast address"
                ));
            }
        },
        Some(_) => {
            return Err(format!(
                "{P}address must be \"auto\" or a multicast address"
            ));
        }
    };
    Ok(OutputConfig {
        enabled: boolean(table, P, false)?,
        name: string(table, "name", P, "aas")?,
        group,
        port: whole(table, "port", P, 1, 65535)?.unwrap_or(5004) as u16,
        ttl: whole(table, "ttl", P, 1, 255)?.unwrap_or(16) as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Option<Aes67Config>, String> {
        from_table(Some(text.parse().unwrap()))
    }

    fn refused(text: &str, key: &str) {
        let e = parse(text).unwrap_err();
        assert!(e.starts_with("aes67: "), "{e}");
        assert!(e.contains(key), "{key} not in {e}");
    }

    #[test]
    fn no_table_is_no_aes67() {
        assert_eq!(from_table(None), Ok(None));
    }

    #[test]
    fn an_empty_table_takes_the_defaults() {
        let c = parse("").unwrap().unwrap();
        assert_eq!(
            c,
            Aes67Config {
                interface: "eth0".into(),
                ptp: PtpSource::Listen,
                domain: 0,
                input: InputConfig {
                    enabled: true,
                    stream: String::new(),
                    latency: Latency::Auto,
                    channels: [1, 2],
                    volume_db: 0.0,
                },
                output: OutputConfig {
                    enabled: false,
                    name: "aas".into(),
                    group: Group::Auto,
                    port: 5004,
                    ttl: 16,
                },
            }
        );
    }

    #[test]
    fn every_key_is_read() {
        let c = parse(
            r#"
interface = "eth1"
ptp = "off"
domain = 4
[input]
enabled = false
stream = "Dante-Out"
latency_ms = 7
channels = [3, 4]
volume_db = -6.5
[output]
enabled = true
name = "studio"
address = "239.1.2.3"
port = 6000
ttl = 4
"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            c,
            Aes67Config {
                interface: "eth1".into(),
                ptp: PtpSource::Off,
                domain: 4,
                input: InputConfig {
                    enabled: false,
                    stream: "Dante-Out".into(),
                    latency: Latency::Ms(7),
                    channels: [3, 4],
                    volume_db: -6.5,
                },
                output: OutputConfig {
                    enabled: true,
                    name: "studio".into(),
                    group: Group::Addr(Ipv4Addr::new(239, 1, 2, 3)),
                    port: 6000,
                    ttl: 4,
                },
            }
        );
    }

    #[test]
    fn latency_auto_is_read() {
        let c = parse("[input]\nlatency_ms = \"auto\"\n").unwrap().unwrap();
        assert_eq!(c.input.latency, Latency::Auto);
    }

    #[test]
    fn bad_values_are_refused_naming_the_key() {
        assert_eq!(
            parse("ptp = \"ptp4l\"\n").unwrap_err(),
            "aes67: ptp must be \"listen\" or \"off\""
        );
        refused("[input]\nlatency_ms = 0\n", "latency_ms");
        refused("[input]\nlatency_ms = 101\n", "latency_ms");
        refused("[input]\nlatency_ms = \"fast\"\n", "latency_ms");
        refused("[input]\nchannels = [0, 1]\n", "channels");
        refused("[input]\nchannels = [1, 9]\n", "channels");
        refused("[input]\nchannels = [1]\n", "channels");
        refused("[input]\nvolume_db = 3\n", "volume_db");
        refused("[output]\nport = 0\n", "port");
        refused("[output]\nttl = 0\n", "ttl");
        refused("[output]\naddress = \"10.0.0.1\"\n", "address");
        refused("[output]\naddress = \"nonsense\"\n", "address");
        refused("domain = 256\n", "domain");
        refused("interface = \"\"\n", "interface");
        refused("input = 1\n", "input");
    }

    #[test]
    fn unknown_keys_are_refused_at_every_level() {
        refused("bogus = 1\n", "bogus");
        refused("[input]\nbogus = 1\n", "bogus");
        refused("[output]\nbogus = 1\n", "bogus");
    }

    #[test]
    fn auto_latency_by_board() {
        assert_eq!(
            latency_ms(
                Latency::Auto,
                Some("Raspberry Pi Compute Module 5 Lite Rev 1.0")
            ),
            3
        );
        assert_eq!(latency_ms(Latency::Auto, Some("Raspberry Pi 5 Model B")), 3);
        assert_eq!(
            latency_ms(Latency::Auto, Some("Raspberry Pi 4 Model B Rev 1.1")),
            20
        );
        assert_eq!(latency_ms(Latency::Auto, None), 20);
        assert_eq!(latency_ms(Latency::Ms(7), None), 7);
    }
}
