//! CLI: command parsing and dispatch.
//!
//! `ominiforge run` executes one turn, drawing all model/provider settings from
//! config files (`.omini/config/providers.toml` + `.omini/profiles/*.toml`, see
//! `doc/profile.md`) rather than hardcoded constants. `ominiforge init`
//! scaffolds those files. API keys are never stored in config: a provider names
//! an env var via `api_key_env`, and the key is read from the environment.
//! See `doc/architecture.md` §3.1, §15.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::agent::{BlockKind, SessionRuntime, StreamSink};
use crate::app::{self, DEFAULT_PROFILE, SESSIONS_SUBDIR};
use crate::config::ConfigStore;
use crate::core::payload::TurnFailureReason;
use crate::llm::Message;
use crate::session::SessionStore;

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

    /// Open the interactive session picker on startup instead of starting a
    /// fresh session. Only meaningful for the bare `ominiforge` (TUI) command.
    #[arg(long, global = true)]
    resume: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a single agent turn against the configured model.
    Run(RunArgs),
    /// Print a derived metrics summary for a session (offline, from its log).
    Inspect(InspectArgs),
    /// Scaffold `.omini/` config files (providers + a default profile).
    Init(InitArgs),
    /// Run the gateway server (HTTP/SSE/WebSocket) in the foreground.
    #[cfg(feature = "gateway")]
    Serve(ServeArgs),
}

/// Arguments for `ominiforge serve`.
#[cfg(feature = "gateway")]
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

/// Arguments for `ominiforge run`.
#[derive(Debug, Parser)]
struct RunArgs {
    /// The instruction to send to the agent.
    prompt: String,

    /// Workspace directory the agent operates in (default: current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// Profile to run (looked up in `.omini/profiles/<name>.toml`).
    #[arg(long, default_value = DEFAULT_PROFILE)]
    profile: String,

    /// Model reference (`provider/model` or `model`), overriding the profile.
    #[arg(long)]
    model: Option<String>,

    /// Sampling temperature, overriding the profile and model default.
    #[arg(long)]
    temperature: Option<f32>,

    /// Do not auto-load a `.env` file; use only the existing environment.
    #[arg(long)]
    no_dotenv: bool,
}

/// Arguments for `ominiforge init`.
#[derive(Debug, Parser)]
struct InitArgs {
    /// Directory to scaffold `.omini/` under (default: current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// Overwrite existing config files instead of skipping them.
    #[arg(long)]
    force: bool,
}

/// Arguments for `ominiforge inspect`.
#[derive(Debug, Parser)]
struct InspectArgs {
    /// The session id to inspect (a directory under `.omini/sessions`).
    session_id: String,

    /// Workspace whose sessions to read (default: current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// Do not auto-load a `.env` file; use only the existing environment.
    #[arg(long)]
    no_dotenv: bool,
}

/// Parse arguments and dispatch. The binary entry point calls this.
///
/// # Errors
/// Surfaces configuration, provider, and session errors to the process exit.
pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config_dir = cli.config_dir;
    match cli.command {
        None => tui_main(config_dir, cli.resume).await,
        Some(Command::Run(args)) => run_turn(config_dir, args).await,
        Some(Command::Inspect(args)) => inspect(config_dir.as_deref(), &args),
        Some(Command::Init(args)) => init(&args),
        #[cfg(feature = "gateway")]
        Some(Command::Serve(args)) => serve_cmd(config_dir, args).await,
    }
}

/// Run the gateway server in the foreground (`doc/architecture.md` §18.1). A
/// systemd user service wraps this; for development it runs directly.
#[cfg(feature = "gateway")]
async fn serve_cmd(config_dir: Option<PathBuf>, args: ServeArgs) -> Result<()> {
    use crate::gateway::{GatewayConfig, SessionDefaults, SessionRegistry, serve};

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
        app::load_dotenv(config_store.roots(), &workspace, &|msg| eprintln!("{msg}"));
    }

    let mut gateway_config =
        GatewayConfig::load(config_store.roots()).context("failed to load gateway.toml")?;
    if let Some(bind) = args.bind {
        gateway_config.bind = bind;
    }

    let authenticated = gateway_config.resolve_api_key().is_some();
    eprintln!("ominiforge gateway listening on {}", gateway_config.bind);
    if authenticated {
        eprintln!(
            "auth: bearer token required (from ${})",
            gateway_config.api_key_env.as_deref().unwrap_or("?")
        );
    } else {
        eprintln!(
            "auth: DISABLED — no api_key_env configured. Only safe behind \
             loopback + a trusted reverse proxy (doc/architecture.md §18.1)."
        );
    }

    // Pricing for the monitor summary endpoint; best-effort (empty = unpriced).
    let pricing = config_store
        .load_providers()
        .and_then(|providers| config_store.load_pricing(&providers))
        .unwrap_or_default();

    let defaults = SessionDefaults {
        config: config_store,
        workspace,
        profile: args.profile,
        no_dotenv: args.no_dotenv,
    };
    let registry = SessionRegistry::new(defaults, &gateway_config);
    serve(registry, &gateway_config, pricing).await
}

