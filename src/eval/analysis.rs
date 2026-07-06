//! Cross-run analysis: the relational layer over `scores.jsonl` (`doc/eval.md` §6).
//!
//! Everything here is a pure function of persisted score rows — no stdout,
//! no `process::exit`, no CLI assumptions — so the same API serves the developer
//! CLI today and the Evolution / gateway workers later (`doc/eval.md` §9).
//!
//! Scope is the CI-gating minimum of §6:
//! - **A1** [`RunReport`] — single-run aggregate: pass rate + per-scorer counts.
//! - **A2** [`RunDiff`] — run-to-run diff: which cases flipped pass↔fail.
//! - **A3** [`RunDiff::has_regression`] — the boolean CI gate decides on.
//!
//! `ScoreRow` is the on-disk shape of one line in `scores.jsonl`; the runner's
//! persistence path serializes it and this layer reads it back, so writer and
//! reader share one definition rather than an inline `json!` on each side.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::SessionId;

use super::error::{EvalError, Result};
use super::score::{Score, ScoreValue};

/// One row of `scores.jsonl`: a single scorer's verdict on a single case.
///
/// This is the persisted form (`doc/eval.md` §5.3). It is deliberately flat —
/// one row per (case, scorer) — so cross-run joins key on `case_id` + `scorer`
/// without unpacking nested structures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreRow {
    /// The case this row scores; the join key across runs.
    pub case_id: String,
    /// Session the case ran in, for traceability back to `events.jsonl`.
    pub session_id: SessionId,
    /// The scorer that produced this verdict (`Scorer::name()`).
    pub scorer: String,
    /// The verdict.
    pub value: ScoreValue,
    /// Why this verdict, for post-hoc review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// Wall-clock duration of the case run in milliseconds.
    pub duration_ms: u64,
}

impl ScoreRow {
    /// Build a row from a scored case. `duration_ms` is the case's wall-clock;
    /// every scorer row for one case shares it (the case ran once).
    #[must_use]
    pub fn new(
        case_id: &str,
        session_id: &SessionId,
        scorer: &str,
        score: &Score,
        duration_ms: u64,
    ) -> Self {
        Self {
            case_id: case_id.to_owned(),
            session_id: session_id.clone(),
            scorer: scorer.to_owned(),
            value: score.value.clone(),
            explanation: score.explanation.clone(),
            duration_ms,
        }
    }
}

/// A case-level verdict, folded from all of a case's scorer rows.
///
/// Distinct from [`ScoreValue`]: that is per-scorer, this is the whole case.
/// `Partial` is intentionally absent — a case either regressed or it did not,
/// and CI gates on a binary. A `Partial` scorer folds into `Pass` (see
/// [`fold_case_verdict`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseVerdict {
    /// At least one scorer passed (or gave partial credit) and none failed.
    Pass,
    /// At least one scorer failed.
    Fail,
    /// Every scorer skipped — nothing was actually judged. Excluded from the
    /// pass-rate denominator (`doc/eval.md` §5.3).
    Skip,
}

impl CaseVerdict {
    /// Stable lowercase label for display and logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

/// Fold every scorer verdict for one case into a single [`CaseVerdict`].
///
/// Rules, in priority order:
/// 1. any `Fail` → `Fail` (one failed dimension fails the case),
/// 2. else any `Pass`/`Partial` → `Pass`,
/// 3. else (all `Skip`, or empty) → `Skip`.
///
/// Rule 1 dominating is the whole point of a regression gate: a case that
/// writes the right file but leaves a tool erroring has not passed.
#[must_use]
pub fn fold_case_verdict<'a>(scorers: impl IntoIterator<Item = &'a ScoreValue>) -> CaseVerdict {
    let mut saw_pass = false;
    for v in scorers {
        match v {
            ScoreValue::Fail => return CaseVerdict::Fail,
            ScoreValue::Pass | ScoreValue::Partial { .. } => saw_pass = true,
            ScoreValue::Skip { .. } => {}
        }
    }
    if saw_pass {
        CaseVerdict::Pass
    } else {
        CaseVerdict::Skip
    }
}

