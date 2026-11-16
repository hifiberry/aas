//! The daemon and HTTP harness the API and `aasctl` tests share.
#![allow(dead_code)]

use aas::config::{Config, Idle, PluginConfig};
use aas::server;
use aas::sink::{MemorySink, Sink};
use aas_client::client::Client;
use aas_client::convert::ClientFormat;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Gives a period of 64 frames at 48 kHz.
pub const LATENCY_MS: f32 = 4.0;
/// Long enough for anything here on a loaded machine.
pub const PATIENCE: Duration = Duration::from_secs(20);

pub struct Daemon {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
    /// The API's port; None with the API off.
    pub port: Option<u16>,
    pub shutdown: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<Result<(), String>>>,
}

impl Daemon {
    /// A daemon playing to a paced memory sink, with the API on a port of
    /// its own.
    pub fn start() -> Daemon {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        Daemon::start_with(Some(listener), Vec::new())
    }

    /// The same, with these plugins on the master.
    pub fn with_plugins(plugins: Vec<PluginConfig>) -> Daemon {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        Daemon::start_with(Some(listener), plugins)
    }

    /// The PulseAudio socket the daemon serves beside its own.
    pub fn pulse_socket(&self) -> PathBuf {
        self.path.parent().unwrap().join("pulse")
    }

    pub fn start_with(api: Option<TcpListener>, plugins: Vec<PluginConfig>) -> Daemon {
        Daemon::start_idle(api, plugins, Idle::Off { delay_s: 60 })
    }

    /// The same, with the master's idle rule `idle`.
    pub fn start_idle(api: Option<TcpListener>, plugins: Vec<PluginConfig>, idle: Idle) -> Daemon {
        let config = Config {
            latency_ms: Some(LATENCY_MS),
            idle,
            plugins,
            ..Config::default()
        };
        Daemon::start_config(api, config)
    }

    /// The same, with the whole `config`.
    pub fn start_config(api: Option<TcpListener>, config: Config) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aas.sock");
        let state = dir.path().join("state.toml");
        let port = api.as_ref().map(|l| l.local_addr().unwrap().port());
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let (path, shutdown) = (path.clone(), shutdown.clone());
            thread::spawn(move || {
                let devices = server::Devices {
                    sink: Box::new(MemorySink::paced()) as Box<dyn Sink>,
                    probe: None,
                    further: Vec::new(),
                    api,
                };
                server::run_outputs(config, devices, &path, &state, None, None, shutdown)
            })
        };
        let daemon = Daemon {
            _dir: dir,
            path,
            port,
            shutdown,
            thread: Some(thread),
        };
        assert!(
            wait_for(|| daemon.path.exists()),
            "the socket never appeared"
        );
        assert!(
            wait_for(|| daemon.pulse_socket().exists()),
            "the PulseAudio socket never appeared"
        );
        daemon
    }

    /// A client playing S32 stereo at 48 kHz, its ring full, started.
    pub fn playing(&self, buffer_frames: u32) -> Client {
        let mut client = Client::connect(&self.path).unwrap();
        client
            .configure(48000, ClientFormat::S32, 2, buffer_frames)
            .unwrap();
        top_up(&mut client);
        client.start().unwrap();
        client
    }

    /// Sends one request and gives the status and the body as JSON.
    pub fn http(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let port = self.port.expect("the API is off");
        http(port, method, path, body)
    }

    pub fn get(&self, path: &str) -> (u16, Value) {
        self.http("GET", path, None)
    }

    pub fn patch(&self, path: &str, body: &str) -> (u16, Value) {
        self.http("PATCH", path, Some(body))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let result = self.thread.take().unwrap().join();
        if !thread::panicking() {
            assert_eq!(result.unwrap(), Ok(()));
        }
    }
}

pub fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    condition()
}

/// Fills the client's ring with quiet, non-zero frames as far as it takes.
pub fn top_up(client: &mut Client) {
    let frame = [1i32.to_le_bytes(), 1i32.to_le_bytes()].concat();
    let data = frame.repeat(4096);
    while client.write(&data).unwrap() > 0 {}
}

/// One HTTP/1.1 request on its own connection. The body of every answer
/// of the API is JSON.
pub fn http(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let (status, _, body) = http_raw(port, method, path, body);
    let body = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body:?}"));
    (status, body)
}

/// The same, with the content type and the body as text.
pub fn http_raw(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").expect("no end of the head");
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let content_type = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-type:")
                .map(|v| v.trim().to_string())
        })
        .unwrap_or_default();
    (status, content_type, body.to_string())
}

/// The name the daemon knows this process by: its `comm`.
pub fn own_name() -> String {
    let comm = std::fs::read_to_string("/proc/self/comm").unwrap();
    comm.trim_end().to_string()
}

/// Percent-encodes everything but letters and digits.
pub fn encode(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The name `/state` gives this process's client, once it runs.
pub fn running_client(daemon: &Daemon) -> String {
    let mut found = None;
    let seen = wait_for(|| {
        let (_, state) = daemon.get("/state");
        let clients = state["clients"].as_array().cloned().unwrap_or_default();
        found = clients
            .into_iter()
            .find(|c| c["name"] == json!(own_name()))
            .filter(|c| c["running"] == json!(true));
        found.is_some()
    });
    assert!(seen, "the client never ran in /state");
    found.unwrap()["name"].as_str().unwrap().to_string()
}

/// A WebSocket on `/events`, subscribed once this returns.
pub fn subscribe(daemon: &Daemon) -> tungstenite::WebSocket<TcpStream> {
    websocket(daemon, "/events")
}

pub fn websocket(daemon: &Daemon, path: &str) -> tungstenite::WebSocket<TcpStream> {
    let port = daemon.port.unwrap();
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    let url = format!("ws://127.0.0.1:{port}{path}");
    let (socket, _) = tungstenite::client(url, stream).unwrap();
    socket
}
