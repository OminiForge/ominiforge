//! UI-agnostic assembly: turn a (workspace, profile, model) selection into a
//! ready-to-run [`Agent`] plus everything a front-end needs to drive it.
//!
//! This is the one place that loads config, resolves the model, builds the
//! provider, registers tools (built-in + MCP + skills), and attaches hooks. The
//! gateway (one assembly per live session) and the eval runner both call
//! [`assemble`] so every entry point gets the *same* agent — the core stays
//! UI-agnostic (`doc/architecture.md` §2.1).
//!
//! The only thing kept out is *what to do with the result*: one turn, an
//! interactive loop, or a network session. Operator diagnostics (a skipped MCP
//! server, a loaded `.env`) go through `tracing`; business/agent events stay in
//! the session's `events.jsonl` (doc/session-storage.md).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::agent::{Agent, AgentConfig};
use crate::config::{ConfigStore, ResolvedModel};
use crate::context::DEFAULT_COMPACTION_THRESHOLD;
use crate::session::SessionStore;
use crate::tool::{EditTool, FindTool, ReadTool, SearchTool, ShellTool, ToolRegistry, WriteTool};

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
    /// The session's sandbox (its shell execution environment). The `shell` tool
    /// runs in it; the session layer owns its lifecycle (`doc/sandbox.md` §3.2).
    pub sandbox: Arc<dyn crate::sandbox::Sandbox>,
    /// The sandbox's persisted descriptor (backend + durable id), stamped on the
    /// session's meta so its environment can be re-attached after a restart.
    pub sandbox_descriptor: crate::session::SandboxDescriptor,
    /// The resolved model (provider/model/window/pricing) for display + config.
    pub resolved: ResolvedModel,
    /// Live MCP subprocess clients; hold these for the session's lifetime.
    pub mcp_clients: Vec<Arc<crate::mcp::McpClient>>,
    /// The session's LSP manager (diagnostics assist), if any server is
    /// configured. Hold it for the session's lifetime — dropping it kills the
    /// language-server subprocesses (mirrors `mcp_clients`).
    pub lsp_manager: Option<Arc<crate::lsp::LspManager>>,
}

/// Resolve a session's sandbox network policy from the precedence chain
/// (`doc/sandbox.md` §6.2, `doc/profile.md` §7):
///
/// ```text
/// workspace override  >  profile [network]  >  gateway fallback
/// ```
///
/// `workspace` is the per-workspace override already parsed gateway-side (its
/// own bad-name fail-loud happens there); when present it wins outright. Kept
/// separate from [`assemble`] so the whole precedence rule is unit-testable
/// without standing up providers/MCP.
///
/// # Errors
/// An unrecognized *profile* policy name — the caller must fail the session
/// start, not silently open or isolate the sandbox (Karpathy §12).
fn resolve_network(
    workspace: Option<crate::sandbox::NetworkPolicy>,
    section: &crate::config::NetworkSection,
    fallback: crate::sandbox::NetworkPolicy,
) -> Result<crate::sandbox::NetworkPolicy, String> {
    if let Some(policy) = workspace {
        return Ok(policy);
    }
    section.policy.as_ref().map_or(Ok(fallback), |name| {
        crate::sandbox::NetworkPolicy::from_policy_name(name, &section.allow)
    })
}

