//! Tells ACR, the audio controller, whether the AES67 input plays: a POST
//! of `{"type":"state_changed","state":"playing"|"stopped"}` to the player
//! `aes67`, as the retired `hifiberry-aes67` agent did.
//!
//! The POST runs on its own thread, so the control thread never waits for
//! ACR. Only the latest state matters: a state that is overtaken before the
//! thread picks it up is dropped. A failed POST is logged once and not
//! retried; the next change of state is the next try.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// Where the player's state goes.
pub const URL: &str = "http://127.0.0.1:1080/api/player/aes67/update";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Slot {
    /// The latest state not yet sent.
    state: Option<&'static str>,
    stop: bool,
}

type Shared = Arc<(Mutex<Slot>, Condvar)>;
type Warn = Arc<dyn Fn(&str) + Send + Sync>;

/// The helper thread `aas-aes67-acr`, started by the first `report`.
pub struct Acr {
    url: String,
    warn: Warn,
    shared: Shared,
    thread: Option<JoinHandle<()>>,
}

impl Acr {
    pub fn new(url: &str) -> Self {
        Self::with_warn(
            url,
            Arc::new(|e| eprintln!("aas: aes67: warning: ACR: {e}")),
        )
    }

    pub fn with_warn(url: &str, warn: Warn) -> Self {
        Acr {
            url: url.to_string(),
            warn,
            shared: Shared::default(),
            thread: None,
        }
    }

    /// Hands `state` ("playing" or "stopped") to the thread and returns at
    /// once, starting the thread on the first call.
    pub fn report(&mut self, state: &'static str) {
        if self.thread.is_none() {
            let shared = self.shared.clone();
            let url = self.url.clone();
            let warn = self.warn.clone();
            let spawned = std::thread::Builder::new()
                .name("aas-aes67-acr".into())
                .spawn(move || run(&shared, &url, warn));
            match spawned {
                Ok(handle) => self.thread = Some(handle),
                // The next report tries again.
                Err(e) => {
                    (self.warn)(&e.to_string());
                    return;
                }
            }
        }
        let (slot, wake) = &*self.shared;
        lock(slot).state = Some(state);
        wake.notify_one();
    }

    /// Ends the thread after it has sent what is pending; a POST under way
    /// is waited for, at most 5 s.
    pub fn stop(&mut self) {
        let (slot, wake) = &*self.shared;
        lock(slot).stop = true;
        wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Acr {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(shared: &Shared, url: &str, warn: Warn) {
    let agent = ureq::AgentBuilder::new().timeout_connect(TIMEOUT).build();
    let (slot, wake) = &**shared;
    loop {
        let state = {
            let mut guard = lock(slot);
            loop {
                // What is pending goes out before the stop is heeded.
                if let Some(state) = guard.state.take() {
                    break state;
                }
                if guard.stop {
                    return;
                }
                guard = wake.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
        };
        if let Err(e) = post(&agent, url, state) {
            warn(&e);
        }
    }
}

fn post(agent: &ureq::Agent, url: &str, state: &str) -> Result<(), String> {
    agent
        .post(url)
        .timeout(TIMEOUT)
        .set("Content-Type", "application/json")
        .send_string(&format!(r#"{{"type":"state_changed","state":"{state}"}}"#))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
pub(crate) mod testing {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// A fake ACR that answers each request and passes on its request line
    /// and body.
    /// The request line, the content type and the body.
    pub(crate) type Request = (String, String, String);

    pub(crate) fn fake_acr() -> (String, mpsc::Receiver<Request>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/api/player/aes67/update",
            listener.local_addr().unwrap()
        );
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut length = 0;
                let mut kind = String::new();
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                        if name.eq_ignore_ascii_case("content-type") {
                            kind = value.trim().to_string();
                        }
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = tx.send((
                    line.trim().to_string(),
                    kind,
                    String::from_utf8(body).unwrap(),
                ));
            }
        });
        (url, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::fake_acr;
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn the_state_is_posted_to_the_player() {
        let (url, requests) = fake_acr();
        let mut acr = Acr::new(&url);
        acr.report("playing");
        let (line, kind, body) = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(line, "POST /api/player/aes67/update HTTP/1.1");
        assert_eq!(kind, "application/json");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "state_changed", "state": "playing"})
        );
        acr.report("stopped");
        let (_, _, body) = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(body.contains("\"stopped\""));
    }

    #[test]
    fn a_pending_state_is_sent_before_the_stop() {
        let (url, requests) = fake_acr();
        let mut acr = Acr::new(&url);
        acr.report("playing");
        acr.report("stopped");
        acr.stop();
        let mut states = Vec::new();
        while let Ok((_, _, body)) = requests.recv_timeout(Duration::from_millis(300)) {
            states.push(body);
        }
        // The first may have been overtaken, the last never is.
        assert!(states.last().unwrap().contains("\"stopped\""));
    }

    #[test]
    fn a_dead_acr_costs_a_log_line() {
        // Accepts and closes at once.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                drop(stream);
            }
        });
        let warned = Arc::new(AtomicUsize::new(0));
        let counter = warned.clone();
        let mut acr = Acr::with_warn(
            &url,
            Arc::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        );
        acr.report("playing");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while warned.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        acr.stop();
        assert_eq!(warned.load(Ordering::SeqCst), 1, "one line, no retry");
    }
}
