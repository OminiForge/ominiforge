//! The `edit` built-in tool: apply content-anchored replacements to files.
//!
//! Input is structured JSON: `edits: [{ path, old, new, replace_all? }]`. Each
//! entry's `old` is located as a contiguous run of lines in the target file
//! (not by line number) and spliced out in favor of `new`. Not knowing the
//! exact current content of the lines you want to touch is the failure mode —
//! there is no separate "read this file first" bookkeeping to bypass, and no
//! whole-file fingerprint to go stale: the model must quote real content, and
//! a `not_found`/`ambiguous` result means it didn't. See `doc/tool-protocol.md`
//! §11.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult, resolve_in_workspace};
use crate::core::payload::{Content, ToolOutput};

/// Applies content-anchored edits relative to the session workspace.
#[derive(Debug, Clone)]
pub struct EditTool {
    workspace: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    edits: Vec<EditEntryArg>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditEntryArg {
    path: String,
    old: Vec<String>,
    new: Vec<String>,
    #[serde(default)]
    replace_all: bool,
}

impl EditTool {
    /// Create an `edit` tool rooted at `workspace`.
    #[must_use]
    pub const fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

fn edit_input_schema() -> serde_json::Value {
    let entry_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File path relative to the workspace root."
            },
            "old": {
                "type": "array",
                "items": { "type": "string" },
                "minItems": 1,
                "description": "The exact current lines to replace, one file line per array item (no embedded newlines). Quote this verbatim from a prior `read`/`edit`/`write` output — a mismatch (not found, or found in more than one place without `replace_all`) is rejected rather than guessed at."
            },
            "new": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Replacement lines, one per array item. Empty array deletes `old` outright. To insert, include an unchanged anchor line in both `old` and `new` alongside the inserted lines."
            },
            "replace_all": {
                "type": "boolean",
                "description": "Replace every non-overlapping occurrence of `old` instead of requiring it to be unique. Defaults to false."
            }
        },
        "required": ["path", "old", "new"],
        "additionalProperties": false
    });
    serde_json::json!({
        "type": "object",
        "properties": {
            "edits": {
                "type": "array",
                "items": entry_schema,
                "minItems": 1,
                "description": "One or more content-anchored replacements, applied atomically: if any entry fails to resolve, no file is changed. Entries may target the same path (applied together, non-overlapping) or different paths."
            }
        },
        "required": ["edits"],
        "additionalProperties": false
    })
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "edit".to_owned(),
            description: "Replace exact lines of text in existing files — no line numbers, \
                          no prior-read tag. Give `edits: [{ path, old, new, replace_all? }]`: \
                          `old` is the exact current content you're replacing (quoted \
                          verbatim from a `read`/`edit`/`write` output, one file line per \
                          array item), `new` is what it becomes (empty array deletes; an \
                          insert keeps an unchanged anchor line in both `old` and `new` \
                          alongside the new lines). `old` must match exactly one place in \
                          the file unless `replace_all` is set, in which case every \
                          non-overlapping occurrence is replaced. A patch is atomic across \
                          all entries (same or different files): if any entry doesn't \
                          resolve, nothing is written. On success the result reports how \
                          many replacements were made per file, not a diff — you already \
                          have the before/after text in this call's own arguments."
                .to_owned(),
            input_schema: edit_input_schema(),
        }
    }

    async fn invoke(&self, input: ToolInput) -> ToolResult {
        let args: EditArgs = serde_json::from_value(input.input)
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

        if args.edits.is_empty() {
            return Err(ToolError::InvalidInput("empty `edits`".to_owned()));
        }
        let entries = validate_entries(args.edits).map_err(ToolError::InvalidInput)?;

        // Group by the RESOLVED absolute path, preserving first-seen order, so
        // the result lists files in the order the model referenced them.
        // Grouping by the raw path string would split "src/a.rs" and
        // "./src/a.rs" into two groups planned independently — the later write
        // would silently clobber the earlier one. A path that fails to resolve
        // is an `invalid_path` business error; a group's display name is the
        // first spelling seen.
        let mut groups: Vec<(String, PathBuf, Vec<Entry>)> = Vec::new();
        for entry in entries {
            let abs_path = match resolve_in_workspace(&self.workspace, &entry.path) {
                Ok(abs_path) => abs_path,
                Err(e) => return Ok(business_error("invalid_path", &e.to_string())),
            };
            match groups.iter_mut().find(|(_, group_abs, _)| *group_abs == abs_path) {
                Some((_, _, ops)) => ops.push(entry),
                None => groups.push((entry.path.clone(), abs_path, vec![entry])),
            }
        }

        // Plan every path first; write nothing until all are validated, so a
        // multi-file patch is all-or-nothing.
        let mut planned: Vec<PlannedWrite> = Vec::with_capacity(groups.len());
        for (rel_path, abs_path, ops) in &groups {
            match Self::plan_path(rel_path, abs_path, ops).await {
                Ok(plan) => planned.push(plan),
                Err(business) => return Ok(business),
            }
        }

        let mut summaries = Vec::with_capacity(planned.len());
        for plan in planned {
            if let Err(e) = tokio::fs::write(&plan.abs_path, plan.new_content.as_bytes()).await {
                return Ok(business_error(
                    "write_failed",
                    &format!("failed to write {}: {e}", plan.rel_path),
                ));
            }
            summaries.push(format!(
                "edited {} ({} replacement{})",
                plan.rel_path,
                plan.replacement_count,
                if plan.replacement_count == 1 { "" } else { "s" }
            ));
        }

        Ok(ToolOutput {
            content: vec![Content::Text(summaries.join("\n"))],
            is_error: false,
            error_code: None,
        })
    }
}