/// Resolve a session's effective tool-call gate from the three-tier chain
/// (`doc/permission.md` §3), parallel to [`resolve_network`] but with a
/// **union** merge rather than override:
///
/// ```text
/// workspace [permission]  >  profile [permission]  >  gateway default_permission
/// ```
///
/// Precedence differs from network by design. Network is a single policy where
/// the most specific tier wins outright. Permission is a **security floor**: the
/// `deny` lists of all three tiers are *unioned* so a lower tier's ban can never
/// be silently dropped by a higher one (a stealth privilege escalation), while
/// each tier's `ask` list overrides the one below it. This is exactly
/// [`PermissionPolicy::layer_over`], applied bottom-up: gateway is the base,
/// profile layers over it, workspace layers on top.
///
/// All three tiers are gateway-trusted or deployer-owned config — none is read
/// from the agent-writable project dir — so the workspace tier widening `deny`
/// is safe (`doc/workspace-config.md`, "Why gateway-side").
fn resolve_permission(
    workspace: crate::permission::PermissionPolicy,
    profile: crate::permission::PermissionPolicy,
    gateway: crate::permission::PermissionPolicy,
) -> crate::permission::PermissionPolicy {
    workspace.layer_over(profile.layer_over(gateway))
}
/// Resolve config and build an [`Agent`] for `profile_name`, with optional model
/// and temperature overrides.
///
/// `config` is the already-discovered [`ConfigStore`] (its roots come from
/// `--config-dir` / launch cwd / home, **not** from `workspace` — config is
/// independent of the session's workspace). `workspace` is only the tool sandbox
/// root + where sessions/skills live.
///
/// Non-fatal diagnostics (a `.env` that was loaded, an MCP server that failed
/// to connect, a hook at an unknown point) are emitted via `tracing`.
///
/// # Errors
/// Fatal configuration problems surface as [`anyhow::Error`]: no providers
/// configured, an unresolvable profile or model, a provider type with no
/// adapter, or an explicitly-named compaction model that cannot be resolved.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn assemble(
    config: &ConfigStore,
    workspace: PathBuf,
    profile_name: &str,
    model: Option<&str>,
    temperature: Option<f32>,
    no_dotenv: bool,
    sandbox_backend: Arc<dyn crate::sandbox::SandboxBackend>,
    injected_sandbox: Option<(
        Arc<dyn crate::sandbox::Sandbox>,
        crate::session::SandboxDescriptor,
    )>,
    default_network: crate::sandbox::NetworkPolicy,
    workspace_network: Option<crate::sandbox::NetworkPolicy>,
    default_permission: crate::permission::PermissionPolicy,
    workspace_permission: crate::permission::PermissionPolicy,
    mounts: Vec<crate::sandbox::VolumeMount>,
) -> Result<Assembled> {
    let workspace = resolve_workspace(&workspace)?;

    let store = config;

    // Activate workspace env before registering tools, unless disabled. The
    // overlay is passed to subprocesses (shell/MCP/LSP) so commands run inside
    // the workspace's development environment without requiring `direnv exec`.
    // Assembly never blocks on a slow direnv evaluation: a fast export, else
    // the last snapshot while a background refresh re-warms (`doc/env.md`).
    let env_overlay = if no_dotenv {
        BTreeMap::new()
    } else {
        let env_cache = store
            .roots()
            .first()
            .map(|root| crate::env::WorkspaceEnvCache::anchored_at(root));
        let env = crate::env::session_env(
            &workspace,
            env_cache.as_ref(),
            &crate::env::EnvActivation::default(),
        )
        .await;
        load_dotenv(store.roots(), &workspace);
        env
    };

    let assemble_started = std::time::Instant::now();
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

    // The session's sandbox: either injected (a fork's CoW child of its parent's
    // sandbox — `doc/sandbox.md` §4.2) or freshly built from the selected backend,
    // honouring the workspace (as cwd) and the activated env overlay (§3.2). The
    // same handle is wired into `shell` and returned for the session layer to own
    // (register into the SandboxManager, persist its descriptor).
    let (sandbox, sandbox_descriptor) = if let Some(pair) = injected_sandbox {
        pair
    } else {
        // Resolve the session's network egress along the precedence chain
        // (`doc/sandbox.md` §6.2): workspace override > profile [network] >
        // gateway default. A malformed profile policy name fails loud rather than
        // silently opening or isolating the sandbox (Karpathy §12).
        let network = resolve_network(workspace_network, &profile.network, default_network)
            .map_err(|e| {
                anyhow::anyhow!("profile `{profile_name}` has an invalid [network]: {e}")
            })?;
        let sandbox = sandbox_backend
            .create(crate::sandbox::SandboxConfig {
                workspace: workspace.clone(),
                env: env_overlay.clone(),
                network,
                volumes: mounts,
                ..crate::sandbox::SandboxConfig::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("failed to create sandbox: {e}"))?;
        let descriptor = crate::session::SandboxDescriptor {
            backend: sandbox_backend.name().to_owned(),
            id: None,
        };
        (sandbox, descriptor)
    };

    // Language servers for the diagnostics assist (doc/lsp.md):
    // load `lsp.toml`, build one manager per session. `None` when no server is
    // configured, so read/edit/write pay nothing. Nothing spawns here — servers
    // start lazily on the first file of their language.
    let lsp_manager = crate::lsp::LspConfig::load(store.roots())
        .context("failed to load lsp.toml")
        .map(|cfg| crate::lsp::LspManager::new(&cfg, workspace.clone(), env_overlay.clone()))?;

    register_profile_tools(
        &mut tools,
        &profile,
        workspace.clone(),
        Arc::clone(&sandbox),
        lsp_manager.clone(),
    );

    // Connect configured MCP servers and register their tools alongside the
    // built-ins (`doc/tool-protocol.md` §5). A broken server is logged and
    // skipped, never fatal. Clients are returned to keep their subprocesses
    // alive for the session.
    let mcp_config =
        crate::mcp::McpConfig::load(store.roots()).context("failed to load mcp.toml")?;
    let mcp_clients = crate::mcp::connect_all(&mcp_config, &env_overlay, &mut tools).await;

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

    // Project guidance: the workspace-root `AGENTS.md` (or `CLAUDE.md` fallback)
    // is always-on context, appended to the system prompt where it stays in the
    // prefix cache (`doc/agents-md.md`). Nested sub-directory files are loaded
    // lazily by the agent loop as their subtrees are touched.
    let root_guidance = crate::agents_md::read_root(&workspace)
        .map(|g| format!("\n\n{}", crate::agents_md::wrap(&g.label, &g.body)))
        .unwrap_or_default();

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
            workspace: workspace.clone(),
            ..AgentConfig::default()
        },
    );

    // Optional dedicated compaction model (`doc/context-management.md`). It
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
        .into_registry();
    if !hooks.is_empty() {
        agent = agent.with_hooks(hooks);
    }

    // Tool-call permission gate resolved across all three tiers
    // (`doc/permission.md` §3): gateway `default_permission` (base) < profile
    // `[permission]` < workspace `[permission]`, with `deny` union-merged into a
    // security floor. Empty (no tier sets a rule) imposes no gate, preserving the
    // pre-permission fast path. The approval gate for `ask` is attached by the
    // front-end (CLI/gateway), not here — headless assembly defaults to the
    // fail-closed `NullGate`.
    let permission = resolve_permission(
        workspace_permission,
        profile.permission.clone(),
        default_permission,
    );
    if !permission.is_empty() {
        agent = agent.with_permission(permission);
    }

    tracing::debug!(
        workspace = %workspace.display(),
        elapsed_ms = u64::try_from(assemble_started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "agent assembled"
    );

    Ok(Assembled {
        agent,
        session_store: SessionStore::new(workspace.join(SESSIONS_SUBDIR)),
        system_prompt: ConfigStore::system_prompt(&profile) + &skill_index + &root_guidance,
        profile_name: profile.profile.name.clone(),
        tool_names,
        workspace,
        sandbox,
        sandbox_descriptor,
        resolved,
        mcp_clients,
        lsp_manager,
    })
}

