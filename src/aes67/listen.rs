//! The `aas-ptp` thread: a receive-only PTP listener.
//!
//! It reads the PTP traffic to 224.0.1.129 through an `AF_PACKET` socket,
//! which needs CAP_NET_RAW, and leaves the PTP ports to whoever binds them
//! (`nqptp` does). A plain UDP socket joins the group, so the switch
//! forwards it, and is never written to. The kernel timestamps each packet
//! (`SO_TIMESTAMPNS`). Nothing here sends a packet.

use std::ffi::{CStr, CString};
use std::io;
use std::mem;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use libc::{
    BPF_ABS, BPF_ALU, BPF_AND, BPF_B, BPF_H, BPF_IND, BPF_JEQ, BPF_JMP, BPF_JSET, BPF_K, BPF_LD,
    BPF_LDX, BPF_MSH, BPF_RET, BPF_W, sock_filter,
};

use super::clock::{Estimator, PtpClock, realtime_ns};

const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);
const EVENT_PORT: u16 = 319;
const GENERAL_PORT: u16 = 320;
const UDP: u8 = 17;
/// `recvmsg` returns at least this often, so `stop` and loss are noticed.
const RECV_TIMEOUT_S: libc::time_t = 1;
const TICK_NS: i128 = 1_000_000_000;

const fn stmt(code: u32, k: u32) -> sock_filter {
    sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    }
}

const fn jump(code: u32, k: u32, jt: u8, jf: u8) -> sock_filter {
    sock_filter {
        code: code as u16,
        jt,
        jf,
        k,
    }
}

/// Classic BPF, run by the kernel on each packet before it is queued. On a
/// `SOCK_DGRAM` packet socket offset 0 is the IPv4 header. Accepts IPv4,
/// UDP, to 224.0.1.129, not a fragment, destination port 319 or 320.
/// Jump offsets count from the next instruction; `parse_ip` checks all of
/// this again, and the lengths.
static FILTER: [sock_filter; 15] = [
    // 0: A = version and header length; version 4, else to 14.
    stmt(BPF_LD | BPF_B | BPF_ABS, 0),
    stmt(BPF_ALU | BPF_AND | BPF_K, 0xf0),
    jump(BPF_JMP | BPF_JEQ | BPF_K, 0x40, 0, 11),
    // 3: protocol UDP, else to 14.
    stmt(BPF_LD | BPF_B | BPF_ABS, 9),
    jump(BPF_JMP | BPF_JEQ | BPF_K, UDP as u32, 0, 9),
    // 5: destination 224.0.1.129, else to 14.
    stmt(BPF_LD | BPF_W | BPF_ABS, 16),
    jump(BPF_JMP | BPF_JEQ | BPF_K, 0xe000_0181, 0, 7),
    // 7: more fragments or a fragment offset: to 14.
    stmt(BPF_LD | BPF_H | BPF_ABS, 6),
    jump(BPF_JMP | BPF_JSET | BPF_K, 0x3fff, 5, 0),
    // 9: X = 4 * IHL, the UDP header; A = its destination port.
    stmt(BPF_LDX | BPF_B | BPF_MSH, 0),
    stmt(BPF_LD | BPF_H | BPF_IND, 2),
    // 11: port 319 or 320 to 13, else to 14.
    jump(BPF_JMP | BPF_JEQ | BPF_K, EVENT_PORT as u32, 1, 0),
    jump(BPF_JMP | BPF_JEQ | BPF_K, GENERAL_PORT as u32, 0, 1),
    // 13: accept the whole packet.
    stmt(BPF_RET | BPF_K, u32::MAX),
    // 14: drop it.
    stmt(BPF_RET | BPF_K, 0),
];

/// Starts the `aas-ptp` thread on `interface`, following the best master on
/// `domain`. It stops within a second of `stop` being set.
///
/// The socket needs CAP_NET_RAW. Without it this returns the error, and the
/// caller logs once
/// `aas: aes67: no PTP listener: <err> (needs CAP_NET_RAW)`
/// and goes on with `PtpClock::off()`.
pub fn start(
    interface: &str,
    domain: u8,
    stop: Arc<AtomicBool>,
) -> io::Result<(PtpClock, JoinHandle<()>)> {
    let addr = interface_ipv4(interface)?;
    let name =
        CString::new(interface).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: name is a valid NUL-terminated string.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = packet_socket(index)?;
    let group = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    group.join_multicast_v4(&GROUP, &addr)?;
    let clock = PtpClock::off();
    let published = clock.clone();
    let thread = thread::Builder::new()
        .name("aas-ptp".into())
        .spawn(move || {
            // The membership lasts as long as the thread.
            let _group = group;
            run(&socket, Estimator::new(domain), &published, &stop);
        })?;
    Ok((clock, thread))
}

