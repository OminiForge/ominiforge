//! The `find` built-in tool: locate files by glob patterns, honoring
//! `.gitignore`.
//!
//! This is a filename/path finder, NOT a content search (that is the planned
//! `search` tool). It walks the workspace with the same ignore rules `git`
//! applies — every `.gitignore` (and `.git/info/exclude`, global excludes) is
//! respected, and `.git/` itself plus hidden dot-files are skipped — so a match
//! list never drowns in `target/` or `node_modules/`.
//!
//! Matching uses standard glob semantics ([`globset`]) against each file's path
//! *relative to the workspace root*, with one convenience rule (see
//! [`normalize_pattern`]): a pattern with no `/` is matched at any depth. One
//! call may pass several patterns; a file is returned if it matches ANY of them
//! (union), listed once. Results are workspace-relative paths, `/`-separated on
//! every platform, capped at [`RESULT_CAP`].

use std::path::PathBuf;

use serde::Deserialize;

use super::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult};
use crate::core::payload::{Content, ToolOutput};

/// Maximum number of paths returned. A larger match set is truncated to this
/// many, with the true total reported so the caller knows to refine the pattern.
const RESULT_CAP: usize = 200;

/// Finds files under the workspace whose path matches any of a set of globs.
#[derive(Debug, Clone)]
pub struct FindTool {
    workspace: PathBuf,
}

#[derive(Deserialize)]
struct FindArgs {
    patterns: Vec<String>,
}

impl FindTool {
    /// Create a `find` tool rooted at `workspace`.
    #[must_use]
    pub const fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for FindTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "find".to_owned(),
            description: "Find files by one or more glob PATTERNS, relative to the \
                          workspace root. Matches file paths only (not contents — that \
                          is a separate concern). Honors .gitignore: ignored paths \
                          (e.g. `target/`, `node_modules/`), the `.git/` directory, and \
                          hidden dot-files are skipped. Glob syntax: `*` matches any run \
                          of characters except `/`, `?` one character, `**` spans \
                          directories, `[abc]`/`[a-z]` a character class, `{a,b}` \
                          alternatives. A pattern with NO `/` matches at any depth (so \
                          `*.rs` finds every `.rs` file), while a pattern containing `/` \
                          is anchored at the workspace root (so `src/*.rs` matches only \
                          the top level of `src/`, and `src/**/*.rs` any depth under \
                          it). Pass several patterns to match any of them in one call \
                          (union) — a file matching more than one is listed once. \
                          Returns workspace-relative paths, one per line, sorted; at \
                          most 200, with the total reported when truncated."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "patterns": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "One or more glob patterns to match file paths \
                                        against, relative to the workspace root. A file \
                                        matching any pattern is returned."
                    }
                },
                "required": ["patterns"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, input: ToolInput) -> ToolResult {
        let args: FindArgs = serde_json::from_value(input.input)
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

        if args.patterns.is_empty() {
            return Ok(business_error(
                "bad_pattern",
                "at least one pattern is required",
            ));
        }

        // Compile every pattern into one `GlobSet`: a single matcher that is a
        // union of all globs, so one walk answers all patterns at once and a
        // file matching several is still visited (and pushed) only once.
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in &args.patterns {
            match globset::GlobBuilder::new(&normalize_pattern(pattern))
                // `*`/`?` must NOT cross `/`; only `**` spans directories. This
                // is the standard glob semantics the descriptor documents, and
                // what makes `src/*.rs` stay top-level while `src/**/*.rs`
                // recurses.
                .literal_separator(true)
                .build()
            {
                Ok(g) => {
                    builder.add(g);
                }
                Err(e) => {
                    return Ok(business_error("bad_pattern", &format!("{pattern}: {e}")));
                }
            }
        }
        let globset = match builder.build() {
            Ok(gs) => gs,
            Err(e) => return Ok(business_error("bad_pattern", &e.to_string())),
        };

        let workspace = self.workspace.clone();
        // `ignore::Walk` is synchronous and does blocking I/O; keep it off the
        // async runtime's worker threads.
        let outcome = tokio::task::spawn_blocking(move || walk(&workspace, &globset))
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(render(&outcome))
    }
}

/// The result of a walk: matches (already sorted, already capped) plus the true
/// total so truncation can be reported honestly.
struct Outcome {
    matches: Vec<String>,
    total: usize,
}

/// Apply the "no `/` → match at any depth" convenience rule (semantics B).
///
/// A bare pattern like `*.rs` or `Cargo.toml` is rewritten to `**/pattern` so it
/// matches at any depth, matching the intuition of tools like `fd`. A pattern
/// that already contains `/` is anchored at the workspace root and returned
/// unchanged, so `src/*.rs` stays top-level-only.
fn normalize_pattern(pattern: &str) -> String {
    if pattern.contains('/') {
        pattern.to_owned()
    } else {
        format!("**/{pattern}")
    }
}

