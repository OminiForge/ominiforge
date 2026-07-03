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
use crate::session::SessionMeta;

use super::actor::{ActorHandle, Command, GatewayEvent};
use super::config::GatewayConfig;
use super::registry::SessionRegistry;
use super::workspace::{WorkspaceId, group_sessions};

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
        .route("/workspaces", get(list_workspaces).post(create_workspace))
        .route(
            "/workspaces/{id}/sessions",
            get(list_workspace_sessions).post(create_workspace_session),
        )
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
        .route(
            "/profiles/{name}",
            get(get_profile).put(put_profile).delete(delete_profile),
        )
        .route("/models", get(list_models))
        .route("/providers", get(get_providers).put(put_providers))
        .route(
            "/secrets/{provider}",
            axum::routing::put(put_secret).delete(delete_secret),
        )
        .route("/secrets", get(list_secrets))
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

/// `GET /workspaces` — sessions grouped by workspace, most-recently-active
/// first (the no-workspace group pinned last). This is the dashboard's read
/// source: one call yields every workspace with its session count + latest
/// activity, computed server-side so the frontend never groups client-side.
async fn list_workspaces(State(state): State<AppState>) -> Response {
    match state.registry.list_metas() {
        Ok(metas) => {
            let workspaces = group_sessions(metas);
            Json(json!({ "workspaces": workspaces })).into_response()
        }
        Err(e) => internal_error(&e),
    }
}

/// `GET /workspaces/{id}/sessions` — the session metadata for one workspace,
/// newest first. `id` is a path-derived [`WorkspaceId`] (or the `"none"`
/// sentinel); sessions whose workspace hashes to it are returned. Returns an
/// empty list for an unknown id rather than 404 — a workspace only exists as
/// long as a session references it, so "no such workspace" and "workspace with
/// no sessions" are the same state.
async fn list_workspace_sessions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let metas = match state.registry.list_metas() {
        Ok(m) => m,
        Err(e) => return internal_error(&e),
    };
    let target = WorkspaceId(id);
    let sessions: Vec<SessionMeta> = metas
        .into_iter()
        .filter(|m| {
            let wid = m
                .workspace
                .as_deref()
                .map_or_else(WorkspaceId::none, WorkspaceId::from_path);
            wid == target
        })
        .collect();
    Json(json!({ "sessions": sessions })).into_response()
}

/// Body of a create-workspace request: the absolute path to open.
#[derive(Debug, Deserialize)]
struct CreateWorkspaceBody {
    /// Absolute path of the workspace directory.
    path: String,
}

/// `POST /workspaces` — register a workspace by path and return its opaque id.
/// The path is canonicalized and recorded in the gateway's workspace map (so a
/// later `POST /workspaces/{id}/sessions` can resolve it server-side); only the
/// id is returned — the path never travels back to the client. A path that does
/// not exist or is not a directory is a client error (400).
async fn create_workspace(
    State(state): State<AppState>,
    Json(body): Json<CreateWorkspaceBody>,
) -> Response {
    let path = std::path::PathBuf::from(&body.path);
    // Guard "is a directory" before recording: sessions run in a directory, and
    // canonicalize alone would accept a regular file.
    match std::fs::canonicalize(&path) {
        Ok(canonical) if canonical.is_dir() => {}
        Ok(_) => {
            return bad_request(&anyhow::anyhow!(
                "workspace path is not a directory: {}",
                body.path
            ));
        }
        Err(_) => {
            return bad_request(&anyhow::anyhow!(
                "workspace path does not exist: {}",
                body.path
            ));
        }
    }
    match state.registry.record_workspace(&path) {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "workspace_id": id.0 }))).into_response(),
        Err(e) => internal_error(&e),
    }
}

/// Optional per-session overrides for [`create_workspace_session`], as query
/// params (`?profile=&model=`). Workspace is absent — it is fixed by the path
/// the workspace id resolves to, never chosen by the client.
#[derive(Debug, Default, Deserialize)]
struct WorkspaceSessionParams {
    /// Profile name to bind; gateway default when absent.
    profile: Option<String>,
    /// Model override (`provider/model_id` or bare `model_id`); profile default
    /// when absent.
    model: Option<String>,
}

