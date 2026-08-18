//! CLI: command parsing and dispatch.
//!
//! The command line is the operator's entry point, not a chat front-end.
//! Configuration is managed via structured config files edited directly or
//! through a facade; session analysis is queryable/exportable (see
//! `doc/design/monitor.md`). API keys are never stored in config: a provider
//! names an env var via `api_key_env`, and the key is read from the environment.
//! See `doc/design/runtime-architecture.md`.

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};


/// Ominiforge command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "ominiforge",
    version,
    about = "A high-performance agent platform"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Directory whose `.omini/` holds config (providers, profiles, mcp, hooks).
    /// Highest-priority config root: `--config-dir` → launch cwd → `~`. Config is
    /// independent of a session's workspace (`doc/design/runtime-architecture.md` §15).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {}

/// Parse arguments and dispatch. The binary entry point calls this.
///
/// # Errors
/// Surfaces configuration, provider, and session errors to the process exit.
pub fn run() -> Result<()> {
    // Operator diagnostics to stderr. Business/agent events never go here —
    // they belong to `events.jsonl` (doc/design/runtime-architecture.md). Default to
    // `info` for our own crate; RUST_LOG overrides (e.g. `RUST_LOG=debug`).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ominiforge=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        None => {
            // The command line is not a chat front-end; with no subcommand,
            // show what the operator commands are.
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

