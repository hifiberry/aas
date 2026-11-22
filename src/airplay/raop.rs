//! The AirPlay 1 (RAOP) session: the handshake up to RECORD, the volume,
//! and TEARDOWN, as PipeWire's RAOP sender does them for a receiver that
//! takes audio in the clear (`et` 0) or after the auth-setup post (`et` 4).

use super::rtsp::{Conn, Request, Response, RtspError};
use std::net::IpAddr;

/// The body of `POST /auth-setup`: a type byte and a fixed public key,
/// PipeWire's, which receivers that want the post take without checking.
pub const AUTH_SETUP: [u8; 33] = [
    0x01, 0x59, 0x02, 0xed, 0xe9, 0x0d, 0x4e, 0xf2, 0xbd, 0x4c, 0xb6, 0x8a, 0x63, 0x30, 0x03, 0x82,
    0x07, 0xa9, 0x4d, 0xbd, 0x50, 0xd8, 0xaa, 0x46, 0x5b, 0x5d, 0x8c, 0x01, 0x2a, 0x0c, 0x7e, 0x1d,
    0x4e,
];

/// The frames a receiver is asked to buffer at least, 250 ms, unless the
/// latency asked for is shorter.
pub const MIN_LATENCY: u32 = 11025;

/// The minimum latency offered for a stream played `latency` frames late:
/// the latency itself, up to `MIN_LATENCY`.
pub fn offered_minimum(latency: u32) -> u32 {
    latency.min(MIN_LATENCY)
}

/// What a session starts with.
pub struct Setup {
    /// Our UDP ports, for the receiver's sync and retransmit requests and
    /// its timing requests.
    pub control_port: u16,
    pub timing_port: u16,
    pub first_seq: u16,
    pub first_rtptime: u32,
    /// Sent after RECORD; None sends none.
    pub volume_db: Option<f32>,
    /// The minimum latency offered to the receiver, in frames.
    pub min_latency: u32,
}

/// The receiver's UDP ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ports {
    pub server: u16,
    pub control: u16,
    pub timing: Option<u16>,
}

/// A session after RECORD.
pub struct Raop {
    conn: Conn,
    url: String,
    headers: Vec<(&'static str, String)>,
    /// The receiver's latency from the RECORD answer, in frames.
    pub audio_latency: Option<u32>,
    /// The status a receiver answered `/auth-setup` with, when it was not
    /// 200: it goes on without.
    pub auth_setup_refused: Option<u16>,
}

impl Raop {
    /// Runs the handshake on `conn` up to RECORD and the first volume.
    pub fn open(conn: Conn, setup: &Setup) -> Result<(Raop, Ports), RtspError> {
        let session = u32::from_be_bytes(random());
        let id: String = random::<8>().iter().map(|b| format!("{b:02X}")).collect();
        let url = match conn.local_ip() {
            IpAddr::V6(ip) => format!("rtsp://[{ip}]/{session}"),
            ip => format!("rtsp://{ip}/{session}"),
        };
        let mut raop = Raop {
            url,
            headers: vec![
                ("Client-Instance", id.clone()),
                ("DACP-ID", id),
                ("User-Agent", format!("aas/{}", env!("CARGO_PKG_VERSION"))),
                ("Apple-Challenge", base64(&random::<16>())),
            ],
            conn,
            audio_latency: None,
            auth_setup_refused: None,
        };
        raop.expect("OPTIONS", None, None, &[], &[])?;
        let auth = raop.send(
            "POST",
            Some("/auth-setup"),
            Some("application/octet-stream"),
            &AUTH_SETUP,
            &[],
        )?;
        match auth.status {
            200 => {}
            401 => {
                return Err(RtspError::Status {
                    method: "POST /auth-setup".into(),
                    status: 401,
                });
            }
            other => raop.auth_setup_refused = Some(other),
        }
        let sdp = sdp(
            session,
            raop.conn.local_ip(),
            raop.conn.peer_ip(),
            setup.min_latency,
        );
        raop.expect(
            "ANNOUNCE",
            None,
            Some("application/sdp"),
            sdp.as_bytes(),
            &[],
        )?;
        raop.headers.retain(|(n, _)| *n != "Apple-Challenge");
        let transport = format!(
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={}",
            setup.control_port, setup.timing_port
        );
        let answer = raop.expect("SETUP", None, None, &[], &[("Transport", transport)])?;
        let ports = ports(&answer)?;
        let session = answer
            .header("Session")
            .ok_or_else(|| RtspError::Malformed("SETUP gave no Session".into()))?
            .to_string();
        raop.headers.push(("Session", session));
        let info = format!("seq={};rtptime={}", setup.first_seq, setup.first_rtptime);
        let extra = [("Range", "npt=0-".to_string()), ("RTP-Info", info)];
        let record = raop.expect("RECORD", None, None, &[], &extra)?;
        raop.audio_latency = record
            .header("Audio-Latency")
            .and_then(|v| v.parse::<u32>().ok());
        // The receiver plays without these; a refusal is not a failure.
        if let Some(db) = setup.volume_db {
            let _ = raop.volume(db);
        }
        let _ = raop.parameter("progress: 0/0/0\r\n");
        Ok((raop, ports))
    }

