//! `aasctl` against the daemon of the harness.

mod common;
use common::*;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

fn aasctl(port: u16, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aasctl"));
    command.args(["--port", &port.to_string()]).args(args);
    command
}

#[test]
fn state_prints_json() {
    let daemon = Daemon::start();
    let out = aasctl(daemon.port.unwrap(), &["state"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let state: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(state["rate"].is_number(), "{state}");
}

#[test]
fn an_error_answer_exits_1() {
    let daemon = Daemon::start();
    let out = aasctl(daemon.port.unwrap(), &["output", "7", "volume", "10"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no output 7"), "{stderr}");
}

#[test]
fn a_change_prints_the_answer() {
    let daemon = Daemon::start();
    let out = aasctl(daemon.port.unwrap(), &["output", "1", "volume", "40"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(daemon.get("/outputs/1").1["volume"], json!(40));
}

#[test]
fn a_usage_error_exits_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_aasctl"))
        .arg("frobnicate")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn no_daemon_exits_1() {
    // A port nothing listens on: bound, then let go.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let out = aasctl(port, &["state"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(!out.stderr.is_empty());
}

#[test]
fn events_prints_a_line() {
    let daemon = Daemon::start();
    let mut child = aasctl(daemon.port.unwrap(), &["events"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(lines.next());
    });
    // The tool subscribes some time after it starts, and only changes after
    // that are told: change the volume until a line arrives.
    let deadline = Instant::now() + PATIENCE;
    let mut volume = 40;
    let line = loop {
        assert!(Instant::now() < deadline, "no event line");
        volume = if volume == 40 { 41 } else { 40 };
        daemon.patch("/outputs/1", &format!(r#"{{"volume": {volume}}}"#));
        if let Ok(line) = receiver.recv_timeout(Duration::from_millis(50)) {
            break line.unwrap().unwrap();
        }
    };
    child.kill().unwrap();
    child.wait().unwrap();
    let event: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(event["type"], json!("output"));
}

#[test]
fn levels_prints_a_line() {
    let daemon = Daemon::start();
    let mut child = aasctl(daemon.port.unwrap(), &["levels"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = lines.next().unwrap().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(line.starts_with("1:"), "{line}");
}

#[test]
fn an_unreadable_matrix_file_exits_1() {
    let out = aasctl(1, &["matrix", "put", "/nonexistent/matrix.json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("matrix.json") && !stderr.contains("usage"),
        "{stderr}"
    );
}

#[test]
fn a_config_that_does_not_load_exits_1() {
    let out = Command::new(env!("CARGO_BIN_EXE_aasctl"))
        .args(["--config", "/nonexistent/aas.toml", "state"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("usage"));
}

#[test]
fn a_closed_pipe_ends_quietly() {
    let daemon = Daemon::start();
    let mut child = aasctl(daemon.port.unwrap(), &["levels"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read one line, then close the pipe as `| head -1` does.
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    lines.next().unwrap().unwrap();
    drop(lines);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
