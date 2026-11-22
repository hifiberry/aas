//! The `[[airplay]]` tables of the config file.

/// The most `[[airplay]]` tables.
pub const MAX_RECEIVERS: usize = 4;

/// Which protocol a receiver is spoken to in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    AirPlay1,
    AirPlay2,
}

/// When the output connects to its receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connect {
    /// When the mix has a signal; it disconnects after `idle_s` of silence.
    Signal,
    /// From the start, and again after any failure.
    Always,
}

/// One `[[airplay]]` table: a receiver to send the master's mix to.
#[derive(Debug, Clone, PartialEq)]
pub struct AirplayConfig {
    /// Shown in the API and the log; the host by default.
    pub name: String,
    /// A name or an address.
    pub host: String,
    pub port: u16,
    pub protocol: Protocol,
    pub connect: Connect,
    /// A 50 ms window above this level is a signal, in dBFS.
    pub threshold_dbfs: f32,
    /// Seconds of silence before a disconnect, under `Connect::Signal`.
    pub idle_s: u32,
    /// The receiver's buffer, in ms.
    pub latency_ms: u32,
    /// The receiver's volume, sent once a session, in dB; None sends none.
    pub volume_db: Option<f32>,
}

/// The receivers of the `[[airplay]]` tables, checked.
pub fn receivers_from(tables: Vec<toml::Table>) -> Result<Vec<AirplayConfig>, String> {
    if tables.len() > MAX_RECEIVERS {
        return Err(format!("at most {MAX_RECEIVERS} [[airplay]] tables"));
    }
    let mut receivers: Vec<AirplayConfig> = Vec::new();
    for (i, table) in tables.into_iter().enumerate() {
        let n = i + 1;
        let r = receiver_from(table).map_err(|e| format!("airplay {n}: {e}"))?;
        if receivers.iter().any(|o| o.name == r.name) {
            return Err(format!("airplay {n}: name \"{}\" is already used", r.name));
        }
        receivers.push(r);
    }
    Ok(receivers)
}

const KEYS: [&str; 9] = [
    "host",
    "port",
    "name",
    "protocol",
    "connect",
    "threshold_dbfs",
    "idle_s",
    "latency_ms",
    "volume_db",
];

/// A whole number in `range`, or `None` for a key that is not there.
fn whole(table: &toml::Table, key: &str, lo: i64, hi: i64) -> Result<Option<i64>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Integer(v)) if (lo..=hi).contains(v) => Ok(Some(*v)),
        Some(_) => Err(format!("{key} must be from {lo} to {hi}")),
    }
}

/// A number, whole or not, in `lo..=hi`.
fn number(value: &toml::Value, lo: f64, hi: f64) -> Option<f32> {
    let v = match value {
        toml::Value::Integer(v) => *v as f64,
        toml::Value::Float(v) => *v,
        _ => return None,
    };
    (lo..=hi).contains(&v).then_some(v as f32)
}

/// One of the strings in `names`, as its index; the first when absent.
fn choice(table: &toml::Table, key: &str, names: [&str; 2]) -> Result<usize, String> {
    match table.get(key) {
        None => Ok(0),
        Some(toml::Value::String(s)) if names.contains(&s.as_str()) => {
            Ok(names.iter().position(|n| n == s).unwrap_or(0))
        }
        Some(_) => Err(format!(
            "{key} must be \"{}\" or \"{}\"",
            names[0], names[1]
        )),
    }
}

