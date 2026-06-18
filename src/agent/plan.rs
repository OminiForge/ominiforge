//! In-turn execution plan: pure state plus the operations that mutate it.
//!
//! Plan is the agent's working checklist for a longer goal — it keeps a turn
//! from losing track of what it set out to do across many model rounds. It is
//! *session-scoped* (lives in [`super::SessionRuntime`], survives across turns)
//! but holds no I/O: this module is just the data model, the op-based mutation,
//! and the rendering the model sees. See `doc/plan.md`.
//!
//! `plan` is a *control* tool, not a leaf tool: it operates on the agent's own
//! state rather than the outside world. So it does **not** implement [`Tool`]
//! and is **not** in the [`ToolRegistry`] — the agent loop contributes its
//! [`descriptor`] alongside the leaf-tool schemas and intercepts the call by
//! name, applying [`apply_plan_op`] to the live plan. See `doc/plan.md` §5.
//!
//! [`Tool`]: crate::tool::Tool
//! [`ToolRegistry`]: crate::tool::ToolRegistry

use serde::{Deserialize, Serialize};

use crate::llm::ToolSchema;

/// The tool name the model uses and the loop intercepts.
pub const PLAN_TOOL_NAME: &str = "plan";

/// One step of the working plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    /// Stable id assigned by the runtime on `init`/`add` ("1", "2", ...). The
    /// model refers to steps by this id; it never changes once assigned.
    pub id: String,
    pub content: String,
    pub status: StepStatus,
    /// Required for `cancelled`/`blocked` (the why); optional otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Lifecycle state of a [`PlanStep`].
///
/// Terminal states are `Completed`/`Cancelled`/`Blocked`; `Pending`/`InProgress`
/// are non-terminal and hold a turn open at the completion gate (`doc/plan.md`
/// §6). `Cancelled` means the step is *objectively* unreachable (no such tool,
/// no permission); `Blocked` means it is reachable but needs the user (missing
/// key, a decision). Neither may be used to dodge a merely hard step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
    Blocked,
}

impl StepStatus {
    /// Whether this is a terminal state (the completion gate ignores these).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Blocked)
    }

    /// A short label for plan rendering.
    const fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Blocked => "blocked",
        }
    }
}

/// A single plan mutation, decoded from the model's tool-call arguments.
///
/// Externally tagged on `op`, matching the `plan` tool schema. Missing required
/// fields (e.g. `reason` on `cancel`/`block`) fail to deserialize and surface as
/// a tool error the model corrects next round (`doc/plan.md` §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlanOp {
    /// Establish the plan from scratch; any existing plan is replaced. Ids are
    /// assigned by the runtime, not the model.
    Init { steps: Vec<NewStep> },
    /// Mark a step `in_progress`.
    Start { id: String },
    /// Mark a step `completed`.
    Complete { id: String },
    /// Mark a step `cancelled` (objectively unreachable). `reason` is required.
    Cancel { id: String, reason: String },
    /// Mark a step `blocked` (needs the user). `reason` is required.
    Block { id: String, reason: String },
    /// Append a new `pending` step, after `after_id` if given else at the end.
    Add {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_id: Option<String>,
    },
}

/// A step as supplied to `init` — content only; the runtime assigns the id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewStep {
    pub content: String,
}

/// Why applying a [`PlanOp`] failed. These map to `is_error` tool results, not
/// protocol errors — the model is expected to read the message and retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("no plan step with id {0:?}")]
    UnknownStep(String),
    #[error("no plan step with id {0:?} to insert after")]
    UnknownAnchor(String),
}

/// Apply one operation to `plan` in place. On success returns nothing; the
/// caller renders the updated plan as the tool result. On failure the plan is
/// left unchanged.
///
/// Id assignment: `init` numbers steps "1".."N"; `add` takes the max numeric id
/// seen plus one, so ids stay unique and stable even after cancellations.
///
/// # Errors
/// [`PlanError`] when an `id`/`after_id` does not exist. Schema-level errors
/// (bad `op`, missing `reason`) are caught earlier at deserialization.
pub fn apply_plan_op(plan: &mut Vec<PlanStep>, op: PlanOp) -> Result<(), PlanError> {
    match op {
        PlanOp::Init { steps } => {
            *plan = steps
                .into_iter()
                .enumerate()
                .map(|(i, s)| PlanStep {
                    id: (i + 1).to_string(),
                    content: s.content,
                    status: StepStatus::Pending,
                    reason: None,
                })
                .collect();
        }
        PlanOp::Start { id } => set_status(plan, &id, StepStatus::InProgress, None)?,
        PlanOp::Complete { id } => set_status(plan, &id, StepStatus::Completed, None)?,
        PlanOp::Cancel { id, reason } => {
            set_status(plan, &id, StepStatus::Cancelled, Some(reason))?;
        }
        PlanOp::Block { id, reason } => {
            set_status(plan, &id, StepStatus::Blocked, Some(reason))?;
        }
        PlanOp::Add { content, after_id } => {
            let step = PlanStep {
                id: next_id(plan),
                content,
                status: StepStatus::Pending,
                reason: None,
            };
            match after_id {
                None => plan.push(step),
                Some(anchor) => {
                    let pos = plan
                        .iter()
                        .position(|s| s.id == anchor)
                        .ok_or(PlanError::UnknownAnchor(anchor))?;
                    plan.insert(pos + 1, step);
                }
            }
        }
    }
    Ok(())
}

