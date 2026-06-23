//! Hook registry, built-in hooks (Rust traits), and the shell hook runner.
//! Before hooks may pass/modify/block; after hooks observe only. See
//! `doc/hook-protocol.md`.
//!
//! A hook fires at a fixed [`HookPoint`] in the agent pipeline (Phase 4 wires
//! four: `turn:start`, `turn:end`, `tool:invoke:before`, `tool:invoke:after`).
//! Built-in hooks implement [`BeforeHook`] / [`AfterHook`] directly (zero IPC);
//! user hooks are shell commands ([`ShellHook`]) that speak JSON over
//! stdin/stdout. Both kinds share one priority-ordered chain per point (§7).
//!
//! The registry only *decides* — it returns [`HookExecution`] records and a
//! [`BeforeOutcome`]; the agent loop is what persists `HookEvent`s and turns a
//! block into the point-specific failure event (`doc/hook-protocol.md` §8, §11).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::core::payload::HookOutcome;

/// A predefined point in the pipeline where hooks may fire. Adding a point
/// needs a release (`doc/hook-protocol.md` §2); Phase 4 wires these four.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    TurnStart,
    TurnEnd,
    ToolInvokeBefore,
    ToolInvokeAfter,
}

impl HookPoint {
    /// The wire/string form used in config and event logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnStart => "turn:start",
            Self::TurnEnd => "turn:end",
            Self::ToolInvokeBefore => "tool:invoke:before",
            Self::ToolInvokeAfter => "tool:invoke:after",
        }
    }

    /// Parse a point from its string form. Unknown / not-yet-wired points
    /// (`model:request:before`, etc.) return `None`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "turn:start" => Some(Self::TurnStart),
            "turn:end" => Some(Self::TurnEnd),
            "tool:invoke:before" => Some(Self::ToolInvokeBefore),
            "tool:invoke:after" => Some(Self::ToolInvokeAfter),
            _ => None,
        }
    }

    /// Whether this point runs *before* hooks (synchronous, can block) or
    /// *after* hooks (observe only). `turn:start` and `tool:invoke:before` are
    /// before; the rest are after (`doc/hook-protocol.md` §3).
    #[must_use]
    pub const fn is_before(self) -> bool {
        matches!(self, Self::TurnStart | Self::ToolInvokeBefore)
    }
}

/// What the pipeline does if a hook errors, times out, or returns bad output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureMode {
    /// Continue the pipeline (logging / metrics hooks). The default.
    #[default]
    Open,
    /// Block the pipeline and surface an error (security / permission hooks).
    Closed,
}

/// The context handed to a hook: which point fired, the payload it may inspect
/// or rewrite, and the tool name when the point is tool-scoped.
#[derive(Debug, Clone)]
pub struct HookRequest {
    pub hook_point: HookPoint,
    /// Point-specific payload. For `tool:invoke:before` this is the tool input
    /// object a before hook may `Modify`; for after points it is the result.
    pub payload: serde_json::Value,
    /// The tool name for `tool:invoke:*` points, else `None`. Used for
    /// `match_tool` filtering.
    pub tool_name: Option<String>,
}

/// A before hook's decision. `Failed` is distinct from `Block` so the log
/// records the hook erroring (vs. deliberately blocking); the registry then
/// applies [`FailureMode`] to decide the pipeline's fate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeforeDecision {
    Pass,
    Modify(serde_json::Value),
    Block { reason: String },
    Failed { error: String },
}

/// A synchronous hook that runs before a pipeline step and may pass, rewrite
/// the payload, or block it (`doc/hook-protocol.md` §5).
#[async_trait::async_trait]
pub trait BeforeHook: Send + Sync {
    fn name(&self) -> &str;
    fn priority(&self) -> u32 {
        100
    }
    fn failure_mode(&self) -> FailureMode {
        FailureMode::Open
    }
    /// Whether this hook applies to `req` (e.g. `match_tool`). Default: always.
    fn matches(&self, _req: &HookRequest) -> bool {
        true
    }
    async fn intercept(&self, req: &HookRequest) -> BeforeDecision;
}

