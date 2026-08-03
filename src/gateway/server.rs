//! The axum HTTP/SSE server.
//!
//! Control plane is REST; the live event stream is SSE (`GET …/events`). All
//! routes except `/healthz` require a bearer token when one is configured
//! (`doc/gateway.md`). TLS is *not* handled here — the gateway binds loopback
//! and a reverse proxy terminates TLS for public exposure
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
use axum::extract::{Path, Query, Request, State};
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
use crate::monitor;
use crate::session::SessionMeta;

use super::actor::{Command, GatewayEvent};
use super::config::GatewayConfig;
use super::registry::SessionRegistry;
use super::status::SessionStatus;
use super::workspace::{WorkspaceId, group_sessions};
use crate::agent::{ApprovalDecision, ApprovalScope};

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
///
/// The session API is nested under `/api/*` so it never collides with the
/// SPA's own client-side routes (which share names like `/sessions`) when the
/// gateway serves the static frontend from the same origin (`doc/gateway.md`).
fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/workspaces", get(list_workspaces).post(create_workspace))
        .route(
            "/workspaces/config/orphans",
            get(list_workspace_config_orphans),
        )
        .route(
            "/workspaces/config/{id}",
            axum::routing::delete(delete_workspace_config),
        )
        .route(
            "/workspaces/{id}/config",
            get(get_workspace_config).put(put_workspace_config),
        )
        .route("/workspaces/{id}/tools", get(list_workspace_tools))
        .route(
            "/workspaces/{id}/sessions",
            get(list_workspace_sessions).post(create_workspace_session),
        )
        .route(
            "/workspaces/{id}/sessions/archived",
            get(list_archived_workspace_sessions),
        )
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/fork", post(fork_session))
        .route("/sessions/{id}/reconfigure", post(reconfigure_session))
        .route("/sessions/{id}/message", post(post_message))
        .route("/sessions/{id}/cancel", post(cancel_turn))
        .route("/sessions/{id}/approve", post(approve_tool_call))
        .route("/sessions/{id}/archive", post(archive_session))
        .route("/sessions/{id}/compact", post(compact_session))
        .route("/sessions/{id}/summary", get(session_summary))
        .route("/sessions/{id}/view", get(session_view))
        .route("/sessions/{id}/snapshot", get(session_snapshot))
        .route("/sessions/{id}/fork-preview", get(fork_preview))
        .route("/sessions/{id}/runtime", get(session_runtime))
        .route("/sessions/{id}/events", get(sse_events))
        .route("/status/events", get(status_events))
        .route("/profiles", get(list_profiles))
        .route(
            "/profiles/{name}",
            get(get_profile).put(put_profile).delete(delete_profile),
        )
        .route("/models", get(list_models))
        .route("/tools", get(list_tools))
        .route("/providers", get(get_providers).put(put_providers))
        .route("/providers/{name}/test", post(test_provider))
        .route(
            "/gateway/permission",
            get(get_gateway_permission).put(put_gateway_permission),
        )
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
        // Compress responses (gzip/br) when the client accepts it: the folded
        // conversation view is multi-MB JSON, and over a relayed link (SSH
        // forward / reverse proxy) the transfer — not the fold — dominates
        // session-open latency. SSE streams pass through unaffected (they are
        // flushed per frame, not buffered).
        .layer(tower_http::compression::CompressionLayer::new())
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

/// True when the session's workspace hashes to `target` — the shared filter
/// behind both workspace-scoped session listings (active and archived), so the
/// two never drift apart on what "belongs to this workspace" means.
fn in_workspace(m: &SessionMeta, target: &WorkspaceId) -> bool {
    let wid = m
        .workspace
        .as_deref()
        .map_or_else(WorkspaceId::none, WorkspaceId::from_path);
    wid == *target
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
        .filter(|m| in_workspace(m, &target))
        .collect();
    Json(json!({ "sessions": sessions })).into_response()
}

/// `GET /workspaces/{id}/sessions/archived` — the workspace's **archived**
/// sessions, newest first (`doc/session-storage.md` §9). The archived section's
/// read source: workspace-scoped like [`list_workspace_sessions`] (a panel only
/// ever shows its own workspace's sessions, active or retired), and from here
/// the only remaining action is a permanent `DELETE /sessions/{id}`. Returns
/// `{ "sessions": [SessionMeta, …] }` — the same shape and empty-list semantics
/// as `list_workspace_sessions`.
async fn list_archived_workspace_sessions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let metas = match state.registry.list_archived_metas() {
        Ok(m) => m,
        Err(e) => return internal_error(&e),
    };
    let target = WorkspaceId(id);
    let sessions: Vec<SessionMeta> = metas
        .into_iter()
        .filter(|m| in_workspace(m, &target))
        .collect();
    Json(json!({ "sessions": sessions })).into_response()
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

/// `GET /workspaces/config/orphans` — per-workspace configs whose workspace path
/// no longer resolves (`doc/workspace-config.md` GC). Read-only: lists orphans a
/// human or ops tool can then explicitly delete; never removes anything. Each
/// entry is `{ workspace_id, path }` (`path` null when it can't be recovered).
async fn list_workspace_config_orphans(State(state): State<AppState>) -> Response {
    let orphans: Vec<_> = state
        .registry
        .list_config_orphans()
        .into_iter()
        .map(|(id, path)| json!({ "workspace_id": id.0, "path": path }))
        .collect();
    Json(json!({ "orphans": orphans })).into_response()
}

/// `DELETE /workspaces/config/{id}` — remove one per-workspace config
/// (`doc/workspace-config.md` GC). The only path that deletes a config file — GC
/// is always explicit. Idempotent: a missing config is still 204.
async fn delete_workspace_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.registry.delete_workspace_config(&WorkspaceId(id)) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal_error(&e),
    }
}

/// `GET /workspaces/{id}/config` — the per-workspace config (network + mounts +
/// permission) for editing (`doc/workspace-config.md`, `doc/permission.md` §3.1,
/// the top tier). Returns the default (all-absent) config when none is stored,
/// so the editor always has a shape to bind. A malformed on-disk file is a 500
/// (fail-loud), an unknown workspace id a 404.
async fn get_workspace_config(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    use crate::gateway::WorkspaceConfigError;
    match state.registry.load_workspace_config(&WorkspaceId(id)) {
        Ok(config) => Json(config).into_response(),
        // Unknown id is the caller's fault (404); a malformed file is a server
        // problem (500) that must not masquerade as "workspace does not exist".
        Err(e @ WorkspaceConfigError::UnknownWorkspace(_)) => not_found(&anyhow::anyhow!(e)),
        Err(e @ WorkspaceConfigError::Load(_)) => internal_error(&anyhow::anyhow!(e)),
    }
}

