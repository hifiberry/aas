//! The control thread's side of the API: each call is handled here, between
//! client requests, exactly like one. Views are built from what `Control`
//! holds; nothing waits on the audio thread.

use super::{ConnId, Control, Stream};
use crate::api::{
    self, ApiError, ApiEvent, Call, ClientView, InputPatch, InputView, MatrixPut, OutputPatch,
    OutputRoutes, OutputState, OutputView, RouteJson, StateView,
};
use crate::bitperfect::NotBitPerfect;
use crate::config::{self, Config, DEFAULT_IDLE_DELAY_S, Format, Idle, Route};
use crate::counters::Counters;
use crate::engine::Command;
use crate::format::db_to_gain;
use crate::matrix::{Bus, INPUT_NAMES, Matrix};
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

/// The shortest time between two matrices sent to one output. A change
/// fades over 10 ms, and a DoP stream that starts meanwhile waits for the
/// fade to end: so it waits for at most one.
const MATRIX_GAP: Duration = Duration::from_millis(20);

/// An output's matrix as the control thread keeps it.
pub(super) struct OutputMatrix {
    /// The width its buffers and chains on the audio thread are built for,
    /// fixed at the start. A matrix of another width is refused.
    width: usize,
    /// The routes as last set; empty is the default matrix.
    routes: Vec<Route>,
    /// The routes of the config, which a stored matrix is checked against.
    config: Vec<StoredRoute>,
    /// A matrix that waits for its turn, and when it is due. A later one
    /// replaces it.
    due: Option<(Instant, Box<Matrix>)>,
    /// When the last one was sent.
    sent_at: Option<Instant>,
    /// The address of the last one sent. The audio thread sends back the
    /// one it replaced, or this one if it refused it, and only the address
    /// tells the two apart: both may hold the same gains.
    sent: Option<usize>,
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
            sent: None,
            refused: false,
        }
    }
}

/// The routes of the default matrix: left to channel 1 and right to
/// channel 2 for both buses, as far as `width` goes.
fn default_routes(width: usize) -> Vec<Route> {
    let sides = Bus::ALL
        .into_iter()
        .flat_map(|bus| bus.inputs().into_iter().enumerate());
    sides
        .filter(|&(side, _)| side < width)
        .map(|(side, from)| Route {
            from,
            to: side + 1,
            gain_db: 0.0,
        })
        .collect()
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
            Call::Matrix => api::answer(&self.matrix_view()),
            Call::PutMatrix(put) => self.put_matrix(put, now),
        };
        self.publish_changes();
        answer
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

    /// The card of the output at engine index `index`, which keys its
    /// state.
    fn card_of(&self, index: usize) -> &str {
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
    fn route_views(&self, index: usize) -> Vec<RouteJson> {
        let slot = &self.matrices[index];
        let default;
        let routes = if slot.routes.is_empty() {
            default = default_routes(slot.width);
            &default
        } else {
            &slot.routes
        };
        routes.iter().map(route_json).collect()
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
        slot.sent = Some(std::ptr::from_ref::<Matrix>(&*matrix).addr());
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
    /// refused, the one it brought. Every put is checked against the
    /// width, so a refusal means the widths kept here are wrong: logged
    /// once.
    pub(super) fn old_matrix(&mut self, output: usize, matrix: &Matrix) {
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
        if slot.sent == Some(std::ptr::from_ref(matrix).addr()) && !slot.refused {
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
                    if let Some((unsent, _)) = &mut self.further[index - 1].unsent {
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
    use crate::api::{
        ApiError, ApiEvent, Call, InputPatch, MatrixPut, OutputPatch, OutputRoutes, RouteJson,
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
        ])
    }

    fn swap_routes() -> serde_json::Value {
        json!([
            {"from": "players.left", "to": 2, "gain_db": 0.0},
            {"from": "players.right", "to": 1, "gain_db": 0.0},
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
        let want = json!([{"from": "players.right", "to": 1, "gain_db": -6.0}]);
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
}
