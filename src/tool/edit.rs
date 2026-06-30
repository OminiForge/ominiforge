//! The `edit` built-in tool: apply a line-anchored patch verified against the
//! per-session snapshot recorded by [`read`](super::ReadTool).
//!
//! A patch is one `input` string of one or more file sections. Each section
//! opens with a `[path#TAG]` header — the same header `read` prints — and is
//! followed by ops that name plain line numbers. Before touching a file the tool
//! checks the cited `TAG` against both the snapshot store and the file's current
//! bytes; a mismatch means the read was stale, so the patch is rejected rather
//! than applied to the wrong lines. See `doc/tool-protocol.md`.
//!
//! Grammar (variant `hashline`):
//! ```text
//! [src/app.rs#1F2A]
//! replace 12..14:
//! +    let x = 1;
//! delete 20..20
//! insert after 30:
//! +// trailing note
//! ```
//! Ops: `replace N..M:`, `delete N..M`, `insert after N:`, `insert before N:`,
//! `insert head:`, `insert tail:`. Payload lines are prefixed `+`.

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
struct EditArgs {
    input: String,
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
        ToolDescriptor {
            name: "edit".to_owned(),
            description: "Apply a line-anchored patch to one or more files. Each file \
                          section starts with the `[path#TAG]` header from a prior \
                          `read`; ops cite 1-based line numbers. Ops: `replace N..M:`, \
                          `delete N..M`, `insert after N:`, `insert before N:`, \
                          `insert head:`, `insert tail:`. Payload lines start with `+`. \
                          If a file changed since you read it the TAG no longer matches \
                          and the patch is rejected — re-`read` and try again."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "The patch: one or more `[path#TAG]` sections with ops."
                    }
                },
                "required": ["input"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, input: ToolInput) -> ToolResult {
        let args: EditArgs = serde_json::from_value(input.input)
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

        // Parse errors are protocol faults: the model sent something that is not
        // a patch at all, so it cannot be retried by reacting to is_error.
        let sections = parse_patch(&args.input).map_err(ToolError::InvalidInput)?;
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

/// Close the in-progress op (if any) into the current section.
fn flush_op(
    current: &mut Option<Section>,
    pending: &mut Option<(AnchorRange, Vec<String>)>,
) -> Result<(), String> {
    if let Some((anchor, payload)) = pending.take() {
        let section = current
            .as_mut()
            .ok_or_else(|| "op before any [path#TAG] header".to_owned())?;
        validate_payload(anchor, &payload)?;
        section.ops.push(Op { anchor, payload });
    }
    Ok(())
}

/// Parse the patch string into sections. Returns a protocol-error message string
/// on any malformed line.
fn parse_patch(input: &str) -> Result<Vec<Section>, String> {
    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<Section> = None;
    let mut pending: Option<(AnchorRange, Vec<String>)> = None;

    for raw in input.lines() {
        if let Some(header) = parse_header(raw) {
            flush_op(&mut current, &mut pending)?;
            if let Some(section) = current.take() {
                sections.push(section);
            }
            let (path, tag) = header;
            current = Some(Section {
                path,
                tag,
                ops: Vec::new(),
            });
            continue;
        }

        if let Some(payload) = raw.strip_prefix('+') {
            let (_, lines) = pending
                .as_mut()
                .ok_or_else(|| format!("payload line with no preceding op: {raw}"))?;
            lines.push(payload.to_owned());
            continue;
        }

        if let Some(anchor) = parse_op(raw)? {
            flush_op(&mut current, &mut pending)?;
            if current.is_none() {
                return Err("op before any [path#TAG] header".to_owned());
            }
            pending = Some((anchor, Vec::new()));
            continue;
        }

        if raw.trim().is_empty() {
            continue;
        }

        return Err(format!("unrecognized patch line: {raw}"));
    }

    flush_op(&mut current, &mut pending)?;
    if let Some(section) = current.take() {
        sections.push(section);
    }
    Ok(sections)
}

/// Parse a `[path#TAG]` header line. `None` if the line is not a header.
fn parse_header(line: &str) -> Option<(String, String)> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let (path, tag) = inner.rsplit_once('#')?;
    if path.is_empty() || tag.is_empty() {
        return None;
    }
    Some((path.to_owned(), tag.to_owned()))
}

