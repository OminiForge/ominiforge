//! The conversation view fold: committed events → render-ready view items.
//!
//! This is the server-side counterpart to the web client's history fold
//! (`frontend/src/lib/conversation.ts`). `GET /sessions/{id}/view` folds the
//! session's committed log once, here, so opening a session never streams the
//! raw event log to the browser for a client-side fold — the client renders
//! these items directly and only folds *live* events itself.
//!
//! The fold is deliberately lossy in the same way the web fold is: it keeps
//! only what the conversation view renders (user turns, assistant text and
//! reasoning, tool calls with their results, todo cards, errors), and it
//! resolves the pairings the view needs (tool call ↔ result, permission ask ↔
//! card). Everything else (monitoring, injections, hooks) is not conversation
//! content and is skipped.
//!
//! Semantics must stay in lockstep with the web fold: any divergence renders
//! history differently from live output. Parity is currently maintained by
//! hand — `frontend/scripts/fold-parity-diff.mjs` can diff the two folds'
//! output on a real session log, but it needs manually generated inputs and
//! is not wired into CI. Treat this comment as the contract, not a
//! guarantee.

use std::collections::HashMap;

use serde::Serialize;

use crate::agent::{LeafOp, TODO_TOOL_NAME, TodoOp};
use crate::core::CoreEvent;
use crate::core::payload::{
    BlockContent, Content, EventPayload, HookEvent, HookOutcome, InjectionEvent, InjectionSource,
    ModelEvent, PermissionEvent, ToolEvent, TurnEvent,
};

/// One row of the rendered conversation.
///
/// Serde field names mirror the web client's `Item` type 1:1 (camelCase
/// included) so the frontend can consume the JSON without a mapping layer.
/// Tagged on `kind`, `snake_case`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ViewItem {
    /// A user turn, folded from `Turn::Started.input`. `seq` is that event's
    /// seq — the fork point for branching at this turn.
    User { id: u64, text: String, seq: u64 },
    /// Assistant free text (committed; never streaming here — a folded view
    /// is always settled content).
    Text { id: u64, text: String },
    /// Assistant reasoning/thinking text.
    Reasoning { id: u64, text: String },
    /// A tool call card. On a folded view every call is settled: `status` is
    /// `done`/`error` once the paired `Tool::Completed/Failed` folded, else
    /// `running` (a call whose result never committed — e.g. a cancelled
    /// turn). Fields mirror the web tool item exactly.
    Tool {
        id: u64,
        seq: u64,
        name: String,
        args: String,
        status: ViewToolStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        diagnostics: Option<String>,
        #[serde(rename = "error_code", skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
        #[serde(rename = "callId", skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        #[serde(rename = "approvalPending", skip_serializing_if = "is_false")]
        approval_pending: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        view: Option<String>,
    },
    /// A todo checklist (one card per `init`; later ops mutate it in place).
    Todo { id: u64, steps: Vec<ViewTodoStep> },
    /// A one-line activity row in the conversation flow (lighter than a tool
    /// card): todo ops and hook executions, which otherwise leave no visible
    /// trace on the timeline. Mirrors the web fold's `activity` item.
    Activity {
        id: u64,
        icon: ViewActivityIcon,
        label: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// A committed `Error::Raised`.
    Error { id: u64, message: String },
}

/// Render hint for an [`ViewItem::Activity`] row, mirroring the web fold's
/// `icon` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewActivityIcon {
    Hook,
    Todo,
    Runtime,
    /// Round-budget reminder (`InjectionSource::RoundBudget`): the loop
    /// warning the model its per-step round budget is running low or out.
    Timer,
}

// serde's `skip_serializing_if` passes a reference; the by-value signature
// clippy prefers does not fit the attribute's calling convention.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(b: &bool) -> bool {
    !*b
}

/// Settled status of a folded tool card (`running` = the result never
/// committed, e.g. the turn was cancelled mid-call).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewToolStatus {
    Running,
    Done,
    Error,
}

/// One todo step as the web client renders it (`status` lowercased, `reason`
/// only when present), mirroring the frontend's `TodoStep`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ViewTodoStep {
    pub id: String,
    pub content: String,
    pub status: ViewTodoStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Step status with the web fold's wire spelling (lowercase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewTodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
    Blocked,
}

/// The folded conversation view for one session.
///
/// Carries the render-ready items plus the high-water seq the fold reached
/// (the client resumes its live stream strictly after this) and the derived
/// view state the client cannot rebuild without folding history itself
/// (turn-running flag, runtime models seen).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionView {
    pub items: Vec<ViewItem>,
    /// The highest committed seq folded (`None` for an empty log). The client
    /// opens its live subscription with `Last-Event-ID: last_seq` so events
    /// committed between the view read and the subscribe are replayed, not
    /// lost.
    pub last_seq: Option<u64>,
    /// Whether a turn is running at the fold point, last-write-wins by the
    /// turn lifecycle (Started/Resumed → true, Completed/Failed/Interrupted
    /// → false). Drives the live-turn indicator, the Cancel affordance, and
    /// send-queueing — a client that can't see a running turn can't cancel it.
    pub turn_running: bool,
    /// Every distinct model a `RequestStarted` actually used (the runtime
    /// layer). The client validates this against the configured model and
    /// fails loud on divergence (a subagent/fork on a different model).
    pub runtime_models: Vec<String>,
}

/// One tool card being accumulated: just the index into `items` that a
/// paired `Tool::Completed/Failed` writes its outcome back to. (The call id
/// bridges permission events via the separate `tools_by_call_id` map.)
#[derive(Debug, Clone, Copy)]
struct ToolCard {
    item_index: usize,
}

