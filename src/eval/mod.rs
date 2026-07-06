//! The eval subsystem: case schema, loading, scoring, and (later) running.
//!
//! Design: `doc/eval.md`. Entry point for users is `ominiforge eval` (CLI,
//! wired in Step 3). This module owns the data model + loader (Step 1) and the
//! scorers (Step 2); the runner (Step 3) adds a sub-module here once designed.

pub mod case;
pub mod error;
pub mod score;
pub mod scorer;

pub use case::{CaseFile, CaseSource, CaseStatus, Checker, Difficulty, EvalCase, ExpectedFile};
pub use error::{EvalError, Result};
pub use score::{EvalContext, Score, ScoreValue};
pub use scorer::{CostUnder, ExactMatch, FuzzyMatch, NoToolError, Scorer, TurnCompleted};
