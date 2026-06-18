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
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::agent::{Agent, AgentConfig, BlockKind, SessionRuntime, StreamSink};
use crate::config::ConfigStore;
use crate::core::payload::TurnFailureReason;
use crate::llm::Message;
use crate::session::SessionStore;
use crate::tool::{ReadTool, ShellTool, ToolRegistry, WriteTool};

const SESSIONS_SUBDIR: &str = ".omini/sessions";
const DEFAULT_PROFILE: &str = "default";

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
    /// Scaffold `.omini/` config files (providers + a default profile).
    Init(InitArgs),
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

/// Parse arguments and dispatch. The binary entry point calls this.
///
/// # Errors
/// Surfaces configuration, provider, and session errors to the process exit.
pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run_turn(args).await,
        Command::Init(args) => init(&args),
    }
}

// __APPEND_MARKER__

async fn run_turn(args: RunArgs) -> Result<()> {
    let workspace = resolve_workspace(args.workspace)?;

    // Config is discovered relative to the workspace (project `.omini` first,
    // then `~/.omini`).
    let store = ConfigStore::discover(&workspace);

    // Load secrets from a `.env` file before anything reads `api_key_env`,
    // unless disabled. Real environment variables are never overwritten.
    if !args.no_dotenv {
        load_dotenv(store.roots(), &workspace);
    }

    let providers = store
        .load_providers()
        .context("failed to load providers.toml")?;
    if providers.providers.is_empty() {
        bail!(
            "no providers configured. Run `ominiforge init` to scaffold \
             .omini/config/providers.toml, then set the model's api_key_env."
        );
    }
    let profile = store
        .load_profile(&args.profile)
        .with_context(|| format!("failed to load profile `{}`", args.profile))?;

    let resolved = store
        .resolve(
            &providers,
            &profile,
            args.model.as_deref(),
            args.temperature,
        )
        .context("failed to resolve model selection")?;

    let provider = crate::provider::build(&resolved)
        .context("provider type has no adapter (only openai-chat is wired)")?;

    let mut tools = ToolRegistry::new();
    register_profile_tools(&mut tools, &profile, workspace.clone());
    let tool_names = tools.descriptors().into_iter().map(|d| d.name).collect();

    let agent = Agent::new(
        provider,
        tools,
        AgentConfig {
            model: resolved.model_id.clone(),
            temperature: resolved.temperature,
            max_tokens: Some(resolved.max_output_tokens),
            tool_timeout: Duration::from_secs(120),
            ..AgentConfig::default()
        },
    );

    let session_store = SessionStore::new(workspace.join(SESSIONS_SUBDIR));
    let mut writer = session_store
        .create_new(
            Some(profile.profile.name.clone()),
            Some(workspace.clone()),
            tool_names,
        )
        .context("failed to create session")?;
    eprintln!(
        "session {} (profile: {}, model: {}/{}, workspace: {})",
        writer.session_id(),
        profile.profile.name,
        resolved.provider_name,
        resolved.model_id,
        workspace.display()
    );

    let mut runtime = SessionRuntime::new(vec![Message::System {
        content: ConfigStore::system_prompt(&profile),
    }]);

    // The answer streams to stdout live as the model produces it; reasoning and
    // tool activity stream to stderr (dimmed) so they stay out of the captured
    // output. See `CliSink`.
    let mut sink = CliSink::new();
    let outcome = agent
        .run_turn_with_sink(&mut writer, &mut runtime, args.prompt, &mut sink)
        .await
        .context("agent turn failed")?;

    eprintln!(
        "[{} round(s), stop: {:?}, tokens: {}in/{}out]",
        outcome.rounds,
        outcome.stop_reason,
        outcome.usage.input_tokens,
        outcome.usage.output_tokens,
    );

    // A turn that ran out of round budget or stalled on its plan is not a crash:
    // the work it did already landed. Warn loudly, but still exit 0 so partial
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
        };
        eprintln!("warning: turn did not complete cleanly — {detail}");
    }
    Ok(())
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

/// Register the built-in tools the profile enables (all three by default),
/// honoring `[tools].builtin` / `[tools].disabled`.
fn register_profile_tools(
    registry: &mut ToolRegistry,
    profile: &crate::config::Profile,
    workspace: PathBuf,
) {
    use std::sync::Arc;
    if profile.tools.allows("read") {
        registry.register(Arc::new(ReadTool::new(workspace.clone())));
    }
    if profile.tools.allows("write") {
        registry.register(Arc::new(WriteTool::new(workspace.clone())));
    }
    if profile.tools.allows("shell") {
        registry.register(Arc::new(ShellTool::new(workspace)));
    }
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

/// Load a single `.env` file into the environment, if one is found.
///
/// Search order: each config root's `.env` (project `.omini` before user
/// `.omini`), then `<workspace>/.env` as a fallback. The first file found is
/// loaded and the search stops. `dotenvy` never overwrites variables already
/// present in the environment, so real env vars / direnv / CI always win.
fn load_dotenv(roots: &[PathBuf], workspace: &Path) {
    let Some(path) = pick_dotenv_path(roots, workspace) else {
        return;
    };
    match dotenvy::from_path(&path) {
        Ok(()) => eprintln!("loaded env from {}", path.display()),
        Err(e) => eprintln!("warning: failed to load {}: {e}", path.display()),
    }
}

/// Choose which `.env` to load: the first existing `<root>/.env` (config roots
/// in priority order), else `<workspace>/.env`, else none. Pure (filesystem
/// reads only) so it is unit-testable without mutating the environment.
fn pick_dotenv_path(roots: &[PathBuf], workspace: &Path) -> Option<PathBuf> {
    roots
        .iter()
        .map(|root| root.join(".env"))
        .find(|p| p.is_file())
        .or_else(|| {
            let ws = workspace.join(".env");
            ws.is_file().then_some(ws)
        })
}

// __APPEND_MARKER2__

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
builtin = ["read", "write", "shell"]
"#;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::pick_dotenv_path;
    use std::path::PathBuf;

    /// A config root's `.env` is preferred over the workspace's, and config
    /// roots are tried in priority order (first one with a `.env` wins).
    #[test]
    fn config_root_env_beats_workspace_and_respects_order() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project/.omini");
        let user = tmp.path().join("home/.omini");
        let workspace = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();

        // Only the user root and the workspace have a `.env`.
        std::fs::write(user.join(".env"), "K=user").unwrap();
        std::fs::write(workspace.join(".env"), "K=ws").unwrap();

        let roots = vec![project.clone(), user.clone()];
        // Project root has no `.env`, so the user root's wins over workspace.
        assert_eq!(
            pick_dotenv_path(&roots, &workspace),
            Some(user.join(".env"))
        );

        // Add a project-root `.env`: highest priority now.
        std::fs::write(project.join(".env"), "K=project").unwrap();
        assert_eq!(
            pick_dotenv_path(&roots, &workspace),
            Some(project.join(".env"))
        );
    }

    /// With no config-root `.env`, the workspace `.env` is the fallback.
    #[test]
    fn falls_back_to_workspace_env() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".omini");
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join(".env"), "K=ws").unwrap();

        assert_eq!(
            pick_dotenv_path(&[root], &workspace),
            Some(workspace.join(".env"))
        );
    }

    /// No `.env` anywhere → nothing to load.
    #[test]
    fn none_when_no_env_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".omini");
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();

        assert_eq!(pick_dotenv_path(&[root], &workspace), None::<PathBuf>);
    }

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
