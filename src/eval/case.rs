//! The `EvalCase` data model and TOML loading.
//!
//! Mirrors `doc/eval.md` §3.1. A case is a four-tuple —
//! **input + target + checker + metadata** — that pins one unit of expected
//! agent behavior. Cases are authored as TOML under `.omini/eval/suites/` and
//! loaded here into typed structs; running them and scoring the result live in
//! later steps (`doc/eval.md` §3.2, §4).
//!
//! Design choices worth flagging against the doc's illustrative TOML:
//! - `origin_session` is `Option<SessionId>` (omitted when absent), not the
//!   `""` sentinel the doc example shows — it matches the codebase's
//!   `skip_serializing_if` convention (see `session::meta::SessionMeta`).
//! - `target` is `Option<String>` for the same reason; the match checkers
//!   (`exact`/`fuzzy`) require it, enforced in [`EvalCase::validate`].
//! - Unknown keys are ignored for forward compatibility, like `Profile`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::SessionId;

use super::error::{EvalError, Result};

/// One eval case: an input to send an agent plus how to judge the result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct EvalCase {
    /// Stable unique id. Also the join key for scores across runs, so it must
    /// be unique within a suite and stable across case edits.
    pub id: String,

    /// How this case came to exist. Defaults to [`CaseSource::Manual`].
    #[serde(default)]
    pub source: CaseSource,

    /// Whether the case is part of the regression gate. Defaults to
    /// [`CaseStatus::Approved`]; auto-ingested cases start `Proposed` and are
    /// promoted by human review (`doc/eval.md` §4.4, §8.4).
    #[serde(default)]
    pub status: CaseStatus,

    /// For `ingested` cases, the session the case was extracted from — kept for
    /// traceability back to the original run. Absent for hand-authored cases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session: Option<SessionId>,

    /// The prompt sent to the agent.
    pub input: String,

    /// Optional string ground truth. Required by the `exact`/`fuzzy` checkers,
    /// unused by `tests`/`state`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,

    /// Free-form labels for dimension slicing in the analysis layer
    /// (`doc/eval.md` §6 A4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// Optional difficulty hint, also for slicing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<Difficulty>,

    /// Files seeded into the scratch workspace before the agent runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<CaseFile>,

    /// How to judge the run.
    pub checker: Checker,
}

/// How a case came to exist (`doc/eval.md` §3.1, §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum CaseSource {
    /// Hand-authored from a real failure (the main line).
    #[default]
    Manual,
    /// Auto-extracted from run history (`doc/eval.md` §8.4).
    Ingested,
    /// Loaded from a public dataset (GAIA / SWE-bench style, §8.2).
    Bootstrap,
}

/// Whether a case participates in the regression gate (`doc/eval.md` §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    /// Reviewed and part of the gate; run by default.
    #[default]
    Approved,
    /// Auto-extracted, not yet human-reviewed; excluded from the gate unless
    /// `--include-proposed` is set.
    Proposed,
}

/// A coarse difficulty hint for slicing pass rate by hardness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum Difficulty {
    Easy,
    Medium,
    Hard,
}

/// A file dropped into the scratch workspace before the agent runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct CaseFile {
    /// Path relative to the scratch workspace root.
    pub path: PathBuf,
    /// The file's contents.
    pub content: String,
}

/// How to judge a run — the four buckets of `doc/eval.md` §3.1.
///
/// `kind` (`snake_case`) selects the variant; each carries only the data its
/// scorer needs. Scorers that consume these are implemented in Step 2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Checker {
    /// Normalized exact string match against `target`.
    Exact,
    /// Fuzzy match against `target` (substring / similarity; policy TBD in
    /// Step 2). Carries no extra config yet.
    Fuzzy,
    /// Run a shell command in the scratch workspace; pass on exit code 0 and,
    /// if given, all `pass_patterns` present in the output (SWE-bench style).
    Tests {
        /// The command to run (e.g. `cargo test`).
        command: String,
        /// Substrings that must appear in stdout/stderr for a pass. Empty =
        /// exit code alone decides.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pass_patterns: Vec<String>,
    },
    /// Diff the scratch workspace against expectations after the run.
    State {
        /// Files whose presence/content is asserted.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        expect: Vec<ExpectedFile>,
    },
    /// LLM-as-judge (later; `doc/eval.md` §3.4). The rubric prompt lives here;
    /// calibration is a precondition to gating on it.
    Judge {
        /// Few-shot critique prompt describing the pass criteria.
        rubric: String,
    },
}

