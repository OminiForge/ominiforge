//! The `search` built-in tool: search file CONTENTS by regex, honoring
//! `.gitignore`.
//!
//! This is the content counterpart to `find` (which matches paths only). The
//! walk uses the same `ignore` rules as `find` (`.gitignore`, `.git/` and
//! hidden files skipped); matching and binary detection come from ripgrep's
//! own crates (`grep-regex` / `grep-searcher`), so behavior — including
//! skipping binary files — matches what `rg` would do, with no external
//! binary and identical results on every platform.
//!
//! Several patterns are combined into ONE alternation `(?:p1)|(?:p2)` (each
//! wrapped in a non-capturing group so `|` inside one pattern cannot leak into
//! the others), so a line matching ANY pattern is returned — the union
//! semantics of `rg -e p1 -e p2`. Output mirrors `grep -rn`: one
//! `path:line:content` line per match, sorted by path then line number, capped
//! at [`RESULT_CAP`] with the true total reported on truncation. Paths are
//! workspace-relative and `/`-separated on every platform.

use std::path::{Path, PathBuf};

use grep_regex::RegexMatcher;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;
use serde::Deserialize;

use super::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult, resolve_in_workspace};
use crate::core::payload::{Content, ToolOutput};

/// Hard ceiling on match lines returned, and the default when the caller
/// passes no `max_results`. A larger match set is truncated to the effective
/// limit, with the true total reported so the caller knows to refine the
/// pattern. The cap is a context-protection ceiling: `max_results` may LOWER
/// it, never raise it.
const RESULT_CAP: usize = 200;

/// Searches file contents under the workspace for lines matching a regex.
#[derive(Debug, Clone)]
pub struct SearchTool {
    workspace: PathBuf,
}

#[derive(Deserialize)]
struct SearchArgs {
    patterns: Vec<String>,
    path: Option<String>,
    include: Option<Vec<String>>,
    max_results: Option<usize>,
}

/// One matching line.
struct Hit {
    path: String,
    line: u64,
    text: String,
}

