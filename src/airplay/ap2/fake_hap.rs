//! A fake receiver's side of HomeKit pairing for tests: pair-pin-start,
//! pair-setup with a known PIN, and pair-verify, after which the channel is
//! sealed with the verify secret.

use super::channel::derive;
use super::hap::{open_once, seal_once};
use super::keys::Peer;
use super::pair::USER;
use super::pair::tests::{B_SECRET, SALT};
use super::srp::Group;
use super::srp::tests::Server;
use super::tlv;
use crate::airplay::fake::{Answer, Seen};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::Sha512;
use std::sync::{Arc, Mutex};
use x25519_dalek::{PublicKey, StaticSecret};

const ID: &str = "FAKE-RECEIVER-ID";

#[derive(Default)]
struct State {
    /// The SRP key after M3, until M5.
    key: Option<Vec<u8>>,
    /// Controllers paired: id and Ed25519 public key.
    paired: Vec<(Vec<u8>, [u8; 32])>,
    /// Pair-verify under way: our X25519 key and the controller's.
    verify: Option<([u8; 32], [u8; 32])>,
    shared: Option<[u8; 32]>,
}

pub struct FakeAccessory {
    pin: &'static str,
    signing: SigningKey,
    state: Mutex<State>,
}

fn body(items: &[(u8, &[u8])]) -> Answer {
    Answer::Body(
        200,
        vec![("Content-Type", "application/octet-stream".into())],
        tlv::encode(items),
    )
}

fn error(state: u8) -> Answer {
    body(&[(tlv::STATE, &[state]), (tlv::ERROR, &[2])])
}

impl FakeAccessory {
    pub fn new(pin: &'static str) -> Arc<FakeAccessory> {
        Arc::new(FakeAccessory {
            pin,
            signing: SigningKey::from_bytes(&[0x42; 32]),
            state: Mutex::new(State::default()),
        })
    }

    /// The keys aas should keep for it.
    pub fn peer(&self) -> Peer {
        Peer {
            id: ID.into(),
            public: self.signing.verifying_key().to_bytes(),
        }
    }

    /// The secret of the last pair-verify.
    pub fn shared(&self) -> Option<[u8; 32]> {
        self.state.lock().unwrap().shared
    }

    /// Forgets every paired controller, as a reset does.
    pub fn forget(&self) {
        self.state.lock().unwrap().paired.clear();
    }

    /// Its answer to `seen`, or `None` for anything but pairing.
    pub fn answer(&self, seen: &Seen) -> Option<Answer> {
        match seen.url.as_str() {
            "/pair-pin-start" => Some(Answer::Status(200, Vec::new())),
            "/pair-setup" => Some(self.setup(seen)),
            "/pair-verify" => Some(self.verify(seen)),
            _ => None,
        }
    }

