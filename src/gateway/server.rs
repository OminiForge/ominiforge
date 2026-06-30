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
    Path, Query, Request, State,
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

use crate::config::ConfigError;
use crate::core::SessionId;
use crate::monitor::{self, PricingTable};

use super::actor::{ActorHandle, Command, GatewayEvent};
use super::config::GatewayConfig;
use super::registry::SessionRegistry;

/// Shared server state: the session registry and the optional bearer token.
#[derive(Clone)]
struct AppState {
    registry: SessionRegistry,
    /// Resolved bearer token; `None` means the gateway runs unauthenticated.
    api_key: Option<Arc<str>>,
    /// Pricing table for deriving session cost on the summary endpoint. Empty
    /// means cost is reported as unpriced.
    pricing: Arc<PricingTable>,
}

/// Run the gateway server until the process is signalled. Binds the configured
/// address (loopback by default), serving `registry`'s sessions.
///
/// # Errors
/// Binding the listener or a fatal serve error.
pub async fn serve(
    registry: SessionRegistry,
    config: &GatewayConfig,
    pricing: PricingTable,
) -> Result<()> {
    let api_key = config.resolve_api_key().map(Arc::from);
    let state = AppState {
        registry,
        api_key,
        pricing: Arc::new(pricing),
    };

    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;

    axum::serve(listener, app)
        .await
        .context("gateway server error")
}

/// Build the router with auth applied to everything but `/healthz`.
///
/// The session API is nested under `/api/*` so it never collides with the
/// SPA's own client-side routes (which share names like `/sessions`) when the
/// gateway serves the static frontend from the same origin (`doc/gateway.md`).
fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/fork", post(fork_session))
        .route("/sessions/{id}/reconfigure", post(reconfigure_session))
        .route("/sessions/{id}/message", post(post_message))
        .route("/sessions/{id}/cancel", post(cancel_turn))
        .route("/sessions/{id}/compact", post(compact_session))
        .route("/sessions/{id}/summary", get(session_summary))
        .route("/sessions/{id}/runtime", get(session_runtime))
        .route("/sessions/{id}/events", get(sse_events))
        .route("/sessions/{id}/ws", get(ws_events))
        .route("/profiles", get(list_profiles))
        .route("/models", get(list_models))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state);

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api", protected)
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

/// Optional per-session overrides for [`create_session`], carried as query
/// params (`?profile=&model=&workspace=`). Query — not a JSON body — so the
/// existing no-arg `POST /sessions` (no body, no content-type) keeps working: an
/// absent query string parses to all-`None`.
#[derive(Debug, Default, Deserialize)]
struct CreateParams {
    /// Profile name to bind; gateway default when absent.
    profile: Option<String>,
    /// Model override (`provider/model_id` or bare `model_id`); profile default
    /// when absent.
    model: Option<String>,
    /// Workspace path; gateway default when absent.
    workspace: Option<String>,
}

/// `POST /sessions` — create a new session; returns its id. Optional
/// `?profile=&model=&workspace=` choose a per-session profile / model override /
/// workspace (not persisted to config). A bad override (unknown model/profile,
/// missing workspace) is a client error → 400, not 500.
async fn create_session(
    State(state): State<AppState>,
    Query(params): Query<CreateParams>,
) -> Response {
    let result = state
        .registry
        .create_with(
            params.profile.as_deref(),
            params.model.as_deref(),
            params.workspace.map(std::path::PathBuf::from),
        )
        .await;
    match result {
        Ok((id, _handle)) => {
            (StatusCode::CREATED, Json(json!({ "session_id": id.0 }))).into_response()
        }
        Err(e) => create_error(&e),
    }
}

