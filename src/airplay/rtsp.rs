//! A minimal RTSP client for AirPlay: requests written as text, responses
//! parsed as they arrive, over a blocking TCP connection with a timeout on
//! every read, on the output's own thread.

use super::ap2::channel::{Keys, Opener, Sealer};
use std::fmt;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// TCP keepalive on a session's connection, in seconds and probes: idle
/// time, interval, count. Nothing is sent on it while audio streams, so a
/// receiver that loses power is found by these probes, within about 15 s.
pub const KEEPALIVE: [libc::c_int; 3] = [5, 2, 5];

/// A response body larger than this is refused.
pub const MAX_BODY: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtspError {
    /// The connection failed, closed or timed out.
    Io(String),
    /// The receiver answered a request with a status other than 200.
    Status { method: String, status: u16 },
    /// The answer could not be read.
    Malformed(String),
    /// AirPlay 2 pairing failed: the receiver refused it, or its proof
    /// was wrong.
    Pairing(String),
}

impl fmt::Display for RtspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RtspError::Io(why) => write!(f, "{why}"),
            RtspError::Status { method, status } => write!(f, "{method} answered {status}"),
            RtspError::Malformed(why) => write!(f, "a malformed answer: {why}"),
            RtspError::Pairing(why) => write!(f, "pairing failed: {why}"),
        }
    }
}

pub struct Request<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(&'a str, String)],
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
}

impl Request<'_> {
    /// The request as it goes on the wire, with `cseq`.
    pub fn write(&self, cseq: u32, out: &mut Vec<u8>) {
        self.write_as("RTSP/1.0", cseq, out);
    }

    /// As `write`, naming the protocol `version`: on the sealed channel of
    /// AirPlay 2 requests go as HTTP/1.1, as PipeWire sends them.
    pub fn write_as(&self, version: &str, cseq: u32, out: &mut Vec<u8>) {
        let mut head = format!("{} {} {version}\r\nCSeq: {cseq}\r\n", self.method, self.url);
        for (name, value) in self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(content_type) = self.content_type {
            head.push_str(&format!(
                "Content-Type: {content_type}\r\nContent-Length: {}\r\n",
                self.body.len()
            ));
        }
        head.push_str("\r\n");
        out.extend_from_slice(head.as_bytes());
        if self.content_type.is_some() {
            out.extend_from_slice(self.body);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// A header's value, its name compared without case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The first response in `buf` and the bytes it took, or None while it is
/// not complete.
pub fn parse(buf: &[u8]) -> Result<Option<(Response, usize)>, RtspError> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Ok(None);
    };
    let head = std::str::from_utf8(&buf[..end])
        .map_err(|_| RtspError::Malformed("a header that is not text".into()))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.split_whitespace();
    if !matches!(parts.next(), Some("RTSP/1.0" | "HTTP/1.1" | "HTTP/1.0")) {
        return Err(RtspError::Malformed(format!("status line {status_line:?}")));
    }
    let status = parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| RtspError::Malformed(format!("status line {status_line:?}")))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect();
    let length = match headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("Content-Length"))
    {
        None => 0,
        Some((_, v)) => v
            .parse::<usize>()
            .map_err(|_| RtspError::Malformed(format!("Content-Length {v:?}")))?,
    };
    if length > MAX_BODY {
        return Err(RtspError::Malformed(format!("a body of {length} bytes")));
    }
    let start = end + 4;
    if buf.len() < start + length {
        return Ok(None);
    }
    let body = buf[start..start + length].to_vec();
    Ok(Some((
        Response {
            status,
            headers,
            body,
        },
        start + length,
    )))
}

/// A connection to a receiver.
pub struct Conn {
    stream: TcpStream,
    cseq: u32,
    /// What has been read and not yet parsed, in the clear.
    buf: Vec<u8>,
    /// Once sealed, AirPlay 2's channel, and the sealed bytes read that do
    /// not yet make a whole frame.
    sealed: Option<(Sealer, Opener)>,
    raw: Vec<u8>,
}

