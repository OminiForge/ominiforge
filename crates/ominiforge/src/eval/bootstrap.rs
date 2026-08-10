//! Bootstrap loader: turn a public Q&A dataset (JSONL) into [`EvalCase`]s (`doc/eval.md` §8.2).
//!
//! Near-zero-cost coverage of general ability to lower the cold-start cost of an
//! eval suite — it does **not** replace cases targeting Ominiforge's own
//! behavior (§8.2).
//!
//! The on-disk form is one JSON object per line with an `input` and a `target`
//! (a question + its ground-truth answer). This is the lowest common shape of
//! GAIA, the OpenAI Evals basics, and most Q&A sets; a specific dataset (e.g.
//! GAIA's `Question`/`Final answer` keys) maps onto it by field rename, so this
//! loader stays dataset-agnostic and each dataset is just a key mapping.
//!
//! Every case loaded here is `source = bootstrap` and judged by a match checker
//! (`exact`/`fuzzy`), which needs no scratch workspace. Dataset entries that
//! reference attached files (GAIA level 2/3) are **skipped**: running them needs
//! the file-serving capability that `doc/eval.md` §10 leaves for later. Skipping
//! is loud (returned in [`BootstrapLoad::skipped`]), never silent.

use std::path::Path;

use serde::Deserialize;

use super::case::{Checker, EvalCase};
use super::error::{EvalError, Result};

/// One line of a bootstrap JSONL dataset: a question and its ground truth.
///
/// Extra keys are ignored (forward-compat, like [`EvalCase`]). `file_name` is
/// read only to detect entries that need an attachment we cannot yet serve.
#[derive(Debug, Clone, Deserialize)]
struct BootstrapEntry {
    /// Stable id for the case. Optional: a positional id is synthesized when
    /// absent so an id-less dataset still loads deterministically.
    #[serde(default)]
    id: Option<String>,
    /// The question / prompt sent to the agent.
    input: String,
    /// The ground-truth answer the match checker compares against.
    target: String,
    /// A referenced attachment, if any. Non-empty means the entry needs the
    /// file-serving capability (`doc/eval.md` §10) and is skipped for now.
    #[serde(default)]
    file_name: Option<String>,
}

/// The result of loading a bootstrap dataset: the usable cases plus the ids of
/// entries deliberately skipped, so the skip count is visible rather than a
/// silently shorter suite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapLoad {
    /// Cases ready to run (no attachment; valid input/target).
    pub cases: Vec<EvalCase>,
    /// Human-readable reasons for each skipped entry (id + why).
    pub skipped: Vec<String>,
}

/// The match checker a bootstrap dataset is judged with. Both need only the
/// case `target` and the model's text, so no workspace is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// Normalized exact string equality ([`Checker::Exact`]). GAIA's grading.
    Exact,
    /// Normalized substring containment ([`Checker::Fuzzy`]).
    Fuzzy,
}

impl MatchKind {
    /// Parse from a CLI flag value (`exact` / `fuzzy`).
    ///
    /// # Errors
    /// Returns [`EvalError::Invalid`] on any other value.
    pub fn parse(s: &str, path: &Path) -> Result<Self> {
        match s {
            "exact" => Ok(Self::Exact),
            "fuzzy" => Ok(Self::Fuzzy),
            other => Err(EvalError::Invalid {
                path: path.to_path_buf(),
                reason: format!("unknown match kind `{other}` (expected `exact` or `fuzzy`)"),
            }),
        }
    }

    /// The [`Checker`] this kind maps to.
    const fn checker(self) -> Checker {
        match self {
            Self::Exact => Checker::Exact,
            Self::Fuzzy => Checker::Fuzzy,
        }
    }
}