async fn tui_main(config_dir: Option<PathBuf>, resume: bool) -> Result<()> {
    let prep = prepare(config_dir, None, DEFAULT_PROFILE, None, None, false).await?;
    let system = vec![Message::System {
        content: prep.system_prompt.clone(),
    }];
    crate::tui::run(
        prep.agent,
        prep.session_store,
        system,
        prep.profile_name,
        prep.tool_names,
        prep.workspace,
        prep.resolved,
        prep.mcp_clients,
        resume,
    )
    .await
}

// __APPEND_MARKER__

/// Everything `run` and the TUI need once config is resolved. This is just
/// [`app::Assembled`] — the model/provider/tool selection lives in the
/// UI-agnostic [`crate::app`] layer so the gateway and scheduler reuse it.
type Prepared = app::Assembled;

/// Resolve config and build the agent for an entry point, routing non-fatal
/// diagnostics to stderr (the CLI's log). `workspace = None` means the current
/// directory (tool sandbox); config roots come from `config_dir` / launch cwd /
/// home, independent of the workspace (`doc/architecture.md` §15). Thin wrapper
/// over [`app::assemble`].
async fn prepare(
    config_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
    profile_name: &str,
    model: Option<&str>,
    temperature: Option<f32>,
    no_dotenv: bool,
) -> Result<Prepared> {
    let launch_cwd = std::env::current_dir().context("cannot determine current directory")?;
    let config = ConfigStore::discover_with(config_dir.as_deref(), &launch_cwd);
    let workspace = workspace.unwrap_or_else(|| launch_cwd.clone());
    app::assemble(
        &config,
        workspace,
        profile_name,
        model,
        temperature,
        no_dotenv,
        &|msg| eprintln!("{msg}"),
    )
    .await
}

async fn run_turn(config_dir: Option<PathBuf>, args: RunArgs) -> Result<()> {
    let prep = prepare(
        config_dir,
        args.workspace,
        &args.profile,
        args.model.as_deref(),
        args.temperature,
        args.no_dotenv,
    )
    .await?;

    let mut writer = prep
        .session_store
        .create_new(
            Some(prep.profile_name.clone()),
            Some(prep.workspace.clone()),
            prep.tool_names.clone(),
        )
        .context("failed to create session")?;
    eprintln!(
        "session {} (profile: {}, model: {}/{}, workspace: {})",
        writer.session_id(),
        prep.profile_name,
        prep.resolved.provider_name,
        prep.resolved.model_id,
        prep.workspace.display()
    );

    let mut runtime = SessionRuntime::new(vec![Message::System {
        content: prep.system_prompt.clone(),
    }]);

    let mut sink = CliSink::new();
    let outcome = prep
        .agent
        .run_turn_with_sink(&mut writer, &mut runtime, args.prompt, &mut sink)
        .await
        .context("agent turn failed")?;

    report_turn(&outcome);
    Ok(())
}

