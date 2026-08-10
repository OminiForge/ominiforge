//! CLI: command parsing and dispatch.
//!
//! The command line is the operator's entry point, not a chat front-end:
//! `ominiforge serve` runs the gateway (the single backend every interactive
//! front-end talks to). Configuration is managed via Lua config files + the GPUI
//! settings panel (see `doc/config-lua.md`); session analysis happens in the GPUI
//! monitor panel (see `doc/gpui-app.md`). API keys are never stored in config: a
//! provider names an env var via `api_key_env`, and the key is read from the
//! environment. See `doc/architecture.md` §3.1, §15.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

use ominiforge::app::{self, DEFAULT_PROFILE};
use ominiforge::config::ConfigStore;

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
    /// independent of a session's workspace (`doc/architecture.md` §15).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the gateway server (HTTP/SSE/WebSocket) in the foreground.
    Serve(ServeArgs),
}

/// Arguments for `ominiforge serve`.
#[derive(Debug, Parser)]
struct ServeArgs {
    /// Workspace the gateway's sessions operate in (default: current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// Profile new sessions are created with.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    profile: String,

    /// Bind address, overriding `gateway.toml` (host:port).
    #[arg(long)]
    bind: Option<String>,

    /// Do not auto-load a `.env` file; use only the existing environment.
    #[arg(long)]
    no_dotenv: bool,
}

/// Parse arguments and dispatch. The binary entry point calls this.
///
/// # Errors
/// Surfaces configuration, provider, and session errors to the process exit.
pub async fn run() -> Result<()> {
    // Operator diagnostics to stderr. Business/agent events never go here —
    // they belong to `events.jsonl` (doc/architecture.md). Default to
    // `info` for our own crate; RUST_LOG overrides (e.g. `RUST_LOG=debug`).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ominiforge=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let config_dir = cli.config_dir;
    match cli.command {
        None => {
            // The command line is not a chat front-end; with no subcommand,
            // show what the operator commands are.
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
        Some(Command::Serve(args)) => serve_cmd(config_dir, args).await,
    }
}

/// Run the gateway server in the foreground (`doc/architecture.md` §18.1). A
/// systemd user service wraps this; for development it runs directly.
async fn serve_cmd(config_dir: Option<PathBuf>, args: ServeArgs) -> Result<()> {
    use ominiforge::gateway::{GatewayConfig, SessionDefaults, SessionRegistry, serve};

    let workspace = match args.workspace {
        Some(path) => path,
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    let workspace = app::resolve_workspace(&workspace)?;

    // Config roots come from --config-dir / launch cwd / home — NOT the
    // workspace (`doc/architecture.md` §15). Launch cwd is the directory the
    // server was started in.
    let launch_cwd = std::env::current_dir().context("cannot determine current directory")?;
    let config_store = ConfigStore::discover_with(config_dir.as_deref(), &launch_cwd);
    if !args.no_dotenv {
        app::load_dotenv(config_store.roots(), &workspace);
    }

    let mut gateway_config =
        GatewayConfig::load(config_store.roots()).context("failed to load gateway.toml")?;
    if let Some(bind) = args.bind {
        gateway_config.bind = bind;
    }

    let authenticated = gateway_config.resolve_api_key().is_some();
    tracing::info!(bind = %gateway_config.bind, "ominiforge gateway listening");
    if authenticated {
        tracing::info!(
            "auth: bearer token required (from ${})",
            gateway_config.api_key_env.as_deref().unwrap_or("?")
        );
    } else {
        tracing::warn!(
            "auth: DISABLED — no api_key_env configured. Only safe behind \
             loopback + a trusted reverse proxy (doc/architecture.md §18)."
        );
    }

    let defaults = SessionDefaults {
        config: config_store,
        workspace,
        profile: args.profile,
        no_dotenv: args.no_dotenv,
    };
    let registry = SessionRegistry::new(defaults, &gateway_config)?;
    serve(registry, &gateway_config).await
}
