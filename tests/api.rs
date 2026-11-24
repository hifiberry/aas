//! The HTTP API end to end: the daemon with a `MemorySink`, clients on a
//! real socket, and HTTP on a listener of its own.

mod common;
use aas::config::{Idle, PluginConfig, PluginPlace};
use aas_client::client::Client;
use aas_client::convert::ClientFormat;
use common::*;
use serde_json::{Value, json};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::{self, Message};

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
        .find(|c| c["name"] == json!(own_name()))
        .expect("the client is not under its bare process name");
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
    // Also beyond what a byte holds, or below 0: out of range, not a
    // value of the wrong type.
    for volume in ["101", "256", "-1", "100000"] {
        let body = format!(r#"{{"volume": {volume}}}"#);
        let (status, body) = daemon.patch("/outputs/1", &body);
        assert_eq!(status, 422, "{volume}");
        assert_eq!(body["error"], json!("volume must be from 0 to 100"));
    }
    // And nothing changed.
    assert_eq!(daemon.get("/outputs/1").1["volume"], json!(100));
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

/// A quiet subscriber is pinged every 10 s; its pong is what keeps the
/// socket from being closed as idle after 30 s.
#[test]
fn a_quiet_subscriber_is_pinged() {
    let daemon = Daemon::start();
    let mut socket = subscribe(&daemon);
    socket
        .get_mut()
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    match socket.read().unwrap() {
        Message::Ping(_) => {}
        other => panic!("not a ping: {other:?}"),
    }
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
}

#[test]
fn the_api_off_leaves_no_thread() {
    let daemon = Daemon::start_with(None, Vec::new());
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

/// Runs `body` in a child process of this test binary, which runs only
/// `test`, and gives what the child wrote to stderr, the daemon's log
/// among it. In the child, where `AAS_CHILD` is set, this runs `body`
/// and gives None.
fn in_child(test: &str, body: impl FnOnce()) -> Option<String> {
    if std::env::var_os("AAS_CHILD").is_some() {
        body();
        return None;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env("AAS_CHILD", "1")
        .output()
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "the child failed:\n{stdout}\n{stderr}"
    );
    Some(stderr)
}

#[test]
fn a_matrix_put_reaches_the_engine_unrefused() {
    let log = in_child("a_matrix_put_reaches_the_engine_unrefused", || {
        let daemon = Daemon::start();
        let mut client = daemon.playing(16384);
        running_client(&daemon);
        let swap = r#"{"outputs": [{"output": 1, "routes": [
            {"from": "players.left", "to": 2}, {"from": "players.right", "to": 1}]}]}"#;
        for body in [swap, r#"{"outputs": [{"output": 1, "routes": []}]}"#] {
            let (status, answer) = daemon.http("PUT", "/matrix", Some(body));
            assert_eq!(status, 200, "{answer}");
            // Past the 20 ms between two matrices, and long enough for
            // the engine to answer and the control thread to read it.
            let until = Instant::now() + Duration::from_millis(200);
            while Instant::now() < until {
                top_up(&mut client);
                thread::sleep(Duration::from_millis(5));
            }
        }
        let (status, matrix) = daemon.get("/matrix");
        assert_eq!(status, 200);
        let routes = &matrix["outputs"][0]["routes"];
        assert_eq!(routes[0]["from"], json!("players.left"), "{matrix}");
        assert_eq!(routes[0]["to"], json!(1), "{matrix}");
    });
    if let Some(log) = log {
        assert!(!log.contains("refused"), "{log}");
    }
}