    /// Whether the receiver has closed the session's connection.
    pub fn closed(&mut self) -> bool {
        self.conn.closed()
    }

    /// Sets the receiver's volume, -30 to 0 dB.
    pub fn volume(&mut self, db: f32) -> Result<(), RtspError> {
        self.parameter(&format!("volume: {db}\r\n"))
    }

    fn parameter(&mut self, text: &str) -> Result<(), RtspError> {
        self.expect(
            "SET_PARAMETER",
            None,
            Some("text/parameters"),
            text.as_bytes(),
            &[],
        )
        .map(|_| ())
    }

    /// Ends the session. An answer is not waited for beyond the read
    /// timeout, and a failure changes nothing.
    pub fn teardown(mut self) {
        // A receiver that has gone does not hold up a stop for long.
        self.conn.set_timeout(std::time::Duration::from_secs(1));
        let _ = self.send("TEARDOWN", None, None, &[], &[]);
    }

    /// Sends a request to the session's URL, or to `url`, with the
    /// session's headers and `extra`.
    fn send(
        &mut self,
        method: &str,
        url: Option<&str>,
        content_type: Option<&str>,
        body: &[u8],
        extra: &[(&'static str, String)],
    ) -> Result<Response, RtspError> {
        let mut headers = self.headers.clone();
        headers.extend_from_slice(extra);
        let url = url.unwrap_or(&self.url).to_string();
        self.conn.request(&Request {
            method,
            url: &url,
            headers: &headers,
            content_type,
            body,
        })
    }

    /// As `send`, with any status but 200 an error naming the request.
    fn expect(
        &mut self,
        method: &str,
        url: Option<&str>,
        content_type: Option<&str>,
        body: &[u8],
        extra: &[(&'static str, String)],
    ) -> Result<Response, RtspError> {
        let answer = self.send(method, url, content_type, body, extra)?;
        if answer.status != 200 {
            return Err(RtspError::Status {
                method: method.to_string(),
                status: answer.status,
            });
        }
        Ok(answer)
    }
}

/// Base64 without padding, as `Apple-Challenge` takes it.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]));
        }
    }
    out
}

/// Random bytes from the kernel.
pub fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    let mut filled = 0;
    while filled < N {
        // SAFETY: the pointer and length name the unfilled part of `out`.
        let n = unsafe { libc::getrandom(out[filled..].as_mut_ptr().cast(), N - filled, 0) };
        if n > 0 {
            filled += n as usize;
        }
    }
    out
}

/// The SDP of ANNOUNCE.
pub fn sdp(session: u32, local: IpAddr, peer: IpAddr, min_latency: u32) -> String {
    let v = |ip: IpAddr| if ip.is_ipv6() { 6 } else { 4 };
    format!(
        "v=0\r\no=iTunes {session} 0 IN IP{} {local}\r\ns=iTunes\r\nc=IN IP{} {peer}\r\n\
         t=0 0\r\nm=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\n\
         a=fmtp:96 {} 0 16 40 10 14 2 255 0 0 {}\r\na=min-latency:{min_latency}\r\n",
        v(local),
        v(peer),
        super::rtp::FRAMES,
        super::rtp::RATE,
    )
}

/// The ports in a SETUP answer's `Transport`.
pub fn ports(answer: &Response) -> Result<Ports, RtspError> {
    let transport = answer
        .header("Transport")
        .ok_or_else(|| RtspError::Malformed("SETUP gave no Transport".into()))?;
    let port = |name: &str| {
        transport.split(';').find_map(|part| {
            let value = part.trim().strip_prefix(name)?.strip_prefix('=')?;
            value.parse::<u16>().ok().filter(|&p| p > 0)
        })
    };
    let missing = |name: &str| RtspError::Malformed(format!("SETUP gave no {name}: {transport}"));
    Ok(Ports {
        server: port("server_port").ok_or_else(|| missing("server_port"))?,
        control: port("control_port").ok_or_else(|| missing("control_port"))?,
        timing: port("timing_port"),
    })
}

#[cfg(test)]
mod tests {
    use super::super::fake::{Answer, FakeReceiver, Seen, accepting};
    use super::*;
    use std::time::Duration;