/// `PUT /workspaces/{id}/config` — overwrite the per-workspace config (full
/// desired state). The file lives under the gateway's trusted
/// `.omini/workspaces/`, never the agent-writable project dir, so a workspace
/// widening its own `deny` floor is safe (`doc/workspace-config.md`).
async fn put_workspace_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(config): Json<crate::gateway::workspace_config::WorkspaceConfig>,
) -> Response {
    match state
        .registry
        .save_workspace_config(&WorkspaceId(id), &config)
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal_error(&e),
    }
}

/// `GET /workspaces/{id}/tools` — the permission-config tool catalog for this
/// workspace: the built-ins plus its MCP tools, enumerated best-effort
/// (`doc/permission.md` §3.2). MCP failures are swallowed server-side, so this
/// returns the built-ins even when a server is down. 404 for an unknown id.
async fn list_workspace_tools(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.registry.list_workspace_tools(&WorkspaceId(id)).await {
        Ok(tools) => Json(json!({ "tools": tools })).into_response(),
        Err(e) => not_found(&e),
    }
}

/// `GET /gateway/permission` — the gateway-wide baseline permission policy
/// (bottom tier, `doc/permission.md` §3.1) for the settings UI.
async fn get_gateway_permission(State(state): State<AppState>) -> Response {
    match state.registry.gateway_permission() {
        Ok(policy) => Json(policy).into_response(),
        Err(e) => internal_error(&e),
    }
}

/// `PUT /gateway/permission` — replace the gateway baseline policy. Applies to
/// **new** sessions immediately (the value is read fresh per session) and is
/// persisted to `gateway.toml` (survives restart). Other gateway fields are
/// preserved through the write.
async fn put_gateway_permission(
    State(state): State<AppState>,
    Json(policy): Json<crate::permission::PermissionPolicy>,
) -> Response {
    match state.registry.set_gateway_permission(policy).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal_error(&e),
    }
}

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

/// `GET /tools` — the built-in tool catalog for the permission-config UI
/// (`doc/permission.md` §3.2): each tool's friendly label + the input fields a
/// gating rule may target. Static (no workspace / subprocess needed), so it
/// serves the profile and gateway config surfaces. MCP tools are enumerated
/// per-workspace elsewhere.
async fn list_tools() -> Response {
    Json(json!({ "tools": crate::tool::builtin_catalog() })).into_response()
}

/// `GET /providers` — the merged provider set (user config plus the built-in
/// catalog) for the settings UI, the set of provider names that have an API
/// key in the secret store, and the names reserved by the built-in catalog
/// (rendered as connect cards, not editable forms). Keys themselves are never
/// returned; the UI shows only whether one is configured.
async fn get_providers(State(state): State<AppState>) -> Response {
    let providers = match state.registry.load_providers() {
        Ok(p) => p,
        Err(e) => return internal_error(&e),
    };
    let secret_names = match state.registry.secret_names() {
        Ok(n) => n,
        Err(e) => return internal_error(&e),
    };
    Json(json!({
        "providers": providers.providers,
        "secret_names": secret_names,
        "builtin_names": crate::config::builtin_provider_names(),
    }))
    .into_response()
}

/// `PUT /providers` — overwrite `providers.toml` with the posted provider set
/// (full desired state). A malformed body or an entry reusing a built-in
/// catalog name is a client error (400).
async fn put_providers(
    State(state): State<AppState>,
    Json(providers): Json<crate::config::ProvidersFile>,
) -> Response {
    match state.registry.save_providers(&providers) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            if e.chain().any(|cause| {
                cause
                    .downcast_ref::<ConfigError>()
                    .is_some_and(|cfg| matches!(cfg, ConfigError::BuiltinProviderConflict(_)))
            }) {
                bad_request(&e)
            } else {
                internal_error(&e)
            }
        }
    }
}

/// The optional body of `POST /providers/{name}/test`: unsaved edits and/or an
/// unsaved key, so the settings UI can probe before persisting.
#[derive(Debug, Default, serde::Deserialize)]
struct TestProviderBody {
    /// Draft connection fields (a custom provider being edited).
    edit: Option<crate::config::ProviderConfig>,
    /// A key the user just typed but has not saved.
    key: Option<String>,
}

/// `POST /providers/{name}/test` — probe the provider with a minimal request.
/// Always 200 with `{ ok, model?, error? }`: the point is to report *why* a
/// connection fails (auth, transport, no model), which is data, not an HTTP
/// fault. An unknown provider name is a real 404.
async fn test_provider(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: Option<Json<TestProviderBody>>,
) -> Response {
    let Json(body) = body.unwrap_or_default();
    match state
        .registry
        .test_provider(&name, body.edit, body.key)
        .await
    {
        Ok(model) => Json(json!({ "ok": true, "model": model })).into_response(),
        Err(e) if e.to_string().contains("unknown provider") => not_found(&e),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })).into_response(),
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
    match state.registry.save_profile(&name, &profile).await {
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
    /// Optional per-turn model override (`provider/model_id`). Resolves against
    /// the configured providers; `None` keeps the session's model.
    #[serde(default)]
    model: Option<String>,
    /// Optional per-turn reasoning-effort tier (a raw string the session's
    /// model declares). `None` keeps the session's configured effort.
    #[serde(default)]
    think_effort: Option<String>,
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
    match handle
        .send(Command::Send {
            text: body.text,
            model: body.model,
            think_effort: body.think_effort,
        })
        .await
    {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(_) => internal_error(&anyhow::anyhow!("session actor is unavailable")),
    }
}

/// Body of an approve request: which suspended tool call, and the decision.
#[derive(Debug, Deserialize)]
struct ApproveBody {
    /// The `call_id` from the `ApprovalRequested` event being answered.
    call_id: String,
    /// `approve` runs the tool; `reject` blocks it (`denied_by_user`).
    decision: ApprovalDecision,
    /// How far the decision reaches: `once` (default — this call only),
    /// `session`, `profile`, or `gateway` (`doc/permission.md` §5).
    #[serde(default)]
    scope: Option<ApprovalScope>,
}

