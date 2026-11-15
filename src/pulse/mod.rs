//! The PulseAudio native-protocol endpoint.
//!
//! It runs inside the daemon. Each connection is a thread, and each
//! playback stream is a client of the daemon over its own connection to
//! the client protocol, as the ALSA plugin is.

pub mod conn;
pub mod objects;
pub mod packet;
pub mod registry;
pub mod tagstruct;
pub mod wire;

use crate::config::Config;
use crate::pulse::registry::Registry;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// A running endpoint. Dropping it stops the listener and removes the
/// socket.
pub struct Pulse {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
}

impl Pulse {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stops the listener and its connections now. Dropping does the same.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Starts the endpoint, unless the config turns it off. `daemon_socket` is
/// the client protocol's socket, which every stream connects to.
pub fn spawn(
    config: &Config,
    daemon_socket: &Path,
    shutdown: Arc<AtomicBool>,
) -> io::Result<Option<Pulse>> {
    let Some(path) = socket_path(config, daemon_socket) else {
        return Ok(None);
    };
    let mut building = path.as_os_str().to_owned();
    building.push(format!(".{}", std::process::id()));
    let building = PathBuf::from(building);
    let _ = fs::remove_file(&building);
    let listener = UnixListener::bind(&building)?;
    let ready = (|| {
        fs::set_permissions(&building, fs::Permissions::from_mode(0o660))?;
        crate::server::set_audio_group(&building);
        listener.set_nonblocking(true)?;
        fs::rename(&building, &path)
    })();
    if let Err(e) = ready {
        let _ = fs::remove_file(&building);
        return Err(e);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let daemon = daemon_socket.to_path_buf();
    let buffer_ms = config.pulse_buffer_ms.map(u32::from).unwrap_or(200);
    let registry = Arc::new(Registry::default());
    let listener = thread::Builder::new()
        .name("aas-pulse".into())
        .spawn(move || accept_loop(listener, daemon, buffer_ms, thread_stop, shutdown, registry))?;
    eprintln!("aas: PulseAudio on {}", path.display());
    Ok(Some(Pulse {
        path,
        stop,
        listener: Some(listener),
    }))
}

fn socket_path(config: &Config, daemon_socket: &Path) -> Option<PathBuf> {
    match &config.pulse_socket {
        Some(path) if path.as_os_str().is_empty() => None,
        Some(path) => Some(path.clone()),
        None => Some(
            daemon_socket
                .parent()
                .map_or_else(|| PathBuf::from("pulse"), |dir| dir.join("pulse")),
        ),
    }
}

fn accept_loop(
    listener: UnixListener,
    daemon: PathBuf,
    buffer_ms: u32,
    stop: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    registry: Arc<Registry>,
) {
    let mut conns: Vec<JoinHandle<()>> = Vec::new();
    while !stop.load(Ordering::Relaxed) && !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let daemon = daemon.clone();
                let stop = stop.clone();
                let shutdown = shutdown.clone();
                let registry = registry.clone();
                let spawned = thread::Builder::new()
                    .name("aas-pulse-conn".into())
                    .spawn(move || {
                        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            conn::serve(stream, &daemon, buffer_ms, &stop, &shutdown, registry);
                        }));
                        if ran.is_err() {
                            eprintln!("aas: pulse: a connection failed");
                        }
                    });
                if let Ok(handle) = spawned {
                    conns.push(handle);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_PAUSE),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                eprintln!("aas: pulse: accept: {e}");
                thread::sleep(ACCEPT_PAUSE);
            }
        }
        let mut index = 0;
        while index < conns.len() {
            if conns[index].is_finished() {
                let handle = conns.remove(index);
                let _ = handle.join();
            } else {
                index += 1;
            }
        }
    }
    for handle in conns {
        let _ = handle.join();
    }
}
