//! The AirPlay output: the master's mix sent to AirPlay receivers named in
//! the config file. See `doc/plans/2026-11-21-airplay-output.md`.

pub mod config;
pub mod gate;
pub mod output;
pub mod raop;
pub mod rtp;
pub mod rtsp;
pub mod stream;

#[cfg(test)]
mod fake;

use crate::api::{AirplayView, ApiEvent};
use crate::rate::ClientId;
use aas_client::ring::Ring;
use config::{AirplayConfig, Connect, Protocol};
use output::{AirplayHandle, AirplayState, Status, TapReader};

/// A receiver as the control thread keeps it: its thread, once started,
/// and the id and rate of the monitor tap it reads.
pub struct Receiver {
    config: AirplayConfig,
    handle: Option<AirplayHandle>,
    tap: Option<(ClientId, u32)>,
    told: Option<Status>,
}

impl Receiver {
    pub fn new(config: AirplayConfig) -> Receiver {
        Receiver {
            config,
            handle: None,
            tap: None,
            told: None,
        }
    }

    /// Starts the thread, once.
    pub fn spawn(&mut self) {
        if self.handle.is_none() {
            self.handle = Some(output::spawn(self.config.clone()));
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
            error: status.error,
        }
    }
}
