//! Ominiforge — a high-performance, extensible Rust agent runtime.
//!
//! The core is UI-agnostic: it executes tasks, manages session state, and
//! emits a unified event stream that facades (editors/IM/TUI) consume. The
//! system is being restructured toward a thin composition runtime + plugins;
//! see `doc/design/runtime-architecture.md` for the target architecture and
//! `doc/decisions/architecture-direction.md` for the rationale.
//!
//! The library target keeps the historical name `ominiforge` so module paths
//! stay valid during the restructure.

pub mod core;

// Dependency direction: everything points down to `core`; `core` depends on
// nothing above it.
pub mod agent;
pub mod agents_md;
pub mod app;
pub mod config;
pub mod context;
pub mod env;
pub mod format;
pub mod gateway;
pub mod hook;
pub mod llm;
pub mod lsp;
pub mod mcp;
pub mod memory;
pub mod monitor;
pub mod permission;
pub(crate) mod process_env;
pub mod provider;
pub mod sandbox;
pub mod secrets;
pub mod session;
pub mod skill;
pub mod tool;
