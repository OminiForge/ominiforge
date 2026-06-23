//! UI-agnostic assembly: turn a (workspace, profile, model) selection into a
//! ready-to-run [`Agent`] plus everything a front-end needs to drive it.
//!
//! This is the one place that loads config, resolves the model, builds the
//! provider, registers tools (built-in + MCP + skills), and attaches hooks. The
//! CLI (`run`, TUI) and the gateway (one assembly per live session) both call
//! [`assemble`] so every entry point gets the *same* agent — the core stays
//! UI-agnostic (`doc/architecture.md` §2.1).
//!
//! The only thing kept out is *what to do with the result*: one turn, an
//! interactive loop, or a network session. Diagnostics (a skipped MCP server, a
//! loaded `.env`) are routed through an `on_warn` callback rather than hardcoded
//! to stderr, so the gateway can send them to its log instead.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::agent::{Agent, AgentConfig};
use crate::config::{ConfigStore, ResolvedModel};
use crate::context::DEFAULT_COMPACTION_THRESHOLD;
use crate::session::SessionStore;
use crate::tool::{ReadTool, ShellTool, ToolRegistry, WriteTool};

/// Sessions live under `<workspace>/.omini/sessions`.
pub const SESSIONS_SUBDIR: &str = ".omini/sessions";
/// Skills live under `<workspace>/.omini/skills`.
pub const SKILLS_SUBDIR: &str = ".omini/skills";
/// The profile used when none is named.
pub const DEFAULT_PROFILE: &str = "default";

/// What a model selection resolves to: the agent and the surrounding bits a
/// front-end needs to start sessions and render identity.
///
/// `mcp_clients` must be kept alive for the lifetime of any session driven by
/// `agent`: dropping a client kills its MCP subprocess.
pub struct Assembled {
    /// The configured agent (provider + tools + hooks + compaction model).
    pub agent: Agent,
    /// Session store rooted at the workspace's `.omini/sessions`.
    pub session_store: SessionStore,
    /// System prompt to seed a fresh runtime (profile prompt + skill index).
    pub system_prompt: String,
    /// Resolved profile name (the session is stamped with it).
    pub profile_name: String,
    /// Names of every registered tool (stamped on a new session's `Created`).
    pub tool_names: Vec<String>,
    /// Canonical workspace path (tool sandbox root).
    pub workspace: PathBuf,
    /// The resolved model (provider/model/window/pricing) for display + config.
    pub resolved: ResolvedModel,
    /// Live MCP subprocess clients; hold these for the session's lifetime.
    pub mcp_clients: Vec<Arc<crate::mcp::McpClient>>,
}

