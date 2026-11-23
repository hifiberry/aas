//! The AES67 input on the control thread: the `aes67` source in the
//! engine, and the threads [`Aes67`] runs that feed it.
//!
//! The source is a client at 48 kHz on the `aes67` bus, named `aes67`: its
//! volume is a client volume, which starts from the config's `volume_db`.
//! It runs while the input receives the selected stream, so it counts for
//! the rate rules and the idle timer exactly then.

use super::{Control, HeaderView, eventfd, min_ring_frames};
use crate::aes67::config::Aes67Config;
use crate::aes67::input::RATE;
use crate::aes67::{Aes67, InputLink};
use crate::engine::{Command, Source};
use crate::latency::PERIODS;
use crate::matrix::Bus;
use crate::rate::ClientId;
use crate::resample::Resampler;
use crate::state::{AES67_CLIENT, StoredAes67};
use aas_client::ring::Ring;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

/// The `aes67` source as the control thread knows it.
pub(super) struct Aes67Source {
    pub(super) id: ClientId,
    capacity: usize,
    pub(super) running: bool,
    /// The output rate and period of the resampler the source has.
    resampler: Option<(u32, u32)>,
    /// That resampler's delay, in output frames.
    filter: u32,
    /// The control thread's own view of the ring, for the engine's delay
    /// after it.
    view: HeaderView,
    /// What the input thread reads of the engine.
    output_delay: Arc<AtomicU32>,
    master_rate: Arc<AtomicU32>,
    /// Tells the input thread the engine reads the ring.
    draining: Arc<AtomicBool>,
}

impl Control {
    /// Adds the `aes67` source to the engine and has `make` start AES67 on
    /// its ring, with what the state file keeps. Without an `[aes67]`
    /// table, nothing.
    pub(super) fn start_aes67(
        &mut self,
        batch: &mut Vec<Command>,
        make: impl FnOnce(Aes67Config, Option<&StoredAes67>, InputLink) -> Aes67,
    ) {
        let Some(config) = self.aes67_config.clone() else {
            return;
        };
        let Some(link) = self.add_aes67_source(batch) else {
            return;
        };
        self.aes67 = Some(make(config, self.state.aes67.as_ref(), link));
    }

