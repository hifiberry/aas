//! The control thread's side of the API: each call is handled here, between
//! client requests, exactly like one. Views are built from what `Control`
//! holds; nothing waits on the audio thread.

use super::{ConnId, Control, Stream};
use crate::api::{
    self, ApiError, ApiEvent, Call, ClientView, InputPatch, InputView, OutputPatch, OutputState,
    OutputView, StateView,
};
use crate::bitperfect::NotBitPerfect;
use crate::config::{Config, DEFAULT_IDLE_DELAY_S, Format, Idle};
use crate::counters::Counters;
use crate::engine::Command;
use crate::format::db_to_gain;
use crate::state::{MAX_CLIENTS, Stored, StoredInput, StoredOutput};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tokio::sync::broadcast;

/// What was last published of the outputs, to publish only changes.
pub(super) struct Published {
    /// The rate the master's cycle runs at, None while it is off.
    rate: Option<u32>,
    /// Each output's state, the master first.
    states: Vec<&'static str>,
}

/// The analog input as the config sets it: its level, not muted.
pub(super) fn config_input(config: &Config) -> StoredInput {
    StoredInput {
        volume_db: config.analog.volume_db,
        muted: false,
    }
}

/// The gain an input plays at: 0 when muted.
pub(super) fn input_gain(input: &StoredInput) -> f32 {
    if input.muted {
        0.0
    } else {
        db_to_gain(input.volume_db)
    }
}

/// An idle rule as its two config keys say it. `idle = "zeros"` has no
/// `idle_delay_s`, which then says what it says when left out.
fn idle_parts(idle: Idle) -> (&'static str, u32) {
    match idle {
        Idle::Zeros => ("zeros", DEFAULT_IDLE_DELAY_S),
        Idle::Off { delay_s } => ("off", delay_s),
    }
}

fn idle_from_parts(kind: &str, delay_s: u32) -> Idle {
    match kind {
        "zeros" => Idle::Zeros,
        _ => Idle::Off { delay_s },
    }
}

/// The idle rule to start with: each of its keys stored, while the config
/// key still says what it said then, else as the config says it.
pub(super) fn resolve_idle(stored: Option<&StoredOutput>, config: Idle) -> Idle {
    let (kind, delay) = idle_parts(config);
    let kind = Stored::resolve(stored.and_then(|s| s.idle.as_ref()), &kind.to_string());
    let delay = Stored::resolve(stored.and_then(|s| s.idle_delay_s.as_ref()), &delay);
    idle_from_parts(&kind, delay)
}

/// The idle rule a patch makes of `current`, with the config's rules, or
/// None when it names neither key.
fn patched_idle(current: Idle, patch: &OutputPatch) -> Result<Option<Idle>, ApiError> {
    if patch.idle.is_none() && patch.idle_delay_s.is_none() {
        return Ok(None);
    }
    let (current_kind, current_delay) = idle_parts(current);
    let kind = match patch.idle.as_deref() {
        None => current_kind,
        Some(kind @ ("zeros" | "off")) => kind,
        Some(_) => return Err(ApiError::unprocessable("idle must be \"zeros\" or \"off\"")),
    };
    if kind == "zeros" && patch.idle_delay_s.is_some() {
        return Err(ApiError::unprocessable("idle_delay_s needs idle = \"off\""));
    }
    let delay = patch.idle_delay_s.unwrap_or(current_delay);
    Ok(Some(idle_from_parts(kind, delay)))
}

/// Refuses an input volume out of range, which includes one not finite.
fn check_input(patch: &InputPatch) -> Result<(), ApiError> {
    match patch.volume_db {
        Some(db) if !(db.is_finite() && (-60.0..=0.0).contains(&db)) => {
            Err(ApiError::unprocessable("volume_db must be from -60 to 0"))
        }
        _ => Ok(()),
    }
}

fn patched_input(current: &StoredInput, patch: &InputPatch) -> StoredInput {
    StoredInput {
        volume_db: patch.volume_db.unwrap_or(current.volume_db),
        muted: patch.muted.unwrap_or(current.muted),
    }
}

/// A client without a stored volume plays at 0 dB.
const UNITY: StoredInput = StoredInput {
    volume_db: 0.0,
    muted: false,
};

fn format_name(format: Format) -> &'static str {
    match format {
        Format::I32 => "i32",
        Format::F32 => "f32",
        Format::F64 => "f64",
    }
}

impl Control {
    /// Publishes events for the API from now on. A send with no subscriber
    /// fails, which changes nothing.
    pub fn set_api(&mut self, events: broadcast::Sender<ApiEvent>) {
        self.api = Some(events);
        self.published = Some(self.snapshot());
    }

    /// The audio thread's counters, for `GET /state` to read its
    /// bit-perfect decision from.
    pub fn set_counters(&mut self, counters: Arc<Counters>) {
        self.counters = counters;
    }

    /// Handles one call of the API and gives its answer.
    pub fn call(&mut self, call: Call, now: Instant) -> api::Answer {
        let answer = match call {
            Call::State => api::answer(&self.state_view()),
            Call::Outputs => {
                let views: Vec<OutputView> = (0..=self.further.len())
                    .filter_map(|index| self.output_view(index))
                    .collect();
                api::answer(&views)
            }
            Call::Output(id) => {
                let index = self.output_index(id)?;
                self.output_answer(index)
            }
            Call::PatchOutput(id, patch) => self.patch_output(id, patch, now),
            Call::Inputs => api::answer(&self.input_views()),
            Call::Analog => {
                self.analog_present()?;
                api::answer(&self.analog_view())
            }
            Call::PatchAnalog(patch) => self.patch_analog(patch, now),
            Call::Client(name) => {
                let known = self.state.clients.contains_key(&name) || self.playing(&name);
                if !known {
                    return Err(ApiError::not_found(format!("no client \"{name}\"")));
                }
                api::answer(&self.client_view(&name))
            }
            Call::PatchClient(name, patch) => self.patch_client(name, patch, now),
            Call::Matrix | Call::PutMatrix(_) => {
                Err(ApiError::not_found("the matrix is not served yet"))
            }
        };
        self.publish_changes();
        answer
    }

