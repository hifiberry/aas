//! A listener that holds at most `max` connections, and a connection that
//! ends when its peer has sent nothing for `idle`. Without the timeout a
//! cap alone would let a few connections that never close keep everyone
//! else out; without the cap every peer that connects holds memory that the
//! daemon has locked.
//!
//! A connection over the cap is answered with a 503 and closed, rather than
//! left to wait in the backlog for a slot.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, Sleep};

/// What a connection over the cap is sent before it is closed.
const REFUSAL: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
    content-type: application/json\r\n\
    content-length: 33\r\n\
    connection: close\r\n\r\n\
    {\"error\":\"too many connections\"}\n";

/// How long a refused connection is read from, so that its request is
/// taken before the close and the peer sees the 503 rather than a reset.
const REFUSAL_LINGER: Duration = Duration::from_secs(1);

/// A `TcpListener` with a cap on the connections it has open.
pub struct Capped {
    listener: TcpListener,
    slots: Arc<Semaphore>,
    idle: Duration,
}

impl Capped {
    pub fn new(listener: TcpListener, max: usize, idle: Duration) -> Capped {
        let slots = Arc::new(Semaphore::new(max));
        Capped {
            listener,
            slots,
            idle,
        }
    }
}

impl axum::serve::Listener for Capped {
    type Io = Conn;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Conn, SocketAddr) {
        loop {
            let (stream, peer) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    if !is_connection_error(&e) {
                        // As out of file descriptors: wait for some to close.
                        eprintln!("aas: API: accept: {e}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    continue;
                }
            };
            match self.slots.clone().try_acquire_owned() {
                Ok(slot) => return (Conn::new(stream, slot, self.idle), peer),
                Err(_) => {
                    tokio::spawn(refuse(stream));
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

/// Sends the 503 and reads what the peer sends until it closes, for at
/// most `REFUSAL_LINGER`.
async fn refuse(stream: TcpStream) {
    // A fresh socket's send buffer takes the whole of it.
    let _ = stream.try_write(REFUSAL);
    let _ = tokio::time::timeout(REFUSAL_LINGER, async {
        let mut sink = [0u8; 1024];
        loop {
            if stream.readable().await.is_err() {
                return;
            }
            match stream.try_read(&mut sink) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        }
    })
    .await;
}

/// A connection holding one slot, which it gives back when dropped. Once
/// its peer has sent nothing for `idle`, a read or a write that waits
/// fails with `TimedOut`, and the server closes it. A write that waits on a
/// peer that does not read counts as idle too, so a peer that neither
/// sends nor reads cannot hold a slot.
pub struct Conn {
    stream: TcpStream,
    _slot: OwnedSemaphorePermit,
    idle: Duration,
    deadline: Pin<Box<Sleep>>,
}

impl Conn {
    fn new(stream: TcpStream, slot: OwnedSemaphorePermit, idle: Duration) -> Conn {
        let deadline = Box::pin(tokio::time::sleep(idle));
        Conn {
            stream,
            _slot: slot,
            idle,
            deadline,
        }
    }

    /// Ready with `TimedOut` once the deadline has passed. Registers the
    /// waker, so that a task waiting on the stream also wakes for it.
    fn timed_out(&mut self, cx: &mut Context<'_>) -> Poll<io::Error> {
        self.deadline.as_mut().poll(cx).map(|()| {
            let s = self.idle.as_secs_f32();
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("nothing from the peer in {s} s"),
            )
        })
    }
}

impl AsyncRead for Conn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.stream).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if buf.filled().len() > before {
                    let next = Instant::now() + self.idle;
                    self.deadline.as_mut().reset(next);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => self.timed_out(cx).map(Err),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.stream).poll_write(cx, buf) {
            Poll::Pending => self.timed_out(cx).map(Err),
            done => done,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).poll_flush(cx) {
            Poll::Pending => self.timed_out(cx).map(Err),
            done => done,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.stream).poll_write_vectored(cx, bufs) {
            Poll::Pending => self.timed_out(cx).map(Err),
            done => done,
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// A server on a loopback port that keeps each connection open, reading
    /// and dropping what it is sent, until the peer closes or goes idle.
    fn serve(max: usize, idle: Duration) -> SocketAddr {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = TcpListener::from_std(std_listener).unwrap();
                let mut capped = Capped::new(listener, max, idle);
                loop {
                    let (mut conn, _) = axum::serve::Listener::accept(&mut capped).await;
                    tokio::spawn(async move {
                        let mut buf = [0u8; 64];
                        loop {
                            let mut read = ReadBuf::new(&mut buf);
                            let r = std::future::poll_fn(|cx| {
                                Pin::new(&mut conn).poll_read(cx, &mut read)
                            })
                            .await;
                            if r.is_err() || read.filled().is_empty() {
                                return;
                            }
                        }
                    });
                }
            });
        });
        addr
    }

    fn connect(addr: SocketAddr) -> std::net::TcpStream {
        let stream = std::net::TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
    }

    /// Whether the server has closed `stream`, by a read that ends.
    fn closed(stream: &mut std::net::TcpStream) -> bool {
        let mut buf = [0u8; 256];
        matches!(stream.read(&mut buf), Ok(0) | Err(_))
    }

    #[test]
    fn a_connection_over_the_cap_is_503_and_a_closed_one_frees_its_slot() {
        let addr = serve(2, Duration::from_secs(30));
        let first = connect(addr);
        let _second = connect(addr);
        let mut third = connect(addr);
        third.write_all(b"GET /state HTTP/1.1\r\n\r\n").unwrap();
        let mut answer = String::new();
        third.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 503 "), "{answer}");
        let body = answer.split("\r\n\r\n").nth(1).unwrap();
        assert_eq!(body.len(), 33);
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["error"], "too many connections");

        drop(first);
        // The slot comes back once the server has seen the close.
        let mut fourth = None;
        for _ in 0..50 {
            let mut stream = connect(addr);
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut buf = [0u8; 16];
            match stream.read(&mut buf) {
                // Nothing to read: held open, so it has a slot.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    fourth = Some(stream);
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        assert!(fourth.is_some(), "no slot after a close");
    }

    #[test]
    fn a_peer_that_sends_nothing_is_closed_after_the_idle_time() {
        let idle = Duration::from_millis(300);
        let addr = serve(4, idle);
        let mut quiet = connect(addr);
        let started = std::time::Instant::now();
        assert!(closed(&mut quiet));
        assert!(started.elapsed() >= idle);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_peer_that_keeps_sending_stays_open() {
        let idle = Duration::from_millis(300);
        let addr = serve(4, idle);
        let mut busy = connect(addr);
        for _ in 0..8 {
            std::thread::sleep(Duration::from_millis(100));
            busy.write_all(b"x").unwrap();
        }
        // 800 ms in, past the idle time twice over, still open.
        busy.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut buf = [0u8; 1];
        let err = busy.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }
}
