//! The command line of `aasctl`, parsed by hand as `aas` parses its own.

use aas::config::DEFAULT_API_PORT;
use serde_json::{Map, Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const USAGE: &str = "usage: aasctl [--port N | --config PATH] COMMAND
  state | metrics | outputs | inputs | matrix | plugins | settings
  graph [--dot]
  output N volume V | volume-db X | idle off|zeros [DELAY]
  analog volume-db X | mute | unmute
  client NAME volume-db X | mute | unmute
  plugin ID set NAME=VALUE... | enable | disable
  settings KEY=VALUE...
  matrix put FILE
  airplay pair NAME [PIN] | airplay NAME enable|disable
  events | levels";

#[derive(Debug, PartialEq)]
pub enum Command {
    /// `GET /name`: state, metrics, outputs, inputs, matrix, plugins, settings.
    Get(&'static str),
    Graph {
        dot: bool,
    },
    Patch {
        path: String,
        body: Value,
    },
    /// `airplay pair NAME [PIN]`: a call that may take a while.
    Post {
        path: String,
        body: Value,
    },
    /// `matrix put FILE`: the file is read when the command runs.
    Put {
        path: String,
        file: String,
    },
    Events,
    Levels,
}

/// Why the command line gave no command.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// Not a command line `aasctl` understands: exit 2 with the usage.
    Usage(String),
    /// A well-formed one that cannot be carried out, such as a config file
    /// that does not load: exit 1.
    Failed(String),
}

#[derive(Debug, PartialEq)]
pub struct Args {
    pub port: u16,
    pub command: Command,
}

const GETS: [&str; 7] = [
    "state", "metrics", "outputs", "inputs", "matrix", "plugins", "settings",
];

/// Reads the command line, without the program's name. `config_port` gives
/// the `api_port` of a config file.
pub fn parse(
    given: impl IntoIterator<Item = OsString>,
    config_port: impl Fn(&Path) -> Result<u16, String>,
) -> Result<Args, Error> {
    let mut port = None;
    let mut config: Option<PathBuf> = None;
    let mut words = Vec::new();
    let mut given = given.into_iter();
    while let Some(arg) = given.next() {
        match arg.to_str() {
            Some("--port") => {
                let value = given.next().ok_or_else(|| usage("--port needs a number"))?;
                let value = value.to_string_lossy();
                port = Some(
                    value
                        .parse::<u16>()
                        .map_err(|_| usage(format!("--port needs a port number, not {value}")))?,
                );
            }
            Some("--config") => {
                config = Some(
                    given
                        .next()
                        .ok_or_else(|| usage("--config needs a path"))?
                        .into(),
                );
            }
            _ => words.push(
                arg.into_string()
                    .map_err(|a| usage(format!("not text: {}", a.to_string_lossy())))?,
            ),
        }
    }
    let port = match (port, config) {
        (Some(port), _) => port,
        (None, Some(path)) => config_port(&path).map_err(Error::Failed)?,
        (None, None) => DEFAULT_API_PORT,
    };
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    Ok(Args {
        port,
        command: command(&words).map_err(Error::Usage)?,
    })
}

fn usage(e: impl Into<String>) -> Error {
    Error::Usage(e.into())
}

fn command(words: &[&str]) -> Result<Command, String> {
    match words {
        [] => Err("no command".into()),
        [name] if GETS.contains(name) => {
            Ok(Command::Get(GETS.iter().find(|g| *g == name).unwrap()))
        }
        ["graph"] => Ok(Command::Graph { dot: false }),
        ["graph", "--dot"] => Ok(Command::Graph { dot: true }),
        ["events"] => Ok(Command::Events),
        ["levels"] => Ok(Command::Levels),
        ["output", n, rest @ ..] => output(n, rest),
        ["analog", rest @ ..] => input("/inputs/analog".into(), rest),
        ["client", name, rest @ ..] => input(format!("/inputs/clients/{}", encode(name)), rest),
        ["plugin", id, rest @ ..] => plugin(id, rest),
        ["settings", pairs @ ..] if !pairs.is_empty() => Ok(Command::Patch {
            path: "/settings".into(),
            body: Value::Object(pairs_of(pairs, setting)?),
        }),
        ["airplay", "pair", name] => Ok(Command::Post {
            path: format!("/airplay/{}/pair", encode(name)),
            body: json!({}),
        }),
        ["airplay", "pair", name, pin] => Ok(Command::Post {
            path: format!("/airplay/{}/pair", encode(name)),
            body: json!({"pin": pin}),
        }),
        ["airplay", name, on @ ("enable" | "disable")] => Ok(Command::Patch {
            path: format!("/airplay/{}", encode(name)),
            body: json!({"enabled": *on == "enable"}),
        }),
        ["matrix", "put", file] => Ok(Command::Put {
            path: "/matrix".into(),
            file: (*file).into(),
        }),
        [other, ..] => Err(format!("unknown or incomplete command {other}")),
    }
}

fn output(n: &str, rest: &[&str]) -> Result<Command, String> {
    n.parse::<u32>()
        .map_err(|_| format!("{n} is not an output number"))?;
    let body = match rest {
        ["volume", v] => json!({"volume": number(v)?}),
        ["volume-db", x] => json!({"volume_db": number(x)?}),
        ["idle", mode @ ("off" | "zeros")] => json!({"idle": mode}),
        ["idle", mode @ ("off" | "zeros"), delay] => {
            json!({"idle": mode, "idle_delay_s": number(delay)?})
        }
        _ => return Err("output N volume V | volume-db X | idle off|zeros [DELAY]".into()),
    };
    Ok(Command::Patch {
        path: format!("/outputs/{n}"),
        body,
    })
}

/// The analog input or a client.
fn input(path: String, rest: &[&str]) -> Result<Command, String> {
    let body = match rest {
        ["volume-db", x] => json!({"volume_db": number(x)?}),
        ["mute"] => json!({"muted": true}),
        ["unmute"] => json!({"muted": false}),
        _ => return Err("expected volume-db X, mute or unmute".into()),
    };
    Ok(Command::Patch { path, body })
}

fn plugin(id: &str, rest: &[&str]) -> Result<Command, String> {
    let parts: Vec<&str> = id.split('/').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return Err(format!("{id} is not a plugin id such as 1/mix/0"));
    }
    let body = match rest {
        ["enable"] => json!({"enabled": true}),
        ["disable"] => json!({"enabled": false}),
        ["set", pairs @ ..] if !pairs.is_empty() => {
            let params = pairs_of(pairs, |_, v| Ok(json!(float(v)?)))?;
            json!({"params": params})
        }
        _ => return Err("plugin ID set NAME=VALUE... | enable | disable".into()),
    };
    let path = format!(
        "/plugins/{}",
        parts
            .iter()
            .map(|p| encode(p))
            .collect::<Vec<_>>()
            .join("/")
    );
    Ok(Command::Patch { path, body })
}

