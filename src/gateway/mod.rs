//! HTTP/SSE/WebSocket gateway for Web, mobile, and external integrations.
//! Feature-gated (`gateway`). See `doc/architecture.md` §18 and `doc/gateway.md`.
//!
//! The gateway is the single backend for every non-TUI front-end. It runs as a
//! user-level service (`ominiforge serve`); the TUI/CLI talk to the core
//! directly and never go through it. The core stays UI-agnostic — the gateway
//! is one more consumer of the same [`Agent`], [`SessionStore`], and event
//! stream the CLI uses (`doc/architecture.md` §2.1).
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
mod config;
mod registry;
mod server;

pub use config::GatewayConfig;
pub use registry::{SessionDefaults, SessionRegistry};
pub use server::serve;
