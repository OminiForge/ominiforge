//! The `edit` built-in tool: apply a line-anchored patch verified against the
//! per-session snapshot recorded by [`read`](super::ReadTool).
//!
//! Input is structured JSON: a single section (`path`, `tag`, `ops`) or
//! multiple `sections`. Each replacement line is one string in `lines`, avoiding
//! the fragile "patch embedded in a JSON string" shape that can double-escape
//! newlines.
//!
//! Before touching a file the tool checks the cited `TAG` against both the
//! snapshot store and the file's current bytes; a mismatch means the read was
//! stale, so the patch is rejected rather than applied to the wrong lines. See
//! `doc/tool-protocol.md`.

use std::path::PathBuf;

use serde::Deserialize;

use super::snapshot::{SnapshotStore, tag_of};
use super::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult, resolve_in_workspace};
use crate::core::payload::{Content, ToolOutput};

/// Applies line-anchored patches relative to the session workspace, verified
/// against snapshots from `read`.
#[derive(Debug, Clone)]
pub struct EditTool {
    workspace: PathBuf,
    snapshots: SnapshotStore,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    /// Single-file structured form.
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    ops: Vec<EditOpArg>,
    /// Multi-file structured form.
    #[serde(default)]
    sections: Vec<EditSectionArg>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditSectionArg {
    path: String,
    tag: String,
    ops: Vec<EditOpArg>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditOpArg {
    op: String,
    #[serde(default)]
    start: Option<usize>,
    #[serde(default)]
    end: Option<usize>,
    #[serde(default)]
    lines: Vec<String>,
}

impl EditTool {
    /// Create an `edit` tool rooted at `workspace`, verifying against the shared
    /// `snapshots` store that `read` populates.
    #[must_use]
    pub const fn new(workspace: PathBuf, snapshots: SnapshotStore) -> Self {
        Self {
            workspace,
            snapshots,
        }
    }
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn descriptor(&self) -> ToolDescriptor {
        let op_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": [
                        "replace",
                        "delete",
                        "insert_after",
                        "insert_before",
                        "insert_head",
                        "insert_tail"
                    ],
                    "description": "Operation to apply."
                },
                "start": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line number. Required for replace/delete/insert_after/insert_before."
                },
                "end": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Inclusive 1-based end line for replace/delete. Defaults to start."
                },
                "lines": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Replacement/inserted content, one output line per array item. Do not embed newline characters; use an empty string for a blank line."
                }
            },
            "required": ["op"],
            "additionalProperties": false
        });
        let section_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to the workspace root."
                },
                "tag": {
                    "type": "string",
                    "description": "Snapshot TAG from the prior read header."
                },
                "ops": {
                    "type": "array",
                    "items": op_schema,
                    "minItems": 1
                }
            },
            "required": ["path", "tag", "ops"],
            "additionalProperties": false
        });
        ToolDescriptor {
            name: "edit".to_owned(),
            description: "Apply line-anchored edits verified against a prior `read` TAG. \
                          Use `path`, `tag`, and `ops` for one file, or `sections` for \
                          multiple files. Each `lines` item is one file line, so \
                          multi-line edits must be arrays, not one string with embedded \
                          newlines."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Single-file form: file path relative to the workspace root."
                    },
                    "tag": {
                        "type": "string",
                        "description": "Single-file form: snapshot TAG from the prior read header."
                    },
                    "ops": {
                        "type": "array",
                        "items": op_schema,
                        "minItems": 1,
                        "description": "Single-file form operations."
                    },
                    "sections": {
                        "type": "array",
                        "items": section_schema,
                        "minItems": 1,
                        "description": "Multi-file form; every section has its own path, tag, and ops."
                    }
                },
                "oneOf": [
                    { "required": ["path", "tag", "ops"] },
                    { "required": ["sections"] }
                ],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, input: ToolInput) -> ToolResult {
        let args: EditArgs = serde_json::from_value(input.input)
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

        // Parse errors are protocol faults: the model sent malformed edit input,
        // so it cannot be retried by reacting to is_error.
        let sections = sections_from_args(args).map_err(ToolError::InvalidInput)?;
        if sections.is_empty() {
            return Err(ToolError::InvalidInput("empty patch".to_owned()));
        }

        // Plan every section first; write nothing until all are validated, so a
        // multi-file patch is all-or-nothing.
        let mut planned: Vec<PlannedWrite> = Vec::with_capacity(sections.len());
        for section in &sections {
            match self.plan_section(section).await {
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
            let new_tag = tag_of(plan.new_content.as_bytes());
            self.snapshots.record(&plan.abs_path, new_tag.clone());
            summaries.push(format!(
                "edited {} ({} ops, now {} lines) -> {}",
                plan.rel_path, plan.op_count, plan.new_line_count, new_tag
            ));
        }

        Ok(ToolOutput {
            content: vec![Content::Text(summaries.join("\n"))],
            is_error: false,
            error_code: None,
        })
    }
}