/// Walk `workspace` (honoring `.gitignore`), collecting workspace-relative paths
/// of files whose path matches any glob in `globset`. Directories themselves are
/// not returned.
///
/// Paths are `/`-separated regardless of platform (so a pattern written with `/`
/// matches on Windows too), collected then sorted for stable output. Each file
/// is visited once, so a file matching several globs appears once. The full
/// match count is kept even past [`RESULT_CAP`]; only the returned vector is
/// truncated.
fn walk(workspace: &std::path::Path, globset: &globset::GlobSet) -> Outcome {
    let mut matches: Vec<String> = Vec::new();
    // `ignore::WalkBuilder` defaults: standard-filters ON (.gitignore, hidden,
    // .git/ excluded) — exactly the "like git" behavior we document.
    // `require_git(false)` makes `.gitignore` files apply even when the
    // workspace is not itself a git repository, so the documented behavior holds
    // unconditionally rather than only inside a checkout.
    for entry in ignore::WalkBuilder::new(workspace)
        .require_git(false)
        .build()
        .flatten()
    {
        // Skip the root and any directory entries — we return files only.
        if entry.depth() == 0 {
            continue;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(workspace) else {
            continue;
        };
        let rel = rel_to_slash(rel);
        if globset.is_match(&rel) {
            matches.push(rel);
        }
    }
    matches.sort();
    let total = matches.len();
    matches.truncate(RESULT_CAP);
    Outcome { matches, total }
}

/// Render a relative path as a `/`-separated string, so glob matching and output
/// are platform-independent.
fn rel_to_slash(rel: &std::path::Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Format the outcome: a header line with the count, then one path per line. On
/// truncation the header names the true total and how many are shown. The UI
/// view is a `listing` envelope so the front-end renders the match list
/// directly, without parsing the header line.
fn render(outcome: &Outcome) -> ToolOutput {
    let header = if outcome.total > outcome.matches.len() {
        format!(
            "{} matches (showing first {})",
            outcome.total,
            outcome.matches.len()
        )
    } else {
        format!("{} matches", outcome.total)
    };
    let mut text = header;
    for path in &outcome.matches {
        text.push('\n');
        text.push_str(path);
    }
    let view = serde_json::json!({
        "kind": "listing",
        "path": "",
        "entries": outcome.matches,
    })
    .to_string();
    ToolOutput {
        content: vec![
            Content::Text(text),
            Content::TextView {
                text: view,
                audience: crate::core::payload::AUDIENCE_UI.to_owned(),
            },
        ],
        is_error: false,
        error_code: None,
    }
}

fn business_error(code: &str, message: &str) -> ToolOutput {
    ToolOutput {
        content: vec![Content::Text(message.to_owned())],
        is_error: true,
        error_code: Some(code.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::time::Duration;

    /// A single-pattern request (the common case in these tests).
    fn input(pattern: &str) -> ToolInput {
        inputs(&[pattern])
    }

    /// A multi-pattern request.
    fn inputs(patterns: &[&str]) -> ToolInput {
        ToolInput {
            call_id: "c1".to_owned(),
            input: serde_json::json!({ "patterns": patterns }),
            timeout: Duration::from_secs(5),
            progress: None,
        }
    }

    fn text(out: &ToolOutput) -> String {
        match &out.content[0] {
            Content::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// The lines after the header, as returned paths.
    fn paths(out: &ToolOutput) -> Vec<String> {
        text(out).lines().skip(1).map(str::to_owned).collect()
    }

    fn write(root: &std::path::Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    // --- semantics B: bare pattern matches at any depth ----------------------

    /// A pattern with no `/` matches files at ANY depth — the `fd`-like
    /// convenience rule that makes `*.rs` find the whole tree, not just the root.
    #[tokio::test]
    async fn bare_pattern_matches_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "top.rs", "");
        write(dir.path(), "src/deep/nested.rs", "");
        write(dir.path(), "notes.txt", "");
        let t = FindTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("*.rs")).await.unwrap();
        assert_eq!(paths(&out), vec!["src/deep/nested.rs", "top.rs"]);
    }

    /// A pattern CONTAINING `/` is anchored at the workspace root: `src/*.rs`
    /// matches only the top level of `src/`, never a deeper file. This is the
    /// half of semantics B that a bare pattern would wrongly widen, so it must be
    /// pinned.
    #[tokio::test]
    async fn slashed_pattern_is_anchored_top_level() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/a.rs", "");
        write(dir.path(), "src/deep/b.rs", "");
        let t = FindTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("src/*.rs")).await.unwrap();
        assert_eq!(paths(&out), vec!["src/a.rs"]);
    }

    /// `**` explicitly spans directories, so `src/**/*.rs` reaches any depth
    /// under `src/` — the escape hatch from the anchored form above.
    #[tokio::test]
    async fn double_star_spans_directories() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/a.rs", "");
        write(dir.path(), "src/deep/b.rs", "");
        write(dir.path(), "other/c.rs", "");
        let t = FindTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("src/**/*.rs")).await.unwrap();
        assert_eq!(paths(&out), vec!["src/a.rs", "src/deep/b.rs"]);
    }

    // --- gitignore is honored ------------------------------------------------

    /// A file matched by a `.gitignore` rule is not returned — this is the whole
    /// point of using the `ignore` walker rather than a raw directory walk. The
    /// test would fail (returning `target/junk.rs`) if standard filters were off.
    #[tokio::test]
    async fn gitignore_excludes_matched_files() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "target/\n");
        write(dir.path(), "keep.rs", "");
        write(dir.path(), "target/junk.rs", "");
        let t = FindTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("*.rs")).await.unwrap();
        assert_eq!(paths(&out), vec!["keep.rs"]);
    }

    // --- files only, sorted --------------------------------------------------

    /// Directories are never returned, only files — a `find` for files should not
    /// surface the directories that contain them.
    #[tokio::test]
    async fn directories_are_not_returned() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "sub/file.rs", "");
        let t = FindTool::new(dir.path().to_path_buf());

        // `sub` matches the glob as a name, but it is a directory → excluded.
        let out = t.invoke(input("**")).await.unwrap();
        assert_eq!(paths(&out), vec!["sub/file.rs"]);
    }

    // --- cap / truncation ----------------------------------------------------

    /// Over the cap, output is truncated to `RESULT_CAP` lines but the header
    /// reports the TRUE total, so the caller learns the pattern was too broad
    /// rather than silently seeing a partial list as if complete.
    #[tokio::test]
    async fn over_cap_truncates_and_reports_total() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(RESULT_CAP + 25) {
            write(dir.path(), &format!("f{i:04}.rs"), "");
        }
        let t = FindTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("*.rs")).await.unwrap();
        let total = RESULT_CAP + 25;
        assert_eq!(
            text(&out).lines().next().unwrap(),
            format!("{total} matches (showing first {RESULT_CAP})")
        );
        assert_eq!(paths(&out).len(), RESULT_CAP);
    }

    // --- no matches / bad pattern --------------------------------------------

    #[tokio::test]
    async fn no_matches_reports_zero() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "");
        let t = FindTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("*.rs")).await.unwrap();
        assert!(!out.is_error);
        assert_eq!(text(&out), "0 matches");
    }

    /// A syntactically invalid glob is a business error the model can correct,
    /// not a protocol fault.
    #[tokio::test]
    async fn invalid_glob_is_business_error() {
        let dir = tempfile::tempdir().unwrap();
        let t = FindTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("[unclosed")).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("bad_pattern"));
    }

    // --- multiple patterns (union) -------------------------------------------

    /// Several patterns match their union: a file matching ANY pattern is
    /// returned. This is the whole point of accepting a list — one walk answers
    /// `*.rs` and `*.toml` together instead of forcing two calls.
    #[tokio::test]
    async fn multiple_patterns_match_union() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "");
        write(dir.path(), "b.toml", "");
        write(dir.path(), "c.md", "");
        let t = FindTool::new(dir.path().to_path_buf());

        let out = t.invoke(inputs(&["*.rs", "*.toml"])).await.unwrap();
        assert_eq!(paths(&out), vec!["a.rs", "b.toml"]);
    }

    /// A file matched by more than one pattern is listed ONCE, not duplicated —
    /// each file is visited a single time regardless of how many globs it hits.
    #[tokio::test]
    async fn overlapping_patterns_do_not_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "lib.rs", "");
        let t = FindTool::new(dir.path().to_path_buf());

        // Both patterns match `lib.rs`.
        let out = t.invoke(inputs(&["*.rs", "lib.*"])).await.unwrap();
        assert_eq!(paths(&out), vec!["lib.rs"]);
        assert_eq!(text(&out).lines().next().unwrap(), "1 matches");
    }

    /// An empty pattern list is a business error the model can correct — nothing
    /// to match is a caller mistake, not an empty result set.
    #[tokio::test]
    async fn empty_patterns_is_business_error() {
        let dir = tempfile::tempdir().unwrap();
        let t = FindTool::new(dir.path().to_path_buf());

        let out = t.invoke(inputs(&[])).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("bad_pattern"));
    }
}
