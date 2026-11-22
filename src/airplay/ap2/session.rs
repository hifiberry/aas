//! The AirPlay 2 session as PipeWire's sender opens it with a third-party
//! receiver: `GET /info`, transient pairing, then on the sealed channel a
//! SETUP of the session (NTP timing at our port), SETPEERS, a SETUP of a
//! realtime stream (ALAC, 44.1 kHz, the stream key `shk`), RECORD and the
//! volume. While it plays, `POST /feedback` every 2 s keeps it alive.

use super::bplist::{self, Value};
use super::channel::Keys;
use super::events::Events;
use super::pair::pair_transient;
use crate::airplay::raop::{Ports, Setup, random};
use crate::airplay::rtp::{FRAMES, RATE};
use crate::airplay::rtsp::{Conn, Request, Response, RtspError};
use std::net::IpAddr;

/// How the sender names itself, as PipeWire's does.
pub const USER_AGENT: &str = "AirPlay/999.0.0";
/// The audio format bit of ALAC at 44.1 kHz, 16 bits, stereo.
const ALAC_44100_16_2: i64 = 0x40000;
/// The compression type of ALAC.
const CT_ALAC: i64 = 2;
/// A realtime stream.
const TYPE_REALTIME: i64 = 0x60;
/// The most frames a receiver is told it may buffer, 2 s.
const LATENCY_MAX: i64 = 88200;
/// `statusFlags` bits of `/info` that transient pairing cannot meet.
const PIN_REQUIRED: i64 = 1 << 3;
const ONE_TIME_PAIRING_REQUIRED: i64 = 1 << 9;
const BPLIST: &str = "application/x-apple-binary-plist";
/// For the connect to the event port.
const EVENTS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// A session after RECORD.
pub struct Ap2 {
    conn: Conn,
    url: String,
    headers: Vec<(&'static str, String)>,
    /// The receiver's latency from the RECORD answer, in frames.
    pub audio_latency: Option<u32>,
    /// The key the audio is sealed with.
    pub shk: [u8; 32],
    /// The event channel, when the receiver has an event port.
    events: Option<Events>,
}

impl Ap2 {
    /// Runs the handshake on `conn` up to RECORD and the first volume.
    pub fn open(mut conn: Conn, setup: &Setup) -> Result<(Ap2, Ports), RtspError> {
        let session = u32::from_be_bytes(random());
        let (local, peer) = (conn.local_ip(), conn.peer_ip());
        let url = match local {
            IpAddr::V6(ip) => format!("rtsp://[{ip}]/{session}"),
            ip => format!("rtsp://{ip}/{session}"),
        };
        let id: String = random::<8>().iter().map(|b| format!("{b:02X}")).collect();
        let headers = vec![("DACP-ID", id), ("User-Agent", USER_AGENT.to_string())];
        let info = conn.request(&Request {
            method: "GET",
            url: "/info",
            headers: &headers,
            content_type: None,
            body: &[],
        })?;
        check(&info, "GET /info")?;
        let info = bplist::decode(&info.body).map_err(|e| RtspError::Malformed(e.to_string()))?;
        let flags = info.get("statusFlags").and_then(Value::as_int).unwrap_or(0);
        if flags & (PIN_REQUIRED | ONE_TIME_PAIRING_REQUIRED) != 0 {
            return Err(RtspError::Pairing(format!(
                "the receiver wants a PIN or HomeKit pairing (statusFlags {flags:#x}), \
                 which this build does not do"
            )));
        }
        let secret = pair_transient(&mut conn, &headers)?;
        conn.seal(&Keys::control(&secret));
        let mut ap2 = Ap2 {
            conn,
            url,
            headers,
            audio_latency: None,
            shk: random(),
            events: None,
        };
        let uuid: String = random::<16>()
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let dash = if matches!(i, 4 | 6 | 8 | 10) { "-" } else { "" };
                format!("{dash}{b:02X}")
            })
            .collect();
        let device: Vec<String> = random::<6>().iter().map(|b| format!("{b:02X}")).collect();
        let answer = ap2.plist(
            "SETUP",
            None,
            &dict(vec![
                ("deviceID", Value::String(device.join(":"))),
                ("sessionUUID", Value::String(uuid)),
                ("timingPort", Value::Int(i64::from(setup.timing_port))),
                ("timingProtocol", Value::String("NTP".into())),
            ]),
        )?;
        check(&answer, "SETUP")?;
        if let Some(s) = answer.header("Session") {
            ap2.headers.push(("Session", s.to_string()));
        }
        // The event channel, which the receiver may wait for before it
        // answers RECORD, as a sound bar does.
        let session_answer =
            bplist::decode(&answer.body).map_err(|e| RtspError::Malformed(e.to_string()))?;
        let event_port = session_answer
            .get("eventPort")
            .and_then(Value::as_int)
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(0);
        if event_port != 0 {
            let addr = std::net::SocketAddr::new(peer, event_port);
            let events = Events::connect(addr, &secret, EVENTS_TIMEOUT)
                .map_err(|e| RtspError::Io(format!("the event port {event_port}: {e}")))?;
            ap2.events = Some(events);
        }
        // Receivers that do not take SETPEERS play all the same.
        let peers = Value::Array(vec![
            Value::String(peer.to_string()),
            Value::String(local.to_string()),
        ]);
        ap2.plist("SETPEERS", Some("/peer-list-changed"), &peers)?;
        let stream = dict(vec![
            ("audioFormat", Value::Int(ALAC_44100_16_2)),
            ("audioMode", Value::String("default".into())),
            ("controlPort", Value::Int(i64::from(setup.control_port))),
            ("ct", Value::Int(CT_ALAC)),
            ("isMedia", Value::Bool(true)),
            ("latencyMax", Value::Int(LATENCY_MAX)),
            ("latencyMin", Value::Int(i64::from(setup.min_latency))),
            ("shk", Value::Data(ap2.shk.to_vec())),
            ("spf", Value::Int(FRAMES as i64)),
            ("sr", Value::Int(i64::from(RATE))),
            ("type", Value::Int(TYPE_REALTIME)),
            ("supportsDynamicStreamID", Value::Bool(false)),
            ("streamConnectionID", Value::Int(i64::from(session))),
        ]);
        let answer = ap2.plist(
            "SETUP",
            None,
            &dict(vec![("streams", Value::Array(vec![stream]))]),
        )?;
        check(&answer, "SETUP")?;
        let ports = stream_ports(&answer)?;
        let info = format!("seq={};rtptime={}", setup.first_seq, setup.first_rtptime);
        let extra = [("Range", "npt=0-".to_string()), ("RTP-Info", info)];
        let record = ap2.send("RECORD", None, None, &[], &extra)?;
        check(&record, "RECORD")?;
        ap2.audio_latency = record
            .header("Audio-Latency")
            .and_then(|v| v.parse::<u32>().ok());
        // The receiver plays without these; a refusal is not a failure.
        if let Some(db) = setup.volume_db {
            let _ = ap2.volume(db);
        }
        let _ = ap2.parameter("progress: 0/0/0\r\n");
        Ok((ap2, ports))
    }

    fn plist(&mut self, method: &str, url: Option<&str>, v: &Value) -> Result<Response, RtspError> {
        self.send(method, url, Some(BPLIST), &bplist::encode(v), &[])
    }

    fn parameter(&mut self, text: &str) -> Result<(), RtspError> {
        let answer = self.send(
            "SET_PARAMETER",
            None,
            Some("text/parameters"),
            text.as_bytes(),
            &[],
        )?;
        check(&answer, "SET_PARAMETER")
    }

    /// Sends a request to the session's URL, or to `url`, with the
    /// session's headers and `extra`, and waits for the answer.
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

    /// Whether the receiver has closed the session's connection.
    pub fn closed(&mut self) -> bool {
        self.conn.closed()
    }

    /// Answers what the receiver sent on the event channel. False once it
    /// has closed the channel.
    pub fn poll_events(&mut self) -> bool {
        self.events.as_mut().is_none_or(Events::poll)
    }

    /// Keeps the session alive, without waiting for the answer.
    pub fn feedback(&mut self) -> Result<(), RtspError> {
        self.conn.send(&Request {
            method: "POST",
            url: "/feedback",
            headers: &self.headers,
            content_type: None,
            body: &[],
        })
    }

    /// Sets the receiver's volume, -30 to 0 dB.
    pub fn volume(&mut self, db: f32) -> Result<(), RtspError> {
        self.parameter(&format!("volume: {db}\r\n"))
    }

    /// Ends the session. An answer is not waited for beyond 1 s.
    pub fn teardown(mut self) {
        self.conn.set_timeout(std::time::Duration::from_secs(1));
        let _ = self.send("TEARDOWN", None, None, &[], &[]);
    }
}

