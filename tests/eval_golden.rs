//! Golden event-log regression tests (eval Step 0).
//!
//! Each test loads a frozen `events.jsonl` fixture through the real
//! `SessionStore::read_events` path (parse + session_id restore) and asserts
//! structural invariants that would break if the agent loop, event schema, or
//! Monitor fold were changed incorrectly.
//!
//! Fixtures live in `tests/fixtures/eval_golden/<session_id>/events.jsonl`.
//! They are **committed unchanged**; do not edit them to make tests green —
//! update the assertions to match intentional schema changes instead.
//!
//! Committed fixtures must be **synthetic**: no real user data, no real paths,
//! no real prompts. This one is hand-written to the real on-disk log format but
//! contains only invented content, so it is safe to publish and its numbers are
//! chosen for clean assertions. Fixtures derived from real runs are
//! user-private and belong under `.omini/` (with de-identification), never git.
//!
//! Synthetic session: one turn, two parallel shell tool calls, clean
//! Completed — a representative single-turn coding/query run.

use std::path::Path;

use ominiforge::core::SessionId;
use ominiforge::core::payload::{EventPayload, SessionEvent, ToolEvent, TurnEvent};
use ominiforge::monitor::{PricingTable, summarize};
use ominiforge::session::SessionStore;

const SESSION_ID: &str = "01SYNTHETICGOLDEN0000000TST";

fn fixture_store() -> (SessionStore, SessionId) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eval_golden");
    let store = SessionStore::new(root);
    let id = SessionId(SESSION_ID.to_owned());
    (store, id)
}

/// The first event of every well-formed session is `Session::Created`.
/// If this breaks, the loop is no longer writing the opening event first —
/// replay and resume would silently lose the initial config snapshot.
#[test]
fn golden_starts_with_session_created() {
    let (store, id) = fixture_store();
    let events = store.read_events(&id).expect("fixture readable");
    assert!(
        matches!(
            events.first().map(|e| &e.payload),
            Some(EventPayload::Session(SessionEvent::Created { .. }))
        ),
        "first event must be Session::Created"
    );
}

/// Sequence numbers must be contiguous 0..N.
/// Gaps mean the writer skipped a seq or the reader dropped a line — either
/// breaks resume (which relies on seq for replay ordering) and CI diff.
#[test]
fn golden_seqs_are_contiguous() {
    let (store, id) = fixture_store();
    let events = store.read_events(&id).expect("fixture readable");
    for (i, e) in events.iter().enumerate() {
        assert_eq!(
            e.seq, i as u64,
            "seq mismatch at index {i}: expected {i}, got {}",
            e.seq
        );
    }
}

/// Exactly one turn was started and one completed — no stale Started without
/// Completed.  Breaking this means the loop is leaking open turns.
#[test]
fn golden_turn_started_and_completed_once() {
    let (store, id) = fixture_store();
    let events = store.read_events(&id).expect("fixture readable");

    let started = events
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::Turn(TurnEvent::Started { .. })))
        .count();
    let completed = events
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::Turn(TurnEvent::Completed { .. })))
        .count();

    assert_eq!(started, 1, "expected 1 TurnEvent::Started");
    assert_eq!(completed, 1, "expected 1 TurnEvent::Completed");
}

/// Two tool calls were started and both completed with no failures.
/// This pins the collector's consolidation: two parallel shell calls must each
/// produce a Started + Completed pair (not one merged block, not a Failed).
#[test]
fn golden_two_tool_calls_both_completed_no_failures() {
    let (store, id) = fixture_store();
    let events = store.read_events(&id).expect("fixture readable");

    let started = events
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::Tool(ToolEvent::Started { .. })))
        .count();
    let completed = events
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::Tool(ToolEvent::Completed { .. })))
        .count();
    let failed = events
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::Tool(ToolEvent::Failed { .. })))
        .count();

    assert_eq!(started, 2, "expected 2 ToolEvent::Started");
    assert_eq!(completed, 2, "expected 2 ToolEvent::Completed");
    assert_eq!(failed, 0, "expected 0 ToolEvent::Failed");
}

/// Monitor::summarize folds the fixture into expected aggregates.
/// This pins the Monitor fold logic against the real log format — if
/// RequestCompleted parsing or token accounting changes, this fails.
///
/// Token sources (from fixture seq 6 + seq 13):
///   r1: input=100, output=20, cache_read=64
///   r2: input=200, output=40, cache_read=64
///   totals: input=300, output=60, cache_read=128
#[test]
fn golden_monitor_summary_aggregates_correctly() {
    let (store, id) = fixture_store();
    let events = store.read_events(&id).expect("fixture readable");
    let summary = summarize(&events, PricingTable::new());

    assert_eq!(summary.total_turns, 1);
    assert_eq!(summary.total_model_requests, 2);
    assert_eq!(summary.total_tool_calls, 2);
    assert_eq!(summary.total_tool_failures, 0);
    assert_eq!(summary.total_input_tokens, 300);
    assert_eq!(summary.total_output_tokens, 60);
    assert_eq!(summary.total_cache_read_tokens, 128);
    // synthetic/test-model is not in the empty pricing table → no cost
    assert_eq!(summary.cost_usd, None);
}

/// The opening turn's user input is captured as the session title.
/// Breaking this means the session list and TUI picker would show blank titles.
#[test]
fn golden_first_user_input_captured() {
    let (store, id) = fixture_store();
    let events = store.read_events(&id).expect("fixture readable");
    let summary = summarize(&events, PricingTable::new());

    assert_eq!(
        summary.first_user_input.as_deref(),
        Some("list the project files"),
        "session title should be the opening turn input"
    );
}