/// A validated section ready to write.
struct PlannedWrite {
    abs_path: PathBuf,
    rel_path: String,
    new_content: String,
    op_count: usize,
    new_line_count: usize,
}

impl EditTool {
    /// Validate one section against disk + snapshot and compute its new content.
    ///
    /// Returns `Err(ToolOutput)` for a *business* failure (stale tag, missing
    /// file, bad range) that the model should see and react to.
    async fn plan_section(&self, section: &Section) -> Result<PlannedWrite, ToolOutput> {
        let abs_path = resolve_in_workspace(&self.workspace, &section.path)
            .map_err(|e| business_error("invalid_path", &e.to_string()))?;

        let content = tokio::fs::read_to_string(&abs_path).await.map_err(|e| {
            business_error(
                "read_failed",
                &format!("failed to read {} for edit: {e}", section.path),
            )
        })?;
        let current_tag = tag_of(content.as_bytes());

        // The cited tag must match both the file on disk now and what `read`
        // recorded. A `None` recorded tag means the file was never read this
        // session; either way the model must (re-)read to get a fresh anchor.
        let recorded = self.snapshots.get(&abs_path);
        if section.tag != current_tag || recorded.as_deref() != Some(section.tag.as_str()) {
            return Err(business_error(
                "stale_snapshot",
                &format!(
                    "stale snapshot for {}: patch cites #{}, file is #{current_tag}. \
                     Re-`read` {} and rebuild the patch against the new TAG.",
                    section.path, section.tag, section.path
                ),
            ));
        }

        let lines: Vec<&str> = content.lines().collect();
        let new_content = apply_ops(&lines, &section.ops, content.ends_with('\n'))
            .map_err(|msg| business_error("bad_range", &format!("{}: {msg}", section.path)))?;
        let new_line_count = new_content.lines().count();

        Ok(PlannedWrite {
            abs_path,
            rel_path: section.path.clone(),
            new_content,
            op_count: section.ops.len(),
            new_line_count,
        })
    }
}

/// One file's worth of the patch.
#[derive(Debug, PartialEq, Eq)]
struct Section {
    path: String,
    tag: String,
    ops: Vec<Op>,
}