fn dict(items: Vec<(&str, Value)>) -> Value {
    Value::Dict(items.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Any status but 200 as an error naming the request.
fn check(answer: &Response, what: &str) -> Result<(), RtspError> {
    if answer.status == 200 {
        return Ok(());
    }
    Err(RtspError::Status {
        method: what.to_string(),
        status: answer.status,
    })
}

/// The ports of the first stream in the stream SETUP's answer.
fn stream_ports(answer: &Response) -> Result<Ports, RtspError> {
    let body = bplist::decode(&answer.body).map_err(|e| RtspError::Malformed(e.to_string()))?;
    let stream = body
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|s| s.first())
        .ok_or_else(|| RtspError::Malformed("the stream SETUP gave no stream".into()))?;
    let port = |name: &str| {
        stream
            .get(name)
            .and_then(Value::as_int)
            .and_then(|p| u16::try_from(p).ok())
            .filter(|&p| p > 0)
            .ok_or_else(|| RtspError::Malformed(format!("the stream SETUP gave no {name}")))
    };
    Ok(Ports {
        server: port("dataPort")?,
        control: port("controlPort")?,
        timing: None,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::airplay::ap2::channel::{Opener, Sealer};
    use crate::airplay::ap2::pair::{PIN, tests::pairing};
    use crate::airplay::fake::{Answer, FakeReceiver, Seen};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn dict(items: &[(&str, Value)]) -> Value {
        Value::Dict(
            items
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    fn plist(v: &Value) -> Answer {
        Answer::Body(
            200,
            vec![("Content-Type", BPLIST.into())],
            bplist::encode(v),
        )
    }

    /// The event side of a fake receiver: whether the sender connected,
    /// and the answers it gave to the event sent on connecting.
    #[derive(Default)]
    pub struct FakeEvents {
        pub connected: bool,
        pub answers: String,
    }

    /// A third-party AirPlay 2 receiver that takes everything: its audio
    /// at `data_port`, `status_flags` in `/info`. Like a sound bar, it
    /// answers RECORD only once the sender has opened its event port.
    pub fn ap2_receiver(
        data_port: u16,
        status_flags: i64,
    ) -> impl Fn(&Seen) -> Answer + Send + 'static {
        ap2_receiver_with(data_port, status_flags, EventPort::Open).0
    }

    /// What a fake receiver does with its event port.
    #[derive(Clone, Copy, PartialEq)]
    pub enum EventPort {
        None,
        /// It sends one event on it and keeps it open.
        Open,
        /// It sends one event and closes it half a second later.
        Closing,
    }

    /// As `ap2_receiver`, with or without an event port; on connecting,
    /// the receiver sends one event on it.
    pub fn ap2_receiver_with(
        data_port: u16,
        status_flags: i64,
        port: EventPort,
    ) -> (
        impl Fn(&Seen) -> Answer + Send + 'static,
        Arc<Mutex<FakeEvents>>,
    ) {
        let events = Arc::new(Mutex::new(FakeEvents::default()));
        let key: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();
        let mut event_port = 0;
        if port != EventPort::None {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            event_port = listener.local_addr().unwrap().port();
            let (events, key) = (events.clone(), key.clone());
            std::thread::spawn(move || {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                events.lock().unwrap().connected = true;
                let secret = key.lock().unwrap().clone().expect("paired before");
                let keys = Keys::events(&secret);
                let (mut sealer, mut opener) = (Sealer::new(&keys.read), Opener::new(&keys.write));
                let mut out = Vec::new();
                sealer.seal(b"POST /command RTSP/1.0\r\nCSeq: 1\r\n\r\n", &mut out);
                let _ = stream.write_all(&out);
                if port == EventPort::Closing {
                    std::thread::sleep(Duration::from_millis(500));
                    return;
                }
                let (mut raw, mut plain, mut buf) = (Vec::new(), Vec::new(), [0u8; 512]);
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    opener.open(&mut raw, &mut plain).unwrap();
                    events.lock().unwrap().answers = String::from_utf8_lossy(&plain).into();
                }
            });
        }
        let shared = events.clone();
        let script = move |seen: &Seen| {
            if let Some(answer) = pairing(seen, PIN) {
                if let Answer::SealAfter(_, secret) = &answer {
                    *key.lock().unwrap() = Some(secret.clone());
                }
                return answer;
            }
            match (seen.method.as_str(), seen.url.as_str()) {
                ("GET", "/info") => plist(&dict(&[
                    ("statusFlags", Value::Int(status_flags)),
                    ("features", Value::Int(1 << 48)),
                ])),
                ("SETUP", _) => {
                    let body = bplist::decode(&seen.body).unwrap();
                    if body.get("streams").is_some() {
                        plist(&dict(&[(
                            "streams",
                            Value::Array(vec![dict(&[
                                ("type", Value::Int(96)),
                                ("dataPort", Value::Int(i64::from(data_port))),
                                ("controlPort", Value::Int(6011)),
                            ])]),
                        )]))
                    } else {
                        let Answer::Body(status, mut headers, body) = plist(&dict(&[
                            ("eventPort", Value::Int(i64::from(event_port))),
                            ("timingPort", Value::Int(0)),
                        ])) else {
                            unreachable!()
                        };
                        headers.push(("Session", "1A2B".into()));
                        Answer::Body(status, headers, body)
                    }
                }
                ("RECORD", _) if event_port != 0 && !shared.lock().unwrap().connected => {
                    Answer::Status(503, Vec::new())
                }
                ("RECORD", _) => Answer::Status(200, vec![("Audio-Latency", "11025".into())]),
                _ => Answer::Status(200, Vec::new()),
            }
        };
        (script, events)
    }

    fn setup() -> Setup {
        Setup {
            control_port: 6001,
            timing_port: 6002,
            first_seq: 5,
            first_rtptime: 77,
            volume_db: Some(-12.5),
            min_latency: crate::airplay::raop::MIN_LATENCY,
        }
    }

    fn open_with(
        script: impl Fn(&Seen) -> Answer + Send + 'static,
    ) -> (Result<(Ap2, Ports), RtspError>, FakeReceiver) {
        open_as(script, &setup())
    }

    fn open_as(
        script: impl Fn(&Seen) -> Answer + Send + 'static,
        setup: &Setup,
    ) -> (Result<(Ap2, Ports), RtspError>, FakeReceiver) {
        let receiver = FakeReceiver::start(script);
        let conn = Conn::connect("127.0.0.1", receiver.port, Duration::from_secs(2)).unwrap();
        (Ap2::open(conn, setup), receiver)
    }

    #[test]
    fn the_stream_setup_offers_the_minimum_latency_it_is_given() {
        let short = Setup {
            min_latency: 4410,
            ..setup()
        };
        let (opened, receiver) = open_as(ap2_receiver(6000, 0x4), &short);
        opened.unwrap().0.teardown();
        let seen = receiver.finish();
        let body = bplist::decode(&seen[5].body).unwrap();
        let stream = &body.get("streams").and_then(Value::as_array).unwrap()[0];
        assert_eq!(stream.get("latencyMin").and_then(Value::as_int), Some(4410));
    }

    fn calls(seen: &[Seen]) -> Vec<(String, String)> {
        seen.iter()
            .map(|s| (s.method.clone(), s.url.clone()))
            .collect()
    }

    #[test]
    fn the_ap2_handshake_runs_in_order_on_the_sealed_channel() {
        let (opened, receiver) = open_with(ap2_receiver(6000, 0x4));
        let (mut ap2, ports) = opened.unwrap();
        assert_eq!(
            ports,
            Ports {
                server: 6000,
                control: 6011,
                timing: None
            }
        );
        assert_eq!(ap2.audio_latency, Some(11025));
        ap2.feedback().unwrap();
        ap2.teardown();
        let seen = receiver.finish();
        let url = seen[3].url.clone();
        assert!(url.starts_with("rtsp://127.0.0.1/"), "{url}");
        let want: Vec<(String, String)> = [
            ("GET", "/info"),
            ("POST", "/pair-setup"),
            ("POST", "/pair-setup"),
            ("SETUP", url.as_str()),
            ("SETPEERS", "/peer-list-changed"),
            ("SETUP", url.as_str()),
            ("RECORD", url.as_str()),
            ("SET_PARAMETER", url.as_str()),
            ("SET_PARAMETER", url.as_str()),
            ("POST", "/feedback"),
            ("TEARDOWN", url.as_str()),
        ]
        .iter()
        .map(|(m, u)| (m.to_string(), u.to_string()))
        .collect();
        assert_eq!(calls(&seen), want);
        for s in &seen {
            assert_eq!(s.header("User-Agent"), Some(USER_AGENT), "{}", s.method);
            assert_eq!(s.header("DACP-ID").map(str::len), Some(16), "{}", s.method);
        }
        // HKP only while pairing.
        assert_eq!(seen[3].header("X-Apple-HKP"), None);
        assert_eq!(seen[6].header("RTP-Info"), Some("seq=5;rtptime=77"));
        // The session SETUP's Session goes with everything after it.
        assert_eq!(seen[3].header("Session"), None);
        for s in &seen[4..] {
            assert_eq!(s.header("Session"), Some("1A2B"), "{}", s.method);
        }
        assert_eq!(seen[6].header("Range"), Some("npt=0-"));
        assert_eq!(seen[7].body, b"volume: -12.5\r\n");
        assert_eq!(seen[8].body, b"progress: 0/0/0\r\n");
        // SETPEERS names both ends.
        assert_eq!(seen[4].header("Content-Type"), Some(BPLIST));
        assert_eq!(
            bplist::decode(&seen[4].body).unwrap(),
            Value::Array(vec![
                Value::String("127.0.0.1".into()),
                Value::String("127.0.0.1".into())
            ])
        );
    }

    #[test]
    fn the_session_setup_asks_for_ntp_timing_at_our_port() {
        let (opened, receiver) = open_with(ap2_receiver(6000, 0x4));
        opened.unwrap().0.teardown();
        let seen = receiver.finish();
        assert_eq!(seen[3].header("Content-Type"), Some(BPLIST));
        let body = bplist::decode(&seen[3].body).unwrap();
        assert_eq!(
            body.get("timingProtocol"),
            Some(&Value::String("NTP".into()))
        );
        assert_eq!(body.get("timingPort"), Some(&Value::Int(6002)));
        let Some(Value::String(uuid)) = body.get("sessionUUID") else {
            panic!("no sessionUUID: {body:?}");
        };
        assert_eq!(uuid.len(), 36);
        let Some(Value::String(id)) = body.get("deviceID") else {
            panic!("no deviceID: {body:?}");
        };
        assert_eq!(id.len(), 17);
    }

    #[test]
    fn the_stream_setup_asks_for_realtime_alac_with_the_stream_key() {
        let (opened, receiver) = open_with(ap2_receiver(6000, 0x4));
        let (ap2, _) = opened.unwrap();
        let shk = ap2.shk;
        ap2.teardown();
        let seen = receiver.finish();
        let body = bplist::decode(&seen[5].body).unwrap();
        let stream = &body.get("streams").and_then(Value::as_array).unwrap()[0];
        let int = |k: &str| stream.get(k).and_then(Value::as_int);
        assert_eq!(int("type"), Some(0x60));
        assert_eq!(int("ct"), Some(2));
        assert_eq!(int("audioFormat"), Some(0x40000));
        assert_eq!(int("spf"), Some(352));
        assert_eq!(int("sr"), Some(44100));
        assert_eq!(int("latencyMin"), Some(11025));
        assert_eq!(int("latencyMax"), Some(88200));
        assert_eq!(int("controlPort"), Some(6001));
        assert_eq!(stream.get("shk"), Some(&Value::Data(shk.to_vec())));
        assert_eq!(stream.get("isMedia"), Some(&Value::Bool(true)));
        assert_eq!(
            stream.get("audioMode"),
            Some(&Value::String("default".into()))
        );
        // The session id of the URL.
        let url_session: i64 = seen[5].url.rsplit('/').next().unwrap().parse().unwrap();
        assert_eq!(int("streamConnectionID"), Some(url_session));
    }

    #[test]
    fn a_receiver_that_wants_a_pin_is_refused_before_pairing() {
        for flags in [PIN_REQUIRED, ONE_TIME_PAIRING_REQUIRED] {
            let (opened, receiver) = open_with(ap2_receiver(6000, flags | 0x4));
            let Err(RtspError::Pairing(why)) = opened else {
                panic!("{flags:#x} opened");
            };
            assert!(why.contains("PIN") || why.contains("HomeKit"), "{why}");
            drop(receiver);
        }
    }

    #[test]
    fn a_stream_setup_without_a_data_port_is_an_error() {
        let base = ap2_receiver(6000, 0x4);
        let (opened, _receiver) = open_with(move |seen: &Seen| {
            if seen.method == "SETUP"
                && bplist::decode(&seen.body).unwrap().get("streams").is_some()
            {
                return plist(&dict(&[("streams", Value::Array(vec![dict(&[])]))]));
            }
            base(seen)
        });
        assert!(matches!(opened, Err(RtspError::Malformed(_))));
    }

    #[test]
    fn the_event_channel_is_open_before_record_and_its_events_are_answered() {
        let (script, events) = ap2_receiver_with(6000, 0x4, EventPort::Open);
        let (opened, _receiver) = open_with(script);
        let (mut ap2, _) = opened.unwrap();
        assert!(events.lock().unwrap().connected);
        for _ in 0..100 {
            assert!(ap2.poll_events());
            if !events.lock().unwrap().answers.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            events.lock().unwrap().answers,
            "RTSP/1.0 200 OK\r\nServer: aas\r\nCSeq: 1\r\n\r\n"
        );
    }

    #[test]
    fn a_receiver_without_an_event_port_plays_all_the_same() {
        let (script, events) = ap2_receiver_with(6000, 0x4, EventPort::None);
        let (opened, _receiver) = open_with(script);
        let (mut ap2, _) = opened.unwrap();
        assert!(!events.lock().unwrap().connected);
        assert!(ap2.poll_events());
    }

    #[test]
    fn an_event_port_that_refuses_is_an_error() {
        let gone = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = gone.local_addr().unwrap().port();
        drop(gone);
        let base = ap2_receiver(6000, 0x4);
        let (opened, _receiver) = open_with(move |seen: &Seen| {
            if seen.method == "SETUP"
                && bplist::decode(&seen.body).unwrap().get("streams").is_none()
            {
                return plist(&dict(&[("eventPort", Value::Int(i64::from(port)))]));
            }
            base(seen)
        });
        let Err(RtspError::Io(why)) = opened else {
            panic!("opened");
        };
        assert!(why.contains("event port"), "{why}");
    }

    #[test]
    fn a_refused_setpeers_is_not_a_failure() {
        let base = ap2_receiver(6000, 0x4);
        let (opened, _receiver) = open_with(move |seen: &Seen| {
            if seen.method == "SETPEERS" {
                return Answer::Status(501, Vec::new());
            }
            base(seen)
        });
        assert!(opened.is_ok());
    }
}
