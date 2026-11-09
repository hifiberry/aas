//! The daemon: `aas [--config PATH] [--socket PATH] [--state PATH]`.

use aas::config::{self, Config};
use aas::rt;
use aas::server;
use aas::sink::Sink;
use aas::state;
use aas::volume::HardwareVolume;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

const USAGE: &str = "usage: aas [--config PATH] [--socket PATH] [--state PATH]";

/// Set by SIGTERM and SIGINT.
static SHUTDOWN: OnceLock<Arc<AtomicBool>> = OnceLock::new();

extern "C" fn on_signal(_: libc::c_int) {
    if let Some(flag) = SHUTDOWN.get() {
        flag.store(true, Ordering::Relaxed);
    }
}

struct Args {
    config: PathBuf,
    socket: PathBuf,
    state: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        config: PathBuf::from(config::DEFAULT_PATH),
        socket: aas_client::proto::default_socket_path(),
        state: PathBuf::from(state::DEFAULT_PATH),
    };
    let mut given = std::env::args_os().skip(1);
    while let Some(arg) = given.next() {
        let slot = match arg.to_str() {
            Some("--config") => &mut args.config,
            Some("--socket") => &mut args.socket,
            Some("--state") => &mut args.state,
            _ => return Err(format!("unknown argument {}", arg.to_string_lossy())),
        };
        *slot = given
            .next()
            .ok_or_else(|| format!("{} needs a path", arg.to_string_lossy()))?
            .into();
    }
    Ok(args)
}

/// Stops the server on SIGTERM and SIGINT. Without SA_RESTART, so that the
/// control thread's poll returns and sees the flag.
fn handle_signals(shutdown: Arc<AtomicBool>) {
    let _ = SHUTDOWN.set(shutdown);
    let handler: extern "C" fn(libc::c_int) = on_signal;
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: an all-zero sigaction is valid, and the handler only
        // stores to an atomic.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

#[cfg(feature = "alsa-output")]
fn output(config: &Config) -> Box<dyn Sink> {
    Box::new(aas::sink_alsa::AlsaSink::new(config.card.clone()))
}

#[cfg(not(feature = "alsa-output"))]
fn output(_config: &Config) -> Box<dyn Sink> {
    eprintln!("aas: warning: built without ALSA, the output goes nowhere");
    Box::new(aas::sink::NullSink::default())
}

/// The card's volume control, unless the config asks for software volume.
#[cfg(feature = "alsa-output")]
fn hardware_volume(config: &Config) -> Option<Box<dyn HardwareVolume>> {
    if config.volume_mode == Some(config::VolumeMode::Software) {
        return None;
    }
    let control = config.volume_control.as_deref();
    let found = aas::sink_alsa::AlsaVolume::open(config.card.as_deref(), control);
    if found.is_none() && config.volume_mode == Some(config::VolumeMode::Hardware) {
        eprintln!("aas: warning: no hardware volume found, using software volume");
    }
    found.map(|v| Box::new(v) as Box<dyn HardwareVolume>)
}

#[cfg(not(feature = "alsa-output"))]
fn hardware_volume(_config: &Config) -> Option<Box<dyn HardwareVolume>> {
    None
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("aas: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let config = match Config::load(&args.config) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("aas: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("aas {} starting", env!("CARGO_PKG_VERSION"));
    if let Err(e) = rt::lock_memory() {
        eprintln!("aas: warning: memory not locked: {e}");
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    handle_signals(shutdown.clone());
    let sink = output(&config);
    let hardware = hardware_volume(&config);
    match server::run(config, sink, &args.socket, &args.state, hardware, shutdown) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("aas: {e}");
            ExitCode::FAILURE
        }
    }
}