/// A single edit operation as parsed from the patch. The `anchor` stores the
/// user-supplied 1-based descriptor; resolution to a 0-based half-open splice
/// happens in [`resolve_splice`]. `replace`/`delete` have `end > start`;
/// inserts are zero-width (`end == start`).
#[derive(Debug, PartialEq, Eq)]
struct Op {
    /// 1-based anchor as written, kept only for error messages.
    anchor: AnchorRange,
    payload: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum AnchorRange {
    /// Inclusive 1-based line range `start..=end`.
    Range(usize, usize),
    /// Insert after the given 1-based line (0 = head, handled separately).
    After(usize),
    /// Insert before the given 1-based line.
    Before(usize),
    Head,
    Tail,
}

/// Convert the structured input form into internal file sections.
fn sections_from_args(args: EditArgs) -> Result<Vec<Section>, String> {
    let has_sections = !args.sections.is_empty();
    let has_single = args.path.is_some() || args.tag.is_some() || !args.ops.is_empty();

    match (has_single, has_sections) {
        (true, true) => {
            Err("edit input must use either `path`/`tag`/`ops` or `sections`, not both".to_owned())
        }
        (false, false) => {
            Err("edit input must include `path`/`tag`/`ops` or `sections`".to_owned())
        }
        (false, true) => args
            .sections
            .into_iter()
            .map(section_from_arg)
            .collect::<Result<Vec<_>, _>>(),
        (true, false) => section_from_parts(
            args.path
                .ok_or_else(|| "structured edit requires `path`".to_owned())?,
            args.tag
                .ok_or_else(|| "structured edit requires `tag`".to_owned())?,
            args.ops,
        )
        .map(|section| vec![section]),
    }
}

fn section_from_arg(arg: EditSectionArg) -> Result<Section, String> {
    section_from_parts(arg.path, arg.tag, arg.ops)
}

fn section_from_parts(path: String, tag: String, ops: Vec<EditOpArg>) -> Result<Section, String> {
    if path.is_empty() {
        return Err("structured edit section has empty `path`".to_owned());
    }
    if tag.is_empty() {
        return Err(format!("{path}: structured edit section has empty `tag`"));
    }
    if ops.is_empty() {
        return Err(format!("{path}: structured edit section has no ops"));
    }

    let ops = ops
        .into_iter()
        .enumerate()
        .map(|(idx, op)| op_from_arg(&path, idx, op))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Section { path, tag, ops })
}

fn op_from_arg(path: &str, idx: usize, arg: EditOpArg) -> Result<Op, String> {
    let EditOpArg {
        op,
        start,
        end,
        lines,
    } = arg;
    let ctx = format!("{path} op {} `{op}`", idx + 1);
    validate_structured_lines(&ctx, &lines)?;

    let anchor = match op.as_str() {
        "replace" => {
            let start = required_start(&ctx, start)?;
            require_lines(&ctx, &lines)?;
            AnchorRange::Range(start, end.unwrap_or(start))
        }
        "delete" => {
            let start = required_start(&ctx, start)?;
            reject_lines(&ctx, &lines)?;
            AnchorRange::Range(start, end.unwrap_or(start))
        }
        "insert_after" => {
            let start = required_start(&ctx, start)?;
            reject_end(&ctx, end)?;
            require_lines(&ctx, &lines)?;
            AnchorRange::After(start)
        }
        "insert_before" => {
            let start = required_start(&ctx, start)?;
            reject_end(&ctx, end)?;
            require_lines(&ctx, &lines)?;
            AnchorRange::Before(start)
        }
        "insert_head" => {
            reject_start(&ctx, start)?;
            reject_end(&ctx, end)?;
            require_lines(&ctx, &lines)?;
            AnchorRange::Head
        }
        "insert_tail" => {
            reject_start(&ctx, start)?;
            reject_end(&ctx, end)?;
            require_lines(&ctx, &lines)?;
            AnchorRange::Tail
        }
        _ => return Err(format!("{ctx}: unknown op")),
    };

    Ok(Op {
        anchor,
        payload: lines,
    })
}

fn required_start(ctx: &str, start: Option<usize>) -> Result<usize, String> {
    match start {
        Some(0) => Err(format!("{ctx}: `start` must be 1-based; 0 is invalid")),
        Some(n) => Ok(n),
        None => Err(format!("{ctx}: missing required `start`")),
    }
}

fn reject_start(ctx: &str, start: Option<usize>) -> Result<(), String> {
    if start.is_some() {
        return Err(format!("{ctx}: `start` is not valid for this op"));
    }
    Ok(())
}

fn reject_end(ctx: &str, end: Option<usize>) -> Result<(), String> {
    if end.is_some() {
        return Err(format!("{ctx}: `end` is only valid for replace/delete"));
    }
    Ok(())
}

