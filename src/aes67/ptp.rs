//! PTPv2 (IEEE 1588-2008) message parsing for the receive-only listener.
//! Plain slicing only; `aas` never sends a PTP packet.

pub type ClockId = [u8; 8];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sync,
    FollowUp,
    Announce,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub domain: u8,
    pub two_step: bool,
    /// The correction field in whole nanoseconds (the wire unit is 2^-16 ns).
    pub correction_ns: i64,
    pub source: ClockId,
    pub port: u16,
    pub seq: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Announce {
    pub gm: ClockId,
    pub priority1: u8,
    pub class: u8,
    pub accuracy: u8,
    pub variance: u16,
    pub priority2: u8,
    pub steps_removed: u16,
}

const HEADER_LEN: usize = 34;
const TIMESTAMP_END: usize = HEADER_LEN + 10;
const ANNOUNCE_LEN: usize = 64;

/// Parse the common header; `None` if too short or not PTP version 2.
pub fn header(payload: &[u8]) -> Option<Header> {
    if payload.len() < HEADER_LEN || payload[1] & 0x0f != 2 {
        return None;
    }
    let kind = match payload[0] & 0x0f {
        0x0 => Kind::Sync,
        0x8 => Kind::FollowUp,
        0xB => Kind::Announce,
        _ => Kind::Other,
    };
    let correction = i64::from_be_bytes(payload[8..16].try_into().ok()?);
    Some(Header {
        kind,
        domain: payload[4],
        two_step: payload[6] & 0x02 != 0,
        correction_ns: correction >> 16,
        source: payload[20..28].try_into().ok()?,
        port: u16::from_be_bytes([payload[28], payload[29]]),
        seq: u16::from_be_bytes([payload[30], payload[31]]),
    })
}

/// The timestamp of a Sync or Follow_Up message in nanoseconds.
pub fn origin_ns(payload: &[u8]) -> Option<i128> {
    let h = header(payload)?;
    if !matches!(h.kind, Kind::Sync | Kind::FollowUp) || payload.len() < TIMESTAMP_END {
        return None;
    }
    let mut secs = [0u8; 8];
    secs[2..].copy_from_slice(&payload[34..40]);
    let secs = u64::from_be_bytes(secs);
    let nanos = u32::from_be_bytes(payload[40..44].try_into().ok()?);
    Some(secs as i128 * 1_000_000_000 + nanos as i128)
}

/// The grandmaster description of an Announce message.
pub fn announce(payload: &[u8]) -> Option<Announce> {
    let h = header(payload)?;
    if h.kind != Kind::Announce || payload.len() < ANNOUNCE_LEN {
        return None;
    }
    Some(Announce {
        priority1: payload[47],
        class: payload[48],
        accuracy: payload[49],
        variance: u16::from_be_bytes([payload[50], payload[51]]),
        priority2: payload[52],
        gm: payload[53..61].try_into().ok()?,
        steps_removed: u16::from_be_bytes([payload[61], payload[62]]),
    })
}

/// IEEE 1588 dataset comparison A, reduced to the fields an Announce carries:
/// priority1, class, accuracy, variance, priority2, then identity; lowest wins.
pub fn better(a: &Announce, b: &Announce) -> bool {
    let key = |x: &Announce| {
        (
            x.priority1,
            x.class,
            x.accuracy,
            x.variance,
            x.priority2,
            x.gm,
        )
    };
    key(a) < key(b)
}