fn run(socket: &OwnedFd, mut est: Estimator, clock: &PtpClock, stop: &AtomicBool) {
    let mut buf = [0u8; 2048];
    let mut failing = false;
    let mut last_tick = realtime_ns();
    while !stop.load(Ordering::Relaxed) {
        match receive(socket, &mut buf) {
            Ok(Some((len, rx_ns))) => {
                failing = false;
                if let Some((from, port, payload)) = parse_ip(&buf[..len]) {
                    est.packet(from, port, payload, rx_ns);
                    clock.publish(&est);
                }
            }
            Ok(None) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => {
                if !failing {
                    eprintln!("aas: aes67: PTP listener: {e}");
                    failing = true;
                }
                thread::sleep(Duration::from_secs(1));
            }
        }
        // Packets of other domains or versions do not tick the estimator.
        let now = realtime_ns();
        if (now - last_tick).abs() >= TICK_NS {
            est.tick(now);
            clock.publish(&est);
            last_tick = now;
        }
    }
}

/// The first IPv4 address of `interface`.
fn interface_ipv4(interface: &str) -> io::Result<Ipv4Addr> {
    let mut list: *mut libc::ifaddrs = ptr::null_mut();
    // SAFETY: list is a valid out pointer; it is freed below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut found = None;
    let mut cur = list;
    while !cur.is_null() {
        // SAFETY: cur is an entry of the list getifaddrs returned; its name is
        // a C string and its address, when set, starts with sa_family.
        let ifa = unsafe { &*cur };
        let ipv4 =
            !ifa.ifa_addr.is_null() && unsafe { (*ifa.ifa_addr).sa_family } as i32 == libc::AF_INET;
        if ipv4 && unsafe { CStr::from_ptr(ifa.ifa_name) }.to_bytes() == interface.as_bytes() {
            // SAFETY: an AF_INET address is a sockaddr_in.
            let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
            found = Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)));
            break;
        }
        cur = ifa.ifa_next;
    }
    // SAFETY: list came from getifaddrs and is not used after this.
    unsafe { libc::freeifaddrs(list) };
    found.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{interface} has no IPv4 address"),
        )
    })
}

