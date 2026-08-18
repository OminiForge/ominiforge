//! In-turn working todo list: pure state plus the operations that mutate it.
//!
//! The todo list is the agent's working checklist for a longer goal — it keeps
//! a turn from losing track of what it set out to do across many model rounds.
//! It is *session-scoped* (lives in [`super::SessionRuntime`], survives across
//! turns) but holds no I/O: this module is just the data model, the op-based
//! mutation, and the rendering the model sees. See `doc/design/runtime-architecture.md`.
//!
//! `todo` is a *control* tool, not a leaf tool: it operates on the agent's own
//! state rather than the outside world. So it does **not** implement [`Tool`]
//! and is **not** in the [`ToolRegistry`] — the agent loop contributes its
//! [`descriptor`] alongside the leaf-tool schemas and intercepts the call by
//! name, applying [`apply_todo_op`] to the live todo list. See `doc/design/runtime-architecture.md`
//! §5.
//!
//! [`Tool`]: crate::tool::Tool
//! [`ToolRegistry`]: crate::tool::ToolRegistry

use serde::{Deserialize, Serialize};

use crate::llm::ToolSchema;

/// The tool name the model uses and the loop intercepts.
pub const TODO_TOOL_NAME: &str = "todo";

/// One item of the working todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// Stable id assigned by the runtime on `init`/`add` ("1", "2", ...). The
    /// model refers to steps by this id; it never changes once assigned.
    pub id: String,
    pub content: String,
    pub status: StepStatus,
    /// Required for `cancelled`/`blocked` (the why); optional otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Lifecycle state of a [`TodoItem`].
///
/// Terminal states are `Completed`/`Cancelled`/`Blocked`; `Pending`/`InProgress`
/// are non-terminal and hold a turn open at the completion gate (`doc/design/runtime-architecture.md`
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

    /// A short label for todo rendering.
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

/// One `todo` tool call, decoded from the model's arguments.
///
/// Two shapes only: `{"op": "init", "steps": [...]}` establishes the list, and
/// `{"ops": [...]}` mutates it — a single change is a one-element `ops` array,
/// matching the batch-first shape of the other tools. `init` cannot appear
/// inside `ops`: [`LeafOp`] has no such variant, so nesting is rejected at
/// deserialization with no runtime check (`doc/design/runtime-architecture.md` §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TodoOp {
    /// Establish the list from scratch; any existing list is replaced. Ids are
    /// assigned by the runtime, not the model.
    Init { steps: Vec<NewStep> },
    /// Apply several leaf ops in array order. Stops at the first error; the
    /// ops before it stay applied.
    Ops { ops: Vec<LeafOp> },
}

/// A single list mutation — the only element allowed inside [`TodoOp::Ops`].
///
/// Externally tagged on `op`. Missing required fields (e.g. `reason` on
/// `cancel`/`block`) fail to deserialize and surface as a tool error the model
/// corrects next round (`doc/design/runtime-architecture.md` §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LeafOp {
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

/// Why applying a [`TodoOp`] failed. These map to `is_error` tool results, not
/// protocol errors — the model is expected to read the message and retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TodoError {
    #[error("no todo item with id {0:?}")]
    UnknownStep(String),
    #[error("no todo item with id {0:?} to insert after")]
    UnknownAnchor(String),
    /// One op inside `ops` failed (1-based position). The ops before it were
    /// applied; it and the rest were not.
    #[error("op {index} failed: {source}")]
    OpFailed { index: usize, source: Box<Self> },
}

/// Apply one call to `todo` in place. On success returns nothing; the caller
/// renders the updated list as the tool result. On failure the list holds
/// whatever the ops before the failing one applied.
///
/// Id assignment: `init` numbers steps "1".."N"; `add` takes the max numeric id
/// seen plus one, so ids stay unique and stable even after cancellations.
///
/// # Errors
/// [`TodoError`] when an `id`/`after_id` does not exist. Schema-level errors
/// (bad `op`, missing `reason`) are caught earlier at deserialization.
pub fn apply_todo_op(plan: &mut Vec<TodoItem>, op: TodoOp) -> Result<(), TodoError> {
    match op {
        TodoOp::Init { steps } => {
            *plan = steps
                .into_iter()
                .enumerate()
                .map(|(i, s)| TodoItem {
                    id: (i + 1).to_string(),
                    content: s.content,
                    status: StepStatus::Pending,
                    reason: None,
                })
                .collect();
        }
        // Sequential, stop-at-first-error: earlier ops stay applied (so an
        // `add` may be referenced by a later op in the same call), and the
        // error names the failing position.
        TodoOp::Ops { ops } => {
            for (i, op) in ops.into_iter().enumerate() {
                apply_leaf_op(plan, op).map_err(|e| TodoError::OpFailed {
                    index: i + 1,
                    source: Box::new(e),
                })?;
            }
        }
    }
    Ok(())
}

