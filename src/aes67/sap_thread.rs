//! The `aas-sap` thread: listens for SAP announcements into the
//! [`Directory`] and announces the stream the output sends.
//!
//! It joins 239.255.255.255:9875 on the interface. Whatever the announce
//! slot holds is announced every 30 s, at once when it changes, and
//! withdrawn with a deletion when it is emptied or the thread stops.

use std::io;
use std::mem;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::directory::Directory;
use super::input::join;
use super::sap::{SapMessage, sap_build, sap_parse};
use super::sdp::{self, Session};

pub const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 255);
pub const PORT: u16 = 9875;
/// Our announcement is repeated this often.
const INTERVAL: Duration = Duration::from_secs(30);
/// The socket returns at least this often, so `stop`, expiry and the
/// announce slot are looked at.
const WAKE: Duration = Duration::from_millis(500);

/// The slot the thread announces from, which the output fills.
pub type Announce = Arc<Mutex<Option<Session>>>;

/// Starts the thread on the interface with address `interface`. It stops
/// within half a second of `stop` being set, withdrawing what it announced.
pub fn start(
    interface: Ipv4Addr,
    directory: Arc<Mutex<Directory>>,
    announce: Announce,
    stop: Arc<AtomicBool>,
) -> io::Result<JoinHandle<()>> {
    let socket = join(GROUP, PORT, interface)?;
    socket.set_read_timeout(Some(WAKE))?;
    socket.set_multicast_loop_v4(true)?;
    multicast_interface(&socket, interface)?;
    thread::Builder::new()
        .name("aas-sap".into())
        .spawn(move || {
            run(&socket, interface, &directory, &announce, &stop);
        })
}

fn run(
    socket: &UdpSocket,
    interface: Ipv4Addr,
    directory: &Mutex<Directory>,
    announce: &Mutex<Option<Session>>,
    stop: &AtomicBool,
) {
    let mut buf = [0u8; 4096];
    let mut sender = Sender::new(interface);
    let mut failing = false;
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((len, _)) => {
                failing = false;
                if let Some(msg) = sap_parse(&buf[..len]) {
                    lock(directory).message(msg, Instant::now());
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => {
                if !failing {
                    eprintln!("aas: aes67: SAP: {e}");
                    failing = true;
                }
                thread::sleep(WAKE);
            }
        }
        let now = Instant::now();
        lock(directory).expire(now);
        let wanted = lock(announce).clone();
        for (msg, ttl) in sender.due(wanted, now) {
            send(socket, &msg, ttl);
        }
    }
    for (msg, ttl) in sender.due(None, Instant::now()) {
        send(socket, &msg, ttl);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sent with the session's own TTL, so it reaches as far as the stream.
fn send(socket: &UdpSocket, msg: &SapMessage, ttl: u8) {
    let _ = socket.set_multicast_ttl_v4(u32::from(ttl));
    if let Err(e) = socket.send_to(&sap_build(msg), (GROUP, PORT)) {
        eprintln!("aas: aes67: SAP: could not send: {e}");
    }
}

/// Sends multicast through the interface, whatever the routes say.
pub(super) fn multicast_interface(socket: &UdpSocket, interface: Ipv4Addr) -> io::Result<()> {
    let addr = libc::in_addr {
        s_addr: u32::from(interface).to_be(),
    };
    // SAFETY: addr is an in_addr of the size passed.
    let r = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            (&raw const addr).cast(),
            mem::size_of::<libc::in_addr>() as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// When our announcement goes out, without the socket.
struct Sender {
    source: Ipv4Addr,
    /// What was last announced, and when.
    sent: Option<(Session, Instant)>,
}

impl Sender {
    fn new(source: Ipv4Addr) -> Sender {
        Sender { source, sent: None }
    }

    /// The messages due at `now` with `wanted` in the slot: an announcement
    /// at once for a new session and every 30 s after, and a deletion of
    /// one that is no longer wanted, each with the TTL to send it at.
    fn due(&mut self, wanted: Option<Session>, now: Instant) -> Vec<(SapMessage, u8)> {
        let mut out = Vec::new();
        if let Some((old, _)) = &self.sent
            && wanted.as_ref() != Some(old)
        {
            let delete = SapMessage::Delete {
                hash: hash(old),
                source: self.source,
            };
            out.push((delete, old.ttl));
            self.sent = None;
        }
        if let Some(session) = wanted {
            let repeat = self
                .sent
                .as_ref()
                .is_none_or(|(_, at)| now.saturating_duration_since(*at) >= INTERVAL);
            if repeat {
                let announce = SapMessage::Announce {
                    hash: hash(&session),
                    source: self.source,
                    sdp: sdp::render(&session),
                };
                out.push((announce, session.ttl));
                self.sent = Some((session, now));
            }
        }
        out
    }
}

/// The message ID hash: it changes with the session's version, as RFC 2974
/// asks of a changed announcement.
fn hash(s: &Session) -> u16 {
    let mixed = s.session_id ^ s.version.rotate_left(16);
    (mixed ^ mixed >> 16 ^ mixed >> 32 ^ mixed >> 48) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aes67::sdp::DANTE_SDP;

    const US: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 20);

    fn session(version: u64) -> Session {
        let mut s = sdp::parse(DANTE_SDP).unwrap();
        s.origin = US;
        s.version = version;
        s
    }

    fn kinds(msgs: &[(SapMessage, u8)]) -> Vec<&'static str> {
        msgs.iter()
            .map(|(m, _)| match m {
                SapMessage::Announce { .. } => "announce",
                SapMessage::Delete { .. } => "delete",
            })
            .collect()
    }

    #[test]
    fn announced_every_30_s_and_deleted_when_the_slot_empties() {
        let t = Instant::now();
        let s = Duration::from_secs;
        let mut sender = Sender::new(US);
        assert!(sender.due(None, t).is_empty());
        let first = sender.due(Some(session(1)), t);
        assert_eq!(kinds(&first), ["announce"]);
        let (SapMessage::Announce { source, sdp, .. }, ttl) = &first[0] else {
            unreachable!()
        };
        assert_eq!(*ttl, session(1).ttl);
        assert_eq!(*source, US);
        assert_eq!(sdp::parse(sdp).unwrap(), session(1));
        assert!(sender.due(Some(session(1)), t + s(29)).is_empty());
        assert_eq!(
            kinds(&sender.due(Some(session(1)), t + s(30))),
            ["announce"]
        );
        let gone = sender.due(None, t + s(31));
        assert_eq!(
            gone,
            [(
                SapMessage::Delete {
                    hash: hash(&session(1)),
                    source: US
                },
                session(1).ttl
            )]
        );
        assert!(sender.due(None, t + s(40)).is_empty());
    }

    #[test]
    fn a_changed_session_withdraws_the_old_and_announces_at_once() {
        let t = Instant::now();
        let mut sender = Sender::new(US);
        sender.due(Some(session(1)), t);
        let msgs = sender.due(Some(session(2)), t + Duration::from_secs(1));
        assert_eq!(kinds(&msgs), ["delete", "announce"]);
        assert_ne!(hash(&session(1)), hash(&session(2)));
    }
}