/// `POST /sessions/{id}/approve` — deliver a human decision for a tool call the
/// permission policy suspended (`doc/permission.md` §5). Returns `202 Accepted`;
/// an unknown or already-resolved `call_id` is accepted and ignored by the
/// actor (idempotent). A stopped actor is a 5xx (respawn/retry).
async fn approve_tool_call(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ApproveBody>,
) -> Response {
    let sid = SessionId(id);
    let handle = match state.registry.get_or_spawn(&sid).await {
        Ok(h) => h,
        Err(e) => return conflict_or_not_found(&e),
    };
    match handle
        .send(Command::Approve {
            call_id: body.call_id,
            decision: body.decision,
            scope: body.scope.unwrap_or(ApprovalScope::Once),
        })
        .await
    {
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

/// `POST /sessions/{id}/archive` — retire a session for good: drop it from the
/// active list while keeping its files for read-only inspection
/// (`doc/session-storage.md` §9). Stops the actor and releases its sandbox
/// (`doc/sandbox.md` §9 Q5). One-way — an archived session cannot be run again
/// (its run paths return 410). A running turn is a 409 (cancel first); an unknown
/// session is a 404.
async fn archive_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let sid = SessionId(id);
    match state.registry.archive(&sid).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => conflict_or_not_found(&e),
    }
}

/// `DELETE /sessions/{id}` — permanently remove a session's files.
/// **Irreversible.** Requires the session to be **archived first**
/// (`doc/session-storage.md` §9): a non-archived session is a 409 ("archive it
/// first"), which is the deliberate two-step confirmation. An unknown session is
/// a 404.
async fn delete_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let sid = SessionId(id);
    match state.registry.delete(&sid) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
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
/// (`doc/monitor.md` §8). A branched session (fork/compaction/reconfiguration)
/// folds the estimated size of its inherited `context_snapshot.json` into
/// `context_tokens`, so the number reflects the context the next request will
/// actually send instead of reading as empty right after the branch.
async fn session_summary(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let sid = SessionId(id);
    let events = state
        .registry
        .store()
        .read_events(&sid)
        .with_context(|| format!("failed to read session `{}`", sid.0));
    match events {
        Ok(events) => {
            let mut summary = monitor::summarize(&events);
            if let Ok(snapshot) = state.registry.store().read_snapshot(&sid) {
                summary.context_tokens = summary
                    .context_tokens
                    .saturating_add(snapshot_tokens(&snapshot));
            }
            Json(summary).into_response()
        }
        Err(e) => not_found(&e),
    }
}

/// Estimated token footprint of a context snapshot, using the ledger's
/// byte-heuristic so a fork's context reads on the same scale as the estimate
/// its first request will report.
fn snapshot_tokens(snapshot: &[crate::llm::Message]) -> u32 {
    let bytes: usize = snapshot.iter().map(crate::context::message_bytes).sum();
    crate::context::bytes_to_tokens(bytes)
}

/// `GET /sessions/{id}/view` — the folded conversation view: the session's
/// committed log run through the server-side conversation fold
/// (`super::view`), returned as render-ready items plus the high-water seq.
/// This is how a client OPENS a session: one request, no replay stream, no
/// actor spawn — the client renders these items, then subscribes to the live
/// stream with `Last-Event-ID: last_seq` so anything committed since the fold
/// is replayed over SSE, not lost. The fold runs on every request (no cache):
/// it is one O(events) pass over a file the summary endpoint already rereads
/// per request.
async fn session_view(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let sid = SessionId(id);
    let events = state
        .registry
        .store()
        .read_events(&sid)
        .with_context(|| format!("failed to read session `{}`", sid.0));
    match events {
        Ok(events) => Json(super::view::fold_view(&events)).into_response(),
        Err(e) => not_found(&e),
    }
}

/// `GET /sessions/{id}/snapshot` — the inherited context a non-`new` session was
/// seeded with: the `context_snapshot.json` (`Vec<Message>`) materialized at
/// fork / compaction / reconfiguration (`doc/architecture.md` §6.1, §7). The
/// frontend renders it as dimmed history above the live conversation so a
/// branched session shows what came before. A `new` session has no snapshot, so
/// this is a 404 the client treats as "no inherited context" — not an error.
async fn session_snapshot(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let sid = SessionId(id);
    match state.registry.store().read_snapshot(&sid) {
        Ok(messages) => Json(messages).into_response(),
        Err(e) => not_found(&anyhow::Error::new(e)),
    }
}

/// Query for [`fork_preview`]: the parent seq to branch at.
#[derive(Debug, Deserialize)]
struct ForkPreviewParams {
    at_seq: u64,
}