/// An observe-only hook that runs after a pipeline step. `Err` records the hook
/// failing; it never affects the pipeline (`doc/hook-protocol.md` §3).
#[async_trait::async_trait]
pub trait AfterHook: Send + Sync {
    fn name(&self) -> &str;
    fn priority(&self) -> u32 {
        100
    }
    fn matches(&self, _req: &HookRequest) -> bool {
        true
    }
    async fn observe(&self, req: &HookRequest) -> Result<(), String>;
}

/// One hook's execution at a point, recorded by the agent as a `HookEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookExecution {
    pub hook_name: String,
    pub hook_point: HookPoint,
    pub outcome: HookOutcome,
    pub duration_ms: u64,
}

/// The result of running a before chain: every hook's execution record plus the
/// chain's effect on the pipeline.
#[derive(Debug, Clone)]
pub struct BeforeOutcome {
    pub executions: Vec<HookExecution>,
    pub effect: BeforeEffect,
}

/// What the before chain decided for the pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeforeEffect {
    /// Proceed with this (possibly hook-modified) payload.
    Proceed(serde_json::Value),
    /// Stop the step. `by` names the hook that blocked (or whose failure under
    /// `closed` mode blocked).
    Block { reason: String, by: String },
}

/// Priority-ordered before and after hooks, keyed by point.
#[derive(Default)]
pub struct HookRegistry {
    before: HashMap<HookPoint, Vec<std::sync::Arc<dyn BeforeHook>>>,
    after: HashMap<HookPoint, Vec<std::sync::Arc<dyn AfterHook>>>,
}

impl std::fmt::Debug for HookRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let before: Vec<_> = self
            .before
            .iter()
            .map(|(p, v)| (p.as_str(), v.len()))
            .collect();
        let after: Vec<_> = self
            .after
            .iter()
            .map(|(p, v)| (p.as_str(), v.len()))
            .collect();
        f.debug_struct("HookRegistry")
            .field("before", &before)
            .field("after", &after)
            .finish()
    }
}

