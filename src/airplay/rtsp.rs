//! A minimal RTSP client for AirPlay: requests written as text, responses
//! parsed as they arrive, over a blocking TCP connection with a timeout on
//! every read, on the output's own thread.

use super::ap2::channel::{Keys, Opener, Sealer};
use std::collections::VecDeque;
use std::fmt;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
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

/// The most bytes a flush takes from the queue, so that a large cover is
/// encoded and written a piece at a time, between packets.
pub const FLUSH_BUDGET: usize = 64 * 1024;

/// The bytes encoded and sealed at a time.
const PIECE: usize = 16 * 1024;

/// Requests sent without waiting that are remembered for a refusal.
const ASKED: usize = 64;

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
    /// Requests queued and not yet started, in order.
    queue: VecDeque<Queued>,
    /// The request being written, encoded in the clear, and how much of it
    /// has gone to `wire`.
    current: Vec<u8>,
    current_at: usize,
    /// Bytes on their way to the socket, sealed when the channel is, and
    /// how many of them are written.
    wire: Vec<u8>,
    wire_at: usize,
    /// What each request sent without waiting was, by its CSeq, so that a
    /// refusal can be named.
    asked: VecDeque<(u32, String)>,
    /// The first refusal not yet told, and whether one was told.
    refusal: Option<String>,
    refusal_told: bool,
}

/// A request waiting to go out, owned.
struct Queued {
    /// A newer request with the same key replaces this one.
    key: Option<&'static str>,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    content_type: Option<String>,
    body: Vec<u8>,
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
                        queue: VecDeque::new(),
                        current: Vec::new(),
                        current_at: 0,
                        wire: Vec::new(),
                        wire_at: 0,
                        asked: VecDeque::new(),
                        refusal: None,
                        refusal_told: false,
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