/// Load a bootstrap dataset from a JSONL file into bootstrap [`EvalCase`]s.
///
/// `checker` selects the match kind (`exact`/`fuzzy`) applied to every case.
/// `tag` is added to every case's `tags` (e.g. `"gaia"`) so the analysis layer
/// can slice by dataset later (`doc/eval.md` §6 A4). Entries with an attachment
/// are skipped (see module docs); a blank line is skipped silently.
///
/// # Errors
/// [`EvalError::NotFound`] if the file is absent, [`EvalError::Io`] on read
/// failure, [`EvalError::Json`] on a malformed line.
pub fn load_bootstrap(path: &Path, checker: MatchKind, tag: &str) -> Result<BootstrapLoad> {
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

    let mut cases = Vec::new();
    let mut skipped = Vec::new();

    for (line_no, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: BootstrapEntry =
            serde_json::from_str(line).map_err(|source| EvalError::Json {
                path: path.to_path_buf(),
                source,
            })?;

        // Positional id keeps an id-less dataset deterministic and unique.
        let id = entry
            .id
            .clone()
            .unwrap_or_else(|| format!("{tag}-{:04}", line_no + 1));

        // Attachments need a file-serving capability we do not have yet
        // (`doc/eval.md` §10): skip loudly rather than run an unwinnable case.
        if entry
            .file_name
            .as_deref()
            .is_some_and(|f| !f.trim().is_empty())
        {
            skipped.push(format!(
                "{id}: needs attachment `{}` (file serving not implemented)",
                entry.file_name.as_deref().unwrap_or_default()
            ));
            continue;
        }

        // Guard the match-checker invariant here so a bad line is one skip, not
        // a whole-suite load failure (validate() would reject an empty target).
        if entry.input.trim().is_empty() || entry.target.trim().is_empty() {
            skipped.push(format!("{id}: empty input or target"));
            continue;
        }

        cases.push(EvalCase {
            id,
            source: super::case::CaseSource::Bootstrap,
            status: super::case::CaseStatus::Approved,
            origin_session: None,
            input: entry.input,
            target: Some(entry.target),
            tags: vec![tag.to_owned()],
            difficulty: None,
            files: vec![],
            checker: checker.checker(),
        });
    }

    Ok(BootstrapLoad { cases, skipped })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::eval::case::{CaseSource, Checker};

    /// Write `content` to a unique temp file and return its path. The file
    /// outlives the test via the returned `PathBuf`; the temp dir is the OS one.
    fn tmp_jsonl(content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ominiforge-bootstrap-test-{}.jsonl",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, content).unwrap();
        path
    }

    /// A plain question/answer line must load as one bootstrap case with the
    /// selected checker and dataset tag — the core happy path.
    #[test]
    fn loads_qa_line_as_bootstrap_case() {
        let path = tmp_jsonl(r#"{"id":"q1","input":"What is 2+2?","target":"4"}"#);
        let load = load_bootstrap(&path, MatchKind::Exact, "gaia").unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(load.cases.len(), 1);
        let case = &load.cases[0];
        assert_eq!(case.id, "q1");
        assert_eq!(case.source, CaseSource::Bootstrap);
        assert_eq!(case.target.as_deref(), Some("4"));
        assert_eq!(case.checker, Checker::Exact);
        assert_eq!(case.tags, vec!["gaia".to_owned()]);
        assert!(load.skipped.is_empty());
    }

    /// An entry with an attachment must be skipped (loudly), not loaded — we
    /// cannot serve the file yet, so running it would guarantee a spurious fail.
    #[test]
    fn skips_entry_with_attachment() {
        let path = tmp_jsonl(
            r#"{"id":"q2","input":"Read the chart.","target":"7","file_name":"chart.png"}"#,
        );
        let load = load_bootstrap(&path, MatchKind::Exact, "gaia").unwrap();
        std::fs::remove_file(&path).ok();

        assert!(load.cases.is_empty());
        assert_eq!(load.skipped.len(), 1);
        assert!(load.skipped[0].contains("chart.png"));
    }

    /// A missing id must get a deterministic positional one so an id-less
    /// dataset still loads with unique, stable ids across runs.
    #[test]
    fn synthesizes_positional_id_when_absent() {
        let path = tmp_jsonl(
            "{\"input\":\"Q one\",\"target\":\"a\"}\n{\"input\":\"Q two\",\"target\":\"b\"}\n",
        );
        let load = load_bootstrap(&path, MatchKind::Fuzzy, "evals").unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(load.cases.len(), 2);
        assert_eq!(load.cases[0].id, "evals-0001");
        assert_eq!(load.cases[1].id, "evals-0002");
    }

    /// An empty target must be skipped, not loaded — the match checkers require
    /// a non-empty target, and a whole-file load must not fail on one bad line.
    #[test]
    fn skips_entry_with_empty_target() {
        let path = tmp_jsonl(r#"{"id":"q3","input":"Question","target":""}"#);
        let load = load_bootstrap(&path, MatchKind::Exact, "gaia").unwrap();
        std::fs::remove_file(&path).ok();

        assert!(load.cases.is_empty());
        assert_eq!(load.skipped.len(), 1);
        assert!(load.skipped[0].contains("q3"));
    }

    /// A malformed JSON line must be a hard error, not a silent skip — a
    /// corrupt dataset is a real problem the operator must see (fail loud).
    #[test]
    fn malformed_line_is_hard_error() {
        let path = tmp_jsonl("{not json}");
        let err = load_bootstrap(&path, MatchKind::Exact, "gaia").unwrap_err();
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, EvalError::Json { .. }), "got {err:?}");
    }

    /// Every loaded bootstrap case must pass `validate` — the loader must not
    /// emit a case the rest of the pipeline would reject.
    #[test]
    fn loaded_cases_pass_validation() {
        let path = tmp_jsonl(r#"{"id":"q1","input":"What is 2+2?","target":"4"}"#);
        let load = load_bootstrap(&path, MatchKind::Exact, "gaia").unwrap();
        std::fs::remove_file(&path).ok();
        load.cases[0].validate(Path::new("bootstrap")).unwrap();
    }
}