impl Checker {
    /// The `kind` discriminant as it appears in TOML — handy for reporting.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Fuzzy => "fuzzy",
            Self::Tests { .. } => "tests",
            Self::State { .. } => "state",
            Self::Judge { .. } => "judge",
        }
    }
}

/// An expectation about one workspace file after the run (`state` checker).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct ExpectedFile {
    /// Path relative to the scratch workspace root.
    pub path: PathBuf,
    /// Exact contents to assert. `None` asserts existence only (or, with
    /// `absent = true`, is ignored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Assert the file does *not* exist. Mutually exclusive with `content`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub absent: bool,
}

impl EvalCase {
    /// Parse one case from a TOML string. Does not validate invariants; call
    /// [`EvalCase::validate`] for that (or use [`load_case`], which does both).
    ///
    /// # Errors
    ///
    /// Returns `toml::de::Error` if `text` is not valid TOML or does not match
    /// the `EvalCase` schema.
    pub fn from_toml(text: &str) -> std::result::Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Check schema invariants that the type system can't. `path` is only used
    /// to build error messages.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError::Invalid`] if a schema invariant is violated (empty
    /// `id`/`input`, missing `target` for match checkers, etc.).
    pub fn validate(&self, path: &Path) -> Result<()> {
        let invalid = |reason: String| {
            Err(EvalError::Invalid {
                path: path.to_path_buf(),
                reason,
            })
        };

        if self.id.trim().is_empty() {
            return invalid("`id` must not be empty".to_owned());
        }
        if self.input.trim().is_empty() {
            return invalid("`input` must not be empty".to_owned());
        }

        match &self.checker {
            Checker::Exact | Checker::Fuzzy => {
                let has_target = self.target.as_ref().is_some_and(|t| !t.is_empty());
                if !has_target {
                    return invalid(format!(
                        "checker `{}` requires a non-empty `target`",
                        self.checker.kind()
                    ));
                }
            }
            Checker::Tests { command, .. } => {
                if command.trim().is_empty() {
                    return invalid("checker `tests` requires a non-empty `command`".to_owned());
                }
            }
            Checker::State { expect } => {
                if expect.is_empty() {
                    return invalid(
                        "checker `state` requires at least one `expect` entry".to_owned(),
                    );
                }
                for e in expect {
                    if e.absent && e.content.is_some() {
                        return invalid(format!(
                            "expected file `{}`: `absent` and `content` are mutually exclusive",
                            e.path.display()
                        ));
                    }
                }
            }
            Checker::Judge { rubric } => {
                if rubric.trim().is_empty() {
                    return invalid("checker `judge` requires a non-empty `rubric`".to_owned());
                }
            }
        }
        Ok(())
    }
}