/// `POST /workspaces/{id}/sessions` — create a session **in the workspace `id`**
/// resolves to, returning its id. The workspace path is looked up server-side
/// from the workspace map (recorded via `POST /workspaces` or seeded from an
/// existing session), so the client never sends a path. Optional
/// `?profile=&model=` choose per-session overrides. An unknown id is a 404; a
/// bad profile/model is a 400.
async fn create_workspace_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<WorkspaceSessionParams>,
) -> Response {
    let wid = WorkspaceId(id);
    let result = state
        .registry
        .create_in_workspace(&wid, params.profile.as_deref(), params.model.as_deref())
        .await;
    match result {
        Ok((sid, _handle)) => {
            (StatusCode::CREATED, Json(json!({ "session_id": sid.0 }))).into_response()
        }
        Err(e) => {
            // An unknown workspace id is a 404; a bad profile/model is a 400
            // (via create_error); anything else is a 500.
            if e.to_string().contains("unknown workspace id") {
                not_found(&e)
            } else {
                create_error(&e)
            }
        }
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

/// `GET /providers` — the full `providers.toml` for the settings UI, plus the
/// set of provider names that have an API key in the secret store. Keys
/// themselves are never returned; the UI shows only whether one is configured.
async fn get_providers(State(state): State<AppState>) -> Response {
    let providers = match state.registry.load_providers() {
        Ok(p) => p,
        Err(e) => return internal_error(&e),
    };
    let secret_names = match state.registry.secret_names() {
        Ok(n) => n,
        Err(e) => return internal_error(&e),
    };
    Json(json!({ "providers": providers.providers, "secret_names": secret_names })).into_response()
}

/// `PUT /providers` — overwrite `providers.toml` with the posted provider set
/// (full desired state). A malformed body is a client error (400).
async fn put_providers(
    State(state): State<AppState>,
    Json(providers): Json<crate::config::ProvidersFile>,
) -> Response {
    match state.registry.save_providers(&providers) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal_error(&e),
    }
}

/// `GET /profiles/{name}` — the raw (unresolved) profile file, for editing.
async fn get_profile(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match state.registry.load_profile_raw(&name) {
        Ok(profile) => Json(profile).into_response(),
        Err(e) => not_found(&e),
    }
}

/// `PUT /profiles/{name}` — overwrite (or create) profile `name`'s file.
async fn put_profile(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(profile): Json<crate::config::Profile>,
) -> Response {
    match state.registry.save_profile(&name, &profile) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal_error(&e),
    }
}

/// `DELETE /profiles/{name}` — remove profile `name`'s file. 404 if absent.
async fn delete_profile(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match state.registry.delete_profile(&name) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("profile `{name}` does not exist") })),
        )
            .into_response(),
        Err(e) => internal_error(&e),
    }
}

/// `GET /secrets` — the provider names that have an API key stored (never the
/// keys themselves).
async fn list_secrets(State(state): State<AppState>) -> Response {
    match state.registry.secret_names() {
        Ok(names) => Json(json!({ "secret_names": names })).into_response(),
        Err(e) => internal_error(&e),
    }
}

/// Body of a set-secret request: the API key to store for a provider.
#[derive(Debug, Deserialize)]
struct SecretBody {
    /// The provider's API key. Stored in the secret store, never in
    /// `providers.toml` and never exported to a subprocess environment.
    api_key: String,
}

/// `PUT /secrets/{provider}` — store (or replace) the API key for `provider`.
async fn put_secret(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Json(body): Json<SecretBody>,
) -> Response {
    match state.registry.set_secret(&provider, &body.api_key) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal_error(&e),
    }
}

