//! The control thread's side of the API: each call is handled here, between
//! client requests, exactly like one. Views are built from what `Control`
//! holds; nothing waits on the audio thread.

use super::{ConnId, Control, Stream, check_alignment, plugins};
use aas_client::proto::Reply;
use crate::api::{
    self, ApiCall, ApiError, ApiEvent, Call, ClientView, InputPatch, InputView, MatrixPut,
    OutputPatch, OutputRoutes, OutputState, OutputView, RouteJson, StateView,
};
use crate::api::{
    AnalogMetrics, LastGap, MasterMetrics, MetricsView, OutputMetrics, SettingsPatch, SettingsView,
    StreamMetrics,
};
use crate::bitperfect::NotBitPerfect;
use crate::config::{self, BitPerfect, Config, DEFAULT_IDLE_DELAY_S, Format, Idle, Quality, Route};
use crate::counters::{Counters, OutputCounters};
use crate::engine::Command;
use crate::format::db_to_gain;
use crate::latency::{PERIODS, period_frames};
use crate::matrix::{Bus, INPUT_NAMES, Matrix};
use crate::plugin::build::widths;
use crate::rate::{MAX_RATE, MIN_RATE};
use crate::state::{MAX_CLIENTS, State, Stored, StoredInput, StoredOutput, StoredRoute};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
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

