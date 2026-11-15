//! `aasctl`, the command line tool on top of the daemon's HTTP API.

mod args;
mod client;

use args::{Command, USAGE};
use serde_json::Value;
use std::io::Write;
use std::net::TcpStream;
use std::process::ExitCode;
use tungstenite::Message;

fn main() -> ExitCode {
    let parsed = args::parse(std::env::args_os().skip(1), |path| {
        let config = aas::config::Config::load(path)?;
        match config.api_port {
            0 => Err(format!("{}: api_port is 0, the API is off", path.display())),
            port => Ok(port),
        }
    });
    let args = match parsed {
        Ok(args) => args,
        Err(args::Error::Usage(e)) => {
            eprintln!("aasctl: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
        Err(args::Error::Failed(e)) => {
            eprintln!("aasctl: {e}");
            return ExitCode::from(1);
        }
    };
    match run(args.port, args.command) {
        Ok(()) | Err(Stop::Pipe) => ExitCode::SUCCESS,
        Err(Stop::Failed(e)) => {
            eprintln!("aasctl: {e}");
            ExitCode::from(1)
        }
    }
}

/// Why a command ended early.
enum Stop {
    /// The reader of stdout went away (`aasctl events | head`): not an error.
    Pipe,
    Failed(String),
}

impl From<String> for Stop {
    fn from(e: String) -> Self {
        Stop::Failed(e)
    }
}

/// Writes one line to stdout; a closed pipe ends the command quietly.
fn say(text: &str) -> Result<(), Stop> {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Err(Stop::Pipe),
        Err(e) => Err(Stop::Failed(format!("cannot write: {e}"))),
    }
}

fn run(port: u16, command: Command) -> Result<(), Stop> {
    match command {
        Command::Get(name) => call(port, "GET", &format!("/{name}"), None),
        Command::Graph { dot } => {
            let path = if dot { "/graph?format=dot" } else { "/graph" };
            call(port, "GET", path, None)
        }
        Command::Patch { path, body } => call(port, "PATCH", &path, Some(&body.to_string())),
        Command::Put { path, file } => {
            let text = std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?;
            let body: Value = serde_json::from_str(&text).map_err(|e| format!("{file}: {e}"))?;
            call(port, "PUT", &path, Some(&body.to_string()))
        }
        Command::Events => stream(port, "/events", |message| match message {
            Message::Text(text) => say(&format!("{text}\n")),
            _ => Ok(()),
        }),
        Command::Levels => stream(port, "/levels", |message| match message {
            Message::Binary(frame) => say(&format!("{}\n", levels_line(&frame))),
            _ => Ok(()),
        }),
    }
}

/// One request; the answer on stdout, or its error on stderr.
fn call(port: u16, method: &str, path: &str, body: Option<&str>) -> Result<(), Stop> {
    let (status, text) = client::request(port, method, path, body)
        .map_err(|e| format!("cannot reach the daemon on port {port}: {e}"))?;
    let json = serde_json::from_str::<Value>(&text).ok();
    if status >= 400 {
        let message = json
            .as_ref()
            .and_then(|j| j["error"].as_str())
            .unwrap_or(text.trim());
        return Err(format!("{status}: {message}").into());
    }
    match json {
        Some(json) => say(&format!(
            "{}\n",
            serde_json::to_string_pretty(&json).unwrap()
        )),
        None => say(&text),
    }
}

/// Calls `each` with every message of a WebSocket until it closes.
fn stream(
    port: u16,
    path: &str,
    mut each: impl FnMut(Message) -> Result<(), Stop>,
) -> Result<(), Stop> {
    let unreachable =
        |e: &dyn std::fmt::Display| format!("cannot reach the daemon on port {port}: {e}");
    let tcp = TcpStream::connect(("127.0.0.1", port)).map_err(|e| unreachable(&e))?;
    let (mut socket, _) = tungstenite::client(format!("ws://127.0.0.1:{port}{path}"), tcp)
        .map_err(|e| unreachable(&e))?;
    loop {
        match socket.read() {
            Ok(message) => each(message)?,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(());
            }
            Err(e) => return Err(Stop::Failed(e.to_string())),
        }
    }
}

/// A `/levels` frame as text: per meter `id:` then each channel's RMS and
/// peak in dBFS, or `CLIP`.
fn levels_line(frame: &[u8]) -> String {
    let mut parts = Vec::new();
    let mut at = 0;
    while at + 2 <= frame.len() {
        let (id, n) = (frame[at], frame[at + 1] as usize);
        let end = at + 2 + 2 * n + 1;
        if end > frame.len() {
            break;
        }
        let clip = frame[end - 1];
        let mut text = format!("{id}:");
        for c in 0..n {
            let (rms, peak) = (frame[at + 2 + 2 * c], frame[at + 3 + 2 * c]);
            let clipped = clip >> c & 1 == 1;
            text += &format!(
                " {}/{}{}",
                db(rms),
                db(peak),
                if clipped { " CLIP" } else { "" }
            );
        }
        parts.push(text);
        at = end;
    }
    parts.join(" | ")
}

/// A level byte in dBFS with one decimal; `-inf` at -60 dBFS or below.
fn db(byte: u8) -> String {
    if byte == 0 {
        "-inf".into()
    } else {
        format!("{:.1}", f32::from(byte) / 255.0 * 60.0 - 60.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_levels_frame_reads_as_dbfs() {
        let frame = [1, 2, 0, 0, 242, 255, 0b10, 16, 1, 128, 128, 0];
        assert_eq!(
            levels_line(&frame),
            "1: -inf/-inf -3.1/0.0 CLIP | 16: -29.9/-29.9"
        );
    }
}