    /// Sends `req` and waits for its answer. Queued requests not yet
    /// started are dropped; one under way is finished first, so that the
    /// stream stays whole.
    pub fn request(&mut self, req: &Request) -> Result<Response, RtspError> {
        let io = |e: std::io::Error| RtspError::Io(format!("{}: {e}", req.method));
        self.queue.clear();
        loop {
            if self.wire_at < self.wire.len() {
                self.stream
                    .write_all(&self.wire[self.wire_at..])
                    .map_err(io)?;
                self.wire.clear();
                self.wire_at = 0;
            }
            if self.current_at == self.current.len() {
                break;
            }
            self.next_piece(usize::MAX);
        }
        self.current.clear();
        self.current_at = 0;
        let mut plain = Vec::new();
        self.encode(req, &mut plain, None);
        let mut out = Vec::new();
        self.seal_into(&plain, &mut out);
        self.stream.write_all(&out).map_err(io)?;
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
                self.note(&response);
                continue;
            }
            let n = self.stream.read(&mut chunk).map_err(io)?;
            if n == 0 {
                return Err(RtspError::Io(format!(
                    "{}: the receiver closed",
                    req.method
                )));
            }
            self.take(&chunk[..n])?;
        }
    }

    /// Queues `req`, written as the socket takes it, without waiting for
    /// its answer. A request with the same `key` that has not started is
    /// replaced, so a receiver that reads slowly gets only the newest.
    pub fn queue(&mut self, key: Option<&'static str>, req: &Request) -> Result<(), RtspError> {
        let queued = Queued {
            key,
            method: req.method.to_string(),
            url: req.url.to_string(),
            headers: req
                .headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.clone()))
                .collect(),
            content_type: req.content_type.map(String::from),
            body: req.body.to_vec(),
        };
        match self
            .queue
            .iter_mut()
            .find(|q| key.is_some() && q.key == key)
        {
            Some(waiting) => *waiting = queued,
            None => self.queue.push_back(queued),
        }
        self.flush()
    }

    /// Sends `req` without waiting for its answer, behind the queue.
    pub fn send(&mut self, req: &Request) -> Result<(), RtspError> {
        self.queue(None, req)
    }

    /// Writes what the socket takes of the queue now, without blocking,
    /// and at most `FLUSH_BUDGET` more of it.
    pub fn flush(&mut self) -> Result<(), RtspError> {
        let mut budget = FLUSH_BUDGET;
        loop {
            while self.wire_at < self.wire.len() {
                let rest = &self.wire[self.wire_at..];
                // SAFETY: the pointer and length are those of `rest`, and the
                // fd is the open stream's.
                let n = unsafe {
                    libc::send(
                        self.stream.as_raw_fd(),
                        rest.as_ptr().cast(),
                        rest.len(),
                        libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                    )
                };
                if n < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        return Ok(());
                    }
                    return Err(RtspError::Io(format!("a queued request: {e}")));
                }
                self.wire_at += n as usize;
            }
            self.wire.clear();
            self.wire_at = 0;
            if budget == 0 {
                return Ok(());
            }
            let moved = self.next_piece(budget);
            if moved == 0 {
                return Ok(());
            }
            budget -= moved;
        }
    }

    /// Moves up to `most` bytes of the current request, or of the next
    /// one, to `wire`, sealed when the channel is. The bytes moved.
    fn next_piece(&mut self, most: usize) -> usize {
        if self.current_at == self.current.len() {
            self.current.clear();
            self.current_at = 0;
            let Some(q) = self.queue.pop_front() else {
                return 0;
            };
            let headers: Vec<(&str, String)> = q
                .headers
                .iter()
                .map(|(n, v)| (n.as_str(), v.clone()))
                .collect();
            let req = Request {
                method: &q.method,
                url: &q.url,
                headers: &headers,
                content_type: q.content_type.as_deref(),
                body: &q.body,
            };
            let mut plain = std::mem::take(&mut self.current);
            let what = match &q.content_type {
                Some(t) => format!("{} {t}", q.method),
                None => q.method.clone(),
            };
            self.encode(&req, &mut plain, Some(what));
            self.current = plain;
        }
        let n = (self.current.len() - self.current_at).min(most).min(PIECE);
        let piece = self.current[self.current_at..self.current_at + n].to_vec();
        let mut wire = std::mem::take(&mut self.wire);
        self.seal_into(&piece, &mut wire);
        self.wire = wire;
        self.current_at += n;
        n
    }

    /// The first answer of an error to a request sent without waiting, as
    /// its method and type and the status, told once a connection.
    pub fn refused(&mut self) -> Option<String> {
        let r = self.refusal.take();
        if r.is_some() {
            self.refusal_told = true;
        }
        r
    }

    /// Bytes queued and not yet written.
    pub fn pending(&self) -> usize {
        self.queue.iter().map(|q| q.body.len() + 64).sum::<usize>()
            + (self.current.len() - self.current_at)
            + (self.wire.len() - self.wire_at)
    }

    /// `req` in the clear with the next CSeq, and what it was for a later
    /// refusal when it is not waited for.
    fn encode(&mut self, req: &Request, plain: &mut Vec<u8>, what: Option<String>) {
        self.cseq += 1;
        let version = if self.sealed.is_some() {
            "HTTP/1.1"
        } else {
            "RTSP/1.0"
        };
        req.write_as(version, self.cseq, plain);
        if let Some(what) = what {
            self.asked.push_back((self.cseq, what));
            if self.asked.len() > ASKED {
                self.asked.pop_front();
            }
        }
    }

    /// `plain` as it goes on the wire: sealed once the channel is.
    fn seal_into(&mut self, plain: &[u8], out: &mut Vec<u8>) {
        match &mut self.sealed {
            None => out.extend_from_slice(plain),
            Some((sealer, _)) => sealer.seal(plain, out),
        }
    }

    /// Keeps the first refusal of a request sent without waiting.
    fn note(&mut self, answer: &Response) {
        let cseq = answer.header("CSeq").and_then(|c| c.parse::<u32>().ok());
        let what = cseq.and_then(|c| {
            let at = self.asked.iter().position(|(n, _)| *n == c)?;
            self.asked.remove(at).map(|(_, w)| w)
        });
        if answer.status >= 300 && !self.refusal_told && self.refusal.is_none() {
            let what = what.unwrap_or_else(|| "a request".into());
            self.refusal = Some(format!("{what}: {}", answer.status));
        }
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
                    // Whole answers nobody waits for are dropped; a part
                    // of one stays for the rest to join.
                    loop {
                        match parse(&self.buf) {
                            Ok(Some((answer, used))) => {
                                self.buf.drain(..used);
                                self.note(&answer);
                            }
                            Ok(None) => break,
                            Err(_) => {
                                self.buf.clear();
                                break;
                            }
                        }
                    }
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

    fn set_parameter(body: &[u8]) -> Request<'_> {
        Request {
            method: "SET_PARAMETER",
            url: "*",
            headers: &[],
            content_type: Some("image/jpeg"),
            body,
        }
    }

    #[test]
    fn a_queued_request_the_peer_does_not_read_is_not_waited_for() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(2));
            drop(s);
        });
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        let big = vec![7u8; 8 << 20];
        let started = std::time::Instant::now();
        conn.queue(None, &set_parameter(&big)).unwrap();
        conn.flush().unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(conn.pending() > 0);
        peer.join().unwrap();
    }

    #[test]
    fn queued_requests_go_out_in_order_as_the_peer_reads() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut chunk = [0u8; 65536];
            while !got.ends_with(b"B") {
                let n = s.read(&mut chunk).unwrap();
                assert!(n > 0, "closed early");
                got.extend_from_slice(&chunk[..n]);
            }
            got
        });
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        let big = vec![b'A'; 2 << 20];
        conn.queue(None, &set_parameter(&big)).unwrap();
        conn.queue(None, &set_parameter(b"B")).unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while conn.pending() > 0 && std::time::Instant::now() < end {
            conn.flush().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(conn.pending(), 0);
        let got = peer.join().unwrap();
        let second = got.windows(7).position(|w| w == b"CSeq: 2").unwrap();
        assert!(got[..second].iter().filter(|&&b| b == b'A').count() >= 2 << 20);
    }

    /// A peer that reads nothing for `wait`, then everything until it has
    /// seen `until`, and gives what it read.
    fn slow_peer(
        listener: TcpListener,
        wait: Duration,
        until: &'static [u8],
    ) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            std::thread::sleep(wait);
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut got = Vec::new();
            let mut chunk = [0u8; 65536];
            while !got.windows(until.len()).any(|w| w == until) {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&chunk[..n]),
                }
            }
            got
        })
    }

    fn drain(conn: &mut Conn) {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while conn.pending() > 0 && std::time::Instant::now() < end {
            conn.flush().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_send_behind_a_queue_the_peer_does_not_read_does_not_wait() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = slow_peer(listener, Duration::from_millis(1500), b"/feedback");
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(5)).unwrap();
        let big = vec![7u8; 8 << 20];
        conn.queue(Some("cover"), &set_parameter(&big)).unwrap();
        let feedback = Request {
            method: "POST",
            url: "/feedback",
            headers: &[],
            content_type: None,
            body: &[],
        };
        let started = std::time::Instant::now();
        conn.send(&feedback).unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "{:?}",
            started.elapsed()
        );
        drain(&mut conn);
        let got = peer.join().unwrap();
        assert!(got.windows(9).any(|w| w == b"/feedback"));
    }

    #[test]
    fn a_newer_request_of_a_kind_replaces_one_not_started() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = slow_peer(listener, Duration::from_millis(500), b"second");
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(5)).unwrap();
        let big = vec![7u8; 8 << 20];
        conn.queue(Some("cover"), &set_parameter(&big)).unwrap();
        conn.queue(Some("text"), &set_parameter(b"first")).unwrap();
        conn.queue(Some("text"), &set_parameter(b"second")).unwrap();
        drain(&mut conn);
        let got = peer.join().unwrap();
        assert!(!got.windows(5).any(|w| w == b"first"));
        assert!(got.windows(6).any(|w| w == b"second"));
    }

    #[test]
    fn a_flush_moves_at_most_its_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _peer = slow_peer(listener, Duration::from_millis(0), b"never");
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(5)).unwrap();
        let big = vec![7u8; 1 << 20];
        conn.queue(Some("cover"), &set_parameter(&big)).unwrap();
        let before = conn.pending();
        conn.flush().unwrap();
        assert!(
            before - conn.pending() <= 2 * FLUSH_BUDGET,
            "{before} {}",
            conn.pending()
        );
        assert!(conn.pending() > 0);
    }

    #[test]
    fn a_request_drops_what_has_not_started_and_finishes_what_has() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            let mut chunk = [0u8; 65536];
            while !got.windows(7).any(|w| w == b"OPTIONS") || !got.ends_with(b"\r\n\r\n") {
                let n = s.read(&mut chunk).unwrap();
                got.extend_from_slice(&chunk[..n]);
            }
            let cseq = if got.windows(7).any(|w| w == b"CSeq: 2") {
                2
            } else {
                3
            };
            s.write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n").as_bytes())
                .unwrap();
            got
        });
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(5)).unwrap();
        let big = vec![b'A'; 2 << 20];
        conn.queue(Some("cover"), &set_parameter(&big)).unwrap();
        conn.queue(Some("text"), &set_parameter(b"dropped"))
            .unwrap();
        let options = Request {
            method: "OPTIONS",
            url: "*",
            headers: &[],
            content_type: None,
            body: &[],
        };
        assert_eq!(conn.request(&options).unwrap().status, 200);
        assert_eq!(conn.pending(), 0);
        let got = peer.join().unwrap();
        assert!(got.iter().filter(|&&b| b == b'A').count() >= 2 << 20);
        assert!(!got.windows(7).any(|w| w == b"dropped"));
    }

    #[test]
    fn a_refused_queued_request_is_told_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).unwrap();
            s.write_all(b"RTSP/1.0 400 Bad Request\r\nCSeq: 1\r\n\r\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(5)).unwrap();
        conn.queue(Some("text"), &set_parameter(b"x")).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!conn.closed());
        assert_eq!(
            conn.refused().as_deref(),
            Some("SET_PARAMETER image/jpeg: 400")
        );
        assert_eq!(conn.refused(), None);
        peer.join().unwrap();
    }

    #[test]
    fn queued_requests_on_a_sealed_channel_open_in_order() {
        let keys = Keys {
            write: [1; 32],
            read: [2; 32],
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut opener = Opener::new(&[1; 32]);
            let (mut raw, mut plain) = (Vec::new(), Vec::new());
            let mut buf = [0u8; 65536];
            while !plain.ends_with(b"tail") {
                let n = s.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                opener.open(&mut raw, &mut plain).unwrap();
            }
            plain
        });
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(5)).unwrap();
        conn.seal(&keys);
        let big = vec![b'A'; 300_000];
        conn.queue(Some("cover"), &set_parameter(&big)).unwrap();
        conn.queue(Some("text"), &set_parameter(b"tail")).unwrap();
        drain(&mut conn);
        let plain = peer.join().unwrap();
        let first = plain.windows(7).position(|w| w == b"CSeq: 1").unwrap();
        let second = plain.windows(7).position(|w| w == b"CSeq: 2").unwrap();
        assert!(first < second);
        assert!(plain.starts_with(b"SET_PARAMETER * HTTP/1.1"));
    }

    #[test]
    fn a_request_writes_the_queue_first() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            let mut chunk = [0u8; 65536];
            while !got.windows(7).any(|w| w == b"CSeq: 2") || !got.ends_with(b"\r\n\r\n") {
                let n = s.read(&mut chunk).unwrap();
                got.extend_from_slice(&chunk[..n]);
            }
            s.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\nRTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n")
                .unwrap();
            got
        });
        let mut conn = Conn::connect("127.0.0.1", port, Duration::from_secs(5)).unwrap();
        let big = vec![b'A'; 2 << 20];
        conn.queue(None, &set_parameter(&big)).unwrap();
        let options = Request {
            method: "OPTIONS",
            url: "*",
            headers: &[],
            content_type: None,
            body: &[],
        };
        assert_eq!(conn.request(&options).unwrap().status, 200);
        assert_eq!(conn.pending(), 0);
        let got = peer.join().unwrap();
        assert!(got.windows(7).position(|w| w == b"OPTIONS").unwrap() > 2 << 20);
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
    fn closed_keeps_a_partly_received_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf).unwrap();
            // The answer to the request sent without waiting, its body
            // after a look by `closed`.
            s.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 2\r\n\r\n")
                .unwrap();
            rx.recv().unwrap();
            s.write_all(b"ok").unwrap();
            let _ = s.read(&mut buf).unwrap();
            s.write_all(b"RTSP/1.0 204 x\r\nCSeq: 2\r\n\r\n").unwrap();
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
        std::thread::sleep(Duration::from_millis(50));
        assert!(!conn.closed());
        tx.send(()).unwrap();
        assert_eq!(conn.request(&req("GET_PARAMETER")).unwrap().status, 204);
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