/// Resolve config and build an [`Agent`] for `profile_name`, with optional model
/// and temperature overrides.
///
/// `on_warn` receives non-fatal diagnostics (a `.env` that was loaded, an MCP
/// server that failed to connect, a hook at an unknown point). The CLI routes it
/// to stderr; the gateway to its log.
///
/// # Errors
/// Fatal configuration problems surface as [`anyhow::Error`]: no providers
/// configured, an unresolvable profile or model, a provider type with no
/// adapter, or an explicitly-named compaction model that cannot be resolved.
pub async fn assemble(
    workspace: PathBuf,
    profile_name: &str,
    model: Option<&str>,
    temperature: Option<f32>,
    no_dotenv: bool,
    on_warn: &(dyn Fn(&str) + Sync),
) -> Result<Assembled> {
    let workspace = resolve_workspace(&workspace)?;

    // Config is discovered relative to the workspace (project `.omini` first,
    // then `~/.omini`).
    let store = ConfigStore::discover(&workspace);

    // Load secrets from a `.env` file before anything reads `api_key_env`,
    // unless disabled. Real environment variables are never overwritten.
    if !no_dotenv {
        load_dotenv(store.roots(), &workspace, on_warn);
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
        .load_profile(profile_name)
        .with_context(|| format!("failed to load profile `{profile_name}`"))?;

    let resolved = store
        .resolve(&providers, &profile, model, temperature)
        .context("failed to resolve model selection")?;

    let provider = crate::provider::build(&resolved)
        .context("provider type has no adapter (only openai-chat is wired)")?;

    let mut tools = ToolRegistry::new();
    register_profile_tools(&mut tools, &profile, workspace.clone());

    // Connect configured MCP servers and register their tools alongside the
    // built-ins (`doc/tool-protocol.md` §5). A broken server is logged and
    // skipped, never fatal. Clients are returned to keep their subprocesses
    // alive for the session.
    let mcp_config =
        crate::mcp::McpConfig::load(store.roots()).context("failed to load mcp.toml")?;
    let mcp_clients = crate::mcp::connect_all(&mcp_config, &mut tools, |msg| on_warn(msg)).await;

    // Skills: list those enabled by the profile (empty = all) and inject their
    // index into the system prompt. The `load_skill` tool is registered only
    // when at least one skill is available (`doc/skill.md` §2).
    let skills_dir = workspace.join(SKILLS_SUBDIR);
    let skills = crate::skill::SkillStore::new(skills_dir.clone()).list(&profile.skills.enabled);
    let skill_index = crate::skill::skill_index_block(&skills);
    if !skills.is_empty() {
        tools.register(Arc::new(crate::skill::LoadSkillTool::new(
            crate::skill::SkillStore::new(skills_dir),
            workspace.clone(),
            profile.profile.name.clone(),
        )));
    }

    let tool_names = tools.descriptors().into_iter().map(|d| d.name).collect();

    let mut agent = Agent::new(
        provider,
        tools,
        AgentConfig {
            model: resolved.model_id.clone(),
            temperature: resolved.temperature,
            max_tokens: Some(resolved.max_output_tokens),
            tool_timeout: Duration::from_secs(120),
            context_window: resolved.context_window,
            compaction_threshold: profile
                .context
                .compaction_threshold
                .unwrap_or(DEFAULT_COMPACTION_THRESHOLD),
            ..AgentConfig::default()
        },
    );

    // Optional dedicated compaction model (`doc/phase2-plan.md` decision B). It
    // may name a different provider, so resolve and build it independently; a bad
    // reference is fatal (the user asked for it explicitly).
    if let Some(model_ref) = profile.context.compaction_model.as_deref() {
        let resolved_compaction = store
            .resolve(&providers, &profile, Some(model_ref), None)
            .with_context(|| format!("failed to resolve compaction_model `{model_ref}`"))?;
        let compaction_provider = crate::provider::build(&resolved_compaction)
            .context("compaction_model provider type has no adapter")?;
        agent = agent.with_compaction_model(compaction_provider, resolved_compaction.model_id);
    }

    // User shell hooks from `.omini/config/hooks.toml` (`doc/hook-protocol.md`
    // §6). A hook at an unknown / not-yet-wired point is logged and skipped,
    // never fatal — same posture as a broken MCP server.
    let hooks = crate::hook::HookConfig::load(store.roots())
        .context("failed to load hooks.toml")?
        .into_registry(|msg| on_warn(msg));
    if !hooks.is_empty() {
        agent = agent.with_hooks(hooks);
    }

    Ok(Assembled {
        agent,
        session_store: SessionStore::new(workspace.join(SESSIONS_SUBDIR)),
        system_prompt: ConfigStore::system_prompt(&profile) + &skill_index,
        profile_name: profile.profile.name.clone(),
        tool_names,
        workspace,
        resolved,
        mcp_clients,
    })
}

/// Register the built-in filesystem/shell tools the profile allows, sandboxed to
/// `workspace`.
fn register_profile_tools(
    registry: &mut ToolRegistry,
    profile: &crate::config::Profile,
    workspace: PathBuf,
) {
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
///
/// # Errors
/// Fails if the directory does not exist (canonicalization requires it).
pub fn resolve_workspace(requested: &Path) -> Result<PathBuf> {
    requested
        .canonicalize()
        .with_context(|| format!("workspace does not exist: {}", requested.display()))
}

/// Load a single `.env` file into the environment, if one is found.
///
/// Search order: each config root's `.env` (project `.omini` before user
/// `.omini`), then `<workspace>/.env` as a fallback. The first file found is
/// loaded and the search stops. `dotenvy` never overwrites variables already
/// present in the environment, so real env vars / direnv / CI always win.
pub fn load_dotenv(roots: &[PathBuf], workspace: &Path, on_warn: &(dyn Fn(&str) + Sync)) {
    let Some(path) = pick_dotenv_path(roots, workspace) else {
        return;
    };
    match dotenvy::from_path(&path) {
        Ok(()) => on_warn(&format!("loaded env from {}", path.display())),
        Err(e) => on_warn(&format!("warning: failed to load {}: {e}", path.display())),
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// `pick_dotenv_path` prefers a config root's `.env` over the workspace's.
    #[test]
    fn dotenv_prefers_config_root_over_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(root.join(".env"), "A=1").unwrap();
        std::fs::write(ws.join(".env"), "A=2").unwrap();

        let picked = pick_dotenv_path(std::slice::from_ref(&root), &ws);
        assert_eq!(picked, Some(root.join(".env")));
    }

    /// With no config-root `.env`, the workspace `.env` is the fallback.
    #[test]
    fn dotenv_falls_back_to_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join(".env"), "A=1").unwrap();

        let picked = pick_dotenv_path(&[dir.path().join("absent")], &ws);
        assert_eq!(picked, Some(ws.join(".env")));
    }

    /// No `.env` anywhere → nothing to load.
    #[test]
    fn dotenv_absent_everywhere_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(pick_dotenv_path(&[dir.path().to_owned()], dir.path()), None);
    }
}
