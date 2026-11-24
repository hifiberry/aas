//! What the API thread and the control thread say to each other: the calls,
//! their answers, the bodies they take and the events the control thread
//! publishes. The API thread decides nothing; each call goes to the control
//! thread, which answers with the view it builds from what it holds.

use crate::engine::Place;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One call of the API, as the control thread handles it.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    /// `GET /state`.
    State,
    /// `GET /outputs`.
    Outputs,
    /// `GET /outputs/{id}`, the master being 1.
    Output(usize),
    PatchOutput(usize, OutputPatch),
    /// `GET /inputs`: the analog input and every client known by name.
    Inputs,
    Analog,
    PatchAnalog(InputPatch),
    /// `GET /inputs/clients/{name}`.
    Client(String),
    PatchClient(String, InputPatch),
    Matrix,
    PutMatrix(MatrixPut),
    /// `GET /metrics`.
    Metrics,
    /// `GET /graph`.
    Graph,
    /// `GET /plugins`.
    Plugins,
    Plugin(PluginId),
    PatchPlugin(PluginId, PluginPatch),
    /// `GET /settings`.
    Settings,
    /// The AirPlay receivers and their state.
    Airplay,
    /// `GET /metadata`: the current track.
    Metadata,
    /// `POST /airplay/{name}/pair`: the start of pairing, or with the PIN
    /// its finish.
    PairAirplay {
        name: String,
        pin: Option<String>,
    },
    /// `PATCH /airplay/{name}`.
    PatchAirplay(String, AirplayPatch),
    /// `GET /aes67`.
    Aes67,
    /// `GET /aes67/streams`.
    Aes67Streams,
    /// `PATCH /aes67/input`.
    PatchAes67Input(Aes67InputPatch),
    /// `PATCH /aes67/output`.
    PatchAes67Output(Aes67OutputPatch),
    PatchSettings(SettingsPatch),
}

/// The body of `PATCH /settings`. A field left out stays as it is. A
/// change lasts until the next restart.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    /// `"auto"`, `"forced"` or `"off"`.
    pub bit_perfect: Option<String>,
    pub latency_ms: Option<f32>,
    pub default_rate: Option<u32>,
    /// The settings of the config that cannot change while the daemon
    /// runs. Naming one is accepted when it is the value it has, so that
    /// the whole of `GET /settings` can be sent back, and 422 when it is
    /// another.
    pub format: Option<String>,
    pub resampling: Option<String>,
    /// The board's figures, read-only like the two above: accepted when
    /// they are what they are, 422 when they are not.
    pub latency_recommended_ms: Option<f32>,
    pub latency_min_ms: Option<f32>,
}

/// `GET /settings`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SettingsView {
    pub bit_perfect: String,
    pub latency_ms: f32,
    pub default_rate: u32,
    /// The processing format, which is the engine's type.
    pub format: String,
    pub resampling: String,
    /// The board's default latency, and the lowest it holds safely.
    pub latency_recommended_ms: f32,
    pub latency_min_ms: f32,
}

impl SettingsView {
    pub fn event(&self) -> ApiEvent {
        ApiEvent::Settings {
            bit_perfect: self.bit_perfect.clone(),
            latency_ms: self.latency_ms,
            default_rate: self.default_rate,
        }
    }
}

/// A plugin: its output, numbered as in `/outputs`, its place, and its
/// position in that chain from 0. Written `1/mix/0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginId {
    pub output: usize,
    pub place: Place,
    pub index: usize,
}

impl PluginId {
    /// The id from the three segments of its path. One that cannot name a
    /// plugin is 404, as an unknown one is.
    pub fn parse(output: &str, place: &str, index: &str) -> Result<PluginId, ApiError> {
        let none = || ApiError::not_found(format!("no plugin {output}/{place}/{index}"));
        let number = |s: &str| {
            s.parse::<usize>()
                .ok()
                .filter(|_| s.bytes().all(|b| b.is_ascii_digit()))
        };
        let output_n = number(output).filter(|&n| n >= 1).ok_or_else(none)?;
        let place_v = match place {
            "analog" => Place::Analog,
            "mix" => Place::Mix,
            "output" => Place::Output,
            _ => return Err(none()),
        };
        let index_n = number(index).ok_or_else(none)?;
        Ok(PluginId {
            output: output_n,
            place: place_v,
            index: index_n,
        })
    }