/// `DELETE /secrets/{provider}` — remove `provider`'s stored API key. 404 if
/// none was stored.
async fn delete_secret(State(state): State<AppState>, Path(provider): Path<String>) -> Response {
    match state.registry.delete_secret(&provider) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no stored key for provider `{provider}`") })),
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
        .await
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

    /// `GET /providers` returns the raw provider set plus which providers have a
    /// stored secret — the settings UI's read source. No key values appear.
    #[tokio::test]
    async fn get_providers_returns_config_and_secret_names() {
        let (registry, _dir) = test_registry_with_config();
        // Store a key so `secret_names` is non-empty.
        registry.set_secret("openai-main", "sk-test").unwrap();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::get(format!("{base}/api/providers")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["providers"][0]["name"], "openai-main");
        assert_eq!(body["secret_names"][0], "openai-main");
        // The key value itself must never be serialized anywhere in the response.
        assert!(
            !serde_json::to_string(&body).unwrap().contains("sk-test"),
            "the API key value must not appear in the providers response"
        );
    }

    /// `PUT /providers` overwrites `providers.toml`; a following `GET` reflects
    /// the new state — the settings UI's save round-trip.
    #[tokio::test]
    async fn put_providers_overwrites_and_reads_back() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let body = json!({
            "providers": [{
                "name": "anthropic-main",
                "type": "anthropic",
                "base_url": "https://api.anthropic.com",
                "api_key_env": "ANTHROPIC_API_KEY",
                "models": [{
                    "id": "claude-sonnet-4-6",
                    "context_window": 200_000,
                    "max_output_tokens": 16_000
                }]
            }]
        });
        let resp = reqwest::Client::new()
            .put(format!("{base}/api/providers"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);

        let got: serde_json::Value = reqwest::get(format!("{base}/api/providers"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(got["providers"][0]["name"], "anthropic-main");
        assert_eq!(got["providers"][0]["models"][0]["id"], "claude-sonnet-4-6");
    }

    /// Profile CRUD round-trip: PUT creates a file, GET reads it back raw, DELETE
    /// removes it (and a second DELETE is a 404).
    #[tokio::test]
    async fn profile_put_get_delete_round_trip() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;
        let client = reqwest::Client::new();

        let profile = json!({
            "profile": { "name": "writing", "description": "Prose agent" },
            "model": { "default": "openai-main/gpt-4o", "temperature": 0.7 }
        });
        let resp = client
            .put(format!("{base}/api/profiles/writing"))
            .json(&profile)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);

        let got: serde_json::Value = client
            .get(format!("{base}/api/profiles/writing"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(got["profile"]["name"], "writing");
        assert_eq!(got["model"]["temperature"], 0.7);

        let resp = client
            .delete(format!("{base}/api/profiles/writing"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        // Gone now → second delete is a 404.
        let resp = client
            .delete(format!("{base}/api/profiles/writing"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    /// Secret set/delete round-trip: PUT stores a key (reflected in the secret
    /// name list), DELETE removes it, and a second DELETE is a 404.
    #[tokio::test]
    async fn secret_put_delete_round_trip() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .put(format!("{base}/api/secrets/openai-main"))
            .json(&json!({ "api_key": "sk-secret" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);

        let listed: serde_json::Value = client
            .get(format!("{base}/api/secrets"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed["secret_names"][0], "openai-main");

        let resp = client
            .delete(format!("{base}/api/secrets/openai-main"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        let resp = client
            .delete(format!("{base}/api/secrets/openai-main"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
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

    /// `GET /workspaces` groups sessions by their workspace path. Two sessions in
    /// the same workspace collapse to one group with `session_count = 2`; a
    /// session in a different workspace is its own group. This is the dashboard's
    /// read source — grouping happens server-side.
    #[tokio::test]
    async fn list_workspaces_groups_by_path() {
        let (registry, dir) = test_registry_with_config();
        let store = registry.store();

        // Two sessions in workspace A, one in workspace B. Use real dirs so the
        // paths are stable (create_new stamps meta.workspace verbatim).
        let ws_a = dir.path().join("proj-a");
        let ws_b = dir.path().join("proj-b");
        std::fs::create_dir_all(&ws_a).unwrap();
        std::fs::create_dir_all(&ws_b).unwrap();

        for _ in 0..2 {
            drop(
                store
                    .create_new(None, Some(ws_a.clone()), vec![])
                    .unwrap(),
            );
        }
        drop(store.create_new(None, Some(ws_b.clone()), vec![]).unwrap());

        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let body: serde_json::Value = reqwest::get(format!("{base}/api/workspaces"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let workspaces = body["workspaces"].as_array().unwrap();
        assert_eq!(workspaces.len(), 2, "two distinct workspaces");

        let a = workspaces
            .iter()
            .find(|w| w["path"].as_str().unwrap().ends_with("proj-a"))
            .unwrap();
        assert_eq!(a["session_count"], 2);
        let b = workspaces
            .iter()
            .find(|w| w["path"].as_str().unwrap().ends_with("proj-b"))
            .unwrap();
        assert_eq!(b["session_count"], 1);
    }

    /// `GET /workspaces/{id}/sessions` returns exactly the sessions whose
    /// workspace hashes to `id`. Sessions in a different workspace are excluded,
    /// so the panel's sidebar only ever shows its own workspace's sessions.
    #[tokio::test]
    async fn list_workspace_sessions_filters_to_the_workspace() {
        let (registry, dir) = test_registry_with_config();
        let store = registry.store();

        let ws_a = dir.path().join("proj-a");
        let ws_b = dir.path().join("proj-b");
        std::fs::create_dir_all(&ws_a).unwrap();
        std::fs::create_dir_all(&ws_b).unwrap();
        let in_a = store
            .create_new(None, Some(ws_a.clone()), vec![])
            .unwrap()
            .session_id()
            .clone();
        drop(store.create_new(None, Some(ws_b.clone()), vec![]).unwrap());

        let wid = crate::gateway::WorkspaceId::from_path(&ws_a).0;

        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let body: serde_json::Value =
            reqwest::get(format!("{base}/api/workspaces/{wid}/sessions"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "only workspace A's session");
        assert_eq!(sessions[0]["id"], in_a.0);
    }

    /// `POST /workspaces` with an existing directory returns 201 + the id that
    /// path hashes to (so the dashboard can route straight to the panel). The id
    /// matches [`WorkspaceId::from_path`] on the canonicalized path.
    #[tokio::test]
    async fn create_workspace_returns_derived_id() {
        let (registry, dir) = test_registry_with_config();
        let ws = dir.path().join("new-proj");
        std::fs::create_dir_all(&ws).unwrap();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/api/workspaces"))
            .json(&json!({ "path": ws.to_str().unwrap() }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let body: serde_json::Value = resp.json().await.unwrap();
        // The returned id hashes the canonicalized path; recompute to compare.
        let canonical = std::fs::canonicalize(&ws).unwrap();
        let expected = crate::gateway::WorkspaceId::from_path(&canonical).0;
        assert_eq!(body["workspace_id"], expected);
    }

    /// `POST /workspaces` with a path that does not exist is a client error (400),
    /// not a 500 — the user typed a bad path.
    #[tokio::test]
    async fn create_workspace_missing_path_is_400() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/api/workspaces"))
            .json(&json!({ "path": "/no/such/dir/ominiforge-ws-test" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    /// `POST /workspaces/{id}/sessions` creates a session in the workspace the id
    /// resolves to: after recording a workspace, the session created via its id
    /// has `meta.workspace` hashing back to that same id — NOT the gateway
    /// default. This is the regression guard for "new sessions all land in the
    /// default workspace" (bug 3).
    #[tokio::test]
    async fn create_workspace_session_lands_in_that_workspace() {
        let (registry, dir) = test_registry_with_config();
        // A real target workspace distinct from the gateway default (dir.path()).
        let ws = dir.path().join("target-ws");
        std::fs::create_dir_all(&ws).unwrap();
        let canonical = std::fs::canonicalize(&ws).unwrap();
        let wid = crate::gateway::WorkspaceId::from_path(&canonical).0;

        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;
        let http = reqwest::Client::new();

        // Record the workspace (returns the same id we computed).
        let rec: serde_json::Value = http
            .post(format!("{base}/api/workspaces"))
            .json(&json!({ "path": ws.to_str().unwrap() }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(rec["workspace_id"], wid);

        // Create a session in that workspace by id.
        let resp = http
            .post(format!("{base}/api/workspaces/{wid}/sessions"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let sid = resp.json::<serde_json::Value>().await.unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned();

        // The new session's workspace hashes back to the id we created under.
        let meta: serde_json::Value = http
            .get(format!("{base}/api/sessions/{sid}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let landed = std::path::PathBuf::from(meta["workspace"].as_str().unwrap());
        assert_eq!(
            crate::gateway::WorkspaceId::from_path(&landed).0,
            wid,
            "session must land in the requested workspace, not the gateway default"
        );
    }

    /// `POST /workspaces/{id}/sessions` for an id the gateway has never seen (no
    /// recorded path, no session to seed from) is a 404 — not a 500 or a session
    /// silently created in the default workspace.
    #[tokio::test]
    async fn create_workspace_session_unknown_id_is_404() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
            pricing: Arc::new(PricingTable::default()),
        };
        let base = serve_test(state).await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/api/workspaces/deadbeefdeadbeef/sessions"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }
}