    /// Sends an event to the API's subscribers, if any.
    pub(super) fn publish(&self, event: ApiEvent) {
        if let Some(api) = &self.api {
            // An error only means nobody listens.
            let _ = api.send(event);
        }
    }

    /// Publishes the output at engine index `index` as it is now.
    pub(super) fn publish_output(&self, index: usize) {
        if self.api.is_some()
            && let Some(view) = self.output_view(index)
        {
            self.publish(view.event());
        }
    }

    pub(super) fn client_changed(&self, name: &str, change: &str) {
        self.publish(ApiEvent::Client {
            name: name.to_string(),
            change: change.to_string(),
        });
    }

    /// A stream of `conn` is replaced: if it ran, its client has stopped.
    pub(super) fn stream_gone(&self, conn: ConnId, stream: &Stream) {
        if let (true, Some(c)) = (stream.running, self.conns.get(&conn)) {
            self.client_changed(&c.name, "stopped");
        }
    }

    /// Publishes what has changed since the last time of the master's rate
    /// and of each output's state. Called after everything that can change
    /// them.
    pub(super) fn publish_changes(&mut self) {
        if self.api.is_none() {
            return;
        }
        let now = self.snapshot();
        let Some(before) = self.published.replace(now) else {
            return;
        };
        let rate = self.cycle_rate();
        if rate != before.rate
            && let Some(rate) = rate
        {
            self.publish(ApiEvent::Rate { rate });
        }
        for index in 0..=self.further.len() {
            if before.states.get(index) != Some(&self.output_state(index)) {
                self.publish_output(index);
            }
        }
    }

    fn snapshot(&self) -> Published {
        Published {
            rate: self.cycle_rate(),
            states: (0..=self.further.len())
                .map(|index| self.output_state(index))
                .collect(),
        }
    }

    /// The rate the master's cycle runs at, on its card or parked on the
    /// timer. None while it is off.
    fn cycle_rate(&self) -> Option<u32> {
        self.output.or(self.parked).map(|(rate, _)| rate)
    }

    /// The gain the streams of a client called `name` play at.
    pub(super) fn client_gain(&self, name: &str) -> f32 {
        input_gain(self.state.clients.get(name).unwrap_or(&UNITY))
    }

    /// The engine index of the output with API id `id`: the master is 1.
    fn output_index(&self, id: usize) -> Result<usize, ApiError> {
        match id.checked_sub(1) {
            Some(index) if index <= self.further.len() => Ok(index),
            _ => Err(ApiError::not_found(format!("no output {id}"))),
        }
    }