/// Collapse a run's flat score rows into per-case verdicts, keyed by `case_id`.
///
/// Groups rows by case, then folds each group with [`fold_case_verdict`].
/// `BTreeMap` gives a stable id-sorted order for deterministic reports/diffs.
#[must_use]
pub fn case_verdicts(rows: &[ScoreRow]) -> BTreeMap<String, CaseVerdict> {
    let mut by_case: BTreeMap<String, Vec<&ScoreValue>> = BTreeMap::new();
    for row in rows {
        by_case
            .entry(row.case_id.clone())
            .or_default()
            .push(&row.value);
    }
    by_case
        .into_iter()
        .map(|(id, values)| (id, fold_case_verdict(values)))
        .collect()
}

// ── A1: single-run aggregate ─────────────────────────────────────────────────

/// Pass/fail/skip counts for one scorer across all cases in a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScorerTally {
    /// Rows this scorer marked `Pass`.
    pub pass: usize,
    /// Rows this scorer marked `Fail`.
    pub fail: usize,
    /// Rows this scorer marked `Partial`.
    pub partial: usize,
    /// Rows this scorer marked `Skip`.
    pub skip: usize,
}

/// A1 single-run aggregate: pass rate plus per-scorer tallies (`doc/eval.md`
/// §6). Pure function of one run's score rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunReport {
    /// Cases with a `Pass` verdict.
    pub passed: usize,
    /// Cases with a `Fail` verdict.
    pub failed: usize,
    /// Cases where every scorer skipped; excluded from `pass_rate`.
    pub skipped: usize,
    /// `passed / (passed + failed)`. Skipped cases are not in the denominator
    /// (`doc/eval.md` §5.3). `0.0` when nothing was judged.
    pub pass_rate: f64,
    /// Per-scorer pass/fail/skip counts, keyed by scorer name.
    pub scorers: BTreeMap<String, ScorerTally>,
}

impl RunReport {
    /// Aggregate one run's score rows (A1).
    #[must_use]
    pub fn from_rows(rows: &[ScoreRow]) -> Self {
        let verdicts = case_verdicts(rows);
        let mut passed = 0;
        let mut failed = 0;
        let mut skipped = 0;
        for v in verdicts.values() {
            match v {
                CaseVerdict::Pass => passed += 1,
                CaseVerdict::Fail => failed += 1,
                CaseVerdict::Skip => skipped += 1,
            }
        }

        let mut scorers: BTreeMap<String, ScorerTally> = BTreeMap::new();
        for row in rows {
            let tally = scorers.entry(row.scorer.clone()).or_default();
            match row.value {
                ScoreValue::Pass => tally.pass += 1,
                ScoreValue::Fail => tally.fail += 1,
                ScoreValue::Partial { .. } => tally.partial += 1,
                ScoreValue::Skip { .. } => tally.skip += 1,
            }
        }

        Self {
            passed,
            failed,
            skipped,
            pass_rate: pass_rate(passed, failed),
            scorers,
        }
    }

    /// `true` when no case failed. The single-run half of the CI gate; the
    /// run-to-run half is [`RunDiff::has_regression`].
    #[must_use]
    pub const fn all_passed(&self) -> bool {
        self.failed == 0
    }
}

/// `passed / (passed + failed)`, ignoring skipped cases. `0.0` when the
/// denominator is zero (nothing judged).
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn pass_rate(passed: usize, failed: usize) -> f64 {
    let judged = passed + failed;
    if judged == 0 {
        0.0
    } else {
        passed as f64 / judged as f64
    }
}

// ── A2/A3: run-to-run diff + regression detection ────────────────────────────

