//! What the API thread and the control thread say to each other: the calls,
//! their answers, the bodies they take and the events the control thread
//! publishes. The API thread decides nothing; each call goes to the control
//! thread, which answers with the view it builds from what it holds.

use serde::{Deserialize, Serialize};

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
    /// The master only: the `Master` control, 0 to 100.
    pub volume: Option<u8>,
    /// A further output only: its offset, -60 to 0 dB.
    pub volume_db: Option<f32>,
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
    /// An output's settings or its state: `"open"`, `"parked"` or `"off"`.
    Output {
        id: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        volume: Option<u8>,
        #[serde(skip_serializing_if = "Option::is_none")]
        volume_db: Option<f32>,
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
            idle: self.idle.clone(),
            idle_delay_s: self.idle_delay_s,
            state: self.state.clone(),
        }
    }
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

/// The answer to a call whose view serialises, which every view does.
pub fn answer<T: Serialize>(view: &T) -> Answer {
    serde_json::to_value(view).map_err(|e| ApiError::unavailable(e.to_string()))
}