#[test]
fn a_body_over_64_kib_is_413() {
    let daemon = Daemon::start();
    // Valid JSON, padded with spaces past the limit.
    let body = format!(r#"{{"volume": 50}}{}"#, " ".repeat(64 * 1024));
    let (status, answer) = daemon.patch("/outputs/1", &body);
    assert_eq!(status, 413);
    assert_eq!(answer["error"], json!("a body has at most 65536 bytes"));
    // Nothing changed, and the daemon answers on.
    assert_eq!(daemon.get("/outputs/1").1["volume"], json!(100));
    // One within the limit is taken.
    let body = format!(r#"{{"volume": 50}}{}"#, " ".repeat(1024));
    assert_eq!(daemon.patch("/outputs/1", &body).0, 200);
}

#[test]
fn a_websocket_message_over_the_limit_closes_that_socket() {
    let daemon = Daemon::start();
    let mut socket = subscribe(&daemon);
    let big = "x".repeat(64 * 1024);
    socket.send(Message::Text(big.into())).unwrap();
    // The daemon closes the socket: a close frame, or the connection gone,
    // but not a read that times out.
    loop {
        match socket.read() {
            Ok(Message::Close(_)) => break,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                panic!("the socket was not closed")
            }
            Err(_) => break,
            Ok(_) => continue,
        }
    }
    // The daemon goes on: it answers, and a new subscriber is told of a
    // change.
    assert_eq!(daemon.get("/state").0, 200);
    let mut other = subscribe(&daemon);
    assert_eq!(daemon.patch("/outputs/1", r#"{"volume": 50}"#).0, 200);
    let event: Value = match other.read().unwrap() {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("not text: {other:?}"),
    };
    assert_eq!(event["volume"], json!(50));
}

#[test]
fn get_metrics_counts_cycles() {
    let daemon = Daemon::start();
    let mut client = daemon.playing(16384);
    running_client(&daemon);
    let cycles = |daemon: &Daemon| {
        daemon.get("/metrics").1["master"]["cycles"]
            .as_u64()
            .unwrap()
    };
    let first = cycles(&daemon);
    thread::sleep(Duration::from_millis(100));
    top_up(&mut client);
    let (status, metrics) = daemon.get("/metrics");
    assert_eq!(status, 200);
    assert!(metrics["master"]["cycles"].as_u64().unwrap() > first);
    let master = &metrics["master"];
    assert!(master["cycle_ns_avg"].as_u64() <= master["cycle_ns_worst"].as_u64());
    assert!(master["buffer_ns"].as_u64().unwrap() > 0);
    assert_eq!(master["period_frames"], json!(64));
    let streams = metrics["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0]["rate"], json!(48000));
    assert_eq!(metrics["analog"], json!(null));
}

#[test]
fn patch_a_plugin_and_read_it_back() {
    let balance = PluginConfig {
        place: PluginPlace::Output,
        kind: "balance".into(),
        path: None,
        params: Default::default(),
    };
    let daemon = Daemon::with_plugins(vec![balance]);
    let (status, list) = daemon.get("/plugins");
    assert_eq!(status, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], json!("1/output/0"));
    assert_eq!(list[0]["type"], json!("balance"));
    assert_eq!(list[0]["params"][0]["value"], json!(0.0));
    let path = "/plugins/1/output/0";
    let (status, body) = daemon.patch(path, r#"{"params": {"balance": 0.5}, "enabled": false}"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["params"][0]["value"], json!(0.5));
    assert_eq!(body["enabled"], json!(false));
    let (status, body) = daemon.get(path);
    assert_eq!(status, 200);
    assert_eq!(body["params"][0]["value"], json!(0.5));
    assert_eq!(body["enabled"], json!(false));
    // Disabled, it is neutral once its fade has run.
    assert!(wait_for(|| daemon.get(path).1["neutral"] == json!(true)));
    // Out of range, unknown, not there.
    let (status, _) = daemon.patch(path, r#"{"params": {"balance": 2}}"#);
    assert_eq!(status, 422);
    let (status, _) = daemon.patch(path, r#"{"params": {"gain": 0}}"#);
    assert_eq!(status, 422);
    let (status, _) = daemon.patch(path, r#"{"colour": "red"}"#);
    assert_eq!(status, 400);
    assert_eq!(daemon.get("/plugins/1/output/1").0, 404);
    assert_eq!(daemon.get("/plugins/1/nowhere/0").0, 404);
    assert_eq!(daemon.get("/plugins/9/output/0").0, 404);
    // Its time per cycle is in the metrics, by its id.
    let (_, metrics) = daemon.get("/metrics");
    assert_eq!(metrics["plugins"][0]["id"], json!("1/output/0"));
}

#[test]
fn graph_as_json() {
    let daemon = Daemon::start();
    let (status, graph) = daemon.get("/graph");
    assert_eq!(status, 200);
    let nodes = graph["nodes"].as_array().unwrap();
    assert!(nodes.iter().any(|n| n["id"] == "out/1/card"));
    assert!(!graph["edges"].as_array().unwrap().is_empty());
}

#[test]
fn graph_as_dot() {
    let daemon = Daemon::start();
    let port = daemon.port.unwrap();
    let (status, content_type, body) = http_raw(port, "GET", "/graph?format=dot", None);
    assert_eq!(status, 200);
    assert_eq!(content_type, "text/vnd.graphviz");
    assert!(body.starts_with("digraph aas {"), "{body}");
    let (status, _, _) = http_raw(port, "GET", "/graph?format=json", None);
    assert_eq!(status, 200);
    let (status, _) = daemon.get("/graph?format=svg");
    assert_eq!(status, 400);
}

#[test]
fn lowering_the_latency_while_playing() {
    let daemon = Daemon::start();
    let mut client = daemon.playing(16384);
    running_client(&daemon);
    let period = |daemon: &Daemon| daemon.get("/metrics").1["master"]["period_frames"].clone();
    assert_eq!(period(&daemon), json!(64));
    let (status, mut settings) = daemon.get("/settings");
    assert_eq!(status, 200);
    // The board's figures depend on the machine the test runs on.
    for key in ["latency_recommended_ms", "latency_min_ms"] {
        assert!(settings[key].is_number(), "{key}");
        settings.as_object_mut().unwrap().remove(key);
    }
    assert_eq!(
        settings,
        json!({"bit_perfect": "auto", "latency_ms": LATENCY_MS, "default_rate": 48000,
               "format": "f32", "resampling": "high"})
    );
    let before = client.underruns();
    let half = format!(r#"{{"latency_ms": {}}}"#, LATENCY_MS / 2.0);
    let (status, body) = daemon.patch("/settings", &half);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["latency_ms"], json!(LATENCY_MS / 2.0));
    // 2 ms: two periods of 32 frames, the shortest there is.
    assert!(wait_for(|| {
        top_up(&mut client);
        period(&daemon) == json!(32)
    }));
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        top_up(&mut client);
        thread::sleep(Duration::from_millis(5));
    }
    let underruns = client.underruns() - before;
    assert!(underruns <= 2, "{underruns} underruns");
    // What cannot change at run time is refused, and changes nothing.
    let (status, body) = daemon.patch("/settings", r#"{"format": "f64"}"#);
    assert_eq!(status, 422);
    assert_eq!(body["error"], json!("format changes only in the config"));
    let (status, body) = daemon.patch("/settings", r#"{"latency_ms": 8}"#);
    assert_eq!(status, 422);
    assert_eq!(
        body["error"],
        json!("latency_ms above the 4 ms the daemon started with needs a restart")
    );
    assert_eq!(period(&daemon), json!(32));
}

/// The next frame of levels, past any ping.
fn next_levels(socket: &mut tungstenite::WebSocket<TcpStream>) -> Vec<u8> {
    loop {
        match socket.read().unwrap() {
            Message::Binary(frame) => return frame.to_vec(),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("not a frame: {other:?}"),
        }
    }
}

#[test]
fn levels_come_every_100_ms() {
    let daemon = Daemon::start();
    let mut socket = websocket(&daemon, "/levels");
    // The stereo master alone: its id, 2 channels, RMS and peak for each,
    // the clip bits.
    let first = next_levels(&mut socket);
    assert_eq!(first.len(), 7, "{first:?}");
    assert_eq!(first[..2], [1, 2]);
    let started = Instant::now();
    for _ in 0..10 {
        let frame = next_levels(&mut socket);
        assert_eq!(frame[0], 1, "{frame:?}");
    }
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(800) && took <= Duration::from_millis(1200),
        "10 frames in {took:?}"
    );
}

/// Fills the client's ring with frames at half scale as far as it takes.
fn top_up_loud(client: &mut Client) {
    let frame = [(1i32 << 30).to_le_bytes(), (1i32 << 30).to_le_bytes()].concat();
    let data = frame.repeat(4096);
    while client.write(&data).unwrap() > 0 {}
}

/// The master's levels read as silence once its idle rule has closed the
/// card: its last window must not stay on the meter, and it stays in the
/// frame.
#[test]
fn an_idle_master_reads_zeros() {
    let api = TcpListener::bind("127.0.0.1:0").unwrap();
    let daemon = Daemon::start_idle(Some(api), Vec::new(), Idle::Off { delay_s: 1 });
    let mut socket = websocket(&daemon, "/levels");
    let mut client = Client::connect(&daemon.path).unwrap();
    client
        .configure(48000, ClientFormat::S32, 2, 16384)
        .unwrap();
    top_up_loud(&mut client);
    client.start().unwrap();
    // Half scale: -6 dB, 229.
    let mut frame = Vec::new();
    let heard = wait_for(|| {
        top_up_loud(&mut client);
        frame = next_levels(&mut socket);
        frame[2] >= 220
    });
    assert!(heard, "the master was never heard: {frame:?}");
    client.stop().unwrap();
    let state = || daemon.get("/outputs/1").1["state"].clone();
    assert!(
        wait_for(|| state() == json!("off")),
        "the master never went off: {}",
        state()
    );
    // A new socket: the old one holds frames sent while the master played.
    drop(socket);
    let mut socket = websocket(&daemon, "/levels");
    for _ in 0..3 {
        let frame = next_levels(&mut socket);
        assert_eq!(frame, [1, 2, 0, 0, 0, 0, 0]);
    }
}

/// Whether a manifest path pattern matches a path remainder, as the
/// HiFiBerry OS security model matches them: `*` is exactly one segment,
/// `**` the rest of the path, also nothing, and only last.
fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.trim_start_matches('/').split('/').collect();
    let path: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    for (i, part) in pattern.iter().enumerate() {
        match *part {
            "**" => {
                assert_eq!(i, pattern.len() - 1, "** not last in {pattern:?}");
                return true;
            }
            "*" if i < path.len() => {}
            _ if i < path.len() && path[i] == *part => {}
            _ => return false,
        }
    }
    pattern.len() == path.len()
}

/// The tier of a browser-facing request under a manifest: the first rule
/// whose method and path match, else the default.
fn tier(manifest: &Value, method: &str, path: &str) -> String {
    let prefix = manifest["match_prefix"].as_str().unwrap();
    let rest = path.strip_prefix(prefix).expect("not under the prefix");
    for rule in manifest["rules"].as_array().unwrap() {
        let methods = rule["methods"].as_array().unwrap();
        let method_hit = methods.iter().any(|m| m == "*" || m == method);
        let path_hit = rule["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| glob_matches(p.as_str().unwrap(), rest));
        if method_hit && path_hit {
            return rule["tier"].as_str().unwrap().to_string();
        }
    }
    manifest["default_tier"].as_str().unwrap().to_string()
}

#[test]
fn the_manifest_marks_only_settings_risky() {
    let file = concat!(env!("CARGO_MANIFEST_DIR"), "/dist/auth.d/aas.json");
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    assert_eq!(manifest["service"], json!("aas"));
    assert_eq!(manifest["match_prefix"], json!("/api/aas/v1"));
    assert_eq!(manifest["default_tier"], json!("risky"));
    // Every route of the API, as the browser sees it.
    let gets = [
        "/state",
        "/outputs",
        "/outputs/1",
        "/outputs/2",
        "/inputs",
        "/inputs/analog",
        "/inputs/clients/mpd",
        "/graph",
        "/graph?format=dot",
        "/metrics",
        "/matrix",
        "/plugins",
        "/plugins/1/mix/0",
        "/plugins/2/output/1",
        "/settings",
        "/events",
        "/levels",
    ];
    for path in gets {
        let path = path.split('?').next().unwrap();
        let full = format!("/api/aas/v1{path}");
        assert_eq!(tier(&manifest, "GET", &full), "ok", "GET {path}");
    }
    let ok_changes = [
        ("PATCH", "/outputs/1"),
        ("PATCH", "/outputs/2"),
        ("PATCH", "/inputs/analog"),
        ("PATCH", "/inputs/clients/mpd"),
        ("PATCH", "/plugins/1/mix/0"),
        ("PATCH", "/plugins/1/analog/2"),
        ("PUT", "/matrix"),
    ];
    for (method, path) in ok_changes {
        let full = format!("/api/aas/v1{path}");
        assert_eq!(tier(&manifest, method, &full), "ok", "{method} {path}");
    }
    let risky = [
        ("PATCH", "/settings"),
        ("PUT", "/settings"),
        ("PATCH", "/outputs"),
        ("PATCH", "/outputs/1/extra"),
        ("PATCH", "/matrix"),
        ("POST", "/matrix"),
        ("PUT", "/plugins/1/mix/0"),
        ("DELETE", "/state"),
        ("POST", "/anything/else"),
    ];
    for (method, path) in risky {
        let full = format!("/api/aas/v1{path}");
        assert_eq!(tier(&manifest, method, &full), "risky", "{method} {path}");
    }
}