/// One case whose verdict changed between two runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictChange {
    /// The case that changed.
    pub case_id: String,
    /// Verdict in the baseline (older) run.
    pub from: CaseVerdict,
    /// Verdict in the candidate (newer) run.
    pub to: CaseVerdict,
}

/// A2 run-to-run diff (`doc/eval.md` §6): which cases flipped between a baseline
/// run and a candidate run. Pure function of two runs' verdicts.
///
/// "Baseline → candidate" is directional: `regressions` are pass→fail (the gate
/// signal), `fixes` are fail→pass. Cases present in only one run are listed
/// separately so a shrinking/growing suite is visible rather than silently
/// changing the pass rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDiff {
    /// Cases that passed in baseline but fail in candidate (the CI signal).
    pub regressions: Vec<String>,
    /// Cases that failed in baseline but pass in candidate.
    pub fixes: Vec<String>,
    /// Other verdict changes (involving `Skip`) that are neither a clean
    /// regression nor a clean fix, kept for completeness.
    pub other_changes: Vec<VerdictChange>,
    /// Cases present in candidate but not baseline (newly added).
    pub added: Vec<String>,
    /// Cases present in baseline but not candidate (removed).
    pub removed: Vec<String>,
}

impl RunDiff {
    /// Diff two runs by their score rows (A2). `baseline` is the older/reference
    /// run, `candidate` the newer one being gated.
    #[must_use]
    pub fn from_rows(baseline: &[ScoreRow], candidate: &[ScoreRow]) -> Self {
        Self::from_verdicts(&case_verdicts(baseline), &case_verdicts(candidate))
    }

    /// Diff two runs already reduced to per-case verdicts. Split out so callers
    /// that already hold verdicts (e.g. an in-memory Evolution run) skip the
    /// re-fold.
    #[must_use]
    pub fn from_verdicts(
        baseline: &BTreeMap<String, CaseVerdict>,
        candidate: &BTreeMap<String, CaseVerdict>,
    ) -> Self {
        let mut regressions = Vec::new();
        let mut fixes = Vec::new();
        let mut other_changes = Vec::new();
        let mut added = Vec::new();
        let removed = baseline
            .keys()
            .filter(|id| !candidate.contains_key(*id))
            .cloned()
            .collect();

        for (id, cand) in candidate {
            let Some(base) = baseline.get(id) else {
                added.push(id.clone());
                continue;
            };
            if base == cand {
                continue;
            }
            match (base, cand) {
                (CaseVerdict::Pass, CaseVerdict::Fail) => regressions.push(id.clone()),
                (CaseVerdict::Fail, CaseVerdict::Pass) => fixes.push(id.clone()),
                _ => other_changes.push(VerdictChange {
                    case_id: id.clone(),
                    from: *base,
                    to: *cand,
                }),
            }
        }

        Self {
            regressions,
            fixes,
            other_changes,
            added,
            removed,
        }
    }

    /// A3 CI gate: `true` when at least one case regressed (pass→fail). This is
    /// the decision a diff-aware gate blocks on — an absolute pass-rate
    /// threshold alone misses cases that traded a pass for a fail while the rate
    /// held steady (`doc/eval.md` §6).
    #[must_use]
    pub const fn has_regression(&self) -> bool {
        !self.regressions.is_empty()
    }
}

// ── loading persisted runs ───────────────────────────────────────────────────