/// A validated `old`/`new` entry for one path.
struct Entry {
    path: String,
    /// 1-based position in the incoming `edits` array, cited in error
    /// messages so the model knows which entry to fix.
    ordinal: usize,
    old: Vec<String>,
    new: Vec<String>,
    replace_all: bool,
}

/// A validated path's worth of edits, ready to write.
struct PlannedWrite {
    abs_path: PathBuf,
    rel_path: String,
    new_content: String,
    replacement_count: usize,
}

impl EditTool {
    /// Validate one path's entries against disk and compute the new content.
    ///
    /// Returns `Err(ToolOutput)` for a *business* failure (no match, ambiguous
    /// match, missing file, overlapping edits) that the model should see and
    /// react to.
    async fn plan_path(
        rel_path: &str,
        abs_path: &Path,
        ops: &[Entry],
    ) -> Result<PlannedWrite, ToolOutput> {
        let content = tokio::fs::read_to_string(abs_path).await.map_err(|e| {
            business_error(
                "read_failed",
                &format!("failed to read {rel_path} for edit: {e}"),
            )
        })?;
        let trailing_newline = content.ends_with('\n');
        let lines: Vec<&str> = content.lines().collect();

        // Resolve every entry's `old` to splices against the ORIGINAL lines
        // (matching, not mutating, as we go) — same "anchor to one snapshot"
        // guarantee the old line-number scheme gave, but the snapshot here is
        // just "the file as read at the top of this call".
        let mut splices: Vec<(usize, usize, &Entry)> = Vec::new();
        for entry in ops {
            let matches = find_matches(&lines, &entry.old);
            match matches.len() {
                0 => {
                    return Err(business_error(
                        "not_found",
                        &format!(
                            "{rel_path}: no match for the given `old` lines (first line: {:?})",
                            entry.old.first().map_or("", String::as_str)
                        ),
                    ));
                }
                1 => splices.push((matches[0], matches[0] + entry.old.len(), entry)),
                _ if entry.replace_all => {
                    splices.extend(
                        matches
                            .into_iter()
                            .map(|start| (start, start + entry.old.len(), entry)),
                    );
                }
                n => {
                    return Err(business_error(
                        "ambiguous",
                        &format!(
                            "{rel_path}: `old` matches {n} places; pass `replace_all: true` or narrow \
                             the quoted lines to make it unique"
                        ),
                    ));
                }
            }
        }

        // Overlap check: two entries touching the same line is rejected as a
        // whole, same as the old op-based scheme. Name the conflicting entries
        // by their 1-based `edits` position so the model can merge or narrow
        // them. (Two splices from one `replace_all` entry can't overlap —
        // `find_matches` skips past each match.)
        splices.sort_by_key(|&(start, ..)| start);
        let mut prev_end = 0usize;
        let mut prev_ordinal = 0usize;
        for (idx, &(start, end, entry)) in splices.iter().enumerate() {
            if idx > 0 && start < prev_end {
                return Err(business_error(
                    "overlapping_edits",
                    &format!(
                        "{rel_path}: entries {prev_ordinal} and {} overlap; merge them into one \
                         entry or quote disjoint lines",
                        entry.ordinal
                    ),
                ));
            }
            prev_end = end;
            prev_ordinal = entry.ordinal;
        }

        let replacement_count = splices.len();

        // Apply high-index first so earlier splices' indices stay valid.
        let mut out: Vec<String> = lines.iter().map(|s| (*s).to_owned()).collect();
        for (start, end, entry) in splices.into_iter().rev() {
            out.splice(start..end, entry.new.iter().cloned());
        }

        let mut new_content = out.join("\n");
        if trailing_newline && !new_content.is_empty() {
            new_content.push('\n');
        }

        Ok(PlannedWrite {
            abs_path: abs_path.to_path_buf(),
            rel_path: rel_path.to_owned(),
            new_content,
            replacement_count,
        })
    }
}

