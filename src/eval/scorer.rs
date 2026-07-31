//! The [`Scorer`] trait and the built-in deterministic scorers.
//!
//! A scorer is a fold over a completed run (`doc/eval.md` §3.2): it reads the
//! [`EvalContext`] and returns one [`Score`]. The event-only scorers here are
//! pure functions of the event stream — they need neither the rebuilt messages
//! nor the scratch workspace, so they run in unit tests with an
//! events-only context. The workspace/test-execution scorers (`TestsPass`,
//! `WorkspaceDiff`) arrive with the runner in Step 3.
//!
//! Scoring is separate from metric aggregation: a scorer emits one verdict per
//! sample; cross-sample reduction (`pass_rate` / pass@k / pass^k) is the runner's
//! job (Step 3).

use async_trait::async_trait;

use crate::core::CoreEvent;
use crate::core::payload::{EventPayload, ToolEvent, ToolOutput, TurnEvent};

use super::case::Checker;
use super::score::{EvalContext, Score};

/// Scores one dimension of an agent run.
///
/// Deterministic scorers are pure functions of [`EvalContext`]; an LLM-judge
/// scorer (Step 4+) will call a provider inside `score`, which is why the
/// method is async even though the built-ins here do no I/O.
#[async_trait]
pub trait Scorer: Send + Sync {
    /// Stable identifier, persisted in `scores.jsonl` (`doc/eval.md` §5.3).
    fn name(&self) -> &str;