    fn setup(&self, seen: &Seen) -> Answer {
        let group = Group::rfc5054_3072();
        let server = Server::new::<Sha512>(&group, USER, self.pin.as_bytes(), &SALT, &B_SECRET);
        let items = tlv::decode(&seen.body).unwrap();
        let mut st = self.state.lock().unwrap();
        match tlv::find(&items, tlv::STATE) {
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
                    return error(4);
                }
                st.key = Some(proofs.key.clone());
                body(&[(tlv::STATE, &[4]), (tlv::PROOF, &proofs.server)])
            }
            Some([5]) => {
                let Some(k) = st.key.take() else {
                    return error(6);
                };
                let enc = derive(&k, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
                let sealed = tlv::find(&items, tlv::ENCRYPTED_DATA).unwrap();
                let Some(plain) = open_once(&enc, b"PS-Msg05", sealed) else {
                    return error(6);
                };
                let sub = tlv::decode(&plain).unwrap();
                let id = tlv::find(&sub, tlv::IDENTIFIER).unwrap().to_vec();
                let ltpk: [u8; 32] = tlv::find(&sub, tlv::PUBLIC_KEY)
                    .unwrap()
                    .try_into()
                    .unwrap();
                let sig = tlv::find(&sub, tlv::SIGNATURE).unwrap();
                let x = derive(
                    &k,
                    "Pair-Setup-Controller-Sign-Salt",
                    "Pair-Setup-Controller-Sign-Info",
                );
                let signed = [&x[..], &id, &ltpk].concat();
                let good = VerifyingKey::from_bytes(&ltpk)
                    .ok()
                    .zip(Signature::from_slice(sig).ok())
                    .is_some_and(|(key, sig)| key.verify(&signed, &sig).is_ok());
                if !good {
                    return error(6);
                }
                st.paired.push((id, ltpk));
                let ax = derive(
                    &k,
                    "Pair-Setup-Accessory-Sign-Salt",
                    "Pair-Setup-Accessory-Sign-Info",
                );
                let mine = self.signing.verifying_key().to_bytes();
                let mut signed = ax.to_vec();
                signed.extend_from_slice(ID.as_bytes());
                signed.extend_from_slice(&mine);
                let sig = self.signing.sign(&signed).to_bytes();
                let plain = tlv::encode(&[
                    (tlv::IDENTIFIER, ID.as_bytes()),
                    (tlv::PUBLIC_KEY, &mine),
                    (tlv::SIGNATURE, &sig),
                ]);
                let sealed = seal_once(&enc, b"PS-Msg06", &plain);
                body(&[(tlv::STATE, &[6]), (tlv::ENCRYPTED_DATA, &sealed)])
            }
            other => panic!("pair-setup in state {other:?}"),
        }
    }

    fn verify(&self, seen: &Seen) -> Answer {
        let items = tlv::decode(&seen.body).unwrap();
        let mut st = self.state.lock().unwrap();
        match tlv::find(&items, tlv::STATE) {
            Some([1]) => {
                let theirs: [u8; 32] = tlv::find(&items, tlv::PUBLIC_KEY)
                    .unwrap()
                    .try_into()
                    .unwrap();
                let secret = StaticSecret::from([0x77; 32]);
                let mine = PublicKey::from(&secret).to_bytes();
                let shared = secret.diffie_hellman(&PublicKey::from(theirs)).to_bytes();
                st.verify = Some((mine, theirs));
                st.shared = Some(shared);
                let enc = derive(
                    &shared,
                    "Pair-Verify-Encrypt-Salt",
                    "Pair-Verify-Encrypt-Info",
                );
                let signed = [&mine[..], ID.as_bytes(), &theirs].concat();
                let sig = self.signing.sign(&signed).to_bytes();
                let plain =
                    tlv::encode(&[(tlv::IDENTIFIER, ID.as_bytes()), (tlv::SIGNATURE, &sig)]);
                let sealed = seal_once(&enc, b"PV-Msg02", &plain);
                body(&[
                    (tlv::STATE, &[2]),
                    (tlv::PUBLIC_KEY, &mine),
                    (tlv::ENCRYPTED_DATA, &sealed),
                ])
            }
            Some([3]) => {
                let (Some((mine, theirs)), Some(shared)) = (st.verify.take(), st.shared) else {
                    return error(4);
                };
                let enc = derive(
                    &shared,
                    "Pair-Verify-Encrypt-Salt",
                    "Pair-Verify-Encrypt-Info",
                );
                let sealed = tlv::find(&items, tlv::ENCRYPTED_DATA).unwrap();
                let Some(plain) = open_once(&enc, b"PV-Msg03", sealed) else {
                    return error(4);
                };
                let sub = tlv::decode(&plain).unwrap();
                let id = tlv::find(&sub, tlv::IDENTIFIER).unwrap();
                let sig = tlv::find(&sub, tlv::SIGNATURE).unwrap();
                let Some((_, ltpk)) = st.paired.iter().find(|(p, _)| p == id) else {
                    return error(4);
                };
                let signed = [&theirs[..], id, &mine].concat();
                let good = VerifyingKey::from_bytes(ltpk)
                    .ok()
                    .zip(Signature::from_slice(sig).ok())
                    .is_some_and(|(key, sig)| key.verify(&signed, &sig).is_ok());
                if !good {
                    return error(4);
                }
                Answer::SealAfter(Box::new(body(&[(tlv::STATE, &[4])])), shared.to_vec())
            }
            other => panic!("pair-verify in state {other:?}"),
        }
    }
}
