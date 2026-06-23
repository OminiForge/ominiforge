//! The axum HTTP/SSE/WebSocket server.
//!
//! Control plane is REST; the live event stream is SSE (`GET …/events`) or
//! WebSocket (`GET …/ws`). All routes except `/healthz` require a bearer token
//! when one is configured (`doc/gateway.md`). TLS is *not* handled here — the
//! gateway binds loopback and a reverse proxy terminates TLS for public exposure
//! (`doc/architecture.md` §18.1).
//!
//! ### Reconnect / resume
//!
//! Every committed event carries its session `seq`. The SSE stream sets each
//! event's `id:` to that seq, so a dropped client reconnects with
//! `Last-Event-ID: <seq>` and the server replays committed events *after* that
//! seq from the log before attaching the live stream — no gap, no duplicate
//! (`doc/monitor.md` §9, the log is the source of truth). Live deltas are
//! ephemeral and intentionally not replayed.

use std::convert::Infallible;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::extract::{
    Path, Request, State,
    ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::core::SessionId;

use super::actor::{ActorHandle, Command, GatewayEvent};
use super::config::GatewayConfig;
use super::registry::SessionRegistry;

/// Shared server state: the session registry and the optional bearer token.
#[derive(Clone)]
struct AppState {
    registry: SessionRegistry,
    /// Resolved bearer token; `None` means the gateway runs unauthenticated.
    api_key: Option<Arc<str>>,
}

/// Run the gateway server until the process is signalled. Binds the configured
/// address (loopback by default), serving `registry`'s sessions.
///
/// # Errors
/// Binding the listener or a fatal serve error.
pub async fn serve(registry: SessionRegistry, config: &GatewayConfig) -> Result<()> {
    let api_key = config.resolve_api_key().map(Arc::from);
    let state = AppState { registry, api_key };

    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;

    axum::serve(listener, app)
        .await
        .context("gateway server error")
}

/// Build the router with auth applied to everything but `/healthz`.
fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/fork", post(fork_session))
        .route("/sessions/{id}/message", post(post_message))
        .route("/sessions/{id}/cancel", post(cancel_turn))
        .route("/sessions/{id}/compact", post(compact_session))
        .route("/sessions/{id}/events", get(sse_events))
        .route("/sessions/{id}/ws", get(ws_events))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state);

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected)
}

/// Bearer-token auth. A no-op when no key is configured; otherwise rejects any
/// request lacking `Authorization: Bearer <token>`.
async fn auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.api_key.as_deref() else {
        return next.run(req).await; // open gateway (loopback + trusted proxy)
    };

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match presented {
        Some(token) if token == expected => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid bearer token" })),
        )
            .into_response(),
    }
}

/// `GET /sessions` — list session ids, newest first.
async fn list_sessions(State(state): State<AppState>) -> Response {
    match state.registry.list() {
        Ok(ids) => {
            let ids: Vec<&str> = ids.iter().map(|s| s.0.as_str()).collect();
            Json(json!({ "sessions": ids })).into_response()
        }
        Err(e) => internal_error(&e),
    }
}

/// `GET /sessions/{id}` — session metadata.
async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let sid = SessionId(id);
    match state.registry.meta(&sid) {
        Ok(meta) => Json(meta).into_response(),
        Err(e) => not_found(&e),
    }
}

/// `POST /sessions` — create a new session; returns its id.
async fn create_session(State(state): State<AppState>) -> Response {
    match state.registry.create().await {
        Ok((id, _handle)) => (
            StatusCode::CREATED,
            Json(json!({ "session_id": id.0 })),
        )
            .into_response(),
        Err(e) => internal_error(&e),
    }
}

/// Body of a fork request.
#[derive(Debug, Deserialize)]
struct ForkBody {
    /// Parent seq to branch at.
    at_seq: u64,
}

/// `POST /sessions/{id}/fork` — branch a new session at `at_seq`.
async fn fork_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ForkBody>,
) -> Response {
    let parent = SessionId(id);
    match state.registry.fork(&parent, body.at_seq).await {
        Ok((new_id, _handle)) => (
            StatusCode::CREATED,
            Json(json!({ "session_id": new_id.0 })),
        )
            .into_response(),
        Err(e) => internal_error(&e),
    }
}

/// Body of a message request.
#[derive(Debug, Deserialize)]
struct MessageBody {
    /// The user input to send to the agent.
    text: String,
}

/// `POST /sessions/{id}/message` — enqueue a turn. Returns `202 Accepted`
/// immediately; the turn runs in the actor and its output streams over the
/// event channel.
async fn post_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<MessageBody>,
) -> Response {
    let sid = SessionId(id);
    let handle = match state.registry.get_or_spawn(&sid).await {
        Ok(h) => h,
        Err(e) => return conflict_or_not_found(&e),
    };
    match handle.send(Command::Send { text: body.text }).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(_) => internal_error(&anyhow::anyhow!("session actor is unavailable")),
    }
}