/// Fold a session's committed events (in seq order) into its conversation
/// view. Single pass, O(events): every pairing is by hash lookup, mirroring
/// the web fold's maps.
#[must_use]
pub fn fold_view(events: &[CoreEvent]) -> SessionView {
    let mut items: Vec<ViewItem> = Vec::new();
    // call seq → tool card (for Tool::Completed/Failed pairing).
    let mut tools_by_seq: HashMap<u64, ToolCard> = HashMap::new();
    // call id → tool card (for Permission::Requested/Decided attachment).
    let mut tools_by_call_id: HashMap<String, usize> = HashMap::new();
    // call id → the Requested event's data, for an ask that arrived before
    // its ToolCall block (out-of-order delivery): the late block backfills.
    let mut pending_asks: HashMap<String, (u64, String, serde_json::Value)> = HashMap::new();
    let mut last_seq: Option<u64> = None;
    let mut turn_running = false;
    // Timestamp of the running turn's `Started` event, kept so the terminator
    // can compute the turn's wall-clock duration from the two committed
    // timestamps — mirroring the web fold's `turnStartedAt`. Cleared when the
    // turn ends.
    let mut turn_started_at: Option<chrono::DateTime<chrono::Utc>> = None;
    // Insertion order is preserved so the client's divergence display is
    // stable across identical folds.
    let mut runtime_models: Vec<String> = Vec::new();

    for ev in events {
        let seq = ev.seq;
        last_seq = Some(seq);
        match &ev.payload {
            EventPayload::Turn(t) => fold_turn(
                seq,
                ev.timestamp,
                t,
                &mut items,
                &mut turn_running,
                &mut turn_started_at,
            ),
            EventPayload::Model(ModelEvent::RequestStarted { model, .. }) => {
                if !runtime_models.contains(model) {
                    runtime_models.push(model.clone());
                }
            }
            EventPayload::Model(ModelEvent::ContentBlock { content, .. }) => {
                fold_block(
                    seq,
                    content,
                    &mut items,
                    &mut tools_by_seq,
                    &mut tools_by_call_id,
                    &mut pending_asks,
                );
            }
            EventPayload::Tool(t) => fold_tool(seq, t, &mut items, &tools_by_seq),
            EventPayload::Permission(p) => {
                fold_permission(seq, p, &mut items, &tools_by_call_id, &mut pending_asks);
            }
            EventPayload::Error(e) => {
                let crate::core::payload::ErrorEvent::Raised(detail) = e;
                items.push(ViewItem::Error {
                    id: seq,
                    message: detail.message.clone(),
                });
            }
            EventPayload::Hook(h) => fold_hook(seq, h, &mut items),
            EventPayload::Injection(i) => fold_injection(seq, i, &mut items),
            // Session/Artifact payloads are not conversation content (they
            // drive inspect/monitoring, not the chat view).
            _ => {}
        }
    }

    SessionView {
        items,
        last_seq,
        turn_running,
        runtime_models,
    }
}

/// Fold a turn lifecycle event: a `Started` with user input opens the user
/// bubble; the lifecycle flips `turn_running` last-write-wins. A turn ending
/// while an ask is pending leaves a zombie prompt (its Decided can never
/// commit): disarm it, mirroring the web fold's Completed/Failed/Interrupted
/// handling. A cleanly-started turn's terminator also appends the wall-clock
/// duration activity row, so the persisted view carries the same turn timer
/// the live web fold shows (otherwise it vanishes on page reload).
fn fold_turn(
    seq: u64,
    timestamp: chrono::DateTime<chrono::Utc>,
    t: &TurnEvent,
    items: &mut Vec<ViewItem>,
    turn_running: &mut bool,
    turn_started_at: &mut Option<chrono::DateTime<chrono::Utc>>,
) {
    if let TurnEvent::Started {
        input: Some(text), ..
    } = t
    {
        items.push(ViewItem::User {
            id: seq,
            text: text.clone(),
            seq,
        });
    }
    match t {
        TurnEvent::Started { .. } => {
            *turn_running = true;
            *turn_started_at = Some(timestamp);
        }
        TurnEvent::Resumed { .. } => *turn_running = true,
        TurnEvent::Completed { .. } | TurnEvent::Failed { .. } | TurnEvent::Interrupted { .. } => {
            *turn_running = false;
            for item in items.iter_mut() {
                if let ViewItem::Tool {
                    approval_pending, ..
                } = item
                {
                    *approval_pending = false;
                }
            }
            // Wall-clock duration from the committed Started/terminator
            // timestamps (covers model requests AND tool execution). Only
            // shown when the turn started cleanly — a resumed turn's Started
            // is in the prior session.
            if let Some(label) = turn_started_at
                .take()
                .and_then(|started| format_turn_duration(timestamp - started))
            {
                items.push(ViewItem::Activity {
                    id: seq,
                    icon: ViewActivityIcon::Timer,
                    label,
                    detail: None,
                });
            }
        }
    }
}

/// Format a turn's wall-clock duration for the activity timeline, mirroring
/// the web fold's `formatTurnDuration`: rounds to one decimal for sub-minute
/// turns, whole minutes:seconds above that ("3.2s", "47s", "2m 5s").
/// Returns `None` for negative/zero durations (clock skew between events).
fn format_turn_duration(d: chrono::Duration) -> Option<String> {
    let ms = d.num_milliseconds();
    if ms <= 0 {
        return None;
    }
    // Sub-minute: tenths of a second via integer math (avoids a float cast).
    if ms < 60_000 {
        return Some(format!("{}.{:01}s", ms / 1000, (ms % 1000) / 100));
    }
    let whole_secs = d.num_seconds();
    Some(format!("{}m {}s", whole_secs / 60, whole_secs % 60))
}