    /// Judge the run described by `ctx`.
    async fn score(&self, ctx: &EvalContext<'_>) -> Score;
}

// ── shared event helpers ───────────────────────────────────────────────────────

/// The concatenated assistant free-text across the whole run.
///
/// Reads persisted `ModelEvent::ContentBlock` text blocks (the consolidated
/// form; see `doc/event-schema.md` §5). This is the model's answer as recorded,
/// independent of the rebuilt `messages`, so match scorers work in unit tests.
fn assistant_text(events: &[CoreEvent]) -> String {
    use crate::core::payload::{BlockContent, ModelEvent};

    let mut out = String::new();
    for e in events {
        if let EventPayload::Model(ModelEvent::ContentBlock {
            content: BlockContent::Text { text },
            ..
        }) = &e.payload
        {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

/// Normalize a string for exact matching: trim, collapse internal whitespace,
/// lowercase. Keeps `ExactMatch` from failing on trivial formatting drift while
/// still demanding the same content.
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

// ── ExactMatch ─────────────────────────────────────────────────────────────────

/// Passes when the run's assistant text, normalized, equals the case `target`.
///
/// Skips (not fails) when the checker is not `exact` or the target is empty —
/// an empty target means "nothing to match against", which is a case-authoring
/// gap, not a run failure.
pub struct ExactMatch;

#[async_trait]
impl Scorer for ExactMatch {
    fn name(&self) -> &'static str {
        "ExactMatch"
    }

    async fn score(&self, ctx: &EvalContext<'_>) -> Score {
        if !matches!(ctx.case.checker, Checker::Exact) {
            return Score::skip("checker is not `exact`");
        }
        let Some(target) = ctx.case.target.as_deref().filter(|t| !t.trim().is_empty()) else {
            return Score::skip("case has no target to match against");
        };
        let got = assistant_text(ctx.events);
        if normalize(&got) == normalize(target) {
            Score::pass("assistant text matches target (normalized)")
        } else {
            Score::fail(format!(
                "assistant text does not match target; got {:?}",
                got.trim()
            ))
        }
    }
}

// ── FuzzyMatch ─────────────────────────────────────────────────────────────────

/// Passes when the run's assistant text contains the normalized `target` as a substring.
///
/// A cheap, deterministic stand-in for semantic similarity: it tolerates
/// surrounding prose (the model answering in a sentence) but still requires the
/// expected content verbatim. Embedding similarity is a later upgrade
/// (`doc/eval.md` §3.3) and would replace this body, not the interface.
pub struct FuzzyMatch;

#[async_trait]
impl Scorer for FuzzyMatch {
    fn name(&self) -> &'static str {
        "FuzzyMatch"
    }

    async fn score(&self, ctx: &EvalContext<'_>) -> Score {
        if !matches!(ctx.case.checker, Checker::Fuzzy) {
            return Score::skip("checker is not `fuzzy`");
        }
        let Some(target) = ctx.case.target.as_deref().filter(|t| !t.trim().is_empty()) else {
            return Score::skip("case has no target to match against");
        };
        let got = normalize(&assistant_text(ctx.events));
        let want = normalize(target);
        if got.contains(&want) {
            Score::pass("assistant text contains target (normalized substring)")
        } else {
            Score::fail("assistant text does not contain target")
        }
    }
}

// ── NoToolError ────────────────────────────────────────────────────────────────

/// Passes when no tool call failed, in either of the two forms the tool
/// protocol distinguishes (`doc/eval.md` §3.3, `doc/tool-protocol.md` §7):
///
/// - `ToolEvent::Failed` — a protocol failure (spawn error, timeout).
/// - `ToolEvent::Completed { result.is_error = true }` — a business failure
///   (the invocation succeeded but the tool reported an error).
///
/// A scorer that only checked `Failed` would silently pass a run where every
/// tool returned `is_error`, so both are counted.
pub struct NoToolError;

#[async_trait]
impl Scorer for NoToolError {
    fn name(&self) -> &'static str {
        "NoToolError"
    }

    async fn score(&self, ctx: &EvalContext<'_>) -> Score {
        let mut protocol = 0usize;
        let mut business = 0usize;
        for e in ctx.events {
            match &e.payload {
                EventPayload::Tool(ToolEvent::Failed { .. }) => protocol += 1,
                EventPayload::Tool(ToolEvent::Completed {
                    result: ToolOutput { is_error: true, .. },
                    ..
                }) => business += 1,
                _ => {}
            }
        }
        if protocol == 0 && business == 0 {
            Score::pass("no tool failures")
        } else {
            Score::fail(format!(
                "{protocol} protocol failure(s), {business} business error(s)"
            ))
        }
    }
}

// ── TurnCompleted ──────────────────────────────────────────────────────────────

/// Passes when the run's turn(s) all ended in `TurnEvent::Completed` — no
/// `Failed` (graceful stop or hard error) and no `Interrupted`
/// (`doc/eval.md` §3.3, `doc/event-schema.md` §4).
///
/// Requires at least one `Completed` and zero non-clean endings; a run with no
/// turn at all skips (nothing happened to judge).
pub struct TurnCompleted;

#[async_trait]
impl Scorer for TurnCompleted {
    fn name(&self) -> &'static str {
        "TurnCompleted"
    }

    async fn score(&self, ctx: &EvalContext<'_>) -> Score {
        let mut completed = 0usize;
        let mut failed = 0usize;
        let mut interrupted = 0usize;
        for e in ctx.events {
            match &e.payload {
                EventPayload::Turn(TurnEvent::Completed { .. }) => completed += 1,
                EventPayload::Turn(TurnEvent::Failed { .. }) => failed += 1,
                EventPayload::Turn(TurnEvent::Interrupted { .. }) => interrupted += 1,
                _ => {}
            }
        }
        if completed == 0 && failed == 0 && interrupted == 0 {
            return Score::skip("no turn ending recorded");
        }
        if failed == 0 && interrupted == 0 {
            Score::pass(format!("{completed} turn(s) completed cleanly"))
        } else {
            Score::fail(format!(
                "{failed} failed, {interrupted} interrupted turn(s)"
            ))
        }
    }
}

// ── TestsPass ──────────────────────────────────────────────────────────────────

/// Passes when the scratch workspace's test command exits 0 and every
/// `pass_pattern` appears in the combined output.
///
/// Only meaningful with `checker.kind = "tests"`. Skips for all other checker
/// kinds and when `ctx.workspace` is `None`.
pub struct TestsPass;