/// Print the per-turn footer (rounds / stop reason / token usage) and, if the
/// turn was cut short, a loud-but-nonfatal warning explaining why. Used by the
/// single-turn `run` command (the TUI renders its own footer).
fn report_turn(outcome: &crate::agent::TurnOutcome) {
    eprintln!(
        "[{} round(s), stop: {:?}, tokens: {}in/{}out]",
        outcome.rounds,
        outcome.stop_reason,
        outcome.usage.input_tokens,
        outcome.usage.output_tokens,
    );

    // Context-window usage: where the running estimate sits against the
    // compaction limit. With an unknown window (no limit) just show the count.
    // Crossing the limit prints a heads-up; single-turn `run` has no loop to
    // compact, so the note stands on its own.
    match outcome.context_limit {
        Some(limit) => {
            let pct = if limit == 0 {
                100
            } else {
                (u64::from(outcome.context_tokens) * 100 / u64::from(limit)).min(999)
            };
            eprintln!(
                "[context: ~{} / {} tokens ({pct}%)]",
                outcome.context_tokens, limit
            );
            if outcome.context_tokens >= limit {
                eprintln!(
                    "warning: context is at or over the compaction threshold \
                     (~{} ≥ {limit} tokens).",
                    outcome.context_tokens
                );
            }
        }
        None => eprintln!(
            "[context: ~{} tokens (window unknown)]",
            outcome.context_tokens
        ),
    }

    // A turn that ran out of round budget or stalled on its plan is not a crash:
    // the work it did already landed. Warn loudly, but still continue so partial
    // output (files written, the streamed answer) is not thrown away.
    if let Some(reason) = &outcome.incomplete {
        let detail = match reason {
            TurnFailureReason::MaxRoundsExceeded { max_rounds } => format!(
                "hit the {max_rounds}-round safety limit before giving a final \
                 answer. Any files it wrote still stand; re-run to continue, or \
                 raise max_rounds if the task is genuinely this long."
            ),
            TurnFailureReason::PlanStalled { incomplete_steps } => format!(
                "stopped with {incomplete_steps} plan step(s) unfinished after \
                 repeated nudges. Check the plan in the session log."
            ),
            TurnFailureReason::BlockedByHook { by, reason } => {
                format!("blocked by the `{by}` hook before any model round ran: {reason}")
            }
        };
        eprintln!("warning: turn did not complete cleanly — {detail}");
    }
}

/// Streams a turn's output as it arrives, splitting it across two writers.
///
/// The model's answer text goes to the **out** writer unstyled, so it can be
/// piped or captured cleanly. Reasoning and tool activity go to the **err**
/// writer, dimmed when it is a TTY, so they read as side-channel progress and
/// never pollute the captured answer. Tracks the open channel so it can bracket
/// the side-channel with newlines and reset styling at the right moments.
///
/// Generic over the two writers so it can be driven with in-memory buffers in
/// tests; [`CliSink::new`] wires the real `stdout`/`stderr` for the CLI.
struct CliSink<O: Write, E: Write> {
    /// Answer channel (stdout in production).
    out: O,
    /// Side channel — reasoning + tool activity (stderr in production).
    err: E,
    /// Whether `err` is a terminal (enables ANSI dimming for the side-channel).
    stderr_tty: bool,
    /// The channel currently streaming, to manage spacing/styling transitions.
    channel: Channel,
    /// Whether any answer text has been written to `out` yet.
    wrote_answer: bool,
}

/// Which output channel the sink last wrote to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel {
    None,
    Answer,
    Side,
}

impl CliSink<std::io::Stdout, std::io::Stderr> {
    /// Build the production sink writing to the real `stdout`/`stderr`.
    fn new() -> Self {
        Self {
            out: std::io::stdout(),
            err: std::io::stderr(),
            stderr_tty: std::io::stderr().is_terminal(),
            channel: Channel::None,
            wrote_answer: false,
        }
    }
}

impl<O: Write, E: Write> CliSink<O, E> {
    /// Switch to the answer channel, closing the side-channel (with a newline +
    /// style reset) if it was open.
    fn enter_answer(&mut self) {
        if self.channel == Channel::Side {
            self.end_side();
        }
        self.channel = Channel::Answer;
    }

    /// Switch to the side-channel, opening dim styling if it is a TTY. Answer
    /// text on `out` is left as-is (the streams are independent).
    fn enter_side(&mut self) {
        if self.channel != Channel::Side {
            if self.stderr_tty {
                let _ = write!(self.err, "\x1b[2m");
            }
            self.channel = Channel::Side;
        }
    }