/// `POST /sessions/{id}/cancel` — abort the running turn, if any.
async fn cancel_turn(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let sid = SessionId(id);
    match state.registry.get_or_spawn(&sid).await {
        Ok(handle) => match handle.send(Command::Cancel).await {
            Ok(()) => StatusCode::ACCEPTED.into_response(),
            Err(_) => internal_error(&anyhow::anyhow!("session actor is unavailable")),
        },
        Err(e) => conflict_or_not_found(&e),
    }
}

/// Body of a compact request.
#[derive(Debug, Default, Deserialize)]
struct CompactBody {
    /// Keep the last N user turns verbatim; `None` summarizes everything.
    #[serde(default)]
    keep_last: Option<usize>,
}

/// `POST /sessions/{id}/compact` — summarize and switch to a compaction session.
async fn compact_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<CompactBody>>,
) -> Response {
    let sid = SessionId(id);
    let keep_last = body.and_then(|Json(b)| b.keep_last);
    match state.registry.get_or_spawn(&sid).await {
        Ok(handle) => match handle.send(Command::Compact { keep_last }).await {
            Ok(()) => StatusCode::ACCEPTED.into_response(),
            Err(_) => internal_error(&anyhow::anyhow!("session actor is unavailable")),
        },
        Err(e) => conflict_or_not_found(&e),
    }
}

/// `GET /sessions/{id}/events` — SSE stream. Replays committed events after
/// `Last-Event-ID` from the log, then attaches the live stream.
async fn sse_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let sid = SessionId(id);
    let handle = match state.registry.get_or_spawn(&sid).await {
        Ok(h) => h,
        Err(e) => return conflict_or_not_found(&e),
    };

    // Parse Last-Event-ID (the seq the client last saw). Replay everything after
    // it from the durable log before attaching the live broadcast.
    let last_seen: Option<u64> = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    let replay = replay_events(&state.registry, &sid, last_seen);
    let live = live_event_stream(handle.subscribe());

    let stream = tokio_stream::iter(replay).chain(live);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Build the replay portion of an SSE stream: committed events strictly after
/// `last_seen`, read from the log. Empty if the session is unreadable (the live
/// stream still attaches).
fn replay_events(
    registry: &SessionRegistry,
    sid: &SessionId,
    last_seen: Option<u64>,
) -> Vec<Result<SseEvent, Infallible>> {
    let events = registry.store().read_events(sid).unwrap_or_default();
    events
        .into_iter()
        .filter(|e| last_seen.is_none_or(|seen| e.seq > seen))
        .map(|event| {
            let gw = GatewayEvent::Event {
                seq: event.seq,
                event: Box::new(event),
            };
            Ok(sse_from_gateway(&gw))
        })
        .collect()
}

/// Adapt a session's outbound broadcast into an SSE event stream, dropping
/// `Lagged` gaps (the client resyncs committed events from the log on reconnect).
fn live_event_stream(
    rx: broadcast::Receiver<GatewayEvent>,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    BroadcastStream::new(rx).filter_map(|res| res.ok().map(|gw| Ok(sse_from_gateway(&gw))))
}

/// Serialize a [`GatewayEvent`] as an SSE event, stamping committed events with
/// their seq as the SSE `id` (the `Last-Event-ID` resume cursor).
fn sse_from_gateway(gw: &GatewayEvent) -> SseEvent {
    let data = serde_json::to_string(gw).unwrap_or_else(|_| "{}".to_owned());
    let event = SseEvent::default().data(data);
    if let GatewayEvent::Event { seq, .. } = gw {
        event.id(seq.to_string())
    } else {
        event
    }
}

/// `GET /sessions/{id}/ws` — bidirectional WebSocket: events stream out, and a
/// client text frame `{"text": "..."}` enqueues a turn.
async fn ws_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let sid = SessionId(id);
    let handle = match state.registry.get_or_spawn(&sid).await {
        Ok(h) => h,
        Err(e) => return conflict_or_not_found(&e),
    };
    ws.on_upgrade(move |socket| ws_loop(socket, handle))
}

/// What a WebSocket client may send.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsClientMessage {
    /// Enqueue a turn.
    Send { text: String },
    /// Abort the running turn.
    Cancel,
}

