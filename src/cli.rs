//! CLI: command parsing and dispatch.
//!
//! Phase 1 ships a single `run` subcommand that executes one turn against an
//! OpenAI-compatible provider configured via environment variables. This is
//! interim wiring: profile and `providers.toml`-based configuration arrive in
//! Phase 3 (`doc/profile.md`). See `doc/architecture.md` §3.1.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::agent::{Agent, AgentConfig};
use crate::llm::Message;
use crate::provider::OpenAiProvider;
use crate::session::SessionStore;
use crate::tool::{ToolRegistry, register_builtin};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_PROVIDER_NAME: &str = "openai";
const DEFAULT_SYSTEM_PROMPT: &str = "You are Ominiforge, a capable software agent. Use the available tools to \
     accomplish the user's task, and explain what you did.";
const SESSIONS_SUBDIR: &str = ".omini/sessions";

/// Ominiforge command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "ominiforge",
    version,
    about = "A high-performance agent platform"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a single agent turn against the configured model.
    Run(RunArgs),
}

/// Arguments for `ominiforge run`.
#[derive(Debug, Parser)]
struct RunArgs {
    /// The instruction to send to the agent.
    prompt: String,

    /// Workspace directory the agent operates in (default: current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// Sampling temperature.
    #[arg(long, default_value_t = 0.0)]
    temperature: f32,
}

/// Parse arguments and dispatch. The binary entry point calls this.
///
/// # Errors
/// Surfaces configuration, provider, and session errors to the process exit.
pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run_turn(args).await,
    }
}

async fn run_turn(args: RunArgs) -> Result<()> {
    let config = ProviderEnv::from_env()?;
    let workspace = resolve_workspace(args.workspace)?;

    let provider = Arc::new(OpenAiProvider::new(
        config.provider_name,
        config.base_url,
        config.api_key,
    ));

    let mut tools = ToolRegistry::new();
    register_builtin(&mut tools, workspace.clone());
    let tool_names = tools.descriptors().into_iter().map(|d| d.name).collect();

    let agent = Agent::new(
        provider,
        tools,
        AgentConfig {
            model: config.model,
            temperature: args.temperature,
            max_tokens: None,
            tool_timeout: Duration::from_secs(120),
            ..AgentConfig::default()
        },
    );

    let store = SessionStore::new(workspace.join(SESSIONS_SUBDIR));
    let mut writer = store
        .create_new(None, Some(workspace.clone()), tool_names)
        .context("failed to create session")?;
    eprintln!("session {} ({})", writer.session_id(), workspace.display());

    let mut context = vec![Message::System {
        content: config.system_prompt,
    }];

    let outcome = agent
        .run_turn(&mut writer, &mut context, args.prompt)
        .await
        .context("agent turn failed")?;

    // The answer is the program's output (stdout); diagnostics go to stderr.
    println!("{}", outcome.answer);
    eprintln!(
        "[{} round(s), stop: {:?}]",
        outcome.rounds, outcome.stop_reason
    );
    Ok(())
}

/// Provider configuration drawn from the environment (Phase 1 interim).
struct ProviderEnv {
    provider_name: String,
    base_url: String,
    api_key: String,
    model: String,
    system_prompt: String,
}

impl ProviderEnv {
    fn from_env() -> Result<Self> {
        let api_key = std::env::var("OMINI_API_KEY")
            .context("OMINI_API_KEY is required (the model provider API key)")?;
        let model =
            std::env::var("OMINI_MODEL").context("OMINI_MODEL is required (e.g. gpt-4o)")?;
        Ok(Self {
            provider_name: env_or("OMINI_PROVIDER_NAME", DEFAULT_PROVIDER_NAME),
            base_url: env_or("OMINI_BASE_URL", DEFAULT_BASE_URL),
            api_key,
            model,
            system_prompt: env_or("OMINI_SYSTEM_PROMPT", DEFAULT_SYSTEM_PROMPT),
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// Resolve and validate the workspace directory, canonicalizing to an absolute
/// path (the tool layer's escape checks compare against it).
fn resolve_workspace(requested: Option<PathBuf>) -> Result<PathBuf> {
    let dir = match requested {
        Some(path) => path,
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    dir.canonicalize()
        .with_context(|| format!("workspace does not exist: {}", dir.display()))
}