/// "00-11-22-FF-FE-33-44-55", as SDP `ts-refclk` writes it.
pub fn clock_id_text(id: &ClockId) -> String {
    id.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
pub(crate) fn header_bytes(
    kind: u8,
    len: usize,
    domain: u8,
    two_step: bool,
    source: ClockId,
    seq: u16,
) -> Vec<u8> {
    let mut b = vec![0u8; len];
    b[0] = kind;
    b[1] = 2;
    b[2..4].copy_from_slice(&(len as u16).to_be_bytes());
    b[4] = domain;
    if two_step {
        b[6] = 0x02;
    }
    b[20..28].copy_from_slice(&source);
    b[28..30].copy_from_slice(&1u16.to_be_bytes());
    b[30..32].copy_from_slice(&seq.to_be_bytes());
    b
}

#[cfg(test)]
pub(crate) fn sync_bytes(domain: u8, source: ClockId, seq: u16) -> Vec<u8> {
    header_bytes(0x0, 44, domain, true, source, seq)
}

/// `correction` is the raw wire value in 2^-16 ns.
#[cfg(test)]
pub(crate) fn follow_up_bytes(
    domain: u8,
    source: ClockId,
    seq: u16,
    secs: u64,
    nanos: u32,
    correction: i64,
) -> Vec<u8> {
    let mut b = header_bytes(0x8, 44, domain, false, source, seq);
    b[8..16].copy_from_slice(&correction.to_be_bytes());
    b[34..40].copy_from_slice(&secs.to_be_bytes()[2..]);
    b[40..44].copy_from_slice(&nanos.to_be_bytes());
    b
}

#[cfg(test)]
pub(crate) fn announce_bytes(domain: u8, source: ClockId, seq: u16, a: &Announce) -> Vec<u8> {
    let mut b = header_bytes(0xB, 64, domain, false, source, seq);
    b[47] = a.priority1;
    b[48] = a.class;
    b[49] = a.accuracy;
    b[50..52].copy_from_slice(&a.variance.to_be_bytes());
    b[52] = a.priority2;
    b[53..61].copy_from_slice(&a.gm);
    b[61..63].copy_from_slice(&a.steps_removed.to_be_bytes());
    b[63] = 0xA0;
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: ClockId = [0x00, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55];

    fn ann() -> Announce {
        Announce {
            gm: ID,
            priority1: 247,
            class: 248,
            accuracy: 254,
            variance: 61536,
            priority2: 112,
            steps_removed: 0,
        }
    }

    #[test]
    fn a_follow_up_gives_its_origin_and_correction() {
        let b = follow_up_bytes(0, ID, 7, 3, 500, 0x0001_0000 * 2);
        let h = header(&b).unwrap();
        assert_eq!(h.kind, Kind::FollowUp);
        assert!(!h.two_step);
        assert_eq!((h.source, h.port, h.seq, h.domain), (ID, 1, 7, 0));
        assert_eq!(h.correction_ns, 2);
        assert_eq!(origin_ns(&b), Some(3_000_000_500));
        // Seconds use all 48 bits.
        let big = follow_up_bytes(0, ID, 1, 0x0001_0000_0000, 1, 0);
        assert_eq!(origin_ns(&big), Some((1i128 << 32) * 1_000_000_000 + 1));
        let s = header(&sync_bytes(3, ID, 9)).unwrap();
        assert_eq!((s.kind, s.two_step, s.domain), (Kind::Sync, true, 3));
    }

    #[test]
    fn an_announce_gives_the_grandmaster() {
        let b = announce_bytes(0, ID, 1, &ann());
        assert_eq!(header(&b).unwrap().kind, Kind::Announce);
        assert_eq!(announce(&b), Some(ann()));
        assert_eq!(announce(&sync_bytes(0, ID, 1)), None);
    }

    #[test]
    fn short_or_wrong_version_is_none() {
        let b = follow_up_bytes(0, ID, 1, 1, 1, 0);
        assert_eq!(origin_ns(&b[..43]), None);
        assert!(header(&b[..33]).is_none());
        let mut v1 = b.clone();
        v1[1] = 1;
        assert!(header(&v1).is_none());
        assert_eq!(origin_ns(&v1), None);
        let a = announce_bytes(0, ID, 1, &ann());
        assert_eq!(announce(&a[..63]), None);
    }

    #[test]
    fn better_compares_in_order() {
        let base = ann();
        let with = |f: fn(&mut Announce)| {
            let mut x = base;
            f(&mut x);
            x
        };
        assert!(!better(&base, &base));
        // Each field decides when the earlier ones tie, and outranks later ones.
        let cases: [(Announce, Announce); 6] = [
            (with(|x| x.priority1 = 100), with(|x| x.class = 1)),
            (with(|x| x.class = 6), with(|x| x.accuracy = 1)),
            (with(|x| x.accuracy = 0x20), with(|x| x.variance = 1)),
            (with(|x| x.variance = 100), with(|x| x.priority2 = 1)),
            (with(|x| x.priority2 = 1), with(|x| x.gm = [0; 8])),
            (with(|x| x.gm[7] = 0x54), base),
        ];
        for (win, lose) in cases {
            assert!(better(&win, &lose), "{win:?} should beat {lose:?}");
            assert!(!better(&lose, &win), "{lose:?} should not beat {win:?}");
        }
    }

    #[test]
    fn clock_id_text_matches_sdp() {
        assert_eq!(clock_id_text(&ID), "00-11-22-FF-FE-33-44-55");
    }
}