    /// Start a fresh side-channel block with `label`.
    ///
    /// If a side block is already open, its line is closed first so blocks do
    /// not run together. This cannot be left to `on_block_stop`: the OpenAI
    /// assembler batches every `BlockStop` at the end of the round (see
    /// `provider::openai::wire`), so consecutive blocks within one round —
    /// reasoning then a tool call — never see a stop between them.
    fn begin_side(&mut self, label: &str) {
        if self.channel == Channel::Side {
            self.end_side();
        } else if self.channel == Channel::Answer {
            // Close the answer line on stdout so the side-channel label
            // doesn't visually glue itself onto the end of the answer text.
            let _ = writeln!(self.out);
        }
        if self.stderr_tty {
            let _ = write!(self.err, "\x1b[2m");
        }
        self.channel = Channel::Side;
        let _ = write!(self.err, "{label}");
        let _ = self.err.flush();
    }

    /// Close the side-channel: reset styling and break the line.
    fn end_side(&mut self) {
        if self.stderr_tty {
            let _ = write!(self.err, "\x1b[0m");
        }
        let _ = writeln!(self.err);
        let _ = self.err.flush();
    }
}

impl<O: Write + Send, E: Write + Send> StreamSink for CliSink<O, E> {
    fn on_block_start(&mut self, _index: u32, block: BlockKind<'_>) {
        match block {
            BlockKind::Text => self.enter_answer(),
            BlockKind::Reasoning => self.begin_side("[thinking] "),
            BlockKind::ToolCall { name } => self.begin_side(&format!("[tool: {name}] ")),
        }
    }

    fn on_text(&mut self, _index: u32, text: &str) {
        if self.channel != Channel::Answer {
            self.enter_answer();
        }
        let _ = write!(self.out, "{text}");
        let _ = self.out.flush();
        self.wrote_answer = self.wrote_answer || !text.is_empty();
    }

    fn on_reasoning(&mut self, _index: u32, text: &str) {
        self.enter_side();
        let _ = write!(self.err, "{text}");
        let _ = self.err.flush();
    }

    fn on_tool_call_delta(&mut self, _index: u32, json_delta: &str) {
        self.enter_side();
        let _ = write!(self.err, "{json_delta}");
        let _ = self.err.flush();
    }

    fn on_block_stop(&mut self, _index: u32) {
        // Close a side-channel block so the next one starts on its own line;
        // answer text keeps flowing until the turn ends.
        if self.channel == Channel::Side {
            self.end_side();
            self.channel = Channel::None;
        }
    }

    fn on_turn_end(&mut self) {
        if self.channel == Channel::Side {
            self.end_side();
        } else if self.wrote_answer {
            // Terminate the streamed answer line on `out`.
            let _ = writeln!(self.out);
            let _ = self.out.flush();
        }
        self.channel = Channel::None;
    }
}

// __APPEND_MARKER2__