/// Every start index (0-based) at which `needle` occurs as a contiguous run in
/// `haystack`, scanning left to right and skipping past a match so overlapping
/// occurrences are not double-counted under `replace_all`.
fn find_matches(haystack: &[&str], needle: &[String]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    let mut starts = Vec::new();
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if haystack[i..i + needle.len()]
            .iter()
            .zip(needle)
            .all(|(h, n)| *h == n.as_str())
        {
            starts.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    starts
}

/// Validate the parsed entries: no embedded newlines, non-empty `old`,
/// non-empty `path`. These are protocol errors (malformed input the model
/// cannot react to via `is_error`), not business errors.
fn validate_entries(edits: Vec<EditEntryArg>) -> Result<Vec<Entry>, String> {
    edits
        .into_iter()
        .enumerate()
        .map(|(idx, e)| {
            let ctx = format!("edits[{idx}]");
            if e.path.is_empty() {
                return Err(format!("{ctx}: empty `path`"));
            }
            if e.old.is_empty() {
                return Err(format!(
                    "{ctx} ({}): empty `old` — edit requires existing content to replace; use `write` to create a new file",
                    e.path
                ));
            }
            validate_lines(&ctx, &e.path, &e.old)?;
            validate_lines(&ctx, &e.path, &e.new)?;
            Ok(Entry {
                path: e.path,
                ordinal: idx + 1,
                old: e.old,
                new: e.new,
                replace_all: e.replace_all,
            })
        })
        .collect()
}

fn validate_lines(ctx: &str, path: &str, lines: &[String]) -> Result<(), String> {
    if lines.iter().any(|l| l.contains('\n') || l.contains('\r')) {
        return Err(format!(
            "{ctx} ({path}): each line item must be one output line; split multi-line content into multiple array items"
        ));
    }
    Ok(())
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

    fn tool(workspace: PathBuf) -> EditTool {
        EditTool::new(workspace)
    }

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    // Takes `edits` by value: it is moved straight into the `json!` object, so a
    // reference would only force callers to clone. clippy::pedantic flags the
    // by-value arg regardless, hence the local allow.
    #[allow(clippy::needless_pass_by_value)]
    fn call(edits: serde_json::Value) -> ToolInput {
        ToolInput {
            call_id: "c1".to_owned(),
            input: serde_json::json!({ "edits": edits }),
            timeout: Duration::from_secs(5),
        }
    }

    fn text(out: &ToolOutput) -> String {
        match &out.content[0] {
            Content::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unique_replace_rewrites_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["b"], "new": ["B"] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(text(&out), "edited f.txt (1 replacement)");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nB\nc\n"
        );
    }

    #[tokio::test]
    async fn multi_line_old_matches_contiguous_block() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["b", "c"], "new": ["B1", "B2", "B3"] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nB1\nB2\nB3\nd\n"
        );
    }

    #[tokio::test]
    async fn empty_new_deletes_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["b"], "new": [] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nc\n"
        );
    }

    /// Insert = an unchanged anchor line kept in both `old` and `new`, with the
    /// new lines added alongside it. No dedicated insert op is needed.
    #[tokio::test]
    async fn insert_via_anchor_line_kept_in_old_and_new() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["a"], "new": ["a", "A1"] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nA1\nb\n"
        );
    }

    #[tokio::test]
    async fn replace_all_replaces_every_non_overlapping_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x\ny\nx\nz\nx\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["x"], "new": ["X"], "replace_all": true }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(text(&out), "edited f.txt (3 replacements)");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "X\ny\nX\nz\nX\n"
        );
    }

    #[tokio::test]
    async fn not_found_is_business_error_and_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["nope"], "new": ["X"] }
            ])))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("not_found"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nb\nc\n"
        );
    }

    /// A stale `old` (file changed out-of-band since the model last saw it) is
    /// just another `not_found` — no separate staleness mechanism needed.
    #[tokio::test]
    async fn stale_old_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());
        std::fs::write(dir.path().join("f.txt"), "a\nB2\nc\n").unwrap();

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["b"], "new": ["B"] }
            ])))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("not_found"));
    }

    #[tokio::test]
    async fn ambiguous_match_without_replace_all_is_business_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x\ny\nx\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["x"], "new": ["X"] }
            ])))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("ambiguous"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "x\ny\nx\n"
        );
    }

    #[tokio::test]
    async fn cross_entry_overlap_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["a", "b"], "new": ["X"] },
                { "path": "f.txt", "old": ["b", "c"], "new": ["Y"] }
            ])))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("overlapping_edits"));
        assert!(
            text(&out).contains("entries 1 and 2"),
            "overlap error must name the conflicting entries by 1-based `edits` position: {}",
            text(&out)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nb\nc\n",
            "overlap must leave the file untouched"
        );
    }

    #[tokio::test]
    async fn disjoint_entries_same_path_both_apply() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "1\n2\n3\n4\n5\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["1"], "new": ["one"] },
                { "path": "f.txt", "old": ["5"], "new": ["five"] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "one\n2\n3\n4\nfive\n"
        );
    }

    /// A multi-file patch is all-or-nothing: if the second file's edit doesn't
    /// resolve, the first file must not have been written.
    #[tokio::test]
    async fn multi_file_patch_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "a.txt", "old": ["a"], "new": ["A"] },
                { "path": "b.txt", "old": ["nope"], "new": ["B"] }
            ])))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "a\n",
            "first file must be untouched when a later path fails"
        );
    }

    #[tokio::test]
    async fn embedded_newline_in_old_is_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["a\nb"], "new": ["x"] }
            ])))
            .await;
        assert!(matches!(out, Err(ToolError::InvalidInput(msg)) if msg.contains("one output line")));
    }

    #[tokio::test]
    async fn empty_old_is_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": [], "new": ["x"] }
            ])))
            .await;
        assert!(matches!(out, Err(ToolError::InvalidInput(msg)) if msg.contains("empty `old`")));
    }

    #[tokio::test]
    async fn empty_edits_is_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(call(serde_json::json!([]))).await;
        assert!(matches!(out, Err(ToolError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn escaping_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "../escape", "old": ["a"], "new": ["x"] }
            ])))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("invalid_path"));
    }

    #[tokio::test]
    async fn missing_file_is_read_failed() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "nope.txt", "old": ["a"], "new": ["x"] }
            ])))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("read_failed"));
    }

    /// Two spellings of one file (`f.txt` vs `./f.txt`) resolve to the same
    /// absolute path, so they must be planned as ONE group — grouping by the
    /// raw spelling would write the file twice and let the second write
    /// silently clobber the first.
    #[tokio::test]
    async fn same_file_different_spellings_apply_together() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["a"], "new": ["A"] },
                { "path": "./f.txt", "old": ["c"], "new": ["C"] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            text(&out),
            "edited f.txt (2 replacements)",
            "one summary line per resolved file, named by its first spelling"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "A\nb\nC\n",
            "both entries must land — raw-spelling grouping would lose one"
        );
    }

    /// Overlap detection must hold across spellings too: the two entries are
    /// one group, so touching the same line is `overlapping_edits`, not two
    /// independent writes.
    #[tokio::test]
    async fn cross_spelling_overlap_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["a", "b"], "new": ["X"] },
                { "path": "./f.txt", "old": ["b", "c"], "new": ["Y"] }
            ])))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("overlapping_edits"));
        assert!(
            text(&out).contains("entries 1 and 2"),
            "overlap error must name the conflicting entries by 1-based `edits` position: {}",
            text(&out)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nb\nc\n",
            "overlap must leave the file untouched"
        );
    }

    // --- trailing newline ---------------------------------------------------
    // `edit` splits the file into lines and rejoins with `\n`; the original
    // trailing-newline convention is restored on write. Pin that deliberate
    // behavior byte-for-byte so a refactor can't silently "normalize" files.

    #[tokio::test]
    async fn missing_trailing_newline_stays_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["b"], "new": ["B"] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"a\nB\nc",
            "edit must not add a trailing newline the file never had"
        );
    }

    #[tokio::test]
    async fn trailing_newline_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["b"], "new": ["B"] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"a\nB\nc\n",
            "edit must keep the file's trailing newline"
        );
    }

    /// Replacing the whole file with nothing yields a 0-byte file, not a lone
    /// `\n` — the rejoin adds no trailing newline to empty content.
    #[tokio::test]
    async fn full_file_old_with_empty_new_yields_zero_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["a", "b"], "new": [] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"",
            "deleting every line must leave a 0-byte file, not a lone newline"
        );
    }

    // --- find_matches ----------------------------------------------------

    #[test]
    fn find_matches_is_non_overlapping() {
        let hay = ["a", "a", "a"];
        let needle = lines(&["a", "a"]);
        // Matches at 0..2, then resumes scanning at 2 — only a single-line "a"
        // left, no second full match, so exactly one hit.
        assert_eq!(find_matches(&hay, &needle), vec![0]);
    }

    #[test]
    fn find_matches_finds_every_occurrence_for_replace_all() {
        let hay = ["x", "y", "x", "z", "x"];
        let needle = lines(&["x"]);
        assert_eq!(find_matches(&hay, &needle), vec![0, 2, 4]);
    }
}