fn pairs_of(
    pairs: &[&str],
    value: impl Fn(&str, &str) -> Result<Value, String>,
) -> Result<Map<String, Value>, String> {
    let mut map = Map::new();
    for pair in pairs {
        let (name, v) = pair
            .split_once('=')
            .filter(|(name, _)| !name.is_empty())
            .ok_or_else(|| format!("{pair} is not NAME=VALUE"))?;
        map.insert(name.to_string(), value(name, v)?);
    }
    Ok(map)
}

/// A settings value: a number if it is one, else text such as `forced`.
fn setting(_: &str, v: &str) -> Result<Value, String> {
    Ok(number(v).unwrap_or_else(|_| json!(v)))
}

/// A whole number stays whole.
fn number(v: &str) -> Result<Value, String> {
    if let Ok(i) = v.parse::<i64>() {
        return Ok(json!(i));
    }
    Ok(json!(float(v)?))
}

fn float(v: &str) -> Result<f64, String> {
    v.parse::<f64>()
        .ok()
        .filter(|f| f.is_finite())
        .ok_or_else(|| format!("{v} is not a number"))
}

/// Percent-encodes everything but letters, digits and `-_.~`.
fn encode(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(line: &str) -> Result<Args, Error> {
        parse(line.split_whitespace().map(OsString::from), |_| Ok(9999))
    }

    fn patch(path: &str, body: Value) -> Command {
        Command::Patch {
            path: path.into(),
            body,
        }
    }

    fn words(words: &[&str]) -> Command {
        parse(words.iter().map(OsString::from), |_| Ok(9999))
            .unwrap()
            .command
    }

    #[test]
    fn airplay_enable() {
        assert_eq!(
            words(&["airplay", "Apple TV", "enable"]),
            patch("/airplay/Apple%20TV", json!({"enabled": true}))
        );
    }

    #[test]
    fn airplay_disable() {
        assert_eq!(
            words(&["airplay", "Sound bar", "disable"]),
            patch("/airplay/Sound%20bar", json!({"enabled": false}))
        );
    }

    #[test]
    fn airplay_pair_starts() {
        assert_eq!(
            words(&["airplay", "pair", "Apple TV"]),
            Command::Post {
                path: "/airplay/Apple%20TV/pair".into(),
                body: json!({}),
            }
        );
    }

    #[test]
    fn airplay_pair_finishes() {
        assert_eq!(
            words(&["airplay", "pair", "Apple TV", "1234"]),
            Command::Post {
                path: "/airplay/Apple%20TV/pair".into(),
                body: json!({"pin": "1234"}),
            }
        );
    }

    #[test]
    fn output_volume() {
        let args = parsed("output 1 volume 60").unwrap();
        assert_eq!(args.port, 2718);
        assert_eq!(args.command, patch("/outputs/1", json!({"volume": 60})));
        assert_eq!(
            parsed("output 2 idle zeros").unwrap().command,
            patch("/outputs/2", json!({"idle": "zeros"}))
        );
        assert_eq!(
            parsed("output 2 idle off 30").unwrap().command,
            patch("/outputs/2", json!({"idle": "off", "idle_delay_s": 30}))
        );
        assert_eq!(
            parsed("output 2 volume-db -6.5").unwrap().command,
            patch("/outputs/2", json!({"volume_db": -6.5}))
        );
    }

    #[test]
    fn client_mute() {
        assert_eq!(
            parsed("client mpd mute").unwrap().command,
            patch("/inputs/clients/mpd", json!({"muted": true}))
        );
        assert_eq!(
            parsed("analog unmute").unwrap().command,
            patch("/inputs/analog", json!({"muted": false}))
        );
        let args = parse(
            ["client", "my player", "volume-db", "-3"].map(OsString::from),
            |_| Ok(1),
        )
        .unwrap();
        assert_eq!(
            args.command,
            patch("/inputs/clients/my%20player", json!({"volume_db": -3}))
        );
    }

    #[test]
    fn plugin_set() {
        assert_eq!(
            parsed("plugin 1/mix/0 set gain_db=-3 bypass=1")
                .unwrap()
                .command,
            patch(
                "/plugins/1/mix/0",
                json!({"params": {"gain_db": -3.0, "bypass": 1.0}})
            )
        );
        assert_eq!(
            parsed("plugin 1/mix/0 disable").unwrap().command,
            patch("/plugins/1/mix/0", json!({"enabled": false}))
        );
    }

    #[test]
    fn settings_values() {
        assert_eq!(
            parsed("settings latency_ms=5 bit_perfect=off")
                .unwrap()
                .command,
            patch("/settings", json!({"latency_ms": 5, "bit_perfect": "off"}))
        );
        assert_eq!(
            parsed("settings").unwrap().command,
            Command::Get("settings")
        );
    }

    #[test]
    fn port_from_config() {
        let args = parsed("--config /etc/aas.toml state").unwrap();
        assert_eq!(args.port, 9999);
        assert_eq!(args.command, Command::Get("state"));
        assert_eq!(
            parse(["--config", "x", "state"].map(OsString::from), |_| {
                Err("broken".into())
            }),
            Err(Error::Failed("broken".into()))
        );
    }

    #[test]
    fn matrix_put_names_its_file_and_reads_nothing() {
        assert_eq!(
            parsed("matrix put /nonexistent.json").unwrap().command,
            Command::Put {
                path: "/matrix".into(),
                file: "/nonexistent.json".into()
            }
        );
    }

    #[test]
    fn port_flag_wins() {
        assert_eq!(parsed("--port 80 --config c state").unwrap().port, 80);
        assert_eq!(parsed("state --port 81").unwrap().port, 81);
        assert!(parsed("--port nope state").is_err());
    }

    #[test]
    fn unknown_command_is_a_usage_error() {
        for line in [
            "",
            "frobnicate",
            "output",
            "output x volume 1",
            "client mpd",
            "graph --svg",
        ] {
            assert!(matches!(parsed(line), Err(Error::Usage(_))), "{line:?}");
        }
    }

    #[test]
    fn a_value_that_is_not_a_number() {
        assert!(parsed("output 1 volume loud").is_err());
        assert!(parsed("plugin 1/mix/0 set gain_db=x").is_err());
        assert!(parsed("plugin 1/mix/0 set gain_db").is_err());
        assert!(parsed("output 1 volume NaN").is_err());
    }
}