/// Fold a tool event: `Completed`/`Failed` pair back to the call's card via
/// `tools_by_seq`; a todo `Started` (control tool — its call folded into a
/// todo card, so there is no card to pair) surfaces an `ops` call as a
/// one-line activity row so the op itself is visible on the timeline (`init`
/// is already marked by the todo card it creates).
fn fold_tool(
    seq: u64,
    t: &ToolEvent,
    items: &mut Vec<ViewItem>,
    tools_by_seq: &HashMap<u64, ToolCard>,
) {
    match t {
        ToolEvent::Completed {
            tool_call_event_id,
            result,
            ..
        } => {
            if let Some(card) = tools_by_seq.get(&tool_call_event_id.seq) {
                let status = if result.is_error {
                    ViewToolStatus::Error
                } else {
                    ViewToolStatus::Done
                };
                apply_tool_outcome(
                    items,
                    card.item_index,
                    status,
                    &result.content,
                    result.error_code.clone(),
                );
            }
        }
        ToolEvent::Failed {
            tool_call_event_id,
            error,
            ..
        } => {
            if let Some(card) = tools_by_seq.get(&tool_call_event_id.seq) {
                let content = vec![Content::Text(error.message.clone())];
                apply_tool_outcome(
                    items,
                    card.item_index,
                    ViewToolStatus::Error,
                    &content,
                    None,
                );
            }
        }
        ToolEvent::Started {
            tool_name, input, ..
        } if tool_name == TODO_TOOL_NAME => {
            if let Some(label) = todo_op_label(input) {
                items.push(ViewItem::Activity {
                    id: seq,
                    icon: ViewActivityIcon::Todo,
                    label,
                    detail: None,
                });
            }
        }
        ToolEvent::Started { .. } => {}
    }
}

/// Fold a permission gate event: a `Requested` marks the gated call's card as
/// awaiting a decision — or, if the `ToolCall` block hasn't folded yet
/// (out-of-order delivery), is remembered for the late block to backfill.
/// `Decided` disarms the prompt. The card's live view (stage 2 of the
/// streaming pipeline) shows what the call will change; the gate itself
/// carries no view (`doc/tool-streaming.md`).
fn fold_permission(
    seq: u64,
    p: &PermissionEvent,
    items: &mut [ViewItem],
    tools_by_call_id: &HashMap<String, usize>,
    pending_asks: &mut HashMap<String, (u64, String, serde_json::Value)>,
) {
    match p {
        PermissionEvent::Requested {
            call_id,
            tool_name,
            input,
        } => {
            if let Some(&idx) = tools_by_call_id.get(call_id) {
                if let ViewItem::Tool {
                    approval_pending, ..
                } = &mut items[idx]
                {
                    *approval_pending = true;
                }
            } else {
                pending_asks.insert(call_id.clone(), (seq, tool_name.clone(), input.clone()));
            }
        }
        PermissionEvent::Decided { call_id, .. } => {
            if let Some(&idx) = tools_by_call_id.get(call_id)
                && let ViewItem::Tool {
                    approval_pending, ..
                } = &mut items[idx]
            {
                *approval_pending = false;
            }
        }
    }
}

/// Fold a hook execution into a one-line activity row: hooks act on the
/// pipeline invisibly otherwise, and a block/modify the user can't see reads
/// as the agent misbehaving.
fn fold_hook(seq: u64, h: &HookEvent, items: &mut Vec<ViewItem>) {
    let HookEvent::Executed {
        hook_name,
        hook_point,
        outcome,
        ..
    } = h;
    let (outcome_label, detail) = match outcome {
        HookOutcome::Pass => ("Pass", None),
        HookOutcome::Modified => ("Modified", None),
        HookOutcome::Observed => ("Observed", None),
        HookOutcome::Blocked { reason } => ("Blocked", Some(reason.clone())),
        HookOutcome::Failed { error } => ("Failed", Some(error.clone())),
    };
    items.push(ViewItem::Activity {
        id: seq,
        icon: ViewActivityIcon::Hook,
        label: format!("{hook_name} @ {hook_point} → {outcome_label}"),
        detail,
    });
}

/// Fold a context injection: Runtime and `RoundBudget` injections surface in
/// the flow — they are the loop nudging itself mid-turn (completion gate /
/// stuck-step / round-budget reminders, not user input, so the user must see
/// why the agent kept going). Assembly-time sources (Memory/RAG/Hook/
/// `ProjectGuidance`) stay inspect-only.
fn fold_injection(seq: u64, i: &InjectionEvent, items: &mut Vec<ViewItem>) {
    let InjectionEvent::ContextInjected {
        source, content, ..
    } = i;
    let icon = match source {
        InjectionSource::Runtime => ViewActivityIcon::Runtime,
        InjectionSource::RoundBudget => ViewActivityIcon::Timer,
        _ => return,
    };
    items.push(ViewItem::Activity {
        id: seq,
        icon,
        label: runtime_injection_label(content),
        detail: Some(content.clone()),
    });
}