impl SearchTool {
    /// Create a `search` tool rooted at `workspace`.
    #[must_use]
    pub const fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for SearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "search".to_owned(),
            description: "Search file CONTENTS for lines matching any of the given \
                          regex PATTERNS — the content counterpart to `find` (which \
                          matches paths only). Prefer this over running grep/rg via \
                          `shell`: it honors .gitignore (`target/`, `node_modules/`, \
                          `.git/`, hidden dot-files skipped) and skips binary files \
                          automatically. HOW TO USE: `patterns` (required) is one or \
                          more regexes (Rust syntax, matched line-by-line); a line \
                          matching ANY pattern is returned (union). Pass several \
                          patterns to find several alternatives in ONE call (e.g. \
                          [\"fn \\\\(\\\\w+\", \"let \\\\(\\\\w+\"]) instead of writing a big \
                          `a|b` regex or making several calls; each pattern is \
                          self-contained, so `|` inside one pattern does not leak \
                          into others. `path` (optional) scopes the search to a \
                          sub-directory or file relative to the workspace root \
                          (omit for the whole workspace). `include` (optional) is \
                          one or more globs (e.g. [\"*.rs\", \"*.toml\"]); a file \
                          matching ANY is searched (union), and a glob with no `/` \
                          matches at any depth. `max_results` (optional) caps \
                          returned lines; it defaults to 200 and may lower but \
                          never raise the built-in 200 ceiling. OUTPUT: one \
                          `path:line:content` line per match (like `grep -rn`), \
                          sorted by path then line; the true total is reported \
                          when truncated (refine with path/include/a tighter \
                          pattern)."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "patterns": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "One or more regexes (Rust syntax). A line \
                                        matching ANY pattern is returned (union)."
                    },
                    "path": {
                        "type": "string",
                        "description": "Optional sub-directory or file, relative to \
                                        the workspace root, to scope the search to. \
                                        Defaults to the whole workspace."
                    },
                    "include": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional globs (e.g. `*.rs`) limiting which \
                                        files are searched; a file matching ANY is \
                                        searched (union). A glob with no `/` matches \
                                        at any depth."
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional cap on returned match lines. \
                                        Defaults to 200; values above 200 are \
                                        clamped to 200. Use a small value when a \
                                        broad pattern only needs a few examples."
                    }
                },
                "required": ["patterns"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, input: ToolInput) -> ToolResult {
        let args: SearchArgs = serde_json::from_value(input.input)
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

        if args.patterns.is_empty() {
            return Ok(business_error(
                "bad_pattern",
                "at least one pattern is required",
            ));
        }

        // Combine the patterns into ONE alternation. Each is wrapped in a
        // non-capturing group `(?:...)` so `|` inside one pattern cannot leak
        // into the others (`a|b` + `c|d` must stay `(a|b)|(c|d)`, not collapse
        // to `a|b|c|d` — `|` binds loosest). Compiled once, scanned once.
        let combined = args
            .patterns
            .iter()
            .map(|p| format!("(?:{p})"))
            .collect::<Vec<_>>()
            .join("|");
        let matcher = match RegexMatcher::new(&combined) {
            Ok(m) => m,
            Err(e) => return Ok(business_error("bad_pattern", &e.to_string())),
        };

        // Resolve the optional `path` scope, keeping it inside the workspace.
        let root = match &args.path {
            None => self.workspace.clone(),
            Some(rel) => resolve_in_workspace(&self.workspace, rel)?,
        };

        // Compile the optional `include` globs into one set (same semantics as
        // `find`: a pattern with no `/` matches at any depth; union).
        let include = match &args.include {
            None => None,
            Some(patterns) => {
                let mut builder = globset::GlobSetBuilder::new();
                for pat in patterns {
                    let normalized = if pat.contains('/') {
                        pat.clone()
                    } else {
                        format!("**/{pat}")
                    };
                    match globset::GlobBuilder::new(&normalized)
                        .literal_separator(true)
                        .build()
                    {
                        Ok(g) => {
                            builder.add(g);
                        }
                        Err(e) => {
                            return Ok(business_error("bad_pattern", &format!("{pat}: {e}")));
                        }
                    }
                }
                match builder.build() {
                    Ok(gs) => Some(gs),
                    Err(e) => return Ok(business_error("bad_pattern", &e.to_string())),
                }
            }
        };

        // The effective limit: the caller's `max_results` (defaulting to the
        // built-in cap) may tighten but never widen the RESULT_CAP ceiling — a
        // model asking for 10_000 lines still gets at most RESULT_CAP.
        let limit = args
            .max_results
            .map_or(RESULT_CAP, |n| n.clamp(1, RESULT_CAP));

        let workspace = self.workspace.clone();
        // `grep_searcher` is synchronous and does blocking I/O; keep it off the
        // async runtime's worker threads.
        let outcome = tokio::task::spawn_blocking(move || {
            walk(&workspace, &root, &matcher, include.as_ref(), limit)
        })
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(render(&outcome))
    }
}

/// The result of a walk: hits (already sorted, already capped) plus the true
/// total so truncation can be reported honestly.
struct Outcome {
    hits: Vec<Hit>,
    total: usize,
}

