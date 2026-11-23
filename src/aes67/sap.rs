//! SAP (RFC 2974) packets carrying SDP, IPv4 only.

use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SapMessage {
    Announce {
        hash: u16,
        source: Ipv4Addr,
        sdp: String,
    },
    Delete {
        hash: u16,
        source: Ipv4Addr,
    },
}

const VERSION: u8 = 1;
const ADDR_IPV6: u8 = 0x10;
const DELETION: u8 = 0x04;
const ENCRYPTED: u8 = 0x02;
const COMPRESSED: u8 = 0x01;
const PAYLOAD_TYPE: &[u8] = b"application/sdp\0";

/// Parse a SAP packet. IPv6, encrypted, compressed and malformed packets
/// give `None`.
pub fn sap_parse(packet: &[u8]) -> Option<SapMessage> {
    if packet.len() < 8 {
        return None;
    }
    let first = packet[0];
    if first >> 5 != VERSION || first & (ADDR_IPV6 | ENCRYPTED | COMPRESSED) != 0 {
        return None;
    }
    let hash = u16::from_be_bytes([packet[2], packet[3]]);
    let source = Ipv4Addr::new(packet[4], packet[5], packet[6], packet[7]);
    let rest = packet.get(8 + packet[1] as usize * 4..)?;
    if first & DELETION != 0 {
        return Some(SapMessage::Delete { hash, source });
    }
    // The payload type is optional: without it the SDP starts at "v=".
    let body = if rest.starts_with(b"v=") {
        rest
    } else {
        let nul = rest.iter().position(|&b| b == 0)?;
        &rest[nul + 1..]
    };
    let sdp = std::str::from_utf8(body).ok()?.to_string();
    Some(SapMessage::Announce { hash, source, sdp })
}

/// Build a packet: IPv4, no authentication, payload type `application/sdp`.
pub fn sap_build(msg: &SapMessage) -> Vec<u8> {
    let (first, hash, source, sdp) = match msg {
        SapMessage::Announce { hash, source, sdp } => (VERSION << 5, hash, source, sdp.as_str()),
        SapMessage::Delete { hash, source } => (VERSION << 5 | DELETION, hash, source, ""),
    };
    let mut p = vec![first, 0];
    p.extend_from_slice(&hash.to_be_bytes());
    p.extend_from_slice(&source.octets());
    p.extend_from_slice(PAYLOAD_TYPE);
    p.extend_from_slice(sdp.as_bytes());
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aes67::sdp::{self, DANTE_SDP};

    const SRC: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);

    /// A packet in Dante's layout, assembled by hand.
    fn packet(first: u8, auth_words: u8, payload_type: Option<&str>, sdp: &str) -> Vec<u8> {
        let mut p = vec![first, auth_words, 0x12, 0x34];
        p.extend_from_slice(&SRC.octets());
        p.extend(std::iter::repeat_n(0xAA, auth_words as usize * 4));
        if let Some(t) = payload_type {
            p.extend_from_slice(t.as_bytes());
            p.push(0);
        }
        p.extend_from_slice(sdp.as_bytes());
        p
    }

    #[test]
    fn sap_header_with_auth_and_ipv4() {
        let p = packet(0x20, 2, Some("application/sdp"), DANTE_SDP);
        let Some(SapMessage::Announce {
            hash,
            source,
            sdp: text,
        }) = sap_parse(&p)
        else {
            panic!("not an announce");
        };
        assert_eq!(hash, 0x1234);
        assert_eq!(source, SRC);
        assert_eq!(sdp::parse(&text).unwrap().name, "AU-U22-000001 : 1");
        // Payload type is optional.
        let p = packet(0x20, 0, None, DANTE_SDP);
        assert!(matches!(sap_parse(&p), Some(SapMessage::Announce { .. })));
    }

    #[test]
    fn sap_delete_is_recognised() {
        let p = packet(0x24, 0, Some("application/sdp"), DANTE_SDP);
        assert_eq!(
            sap_parse(&p),
            Some(SapMessage::Delete {
                hash: 0x1234,
                source: SRC
            })
        );
    }

    #[test]
    fn ipv6_encrypted_compressed_and_short_are_ignored() {
        for first in [0x30, 0x22, 0x21] {
            assert_eq!(sap_parse(&packet(first, 0, None, DANTE_SDP)), None);
        }
        assert_eq!(sap_parse(&[0x20, 0, 0]), None);
        // Wrong version.
        assert_eq!(sap_parse(&packet(0x40, 0, None, DANTE_SDP)), None);
    }

    #[test]
    fn build_then_parse_round_trips() {
        let a = SapMessage::Announce {
            hash: 9,
            source: SRC,
            sdp: DANTE_SDP.into(),
        };
        let bytes = sap_build(&a);
        assert_eq!(bytes[0], 0x20);
        assert_eq!(&bytes[8..24], b"application/sdp\0");
        assert_eq!(sap_parse(&bytes), Some(a));
        let d = SapMessage::Delete {
            hash: 9,
            source: SRC,
        };
        assert_eq!(bytes_type(&sap_build(&d)), 1);
        assert_eq!(sap_parse(&sap_build(&d)), Some(d));
    }

    #[test]
    fn a_captured_dante_announcement_parses() {
        // Received from a Dante device in AES67 mode; addresses, clock ID and
        // the name's MAC suffix replaced. The SDP ends with CRLF, no NUL.
        let p = include_bytes!("../../tests/fixtures/aes67/sap-dante.bin");
        let Some(SapMessage::Announce { source, sdp, .. }) = sap_parse(p) else {
            panic!("not an announce");
        };
        assert_eq!(source, SRC);
        assert!(sdp.ends_with("\r\n"));
        let s = sdp::parse(&sdp).unwrap();
        assert_eq!(s.name, "AU-U22-000002 : 1");
        assert_eq!(s.group, Ipv4Addr::new(239, 69, 0, 121));
        assert_eq!((s.port, s.payload_type, s.channels), (5004, 97, 2));
        assert_eq!(s.encoding, sdp::Encoding::L24);
        assert_eq!(s.ptime_us, 1000);
        assert_eq!(
            s.refclk,
            Some(([0x00, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55], 0))
        );
        assert_eq!(s.mediaclk_offset, 0);
        assert!(sdp::selectable(&s).is_ok());
    }

    fn bytes_type(b: &[u8]) -> u8 {
        (b[0] >> 2) & 1
    }
}
