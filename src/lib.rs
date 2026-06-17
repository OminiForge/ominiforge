//! Ominiforge — a high-performance, extensible Rust agent platform.
//!
//! The core runtime is UI-agnostic: it executes tasks, manages session state,
//! and emits a unified event stream that every front-end (CLI / TUI / Web)
//! consumes. See `doc/architecture.md` for the full design and `doc/todo.md`
//! for the phased implementation plan.

pub mod core;

// Subsystems below are scaffolded and filled in incrementally per the phase
// plan in `doc/todo.md`. Dependency direction (see `doc/workspace-plan.md`):
// everything points down to `core`; `core` depends on nothing above it.
pub mod agent;
pub mod cli;
pub mod config;
pub mod context;
pub mod evolution;
pub mod gateway;
pub mod hook;
pub mod llm;
pub mod mcp;
pub mod memory;
pub mod monitor;
pub mod provider;
pub mod session;
pub mod skill;
pub mod tool;
pub mod tui;