#[async_trait]
impl Scorer for TestsPass {
    fn name(&self) -> &'static str {
        "TestsPass"
    }

    async fn score(&self, ctx: &EvalContext<'_>) -> Score {
        use crate::eval::case::Checker;

        let Checker::Tests {
            command,
            pass_patterns,
        } = &ctx.case.checker
        else {
            return Score::skip("not a tests checker");
        };

        let Some(workspace) = ctx.workspace else {
            return Score::skip("no workspace available");
        };

        // Run the command in the scratch workspace.
        let output = match std::process::Command::new("sh")
            .args(["-c", command])
            .current_dir(workspace)
            .output()
        {
            Ok(out) => out,
            Err(e) => {
                return Score::fail(format!("failed to spawn command `{command}`: {e}"));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{stdout}{stderr}");

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return Score::fail(format!("command `{command}` exited {code}\n{combined}"));
        }

        // Check that every pass_pattern appears in the combined output.
        let missing: Vec<&str> = pass_patterns
            .iter()
            .filter(|p| !combined.contains(p.as_str()))
            .map(String::as_str)
            .collect();

        if missing.is_empty() {
            Score::pass(format!(
                "command `{command}` passed ({} pattern(s) matched)",
                pass_patterns.len()
            ))
        } else {
            Score::fail(format!(
                "command `{command}` passed but missing patterns: {missing:?}"
            ))
        }
    }
}

// ── WorkspaceDiff ──────────────────────────────────────────────────────────────

/// Verifies the scratch workspace state matches the case's `state` checker
/// expectations: each expected file must exist (or be absent) with the right
/// content.
///
/// Only meaningful with `checker.kind = "state"`. Skips for all other checker
/// kinds and when `ctx.workspace` is `None`.
pub struct WorkspaceDiff;

