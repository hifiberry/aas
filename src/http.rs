//! The HTTP API, on a thread `aas-api` of its own with a single-threaded
//! `tokio` runtime. It decides nothing: each request becomes a `Call` for
//! the control thread, sent on a channel with an eventfd that wakes its
//! poll, and the answer comes back on a one-shot channel. WebSocket
//! `/events` forwards what the control thread publishes. WebSocket
//! `/levels` is the exception: it reads the levels the audio thread
//! publishes itself, and never asks the control thread.
//!
//! The paths here are without the `/api/aas/v1` that nginx puts in front.

mod conns;

use crate::api::{ApiCall, ApiError, ApiEvent, Call, PluginId};
use crate::counters::monotonic_ns;
use crate::levels::{self, Levels};
use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::de::DeserializeOwned;
use std::future::{Future, poll_fn};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::pin::pin;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::task::Poll;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::oneshot;

/// How long a request waits for the control thread before it fails.
const ANSWER_WITHIN: Duration = Duration::from_secs(2);

/// The largest request body taken. The daemon locks all of its memory, so
/// what a peer sends must stay small; the largest real body, a matrix of
/// every route, is a few KiB.
const MAX_BODY: usize = 64 * 1024;

/// The largest WebSocket message or frame taken from a subscriber, whose
/// messages are read only to be dropped.
const MAX_PEER_MESSAGE: usize = 4096;

/// The most connections open at once; one more is answered 503.
const MAX_CONNECTIONS: usize = 32;

/// How long a connection may go without a byte from its peer before it is
/// closed, so that the cap cannot be filled by connections left open.
const IDLE: Duration = Duration::from_secs(30);

/// How often a subscriber is sent a ping. Its pong is what keeps a quiet
/// `/events` socket from counting as idle.
const PING_EVERY: Duration = Duration::from_secs(10);

/// How often a `/levels` subscriber is sent a frame.
const LEVELS_EVERY: Duration = Duration::from_millis(100);

/// What every handler shares.
struct Shared {
    calls: Sender<ApiCall>,
    /// Written after each call, to wake the control thread's poll.
    wake: OwnedFd,
    /// Weak, so that the channel closes when the control thread drops its
    /// sender, and the thread then ends.
    events: broadcast::WeakSender<ApiEvent>,
    /// What the audio thread measures, read for `/levels`.
    levels: Arc<Levels>,
}

/// Starts the thread `aas-api`, serving on `listener` until the control
/// thread drops `events`. Calls go out on `calls`, each followed by a
/// write to the eventfd `wake`. `/levels` reads `levels`.
pub fn spawn(
    listener: std::net::TcpListener,
    calls: Sender<ApiCall>,
    wake: OwnedFd,
    events: broadcast::Sender<ApiEvent>,
    levels: Arc<Levels>,
) -> io::Result<JoinHandle<()>> {
    listener.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    // Subscribed here, so that the thread sees the channel close even if
    // the control thread drops its sender before the thread runs.
    let mut ended = events.subscribe();
    let shared = Arc::new(Shared {
        calls,
        wake,
        events: events.downgrade(),
        levels,
    });
    drop(events);
    thread::Builder::new()
        .name("aas-api".into())
        .spawn(move || {
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(e) => {
                        eprintln!("aas: API: {e}, playing on without the API");
                        return;
                    }
                };
                let app = router(shared);
                let listener = conns::Capped::new(listener, MAX_CONNECTIONS, IDLE);
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, app).await {
                        eprintln!("aas: API: {e}");
                    }
                });
                // Every published event passes here too, and is dropped.
                while !matches!(ended.recv().await, Err(RecvError::Closed)) {}
            });
            // Dropping the runtime drops every connection with it, also one
            // whose handler waits on a peer that does not read.
            drop(runtime);
        })
}

fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/state", get(|s| ask(s, Call::State)))
        .route("/airplay", get(|s| ask(s, Call::Airplay)))
        .route("/metadata", get(|s| ask(s, Call::Metadata)))
        .route("/outputs", get(|s| ask(s, Call::Outputs)))
        .route("/outputs/{id}", get(get_output).patch(patch_output))
        .route("/inputs", get(|s| ask(s, Call::Inputs)))
        .route(
            "/inputs/analog",
            get(|s| ask(s, Call::Analog)).patch(patch_analog),
        )
        .route(
            "/inputs/clients/{name}",
            get(get_client).patch(patch_client),
        )
        .route("/graph", get(get_graph))
        .route("/metrics", get(|s| ask(s, Call::Metrics)))
        .route("/matrix", get(|s| ask(s, Call::Matrix)).put(put_matrix))
        .route("/plugins", get(|s| ask(s, Call::Plugins)))
        .route(
            "/plugins/{output}/{place}/{index}",
            get(get_plugin).patch(patch_plugin),
        )
        .route(
            "/settings",
            get(|s| ask(s, Call::Settings)).patch(patch_settings),
        )
        .route("/events", get(events))
        .route("/levels", get(levels))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "no such path") })
        .method_not_allowed_fallback(|| async {
            error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
        })
        .with_state(shared)
}

/// `{"error": message}` with `status`.
fn error(status: StatusCode, message: impl Into<String>) -> Response {
    let body = serde_json::json!({ "error": message.into() });
    (status, axum::Json(body)).into_response()
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        error(status, self.message)
    }
}