/// Map a `create_with` failure to a status. A user-chosen bad override (unknown
/// model/provider, no model, a workspace that does not exist) is a client error
/// (400); anything else (provider build, MCP, io) is a server error (500).
///
/// The config error is usually wrapped by an `anyhow` context (e.g. "failed to
/// resolve model selection"), so we walk the whole source chain rather than only
/// inspecting the outermost error.
fn create_error(e: &anyhow::Error) -> Response {
    let is_client_config_error = e.chain().any(|cause| {
        cause.downcast_ref::<ConfigError>().is_some_and(|cfg| {
            matches!(
                cfg,
                ConfigError::UnknownModel(_)
                    | ConfigError::UnknownProvider(_)
                    | ConfigError::NoModel(_)
                    | ConfigError::NotFound(_)
                    | ConfigError::UnsupportedProviderType(_)
            )
        })
    });
    // A missing workspace comes from `resolve_workspace` (canonicalize) as a
    // plain io context string, not a ConfigError — treat "workspace does not
    // exist" as a client error too. Likewise, a workspace override pointing at a
    // directory with no provider config (and no `~/.omini` fallback) bails with
    // "no providers configured" — also the user's bad choice, not a server fault.
    let is_workspace_input_error = e.chain().any(|cause| {
        let msg = cause.to_string();
        msg.contains("workspace does not exist") || msg.contains("no providers configured")
    });

    if is_client_config_error || is_workspace_input_error {
        bad_request(e)
    } else {
        internal_error(e)
    }
}

/// `GET /profiles` — profiles available for a new session (name + description).
async fn list_profiles(State(state): State<AppState>) -> Response {
    let profiles = state.registry.list_profiles();
    Json(json!({ "profiles": profiles })).into_response()
}

/// `GET /models` — models available for a per-session override, flattened from
/// the configured providers.
async fn list_models(State(state): State<AppState>) -> Response {
    match state.registry.list_models() {
        Ok(models) => Json(json!({ "models": models })).into_response(),
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
        Ok((new_id, _handle)) => {
            (StatusCode::CREATED, Json(json!({ "session_id": new_id.0 }))).into_response()
        }
        Err(e) => internal_error(&e),
    }
}

/// Optional config changes for [`reconfigure_session`], as query params
/// (`?profile=&model=`). Workspace is intentionally absent — it is a session
/// property, not a reconfigurable one (`doc/profile.md` §5).
#[derive(Debug, Default, Deserialize)]
struct ReconfigureParams {
    /// New profile to bind; unchanged from the parent when absent.
    profile: Option<String>,
    /// New model override (`provider/model_id` or bare `model_id`); the new
    /// profile's default when absent.
    model: Option<String>,
}

