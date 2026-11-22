//! The keys of AirPlay 2 pairing: aas's own identity, made once, and the
//! long-term public key of each paired receiver, kept in a file beside the
//! state file that only aas can read.

use ed25519_dalek::SigningKey;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// aas as a pairing controller.
#[derive(Clone)]
pub struct Identity {
    pub id: String,
    pub signing: SigningKey,
}

/// A paired receiver: its pairing id and its Ed25519 public key.
#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub id: String,
    pub public: [u8; 32],
}

#[derive(Default)]
struct Data {
    identity: Option<(String, [u8; 32])>,
    receivers: BTreeMap<String, Peer>,
}

/// The pairing keys, shared by the receivers' threads.
#[derive(Default)]
pub struct Pairings {
    path: Mutex<Option<PathBuf>>,
    data: Mutex<Data>,
}

impl Pairings {
    pub fn new() -> Arc<Pairings> {
        Arc::new(Pairings::default())
    }

    /// Reads the keys from `path`, which later changes are written to. A
    /// file that cannot be read is logged and taken as empty.
    pub fn load(&self, path: PathBuf) {
        let data = match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text).unwrap_or_else(|| {
                eprintln!(
                    "aas: {} cannot be read; no receiver is paired",
                    path.display()
                );
                Data::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(e) => {
                eprintln!("aas: {}: {e}; no receiver is paired", path.display());
                Data::default()
            }
        };
        *self.data.lock().unwrap() = data;
        *self.path.lock().unwrap() = Some(path);
    }

    /// aas's identity, made and kept the first time it is asked for.
    pub fn identity(&self) -> Identity {
        let mut data = self.data.lock().unwrap();
        let (id, seed) = match &data.identity {
            Some(known) => known.clone(),
            None => {
                let made = (uuid(), crate::airplay::raop::random::<32>());
                data.identity = Some(made.clone());
                if let Err(e) = self.save(&data) {
                    eprintln!("aas: the AirPlay pairing identity cannot be kept: {e}");
                }
                made
            }
        };
        Identity {
            id,
            signing: SigningKey::from_bytes(&seed),
        }
    }

    /// The receiver paired under `name`.
    pub fn peer(&self, name: &str) -> Option<Peer> {
        self.data.lock().unwrap().receivers.get(name).cloned()
    }

    /// Keeps `peer` as the receiver named `name`.
    pub fn store(&self, name: &str, peer: Peer) -> std::io::Result<()> {
        let mut data = self.data.lock().unwrap();
        data.receivers.insert(name.to_string(), peer);
        self.save(&data)
    }

    /// Writes `data` to the file, atomically and for the owner only.
    fn save(&self, data: &Data) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let Some(path) = self.path.lock().unwrap().clone() else {
            return Ok(());
        };
        let mut table = toml::Table::new();
        if let Some((id, seed)) = &data.identity {
            let mut t = toml::Table::new();
            t.insert("id".into(), id.clone().into());
            t.insert("seed".into(), hex(seed).into());
            table.insert("identity".into(), t.into());
        }
        let mut receivers = toml::Table::new();
        for (name, peer) in &data.receivers {
            let mut t = toml::Table::new();
            t.insert("id".into(), peer.id.clone().into());
            t.insert("public".into(), hex(&peer.public).into());
            receivers.insert(name.clone(), t.into());
        }
        table.insert("receivers".into(), receivers.into());
        let text = toml::to_string(&table).map_err(std::io::Error::other)?;
        let temp = path.with_extension("toml.new");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    }
}

/// The keys in the file's text; `None` when it is not what was written.
fn parse(text: &str) -> Option<Data> {
    let table: toml::Table = toml::from_str(text).ok()?;
    let mut data = Data::default();
    if let Some(identity) = table.get("identity") {
        let id = identity.get("id")?.as_str()?.to_string();
        let seed = unhex(identity.get("seed")?.as_str()?)?;
        data.identity = Some((id, seed));
    }
    if let Some(receivers) = table.get("receivers").and_then(toml::Value::as_table) {
        for (name, v) in receivers {
            let id = v.get("id")?.as_str()?.to_string();
            let public = unhex(v.get("public")?.as_str()?)?;
            data.receivers.insert(name.clone(), Peer { id, public });
        }
    }
    Some(data)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A random UUID, as the pairing id of aas.
fn uuid() -> String {
    let b = crate::airplay::raop::random::<16>();
    let h = hex(&b).to_uppercase();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn peer(n: u8) -> Peer {
        Peer {
            id: format!("AA:BB:{n}"),
            public: [n; 32],
        }
    }

    #[test]
    fn a_stored_peer_is_read_back_after_a_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("airplay-pairing.toml");
        let p = Pairings::new();
        p.load(path.clone());
        p.store("Apple TV", peer(7)).unwrap();
        let q = Pairings::new();
        q.load(path);
        assert_eq!(q.peer("Apple TV"), Some(peer(7)));
        assert_eq!(q.peer("Other"), None);
    }

    #[test]
    fn the_identity_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("airplay-pairing.toml");
        let p = Pairings::new();
        p.load(path.clone());
        let a = p.identity();
        let b = p.identity();
        assert_eq!(a.id, b.id);
        assert_eq!(a.signing.to_bytes(), b.signing.to_bytes());
        let q = Pairings::new();
        q.load(path);
        let c = q.identity();
        assert_eq!(a.id, c.id);
        assert_eq!(a.signing.to_bytes(), c.signing.to_bytes());
        assert_eq!(a.id.len(), 36);
    }

    #[test]
    fn the_file_is_only_the_owners() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("airplay-pairing.toml");
        let p = Pairings::new();
        p.load(path.clone());
        p.store("Apple TV", peer(1)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn garbage_is_read_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("airplay-pairing.toml");
        std::fs::write(&path, "not toml [[[").unwrap();
        let p = Pairings::new();
        p.load(path.clone());
        assert_eq!(p.peer("Apple TV"), None);
        let id = p.identity();
        let q = Pairings::new();
        q.load(path);
        assert_eq!(q.identity().id, id.id);
    }

    #[test]
    fn no_path_keeps_keys_in_memory() {
        let p = Pairings::new();
        p.store("Apple TV", peer(3)).unwrap();
        assert_eq!(p.peer("Apple TV"), Some(peer(3)));
        assert_eq!(p.identity().id, p.identity().id);
    }
}