fn require_lines(ctx: &str, lines: &[String]) -> Result<(), String> {
    if lines.is_empty() {
        return Err(format!("{ctx}: missing required `lines`"));
    }
    Ok(())
}

fn reject_lines(ctx: &str, lines: &[String]) -> Result<(), String> {
    if !lines.is_empty() {
        return Err(format!("{ctx}: `lines` is not valid for delete"));
    }
    Ok(())
}

fn validate_structured_lines(ctx: &str, lines: &[String]) -> Result<(), String> {
    if lines
        .iter()
        .any(|line| line.contains('\n') || line.contains('\r'))
    {
        return Err(format!(
            "{ctx}: each `lines` item must be one output line; split multi-line content into multiple array items"
        ));
    }
    Ok(())
}

/// Apply a section's ops to `lines`, returning the new file content.
///
/// Each op becomes a half-open splice `[start, end)` on the 0-based line vector.
/// Splices are applied high-index first so earlier line numbers stay valid as
/// later lines are rewritten; overlapping ranges are rejected.
fn apply_ops(lines: &[&str], ops: &[Op], trailing_newline: bool) -> Result<String, String> {
    let n = lines.len();
    // Resolve each op to (start, end, replacement, declaration_order).
    let mut splices: Vec<(usize, usize, Vec<String>, usize)> = Vec::with_capacity(ops.len());
    for (order, op) in ops.iter().enumerate() {
        let (start, end) = resolve_splice(op.anchor, n)?;
        splices.push((start, end, op.payload.clone(), order));
    }

    // Sort by start ascending (ties: declaration order) to check for overlap…
    splices.sort_by(|a, b| a.0.cmp(&b.0).then(a.3.cmp(&b.3)));
    let mut prev_end: Option<usize> = None;
    for (start, end, _, _) in &splices {
        if let Some(pe) = prev_end
            && *start < pe
        {
            return Err(format!(
                "overlapping edits touch line index {start} more than once"
            ));
        }
        // Zero-width inserts (start == end) don't advance the barrier past a
        // following real range at the same index.
        prev_end = Some((*end).max(prev_end.unwrap_or(0)));
    }

    // …then apply high-index first so indices below an edit are unaffected.
    splices.sort_by(|a, b| b.0.cmp(&a.0).then(b.3.cmp(&a.3)));
    let mut out: Vec<String> = lines.iter().map(|s| (*s).to_owned()).collect();
    for (start, end, replacement, _) in splices {
        out.splice(start..end, replacement);
    }

    let mut joined = out.join("\n");
    if trailing_newline && !joined.is_empty() {
        joined.push('\n');
    }
    Ok(joined)
}