    /// Its place as the path and the state file write it.
    pub fn place_name(&self) -> &'static str {
        match self.place {
            Place::Analog => "analog",
            Place::Mix => "mix",
            Place::Output => "output",
        }
    }
}

impl std::fmt::Display for PluginId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.output, self.place_name(), self.index)
    }
}

/// The body of `PATCH /plugins/{output}/{place}/{index}`. A field left out
/// stays as it is.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPatch {
    /// Values by parameter name.
    pub params: Option<BTreeMap<String, f64>>,
    pub enabled: Option<bool>,
}

/// A plugin as `GET /plugins` gives it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginView {
    /// `1/mix/0`.
    pub id: String,
    pub output: usize,
    pub place: String,
    pub index: usize,
    /// `detect`, `balance` or `library`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The library's path; None for a built-in.
    pub path: Option<String>,
    /// The name the library's descriptor gives, or the built-in's.
    pub name: String,
    pub enabled: bool,
    /// Whether it leaves the audio as it is now, a disabled one too.
    pub neutral: bool,
    /// 0 while it is disabled.
    pub latency_frames: u32,
    pub params: Vec<ParamView>,
}

/// One parameter of a plugin.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParamView {
    pub name: String,
    pub unit: String,
    /// `float`, `integer`, `switch`, `enumeration` or `text`.
    #[serde(rename = "type")]
    pub kind: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    /// The names of an enumeration's values, from 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
    /// A number, or a text parameter's config text; null for a text the
    /// config does not give.
    pub value: serde_json::Value,
}

/// A call and where its answer goes.
#[derive(Debug)]
pub struct ApiCall {
    pub call: Call,
    pub answer: tokio::sync::oneshot::Sender<Answer>,
}

/// The body of a successful call, or why it failed.
pub type Answer = Result<serde_json::Value, ApiError>;

/// A failed call: its HTTP status, 400, 404, 422 or 503, and the message
/// the config would give, sent as `{"error": message}`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    /// A body that does not parse, or has an unknown field.
    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError {
            status: 400,
            message: message.into(),
        }
    }

    /// An unknown output or input.
    pub fn not_found(message: impl Into<String>) -> ApiError {
        ApiError {
            status: 404,
            message: message.into(),
        }
    }

    /// A value out of range, or one that breaks a rule of the config.
    pub fn unprocessable(message: impl Into<String>) -> ApiError {
        ApiError {
            status: 422,
            message: message.into(),
        }
    }

    /// A fault of the daemon's own, such as a view that does not
    /// serialise.
    pub fn internal(message: impl Into<String>) -> ApiError {
        ApiError {
            status: 500,
            message: message.into(),
        }
    }

    /// The control thread has not answered in time.
    pub fn unavailable(message: impl Into<String>) -> ApiError {
        ApiError {
            status: 503,
            message: message.into(),
        }
    }
}

/// The body of `PATCH /outputs/{id}`. A field left out stays as it is.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputPatch {
    /// The master only: the `Master` control, 0 to 100. Taken wide, so
    /// that a value out of that range is refused as one, not as a value
    /// of the wrong type.
    pub volume: Option<i64>,
    /// A further output only: its offset, -60 to 0 dB.
    pub volume_db: Option<f32>,
    /// The master only: its volume limit, -60 to 0 dB, 0 for none.
    pub limit_db: Option<f32>,
    /// `"zeros"` or `"off"`.
    pub idle: Option<String>,
    /// Only with `idle` off.
    pub idle_delay_s: Option<u32>,
}

/// The body of `PATCH /inputs/...`. A field left out stays as it is.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputPatch {
    /// -60 to 0.
    pub volume_db: Option<f32>,
    pub muted: Option<bool>,
}

/// The body of `PUT /matrix`, and what `GET /matrix` gives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixPut {
    pub outputs: Vec<OutputRoutes>,
}

/// One output's routes in a `PUT /matrix`. An empty list is the default
/// matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRoutes {
    pub output: usize,
    pub routes: Vec<RouteJson>,
}