/// Parse an op line into its anchor. `Ok(None)` if the line is not an op line at
/// all (so the caller can try other line kinds); `Err` if it looks like an op
/// but is malformed.
fn parse_op(line: &str) -> Result<Option<AnchorRange>, String> {
    let line = line.trim_end();
    if let Some(rest) = line.strip_prefix("replace ") {
        let spec = rest
            .strip_suffix(':')
            .ok_or_else(|| format!("`replace` needs a trailing `:` — {line}"))?;
        let (a, b) = parse_range(spec)?;
        return Ok(Some(AnchorRange::Range(a, b)));
    }
    if let Some(rest) = line.strip_prefix("delete ") {
        let (a, b) = parse_range(rest)?;
        return Ok(Some(AnchorRange::Range(a, b)));
    }
    if let Some(rest) = line.strip_prefix("insert ") {
        let rest = rest.trim();
        if rest == "head:" || rest == "head" {
            return Ok(Some(AnchorRange::Head));
        }
        if rest == "tail:" || rest == "tail" {
            return Ok(Some(AnchorRange::Tail));
        }
        if let Some(n) = rest.strip_prefix("after ") {
            let n = parse_line_no(n.trim_end_matches(':'))?;
            return Ok(Some(AnchorRange::After(n)));
        }
        if let Some(n) = rest.strip_prefix("before ") {
            let n = parse_line_no(n.trim_end_matches(':'))?;
            return Ok(Some(AnchorRange::Before(n)));
        }
        return Err(format!("unknown insert form: {line}"));
    }
    Ok(None)
}

/// Parse an inclusive `N..M` range (or a bare `N` as `N..N`).
fn parse_range(spec: &str) -> Result<(usize, usize), String> {
    let spec = spec.trim();
    if let Some((a, b)) = spec.split_once("..") {
        Ok((parse_line_no(a)?, parse_line_no(b)?))
    } else {
        let n = parse_line_no(spec)?;
        Ok((n, n))
    }
}

fn parse_line_no(s: &str) -> Result<usize, String> {
    s.trim()
        .parse::<usize>()
        .map_err(|_| format!("invalid line number: {s:?}"))
}

/// Reject payloads that cannot belong to their op: insert ops require at least
/// one `+` line, otherwise the op is a no-op and almost certainly a mistake. A
/// `Range` with no payload is a valid delete; with payload, a valid replace.
fn validate_payload(anchor: AnchorRange, payload: &[String]) -> Result<(), String> {
    match anchor {
        AnchorRange::Range(..) => Ok(()),
        _ if payload.is_empty() => Err("insert op has no `+` payload lines".to_owned()),
        _ => Ok(()),
    }
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

    fn call(patch: &str) -> ToolInput {
        ToolInput {
            call_id: "c1".to_owned(),
            input: serde_json::json!({ "input": patch }),
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
        let patch = format!("[f.txt#{}]\nreplace 2..2:\n+B", tag(src));

        let out = tool.invoke(call(&patch)).await.unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nB\nc\n"
        );
    }

    #[tokio::test]
    async fn delete_range_removes_lines() {
        let dir = tempfile::tempdir().unwrap();
        let src = "a\nb\nc\n";
        let tool = primed(dir.path(), "f.txt", src);
        let patch = format!("[f.txt#{}]\ndelete 2..2", tag(src));

        let out = tool.invoke(call(&patch)).await.unwrap();
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
        let patch = format!(
            "[f.txt#{}]\ninsert after 1:\n+A1\ninsert before 1:\n+B0",
            tag(src)
        );

        let out = tool.invoke(call(&patch)).await.unwrap();
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
        let patch = format!(
            "[f.txt#{}]\ninsert head:\n+top\ninsert tail:\n+bottom",
            tag(src)
        );

        let out = tool.invoke(call(&patch)).await.unwrap();
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
        let patch = format!(
            "[f.txt#{}]\nreplace 1..1:\n+one\nreplace 5..5:\n+five",
            tag(src)
        );

        let out = tool.invoke(call(&patch)).await.unwrap();
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
        // Patch cites the original tag…
        let patch = format!("[f.txt#{}]\nreplace 2..2:\n+B", tag(original));
        // …but the file changes out-of-band before edit runs.
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\n").unwrap();

        let out = tool.invoke(call(&patch)).await.unwrap();
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
        let patch = format!("[f.txt#{}]\nreplace 1..1:\n+A", tag(src));

        let out = tool.invoke(call(&patch)).await.unwrap();
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
        let patch = format!(
            "[a.txt#{}]\nreplace 1..1:\n+A\n[b.txt#{}]\nreplace 1..1:\n+B",
            tag("a\n"),
            tag("b\n")
        );

        let out = tool.invoke(call(&patch)).await.unwrap();
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
        let patch = format!("[f.txt#{}]\nreplace 5..5:\n+x", tag(src));

        let out = tool.invoke(call(&patch)).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("bad_range"));
    }

    #[tokio::test]
    async fn malformed_patch_is_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = primed(dir.path(), "f.txt", "a\n");
        assert!(matches!(
            tool.invoke(call("not a patch at all")).await,
            Err(ToolError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn escaping_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tool = primed(dir.path(), "f.txt", "a\n");
        // A header that escapes the workspace resolves to a path error surfaced
        // as a business error (per-section), not a panic.
        let patch = "[../escape#AAAA]\nreplace 1..1:\n+x";
        let out = tool.invoke(call(patch)).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("invalid_path"));
    }
}
