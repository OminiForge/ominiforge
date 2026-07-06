//! The eval subsystem: case schema, loading, and (later) scoring and running.
//!
//! Design: `doc/eval.md`. Entry point for users is `ominiforge eval` (CLI,
//! wired in Step 3). This module only owns the data model and loader; scorers
//! (Step 2) and the runner (Step 3) add sub-modules here once designed.

pub mod case;
pub mod error;

pub use case::{CaseFile, CaseSource, CaseStatus, Checker, Difficulty, EvalCase, ExpectedFile};
pub use error::{EvalError, Result};