/// Walk `root` (honoring `.gitignore`), collecting one [`Hit`] per line whose
/// content matches `matcher`. Binary files are skipped by the searcher.
///
/// `root` may be a directory (walked recursively) or a single FILE — the
/// walker's depth-0 entry is the root itself, which for a directory is
/// skipped (directories carry no content) but for a file IS the one and only
/// candidate, so the depth-0 skip must only apply when that entry is not a
/// file.
///
/// Paths are `/`-separated regardless of platform; hits are collected then
/// sorted by path then line for stable output. The full match count is kept
/// even past `limit`; only the returned vector is truncated.
fn walk(
    workspace: &Path,
    root: &Path,
    matcher: &RegexMatcher,
    include: Option<&globset::GlobSet>,
    limit: usize,
) -> Outcome {
    let mut hits: Vec<Hit> = Vec::new();

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        // Skip files whose first bytes look binary (contain a NUL), like `rg`.
        .binary_detection(grep_searcher::BinaryDetection::quit(b'\x00'))
        .build();

    for entry in ignore::WalkBuilder::new(root)
        .require_git(false)
        .build()
        .flatten()
    {
        // Skip directory entries (including the root dir at depth 0); keep
        // files — a depth-0 entry that IS a file is the caller's file-scoped
        // `path` and must be searched, not skipped.
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(workspace) else {
            continue;
        };
        let rel = rel_to_slash(rel);
        if let Some(g) = include
            && !g.is_match(&rel)
        {
            continue;
        }
        // Collect this file's matching lines. `UTF8` decodes each match to
        // text; a file with invalid UTF-8 contributes no lines (like `rg`
        // without `-a`).
        let _ = searcher.search_path(
            matcher,
            entry.path(),
            UTF8(|line, text| {
                hits.push(Hit {
                    path: rel.clone(),
                    line,
                    text: text.trim_end_matches(['\r', '\n']).to_owned(),
                });
                Ok(true)
            }),
        );
    }

    hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    let total = hits.len();
    hits.truncate(limit);
    Outcome { hits, total }
}