    fn setup() -> Setup {
        Setup {
            control_port: 6001,
            timing_port: 6002,
            first_seq: 5,
            first_rtptime: 77,
            volume_db: Some(-12.5),
            min_latency: MIN_LATENCY,
        }
    }

    fn open_with(
        script: impl Fn(&Seen) -> Answer + Send + 'static,
        setup: &Setup,
    ) -> (Result<(Raop, Ports), RtspError>, FakeReceiver) {
        let receiver = FakeReceiver::start(script);
        let conn = Conn::connect("127.0.0.1", receiver.port, Duration::from_secs(2)).unwrap();
        (Raop::open(conn, setup), receiver)
    }

    fn methods(seen: &[Seen]) -> Vec<&str> {
        seen.iter().map(|s| s.method.as_str()).collect()
    }

    #[test]
    fn the_handshake_sends_options_auth_setup_announce_setup_record_in_order() {
        let (opened, receiver) = open_with(accepting, &setup());
        let (raop, ports) = opened.unwrap();
        raop.teardown();
        let seen = receiver.finish();
        assert_eq!(
            methods(&seen),
            [
                "OPTIONS",
                "POST",
                "ANNOUNCE",
                "SETUP",
                "RECORD",
                "SET_PARAMETER",
                "SET_PARAMETER",
                "TEARDOWN"
            ]
        );
        assert_eq!(seen[1].url, "/auth-setup");
        assert_eq!(
            seen[1].header("Content-Type"),
            Some("application/octet-stream")
        );
        assert_eq!(seen[1].body, AUTH_SETUP);
        let url = &seen[2].url;
        let session: u32 = url
            .strip_prefix("rtsp://127.0.0.1/")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(seen[0].url, *url);
        assert_eq!(seen[2].header("Content-Type"), Some("application/sdp"));
        assert_eq!(
            String::from_utf8(seen[2].body.clone()).unwrap(),
            format!(
                "v=0\r\no=iTunes {session} 0 IN IP4 127.0.0.1\r\ns=iTunes\r\n\
                 c=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 96\r\n\
                 a=rtpmap:96 AppleLossless\r\n\
                 a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\na=min-latency:11025\r\n"
            )
        );
        assert_eq!(
            seen[3].header("Transport"),
            Some(
                "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=6001;\
                 timing_port=6002"
            )
        );
        assert_eq!(
            ports,
            Ports {
                server: 6010,
                control: 6011,
                timing: Some(6012)
            }
        );
    }