/// A route as the API writes it, with the names and rules of a
/// `[[matrix]]` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteJson {
    pub from: String,
    pub to: i64,
    pub gain_db: Option<f64>,
}

/// One change, as WebSocket `/events` sends it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ApiEvent {
    /// An AirPlay receiver's state changed: `"idle"`, `"connecting"`,
    /// `"playing"` or `"retrying"`, with the last failure.
    Airplay {
        name: String,
        state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// An output's settings or its state: `"open"`, `"parked"` or `"off"`.
    Output {
        id: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        volume: Option<u8>,
        #[serde(skip_serializing_if = "Option::is_none")]
        volume_db: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit_db: Option<f32>,
        idle: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        idle_delay_s: Option<u32>,
        state: String,
    },
    /// An input's volume or mute. `id` is `"analog"` or `"clients/<name>"`.
    Input {
        id: String,
        volume_db: f32,
        muted: bool,
    },
    /// A client `"connected"`, `"running"`, `"stopped"` or `"gone"`.
    Client { name: String, change: String },
    /// An output's routes.
    Matrix {
        output: usize,
        routes: Vec<RouteJson>,
    },
    /// The rate the master runs at.
    Rate { rate: u32 },
    /// A plugin's values or whether it is enabled. `id` is `1/mix/0`.
    Plugin {
        id: String,
        enabled: bool,
        params: BTreeMap<String, f64>,
    },
    /// The settings `PATCH /settings` changes, after a change.
    Settings {
        bit_perfect: String,
        latency_ms: f32,
        default_rate: u32,
    },
    /// Something of `GET /aes67` or `GET /aes67/streams` changed: the
    /// clock's lock or grandmaster, the input or output, or the list of
    /// streams. Carries `GET /aes67` as it is now.
    Aes67(Aes67View),
    /// The subscriber fell behind and should read `/state` again.
    Resync,
}

/// An output as `GET /outputs` gives it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputView {
    pub id: usize,
    /// As the config names it; empty for a master card found on its own.
    pub card: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_db: Option<f32>,
    /// The master only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_db: Option<f32>,
    /// The master only, and only as `"capture"`: it runs on its card's
    /// capture clock and plays nowhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<&'static str>,
    pub idle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_delay_s: Option<u32>,
    /// `"open"`, `"parked"` or `"off"`.
    pub state: String,
}

impl OutputView {
    /// The event that tells of this output as it is.
    pub fn event(&self) -> ApiEvent {
        ApiEvent::Output {
            id: self.id,
            volume: self.volume,
            volume_db: self.volume_db,
            limit_db: self.limit_db,
            idle: self.idle.clone(),
            idle_delay_s: self.idle_delay_s,
            state: self.state.clone(),
        }
    }
}

/// An AirPlay receiver as `GET /airplay` gives it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AirplayView {
    pub name: String,
    pub host: String,
    pub port: u16,
    /// `"airplay1"` or `"airplay2"`.
    pub protocol: &'static str,
    /// `"signal"` or `"always"`.
    pub connect: &'static str,
    /// `"idle"`, `"connecting"`, `"playing"` or `"retrying"`.
    pub state: &'static str,
    /// Whether it is played to.
    pub enabled: bool,
    /// Whether an AirPlay 2 receiver is paired; absent for AirPlay 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paired: Option<bool>,
    /// The last failure, until a session plays again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// AES67 as `GET /aes67` gives it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Aes67View {
    pub clock: Aes67ClockView,
    pub input: Aes67InputView,
    pub output: Aes67OutputView,
}

