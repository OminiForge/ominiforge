//! The score value, verdict types, and eval execution context.
//!
//! `Score` is the per-sample output of one [`super::scorer::Scorer`] pass.
//! `EvalContext` is the read-only view of a completed run passed to every
//! scorer; fields populated by the runner (Step 3) are `Option` and may be
//! `None` in unit tests — event-only scorers must not require them.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::CoreEvent;
use crate::llm::Message;

use super::case::EvalCase;

// ── Score ────────────────────────────────────────────────────────────────────

/// The outcome of one scorer applied to one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Score {
    /// The verdict.
    pub value: ScoreValue,
    /// Human-readable explanation: what was checked and why this verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// Scorer-specific extra data (metrics, diffs, matched patterns, etc.).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, Value>,
}

impl Score {
    /// Convenience constructor for a clean pass.
    pub fn pass(explanation: impl Into<String>) -> Self {
        Self {
            value: ScoreValue::Pass,
            explanation: Some(explanation.into()),
            metadata: HashMap::new(),
        }
    }

    /// Convenience constructor for a fail.
    pub fn fail(explanation: impl Into<String>) -> Self {
        Self {
            value: ScoreValue::Fail,
            explanation: Some(explanation.into()),
            metadata: HashMap::new(),
        }
    }

    /// Convenience constructor for a scorer that could not run.
    pub fn skip(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            value: ScoreValue::Skip {
                reason: reason.clone(),
            },
            explanation: Some(reason),
            metadata: HashMap::new(),
        }
    }
}

/// The verdict a scorer can return.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ScoreValue {
    Pass,
    Fail,
    /// Partial credit in `[0.0, 1.0]`. Used by scorers that measure degrees
    /// of correctness (e.g. fuzzy similarity score).
    Partial {
        score: f64,
    },
    /// The scorer could not run (missing workspace, missing target, etc.).
    /// Does not count as pass or fail; excluded from the pass-rate denominator.
    Skip {
        reason: String,
    },
}

impl ScoreValue {
    /// Returns `true` for `Pass` only.
    #[must_use]
    pub const fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    /// Returns `true` for `Fail` only.
    #[must_use]
    pub const fn is_fail(&self) -> bool {
        matches!(self, Self::Fail)
    }

    /// Returns `true` for `Skip`.
    #[must_use]
    pub const fn is_skip(&self) -> bool {
        matches!(self, Self::Skip { .. })
    }
}

// ── EvalContext ───────────────────────────────────────────────────────────────

/// The read-only view of a completed run, passed to every scorer.
///
/// `messages` and `workspace` are populated by the runner (Step 3) and are
/// `None` here until then. Scorers that require them return
/// [`ScoreValue::Skip`] when `None`; event-only scorers ignore them.
pub struct EvalContext<'a> {
    /// The full event stream from the run's session.
    pub events: &'a [CoreEvent],
    /// The conversation as provider-neutral messages, rebuilt from events.
    /// `None` until the runner wires it up (Step 3).
    pub messages: Option<&'a [Message]>,
    /// The scratch workspace directory after the run completed.
    /// `None` until the runner wires it up (Step 3).
    pub workspace: Option<&'a Path>,
    /// The case that was evaluated.
    pub case: &'a EvalCase,
}
