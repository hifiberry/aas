//! The event channel of an AirPlay 2 session: a TCP connection the sender
//! opens to the receiver's `eventPort` after the session SETUP, sealed with
//! the event keys. The receiver sends requests on it, such as a change of
//! its volume, and the sender answers each with 200. A receiver may hold
//! back its answer to RECORD until this channel is there.

use super::channel::{Keys, Opener, Sealer};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// What may wait unanswered before the channel is given up.
const MOST: usize = 64 * 1024;

pub struct Events {
    stream: TcpStream,
    sealer: Sealer,
    opener: Opener,
    raw: Vec<u8>,
    plain: Vec<u8>,
    /// Once the channel has failed it stays failed.
    failed: bool,
}

impl Events {
    /// Connects to the receiver's event port, within `timeout`.
    pub fn connect(addr: SocketAddr, secret: &[u8], timeout: Duration) -> std::io::Result<Events> {
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_nonblocking(true)?;
        let keys = Keys::events(secret);
        Ok(Events {
            stream,
            sealer: Sealer::new(&keys.write),
            opener: Opener::new(&keys.read),
            raw: Vec::new(),
            plain: Vec::new(),
            failed: false,
        })
    }

    /// Answers every whole request the receiver has sent. False once the
    /// receiver has closed the channel or it failed. Does not wait.
    pub fn poll(&mut self) -> bool {
        self.failed = self.failed || !self.answer();
        !self.failed
    }

    fn answer(&mut self) -> bool {
        let mut chunk = [0u8; 1024];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return false,
                Ok(n) => {
                    self.raw.extend_from_slice(&chunk[..n]);
                    if self.opener.open(&mut self.raw, &mut self.plain).is_err()
                        || self.raw.len() + self.plain.len() > MOST
                    {
                        return false;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
        while let Some(parsed) = request(&self.plain) {
            let Some((cseq, used)) = parsed else {
                return false;
            };
            self.plain.drain(..used);
            let answer = format!("RTSP/1.0 200 OK\r\nServer: aas\r\nCSeq: {cseq}\r\n\r\n");
            let mut out = Vec::new();
            self.sealer.seal(answer.as_bytes(), &mut out);
            // The answers are small; a full socket buffer means the
            // receiver no longer reads.
            if self.stream.write_all(&out).is_err() {
                return false;
            }
        }
        self.raw.len() + self.plain.len() <= MOST
    }
}

/// The CSeq of the first whole request in `buf` and the bytes it takes;
/// None while it is not whole, Some(None) for a length beyond `MOST`.
fn request(buf: &[u8]) -> Option<Option<(String, usize)>> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..end]);
    let header = |name: &str| {
        head.split("\r\n").skip(1).find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    let length = header("Content-Length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if length > MOST {
        return Some(None);
    }
    let used = end + 4 + length;
    (buf.len() >= used).then(|| Some((header("CSeq").unwrap_or_else(|| "0".into()), used)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    const SECRET: [u8; 64] = [7; 64];

    /// The receiver's end: it seals with our read key and opens with our
    /// write key.
    struct Receiver {
        stream: TcpStream,
        sealer: Sealer,
        opener: Opener,
        raw: Vec<u8>,
        plain: Vec<u8>,
    }

    impl Receiver {
        fn send(&mut self, text: &[u8]) {
            let mut out = Vec::new();
            self.sealer.seal(text, &mut out);
            self.stream.write_all(&out).unwrap();
        }

        /// Reads until `n` answers have come, and gives them.
        fn answers(&mut self, n: usize) -> String {
            let mut buf = [0u8; 1024];
            while self.plain.windows(4).filter(|w| w == b"\r\n\r\n").count() < n {
                let got = self.stream.read(&mut buf).unwrap();
                assert!(got > 0, "closed");
                self.raw.extend_from_slice(&buf[..got]);
                self.opener.open(&mut self.raw, &mut self.plain).unwrap();
            }
            String::from_utf8(std::mem::take(&mut self.plain)).unwrap()
        }
    }

    fn pair() -> (Events, Receiver) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(listener.accept().unwrap().0).unwrap());
        let events = Events::connect(addr, &SECRET, Duration::from_secs(1)).unwrap();
        let keys = Keys::events(&SECRET);
        let stream = rx.recv().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let receiver = Receiver {
            stream,
            sealer: Sealer::new(&keys.read),
            opener: Opener::new(&keys.write),
            raw: Vec::new(),
            plain: Vec::new(),
        };
        (events, receiver)
    }

    /// Polls until `done` or a second has passed.
    fn poll_until(events: &mut Events, done: impl Fn(&mut Events) -> bool) {
        for _ in 0..100 {
            if done(events) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn each_event_is_answered_with_200_and_its_cseq() {
        let (mut events, mut receiver) = pair();
        assert!(events.poll());
        receiver.send(
            b"POST /command RTSP/1.0\r\nCSeq: 3\r\nContent-Length: 4\r\n\r\nbodyPOST /command RTSP/1.0\r\nCSeq: 4\r\n\r\n",
        );
        poll_until(&mut events, |e| {
            e.poll();
            false
        });
        let answers = receiver.answers(2);
        assert_eq!(
            answers,
            "RTSP/1.0 200 OK\r\nServer: aas\r\nCSeq: 3\r\n\r\nRTSP/1.0 200 OK\r\nServer: aas\r\nCSeq: 4\r\n\r\n"
        );
    }

    #[test]
    fn an_event_split_across_frames_is_answered_once_whole() {
        let (mut events, mut receiver) = pair();
        receiver.send(b"POST /command RTSP/1.0\r\nCSeq: 9\r\nContent-Le");
        std::thread::sleep(Duration::from_millis(30));
        assert!(events.poll());
        receiver.send(b"ngth: 2\r\n\r\nx");
        std::thread::sleep(Duration::from_millis(30));
        assert!(events.poll());
        receiver.send(b"y");
        poll_until(&mut events, |e| {
            e.poll();
            false
        });
        assert_eq!(
            receiver.answers(1),
            "RTSP/1.0 200 OK\r\nServer: aas\r\nCSeq: 9\r\n\r\n"
        );
    }

    #[test]
    fn a_huge_or_endless_request_fails_the_channel() {
        let (mut events, mut receiver) = pair();
        receiver
            .send(b"POST /x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 18446744073709551615\r\n\r\n");
        poll_until(&mut events, |e| !e.poll());
        assert!(!events.poll());
        let (mut events, mut receiver) = pair();
        receiver.send(b"POST /x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 100000\r\n\r\n");
        receiver.send(&vec![b'x'; 70 * 1024]);
        poll_until(&mut events, |e| !e.poll());
        assert!(!events.poll());
    }

    #[test]
    fn a_closed_or_broken_channel_polls_false() {
        let (mut events, receiver) = pair();
        drop(receiver);
        poll_until(&mut events, |e| !e.poll());
        assert!(!events.poll());
        let (mut events, mut receiver) = pair();
        receiver.stream.write_all(&[5, 0, 1, 2, 3, 4, 5]).unwrap();
        receiver.stream.write_all(&[0; 16]).unwrap();
        poll_until(&mut events, |e| !e.poll());
        assert!(!events.poll());
    }
}
