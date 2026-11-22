//! The AirPlay output: the master's mix sent to AirPlay receivers named in
//! the config file. See `doc/plans/2026-11-21-airplay-output.md`.

pub mod ap2;
pub mod config;
pub mod gate;
pub mod meta;
pub mod output;
pub mod pairing;
pub mod raop;
pub mod rtp;
pub mod rtsp;
pub mod stream;

#[cfg(test)]
pub mod fake;

use crate::api::{AirplayView, ApiEvent};
use crate::rate::ClientId;
use aas_client::ring::Ring;
use config::{AirplayConfig, Connect, Protocol};
use output::{AirplayHandle, AirplayState, Status, TapReader};

/// A receiver as the control thread keeps it: its thread, once started,
/// and the id and rate of the monitor tap it reads.
pub struct Receiver {
    config: AirplayConfig,
    /// What the config file says of `enabled`, whatever the API set since.
    config_enabled: bool,
    now_playing: std::sync::Arc<crate::metadata::NowPlaying>,
    pairings: std::sync::Arc<ap2::keys::Pairings>,
    /// A pairing started and waiting for its PIN, kept here so that a
    /// receiver that is off can be paired too.
    held: std::sync::Arc<std::sync::Mutex<Option<pairing::Held>>>,
    handle: Option<AirplayHandle>,
    tap: Option<(ClientId, u32)>,
    told: Option<Status>,
}

impl Receiver {
    pub fn new(
        config: AirplayConfig,
        now_playing: std::sync::Arc<crate::metadata::NowPlaying>,
        pairings: std::sync::Arc<ap2::keys::Pairings>,
    ) -> Receiver {
        Receiver {
            config_enabled: config.enabled,
            config,
            now_playing,
            pairings,
            held: Default::default(),
            handle: None,
            tap: None,
            told: None,
        }
    }

    /// Whether it reads `now_playing`.
    pub fn shares(&self, now_playing: &std::sync::Arc<crate::metadata::NowPlaying>) -> bool {
        std::sync::Arc::ptr_eq(&self.now_playing, now_playing)
    }

    /// Whether its thread runs.
    pub fn running(&self) -> bool {
        self.handle.is_some()
    }

    /// Whether it is played to.
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn protocol(&self) -> Protocol {
        self.config.protocol
    }

    /// Runs a pairing step in a helper thread, on or off, so that neither
    /// the control thread nor a session waits for it; `answer` gets the
    /// result. A start nobody finishes is dropped after `PIN_WAIT`.
    pub fn pair(
        &mut self,
        pin: Option<String>,
        answer: tokio::sync::oneshot::Sender<crate::api::Answer>,
    ) {
        let (config, pairings, held) = (
            self.config.clone(),
            self.pairings.clone(),
            self.held.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("aas-pairing".into())
            .spawn(move || {
                let starting = pin.is_none();
                let now = std::time::Instant::now();
                let result = pairing::step(&config, &pairings, &held, pin.as_deref(), now);
                let started = starting && result.is_ok();
                let _ = answer.send(result);
                if started {
                    std::thread::sleep(pairing::PIN_WAIT + std::time::Duration::from_secs(1));
                    pairing::expire(&held, std::time::Instant::now());
                }
            });
        if let Err(e) = spawned {
            eprintln!(
                "aas: airplay \"{}\": no pairing thread: {e}",
                self.config.name
            );
        }
    }

    /// What the config file says of `enabled`.
    pub fn config_enabled(&self) -> bool {
        self.config_enabled
    }

    /// Turns it on or off. Off ends a session with TEARDOWN, in a thread of
    /// its own so that nobody waits for it, and gives the tap the engine is
    /// to drop; on starts the thread.
    pub fn set_enabled(&mut self, on: bool) -> Option<ClientId> {
        self.config.enabled = on;
        self.told = None;
        if on {
            self.spawn();
            return None;
        }
        if let Some(handle) = self.handle.take() {
            handle.ask_stop();
            // Dropping the handle joins the thread.
            std::thread::spawn(move || drop(handle));
        }
        self.take_tap()
    }

    /// Takes the state file's `enabled` before the thread starts.
    pub fn restore_enabled(&mut self, on: bool) {
        self.config.enabled = on;
    }

    /// Starts the thread, once, if it is on.
    pub fn spawn(&mut self) {
        if self.config.enabled && self.handle.is_none() {
            self.handle = Some(output::spawn(
                self.config.clone(),
                self.now_playing.clone(),
                self.pairings.clone(),
            ));
        }
    }

    /// Whether it needs a tap at `rate`: it runs and has none at that rate.
    pub fn wants_tap(&self, rate: u32) -> bool {
        self.handle.is_some() && self.tap.map(|(_, r)| r) != Some(rate)
    }

    /// The id of the tap it had, for the audio thread to remove.
    pub fn take_tap(&mut self) -> Option<ClientId> {
        self.tap.take().map(|(id, _)| id)
    }

    /// Hands the thread the reading end of its new tap.
    pub fn give_tap(&mut self, id: ClientId, rate: u32, reader: Ring) {
        if let Some(handle) = &self.handle {
            handle.tap(TapReader { ring: reader, rate });
            self.tap = Some((id, rate));
        }
    }

    /// Asks the thread to end its session, without waiting for it.
    pub fn ask_stop(&self) {
        if let Some(handle) = &self.handle {
            handle.ask_stop();
        }
    }

    /// Ends a session with TEARDOWN and joins the thread.
    pub fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.stop();
        }
    }

    fn status(&self) -> Status {
        self.handle.as_ref().map_or(
            Status {
                state: AirplayState::Idle,
                error: None,
            },
            AirplayHandle::status,
        )
    }

    /// The event for its state, if that changed since the last one.
    pub fn changed(&mut self) -> Option<ApiEvent> {
        let status = self.status();
        if self.told.as_ref() == Some(&status) {
            return None;
        }
        self.told = Some(status.clone());
        Some(ApiEvent::Airplay {
            name: self.config.name.clone(),
            state: status.state.name().to_string(),
            error: status.error,
        })
    }

    pub fn view(&self) -> AirplayView {
        let status = self.status();
        let c = &self.config;
        AirplayView {
            name: c.name.clone(),
            host: c.host.clone(),
            port: c.port,
            protocol: match c.protocol {
                Protocol::AirPlay1 => "airplay1",
                Protocol::AirPlay2 => "airplay2",
            },
            connect: match c.connect {
                Connect::Signal => "signal",
                Connect::Always => "always",
            },
            state: status.state.name(),
            enabled: c.enabled,
            paired: match c.protocol {
                Protocol::AirPlay1 => None,
                Protocol::AirPlay2 => Some(self.pairings.peer(&c.name).is_some()),
            },
            error: status.error,
        }
    }
}