impl HookRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any hook is registered at all (lets the agent skip the machinery).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.before.values().all(Vec::is_empty) && self.after.values().all(Vec::is_empty)
    }

    /// Register a before hook at `point`, keeping the chain sorted by ascending
    /// priority (`doc/hook-protocol.md` §7). Same priority preserves insertion
    /// order (stable sort).
    pub fn register_before(&mut self, point: HookPoint, hook: std::sync::Arc<dyn BeforeHook>) {
        let chain = self.before.entry(point).or_default();
        chain.push(hook);
        chain.sort_by_key(|h| h.priority());
    }

    /// Register an after hook at `point`, sorted by ascending priority.
    pub fn register_after(&mut self, point: HookPoint, hook: std::sync::Arc<dyn AfterHook>) {
        let chain = self.after.entry(point).or_default();
        chain.push(hook);
        chain.sort_by_key(|h| h.priority());
    }

    /// Run the before chain at `point`. Threads each hook's `Modify` into the
    /// next hook's payload; stops at the first `Block` (or `Failed` under
    /// `closed` mode). A hook that fails under `open` mode is recorded and
    /// skipped (`doc/hook-protocol.md` §7, §9).
    pub async fn run_before(
        &self,
        point: HookPoint,
        payload: serde_json::Value,
        tool_name: Option<String>,
    ) -> BeforeOutcome {
        let mut executions = Vec::new();
        let mut req = HookRequest {
            hook_point: point,
            payload,
            tool_name,
        };
        let Some(chain) = self.before.get(&point) else {
            return BeforeOutcome {
                executions,
                effect: BeforeEffect::Proceed(req.payload),
            };
        };
        for hook in chain {
            if !hook.matches(&req) {
                continue;
            }
            let started = Instant::now();
            let decision = hook.intercept(&req).await;
            let duration_ms = duration_ms(started.elapsed());
            match decision {
                BeforeDecision::Pass => executions.push(HookExecution {
                    hook_name: hook.name().to_owned(),
                    hook_point: point,
                    outcome: HookOutcome::Pass,
                    duration_ms,
                }),
                BeforeDecision::Modify(new_payload) => {
                    executions.push(HookExecution {
                        hook_name: hook.name().to_owned(),
                        hook_point: point,
                        outcome: HookOutcome::Modified,
                        duration_ms,
                    });
                    req.payload = new_payload;
                }
                BeforeDecision::Block { reason } => {
                    executions.push(HookExecution {
                        hook_name: hook.name().to_owned(),
                        hook_point: point,
                        outcome: HookOutcome::Blocked {
                            reason: reason.clone(),
                        },
                        duration_ms,
                    });
                    return BeforeOutcome {
                        executions,
                        effect: BeforeEffect::Block {
                            reason,
                            by: hook.name().to_owned(),
                        },
                    };
                }
                BeforeDecision::Failed { error } => {
                    executions.push(HookExecution {
                        hook_name: hook.name().to_owned(),
                        hook_point: point,
                        outcome: HookOutcome::Failed {
                            error: error.clone(),
                        },
                        duration_ms,
                    });
                    // `closed` mode turns a hook failure into a block; `open`
                    // mode logs it and lets the pipeline continue (§9).
                    if hook.failure_mode() == FailureMode::Closed {
                        let reason = format!("hook `{}` failed: {error}", hook.name());
                        return BeforeOutcome {
                            executions,
                            effect: BeforeEffect::Block {
                                reason,
                                by: hook.name().to_owned(),
                            },
                        };
                    }
                }
            }
        }
        BeforeOutcome {
            executions,
            effect: BeforeEffect::Proceed(req.payload),
        }
    }

    /// Run every after hook at `point`, returning each one's execution record.
    /// After hooks cannot affect the pipeline; a failure is logged as
    /// [`HookOutcome::Failed`] (`doc/hook-protocol.md` §3).
    pub async fn run_after(
        &self,
        point: HookPoint,
        payload: serde_json::Value,
        tool_name: Option<String>,
    ) -> Vec<HookExecution> {
        let mut executions = Vec::new();
        let req = HookRequest {
            hook_point: point,
            payload,
            tool_name,
        };
        let Some(chain) = self.after.get(&point) else {
            return executions;
        };
        for hook in chain {
            if !hook.matches(&req) {
                continue;
            }
            let started = Instant::now();
            let outcome = match hook.observe(&req).await {
                Ok(()) => HookOutcome::Observed,
                Err(error) => HookOutcome::Failed { error },
            };
            executions.push(HookExecution {
                hook_name: hook.name().to_owned(),
                hook_point: point,
                outcome,
                duration_ms: duration_ms(started.elapsed()),
            });
        }
        executions
    }
}

fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

// ── User shell hooks ──────────────────────────────────────────────────────

/// The parsed contents of `.omini/config/hooks.toml` (`doc/hook-protocol.md`
/// §6.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct HookConfig {
    /// Each `[[hooks]]` table.
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
}

/// One configured user (shell) hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HookSpec {
    /// Unique name; namespaces the hook in logs.
    pub name: String,
    /// The pipeline point, e.g. `"tool:invoke:before"`.
    pub hook_point: String,
    /// Only fire for this tool (optional; `tool:invoke:*` points only).
    #[serde(default)]
    pub match_tool: Option<String>,
    /// The shell command to run (via `sh -c`).
    pub command: String,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default)]
    pub failure_mode: FailureMode,
    /// Execution budget. Defaults to 5s for before hooks, 30s for after.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

const fn default_priority() -> u32 {
    100
}