/// `GET /sessions/{id}/fork-preview?at_seq=N` — the context a fork at `at_seq`
/// WOULD inherit, computed WITHOUT creating a session. The draft branch view
/// fetches this so a fork shows its inherited history before the first send
/// (until then no real session — hence no `context_snapshot.json` — exists).
///
/// Mirrors the real fork's seeding exactly: both rebuild the parent's context
/// from events up to (and including) `at_seq` via [`rebuild_runtime`]. The
/// system prompt is omitted here (an empty seed) because the client drops System
/// messages when rendering inherited history, so the user-visible content is
/// identical to what `fork` will persist — and this stays read-only, skipping
/// the heavy agent assembly a real fork does. An `at_seq` before any event is a
/// 404 the client treats as "no inherited context".
async fn fork_preview(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ForkPreviewParams>,
) -> Response {
    let parent = SessionId(id);
    let all = match state.registry.store().read_events(&parent) {
        Ok(events) => events,
        Err(e) => return not_found(&anyhow::Error::new(e)),
    };
    let upto: Vec<_> = all.into_iter().filter(|e| e.seq <= params.at_seq).collect();
    if upto.is_empty() {
        return not_found(&anyhow::anyhow!(
            "session `{}` has no event at or before seq {}",
            parent.0,
            params.at_seq
        ));
    }
    let runtime = crate::agent::rebuild_runtime(&upto, Vec::new());
    Json(runtime.context).into_response()
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
        .runtime_info(
            meta.profile_id.as_deref(),
            meta.model.as_deref(),
            meta.workspace.as_deref(),
        )
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

    // Parse Last-Event-ID (the seq the client last saw). Replay everything after
    // it from the durable log before attaching the live broadcast.
    let last_seen: Option<u64> = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    // An archived session has no actor to spawn (`doc/session-storage.md` §9) —
    // `get_or_spawn` would 410 it. Serve its committed history replay-only
    // instead, holding the connection open with keep-alives: the read-only view
    // renders the full history without a reconnect spin, and nothing live can
    // ever arrive (a retired session produces no events).
    if state.registry.store().is_archived(&sid) {
        let stream = tokio_stream::iter(replay_events(&state.registry, &sid, last_seen))
            .chain(tokio_stream::iter([Ok(sse_from_gateway(
                &GatewayEvent::ReplayEnd,
            ))]))
            .chain(tokio_stream::pending());
        return Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response();
    }

    let handle = match state.registry.get_or_spawn(&sid).await {
        Ok(h) => h,
        Err(e) => return conflict_or_not_found(&e),
    };

    // Subscribe BEFORE reading the log. An event committed between the read and
    // the subscribe would otherwise be lost — not yet in the log at read time,
    // and already broadcast before the subscription existed. Subscribing first
    // means such an event lands in the broadcast buffer; the replay read then
    // also includes it (it committed before the read), so it is delivered twice.
    // The frontend dedups committed events by seq, making the overlap harmless.
    let live = live_event_stream(handle.subscribe());
    let replay = replay_events(&state.registry, &sid, last_seen);

    // Replay, then the ReplayEnd boundary marker, then the live stream: the
    // marker tells the client "everything folded so far is history; what
    // follows is live", which is when it presents the conversation.
    let stream = tokio_stream::iter(replay)
        .chain(tokio_stream::iter([Ok(sse_from_gateway(
            &GatewayEvent::ReplayEnd,
        ))]))
        .chain(live);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `GET /status/events` — the gateway-wide session-activity stream: one SSE
/// feed carrying every session's `running | awaiting_input | idle` status,
/// across all workspaces, so the session list lights up without subscribing to
/// each session's own event stream.
///
/// Ordering mirrors [`sse_events`]' replay-then-live: subscribe to the hub
/// **first**, then take a snapshot, then stream `snapshot ++ live`. Subscribing
/// before snapshotting means a transition landing in the gap is delivered live
/// rather than lost; the frontend applies each delta idempotently (last-write-wins
/// by session id), so a status appearing in both the snapshot and the live tail is
/// harmless. Unlike the per-session stream there is no `Last-Event-ID` resume — a
/// reconnect simply re-snapshots.
async fn status_events(State(state): State<AppState>) -> Response {
    let hub = state.registry.status_hub();
    // Subscribe before snapshot so no transition slips through the gap.
    let live = live_status_stream(hub.subscribe());
    let snapshot: Vec<Result<SseEvent, Infallible>> = hub
        .snapshot()
        .iter()
        .map(|s| Ok(sse_from_status(s)))
        .collect();

    let stream = tokio_stream::iter(snapshot).chain(live);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Adapt the hub's status broadcast into an SSE stream, dropping `Lagged` gaps
/// (a lagging client's snapshot on reconnect resyncs the full current state).
fn live_status_stream(
    rx: broadcast::Receiver<SessionStatus>,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    BroadcastStream::new(rx).filter_map(|res| res.ok().map(|s| Ok(sse_from_status(&s))))
}

/// Serialize a [`SessionStatus`] as one SSE `data:` frame. No `id:` — this stream
/// is not resumed by seq (a reconnect re-snapshots).
fn sse_from_status(status: &SessionStatus) -> SseEvent {
    let data = serde_json::to_string(status).unwrap_or_else(|_| "{}".to_owned());
    SseEvent::default().data(data)
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

/// Map a registry error to 410 (archived), 404 (not found) or 409 (locked)
/// heuristically. An archived session is `Gone` — it existed but is retired for
/// good (`doc/session-storage.md` §9). The registry surfaces a "locked or
/// missing" context for `open` failures; a clean `NotFound` from metadata reads
/// is a 404.
/// Map a registry error to a status heuristically, from the messages in its
/// whole source chain (the registry wraps the typed store error in an `anyhow`
/// context, so the distinguishing phrase is often a *cause*, not the outermost
/// message):
/// - "not archived" → 409 (must `archive` before `DELETE`, `doc/session-storage.md` §9);
/// - "archived" (retired) → 410 Gone — it existed but is retired for good;
/// - "locked" → 409 (a turn is running / another writer holds it);
/// - otherwise a clean `NotFound` → 404.
///
/// The "not archived" test runs first because its message also contains the
/// substring "archived".
fn conflict_or_not_found(e: &anyhow::Error) -> Response {
    let full = e
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    if full.contains("not archived") {
        (StatusCode::CONFLICT, Json(json!({ "error": full }))).into_response()
    } else if full.contains("archived") {
        (StatusCode::GONE, Json(json!({ "error": full }))).into_response()
    } else if full.contains("locked") {
        (StatusCode::CONFLICT, Json(json!({ "error": full }))).into_response()
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
        let registry = SessionRegistry::new(defaults, &GatewayConfig::default()).unwrap();
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

[[providers.models]]
id = "gpt-4o-mini"
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
        let registry = SessionRegistry::new(defaults, &GatewayConfig::default()).unwrap();
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
        let registry = SessionRegistry::new(defaults, &GatewayConfig::default()).unwrap();

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
    /// folds to all-zero counts — proving the endpoint replays the log through
    /// the monitor rather than 404ing.
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
        };
        let base = serve_test(state).await;

        let resp = reqwest::get(format!("{base}/api/sessions/{}/summary", sid.0))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["total_turns"], 0);
        assert_eq!(body["total_tool_calls"], 0);
        assert_eq!(body["context_tokens"], 0);
        assert!(body["tools_used"].is_object());
    }

    /// A branched session's summary folds the inherited snapshot's estimated
    /// size into `context_tokens` — the number answers "how full is the
    /// context" from the moment of the fork, not just after its first request.
    #[tokio::test]
    async fn summary_endpoint_folds_snapshot_into_context_tokens() {
        let (registry, _dir) = test_registry();
        let store = registry.store();
        let parent = store.create_new(None, None, vec![]).unwrap();
        let parent_id = parent.session_id().clone();
        drop(parent);
        let snapshot = vec![
            crate::llm::Message::System {
                content: "s".repeat(400),
            },
            crate::llm::Message::User {
                content: "u".repeat(400),
            },
        ];
        let writer = store
            .create_fork(
                store.mint_id(),
                parent_id,
                0,
                None,
                None,
                None,
                vec![],
                &snapshot,
            )
            .unwrap();
        let sid = writer.session_id().clone();
        drop(writer);

        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;

        let resp = reqwest::get(format!("{base}/api/sessions/{}/summary", sid.0))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        // No requests yet, so the whole reading is the snapshot: 800 bytes / 4.
        assert_eq!(body["context_tokens"], 200);
    }

    /// An unknown session id yields 404 from the summary endpoint, not a 500 or
    /// an empty summary.
    #[tokio::test]
    async fn summary_endpoint_unknown_session_is_404() {
        let (registry, _dir) = test_registry();
        let state = AppState {
            registry,
            api_key: None,
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

    /// The RUNTIME panel's source (`GET /sessions/{id}/runtime`) must reflect a
    /// per-session model *override*, not the profile default. This is the exact
    /// bug the panel had: a session created with `?model=` still reported the
    /// profile's default model, so the frontend flagged a spurious runtime
    /// divergence. Creating with an override and reading runtime back must return
    /// the overridden model.
    #[tokio::test]
    async fn runtime_reflects_per_session_model_override() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let client = reqwest::Client::new();

        // Create a session choosing a model that is NOT the profile default
        // (profile "coding" defaults to openai-main/gpt-4o).
        let resp = client
            .post(format!("{base}/api/sessions?model=openai-main/gpt-4o-mini"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let id = resp.json::<serde_json::Value>().await.unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned();

        // The runtime endpoint must now report the OVERRIDE, not the default.
        let resp = reqwest::get(format!("{base}/api/sessions/{id}/runtime"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let rt: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            rt["model"], "gpt-4o-mini",
            "runtime must reflect the per-session override, not the profile default"
        );
        assert_eq!(rt["provider"], "openai-main");
    }

    /// The complement of the override test: a session created with NO model
    /// override must report the profile default — proving the override is
    /// session-private and does not leak into sessions that did not ask for it
    /// (the user's explicit constraint: changing one session's model must not
    /// change every future session's model).
    #[tokio::test]
    async fn runtime_without_override_reports_profile_default() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let client = reqwest::Client::new();

        // Create a session with no override at all.
        let resp = client
            .post(format!("{base}/api/sessions"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let id = resp.json::<serde_json::Value>().await.unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let resp = reqwest::get(format!("{base}/api/sessions/{id}/runtime"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let rt: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            rt["model"], "gpt-4o",
            "with no override the runtime must be the profile default gpt-4o"
        );
    }

    /// A fork must inherit the parent's per-session model override — on BOTH the
    /// displayed runtime and the persisted meta. A fork is a branch of the same
    /// conversation; silently switching it to the profile default would change
    /// the model out from under the user. Complements the create/runtime tests
    /// by covering the derived-session path the first fix left on the default.
    #[tokio::test]
    async fn fork_inherits_parent_model_override() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let client = reqwest::Client::new();

        // Parent chosen on a non-default model.
        let parent_id = client
            .post(format!("{base}/api/sessions?model=openai-main/gpt-4o-mini"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned();

        // Fork at the parent's Created event (seq 0).
        let resp = client
            .post(format!("{base}/api/sessions/{parent_id}/fork"))
            .json(&serde_json::json!({ "at_seq": 0 }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "fork should succeed");
        let fork_id = resp.json::<serde_json::Value>().await.unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned();

        // The fork's runtime must report the inherited override, not the default.
        let rt: serde_json::Value = reqwest::get(format!("{base}/api/sessions/{fork_id}/runtime"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            rt["model"], "gpt-4o-mini",
            "fork must inherit the parent's model override on the runtime panel"
        );
    }

    /// Reconfiguring a session to a new model must persist that model on the new
    /// session (so its runtime panel and post-eviction respawn both track it),
    /// exactly like a per-session override at creation.
    #[tokio::test]
    async fn reconfigure_persists_new_model_override() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let client = reqwest::Client::new();

        // Parent on the profile default (gpt-4o).
        let parent_id = client
            .post(format!("{base}/api/sessions"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned();

        // Reconfigure to a different model.
        let resp = client
            .post(format!(
                "{base}/api/sessions/{parent_id}/reconfigure?model=openai-main/gpt-4o-mini"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "reconfigure should succeed");
        let new_id = resp.json::<serde_json::Value>().await.unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let rt: serde_json::Value = reqwest::get(format!("{base}/api/sessions/{new_id}/runtime"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            rt["model"], "gpt-4o-mini",
            "reconfigured session must report and persist the new model"
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

    /// The providers API marks built-in catalog entries and refuses to persist
    /// them: `GET` reports them under `builtin_names`, and a `PUT` reusing a
    /// built-in name is a 400, not a silent fork of the catalog.
    #[tokio::test]
    async fn builtin_providers_are_marked_and_write_protected() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;

        let body: serde_json::Value = reqwest::get(format!("{base}/api/providers"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let builtins = body["builtin_names"].as_array().unwrap();
        assert!(!builtins.is_empty());
        let builtin = builtins[0].as_str().unwrap();
        // Built-ins are present in the merged list, after user entries.
        assert!(
            body["providers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["name"] == builtin)
        );

        let put = json!({
            "providers": [{
                "name": builtin,
                "type": "openai-chat",
                "base_url": "https://example.com/v1",
                "api_key_env": "HOME",
                "models": []
            }]
        });
        let resp = reqwest::Client::new()
            .put(format!("{base}/api/providers"))
            .json(&put)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    /// `POST /providers/{name}/test` never 5xx's a *connection* failure: an
    /// unknown provider is a 404, but a known provider that cannot authenticate
    /// (no key anywhere) returns 200 with `ok: false` and the reason — the
    /// failure is the probe's result, not a server fault.
    #[tokio::test]
    async fn test_provider_reports_failure_without_crashing() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let client = reqwest::Client::new();

        // Unknown provider → 404.
        let resp = client
            .post(format!("{base}/api/providers/ghost/test"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        // A built-in provider with no key stored and no usable env fallback →
        // 200 with ok:false. We force the no-credential path by passing an
        // `edit` whose api_key_env names a var that cannot exist in any env
        // (the catalog's real env var might be set on a developer machine).
        let builtin = crate::config::builtin_provider_names().remove(0);
        let edit = serde_json::json!({
            "name": builtin,
            "type": "openai-chat",
            "base_url": "https://api.kimi.com/coding/v1",
            "api_key_env": "OMINI_DEFINITELY_UNSET_VAR_XYZ",
            "models": []
        });
        let resp = client
            .post(format!("{base}/api/providers/{builtin}/test"))
            .json(&serde_json::json!({ "edit": edit, "key": null }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["ok"], false);
        assert!(body["error"].as_str().unwrap().contains("API key"));
    }

    /// Profile CRUD round-trip: PUT creates a file, GET reads it back raw, DELETE
    /// removes it (and a second DELETE is a 404).
    #[tokio::test]
    async fn profile_put_get_delete_round_trip() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
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

    /// Archive over HTTP: a session drops out of the active list on archive
    /// (204), stays readable by id (the analysis path), and — being retired for
    /// good — refuses to run again (410 on `POST .../message`). This is the
    /// session-lifecycle close (`doc/session-storage.md` §9) and the sandbox
    /// `release` trigger (`doc/sandbox.md` §9 Q5).
    #[tokio::test]
    async fn archive_retires_session_one_way() {
        let (registry, _dir) = test_registry();
        // Create a session directly on the store (no turn ⇒ never `Running`, so
        // archive is allowed). Drop the writer to release its lock.
        let sid = {
            let writer = registry.store().create_new(None, None, vec![]).unwrap();
            let id = writer.session_id().clone();
            drop(writer);
            id
        };
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let client = reqwest::Client::new();

        let listed = |base: String| async move {
            reqwest::get(format!("{base}/api/sessions"))
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()["sessions"]
                .as_array()
                .unwrap()
                .len()
        };

        assert_eq!(listed(base.clone()).await, 1, "session is active initially");

        // Archive → 204, gone from the active list for good...
        let resp = client
            .post(format!("{base}/api/sessions/{}/archive", sid.0))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        assert_eq!(
            listed(base.clone()).await,
            0,
            "archived → not in active list"
        );

        // ...but still readable by id (the analysis path).
        let resp = reqwest::get(format!("{base}/api/sessions/{}", sid.0))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "archived session still readable by id");

        // ...yet cannot be run again — a run path is 410 Gone, not a silent
        // respawn. This is the one-way guarantee: no reviving a retired session.
        let resp = client
            .post(format!("{base}/api/sessions/{}/message", sid.0))
            .json(&json!({ "text": "hello" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 410, "archived session refuses to run");

        // Archiving an unknown session is a 404, not a silent success.
        let resp = client
            .post(format!("{base}/api/sessions/does-not-exist/archive"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    /// An archived session's event stream is replay-only: 200 (not the 410 a run
    /// path returns), streaming its committed history so the read-only view can
    /// render it. The stream then stays open with keep-alives — no live events
    /// can ever arrive on a retired session.
    #[tokio::test]
    async fn archived_session_events_stream_is_replay_only() {
        let (registry, _dir) = test_registry();
        let sid = {
            let writer = registry.store().create_new(None, None, vec![]).unwrap();
            let id = writer.session_id().clone();
            drop(writer);
            id
        };
        registry.store().archive(&sid).unwrap();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;

        let mut resp = reqwest::get(format!("{base}/api/sessions/{}/events", sid.0))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "archived history stays streamable");

        // The replay (the session's `Created` event) arrives promptly; the test
        // never blocks on the open-ended keep-alive tail.
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), resp.chunk())
            .await
            .expect("replay frame should arrive promptly")
            .expect("stream readable")
            .expect("a replay frame is present");
        let text = String::from_utf8_lossy(&chunk);
        assert!(
            text.contains(&sid.0),
            "replay must carry the session's committed events, got: {text}"
        );
    }

    /// Archiving a session whose turn is running is a 409, not a silent retire:
    /// tearing an actor down mid-turn would drop uncommitted work, so the guard
    /// forces a `cancel` first (`doc/session-storage.md` §9). We simulate the
    /// running state by publishing `Running` to the shared status hub — the same
    /// signal a live turn raises — rather than driving a real model turn.
    #[tokio::test]
    async fn archive_running_session_is_conflict() {
        let (registry, _dir) = test_registry();
        let sid = {
            let writer = registry.store().create_new(None, None, vec![]).unwrap();
            let id = writer.session_id().clone();
            drop(writer);
            id
        };
        // Mark it running on the hub the registry reads through `archive`.
        registry
            .status_hub()
            .publish(crate::gateway::SessionStatus {
                session_id: sid.clone(),
                workspace_id: WorkspaceId::none(),
                status: crate::gateway::ActivityStatus::Running,
                latest_seq: 0,
            });
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/api/sessions/{}/archive", sid.0))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409, "a running turn blocks archive");

        // And it is still active (not archived) after the rejected attempt.
        let count = reqwest::get(format!("{base}/api/sessions"))
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["sessions"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(count, 1, "rejected archive left the session active");
    }

    /// Hard-delete over HTTP is gated on archive-first: a live session's `DELETE`
    /// is a 409 ("archive it first"), and only after archiving does `DELETE`
    /// remove it (204), after which it is gone entirely (a second `DELETE` is a
    /// 404). This is the irreversible-op confirmation (`doc/session-storage.md`
    /// §9) — a one-step delete of a live session must be impossible.
    #[tokio::test]
    async fn delete_requires_archive_first_then_removes() {
        let (registry, _dir) = test_registry();
        let sid = {
            let writer = registry.store().create_new(None, None, vec![]).unwrap();
            let id = writer.session_id().clone();
            drop(writer);
            id
        };
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let client = reqwest::Client::new();

        // Deleting a live (non-archived) session is refused: 409, files intact.
        let resp = client
            .delete(format!("{base}/api/sessions/{}", sid.0))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409, "cannot hard-delete before archiving");
        let resp = reqwest::get(format!("{base}/api/sessions/{}", sid.0))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "refused delete left the session readable"
        );

        // Archive, then delete → 204, and it is gone for real.
        let resp = client
            .post(format!("{base}/api/sessions/{}/archive", sid.0))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        let resp = client
            .delete(format!("{base}/api/sessions/{}", sid.0))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204, "archived session deletes cleanly");

        // Gone: reading it and re-deleting are both 404.
        let resp = reqwest::get(format!("{base}/api/sessions/{}", sid.0))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "deleted session is gone");
        let resp = client
            .delete(format!("{base}/api/sessions/{}", sid.0))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "second delete is a 404");
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

    /// Workspace-config GC (`doc/workspace-config.md`): a config whose workspace
    /// path is gone lists as an orphan and can be explicitly deleted; the delete
    /// is idempotent. Pins the "never auto-delete, only explicit" contract.
    #[tokio::test]
    async fn workspace_config_orphan_lists_then_deletes() {
        let (registry, dir) = test_registry();
        // Register a real workspace so its id -> path is known, then plant a
        // config file for it under the gateway's trusted config dir.
        let ws = dir.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let id = registry.record_workspace(&ws).unwrap();
        let cfg_dir = dir.path().join(".omini").join("workspaces");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join(format!("{}.toml", id.0)),
            "[network]\npolicy = \"isolated\"\n",
        )
        .unwrap();

        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let client = reqwest::Client::new();

        // Workspace still exists → not an orphan.
        let resp = client
            .get(format!("{base}/api/workspaces/config/orphans"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["orphans"].as_array().unwrap().len(), 0);

        // Remove the workspace dir → the config is now an orphan.
        std::fs::remove_dir_all(&ws).unwrap();
        let body: serde_json::Value = client
            .get(format!("{base}/api/workspaces/config/orphans"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let orphans = body["orphans"].as_array().unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0]["workspace_id"], id.0);

        // Explicit delete → 204, and the orphan is gone (idempotent second call).
        for _ in 0..2 {
            let resp = client
                .delete(format!("{base}/api/workspaces/config/{}", id.0))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 204);
        }
        let body: serde_json::Value = client
            .get(format!("{base}/api/workspaces/config/orphans"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["orphans"].as_array().unwrap().len(), 0);
    }

    /// A `?model=` override that names no configured model is a CLIENT error
    /// (400), not a 500 — the user picked a stale model, not a server fault.
    #[tokio::test]
    async fn create_session_unknown_model_is_400() {
        let (registry, _dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
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
            drop(store.create_new(None, Some(ws_a.clone()), vec![]).unwrap());
        }
        drop(store.create_new(None, Some(ws_b.clone()), vec![]).unwrap());

        let state = AppState {
            registry,
            api_key: None,
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
        };
        let base = serve_test(state).await;

        let body: serde_json::Value = reqwest::get(format!("{base}/api/workspaces/{wid}/sessions"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "only workspace A's session");
        assert_eq!(sessions[0]["id"], in_a.0);
    }

    /// `GET /workspaces/{id}/sessions/archived` applies the same workspace
    /// filter as the active listing: archived sessions from other workspaces
    /// are excluded, so a panel's archived section only ever shows its own
    /// workspace's retired sessions.
    #[tokio::test]
    async fn list_archived_workspace_sessions_filters_to_the_workspace() {
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
        let in_b = store
            .create_new(None, Some(ws_b.clone()), vec![])
            .unwrap()
            .session_id()
            .clone();
        store.archive(&in_a).unwrap();
        store.archive(&in_b).unwrap();

        let wid = crate::gateway::WorkspaceId::from_path(&ws_a).0;

        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;

        let body: serde_json::Value =
            reqwest::get(format!("{base}/api/workspaces/{wid}/sessions/archived"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "only workspace A's archived session");
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
        };
        let base = serve_test(state).await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/api/workspaces/deadbeefdeadbeef/sessions"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    /// The SSE stream carries a `replay_end` frame between the replayed
    /// history and the live tail: the web client folds the burst off-screen
    /// and only presents the conversation when this boundary lands, so history
    /// never visibly scrolls past. Pinned via the archived (replay-only) path,
    /// which is the one this test registry can serve without provider config.
    #[tokio::test]
    async fn events_stream_marks_replay_boundary() {
        let (registry, _dir) = test_registry();
        let sid = {
            let writer = registry.store().create_new(None, None, vec![]).unwrap();
            let id = writer.session_id().clone();
            drop(writer);
            id
        };
        registry.store().archive(&sid).unwrap();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;

        let mut resp = reqwest::get(format!("{base}/api/sessions/{}/events", sid.0))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Collect frames until the boundary marker shows up (the live tail is
        // open-ended, so stop as soon as the marker arrives).
        let mut saw_replay_end = false;
        let mut saw_committed = false;
        for _ in 0..8 {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), resp.chunk())
                .await
                .expect("frames should arrive promptly")
                .expect("stream readable")
                .expect("a frame is present");
            let text = String::from_utf8_lossy(&chunk);
            if text.contains("\"type\":\"event\"") {
                saw_committed = true;
            }
            if text.contains("\"type\":\"replay_end\"") {
                saw_replay_end = true;
                break;
            }
        }
        assert!(
            saw_committed,
            "the replay (Created event) must stream first"
        );
        assert!(
            saw_replay_end,
            "a replay_end boundary frame must follow the replay"
        );
    }

    /// The folded view is multi-MB of compressible JSON; over a relayed link
    /// (SSH forward / reverse proxy) the transfer dominates session-open
    /// latency, so the gateway must honor `Accept-Encoding: gzip`. The
    /// compression layer only kicks in above a size threshold, so this seeds
    /// a session with a large turn input to push the view over it.
    #[tokio::test]
    async fn view_endpoint_compresses_when_accepted() {
        let (registry, _dir) = test_registry();
        let sid = {
            let mut writer = registry.store().create_new(None, None, vec![]).unwrap();
            let id = writer.session_id().clone();
            // A large committed turn input so the folded view clears the
            // compression layer's size threshold.
            let big = "x".repeat(64 * 1024);
            writer
                .append(
                    crate::core::EventSource {
                        kind: crate::core::SourceKind::Runtime,
                        id: "test".to_owned(),
                    },
                    crate::core::EventPayload::Turn(crate::core::payload::TurnEvent::Started {
                        turn_id: crate::core::TurnId("t".to_owned()),
                        input: Some(big),
                    }),
                    None,
                    None,
                )
                .unwrap();
            drop(writer);
            id
        };
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;

        let resp = reqwest::Client::new()
            .get(format!("{base}/api/sessions/{}/view", sid.0))
            .header("accept-encoding", "gzip")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // reqwest transparently decompresses, so the marker is the response
        // header the layer stamps, not the decoded body.
        assert_eq!(
            resp.headers()
                .get("content-encoding")
                .and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "the view must be gzip-compressed when the client accepts it"
        );
    }

    /// `GET /status/events` streams the gateway-wide status snapshot: a status
    /// published to the registry's hub before the client connects appears in the
    /// initial SSE frames. This is the session list's cross-session read source.
    #[tokio::test]
    async fn status_events_streams_snapshot() {
        use crate::gateway::{ActivityStatus, SessionStatus};

        let (registry, _dir) = test_registry();
        // Seed a status as if an actor had published it.
        registry.status_hub().publish(SessionStatus {
            session_id: crate::core::SessionId("sess-1".to_owned()),
            workspace_id: crate::gateway::WorkspaceId::none(),
            status: ActivityStatus::Running,
            latest_seq: 4,
        });
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;

        let mut resp = reqwest::get(format!("{base}/api/status/events"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.starts_with("text/event-stream")),
            "status stream must be SSE"
        );

        // Read the first chunk (the snapshot frame) with a timeout, so the test
        // never blocks on the open-ended stream. `chunk()` is reqwest-native — no
        // extra stream adapter crates needed.
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), resp.chunk())
            .await
            .expect("snapshot frame should arrive promptly")
            .expect("stream readable")
            .expect("a snapshot frame is present");
        let text = String::from_utf8_lossy(&chunk);
        assert!(
            text.contains("sess-1") && text.contains("running"),
            "snapshot must carry the seeded running status, got: {text}"
        );
    }

    /// `PUT /gateway/permission` then `GET` round-trips the gateway baseline gate,
    /// AND persists it to `gateway.toml`. The persisted-file assertion is the one
    /// that matters: a handler that only updated memory would pass the GET but
    /// silently lose the policy on restart (Karpathy §12).
    #[tokio::test]
    async fn gateway_permission_put_then_get_and_persists() {
        let (registry, dir) = test_registry_with_config();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let http = reqwest::Client::new();

        let policy = json!({ "deny": [{ "tool": "shell", "contains": ["curl"] }] });
        let resp = http
            .put(format!("{base}/api/gateway/permission"))
            .json(&policy)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);

        // GET returns what we PUT.
        let got: serde_json::Value = http
            .get(format!("{base}/api/gateway/permission"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(got["deny"][0]["tool"], "shell");
        assert_eq!(got["deny"][0]["contains"][0], "curl");

        // Persisted to gateway.toml under the config root (survives restart).
        let toml_text =
            std::fs::read_to_string(dir.path().join(".omini/config/gateway.toml")).unwrap();
        assert!(
            toml_text.contains("default_permission") && toml_text.contains("curl"),
            "policy must be written to gateway.toml, got: {toml_text}"
        );
    }

    /// `PUT /workspaces/{id}/config` then `GET` round-trips a workspace config
    /// (network + permission). Uses a recorded real workspace so the id resolves
    /// to a path the store can key on.
    #[tokio::test]
    async fn workspace_config_put_then_get_round_trips() {
        let (registry, dir) = test_registry_with_config();
        let ws = dir.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let id = registry.record_workspace(&ws).unwrap();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let http = reqwest::Client::new();

        let config = json!({
            "network": { "policy": "isolated" },
            "permission": { "deny": [{ "tool": "shell", "contains": ["rm -rf"] }] }
        });
        let resp = http
            .put(format!("{base}/api/workspaces/{}/config", id.0))
            .json(&config)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);

        let got: serde_json::Value = http
            .get(format!("{base}/api/workspaces/{}/config", id.0))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(got["network"]["policy"], "isolated");
        assert_eq!(got["permission"]["deny"][0]["contains"][0], "rm -rf");
    }

    /// A malformed on-disk workspace config is a 500, NOT a 404: the file exists,
    /// so reporting "workspace does not exist" would send the user chasing the
    /// wrong problem. Regression guard for the error-mapping fix.
    #[tokio::test]
    async fn workspace_config_malformed_is_500_not_404() {
        let (registry, dir) = test_registry();
        let ws = dir.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let id = registry.record_workspace(&ws).unwrap();
        // Plant a broken config file for this workspace's id.
        let cfg_dir = dir.path().join(".omini").join("workspaces");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join(format!("{}.toml", id.0)),
            "this is = not valid ][",
        )
        .unwrap();

        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let resp = reqwest::get(format!("{base}/api/workspaces/{}/config", id.0))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            500,
            "malformed config is a server error, not a 404"
        );
    }

    /// `GET /workspaces/{id}/tools` returns at least the built-in catalog for a
    /// workspace with no MCP servers — the config UI's per-workspace card source
    /// degrades to built-ins rather than erroring.
    #[tokio::test]
    async fn workspace_tools_endpoint_returns_builtins() {
        let (registry, dir) = test_registry_with_config();
        let ws = dir.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let id = registry.record_workspace(&ws).unwrap();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let body: serde_json::Value = reqwest::get(format!("{base}/api/workspaces/{}/tools", id.0))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let names: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        // Built-ins present (MCP absent → just these).
        assert!(names.contains(&"shell") && names.contains(&"read"));
    }

    /// `GET /tools` returns the built-in catalog with the fields the config UI
    /// needs (shell→command, path tools→path). A test that only checked the count
    /// would pass even if the field metadata the cards depend on were dropped.
    #[tokio::test]
    async fn tools_endpoint_returns_builtin_catalog() {
        let (registry, _dir) = test_registry();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;
        let body: serde_json::Value = reqwest::get(format!("{base}/api/tools"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let tools = body["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "find",
                "search",
                "read",
                "write",
                "edit",
                "shell",
                "web_fetch"
            ]
        );
        // shell exposes a `command` field the UI scopes rules to.
        let shell = tools.iter().find(|t| t["name"] == "shell").unwrap();
        assert_eq!(shell["fields"][0]["key"], "command");
        // web_fetch exposes a `url` field the UI scopes rules to.
        let web_fetch = tools.iter().find(|t| t["name"] == "web_fetch").unwrap();
        assert_eq!(web_fetch["fields"][0]["key"], "url");
        // write's path field is flagged is_path so the UI offers prefix controls.
        let write = tools.iter().find(|t| t["name"] == "write").unwrap();
        let path_field = write["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "path")
            .unwrap();
        assert_eq!(path_field["is_path"], true);
    }

    /// `GET /workspaces/{id}/config` for a workspace with no stored config returns
    /// the default (all-absent) shape at 200 — the editor always has something to
    /// bind, rather than a 404 it would have to special-case.
    #[tokio::test]
    async fn workspace_config_get_absent_is_default() {
        let (registry, dir) = test_registry_with_config();
        let ws = dir.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let id = registry.record_workspace(&ws).unwrap();
        let state = AppState {
            registry,
            api_key: None,
        };
        let base = serve_test(state).await;

        let resp = reqwest::get(format!("{base}/api/workspaces/{}/config", id.0))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let got: serde_json::Value = resp.json().await.unwrap();
        // Empty policy: deny/ask omitted (skip_serializing_if), network absent.
        assert!(
            got["permission"].get("deny").is_none()
                || got["permission"]["deny"].as_array().unwrap().is_empty()
        );
    }
}
