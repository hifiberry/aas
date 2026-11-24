//! SDP for AES67: reading a stream's description and writing ours.

use std::net::Ipv4Addr;

use super::ptp::{self, ClockId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    L16,
    L24,
    /// Any other encoding name; kept so `selectable` can refuse it by name.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    pub origin: Ipv4Addr,
    pub session_id: u64,
    pub version: u64,
    pub group: Ipv4Addr,
    pub port: u16,
    pub ttl: u8,
    pub payload_type: u8,
    pub encoding: Encoding,
    pub rate: u32,
    pub channels: u8,
    pub ptime_us: u32,
    pub refclk: Option<(ClockId, u8)>,
    pub mediaclk_offset: u32,
}

pub fn parse(sdp: &str) -> Result<Session, String> {
    let mut name = None;
    let mut origin = None;
    let mut conn = None;
    let mut media = None;
    let mut rtpmap = None;
    let mut ptime_us = 1000;
    let mut refclk = None;
    let mut mediaclk_offset = 0;

    for line in sdp.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "o" => origin = parse_origin(value),
            "s" => name = Some(value.to_string()),
            "c" => conn = conn.or_else(|| parse_connection(value)),
            "m" => {
                let f: Vec<&str> = value.split_whitespace().collect();
                if media.is_none() && f.len() >= 4 && f[0] == "audio" {
                    // Only the first payload type is used.
                    let port = f[1].split('/').next().and_then(|p| p.parse::<u16>().ok());
                    media = port.zip(f[3].parse::<u8>().ok());
                }
            }
            "a" => {
                let (attr, v) = value.split_once(':').unwrap_or((value, ""));
                match attr {
                    "rtpmap" => {
                        if let Some((pt, map)) = v.split_once(' ')
                            && let Ok(pt) = pt.trim().parse::<u8>()
                            && media.is_some_and(|(_, m)| m == pt)
                        {
                            rtpmap = parse_rtpmap(map.trim());
                        }
                    }
                    "ptime" => {
                        let ms: f64 = v.trim().parse().map_err(|_| format!("ptime {v:?}"))?;
                        ptime_us = (ms * 1000.0).round() as u32;
                    }
                    "ts-refclk" => refclk = parse_refclk(v.trim()),
                    "mediaclk" => {
                        if let Some(n) = v.trim().strip_prefix("direct=") {
                            let n = n.split_whitespace().next().unwrap_or("");
                            mediaclk_offset = n.parse().map_err(|_| format!("mediaclk {v:?}"))?;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let (session_id, version, origin) = origin.ok_or("no o= line")?;
    let (group, ttl) = conn.ok_or("no c= line")?;
    let (port, payload_type) = media.ok_or("no audio m= line")?;
    let (encoding, rate, channels) = rtpmap.ok_or("no rtpmap for the audio payload")?;
    Ok(Session {
        name: name.ok_or("no s= line")?,
        origin,
        session_id,
        version,
        group,
        port,
        ttl,
        payload_type,
        encoding,
        rate,
        channels,
        ptime_us,
        refclk,
        mediaclk_offset,
    })
}

/// `- <id> <version> IN IP4 <addr>`
fn parse_origin(v: &str) -> Option<(u64, u64, Ipv4Addr)> {
    let f: Vec<&str> = v.split_whitespace().collect();
    if f.len() < 6 || f[3] != "IN" || f[4] != "IP4" {
        return None;
    }
    Some((f[1].parse().ok()?, f[2].parse().ok()?, f[5].parse().ok()?))
}

/// `IN IP4 <group>[/<ttl>[/<count>]]`; an absent TTL is 1.
fn parse_connection(v: &str) -> Option<(Ipv4Addr, u8)> {
    let f: Vec<&str> = v.split_whitespace().collect();
    if f.len() < 3 || f[0] != "IN" || f[1] != "IP4" {
        return None;
    }
    let mut parts = f[2].split('/');
    let group = parts.next()?.parse().ok()?;
    let ttl = match parts.next() {
        Some(t) => t.parse().ok()?,
        None => 1,
    };
    Some((group, ttl))
}

/// `L24/48000/2`; channels default to 1.
fn parse_rtpmap(v: &str) -> Option<(Encoding, u32, u8)> {
    let mut parts = v.split('/');
    let encoding = match parts.next()? {
        "L16" => Encoding::L16,
        "L24" => Encoding::L24,
        _ => Encoding::Other,
    };
    let rate = parts.next()?.parse().ok()?;
    let channels = match parts.next() {
        Some(c) => c.parse().ok()?,
        None => 1,
    };
    Some((encoding, rate, channels))
}

/// `ptp=IEEE1588-2008:<clock id>[:<domain>]`; other sources give `None`.
fn parse_refclk(v: &str) -> Option<(ClockId, u8)> {
    let rest = v.strip_prefix("ptp=")?;
    let mut parts = rest.split(':');
    parts.next()?; // version, e.g. IEEE1588-2008
    let mut id = [0u8; 8];
    let mut octets = parts.next()?.split('-');
    for b in &mut id {
        *b = u8::from_str_radix(octets.next()?, 16).ok()?;
    }
    if octets.next().is_some() {
        return None;
    }
    let domain = match parts.next() {
        Some(d) => d.parse().ok()?,
        None => 0,
    };
    Some((id, domain))
}

pub fn render(s: &Session) -> String {
    let encoding = match s.encoding {
        Encoding::L16 => "L16",
        Encoding::L24 => "L24",
        Encoding::Other => "unknown",
    };
    let mut out = format!(
        "v=0\r\no=- {} {} IN IP4 {}\r\ns={}\r\nc=IN IP4 {}/{}\r\nt=0 0\r\n\
         m=audio {} RTP/AVP {}\r\na=rtpmap:{} {}/{}/{}\r\na=ptime:{}\r\n",
        s.session_id,
        s.version,
        s.origin,
        s.name,
        s.group,
        s.ttl,
        s.port,
        s.payload_type,
        s.payload_type,
        encoding,
        s.rate,
        s.channels,
        ptime_text(s.ptime_us),
    );
    if let Some((id, domain)) = &s.refclk {
        out.push_str(&format!(
            "a=ts-refclk:ptp=IEEE1588-2008:{}:{domain}\r\n",
            ptp::clock_id_text(id)
        ));
    }
    out.push_str(&format!("a=mediaclk:direct={}\r\n", s.mediaclk_offset));
    out
}

/// Milliseconds with up to 3 decimals, trailing zeros stripped.
fn ptime_text(us: u32) -> String {
    let text = format!("{}.{:03}", us / 1000, us % 1000);
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Whether the input can play the stream; the error names the offending field.
pub fn selectable(s: &Session) -> Result<(), &'static str> {
    if s.rate != 48000 {
        return Err("rate");
    }
    if !matches!(s.encoding, Encoding::L16 | Encoding::L24) {
        return Err("encoding");
    }
    if !(1..=8).contains(&s.channels) {
        return Err("channels");
    }
    if s.ptime_us == 0 || s.ptime_us > 4000 {
        return Err("ptime");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) const DANTE_SDP: &str = "v=0\r\n\
o=- 1311738121 1311738121 IN IP4 192.0.2.10\r\n\
s=AU-U22-000001 : 1\r\n\
c=IN IP4 239.69.0.121/32\r\n\
t=0 0\r\n\
a=keywds:Dante\r\n\
m=audio 5004 RTP/AVP 97\r\n\
i=2 channels: Left, Right\r\n\
a=recvonly\r\n\
a=rtpmap:97 L24/48000/2\r\n\
a=ptime:1\r\n\
a=ts-refclk:ptp=IEEE1588-2008:00-11-22-FF-FE-33-44-55:0\r\n\
a=mediaclk:direct=0\r\n";

#[cfg(test)]
mod tests {
    use super::*;

    const ID: ClockId = [0x00, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55];

    fn sample() -> Session {
        Session {
            name: "aas".into(),
            origin: Ipv4Addr::new(192, 0, 2, 20),
            session_id: 42,
            version: 7,
            group: Ipv4Addr::new(239, 69, 1, 2),
            port: 5004,
            ttl: 16,
            payload_type: 96,
            encoding: Encoding::L24,
            rate: 48000,
            channels: 2,
            ptime_us: 1000,
            refclk: Some((ID, 0)),
            mediaclk_offset: 0,
        }
    }

    #[test]
    fn a_dante_announcement_parses() {
        let s = parse(DANTE_SDP).unwrap();
        assert_eq!(s.name, "AU-U22-000001 : 1");
        assert_eq!(s.origin, Ipv4Addr::new(192, 0, 2, 10));
        assert_eq!(s.session_id, 1311738121);
        assert_eq!(s.version, 1311738121);
        assert_eq!(s.group, Ipv4Addr::new(239, 69, 0, 121));
        assert_eq!(s.ttl, 32);
        assert_eq!(s.port, 5004);
        assert_eq!(s.payload_type, 97);
        assert_eq!(s.encoding, Encoding::L24);
        assert_eq!(s.rate, 48000);
        assert_eq!(s.channels, 2);
        assert_eq!(s.ptime_us, 1000);
        assert_eq!(s.refclk, Some((ID, 0)));
        assert_eq!(s.mediaclk_offset, 0);
        assert_eq!(selectable(&s), Ok(()));
    }

    #[test]
    fn absent_ttl_is_one_and_lower_case_clock_parses() {
        let sdp = DANTE_SDP
            .replace("/32", "")
            .replace("00-11-22-FF-FE-33-44-55", "00-11-22-ff-fe-33-44-55")
            .replace("direct=0", "direct=1234");
        let s = parse(&sdp).unwrap();
        assert_eq!(s.ttl, 1);
        assert_eq!(s.refclk, Some((ID, 0)));
        assert_eq!(s.mediaclk_offset, 1234);
    }

    #[test]
    fn render_then_parse_is_identity() {
        let s = sample();
        let text = render(&s);
        for line in [
            "a=rtpmap:96 L24/48000/2",
            "a=ptime:1",
            "a=ts-refclk:ptp=IEEE1588-2008:00-11-22-FF-FE-33-44-55:0",
            "a=mediaclk:direct=0",
        ] {
            assert!(text.lines().any(|l| l == line), "missing {line} in {text}");
        }
        assert_eq!(parse(&text).unwrap(), s);
    }

    #[test]
    fn fractional_ptime_parses() {
        let s = parse(&DANTE_SDP.replace("ptime:1", "ptime:0.125")).unwrap();
        assert_eq!(s.ptime_us, 125);
        let text = render(&s);
        assert!(text.lines().any(|l| l == "a=ptime:0.125"));
    }

    #[test]
    fn missing_media_is_an_error() {
        assert!(parse("v=0\r\ns=x\r\n").is_err());
    }

    #[test]
    fn unsupported_is_named() {
        let mut s = sample();
        s.rate = 44100;
        assert_eq!(selectable(&s), Err("rate"));
        let s = parse(&DANTE_SDP.replace("L24", "L32")).unwrap();
        assert_eq!(selectable(&s), Err("encoding"));
        let s = parse(&DANTE_SDP.replace("48000/2", "48000/16")).unwrap();
        assert_eq!(selectable(&s), Err("channels"));
        let mut s = sample();
        s.ptime_us = 5000;
        assert_eq!(selectable(&s), Err("ptime"));
    }
}
