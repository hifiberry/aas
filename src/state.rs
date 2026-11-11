//! Values changed at runtime, kept across restarts. The config file is
//! never written; this file is.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

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
    /// never part of one. The directory is synced after the rename, so
    /// that the new name survives a power loss too.
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
        written?;
        fs::File::open(directory_of(path))?.sync_all()
    }
}

/// The directory a file is in. A bare file name is in the current one.
fn directory_of(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// Stack for the thread that saves. It formats a few bytes and calls the
/// file system, and with memory locked its whole stack is resident.
const SAVER_STACK: usize = 256 * 1024;

/// Writes the state file from a thread of its own. The sync to disk can
/// take long on a slow SD card, and would hold up the clients' requests on
/// the control thread.
pub struct Saver {
    path: PathBuf,
    queue: Option<mpsc::Sender<State>>,
    thread: Option<JoinHandle<()>>,
}

impl Saver {
    /// Starts the thread. Without one the file is written in place.
    pub fn new(path: PathBuf) -> Saver {
        let (queue, waiting) = mpsc::channel::<State>();
        let target = path.clone();
        let builder = thread::Builder::new()
            .name("aas-state".into())
            .stack_size(SAVER_STACK);
        let spawned = builder.spawn(move || {
            while let Ok(mut state) = waiting.recv() {
                // Only the latest state matters.
                while let Ok(newer) = waiting.try_recv() {
                    state = newer;
                }
                write(&state, &target);
            }
        });
        match spawned {
            Ok(thread) => Saver {
                path,
                queue: Some(queue),
                thread: Some(thread),
            },
            Err(e) => {
                eprintln!("aas: no thread to save the state, saving in place: {e}");
                Saver {
                    path,
                    queue: None,
                    thread: None,
                }
            }
        }
    }

    /// Hands the state over to be written. Returns at once.
    pub fn save(&self, state: State) {
        if let Some(queue) = &self.queue {
            if queue.send(state).is_ok() {
                return;
            }
        }
        write(&state, &self.path);
    }

    /// Returns once everything handed over is on disk. A later `save`
    /// writes in place.
    pub fn finish(&mut self) {
        self.queue = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn write(state: &State, path: &Path) {
    if let Err(e) = state.save(path) {
        eprintln!("aas: could not save {}: {e}", path.display());
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
    fn a_bare_file_name_is_in_the_current_directory() {
        assert_eq!(
            directory_of(Path::new("/var/lib/aas/state.toml")),
            Path::new("/var/lib/aas")
        );
        assert_eq!(directory_of(Path::new("aas/state.toml")), Path::new("aas"));
        assert_eq!(directory_of(Path::new("state.toml")), Path::new("."));
    }

    #[test]
    fn the_saver_has_written_the_latest_state_when_it_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let mut saver = Saver::new(path.clone());
        for output_percent in [10, 20, 30] {
            saver.save(State { output_percent });
        }
        saver.finish();
        assert_eq!(State::load(&path).output_percent, 30);
        // After that it writes in place.
        saver.save(State { output_percent: 40 });
        assert_eq!(State::load(&path).output_percent, 40);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_save_that_fails_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        // The target is a directory with something in it, so the rename fails.
        let path = dir.path().join("state.toml");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("x"), "").unwrap();
        assert!(State { output_percent: 5 }.save(&path).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
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
