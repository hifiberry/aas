//! A fake AirPlay receiver for tests: it accepts one RTSP connection,
//! records every request and answers each as its script says.

use super::ap2::channel::{Keys, Opener, Sealer};
use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// A request as the receiver saw it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// What the receiver does with a request.
pub enum Answer {
    Status(u16, Vec<(&'static str, String)>),
    /// A status with a body, its type among the headers.
    Body(u16, Vec<(&'static str, String)>, Vec<u8>),
    /// The answer, in the clear, after which both sides seal the channel
    /// with the keys of this secret.
    SealAfter(Box<Answer>, Vec<u8>),
    /// Closes the connection without an answer.
    Close,
}

/// The answers of a receiver that takes everything: SETUP gives ports and
/// a session, RECORD a latency.
pub fn accepting(seen: &Seen) -> Answer {
    match seen.method.as_str() {
        "SETUP" => Answer::Status(
            200,
            vec![
                ("Session", "DEADBEEF".into()),
                (
                    "Transport",
                    "RTP/AVP/UDP;unicast;mode=record;server_port=6010;control_port=6011;\
                     timing_port=6012"
                        .into(),
                ),
            ],
        ),
        "RECORD" => Answer::Status(200, vec![("Audio-Latency", "11025".into())]),
        _ => Answer::Status(200, Vec::new()),
    }
}

pub struct FakeReceiver {
    pub port: u16,
    /// The receiver's audio socket, at the port `udp_accepting` gives.
    pub data: UdpSocket,
    /// Connections accepted so far.
    pub accepted: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Seen>>>,
    thread: Option<JoinHandle<()>>,
}

impl FakeReceiver {
    pub fn start(script: impl Fn(&Seen) -> Answer + Send + 'static) -> FakeReceiver {
        FakeReceiver::serving(1, |_| script)
    }

    /// A receiver that takes `connections` connections one after another.
    /// `make` builds the script from the port of the receiver's audio
    /// socket.
    pub fn serving<S: Fn(&Seen) -> Answer + Send + 'static>(
        connections: usize,
        make: impl FnOnce(u16) -> S,
    ) -> FakeReceiver {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let data = UdpSocket::bind("127.0.0.1:0").unwrap();
        let script = make(data.local_addr().unwrap().port());
        data.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicUsize::new(0));
        let (log, count) = (seen.clone(), accepted.clone());
        let thread = std::thread::spawn(move || {
            for _ in 0..connections {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                count.fetch_add(1, Ordering::SeqCst);
                serve(stream, &script, &log);
            }
        });
        FakeReceiver {
            port,
            data,
            accepted,
            seen,
            thread: Some(thread),
        }
    }

    /// As `serving`, with each connection served at the same time as the
    /// others, in a thread of its own.
    pub fn together<S: Fn(&Seen) -> Answer + Send + Sync + 'static>(
        connections: usize,
        make: impl FnOnce(u16) -> S,
    ) -> FakeReceiver {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let data = UdpSocket::bind("127.0.0.1:0").unwrap();
        let script = Arc::new(make(data.local_addr().unwrap().port()));
        data.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicUsize::new(0));
        let (log, count) = (seen.clone(), accepted.clone());
        let thread = std::thread::spawn(move || {
            let mut served = Vec::new();
            for _ in 0..connections {
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                count.fetch_add(1, Ordering::SeqCst);
                let (script, log) = (script.clone(), log.clone());
                served.push(std::thread::spawn(move || serve(stream, &*script, &log)));
            }
            for t in served {
                let _ = t.join();
            }
        });
        FakeReceiver {
            port,
            data,
            accepted,
            seen,
            thread: Some(thread),
        }
    }

    /// The requests so far.
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// Waits for the connection to end, then gives every request.
    pub fn finish(mut self) -> Vec<Seen> {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.seen()
    }
}

/// The first whole request in `buf`, taken out of it.
fn take_request(buf: &mut Vec<u8>) -> Option<Seen> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let (method, url) = (first.next()?.to_string(), first.next()?.to_string());
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    if buf.len() < end + 4 + length {
        return None;
    }
    let body = buf[end + 4..end + 4 + length].to_vec();
    buf.drain(..end + 4 + length);
    Some(Seen {
        method,
        url,
        headers,
        body,
    })
}

/// The receiver's side of a sealed channel: it opens with the sender's
/// write key and seals with its read key.
type Sealed = (Opener, Sealer);

/// Answers the requests on one connection as `script` says, until it
/// closes or the script closes it.
fn serve(
    mut stream: std::net::TcpStream,
    script: &dyn Fn(&Seen) -> Answer,
    log: &Mutex<Vec<Seen>>,
) {
    let mut buf = Vec::new();
    let mut raw = Vec::new();
    let mut sealed: Option<Sealed> = None;
    let mut chunk = [0u8; 4096];
    loop {
        let request = loop {
            if let Some(r) = take_request(&mut buf) {
                break Some(r);
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break None,
                Ok(n) => match &mut sealed {
                    None => buf.extend_from_slice(&chunk[..n]),
                    Some((opener, _)) => {
                        raw.extend_from_slice(&chunk[..n]);
                        if opener.open(&mut raw, &mut buf).is_err() {
                            break None;
                        }
                    }
                },
            }
        };
        let Some(request) = request else { return };
        let cseq = request.header("CSeq").unwrap_or("0").to_string();
        let mut answer = script(&request);
        log.lock().unwrap().push(request);
        let mut then_seal = None;
        if let Answer::SealAfter(inner, secret) = answer {
            answer = *inner;
            then_seal = Some(secret);
        }
        let (status, headers, body) = match answer {
            Answer::Close => return,
            Answer::Status(status, headers) => (status, headers, Vec::new()),
            Answer::Body(status, headers, body) => (status, headers, body),
            Answer::SealAfter(..) => unreachable!("one seal at a time"),
        };
        let version = if sealed.is_some() {
            "HTTP/1.1"
        } else {
            "RTSP/1.0"
        };
        let mut text = format!("{version} {status} X\r\nCSeq: {cseq}\r\n");
        for (n, v) in headers {
            text.push_str(&format!("{n}: {v}\r\n"));
        }
        if !body.is_empty() {
            text.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        text.push_str("\r\n");
        let mut plain = text.into_bytes();
        plain.extend_from_slice(&body);
        let out = match &mut sealed {
            None => plain,
            Some((_, sealer)) => {
                let mut out = Vec::new();
                sealer.seal(&plain, &mut out);
                out
            }
        };
        if stream.write_all(&out).is_err() {
            return;
        }
        if let Some(secret) = then_seal {
            let keys = Keys::control(&secret);
            sealed = Some((Opener::new(&keys.write), Sealer::new(&keys.read)));
        }
    }
}

/// As `accepting`, with the audio going to `data_port`.
pub fn accepting_on(data_port: u16) -> impl Fn(&Seen) -> Answer + Send + 'static {
    move |seen: &Seen| match seen.method.as_str() {
        "SETUP" => Answer::Status(
            200,
            vec![
                ("Session", "DEADBEEF".into()),
                (
                    "Transport",
                    format!("RTP/AVP/UDP;unicast;server_port={data_port};control_port=9"),
                ),
            ],
        ),
        _ => accepting(seen),
    }
}
