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
//! reasoning, tool calls with their results, plan cards, errors), and it
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

use crate::agent::{LeafOp, PlanOp};
use crate::core::CoreEvent;
use crate::core::payload::{BlockContent, Content, EventPayload, PermissionEvent, ToolEvent};

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
        #[serde(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    /// A plan checklist (one card per `init`; later ops mutate it in place).
    Plan { id: u64, steps: Vec<ViewPlanStep> },
    /// A committed `Error::Raised`.
    Error { id: u64, message: String },
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

/// One plan step as the web client renders it (`status` lowercased, `reason`
/// only when present), mirroring the frontend's `PlanStep`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ViewPlanStep {
    pub id: String,
    pub content: String,
    pub status: ViewPlanStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Step status with the web fold's wire spelling (lowercase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewPlanStatus {
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
    let mut pending_asks: HashMap<String, (u64, String, serde_json::Value, Option<String>)> =
        HashMap::new();
    let mut last_seq: Option<u64> = None;
    let mut turn_running = false;
    // Insertion order is preserved so the client's divergence display is
    // stable across identical folds.
    let mut runtime_models: Vec<String> = Vec::new();

    for ev in events {
        let seq = ev.seq;
        last_seq = Some(seq);
        match &ev.payload {
            EventPayload::Turn(t) => {
                if let crate::core::payload::TurnEvent::Started {
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
                    crate::core::payload::TurnEvent::Started { .. }
                    | crate::core::payload::TurnEvent::Resumed { .. } => turn_running = true,
                    crate::core::payload::TurnEvent::Completed { .. }
                    | crate::core::payload::TurnEvent::Failed { .. }
                    | crate::core::payload::TurnEvent::Interrupted { .. } => turn_running = false,
                }
                // A turn ending while an ask is pending leaves a zombie prompt
                // (its Decided can never commit): disarm it, mirroring the web
                // fold's Completed/Failed/Interrupted handling.
                if matches!(
                    t,
                    crate::core::payload::TurnEvent::Completed { .. }
                        | crate::core::payload::TurnEvent::Failed { .. }
                        | crate::core::payload::TurnEvent::Interrupted { .. }
                ) {
                    for item in &mut items {
                        if let ViewItem::Tool {
                            approval_pending, ..
                        } = item
                        {
                            *approval_pending = false;
                        }
                    }
                }
            }
            EventPayload::Model(crate::core::payload::ModelEvent::RequestStarted {
                model, ..
            }) => {
                if !runtime_models.contains(model) {
                    runtime_models.push(model.clone());
                }
            }
            EventPayload::Model(crate::core::payload::ModelEvent::ContentBlock {
                content, ..
            }) => {
                fold_block(
                    seq,
                    content,
                    &mut items,
                    &mut tools_by_seq,
                    &mut tools_by_call_id,
                    &mut pending_asks,
                );
            }
            EventPayload::Tool(t) => match t {
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
                            &mut items,
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
                            &mut items,
                            card.item_index,
                            ViewToolStatus::Error,
                            &content,
                            None,
                        );
                    }
                }
                ToolEvent::Started { .. } => {}
            },
            EventPayload::Permission(p) => match p {
                PermissionEvent::Requested {
                    call_id,
                    tool_name,
                    input,
                    preview,
                } => {
                    if let Some(&idx) = tools_by_call_id.get(call_id) {
                        if let ViewItem::Tool {
                            approval_pending,
                            preview: slot,
                            ..
                        } = &mut items[idx]
                        {
                            *approval_pending = true;
                            slot.clone_from(preview);
                        }
                    } else {
                        // The ToolCall block hasn't folded (out-of-order
                        // delivery): remember the ask; the late block
                        // backfills the card with it.
                        pending_asks.insert(
                            call_id.clone(),
                            (seq, tool_name.clone(), input.clone(), preview.clone()),
                        );
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
            },
            EventPayload::Error(e) => {
                let crate::core::payload::ErrorEvent::Raised(detail) = e;
                items.push(ViewItem::Error {
                    id: seq,
                    message: detail.message.clone(),
                });
            }
            // Session/Artifact/Injection/Hook payloads are not conversation
            // content (they drive inspect/monitoring, not the chat view).
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

/// Fold one committed content block: text/reasoning append (empty blocks are
/// dropped, mirroring the web fold); a `plan` call folds into a plan card;
/// any other tool call opens a tool card, backfilling an outstanding ask.
fn fold_block(
    seq: u64,
    content: &BlockContent,
    items: &mut Vec<ViewItem>,
    tools_by_seq: &mut HashMap<u64, ToolCard>,
    tools_by_call_id: &mut HashMap<String, usize>,
    pending_asks: &mut HashMap<String, (u64, String, serde_json::Value, Option<String>)>,
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
            if name == "plan" {
                fold_plan_op(seq, arguments, items);
                return;
            }
            let (approval_pending, preview) = pending_asks
                .remove(id)
                .map_or((false, None), |(_, _, _, p)| (true, p));
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
                preview,
            });
            tools_by_seq.insert(seq, ToolCard { item_index });
            tools_by_call_id.insert(id.clone(), item_index);
        }
    }
}