/// Sends `call` to the control thread and gives its answer. 503 when the
/// control thread does not answer in time, or has stopped.
async fn ask(s: State<Arc<Shared>>, call: Call) -> Response {
    match fetch(s, call).await {
        Ok(body) => axum::Json(body).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Sends `call` to the control thread and gives its answer, or why there
/// is none.
async fn fetch(
    State(shared): State<Arc<Shared>>,
    call: Call,
) -> Result<serde_json::Value, ApiError> {
    let stopped = || ApiError::unavailable("the control thread has stopped");
    let (answer, answered) = oneshot::channel();
    if shared.calls.send(ApiCall { call, answer }).is_err() {
        return Err(stopped());
    }
    let one = 1u64;
    // SAFETY: a plain write of 8 bytes to a non-blocking eventfd. One that
    // fails because the count is full leaves it readable all the same.
    unsafe {
        libc::write(
            shared.wake.as_raw_fd(),
            (&raw const one).cast(),
            size_of::<u64>(),
        )
    };
    match tokio::time::timeout(ANSWER_WITHIN, answered).await {
        Err(_) => {
            let s = ANSWER_WITHIN.as_secs();
            Err(ApiError::unavailable(format!(
                "no answer from the control thread in {s} s"
            )))
        }
        Ok(Err(_)) => Err(stopped()),
        Ok(Ok(answer)) => answer,
    }
}

/// The value of the query's `format`, if it has one.
fn format_of(uri: &Uri) -> Option<&str> {
    let query = uri.query()?;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("format="))
}

/// `GET /graph`: JSON, or DOT with `?format=dot`. The DOT is made here, from
/// the control thread's JSON, so that it costs the control thread nothing.
async fn get_graph(s: State<Arc<Shared>>, uri: Uri) -> Response {
    let dot = match format_of(&uri) {
        None | Some("json") => false,
        Some("dot") => true,
        Some(other) => {
            let message = format!("format is json or dot, not \"{other}\"");
            return ApiError::bad_request(message).into_response();
        }
    };
    match fetch(s, Call::Graph).await {
        Ok(graph) if dot => {
            let text = crate::control::to_dot(&graph);
            ([(header::CONTENT_TYPE, "text/vnd.graphviz")], text).into_response()
        }
        Ok(graph) => axum::Json(graph).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Asks the control thread for `call`, or answers with why there is none.
async fn ask_for(s: State<Arc<Shared>>, call: Result<Call, ApiError>) -> Response {
    match call {
        Ok(call) => ask(s, call).await,
        Err(e) => e.into_response(),
    }
}

/// A request body, or why it could not be read: 413 for one over
/// `MAX_BODY`.
type BodyOf = Result<Bytes, axum::extract::rejection::BytesRejection>;

/// A JSON body, or a 400 with why not: one that does not parse, has a
/// field of the wrong type or one not known. A body that could not be read
/// gets the status of why, as JSON like every error.
fn parse<T: DeserializeOwned>(body: &BodyOf) -> Result<T, ApiError> {
    let body = match body {
        Ok(body) => body,
        Err(e) => {
            let status = e.status();
            let message = match status {
                StatusCode::PAYLOAD_TOO_LARGE => format!("a body has at most {MAX_BODY} bytes"),
                _ => e.body_text(),
            };
            let status = status.as_u16();
            return Err(ApiError { status, message });
        }
    };
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(e.to_string()))
}

/// An output's id. One that is not a number is not an output either.
fn output_id(id: &str) -> Result<usize, ApiError> {
    id.parse()
        .map_err(|_| ApiError::not_found(format!("no output {id}")))
}

type PathOf<T> = Result<Path<T>, axum::extract::rejection::PathRejection>;

/// A path parameter, or a 400, as for a name that is not UTF-8.
fn path<T>(path: PathOf<T>) -> Result<T, ApiError> {
    match path {
        Ok(Path(value)) => Ok(value),
        Err(e) => Err(ApiError::bad_request(e.body_text())),
    }
}

async fn get_output(s: State<Arc<Shared>>, id: PathOf<String>) -> Response {
    let call = path(id).and_then(|id| Ok(Call::Output(output_id(&id)?)));
    ask_for(s, call).await
}

async fn patch_output(s: State<Arc<Shared>>, id: PathOf<String>, body: BodyOf) -> Response {
    let call = path(id).and_then(|id| {
        let id = output_id(&id)?;
        Ok(Call::PatchOutput(id, parse(&body)?))
    });
    ask_for(s, call).await
}

async fn patch_analog(s: State<Arc<Shared>>, body: BodyOf) -> Response {
    ask_for(s, parse(&body).map(Call::PatchAnalog)).await
}

async fn get_client(s: State<Arc<Shared>>, name: PathOf<String>) -> Response {
    ask_for(s, path(name).map(Call::Client)).await
}

async fn patch_client(s: State<Arc<Shared>>, name: PathOf<String>, body: BodyOf) -> Response {
    let call = path(name).and_then(|name| Ok(Call::PatchClient(name, parse(&body)?)));
    ask_for(s, call).await
}

/// The three segments of a plugin's path.
type PluginPath = PathOf<(String, String, String)>;

fn plugin_id(id: PluginPath) -> Result<PluginId, ApiError> {
    let (output, place, index) = path(id)?;
    PluginId::parse(&output, &place, &index)
}

async fn get_plugin(s: State<Arc<Shared>>, id: PluginPath) -> Response {
    ask_for(s, plugin_id(id).map(Call::Plugin)).await
}

async fn patch_plugin(s: State<Arc<Shared>>, id: PluginPath, body: BodyOf) -> Response {
    let call = plugin_id(id).and_then(|id| Ok(Call::PatchPlugin(id, parse(&body)?)));
    ask_for(s, call).await
}

async fn patch_settings(s: State<Arc<Shared>>, body: BodyOf) -> Response {
    ask_for(s, parse(&body).map(Call::PatchSettings)).await
}

async fn put_matrix(s: State<Arc<Shared>>, body: BodyOf) -> Response {
    ask_for(s, parse(&body).map(Call::PutMatrix)).await
}

/// WebSocket `/events`. Subscribed before the upgrade is answered, so that
/// a change made after the handshake is always sent.
async fn events(
    State(shared): State<Arc<Shared>>,
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(e) => return error(e.status(), e.body_text()),
    };
    let Some(events) = shared.events.upgrade() else {
        return ApiError::unavailable("the control thread has stopped").into_response();
    };
    let receiver = events.subscribe();
    drop(events);
    // A larger message from the peer ends its socket.
    let upgrade = upgrade
        .max_message_size(MAX_PEER_MESSAGE)
        .max_frame_size(MAX_PEER_MESSAGE);
    upgrade.on_upgrade(move |socket| forward(socket, receiver))
}

/// What a subscriber's task waits for: the next event, something from its
/// peer, or the time for a ping.
enum Next {
    Event(Option<ApiEvent>),
    Peer(Option<Result<Message, axum::Error>>),
    Ping,
}

/// The next event for a subscriber: a `resync` if it has fallen behind
/// by more than the channel holds, None once the channel has closed.
/// Cancel-safe, as `recv` is.
async fn next_event(events: &mut broadcast::Receiver<ApiEvent>) -> Option<ApiEvent> {
    match events.recv().await {
        Ok(event) => Some(event),
        Err(RecvError::Lagged(_)) => Some(ApiEvent::Resync),
        Err(RecvError::Closed) => None,
    }
}

/// Sends each event to one subscriber as a JSON text message, until either
/// side closes. A send to a peer that does not read waits in this task
/// alone; the channel goes on without it, and when the task gets to read
/// again it has lagged, which it tells the peer with a `resync`. A ping
/// goes out every `PING_EVERY`; a peer that answers none is closed as idle.
async fn forward(mut socket: WebSocket, mut events: broadcast::Receiver<ApiEvent>) {
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick is at once; the first ping is one period in.
    ping.reset();
    loop {
        // All three are cancel-safe, so those that did not finish lose
        // nothing.
        let next = {
            let mut event = pin!(next_event(&mut events));
            let mut peer = pin!(socket.recv());
            let mut tick = pin!(ping.tick());
            poll_fn(|cx| {
                if let Poll::Ready(e) = event.as_mut().poll(cx) {
                    return Poll::Ready(Next::Event(e));
                }
                if let Poll::Ready(p) = peer.as_mut().poll(cx) {
                    return Poll::Ready(Next::Peer(p));
                }
                tick.as_mut().poll(cx).map(|_| Next::Ping)
            })
            .await
        };
        let event = match next {
            Next::Event(Some(event)) => event,
            Next::Event(None) => return,
            Next::Peer(None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => return,
            Next::Peer(Some(Ok(_))) => continue,
            Next::Ping => {
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    return;
                }
                continue;
            }
        };
        let Ok(text) = serde_json::to_string(&event) else {
            continue;
        };
        if socket.send(Message::Text(text.into())).await.is_err() {
            return;
        }
    }
}

/// WebSocket `/levels`. A frame every 100 ms, read from the atomics the
/// audio thread publishes.
async fn levels(
    State(shared): State<Arc<Shared>>,
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(e) => return error(e.status(), e.body_text()),
    };
    let levels = shared.levels.clone();
    // A larger message from the peer ends its socket.
    let upgrade = upgrade
        .max_message_size(MAX_PEER_MESSAGE)
        .max_frame_size(MAX_PEER_MESSAGE);
    upgrade.on_upgrade(move |socket| send_levels(socket, levels))
}