/// The PTP clock the AES67 threads follow.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Aes67ClockView {
    /// `"listen"` or `"off"`, as the config says.
    pub source: &'static str,
    pub domain: u8,
    /// Whether there is PTP time now.
    pub locked: bool,
    /// The grandmaster's clock ID, as `00-11-22-FF-FE-33-44-55`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grandmaster: Option<String>,
    /// How fast PTP time runs against the local clock, in ppm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_ppm: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Aes67InputView {
    pub enabled: bool,
    /// The selected stream's name, also while it is not announced.
    pub stream: Option<String>,
    /// `"ptp"` or `"buffer"` while a stream plays.
    pub mode: Option<&'static str>,
    pub receiving: bool,
    pub latency_ms: u32,
    /// Whether the latency is the board's default.
    pub latency_auto: bool,
    /// While a stream plays, the least latency the board holds now; a
    /// lower `latency_ms` is held at this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_floor_ms: Option<u32>,
    /// Packets that came after their play time.
    pub late: u64,
    /// Frames played as silence because they never came.
    pub lost: u64,
    /// Times a stream that was priming or playing started over.
    pub restarts: u64,
    /// Why it last did: `"selection"`, `"latency"`, `"grandmaster"`,
    /// `"stopped"`, `"resumed"`, `"no stored packets"`, `"delay"` or
    /// `"timestamps"` (a stream in PTP mode that fell back to buffer mode).
    pub last_restart: Option<&'static str>,
    /// In PTP mode, the delay the controller holds at `latency_ms`, by part.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay: Option<Aes67DelayView>,
}

/// The delay of a stream in PTP mode, as the controller last read it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Aes67DelayView {
    pub total_ms: f64,
    /// From a frame's media-clock time to where the jitter buffer reads.
    pub network_ms: f64,
    /// The resampler's filter.
    pub filter_ms: f64,
    /// The ring to the engine.
    pub ring_ms: f64,
    /// The engine's delay after the ring: its card buffer and filters.
    pub engine_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Aes67OutputView {
    pub enabled: bool,
    pub sending: bool,
    /// The multicast group; absent while `"auto"` is not resolved.
    pub group: Option<String>,
    pub port: u16,
    /// While sending, the delay from the master's tap to the network as
    /// the controller last read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<f64>,
    /// Packets sent as zeros as the master's frames ran out, since the
    /// output started.
    pub padded: u64,
}

/// An announced stream as `GET /aes67/streams` gives it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Aes67StreamView {
    pub name: String,
    pub origin: String,
    pub group: String,
    pub port: u16,
    /// `"L16"`, `"L24"` or `"other"`.
    pub format: &'static str,
    pub rate: u32,
    pub channels: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grandmaster: Option<String>,
    /// Whether the input can play it; not our own and of a format it takes.
    pub selectable: bool,
    /// Why it cannot be selected: `"own"`, or the field it fails on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// `"ptp"` or `"buffer"`: how it would play now, when selectable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<&'static str>,
}

/// An input as `GET /inputs` gives it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InputView {
    /// `"analog"` or `"clients/<name>"`.
    pub id: String,
    pub volume_db: f32,
    pub muted: bool,
}

impl InputView {
    pub fn event(&self) -> ApiEvent {
        ApiEvent::Input {
            id: self.id.clone(),
            volume_db: self.volume_db,
            muted: self.muted,
        }
    }
}

/// A connected client as `GET /state` gives it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClientView {
    pub name: String,
    /// The rate of its stream; None before it has configured one.
    pub rate: Option<u32>,
    pub running: bool,
}

/// An output in `GET /state`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputState {
    pub id: usize,
    pub card: String,
    pub state: String,
}

/// `GET /state`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StateView {
    /// The rate the master runs at; None while it is off.
    pub rate: Option<u32>,
    /// The processing format: `"i32"`, `"f32"` or `"f64"`.
    pub format: String,
    pub outputs: Vec<OutputState>,
    pub clients: Vec<ClientView>,
    pub bit_perfect: bool,
    /// Why the master is not bit-perfect; None when it is.
    pub reason: Option<String>,
}

/// `GET /metrics`: values since the daemon started, never reset.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricsView {
    pub master: MasterMetrics,
    pub outputs: Vec<OutputMetrics>,
    pub streams: Vec<StreamMetrics>,
    /// None without an analog input.
    pub analog: Option<AnalogMetrics>,
    pub plugins: Vec<PluginMetrics>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MasterMetrics {
    pub cycles: u64,
    pub cycle_ns_avg: u64,
    pub cycle_ns_worst: u64,
    pub buffer_ns: u64,
    pub xruns: u64,
    pub wake_gaps: u64,
    pub worst_gap_ns: u64,
    pub last_gap: Option<LastGap>,
    /// The period asked for, as the master was last opened.
    pub period_frames: u32,
    /// `PERIODS` periods: the buffer asked for, not what the card granted.
    pub card_frames: u32,
    pub delay_frames: u32,
}