/// Apply a settled outcome to a tool card: status + the result/diagnostics/
/// view split, mirroring the web fold's `pairResult` exactly — `TextView`
/// (audience "ui") becomes `view`; `Text` blocks join into the result; the
/// built-in file tools split trailing `Text` off as LSP diagnostics; a
/// missing view keeps the approval preview.
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
        preview,
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
    *view = ui_view.or_else(|| preview.clone());
    *ec = error_code;
}

/// Fold one committed `plan` call: `init` pushes a fresh card (ids "1".."N");
/// `ops` mutate the LATEST card in place. Malformed ops are a benign no-op,
/// mirroring both the runtime and the web fold.
fn fold_plan_op(seq: u64, args: &str, items: &mut Vec<ViewItem>) {
    let Ok(op) = serde_json::from_str::<PlanOp>(args) else {
        return;
    };
    match op {
        PlanOp::Init { steps } => {
            let steps = steps
                .into_iter()
                .enumerate()
                .map(|(i, s)| ViewPlanStep {
                    id: (i + 1).to_string(),
                    content: s.content,
                    status: ViewPlanStatus::Pending,
                    reason: None,
                })
                .collect();
            items.push(ViewItem::Plan { id: seq, steps });
        }
        PlanOp::Ops { ops } => {
            let Some(ViewItem::Plan { steps, .. }) = items
                .iter_mut()
                .rev()
                .find(|i| matches!(i, ViewItem::Plan { .. }))
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
fn apply_leaf(steps: &mut Vec<ViewPlanStep>, op: LeafOp) {
    match op {
        LeafOp::Start { id } => set_status(steps, &id, ViewPlanStatus::InProgress, None),
        LeafOp::Complete { id } => set_status(steps, &id, ViewPlanStatus::Completed, None),
        LeafOp::Cancel { id, reason } => {
            set_status(steps, &id, ViewPlanStatus::Cancelled, Some(reason))
        }
        LeafOp::Block { id, reason } => {
            set_status(steps, &id, ViewPlanStatus::Blocked, Some(reason))
        }
        LeafOp::Add { content, after_id } => {
            let max = steps
                .iter()
                .filter_map(|s| s.id.parse::<u64>().ok())
                .max()
                .unwrap_or(0);
            let step = ViewPlanStep {
                id: (max + 1).to_string(),
                content,
                status: ViewPlanStatus::Pending,
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
    steps: &mut [ViewPlanStep],
    id: &str,
    status: ViewPlanStatus,
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
        ErrorDetail, ErrorSeverity, ModelEvent, PermissionOutcome, SessionEvent, StopReason,
        ToolOutput, TurnEvent, Usage,
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
                ViewItem::Plan { .. } => "plan",
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
                    preview: Some("DIFF".into()),
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
            preview,
            view,
            status,
            ..
        } = &view.items[0]
        else {
            panic!("tool card")
        };
        assert!(!approval_pending, "the Decided cleared it");
        assert_eq!(
            view.as_deref(),
            Some("DIFF"),
            "no executed view → the preview stays"
        );
        assert_eq!(*status, ViewToolStatus::Done);
        let _ = preview;
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
                    preview: None,
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

    /// Plan calls fold into one card per init; later ops mutate the latest
    /// card in place (never a generic tool block, never a second card).
    #[test]
    fn plan_calls_fold_into_cards() {
        let events = vec![
            block(
                1,
                tool_call(
                    "p1",
                    "plan",
                    r#"{"op":"init","steps":[{"content":"a"},{"content":"b"}]}"#,
                ),
            ),
            block(
                2,
                tool_call(
                    "p2",
                    "plan",
                    r#"{"ops":[{"op":"start","id":"1"},{"op":"complete","id":"1"}]}"#,
                ),
            ),
        ];
        let view = fold_view(&events);
        assert_eq!(view.items.len(), 1);
        let ViewItem::Plan { steps, .. } = &view.items[0] else {
            panic!("plan card")
        };
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].status, ViewPlanStatus::Completed);
        assert_eq!(steps[1].status, ViewPlanStatus::Pending);
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
}