/// What a `/levels` subscriber's task waits for.
enum LevelsNext {
    Frame,
    Peer(Option<Result<Message, axum::Error>>),
    Ping,
}

/// Sends a frame of levels every 100 ms as a binary message, until either
/// side closes. A peer that does not read holds up this task alone, and
/// the frames due meanwhile are skipped, not sent late. What the peer
/// sends is dropped. A ping goes out every `PING_EVERY`, as on `/events`.
async fn send_levels(mut socket: WebSocket, levels: Arc<Levels>) {
    let mut every = tokio::time::interval(LEVELS_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first ping is one period in.
    ping.reset();
    loop {
        // All three are cancel-safe.
        let next = {
            let mut frame = pin!(every.tick());
            let mut peer = pin!(socket.recv());
            let mut tick = pin!(ping.tick());
            poll_fn(|cx| {
                if let Poll::Ready(p) = peer.as_mut().poll(cx) {
                    return Poll::Ready(LevelsNext::Peer(p));
                }
                if frame.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(LevelsNext::Frame);
                }
                tick.as_mut().poll(cx).map(|_| LevelsNext::Ping)
            })
            .await
        };
        let message = match next {
            LevelsNext::Peer(None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => return,
            LevelsNext::Peer(Some(Ok(_))) => continue,
            LevelsNext::Ping => Message::Ping(Bytes::new()),
            LevelsNext::Frame => {
                let frame = levels::frame(&levels, monotonic_ns());
                Message::Binary(frame.into())
            }
        };
        if socket.send(message).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SettingsPatch;

    fn rate(rate: u32) -> ApiEvent {
        ApiEvent::Rate { rate }
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(f)
    }

    #[test]
    fn a_subscriber_that_falls_behind_is_told_to_resync_and_goes_on() {
        let (events, mut receiver) = broadcast::channel(256);
        for i in 0..300 {
            events.send(rate(i)).unwrap();
        }
        block_on(async {
            assert_eq!(next_event(&mut receiver).await, Some(ApiEvent::Resync));
            // Then the 256 newest, oldest first.
            assert_eq!(next_event(&mut receiver).await, Some(rate(44)));
        });
        drop(events);
        let rest = block_on(async {
            let mut n = 0;
            while next_event(&mut receiver).await.is_some() {
                n += 1;
            }
            n
        });
        assert_eq!(rest, 255);
    }

    /// What the handlers share, with the receiving end of the calls.
    fn shared() -> (State<Arc<Shared>>, std::sync::mpsc::Receiver<ApiCall>) {
        let (calls, received) = std::sync::mpsc::channel();
        let (events, _) = broadcast::channel(1);
        let shared = Shared {
            calls,
            wake: crate::control::eventfd().unwrap(),
            events: events.downgrade(),
            levels: Arc::new(Levels::new([0; levels::METERS])),
        };
        (State(Arc::new(shared)), received)
    }

    async fn status_and_body(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 4096).await;
        (status, serde_json::from_slice(&body.unwrap()).unwrap())
    }

    fn timed_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn a_call_the_control_thread_does_not_answer_in_2_s_is_503() {
        let (state, received) = shared();
        let started = std::time::Instant::now();
        let response = timed_runtime().block_on(ask(state, Call::State));
        assert!(started.elapsed() >= ANSWER_WITHIN);
        let (status, body) = timed_runtime().block_on(status_and_body(response));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["error"].is_string());
        // The call did go out, and the eventfd was written.
        assert!(received.try_recv().is_ok());
    }

    #[test]
    fn a_call_to_a_control_thread_that_has_gone_is_503() {
        let (state, received) = shared();
        drop(received);
        let response = timed_runtime().block_on(ask(state, Call::State));
        let (status, _) = timed_runtime().block_on(status_and_body(response));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn a_setting_body_names_only_known_fields() {
        let body = |text: &str| -> BodyOf { Ok(Bytes::from(text.to_string())) };
        let e = parse::<SettingsPatch>(&body(r#"{"latency": 4}"#)).unwrap_err();
        assert_eq!(e.status, 400);
        let patch: SettingsPatch = parse(&body(r#"{"bit_perfect": "off"}"#)).unwrap();
        assert_eq!(patch.bit_perfect.as_deref(), Some("off"));
    }

    #[test]
    fn the_resync_is_sent_as_its_type_alone() {
        let text = serde_json::to_string(&ApiEvent::Resync).unwrap();
        assert_eq!(text, r#"{"type":"resync"}"#);
    }
}