    /// An output's state as the control thread knows it: `"open"`,
    /// `"parked"` (the master's card closed while a further output keeps
    /// the cycle running) or `"off"`.
    fn output_state(&self, index: usize) -> &'static str {
        if index == 0 {
            if self.output.is_some() {
                "open"
            } else if self.parked.is_some() {
                "parked"
            } else {
                "off"
            }
        } else if self.further.get(index - 1).is_some_and(|o| o.open) {
            "open"
        } else {
            "off"
        }
    }

    fn output_view(&self, index: usize) -> Option<OutputView> {
        let (card, volume, volume_db, idle) = if index == 0 {
            (
                self.card.clone(),
                Some(self.volume.percent()),
                None,
                self.idle,
            )
        } else {
            let output = self.further.get(index - 1)?;
            let volume_db = Some(output.volume_db);
            (output.card.clone(), None, volume_db, output.idle)
        };
        let (kind, delay) = idle_parts(idle);
        Some(OutputView {
            id: index + 1,
            card,
            volume,
            volume_db,
            idle: kind.to_string(),
            idle_delay_s: (idle != Idle::Zeros).then_some(delay),
            state: self.output_state(index).to_string(),
        })
    }

    fn output_answer(&self, index: usize) -> api::Answer {
        match self.output_view(index) {
            Some(view) => api::answer(&view),
            None => Err(ApiError::not_found(format!("no output {}", index + 1))),
        }
    }

    /// Checks every field first, so that a refused patch changes nothing.
    fn patch_output(&mut self, id: usize, patch: OutputPatch, now: Instant) -> api::Answer {
        let index = self.output_index(id)?;
        if index == 0 {
            if patch.volume_db.is_some() {
                return Err(ApiError::unprocessable(
                    "volume_db is only for a further output; the master has volume",
                ));
            }
            if patch.volume.is_some_and(|v| v > 100) {
                return Err(ApiError::unprocessable("volume must be from 0 to 100"));
            }
        } else {
            if patch.volume.is_some() {
                return Err(ApiError::unprocessable(
                    "volume is only for the master; a further output has volume_db",
                ));
            }
            if patch
                .volume_db
                .is_some_and(|db| !(db.is_finite() && (-60.0..=0.0).contains(&db)))
            {
                return Err(ApiError::unprocessable("volume_db must be from -60 to 0"));
            }
        }
        let current = match index {
            0 => self.idle,
            _ => self.further[index - 1].idle,
        };
        let idle = patched_idle(current, &patch)?.filter(|&idle| idle != current);
        if let Some(percent) = patch.volume {
            self.set_volume(percent, now);
        }
        let mut changed = false;
        if let Some(db) = patch.volume_db
            && db != self.further[index - 1].volume_db
        {
            let output = &mut self.further[index - 1];
            output.volume_db = db;
            let (card, config, engine) =
                (output.card.clone(), output.config_volume_db, output.index);
            let gain = self.further_gain(&self.further[index - 1]);
            self.send(vec![Command::SetGain(engine, gain)]);
            let entry = self.state.outputs.entry(card).or_default();
            entry.volume_db = Some(Stored { value: db, config });
            changed = true;
        }
        if let Some(idle) = idle {
            self.set_idle(index, idle);
            changed = true;
        }
        if changed {
            self.publish_output(index);
            self.save_later(now);
        }
        self.output_answer(index)
    }

    /// Sets an output's idle rule, which applies at its next idle decision,
    /// and stores it with what the config says. A further output its rule
    /// has closed stays closed until a source is active, as the master
    /// does.
    fn set_idle(&mut self, index: usize, idle: Idle) {
        let (card, config) = if index == 0 {
            self.idle = idle;
            (self.card.clone(), self.config_idle)
        } else {
            let cycle = self.output.or(self.parked).is_some();
            let output = &mut self.further[index - 1];
            // Closed while it could play: by its idle rule.
            if cycle && output.configured && output.pending.is_none() && !output.open {
                output.held_off = true;
            }
            output.idle = idle;
            (output.card.clone(), output.config_idle)
        };
        let ((kind, delay), (config_kind, config_delay)) = (idle_parts(idle), idle_parts(config));
        let entry = self.state.outputs.entry(card).or_default();
        entry.idle = Some(Stored {
            value: kind.to_string(),
            config: config_kind.to_string(),
        });
        entry.idle_delay_s = Some(Stored {
            value: delay,
            config: config_delay,
        });
    }

    fn analog_present(&self) -> Result<(), ApiError> {
        match self.analog {
            Some(_) => Ok(()),
            None => Err(ApiError::not_found("no analog input")),
        }
    }

    fn analog_view(&self) -> InputView {
        InputView {
            id: "analog".into(),
            volume_db: self.analog_input.volume_db,
            muted: self.analog_input.muted,
        }
    }

    fn patch_analog(&mut self, patch: InputPatch, now: Instant) -> api::Answer {
        self.analog_present()?;
        check_input(&patch)?;
        let input = patched_input(&self.analog_input, &patch);
        if input != self.analog_input {
            self.analog_input = input.clone();
            self.send(vec![Command::SetAnalogGain(input_gain(&input))]);
            self.state.analog = Some(Stored {
                value: input,
                config: self.analog_config.clone(),
            });
            self.publish(self.analog_view().event());
            self.save_later(now);
        }
        api::answer(&self.analog_view())
    }

    /// Whether a connection called `name` has a stream.
    fn playing(&self, name: &str) -> bool {
        let mut conns = self.conns.values();
        conns.any(|c| c.name == name && c.stream.is_some())
    }

    fn client_view(&self, name: &str) -> InputView {
        let input = self.state.clients.get(name).unwrap_or(&UNITY);
        InputView {
            id: format!("clients/{name}"),
            volume_db: input.volume_db,
            muted: input.muted,
        }
    }

    /// Sets the volume of every stream of the clients called `name`, now
    /// and after a reconnect. A name not connected can be set too.
    fn patch_client(&mut self, name: String, patch: InputPatch, now: Instant) -> api::Answer {
        check_input(&patch)?;
        let stored = self.state.clients.get(&name);
        let input = patched_input(stored.unwrap_or(&UNITY), &patch);
        if input == *stored.unwrap_or(&UNITY) {
            return api::answer(&self.client_view(&name));
        }
        if stored.is_none() && self.state.clients.len() >= MAX_CLIENTS {
            return Err(ApiError::unprocessable(format!(
                "too many clients with a volume ({MAX_CLIENTS})"
            )));
        }
        let gain = input_gain(&input);
        self.state.clients.insert(name.clone(), input);
        let streams = self.conns.values().filter(|c| c.name == name);
        let batch = streams
            .filter_map(|c| c.stream.as_ref())
            .map(|s| Command::SetSourceGain(s.id, gain))
            .collect();
        self.send(batch);
        let view = self.client_view(&name);
        self.publish(view.event());
        self.save_later(now);
        api::answer(&view)
    }

    /// The analog input, if there is one, and every client known by name:
    /// those with a stored volume and those with a stream.
    fn input_views(&self) -> Vec<InputView> {
        let mut names: BTreeSet<&str> = self.state.clients.keys().map(String::as_str).collect();
        let streams = self.conns.values().filter(|c| c.stream.is_some());
        names.extend(streams.map(|c| c.name.as_str()));
        let analog = self.analog.as_ref().map(|_| self.analog_view());
        analog
            .into_iter()
            .chain(names.into_iter().map(|name| self.client_view(name)))
            .collect()
    }

    /// Whether the master is bit-perfect, and why not.
    ///
    /// A DoP stream that holds the output reaches the card unaltered: no
    /// volume, plugin or mixing touches it. So it is bit-perfect by nature,
    /// and is reported so whatever the engine last decided, which it
    /// decides only in cycles that play PCM. While the master's card is
    /// closed or parked it plays nothing: not bit-perfect, with the reason
    /// "no source", rather than the decision of the last cycle it played.
    /// Otherwise the engine's own decision, from its counters.
    fn bit_perfect(&self) -> Result<(), NotBitPerfect> {
        if self.dop_owner.is_some() {
            return Ok(());
        }
        if self.output.is_none() {
            return Err(NotBitPerfect::NoSource);
        }
        let code = self.counters.bit_perfect.load(Ordering::Relaxed);
        NotBitPerfect::from_code(code).unwrap_or(Err(NotBitPerfect::NoSource))
    }

    fn state_view(&self) -> StateView {
        let outputs = (0..=self.further.len())
            .filter_map(|index| self.output_view(index))
            .map(|view| OutputState {
                id: view.id,
                card: view.card,
                state: view.state,
            })
            .collect();
        let clients = self
            .conns
            .values()
            .map(|c| {
                let stream = c.stream.as_ref().map(|s| (s.rate, s.running));
                let recorder = c.recorder.as_ref().map(|r| (r.rate, r.running));
                let (rate, running) = match stream.or(recorder) {
                    Some((rate, running)) => (Some(rate), running),
                    None => (None, false),
                };
                ClientView {
                    name: c.name.clone(),
                    rate,
                    running,
                }
            })
            .collect();
        let decision = self.bit_perfect();
        StateView {
            rate: self.cycle_rate(),
            format: format_name(self.format).to_string(),
            outputs,
            clients,
            bit_perfect: decision.is_ok(),
            reason: decision.err().map(|r| r.name().to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        FakeEngine, Opener, analog_config, configure, further, has, started, started_with,
        unstarted_with,
    };
    use super::super::*;
    use crate::api::{ApiError, ApiEvent, Call, InputPatch, OutputPatch};
    use crate::bitperfect::NotBitPerfect;
    use crate::config::{self, Idle};
    use crate::counters::Counters;
    use crate::format::db_to_gain;
    use crate::state::{Stored, StoredInput, StoredOutput};
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    fn subscribe(control: &mut Control) -> broadcast::Receiver<ApiEvent> {
        let (events, receiver) = broadcast::channel(256);
        control.set_api(events);
        receiver
    }

    /// The events published since the last time.
    fn events(receiver: &mut broadcast::Receiver<ApiEvent>) -> Vec<ApiEvent> {
        let mut seen = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            seen.push(event);
        }
        seen
    }

    fn volume(percent: u8) -> OutputPatch {
        OutputPatch {
            volume: Some(percent),
            ..OutputPatch::default()
        }
    }

    fn offset(db: f32) -> OutputPatch {
        OutputPatch {
            volume_db: Some(db),
            ..OutputPatch::default()
        }
    }

    fn input_db(db: f32) -> InputPatch {
        InputPatch {
            volume_db: Some(db),
            muted: None,
        }
    }

    fn muted(on: bool) -> InputPatch {
        InputPatch {
            volume_db: None,
            muted: Some(on),
        }
    }

    fn status(answer: crate::api::Answer) -> u16 {
        match answer {
            Ok(_) => 200,
            Err(ApiError { status, .. }) => status,
        }
    }

    fn message(answer: crate::api::Answer) -> String {
        answer.expect_err("not an error").message
    }

    /// Connects a client called `name` on `conn` and has it play at 48 kHz.
    fn play_as(control: &mut Control, conn: ConnId, name: &str, now: Instant) {
        control.connected(conn, name.into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(conn, Some(hello), now);
        let buffer = min_buffer_frames(48000, 12.0);
        let answer = control.request(conn, Some(configure(48000, buffer)), now);
        assert_eq!(answer.reply, Some(Reply::Configured { rate: 0 }));
        let start = control.request(conn, Some(Request::Start), now);
        assert_eq!(start.reply, Some(Reply::Ok));
    }

    /// A started control thread whose master is open, with nothing queued.
    fn open(config: &Config, opener: Option<&Opener>) -> (Control, FakeEngine, Instant) {
        let (mut control, mut engine, now) = started(config, opener);
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        (control, engine, now)
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn the_master_volume_through_the_api_tells_the_mixer_subscribers() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        let mut published = subscribe(&mut control);
        control.connected(1, "mixer".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(1, Some(hello), now);
        control.request(1, Some(Request::SubscribeVolume), now);
        engine.gains.clear();
        let view = control.call(Call::PatchOutput(1, volume(30)), now).unwrap();
        assert_eq!(view["volume"], json!(30));
        engine.take(true);
        assert_eq!(engine.gains.len(), 1, "{:?}", engine.gains);
        assert_eq!(engine.gains[0].0, 0);
        assert!(close(engine.gains[0].1, db_to_gain(-42.0)));
        let notice = Reply::VolumeChanged { percent: 30 };
        assert_eq!(control.take_notices(), [(1, notice)]);
        let seen = events(&mut published);
        let told = seen.iter().any(|e| {
            matches!(
                e,
                ApiEvent::Output {
                    id: 1,
                    volume: Some(30),
                    ..
                }
            )
        });
        assert!(told, "{seen:?}");
        // A mixer's change is seen through the API too.
        control.request(1, Some(Request::SetVolume { percent: 70 }), now);
        let seen = events(&mut published);
        assert!(
            seen.iter().any(|e| matches!(
                e,
                ApiEvent::Output {
                    id: 1,
                    volume: Some(70),
                    ..
                }
            )),
            "{seen:?}"
        );
        let view = control.call(Call::Output(1), now).unwrap();
        assert_eq!(view["volume"], json!(70));
    }

    #[test]
    fn a_master_volume_above_100_is_422() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        let answer = control.call(Call::PatchOutput(1, volume(101)), now);
        assert_eq!(message(answer.clone()), "volume must be from 0 to 100");
        assert_eq!(status(answer), 422);
        assert!(engine.take(true).is_empty());
        assert_eq!(control.volume.percent(), 100);
    }

    #[test]
    fn volume_db_on_the_master_is_422() {
        let (mut control, _engine, now) = open(&Config::default(), None);
        let answer = control.call(Call::PatchOutput(1, offset(-6.0)), now);
        assert_eq!(status(answer), 422);
    }

    #[test]
    fn volume_on_a_further_output_is_422() {
        let (mut control, mut engine, _logs, now) =
            started_with(&Config::default(), vec![further(&[])]);
        engine.take(true);
        let answer = control.call(Call::PatchOutput(2, volume(50)), now);
        assert_eq!(status(answer), 422);
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn a_further_offset_sets_its_gain() {
        let (mut control, mut engine, _logs, now) =
            started_with(&Config::default(), vec![further(&[])]);
        engine.take(true);
        engine.gains.clear();
        let view = control
            .call(Call::PatchOutput(2, offset(-6.0)), now)
            .unwrap();
        assert_eq!(view["volume_db"], json!(-6.0));
        engine.take(true);
        let expected = control.volume.further_gain(-6.0);
        assert_eq!(engine.gains, [(1, expected)]);
        // Out of range is refused, not clamped.
        let answer = control.call(Call::PatchOutput(2, offset(-61.0)), now);
        assert_eq!(message(answer), "volume_db must be from -60 to 0");
        let answer = control.call(Call::PatchOutput(2, offset(f32::NAN)), now);
        assert_eq!(status(answer), 422);
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn zeros_with_a_delay_is_422() {
        let (mut control, _engine, now) = open(&Config::default(), None);
        let patch = OutputPatch {
            idle: Some("zeros".into()),
            idle_delay_s: Some(5),
            ..OutputPatch::default()
        };
        let answer = control.call(Call::PatchOutput(1, patch), now);
        assert_eq!(message(answer), "idle_delay_s needs idle = \"off\"");
        // A delay alone on an output at zeros is the same.
        let zeros = OutputPatch {
            idle: Some("zeros".into()),
            ..OutputPatch::default()
        };
        let view = control.call(Call::PatchOutput(1, zeros), now).unwrap();
        assert_eq!(view["idle"], json!("zeros"));
        assert!(view.get("idle_delay_s").is_none());
        let delay = OutputPatch {
            idle_delay_s: Some(5),
            ..OutputPatch::default()
        };
        assert_eq!(status(control.call(Call::PatchOutput(1, delay), now)), 422);
        let sleep = OutputPatch {
            idle: Some("sleep".into()),
            ..OutputPatch::default()
        };
        let answer = control.call(Call::PatchOutput(1, sleep), now);
        assert_eq!(message(answer), "idle must be \"zeros\" or \"off\"");
        assert_eq!(control.idle, Idle::Zeros);
    }

    #[test]
    fn output_5_is_404() {
        let (mut control, _engine, _logs, now) =
            started_with(&Config::default(), vec![further(&[])]);
        for id in [0, 3, 5] {
            assert_eq!(status(control.call(Call::Output(id), now)), 404, "{id}");
            let patch = Call::PatchOutput(id, offset(-6.0));
            assert_eq!(status(control.call(patch, now)), 404, "{id}");
        }
        assert_eq!(status(control.call(Call::Output(2), now)), 200);
    }

    #[test]
    fn a_patch_with_no_fields_changes_nothing() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        let mut published = subscribe(&mut control);
        let before = control.call(Call::Output(1), now).unwrap();
        let after = control.call(Call::PatchOutput(1, OutputPatch::default()), now);
        assert_eq!(after.unwrap(), before);
        assert_eq!(
            status(control.call(Call::PatchClient("mpd".into(), InputPatch::default()), now)),
            200
        );
        assert!(engine.take(true).is_empty());
        assert!(events(&mut published).is_empty());
        assert!(control.state.clients.is_empty());
    }

    #[test]
    fn a_client_volume_applies_to_every_stream_of_the_name() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        play_as(&mut control, 1, "mpd", now);
        play_as(&mut control, 2, "mpd", now);
        play_as(&mut control, 3, "other", now);
        engine.take(true);
        let view = control.call(Call::PatchClient("mpd".into(), input_db(-6.0)), now);
        assert_eq!(view.unwrap()["volume_db"], json!(-6.0));
        let gain = db_to_gain(-6.0);
        let seen = engine.take(true);
        assert!(has(&seen, &format!("SetSourceGain 1 {gain}")), "{seen:?}");
        assert!(has(&seen, &format!("SetSourceGain 2 {gain}")), "{seen:?}");
        assert!(!has(&seen, "SetSourceGain 3"), "{seen:?}");
        // A third stream of the name, configured after, comes with it.
        play_as(&mut control, 4, "mpd", now);
        engine.take(true);
        assert_eq!(engine.added.last(), Some(&(4, gain)));
    }

    #[test]
    fn a_stored_client_volume_is_in_the_source_at_add() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        let answer = control.call(Call::PatchClient("mpd".into(), input_db(-6.0)), now);
        assert!(answer.is_ok());
        assert!(engine.take(true).is_empty());
        play_as(&mut control, 1, "mpd", now);
        let seen = engine.take(true);
        assert_eq!(engine.added, [(1, db_to_gain(-6.0))]);
        assert!(!has(&seen, "SetSourceGain"), "{seen:?}");
        // A name without a stored volume plays at 0 dB.
        play_as(&mut control, 2, "other", now);
        engine.take(true);
        assert_eq!(engine.added.last(), Some(&(2, 1.0)));
    }

    #[test]
    fn a_muted_client_has_gain_0() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        play_as(&mut control, 1, "mpd", now);
        engine.take(true);
        let name = || "mpd".to_string();
        control
            .call(Call::PatchClient(name(), input_db(-6.0)), now)
            .unwrap();
        engine.take(true);
        let view = control.call(Call::PatchClient(name(), muted(true)), now);
        let view = view.unwrap();
        assert_eq!(view["muted"], json!(true));
        assert_eq!(view["volume_db"], json!(-6.0));
        assert!(has(&engine.take(true), "SetSourceGain 1 0"));
        play_as(&mut control, 2, "mpd", now);
        engine.take(true);
        assert_eq!(engine.added.last(), Some(&(2, 0.0)));
        // Unmuted, it plays at its volume again.
        control
            .call(Call::PatchClient(name(), muted(false)), now)
            .unwrap();
        let gain = db_to_gain(-6.0);
        assert!(has(&engine.take(true), &format!("SetSourceGain 1 {gain}")));
    }

    #[test]
    fn a_name_not_connected_can_be_given_a_volume() {
        let (mut control, _engine, now) = open(&Config::default(), None);
        let name = "squeezelite".to_string();
        assert_eq!(status(control.call(Call::Client(name.clone()), now)), 404);
        let view = control.call(Call::PatchClient(name.clone(), input_db(-10.0)), now);
        assert_eq!(view.unwrap()["id"], json!("clients/squeezelite"));
        let view = control.call(Call::Client(name), now).unwrap();
        assert_eq!(view["volume_db"], json!(-10.0));
        assert_eq!(view["muted"], json!(false));
        // A name connected but never given a volume is at 0 dB.
        play_as(&mut control, 1, "mpd", now);
        let view = control.call(Call::Client("mpd".into()), now).unwrap();
        assert_eq!(view["volume_db"], json!(0.0));
        let list = control.call(Call::Inputs, now).unwrap();
        let ids: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["clients/mpd", "clients/squeezelite"]);
        assert_eq!(status(control.call(Call::Client("never".into()), now)), 404);
    }

    #[test]
    fn the_65th_name_is_422() {
        let (mut control, _engine, now) = open(&Config::default(), None);
        for n in 0..64 {
            let call = Call::PatchClient(format!("player{n}"), input_db(-1.0));
            assert_eq!(status(control.call(call, now)), 200, "{n}");
        }
        let call = Call::PatchClient("one-more".into(), input_db(-1.0));
        let answer = control.call(call, now);
        assert_eq!(status(answer.clone()), 422);
        assert_eq!(message(answer), "too many clients with a volume (64)");
        // A name already stored can still change.
        let call = Call::PatchClient("player7".into(), input_db(-2.0));
        assert_eq!(status(control.call(call, now)), 200);
        assert_eq!(control.state.clients.len(), 64);
    }

    #[test]
    fn the_analog_volume_sets_its_gain() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = open(&analog_config(), Some(&opener));
        let view = control.call(Call::Analog, now).unwrap();
        assert_eq!(
            view,
            json!({"id": "analog", "volume_db": -20.0, "muted": false})
        );
        let view = control
            .call(Call::PatchAnalog(input_db(-10.0)), now)
            .unwrap();
        assert_eq!(view["volume_db"], json!(-10.0));
        let gain = db_to_gain(-10.0);
        assert_eq!(engine.take(true), [format!("SetAnalogGain {gain}")]);
        control.call(Call::PatchAnalog(muted(true)), now).unwrap();
        assert_eq!(engine.take(true), ["SetAnalogGain 0"]);
        // Without an analog input there is none to set.
        let (mut control, _engine, now) = open(&Config::default(), None);
        assert_eq!(status(control.call(Call::Analog, now)), 404);
        let answer = control.call(Call::PatchAnalog(input_db(-10.0)), now);
        assert_eq!(status(answer), 404);
    }

    #[test]
    fn input_volume_db_minus_61_is_422() {
        let opener = Opener::default();
        let (mut control, mut engine, now) = open(&analog_config(), Some(&opener));
        for db in [-61.0, 1.0, f32::NAN, f32::INFINITY] {
            let answer = control.call(Call::PatchAnalog(input_db(db)), now);
            assert_eq!(message(answer), "volume_db must be from -60 to 0", "{db}");
            let call = Call::PatchClient("mpd".into(), input_db(db));
            assert_eq!(status(control.call(call, now)), 422, "{db}");
        }
        assert!(engine.take(true).is_empty());
        assert!(control.state.clients.is_empty());
        assert!(control.save_at.is_none());
    }

    #[test]
    fn every_change_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let opener = Opener::default();
        let (mut control, _engine, _logs) = unstarted_with(&analog_config(), vec![further(&[])]);
        control.set_analog(opener.open());
        let output_volume = OutputVolume::new(None, config::BitPerfect::Auto);
        let cards = ["sndrpihifiberry".to_string(), "Loopback".to_string()];
        control.restore(output_volume, path.clone(), &cards);
        let now = Instant::now();
        control.start(now);
        let idle = OutputPatch {
            idle: Some("off".into()),
            idle_delay_s: Some(30),
            ..OutputPatch::default()
        };
        let calls = [
            Call::PatchOutput(1, volume(40)),
            Call::PatchOutput(1, idle.clone()),
            Call::PatchOutput(2, offset(-6.0)),
            Call::PatchOutput(2, idle),
            Call::PatchAnalog(input_db(-10.0)),
            Call::PatchClient("mpd".into(), muted(true)),
        ];
        for call in calls {
            control.save_at = None;
            let what = format!("{call:?}");
            assert!(control.call(call, now).is_ok(), "{what}");
            assert!(control.save_at.is_some(), "{what}");
        }
        // A refused change saves nothing.
        control.save_at = None;
        assert!(
            control
                .call(Call::PatchOutput(1, volume(101)), now)
                .is_err()
        );
        assert!(control.save_at.is_none());
        control.save_at = Some(now);
        control.shutdown();
        let state = State::load(&path);
        assert_eq!(state.output_percent, 40);
        let master = &state.outputs["sndrpihifiberry"];
        let stored = |value: &str, config: &str| Stored {
            value: value.to_string(),
            config: config.to_string(),
        };
        assert_eq!(master.idle, Some(stored("off", "off")));
        assert_eq!(
            master.idle_delay_s,
            Some(Stored {
                value: 30,
                config: 60
            })
        );
        assert_eq!(master.volume_db, None);
        let output = &state.outputs["Loopback"];
        assert_eq!(
            output.volume_db,
            Some(Stored {
                value: -6.0,
                config: 0.0
            })
        );
        assert_eq!(output.idle, Some(stored("off", "zeros")));
        assert_eq!(
            output.idle_delay_s,
            Some(Stored {
                value: 30,
                config: 60
            })
        );
        let analog = state.analog.unwrap();
        assert_eq!(
            analog.value,
            StoredInput {
                volume_db: -10.0,
                muted: false
            }
        );
        assert_eq!(
            analog.config,
            StoredInput {
                volume_db: -20.0,
                muted: false
            }
        );
        assert_eq!(
            state.clients["mpd"],
            StoredInput {
                volume_db: 0.0,
                muted: true
            }
        );
    }

    /// A state file with the master at 25 %, output cards "A" and "B" each
    /// with an offset of -12 dB, A's stored against a config at 0 dB and
    /// B's against one at -3 dB, and an entry for a card no longer there.
    fn stored_state(path: &std::path::Path) {
        let mut state = State {
            output_percent: 25,
            ..State::default()
        };
        let offset = |config| StoredOutput {
            volume_db: Some(Stored {
                value: -12.0,
                config,
            }),
            ..StoredOutput::default()
        };
        let mut a = offset(0.0);
        a.idle = Some(Stored {
            value: "zeros".into(),
            config: "off".into(),
        });
        state.outputs.insert("A".into(), a);
        state.outputs.insert("B".into(), offset(-3.0));
        state.outputs.insert("Gone".into(), offset(-3.0));
        state.analog = Some(Stored {
            value: StoredInput {
                volume_db: -30.0,
                muted: true,
            },
            config: StoredInput {
                volume_db: -20.0,
                muted: false,
            },
        });
        state.save(path).unwrap();
    }

    /// Starts a control thread with further outputs on `cards`, each with a
    /// config offset of -3 dB, restored from the file at `path`. Gives the
    /// gain each output's mix started at, by engine index.
    fn restored(path: &std::path::Path, cards: &[&str]) -> (Control, Vec<(usize, f32)>) {
        let outputs = cards
            .iter()
            .map(|card| {
                let (mut output, rates) = further(&[]);
                output.card = card.to_string();
                output.volume_db = -3.0;
                output.idle = Idle::Off { delay_s: 60 };
                (output, rates)
            })
            .collect();
        let opener = Opener::default();
        let (mut control, mut engine, _logs) = unstarted_with(&analog_config(), outputs);
        control.set_analog(opener.open());
        let volume = OutputVolume::new(None, config::BitPerfect::Auto);
        let mut names = vec!["master".to_string()];
        names.extend(cards.iter().map(|c| c.to_string()));
        control.restore(volume, path.to_path_buf(), &names);
        control.start(Instant::now());
        engine.take(false);
        (control, engine.outputs)
    }

    #[test]
    fn restore_applies_the_rule_of_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        stored_state(&path);
        let now = Instant::now();
        for order in [["A", "B"], ["B", "A"]] {
            let (mut control, started) = restored(&path, &order);
            // A's config moved from 0 to -3 dB since: the config wins. B's
            // still says -3 dB: the stored value applies.
            let want = |card: &str| if card == "A" { -3.0 } else { -12.0 };
            for (i, card) in order.iter().enumerate() {
                let (index, gain) = started[i];
                assert_eq!(index, i + 1);
                let expected = control.volume.further_gain(want(card));
                assert!(close(gain, expected), "{card}: {gain}");
                let view = control.call(Call::Output(i + 2), now).unwrap();
                assert_eq!(view["card"], json!(card));
                assert_eq!(view["volume_db"], json!(want(card)), "{card}");
            }
            // A's idle was stored against "off", which the config still says.
            let a = order.iter().position(|c| *c == "A").unwrap() + 2;
            let view = control.call(Call::Output(a), now).unwrap();
            assert_eq!(view["idle"], json!("zeros"));
            // The master's volume has no config key: always restored.
            assert_eq!(control.volume.percent(), 25);
            // The analog input's was stored against the config's -20 dB.
            let view = control.call(Call::Analog, now).unwrap();
            assert_eq!(view["volume_db"], json!(-30.0));
            assert_eq!(view["muted"], json!(true));
            // A save keeps the card no longer in the config.
            control.call(Call::PatchOutput(1, volume(30)), now).unwrap();
            control.shutdown();
            assert!(State::load(&path).outputs.contains_key("Gone"));
            stored_state(&path);
        }
    }

    #[test]
    fn client_events_are_published() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        let mut published = subscribe(&mut control);
        play_as(&mut control, 1, "mpd", now);
        control.request(1, Some(Request::Stop), now);
        control.disconnected(1, now);
        engine.take(true);
        let changes: Vec<String> = events(&mut published)
            .into_iter()
            .filter_map(|e| match e {
                ApiEvent::Client { name, change } => {
                    assert_eq!(name, "mpd");
                    Some(change)
                }
                _ => None,
            })
            .collect();
        assert_eq!(changes, ["connected", "running", "stopped", "gone"]);
    }

    #[test]
    fn output_state_and_rate_changes_are_published() {
        let config = Config {
            idle: Idle::Off { delay_s: 1 },
            ..Config::default()
        };
        let (mut control, mut engine, now) = started(&config, None);
        let mut published = subscribe(&mut control);
        engine.take(true);
        control.handle_events(now);
        play_as(&mut control, 1, "mpd", now);
        control.request(1, Some(Request::Stop), now);
        engine.take(true);
        control.tick(now + Duration::from_secs(2));
        let seen = events(&mut published);
        let off = seen
            .iter()
            .any(|e| matches!(e, ApiEvent::Output { id: 1, state, .. } if state == "off"));
        assert!(off, "{seen:?}");
        // A player at another rate opens it there.
        control.connected(2, "hq".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(2, Some(hello), now);
        let buffer = min_buffer_frames(96000, 12.0);
        control.request(2, Some(configure(96000, buffer)), now);
        control.request(2, Some(Request::Start), now);
        let seen = events(&mut published);
        assert!(seen.contains(&ApiEvent::Rate { rate: 96000 }), "{seen:?}");
        let open = seen
            .iter()
            .any(|e| matches!(e, ApiEvent::Output { id: 1, state, .. } if state == "open"));
        assert!(open, "{seen:?}");
    }

    #[test]
    fn an_idle_change_applies_at_the_next_idle_decision() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        let patch = OutputPatch {
            idle_delay_s: Some(2),
            ..OutputPatch::default()
        };
        control.call(Call::PatchOutput(1, patch), now).unwrap();
        assert!(engine.take(true).is_empty());
        control.tick(now + Duration::from_secs(2));
        assert!(has(&engine.take(true), "CloseOutput"));
    }

    #[test]
    fn a_further_output_off_stays_off_after_a_change_to_zeros_until_a_client_plays() {
        let config = Config {
            idle: Idle::Zeros,
            ..Config::default()
        };
        let (mut output, rates) = further(&[]);
        output.idle = Idle::Off { delay_s: 1 };
        let (mut control, mut engine, logs, now) = started_with(&config, vec![(output, rates)]);
        engine.take(true);
        control.handle_events(now);
        logs[0].take();
        control.tick(now + Duration::from_secs(1));
        assert_eq!(logs[0].take(), ["open false"]);
        let zeros = OutputPatch {
            idle: Some("zeros".into()),
            ..OutputPatch::default()
        };
        let view = control.call(Call::PatchOutput(2, zeros), now + Duration::from_secs(2));
        assert_eq!(view.unwrap()["state"], json!("off"));
        control.tick(now + Duration::from_secs(5));
        assert!(logs[0].take().is_empty());
        play_as(&mut control, 1, "mpd", now + Duration::from_secs(6));
        assert_eq!(logs[0].take(), ["open true"]);
        // Quiet again, it stays open, as zeros says.
        control.request(1, Some(Request::Stop), now + Duration::from_secs(7));
        control.tick(now + Duration::from_secs(60));
        assert!(logs[0].take().is_empty());
    }

    #[test]
    fn the_state_tells_the_rate_outputs_clients_and_bit_perfect() {
        let config = Config {
            idle: Idle::Off { delay_s: 1 },
            ..Config::default()
        };
        let (mut control, mut engine, _logs, now) = started_with(&config, vec![further(&[])]);
        let counters = Arc::new(Counters::default());
        control.set_counters(counters.clone());
        engine.take(true);
        control.handle_events(now);
        play_as(&mut control, 1, "mpd", now);
        engine.take(true);
        let code = NotBitPerfect::Volume.code();
        counters.bit_perfect.store(code, Ordering::Relaxed);
        let state = control.call(Call::State, now).unwrap();
        assert_eq!(state["rate"], json!(48000));
        assert_eq!(state["format"], json!("f32"));
        assert_eq!(state["outputs"][0]["id"], json!(1));
        assert_eq!(state["outputs"][0]["state"], json!("open"));
        assert_eq!(state["outputs"][1]["card"], json!("Loopback"));
        assert_eq!(state["outputs"][1]["state"], json!("open"));
        let client = json!({"name": "mpd", "rate": 48000, "running": true});
        assert_eq!(state["clients"], json!([client]));
        assert_eq!(state["bit_perfect"], json!(false));
        assert_eq!(state["reason"], json!("volume"));
        counters.bit_perfect.store(0, Ordering::Relaxed);
        let state = control.call(Call::State, now).unwrap();
        assert_eq!(state["bit_perfect"], json!(true));
        assert_eq!(state["reason"], json!(null));
        // A DoP stream reaches the card unaltered, whatever the engine last
        // decided for PCM.
        counters.bit_perfect.store(code, Ordering::Relaxed);
        engine.send(Event::DopStarted(1, 1));
        control.handle_events(now);
        let state = control.call(Call::State, now).unwrap();
        assert_eq!(state["bit_perfect"], json!(true));
        assert_eq!(state["reason"], json!(null));
        engine.send(Event::DopStopped(1));
        control.handle_events(now);
        // Off, the master plays nothing.
        control.request(1, Some(Request::Stop), now);
        counters.bit_perfect.store(0, Ordering::Relaxed);
        control.tick(now + Duration::from_secs(5));
        engine.take(true);
        let state = control.call(Call::State, now).unwrap();
        assert_eq!(state["outputs"][0]["state"], json!("parked"));
        assert_eq!(state["bit_perfect"], json!(false));
        assert_eq!(state["reason"], json!("no source"));
    }
}