/// Drive one WebSocket connection: forward outbound events to the client and
/// translate inbound frames into actor commands. Ends when either side closes.
async fn ws_loop(socket: WebSocket, handle: ActorHandle) {
    use futures_util::{SinkExt, StreamExt as _};

    let mut rx = handle.subscribe();
    let (mut sink, mut stream) = socket.split();

    loop {
        tokio::select! {
            // Outbound: a session event → JSON text frame.
            event = rx.recv() => match event {
                Ok(gw) => {
                    let text = serde_json::to_string(&gw).unwrap_or_else(|_| "{}".to_owned());
                    if sink.send(WsMessage::Text(text.into())).await.is_err() {
                        return; // client gone
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {} // skip gap
                Err(broadcast::error::RecvError::Closed) => return, // actor stopped
            },
            // Inbound: a client frame → actor command.
            msg = futures_util::StreamExt::next(&mut stream) => match msg {
                Some(Ok(WsMessage::Text(text))) => {
                    if let Ok(cmd) = serde_json::from_str::<WsClientMessage>(&text) {
                        let command = match cmd {
                            WsClientMessage::Send { text } => Command::Send { text },
                            WsClientMessage::Cancel => Command::Cancel,
                        };
                        if handle.send(command).await.is_err() {
                            return;
                        }
                    }
                }
                // Close, end-of-stream, or a transport error all end the loop.
                Some(Ok(WsMessage::Close(_)) | Err(_)) | None => return,
                // Other frame kinds (binary/ping/pong) are ignored.
                Some(Ok(_)) => {}
            },
        }
    }
}

/// Map a registry error to 404 (not found) or 409 (locked) heuristically. The
/// registry surfaces a "locked or missing" context for `open` failures; a clean
/// `NotFound` from metadata reads is a 404.
fn conflict_or_not_found(e: &anyhow::Error) -> Response {
    let msg = e.to_string();
    if msg.contains("locked") {
        (
            StatusCode::CONFLICT,
            Json(json!({ "error": msg })),
        )
            .into_response()
    } else {
        not_found(e)
    }
}

fn not_found(e: &anyhow::Error) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": e.to_string() })),
    )
        .into_response()
}

fn internal_error(e: &anyhow::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": e.to_string() })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::gateway::{GatewayConfig, SessionDefaults};

    /// Build a registry over an empty temp workspace (no provider config needed
    /// for the routes these tests hit: `/healthz` and `/sessions` list only read
    /// the store directory). Returns the registry and the temp dir to keep alive.
    fn test_registry() -> (SessionRegistry, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let defaults = SessionDefaults {
            workspace: dir.path().to_owned(),
            profile: "default".to_owned(),
            no_dotenv: true,
        };
        let registry = SessionRegistry::new(defaults, &GatewayConfig::default());
        (registry, dir)
    }

    /// Bind the router on an ephemeral loopback port, serve it on a background
    /// task, and return the base URL. The task is detached; the test process
    /// exiting tears it down.
    async fn serve_test(state: AppState) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// `/healthz` is always open, even when auth is configured.
    #[tokio::test]
    async fn healthz_is_open_without_auth() {
        let (registry, _dir) = test_registry();
        let state = AppState {
            registry,
            api_key: Some(Arc::from("secret")),
        };
        let base = serve_test(state).await;
        let resp = reqwest::get(format!("{base}/healthz")).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    /// With a key configured, a protected route rejects a request that lacks the
    /// bearer token (401) and accepts one that presents it (200).
    #[tokio::test]
    async fn protected_route_requires_bearer_token() {
        let (registry, _dir) = test_registry();
        let state = AppState {
            registry,
            api_key: Some(Arc::from("s3cret")),
        };
        let base = serve_test(state).await;
        let client = reqwest::Client::new();

        // No token → 401.
        let resp = client.get(format!("{base}/sessions")).send().await.unwrap();
        assert_eq!(resp.status(), 401);

        // Wrong token → 401.
        let resp = client
            .get(format!("{base}/sessions"))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // Correct token → 200 (empty session list).
        let resp = client
            .get(format!("{base}/sessions"))
            .bearer_auth("s3cret")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    /// With no key configured, protected routes are reachable without a token
    /// (open gateway — only safe behind loopback + trusted proxy).
    #[tokio::test]
    async fn open_gateway_allows_unauthenticated() {
        let (registry, _dir) = test_registry();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let resp = reqwest::get(format!("{base}/sessions")).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    /// `replay_events` includes only events strictly after `Last-Event-ID`, so a
    /// reconnecting client resumes without duplicating what it already saw and
    /// without skipping anything. This is the SSE resume boundary.
    #[test]
    fn replay_filters_strictly_after_last_seen() {
        let dir = tempfile::tempdir().unwrap();
        let defaults = SessionDefaults {
            workspace: dir.path().to_owned(),
            profile: "default".to_owned(),
            no_dotenv: true,
        };
        let registry = SessionRegistry::new(defaults, &GatewayConfig::default());

        // Create a session with a few events (Created = seq 0, plus appends).
        let store = registry.store();
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        for _ in 0..3 {
            writer
                .append(
                    crate::core::EventSource {
                        kind: crate::core::SourceKind::Runtime,
                        id: "test".to_owned(),
                    },
                    crate::core::EventPayload::Session(
                        crate::core::payload::SessionEvent::Paused,
                    ),
                    None,
                    None,
                )
                .unwrap();
        }
        drop(writer); // release the lock so read_events works cleanly

        // Last seen seq 1 → replay should yield only seqs 2 and 3.
        let replay = replay_events(&registry, &sid, Some(1));
        assert_eq!(replay.len(), 2, "events 2 and 3 are after seq 1");

        // No Last-Event-ID → replay everything (4 events: seqs 0..=3).
        let all = replay_events(&registry, &sid, None);
        assert_eq!(all.len(), 4);
    }
}
