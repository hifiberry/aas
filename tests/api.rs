//! The HTTP API end to end: the daemon with a `MemorySink`, clients on a
//! real socket, and HTTP on a listener of its own.

use aas::config::{Config, Idle};
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
use tokio_tungstenite::tungstenite::{self, Message};

/// Gives a period of 64 frames at 48 kHz.
const LATENCY_MS: f32 = 4.0;
/// Long enough for anything here on a loaded machine.
const PATIENCE: Duration = Duration::from_secs(20);

struct Daemon {
    _dir: tempfile::TempDir,
    path: PathBuf,
    /// The API's port; None with the API off.
    port: Option<u16>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl Daemon {
    /// A daemon playing to a paced memory sink, with the API on a port of
    /// its own.
    fn start() -> Daemon {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        Daemon::start_with(Some(listener))
    }

    fn start_with(api: Option<TcpListener>) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aas.sock");
        let state = dir.path().join("state.toml");
        let port = api.as_ref().map(|l| l.local_addr().unwrap().port());
        let shutdown = Arc::new(AtomicBool::new(false));
        let config = Config {
            latency_ms: Some(LATENCY_MS),
            idle: Idle::Off { delay_s: 60 },
            ..Config::default()
        };
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
        daemon
    }

    /// A client playing S32 stereo at 48 kHz, its ring full, started.
    fn playing(&self, buffer_frames: u32) -> Client {
        let mut client = Client::connect(&self.path).unwrap();
        client
            .configure(48000, ClientFormat::S32, 2, buffer_frames)
            .unwrap();
        top_up(&mut client);
        client.start().unwrap();
        client
    }

    /// Sends one request and gives the status and the body as JSON.
    fn http(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let port = self.port.expect("the API is off");
        http(port, method, path, body)
    }

    fn get(&self, path: &str) -> (u16, Value) {
        self.http("GET", path, None)
    }