/// Print a derived metrics summary for a session, computed offline by replaying
/// its `events.jsonl` through the monitor (`doc/monitor.md` §8). Pricing comes
/// from `providers.toml` + `pricing.toml`, so cost reflects current prices, not
/// whatever was in effect when the session ran.
fn inspect(config_dir: Option<&Path>, args: &InspectArgs) -> Result<()> {
    let requested = match args.workspace.clone() {
        Some(path) => path,
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    let workspace = app::resolve_workspace(&requested)?;
    // Config (providers + pricing) comes from --config-dir / launch cwd / home,
    // independent of the session's workspace (`doc/architecture.md` §15).
    let launch_cwd = std::env::current_dir().context("cannot determine current directory")?;
    let config = ConfigStore::discover_with(config_dir, &launch_cwd);
    if !args.no_dotenv {
        app::load_dotenv(config.roots(), &workspace, &|msg| eprintln!("{msg}"));
    }

    // Pricing is best-effort: a missing/empty table just means cost is unpriced.
    let pricing = config
        .load_providers()
        .and_then(|providers| config.load_pricing(&providers))
        .unwrap_or_default();

    let store = SessionStore::new(workspace.join(SESSIONS_SUBDIR));
    let sid = crate::core::SessionId(args.session_id.clone());
    let events = store
        .read_events(&sid)
        .with_context(|| format!("failed to read session `{}`", args.session_id))?;

    let summary = crate::monitor::summarize(&events, pricing);
    print_summary(&args.session_id, &summary);
    Ok(())
}

/// Render a [`SessionSummary`](crate::monitor::SessionSummary) to stdout.
fn print_summary(session_id: &str, s: &crate::monitor::SessionSummary) {
    println!("session {session_id}");
    println!("  turns:          {}", s.total_turns);
    println!("  model requests: {}", s.total_model_requests);
    println!(
        "  tool calls:     {} ({} failed)",
        s.total_tool_calls, s.total_tool_failures
    );
    println!(
        "  tokens:         {} in / {} out",
        s.total_input_tokens, s.total_output_tokens
    );
    println!(
        "  cache hit rate: {:.1}% ({} read tokens)",
        s.cache_hit_rate * 100.0,
        s.total_cache_read_tokens
    );
    match s.cost_usd {
        Some(cost) => println!("  cost:           ${cost:.4}"),
        None => println!("  cost:           (unpriced — no pricing for the model(s) used)"),
    }
    if !s.tools_used.is_empty() {
        let mut tools: Vec<_> = s.tools_used.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let rendered: Vec<String> = tools.iter().map(|(n, c)| format!("{n}×{c}")).collect();
        println!("  tools used:     {}", rendered.join(", "));
    }
    if !s.errors.is_empty() {
        let mut errors: Vec<_> = s.errors.iter().collect();
        errors.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let rendered: Vec<String> = errors.iter().map(|(c, n)| format!("{c}×{n}")).collect();
        println!("  errors:         {}", rendered.join(", "));
    }
}

/// Scaffold `.omini/config/providers.toml` and `.omini/profiles/default.toml`.
fn init(args: &InitArgs) -> Result<()> {
    let base = match &args.workspace {
        Some(path) => path.clone(),
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    let omini = base.join(".omini");
    let config_dir = omini.join("config");
    let profiles_dir = omini.join("profiles");
    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("failed to create {}", config_dir.display()))?;
    std::fs::create_dir_all(&profiles_dir)
        .with_context(|| format!("failed to create {}", profiles_dir.display()))?;

    write_scaffold(
        &config_dir.join("providers.toml"),
        PROVIDERS_TEMPLATE,
        args.force,
    )?;
    write_scaffold(
        &profiles_dir.join("default.toml"),
        PROFILE_TEMPLATE,
        args.force,
    )?;

    #[cfg(feature = "gateway")]
    write_scaffold(
        &config_dir.join("gateway.toml"),
        GATEWAY_TEMPLATE,
        args.force,
    )?;

    eprintln!(
        "scaffolded {}\n  edit config/providers.toml, set the api_key_env vars, then:\n  ominiforge run \"your prompt\"",
        omini.display()
    );
    Ok(())
}

/// Write `contents` to `path`, skipping (unless `force`) if it already exists.
fn write_scaffold(path: &Path, contents: &str, force: bool) -> Result<()> {
    if path.exists() && !force {
        eprintln!("skip (exists): {}", path.display());
        return Ok(());
    }
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    eprintln!("wrote: {}", path.display());
    Ok(())
}

/// Starter `providers.toml`. Keys are referenced by env-var name, never inlined.
const PROVIDERS_TEMPLATE: &str = r#"# Provider + model definitions. See doc/profile.md §2.
# API keys are NOT stored here: `api_key_env` names an environment variable
# that holds the key (set it in your shell or a git-ignored .env file).

[[providers]]
name = "openai-main"
type = "openai-chat"                  # openai-chat is the only wired type today
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[[providers.models]]
id = "gpt-4o"
context_window = 128000
max_output_tokens = 16384
default_temperature = 0.0
pricing = { input_per_million = 2.50, output_per_million = 10.00, cache_read_per_million = 1.25 }

# Any OpenAI-compatible endpoint works (local servers, third parties, Xiaomi
# MiMo via an OpenAI-shaped gateway, ...). Example:
#
# [[providers]]
# name = "xiaomi-local"
# type = "openai-chat"
# base_url = "http://localhost:8080/v1"
# api_key_env = "XIAOMI_MIMO_API_KEY"
#
# [[providers.models]]
# id = "mimo-7b"
# context_window = 32000
# max_output_tokens = 8192
# default_temperature = 0.7
"#;

/// Starter `default.toml` profile. Points at the example provider/model above.
const PROFILE_TEMPLATE: &str = r#"# The default agent profile. See doc/profile.md §3.

[profile]
name = "default"
description = "Default agent profile"

[prompt]
system = """
You are Ominiforge, a capable software agent. Use the available tools to
accomplish the user's task, and explain what you did.
"""

[model]
default = "openai-main/gpt-4o"        # provider_name/model_id

[tools]
builtin = ["read", "write", "edit", "shell"]
"#;

/// Starter `gateway.toml`. Auth is off until you uncomment `api_key_env` and set
/// the named env var; the gateway binds loopback so it is only reachable behind a
/// reverse proxy. See doc/gateway.md.
#[cfg(feature = "gateway")]
const GATEWAY_TEMPLATE: &str = r#"# Gateway server config. See doc/gateway.md.
# The gateway is the backend for Web/desktop/mobile; the TUI/CLI bypass it.

bind = "127.0.0.1:7878"            # loopback; a reverse proxy terminates TLS

# Bearer-token auth. Uncomment and set the named env var to require a token on
# every route except /healthz. Left unset, the gateway is UNAUTHENTICATED —
# only safe behind loopback + a trusted reverse proxy.
# api_key_env = "OMINI_GATEWAY_KEY"

idle_timeout_secs = 1800           # evict an idle session actor after 30 min
"#;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{BlockKind, Channel, CliSink, StreamSink};

    /// Build a sink writing to in-memory buffers, with TTY styling disabled so
    /// assertions see plain text (no ANSI escapes).
    fn buffered_sink() -> CliSink<Vec<u8>, Vec<u8>> {
        CliSink {
            out: Vec::new(),
            err: Vec::new(),
            stderr_tty: false,
            channel: Channel::None,
            wrote_answer: false,
        }
    }

    /// The side channel (stderr) as a string.
    fn side(sink: &CliSink<Vec<u8>, Vec<u8>>) -> String {
        String::from_utf8(sink.err.clone()).unwrap()
    }

    /// Two consecutive side-channel blocks (reasoning → tool call) must be
    /// separated by a newline. Regression for blocks running together when the
    /// OpenAI assembler batches every `BlockStop` at the end of the round, so a
    /// block start cannot rely on the previous block's stop having arrived.
    #[test]
    fn consecutive_side_blocks_are_newline_separated() {
        let mut sink = buffered_sink();

        // Reasoning block, then — within the same round, no BlockStop yet — a
        // tool-call block. This is exactly the streamed order that glued the
        // two together before the fix.
        sink.on_block_start(1, BlockKind::Reasoning);
        sink.on_reasoning(1, "thinking about it");
        sink.on_block_start(2, BlockKind::ToolCall { name: "shell" });
        sink.on_tool_call_delta(2, r#"{"command":"date"}"#);
        sink.on_turn_end();

        let out = side(&sink);
        assert_eq!(
            out, "[thinking] thinking about it\n[tool: shell] {\"command\":\"date\"}\n",
            "reasoning and tool-call blocks must each occupy their own line"
        );
        // No block's label should ever sit on the same line as prior content.
        assert!(
            !out.contains("it[tool:"),
            "tool-call label glued onto reasoning text: {out:?}"
        );
    }

    /// Answer text goes to `out` (stdout) and is newline-terminated once at the
    /// end; side-channel chatter never leaks into it.
    #[test]
    fn answer_streams_to_out_without_side_noise() {
        let mut sink = buffered_sink();

        sink.on_block_start(0, BlockKind::Reasoning);
        sink.on_reasoning(0, "plan");
        sink.on_block_start(1, BlockKind::Text);
        sink.on_text(1, "the ");
        sink.on_text(1, "answer");
        sink.on_turn_end();

        assert_eq!(String::from_utf8(sink.out.clone()).unwrap(), "the answer\n");
        assert_eq!(side(&sink), "[thinking] plan\n");
    }

    /// When answer text is followed by a side-channel block (tool call /
    /// reasoning), the answer line must be closed on stdout so the side-channel
    /// label doesn't visually glue itself onto the end of the answer.
    #[test]
    fn answer_to_side_transition_closes_stdout_line() {
        let mut sink = buffered_sink();

        sink.on_block_start(0, BlockKind::Text);
        sink.on_text(0, "the answer");
        sink.on_block_start(1, BlockKind::ToolCall { name: "shell" });
        sink.on_tool_call_delta(1, r#"{"cmd":"ls"}"#);
        sink.on_turn_end();

        // stdout must have a newline before the tool call began.
        assert_eq!(String::from_utf8(sink.out.clone()).unwrap(), "the answer\n");
        // stderr must start on its own line, not glued to answer text.
        assert_eq!(side(&sink), "[tool: shell] {\"cmd\":\"ls\"}\n");
    }
}