/// Fold one committed content block: text/reasoning append (empty blocks are
/// dropped, mirroring the web fold); a `todo` call folds into a todo card;
/// any other tool call opens a tool card, backfilling an outstanding ask.
fn fold_block(
    seq: u64,
    content: &BlockContent,
    items: &mut Vec<ViewItem>,
    tools_by_seq: &mut HashMap<u64, ToolCard>,
    tools_by_call_id: &mut HashMap<String, usize>,
    pending_asks: &mut HashMap<String, (u64, String, serde_json::Value)>,
) {
    match content {
        BlockContent::Text { text } => {
            if !text.trim().is_empty() {
                items.push(ViewItem::Text {
                    id: seq,
                    text: text.clone(),
                });
            }
        }
        BlockContent::Reasoning { text } => {
            if !text.trim().is_empty() {
                items.push(ViewItem::Reasoning {
                    id: seq,
                    text: text.clone(),
                });
            }
        }
        BlockContent::ToolCall {
            id,
            name,
            arguments,
            summary,
        } => {
            if name == "todo" {
                fold_todo_op(seq, arguments, items);
                return;
            }
            let approval_pending = pending_asks.remove(id).is_some();
            let item_index = items.len();
            items.push(ViewItem::Tool {
                id: seq,
                seq,
                name: name.clone(),
                args: arguments.clone(),
                status: ViewToolStatus::Running,
                summary: summary.clone(),
                result: None,
                diagnostics: None,
                error_code: None,
                call_id: Some(id.clone()),
                approval_pending,
                view: None,
            });
            tools_by_seq.insert(seq, ToolCard { item_index });
            tools_by_call_id.insert(id.clone(), item_index);
        }
    }
}

/// Apply a settled outcome to a tool card: status + the result/diagnostics/
/// view split, mirroring the web fold's `pairResult` exactly — `TextView`
/// (audience "ui") becomes `view`; `Text` blocks join into the result; the
/// built-in file tools split trailing `Text` off as LSP diagnostics.
fn apply_tool_outcome(
    items: &mut [ViewItem],
    item_index: usize,
    status: ViewToolStatus,
    content: &[Content],
    error_code: Option<String>,
) {
    let Some(ViewItem::Tool {
        name,
        status: s,
        result,
        diagnostics,
        view,
        error_code: ec,
        ..
    }) = items.get_mut(item_index)
    else {
        return;
    };
    let is_assist_tool = name == "read" || name == "write" || name == "edit";
    let mut texts: Vec<&str> = Vec::new();
    let mut ui_view: Option<String> = None;
    for c in content {
        match c {
            Content::Text(t) => texts.push(t),
            Content::TextView { text, audience }
                if audience == crate::core::payload::AUDIENCE_UI =>
            {
                ui_view = Some(text.clone());
            }
            _ => texts.push("[binary]"),
        }
    }
    let (text, diag) = if is_assist_tool && texts.len() > 1 {
        (texts[0].to_owned(), Some(texts[1..].concat()))
    } else {
        (texts.concat(), None)
    };
    *s = status;
    *result = Some(text);
    *diagnostics = diag;
    *view = ui_view;
    *ec = error_code;
}

/// One-line label for a runtime-injected reminder (completion gate /
/// stuck-step), mirroring the web fold's `runtimeInjectionLabel`: the first
/// line of the `<reminder>` block with the tags stripped. The prefix names
/// the audience — the reminder addresses the MODEL, not the user.
fn runtime_injection_label(content: &str) -> String {
    let stripped = content.replace("<reminder>", "").replace("</reminder>", "");
    let first_line = stripped.trim().lines().next().unwrap_or("");
    format!("Model reminder: {first_line}")
}

/// One-line label for a committed todo `ops` call (from its `Tool::Started`
/// input), mirroring the web fold's `todoOpLabel`: `None` for `init` (the
/// todo card itself marks it) and for malformed input. Fields are read
/// tolerantly (reasons are optional) so a schema-invalid op the runtime will
/// reject as `is_error` still gets a row — hiding it would leave the failed
/// op invisible.
fn todo_op_label(input: &serde_json::Value) -> Option<String> {
    let ops = input.get("ops")?.as_array()?;
    if ops.is_empty() {
        return Some("Updated todo list (no changes)".to_owned());
    }
    let mut parts: Vec<String> = Vec::with_capacity(ops.len());
    for op in ops {
        let kind = op.get("op")?.as_str()?;
        let id = || {
            op.get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?")
        };
        let reason = op.get("reason").and_then(serde_json::Value::as_str);
        let part = match kind {
            "start" => format!("Started step {}", id()),
            "complete" => format!("Completed step {}", id()),
            "cancel" => reason.map_or_else(
                || format!("Cancelled step {}", id()),
                |r| format!("Cancelled step {}: {r}", id()),
            ),
            "block" => reason.map_or_else(
                || format!("Blocked step {}", id()),
                |r| format!("Blocked step {}: {r}", id()),
            ),
            "add" => {
                let content = op.get("content").and_then(serde_json::Value::as_str)?;
                format!("Added step: {content}")
            }
            _ => return None,
        };
        parts.push(part);
    }
    Some(parts.join(" · "))
}

/// Fold one committed `todo` call: `init` pushes a fresh card (ids "1".."N");
/// `ops` mutate the LATEST card in place. Malformed ops are a benign no-op,
/// mirroring both the runtime and the web fold.
fn fold_todo_op(seq: u64, args: &str, items: &mut Vec<ViewItem>) {
    let Ok(op) = serde_json::from_str::<TodoOp>(args) else {
        return;
    };
    match op {
        TodoOp::Init { steps } => {
            let steps = steps
                .into_iter()
                .enumerate()
                .map(|(i, s)| ViewTodoStep {
                    id: (i + 1).to_string(),
                    content: s.content,
                    status: ViewTodoStatus::Pending,
                    reason: None,
                })
                .collect();
            items.push(ViewItem::Todo { id: seq, steps });
        }
        TodoOp::Ops { ops } => {
            let Some(ViewItem::Todo { steps, .. }) = items
                .iter_mut()
                .rev()
                .find(|i| matches!(i, ViewItem::Todo { .. }))
            else {
                return;
            };
            for op in ops {
                apply_leaf(steps, op);
            }
        }
    }
}

