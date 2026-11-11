//! The daemon: `aas [--config PATH] [--socket PATH] [--state PATH]`.

use aas::config::{self, Config};
use aas::rt;
use aas::server;
use aas::sink::Sink;
use aas::state;
#[cfg(feature = "alsa-output")]
use aas::volume::HardwareVolume;
use aas::volume::OpenVolume;
use std::ffi::OsString;
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

#[derive(Debug, PartialEq)]
struct Args {
    config: PathBuf,
    socket: PathBuf,
    state: PathBuf,
}

/// Reads the command line, without the program's name.
fn parse_args(given: impl IntoIterator<Item = OsString>) -> Result<Args, String> {
    let mut args = Args {
        config: PathBuf::from(config::DEFAULT_PATH),
        socket: aas_client::proto::default_socket_path(),
        state: PathBuf::from(state::DEFAULT_PATH),
    };
    let mut given = given.into_iter();
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

/// Opens the card's volume control, unless the config asks for software
/// volume. The server calls it at the start and each time the card comes
/// back.
#[cfg(feature = "alsa-output")]
fn hardware_volume(config: &Config) -> Option<OpenVolume> {
    if config.volume_mode == Some(config::VolumeMode::Software) {
        return None;
    }
    let card = config.card.clone();
    let control = config.volume_control.clone();
    let forced = config.volume_mode == Some(config::VolumeMode::Hardware);
    Some(Box::new(move || {
        let found = aas::sink_alsa::AlsaVolume::open(card.as_deref(), control.as_deref());
        if found.is_none() && forced {
            eprintln!("aas: warning: no hardware volume found, using software volume");
        }
        found.map(|v| Box::new(v) as Box<dyn HardwareVolume>)
    }))
}

#[cfg(not(feature = "alsa-output"))]
fn hardware_volume(config: &Config) -> Option<OpenVolume> {
    if config.volume_mode == Some(config::VolumeMode::Hardware) {
        eprintln!("aas: warning: built without ALSA, volume_mode = \"hardware\" is ignored");
    }
    None
}

/// The analog input, if the config enables it and the card to play to has
/// an ADC in the table. The card is looked up once, at the start; the
/// capture opens the same card each time the output opens.
#[cfg(feature = "alsa-output")]
fn analog_input(config: &Config) -> Option<aas::control::OpenCapture> {
    if !config.analog.enabled {
        return None;
    }
    let Some((card, driver)) = aas::sink_alsa::card_driver_now(config.card.as_deref()) else {
        eprintln!("aas: no sound card found for the analog input, it stays off");
        return None;
    };
    let open: aas::control::OpenCapture = Box::new(move |rate, period| {
        aas::sink_alsa::AlsaCapture::open(&card, rate, period)
            .map(|c| Box::new(c) as Box<dyn aas::capture::Capture>)
    });
    server::analog_capture(config, &driver, open)
}

#[cfg(not(feature = "alsa-output"))]
fn analog_input(config: &Config) -> Option<aas::control::OpenCapture> {
    if config.analog.enabled {
        eprintln!("aas: warning: built without ALSA, the analog input stays off");
    }
    None
}

/// Looks for the card on the control thread while it is gone, so that the
/// audio thread tries to open it only once it is there. A build without
/// ALSA never asks: its output is never gone.
fn card_probe(config: &Config) -> server::CardProbe {
    let card = config.card.clone();
    Box::new(move || aas::sink_alsa::card_is_present(card.as_deref()))
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args_os().skip(1)) {
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
    let probe = Some(card_probe(&config));
    let capture = analog_input(&config);
    let (socket, state) = (&args.socket, &args.state);
    match server::run_probing(
        config, sink, socket, state, hardware, probe, capture, shutdown,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("aas: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn no_arguments_give_the_default_paths() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.config, PathBuf::from(config::DEFAULT_PATH));
        assert_eq!(args.socket, aas_client::proto::default_socket_path());
        assert_eq!(args.state, PathBuf::from(state::DEFAULT_PATH));
    }

    #[test]
    fn each_flag_sets_its_own_path() {
        let defaults = parse(&[]).unwrap();
        let args = parse(&["--config", "/a/c.toml"]).unwrap();
        assert_eq!(args.config, PathBuf::from("/a/c.toml"));
        assert_eq!(
            (&args.socket, &args.state),
            (&defaults.socket, &defaults.state)
        );
        let args = parse(&["--socket", "/a/s.sock"]).unwrap();
        assert_eq!(args.socket, PathBuf::from("/a/s.sock"));
        assert_eq!(
            (&args.config, &args.state),
            (&defaults.config, &defaults.state)
        );
        let args = parse(&["--state", "/a/state.toml"]).unwrap();
        assert_eq!(args.state, PathBuf::from("/a/state.toml"));
        assert_eq!(
            (&args.config, &args.socket),
            (&defaults.config, &defaults.socket)
        );
    }

    #[test]
    fn the_flags_combine_in_any_order_and_the_last_one_wins() {
        let args = parse(&[
            "--state", "s1", "--config", "c1", "--socket", "k1", "--config", "c2",
        ])
        .unwrap();
        assert_eq!(
            args,
            Args {
                config: PathBuf::from("c2"),
                socket: PathBuf::from("k1"),
                state: PathBuf::from("s1"),
            }
        );
    }

    #[test]
    fn a_flag_without_its_path_is_an_error() {
        for flag in ["--config", "--socket", "--state"] {
            assert_eq!(parse(&[flag]), Err(format!("{flag} needs a path")));
            assert!(parse(&["--config", "c", flag]).is_err());
        }
    }

    #[test]
    fn an_unknown_argument_is_an_error() {
        for arg in ["--help", "-c", "aas.toml", "--config=/a/c.toml", ""] {
            assert_eq!(parse(&[arg]), Err(format!("unknown argument {arg}")));
        }
        assert!(parse(&["--config", "c", "extra"]).is_err());
    }
}