/// Read and validate a single case TOML file.
///
/// # Errors
///
/// Returns [`EvalError::NotFound`] if the file is absent, [`EvalError::Io`]
/// on read failure, [`EvalError::Parse`] on invalid TOML, or
/// [`EvalError::Invalid`] if the case violates a schema invariant.
pub fn load_case(path: &Path) -> Result<EvalCase> {
    let text = std::fs::read_to_string(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            EvalError::NotFound(path.to_path_buf())
        } else {
            EvalError::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    let case = EvalCase::from_toml(&text).map_err(|source| EvalError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    case.validate(path)?;
    Ok(case)
}

/// Load every `*.toml` case under `dir` (recursively), validating each and
/// rejecting duplicate ids. Returns cases sorted by id for deterministic
/// ordering across runs.
///
/// # Errors
///
/// Returns [`EvalError::NotFound`] if `dir` does not exist,
/// [`EvalError::Io`] on filesystem read failures,
/// [`EvalError::Parse`]/[`EvalError::Invalid`] on invalid case files, or
/// [`EvalError::DuplicateId`] if two cases share an `id`.
pub fn load_suite(dir: &Path) -> Result<Vec<EvalCase>> {
    if !dir.exists() {
        return Err(EvalError::NotFound(dir.to_path_buf()));
    }

    let mut files = Vec::new();
    collect_toml_files(dir, &mut files)?;
    files.sort();

    // Track where each id was first seen, to point at both files on collision.
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut cases = Vec::with_capacity(files.len());
    for path in files {
        let case = load_case(&path)?;
        if let Some(first) = seen.get(&case.id) {
            return Err(EvalError::DuplicateId {
                id: case.id,
                first: first.clone(),
                second: path,
            });
        }
        seen.insert(case.id.clone(), path);
        cases.push(case);
    }

    cases.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(cases)
}

/// Recursively collect `*.toml` paths under `dir` into `out`.
fn collect_toml_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|source| EvalError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| EvalError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_toml_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "toml") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    // ── helpers ─────────────────────────────────────────────────────────────

    fn tests_checker() -> Checker {
        Checker::Tests {
            command: "cargo test".to_owned(),
            pass_patterns: vec![],
        }
    }

    fn minimal_case(id: &str) -> EvalCase {
        EvalCase {
            id: id.to_owned(),
            source: CaseSource::Manual,
            status: CaseStatus::Approved,
            origin_session: None,
            input: "do something".to_owned(),
            target: None,
            tags: vec![],
            difficulty: None,
            files: vec![],
            checker: tests_checker(),
        }
    }

    // ── round-trip: TOML ↔ struct ────────────────────────────────────────────

    #[test]
    fn tests_checker_round_trips_toml() {
        // Mirrors the doc/eval.md §3.1 example shape.
        let src = r#"
id = "fix-off-by-one-001"
source = "manual"
status = "approved"
input = "The function returns n+1 instead of n. Fix it."
tags = ["regression", "coding"]
difficulty = "easy"

[[files]]
path = "src/counter.rs"
content = "pub fn count(n: u32) -> u32 { n + 1 }\n"

[checker]
kind = "tests"
command = "cargo test"
pass_patterns = ["test_count_returns_n"]
"#;
        let case: EvalCase = EvalCase::from_toml(src).unwrap();
        assert_eq!(case.id, "fix-off-by-one-001");
        assert_eq!(case.source, CaseSource::Manual);
        assert_eq!(case.status, CaseStatus::Approved);
        assert_eq!(case.input, "The function returns n+1 instead of n. Fix it.");
        assert_eq!(case.tags, ["regression", "coding"]);
        assert_eq!(case.difficulty, Some(Difficulty::Easy));
        assert_eq!(case.files.len(), 1);
        assert_eq!(case.files[0].path, PathBuf::from("src/counter.rs"));
        match &case.checker {
            Checker::Tests {
                command,
                pass_patterns,
            } => {
                assert_eq!(command, "cargo test");
                assert_eq!(pass_patterns, &["test_count_returns_n"]);
            }
            other => panic!("expected Tests checker, got {other:?}"),
        }

        // Round-trip: serialize back and re-parse.
        let reserialized = toml::to_string(&case).unwrap();
        let case2: EvalCase = EvalCase::from_toml(&reserialized).unwrap();
        assert_eq!(case, case2);
    }

    #[test]
    fn exact_checker_round_trips_toml() {
        let src = r#"
id = "gaia-001"
input = "What is 2+2?"
target = "4"

[checker]
kind = "exact"
"#;
        let case: EvalCase = EvalCase::from_toml(src).unwrap();
        assert_eq!(case.checker, Checker::Exact);
        assert_eq!(case.target.as_deref(), Some("4"));
        // Defaults.
        assert_eq!(case.source, CaseSource::Manual);
        assert_eq!(case.status, CaseStatus::Approved);
        assert!(case.origin_session.is_none());

        let reserialized = toml::to_string(&case).unwrap();
        let case2: EvalCase = EvalCase::from_toml(&reserialized).unwrap();
        assert_eq!(case, case2);
    }

    #[test]
    fn state_checker_round_trips_toml() {
        let src = r#"
id = "state-001"
input = "Create a README."

[checker]
kind = "state"

[[checker.expect]]
path = "README.md"

[[checker.expect]]
path = "tmp/scratch.txt"
absent = true
"#;
        let case: EvalCase = EvalCase::from_toml(src).unwrap();
        match &case.checker {
            Checker::State { expect } => {
                assert_eq!(expect.len(), 2);
                assert_eq!(expect[0].path, PathBuf::from("README.md"));
                assert!(!expect[0].absent);
                assert!(expect[1].absent);
            }
            other => panic!("expected State checker, got {other:?}"),
        }
    }

    #[test]
    fn ingested_case_carries_origin_session() {
        let src = r#"
id = "ingested-001"
source = "ingested"
status = "proposed"
origin_session = "01SYNTHETICGOLDEN0000000TST"
input = "some task"

[checker]
kind = "tests"
command = "cargo test"
"#;
        let case: EvalCase = EvalCase::from_toml(src).unwrap();
        assert_eq!(case.source, CaseSource::Ingested);
        assert_eq!(case.status, CaseStatus::Proposed);
        assert_eq!(
            case.origin_session,
            Some(SessionId("01SYNTHETICGOLDEN0000000TST".to_owned()))
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        // Forward-compat: a field from a future schema version must not break loading.
        let src = r#"
id = "compat-001"
input = "x"
future_field = "ignored"

[checker]
kind = "tests"
command = "echo ok"
"#;
        EvalCase::from_toml(src).unwrap();
    }

    // ── validation ───────────────────────────────────────────────────────────

    #[test]
    fn validate_rejects_empty_id() {
        let mut case = minimal_case("ok");
        case.id = String::new();
        let err = case.validate(Path::new("fake.toml")).unwrap_err();
        assert!(err.to_string().contains("id"));
    }

    #[test]
    fn validate_rejects_empty_input() {
        let mut case = minimal_case("ok");
        case.input = "   ".to_owned();
        let err = case.validate(Path::new("fake.toml")).unwrap_err();
        assert!(err.to_string().contains("input"));
    }

    #[test]
    fn validate_exact_requires_nonempty_target() {
        let mut case = minimal_case("ok");
        case.checker = Checker::Exact;
        case.target = None;
        assert!(case.validate(Path::new("f.toml")).is_err());

        case.target = Some(String::new());
        assert!(case.validate(Path::new("f.toml")).is_err());

        case.target = Some("4".to_owned());
        assert!(case.validate(Path::new("f.toml")).is_ok());
    }

    #[test]
    fn validate_tests_requires_nonempty_command() {
        let mut case = minimal_case("ok");
        case.checker = Checker::Tests {
            command: "  ".to_owned(),
            pass_patterns: vec![],
        };
        let err = case.validate(Path::new("f.toml")).unwrap_err();
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn validate_state_requires_at_least_one_expect() {
        let mut case = minimal_case("ok");
        case.checker = Checker::State { expect: vec![] };
        let err = case.validate(Path::new("f.toml")).unwrap_err();
        assert!(err.to_string().contains("expect"));
    }

    #[test]
    fn validate_state_rejects_absent_with_content() {
        let mut case = minimal_case("ok");
        case.checker = Checker::State {
            expect: vec![ExpectedFile {
                path: PathBuf::from("foo.txt"),
                content: Some("data".to_owned()),
                absent: true,
            }],
        };
        let err = case.validate(Path::new("f.toml")).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn validate_judge_requires_nonempty_rubric() {
        let mut case = minimal_case("ok");
        case.checker = Checker::Judge {
            rubric: String::new(),
        };
        let err = case.validate(Path::new("f.toml")).unwrap_err();
        assert!(err.to_string().contains("rubric"));
    }

    // ── load_suite ───────────────────────────────────────────────────────────

    #[test]
    fn load_suite_collects_all_toml_files() {
        let dir = tempdir();
        let subdir = dir.path().join("coding");
        std::fs::create_dir(&subdir).unwrap();

        write_case_file(dir.path().join("a.toml"), "a-001", "cargo test");
        write_case_file(subdir.join("b.toml"), "b-001", "cargo test");

        let cases = load_suite(dir.path()).unwrap();
        assert_eq!(cases.len(), 2);
        // Sorted by id.
        assert_eq!(cases[0].id, "a-001");
        assert_eq!(cases[1].id, "b-001");
    }

    #[test]
    fn load_suite_rejects_duplicate_ids() {
        let dir = tempdir();
        write_case_file(dir.path().join("x.toml"), "dup-001", "cargo test");
        write_case_file(dir.path().join("y.toml"), "dup-001", "cargo test");

        let err = load_suite(dir.path()).unwrap_err();
        assert!(err.to_string().contains("dup-001"));
    }

    #[test]
    fn load_suite_returns_not_found_for_missing_dir() {
        let err = load_suite(Path::new("/nonexistent/suite/dir")).unwrap_err();
        assert!(
            matches!(err, EvalError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }

    // ── test helpers ─────────────────────────────────────────────────────────

    /// Minimal in-memory temp dir backed by a real filesystem path.
    fn tempdir() -> TempDir {
        TempDir::new()
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ominiforge-eval-test-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn write_case_file(path: PathBuf, id: &str, command: &str) {
        let content = format!(
            r#"
id = "{id}"
input = "do something"

[checker]
kind = "tests"
command = "{command}"
"#,
        );
        std::fs::write(path, content).unwrap();
    }
}
