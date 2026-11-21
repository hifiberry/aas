//! SRP-6a, the client side, as AirPlay receivers check it in transient
//! pairing: RFC 5054 with the operands of `k` and `u` padded to the length
//! of N, and RFC 2945's proofs over a key that is the hash of the premaster
//! secret at its own length, as PipeWire's `srp.c` computes them.

use num_bigint::BigUint;
use sha2::Digest;
use std::fmt;

/// A group: the modulus and the generator.
pub struct Group {
    pub n: BigUint,
    pub g: BigUint,
}

impl Group {
    /// The 3072-bit group of RFC 5054 appendix A, the one receivers use.
    pub fn rfc5054_3072() -> Group {
        Group {
            n: BigUint::parse_bytes(N_3072.as_bytes(), 16).expect("the modulus is hex"),
            g: BigUint::from(5u32),
        }
    }

    /// The length of N in bytes, which `k` and `u` pad their operands to.
    pub fn len(&self) -> usize {
        self.n.to_bytes_be().len()
    }
}

const N_3072: &str = "\
FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B\
139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485\
B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1F\
E649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23\
DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32\
905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF69558\
17183995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33A85521\
ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7ABF5AE8CDB0933D7\
1E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B1817\
7B200CBBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFCE0FD108E4B82\
D120A93AD2CAFFFFFFFFFFFFFFFF";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrpError(pub String);

impl fmt::Display for SrpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What the client sends and expects once it has the salt and B.
#[derive(Debug, Clone)]
pub struct Proofs {
    /// M1, the client's proof.
    pub client: Vec<u8>,
    /// M2, the proof the receiver must answer with.
    pub server: Vec<u8>,
    /// K, the shared key.
    pub key: Vec<u8>,
}

/// The client of one exchange.
pub struct Client<'a> {
    group: &'a Group,
    identity: &'a str,
    password: &'a [u8],
    a: BigUint,
    public: Vec<u8>,
}

impl<'a> Client<'a> {
    /// A client with the secret exponent `a`, 32 random bytes in use.
    pub fn new(group: &'a Group, identity: &'a str, password: &'a [u8], a: &[u8]) -> Client<'a> {
        let a = BigUint::from_bytes_be(a);
        let public = group.g.modpow(&a, &group.n).to_bytes_be();
        Client {
            group,
            identity,
            password,
            a,
            public,
        }
    }

    /// A, as it goes on the wire.
    pub fn public(&self) -> &[u8] {
        &self.public
    }

    /// The proofs and the key for the receiver's salt and B.
    pub fn respond<D: Digest>(&self, salt: &[u8], b: &[u8]) -> Result<Proofs, SrpError> {
        let group = self.group;
        if salt.is_empty() {
            return Err(SrpError("an empty salt".into()));
        }
        if b.len() > group.len() {
            return Err(SrpError(format!("a B of {} bytes", b.len())));
        }
        let b_num = BigUint::from_bytes_be(b);
        let a_num = BigUint::from_bytes_be(&self.public);
        let u = scrambler::<D>(group, &a_num, &b_num);
        // SRP-6a's checks: B mod N and u must not be zero.
        if (&b_num % &group.n) == BigUint::ZERO || u == BigUint::ZERO {
            return Err(SrpError("the receiver's B fails the SRP-6a check".into()));
        }
        let x = private_key::<D>(salt, self.identity, self.password);
        let k = multiplier::<D>(group);
        let n = &group.n;
        // (B - k g^x) mod N, kept positive.
        let kgx = (k * group.g.modpow(&x, n)) % n;
        let base = (&b_num + n - kgx) % n;
        let premaster = base.modpow(&(&self.a + u * x), n);
        let key = D::digest(premaster.to_bytes_be()).to_vec();
        let (hn, hg) = (
            D::digest(group.n.to_bytes_be()),
            D::digest(group.g.to_bytes_be()),
        );
        let xored: Vec<u8> = hn.iter().zip(hg.iter()).map(|(x, y)| x ^ y).collect();
        let mut m1 = D::new();
        m1.update(&xored);
        m1.update(D::digest(self.identity.as_bytes()));
        m1.update(salt);
        m1.update(&self.public);
        m1.update(b);
        m1.update(&key);
        let client = m1.finalize().to_vec();
        let mut m2 = D::new();
        m2.update(&self.public);
        m2.update(&client);
        m2.update(&key);
        Ok(Proofs {
            client,
            server: m2.finalize().to_vec(),
            key,
        })
    }
}

/// `H(PAD(x) | PAD(y))`, both padded to the length of N.
fn hash_padded<D: Digest>(group: &Group, x: &BigUint, y: &BigUint) -> BigUint {
    let len = group.len();
    let mut h = D::new();
    for v in [x, y] {
        let bytes = v.to_bytes_be();
        h.update(vec![0u8; len.saturating_sub(bytes.len())]);
        h.update(&bytes);
    }
    BigUint::from_bytes_be(&h.finalize())
}

/// The multiplier `k = H(N | PAD(g))`.
pub fn multiplier<D: Digest>(group: &Group) -> BigUint {
    hash_padded::<D>(group, &group.n, &group.g)
}

/// The private key `x = H(salt | H(I ":" P))`.
pub fn private_key<D: Digest>(salt: &[u8], identity: &str, password: &[u8]) -> BigUint {
    let mut inner = D::new();
    inner.update(identity.as_bytes());
    inner.update(b":");
    inner.update(password);
    let mut outer = D::new();
    outer.update(salt);
    outer.update(inner.finalize());
    BigUint::from_bytes_be(&outer.finalize())
}

/// The scrambler `u = H(PAD(A) | PAD(B))`.
pub fn scrambler<D: Digest>(group: &Group, a: &BigUint, b: &BigUint) -> BigUint {
    hash_padded::<D>(group, a, b)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use sha1::Sha1;
    use sha2::Sha512;

    fn hex(s: &str) -> BigUint {
        BigUint::parse_bytes(s.as_bytes(), 16).unwrap()
    }

    /// The 1024-bit group of RFC 5054 appendix A, for its test vectors.
    fn group_1024() -> Group {
        Group {
            n: hex(
                "EEAF0AB9ADB38DD69C33F80AFA8FC5E86072618775FF3C0B9EA2314C9C256576D674DF7496EA81D33\
                 83B4813D692C6E0E0D5D8E250B98BE48E495C1D6089DAD15DC7D7B46154D6B6CE8EF4AD69B15D49\
                 82559B297BCF1885C529F566660E57EC68EDBC3C05726CC02FD4CBF4976EAA9AFD5138FE8376435\
                 B9FC61D2FC0EB06E3"
                    .replace(' ', "")
                    .as_str(),
            ),
            g: BigUint::from(2u32),
        }
    }

    const SALT: &str = "BEB25379D1A8581EB5A727673A2441EE";
    const A_SECRET: &str = "60975527035CF2AD1989806F0407210BC81EDC04E2762A56AFD529DDDA2D4393";
    const B_SECRET: &str = "E487CB59D31AC550471E81F00F6928E01DDA08E974A004F49E61F5D105284D20";

    /// The receiver's side, for the tests and the fake receiver: the
    /// verifier from the password, B from `b`, and the premaster secret
    /// `(A v^u)^b`, computed the other way round from the client's.
    pub struct Server<'a> {
        pub group: &'a Group,
        pub v: BigUint,
        pub b: BigUint,
    }