/// Turn a 1-based anchor into a 0-based half-open `[start, end)` splice over a
/// file of `n` lines, bounds-checked.
fn resolve_splice(anchor: AnchorRange, n: usize) -> Result<(usize, usize), String> {
    match anchor {
        AnchorRange::Range(a, b) => {
            if a == 0 || b == 0 {
                return Err("line numbers are 1-based; 0 is invalid".to_owned());
            }
            if a > b {
                return Err(format!("inverted range {a}..{b}"));
            }
            if b > n {
                return Err(format!("range {a}..{b} past end of file ({n} lines)"));
            }
            Ok((a - 1, b))
        }
        AnchorRange::After(k) => {
            if k > n {
                return Err(format!("`insert after {k}` past end of file ({n} lines)"));
            }
            Ok((k, k))
        }
        AnchorRange::Before(k) => {
            if k == 0 || k > n.max(1) {
                return Err(format!("`insert before {k}` out of range (1..={n})"));
            }
            Ok((k - 1, k - 1))
        }
        AnchorRange::Head => Ok((0, 0)),
        AnchorRange::Tail => Ok((n, n)),
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

    /// Build a tool whose store already holds the current tag for `path`, as if
    /// `read` had just run — the precondition every real `edit` has.
    fn primed(dir: &std::path::Path, rel: &str, content: &str) -> EditTool {
        std::fs::write(dir.join(rel), content).unwrap();
        let store = SnapshotStore::new();
        store.record(&dir.join(rel), tag_of(content.as_bytes()));
        EditTool::new(dir.to_path_buf(), store)
    }

    fn call_edit(path: &str, content: &str, ops: serde_json::Value) -> ToolInput {
        let mut input = serde_json::Map::new();
        input.insert(
            "path".to_owned(),
            serde_json::Value::String(path.to_owned()),
        );
        input.insert("tag".to_owned(), serde_json::Value::String(tag(content)));
        input.insert("ops".to_owned(), ops);
        call_json(serde_json::Value::Object(input))
    }

    fn call_json(input: serde_json::Value) -> ToolInput {
        ToolInput {
            call_id: "c1".to_owned(),
            input,
            timeout: Duration::from_secs(5),
        }
    }

    fn tag(content: &str) -> String {
        tag_of(content.as_bytes())
    }

    #[tokio::test]
    async fn replace_range_rewrites_lines() {
        let dir = tempfile::tempdir().unwrap();
        let src = "a\nb\nc\n";
        let tool = primed(dir.path(), "f.txt", src);

        let out = tool
            .invoke(call_edit(
                "f.txt",
                src,
                serde_json::json!([{ "op": "replace", "start": 2, "end": 2, "lines": ["B"] }]),
            ))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nB\nc\n"
        );
    }

    #[tokio::test]
    async fn replace_range_preserves_multiline_payload() {
        let dir = tempfile::tempdir().unwrap();
        let src = "a\nb\nc\n";
        let tool = primed(dir.path(), "f.txt", src);

        let out = tool
            .invoke(call_edit(
                "f.txt",
                src,
                serde_json::json!([{ "op": "replace", "start": 2, "end": 2, "lines": ["B1", "B2"] }]),
            ))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nB1\nB2\nc\n"
        );
    }

    #[tokio::test]
    async fn structured_lines_reject_embedded_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let src = "a\nb\nc\n";
        let tool = primed(dir.path(), "f.txt", src);
        let input = serde_json::json!({
            "path": "f.txt",
            "tag": tag(src),
            "ops": [
                { "op": "replace", "start": 2, "lines": ["B1\nB2"] }
            ]
        });

        assert!(matches!(
            tool.invoke(call_json(input)).await,
            Err(ToolError::InvalidInput(msg)) if msg.contains("one output line")
        ));
    }

    #[tokio::test]
    async fn delete_range_removes_lines() {
        let dir = tempfile::tempdir().unwrap();
        let src = "a\nb\nc\n";
        let tool = primed(dir.path(), "f.txt", src);

        let out = tool
            .invoke(call_edit(
                "f.txt",
                src,
                serde_json::json!([{ "op": "delete", "start": 2, "end": 2 }]),
            ))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nc\n"
        );
    }

    #[tokio::test]
    async fn insert_after_and_before() {
        let dir = tempfile::tempdir().unwrap();
        let src = "a\nb\n";
        let tool = primed(dir.path(), "f.txt", src);

        let out = tool
            .invoke(call_edit(
                "f.txt",
                src,
                serde_json::json!([
                    { "op": "insert_after", "start": 1, "lines": ["A1"] },
                    { "op": "insert_before", "start": 1, "lines": ["B0"] }
                ]),
            ))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        // before-1 inserts at head, after-1 inserts following the original a.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "B0\na\nA1\nb\n"
        );
    }

    #[tokio::test]
    async fn insert_head_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let src = "x\n";
        let tool = primed(dir.path(), "f.txt", src);

        let out = tool
            .invoke(call_edit(
                "f.txt",
                src,
                serde_json::json!([
                    { "op": "insert_head", "lines": ["top"] },
                    { "op": "insert_tail", "lines": ["bottom"] }
                ]),
            ))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "top\nx\nbottom\n"
        );
    }

    /// Multiple ops in one section apply without line-number drift: the high
    /// line is rewritten first so the low line's number is still valid.
    #[tokio::test]
    async fn multi_op_no_drift() {
        let dir = tempfile::tempdir().unwrap();
        let src = "1\n2\n3\n4\n5\n";
        let tool = primed(dir.path(), "f.txt", src);

        let out = tool
            .invoke(call_edit(
                "f.txt",
                src,
                serde_json::json!([
                    { "op": "replace", "start": 1, "end": 1, "lines": ["one"] },
                    { "op": "replace", "start": 5, "end": 5, "lines": ["five"] }
                ]),
            ))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "one\n2\n3\n4\nfive\n"
        );
    }

    /// The core safety property: a file changed on disk after the read (tag no
    /// longer matches) is rejected with `stale_snapshot`, and left untouched.
    /// If verification were bypassed this would silently clobber the wrong line.
    #[tokio::test]
    async fn stale_snapshot_is_rejected_and_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let original = "a\nb\nc\n";
        let tool = primed(dir.path(), "f.txt", original);
        let input = call_edit(
            "f.txt",
            original,
            serde_json::json!([{ "op": "replace", "start": 2, "end": 2, "lines": ["B"] }]),
        );
        // The file changes out-of-band before edit runs.
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\n").unwrap();

        let out = tool.invoke(input).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("stale_snapshot"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nb\nc\nd\n",
            "edit must not modify a stale file"
        );
    }

    /// A file never `read` this session has no recorded snapshot, so even a tag
    /// that happens to match the disk is rejected — the model must read first.
    #[tokio::test]
    async fn unread_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let src = "a\n";
        std::fs::write(dir.path().join("f.txt"), src).unwrap();
        // Empty store: never read.
        let tool = EditTool::new(dir.path().to_path_buf(), SnapshotStore::new());

        let out = tool
            .invoke(call_edit(
                "f.txt",
                src,
                serde_json::json!([{ "op": "replace", "start": 1, "end": 1, "lines": ["A"] }]),
            ))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("stale_snapshot"));
    }

    /// A multi-file patch is all-or-nothing: if the second section is stale, the
    /// first file must not have been written.
    #[tokio::test]
    async fn multi_section_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        store.record(&dir.path().join("a.txt"), tag_of(b"a\n"));
        // b.txt deliberately NOT recorded -> its section will be stale.
        let tool = EditTool::new(dir.path().to_path_buf(), store);
        let input = serde_json::json!({
            "sections": [
                {
                    "path": "a.txt",
                    "tag": tag("a\n"),
                    "ops": [{ "op": "replace", "start": 1, "end": 1, "lines": ["A"] }]
                },
                {
                    "path": "b.txt",
                    "tag": tag("b\n"),
                    "ops": [{ "op": "replace", "start": 1, "end": 1, "lines": ["B"] }]
                }
            ]
        });

        let out = tool.invoke(call_json(input)).await.unwrap();
        assert!(out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "a\n",
            "first file must be untouched when a later section fails"
        );
    }

    #[tokio::test]
    async fn out_of_range_is_bad_range() {
        let dir = tempfile::tempdir().unwrap();
        let src = "a\nb\n";
        let tool = primed(dir.path(), "f.txt", src);

        let out = tool
            .invoke(call_edit(
                "f.txt",
                src,
                serde_json::json!([{ "op": "replace", "start": 5, "end": 5, "lines": ["x"] }]),
            ))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("bad_range"));
    }

    #[tokio::test]
    async fn input_field_is_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = primed(dir.path(), "f.txt", "a\n");
        assert!(matches!(
            tool.invoke(call_json(serde_json::json!({ "input": "not supported" })))
                .await,
            Err(ToolError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn escaping_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tool = primed(dir.path(), "f.txt", "a\n");
        let out = tool
            .invoke(call_json(serde_json::json!({
                "path": "../escape",
                "tag": "AAAA",
                "ops": [{ "op": "replace", "start": 1, "end": 1, "lines": ["x"] }]
            })))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("invalid_path"));
    }
}
