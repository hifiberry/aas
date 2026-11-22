//! A fake AudioControl for tests: `GET /api/now-playing`, a few covers, and
//! the events WebSocket, all on one port.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A JPEG's first bytes, enough for `cover_mime`.
pub const JPEG: [u8; 6] = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];

#[derive(Default)]
struct Shared {
    now_playing: Mutex<String>,
    events: Mutex<VecDeque<String>>,
    /// Ends every open WebSocket once.
    close: AtomicBool,
    /// Requests for each path so far.
    seen: Mutex<Vec<String>>,
    sockets: AtomicUsize,
}

pub struct FakeAcr {
    pub port: u16,
    shared: Arc<Shared>,
}

impl FakeAcr {
    pub fn start(now_playing: &str) -> FakeAcr {
        FakeAcr::start_on(TcpListener::bind("127.0.0.1:0").unwrap(), now_playing)
    }

    pub fn start_on(listener: TcpListener, now_playing: &str) -> FakeAcr {
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Shared::default());
        *shared.now_playing.lock().unwrap() = now_playing.to_string();
        let s = shared.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let s = s.clone();
                std::thread::spawn(move || serve(stream, &s));
            }
        });
        FakeAcr { port, shared }
    }

    /// The base URL of its API.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/api", self.port)
    }

    pub fn set_now_playing(&self, json: &str) {
        *self.shared.now_playing.lock().unwrap() = json.to_string();
    }

    /// Sends an event on the open WebSocket.
    pub fn push(&self, json: &str) {
        self.shared
            .events
            .lock()
            .unwrap()
            .push_back(json.to_string());
    }

    /// Closes the open WebSockets.
    pub fn close_sockets(&self) {
        self.shared.close.store(true, Ordering::SeqCst);
    }

    /// WebSockets accepted so far.
    pub fn sockets(&self) -> usize {
        self.shared.sockets.load(Ordering::SeqCst)
    }

    /// Whether `path` has been asked for.
    pub fn saw(&self, path: &str) -> bool {
        self.shared.seen.lock().unwrap().iter().any(|p| p == path)
    }
}

fn serve(mut stream: TcpStream, s: &Shared) {
    let mut head = [0u8; 2048];
    let Ok(n) = stream.peek(&mut head) else {
        return;
    };
    let text = String::from_utf8_lossy(&head[..n]).to_string();
    let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
    s.seen.lock().unwrap().push(path.clone());
    if path == "/api/events" {
        return socket(stream, s);
    }
    // Consume the request before answering.
    let _ = stream.read(&mut head);
    let (status, kind, body): (&str, &str, Vec<u8>) = match path.as_str() {
        "/api/now-playing" => (
            "200 OK",
            "application/json",
            s.now_playing.lock().unwrap().clone().into_bytes(),
        ),
        "/cover.jpg" => ("200 OK", "image/jpeg", JPEG.to_vec()),
        "/big.jpg" => {
            let mut big = JPEG.to_vec();
            big.resize(5 * 1024 * 1024, 0);
            ("200 OK", "image/jpeg", big)
        }
        "/slow.jpg" => {
            std::thread::sleep(Duration::from_secs(30));
            return;
        }
        _ => ("404 Not Found", "text/plain", b"no".to_vec()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

fn socket(stream: TcpStream, s: &Shared) {
    let Ok(mut ws) = tungstenite::accept(stream) else {
        return;
    };
    s.sockets.fetch_add(1, Ordering::SeqCst);
    s.close.store(false, Ordering::SeqCst);
    loop {
        if s.close.swap(false, Ordering::SeqCst) {
            let _ = ws.close(None);
            let _ = ws.flush();
            return;
        }
        let next = s.events.lock().unwrap().pop_front();
        if let Some(event) = next
            && ws.send(tungstenite::Message::text(event)).is_err()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