/// Read and parse a run's `scores.jsonl` into typed rows.
///
/// Path is the run directory (`.omini/eval/runs/<run_id>/`); `scores.jsonl` is
/// read from inside it. Blank lines are skipped; a malformed line is a hard
/// error (silent history loss is worse than a loud parse failure).
///
/// # Errors
/// [`EvalError::NotFound`] if the file is absent, [`EvalError::Io`] on read
/// failure, [`EvalError::Json`] on a malformed row.
pub fn load_scores(run_dir: &Path) -> Result<Vec<ScoreRow>> {
    let path = run_dir.join("scores.jsonl");
    let text = std::fs::read_to_string(&path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            EvalError::NotFound(path.clone())
        } else {
            EvalError::Io {
                path: path.clone(),
                source,
            }
        }
    })?;

    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: ScoreRow = serde_json::from_str(line).map_err(|source| EvalError::Json {
            path: path.clone(),
            source,
        })?;
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;

    fn sid() -> SessionId {
        SessionId("01SYNTHETICANALYSIS00000TST".to_owned())
    }

    fn row(case_id: &str, scorer: &str, value: ScoreValue) -> ScoreRow {
        ScoreRow {
            case_id: case_id.to_owned(),
            session_id: sid(),
            scorer: scorer.to_owned(),
            value,
            explanation: None,
            duration_ms: 100,
        }
    }

    fn skip() -> ScoreValue {
        ScoreValue::Skip {
            reason: "n/a".to_owned(),
        }
    }

    // ── fold_case_verdict ─────────────────────────────────────────────────────

    /// One failing scorer must fail the whole case even when others pass — the
    /// regression gate's core rule. Without it, a case that writes the right
    /// file but leaves a tool erroring would score as a pass.
    #[test]
    fn fold_fails_case_when_any_scorer_fails() {
        let values = [ScoreValue::Pass, ScoreValue::Fail, ScoreValue::Pass];
        assert_eq!(fold_case_verdict(values.iter()), CaseVerdict::Fail);
    }

    /// A `Partial` with no failures folds to `Pass` — the case-level verdict is
    /// binary, and partial credit is still forward progress, not a failure.
    #[test]
    fn fold_treats_partial_as_pass_without_failures() {
        let values = [ScoreValue::Partial { score: 0.5 }, skip()];
        assert_eq!(fold_case_verdict(values.iter()), CaseVerdict::Pass);
    }

    /// All-skip must fold to `Skip`, not `Pass` — nothing was judged, so it must
    /// stay out of the pass-rate denominator. This is the bug the old
    /// `count_passed` had (all-skip counted as passed).
    #[test]
    fn fold_all_skip_is_skip_not_pass() {
        let values = [skip(), skip()];
        assert_eq!(fold_case_verdict(values.iter()), CaseVerdict::Skip);
    }

    // ── RunReport (A1) ────────────────────────────────────────────────────────

    /// Pass rate must exclude skipped cases from the denominator: 1 pass, 1
    /// fail, 1 all-skip → rate 0.5, not 0.33. A skip is "not judged", not "not
    /// passed".
    #[test]
    fn report_excludes_skipped_from_pass_rate() {
        let rows = vec![
            row("pass-case", "TestsPass", ScoreValue::Pass),
            row("fail-case", "TestsPass", ScoreValue::Fail),
            row("skip-case", "TestsPass", skip()),
        ];
        let report = RunReport::from_rows(&rows);
        assert_eq!(report.passed, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.pass_rate, 0.5);
        assert!(!report.all_passed());
    }

    /// Per-scorer tallies must count every verdict kind so the report can show
    /// which dimension is weak, not just the case-level roll-up.
    #[test]
    fn report_tallies_per_scorer() {
        let rows = vec![
            row("c1", "TestsPass", ScoreValue::Pass),
            row("c1", "NoToolError", ScoreValue::Fail),
            row("c2", "TestsPass", ScoreValue::Pass),
            row("c2", "NoToolError", skip()),
        ];
        let report = RunReport::from_rows(&rows);
        let tp = report.scorers.get("TestsPass").unwrap();
        assert_eq!(tp.pass, 2);
        let nte = report.scorers.get("NoToolError").unwrap();
        assert_eq!(nte.fail, 1);
        assert_eq!(nte.skip, 1);
    }

    /// An all-skip run has a 0.0 pass rate but must not report `all_passed` as a
    /// failure signal spuriously — nothing failed, so `all_passed` is true even
    /// though the rate is zero. The gate keys on `failed`, not the rate.
    #[test]
    fn report_all_skip_run_has_no_failures() {
        let rows = vec![row("c1", "TestsPass", skip())];
        let report = RunReport::from_rows(&rows);
        assert_eq!(report.pass_rate, 0.0);
        assert_eq!(report.skipped, 1);
        assert!(report.all_passed());
    }

    // ── RunDiff (A2/A3) ───────────────────────────────────────────────────────

    /// A pass→fail flip must be classified as a regression and trip the gate —
    /// this is A3's whole reason to exist.
    #[test]
    fn diff_detects_regression() {
        let baseline = vec![row("c1", "TestsPass", ScoreValue::Pass)];
        let candidate = vec![row("c1", "TestsPass", ScoreValue::Fail)];
        let diff = RunDiff::from_rows(&baseline, &candidate);
        assert_eq!(diff.regressions, vec!["c1".to_owned()]);
        assert!(diff.has_regression());
    }

    /// A fail→pass flip is a fix, not a regression — it must not trip the gate.
    #[test]
    fn diff_detects_fix_without_tripping_gate() {
        let baseline = vec![row("c1", "TestsPass", ScoreValue::Fail)];
        let candidate = vec![row("c1", "TestsPass", ScoreValue::Pass)];
        let diff = RunDiff::from_rows(&baseline, &candidate);
        assert_eq!(diff.fixes, vec!["c1".to_owned()]);
        assert!(diff.regressions.is_empty());
        assert!(!diff.has_regression());
    }

    /// Cases present in only one run must surface as added/removed rather than
    /// silently shifting the pass rate — a shrinking suite must be visible.
    #[test]
    fn diff_tracks_added_and_removed_cases() {
        let baseline = vec![row("gone", "TestsPass", ScoreValue::Pass)];
        let candidate = vec![row("new", "TestsPass", ScoreValue::Pass)];
        let diff = RunDiff::from_rows(&baseline, &candidate);
        assert_eq!(diff.added, vec!["new".to_owned()]);
        assert_eq!(diff.removed, vec!["gone".to_owned()]);
        assert!(!diff.has_regression());
    }

    /// A flip involving `Skip` is neither a clean regression nor fix; it belongs
    /// in `other_changes` so it is not miscounted as a gate signal.
    #[test]
    fn diff_classifies_skip_transitions_as_other() {
        let baseline = vec![row("c1", "TestsPass", ScoreValue::Pass)];
        let candidate = vec![row("c1", "TestsPass", skip())];
        let diff = RunDiff::from_rows(&baseline, &candidate);
        assert!(diff.regressions.is_empty());
        assert_eq!(diff.other_changes.len(), 1);
        assert_eq!(diff.other_changes[0].from, CaseVerdict::Pass);
        assert_eq!(diff.other_changes[0].to, CaseVerdict::Skip);
        assert!(!diff.has_regression());
    }

    // ── ScoreRow round-trip ───────────────────────────────────────────────────

    /// A row must survive serialize → deserialize unchanged so the writer
    /// (`persist_run`) and reader (`load_scores`) agree on one format. A drift
    /// here silently breaks all cross-run history.
    #[test]
    fn score_row_round_trips_json() {
        let original = ScoreRow {
            case_id: "fix-off-by-one-001".to_owned(),
            session_id: sid(),
            scorer: "TestsPass".to_owned(),
            value: ScoreValue::Pass,
            explanation: Some("3 passed".to_owned()),
            duration_ms: 4200,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ScoreRow = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    /// `session_id` must serialize as a bare string (transparent), matching the
    /// runner's persisted rows. Guards against the enum-tagged form re-appearing.
    #[test]
    fn score_row_session_id_is_bare_string() {
        let row = row("c1", "TestsPass", ScoreValue::Pass);
        let json = serde_json::to_string(&row).unwrap();
        assert!(
            json.contains(r#""session_id":"01SYNTHETICANALYSIS00000TST""#),
            "session_id must be a bare string, got: {json}"
        );
    }
}