/// The latest wake-up gap longer than the buffer, and when it ended on
/// CLOCK_MONOTONIC.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LastGap {
    pub ns: u64,
    pub at_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputMetrics {
    pub id: usize,
    pub card: String,
    pub xruns: u64,
    pub wake_gaps: u64,
    pub worst_gap_ns: u64,
    pub starves: u64,
    pub overflows: u64,
    pub skipped: u64,
    pub ppm: f64,
    pub locked: bool,
    /// Card frames per master frame; None before the writer has run.
    pub ratio: Option<f64>,
    /// The card's period, as it was asked for.
    pub period_frames: u32,
    /// The card's buffer, `PERIODS` periods: what was asked for, not what
    /// the card granted. None while the output has no configuration.
    pub card_frames: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamMetrics {
    pub name: String,
    pub rate: u32,
    pub resampled: bool,
    pub underruns: u64,
    pub fill_frames: u64,
    pub capacity_frames: usize,
    pub latency_frames: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnalogMetrics {
    pub open: bool,
    pub padded_frames: u64,
    pub dropped_frames: u64,
    pub latency_frames: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginMetrics {
    pub id: String,
    pub runs: u64,
    pub ns_avg: u64,
    pub ns_worst: u64,
}

/// The answer to a call whose view serialises, which every view does.
/// The body of `PATCH /airplay/{name}`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AirplayPatch {
    pub enabled: Option<bool>,
}

/// The body of `PATCH /aes67/input`. A field left out stays as it is.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Aes67InputPatch {
    /// A stream's name; `null` selects none.
    #[serde(default, deserialize_with = "some_or_null")]
    pub stream: Option<Option<String>>,
    /// A number of ms from 1 to 100, or `"auto"`; checked by the control
    /// thread, which names the field when it is neither.
    pub latency_ms: Option<serde_json::Value>,
    pub enabled: Option<bool>,
    /// The client volume named `aes67`, as `PATCH /inputs/clients/aes67`.
    pub volume_db: Option<f32>,
    pub mute: Option<bool>,
}

/// The body of `PATCH /aes67/output`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Aes67OutputPatch {
    pub enabled: Option<bool>,
}

/// A field present as `null` is `Some(None)`, one left out `None`.
fn some_or_null<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

/// `GET /metadata`: the current track, the cover as its type and size.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetadataView {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_s: Option<f64>,
    pub position_s: Option<f64>,
    pub playing: bool,
    pub cover: Option<CoverView>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CoverView {
    pub mime: &'static str,
    pub bytes: usize,
}

pub fn answer<T: Serialize>(view: &T) -> Answer {
    serde_json::to_value(view).map_err(|e| ApiError::internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_view_that_does_not_serialise_is_500() {
        // JSON has no map with keys that are not strings.
        let view: BTreeMap<(u8, u8), u8> = [((1, 2), 3)].into();
        assert_eq!(answer(&view).unwrap_err().status, 500);
    }

    #[test]
    fn plugin_ids_parse_and_print() {
        let id = PluginId::parse("1", "mix", "0").unwrap();
        assert_eq!(
            id,
            PluginId {
                output: 1,
                place: Place::Mix,
                index: 0
            }
        );
        assert_eq!(id.to_string(), "1/mix/0");
        let id = PluginId::parse("4", "analog", "12").unwrap();
        assert_eq!(id.to_string(), "4/analog/12");
        let refused = [
            ("0", "mix", "0"),
            ("1", "nowhere", "0"),
            ("1", "mix", "x"),
            ("1", "mix", "-1"),
            ("+1", "mix", "0"),
            ("", "mix", "0"),
        ];
        for (output, place, index) in refused {
            let e = PluginId::parse(output, place, index).unwrap_err();
            assert_eq!(e.status, 404, "{output}/{place}/{index}");
        }
    }

    #[test]
    fn a_plugin_event_has_its_type() {
        let event = ApiEvent::Plugin {
            id: "1/mix/0".into(),
            enabled: true,
            params: [("gain".to_string(), -6.0)].into(),
        };
        let text = serde_json::to_string(&event).unwrap();
        assert_eq!(
            text,
            r#"{"type":"plugin","id":"1/mix/0","enabled":true,"params":{"gain":-6.0}}"#
        );
    }
}