impl Conn {
    /// Connects to `host`, a name or an address, within `timeout` for the
    /// connect and for each read after.
    pub fn connect(host: &str, port: u16, timeout: Duration) -> Result<Conn, RtspError> {
        let io = |e: std::io::Error| RtspError::Io(format!("{host}:{port}: {e}"));
        let addrs: Vec<_> = (host, port).to_socket_addrs().map_err(io)?.collect();
        let mut last = RtspError::Io(format!("{host}: no address"));
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, timeout) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(timeout)).map_err(io)?;
                    stream.set_write_timeout(Some(timeout)).map_err(io)?;
                    let _ = stream.set_nodelay(true);
                    keepalive(&stream);
                    return Ok(Conn {
                        stream,
                        cseq: 0,
                        buf: Vec::new(),
                        sealed: None,
                        raw: Vec::new(),
                    });
                }
                Err(e) => last = io(e),
            }
        }
        Err(last)
    }

    /// From now on every byte either way goes sealed with `keys`.
    pub fn seal(&mut self, keys: &Keys) {
        self.sealed = Some((Sealer::new(&keys.write), Opener::new(&keys.read)));
    }

    /// Sends `req` and waits for its answer.
    pub fn request(&mut self, req: &Request) -> Result<Response, RtspError> {
        self.send(req)?;
        let mut chunk = [0u8; 4096];
        loop {
            if let Some((response, used)) = parse(&self.buf)? {
                self.buf.drain(..used);
                // The answer to a request sent without waiting is passed
                // over.
                let cseq = response.header("CSeq").and_then(|c| c.parse::<u32>().ok());
                if cseq.is_none_or(|c| c == self.cseq) {
                    return Ok(response);
                }
                continue;
            }
            let n = self
                .stream
                .read(&mut chunk)
                .map_err(|e| RtspError::Io(format!("{}: {e}", req.method)))?;
            if n == 0 {
                return Err(RtspError::Io(format!(
                    "{}: the receiver closed",
                    req.method
                )));
            }
            self.take(&chunk[..n])?;
        }
    }

    /// Sends `req` without waiting for its answer, which `closed` drops.
    pub fn send(&mut self, req: &Request) -> Result<(), RtspError> {
        self.cseq += 1;
        let mut out = Vec::new();
        match &mut self.sealed {
            None => req.write(self.cseq, &mut out),
            Some((sealer, _)) => {
                let mut plain = Vec::new();
                req.write_as("HTTP/1.1", self.cseq, &mut plain);
                sealer.seal(&plain, &mut out);
            }
        }
        self.stream
            .write_all(&out)
            .map_err(|e| RtspError::Io(format!("{}: {e}", req.method)))
    }

    /// Takes bytes read from the connection, opening them when sealed.
    fn take(&mut self, bytes: &[u8]) -> Result<(), RtspError> {
        match &mut self.sealed {
            None => self.buf.extend_from_slice(bytes),
            Some((_, opener)) => {
                self.raw.extend_from_slice(bytes);
                opener
                    .open(&mut self.raw, &mut self.buf)
                    .map_err(|e| RtspError::Malformed(e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Whether the receiver has closed the connection or it failed. Reads
    /// and drops anything it sent unasked. Does not wait.
    pub fn closed(&mut self) -> bool {
        if self.stream.set_nonblocking(true).is_err() {
            return true;
        }
        let mut chunk = [0u8; 512];
        let closed = loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => break true,
                // Sealed bytes are opened all the same, so that the count
                // of frames stays in step; a frame that does not open ends
                // the session as a close would.
                Ok(n) => {
                    if self.take(&chunk[..n]).is_err() {
                        break true;
                    }
                    self.buf.clear();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break false,
                Err(_) => break true,
            }
        };
        closed || self.stream.set_nonblocking(false).is_err()
    }

    /// A new limit for each read, such as a short one for the last request.
    pub fn set_timeout(&mut self, timeout: Duration) {
        let _ = self.stream.set_read_timeout(Some(timeout));
    }

    pub fn local_ip(&self) -> IpAddr {
        self.stream
            .local_addr()
            .map_or(IpAddr::from([0, 0, 0, 0]), |a| a.ip())
    }

    pub fn peer_ip(&self) -> IpAddr {
        self.stream
            .peer_addr()
            .map_or(IpAddr::from([0, 0, 0, 0]), |a| a.ip())
    }
}

/// Turns on TCP keepalive with `KEEPALIVE`. A failure leaves the
/// connection as it is: a receiver that closes it is still seen.
fn keepalive(stream: &TcpStream) {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let set = |level, name, value: libc::c_int| {
        // SAFETY: a plain integer option on a socket this function borrows.
        unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                (&raw const value).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, KEEPALIVE[0]);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, KEEPALIVE[1]);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, KEEPALIVE[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn a_request_is_written_as_rtsp() {
        let headers = [("User-Agent", "aas/0.2.0".to_string())];
        let req = Request {
            method: "POST",
            url: "/auth-setup",
            headers: &headers,
            content_type: Some("application/octet-stream"),
            body: &[1, 2],
        };
        let mut out = Vec::new();
        req.write(3, &mut out);
        let mut want = b"POST /auth-setup RTSP/1.0\r\nCSeq: 3\r\nUser-Agent: aas/0.2.0\r\n\
            Content-Type: application/octet-stream\r\nContent-Length: 2\r\n\r\n"
            .to_vec();
        want.extend_from_slice(&[1, 2]);
        assert_eq!(out, want);
        let bare = Request {
            method: "OPTIONS",
            url: "*",
            headers: &[],
            content_type: None,
            body: &[],
        };
        out.clear();
        bare.write(1, &mut out);
        assert_eq!(out, b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n");
    }

    const ANSWER: &[u8] = b"RTSP/1.0 200 OK\r\nCSeq: 4\r\nsession: 1A2B\r\n\
        Content-Length: 3\r\n\r\nabcRTSP/1.0 404";

    #[test]
    fn a_response_is_parsed_across_reads() {
        // The first response is 65 bytes; every shorter prefix waits.
        for end in 0..65 {
            assert_eq!(parse(&ANSWER[..end]).unwrap(), None, "{end}");
        }
        let (r, used) = parse(ANSWER).unwrap().unwrap();
        assert_eq!(used, 65);
        assert_eq!(r.status, 200);
        assert_eq!(r.header("Session"), Some("1A2B"));
        assert_eq!(r.header("cseq"), Some("4"));
        assert_eq!(r.header("Transport"), None);
        assert_eq!(r.body, b"abc");
    }

    #[test]
    fn content_length_over_64k_is_an_error() {
        let text = format!(
            "RTSP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert!(matches!(
            parse(text.as_bytes()),
            Err(RtspError::Malformed(_))
        ));
    }

    #[test]
    fn a_status_line_that_is_neither_rtsp_nor_http_is_an_error() {
        assert!(matches!(
            parse(b"SIP/2.0 200 OK\r\n\r\n"),
            Err(RtspError::Malformed(_))
        ));
        // The sealed channel of AirPlay 2 answers as HTTP.
        let (r, _) = parse(b"HTTP/1.1 200 OK\r\n\r\n").unwrap().unwrap();
        assert_eq!(r.status, 200);
    }

    #[test]
    fn a_sealed_conn_seals_requests_as_http_and_opens_answers() {
        let keys = Keys {
            write: [1; 32],
            read: [2; 32],
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // The receiver's side: it opens with our write key and seals
            // with our read key.
            let (mut opener, mut sealer) = (Opener::new(&[1; 32]), Sealer::new(&[2; 32]));
            let mut raw = Vec::new();
            let mut plain = Vec::new();
            let mut buf = [0u8; 4096];
            while !plain.ends_with(b"\r\n\r\n") {
                let n = s.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                opener.open(&mut raw, &mut plain).unwrap();
            }
            let mut out = Vec::new();
            sealer.seal(
                b"HTTP/1.1 200 OK\r\nCSeq: 1\r\nContent-Length: 2\r\n\r\nok",
                &mut out,
            );
            // In two writes, the second a part of a frame.
            s.write_all(&out[..5]).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            s.write_all(&out[5..]).unwrap();
            // Then an answer nobody waits for, which `closed` must open to
            // stay in step, and one that is waited for.
            out.clear();
            sealer.seal(b"HTTP/1.1 200 OK\r\nCSeq: 2\r\n\r\n", &mut out);
            s.write_all(&out).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            out.clear();
            sealer.seal(b"HTTP/1.1 204 OK\r\nCSeq: 3\r\n\r\n", &mut out);
            s.write_all(&out).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            String::from_utf8(plain).unwrap()
        });
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        conn.seal(&keys);
        let req = |method| Request {
            method,
            url: "/info",
            headers: &[],
            content_type: None,
            body: &[],
        };
        let r = conn.request(&req("GET")).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"ok"[..]));
        conn.send(&req("POST")).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        assert!(!conn.closed());
        assert_eq!(conn.request(&req("GET")).unwrap().status, 204);
        assert!(
            peer.join()
                .unwrap()
                .starts_with("GET /info HTTP/1.1\r\nCSeq: 1\r\n")
        );
    }

    #[test]
    fn conn_sends_and_reads_with_counting_cseq() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            for answer in [
                "RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n",
                "RTSP/1.0 451 x\r\nCSeq: 2\r\n\r\n",
            ] {
                let mut buf = [0u8; 256];
                let n = s.read(&mut buf).unwrap();
                seen.push(String::from_utf8_lossy(&buf[..n]).to_string());
                s.write_all(answer.as_bytes()).unwrap();
            }
            seen
        });
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        let req = |method| Request {
            method,
            url: "*",
            headers: &[],
            content_type: None,
            body: &[],
        };
        assert_eq!(conn.request(&req("OPTIONS")).unwrap().status, 200);
        assert_eq!(conn.request(&req("GET_PARAMETER")).unwrap().status, 451);
        let seen = peer.join().unwrap();
        assert!(seen[0].starts_with("OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n"));
        assert!(seen[1].starts_with("GET_PARAMETER * RTSP/1.0\r\nCSeq: 2\r\n"));
        assert_eq!(conn.local_ip().to_string(), "127.0.0.1");
        assert_eq!(conn.peer_ip().to_string(), "127.0.0.1");
    }

    #[test]
    fn an_answer_to_an_earlier_request_is_passed_over() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let mut seen = 0;
            while seen < 2 {
                let n = s.read(&mut buf).unwrap();
                seen += buf[..n].windows(4).filter(|w| w == b"\r\n\r\n").count();
            }
            // Both answers in one write: the first is to the request sent
            // without waiting.
            s.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\nRTSP/1.0 451 x\r\nCSeq: 2\r\n\r\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        let req = |method| Request {
            method,
            url: "*",
            headers: &[],
            content_type: None,
            body: &[],
        };
        conn.send(&req("POST")).unwrap();
        assert_eq!(conn.request(&req("GET_PARAMETER")).unwrap().status, 451);
        peer.join().unwrap();
    }

    #[test]
    fn closed_tells_a_closed_connection_from_a_quiet_one() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"unasked").unwrap();
            rx.recv().unwrap();
            drop(s);
        });
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!conn.closed());
        tx.send(()).unwrap();
        peer.join().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(conn.closed());
    }

    #[test]
    fn a_connection_probes_a_silent_peer() {
        use std::os::fd::AsRawFd;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _peer = std::thread::spawn(move || listener.accept().map(|(s, _)| s));
        let conn = Conn::connect("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        let get = |level, name| {
            let mut v: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: v and len are valid for the call.
            let r = unsafe {
                libc::getsockopt(
                    conn.stream.as_raw_fd(),
                    level,
                    name,
                    (&raw mut v).cast(),
                    &raw mut len,
                )
            };
            assert_eq!(r, 0);
            v
        };
        assert_eq!(get(libc::SOL_SOCKET, libc::SO_KEEPALIVE), 1);
        assert_eq!(get(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE), KEEPALIVE[0]);
        assert_eq!(get(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL), KEEPALIVE[1]);
        assert_eq!(get(libc::IPPROTO_TCP, libc::TCP_KEEPCNT), KEEPALIVE[2]);
    }

    #[test]
    fn a_closed_connection_is_an_error_not_a_hang() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || drop(listener.accept().unwrap()));
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        peer.join().unwrap();
        let req = Request {
            method: "OPTIONS",
            url: "*",
            headers: &[],
            content_type: None,
            body: &[],
        };
        assert!(matches!(conn.request(&req), Err(RtspError::Io(_))));
    }

    #[test]
    fn a_silent_receiver_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _peer = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(2));
            drop(s);
        });
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_millis(200)).unwrap();
        let req = Request {
            method: "OPTIONS",
            url: "*",
            headers: &[],
            content_type: None,
            body: &[],
        };
        let started = std::time::Instant::now();
        assert!(matches!(conn.request(&req), Err(RtspError::Io(_))));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_name_that_does_not_resolve_is_an_error() {
        let e = Conn::connect("nonexistent.invalid", 7000, Duration::from_secs(1));
        assert!(matches!(e, Err(RtspError::Io(_))));
    }
}