impl HookConfig {
    /// Load and merge `config/hooks.toml` from each root (highest priority
    /// first; a hook name in a higher root shadows a lower one). A missing file
    /// contributes nothing; absent everywhere yields an empty config.
    ///
    /// # Errors
    /// Returns the offending path and parse error if a present file is malformed.
    pub fn load(roots: &[PathBuf]) -> Result<Self, ConfigError> {
        let mut merged: Vec<HookSpec> = Vec::new();
        for root in roots {
            let path = root.join("config").join("hooks.toml");
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => return Err(ConfigError::Io { path, source }),
            };
            let file: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?;
            for hook in file.hooks {
                if !merged.iter().any(|h| h.name == hook.name) {
                    merged.push(hook);
                }
            }
        }
        Ok(Self { hooks: merged })
    }

    /// Build a [`HookRegistry`] from the configured shell hooks, skipping any
    /// whose `hook_point` is unknown / not yet wired (reported via `on_warn`).
    #[must_use]
    pub fn into_registry(self, on_warn: impl Fn(&str)) -> HookRegistry {
        let mut registry = HookRegistry::new();
        for spec in self.hooks {
            let Some(point) = HookPoint::parse(&spec.hook_point) else {
                on_warn(&format!(
                    "hook: skipping `{}` — unknown hook_point `{}`",
                    spec.name, spec.hook_point
                ));
                continue;
            };
            let hook = std::sync::Arc::new(ShellHook::new(spec, point));
            if point.is_before() {
                registry.register_before(point, hook);
            } else {
                registry.register_after(point, hook);
            }
        }
        registry
    }
}

/// Why loading `hooks.toml` failed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

/// A user hook backed by a shell command. Implements both hook traits; the
/// config's point decides which one the registry uses.
///
/// Communication is JSON over stdin/stdout (`doc/hook-protocol.md` §6.2): the
/// host writes a request object to stdin; a before hook writes its action to
/// stdout, an after hook writes nothing.
pub struct ShellHook {
    spec: HookSpec,
    point: HookPoint,
}

impl ShellHook {
    #[must_use]
    pub const fn new(spec: HookSpec, point: HookPoint) -> Self {
        Self { spec, point }
    }

    fn timeout(&self) -> Duration {
        let default_ms = if self.point.is_before() {
            5_000
        } else {
            30_000
        };
        Duration::from_millis(self.spec.timeout_ms.unwrap_or(default_ms))
    }

    fn applies(&self, req: &HookRequest) -> bool {
        // A `match_tool` filter only fires when the request names that tool.
        self.spec
            .match_tool
            .as_ref()
            .is_none_or(|want| req.tool_name.as_deref() == Some(want.as_str()))
    }

