//! HomeKit pairing over AirPlay 2: pair-setup with the PIN a receiver
//! shows, once, and pair-verify with the kept keys at every session. See
//! `doc/plans/2026-11-22-airplay-pin-pairing-design.md`.

use super::channel::derive;
use super::keys::{Identity, Peer};
use super::pair::{USER, exchange};
use super::srp::{Client, Group};
use super::tlv;
use crate::airplay::raop::random;
use crate::airplay::rtsp::{Conn, Request, RtspError};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey};
use sha2::Sha512;
use x25519_dalek::{PublicKey, StaticSecret};

/// `X-Apple-HKP` of pair-setup and pair-verify that keep their keys.
pub const HKP: &str = "3";

/// What M2 of pair-setup gave, kept until the PIN is known.
pub struct Started {
    secret: [u8; 32],
    salt: Vec<u8>,
    b: Vec<u8>,
}

/// Makes the receiver show its PIN.
pub fn pin_start(conn: &mut Conn, headers: &[(&'static str, String)]) -> Result<(), RtspError> {
    let mut all = headers.to_vec();
    all.push(("X-Apple-HKP", HKP.to_string()));
    let answer = conn.request(&Request {
        method: "POST",
        url: "/pair-pin-start",
        headers: &all,
        content_type: None,
        body: &[],
    })?;
    if answer.status != 200 {
        return Err(RtspError::Status {
            method: "POST /pair-pin-start".into(),
            status: answer.status,
        });
    }
    Ok(())
}

/// M1 and M2 of pair-setup.
pub fn setup_start(
    conn: &mut Conn,
    headers: &[(&'static str, String)],
) -> Result<Started, RtspError> {
    let m2 = exchange(
        conn,
        headers,
        "/pair-setup",
        HKP,
        &[(tlv::STATE, &[1]), (tlv::METHOD, &[0])],
    )?;
    refused(&m2)?;
    let salt = tlv::find(&m2, tlv::SALT).ok_or_else(|| missing("a salt"))?;
    let b = tlv::find(&m2, tlv::PUBLIC_KEY).ok_or_else(|| missing("a public key"))?;
    Ok(Started {
        secret: random(),
        salt: salt.to_vec(),
        b: b.to_vec(),
    })
}

/// M3 to M6 of pair-setup with `pin`: the receiver's keys.
pub fn setup_finish(
    conn: &mut Conn,
    headers: &[(&'static str, String)],
    started: Started,
    pin: &str,
    me: &Identity,
) -> Result<Peer, RtspError> {
    let group = Group::rfc5054_3072();
    let client = Client::new(&group, USER, pin.as_bytes(), &started.secret);
    let proofs = client
        .respond::<Sha512>(&started.salt, &started.b)
        .map_err(|e| RtspError::Pairing(e.to_string()))?;
    let m4 = exchange(
        conn,
        headers,
        "/pair-setup",
        HKP,
        &[
            (tlv::STATE, &[3]),
            (tlv::PUBLIC_KEY, client.public()),
            (tlv::PROOF, &proofs.client),
        ],
    )
    .map_err(|e| match e {
        RtspError::Status { .. } => RtspError::WrongPin,
        e => e,
    })?;
    if tlv::find(&m4, tlv::ERROR).is_some()
        || tlv::find(&m4, tlv::PROOF) != Some(proofs.server.as_slice())
    {
        return Err(RtspError::WrongPin);
    }
    let k = &proofs.key;
    // M5: our identity, signed, sealed under the SRP key.
    let x = derive(
        k,
        "Pair-Setup-Controller-Sign-Salt",
        "Pair-Setup-Controller-Sign-Info",
    );
    let ltpk = me.signing.verifying_key().to_bytes();
    let signed = [&x[..], me.id.as_bytes(), &ltpk].concat();
    let signature = me.signing.sign(&signed).to_bytes();
    let plain = tlv::encode(&[
        (tlv::IDENTIFIER, me.id.as_bytes()),
        (tlv::PUBLIC_KEY, &ltpk),
        (tlv::SIGNATURE, &signature),
    ]);
    let enc = derive(k, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
    let sealed = seal_once(&enc, b"PS-Msg05", &plain);
    let m6 = exchange(
        conn,
        headers,
        "/pair-setup",
        HKP,
        &[(tlv::STATE, &[5]), (tlv::ENCRYPTED_DATA, &sealed)],
    )?;
    refused(&m6)?;
    // M6: the receiver's identity, signed.
    let sealed = tlv::find(&m6, tlv::ENCRYPTED_DATA).ok_or_else(|| missing("its keys"))?;
    let plain = open_once(&enc, b"PS-Msg06", sealed)
        .ok_or_else(|| RtspError::Pairing("the receiver's keys do not open".into()))?;
    let sub = tlv::decode(&plain).map_err(|e| RtspError::Malformed(e.to_string()))?;
    let id = tlv::find(&sub, tlv::IDENTIFIER).ok_or_else(|| missing("an identifier"))?;
    let public: [u8; 32] = tlv::find(&sub, tlv::PUBLIC_KEY)
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| missing("a public key"))?;
    let signature = tlv::find(&sub, tlv::SIGNATURE).ok_or_else(|| missing("a signature"))?;
    let ax = derive(
        k,
        "Pair-Setup-Accessory-Sign-Salt",
        "Pair-Setup-Accessory-Sign-Info",
    );
    let signed = [&ax[..], id, &public].concat();
    if !signed_by(&public, &signed, signature) {
        return Err(RtspError::Pairing(
            "the receiver's signature is wrong".into(),
        ));
    }
    Ok(Peer {
        id: String::from_utf8_lossy(id).into_owned(),
        public,
    })
}

/// Pair-verify with the kept keys: the X25519 shared secret, which seals
/// the control channel as the transient key does.
pub fn verify(
    conn: &mut Conn,
    headers: &[(&'static str, String)],
    me: &Identity,
    peer: &Peer,
) -> Result<[u8; 32], RtspError> {
    let secret = StaticSecret::from(random::<32>());
    let ours = PublicKey::from(&secret).to_bytes();
    let m2 = exchange(
        conn,
        headers,
        "/pair-verify",
        HKP,
        &[(tlv::STATE, &[1]), (tlv::PUBLIC_KEY, &ours)],
    )
    .map_err(refused_by_status)?;
    if tlv::find(&m2, tlv::ERROR).is_some() {
        return Err(RtspError::PairingRefused);
    }
    let theirs: [u8; 32] = tlv::find(&m2, tlv::PUBLIC_KEY)
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| missing("a public key"))?;
    let shared = secret.diffie_hellman(&PublicKey::from(theirs)).to_bytes();
    let enc = derive(
        &shared,
        "Pair-Verify-Encrypt-Salt",
        "Pair-Verify-Encrypt-Info",
    );
    let sealed = tlv::find(&m2, tlv::ENCRYPTED_DATA).ok_or_else(|| missing("its proof"))?;
    let plain = open_once(&enc, b"PV-Msg02", sealed).ok_or(RtspError::PairingRefused)?;
    let sub = tlv::decode(&plain).map_err(|e| RtspError::Malformed(e.to_string()))?;
    let id = tlv::find(&sub, tlv::IDENTIFIER).ok_or_else(|| missing("an identifier"))?;
    let signature = tlv::find(&sub, tlv::SIGNATURE).ok_or_else(|| missing("a signature"))?;
    let signed = [&theirs[..], id, &ours].concat();
    if id != peer.id.as_bytes() || !signed_by(&peer.public, &signed, signature) {
        return Err(RtspError::PairingRefused);
    }
    let signed = [&ours[..], me.id.as_bytes(), &theirs].concat();
    let signature = me.signing.sign(&signed).to_bytes();
    let plain = tlv::encode(&[
        (tlv::IDENTIFIER, me.id.as_bytes()),
        (tlv::SIGNATURE, &signature),
    ]);
    let sealed = seal_once(&enc, b"PV-Msg03", &plain);
    let m4 = exchange(
        conn,
        headers,
        "/pair-verify",
        HKP,
        &[(tlv::STATE, &[3]), (tlv::ENCRYPTED_DATA, &sealed)],
    )
    .map_err(refused_by_status)?;
    if tlv::find(&m4, tlv::ERROR).is_some() {
        return Err(RtspError::PairingRefused);
    }
    Ok(shared)
}

/// A status other than 200 to pair-verify refuses the pairing, as an error
/// item does.
fn refused_by_status(e: RtspError) -> RtspError {
    match e {
        RtspError::Status { .. } => RtspError::PairingRefused,
        e => e,
    }
}

/// Whether `signature` over `message` is by the Ed25519 key `public`.
fn signed_by(public: &[u8; 32], message: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(signature) else {
        return false;
    };
    key.verify(message, &signature).is_ok()
}

/// What a refusal without a reason of its own says: the receiver's access
/// setting keeps aas out.
pub const ACCESS_REFUSED: &str =
    "the receiver refused pairing: allow AirPlay access for everyone on the network";

/// The receiver's error item, if it sent one, as what it means.
fn refused(items: &[(u8, Vec<u8>)]) -> Result<(), RtspError> {
    let Some(code) = tlv::find(items, tlv::ERROR) else {
        return Ok(());
    };
    let why = match code.first().copied().unwrap_or(0) {
        3 | 7 => "the receiver is busy: try again in a moment",
        5 => "the receiver refuses after too many tries: restart it, then pair again",
        6 => "the receiver cannot pair now: try again in a moment",
        _ => ACCESS_REFUSED,
    };
    Err(RtspError::Pairing(why.into()))
}

fn missing(what: &str) -> RtspError {
    RtspError::Malformed(format!("pairing gave no {what}"))
}

/// The 12-byte nonce of a pairing message: four zero bytes, then its label.
fn nonce(label: &[u8; 8]) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(label);
    Nonce::from(n)
}

/// Seals one pairing message under `key`, its tag appended.
pub fn seal_once(key: &[u8; 32], label: &[u8; 8], plain: &[u8]) -> Vec<u8> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(&nonce(label), plain)
        .expect("a pairing message seals")
}

/// Opens one pairing message; `None` when it does not open.
pub fn open_once(key: &[u8; 32], label: &[u8; 8], sealed: &[u8]) -> Option<Vec<u8>> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(&nonce(label), sealed)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::super::fake_hap::FakeAccessory;
    use super::super::keys::Pairings;
    use super::*;
    use crate::airplay::fake::{Answer, FakeReceiver};
    use std::sync::Arc;
    use std::time::Duration;

    fn headers() -> Vec<(&'static str, String)> {
        vec![("User-Agent", "AirPlay/999.0.0".into())]
    }

    fn receiver(acc: &Arc<FakeAccessory>) -> FakeReceiver {
        let acc = acc.clone();
        FakeReceiver::start(move |seen| acc.answer(seen).unwrap_or(Answer::Status(200, Vec::new())))
    }

    fn conn(r: &FakeReceiver) -> Conn {
        Conn::connect("127.0.0.1", r.port, Duration::from_secs(2)).unwrap()
    }

    /// Pairs with `acc` on one connection and gives aas's identity and the
    /// receiver's keys.
    fn paired(acc: &Arc<FakeAccessory>) -> (Identity, Peer) {
        let r = receiver(acc);
        let mut c = conn(&r);
        let me = Pairings::new().identity();
        pin_start(&mut c, &headers()).unwrap();
        let started = setup_start(&mut c, &headers()).unwrap();
        let peer = setup_finish(&mut c, &headers(), started, "1234", &me).unwrap();
        (me, peer)
    }

    #[test]
    fn pairing_with_the_right_pin_gives_the_receivers_keys() {
        let acc = FakeAccessory::new("1234");
        let (_, peer) = paired(&acc);
        assert_eq!(peer, acc.peer());
    }

    #[test]
    fn a_wrong_pin_is_told_as_such() {
        let acc = FakeAccessory::new("1234");
        let r = receiver(&acc);
        let mut c = conn(&r);
        let me = Pairings::new().identity();
        let started = setup_start(&mut c, &headers()).unwrap();
        let e = setup_finish(&mut c, &headers(), started, "0000", &me).unwrap_err();
        assert!(matches!(e, RtspError::WrongPin), "{e}");
    }

    #[test]
    fn verify_after_pairing_gives_the_same_secret_on_both_sides() {
        let acc = FakeAccessory::new("1234");
        let (me, peer) = paired(&acc);
        let r = receiver(&acc);
        let mut c = conn(&r);
        let shared = verify(&mut c, &headers(), &me, &peer).unwrap();
        assert_eq!(Some(shared), acc.shared());
    }

    #[test]
    fn verify_with_a_forgotten_pairing_is_refused() {
        let acc = FakeAccessory::new("1234");
        let (me, peer) = paired(&acc);
        acc.forget();
        let r = receiver(&acc);
        let mut c = conn(&r);
        let e = verify(&mut c, &headers(), &me, &peer).unwrap_err();
        assert!(matches!(e, RtspError::PairingRefused), "{e}");
    }

    /// A receiver that answers `url` in state `state` with HTTP 470.
    fn refusing(acc: &Arc<FakeAccessory>, url: &'static str, state: u8) -> FakeReceiver {
        let acc = acc.clone();
        FakeReceiver::start(move |seen| {
            let items = tlv::decode(&seen.body).unwrap_or_default();
            if seen.url == url && tlv::find(&items, tlv::STATE) == Some(&[state][..]) {
                return Answer::Status(470, Vec::new());
            }
            acc.answer(seen).unwrap_or(Answer::Status(200, Vec::new()))
        })
    }

    #[test]
    fn a_verify_refused_by_status_is_a_refused_pairing() {
        let acc = FakeAccessory::new("1234");
        let (me, peer) = paired(&acc);
        let r = refusing(&acc, "/pair-verify", 1);
        let mut c = conn(&r);
        let e = verify(&mut c, &headers(), &me, &peer).unwrap_err();
        assert!(matches!(e, RtspError::PairingRefused), "{e}");
    }

    #[test]
    fn a_pin_refused_by_status_is_a_wrong_pin() {
        let acc = FakeAccessory::new("1234");
        let r = refusing(&acc, "/pair-setup", 3);
        let mut c = conn(&r);
        let me = Pairings::new().identity();
        let started = setup_start(&mut c, &headers()).unwrap();
        let e = setup_finish(&mut c, &headers(), started, "1234", &me).unwrap_err();
        assert!(matches!(e, RtspError::WrongPin), "{e}");
    }

    #[test]
    fn pair_setup_says_hkp_3() {
        let acc = FakeAccessory::new("1234");
        let (me, peer) = paired(&acc);
        let r = receiver(&acc);
        let mut c = conn(&r);
        verify(&mut c, &headers(), &me, &peer).unwrap();
        drop(c);
        for seen in r.finish() {
            if seen.url == "/pair-setup" || seen.url == "/pair-verify" {
                assert_eq!(seen.header("X-Apple-HKP"), Some("3"), "{}", seen.url);
            }
        }
    }

    #[test]
    fn a_message_seals_and_opens_with_its_label() {
        let key = [7u8; 32];
        let sealed = seal_once(&key, b"PS-Msg05", b"hello");
        assert_eq!(sealed.len(), 5 + 16);
        assert_eq!(
            open_once(&key, b"PS-Msg05", &sealed).as_deref(),
            Some(&b"hello"[..])
        );
        assert_eq!(open_once(&key, b"PS-Msg06", &sealed), None);
    }
}