/// Apply one leaf op to `todo` in place.
fn apply_leaf_op(plan: &mut Vec<TodoItem>, op: LeafOp) -> Result<(), TodoError> {
    match op {
        LeafOp::Start { id } => set_status(plan, &id, StepStatus::InProgress, None),
        LeafOp::Complete { id } => set_status(plan, &id, StepStatus::Completed, None),
        LeafOp::Cancel { id, reason } => set_status(plan, &id, StepStatus::Cancelled, Some(reason)),
        LeafOp::Block { id, reason } => set_status(plan, &id, StepStatus::Blocked, Some(reason)),
        LeafOp::Add { content, after_id } => {
            let step = TodoItem {
                id: next_id(plan),
                content,
                status: StepStatus::Pending,
                reason: None,
            };
            match after_id {
                None => {
                    plan.push(step);
                    Ok(())
                }
                Some(anchor) => {
                    let pos = plan
                        .iter()
                        .position(|s| s.id == anchor)
                        .ok_or(TodoError::UnknownAnchor(anchor))?;
                    plan.insert(pos + 1, step);
                    Ok(())
                }
            }
        }
    }
}

/// Set a step's status (and reason), or [`TodoError::UnknownStep`] if absent.
fn set_status(
    plan: &mut [TodoItem],
    id: &str,
    status: StepStatus,
    reason: Option<String>,
) -> Result<(), TodoError> {
    let step = plan
        .iter_mut()
        .find(|s| s.id == id)
        .ok_or_else(|| TodoError::UnknownStep(id.to_owned()))?;
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
fn next_id(plan: &[TodoItem]) -> String {
    let max = plan
        .iter()
        .filter_map(|s| s.id.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    (max + 1).to_string()
}

/// Render the whole list as the tool result the model sees after every op, so
/// it always works against the current state.
#[must_use]
pub fn render(plan: &[TodoItem]) -> String {
    use std::fmt::Write;

    if plan.is_empty() {
        return "(todo list is empty)".to_owned();
    }
    let mut out = String::from("Todo list:\n");
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
/// (`doc/design/runtime-architecture.md` §6).
#[must_use]
pub fn render_incomplete(plan: &[TodoItem]) -> String {
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

/// The `todo` tool descriptor the agent loop broadcasts alongside leaf tools.
///
/// Behavioral guidance lives here in the `description` (not the profile's system
/// prompt): tool usage is the tool's concern (`doc/design/runtime-architecture.md` §9). Structural
/// facts (which ops exist, which fields each takes) belong to the schema alone
/// — the description does not repeat them.
#[must_use]
pub fn descriptor() -> ToolSchema {
    ToolSchema {
        name: TODO_TOOL_NAME.to_owned(),
        description: TODO_DESCRIPTION.to_owned(),
        parameters: schema(),
    }
}

const TODO_DESCRIPTION: &str = "\
Maintain a working todo list for the current task. This tool only tracks todo \
state; it performs no actions (no file or command access).

Usage:
- For a multi-step task, first call with `init`, then drive the steps: \
`start` a step before working on it, `complete` it when done. Mutations go \
through `ops` — one call, in order; do not fire several parallel todo calls. \
A single change is a one-element `ops` array.
- Step ids are assigned by the runtime, never by you: `init`'s `steps` carry \
`content` only; refer to steps by the ids the tool result shows. The result \
re-renders the full list after every call, so it always reflects current state.
- Ops apply in order and stop at the first error; the ones before it stay \
applied.
- Trivial single-step tasks need no todo list.
- `cancel` a step ONLY when it is objectively unreachable (no such tool, no \
permission); `reason` must be specific.
- `block` a step ONLY when it needs the user (missing API key, an environment \
variable, a decision, an external command); `reason` must say what the user \
must do.
- NEVER cancel or block a step merely because it is hard or you would rather \
not do it.
- Every step must reach a terminal state (completed / cancelled / blocked) \
before the task can finish.

Round budget: each turn and each todo step runs under a soft round budget. \
As the budget runs low — and again when it runs out — the loop will remind \
you. Plan steps that fit a budget; split a large step into smaller ones via \
`add` + `cancel` rather than letting one step run long.";

/// JSON Schema for the `todo` tool arguments: two shapes, `oneOf`.
///
/// - `{"op": "init", "steps": [...]}` — establish the list.
/// - `{"ops": [leaf, ...]}` — mutate it; each leaf is exactly one op with
///   only its own fields (`oneOf` per op, `additionalProperties: false`), so
///   `init` cannot nest and a malformed leaf fails schema validation.
fn schema() -> serde_json::Value {
    let id_prop =
        |description: &str| serde_json::json!({ "type": "string", "description": description });
    let leaf = |op: &str, extra: serde_json::Value, required: &[&str]| {
        let mut properties = serde_json::Map::new();
        properties.insert("op".to_owned(), serde_json::json!({ "const": op }));
        if let serde_json::Value::Object(map) = extra {
            properties.extend(map);
        }
        let mut req = vec!["op"];
        req.extend_from_slice(required);
        serde_json::json!({
            "type": "object",
            "properties": serde_json::Value::Object(properties),
            "required": req,
            "additionalProperties": false
        })
    };
    let start = leaf(
        "start",
        serde_json::json!({ "id": id_prop("Step id to mark in_progress.") }),
        &["id"],
    );
    let complete = leaf(
        "complete",
        serde_json::json!({ "id": id_prop("Step id to mark completed.") }),
        &["id"],
    );
    let cancel = leaf(
        "cancel",
        serde_json::json!({
            "id": id_prop("Step id to mark cancelled."),
            "reason": { "type": "string", "description": "Why the step is objectively unreachable; be specific." }
        }),
        &["id", "reason"],
    );
    let block = leaf(
        "block",
        serde_json::json!({
            "id": id_prop("Step id to mark blocked."),
            "reason": { "type": "string", "description": "What the user must provide or decide; be specific." }
        }),
        &["id", "reason"],
    );
    let add = leaf(
        "add",
        serde_json::json!({
            "content": { "type": "string", "description": "The new step's text." },
            "after_id": id_prop("Insert after this step id (default: append at the end).")
        }),
        &["content"],
    );

    serde_json::json!({
        "type": "object",
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "op": { "const": "init" },
                    "steps": {
                        "type": "array",
                        "minItems": 1,
                        "description": "The ordered steps (content only; ids are assigned by the runtime).",
                        "items": {
                            "type": "object",
                            "properties": { "content": { "type": "string" } },
                            "required": ["content"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["op", "steps"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "ops": {
                        "type": "array",
                        "minItems": 1,
                        "description": "The ops to apply in order.",
                        "items": { "oneOf": [start, complete, cancel, block, add] }
                    }
                },
                "required": ["ops"],
                "additionalProperties": false
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn op(json: &str) -> TodoOp {
        serde_json::from_str(json).unwrap()
    }

    fn init_three() -> Vec<TodoItem> {
        let mut plan = Vec::new();
        apply_todo_op(
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
    fn init_replaces_existing_list() {
        let mut plan = init_three();
        apply_todo_op(
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
        apply_todo_op(&mut plan, op(r#"{"ops":[{"op":"start","id":"1"}]}"#)).unwrap();
        apply_todo_op(&mut plan, op(r#"{"ops":[{"op":"complete","id":"1"}]}"#)).unwrap();
        assert_eq!(plan[0].status, StepStatus::Completed);
        assert!(plan[0].status.is_terminal());
    }

    #[test]
    fn cancel_and_block_record_reason() {
        let mut plan = init_three();
        apply_todo_op(
            &mut plan,
            op(r#"{"ops":[{"op":"cancel","id":"2","reason":"no such tool"}]}"#),
        )
        .unwrap();
        apply_todo_op(
            &mut plan,
            op(r#"{"ops":[{"op":"block","id":"3","reason":"needs API key"}]}"#),
        )
        .unwrap();
        assert_eq!(plan[1].status, StepStatus::Cancelled);
        assert_eq!(plan[1].reason.as_deref(), Some("no such tool"));
        assert_eq!(plan[2].status, StepStatus::Blocked);
        assert_eq!(plan[2].reason.as_deref(), Some("needs API key"));
    }

    #[test]
    fn cancel_missing_reason_fails_to_deserialize() {
        let err = serde_json::from_str::<TodoOp>(r#"{"ops":[{"op":"cancel","id":"1"}]}"#);
        assert!(err.is_err(), "cancel without reason must be rejected");
        let err = serde_json::from_str::<TodoOp>(r#"{"ops":[{"op":"block","id":"1"}]}"#);
        assert!(err.is_err(), "block without reason must be rejected");
    }

    #[test]
    fn unknown_id_is_an_error() {
        let mut plan = init_three();
        assert_eq!(
            apply_todo_op(&mut plan, op(r#"{"ops":[{"op":"start","id":"99"}]}"#)),
            Err(TodoError::OpFailed {
                index: 1,
                source: Box::new(TodoError::UnknownStep("99".to_owned()))
            })
        );
    }

    #[test]
    fn add_appends_and_inserts_after() {
        let mut plan = init_three();
        apply_todo_op(&mut plan, op(r#"{"ops":[{"op":"add","content":"end"}]}"#)).unwrap();
        assert_eq!(plan.last().unwrap().id, "4");
        assert_eq!(plan.last().unwrap().content, "end");

        apply_todo_op(
            &mut plan,
            op(r#"{"ops":[{"op":"add","content":"mid","after_id":"1"}]}"#),
        )
        .unwrap();
        // Inserted right after step "1".
        let pos = plan.iter().position(|s| s.content == "mid").unwrap();
        assert_eq!(plan[pos - 1].id, "1");
        assert_eq!(plan[pos].id, "5", "id is max+1, unaffected by position");
    }

    #[test]
    fn ops_apply_in_order() {
        let mut plan = init_three();
        apply_todo_op(
            &mut plan,
            op(r#"{"ops":[
                {"op":"start","id":"1"},
                {"op":"complete","id":"1"},
                {"op":"cancel","id":"2","reason":"obsolete"}
            ]}"#),
        )
        .unwrap();
        assert_eq!(plan[0].status, StepStatus::Completed);
        assert_eq!(plan[1].status, StepStatus::Cancelled);
        assert_eq!(plan[1].reason.as_deref(), Some("obsolete"));
        assert_eq!(plan[2].status, StepStatus::Pending);
    }

    #[test]
    fn ops_stop_at_first_error_keeping_earlier_ops() {
        let mut plan = init_three();
        let err = apply_todo_op(
            &mut plan,
            op(r#"{"ops":[
                {"op":"complete","id":"1"},
                {"op":"start","id":"99"},
                {"op":"complete","id":"3"}
            ]}"#),
        )
        .unwrap_err();
        assert_eq!(
            err,
            TodoError::OpFailed {
                index: 2,
                source: Box::new(TodoError::UnknownStep("99".to_owned()))
            }
        );
        // Op 1 landed; op 3 never ran.
        assert_eq!(plan[0].status, StepStatus::Completed);
        assert_eq!(plan[2].status, StepStatus::Pending);
    }

    #[test]
    fn ops_can_start_a_step_added_in_the_same_call() {
        let mut plan = init_three();
        apply_todo_op(
            &mut plan,
            op(r#"{"ops":[
                {"op":"add","content":"new"},
                {"op":"start","id":"4"}
            ]}"#),
        )
        .unwrap();
        assert_eq!(plan[3].content, "new");
        assert_eq!(plan[3].status, StepStatus::InProgress);
    }

    #[test]
    fn init_cannot_nest_inside_ops() {
        // `init` is not a `LeafOp` variant, so it surfaces as the usual
        // "unknown variant" tool error — no runtime check needed.
        let err = serde_json::from_str::<TodoOp>(r#"{"ops":[{"op":"init","steps":[]}]}"#);
        assert!(err.is_err(), "init must not nest inside ops");
    }

    #[test]
    fn add_after_unknown_anchor_errors() {
        let mut plan = init_three();
        assert_eq!(
            apply_todo_op(
                &mut plan,
                op(r#"{"ops":[{"op":"add","content":"x","after_id":"nope"}]}"#)
            ),
            Err(TodoError::OpFailed {
                index: 1,
                source: Box::new(TodoError::UnknownAnchor("nope".to_owned()))
            })
        );
    }

    #[test]
    fn render_lists_every_step_with_status_and_reason() {
        let mut plan = init_three();
        apply_todo_op(&mut plan, op(r#"{"ops":[{"op":"start","id":"1"}]}"#)).unwrap();
        apply_todo_op(
            &mut plan,
            op(r#"{"ops":[{"op":"block","id":"2","reason":"needs key"}]}"#),
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
        apply_todo_op(&mut plan, op(r#"{"ops":[{"op":"complete","id":"1"}]}"#)).unwrap();
        apply_todo_op(
            &mut plan,
            op(r#"{"ops":[{"op":"cancel","id":"2","reason":"x"}]}"#),
        )
        .unwrap();
        let text = render_incomplete(&plan);
        assert!(!text.contains("— a"));
        assert!(!text.contains("— b"));
        assert!(text.contains("[3] pending — c"));
    }

    #[test]
    fn descriptor_advertises_todo_tool() {
        let d = descriptor();
        assert_eq!(d.name, TODO_TOOL_NAME);
        assert!(d.description.contains("terminal state"));
        assert_eq!(d.parameters["type"], "object");
        let one_of = d.parameters["oneOf"].as_array().unwrap();
        assert_eq!(one_of.len(), 2, "init branch and ops branch");
        // init branch: only op + steps.
        assert_eq!(one_of[0]["properties"]["op"]["const"], "init");
        assert_eq!(one_of[0]["required"], serde_json::json!(["op", "steps"]));
        // ops branch: each leaf is one of the five ops, no init nesting.
        let leaf_ops = one_of[1]["properties"]["ops"]["items"]["oneOf"]
            .as_array()
            .unwrap();
        let consts: Vec<&str> = leaf_ops
            .iter()
            .map(|l| l["properties"]["op"]["const"].as_str().unwrap())
            .collect();
        assert_eq!(consts, ["start", "complete", "cancel", "block", "add"]);
    }
}