    /// Run the command, feeding `stdin_json` to stdin, and return its stdout (on
    /// success) or an error string (non-zero exit, timeout, spawn failure).
    async fn run(&self, stdin_json: String) -> Result<String, String> {
        use tokio::io::AsyncWriteExt as _;

        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&self.spec.command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;

        if let Some(mut stdin) = child.stdin.take() {
            // Ignore a broken pipe: a hook that never reads stdin is allowed.
            let _ = stdin.write_all(stdin_json.as_bytes()).await;
            drop(stdin);
        }

        match tokio::time::timeout(self.timeout(), child.wait_with_output()).await {
            Ok(Ok(out)) if out.status.success() => {
                Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
            }
            Ok(Ok(out)) => {
                let code = out.status.code().unwrap_or(-1);
                Err(format!(
                    "exit {code}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
            Ok(Err(e)) => Err(format!("io error: {e}")),
            Err(_) => Err(format!("timeout after {}ms", self.timeout().as_millis())),
        }
    }
}

#[async_trait::async_trait]
impl BeforeHook for ShellHook {
    fn name(&self) -> &str {
        &self.spec.name
    }
    fn priority(&self) -> u32 {
        self.spec.priority
    }
    fn failure_mode(&self) -> FailureMode {
        self.spec.failure_mode
    }
    fn matches(&self, req: &HookRequest) -> bool {
        self.applies(req)
    }

    async fn intercept(&self, req: &HookRequest) -> BeforeDecision {
        let stdin_json = serde_json::json!({
            "hook_point": self.point.as_str(),
            "payload": req.payload,
        })
        .to_string();

        let stdout = match self.run(stdin_json).await {
            Ok(s) => s,
            Err(e) => return BeforeDecision::Failed { error: e },
        };
        if stdout.is_empty() {
            // No output → pass (a before hook that only side-effects).
            return BeforeDecision::Pass;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&stdout) else {
            return BeforeDecision::Failed {
                error: format!("invalid JSON on stdout: {stdout}"),
            };
        };
        match value.get("action").and_then(|a| a.as_str()) {
            Some("pass") => BeforeDecision::Pass,
            Some("modify") => value.get("payload").map_or_else(
                || BeforeDecision::Failed {
                    error: "modify action missing `payload`".to_owned(),
                },
                |p| BeforeDecision::Modify(p.clone()),
            ),
            Some("block") => BeforeDecision::Block {
                reason: value
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("blocked by hook")
                    .to_owned(),
            },
            other => BeforeDecision::Failed {
                error: format!("unknown action: {other:?}"),
            },
        }
    }
}

#[async_trait::async_trait]
impl AfterHook for ShellHook {
    fn name(&self) -> &str {
        &self.spec.name
    }
    fn priority(&self) -> u32 {
        self.spec.priority
    }
    fn matches(&self, req: &HookRequest) -> bool {
        self.applies(req)
    }

    async fn observe(&self, req: &HookRequest) -> Result<(), String> {
        let stdin_json = serde_json::json!({
            "hook_point": self.point.as_str(),
            "payload": req.payload,
        })
        .to_string();
        self.run(stdin_json).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::sync::Arc;

    /// A built-in before hook that always returns a fixed decision, for testing
    /// chain semantics without spawning processes.
    struct FixedBefore {
        name: &'static str,
        priority: u32,
        failure_mode: FailureMode,
        decision: BeforeDecision,
    }

    #[async_trait::async_trait]
    impl BeforeHook for FixedBefore {
        fn name(&self) -> &str {
            self.name
        }
        fn priority(&self) -> u32 {
            self.priority
        }
        fn failure_mode(&self) -> FailureMode {
            self.failure_mode
        }
        async fn intercept(&self, _req: &HookRequest) -> BeforeDecision {
            self.decision.clone()
        }
    }

    fn before(name: &'static str, priority: u32, decision: BeforeDecision) -> Arc<dyn BeforeHook> {
        Arc::new(FixedBefore {
            name,
            priority,
            failure_mode: FailureMode::Open,
            decision,
        })
    }

    #[test]
    fn hook_point_round_trips_string() {
        for p in [
            HookPoint::TurnStart,
            HookPoint::TurnEnd,
            HookPoint::ToolInvokeBefore,
            HookPoint::ToolInvokeAfter,
        ] {
            assert_eq!(HookPoint::parse(p.as_str()), Some(p));
        }
        assert_eq!(HookPoint::parse("model:request:before"), None);
    }

    #[test]
    fn before_points_are_before_after_points_are_after() {
        assert!(HookPoint::TurnStart.is_before());
        assert!(HookPoint::ToolInvokeBefore.is_before());
        assert!(!HookPoint::TurnEnd.is_before());
        assert!(!HookPoint::ToolInvokeAfter.is_before());
    }

    /// A `block` from any hook stops the chain and reports the blocker; later
    /// hooks do not run (`doc/hook-protocol.md` §7).
    #[tokio::test]
    async fn before_chain_blocks_and_short_circuits() {
        let mut reg = HookRegistry::new();
        reg.register_before(
            HookPoint::TurnStart,
            before(
                "guard",
                10,
                BeforeDecision::Block {
                    reason: "nope".to_owned(),
                },
            ),
        );
        reg.register_before(
            HookPoint::TurnStart,
            before("late", 20, BeforeDecision::Pass),
        );

        let out = reg
            .run_before(HookPoint::TurnStart, serde_json::json!({}), None)
            .await;
        assert_eq!(
            out.effect,
            BeforeEffect::Block {
                reason: "nope".to_owned(),
                by: "guard".to_owned()
            }
        );
        // Only the blocker ran; the lower-priority hook never executed.
        assert_eq!(out.executions.len(), 1);
        assert_eq!(out.executions[0].hook_name, "guard");
    }

    /// Priority order: a higher-priority (lower number) hook's `Modify` feeds
    /// the next hook, and the final payload proceeds.
    #[tokio::test]
    async fn before_chain_threads_modify_in_priority_order() {
        let mut reg = HookRegistry::new();
        // Registered out of order; the registry sorts by priority.
        reg.register_before(
            HookPoint::ToolInvokeBefore,
            before("second", 20, BeforeDecision::Pass),
        );
        reg.register_before(
            HookPoint::ToolInvokeBefore,
            before(
                "first",
                10,
                BeforeDecision::Modify(serde_json::json!({"edited": true})),
            ),
        );

        let out = reg
            .run_before(
                HookPoint::ToolInvokeBefore,
                serde_json::json!({"edited": false}),
                Some("write".to_owned()),
            )
            .await;
        assert_eq!(
            out.effect,
            BeforeEffect::Proceed(serde_json::json!({"edited": true}))
        );
        assert_eq!(out.executions.len(), 2);
        assert_eq!(out.executions[0].hook_name, "first");
        assert_eq!(out.executions[0].outcome, HookOutcome::Modified);
    }

    /// Failure under `open` mode is recorded but does not block; `closed` mode
    /// blocks (`doc/hook-protocol.md` §9).
    #[tokio::test]
    async fn failure_mode_governs_blocking() {
        let open = Arc::new(FixedBefore {
            name: "flaky-open",
            priority: 10,
            failure_mode: FailureMode::Open,
            decision: BeforeDecision::Failed {
                error: "boom".to_owned(),
            },
        });
        let mut reg = HookRegistry::new();
        reg.register_before(HookPoint::TurnStart, open);
        let out = reg
            .run_before(HookPoint::TurnStart, serde_json::json!({}), None)
            .await;
        assert!(matches!(out.effect, BeforeEffect::Proceed(_)));
        assert_eq!(
            out.executions[0].outcome,
            HookOutcome::Failed {
                error: "boom".to_owned()
            }
        );

        let closed = Arc::new(FixedBefore {
            name: "flaky-closed",
            priority: 10,
            failure_mode: FailureMode::Closed,
            decision: BeforeDecision::Failed {
                error: "boom".to_owned(),
            },
        });
        let mut reg = HookRegistry::new();
        reg.register_before(HookPoint::TurnStart, closed);
        let out = reg
            .run_before(HookPoint::TurnStart, serde_json::json!({}), None)
            .await;
        assert!(matches!(out.effect, BeforeEffect::Block { .. }));
    }

    /// `match_tool` filters a shell hook to one tool: a non-matching request is
    /// skipped entirely.
    #[tokio::test]
    async fn match_tool_filters_shell_hook() {
        let spec = HookSpec {
            name: "lint-write".to_owned(),
            hook_point: "tool:invoke:before".to_owned(),
            match_tool: Some("write".to_owned()),
            command: "echo '{\"action\":\"block\",\"reason\":\"no\"}'".to_owned(),
            priority: 100,
            failure_mode: FailureMode::Open,
            timeout_ms: None,
        };
        let mut reg = HookRegistry::new();
        reg.register_before(
            HookPoint::ToolInvokeBefore,
            Arc::new(ShellHook::new(spec, HookPoint::ToolInvokeBefore)),
        );

        // A `read` call does not match → chain proceeds, no executions.
        let out = reg
            .run_before(
                HookPoint::ToolInvokeBefore,
                serde_json::json!({}),
                Some("read".to_owned()),
            )
            .await;
        assert!(out.executions.is_empty());
        assert!(matches!(out.effect, BeforeEffect::Proceed(_)));
    }

    /// A shell before hook that emits a `block` action blocks the pipeline.
    #[tokio::test]
    async fn shell_before_block_action() {
        let spec = HookSpec {
            name: "deny".to_owned(),
            hook_point: "tool:invoke:before".to_owned(),
            match_tool: None,
            command: "echo '{\"action\":\"block\",\"reason\":\"dangerous\"}'".to_owned(),
            priority: 100,
            failure_mode: FailureMode::Open,
            timeout_ms: None,
        };
        let hook = ShellHook::new(spec, HookPoint::ToolInvokeBefore);
        let req = HookRequest {
            hook_point: HookPoint::ToolInvokeBefore,
            payload: serde_json::json!({"cmd": "rm -rf /"}),
            tool_name: Some("shell".to_owned()),
        };
        assert_eq!(
            hook.intercept(&req).await,
            BeforeDecision::Block {
                reason: "dangerous".to_owned()
            }
        );
    }

    /// hooks.toml parses the doc example into a before spec with its fields.
    #[test]
    fn config_parses_doc_example() {
        let toml_src = r#"
[[hooks]]
name = "lint-before-write"
hook_point = "tool:invoke:before"
match_tool = "write"
command = "python3 ~/.omini/hooks/lint-check.py"
priority = 50
failure_mode = "open"
timeout_ms = 5000

[[hooks]]
name = "notify-on-complete"
hook_point = "turn:end"
command = "~/.omini/hooks/notify.sh"
"#;
        let config: HookConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(config.hooks.len(), 2);
        let lint = &config.hooks[0];
        assert_eq!(lint.match_tool.as_deref(), Some("write"));
        assert_eq!(lint.priority, 50);
        assert_eq!(lint.failure_mode, FailureMode::Open);
        // Defaults applied to the second hook.
        let notify = &config.hooks[1];
        assert_eq!(notify.priority, 100);
        assert!(notify.timeout_ms.is_none());
    }

    /// `into_registry` routes a before-point spec to the before chain and an
    /// after-point spec to the after chain, and skips unknown points.
    #[test]
    fn into_registry_routes_by_point_and_skips_unknown() {
        let config = HookConfig {
            hooks: vec![
                HookSpec {
                    name: "b".to_owned(),
                    hook_point: "tool:invoke:before".to_owned(),
                    match_tool: None,
                    command: "true".to_owned(),
                    priority: 100,
                    failure_mode: FailureMode::Open,
                    timeout_ms: None,
                },
                HookSpec {
                    name: "a".to_owned(),
                    hook_point: "turn:end".to_owned(),
                    match_tool: None,
                    command: "true".to_owned(),
                    priority: 100,
                    failure_mode: FailureMode::Open,
                    timeout_ms: None,
                },
                HookSpec {
                    name: "unknown".to_owned(),
                    hook_point: "model:request:before".to_owned(),
                    match_tool: None,
                    command: "true".to_owned(),
                    priority: 100,
                    failure_mode: FailureMode::Open,
                    timeout_ms: None,
                },
            ],
        };
        let skipped = std::cell::RefCell::new(Vec::new());
        let registry = config.into_registry(|m| skipped.borrow_mut().push(m.to_owned()));
        assert!(!registry.is_empty());
        let skipped = skipped.into_inner();
        assert_eq!(skipped.len(), 1, "unknown point reported");
        assert!(skipped[0].contains("model:request:before"));
    }

    /// A missing hooks.toml everywhere is an empty config, not an error.
    #[test]
    fn config_missing_everywhere_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config = HookConfig::load(&[dir.path().to_path_buf()]).unwrap();
        assert!(config.hooks.is_empty());
    }
}