#[async_trait]
impl Scorer for WorkspaceDiff {
    fn name(&self) -> &'static str {
        "WorkspaceDiff"
    }

    async fn score(&self, ctx: &EvalContext<'_>) -> Score {
        use crate::eval::case::Checker;

        let Checker::State { expect } = &ctx.case.checker else {
            return Score::skip("not a state checker");
        };

        let Some(workspace) = ctx.workspace else {
            return Score::skip("no workspace available");
        };

        let mut failures: Vec<String> = Vec::new();

        for entry in expect {
            let path = workspace.join(&entry.path);

            if entry.absent {
                if path.exists() {
                    failures.push(format!(
                        "{}: expected absent but exists",
                        entry.path.display()
                    ));
                }
                continue;
            }

            // File must exist.
            match std::fs::read_to_string(&path) {
                Err(e) => {
                    failures.push(format!(
                        "{}: expected to exist but cannot read: {e}",
                        entry.path.display()
                    ));
                }
                Ok(actual) => {
                    if entry
                        .content
                        .as_deref()
                        .is_some_and(|exp| actual.trim() != exp.trim())
                    {
                        let exp = entry.content.as_deref().unwrap_or("");
                        failures.push(format!(
                            "{}: content mismatch\n  expected: {:?}\n  actual:   {:?}",
                            entry.path.display(),
                            exp.trim(),
                            actual.trim()
                        ));
                    }
                }
            }
        }

        if failures.is_empty() {
            Score::pass(format!("{} file expectation(s) satisfied", expect.len()))
        } else {
            Score::fail(failures.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use crate::core::payload::{BlockContent, ErrorDetail, ErrorSeverity, ModelEvent};
    use crate::core::payload::{EventPayload, ToolEvent, ToolOutput, TurnEvent};
    use crate::core::{
        CoreEvent, EventId, EventSource, SCHEMA_VERSION, SessionId, SourceKind, TurnId,
    };
    use crate::eval::case::{Checker, EvalCase};
    use crate::eval::score::{EvalContext, ScoreValue};

    fn sid() -> SessionId {
        SessionId("01SYNTHETICSCORER0000000TST".to_owned())
    }

    fn ev(seq: u64, kind: SourceKind, id: &str, payload: EventPayload) -> CoreEvent {
        CoreEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            seq,
            session_id: sid(),
            timestamp: chrono::Utc::now(),
            source: EventSource {
                kind,
                id: id.to_owned(),
            },
            parent_event_id: None,
            turn_id: Some(TurnId("t".to_owned())),
            payload,
        }
    }

    fn eid(seq: u64) -> EventId {
        EventId {
            session_id: sid(),
            seq,
        }
    }

    fn text_block(text: &str) -> EventPayload {
        EventPayload::Model(ModelEvent::ContentBlock {
            request_id: "r1".to_owned(),
            index: 0,
            content: BlockContent::Text {
                text: text.to_owned(),
            },
        })
    }

    fn turn_completed() -> EventPayload {
        EventPayload::Turn(TurnEvent::Completed {
            turn_id: TurnId("t".to_owned()),
        })
    }

    fn turn_failed() -> EventPayload {
        EventPayload::Turn(TurnEvent::Failed {
            turn_id: TurnId("t".to_owned()),
            failed_at_event_id: eid(1),
            retryable: false,
            reason: None,
        })
    }

    fn tool_completed(is_error: bool) -> EventPayload {
        EventPayload::Tool(ToolEvent::Completed {
            tool_call_event_id: eid(1),
            result: ToolOutput {
                content: vec![],
                is_error,
                error_code: None,
            },
            duration_ms: 1,
            output_bytes: 0,
            artifacts_created: vec![],
        })
    }

    fn tool_failed() -> EventPayload {
        EventPayload::Tool(ToolEvent::Failed {
            tool_call_event_id: eid(1),
            duration_ms: 1,
            error: ErrorDetail {
                code: "spawn_failed".to_owned(),
                message: "boom".to_owned(),
                severity: ErrorSeverity::Error,
                retryable: false,
                source_event_id: None,
                provider_raw: None,
            },
        })
    }

    fn case_with(checker: Checker, target: Option<&str>) -> EvalCase {
        EvalCase {
            id: "c1".to_owned(),
            source: crate::eval::case::CaseSource::Manual,
            status: crate::eval::case::CaseStatus::Approved,
            origin_session: None,
            input: "do the thing".to_owned(),
            target: target.map(str::to_owned),
            tags: vec![],
            difficulty: None,
            files: vec![],
            checker,
        }
    }

    fn ctx<'a>(events: &'a [CoreEvent], case: &'a EvalCase) -> EvalContext<'a> {
        EvalContext {
            events,
            messages: None,
            workspace: None,
            case,
        }
    }

    // ── ExactMatch ──────────────────────────────────────────────────────────

    /// `ExactMatch` must pass only when the assistant text equals the target after
    /// normalization — otherwise a Q&A case would score correct on a wrong answer.
    #[tokio::test]
    async fn exact_match_passes_on_normalized_equality() {
        let case = case_with(Checker::Exact, Some("The Answer"));
        let events = vec![ev(
            0,
            SourceKind::Model,
            "m",
            text_block("  the   answer  "),
        )];
        let score = ExactMatch.score(&ctx(&events, &case)).await;
        assert_eq!(score.value, ScoreValue::Pass);
    }

    /// A different answer must fail, not pass — this is the whole point of the scorer.
    #[tokio::test]
    async fn exact_match_fails_on_wrong_answer() {
        let case = case_with(Checker::Exact, Some("42"));
        let events = vec![ev(0, SourceKind::Model, "m", text_block("41"))];
        let score = ExactMatch.score(&ctx(&events, &case)).await;
        assert_eq!(score.value, ScoreValue::Fail);
    }

    /// Wrong checker kind must skip, not fail — `ExactMatch` has no opinion on a
    /// `tests` case, and a spurious Fail would corrupt that case's pass rate.
    #[tokio::test]
    async fn exact_match_skips_when_checker_is_not_exact() {
        let case = case_with(
            Checker::Tests {
                command: "cargo test".to_owned(),
                pass_patterns: vec![],
            },
            None,
        );
        let events = vec![ev(0, SourceKind::Model, "m", text_block("42"))];
        let score = ExactMatch.score(&ctx(&events, &case)).await;
        assert!(score.value.is_skip());
    }

    // ── FuzzyMatch ──────────────────────────────────────────────────────────

    /// `FuzzyMatch` passes when the target appears as a substring — tolerating the
    /// model answering in a full sentence rather than a bare token.
    #[tokio::test]
    async fn fuzzy_match_passes_on_substring() {
        let case = case_with(Checker::Fuzzy, Some("Paris"));
        let events = vec![ev(
            0,
            SourceKind::Model,
            "m",
            text_block("The capital of France is Paris."),
        )];
        let score = FuzzyMatch.score(&ctx(&events, &case)).await;
        assert_eq!(score.value, ScoreValue::Pass);
    }

    /// If the target is absent from the text, fuzzy must fail — substring is the
    /// weakest bar; failing it means the answer is genuinely missing.
    #[tokio::test]
    async fn fuzzy_match_fails_when_target_absent() {
        let case = case_with(Checker::Fuzzy, Some("Berlin"));
        let events = vec![ev(
            0,
            SourceKind::Model,
            "m",
            text_block("The capital of France is Paris."),
        )];
        let score = FuzzyMatch.score(&ctx(&events, &case)).await;
        assert_eq!(score.value, ScoreValue::Fail);
    }

    // ── NoToolError ─────────────────────────────────────────────────────────

    /// A clean run with only successful tool calls passes.
    #[tokio::test]
    async fn no_tool_error_passes_when_all_tools_succeed() {
        let case = case_with(Checker::Exact, Some("x"));
        let events = vec![
            ev(0, SourceKind::Tool, "shell", tool_completed(false)),
            ev(1, SourceKind::Tool, "read", tool_completed(false)),
        ];
        let score = NoToolError.score(&ctx(&events, &case)).await;
        assert_eq!(score.value, ScoreValue::Pass);
    }

    /// A business error (Completed with `is_error=true`) must fail — this is the
    /// case a naive "only check Failed" scorer would wrongly pass.
    #[tokio::test]
    async fn no_tool_error_fails_on_business_error() {
        let case = case_with(Checker::Exact, Some("x"));
        let events = vec![ev(0, SourceKind::Tool, "shell", tool_completed(true))];
        let score = NoToolError.score(&ctx(&events, &case)).await;
        assert_eq!(score.value, ScoreValue::Fail);
    }

    /// A protocol failure (`ToolEvent::Failed`) must also fail.
    #[tokio::test]
    async fn no_tool_error_fails_on_protocol_failure() {
        let case = case_with(Checker::Exact, Some("x"));
        let events = vec![ev(0, SourceKind::Tool, "shell", tool_failed())];
        let score = NoToolError.score(&ctx(&events, &case)).await;
        assert_eq!(score.value, ScoreValue::Fail);
    }

    // ── TurnCompleted ─────────────────────────────────────────────────────────

    /// A cleanly completed turn passes.
    #[tokio::test]
    async fn turn_completed_passes_on_clean_completion() {
        let case = case_with(Checker::Exact, Some("x"));
        let events = vec![ev(0, SourceKind::Runtime, "o", turn_completed())];
        let score = TurnCompleted.score(&ctx(&events, &case)).await;
        assert_eq!(score.value, ScoreValue::Pass);
    }

    /// A failed turn must fail even if a completed turn also exists — a run that
    /// broke partway is not a clean completion.
    #[tokio::test]
    async fn turn_completed_fails_when_any_turn_failed() {
        let case = case_with(Checker::Exact, Some("x"));
        let events = vec![
            ev(0, SourceKind::Runtime, "o", turn_failed()),
            ev(1, SourceKind::Runtime, "o", turn_completed()),
        ];
        let score = TurnCompleted.score(&ctx(&events, &case)).await;
        assert_eq!(score.value, ScoreValue::Fail);
    }

    /// No turn ending at all skips — there is nothing to judge.
    #[tokio::test]
    async fn turn_completed_skips_when_no_turn_ending() {
        let case = case_with(Checker::Exact, Some("x"));
        let events = vec![ev(0, SourceKind::Model, "m", text_block("hi"))];
        let score = TurnCompleted.score(&ctx(&events, &case)).await;
        assert!(score.value.is_skip());
    }
}