fn receiver_from(table: toml::Table) -> Result<AirplayConfig, String> {
    if let Some(key) = table.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(format!("unknown key {key}"));
    }
    let host = match table.get("host") {
        Some(toml::Value::String(h)) if !h.is_empty() => h.clone(),
        _ => return Err("host is needed".into()),
    };
    let name = match table.get("name") {
        None => host.clone(),
        Some(toml::Value::String(n)) => n.clone(),
        Some(_) => return Err("name must be a string".into()),
    };
    let port = whole(&table, "port", 1, 65535)?.unwrap_or(7000) as u16;
    let protocol = [Protocol::AirPlay1, Protocol::AirPlay2]
        [choice(&table, "protocol", ["airplay1", "airplay2"])?];
    let connect =
        [Connect::Signal, Connect::Always][choice(&table, "connect", ["signal", "always"])?];
    let threshold_dbfs = match table.get("threshold_dbfs") {
        None => -50.0,
        Some(v) => number(v, -90.0, 0.0).ok_or("threshold_dbfs must be from -90 to 0")?,
    };
    let idle_s = whole(&table, "idle_s", 1, 86400)?.unwrap_or(120) as u32;
    let latency_ms = whole(&table, "latency_ms", 50, 5000)?.unwrap_or(1500) as u32;
    let volume_db = match table.get("volume_db") {
        None => Some(0.0),
        Some(toml::Value::String(s)) if s == "off" => None,
        Some(v) => {
            Some(number(v, -30.0, 0.0).ok_or("volume_db must be from -30 to 0, or \"off\"")?)
        }
    };
    Ok(AirplayConfig {
        name,
        host,
        port,
        protocol,
        connect,
        threshold_dbfs,
        idle_s,
        latency_ms,
        volume_db,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Vec<AirplayConfig>, String> {
        let doc: toml::Table = text.parse().unwrap();
        let tables = match doc.get("airplay") {
            Some(toml::Value::Array(a)) => {
                a.iter().map(|v| v.as_table().unwrap().clone()).collect()
            }
            _ => Vec::new(),
        };
        receivers_from(tables)
    }

    fn one(body: &str) -> Result<AirplayConfig, String> {
        parse(&format!("[[airplay]]\n{body}")).map(|mut v| v.remove(0))
    }

    #[test]
    fn a_host_alone_takes_every_default() {
        let r = one("host = \"soundbar.local\"\n").unwrap();
        assert_eq!(
            r,
            AirplayConfig {
                name: "soundbar.local".into(),
                host: "soundbar.local".into(),
                port: 7000,
                protocol: Protocol::AirPlay1,
                connect: Connect::Signal,
                threshold_dbfs: -50.0,
                idle_s: 120,
                latency_ms: 1500,
                volume_db: Some(0.0),
            }
        );
    }

    #[test]
    fn every_key_is_read() {
        let r = one(
            "host = \"192.0.2.7\"\nport = 5000\nname = \"Study\"\nprotocol = \"airplay2\"\n\
             connect = \"always\"\nthreshold_dbfs = -62.5\nidle_s = 30\nlatency_ms = 2000\n\
             volume_db = -6\n",
        )
        .unwrap();
        assert_eq!(r.name, "Study");
        assert_eq!(r.host, "192.0.2.7");
        assert_eq!(r.port, 5000);
        assert_eq!(r.protocol, Protocol::AirPlay2);
        assert_eq!(r.connect, Connect::Always);
        assert_eq!(r.threshold_dbfs, -62.5);
        assert_eq!(r.idle_s, 30);
        assert_eq!(r.latency_ms, 2000);
        assert_eq!(
            one("host = \"h\"\nlatency_ms = 100\n").unwrap().latency_ms,
            100
        );
        assert_eq!(r.volume_db, Some(-6.0));
        assert_eq!(
            one("host = \"h\"\nvolume_db = \"off\"\n")
                .unwrap()
                .volume_db,
            None
        );
    }

    #[test]
    fn each_key_is_checked() {
        let cases = [
            ("", "airplay 1: host is needed"),
            ("host = \"\"\n", "airplay 1: host is needed"),
            (
                "host = \"h\"\nport = 0\n",
                "airplay 1: port must be from 1 to 65535",
            ),
            (
                "host = \"h\"\nport = 65536\n",
                "airplay 1: port must be from 1 to 65535",
            ),
            (
                "host = \"h\"\nname = 3\n",
                "airplay 1: name must be a string",
            ),
            (
                "host = \"h\"\nprotocol = \"raop\"\n",
                "airplay 1: protocol must be \"airplay1\" or \"airplay2\"",
            ),
            (
                "host = \"h\"\nconnect = \"never\"\n",
                "airplay 1: connect must be \"signal\" or \"always\"",
            ),
            (
                "host = \"h\"\nthreshold_dbfs = -91\n",
                "airplay 1: threshold_dbfs must be from -90 to 0",
            ),
            (
                "host = \"h\"\nthreshold_dbfs = 1\n",
                "airplay 1: threshold_dbfs must be from -90 to 0",
            ),
            (
                "host = \"h\"\nidle_s = 0\n",
                "airplay 1: idle_s must be from 1 to 86400",
            ),
            (
                "host = \"h\"\nidle_s = 1.5\n",
                "airplay 1: idle_s must be from 1 to 86400",
            ),
            (
                "host = \"h\"\nlatency_ms = 49\n",
                "airplay 1: latency_ms must be from 50 to 5000",
            ),
            (
                "host = \"h\"\nlatency_ms = 5001\n",
                "airplay 1: latency_ms must be from 50 to 5000",
            ),
            (
                "host = \"h\"\nvolume_db = -31\n",
                "airplay 1: volume_db must be from -30 to 0, or \"off\"",
            ),
            (
                "host = \"h\"\nvolume_db = \"loud\"\n",
                "airplay 1: volume_db must be from -30 to 0, or \"off\"",
            ),
            ("host = \"h\"\nhots = 1\n", "airplay 1: unknown key hots"),
        ];
        for (body, want) in cases {
            assert_eq!(one(body).unwrap_err(), want, "{body}");
        }
    }

    #[test]
    fn at_most_four_tables_with_distinct_names() {
        let table = |name: &str| format!("[[airplay]]\nhost = \"{name}\"\n");
        let four: String = ["a", "b", "c", "d"].map(table).concat();
        assert_eq!(parse(&four).unwrap().len(), 4);
        let five = four + &table("e");
        assert_eq!(parse(&five).unwrap_err(), "at most 4 [[airplay]] tables");
        let twice = table("a") + "[[airplay]]\nhost = \"b\"\nname = \"a\"\n";
        assert_eq!(
            parse(&twice).unwrap_err(),
            "airplay 2: name \"a\" is already used"
        );
    }

    #[test]
    fn none_is_none() {
        assert_eq!(parse("").unwrap(), Vec::new());
    }
}
