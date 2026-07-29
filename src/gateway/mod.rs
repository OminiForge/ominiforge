//! HTTP/SSE/WebSocket gateway for Web, mobile, and external integrations.
//! See `doc/architecture.md` §18 and `doc/gateway.md`.
//!
//! The gateway is the single backend for every interactive front-end. It runs
//! as a user-level service (`ominiforge serve`); the core stays UI-agnostic —
//! the gateway is a consumer of the same [`Agent`], [`SessionStore`], and
//! event stream (`doc/architecture.md` §2.1).
//!
//! ## Shape
//!
//! A session is live in exactly one place (an OS file lock guards the event
//! log), so many network clients fan into one owner per session — a
//! [`SessionActor`]. The [`SessionRegistry`] maps a session id to its actor and
//! spawns one on demand. The [`server`] exposes REST for control plane
//! (list/create/fork/message/cancel/compact) and SSE + WebSocket for the live
//! event stream.
//!
//! [`Agent`]: crate::agent::Agent
//! [`SessionStore`]: crate::session::SessionStore
//! [`SessionActor`]: actor::SessionActor
//! [`SessionRegistry`]: registry::SessionRegistry

mod actor;
mod approval;
mod config;
mod registry;
mod server;
mod status;
pub mod view;
mod workspace;
pub(crate) mod workspace_config;

pub use config::GatewayConfig;
pub use registry::{RuntimeInfo, SessionDefaults, SessionRegistry, WorkspaceConfigError};
pub use server::serve;
pub use status::{ActivityStatus, SessionStatus, StatusHub};
pub use workspace::{WorkspaceId, WorkspaceSummary, group_sessions};