    #[test]
    fn every_request_names_the_client_and_the_challenge_only_until_announce() {
        let (opened, receiver) = open_with(accepting, &setup());
        opened.unwrap().0.teardown();
        let seen = receiver.finish();
        let id = seen[0].header("Client-Instance").unwrap().to_string();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        for s in &seen {
            assert_eq!(
                s.header("Client-Instance"),
                Some(id.as_str()),
                "{}",
                s.method
            );
            assert_eq!(s.header("DACP-ID"), Some(id.as_str()), "{}", s.method);
            assert!(s.header("User-Agent").unwrap().starts_with("aas/"));
        }
        let challenge = seen[0].header("Apple-Challenge").unwrap();
        assert_eq!(challenge.len(), 22);
        for s in &seen[..3] {
            assert_eq!(s.header("Apple-Challenge"), Some(challenge), "{}", s.method);
        }
        for s in &seen[3..] {
            assert_eq!(s.header("Apple-Challenge"), None, "{}", s.method);
        }
        // The session from SETUP's answer goes with everything after it.
        for s in &seen[..4] {
            assert_eq!(s.header("Session"), None, "{}", s.method);
        }
        for s in &seen[4..] {
            assert_eq!(s.header("Session"), Some("DEADBEEF"), "{}", s.method);
        }
    }

    #[test]
    fn record_carries_seq_and_rtptime_then_volume_and_progress() {
        let (opened, receiver) = open_with(accepting, &setup());
        let (raop, _) = opened.unwrap();
        assert_eq!(raop.audio_latency, Some(11025));
        assert_eq!(raop.auth_setup_refused, None);
        raop.teardown();
        let seen = receiver.finish();
        assert_eq!(seen[4].header("Range"), Some("npt=0-"));
        assert_eq!(seen[4].header("RTP-Info"), Some("seq=5;rtptime=77"));
        assert_eq!(seen[5].header("Content-Type"), Some("text/parameters"));
        assert_eq!(seen[5].body, b"volume: -12.5\r\n");
        assert_eq!(seen[6].body, b"progress: 0/0/0\r\n");
    }

    #[test]
    fn no_volume_sends_only_progress() {
        let quiet = Setup {
            volume_db: None,
            ..setup()
        };
        let (opened, receiver) = open_with(accepting, &quiet);
        opened.unwrap().0.teardown();
        let seen = receiver.finish();
        assert_eq!(&methods(&seen)[5..], ["SET_PARAMETER", "TEARDOWN"]);
        assert_eq!(seen[5].body, b"progress: 0/0/0\r\n");
    }

    #[test]
    fn a_404_to_auth_setup_goes_on() {
        let script = |s: &Seen| match s.method.as_str() {
            "POST" => Answer::Status(404, Vec::new()),
            _ => accepting(s),
        };
        let (opened, receiver) = open_with(script, &setup());
        let (raop, _) = opened.unwrap();
        assert_eq!(raop.auth_setup_refused, Some(404));
        raop.teardown();
        assert_eq!(methods(&receiver.finish())[2], "ANNOUNCE");
    }

    #[test]
    fn a_401_to_auth_setup_ends_the_attempt() {
        let script = |s: &Seen| match s.method.as_str() {
            "POST" => Answer::Status(401, Vec::new()),
            _ => accepting(s),
        };
        let (opened, _receiver) = open_with(script, &setup());
        let e = opened.err().unwrap();
        assert_eq!(
            e,
            RtspError::Status {
                method: "POST /auth-setup".into(),
                status: 401
            }
        );
    }

