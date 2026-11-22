//! HTTP/1.1 to the daemon on 127.0.0.1: one request on one connection.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// A call the daemon has not answered in 2 s is answered 503; this is more.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// The wait for a pairing step, which the daemon gives 40 s.
pub const PAIR_PATIENCE: Duration = Duration::from_secs(45);

/// Sends one request and gives the status and the body of the answer.
pub fn request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> io::Result<(u16, String)> {
    request_within(port, method, path, body, PATIENCE)
}

/// As `request`, waiting up to `patience` for the answer.
pub fn request_within(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    patience: Duration,
) -> io::Result<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(patience))?;
    stream.set_write_timeout(Some(patience))?;
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer)?;
    let answer = String::from_utf8_lossy(&answer);
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "not an HTTP answer");
    let (head, body) = answer.split_once("\r\n\r\n").ok_or_else(bad)?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(bad)?;
    Ok((status, body.to_string()))
}
