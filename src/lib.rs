//! Ominiforge — a high-performance, extensible Rust agent platform.
//!
//! The core runtime is UI-agnostic: it executes tasks, manages session state,
//! and emits a unified event stream that every front-end (Web / desktop /
//! mobile, all via the gateway) consumes. See `doc/architecture.md` for the
//! full design.

pub mod core;

// Dependency direction: everything points down to `core`; `core` depends on
// nothing above it.
pub mod agent;
pub mod agents_md;
pub mod app;
pub mod cli;
pub mod config;
pub mod context;
pub mod env;
pub mod eval;
pub mod evolution;
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