/// `POST /sessions/{id}/reconfigure` — materialize a config change (profile /
/// model) as a new session seeded with this session's full conversation
/// (`origin.kind = reconfiguration`). Returns the new session id. A bad
/// override is a client error → 400, mirroring `create_session`.
async fn reconfigure_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ReconfigureParams>,
) -> Response {
    let parent = SessionId(id);
    let result = state
        .registry
        .reconfigure(&parent, params.profile.as_deref(), params.model.as_deref())
        .await;
    match result {
        Ok((new_id, _handle)) => {
            (StatusCode::CREATED, Json(json!({ "session_id": new_id.0 }))).into_response()
        }
        // A parent-not-found is a 404; a bad profile/model is a 400; else 500.
        Err(e) => {
            if e.chain().any(|c| {
                c.downcast_ref::<crate::session::SessionError>()
                    .is_some_and(|se| matches!(se, crate::session::SessionError::NotFound(_)))
            }) {
                not_found(&e)
            } else {
                create_error(&e)
            }
        }
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

/// `GET /sessions/{id}/summary` — derived monitor metrics for one session,
/// computed by replaying its committed `events.jsonl` through the monitor fold
/// (`doc/monitor.md` §8). Cost is priced with the gateway's pricing table at
/// read time, so it reflects current prices.
async fn session_summary(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let sid = SessionId(id);
    let events = state
        .registry
        .store()
        .read_events(&sid)
        .with_context(|| format!("failed to read session `{}`", sid.0));
    match events {
        Ok(events) => {
            let summary = monitor::summarize(&events, (*state.pricing).clone());
            Json(summary).into_response()
        }
        Err(e) => not_found(&e),
    }
}

/// `GET /sessions/{id}/runtime` — the config-layer provider/model the gateway
/// resolves for this session (the RUNTIME panel's display source). Derived from
/// the session's profile via config, not from the live event stream, so it
/// stays stable across subagent/fork model switches (`doc/frontend.md`, B1).
async fn session_runtime(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let sid = SessionId(id);
    // Establish the session exists (404 otherwise), then read its configured
    // profile to resolve the model. A resolve failure is a server-side config
    // problem (500), not a missing session.
    let meta = match state.registry.meta(&sid) {
        Ok(meta) => meta,
        Err(e) => return not_found(&e),
    };
    match state
        .registry
        .runtime_info(meta.profile_id.as_deref(), meta.workspace.as_deref())
    {
        Ok(info) => Json(info).into_response(),
        Err(e) => internal_error(&e),
    }
}
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
    if let GatewayEvent::Event { event: core } = gw {
        event.id(core.seq.to_string())
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
        (StatusCode::CONFLICT, Json(json!({ "error": msg }))).into_response()
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

fn bad_request(e: &anyhow::Error) -> Response {
    (
        StatusCode::BAD_REQUEST,
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
            config: crate::config::ConfigStore::discover(dir.path()),
            workspace: dir.path().to_owned(),
            profile: "default".to_owned(),
            no_dotenv: true,
        };
        let registry = SessionRegistry::new(defaults, &GatewayConfig::default());
        (registry, dir)
    }

    /// Build a registry over a temp workspace seeded with a minimal
    /// `.omini/config/providers.toml` and one `.omini/profiles/coding.toml`, so
    /// the config-enumeration + override routes have real config to read.
    ///
    /// The provider's `api_key_env` points at `PATH` (always set in the test
    /// environment) so model resolution finds a key without this crate mutating
    /// the environment — `unsafe` (and thus `std::env::set_var`) is forbidden.
    fn test_registry_with_config() -> (SessionRegistry, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let omini = dir.path().join(".omini");
        std::fs::create_dir_all(omini.join("config")).unwrap();
        std::fs::create_dir_all(omini.join("profiles")).unwrap();
        std::fs::write(
            omini.join("config/providers.toml"),
            r#"
[[providers]]
name = "openai-main"
type = "openai-chat"
base_url = "https://example.test/v1"
api_key_env = "PATH"

[[providers.models]]
id = "gpt-4o"
context_window = 128000
max_output_tokens = 16384
"#,
        )
        .unwrap();
        std::fs::write(
            omini.join("profiles/coding.toml"),
            r#"
[profile]
name = "coding"
description = "Software development agent"

[model]
default = "openai-main/gpt-4o"
"#,
        )
        .unwrap();
        let defaults = SessionDefaults {
            config: crate::config::ConfigStore::discover(dir.path()),
            workspace: dir.path().to_owned(),
            profile: "coding".to_owned(),
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
            pricing: Arc::new(PricingTable::default()),
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
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;
        let client = reqwest::Client::new();

        // No token → 401.
        let resp = client
            .get(format!("{base}/api/sessions"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // Wrong token → 401.
        let resp = client
            .get(format!("{base}/api/sessions"))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // Correct token → 200 (empty session list).
        let resp = client
            .get(format!("{base}/api/sessions"))
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
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;
        let resp = reqwest::get(format!("{base}/api/sessions")).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    /// `replay_events` includes only events strictly after `Last-Event-ID`, so a
    /// reconnecting client resumes without duplicating what it already saw and
    /// without skipping anything. This is the SSE resume boundary.
    #[test]
    fn replay_filters_strictly_after_last_seen() {
        let dir = tempfile::tempdir().unwrap();
        let defaults = SessionDefaults {
            config: crate::config::ConfigStore::discover(dir.path()),
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
                    crate::core::EventPayload::Session(crate::core::payload::SessionEvent::Paused),
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

    /// `GET /sessions/{id}/summary` returns a derived `SessionSummary` as typed
    /// JSON for an existing session. A fresh session with no model/tool activity
    /// folds to all-zero counts and an unpriced (`null`) cost — proving the
    /// endpoint replays the log through the monitor rather than 404ing.
    #[tokio::test]
    async fn summary_endpoint_returns_typed_json() {
        let (registry, _dir) = test_registry();
        let store = registry.store();
        let writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        drop(writer); // release the lock before the handler reads the log

        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::get(format!("{base}/api/sessions/{}/summary", sid.0))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["total_turns"], 0);
        assert_eq!(body["total_tool_calls"], 0);
        assert!(body["cost_usd"].is_null(), "no priced model ran");
        assert!(body["tools_used"].is_object());
    }

    /// An unknown session id yields 404 from the summary endpoint, not a 500 or
    /// an empty summary.
    #[tokio::test]
    async fn summary_endpoint_unknown_session_is_404() {
        let (registry, _dir) = test_registry();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::get(format!("{base}/api/sessions/does-not-exist/summary"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    /// `GET /profiles` lists the profiles found in the config roots (name +
    /// description), so a Web client can populate the new-session picker.
    #[tokio::test]
    async fn profiles_endpoint_lists_configured_profiles() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::get(format!("{base}/api/profiles")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let profiles = body["profiles"].as_array().unwrap();
        assert!(
            profiles
                .iter()
                .any(|p| p["name"] == "coding" && p["description"] == "Software development agent"),
            "coding profile with its description must be listed, got {profiles:?}"
        );
    }

    /// `GET /models` flattens the configured providers' models, each carrying its
    /// provider so the override can be sent back as `provider/model_id`.
    #[tokio::test]
    async fn models_endpoint_lists_configured_models() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::get(format!("{base}/api/models")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let models = body["models"].as_array().unwrap();
        assert!(
            models
                .iter()
                .any(|m| m["provider"] == "openai-main" && m["model_id"] == "gpt-4o"),
            "gpt-4o under openai-main must be listed, got {models:?}"
        );
    }

    /// A no-arg `POST /sessions` (no query string) still creates a session on the
    /// gateway defaults — the query-param overrides are optional, so the existing
    /// frontend call keeps working.
    #[tokio::test]
    async fn create_session_no_overrides_still_succeeds() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/api/sessions"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["session_id"].is_string());
    }

    /// A `?model=` override that names no configured model is a CLIENT error
    /// (400), not a 500 — the user picked a stale model, not a server fault.
    #[tokio::test]
    async fn create_session_unknown_model_is_400() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/api/sessions?model=bogus/nope"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    /// A `?workspace=` override pointing at a path that does not exist is a
    /// CLIENT error (400) — canonicalization fails on the user's bad input.
    #[tokio::test]
    async fn create_session_missing_workspace_is_400() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::Client::new()
            .post(format!(
                "{base}/api/sessions?workspace=/no/such/dir/ominiforge-test"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    /// A `?workspace=` override pointing at a real directory with no `.omini`
    /// still SUCCEEDS (201): config is independent of the workspace
    /// (`doc/architecture.md` §15) — it comes from the gateway's config store
    /// (launch cwd / --config-dir / home), not the session's workspace. This is
    /// the regression guard for the bug where config discovery followed the
    /// workspace and a config-less workspace wrongly failed.
    #[tokio::test]
    async fn create_session_workspace_without_config_uses_gateway_config() {
        let (registry, _dir) = test_registry_with_config();
        let empty = tempfile::tempdir().unwrap();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let ws = empty.path().to_str().unwrap();
        let resp = reqwest::Client::new()
            .post(format!("{base}/api/sessions?workspace={ws}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            201,
            "config comes from the gateway store, not the workspace"
        );
    }

    /// `POST /sessions/{id}/reconfigure` materializes a config change as a NEW
    /// session: a different id, `origin.kind = reconfiguration`, the parent
    /// recorded, and the new profile stamped on the new session's meta. The
    /// parent is left intact (history is immutable).
    #[tokio::test]
    async fn reconfigure_creates_new_session_with_reconfiguration_origin() {
        let (registry, _dir) = test_registry_with_config();
        // Seed a real parent session on disk (profile "coding").
        let (parent, _h) = registry.create().await.unwrap();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::Client::new()
            .post(format!(
                "{base}/api/sessions/{}/reconfigure?profile=coding",
                parent.0
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let body: serde_json::Value = resp.json().await.unwrap();
        let new_id = body["session_id"].as_str().unwrap();
        assert_ne!(new_id, parent.0, "reconfiguration mints a new session");

        // The new session's meta records the reconfiguration origin + parent.
        let meta: serde_json::Value = reqwest::get(format!("{base}/api/sessions/{new_id}"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(meta["origin"]["kind"], "reconfiguration");
        assert_eq!(meta["origin"]["parent_id"], parent.0);
        assert_eq!(meta["profile_id"], "coding");
    }

    /// Reconfiguring an unknown session is a 404, not a 500.
    #[tokio::test]
    async fn reconfigure_unknown_session_is_404() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::Client::new()
            .post(format!(
                "{base}/api/sessions/does-not-exist/reconfigure?profile=coding"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    /// Reconfiguring to an unknown model is a client error (400).
    #[tokio::test]
    async fn reconfigure_unknown_model_is_400() {
        let (registry, _dir) = test_registry_with_config();
        let (parent, _h) = registry.create().await.unwrap();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::Client::new()
            .post(format!(
                "{base}/api/sessions/{}/reconfigure?model=bogus/nope",
                parent.0
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }
}