/// Set a step's status (and reason), or [`PlanError::UnknownStep`] if absent.
fn set_status(
    plan: &mut [PlanStep],
    id: &str,
    status: StepStatus,
    reason: Option<String>,
) -> Result<(), PlanError> {
    let step = plan
        .iter_mut()
        .find(|s| s.id == id)
        .ok_or_else(|| PlanError::UnknownStep(id.to_owned()))?;
    step.status = status;
    // Keep a prior reason on transitions that do not carry one (e.g. re-`start`
    // a blocked step) only when the new status is non-terminal; terminal status
    // changes always set the freshly-supplied reason.
    if reason.is_some() {
        step.reason = reason;
    }
    Ok(())
}

/// Next id for `add`: one past the largest numeric id currently present.
fn next_id(plan: &[PlanStep]) -> String {
    let max = plan
        .iter()
        .filter_map(|s| s.id.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    (max + 1).to_string()
}

/// Render the whole plan as the tool result the model sees after every op, so
/// it always works against the current state.
#[must_use]
pub fn render(plan: &[PlanStep]) -> String {
    use std::fmt::Write;

    if plan.is_empty() {
        return "(plan is empty)".to_owned();
    }
    let mut out = String::from("Plan:\n");
    for step in plan {
        let _ = write!(
            out,
            "  [{}] {} — {}",
            step.id,
            step.status.label(),
            step.content
        );
        if let Some(reason) = &step.reason {
            let _ = write!(out, " (reason: {reason})");
        }
        out.push('\n');
    }
    out
}

/// Render only the non-terminal steps, for the completion-gate reminder
/// (`doc/plan.md` §6).
#[must_use]
pub fn render_incomplete(plan: &[PlanStep]) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    for step in plan.iter().filter(|s| !s.status.is_terminal()) {
        let _ = writeln!(
            out,
            "  [{}] {} — {}",
            step.id,
            step.status.label(),
            step.content
        );
    }
    out
}

/// The `plan` tool descriptor the agent loop broadcasts alongside leaf tools.
///
/// Behavioral guidance lives here in the `description` (not the profile's system
/// prompt): tool usage is the tool's concern (`doc/plan.md` §9).
#[must_use]
pub fn descriptor() -> ToolSchema {
    ToolSchema {
        name: PLAN_TOOL_NAME.to_owned(),
        description: PLAN_DESCRIPTION.to_owned(),
        parameters: schema(),
    }
}

const PLAN_DESCRIPTION: &str = "\
Maintain a working plan for the current task. This tool only tracks plan state; \
it performs no actions (no file or command access).

Usage:
- For a multi-step task, first call `init` with the ordered steps, then drive \
them: `start` a step before working on it, `complete` it when done.
- Trivial single-step tasks need no plan.
- `cancel` a step ONLY when it is objectively unreachable (no such tool, no \
permission); `reason` must be specific.
- `block` a step ONLY when it needs the user (missing API key, an environment \
variable, a decision, an external command); `reason` must say what the user \
must do.
- NEVER cancel or block a step merely because it is hard or you would rather \
not do it.
- `add` inserts a new step at the end, or after `after_id`.
- Every step must reach a terminal state (completed / cancelled / blocked) \
before the task can finish.";