/// The analog input to start with. Its volume is the stored one while
/// the config's `volume_db` still says what it said then, else the
/// config's. Its mute has no config key: the stored one always applies.
/// The file keeps the input whole, with the config's side as it was, so
/// files written before this was so load as they are.
pub(super) fn resolve_input(
    stored: Option<&Stored<StoredInput>>,
    config: &StoredInput,
) -> StoredInput {
    let Some(stored) = stored else {
        return config.clone();
    };
    let volume_db = Stored {
        value: stored.value.volume_db,
        config: stored.config.volume_db,
    };
    StoredInput {
        volume_db: Stored::resolve(Some(&volume_db), &config.volume_db),
        muted: stored.value.muted,
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

/// The delay `idle = "off"` takes when a patch names none and the rule in
/// place is zeros: the output's own from the config, `config`, as a config
/// that says `idle = "off"` alone would give it.
fn fallback_delay(config: Idle) -> u32 {
    idle_parts(config).1
}

/// The idle rule a patch makes of `current`, with the config's rules, or
/// None when it names neither key. `config` is the output's rule in the
/// config.
fn patched_idle(
    current: Idle,
    config: Idle,
    patch: &OutputPatch,
) -> Result<Option<Idle>, ApiError> {
    if patch.idle.is_none() && patch.idle_delay_s.is_none() {
        return Ok(None);
    }
    let (current_kind, current_delay) = match current {
        Idle::Zeros => ("zeros", fallback_delay(config)),
        Idle::Off { delay_s } => ("off", delay_s),
    };
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

/// The longest process name, in bytes, as the kernel keeps it.
const MAX_NAME: usize = 15;

/// A client without a stored volume plays at 0 dB.
const UNITY: StoredInput = StoredInput {
    volume_db: 0.0,
    muted: false,
};

/// The shortest time between two matrices sent to one output. A change
/// fades over 10 ms, and a DoP stream that starts meanwhile waits for the
/// fade to end: so it waits for at most one.
const MATRIX_GAP: Duration = Duration::from_millis(20);

/// An output's matrix as the control thread keeps it.
pub(super) struct OutputMatrix {
    /// The width its buffers and chains on the audio thread are built for,
    /// fixed at the start. A matrix of another width is refused.
    pub(super) width: usize,
    /// The routes as last set; empty is the default matrix.
    routes: Vec<Route>,
    /// The routes of the config, which a stored matrix is checked against.
    config: Vec<StoredRoute>,
    /// A matrix that waits for its turn, and when it is due. A later one
    /// replaces it.
    due: Option<(Instant, Box<Matrix>)>,
    /// When the last one was sent.
    sent_at: Option<Instant>,
    /// A refused matrix has been logged.
    refused: bool,
}

impl OutputMatrix {
    /// `config` is the config's routes, None for the default matrix.
    pub(super) fn new(width: usize, config: Option<&[Route]>) -> OutputMatrix {
        let routes = config.unwrap_or_default().to_vec();
        OutputMatrix {
            width,
            config: stored_routes(&routes),
            routes,
            due: None,
            sent_at: None,
            refused: false,
        }
    }
}

/// The routes of the default matrix: left to channel 1 and right to
/// channel 2 for every bus, as far as `width` goes.
fn default_routes(width: usize) -> Vec<Route> {
    Bus::ALL
        .into_iter()
        .flat_map(|bus| bus_default_routes(bus, width))
        .collect()
}

/// The routes of `bus` in the default matrix, `width` channels wide.
fn bus_default_routes(bus: Bus, width: usize) -> impl Iterator<Item = Route> {
    let sides = bus.inputs().into_iter().enumerate();
    sides
        .filter(move |&(side, _)| side < width)
        .map(|(side, from)| Route {
            from,
            to: side + 1,
            gain_db: 0.0,
        })
}

/// The matrix of `routes`, `width` channels wide, with the config's rules
/// and messages. No route is the default matrix, as `matrix = []` is.
fn matrix_of(routes: &[Route], width: usize) -> Result<Matrix, String> {
    if routes.is_empty() {
        Ok(Matrix::default_for(width))
    } else {
        Matrix::from_routes(routes, width)
    }
}

/// Checks routes given by their parts, each as a `[[matrix]]` table, and
/// makes their matrix.
fn routes_and_matrix(
    parts: impl Iterator<Item = (String, i64, f64)>,
    width: usize,
) -> Result<(Vec<Route>, Matrix), String> {
    let routes = parts
        .enumerate()
        .map(|(i, (from, to, gain_db))| config::route_from_parts(i + 1, &from, to, gain_db))
        .collect::<Result<Vec<Route>, String>>()?;
    let matrix = matrix_of(&routes, width)?;
    Ok((routes, matrix))
}

fn stored_routes(routes: &[Route]) -> Vec<StoredRoute> {
    let stored = |r: &Route| StoredRoute {
        from: INPUT_NAMES[r.from].to_string(),
        to: r.to,
        gain_db: r.gain_db,
    };
    routes.iter().map(stored).collect()
}

/// A route as the API writes it. The gain is kept as an `f32`; it is
/// widened through its shortest text, so that -0.1 dB reads as -0.1 and
/// not as the `f64` nearest the `f32`.
fn route_json(route: &Route) -> RouteJson {
    RouteJson {
        from: INPUT_NAMES[route.from].to_string(),
        to: route.to as i64,
        gain_db: Some(route.gain_db.to_string().parse().unwrap_or(0.0)),
    }
}

/// A bit-perfect mode as the config writes it.
fn mode_name(mode: BitPerfect) -> &'static str {
    match mode {
        BitPerfect::Auto => "auto",
        BitPerfect::Forced => "forced",
        BitPerfect::Off => "off",
    }
}

fn quality_name(quality: Quality) -> &'static str {
    match quality {
        Quality::High => "high",
        Quality::Medium => "medium",
    }
}

pub(super) fn format_name(format: Format) -> &'static str {
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

    /// The further outputs' counters, in the order of the config, for
    /// `GET /metrics`.
    pub fn set_output_counters(&mut self, counters: Vec<Arc<OutputCounters>>) {
        self.output_counters = counters;
    }

    /// Handles one call of the API and gives its answer.
    pub fn call(&mut self, call: Call, now: Instant) -> crate::api::Answer {
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
            Call::Matrix => api::answer(&self.matrix_view()),
            Call::PutMatrix(put) => self.put_matrix(put, now),
            Call::Metrics => api::answer(&self.metrics_view()),
            Call::Graph => Ok(self.graph()),
            Call::Plugins => api::answer(&self.plugin_views()),
            Call::Plugin(id) => self.plugin_answer(&id),
            Call::PatchPlugin(id, patch) => self.patch_plugin(id, patch, now),
            Call::Settings => api::answer(&self.settings_view()),
            Call::Airplay => api::answer(&self.airplay_views()),
            Call::Metadata => api::answer(&self.metadata_view(now)),
            Call::PairAirplay { .. } => Err(ApiError::internal("not yet")),
            Call::PatchAirplay(name, patch) => self.patch_airplay(&name, patch, now),
            Call::PatchSettings(patch) => self.patch_settings(patch, now),
        };
        self.publish_changes();
        answer
    }

    /// Handles a call from the API thread and sends its answer back. One
    /// whose caller has given up, answered 503 after 2 s, is dropped
    /// unapplied. A caller can still give up between this check and the
    /// answer; it then got a 503 for a call that applied, which is why a
    /// 503 on a change says to repeat it rather than that nothing changed.
    pub fn answer(&mut self, call: ApiCall, now: Instant) {
        if call.answer.is_closed() {
            return;
        }
        // Pairing is answered by the receiver's thread, when it is done.
        if let Call::PairAirplay { name, pin } = call.call {
            return self.pair_airplay(name, pin, call.answer);
        }
        // A caller that has given up meanwhile has dropped its one-shot.
        let _ = call.answer.send(self.call(call.call, now));
    }

    /// Hands a pairing step to the receiver named `name`, or answers why
    /// not.
    fn pair_airplay(
        &mut self,
        name: String,
        pin: Option<String>,
        answer: tokio::sync::oneshot::Sender<api::Answer>,
    ) {
        let Some(receiver) = self.airplay.iter_mut().find(|r| r.name() == name) else {
            let _ = answer.send(Err(ApiError::not_found(format!(
                "no AirPlay receiver \"{name}\""
            ))));
            return;
        };
        if receiver.protocol() == crate::airplay::config::Protocol::AirPlay1 {
            let _ = answer.send(Err(ApiError::unprocessable(format!(
                "\"{name}\" is an AirPlay 1 receiver, which needs no pairing"
            ))));
            return;
        }
        if let Some(pin) = &pin
            && (pin.len() != 4 || !pin.bytes().all(|b| b.is_ascii_digit()))
        {
            let _ = answer.send(Err(ApiError::bad_request("pin is 4 digits")));
            return;
        }
        receiver.pair(pin, answer);
    }

    /// When the earliest matrix that waits for its turn is due, so that
    /// the poll can wake for it before its tick.
    pub fn next_due(&self) -> Option<Instant> {
        let due = self.matrices.iter().filter_map(|m| m.due.as_ref());
        due.map(|&(at, _)| at).min()
    }

    /// The width of the master's matrix, as its chains make it, where the
    /// config's channels are taken until then. Call before `restore`.
    pub fn set_matrix_width(&mut self, width: usize) {
        self.matrices[0].width = width;
    }

    pub(super) fn metadata_view(&self, now: Instant) -> api::MetadataView {
        let s = self.now_playing.snapshot(now);
        api::MetadataView {
            title: s.track.title,
            artist: s.track.artist,
            album: s.track.album,
            duration_s: s.track.duration_s,
            position_s: s.position_s,
            playing: s.track.playing,
            cover: s.cover.map(|c| api::CoverView {
                mime: c.mime,
                bytes: c.bytes.len(),
            }),
        }
    }

    /// `PATCH /airplay/{name}`: turns a receiver on or off, and keeps it.
    fn patch_airplay(&mut self, name: &str, patch: api::AirplayPatch, now: Instant) -> api::Answer {
        let Some(i) = self.airplay.iter().position(|r| r.name() == name) else {
            return Err(ApiError::not_found(format!("no AirPlay receiver \"{name}\"")));
        };
        if let Some(on) = patch.enabled
            && on != self.airplay[i].enabled()
        {
            let mut batch = Vec::new();
            if let Some(tap) = self.airplay[i].set_enabled(on) {
                batch.push(Command::RemoveTap(tap));
                self.taps_len -= 1;
            }
            if on && let Some((rate, _)) = self.output {
                self.airplay_taps(rate, &mut batch);
            }
            if !batch.is_empty() {
                self.send(batch);
            }
            let config = self.airplay[i].config_enabled();
            self.state.airplay.entry(name.to_string()).or_default().enabled =
                Some(Stored { value: on, config });
            self.save_later(now);
            self.publish_airplay();
        }
        api::answer(&self.airplay[i].view())
    }

    pub(super) fn airplay_views(&self) -> Vec<api::AirplayView> {
        self.airplay.iter().map(crate::airplay::Receiver::view).collect()
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

    /// The client on `conn` has changed, as `client_changed` says.
    pub(super) fn conn_changed(&self, conn: ConnId, change: &str) {
        if let Some(c) = self.conns.get(&conn) {
            self.client_changed(&c.name, change);
        }
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
    pub(super) fn cycle_rate(&self) -> Option<u32> {
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
        let (card, volume, volume_db, limit_db, idle) = if index == 0 {
            (
                self.card.clone(),
                Some(self.volume.percent()),
                None,
                Some(self.volume.limit_db()),
                self.idle,
            )
        } else {
            let output = self.further.get(index - 1)?;
            let volume_db = Some(output.volume_db);
            (output.card.clone(), None, volume_db, None, output.idle)
        };
        let (kind, delay) = idle_parts(idle);
        Some(OutputView {
            id: index + 1,
            card,
            volume,
            volume_db,
            limit_db,
            clock: (index == 0 && self.capture_clock).then_some("capture"),
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
            if patch.volume.is_some_and(|v| !(0..=100).contains(&v)) {
                return Err(ApiError::unprocessable("volume must be from 0 to 100"));
            }
            if patch
                .limit_db
                .is_some_and(|db| !(db.is_finite() && (-60.0..=0.0).contains(&db)))
            {
                return Err(ApiError::unprocessable("limit_db must be from -60 to 0"));
            }
        } else {
            if patch.limit_db.is_some() {
                return Err(ApiError::unprocessable(
                    "limit_db is only for the master; a further output has volume_db",
                ));
            }
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
        let (current, config) = match index {
            0 => (self.idle, self.config_idle),
            _ => (
                self.further[index - 1].idle,
                self.further[index - 1].config_idle,
            ),
        };
        let idle = patched_idle(current, config, &patch)?.filter(|&idle| idle != current);
        if let Some(percent) = patch.volume.and_then(|v| u8::try_from(v).ok()) {
            self.set_volume(percent, now);
        }
        let mut changed = false;
        if let Some(db) = patch.limit_db
            && db != self.volume.limit_db()
        {
            self.set_limit(db);
            changed = true;
        }
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

    /// Sets the master's volume limit, stored with what the config says,
    /// and gives the engine every output's gain under it.
    fn set_limit(&mut self, db: f32) {
        self.volume.set_limit(db);
        let mut batch = vec![Command::SetLimited(self.volume.limited())];
        batch.push(Command::SetGain(0, self.volume.gain()));
        for output in &self.further {
            batch.push(Command::SetGain(output.index, self.further_gain(output)));
        }
        self.send(batch);
        let config = self.config_limit_db;
        let entry = self.state.outputs.entry(self.card.clone()).or_default();
        entry.limit_db = Some(Stored { value: db, config });
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

    /// The card of the output at engine index `index`, which keys its
    /// state.
    pub(super) fn card_of(&self, index: usize) -> &str {
        match index {
            0 => &self.card,
            _ => &self.further[index - 1].card,
        }
    }

    /// The engine index of the output at `index`: its place in the config.
    fn engine_index(&self, index: usize) -> usize {
        match index {
            0 => 0,
            _ => self.further[index - 1].index,
        }
    }

    /// The routes of the output at `index` as last set, the default matrix
    /// written out route by route.
    pub(super) fn route_views(&self, index: usize) -> Vec<RouteJson> {
        let slot = &self.matrices[index];
        if slot.routes.is_empty() {
            return default_routes(slot.width).iter().map(route_json).collect();
        }
        let mut views: Vec<RouteJson> = slot.routes.iter().map(route_json).collect();
        // Routes that name no `aes67` input play it as the default matrix
        // does, as `Matrix::from_routes` says.
        let aes67 = Bus::Aes67.inputs();
        if !slot.routes.iter().any(|r| aes67.contains(&r.from)) {
            let width = slot.width;
            views.extend(bus_default_routes(Bus::Aes67, width).map(|r| route_json(&r)));
        }
        views
    }

    fn matrix_view(&self) -> MatrixPut {
        let outputs = (0..self.matrices.len())
            .map(|index| OutputRoutes {
                output: index + 1,
                routes: self.route_views(index),
            })
            .collect();
        MatrixPut { outputs }
    }

    /// Checks every output's routes first, so that a refused put changes
    /// no output's matrix. An output not named keeps its matrix.
    fn put_matrix(&mut self, put: MatrixPut, now: Instant) -> api::Answer {
        let mut checked: Vec<(usize, Vec<Route>, Matrix)> = Vec::new();
        for OutputRoutes { output: id, routes } in put.outputs {
            let index = self.output_index(id)?;
            if checked.iter().any(|&(i, ..)| i == index) {
                let message = format!("output {id} is named twice");
                return Err(ApiError::unprocessable(message));
            }
            let parts = routes
                .into_iter()
                .map(|r| (r.from, r.to, r.gain_db.unwrap_or(0.0)));
            // The config names a further output's matrix by its output.
            let (routes, matrix) =
                routes_and_matrix(parts, self.matrices[index].width).map_err(|e| match index {
                    0 => ApiError::unprocessable(e),
                    _ => ApiError::unprocessable(format!("output {id}: {e}")),
                })?;
            checked.push((index, routes, matrix));
        }
        for (index, routes, matrix) in checked {
            if routes != self.matrices[index].routes {
                self.set_matrix(index, routes, matrix, now);
            }
        }
        api::answer(&self.matrix_view())
    }

    /// Sets the matrix of the output at `index`, stores it with what the
    /// config says, and sends it when its turn comes.
    fn set_matrix(&mut self, index: usize, routes: Vec<Route>, matrix: Matrix, now: Instant) {
        let card = self.card_of(index).to_string();
        let slot = &mut self.matrices[index];
        let stored = Stored {
            value: stored_routes(&routes),
            config: slot.config.clone(),
        };
        slot.routes = routes;
        self.state.outputs.entry(card).or_default().matrix = Some(stored);
        self.publish(ApiEvent::Matrix {
            output: index + 1,
            routes: self.route_views(index),
        });
        self.save_later(now);
        let slot = &mut self.matrices[index];
        match slot.sent_at {
            Some(at) if now < at + MATRIX_GAP => {
                slot.due = Some((at + MATRIX_GAP, Box::new(matrix)))
            }
            _ => {
                slot.due = None;
                self.send_matrix(index, Box::new(matrix), now);
            }
        }
    }

    fn send_matrix(&mut self, index: usize, matrix: Box<Matrix>, now: Instant) {
        let slot = &mut self.matrices[index];
        slot.sent_at = Some(now);
        let engine = self.engine_index(index);
        self.send(vec![Command::SetMatrix(engine, matrix)]);
    }

    /// Sends each matrix whose turn has come.
    pub(super) fn send_due_matrices(&mut self, now: Instant) {
        for index in 0..self.matrices.len() {
            let slot = &mut self.matrices[index];
            if slot.due.as_ref().is_some_and(|&(at, _)| now >= at)
                && let Some((_, matrix)) = slot.due.take()
            {
                self.send_matrix(index, matrix, now);
            }
        }
    }

    /// A matrix the audio thread sent back for output `output`, which is
    /// dropped here. It is the one a `SetMatrix` replaced, or, if that was
    /// `refused`, the one it brought. Every put is checked against the
    /// width, so a refusal means the widths kept here are wrong: logged
    /// once.
    pub(super) fn old_matrix(&mut self, output: usize, matrix: &Matrix, refused: bool) {
        let index = match output {
            0 => Some(0),
            _ => self
                .further
                .iter()
                .position(|o| o.index == output)
                .map(|i| i + 1),
        };
        let Some(slot) = index.and_then(|i| self.matrices.get_mut(i)) else {
            return;
        };
        if refused && !slot.refused {
            slot.refused = true;
            eprintln!(
                "aas: output {}: a matrix of {} channels was refused, the one in place stays",
                output + 1,
                matrix.width()
            );
        }
    }

    /// Takes each output's stored matrix while the config's routes still
    /// say what they said when it was stored. A further output's goes out
    /// with its mix at the start; the master's is sent then. One that no
    /// longer fits the output's width is logged, and the config's stays.
    pub(super) fn restore_matrices(&mut self, stored: &State) {
        for index in 0..self.matrices.len() {
            let entry = stored.outputs.get(self.card_of(index));
            let slot = &self.matrices[index];
            let saved = entry.and_then(|e| e.matrix.as_ref());
            let resolved = Stored::resolve(saved, &slot.config);
            if resolved == slot.config {
                continue;
            }
            let parts = resolved.into_iter().map(|r| {
                let to = i64::try_from(r.to).unwrap_or(i64::MAX);
                (r.from, to, f64::from(r.gain_db))
            });
            let (routes, matrix) = match routes_and_matrix(parts, slot.width) {
                Ok(made) => made,
                Err(e) => {
                    let n = index + 1;
                    eprintln!("aas: the stored matrix of output {n}: {e}, the config's applies");
                    continue;
                }
            };
            self.matrices[index].routes = routes;
            match index {
                0 => self.matrices[0].due = Some((self.epoch, Box::new(matrix))),
                _ => {
                    // Restore runs before start, which sends the mix.
                    let unsent = &mut self.further[index - 1].unsent;
                    debug_assert!(unsent.is_some(), "matrices restored after the start");
                    if let Some((unsent, _)) = unsent {
                        *unsent = matrix;
                    }
                }
            }
        }
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
    /// and after a reconnect. A name not connected can be set too. A name
    /// set back to 0 dB, unmuted, is what one without a stored volume
    /// plays at: it is no longer stored, and frees its place among the 64.
    /// A name is a process name, which the kernel cuts to 15 bytes, so a
    /// longer one could never match a client.
    fn patch_client(&mut self, name: String, patch: InputPatch, now: Instant) -> api::Answer {
        self.set_client_input(name, patch, now)
            .map(|view| api::answer(&view))
            .unwrap_or_else(Err)
    }

    /// The body of `patch_client`, shared with the client protocol's
    /// `SetClientVolume`. It sends a `ClientVolumeChanged` notice to every
    /// connection of that client, from a volume set here or by the API.
    pub(super) fn set_client_input(
        &mut self,
        name: String,
        patch: InputPatch,
        now: Instant,
    ) -> Result<InputView, ApiError> {
        if name.len() > MAX_NAME {
            let message = format!("a process name has at most {MAX_NAME} bytes");
            return Err(ApiError::unprocessable(message));
        }
        check_input(&patch)?;
        let stored = self.state.clients.get(&name);
        let input = patched_input(stored.unwrap_or(&UNITY), &patch);
        if input == *stored.unwrap_or(&UNITY) {
            return Ok(self.client_view(&name));
        }
        if stored.is_none() && self.state.clients.len() >= MAX_CLIENTS {
            return Err(ApiError::unprocessable(format!(
                "too many clients with a volume ({MAX_CLIENTS})"
            )));
        }
        let gain = input_gain(&input);
        if input == UNITY {
            self.state.clients.remove(&name);
        } else {
            self.state.clients.insert(name.clone(), input.clone());
        }
        let streams = self.conns.values().filter(|c| c.name == name);
        let batch = streams
            .filter_map(|c| c.stream.as_ref())
            .map(|s| Command::SetSourceGain(s.id, gain))
            .collect();
        self.send(batch);
        let notice = Reply::ClientVolumeChanged {
            db_millis: (input.volume_db * 1000.0).round() as i32,
            muted: input.muted,
        };
        let conns: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.name == name)
            .map(|(&conn, _)| conn)
            .collect();
        for conn in conns {
            self.notices.push((conn, notice));
        }
        let view = self.client_view(&name);
        self.publish(view.event());
        self.save_later(now);
        Ok(view)
    }

    /// A client's gain, as the state file has it. A client without one
    /// plays at 0 dB.
    pub(super) fn client_input(&self, name: &str) -> StoredInput {
        self.state.clients.get(name).cloned().unwrap_or(UNITY)
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

    fn master_metrics(&self) -> MasterMetrics {
        let c = &self.counters;
        let load = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        let cycles = load(&c.cycles);
        let gap = load(&c.last_gap_ns);
        let period = self.output.or(self.parked).map_or(0, |(_, period)| period);
        MasterMetrics {
            cycles,
            cycle_ns_avg: load(&c.cycle_ns_total).checked_div(cycles).unwrap_or(0),
            cycle_ns_worst: load(&c.worst_cycle_ns),
            buffer_ns: load(&c.buffer_ns),
            xruns: load(&c.xruns),
            wake_gaps: load(&c.wake_gaps),
            worst_gap_ns: load(&c.worst_gap_ns),
            last_gap: (gap > 0).then(|| LastGap {
                ns: gap,
                at_ns: load(&c.last_gap_at_ns),
            }),
            period_frames: period,
            card_frames: period * PERIODS,
            delay_frames: self.delay.unwrap_or(0),
        }
    }

    fn output_metrics(&self) -> Vec<OutputMetrics> {
        let mut views = Vec::new();
        for (i, output) in self.further.iter().enumerate() {
            let Some(c) = self.output_counters.get(i) else {
                continue;
            };
            let load = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
            let bits = load(&c.ratio_bits);
            views.push(OutputMetrics {
                id: output.index + 1,
                card: output.card.clone(),
                xruns: load(&c.xruns),
                wake_gaps: load(&c.wake_gaps),
                worst_gap_ns: load(&c.worst_gap_ns),
                starves: load(&c.starves),
                overflows: load(&c.overflows),
                skipped: load(&c.skipped),
                ppm: c.ppm_milli.load(Ordering::Relaxed) as f64 / 1e3,
                locked: c.locked.load(Ordering::Relaxed),
                ratio: (bits != 0).then(|| f64::from_bits(bits)),
                period_frames: output.current.map_or(0, |(_, period, _)| period),
                card_frames: output.current.map(|(_, period, _)| period * PERIODS),
            });
        }
        views
    }

    fn stream_metrics(&self) -> Vec<StreamMetrics> {
        let streams = self
            .conns
            .values()
            .filter_map(|c| c.stream.as_ref().map(|s| (c, s)));
        streams
            .map(|(conn, stream)| {
                let header = stream.view.header();
                let written = header.write_pos.load(Ordering::Acquire);
                let read = header.read_pos.load(Ordering::Acquire);
                StreamMetrics {
                    name: conn.name.clone(),
                    rate: stream.rate,
                    resampled: stream.resampler.is_some(),
                    underruns: header.underruns.load(Ordering::Relaxed),
                    fill_frames: written.saturating_sub(read),
                    capacity_frames: stream.capacity,
                    latency_frames: header.output_delay_frames.load(Ordering::Relaxed),
                }
            })
            .collect()
    }

    fn metrics_view(&self) -> MetricsView {
        let c = &self.counters;
        let analog = self.analog.as_ref().map(|_| AnalogMetrics {
            open: c.analog_open.load(Ordering::Relaxed) == 1,
            padded_frames: c.analog_padded_frames.load(Ordering::Relaxed),
            dropped_frames: c.analog_dropped_frames.load(Ordering::Relaxed),
            latency_frames: c.analog_latency_frames.load(Ordering::Relaxed) as u32,
        });
        MetricsView {
            master: self.master_metrics(),
            outputs: self.output_metrics(),
            streams: self.stream_metrics(),
            analog,
            plugins: self.plugin_metrics(),
        }
    }

    fn settings_view(&self) -> SettingsView {
        SettingsView {
            bit_perfect: mode_name(self.bit_perfect).to_string(),
            latency_ms: self.latency_ms,
            default_rate: self.default_rate,
            format: format_name(self.format).to_string(),
            resampling: quality_name(self.quality).to_string(),
        }
    }

    /// Checks every field first, with the config's rules and messages, so
    /// that a refused patch changes nothing. A change lasts until the next
    /// restart: nothing of it is stored.
    fn patch_settings(&mut self, patch: SettingsPatch, now: Instant) -> api::Answer {
        for (key, given, now) in [
            ("format", &patch.format, format_name(self.format)),
            ("resampling", &patch.resampling, quality_name(self.quality)),
        ] {
            if given.as_deref().is_some_and(|g| g != now) {
                let message = format!("{key} changes only in the config");
                return Err(ApiError::unprocessable(message));
            }
        }
        let mode = match patch.bit_perfect.as_deref() {
            None => None,
            Some("auto") => Some(BitPerfect::Auto),
            Some("forced") => Some(BitPerfect::Forced),
            Some("off") => Some(BitPerfect::Off),
            Some(_) => {
                return Err(ApiError::unprocessable(
                    "bit_perfect must be \"auto\", \"forced\" or \"off\"",
                ));
            }
        };
        if mode == Some(BitPerfect::Forced) {
            self.check_forced()?;
        }
        if let Some(ms) = patch.latency_ms {
            self.check_latency(ms)?;
        }
        if patch
            .default_rate
            .is_some_and(|rate| !(MIN_RATE..=MAX_RATE).contains(&rate))
        {
            let message = format!("default_rate must be from {MIN_RATE} to {MAX_RATE}");
            return Err(ApiError::unprocessable(message));
        }
        let before = self.settings_view();
        let mut batch = Vec::new();
        if let Some(mode) = mode.filter(|&m| m != self.bit_perfect) {
            // The engine takes the mode with no reopen, and the master's
            // gain as the volume now gives it.
            self.bit_perfect = mode;
            self.volume.set_mode(mode);
            batch.push(Command::SetMode(mode));
            batch.push(Command::SetGain(0, self.volume.gain()));
            eprintln!(
                "aas: bit_perfect = \"{}\" until the next restart",
                mode_name(mode)
            );
        }
        let mut decide = false;
        if let Some(ms) = patch.latency_ms.filter(|&ms| ms != self.latency_ms) {
            self.set_latency(ms);
            decide = true;
        }
        if let Some(rate) = patch.default_rate.filter(|&r| r != self.default_rate) {
            self.default_rate = rate;
            self.policy.set_base(rate);
            self.new_rate_decision();
            eprintln!("aas: default_rate = {rate} until the next restart");
            decide = true;
        }
        if decide {
            self.decide(now, false, &mut batch);
        }
        self.send(batch);
        let view = self.settings_view();
        if view != before {
            self.publish(view.event());
        }
        api::answer(&view)
    }

    /// Forced bit-perfect bypasses the `mix` and `output` chains, which then
    /// must keep the channel count, as the config's check at startup says.
    fn check_forced(&mut self) -> Result<(), ApiError> {
        let config = plugins::configs(&self.plugins);
        let w = widths(&config, self.channels, &mut self.libraries)
            .map_err(|(_, e)| ApiError::unprocessable(e.message))?;
        if w.changes || w.mix != self.channels {
            return Err(ApiError::unprocessable(
                "bit_perfect = \"forced\" needs chains that keep the channel count",
            ));
        }
        Ok(())
    }

    /// A latency the config takes, no longer than the one the daemon
    /// started with, which the rings of the clients already playing, the
    /// feeds and the delay line were sized for, and one every further
    /// output can be aligned with, as the start checks it.
    fn check_latency(&self, ms: f32) -> Result<(), ApiError> {
        if !(ms.is_finite() && ms > 0.0) {
            return Err(ApiError::unprocessable(
                "latency_ms must be a positive number",
            ));
        }
        let startup = self.startup_latency_ms;
        if ms > startup {
            return Err(ApiError::unprocessable(format!(
                "latency_ms above the {startup} ms the daemon started with needs a restart"
            )));
        }
        for output in &self.further {
            let own = if output.own_latency {
                output.latency_ms
            } else {
                ms
            };
            check_alignment(ms, output.number(), own, self.quality)
                .map_err(ApiError::unprocessable)?;
        }
        Ok(())
    }

    /// Takes the latency `ms` from the next open of the outputs on, for
    /// the master and every further output without one of its own. The
    /// outputs reopen at once, as for an urgent change of rate, if a period
    /// changes, and once a DoP stream has ended if one plays.
    fn set_latency(&mut self, ms: f32) {
        self.latency_ms = ms;
        for output in self.further.iter_mut().filter(|o| !o.own_latency) {
            output.latency_ms = ms;
        }
        let master = self
            .output
            .is_some_and(|(rate, period)| period_frames(rate, ms) != period);
        let further = self.further.iter().any(|o| {
            o.current
                .is_some_and(|(card, period, _)| period_frames(card, o.latency_ms) != period)
        });
        self.latency_due |= master || further;
        eprintln!("aas: latency_ms = {ms} until the next restart");
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
        FakeEngine, Opener, analog_config, configure, further, has, started, started_at,
        started_with, unstarted_with,
    };
    use super::super::*;
    use crate::api::{
        ApiCall, ApiError, ApiEvent, Call, InputPatch, MatrixPut, OutputPatch, OutputRoutes,
        RouteJson, SettingsPatch,
    };
    use crate::bitperfect::NotBitPerfect;
    use crate::config::{self, Idle, Route};
    use crate::counters::Counters;
    use crate::format::db_to_gain;
    use crate::state::{Stored, StoredInput, StoredOutput, StoredRoute};
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
            volume: Some(i64::from(percent)),
            ..OutputPatch::default()
        }
    }

    fn offset(db: f32) -> OutputPatch {
        OutputPatch {
            volume_db: Some(db),
            ..OutputPatch::default()
        }
    }

    fn limit(db: f32) -> OutputPatch {
        OutputPatch {
            limit_db: Some(db),
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
        play_labelled(control, conn, name, name, now);
    }

    /// As `play_as`, with `label` the client's name in the log.
    fn play_labelled(control: &mut Control, conn: ConnId, name: &str, label: &str, now: Instant) {
        control.connected_as(conn, name.into(), label.into());
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

    #[test]
    fn metrics_list_every_output_and_stream() {
        let (mut control, _engine, _logs, now) =
            started_with(&Config::default(), vec![further(&[])]);
        control.set_output_counters(vec![Arc::new(OutputCounters::default())]);
        play_as(&mut control, 1, "mpd", now);
        let view = control.call(Call::Metrics, now).unwrap();
        assert_eq!(view["outputs"][0]["id"], json!(2));
        assert_eq!(view["outputs"][0]["card"], json!("Loopback"));
        assert_eq!(view["outputs"][0]["ratio"], json!(null));
        let period = view["outputs"][0]["period_frames"].as_u64().unwrap();
        let card = &view["outputs"][0]["card_frames"];
        if period == 0 {
            assert_eq!(*card, json!(null));
        } else {
            assert_eq!(*card, json!(period * u64::from(PERIODS)));
        }
        assert_eq!(view["streams"].as_array().unwrap().len(), 1);
        assert_eq!(view["streams"][0]["name"], json!("mpd"));
        assert_eq!(view["streams"][0]["rate"], json!(48000));
        assert_eq!(view["analog"], json!(null));
        assert_eq!(view["plugins"], json!([]));
        assert_eq!(view["master"]["cycles"], json!(0));
        assert_eq!(view["master"]["cycle_ns_avg"], json!(0));
        assert_eq!(view["master"]["last_gap"], json!(null));
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
    fn a_limit_lowers_every_gain_tells_the_engine_and_is_published() {
        let (mut control, mut engine, _logs, now) =
            started_with(&Config::default(), vec![further(&[])]);
        let mut published = subscribe(&mut control);
        engine.take(true);
        engine.gains.clear();
        let view = control
            .call(Call::PatchOutput(1, limit(-10.0)), now)
            .unwrap();
        assert_eq!(view["limit_db"], json!(-10.0));
        assert_eq!(view["volume"], json!(100));
        let seen = engine.take(true);
        assert!(seen.contains(&"SetLimited true".to_string()), "{seen:?}");
        // The master and the further output, both below the limit.
        assert_eq!(engine.gains.len(), 2, "{:?}", engine.gains);
        assert!(close(engine.gains[0].1, db_to_gain(-10.0)));
        assert!(close(engine.gains[1].1, db_to_gain(-10.0)));
        let told = events(&mut published).into_iter().any(
            |e| matches!(e, ApiEvent::Output { id: 1, limit_db: Some(db), .. } if db == -10.0),
        );
        assert!(told);
        let stored = control.state.outputs[&control.card]
            .limit_db
            .as_ref()
            .unwrap();
        assert_eq!((stored.value, stored.config), (-10.0, 0.0));
        // Lifted, the engine hears of it as well.
        control.call(Call::PatchOutput(1, limit(0.0)), now).unwrap();
        let seen = engine.take(true);
        assert!(seen.contains(&"SetLimited false".to_string()), "{seen:?}");
        // The same limit again changes nothing.
        control.call(Call::PatchOutput(1, limit(0.0)), now).unwrap();
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn a_limit_out_of_range_or_on_a_further_output_is_422() {
        let (mut control, mut engine, _logs, now) =
            started_with(&Config::default(), vec![further(&[])]);
        engine.take(true);
        for db in [-61.0, 0.5, f32::NAN] {
            let answer = control.call(Call::PatchOutput(1, limit(db)), now);
            assert_eq!(message(answer), "limit_db must be from -60 to 0");
        }
        let answer = control.call(Call::PatchOutput(2, limit(-6.0)), now);
        assert_eq!(status(answer), 422);
        assert!(engine.take(true).is_empty());
        assert!(!control.volume.limited());
    }

    #[test]
    fn a_limit_in_the_config_is_sent_at_start() {
        let config = Config {
            volume_limit_db: -6.0,
            ..Config::default()
        };
        let (mut control, mut engine, _logs, now) = started_with(&config, Vec::new());
        let seen = engine.take(true);
        assert!(seen.contains(&"SetLimited true".to_string()), "{seen:?}");
        assert!(close(engine.gains[0].1, db_to_gain(-6.0)));
        let view = control.call(Call::Output(1), now).unwrap();
        assert_eq!(view["limit_db"], json!(-6.0));
    }

    fn with_a_receiver() -> Config {
        use crate::airplay::config::{AirplayConfig, Connect, Protocol};
        Config {
            airplay: vec![AirplayConfig {
                name: "Study".into(),
                host: "127.0.0.1".into(),
                // The discard port: nothing answers there.
                port: 9,
                protocol: Protocol::AirPlay1,
                connect: Connect::Signal,
                threshold_dbfs: -50.0,
                idle_s: 120,
                latency_ms: 1500,
                volume_db: Some(-25.0),
                enabled: true,
            }],
            ..Config::default()
        }
    }

    /// The answer of a pairing call that is answered at once.
    fn pair_now(control: &mut Control, name: &str, pin: Option<&str>, now: Instant) -> ApiError {
        let (answer, mut answered) = tokio::sync::oneshot::channel();
        let call = Call::PairAirplay {
            name: name.into(),
            pin: pin.map(String::from),
        };
        control.answer(ApiCall { call, answer }, now);
        answered.try_recv().unwrap().unwrap_err()
    }

    fn with_an_airplay2_receiver() -> Config {
        let mut c = with_a_receiver();
        c.airplay[0].protocol = crate::airplay::config::Protocol::AirPlay2;
        c
    }

    fn off(mut c: Config) -> Config {
        c.airplay[0].enabled = false;
        c
    }

    fn patch_enabled(control: &mut Control, name: &str, on: bool, now: Instant) -> crate::api::Answer {
        let patch = crate::api::AirplayPatch { enabled: Some(on) };
        control.call(Call::PatchAirplay(name.into(), patch), now)
    }

    #[test]
    fn a_receiver_that_is_off_has_no_thread_and_says_so() {
        let (mut control, _engine, _logs, now) = started_with(&off(with_a_receiver()), Vec::new());
        assert!(!control.airplay[0].running());
        let view = control.call(Call::Airplay, now).unwrap();
        assert_eq!(view[0]["enabled"], json!(false));
    }

    #[test]
    fn patch_airplay_turns_a_receiver_off_and_on() {
        let (mut control, _engine, _logs, now) = started_with(&with_a_receiver(), Vec::new());
        assert!(control.airplay[0].running());
        let view = patch_enabled(&mut control, "Study", false, now).unwrap();
        assert_eq!(view["enabled"], json!(false));
        assert!(!control.airplay[0].running());
        let view = patch_enabled(&mut control, "Study", true, now).unwrap();
        assert_eq!(view["enabled"], json!(true));
        assert!(control.airplay[0].running());
    }

    #[test]
    fn patch_airplay_is_kept_in_the_state() {
        let (mut control, _engine, _logs, now) = started_with(&with_a_receiver(), Vec::new());
        patch_enabled(&mut control, "Study", false, now).unwrap();
        assert_eq!(
            control.state.airplay["Study"].enabled,
            Some(Stored {
                value: false,
                config: true
            })
        );
    }

    #[test]
    fn the_state_does_not_outlast_a_changed_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let mut state = State::default();
        state.airplay.entry("Study".into()).or_default().enabled = Some(Stored {
            value: false,
            config: true,
        });
        state.save(&path).unwrap();
        let restore = |config: Config| {
            let (mut control, _engine, _logs) = unstarted_with(&config, Vec::new());
            let volume = OutputVolume::new(None, config::BitPerfect::Auto);
            control.restore(volume, path.clone(), &[]);
            control.airplay[0].enabled()
        };
        // The config still says on: the stored off applies.
        assert!(!restore(with_a_receiver()));
        // The config says off now: it wins, and so would on.
        assert!(!restore(off(with_a_receiver())));
    }

    #[test]
    fn a_receiver_that_is_off_can_be_paired() {
        use crate::airplay::ap2::fake_hap::FakeAccessory;
        use crate::airplay::fake::{Answer as Reply, FakeReceiver};
        let acc = FakeAccessory::new("1234");
        let receiver = FakeReceiver::serving(1, move |_| {
            move |seen: &crate::airplay::fake::Seen| {
                acc.answer(seen).unwrap_or(Reply::Status(200, Vec::new()))
            }
        });
        let mut config = off(with_an_airplay2_receiver());
        config.airplay[0].port = receiver.port;
        let (mut control, _engine, _logs, now) = started_with(&config, Vec::new());
        let (answer, answered) = tokio::sync::oneshot::channel();
        let call = Call::PairAirplay {
            name: "Study".into(),
            pin: None,
        };
        control.answer(ApiCall { call, answer }, now);
        assert_eq!(answered.blocking_recv().unwrap().unwrap(), json!({"pin": "shown"}));
        assert!(!control.airplay[0].running());
    }

    #[test]
    fn patch_airplay_for_an_unknown_receiver_is_404() {
        let (mut control, _engine, _logs, now) = started_with(&with_a_receiver(), Vec::new());
        let e = patch_enabled(&mut control, "Kitchen", false, now).unwrap_err();
        assert_eq!(e.status, 404);
    }

    #[test]
    fn pairing_an_unknown_receiver_is_404() {
        let (mut control, _engine, _logs, now) = started_with(&with_an_airplay2_receiver(), Vec::new());
        assert_eq!(pair_now(&mut control, "Kitchen", None, now).status, 404);
    }

    #[test]
    fn pairing_an_airplay1_receiver_is_422() {
        let (mut control, _engine, _logs, now) = started_with(&with_a_receiver(), Vec::new());
        let e = pair_now(&mut control, "Study", None, now);
        assert_eq!(e.status, 422);
        assert_eq!(e.message, "\"Study\" is an AirPlay 1 receiver, which needs no pairing");
    }

    #[test]
    fn a_pin_that_is_not_four_digits_is_400() {
        let (mut control, _engine, _logs, now) = started_with(&with_an_airplay2_receiver(), Vec::new());
        for pin in ["123", "12345", "12a4", ""] {
            let e = pair_now(&mut control, "Study", Some(pin), now);
            assert_eq!((e.status, e.message.as_str()), (400, "pin is 4 digits"), "{pin}");
        }
    }

    #[test]
    fn get_airplay_says_whether_an_airplay2_receiver_is_paired() {
        let (mut control, _engine, _logs, now) = started_with(&with_an_airplay2_receiver(), Vec::new());
        let view = control.call(Call::Airplay, now).unwrap();
        assert_eq!(view[0]["paired"], json!(false));
        let (mut control, _engine, _logs, now) = started_with(&with_a_receiver(), Vec::new());
        let view = control.call(Call::Airplay, now).unwrap();
        assert!(view[0].get("paired").is_none());
    }

    #[test]
    fn metadata_gives_the_current_track_without_the_cover_bytes() {
        let (mut control, _engine, _logs, now) = started_with(&Config::default(), Vec::new());
        assert_eq!(
            control.call(Call::Metadata, now).unwrap(),
            json!({"title": null, "artist": null, "album": null, "duration_s": null,
                   "position_s": null, "playing": false, "cover": null})
        );
        control.now_playing.set_track(crate::metadata::Track {
            title: Some("T".into()),
            ..Default::default()
        });
        control.now_playing.set_cover(Some(crate::metadata::Cover {
            mime: "image/jpeg",
            bytes: vec![0xFF, 0xD8, 0xFF],
        }));
        assert_eq!(
            control.call(Call::Metadata, now).unwrap(),
            json!({"title": "T", "artist": null, "album": null, "duration_s": null,
                   "position_s": null, "playing": false,
                   "cover": {"mime": "image/jpeg", "bytes": 3}})
        );
    }

    #[test]
    fn every_receiver_shares_the_one_current_track() {
        let (control, _engine, _logs, _now) = started_with(&with_a_receiver(), Vec::new());
        assert!(control.airplay.iter().all(|r| r.shares(&control.now_playing)));
    }

    #[test]
    fn get_airplay_lists_each_receiver_with_its_state() {
        let (mut control, _engine, _logs, now) = started_with(&with_a_receiver(), Vec::new());
        let view = control.call(Call::Airplay, now).unwrap();
        assert_eq!(
            view,
            json!([{
                "name": "Study",
                "host": "127.0.0.1",
                "port": 9,
                "protocol": "airplay1",
                "connect": "signal",
                "state": "idle",
                "enabled": true
            }])
        );
        let (mut control, _engine, _logs, now) = started_with(&Config::default(), Vec::new());
        assert_eq!(control.call(Call::Airplay, now).unwrap(), json!([]));
    }

    #[test]
    fn a_receivers_state_is_published_once_per_change() {
        let mut config = with_a_receiver();
        // Connects at once to a port nothing answers: it fails and retries.
        config.airplay[0].connect = crate::airplay::config::Connect::Always;
        let (mut control, _engine, _logs, now) = started_with(&config, Vec::new());
        let mut published = subscribe(&mut control);
        let mut states = Vec::new();
        let end = Instant::now() + std::time::Duration::from_secs(3);
        while Instant::now() < end && !states.iter().any(|s: &String| s == "retrying") {
            control.tick(now);
            for e in events(&mut published) {
                if let ApiEvent::Airplay { name, state, error } = e {
                    assert_eq!(name, "Study");
                    if state == "retrying" {
                        assert!(error.is_some());
                    }
                    states.push(state);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(states.last().map(String::as_str), Some("retrying"), "{states:?}");
        // Nothing new, nothing told.
        control.tick(now);
        assert!(events(&mut published).is_empty());
    }

    #[test]
    fn opening_the_output_gives_each_receiver_a_monitor_tap() {
        let (_control, mut engine, _logs, _now) = started_with(&with_a_receiver(), Vec::new());
        let seen = engine.take(true);
        assert_eq!(seen.iter().filter(|c| c.starts_with("AddTap")).count(), 1, "{seen:?}");
        let (_control, mut engine, _logs, _now) = started_with(&Config::default(), Vec::new());
        assert!(!engine.take(true).iter().any(|c| c.starts_with("AddTap")));
    }

    #[test]
    fn the_master_says_its_clock_when_it_is_the_capture() {
        let config = Config {
            clock: crate::config::Clock::Capture,
            ..Config::default()
        };
        let (mut control, _engine, _logs, now) = started_with(&config, Vec::new());
        let view = control.call(Call::Output(1), now).unwrap();
        assert_eq!(view["clock"], json!("capture"));
        let (mut control, _engine, _logs, now) = started_with(&Config::default(), Vec::new());
        let view = control.call(Call::Output(1), now).unwrap();
        assert!(view.get("clock").is_none(), "{view}");
    }

    #[test]
    fn a_stored_limit_applies_while_the_config_says_what_it_said() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let mut state = State::default();
        state.outputs.entry("master".into()).or_default().limit_db = Some(Stored {
            value: -12.0,
            config: 0.0,
        });
        state.save(&path).unwrap();
        let restore = |config_db: f32| {
            let config = Config {
                volume_limit_db: config_db,
                ..Config::default()
            };
            let (mut control, _engine, _logs) = unstarted_with(&config, Vec::new());
            let volume = OutputVolume::new(None, config::BitPerfect::Auto);
            control.restore(volume, path.clone(), &["master".to_string()]);
            control.volume.limit_db()
        };
        assert_eq!(restore(0.0), -12.0);
        // The config's limit edited since: it wins.
        assert_eq!(restore(-3.0), -3.0);
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
    fn a_client_volume_applies_to_the_same_process_name_after_a_reconnect() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        play_labelled(&mut control, 1, "mpd", "mpd (100)", now);
        let answer = control.call(Call::PatchClient("mpd".into(), input_db(-6.0)), now);
        assert!(answer.is_ok());
        control.disconnected(1, now);
        engine.take(true);
        // Another process of the same name, so another pid.
        play_labelled(&mut control, 2, "mpd", "mpd (200)", now);
        engine.take(true);
        assert_eq!(engine.added.last(), Some(&(2, db_to_gain(-6.0))));
        let state = control.call(Call::State, now).unwrap();
        assert_eq!(state["clients"][0]["name"], json!("mpd"));
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
    fn a_client_back_at_0_db_frees_its_slot() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        play_as(&mut control, 1, "player7", now);
        engine.take(true);
        for n in 0..64 {
            let call = Call::PatchClient(format!("player{n}"), input_db(-1.0));
            assert_eq!(status(control.call(call, now)), 200, "{n}");
        }
        engine.take(true);
        let mut published = subscribe(&mut control);
        let call = Call::PatchClient("player7".into(), input_db(0.0));
        let answer = control.call(call, now).unwrap();
        assert_eq!(answer["volume_db"], json!(0.0));
        assert_eq!(control.state.clients.len(), 63);
        // Its stream plays at 0 dB, and the change is published.
        assert_eq!(engine.take(true), ["SetSourceGain 1 1"]);
        let id = |e: &ApiEvent| matches!(e, ApiEvent::Input { id, .. } if id == "clients/player7");
        assert!(events(&mut published).iter().any(id));
        // A new name is taken in its place.
        let call = Call::PatchClient("one-more".into(), input_db(-1.0));
        assert_eq!(status(control.call(call, now)), 200);
        // Muted is not 0 dB unmuted: that one keeps its slot.
        let mute = InputPatch {
            volume_db: Some(0.0),
            muted: Some(true),
        };
        assert_eq!(
            status(control.call(Call::PatchClient("player8".into(), mute), now)),
            200
        );
        assert!(control.state.clients.contains_key("player8"));
        // A freed name that is not connected is not known any more.
        let call = Call::PatchClient("player9".into(), input_db(0.0));
        assert_eq!(status(control.call(call, now)), 200);
        assert_eq!(
            status(control.call(Call::Client("player9".into()), now)),
            404
        );
    }

    #[test]
    fn a_call_whose_caller_has_given_up_is_not_applied() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        let (answer, answered) = tokio::sync::oneshot::channel();
        // The API thread answered 503 after 2 s and dropped its end.
        drop(answered);
        let call = Call::PatchClient("mpd".into(), input_db(-6.0));
        control.answer(ApiCall { call, answer }, now);
        assert!(control.state.clients.is_empty());
        assert!(engine.take(true).is_empty());
        // One still waited for is applied and answered.
        let (answer, mut answered) = tokio::sync::oneshot::channel();
        let call = Call::PatchClient("mpd".into(), input_db(-6.0));
        control.answer(ApiCall { call, answer }, now);
        assert!(control.state.clients.contains_key("mpd"));
        assert!(answered.try_recv().unwrap().is_ok());
    }

    #[test]
    fn a_name_longer_than_a_process_name_is_422() {
        let (mut control, _engine, now) = open(&Config::default(), None);
        let call = Call::PatchClient("a".repeat(16), input_db(-1.0));
        let answer = control.call(call, now);
        assert_eq!(status(answer.clone()), 422);
        assert_eq!(message(answer), "a process name has at most 15 bytes");
        assert!(control.state.clients.is_empty());
        // 15 bytes, two of them one character, is a name.
        let call = Call::PatchClient(format!("{}é", "a".repeat(13)), input_db(-1.0));
        assert_eq!(status(control.call(call, now)), 200);
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
    fn the_analog_mute_survives_an_edit_of_the_configs_volume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        // Muted at -10 dB through the API while the config said -6 dB.
        let state = State {
            analog: Some(Stored {
                value: StoredInput {
                    volume_db: -10.0,
                    muted: true,
                },
                config: StoredInput {
                    volume_db: -6.0,
                    muted: false,
                },
            }),
            ..State::default()
        };
        state.save(&path).unwrap();
        let restore = |config: &Config| {
            let (mut control, _engine, _logs) = unstarted_with(config, Vec::new());
            let volume = OutputVolume::new(None, config::BitPerfect::Auto);
            control.restore(volume, path.clone(), &[]);
            control.analog_input
        };
        let with_db = |volume_db: f32| Config {
            analog: config::Analog {
                enabled: true,
                volume_db,
            },
            ..Config::default()
        };
        // The config left alone: both as stored.
        let input = restore(&with_db(-6.0));
        assert_eq!((input.volume_db, input.muted), (-10.0, true));
        // The config's volume edited: it wins, and the mute, which has no
        // config key, stays.
        let input = restore(&with_db(-3.0));
        assert_eq!((input.volume_db, input.muted), (-3.0, true));
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

    /// The client changes published since the last time, by name.
    fn changes(published: &mut broadcast::Receiver<ApiEvent>) -> Vec<(String, String)> {
        events(published)
            .into_iter()
            .filter_map(|e| match e {
                ApiEvent::Client { name, change } => Some((name, change)),
                _ => None,
            })
            .collect()
    }

    fn recorder(control: &mut Control, conn: ConnId, now: Instant) {
        control.connected(conn, "arecord".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(conn, Some(hello), now);
        let capture = Request::Configure {
            rate: 0,
            format: ClientFormat::S32,
            channels: 2,
            buffer_frames: 4096,
            capture: true,
            monitor: false,
        };
        let answer = control.request(conn, Some(capture), now);
        assert!(matches!(answer.reply, Some(Reply::Configured { .. })));
    }

    #[test]
    fn recorder_events_are_published() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        let mut published = subscribe(&mut control);
        recorder(&mut control, 1, now);
        assert_eq!(
            control.request(1, Some(Request::Start), now).reply,
            Some(Reply::Ok)
        );
        let state = control.call(Call::State, now).unwrap();
        assert_eq!(state["clients"][0]["running"], json!(true));
        control.request(1, Some(Request::Stop), now);
        control.disconnected(1, now);
        engine.take(true);
        let name = |change: &str| ("arecord".to_string(), change.to_string());
        let want = ["connected", "running", "stopped", "gone"].map(name);
        assert_eq!(changes(&mut published), want);
        // A recorder that runs is stopped by a reset, by a disconnect, and
        // by the output moving to another rate, which ends it.
        recorder(&mut control, 2, now);
        control.request(2, Some(Request::Start), now);
        control.request(2, Some(Request::Reset), now);
        control.request(2, Some(Request::Start), now);
        control.disconnected(2, now);
        recorder(&mut control, 3, now);
        control.request(3, Some(Request::Start), now);
        play_as(&mut control, 4, "hq", now);
        let buffer = min_buffer_frames(96000, 12.0);
        control.request(4, Some(configure(96000, buffer)), now);
        control.request(4, Some(Request::Start), now);
        engine.take(true);
        let seen: Vec<(String, String)> = changes(&mut published)
            .into_iter()
            .filter(|(n, _)| n == "arecord")
            .collect();
        let want = [
            "connected",
            "running",
            "stopped",
            "running",
            "stopped",
            "gone",
            "connected",
            "running",
            "stopped",
        ]
        .map(name);
        assert_eq!(seen, want);
        let state = control.call(Call::State, now).unwrap();
        let clients = state["clients"].as_array().unwrap();
        assert!(
            clients
                .iter()
                .all(|c| c["name"] != json!("arecord") || c["running"] == json!(false))
        );
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
    fn off_after_zeros_takes_the_configs_delay() {
        let config = Config {
            idle: Idle::Off { delay_s: 30 },
            ..Config::default()
        };
        let (mut output, rates) = further(&[]);
        output.idle = Idle::Off { delay_s: 20 };
        let (mut control, mut engine, _logs, now) =
            started_with(&config, vec![(output, rates), further(&[])]);
        engine.take(true);
        let kind = |kind: &str| OutputPatch {
            idle: Some(kind.into()),
            ..OutputPatch::default()
        };
        // Each output's own delay from the config, or 60 s where the
        // config says zeros.
        for (id, delay) in [(1, 30), (2, 20), (3, 60)] {
            control
                .call(Call::PatchOutput(id, kind("zeros")), now)
                .unwrap();
            let view = control
                .call(Call::PatchOutput(id, kind("off")), now)
                .unwrap();
            assert_eq!(view["idle_delay_s"], json!(delay), "output {id}");
        }
    }

    #[test]
    fn a_further_output_held_off_does_not_keep_the_master_running() {
        let config = Config {
            idle: Idle::Off { delay_s: 10 },
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
        control
            .call(Call::PatchOutput(2, zeros), now + Duration::from_secs(2))
            .unwrap();
        engine.take(true);
        // The master closes, rather than parking for an output that is off.
        control.tick(now + Duration::from_secs(10));
        let commands = engine.take(true);
        assert!(has(&commands, "CloseOutput"), "{commands:?}");
        assert!(!has(&commands, "ParkOutput"), "{commands:?}");
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

    fn route(from: &str, to: i64, gain_db: Option<f64>) -> RouteJson {
        RouteJson {
            from: from.into(),
            to,
            gain_db,
        }
    }

    /// Left to channel 2 and right to channel 1.
    fn swap() -> Vec<RouteJson> {
        vec![
            route("players.left", 2, None),
            route("players.right", 1, Some(0.0)),
        ]
    }

    fn swap_matrix(width: usize) -> Matrix {
        let routes = [
            Route {
                from: 0,
                to: 2,
                gain_db: 0.0,
            },
            Route {
                from: 1,
                to: 1,
                gain_db: 0.0,
            },
        ];
        Matrix::from_routes(&routes, width).unwrap()
    }

    fn put(outputs: Vec<(usize, Vec<RouteJson>)>) -> Call {
        let outputs = outputs
            .into_iter()
            .map(|(output, routes)| OutputRoutes { output, routes })
            .collect();
        Call::PutMatrix(MatrixPut { outputs })
    }

    /// The routes `GET /matrix` gives for API output `id`.
    fn routes_of(control: &mut Control, id: usize, now: Instant) -> serde_json::Value {
        let view = control.call(Call::Matrix, now).unwrap();
        let outputs = view["outputs"].as_array().unwrap();
        let entry = outputs.iter().find(|o| o["output"] == json!(id)).unwrap();
        entry["routes"].clone()
    }

    fn default_routes() -> serde_json::Value {
        json!([
            {"from": "players.left", "to": 1, "gain_db": 0.0},
            {"from": "players.right", "to": 2, "gain_db": 0.0},
            {"from": "analog.left", "to": 1, "gain_db": 0.0},
            {"from": "analog.right", "to": 2, "gain_db": 0.0},
            {"from": "aes67.left", "to": 1, "gain_db": 0.0},
            {"from": "aes67.right", "to": 2, "gain_db": 0.0},
        ])
    }

    /// `swap()` as `GET /matrix` gives it: it names no `aes67` route, so
    /// the `aes67` bus plays as in the default matrix.
    fn swap_routes() -> serde_json::Value {
        json!([
            {"from": "players.left", "to": 2, "gain_db": 0.0},
            {"from": "players.right", "to": 1, "gain_db": 0.0},
            {"from": "aes67.left", "to": 1, "gain_db": 0.0},
            {"from": "aes67.right", "to": 2, "gain_db": 0.0},
        ])
    }

    #[test]
    fn a_matrix_put_reaches_the_engine() {
        let (mut control, mut engine, _logs, now) =
            started_with(&Config::default(), vec![further(&[])]);
        engine.take(true);
        for id in [1, 2] {
            let answer = control.call(put(vec![(id, swap())]), now);
            assert_eq!(status(answer), 200, "{id}");
            assert_eq!(engine.take(true), ["SetMatrix"], "{id}");
            assert_eq!(engine.matrices, [(id - 1, swap_matrix(2))], "{id}");
            engine.matrices.clear();
            assert_eq!(routes_of(&mut control, id, now), swap_routes(), "{id}");
        }
    }

    #[test]
    fn a_matrix_that_names_an_aes67_route_shows_only_the_routes_named() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        let routes = vec![
            route("players.left", 1, None),
            route("aes67.right", 2, Some(-3.0)),
        ];
        assert_eq!(status(control.call(put(vec![(1, routes)]), now)), 200);
        engine.take(true);
        let want = json!([
            {"from": "players.left", "to": 1, "gain_db": 0.0},
            {"from": "aes67.right", "to": 2, "gain_db": -3.0},
        ]);
        assert_eq!(routes_of(&mut control, 1, now), want);
    }

    #[test]
    fn a_matrix_of_another_width_is_422() {
        let (mut output, rates) = further(&[]);
        output.channels = 1;
        let (mut control, mut engine, _logs, now) =
            started_with(&Config::default(), vec![(output, rates)]);
        engine.take(true);
        // A stereo matrix for a mono output.
        let answer = control.call(put(vec![(2, swap())]), now);
        assert_eq!(status(answer.clone()), 422);
        assert_eq!(
            message(answer),
            "output 2: matrix 1: channel 2 is outside the matrix, which has 1 channel"
        );
        assert!(engine.take(true).is_empty());
        let mono = json!([
            {"from": "players.left", "to": 1, "gain_db": 0.0},
            {"from": "analog.left", "to": 1, "gain_db": 0.0},
            {"from": "aes67.left", "to": 1, "gain_db": 0.0},
        ]);
        assert_eq!(routes_of(&mut control, 2, now), mono);
    }

    #[test]
    fn a_route_above_the_width_is_422() {
        let (mut control, mut engine, _logs, now) =
            started_with(&Config::default(), vec![further(&[])]);
        engine.take(true);
        let mut published = subscribe(&mut control);
        let routes = vec![
            route("players.left", 1, None),
            route("analog.left", 3, None),
        ];
        let answer = control.call(put(vec![(1, routes)]), now);
        assert_eq!(
            message(answer),
            "matrix 2: channel 3 is outside the matrix, which has 2 channels"
        );
        let answer = control.call(put(vec![(1, vec![route("players.left", 9, None)])]), now);
        assert_eq!(
            message(answer),
            "matrix 1: to must be a channel from 1 to 8"
        );
        let loud = vec![route("players.left", 1, Some(0.5))];
        let answer = control.call(put(vec![(1, loud)]), now);
        assert_eq!(message(answer), "matrix 1: gain_db must be from -100 to 0");
        // A 422 for one output changes nothing for another named before it.
        let bad = vec![route("players.left", 3, None)];
        let answer = control.call(put(vec![(2, swap()), (1, bad)]), now);
        assert_eq!(status(answer), 422);
        assert!(engine.take(true).is_empty());
        assert!(events(&mut published).is_empty());
        assert!(control.save_at.is_none());
        assert_eq!(routes_of(&mut control, 2, now), default_routes());
    }

    #[test]
    fn an_unknown_source_is_422() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        let routes = vec![
            route("players.left", 1, None),
            route("players.middle", 1, None),
        ];
        let answer = control.call(put(vec![(1, routes)]), now);
        assert_eq!(status(answer.clone()), 422);
        let want = config::route_from_parts(2, "players.middle", 1, 0.0).unwrap_err();
        assert_eq!(message(answer), want);
        assert!(
            want.starts_with("matrix 2: from \"players.middle\""),
            "{want}"
        );
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn output_9_is_404() {
        let (mut control, mut engine, _logs, now) =
            started_with(&Config::default(), vec![further(&[])]);
        engine.take(true);
        for id in [0, 3, 9] {
            let answer = control.call(put(vec![(1, swap()), (id, swap())]), now);
            assert_eq!(status(answer.clone()), 404, "{id}");
            assert_eq!(message(answer), format!("no output {id}"));
        }
        assert!(engine.take(true).is_empty());
        assert_eq!(routes_of(&mut control, 1, now), default_routes());
    }

    #[test]
    fn an_output_named_twice_is_422() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        let answer = control.call(put(vec![(1, swap()), (1, Vec::new())]), now);
        assert_eq!(status(answer.clone()), 422);
        assert_eq!(message(answer), "output 1 is named twice");
        assert!(engine.take(true).is_empty());
    }

    #[test]
    fn an_empty_route_list_is_the_default_matrix() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        control.call(put(vec![(1, swap())]), now).unwrap();
        engine.take(true);
        engine.matrices.clear();
        let later = now + Duration::from_millis(20);
        control.call(put(vec![(1, Vec::new())]), later).unwrap();
        engine.take(true);
        assert_eq!(engine.matrices, [(0, Matrix::default_for(2))]);
        assert_eq!(routes_of(&mut control, 1, later), default_routes());
    }

    #[test]
    fn an_output_not_named_keeps_its_matrix() {
        let (mut control, mut engine, _logs, now) =
            started_with(&Config::default(), vec![further(&[])]);
        engine.take(true);
        control.call(put(vec![(2, swap())]), now).unwrap();
        engine.take(true);
        engine.matrices.clear();
        control.call(put(vec![(1, swap())]), now).unwrap();
        engine.take(true);
        assert_eq!(engine.matrices, [(0, swap_matrix(2))]);
        assert_eq!(routes_of(&mut control, 2, now), swap_routes());
    }

    #[test]
    fn matrices_are_paced_at_20_ms() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        // Each put a little quieter, so that each matrix is told apart.
        let quieter = |i: u32| vec![route("players.left", 1, Some(-f64::from(i) * 0.5))];
        let matrix_of = |i: u32| {
            let route = Route {
                from: 0,
                to: 1,
                gain_db: -(i as f32) * 0.5,
            };
            Matrix::from_routes(&[route], 2).unwrap()
        };
        for i in 0..100 {
            let at = now + Duration::from_micros(10 * u64::from(i));
            control.call(put(vec![(1, quieter(i))]), at).unwrap();
        }
        assert_eq!(engine.take(true), ["SetMatrix"]);
        assert_eq!(engine.matrices, [(0, matrix_of(0))]);
        let due = now + Duration::from_millis(20);
        assert_eq!(control.next_due(), Some(due));
        control.handle_events(now + Duration::from_millis(1));
        control.tick(now + Duration::from_millis(19));
        assert!(engine.take(true).is_empty());
        control.tick(due);
        assert_eq!(engine.take(true), ["SetMatrix"]);
        assert_eq!(engine.matrices, [(0, matrix_of(0)), (0, matrix_of(99))]);
        assert_eq!(control.next_due(), None);
        control.tick(due + Duration::from_millis(40));
        assert!(engine.take(true).is_empty());
        // Every matrix that came back is taken off the queue, and freed.
        control.handle_events(due);
        assert!(control.events.is_empty());
        assert!(control.stash.is_empty());
        // Once 20 ms have passed, the next one goes at once.
        let later = due + Duration::from_millis(20);
        control.call(put(vec![(1, quieter(1))]), later).unwrap();
        assert_eq!(engine.take(true), ["SetMatrix"]);
        assert_eq!(control.next_due(), None);
    }

    #[test]
    fn a_refused_matrix_is_noticed_once() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        control.call(put(vec![(1, swap())]), now).unwrap();
        engine.take(true);
        control.handle_events(now);
        assert!(!control.matrices[0].refused);
        engine.refuse_matrices = true;
        let later = now + Duration::from_millis(20);
        control.call(put(vec![(1, Vec::new())]), later).unwrap();
        engine.take(true);
        control.handle_events(later);
        assert!(control.matrices[0].refused);
        assert!(control.events.is_empty());
    }

    #[test]
    fn a_matrix_put_is_saved_and_published() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let (mut control, _engine, now) = restored_matrices(&path, None);
        let mut published = subscribe(&mut control);
        let answer = control.call(put(vec![(2, swap())]), now).unwrap();
        assert_eq!(answer["outputs"][1]["routes"], swap_routes());
        let want = ApiEvent::Matrix {
            output: 2,
            routes: vec![
                route("players.left", 2, Some(0.0)),
                route("players.right", 1, Some(0.0)),
                route("aes67.left", 1, Some(0.0)),
                route("aes67.right", 2, Some(0.0)),
            ],
        };
        assert_eq!(events(&mut published), [want]);
        assert!(control.save_at.is_some());
        let swap = vec![
            stored_route("players.left", 2),
            stored_route("players.right", 1),
        ];
        control.save_at = Some(now);
        control.shutdown();
        let stored = |value| {
            Some(Stored {
                value,
                config: Vec::new(),
            })
        };
        assert_eq!(State::load(&path).outputs["A"].matrix, stored(swap));
        // The default matrix is stored as an empty list.
        let (mut control, _engine, now) = restored_matrices(&path, None);
        assert_eq!(routes_of(&mut control, 2, now), swap_routes());
        control.call(put(vec![(2, Vec::new())]), now).unwrap();
        assert!(control.save_at.is_some());
        control.save_at = Some(now);
        control.shutdown();
        assert_eq!(State::load(&path).outputs["A"].matrix, stored(Vec::new()));
    }

    fn stored_route(from: &str, to: usize) -> StoredRoute {
        StoredRoute {
            from: from.into(),
            to,
            gain_db: 0.0,
        }
    }

    /// A state file whose master ("master") and further output ("A") each
    /// keep the swap, stored against the config matrix given.
    fn stored_matrices(path: &std::path::Path, config: Vec<StoredRoute>) {
        let mut state = State::default();
        let swap = vec![
            stored_route("players.left", 2),
            stored_route("players.right", 1),
        ];
        for card in ["master", "A"] {
            let output = StoredOutput {
                matrix: Some(Stored {
                    value: swap.clone(),
                    config: config.clone(),
                }),
                ..StoredOutput::default()
            };
            state.outputs.insert(card.into(), output);
        }
        state.save(path).unwrap();
    }

    /// Starts a control thread with a further output on card "A", both it
    /// and the master with the config matrix given, restored from `path`.
    fn restored_matrices(
        path: &std::path::Path,
        matrix: Option<Vec<Route>>,
    ) -> (Control, FakeEngine, Instant) {
        let config = Config {
            matrix: matrix.clone(),
            ..Config::default()
        };
        let (mut output, rates) = further(&[]);
        output.card = "A".into();
        output.matrix = matrix;
        let (mut control, mut engine, _logs) = unstarted_with(&config, vec![(output, rates)]);
        let volume = OutputVolume::new(None, config::BitPerfect::Auto);
        let cards = ["master".to_string(), "A".to_string()];
        control.restore(volume, path.to_path_buf(), &cards);
        let now = Instant::now();
        control.start(now);
        engine.take(true);
        (control, engine, now)
    }

    #[test]
    fn a_stored_matrix_is_restored_while_the_config_matrix_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        stored_matrices(&path, Vec::new());
        let (mut control, engine, now) = restored_matrices(&path, None);
        assert_eq!(engine.matrices, [(0, swap_matrix(2))]);
        assert_eq!(engine.mixes, [(1, swap_matrix(2))]);
        for id in [1, 2] {
            assert_eq!(routes_of(&mut control, id, now), swap_routes(), "{id}");
        }
        // The pacing starts from the restore's matrix.
        control.call(put(vec![(1, Vec::new())]), now).unwrap();
        assert!(control.next_due().is_some());
    }

    #[test]
    fn an_edited_config_matrix_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        // Stored against the default matrix; the config has routes now.
        stored_matrices(&path, Vec::new());
        let right = Route {
            from: 1,
            to: 1,
            gain_db: -6.0,
        };
        let (mut control, engine, now) = restored_matrices(&path, Some(vec![right]));
        let configured = Matrix::from_routes(&[right], 2).unwrap();
        assert!(engine.matrices.is_empty());
        assert_eq!(engine.mixes, [(1, configured)]);
        let want = json!([
            {"from": "players.right", "to": 1, "gain_db": -6.0},
            {"from": "aes67.left", "to": 1, "gain_db": 0.0},
            {"from": "aes67.right", "to": 2, "gain_db": 0.0},
        ]);
        for id in [1, 2] {
            assert_eq!(routes_of(&mut control, id, now), want, "{id}");
        }
        // Stored against routes the config no longer has: the default.
        stored_matrices(&path, vec![stored_route("players.left", 1)]);
        let (mut control, engine, now) = restored_matrices(&path, None);
        assert!(engine.matrices.is_empty());
        assert_eq!(engine.mixes, [(1, Matrix::default_for(2))]);
        assert_eq!(routes_of(&mut control, 1, now), default_routes());
        // A stored matrix that does not fit the width any more is dropped
        // for the config's, while the config is unchanged.
        let mut state = State::default();
        let wide = vec![stored_route("players.left", 3)];
        let output = StoredOutput {
            matrix: Some(Stored {
                value: wide,
                config: Vec::new(),
            }),
            ..StoredOutput::default()
        };
        state.outputs.insert("master".into(), output);
        state.save(&path).unwrap();
        let (mut control, engine, now) = restored_matrices(&path, None);
        assert!(engine.matrices.is_empty());
        assert_eq!(routes_of(&mut control, 1, now), default_routes());
    }

    fn settings(control: &mut Control, patch: SettingsPatch, now: Instant) -> crate::api::Answer {
        control.call(Call::PatchSettings(patch), now)
    }

    fn mode(name: &str) -> SettingsPatch {
        SettingsPatch {
            bit_perfect: Some(name.into()),
            ..SettingsPatch::default()
        }
    }

    fn latency(ms: f32) -> SettingsPatch {
        SettingsPatch {
            latency_ms: Some(ms),
            ..SettingsPatch::default()
        }
    }

    fn default_rate(rate: u32) -> SettingsPatch {
        SettingsPatch {
            default_rate: Some(rate),
            ..SettingsPatch::default()
        }
    }

    /// Open at a latency of `latency_ms`, with nothing queued.
    fn open_at(config: &Config, latency_ms: f32) -> (Control, FakeEngine, Instant) {
        let (mut control, mut engine, now) = started_at(config, None, latency_ms);
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        (control, engine, now)
    }

    #[test]
    fn settings_give_the_configs_values_with_the_fixed_ones() {
        let config = Config {
            default_rate: 44100,
            format: Format::F64,
            ..Config::default()
        };
        let (mut control, _engine, now) = open_at(&config, 10.0);
        let view = control.call(Call::Settings, now).unwrap();
        assert_eq!(
            view,
            json!({"bit_perfect": "auto", "latency_ms": 10.0, "default_rate": 44100,
                   "format": "f64", "resampling": "high"})
        );
    }

    #[test]
    fn a_fixed_setting_is_accepted_as_it_is_and_refused_otherwise() {
        let (mut control, _engine, now) = open_at(&Config::default(), 10.0);
        // The whole of GET /settings can be sent back, edited.
        let mut body = control.call(Call::Settings, now).unwrap();
        body["latency_ms"] = json!(8.0);
        let patch: SettingsPatch = serde_json::from_value(body).unwrap();
        let view = settings(&mut control, patch, now).unwrap();
        assert_eq!(view["latency_ms"], json!(8.0));
        for (key, other) in [("format", "f64"), ("resampling", "fast")] {
            let patch: SettingsPatch = serde_json::from_value(json!({
                "latency_ms": 6.0, key: other
            }))
            .unwrap();
            let e = settings(&mut control, patch, now).unwrap_err();
            assert_eq!(e.status, 422, "{key}");
            assert_eq!(e.message, format!("{key} changes only in the config"));
        }
        // The refused patches changed nothing.
        let view = control.call(Call::Settings, now).unwrap();
        assert_eq!(view["latency_ms"], json!(8.0));
    }

    #[test]
    fn a_mode_goes_to_the_engine_with_the_gain_it_now_takes() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        control.call(Call::PatchOutput(1, volume(50)), now).unwrap();
        engine.take(true);
        engine.gains.clear();
        let view = settings(&mut control, mode("forced"), now).unwrap();
        assert_eq!(view["bit_perfect"], json!("forced"));
        assert_eq!(engine.take(true), ["SetMode Forced", "SetGain"]);
        // A software volume has no effect in forced bit-perfect.
        assert_eq!(engine.gains, [(0, 1.0)]);
        settings(&mut control, mode("auto"), now).unwrap();
        assert_eq!(engine.take(true), ["SetMode Auto", "SetGain"]);
        assert!(close(engine.gains[1].1, db_to_gain(-30.0)));
        // The same mode again sends nothing.
        settings(&mut control, mode("auto"), now).unwrap();
        assert!(engine.take(true).is_empty());
        let answer = settings(&mut control, mode("always"), now);
        assert_eq!(
            message(answer),
            "bit_perfect must be \"auto\", \"forced\" or \"off\""
        );
    }

    #[test]
    fn forced_with_a_chain_that_changes_channels_is_422() {
        let config = Config {
            channels: 4,
            plugins: vec![config::PluginConfig {
                place: config::PluginPlace::Output,
                kind: "widen".into(),
                path: None,
                params: Default::default(),
            }],
            ..Config::default()
        };
        let (mut control, mut engine, now) = open(&config, None);
        let answer = settings(&mut control, mode("forced"), now);
        assert_eq!(status(answer.clone()), 422);
        assert_eq!(
            message(answer),
            "bit_perfect = \"forced\" needs chains that keep the channel count"
        );
        assert!(engine.take(true).is_empty());
        let view = control.call(Call::Settings, now).unwrap();
        assert_eq!(view["bit_perfect"], json!("auto"));
        // Off needs nothing of the chains.
        settings(&mut control, mode("off"), now).unwrap();
        assert_eq!(engine.take(true), ["SetMode Off", "SetGain"]);
    }

    #[test]
    fn a_latency_above_the_startup_one_is_422() {
        let (mut control, mut engine, now) = open_at(&Config::default(), 10.0);
        let answer = settings(&mut control, latency(12.0), now);
        assert_eq!(status(answer.clone()), 422);
        assert_eq!(
            message(answer),
            "latency_ms above the 10 ms the daemon started with needs a restart"
        );
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let answer = settings(&mut control, latency(bad), now);
            assert_eq!(
                message(answer),
                "latency_ms must be a positive number",
                "{bad}"
            );
        }
        assert!(engine.take(true).is_empty());
        let view = settings(&mut control, latency(5.0), now).unwrap();
        assert_eq!(view["latency_ms"], json!(5.0));
        // Back up to the startup one is a change like any.
        assert!(settings(&mut control, latency(10.0), now).is_ok());
    }

    #[test]
    fn a_refused_patch_changes_nothing() {
        let (mut control, mut engine, now) = open_at(&Config::default(), 10.0);
        let patch = SettingsPatch {
            bit_perfect: Some("off".into()),
            latency_ms: Some(12.0),
            default_rate: Some(44100),
            ..SettingsPatch::default()
        };
        assert_eq!(status(settings(&mut control, patch, now)), 422);
        assert!(engine.take(true).is_empty());
        let view = control.call(Call::Settings, now).unwrap();
        assert_eq!(view["bit_perfect"], json!("auto"));
        assert_eq!(view["default_rate"], json!(48000));
    }

    #[test]
    fn a_lower_latency_reopens_with_the_shorter_period() {
        let (mut control, mut engine, now) = open_at(&Config::default(), 10.0);
        play_as(&mut control, 1, "mpd", now);
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        assert_eq!(control.output, Some((48000, period_frames(48000, 10.0))));
        settings(&mut control, latency(5.0), now).unwrap();
        let seen = engine.take(true);
        let reopen = format!("Reconfigure 48000 {}", period_frames(48000, 5.0));
        assert!(has(&seen, &reopen), "{seen:?}");
        assert_eq!(control.latency_ms, 5.0);
        // A ring is still sized for the latency the daemon started with,
        // which the API may set again while it plays.
        let short = min_buffer_frames(48000, 5.0);
        assert!(short < min_buffer_frames(48000, 10.0));
        control.connected(2, "short".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(2, Some(hello), now);
        let answer = control.request(2, Some(configure(48000, short)), now);
        let code = libc::EINVAL as u32;
        assert_eq!(answer.reply, Some(Reply::Error { code }));
        // A latency that keeps the period reopens nothing.
        control.handle_events(now);
        engine.take(true);
        settings(&mut control, latency(4.5), now).unwrap();
        assert!(!has(&engine.take(true), "Reconfigure"));
    }

    #[test]
    fn further_outputs_on_the_masters_latency_follow_it_and_others_keep_theirs() {
        let (mut own, rates) = further(&[]);
        own.latency_ms = Some(12.0);
        let outputs = vec![(own, rates), further(&[])];
        let (mut control, mut engine, logs, now) = started_with(&Config::default(), outputs);
        engine.take(true);
        control.handle_events(now);
        for log in &logs {
            assert_eq!(log.take(), ["reconfigure 48000 48000 256", "open true"]);
        }
        settings(&mut control, latency(6.0), now).unwrap();
        let seen = engine.take(true);
        assert!(has(&seen, "Reconfigure 48000 128"), "{seen:?}");
        control.handle_events(now);
        assert_eq!(
            logs[0].take(),
            ["open false", "reconfigure 48000 48000 256", "open true"]
        );
        assert_eq!(
            logs[1].take(),
            ["open false", "reconfigure 48000 48000 128", "open true"]
        );
    }

    #[test]
    fn a_latency_change_waits_during_dop() {
        let (mut control, mut engine, now) = open_at(&Config::default(), 10.0);
        play_as(&mut control, 1, "mpd", now);
        engine.take(true);
        control.handle_events(now);
        engine.take(true);
        let id = control.streams().next().unwrap().id;
        engine.send(Event::DopStarted(id, 1));
        control.handle_events(now);
        engine.take(true);
        assert_eq!(control.dop_owner, Some(id));
        settings(&mut control, latency(5.0), now).unwrap();
        assert!(!has(&engine.take(true), "Reconfigure"));
        control.tick(now + Duration::from_secs(1));
        assert!(!has(&engine.take(true), "Reconfigure"));
        // When DoP ends, the output reopens with the new period.
        engine.send(Event::DopStopped(id));
        control.handle_events(now + Duration::from_secs(1));
        let seen = engine.take(true);
        let reopen = format!("Reconfigure 48000 {}", period_frames(48000, 5.0));
        assert!(has(&seen, &reopen), "{seen:?}");
    }

    #[test]
    fn a_default_rate_the_config_refuses_is_422() {
        let (mut control, mut engine, now) = open(&Config::default(), None);
        for bad in [0, 7999, 384001] {
            let answer = settings(&mut control, default_rate(bad), now);
            assert_eq!(status(answer.clone()), 422, "{bad}");
            assert_eq!(message(answer), "default_rate must be from 8000 to 384000");
        }
        assert!(engine.take(true).is_empty());
        // With no client, the output moves to it, after the settle time
        // as any change of rate with nothing playing.
        settings(&mut control, default_rate(44100), now).unwrap();
        control.tick(now + Duration::from_secs(1));
        assert!(has(&engine.take(true), "Reconfigure 44100"));
    }

    #[test]
    fn settings_are_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let (mut control, _engine, now) = open_at(&Config::default(), 10.0);
        let output_volume = OutputVolume::new(None, config::BitPerfect::Auto);
        control.restore(output_volume, path.clone(), &[]);
        let patch = SettingsPatch {
            bit_perfect: Some("off".into()),
            latency_ms: Some(5.0),
            default_rate: Some(44100),
            ..SettingsPatch::default()
        };
        settings(&mut control, patch, now).unwrap();
        assert_eq!(control.save_at, None);
        // A change that is stored saves the file: it has no settings.
        control.call(Call::PatchOutput(1, volume(40)), now).unwrap();
        control.shutdown();
        let text = std::fs::read_to_string(&path).unwrap();
        for key in ["bit_perfect", "latency", "default_rate"] {
            assert!(!text.contains(key), "{key} in {text}");
        }
    }

    #[test]
    fn a_change_publishes_a_settings_event() {
        let (mut control, _engine, now) = open_at(&Config::default(), 10.0);
        let mut published = subscribe(&mut control);
        settings(&mut control, latency(5.0), now).unwrap();
        let seen = events(&mut published);
        let event = ApiEvent::Settings {
            bit_perfect: "auto".into(),
            latency_ms: 5.0,
            default_rate: 48000,
        };
        assert!(seen.contains(&event), "{seen:?}");
        // No change, no event.
        settings(&mut control, latency(5.0), now).unwrap();
        let seen = events(&mut published);
        assert!(!seen.iter().any(|e| matches!(e, ApiEvent::Settings { .. })));
        let text = serde_json::to_string(&event).unwrap();
        assert_eq!(
            text,
            r#"{"type":"settings","bit_perfect":"auto","latency_ms":5.0,"default_rate":48000}"#
        );
    }

    #[test]
    fn identify_names_a_connection_from_its_pid() {
        let (mut control, _engine, _logs, now) =
            started_with(&Config::default(), Vec::new());
        control.connected_as(7, "aas".into(), "aas (7)".into());
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        control.request(7, Some(hello), now);
        // A connection from another process may not name itself.
        let answer = control.request(
            7,
            Some(Request::Identify {
                pid: std::process::id(),
            }),
            now,
        );
        assert_eq!(answer.reply, Some(Reply::Error { code: libc::EPERM as u32 }));
        control.allow_identify(7);
        let hello = Request::Hello {
            version: proto::VERSION,
        };
        assert_eq!(
            control.request(7, Some(hello), now).reply,
            Some(Reply::Hello {
                version: proto::VERSION
            })
        );
        let answer = control.request(
            7,
            Some(Request::Identify {
                pid: std::process::id(),
            }),
            now,
        );
        assert_eq!(answer.reply, Some(Reply::Ok));
        let comm = std::fs::read_to_string("/proc/self/comm")
            .unwrap()
            .trim_end()
            .to_string();
        assert_eq!(control.conns[&7].name, comm);
        // Naming a connection that already plays is refused.
        control.connected_as(8, "mpd".into(), "mpd (8)".into());
        control.request(
            8,
            Some(Request::Hello {
                version: proto::VERSION,
            }),
            now,
        );
        play_as(&mut control, 8, "mpd", now);
        // play_as connects afresh, so the permission is given after it.
        control.allow_identify(8);
        let answer = control.request(
            8,
            Some(Request::Identify {
                pid: std::process::id(),
            }),
            now,
        );
        assert_eq!(answer.reply, Some(Reply::Error { code: libc::EINVAL as u32 }));
    }

    #[test]
    fn a_client_volume_change_is_sent_to_its_connection() {
        let (mut control, _engine, _logs, now) =
            started_with(&Config::default(), Vec::new());
        control.connected_as(3, "mpv".into(), "mpv (3)".into());
        control.request(
            3,
            Some(Request::Hello {
                version: proto::VERSION,
            }),
            now,
        );
        let answer = control.request(
            3,
            Some(Request::SetClientVolume {
                db_millis: -6000,
                muted: false,
            }),
            now,
        );
        assert_eq!(answer.reply, Some(Reply::Ok));
        let notice = Reply::ClientVolumeChanged {
            db_millis: -6000,
            muted: false,
        };
        assert!(control.take_notices().iter().any(|(c, r)| *c == 3 && *r == notice));
        let answer = control.request(3, Some(Request::GetClientVolume), now);
        assert_eq!(
            answer.reply,
            Some(Reply::ClientVolume {
                db_millis: -6000,
                muted: false
            })
        );
        // Out of range is refused, and the stored value is unchanged.
        let answer = control.request(
            3,
            Some(Request::SetClientVolume {
                db_millis: -60001,
                muted: false,
            }),
            now,
        );
        assert_eq!(answer.reply, Some(Reply::Error { code: libc::EINVAL as u32 }));
        let answer = control.request(3, Some(Request::GetClientVolume), now);
        assert_eq!(
            answer.reply,
            Some(Reply::ClientVolume {
                db_millis: -6000,
                muted: false
            })
        );
    }

    #[test]
    fn the_output_rate_is_reported() {
        let (mut control, _engine, _logs, now) =
            started_with(&Config::default(), Vec::new());
        control.connected_as(4, "mpv".into(), "mpv (4)".into());
        control.request(
            4,
            Some(Request::Hello {
                version: proto::VERSION,
            }),
            now,
        );
        let answer = control.request(4, Some(Request::GetOutputRate), now);
        assert!(matches!(answer.reply, Some(Reply::OutputRate { .. })));
    }
}