/// Apply one leaf op to a step list (unknown id/anchor = no-op), mirroring
/// the web fold's `applyLeafOp`.
fn apply_leaf(steps: &mut Vec<ViewTodoStep>, op: LeafOp) {
    match op {
        LeafOp::Start { id } => set_status(steps, &id, ViewTodoStatus::InProgress, None),
        LeafOp::Complete { id } => set_status(steps, &id, ViewTodoStatus::Completed, None),
        LeafOp::Cancel { id, reason } => {
            set_status(steps, &id, ViewTodoStatus::Cancelled, Some(reason));
        }
        LeafOp::Block { id, reason } => {
            set_status(steps, &id, ViewTodoStatus::Blocked, Some(reason));
        }
        LeafOp::Add { content, after_id } => {
            let max = steps
                .iter()
                .filter_map(|s| s.id.parse::<u64>().ok())
                .max()
                .unwrap_or(0);
            let step = ViewTodoStep {
                id: (max + 1).to_string(),
                content,
                status: ViewTodoStatus::Pending,
                reason: None,
            };
            match after_id.and_then(|a| steps.iter().position(|s| s.id == a)) {
                Some(at) => steps.insert(at + 1, step),
                None => steps.push(step),
            }
        }
    }
}

fn set_status(
    steps: &mut [ViewTodoStep],
    id: &str,
    status: ViewTodoStatus,
    reason: Option<String>,
) {
    if let Some(step) = steps.iter_mut().find(|s| s.id == id) {
        step.status = status;
        step.reason = reason.or_else(|| step.reason.clone());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::core::EventId;
    use crate::core::payload::{
        ErrorDetail, ErrorSeverity, HookEvent, HookOutcome, ModelEvent, PermissionOutcome,
        SessionEvent, StopReason, ToolOutput, ToolSource, TurnEvent, Usage,
    };
    use crate::core::{EventSource, SCHEMA_VERSION, SessionId, SourceKind};
    use chrono::{TimeZone, Utc};

    fn ev(seq: u64, payload: EventPayload) -> CoreEvent {
        CoreEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            seq,
            session_id: SessionId("s".to_owned()),
            timestamp: Utc.with_ymd_and_hms(2026, 6, 24, 0, 0, 0).unwrap(),
            source: EventSource {
                kind: SourceKind::Runtime,
                id: "test".to_owned(),
            },
            parent_event_id: None,
            turn_id: None,
            payload,
        }
    }

    fn block(seq: u64, content: BlockContent) -> CoreEvent {
        ev(
            seq,
            EventPayload::Model(ModelEvent::ContentBlock {
                request_id: "r".into(),
                index: 0,
                content,
            }),
        )
    }

    fn tool_call(id: &str, name: &str, args: &str) -> BlockContent {
        BlockContent::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.into(),
            summary: None,
        }
    }

    fn completed(seq: u64, call_seq: u64, text: &str) -> CoreEvent {
        ev(
            seq,
            EventPayload::Tool(ToolEvent::Completed {
                tool_call_event_id: EventId {
                    session_id: SessionId("s".into()),
                    seq: call_seq,
                },
                result: ToolOutput {
                    content: vec![Content::Text(text.into())],
                    is_error: false,
                    error_code: None,
                },
                duration_ms: 1,
                output_bytes: 1,
                artifacts_created: vec![],
            }),
        )
    }

    /// A full turn folds into the exact item sequence the web fold produces:
    /// user → reasoning → text → settled tool card, with the result paired by
    /// the `ToolCall` block's seq.
    #[test]
    fn a_turn_folds_into_view_items() {
        let events = vec![
            ev(
                0,
                EventPayload::Session(SessionEvent::Created {
                    profile_id: None,
                    tools: vec![],
                    workspace: None,
                }),
            ),
            ev(
                1,
                EventPayload::Turn(TurnEvent::Started {
                    turn_id: crate::core::TurnId("t".into()),
                    input: Some("hello".into()),
                }),
            ),
            block(
                2,
                BlockContent::Reasoning {
                    text: "think".into(),
                },
            ),
            block(
                3,
                BlockContent::Text {
                    text: "answer".into(),
                },
            ),
            block(4, tool_call("c1", "read", "{\"path\":\"a.txt\"}")),
            completed(5, 4, "[a.txt]\n1:hi"),
            ev(
                6,
                EventPayload::Turn(TurnEvent::Completed {
                    turn_id: crate::core::TurnId("t".into()),
                }),
            ),
        ];
        let view = fold_view(&events);
        assert_eq!(view.last_seq, Some(6));
        let kinds: Vec<&str> = view
            .items
            .iter()
            .map(|i| match i {
                ViewItem::User { .. } => "user",
                ViewItem::Reasoning { .. } => "reasoning",
                ViewItem::Text { .. } => "text",
                ViewItem::Tool { .. } => "tool",
                ViewItem::Todo { .. } => "todo",
                ViewItem::Activity { .. } => "activity",
                ViewItem::Error { .. } => "error",
            })
            .collect();
        assert_eq!(kinds, ["user", "reasoning", "text", "tool"]);
        let ViewItem::Tool { status, result, .. } = &view.items[3] else {
            panic!("tool card")
        };
        assert_eq!(*status, ViewToolStatus::Done);
        assert_eq!(result.as_deref(), Some("[a.txt]\n1:hi"));
    }

    /// An ask that arrives before its `ToolCall` block (out-of-order delivery)
    /// must attach to the late block's card — the prompt is never lost.
    #[test]
    fn an_early_permission_ask_backfills_the_late_tool_card() {
        let events = vec![
            ev(
                1,
                EventPayload::Permission(PermissionEvent::Requested {
                    call_id: "c1".into(),
                    tool_name: "write".into(),
                    input: serde_json::json!({"path":"x.txt"}),
                }),
            ),
            block(2, tool_call("c1", "write", "{\"path\":\"x.txt\"}")),
            ev(
                3,
                EventPayload::Permission(PermissionEvent::Decided {
                    call_id: "c1".into(),
                    outcome: PermissionOutcome::Approved,
                    decided_by: "user".into(),
                    scope: None,
                }),
            ),
            completed(4, 2, "wrote x.txt"),
        ];
        let view = fold_view(&events);
        assert_eq!(
            view.items.len(),
            1,
            "one card, completed in place — never duplicated"
        );
        let ViewItem::Tool {
            approval_pending,
            status,
            ..
        } = &view.items[0]
        else {
            panic!("tool card")
        };
        assert!(!approval_pending, "the Decided cleared it");
        assert_eq!(*status, ViewToolStatus::Done);
    }

    /// A turn ending mid-ask disarms the zombie prompt (its Decided can never
    /// commit), mirroring the web fold's Interrupted handling.
    #[test]
    fn an_interrupted_turn_disarms_a_pending_ask() {
        let events = vec![
            block(1, tool_call("c1", "shell", "{\"command\":\"x\"}")),
            ev(
                2,
                EventPayload::Permission(PermissionEvent::Requested {
                    call_id: "c1".into(),
                    tool_name: "shell".into(),
                    input: serde_json::json!({}),
                }),
            ),
            ev(
                3,
                EventPayload::Turn(TurnEvent::Interrupted {
                    turn_id: crate::core::TurnId("t".into()),
                    interrupted_at_event_id: EventId {
                        session_id: SessionId("s".into()),
                        seq: 2,
                    },
                }),
            ),
        ];
        let view = fold_view(&events);
        let ViewItem::Tool {
            approval_pending, ..
        } = &view.items[0]
        else {
            panic!("tool card")
        };
        assert!(!approval_pending);
    }

    /// Todo calls fold into one card per init; later ops mutate the latest
    /// card in place (never a generic tool block, never a second card).
    #[test]
    fn todo_calls_fold_into_cards() {
        let events = vec![
            block(
                1,
                tool_call(
                    "p1",
                    "todo",
                    r#"{"op":"init","steps":[{"content":"a"},{"content":"b"}]}"#,
                ),
            ),
            block(
                2,
                tool_call(
                    "p2",
                    "todo",
                    r#"{"ops":[{"op":"start","id":"1"},{"op":"complete","id":"1"}]}"#,
                ),
            ),
        ];
        let view = fold_view(&events);
        assert_eq!(view.items.len(), 1);
        let ViewItem::Todo { steps, .. } = &view.items[0] else {
            panic!("todo card")
        };
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].status, ViewTodoStatus::Completed);
        assert_eq!(steps[1].status, ViewTodoStatus::Pending);
    }

    /// A todo `ops` call surfaces as a one-line activity row (the op itself is
    /// otherwise invisible once its card scrolls away); `init` does not — the
    /// fresh card already marks it. Non-todo `Tool::Started` events fold to
    /// nothing, as before.
    #[test]
    fn todo_ops_fold_into_activity_rows() {
        let started = |seq: u64, input: serde_json::Value| {
            ev(
                seq,
                EventPayload::Tool(ToolEvent::Started {
                    tool_call_event_id: EventId {
                        session_id: SessionId("s".into()),
                        seq,
                    },
                    tool_name: "todo".into(),
                    source: ToolSource::Builtin,
                    input,
                    working_dir: None,
                }),
            )
        };
        let events = vec![
            block(
                1,
                tool_call(
                    "p1",
                    "todo",
                    r#"{"op":"init","steps":[{"content":"a"},{"content":"b"}]}"#,
                ),
            ),
            started(
                2,
                serde_json::json!({"op":"init","steps":[{"content":"a"},{"content":"b"}]}),
            ),
            started(
                3,
                serde_json::json!({"ops":[{"op":"start","id":"1"},{"op":"complete","id":"1"}]}),
            ),
            started(
                4,
                serde_json::json!({"ops":[{"op":"block","id":"2","reason":"needs API key"}]}),
            ),
        ];
        let view = fold_view(&events);
        let kinds: Vec<&str> = view
            .items
            .iter()
            .map(|i| match i {
                ViewItem::Todo { .. } => "todo",
                ViewItem::Activity { .. } => "activity",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["todo", "activity", "activity"]);
        let ViewItem::Activity { icon, label, .. } = &view.items[1] else {
            panic!("activity row")
        };
        assert_eq!(*icon, ViewActivityIcon::Todo);
        assert_eq!(label, "Started step 1 · Completed step 1");
        let ViewItem::Activity { label, .. } = &view.items[2] else {
            panic!("activity row")
        };
        assert_eq!(label, "Blocked step 2: needs API key");
    }

    /// Every hook execution folds into a one-line activity row: hooks act on
    /// the pipeline invisibly otherwise, and a block the user can't see reads
    /// as the agent misbehaving.
    #[test]
    fn hook_executions_fold_into_activity_rows() {
        let hook = |seq: u64, outcome: HookOutcome| {
            ev(
                seq,
                EventPayload::Hook(HookEvent::Executed {
                    hook_name: "security-guard".into(),
                    hook_point: "tool:invoke:before".into(),
                    outcome,
                    duration_ms: 3,
                }),
            )
        };
        let view = fold_view(&[
            hook(1, HookOutcome::Pass),
            hook(
                2,
                HookOutcome::Blocked {
                    reason: "rm -rf".into(),
                },
            ),
        ]);
        assert_eq!(view.items.len(), 2);
        let ViewItem::Activity {
            icon,
            label,
            detail,
            ..
        } = &view.items[0]
        else {
            panic!("activity row")
        };
        assert_eq!(*icon, ViewActivityIcon::Hook);
        assert_eq!(label, "security-guard @ tool:invoke:before → Pass");
        assert_eq!(*detail, None);
        let ViewItem::Activity { label, detail, .. } = &view.items[1] else {
            panic!("activity row")
        };
        assert_eq!(label, "security-guard @ tool:invoke:before → Blocked");
        assert_eq!(detail.as_deref(), Some("rm -rf"));
    }

    /// A runtime reminder (completion gate / stuck-step nudge) folds into an
    /// activity row — it is the loop nudging itself mid-turn, not user input,
    /// so the user must see why the agent kept going. Assembly-time sources
    /// (Memory et al.) stay inspect-only.
    #[test]
    fn runtime_injections_fold_into_activity_rows() {
        let inject = |seq: u64, source: crate::core::payload::InjectionSource, content: &str| {
            ev(
                seq,
                EventPayload::Injection(crate::core::payload::InjectionEvent::ContextInjected {
                    source,
                    content: content.into(),
                    token_count: 10,
                }),
            )
        };
        let reminder = "<reminder>The following todo items are not in a terminal state. \
                        Continue working on them:\n- [ ] 2. write tests</reminder>";
        let view = fold_view(&[
            inject(1, crate::core::payload::InjectionSource::Runtime, reminder),
            inject(
                2,
                crate::core::payload::InjectionSource::Memory,
                "user prefers dark mode",
            ),
        ]);
        assert_eq!(view.items.len(), 1);
        let ViewItem::Activity {
            icon,
            label,
            detail,
            ..
        } = &view.items[0]
        else {
            panic!("activity row")
        };
        assert_eq!(*icon, ViewActivityIcon::Runtime);
        assert_eq!(
            label,
            "Model reminder: The following todo items are not in a terminal state. Continue working on them:"
        );
        assert_eq!(detail.as_deref(), Some(reminder));
    }

    /// A round-budget reminder folds into an activity row with the `Timer`
    /// icon, so the user can tell a budget nudge apart from a completion-gate
    /// nudge (`doc/architecture.md` §8.6).
    #[test]
    fn round_budget_injections_fold_into_timer_activity_rows() {
        let inject = |seq: u64, content: &str| {
            ev(
                seq,
                EventPayload::Injection(crate::core::payload::InjectionEvent::ContextInjected {
                    source: crate::core::payload::InjectionSource::RoundBudget,
                    content: content.into(),
                    token_count: 10,
                }),
            )
        };
        let reminder = "<reminder>You have used 16/20 rounds this turn (4 left) without an active todo \
             list. If this is a multi-step task, open a todo now.</reminder>";
        let view = fold_view(&[inject(1, reminder)]);
        assert_eq!(view.items.len(), 1);
        let ViewItem::Activity { icon, label, .. } = &view.items[0] else {
            panic!("activity row")
        };
        assert_eq!(*icon, ViewActivityIcon::Timer);
        assert!(label.starts_with("Model reminder: You have used 16/20"));
    }

    /// The file tools split a trailing Text block off as diagnostics; other
    /// tools keep all Text in the result. TextView(ui) lands on `view`.
    #[test]
    fn tool_outcome_splits_diagnostics_and_view() {
        let events = vec![
            block(1, tool_call("c1", "write", "{}")),
            ev(
                2,
                EventPayload::Tool(ToolEvent::Completed {
                    tool_call_event_id: EventId {
                        session_id: SessionId("s".into()),
                        seq: 1,
                    },
                    result: ToolOutput {
                        content: vec![
                            Content::Text("wrote a.rs".into()),
                            Content::TextView {
                                text: "DIFF".into(),
                                audience: "ui".into(),
                            },
                            Content::Text("[diagnostics: a.rs] err".into()),
                        ],
                        is_error: false,
                        error_code: None,
                    },
                    duration_ms: 1,
                    output_bytes: 1,
                    artifacts_created: vec![],
                }),
            ),
        ];
        let view = fold_view(&events);
        let ViewItem::Tool {
            result,
            diagnostics,
            view,
            ..
        } = &view.items[0]
        else {
            panic!("tool card")
        };
        assert_eq!(result.as_deref(), Some("wrote a.rs"));
        assert_eq!(diagnostics.as_deref(), Some("[diagnostics: a.rs] err"));
        assert_eq!(view.as_deref(), Some("DIFF"));
    }

    /// Non-conversation payloads (model request lifecycle, session, hooks)
    /// produce no items, and errors fold as error items.
    #[test]
    fn non_conversation_payloads_are_skipped() {
        let events = vec![
            ev(
                0,
                EventPayload::Session(SessionEvent::Created {
                    profile_id: None,
                    tools: vec![],
                    workspace: None,
                }),
            ),
            ev(
                1,
                EventPayload::Model(ModelEvent::RequestStarted {
                    request_id: "r".into(),
                    provider: "p".into(),
                    model: "m".into(),
                    temperature: 0.0,
                    max_tokens: None,
                    tool_schemas_count: 0,
                    input_tokens_estimate: 0,
                }),
            ),
            ev(
                2,
                EventPayload::Model(ModelEvent::RequestCompleted {
                    request_id: "r".into(),
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                    duration_ms: 1,
                    time_to_first_token_ms: None,
                    provider_request_id: None,
                }),
            ),
            ev(
                3,
                EventPayload::Error(crate::core::payload::ErrorEvent::Raised(ErrorDetail {
                    code: "x".into(),
                    message: "boom".into(),
                    severity: ErrorSeverity::Error,
                    retryable: false,
                    source_event_id: None,
                    provider_raw: None,
                })),
            ),
        ];
        let view = fold_view(&events);
        assert_eq!(view.items.len(), 1);
        let ViewItem::Error { message, .. } = &view.items[0] else {
            panic!("error item")
        };
        assert_eq!(message, "boom");
    }

    /// `turn_running` reconstructs from the committed turn lifecycle, so a client
    /// opening a session mid-turn sees the live indicator + Cancel affordance
    /// (and send-queueing) exactly as if it had folded the history itself.
    #[test]
    fn turn_running_tracks_the_lifecycle() {
        // An unfinished turn: running.
        let running = fold_view(&[ev(
            1,
            EventPayload::Turn(TurnEvent::Started {
                turn_id: crate::core::TurnId("t".into()),
                input: Some("go".into()),
            }),
        )]);
        assert!(running.turn_running);

        // A completed turn: not running.
        let done = fold_view(&[
            ev(
                1,
                EventPayload::Turn(TurnEvent::Started {
                    turn_id: crate::core::TurnId("t".into()),
                    input: Some("go".into()),
                }),
            ),
            ev(
                2,
                EventPayload::Turn(TurnEvent::Completed {
                    turn_id: crate::core::TurnId("t".into()),
                }),
            ),
        ]);
        assert!(!done.turn_running);

        // A second turn re-arms after the first settled (last-write-wins).
        let rearmed = fold_view(&[
            ev(
                1,
                EventPayload::Turn(TurnEvent::Started {
                    turn_id: crate::core::TurnId("t".into()),
                    input: Some("one".into()),
                }),
            ),
            ev(
                2,
                EventPayload::Turn(TurnEvent::Completed {
                    turn_id: crate::core::TurnId("t".into()),
                }),
            ),
            ev(
                3,
                EventPayload::Turn(TurnEvent::Started {
                    turn_id: crate::core::TurnId("t2".into()),
                    input: Some("two".into()),
                }),
            ),
        ]);
        assert!(rearmed.turn_running);
    }

    /// `runtime_models` collects every distinct model a `RequestStarted` used,
    /// deduplicated, in first-seen order — the divergence-check source (B4).
    #[test]
    fn runtime_models_are_collected_deduped() {
        let started = |seq, model: &str| {
            ev(
                seq,
                EventPayload::Model(ModelEvent::RequestStarted {
                    request_id: format!("r{seq}"),
                    provider: "p".into(),
                    model: model.into(),
                    temperature: 0.0,
                    max_tokens: None,
                    tool_schemas_count: 0,
                    input_tokens_estimate: 0,
                }),
            )
        };
        let view = fold_view(&[
            started(1, "sonnet"),
            started(2, "sonnet"),
            started(3, "haiku"),
            started(4, "sonnet"),
        ]);
        assert_eq!(view.runtime_models, ["sonnet", "haiku"]);
        assert!(fold_view(&[]).runtime_models.is_empty());
    }

    /// A turn event `secs` seconds after the shared `ev` helper's pinned
    /// instant (which alone can't express a non-zero duration).
    fn ev_at(seq: u64, secs: i64, payload: EventPayload) -> CoreEvent {
        let mut e = ev(seq, payload);
        e.timestamp += chrono::Duration::seconds(secs);
        e
    }

    /// A cleanly-started turn's terminator appends the wall-clock duration
    /// activity row — this is what lets the turn timer survive a page reload
    /// (the web fold only shows it live; the persisted view must carry it).
    /// Mirrors the web fold's `formatTurnDuration`.
    #[test]
    fn a_completed_turn_folds_into_a_timer_activity_row() {
        let turn = |seq, secs, started: Option<&str>| {
            let tid = || crate::core::TurnId("t".into());
            let payload = started.map_or_else(
                || EventPayload::Turn(TurnEvent::Completed { turn_id: tid() }),
                |input| {
                    EventPayload::Turn(TurnEvent::Started {
                        turn_id: tid(),
                        input: Some(input.into()),
                    })
                },
            );
            ev_at(seq, secs, payload)
        };

        // Sub-minute: one decimal.
        let view = fold_view(&[turn(1, 0, Some("go")), turn(2, 3, None)]);
        let last = view.items.last().unwrap();
        let ViewItem::Activity { icon, label, .. } = last else {
            panic!("timer activity row, got {last:?}")
        };
        assert_eq!(*icon, ViewActivityIcon::Timer);
        assert_eq!(label, "3.0s");

        // Over a minute: whole minutes:seconds.
        let view = fold_view(&[turn(1, 0, Some("go")), turn(2, 125, None)]);
        let ViewItem::Activity { label, .. } = view.items.last().unwrap() else {
            panic!("timer activity row")
        };
        assert_eq!(label, "2m 5s");
    }

    /// A resumed turn's `Started` lives in the prior session, so there is no
    /// clean start timestamp — no timer row (mirrors the web fold, which only
    /// shows the duration "when the turn started cleanly").
    #[test]
    fn a_resumed_turn_yields_no_timer_row() {
        let resumed = ev_at(
            1,
            0,
            EventPayload::Turn(TurnEvent::Resumed {
                turn_id: crate::core::TurnId("t".into()),
                resume_from_event_id: EventId {
                    session_id: SessionId("s".into()),
                    seq: 0,
                },
            }),
        );
        let completed = ev_at(
            2,
            30,
            EventPayload::Turn(TurnEvent::Completed {
                turn_id: crate::core::TurnId("t".into()),
            }),
        );
        let view = fold_view(&[resumed, completed]);
        assert!(
            !view
                .items
                .iter()
                .any(|i| matches!(i, ViewItem::Activity { .. }))
        );
    }
}
