//! Transient pairing, as third-party receivers take it: `POST /pair-setup`
//! twice with TLV8 bodies, SRP-6a with the user "Pair-Setup" and the PIN
//! 3939, and a shared key of 64 bytes at the end, which seals the control
//! channel and nothing else is kept.

use super::srp::{Client, Group};
use super::tlv;
use crate::airplay::raop::random;
use crate::airplay::rtsp::{Conn, Request, RtspError};
use sha2::Sha512;

pub const USER: &str = "Pair-Setup";
pub const PIN: &[u8] = b"3939";

/// Pairs on `conn`, sending `headers` with each request, and gives the
/// shared key.
pub fn pair_transient(
    conn: &mut Conn,
    headers: &[(&'static str, String)],
) -> Result<Vec<u8>, RtspError> {
    let group = Group::rfc5054_3072();
    let secret = random::<32>();
    let client = Client::new(&group, USER, PIN, &secret);
    // M1: state 1, the pair-setup method, the transient flag.
    let m2 = post(
        conn,
        headers,
        &[
            (tlv::STATE, &[1]),
            (tlv::METHOD, &[0]),
            (tlv::FLAGS, &[0x10]),
        ],
    )?;
    let salt = tlv::find(&m2, tlv::SALT).ok_or_else(|| missing("a salt"))?;
    let b = tlv::find(&m2, tlv::PUBLIC_KEY).ok_or_else(|| missing("a public key"))?;
    let proofs = client
        .respond::<Sha512>(salt, b)
        .map_err(|e| RtspError::Pairing(e.to_string()))?;
    let m4 = post(
        conn,
        headers,
        &[
            (tlv::STATE, &[3]),
            (tlv::PUBLIC_KEY, client.public()),
            (tlv::PROOF, &proofs.client),
        ],
    )?;
    if tlv::find(&m4, tlv::PROOF) != Some(proofs.server.as_slice()) {
        return Err(RtspError::Pairing("the receiver's proof is wrong".into()));
    }
    Ok(proofs.key)
}

fn missing(what: &str) -> RtspError {
    RtspError::Malformed(format!("pair-setup gave no {what}"))
}

/// Posts one pairing message and gives the items of the answer, or the
/// receiver's error as one.
fn post(
    conn: &mut Conn,
    headers: &[(&'static str, String)],
    items: &[(u8, &[u8])],
) -> Result<Vec<(u8, Vec<u8>)>, RtspError> {
    let mut all = headers.to_vec();
    all.push(("X-Apple-HKP", "4".into()));
    let body = tlv::encode(items);
    let answer = conn.request(&Request {
        method: "POST",
        url: "/pair-setup",
        headers: &all,
        content_type: Some("application/octet-stream"),
        body: &body,
    })?;
    if answer.status != 200 {
        return Err(RtspError::Status {
            method: "POST /pair-setup".into(),
            status: answer.status,
        });
    }
    let items = tlv::decode(&answer.body).map_err(|e| RtspError::Malformed(e.to_string()))?;
    if let Some(code) = tlv::find(&items, tlv::ERROR) {
        let code = code.first().copied().unwrap_or(0);
        let why = match code {
            2 => "authentication",
            3 => "try again later",
            4 => "too many controllers",
            5 => "too many tries",
            6 => "unavailable",
            7 => "busy",
            _ => "unknown",
        };
        return Err(RtspError::Pairing(format!(
            "the receiver refused it (error {code}: {why})"
        )));
    }
    Ok(items)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::airplay::ap2::srp::tests::Server;
    use crate::airplay::fake::{Answer, FakeReceiver, Seen};
    use std::time::Duration;

    pub const SALT: [u8; 16] = [0x5a; 16];
    pub const B_SECRET: [u8; 32] = [0x3c; 32];

    /// A receiver's answers to `/pair-setup`, with `pin` as its PIN; M4
    /// seals the channel. `None` for anything else.
    pub fn pairing(seen: &Seen, pin: &'static [u8]) -> Option<Answer> {
        if seen.url != "/pair-setup" {
            return None;
        }
        let group = Group::rfc5054_3072();
        let server = Server::new::<Sha512>(&group, USER, pin, &SALT, &B_SECRET);
        let items = tlv::decode(&seen.body).unwrap();
        let body = |items: &[(u8, &[u8])]| {
            Answer::Body(
                200,
                vec![("Content-Type", "application/octet-stream".into())],
                tlv::encode(items),
            )
        };
        Some(match tlv::find(&items, tlv::STATE) {
            Some([1]) => {
                let b = server.public::<Sha512>().to_bytes_be();
                body(&[
                    (tlv::STATE, &[2]),
                    (tlv::SALT, &SALT),
                    (tlv::PUBLIC_KEY, &b),
                ])
            }
            Some([3]) => {
                let a = tlv::find(&items, tlv::PUBLIC_KEY).unwrap();
                let proofs = server.proofs::<Sha512>(USER, &SALT, a);
                if tlv::find(&items, tlv::PROOF) != Some(proofs.client.as_slice()) {
                    body(&[(tlv::STATE, &[4]), (tlv::ERROR, &[2])])
                } else {
                    Answer::SealAfter(
                        Box::new(body(&[(tlv::STATE, &[4]), (tlv::PROOF, &proofs.server)])),
                        proofs.key,
                    )
                }
            }
            other => panic!("pair-setup in state {other:?}"),
        })
    }

    fn headers() -> Vec<(&'static str, String)> {
        vec![("User-Agent", "AirPlay/999.0.0".into())]
    }

    fn connect(receiver: &FakeReceiver) -> Conn {
        Conn::connect("127.0.0.1", receiver.port, Duration::from_secs(2)).unwrap()
    }

    #[test]
    fn pairing_gives_the_receivers_key_and_seals_nothing_itself() {
        let receiver = FakeReceiver::start(|seen| {
            pairing(seen, PIN).unwrap_or(Answer::Status(200, Vec::new()))
        });
        let mut conn = connect(&receiver);
        let key = pair_transient(&mut conn, &headers()).unwrap();
        assert_eq!(key.len(), 64);
        drop(conn);
        let seen = receiver.finish();
        assert_eq!(seen.len(), 2);
        for s in &seen {
            assert_eq!((s.method.as_str(), s.url.as_str()), ("POST", "/pair-setup"));
            assert_eq!(s.header("X-Apple-HKP"), Some("4"));
            assert_eq!(s.header("Content-Type"), Some("application/octet-stream"));
            assert_eq!(s.header("User-Agent"), Some("AirPlay/999.0.0"));
        }
        // M1: state 1, method 0 (pair-setup), flags 0x10 (transient).
        assert_eq!(seen[0].body, [6, 1, 1, 0, 1, 0, 19, 1, 0x10]);
        let m3 = tlv::decode(&seen[1].body).unwrap();
        assert_eq!(tlv::find(&m3, tlv::STATE), Some(&[3u8][..]));
        assert_eq!(tlv::find(&m3, tlv::PROOF).map(<[u8]>::len), Some(64));
        // The key is the receiver's.
        let group = Group::rfc5054_3072();
        let server = Server::new::<Sha512>(&group, USER, PIN, &SALT, &B_SECRET);
        let a = tlv::find(&m3, tlv::PUBLIC_KEY).unwrap();
        assert_eq!(server.proofs::<Sha512>(USER, &SALT, a).key, key);
    }

    #[test]
    fn a_receiver_with_another_pin_refuses() {
        let receiver = FakeReceiver::start(|seen| {
            pairing(seen, b"1234").unwrap_or(Answer::Status(200, Vec::new()))
        });
        let e = pair_transient(&mut connect(&receiver), &headers()).unwrap_err();
        assert_eq!(
            e,
            RtspError::Pairing("the receiver refused it (error 2: authentication)".into())
        );
    }

    #[test]
    fn a_wrong_m4_proof_is_an_error() {
        let receiver = FakeReceiver::start(|seen| match pairing(seen, PIN) {
            Some(Answer::SealAfter(..)) => Answer::Body(
                200,
                Vec::new(),
                tlv::encode(&[(tlv::STATE, &[4]), (tlv::PROOF, &[0; 64])]),
            ),
            Some(other) => other,
            None => Answer::Status(200, Vec::new()),
        });
        let e = pair_transient(&mut connect(&receiver), &headers()).unwrap_err();
        assert_eq!(
            e,
            RtspError::Pairing("the receiver's proof is wrong".into())
        );
    }

    #[test]
    fn a_status_other_than_200_and_a_missing_salt_are_errors() {
        let receiver = FakeReceiver::start(|_| Answer::Status(470, Vec::new()));
        let e = pair_transient(&mut connect(&receiver), &headers()).unwrap_err();
        assert_eq!(
            e,
            RtspError::Status {
                method: "POST /pair-setup".into(),
                status: 470
            }
        );
        let receiver = FakeReceiver::start(|_| {
            Answer::Body(
                200,
                Vec::new(),
                tlv::encode(&[(tlv::STATE, &[2]), (tlv::PUBLIC_KEY, &[9])]),
            )
        });
        let e = pair_transient(&mut connect(&receiver), &headers()).unwrap_err();
        assert!(matches!(e, RtspError::Malformed(_)), "{e:?}");
        // A receiver that wants to back off says so.
        let receiver = FakeReceiver::start(|_| {
            Answer::Body(
                200,
                Vec::new(),
                tlv::encode(&[(tlv::STATE, &[2]), (tlv::ERROR, &[3])]),
            )
        });
        let e = pair_transient(&mut connect(&receiver), &headers()).unwrap_err();
        assert_eq!(
            e,
            RtspError::Pairing("the receiver refused it (error 3: try again later)".into())
        );
    }
}
