//! Values changed at runtime, kept across restarts. The config file is
//! never written; this file is.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

/// Default state path. Under systemd this is the service's StateDirectory.
pub const DEFAULT_PATH: &str = "/var/lib/aas/state.toml";

/// The most clients whose volume is kept.
pub const MAX_CLIENTS: usize = 64;

/// A value set at run time, with what the config said when it was set. The
/// value applies only while the config still says the same.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stored<T> {
    pub value: T,
    /// What the config said then.
    pub config: T,
}

impl<T: PartialEq + Clone> Stored<T> {
    /// The value to use now: the stored one while the config still says
    /// `config`, else what the config says.
    pub fn resolve(stored: Option<&Stored<T>>, config_now: &T) -> T {
        match stored {
            Some(s) if s.config == *config_now => s.value.clone(),
            _ => config_now.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredInput {
    pub volume_db: f32,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRoute {
    pub from: String,
    pub to: usize,
    pub gain_db: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StoredOutput {
    /// Further outputs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_db: Option<Stored<f32>>,
    /// "zeros" or "off".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<Stored<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_delay_s: Option<Stored<u32>>,
    /// Empty means the default matrix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<Stored<Vec<StoredRoute>>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Written as 2. A file without a version is version 1, which held
    /// only the output volume.
    pub version: u32,
    /// The output volume, 0 to 100.
    pub output_percent: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analog: Option<Stored<StoredInput>>,
    /// At most 64.
    pub clients: BTreeMap<String, StoredInput>,
    /// Keyed by card.
    pub outputs: BTreeMap<String, StoredOutput>,
}

impl Default for State {
    /// A fresh install starts at 0 dB.
    fn default() -> State {
        State {
            version: 2,
            output_percent: 100,
            analog: None,
            clients: BTreeMap::new(),
            outputs: BTreeMap::new(),
        }
    }
}

fn input_ok(i: &StoredInput) -> bool {
    i.volume_db.is_finite() && (-60.0..=0.0).contains(&i.volume_db)
}

fn db_ok(db: f32, low: f32) -> bool {
    db.is_finite() && (low..=0.0).contains(&db)
}

impl State {
    /// Drops what is out of range, one entry at a time, and says so once
    /// for each. The rest stays.
    fn sanitise(&mut self, path: &Path) {
        let say = |what: &str| eprintln!("aas: {}: {what} out of range, dropped", path.display());
        self.version = 2;
        if let Some(a) = &self.analog {
            if !input_ok(&a.value) || !input_ok(&a.config) {
                say("the analog input's volume");
                self.analog = None;
            }
        }
        self.clients.retain(|name, i| {
            let ok = input_ok(i);
            if !ok {
                say(&format!("the volume of client {name:?}"));
            }
            ok
        });
        while self.clients.len() > MAX_CLIENTS {
            if let Some(name) = self.clients.keys().next_back().cloned() {
                say(&format!("client {name:?}, more than {MAX_CLIENTS}:"));
                self.clients.remove(&name);
            }
        }
        for (card, o) in &mut self.outputs {
            if let Some(v) = &o.volume_db {
                if !db_ok(v.value, -60.0) {
                    say(&format!("the volume of output {card:?}"));
                    o.volume_db = None;
                }
            }
            if let Some(v) = &o.idle {
                if !["zeros", "off"].contains(&v.value.as_str()) {
                    say(&format!("the idle rule of output {card:?}"));
                    o.idle = None;
                }
            }
            if let Some(m) = &o.matrix {
                if !m.value.iter().all(|r| db_ok(r.gain_db, -100.0)) {
                    say(&format!("the matrix of output {card:?}"));
                    o.matrix = None;
                }
            }
        }
    }

    /// Reads the file. A missing or unreadable file, or a master volume out
    /// of range, gives the defaults. Any other entry out of range is
    /// dropped alone.
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
            Ok(mut state) if state.output_percent <= 100 => {
                state.sanitise(path);
                state
            }
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
        written.map_err(|e| explain("could not write the file", e))?;
        sync_directory(path)
    }
}

/// An error that says which step of a save failed.
fn explain(what: &str, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// Syncs the directory of a file that has been renamed into place. A
/// failure here leaves the file saved, and says so.
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(directory_of(path))
        .and_then(|dir| dir.sync_all())
        .map_err(|e| {
            explain(
                "the file is saved, but its directory could not be synced",
                e,
            )
        })
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
        let state = match &self.queue {
            Some(queue) => match queue.send(state) {
                Ok(()) => return,
                Err(mpsc::SendError(state)) => state,
            },
            None => state,
        };
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
        eprintln!("aas: {}: {e}", path.display());
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
    fn an_error_says_which_step_failed() {
        let dir = tempfile::tempdir().unwrap();
        // The file cannot be written: its directory is missing.
        let path = dir.path().join("missing/state.toml");
        let e = State::default().save(&path).unwrap_err().to_string();
        assert!(e.starts_with("could not write the file: "), "{e}");
        // Written and in place, but the directory cannot be synced.
        let e = sync_directory(&path).unwrap_err().to_string();
        assert!(
            e.starts_with("the file is saved, but its directory could not be synced: "),
            "{e}"
        );
        assert!(sync_directory(&dir.path().join("state.toml")).is_ok());
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
            saver.save(State {
                output_percent,
                ..State::default()
            });
        }
        saver.finish();
        assert_eq!(State::load(&path).output_percent, 30);
        // After that it writes in place.
        saver.save(State {
            output_percent: 40,
            ..State::default()
        });
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
        let state = State {
            output_percent: 5,
            ..State::default()
        };
        assert!(state.save(&path).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn state_survives_a_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let state = State {
            output_percent: 42,
            ..State::default()
        };
        state.save(&path).unwrap();
        assert_eq!(State::load(&path), state);
        // Saved again over the old file, and no temporary file is left.
        State {
            output_percent: 7,
            ..State::default()
        }
        .save(&path)
        .unwrap();
        assert_eq!(State::load(&path).output_percent, 7);
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1);
    }

    fn full() -> State {
        let stored = |value: f32| Stored { value, config: 0.0 };
        let mut s = State {
            output_percent: 33,
            analog: Some(Stored {
                value: StoredInput {
                    volume_db: -6.5,
                    muted: true,
                },
                config: StoredInput {
                    volume_db: 0.0,
                    muted: false,
                },
            }),
            ..State::default()
        };
        s.clients.insert(
            "player".into(),
            StoredInput {
                volume_db: -12.0,
                muted: false,
            },
        );
        s.outputs.insert(
            "Loopback".into(),
            StoredOutput {
                volume_db: Some(stored(-3.0)),
                idle: Some(Stored {
                    value: "off".into(),
                    config: "zeros".into(),
                }),
                idle_delay_s: Some(Stored {
                    value: 5,
                    config: 10,
                }),
                matrix: Some(Stored {
                    value: vec![StoredRoute {
                        from: "analog".into(),
                        to: 1,
                        gain_db: -6.0,
                    }],
                    config: vec![],
                }),
            },
        );
        s.outputs
            .insert("sndrpihifiberry".into(), StoredOutput::default());
        s
    }

    #[test]
    fn a_version_1_file_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        std::fs::write(&path, "output_percent = 40\n").unwrap();
        let state = State::load(&path);
        assert_eq!(state.version, 2);
        assert_eq!(
            state,
            State {
                output_percent: 40,
                ..State::default()
            }
        );
    }

    #[test]
    fn version_2_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let state = full();
        state.save(&path).unwrap();
        assert_eq!(State::load(&path), state);
    }

    #[test]
    fn a_stored_value_applies_while_the_config_is_unchanged() {
        let s = Stored {
            value: -6.0,
            config: 0.0,
        };
        assert_eq!(Stored::resolve(Some(&s), &0.0), -6.0);
    }

    #[test]
    fn a_config_edit_wins() {
        let s = Stored {
            value: -6.0,
            config: 0.0,
        };
        assert_eq!(Stored::resolve(Some(&s), &-3.0), -3.0);
    }

    #[test]
    fn nothing_stored_takes_the_config() {
        assert_eq!(Stored::<f32>::resolve(None, &-3.0), -3.0);
    }

    #[test]
    fn outputs_are_keyed_by_card() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let one = StoredOutput {
            idle_delay_s: Some(Stored {
                value: 1,
                config: 2,
            }),
            ..StoredOutput::default()
        };
        let two = StoredOutput {
            idle_delay_s: Some(Stored {
                value: 3,
                config: 4,
            }),
            ..StoredOutput::default()
        };
        for order in [
            [("Loopback", &one), ("sndrpihifiberry", &two)],
            [("sndrpihifiberry", &two), ("Loopback", &one)],
        ] {
            let mut state = State::default();
            for (card, o) in order {
                state.outputs.insert(card.into(), o.clone());
            }
            state.save(&path).unwrap();
            let loaded = State::load(&path);
            assert_eq!(loaded.outputs["Loopback"], one);
            assert_eq!(loaded.outputs["sndrpihifiberry"], two);
        }
    }

    #[test]
    fn a_client_entry_out_of_range_is_dropped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let text = "output_percent = 50\n\
            [clients.loud]\nvolume_db = -70\nmuted = false\n\
            [clients.fine]\nvolume_db = -10.0\nmuted = true\n\
            [outputs.x.volume_db]\nvalue = 5.0\nconfig = 0.0\n\
            [outputs.x.idle]\nvalue = \"zeros\"\nconfig = \"off\"\n";
        std::fs::write(&path, text).unwrap();
        let state = State::load(&path);
        assert_eq!(state.output_percent, 50);
        assert_eq!(state.clients.keys().collect::<Vec<_>>(), ["fine"]);
        assert!(state.outputs["x"].volume_db.is_none());
        assert!(state.outputs["x"].idle.is_some());
    }

    #[test]
    fn clients_beyond_64_are_dropped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let mut state = State::default();
        for n in 0..70 {
            state.clients.insert(
                format!("c{n:02}"),
                StoredInput {
                    volume_db: -1.0,
                    muted: false,
                },
            );
        }
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).clients.len(), MAX_CLIENTS);
    }
}
