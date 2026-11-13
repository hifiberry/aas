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
    Box::new(aas::sink_alsa::AlsaSink::new(
        config.card.clone(),
        config.channels,
    ))
}

#[cfg(not(feature = "alsa-output"))]
fn output(config: &Config) -> Box<dyn Sink> {
    eprintln!("aas: warning: built without ALSA, the output goes nowhere");
    Box::new(aas::sink::NullSink::new(config.channels))
}

/// A card for each `[[output]]` table, buffering `further::BUFFERED`
/// periods, each looked for while it is gone. Two outputs on one card are
/// refused, by the card's index, whatever they name it by.
#[cfg(feature = "alsa-output")]
fn further_outputs(config: &Config) -> Result<Vec<server::FurtherSink>, String> {
    use aas::further::BUFFERED;
    use aas::sink_alsa::{AlsaSink, cards_text, distinct_cards};
    if config.outputs.is_empty() {
        return Ok(Vec::new());
    }
    let mut cards = vec![config.card.clone()];
    cards.extend(config.outputs.iter().map(|o| Some(o.card.clone())));
    distinct_cards(&cards_text(), &cards)?;
    let outputs = config.outputs.iter().map(|output| {
        let card = Some(output.card.clone());
        server::FurtherSink {
            sink: Box::new(AlsaSink::buffering(card.clone(), output.channels, BUFFERED)),
            probe: Some(card_probe(card)),
        }
    });
    Ok(outputs.collect())
}

#[cfg(not(feature = "alsa-output"))]
fn further_outputs(config: &Config) -> Result<Vec<server::FurtherSink>, String> {
    if !config.outputs.is_empty() {
        eprintln!("aas: warning: built without ALSA, the further outputs go nowhere");
    }
    let outputs = config.outputs.iter().map(|output| server::FurtherSink {
        sink: Box::new(aas::sink::NullSink::new(output.channels)),
        probe: None,
    });
    Ok(outputs.collect())
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

/// The analog input, if the config enables it. A card that is there at
/// the start is decided on now, by its driver, in `analog_capture`. A card
/// that is not there yet is looked for at each open, so a driver that
/// probes after the start is picked up; until then, and for a card with no
/// ADC aas knows, the open fails and the control thread tries again.
#[cfg(feature = "alsa-output")]
fn analog_input(config: &Config) -> Option<aas::control::OpenCapture> {
    use aas::capture::{Capture, OpenError};
    use aas::sink_alsa::{AlsaCapture, AnalogCard, analog_card, capture_card, cards_text};
    match analog_card(&cards_text(), config) {
        AnalogCard::Off => None,
        AnalogCard::Present { card, driver } => {
            let open: aas::control::OpenCapture = Box::new(move |rate, period| {
                AlsaCapture::open(&card, rate, period)
                    .map(|c| Box::new(c) as Box<dyn Capture>)
                    .map_err(OpenError::Again)
            });
            server::analog_capture(config, &driver, open)
        }
        AnalogCard::Absent => {
            eprintln!("aas: analog input on, its card is not there yet");
            let wanted = config.card.clone();
            Some(Box::new(move |rate, period| {
                let card = capture_card(&cards_text(), wanted.as_deref())?;
                AlsaCapture::open(&card, rate, period)
                    .map(|c| Box::new(c) as Box<dyn Capture>)
                    .map_err(OpenError::Again)
            }))
        }
    }
}

#[cfg(not(feature = "alsa-output"))]
fn analog_input(config: &Config) -> Option<aas::control::OpenCapture> {
    if config.analog.enabled {
        eprintln!("aas: warning: built without ALSA, the analog input stays off");
    }
    None
}

/// Looks for `card` on the control thread while it is gone, so that the
/// thread that plays to it tries to open it only once it is there. A build
/// without ALSA never asks: its outputs are never gone.
fn card_probe(card: Option<String>) -> server::CardProbe {
    let mut list = server::ListChange::default();
    Box::new(move |refused| {
        if refused {
            list.changed(aas::sink_alsa::card_ids())
        } else {
            list.reset();
            aas::sink_alsa::card_is_present(card.as_deref())
        }
    })
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
    if rt::compaction_moves_locked_pages() == Some(true) {
        eprintln!(
            "aas: warning: vm.compact_unevictable_allowed is 1: memory compaction can stall \
             the audio thread; install dist/90-aas.conf in /etc/sysctl.d"
        );
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    handle_signals(shutdown.clone());
    let sink = output(&config);
    let further = match further_outputs(&config) {
        Ok(further) => further,
        Err(e) => {
            eprintln!("aas: {e}");
            return ExitCode::FAILURE;
        }
    };
    let devices = server::Devices {
        sink,
        probe: Some(card_probe(config.card.clone())),
        further,
    };
    let hardware = hardware_volume(&config);
    let capture = analog_input(&config);
    let (socket, state) = (&args.socket, &args.state);
    match server::run_outputs(config, devices, socket, state, hardware, capture, shutdown) {
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