    impl<'a> Server<'a> {
        pub fn new<D: Digest>(
            group: &'a Group,
            identity: &str,
            password: &[u8],
            salt: &[u8],
            b: &[u8],
        ) -> Server<'a> {
            let x = private_key::<D>(salt, identity, password);
            Server {
                group,
                v: group.g.modpow(&x, &group.n),
                b: BigUint::from_bytes_be(b),
            }
        }

        pub fn public<D: Digest>(&self) -> BigUint {
            let g = self.group;
            (multiplier::<D>(g) * &self.v + g.g.modpow(&self.b, &g.n)) % &g.n
        }

        pub fn premaster<D: Digest>(&self, a: &BigUint) -> BigUint {
            let g = self.group;
            let u = scrambler::<D>(g, a, &self.public::<D>());
            (a * self.v.modpow(&u, &g.n)).modpow(&self.b, &g.n)
        }

        /// K, M1 and M2 as the receiver computes them.
        pub fn proofs<D: Digest>(&self, identity: &str, salt: &[u8], a: &[u8]) -> Proofs {
            let g = self.group;
            let key = D::digest(
                self.premaster::<D>(&BigUint::from_bytes_be(a))
                    .to_bytes_be(),
            )
            .to_vec();
            let (hn, hg) = (D::digest(g.n.to_bytes_be()), D::digest(g.g.to_bytes_be()));
            let xored: Vec<u8> = hn.iter().zip(hg.iter()).map(|(x, y)| x ^ y).collect();
            let mut m1 = D::new();
            m1.update(&xored);
            m1.update(D::digest(identity.as_bytes()));
            m1.update(salt);
            m1.update(a);
            m1.update(self.public::<D>().to_bytes_be());
            m1.update(&key);
            let client = m1.finalize().to_vec();
            let mut m2 = D::new();
            m2.update(a);
            m2.update(&client);
            m2.update(&key);
            Proofs {
                client,
                server: m2.finalize().to_vec(),
                key,
            }
        }
    }

    fn bytes(s: &str) -> Vec<u8> {
        hex(s).to_bytes_be()
    }

    #[test]
    fn the_steps_match_rfc5054_appendix_b() {
        let group = group_1024();
        let salt = bytes(SALT);
        assert_eq!(
            multiplier::<Sha1>(&group),
            hex("7556AA045AEF2CDD07ABAF0F665C3E818913186F")
        );
        assert_eq!(
            private_key::<Sha1>(&salt, "alice", b"password123"),
            hex("94B7555AABE9127CC58CCF4993DB6CF84D16C124")
        );
        let client = Client::new(&group, "alice", b"password123", &bytes(A_SECRET));
        let a = BigUint::from_bytes_be(client.public());
        assert_eq!(
            a,
            hex(
                "61D5E490F6F1B79547B0704C436F523DD0E560F0C64115BB72557EC44352E8903211C04692272D8B\
                 2D1A5358A2CF1B6E0BFCF99F921530EC8E39356179EAE45E42BA92AEACED825171E1E8B9AF6D9C03\
                 E1327F44BE087EF06530E69F66615261EEF54073CA11CF5858F0EDFDFE15EFEAB349EF5D76988A36\
                 72FAC47B0769447B"
                    .replace(' ', "")
                    .as_str()
            )
        );
        let server = Server::new::<Sha1>(&group, "alice", b"password123", &salt, &bytes(B_SECRET));
        let b = server.public::<Sha1>();
        assert_eq!(
            b,
            hex(
                "BD0C61512C692C0CB6D041FA01BB152D4916A1E77AF46AE105393011BAF38964DC46A0670DD125B9\
                 5A981652236F99D9B681CBF87837EC996C6DA04453728610D0C6DDB58B318885D7D82C7F8DEB75CE\
                 7BD4FBAA37089E6F9C6059F388838E7A00030B331EB76840910440B1B27AAEAEEB4012B7D7665238\
                 A8E3FB004B117B58"
                    .replace(' ', "")
                    .as_str()
            )
        );
        assert_eq!(
            scrambler::<Sha1>(&group, &a, &b),
            hex("CE38B9593487DA98554ED47D70A7AE5F462EF019")
        );
        // The client's proofs are those the receiver computes from the
        // premaster secret of the RFC.
        let ours = client.respond::<Sha1>(&salt, &b.to_bytes_be()).unwrap();
        let theirs = server.proofs::<Sha1>("alice", &salt, client.public());
        assert_eq!(
            server.premaster::<Sha1>(&a),
            hex(
                "B0DC82BABCF30674AE450C0287745E7990A3381F63B387AAF271A10D233861E359B48220F7C4693C\
                 9AE12B0A6F67809F0876E2D013800D6C41BB59B6D5979B5C00A172B4A2A5903A0BDCAF8A709585EB\
                 2AFAFA8F3499B200210DCC1F10EB33943CD67FC88A2F39A4BE5BEC4EC0A3212DC346D7E474B29EDE\
                 8A469FFECA686E5A"
                    .replace(' ', "")
                    .as_str()
            )
        );
        assert_eq!(ours.key, theirs.key);
        assert_eq!(ours.client, theirs.client);
        assert_eq!(ours.server, theirs.server);
    }

    #[test]
    fn the_3072_bit_group_with_sha512_agrees_with_the_receiver() {
        let group = Group::rfc5054_3072();
        assert_eq!(group.len(), 384);
        let salt = [0x42u8; 16];
        let client = Client::new(&group, "Pair-Setup", b"3939", &[7u8; 32]);
        let server = Server::new::<Sha512>(&group, "Pair-Setup", b"3939", &salt, &[9u8; 32]);
        let b = server.public::<Sha512>().to_bytes_be();
        let ours = client.respond::<Sha512>(&salt, &b).unwrap();
        let theirs = server.proofs::<Sha512>("Pair-Setup", &salt, client.public());
        assert_eq!(ours.key.len(), 64);
        assert_eq!(ours.key, theirs.key);
        assert_eq!(ours.client, theirs.client);
        assert_eq!(ours.server, theirs.server);
        // A wrong PIN on one side gives other proofs.
        let wrong = Server::new::<Sha512>(&group, "Pair-Setup", b"3940", &salt, &[9u8; 32]);
        let b = wrong.public::<Sha512>().to_bytes_be();
        let ours = client.respond::<Sha512>(&salt, &b).unwrap();
        assert_ne!(
            ours.client,
            wrong
                .proofs::<Sha512>("Pair-Setup", &salt, client.public())
                .client
        );
    }

    #[test]
    fn a_b_that_is_a_multiple_of_n_is_refused() {
        let group = Group::rfc5054_3072();
        let client = Client::new(&group, "Pair-Setup", b"3939", &[7u8; 32]);
        assert!(client.respond::<Sha512>(&[1; 16], &[0]).is_err());
        assert!(
            client
                .respond::<Sha512>(&[1; 16], &group.n.to_bytes_be())
                .is_err()
        );
        // Wider than N.
        assert!(client.respond::<Sha512>(&[1; 16], &[1; 385]).is_err());
        assert!(client.respond::<Sha512>(&[], &[5]).is_err());
    }
}