fn set_option<T>(fd: &OwnedFd, name: libc::c_int, value: &T) -> io::Result<()> {
    // SAFETY: value points to a T of the size passed.
    let r = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            name,
            value as *const T as *const libc::c_void,
            mem::size_of::<T>() as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// An `AF_PACKET` socket for IPv4 on interface `index`, filtered by
/// `FILTER`, timestamping, with a receive timeout.
fn packet_socket(index: u32) -> io::Result<OwnedFd> {
    // Protocol 0 receives nothing until the bind below, after the filter.
    // SAFETY: plain socket call; the fd is owned from here on.
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a new, valid descriptor nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let prog = libc::sock_fprog {
        len: FILTER.len() as u16,
        filter: FILTER.as_ptr() as *mut sock_filter,
    };
    set_option(&fd, libc::SO_ATTACH_FILTER, &prog)?;
    set_option(&fd, libc::SO_TIMESTAMPNS, &1 as &libc::c_int)?;
    let timeout = libc::timeval {
        tv_sec: RECV_TIMEOUT_S,
        tv_usec: 0,
    };
    set_option(&fd, libc::SO_RCVTIMEO, &timeout)?;
    // SAFETY: an all-zero sockaddr_ll is valid.
    let mut sll: libc::sockaddr_ll = unsafe { mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_protocol = (libc::ETH_P_IP as u16).to_be();
    sll.sll_ifindex = index as i32;
    // SAFETY: sll is a sockaddr_ll of the size passed.
    let r = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &sll as *const libc::sockaddr_ll as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// One packet and its kernel receive time (CLOCK_REALTIME ns). `None` for a
/// packet this host sent, or one larger than `buf`.
fn receive(socket: &OwnedFd, buf: &mut [u8]) -> io::Result<Option<(usize, i128)>> {
    // u64 keeps the control buffer aligned for cmsghdr.
    let mut control = [0u64; 16];
    // SAFETY: all-zero sockaddr_ll and msghdr are valid.
    let mut from: libc::sockaddr_ll = unsafe { mem::zeroed() };
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_name = &mut from as *mut libc::sockaddr_ll as *mut libc::c_void;
    msg.msg_namelen = mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = mem::size_of_val(&control) as _;
    // SAFETY: msg points to live buffers of the sizes it states.
    let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if from.sll_pkttype == libc::PACKET_OUTGOING || msg.msg_flags & libc::MSG_TRUNC != 0 {
        return Ok(None);
    }
    let mut rx_ns = None;
    // SAFETY: the CMSG macros walk the control buffer recvmsg filled in.
    let mut c = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !c.is_null() {
        let h = unsafe { &*c };
        if h.cmsg_level == libc::SOL_SOCKET && h.cmsg_type == libc::SCM_TIMESTAMPNS {
            // SAFETY: an SCM_TIMESTAMPNS message carries a timespec.
            let ts: libc::timespec =
                unsafe { ptr::read_unaligned(libc::CMSG_DATA(c) as *const libc::timespec) };
            rx_ns = Some(ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128);
        }
        c = unsafe { libc::CMSG_NXTHDR(&msg, c) };
    }
    Ok(Some((n as usize, rx_ns.unwrap_or_else(realtime_ns))))
}

/// Source address, UDP destination port and payload of a PTP packet to the
/// group, from its IPv4 header on. Checks everything the filter does, and
/// that the lengths hold.
fn parse_ip(packet: &[u8]) -> Option<(Ipv4Addr, u16, &[u8])> {
    let first = *packet.first()?;
    let ihl = (first & 0x0f) as usize * 4;
    if first >> 4 != 4 || ihl < 20 || packet.len() < ihl + 8 {
        return None;
    }
    let total = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    let fragment = u16::from_be_bytes([packet[6], packet[7]]) & 0x3fff != 0;
    let dst = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);
    if total > packet.len() || total < ihl + 8 || fragment || packet[9] != UDP || dst != GROUP {
        return None;
    }
    let udp = &packet[ihl..total];
    let port = u16::from_be_bytes([udp[2], udp[3]]);
    let len = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    if !(port == EVENT_PORT || port == GENERAL_PORT) || len < 8 || len > udp.len() {
        return None;
    }
    let src = Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]);
    Some((src, port, &udp[8..len]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aes67::ptp::ClockId;

    const GM: ClockId = [0x00, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55];
    const CAPTURE: &str = include_str!("../../tests/fixtures/aes67/ptp-capture.txt");
    const L0: i128 = 1_770_000_000_000_000_000;

    /// (receive time from the first packet, source, port, payload).
    fn capture() -> Vec<(i128, Ipv4Addr, u16, Vec<u8>)> {
        CAPTURE
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| {
                let f: Vec<&str> = l.split(' ').collect();
                let bytes = (0..f[3].len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&f[3][i..i + 2], 16).unwrap())
                    .collect();
                (
                    f[0].parse().unwrap(),
                    f[1].parse().unwrap(),
                    f[2].parse().unwrap(),
                    bytes,
                )
            })
            .collect()
    }

    #[test]
    fn a_recorded_capture_locks() {
        let mut est = Estimator::new(0);
        let packets = capture();
        for (t, from, port, payload) in &packets {
            est.packet(*from, *port, payload, L0 + t);
        }
        let last = L0 + packets.last().unwrap().0;
        let now = est.now(last).expect("locked");
        assert_eq!(now.gm, GM);
        assert!((now.rate - 1.0).abs() < 100e-6, "rate {}", now.rate);
        let stats = est.stats();
        assert_eq!(stats.gm_addr, Some(Ipv4Addr::new(192, 0, 2, 1)));
        // Every PTPv2 Sync pairs; the PTPv1 traffic on the same ports is ignored.
        assert_eq!((stats.pairs, stats.unmatched_syncs), (20, 0));
    }

    /// An IPv4/UDP packet to 224.0.1.129 from 192.0.2.1 carrying `payload`.
    fn ip_packet(port: u16, payload: &[u8]) -> Vec<u8> {
        let total = 20 + 8 + payload.len();
        let mut p = vec![0x45, 0, 0, 0, 0, 0, 0x40, 0, 1, 17, 0, 0];
        p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
        p.extend_from_slice(&[192, 0, 2, 1, 224, 0, 1, 129]);
        p.extend_from_slice(&56521u16.to_be_bytes());
        p.extend_from_slice(&port.to_be_bytes());
        p.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        p.extend_from_slice(&[0, 0]);
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn the_ip_parse_takes_ptp_to_the_group_only() {
        let payload = [0x08, 0x02, 0, 44];
        let p = ip_packet(320, &payload);
        assert_eq!(
            parse_ip(&p),
            Some((Ipv4Addr::new(192, 0, 2, 1), 320, &payload[..]))
        );
        assert_eq!(parse_ip(&ip_packet(319, &payload)).unwrap().1, 319);
        // Trailing Ethernet padding is not payload.
        let mut padded = p.clone();
        padded.extend_from_slice(&[0; 6]);
        assert_eq!(parse_ip(&padded).unwrap().2, &payload[..]);
        // IP options move the UDP header.
        let mut opts = p.clone();
        opts[0] = 0x46;
        opts.splice(20..20, [1, 1, 1, 1]);
        let total = opts.len() as u16;
        opts[2..4].copy_from_slice(&total.to_be_bytes());
        assert_eq!(parse_ip(&opts).unwrap().2, &payload[..]);
    }

    #[test]
    fn the_ip_parse_rejects_everything_else() {
        let good = ip_packet(320, &[0x08, 0x02, 0, 44]);
        type Change = fn(&mut Vec<u8>);
        let bad: [(&str, Change); 11] = [
            ("IPv6", |p| p[0] = 0x65),
            ("short header", |p| p[0] = 0x44),
            ("TCP", |p| p[9] = 6),
            ("other group", |p| p[19] = 130),
            ("more fragments", |p| p[6] = 0x20),
            ("fragment offset", |p| p[7] = 1),
            ("port 9875", |p| {
                p[22..24].copy_from_slice(&9875u16.to_be_bytes())
            }),
            ("total length too long", |p| p[3] += 1),
            ("UDP length too long", |p| p[25] += 1),
            ("UDP length too short", |p| p[25] = 7),
            ("truncated", |p| p.truncate(22)),
        ];
        for (what, change) in bad {
            let mut p = good.clone();
            change(&mut p);
            assert_eq!(parse_ip(&p), None, "{what}");
            assert_eq!(
                bpf_run(&FILTER, &p) != 0,
                accepted_by_filter(what),
                "{what}"
            );
        }
        assert_ne!(bpf_run(&FILTER, &good), 0);
        assert_ne!(bpf_run(&FILTER, &ip_packet(319, &[0])), 0);
    }

    /// Classic BPF, the instructions `FILTER` uses, as the kernel runs them:
    /// a load past the end rejects the packet.
    fn bpf_run(prog: &[libc::sock_filter], p: &[u8]) -> u32 {
        let (mut a, mut x, mut pc) = (0u32, 0u32, 0usize);
        let load = |off: u32, n: usize| -> Option<u32> {
            let off = off as usize;
            let b = p.get(off..off + n)?;
            Some(b.iter().fold(0, |v, &b| v << 8 | b as u32))
        };
        loop {
            let i = prog[pc];
            pc += 1;
            let code = i.code as u32;
            match code {
                c if c == BPF_LD | BPF_B | BPF_ABS => match load(i.k, 1) {
                    Some(v) => a = v,
                    None => return 0,
                },
                c if c == BPF_LD | BPF_H | BPF_ABS => match load(i.k, 2) {
                    Some(v) => a = v,
                    None => return 0,
                },
                c if c == BPF_LD | BPF_W | BPF_ABS => match load(i.k, 4) {
                    Some(v) => a = v,
                    None => return 0,
                },
                c if c == BPF_LD | BPF_H | BPF_IND => match load(x + i.k, 2) {
                    Some(v) => a = v,
                    None => return 0,
                },
                c if c == BPF_LDX | BPF_B | BPF_MSH => match load(i.k, 1) {
                    Some(v) => x = 4 * (v & 0xf),
                    None => return 0,
                },
                c if c == BPF_ALU | BPF_AND | BPF_K => a &= i.k,
                c if c == BPF_JMP | BPF_JEQ | BPF_K || c == BPF_JMP | BPF_JSET | BPF_K => {
                    let hit = if c & BPF_JSET != 0 {
                        a & i.k != 0
                    } else {
                        a == i.k
                    };
                    pc += if hit { i.jt } else { i.jf } as usize;
                }
                c if c == BPF_RET | BPF_K => return i.k,
                c => panic!("instruction {c:#x} not interpreted"),
            }
        }
    }

    /// The filter checks the header fields; lengths are left to `parse_ip`.
    fn accepted_by_filter(what: &str) -> bool {
        what.contains("length")
    }
}
