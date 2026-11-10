//! Values changed at runtime, kept across restarts. The config file is
//! never written; this file is.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Default state path. Under systemd this is the service's StateDirectory.
pub const DEFAULT_PATH: &str = "/var/lib/aas/state.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// The output volume, 0 to 100.
    pub output_percent: u8,
}

impl Default for State {
    /// A fresh install starts at 0 dB.
    fn default() -> State {
        State {
            output_percent: 100,
        }
    }
}

impl State {
    /// Reads the file. A missing or unreadable file, or a value out of
    /// range, gives the defaults.
    pub fn load(path: &Path) -> State {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return State::default(),
            Err(e) => {
                eprintln!("aas: {}: {e}, starting from defaults", path.display());
                return State::default();
            }
        };
        match toml::from_str::<State>(&text) {
            Ok(state) if state.output_percent <= 100 => state,
            Ok(_) => {
                eprintln!(
                    "aas: {}: volume out of range, starting from defaults",
                    path.display()
                );
                State::default()
            }
            Err(e) => {
                eprintln!("aas: {}: {e}, starting from defaults", path.display());
                State::default()
            }
        }
    }

    /// Writes the file to a temporary file beside it and renames that into
    /// place, so that a crash or power loss leaves the old or the new file,
    /// never part of one.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = toml::to_string(self).map_err(io::Error::other)?;
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".new");
        let temporary = Path::new(&temporary);
        let written = (|| {
            let mut file = fs::File::create(temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(temporary, path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(temporary);
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_corrupt_state_file_starts_at_0_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        assert_eq!(State::load(&path).output_percent, 100);
        let corrupt = [
            "output_percent = \"x\"\n",
            "output_percent = 300\n",
            "output_percent = 150\n",
            "\0\0",
        ];
        for text in corrupt {
            std::fs::write(&path, text).unwrap();
            assert_eq!(State::load(&path).output_percent, 100, "{text:?}");
        }
    }

    #[test]
    fn state_survives_a_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let state = State { output_percent: 42 };
        state.save(&path).unwrap();
        assert_eq!(State::load(&path), state);
        // Saved again over the old file, and no temporary file is left.
        State { output_percent: 7 }.save(&path).unwrap();
        assert_eq!(State::load(&path).output_percent, 7);
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1);
    }
}