/// Render a relative path as a `/`-separated string, so matching and output
/// are platform-independent.
fn rel_to_slash(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Format the outcome: a header line with the count, then one
/// `path:line:content` line per match. On truncation the header names the true
/// total and how many are shown.
fn render(outcome: &Outcome) -> ToolOutput {
    use std::fmt::Write as _;
    let header = if outcome.total == 0 {
        // Never answer a 0-hit query with bare emptiness: name the miss and the
        // way out, so the model's next step is actionable.
        "0 matches — no lines matched; broaden the pattern or widen the path/include scope"
            .to_owned()
    } else if outcome.total > outcome.hits.len() {
        format!(
            "{} matches (showing first {})",
            outcome.total,
            outcome.hits.len()
        )
    } else {
        format!("{} matches", outcome.total)
    };
    let mut text = header;
    for hit in &outcome.hits {
        let _ = write!(text, "\n{}:{}:{}", hit.path, hit.line, hit.text);
    }
    ToolOutput {
        content: vec![Content::Text(text)],
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

    fn input(pattern: &str) -> ToolInput {
        inputs(&[pattern])
    }

    fn inputs(patterns: &[&str]) -> ToolInput {
        ToolInput {
            call_id: "c1".to_owned(),
            input: serde_json::json!({ "patterns": patterns }),
            timeout: Duration::from_secs(5),
        }
    }

    fn text(out: &ToolOutput) -> String {
        match &out.content[0] {
            Content::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// The lines after the header, as `path:line:content`.
    fn matches(out: &ToolOutput) -> Vec<String> {
        text(out).lines().skip(1).map(str::to_owned).collect()
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// The core case: a literal match reports `path:line:content`, so the
    /// model can jump straight to the location without a follow-up read.
    #[tokio::test]
    async fn reports_path_line_and_content() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "fn foo() {}\nlet target = 1;\n");
        let t = SearchTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("target")).await.unwrap();
        assert_eq!(matches(&out), vec!["a.rs:2:let target = 1;"]);
    }

    /// Regex (not just literal) matching is the point of the tool.
    #[tokio::test]
    async fn matches_regex() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "let x1 = 1;\nlet xy = 2;\n");
        let t = SearchTool::new(dir.path().to_path_buf());

        let out = t.invoke(input(r"x\d")).await.unwrap();
        assert_eq!(matches(&out), vec!["a.rs:1:let x1 = 1;"]);
    }

    /// Several patterns match their union: a line matching ANY pattern is
    /// returned. This is the whole point of accepting a list — one walk finds
    /// several alternatives instead of forcing several calls.
    #[tokio::test]
    async fn multiple_patterns_match_union() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.rs",
            "let foo = 1;\nlet bar = 2;\nlet baz = 3;\n",
        );
        let t = SearchTool::new(dir.path().to_path_buf());

        let out = t.invoke(inputs(&["foo", "bar"])).await.unwrap();
        assert_eq!(
            matches(&out),
            vec!["a.rs:1:let foo = 1;", "a.rs:2:let bar = 2;"]
        );
    }

    /// `|` inside one pattern must NOT leak into the others: each pattern is
    /// wrapped in `(?:...)` before being joined, so `a|b` and `c` stay
    /// `(a|b)|(c)` rather than collapsing to `a|b|c`. If the grouping were
    /// dropped, an alternation's branch could swallow a neighboring pattern.
    #[tokio::test]
    async fn alternation_in_one_pattern_does_not_leak() {
        let dir = tempfile::tempdir().unwrap();
        // "foo" matches only via the first pattern's alternation; "bar" only
        // via the second. Neither should match "zap".
        write(dir.path(), "a.rs", "foo\nbar\nzap\n");
        let t = SearchTool::new(dir.path().to_path_buf());

        let out = t.invoke(inputs(&["foo|fo+", "bar"])).await.unwrap();
        assert_eq!(matches(&out), vec!["a.rs:1:foo", "a.rs:2:bar"]);
    }

    /// `.gitignore` rules are honored — searching must not drown in `target/`
    /// or `node_modules/`. This is why the tool beats `grep -r` via shell.
    #[tokio::test]
    async fn gitignore_excludes_matches() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "target/\n");
        write(dir.path(), "keep.rs", "hit\n");
        write(dir.path(), "target/junk.rs", "hit\n");
        let t = SearchTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("hit")).await.unwrap();
        assert_eq!(matches(&out), vec!["keep.rs:1:hit"]);
    }

    /// Binary files are skipped rather than dumping garbage into the context.
    #[tokio::test]
    async fn binary_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "hit\n");
        std::fs::write(dir.path().join("bin.dat"), b"hi\x00hit\x00").unwrap();
        let t = SearchTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("hit")).await.unwrap();
        assert_eq!(matches(&out), vec!["a.rs:1:hit"]);
    }

    /// `path` scopes the search to a sub-directory.
    #[tokio::test]
    async fn path_scopes_search() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/a.rs", "hit\n");
        write(dir.path(), "other/b.rs", "hit\n");
        let t = SearchTool::new(dir.path().to_path_buf());

        let mut i = input("hit");
        i.input["path"] = serde_json::json!("src");
        let out = t.invoke(i).await.unwrap();
        assert_eq!(matches(&out), vec!["src/a.rs:1:hit"]);
    }

    /// `path` may name a single FILE, not just a directory: the walker's
    /// root entry (depth 0) IS that file, so the depth-0 skip must not drop
    /// it. Regression test for the bug where a file-scoped `path` always
    /// returned 0 matches.
    #[tokio::test]
    async fn path_scopes_search_to_a_single_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/a.rs", "hit\n");
        write(dir.path(), "other/b.rs", "hit\n");
        let t = SearchTool::new(dir.path().to_path_buf());

        let mut i = input("hit");
        i.input["path"] = serde_json::json!("src/a.rs");
        let out = t.invoke(i).await.unwrap();
        assert_eq!(matches(&out), vec!["src/a.rs:1:hit"]);
    }

    /// A file-scoped `path` still honors `include`: a glob that doesn't match
    /// the file yields zero hits rather than silently ignoring the filter.
    #[tokio::test]
    async fn file_path_still_honors_include() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/a.rs", "hit\n");
        let t = SearchTool::new(dir.path().to_path_buf());

        let mut i = input("hit");
        i.input["path"] = serde_json::json!("src/a.rs");
        i.input["include"] = serde_json::json!(["*.txt"]);
        let out = t.invoke(i).await.unwrap();
        assert!(text(&out).starts_with("0 matches"));
    }

    /// `path` may not escape the workspace.
    #[tokio::test]
    async fn path_outside_workspace_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let t = SearchTool::new(dir.path().to_path_buf());

        let mut i = input("hit");
        i.input["path"] = serde_json::json!("../secret");
        assert!(t.invoke(i).await.is_err());
    }

    /// `include` limits which files are searched.
    #[tokio::test]
    async fn include_filters_files() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "hit\n");
        write(dir.path(), "a.txt", "hit\n");
        let t = SearchTool::new(dir.path().to_path_buf());

        let mut i = input("hit");
        i.input["include"] = serde_json::json!(["*.rs"]);
        let out = t.invoke(i).await.unwrap();
        assert_eq!(matches(&out), vec!["a.rs:1:hit"]);
    }

    /// Several `include` globs match their union: a file matching ANY glob is
    /// searched.
    #[tokio::test]
    async fn multiple_includes_match_union() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "hit\n");
        write(dir.path(), "b.toml", "hit\n");
        write(dir.path(), "c.txt", "hit\n");
        let t = SearchTool::new(dir.path().to_path_buf());

        let mut i = input("hit");
        i.input["include"] = serde_json::json!(["*.rs", "*.toml"]);
        let out = t.invoke(i).await.unwrap();
        assert_eq!(matches(&out), vec!["a.rs:1:hit", "b.toml:1:hit"]);
    }

    /// An invalid regex is a business error the model can correct, not a
    /// protocol fault.
    #[tokio::test]
    async fn invalid_regex_is_business_error() {
        let dir = tempfile::tempdir().unwrap();
        let t = SearchTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("[unclosed")).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("bad_pattern"));
    }

    /// An empty pattern list is a business error — nothing to match is a
    /// caller mistake, not an empty result set.
    #[tokio::test]
    async fn empty_patterns_is_business_error() {
        let dir = tempfile::tempdir().unwrap();
        let t = SearchTool::new(dir.path().to_path_buf());

        let out = t.invoke(inputs(&[])).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("bad_pattern"));
    }

    /// A 0-hit query must not read as an empty output: the message names the
    /// miss and the way out (broaden the pattern / widen the scope), so the
    /// model has an actionable next step rather than a bare "0 matches".
    #[tokio::test]
    async fn no_matches_reports_zero() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "hello\n");
        let t = SearchTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("zzz")).await.unwrap();
        assert!(!out.is_error);
        assert!(text(&out).starts_with("0 matches"));
        assert!(text(&out).contains("broaden"));
    }

    // --- caller-set limit ----------------------------------------------------

    /// `max_results` lets the model tighten the cap when a broad pattern only
    /// needs a few examples — a small ask must not return the full 200.
    #[tokio::test]
    async fn max_results_tightens_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "hit\nhit\nhit\nhit\nhit\n");
        let t = SearchTool::new(dir.path().to_path_buf());

        let mut i = input("hit");
        i.input["max_results"] = serde_json::json!(2);
        let out = t.invoke(i).await.unwrap();
        assert_eq!(matches(&out).len(), 2);
        assert_eq!(
            text(&out).lines().next().unwrap(),
            "5 matches (showing first 2)"
        );
    }

    /// A `max_results` above the built-in ceiling is clamped DOWN to it — the
    /// model may tighten the context guard, never widen it.
    #[tokio::test]
    async fn max_results_above_cap_is_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let body = "hit\n".repeat(RESULT_CAP + 5);
        write(dir.path(), "a.rs", &body);
        let t = SearchTool::new(dir.path().to_path_buf());

        let mut i = input("hit");
        i.input["max_results"] = serde_json::json!(10_000);
        let out = t.invoke(i).await.unwrap();
        assert_eq!(matches(&out).len(), RESULT_CAP);
    }

    /// Multiple matches across files are sorted by path then line, so output
    /// is stable and diff-friendly.
    #[tokio::test]
    async fn sorted_by_path_then_line() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "b.rs", "hit\n");
        write(dir.path(), "a.rs", "x\nhit\nhit\n");
        let t = SearchTool::new(dir.path().to_path_buf());

        let out = t.invoke(input("hit")).await.unwrap();
        assert_eq!(
            matches(&out),
            vec!["a.rs:2:hit", "a.rs:3:hit", "b.rs:1:hit"]
        );
    }
}