    fn patch(&self, path: &str, body: &str) -> (u16, Value) {
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

fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
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
fn top_up(client: &mut Client) {
    let frame = [1i32.to_le_bytes(), 1i32.to_le_bytes()].concat();
    let data = frame.repeat(4096);
    while client.write(&data).unwrap() > 0 {}
}

/// One HTTP/1.1 request on its own connection. The body of every answer
/// of the API is JSON.
fn http(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
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
    let body = serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body:?}"));
    (status, body)
}

/// The name the daemon knows this process by: its `comm`.
fn own_name() -> String {
    let comm = std::fs::read_to_string("/proc/self/comm").unwrap();
    comm.trim_end().to_string()
}

/// Percent-encodes everything but letters and digits.
fn encode(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The name `/state` gives this process's client, once it runs.
fn running_client(daemon: &Daemon) -> String {
    let mut found = None;
    let seen = wait_for(|| {
        let (_, state) = daemon.get("/state");
        let clients = state["clients"].as_array().cloned().unwrap_or_default();
        found = clients
            .into_iter()
            .find(|c| c["name"].as_str().unwrap().starts_with(&own_name()))
            .filter(|c| c["running"] == json!(true));
        found.is_some()
    });
    assert!(seen, "the client never ran in /state");
    found.unwrap()["name"].as_str().unwrap().to_string()
}

#[test]
fn get_state_lists_a_playing_client() {
    let daemon = Daemon::start();
    let mut client = daemon.playing(16384);
    running_client(&daemon);
    // The client plays alone at the card's rate, untouched.
    assert!(wait_for(|| {
        top_up(&mut client);
        daemon.get("/state").1["bit_perfect"] == json!(true)
    }));
    let (status, state) = daemon.get("/state");
    assert_eq!(status, 200);
    assert_eq!(state["rate"], json!(48000));
    let clients = state["clients"].as_array().unwrap();
    let ours = clients
        .iter()
        .find(|c| c["name"].as_str().unwrap().starts_with(&own_name()))
        .unwrap();
    assert_eq!(ours["running"], json!(true));
    assert_eq!(ours["rate"], json!(48000));
}

#[test]
fn patch_a_client_volume_and_read_it_back() {
    let daemon = Daemon::start();
    let mut client = daemon.playing(16384);
    let name = running_client(&daemon);
    let path = format!("/inputs/clients/{}", encode(&name));
    let (status, body) = daemon.patch(&path, r#"{"volume_db": -6}"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["volume_db"], json!(-6.0));
    let (status, body) = daemon.get(&path);
    assert_eq!(status, 200);
    assert_eq!(body["volume_db"], json!(-6.0));
    assert!(
        wait_for(|| {
            top_up(&mut client);
            let (_, state) = daemon.get("/state");
            state["bit_perfect"] == json!(false) && state["reason"] == json!("volume")
        }),
        "a client's volume left the master bit-perfect"
    );
}

#[test]
fn an_unknown_field_is_400() {
    let daemon = Daemon::start();
    let (status, body) = daemon.patch("/outputs/1", r#"{"loudness": 3}"#);
    assert_eq!(status, 400);
    assert!(
        body["error"].as_str().unwrap().contains("loudness"),
        "{body}"
    );
    let (status, body) = daemon.patch("/outputs/1", "{not json");
    assert_eq!(status, 400);
    assert!(body["error"].is_string(), "{body}");
}

#[test]
fn output_7_is_404() {
    let daemon = Daemon::start();
    let (status, body) = daemon.get("/outputs/7");
    assert_eq!(status, 404);
    assert_eq!(body["error"], json!("no output 7"));
    let (status, body) = daemon.patch("/outputs/7", r#"{"volume": 50}"#);
    assert_eq!(status, 404);
    assert!(body["error"].is_string());
}

#[test]
fn volume_101_is_422() {
    let daemon = Daemon::start();
    let (status, body) = daemon.patch("/outputs/1", r#"{"volume": 101}"#);
    assert_eq!(status, 422);
    assert!(body["error"].is_string(), "{body}");
    // And nothing changed.
    assert_eq!(daemon.get("/outputs/1").1["volume"], json!(100));
}

/// A WebSocket on `/events`, subscribed once this returns.
fn subscribe(daemon: &Daemon) -> tungstenite::WebSocket<TcpStream> {
    let port = daemon.port.unwrap();
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    let url = format!("ws://127.0.0.1:{port}/events");
    let (socket, _) = tungstenite::client(url, stream).unwrap();
    socket
}

#[test]
fn events_announce_a_volume_change() {
    let daemon = Daemon::start();
    let mut socket = subscribe(&daemon);
    let (status, _) = daemon.patch("/outputs/1", r#"{"volume": 50}"#);
    assert_eq!(status, 200);
    let event: Value = match socket.read().unwrap() {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("not text: {other:?}"),
    };
    assert_eq!(event["type"], json!("output"));
    assert_eq!(event["id"], json!(1));
    assert_eq!(event["volume"], json!(50));
}

#[test]
fn a_reader_that_never_reads_does_not_slow_the_daemon() {
    let daemon = Daemon::start();
    // Never read. On loopback the kernel's buffers take every event
    // this test makes, so it shows that a reader that does not keep up
    // costs the rest nothing; the resync it would get is shown in the
    // unit tests of the API thread.
    let _stuck = subscribe(&daemon);
    // About 680 ms of audio in the ring, topped up after each patch.
    let mut client = daemon.playing(32768);
    running_client(&daemon);
    let mut slowest = Duration::ZERO;
    for i in 0..1000 {
        let body = format!(r#"{{"volume": {}}}"#, 50 + i % 2);
        let started = Instant::now();
        let (status, _) = daemon.patch("/outputs/1", &body);
        slowest = slowest.max(started.elapsed());
        assert_eq!(status, 200);
        top_up(&mut client);
    }
    assert_eq!(client.underruns(), 0, "the playing client ran short");
    assert!(
        slowest < Duration::from_millis(100),
        "a patch took {slowest:?}"
    );
    // Its handler did wait, and fell behind by more than the channel holds.
}

#[test]
fn the_api_off_leaves_no_thread() {
    let daemon = Daemon::start_with(None);
    let mut client = daemon.playing(16384);
    let start = client.hw_position().unwrap();
    assert!(
        wait_for(|| {
            top_up(&mut client);
            client.hw_position().unwrap() >= start + 4800
        }),
        "the daemon did not play without the API"
    );
}