    /// The source, sized as a client's ring is.
    fn add_aes67_source(&mut self, batch: &mut Vec<Command>) -> Option<InputLink> {
        let capacity = min_ring_frames(RATE, self.startup_latency_ms);
        let made = Ring::create(capacity).and_then(|(ring, memfd)| {
            let view = HeaderView::map(memfd.as_fd())?;
            Ok((ring, memfd, view, eventfd()?))
        });
        let (ring, memfd, view, wake) = match made {
            Ok(made) => made,
            Err(e) => {
                eprintln!("aas: aes67: no ring for the input: {e}");
                return None;
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        if self.table_len == self.table_capacity {
            self.table_capacity *= 2;
            batch.push(Command::Grow(Vec::with_capacity(self.table_capacity)));
        }
        // Nothing reads the eventfd the engine writes: the input thread
        // keeps the ring full on its own timing.
        let mut source = Source::new(id, ring, wake, RATE);
        source.bus = Bus::Aes67;
        source.gain = self.client_gain(AES67_CLIENT);
        batch.push(Command::Add(Box::new(source)));
        self.table_len += 1;
        self.aes67_input = true;
        let output_delay = Arc::new(AtomicU32::new(0));
        let master_rate = Arc::new(AtomicU32::new(RATE));
        let draining = Arc::new(AtomicBool::new(false));
        self.aes67_source = Some(Aes67Source {
            id,
            capacity: capacity as usize,
            running: false,
            resampler: None,
            filter: 0,
            view,
            output_delay: output_delay.clone(),
            master_rate: master_rate.clone(),
            draining: draining.clone(),
        });
        Some(InputLink {
            memfd,
            output_delay,
            master_rate,
            draining,
        })
    }

    /// On the timer: AES67's own tick, its events, the source running
    /// while the input receives, and what the input reads of the engine.
    pub(super) fn sync_aes67(&mut self, now: Instant) {
        let Some(aes67) = &mut self.aes67 else {
            return;
        };
        aes67.tick(now);
        let receiving = aes67.receiving();
        if let Some(event) = aes67.changed() {
            self.publish(event);
        }
        self.aes67_follow(receiving, now);
        self.aes67_delay();
        self.aes67_draining();
    }

    /// Runs the source while the input receives, as a client's stream
    /// runs while it plays. A stopped source's ring is emptied, so what
    /// was left in it does not play at the next start.
    fn aes67_follow(&mut self, receiving: bool, now: Instant) {
        let urgent = self.running_count() == 0;
        let Some(source) = &mut self.aes67_source else {
            return;
        };
        if receiving == source.running {
            return;
        }
        source.running = receiving;
        let id = source.id;
        self.new_rate_decision();
        let t = now - self.epoch;
        let mut batch = Vec::new();
        if receiving {
            self.client_changed(AES67_CLIENT, "running");
            self.policy.client_started(id, RATE, t);
            self.decide(now, urgent, &mut batch);
            batch.push(Command::SetRunning(id, true));
        } else {
            self.client_changed(AES67_CLIENT, "stopped");
            self.policy.client_stopped(id, t);
            self.aes67_draining();
            batch.push(Command::SetRunning(id, false));
            batch.push(Command::Flush(id));
            self.decide(now, false, &mut batch);
        }
        self.send(batch);
    }

    /// Whether the engine reads the source's ring: it runs and the master
    /// is open, not reopening. Until then the input writes nothing. Called
    /// by `send`, so it follows every change of the output as it is sent.
    pub(super) fn aes67_draining(&self) {
        let Some(source) = &self.aes67_source else {
            return;
        };
        let on = source.running && self.output.is_some() && self.reopening == 0;
        source.draining.store(on, Ordering::Relaxed);
    }

    /// Tells the input thread the master's rate and the delay from the
    /// ring to the card, in master frames: the engine's own figure while
    /// the source runs, which has the card's buffer, the plugins and the
    /// delay line in it; before that, the buffer the card is asked for, the
    /// delay line and the resampler's filter.
    fn aes67_delay(&self) {
        let (Some(source), Some((rate, period))) = (&self.aes67_source, self.output) else {
            return;
        };
        let measured = source
            .view
            .header()
            .output_delay_frames
            .load(Ordering::Relaxed);
        let frames = if source.running && measured > 0 {
            (u64::from(measured) * u64::from(rate) / u64::from(RATE)) as u32
        } else {
            PERIODS * period + self.delay.unwrap_or(0) + source.filter
        };
        source.output_delay.store(frames, Ordering::Relaxed);
        source.master_rate.store(rate, Ordering::Relaxed);
    }

    /// Gives the `aes67` source a resampler to the output's rate when that
    /// is not 48 kHz, as `sync_resamplers` does for a client's.
    pub(super) fn sync_aes67_resampler(&mut self, batch: &mut Vec<Command>) {
        let (quality, format) = (self.quality, self.format);
        let Some(source) = &mut self.aes67_source else {
            return;
        };
        let want = match self.output {
            Some((rate, period)) if rate != RATE => Some((rate, period)),
            _ => None,
        };
        if want == source.resampler {
            return;
        }
        source.resampler = want;
        source.filter = 0;
        let resampler = want.map(|(rate, period)| {
            let r = Resampler::new(RATE, rate, period, quality, format);
            source.filter = r.delay_frames();
            if r.input_frames_max() > source.capacity {
                eprintln!(
                    "aas: aes67: a ring of {} frames is too small to resample from {RATE} Hz",
                    source.capacity
                );
            }
            Box::new(r)
        });
        batch.push(Command::SetResampler(source.id, resampler));
    }

    /// Whether the `aes67` source is running.
    pub(super) fn aes67_running(&self) -> bool {
        self.aes67_source.as_ref().is_some_and(|s| s.running)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{FakeEngine, started};
    use super::*;
    use crate::aes67::testing::{FakeInput, announce, config};
    use crate::api::InputPatch;
    use crate::config::Config;
    use crate::format::db_to_gain;
    use crate::state::StoredInput;
    use std::net::Ipv4Addr;

    const THEM: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);

    /// A started control thread with AES67 on a fake input, its config
    /// changed by `edit`, and the client volumes `clients` stored.
    fn with_aes67(
        edit: impl FnOnce(&mut Aes67Config),
        clients: &[(&str, f32)],
    ) -> (Control, FakeEngine, Instant, FakeInput) {
        let (mut control, mut engine, now) = started(&Config::default(), None);
        engine.take(true);
        control.handle_events(now);
        for (name, db) in clients {
            let input = StoredInput {
                volume_db: *db,
                muted: false,
            };
            control.state.clients.insert(name.to_string(), input);
        }
        let mut c = config();
        edit(&mut c);
        control.aes67_config = Some(c);
        let input = FakeInput::default();
        let fake = input.clone();
        let mut batch = Vec::new();
        control.start_aes67(&mut batch, |c, stored, _link| {
            Aes67::with_input(c, stored, None, fake, None)
        });
        control.send(batch);
        (control, engine, now, input)
    }

    #[test]
    fn the_aes67_source_uses_the_client_volume_named_aes67() {
        // Without a client volume, the config's.
        let (control, mut engine, _, _) = with_aes67(|c| c.input.volume_db = -6.0, &[]);
        let seen = engine.take(true);
        assert!(seen.contains(&"Add".to_string()), "{seen:?}");
        let id = control.aes67_source.as_ref().unwrap().id;
        assert_eq!(engine.added, [(id, db_to_gain(-6.0))]);
        assert_eq!(engine.buses, [(id, Bus::Aes67)]);
        assert!(control.aes67_input);
        // A stored one wins.
        let edit = |c: &mut Aes67Config| c.input.volume_db = -6.0;
        let (mut control, mut engine, now, _) = with_aes67(edit, &[("aes67", -10.0)]);
        engine.take(true);
        let id = control.aes67_source.as_ref().unwrap().id;
        assert_eq!(engine.added, [(id, db_to_gain(-10.0))]);
        // Set as any client's, it reaches the source.
        let patch = InputPatch {
            volume_db: Some(-3.0),
            muted: None,
        };
        control
            .set_client_input("aes67".into(), patch, now)
            .unwrap();
        let seen = engine.take(true);
        let want = format!("SetSourceGain {id} {}", db_to_gain(-3.0));
        assert!(seen.contains(&want), "{seen:?}");
        // Back at the config's volume it is no longer stored, and 0 dB is.
        let patch = InputPatch {
            volume_db: Some(-6.0),
            muted: None,
        };
        control
            .set_client_input("aes67".into(), patch, now)
            .unwrap();
        assert!(!control.state.clients.contains_key("aes67"));
        let patch = InputPatch {
            volume_db: Some(0.0),
            muted: None,
        };
        let view = control
            .set_client_input("aes67".into(), patch, now)
            .unwrap();
        assert_eq!(view.volume_db, 0.0);
        assert!(control.state.clients.contains_key("aes67"));
        let views = control.input_views();
        assert!(views.iter().any(|v| v.id == "clients/aes67"), "{views:?}");
    }

    #[test]
    fn the_source_runs_only_while_receiving() {
        let (mut control, mut engine, now, input) =
            with_aes67(|c| c.input.stream = "studio".into(), &[]);
        engine.take(true);
        let id = control.aes67_source.as_ref().unwrap().id;
        // Packets of nothing selected do not count.
        input.set_receiving(true);
        control.tick(now);
        assert!(
            !engine
                .take(true)
                .iter()
                .any(|s| s.starts_with("SetRunning"))
        );
        assert_eq!(control.running_count(), 0);
        // The selection announced, and arriving.
        let aes67 = control.aes67.as_ref().unwrap();
        announce(aes67, 1, THEM, "studio");
        control.tick(now);
        let seen = engine.take(true);
        assert!(seen.contains(&format!("SetRunning {id} true")), "{seen:?}");
        assert_eq!(control.running_count(), 1);
        assert_eq!(control.idle_since, None);
        // It stops arriving.
        input.set_receiving(false);
        control.tick(now);
        let seen = engine.take(true);
        assert!(seen.contains(&format!("SetRunning {id} false")), "{seen:?}");
        assert_eq!(control.running_count(), 0);
        assert!(control.idle_since.is_some());
    }

    #[test]
    fn draining_follows_the_source_running_and_the_master_open() {
        let (mut control, mut engine, now, input) =
            with_aes67(|c| c.input.stream = "studio".into(), &[]);
        engine.take(true);
        control.handle_events(now);
        let draining = |control: &Control| {
            let source = control.aes67_source.as_ref().unwrap();
            source.draining.load(Ordering::Relaxed)
        };
        let id = control.aes67_source.as_ref().unwrap().id;
        control.tick(now);
        assert!(!draining(&control), "not running");
        announce(control.aes67.as_ref().unwrap(), 1, THEM, "studio");
        input.set_receiving(true);
        control.tick(now);
        engine.take(true);
        control.handle_events(now);
        control.tick(now);
        assert!(control.output.is_some() && control.reopening == 0);
        assert!(draining(&control));
        // While the master reopens the engine does not read the ring.
        control.reopening = 1;
        control.tick(now);
        assert!(!draining(&control));
        control.reopening = 0;
        control.tick(now);
        assert!(draining(&control));
        // Stopped: not read, and the ring is emptied.
        input.set_receiving(false);
        control.tick(now);
        assert!(!draining(&control));
        let seen = engine.take(true);
        assert!(seen.contains(&format!("Flush {id}")), "{seen:?}");
    }

    #[test]
    fn a_settled_rate_switch_stops_draining_until_the_card_is_open() {
        let (mut control, mut engine, now, input) =
            with_aes67(|c| c.input.stream = "studio".into(), &[]);
        let draining = |control: &Control| {
            let source = control.aes67_source.as_ref().unwrap();
            source.draining.load(Ordering::Relaxed)
        };
        announce(control.aes67.as_ref().unwrap(), 1, THEM, "studio");
        input.set_receiving(true);
        control.tick(now);
        engine.take(true);
        control.handle_events(now);
        control.tick(now);
        assert!(draining(&control));
        // The master at another rate, its switch to 48 kHz due now: the
        // tick reopens the card, and the ring is not read meanwhile.
        let (_, period) = control.output.unwrap();
        control.output = Some((44_100, period));
        control.switch_at = Some(now);
        control.tick(now);
        assert_eq!(control.reopening, 1);
        assert!(!draining(&control));
        engine.take(true);
        control.handle_events(now);
        assert_eq!(control.reopening, 0);
        assert!(draining(&control));
    }

    #[test]
    fn the_input_learns_the_masters_rate_and_the_delay_after_the_ring() {
        let (mut control, mut engine, now, _) = with_aes67(|_| {}, &[]);
        engine.take(true);
        control.tick(now);
        let source = control.aes67_source.as_ref().unwrap();
        let (rate, period) = control.output.unwrap();
        assert_eq!(source.master_rate.load(Ordering::Relaxed), rate);
        let delay = PERIODS * period + control.delay.unwrap_or(0) + source.filter;
        assert_eq!(source.output_delay.load(Ordering::Relaxed), delay);
        // At another rate than 48 kHz the source has a resampler, and its
        // filter is part of the delay.
        let mut batch = Vec::new();
        control.output = Some((44_100, period));
        control.sync_aes67_resampler(&mut batch);
        control.send(batch);
        assert!(has_resampler(&engine.take(true)));
        control.aes67_delay();
        let source = control.aes67_source.as_ref().unwrap();
        assert!(source.filter > 0);
        let delay = PERIODS * period + control.delay.unwrap_or(0) + source.filter;
        assert_eq!(source.output_delay.load(Ordering::Relaxed), delay);
        assert_eq!(source.master_rate.load(Ordering::Relaxed), 44_100);
    }

    fn has_resampler(seen: &[String]) -> bool {
        seen.iter()
            .any(|s| s.starts_with("SetResampler") && s.ends_with("true"))
    }
}