/// JSON Schema for the `plan` tool's single object argument.
fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["op"],
        "properties": {
            "op": {
                "type": "string",
                "enum": ["init", "start", "complete", "cancel", "block", "add"],
                "description": "Which plan operation to perform."
            },
            "steps": {
                "type": "array",
                "description": "For `init`: the ordered steps (content only; ids are assigned).",
                "items": {
                    "type": "object",
                    "required": ["content"],
                    "properties": { "content": { "type": "string" } }
                }
            },
            "id": {
                "type": "string",
                "description": "Step id for start/complete/cancel/block."
            },
            "reason": {
                "type": "string",
                "description": "Required for cancel/block; a specific, concrete reason."
            },
            "content": {
                "type": "string",
                "description": "For `add`: the new step's text."
            },
            "after_id": {
                "type": "string",
                "description": "For `add`: insert after this step id (default: append at end)."
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn op(json: &str) -> PlanOp {
        serde_json::from_str(json).unwrap()
    }

    fn init_three() -> Vec<PlanStep> {
        let mut plan = Vec::new();
        apply_plan_op(
            &mut plan,
            op(r#"{"op":"init","steps":[{"content":"a"},{"content":"b"},{"content":"c"}]}"#),
        )
        .unwrap();
        plan
    }

    #[test]
    fn init_assigns_sequential_ids_all_pending() {
        let plan = init_three();
        let ids: Vec<&str> = plan.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["1", "2", "3"]);
        assert!(plan.iter().all(|s| s.status == StepStatus::Pending));
    }

    #[test]
    fn init_replaces_existing_plan() {
        let mut plan = init_three();
        apply_plan_op(
            &mut plan,
            op(r#"{"op":"init","steps":[{"content":"only"}]}"#),
        )
        .unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].id, "1");
        assert_eq!(plan[0].content, "only");
    }

    #[test]
    fn status_transitions_apply() {
        let mut plan = init_three();
        apply_plan_op(&mut plan, op(r#"{"op":"start","id":"1"}"#)).unwrap();
        apply_plan_op(&mut plan, op(r#"{"op":"complete","id":"1"}"#)).unwrap();
        assert_eq!(plan[0].status, StepStatus::Completed);
        assert!(plan[0].status.is_terminal());
    }

    #[test]
    fn cancel_and_block_record_reason() {
        let mut plan = init_three();
        apply_plan_op(
            &mut plan,
            op(r#"{"op":"cancel","id":"2","reason":"no such tool"}"#),
        )
        .unwrap();
        apply_plan_op(
            &mut plan,
            op(r#"{"op":"block","id":"3","reason":"needs API key"}"#),
        )
        .unwrap();
        assert_eq!(plan[1].status, StepStatus::Cancelled);
        assert_eq!(plan[1].reason.as_deref(), Some("no such tool"));
        assert_eq!(plan[2].status, StepStatus::Blocked);
        assert_eq!(plan[2].reason.as_deref(), Some("needs API key"));
    }

    #[test]
    fn cancel_missing_reason_fails_to_deserialize() {
        let err = serde_json::from_str::<PlanOp>(r#"{"op":"cancel","id":"1"}"#);
        assert!(err.is_err(), "cancel without reason must be rejected");
        let err = serde_json::from_str::<PlanOp>(r#"{"op":"block","id":"1"}"#);
        assert!(err.is_err(), "block without reason must be rejected");
    }

    #[test]
    fn unknown_id_is_an_error() {
        let mut plan = init_three();
        assert_eq!(
            apply_plan_op(&mut plan, op(r#"{"op":"start","id":"99"}"#)),
            Err(PlanError::UnknownStep("99".to_owned()))
        );
    }

    #[test]
    fn add_appends_and_inserts_after() {
        let mut plan = init_three();
        apply_plan_op(&mut plan, op(r#"{"op":"add","content":"end"}"#)).unwrap();
        assert_eq!(plan.last().unwrap().id, "4");
        assert_eq!(plan.last().unwrap().content, "end");

        apply_plan_op(
            &mut plan,
            op(r#"{"op":"add","content":"mid","after_id":"1"}"#),
        )
        .unwrap();
        // Inserted right after step "1".
        let pos = plan.iter().position(|s| s.content == "mid").unwrap();
        assert_eq!(plan[pos - 1].id, "1");
        assert_eq!(plan[pos].id, "5", "id is max+1, unaffected by position");
    }

    #[test]
    fn add_after_unknown_anchor_errors() {
        let mut plan = init_three();
        assert_eq!(
            apply_plan_op(
                &mut plan,
                op(r#"{"op":"add","content":"x","after_id":"nope"}"#)
            ),
            Err(PlanError::UnknownAnchor("nope".to_owned()))
        );
    }

    #[test]
    fn render_lists_every_step_with_status_and_reason() {
        let mut plan = init_three();
        apply_plan_op(&mut plan, op(r#"{"op":"start","id":"1"}"#)).unwrap();
        apply_plan_op(
            &mut plan,
            op(r#"{"op":"block","id":"2","reason":"needs key"}"#),
        )
        .unwrap();
        let text = render(&plan);
        assert!(text.contains("[1] in_progress — a"));
        assert!(text.contains("[2] blocked — b (reason: needs key)"));
        assert!(text.contains("[3] pending — c"));
    }

    #[test]
    fn render_incomplete_only_lists_non_terminal() {
        let mut plan = init_three();
        apply_plan_op(&mut plan, op(r#"{"op":"complete","id":"1"}"#)).unwrap();
        apply_plan_op(&mut plan, op(r#"{"op":"cancel","id":"2","reason":"x"}"#)).unwrap();
        let text = render_incomplete(&plan);
        assert!(!text.contains("— a"));
        assert!(!text.contains("— b"));
        assert!(text.contains("[3] pending — c"));
    }

    #[test]
    fn descriptor_advertises_plan_tool() {
        let d = descriptor();
        assert_eq!(d.name, PLAN_TOOL_NAME);
        assert!(d.description.contains("terminal state"));
        assert_eq!(d.parameters["properties"]["op"]["enum"][0], "init");
    }
}