/// Register the built-in filesystem/shell tools the profile allows. The
/// filesystem tools are rooted at `workspace`; `shell` runs in the session's
/// `sandbox` (`doc/sandbox.md` §3.2).
fn register_profile_tools(
    registry: &mut ToolRegistry,
    profile: &crate::config::Profile,
    workspace: PathBuf,
    sandbox: Arc<dyn crate::sandbox::Sandbox>,
    lsp: Option<Arc<crate::lsp::LspManager>>,
) {
    if profile.tools.allows("find") {
        registry.register(Arc::new(FindTool::new(workspace.clone())));
    }
    if profile.tools.allows("search") {
        registry.register(Arc::new(SearchTool::new(workspace.clone())));
    }
    if profile.tools.allows("read") {
        registry.register(Arc::new(ReadTool::new(workspace.clone())));
    }
    if profile.tools.allows("write") {
        registry.register(Arc::new(
            WriteTool::new(workspace.clone()).with_lsp(lsp.clone()),
        ));
    }
    if profile.tools.allows("edit") {
        registry.register(Arc::new(EditTool::new(workspace.clone()).with_lsp(lsp)));
    }
    if profile.tools.allows("shell") {
        registry.register(Arc::new(ShellTool::new(sandbox)));
    }
    if profile.tools.allows("web_fetch") {
        let policy = profile.tools.web_fetch.as_ref().map_or_else(
            crate::tool::WebFetchPolicy::default,
            crate::tool::WebFetchPolicy::from_config,
        );
        registry.register(Arc::new(
            crate::tool::WebFetchTool::new(workspace).with_policy(policy),
        ));
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
pub fn load_dotenv(roots: &[PathBuf], workspace: &Path) {
    let Some(path) = pick_dotenv_path(roots, workspace) else {
        return;
    };
    match dotenvy::from_path(&path) {
        Ok(()) => tracing::debug!(path = %path.display(), "loaded env"),
        Err(e) => tracing::warn!("failed to load {}: {e}", path.display()),
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

    /// The default profile leaves `[tools].builtin` unset, which means "all
    /// built-ins". `edit` must register — regression guard for a profile that
    /// shipped an explicit `["read","write","shell"]` list and silently dropped
    /// `edit`.
    #[test]
    fn default_profile_registers_edit() {
        let profile = crate::config::Profile::builtin_default();
        let mut reg = ToolRegistry::new();
        register_profile_tools(
            &mut reg,
            &profile,
            PathBuf::from("/tmp/ws"),
            Arc::new(crate::sandbox::passthrough::PassthroughSandbox::new(
                PathBuf::from("/tmp/ws"),
                BTreeMap::new(),
            )),
            None,
        );
        let names: Vec<String> = reg.descriptors().into_iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec![
                "edit",
                "find",
                "read",
                "search",
                "shell",
                "web_fetch",
                "write"
            ]
        );
    }

    /// An explicit `builtin` list that omits `edit` must not register it — the
    /// allowlist is authoritative.
    #[test]
    fn explicit_builtin_list_excludes_edit() {
        let mut profile = crate::config::Profile::builtin_default();
        profile.tools.builtin = Some(vec!["read".to_owned(), "write".to_owned()]);
        let mut reg = ToolRegistry::new();
        register_profile_tools(
            &mut reg,
            &profile,
            PathBuf::from("/tmp/ws"),
            Arc::new(crate::sandbox::passthrough::PassthroughSandbox::new(
                PathBuf::from("/tmp/ws"),
                BTreeMap::new(),
            )),
            None,
        );
        let names: Vec<String> = reg.descriptors().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["read", "write"]);
    }

    /// Network precedence (`doc/sandbox.md` §6.2): workspace override > profile >
    /// gateway fallback. The test pins the *override direction* at each tier — a
    /// regression that ignored a higher tier would pass a weaker "is it a valid
    /// policy" check but fail this one.
    #[test]
    fn network_precedence_workspace_then_profile_then_fallback() {
        use crate::config::NetworkSection;
        use crate::sandbox::NetworkPolicy;

        // Workspace override wins over BOTH a set profile and the fallback.
        let isolating = NetworkSection {
            policy: Some("isolated".to_owned()),
            allow: Vec::new(),
        };
        assert_eq!(
            resolve_network(
                Some(NetworkPolicy::Open),
                &isolating,
                NetworkPolicy::Isolated
            )
            .unwrap(),
            NetworkPolicy::Open
        );

        // No workspace override: profile sets isolated → wins over an Open fallback.
        assert_eq!(
            resolve_network(None, &isolating, NetworkPolicy::Open).unwrap(),
            NetworkPolicy::Isolated
        );

        // No workspace, profile silent → inherits the fallback verbatim.
        let silent = NetworkSection::default();
        assert_eq!(
            resolve_network(None, &silent, NetworkPolicy::Open).unwrap(),
            NetworkPolicy::Open
        );

        // A typo in the profile fails loud, not silently to the fallback.
        let bad = NetworkSection {
            policy: Some("opne".to_owned()),
            allow: Vec::new(),
        };
        assert!(resolve_network(None, &bad, NetworkPolicy::Open).is_err());
    }

    /// Permission precedence (`doc/permission.md` §3): three tiers, `deny`/`allow`
    /// unioned into a floor, `ask` overridden top-down. The test pins the
    /// security-critical asymmetry — a lower tier's `deny` MUST survive a higher
    /// tier that sets its own rules, or a workspace/profile could silently reopen
    /// a gateway-banned tool. A regression that used plain override (like network)
    /// would drop the gateway ban and this test would catch it.
    #[test]
    fn permission_resolution_unions_deny_across_tiers() {
        use crate::permission::{Decision, PermissionPolicy, Rule};
        let rule = |tool: &str, pat: &str| {
            Rule::contains(
                tool,
                if pat.is_empty() {
                    vec![]
                } else {
                    vec![pat.to_owned()]
                },
            )
        };

        let gateway = PermissionPolicy {
            deny: vec![rule("shell", "curl")],
            allow: vec![],
            ask: vec![],
        };
        let profile = PermissionPolicy {
            deny: vec![rule("shell", "rm -rf")],
            allow: vec![],
            ask: vec![rule("write", "")],
        };
        let workspace = PermissionPolicy {
            deny: vec![rule("net", "")],
            allow: vec![],
            ask: vec![rule("read", "")],
        };

        let effective = resolve_permission(workspace, profile, gateway);

        // Every tier's deny survived — the union floor.
        assert_eq!(
            effective.evaluate("shell", &serde_json::json!({"c": "curl x"})),
            Decision::Deny
        );
        assert_eq!(
            effective.evaluate("shell", &serde_json::json!({"c": "rm -rf /"})),
            Decision::Deny
        );
        assert_eq!(
            effective.evaluate("net", &serde_json::json!({})),
            Decision::Deny
        );
        // Workspace ask (top tier) replaced the profile ask: `read` asks, `write` no longer does.
        assert_eq!(
            effective.evaluate("read", &serde_json::json!({"p": "x"})),
            Decision::Ask
        );
        assert_eq!(
            effective.evaluate("write", &serde_json::json!({"p": "x"})),
            Decision::Allow
        );
    }

    /// `allow` unions across the tiers exactly like `deny`: a lower tier's
    /// pinned approvals survive a higher tier that pins its own, and the union
    /// collapses duplicates. A pinned approval must ALSO win over a lower
    /// tier's ask on the same call (deny > allow > ask).
    #[test]
    fn permission_resolution_unions_allow_across_tiers() {
        use crate::permission::{Decision, PermissionPolicy, Rule};
        let rule = |tool: &str, pat: &str| {
            Rule::contains(
                tool,
                if pat.is_empty() {
                    vec![]
                } else {
                    vec![pat.to_owned()]
                },
            )
        };

        let gateway = PermissionPolicy {
            deny: vec![],
            allow: vec![rule("shell", "cargo test")],
            ask: vec![rule("shell", "")],
        };
        let profile = PermissionPolicy {
            deny: vec![],
            allow: vec![rule("shell", "cargo test"), rule("read", "src/")],
            ask: vec![],
        };
        let workspace = PermissionPolicy::default();

        let effective = resolve_permission(workspace, profile, gateway);
        // Both tiers' pinned approvals survived; the shared one appears once.
        assert_eq!(effective.allow.len(), 2);
        // The pinned approval outranks the gateway tier's ask-all-shell rule.
        assert_eq!(
            effective.evaluate("shell", &serde_json::json!({"command": "cargo test --all"})),
            Decision::Allow
        );
        assert_eq!(
            effective.evaluate("read", &serde_json::json!({"path": "src/main.rs"})),
            Decision::Allow
        );
        // A shell call outside the allow list still falls through to ask.
        assert_eq!(
            effective.evaluate("shell", &serde_json::json!({"command": "make"})),
            Decision::Ask
        );
    }

    /// All tiers empty → empty effective policy → the pre-permission fast path is
    /// preserved (the agent skips the gate). A tier accidentally injecting a rule
    /// would flip this.
    #[test]
    fn permission_resolution_all_empty_is_empty() {
        use crate::permission::PermissionPolicy;
        let effective = resolve_permission(
            PermissionPolicy::default(),
            PermissionPolicy::default(),
            PermissionPolicy::default(),
        );
        assert!(effective.is_empty());
    }
}