    #[test]
    fn a_non_200_is_an_error_naming_the_request() {
        for method in ["OPTIONS", "ANNOUNCE", "SETUP", "RECORD"] {
            let script = move |s: &Seen| {
                if s.method == method {
                    Answer::Status(453, Vec::new())
                } else {
                    accepting(s)
                }
            };
            let (opened, _receiver) = open_with(script, &setup());
            let e = opened.err().unwrap();
            assert_eq!(
                e,
                RtspError::Status {
                    method: method.into(),
                    status: 453
                }
            );
        }
    }

    #[test]
    fn a_refused_volume_still_opens() {
        let script = |s: &Seen| match s.method.as_str() {
            "SET_PARAMETER" => Answer::Status(400, Vec::new()),
            _ => accepting(s),
        };
        let (opened, _receiver) = open_with(script, &setup());
        assert!(opened.is_ok());
    }

    #[test]
    fn a_closed_socket_mid_handshake_is_an_error_not_a_hang() {
        let script = |s: &Seen| match s.method.as_str() {
            "ANNOUNCE" => Answer::Close,
            _ => accepting(s),
        };
        let started = std::time::Instant::now();
        let (opened, _receiver) = open_with(script, &setup());
        assert!(matches!(opened, Err(RtspError::Io(_))));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn volume_is_sent_as_text() {
        let (opened, receiver) = open_with(accepting, &setup());
        let (mut raop, _) = opened.unwrap();
        raop.volume(-30.0).unwrap();
        raop.teardown();
        let seen = receiver.finish();
        assert_eq!(seen[7].body, b"volume: -30\r\n");
    }

    fn answer(headers: &[(&str, &str)]) -> Response {
        Response {
            status: 200,
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    #[test]
    fn ports_reads_the_transport() {
        let t = "RTP/AVP/UDP;unicast;server_port=53561;control_port=63379;timing_port=50000";
        assert_eq!(
            ports(&answer(&[("Transport", t)])).unwrap(),
            Ports {
                server: 53561,
                control: 63379,
                timing: Some(50000)
            }
        );
        let no_timing = "RTP/AVP/UDP;server_port=1;control_port=2";
        assert_eq!(
            ports(&answer(&[("Transport", no_timing)])).unwrap().timing,
            None
        );
        for bad in [
            "RTP/AVP/UDP;control_port=2",
            "RTP/AVP/UDP;server_port=1",
            "server_port=0;control_port=2",
        ] {
            assert!(
                matches!(
                    ports(&answer(&[("Transport", bad)])),
                    Err(RtspError::Malformed(_))
                ),
                "{bad}"
            );
        }
        assert!(matches!(ports(&answer(&[])), Err(RtspError::Malformed(_))));
    }

    #[test]
    fn base64_has_no_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg");
        assert_eq!(base64(b"fo"), "Zm8");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0xff; 16]).len(), 22);
        assert_eq!(base64(&[0xfb, 0xff]), "+/8");
    }

    #[test]
    fn random_bytes_differ() {
        assert_ne!(random::<16>(), random::<16>());
    }

    #[test]
    fn the_sdp_names_ipv6_as_such() {
        let local: IpAddr = "fe80::1".parse().unwrap();
        let peer: IpAddr = "fe80::2".parse().unwrap();
        let text = sdp(9, local, peer, MIN_LATENCY);
        assert!(text.contains("o=iTunes 9 0 IN IP6 fe80::1\r\n"), "{text}");
        assert!(text.contains("c=IN IP6 fe80::2\r\n"), "{text}");
    }

    #[test]
    fn the_sdp_offers_the_minimum_latency_it_is_given() {
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let text = sdp(9, ip, ip, 4410);
        assert!(text.ends_with("a=min-latency:4410\r\n"), "{text}");
    }

    #[test]
    fn the_offered_minimum_is_the_latency_up_to_250_ms() {
        assert_eq!(offered_minimum(4410), 4410);
        assert_eq!(offered_minimum(11025), 11025);
        assert_eq!(offered_minimum(66150), 11025);
    }
}
