//! Runner: executes [`EvalCase`] in isolated sessions.
//!
//! Each case gets its own session + scratch workspace. The runner assembles an
//! agent, runs one turn with `case.input`, collects events, rebuilds the runtime
//! (for scorers that need `messages`), and calls every registered scorer.
//!
//! Design: `doc/eval.md` §4.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::agent::{SessionRuntime, rebuild_runtime};
use crate::app;
use crate::config::ConfigStore;
use crate::core::SessionId;
use crate::eval::{EvalCase, EvalContext, Score, Scorer};
use crate::llm::Message;

/// Result of running one case: the session it produced + per-scorer scores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseResult {
    /// The case id that was run.
    pub case_id: String,
    /// Session id where this case ran (for traceability).
    pub session_id: SessionId,
    /// Per-scorer results, keyed by `Scorer::name()`.
    pub scores: HashMap<String, Score>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Configuration for a single case execution.
pub struct RunConfig<'a> {
    /// Config store (providers + profiles).
    pub config: &'a ConfigStore,
    /// Profile name to use for the agent.
    pub profile: &'a str,
    /// Optional model override (`provider/model` or just `model`).
    pub model: Option<&'a str>,
    /// Optional temperature override.
    pub temperature: Option<f32>,
    /// Scorers to apply after the turn completes.
    pub scorers: &'a [Arc<dyn Scorer>],
}

/// Execute one [`EvalCase`]: build a scratch workspace, create a new session,
/// run the agent with `case.input`, read back the event stream, and score.
///
/// # Errors
/// Surfaces agent, session, or workspace setup failures.
pub async fn run_case(case: &EvalCase, config: &RunConfig<'_>) -> Result<CaseResult> {
    let start = std::time::Instant::now();

    // 1. Build scratch workspace from case.files.
    let scratch = create_scratch_workspace(case)?;

    // 2. Assemble agent (same as CLI `run`).
    let assembled = app::assemble(
        config.config,
        scratch.clone(),
        config.profile,
        config.model,
        config.temperature,
        false,
        std::sync::Arc::new(crate::sandbox::passthrough::PassthroughBackend::new()),
        None,
        // Eval runs on the passthrough backend, which ignores the network policy.
        crate::sandbox::NetworkPolicy::Open,
        None,
        // Eval exercises the profile gate only; no gateway/workspace tiers.
        crate::permission::PermissionPolicy::default(),
        crate::permission::PermissionPolicy::default(),
        // Eval runs on passthrough; no auxiliary mounts.
        Vec::new(),
        // Each eval case is its own scratch workspace, so there is nothing to
        // share servers across: a per-run service is correct (`doc/lsp.md` §5.2).
        std::sync::Arc::new(crate::lsp::ProcessLspService::new()),
    )
    .await
    .context("failed to assemble agent for eval case")?;

    // 3. Create session.
    let mut writer = assembled
        .session_store
        .create_new(
            Some(assembled.profile_name.clone()),
            Some(assembled.workspace.clone()),
            assembled.tool_names.clone(),
        )
        .context("failed to create eval session")?;

    let session_id = writer.session_id().clone();

    // 4. Run one turn with case.input.
    let mut runtime = SessionRuntime::new(vec![Message::System {
        content: assembled.system_prompt.clone(),
    }]);

    assembled
        .agent
        .run_turn(&mut writer, &mut runtime, case.input.clone())
        .await
        .context("agent turn failed during eval")?;

    // 5. Read back events.
    let events = assembled
        .session_store
        .read_events(&session_id)
        .context("failed to read eval session events")?;

    // 6. Rebuild runtime for scorers that need messages.
    let system_seed = vec![Message::System {
        content: assembled.system_prompt.clone(),
    }];
    let rebuilt = rebuild_runtime(&events, system_seed);

    // 7. Score.
    let ctx = EvalContext {
        events: &events,
        messages: Some(&rebuilt.context),
        workspace: Some(&scratch),
        case,
    };

    let mut scores = HashMap::new();
    for scorer in config.scorers {
        let score = scorer.score(&ctx).await;
        scores.insert(scorer.name().to_string(), score);
    }

    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    Ok(CaseResult {
        case_id: case.id.clone(),
        session_id,
        scores,
        duration_ms,
    })
}

/// Create a scratch workspace and populate it with `case.files`.
///
/// Path: `.omini/eval/scratch/<case_id>`. Cleaned before each run so reruns
/// start from a known state.
///
/// # Errors
/// Returns error on filesystem failures.
fn create_scratch_workspace(case: &EvalCase) -> Result<PathBuf> {
    let scratch_root = PathBuf::from(".omini/eval/scratch");
    std::fs::create_dir_all(&scratch_root).context("failed to create scratch root")?;

    let scratch = scratch_root.join(&case.id);

    if scratch.exists() {
        std::fs::remove_dir_all(&scratch)
            .with_context(|| format!("failed to clean scratch workspace: {}", scratch.display()))?;
    }

    std::fs::create_dir_all(&scratch)
        .with_context(|| format!("failed to create scratch workspace: {}", scratch.display()))?;

    for file in &case.files {
        let target = scratch.join(&file.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create parent dir: {}", parent.display()))?;
        }
        std::fs::write(&target, &file.content)
            .with_context(|| format!("failed to write case file: {}", target.display()))?;
    }

    Ok(scratch)
}
