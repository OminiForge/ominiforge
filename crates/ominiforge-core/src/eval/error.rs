//! Errors for the eval layer.

use std::path::PathBuf;

/// Result alias for eval operations.
pub type Result<T> = std::result::Result<T, EvalError>;

/// Something went wrong loading, validating, or running an eval case/suite.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// A suite path (case file or directory) does not exist.
    #[error("eval path not found: {0}")]
    NotFound(PathBuf),

    /// A case file or suite directory could not be read.
    #[error("eval io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A case file could not be parsed as TOML.
    #[error("failed to parse case {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// A run's `scores.jsonl` or `manifest.json` could not be parsed as JSON.
    /// Guards the cross-run analysis layer (`doc/eval.md` §6) against silent
    /// history loss when the on-disk score format drifts.
    #[error("failed to parse eval json at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// A case is structurally valid TOML but violates a schema invariant
    /// (empty id/input, missing target for a match checker, etc.).
    #[error("invalid case in {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },

    /// Two cases in the same suite share an `id` (ids must be unique so scores
    /// join unambiguously across runs).
    #[error("duplicate case id `{id}`: defined in both {first} and {second}")]
    DuplicateId {
        id: String,
        first: PathBuf,
        second: PathBuf,
    },
}
