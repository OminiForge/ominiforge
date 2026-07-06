//! The eval subsystem: case schema, loading, scoring, and running.
//!
//! Design: `doc/eval.md`. Entry point for users is `ominiforge eval` (CLI).
//! This module owns the data model + loader (Step 1), scorers (Step 2), and
//! runner (Step 3).

pub mod analysis;
pub mod case;
pub mod error;
pub mod runner;
pub mod score;
pub mod scorer;

pub use analysis::{
    CaseVerdict, RunDiff, RunReport, ScoreRow, ScorerTally, VerdictChange, case_verdicts,
    fold_case_verdict, load_scores, pass_rate,
};
pub use case::{CaseFile, CaseSource, CaseStatus, Checker, Difficulty, EvalCase, ExpectedFile};
pub use error::{EvalError, Result};
pub use runner::{CaseResult, RunConfig, run_case};
pub use score::{EvalContext, Score, ScoreValue};
pub use scorer::{
    CostUnder, ExactMatch, FuzzyMatch, NoToolError, Scorer, TestsPass, TurnCompleted, WorkspaceDiff,
};
