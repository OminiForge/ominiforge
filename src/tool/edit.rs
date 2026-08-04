//! The `edit` built-in tool: apply content-anchored replacements to files.
//!
//! Input is structured JSON: `edits: [{ path, old, new, replace_all? }]`. Each
//! entry's `old` is located as a contiguous run of lines in the target file
//! (not by line number) and spliced out in favor of `new`. Not knowing the
//! exact current content of the lines you want to touch is the failure mode —
//! there is no separate "read this file first" bookkeeping to bypass, and no
//! whole-file fingerprint to go stale: the model must quote real content, and
//! a `not_found`/`ambiguous` result means it didn't. Line arrays are
//! normalized (embedded newlines split) before matching, so a pasted
//! multi-line block in one array item works the same as one-item-per-line.
//! See `doc/tool-protocol.md` §11.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use super::{
    Tool, ToolDescriptor, ToolError, ToolInput, ToolResult, append_diagnostics,
    resolve_in_workspace,
};
use crate::core::payload::{Content, ToolOutput};
use crate::format::FormatManager;
use crate::lsp::LspManager;

/// Applies content-anchored edits relative to the session workspace.
#[derive(Clone)]
pub struct EditTool {
    workspace: PathBuf,
    /// Optional LSP assist: when set, a successful edit appends each written
    /// file's diagnostics to the result (`doc/lsp.md`).
    lsp: Option<Arc<LspManager>>,
    /// Optional auto-format: when set, the written file is formatted before
    /// the diff/diagnostics are produced (`doc/format.md`).
    format: Option<Arc<FormatManager>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    /// A model wrapping a single replacement as a bare object instead of a
    /// one-element array is unambiguous — normalize it (`doc/tool-protocol.md`
    /// §11.2). Untagged representation tries the canonical form first, so
    /// well-formed calls parse exactly as before.
    #[serde(deserialize_with = "one_or_many")]
    edits: Vec<EditEntryArg>,
}

/// Accept either `edits: [ {...}, ... ]` or a single `edits: {...}`.
fn one_or_many<'de, D>(deserializer: D) -> Result<Vec<EditEntryArg>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Many(Vec<EditEntryArg>),
        One(Box<EditEntryArg>),
    }
    match OneOrMany::deserialize(deserializer)? {
        OneOrMany::Many(v) => Ok(v),
        OneOrMany::One(e) => Ok(vec![*e]),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditEntryArg {
    path: String,
    /// The canonical form is an array with one file line per item; a single
    /// string is accepted and split on newlines (same normalization as
    /// `split_lines` applies to multi-line items).
    #[serde(deserialize_with = "string_or_lines")]
    old: Vec<String>,
    #[serde(deserialize_with = "string_or_lines")]
    new: Vec<String>,
    #[serde(default)]
    replace_all: bool,
}

/// Accept either `"old": ["line1", ...]` or a single `"old": "line1\nline2"`.
fn string_or_lines<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrLines {
        Lines(Vec<String>),
        One(String),
    }
    match StringOrLines::deserialize(deserializer)? {
        StringOrLines::Lines(v) => Ok(v),
        StringOrLines::One(s) => Ok(vec![s]),
    }
}

impl EditTool {
    /// Create an `edit` tool rooted at `workspace`.
    #[must_use]
    pub const fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            lsp: None,
            format: None,
        }
    }

    /// Attach an [`LspManager`] so successful edits carry diagnostics.
    #[must_use]
    pub fn with_lsp(mut self, lsp: Option<Arc<LspManager>>) -> Self {
        self.lsp = lsp;
        self
    }

    /// Attach a [`FormatManager`] so successful edits are formatted before
    /// their diff/diagnostics are produced (`doc/format.md`).
    #[must_use]
    pub fn with_format(mut self, format: Option<Arc<FormatManager>>) -> Self {
        self.format = format;
        self
    }
}

fn edit_input_schema() -> serde_json::Value {
    let entry_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File path relative to the workspace root. Emit this \
                                field FIRST (before `old`/`new`) so streaming can \
                                render the file immediately."
            },
            "old": {
                "type": "array",
                "items": { "type": "string" },
                "minItems": 1,
                "description": "The exact current lines to replace, one file line per array item. Quote this verbatim from a FRESH `read` — paraphrasing a long line from memory almost always diffs by one character and is rejected rather than guessed at (a `not_found` result names the first differing line so you can repair the quote). A single string is accepted and split on newlines; an array item containing a newline is likewise split, but one-per-item stays the canonical form."
            },
            "new": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Replacement lines, one per array item. Empty array deletes `old` outright. To insert, include an unchanged anchor line in both `old` and `new` alongside the inserted lines. (Newlines inside an item are split as for `old`.)"
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
                "description": "One or more content-anchored replacements, applied atomically: if any entry fails to resolve, no file is changed. Entries may target the same path (applied together, non-overlapping) or different paths. A bare single object is accepted and wrapped as a one-element array. To rewrite most of a file, use `write` instead."
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
                          no prior-read tag. Give `edits: [{ path, old, new, replace_all? }]`, \
                          emitting each entry's `path` FIRST (streaming renders the file \
                          as soon as `path` arrives): \
                          `old` is the exact current content you're replacing (quoted \
                          verbatim from a `read`/`edit`/`write` output, one file line per \
                          array item), `new` is what it becomes (empty array deletes; an \
                          insert keeps an unchanged anchor line in both `old` and `new` \
                          alongside the new lines). Quote `old` from a FRESH `read` — \
                          paraphrasing or recalling a long line from memory almost always \
                          diffs by one character and is rejected rather than guessed at. \
                          `old` must match exactly one place in the file unless \
                          `replace_all` is set, in which case every \
                          non-overlapping occurrence is replaced. A patch is atomic across \
                          all entries (same or different files): if any entry doesn't \
                          resolve, nothing is written. On success the result reports how \
                          many replacements were made per file, not a diff — you already \
                          have the before/after text in this call's own arguments. \
                          Rewriting most of a file, or replacing very long lines you \
                          cannot quote exactly? Use `write` with the full new content \
                          instead — do not fight `edit` over it."
                .to_owned(),
            input_schema: edit_input_schema(),
        }
    }

    /// Stage-2 streaming (`doc/tool-streaming.md`): a per-call presenter that
    /// renders the entry the model is currently writing — the file read once
    /// `path` closes, the anchor located once `old` closes, and the
    /// replacement grown as `new` streams in.
    fn stream_presenter(&self) -> Option<Box<dyn super::StreamPresenter>> {
        Some(Box::new(super::edit_stream::EditStreamPresenter::new(
            self.workspace.clone(),
        )))
    }

    async fn invoke(&self, input: ToolInput) -> ToolResult {
        let planned = match self.plan_all(input.input).await {
            Ok(planned) => planned,
            Err(PlanErr::Protocol(e)) => return Err(e),
            Err(PlanErr::Business(out)) => return Ok(out),
        };

        // Auto-format each planned file BEFORE it lands (`doc/format.md` §6):
        // the formatter consumes the model's target text in memory, the FINAL
        // text is written once, and the diff/diagnostics below are anchored to
        // it — so the model's next edit anchors to the real, formatted state
        // and never hits a `not_found` from formatting drift. Fail-closed: a
        // skip keeps the model's text. `written` remembers (abs path, display
        // name, final text, formatter) for the view + diagnostics.
        let mut written: Vec<WrittenFile> = Vec::with_capacity(planned.len());
        for plan in planned {
            let outcome = match &self.format {
                Some(fmt) => {
                    fmt.format(&plan.abs_path, &plan.new_content, plan.edited_lines)
                        .await
                }
                None => crate::format::FormatOutcome::Skipped {
                    text: plan.new_content.clone(),
                },
            };
            // Record the formatter only when it actually changed the text —
            // an already-formatted file carries no annotation. The adjustment
            // count is the formatter's OWN change regions (model text →
            // formatted text), so the annotation attributes only its edits.
            let formatter = match &outcome {
                crate::format::FormatOutcome::Formatted { formatter, text }
                    if *text != plan.new_content =>
                {
                    Some((
                        formatter.clone(),
                        super::diffview::change_region_count(&plan.new_content, text),
                    ))
                }
                _ => None,
            };
            let final_text = outcome.into_text();
            if let Err(e) = tokio::fs::write(&plan.abs_path, final_text.as_bytes()).await {
                return Ok(business_error(
                    "write_failed",
                    &format!("failed to write {}: {e}", plan.rel_path),
                ));
            }
            written.push(WrittenFile {
                abs_path: plan.abs_path,
                rel_path: plan.rel_path,
                old_content: plan.old_content,
                final_text,
                replacement_count: plan.replacement_count,
                formatter,
            });
        }

        let mut summaries = Vec::with_capacity(written.len());
        for w in &written {
            summaries.push(format!(
                "edited {} ({} replacement{})",
                w.rel_path,
                w.replacement_count,
                if w.replacement_count == 1 { "" } else { "s" }
            ));
        }

        let mut output = ToolOutput {
            content: vec![Content::Text(summaries.join("\n"))],
            is_error: false,
            error_code: None,
        };
        // The UI diff view rides as a `TextView` block after the model-facing
        // summary: rendered by the front-end, skipped by `render_output`, so
        // the model never pays tokens for a diff of its own arguments
        // (`doc/tool-view.md`). The diff is `old_content → final_text` — when
        // a formatter ran it includes the formatter's reflow, annotated with
        // `formatted_by` so the reader knows part of the change is not the
        // model's (`doc/format.md` §6).
        let view_text = written_view(&written);
        if !view_text.is_empty() {
            output.content.push(Content::TextView {
                text: view_text,
                audience: crate::core::payload::AUDIENCE_UI.to_owned(),
            });
        }
        for w in &written {
            append_diagnostics(
                self.lsp.as_ref(),
                &mut output,
                &w.abs_path,
                &w.rel_path,
                &w.final_text,
            )
            .await;
        }
        Ok(output)
    }
}

/// One file that landed on disk: everything the diff view and diagnostics
/// need, anchored to the FINAL (possibly formatted) text.
struct WrittenFile {
    abs_path: PathBuf,
    rel_path: String,
    old_content: String,
    final_text: String,
    replacement_count: usize,
    /// The formatter that changed the text plus how many change regions it
    /// made (drives `formatted_by` / the "N 处调整" annotation).
    formatter: Option<(String, usize)>,
}

/// Render the written files' diff views into a JSON envelope
/// `{ kind: "diff", files: [{ path, patch, formatted_by? }] }`. The diff is
/// `old_content → final_text` (`doc/format.md` §6): when a formatter changed
/// the text, the diff includes its reflow and the file entry carries a
/// `formatted_by` annotation so the reader knows part of the change is not
/// the model's. `render_hunks`'s splice-anchored render can't be used here
/// because the formatter's edits aren't among the model's splices — so this
/// runs a real line-level diff (`similar`, same as `write`).
fn written_view(written: &[WrittenFile]) -> String {
    let mut files: Vec<serde_json::Value> = Vec::new();
    for w in written {
        if w.old_content == w.final_text {
            continue; // no change — no diff block
        }
        let patch = super::diffview::write_diff(
            &w.old_content,
            &w.final_text,
            super::diffview::default_context(),
        );
        if patch.is_empty() {
            continue;
        }
        let mut entry = serde_json::json!({
            "path": w.rel_path,
            "patch": patch,
        });
        if let Some((formatter, adjustments)) = &w.formatter {
            entry["formatted_by"] = serde_json::Value::String(formatter.clone());
            entry["format_adjustments"] = serde_json::json!(adjustments);
        }
        files.push(entry);
    }
    if files.is_empty() {
        return String::new();
    }
    serde_json::json!({
        "kind": "diff",
        "files": files,
    })
    .to_string()
}

/// The two failure channels of [`EditTool::plan_all`]: a protocol error means
/// the input itself was malformed (surfaced as `Err(ToolError)`); a business
/// error means the input was well-formed but didn't match the file (surfaced
/// as an `is_error` `ToolOutput` the model reacts to).
enum PlanErr {
    Protocol(ToolError),
    Business(ToolOutput),
}

impl EditTool {
    /// Parse, validate, group and plan every entry — everything short of
    /// writing. Shared by `invoke` (which then writes) and `preview` (which
    /// only renders the would-be diff). Malformed input (bad JSON, empty
    /// `edits`, an invalid entry) is a PROTOCOL error; a content failure
    /// (no match, ambiguous, overlapping, invalid path) is a BUSINESS error
    /// the model reacts to.
    async fn plan_all(&self, input: serde_json::Value) -> Result<Vec<PlannedWrite>, PlanErr> {
        let args: EditArgs = serde_json::from_value(input)
            .map_err(|e| PlanErr::Protocol(ToolError::InvalidInput(e.to_string())))?;
        if args.edits.is_empty() {
            return Err(PlanErr::Protocol(ToolError::InvalidInput(
                "empty `edits`".to_owned(),
            )));
        }
        let entries = validate_entries(args.edits)
            .map_err(|e| PlanErr::Protocol(ToolError::InvalidInput(e)))?;

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
                Err(e) => {
                    return Err(PlanErr::Business(business_error(
                        "invalid_path",
                        &e.to_string(),
                    )));
                }
            };
            match groups
                .iter_mut()
                .find(|(_, group_abs, _)| *group_abs == abs_path)
            {
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
                Err(business) => return Err(PlanErr::Business(business)),
            }
        }
        Ok(planned)
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
    /// The file's content before the edit — the diff's "before" side and the
    /// base for re-rendering after auto-format (`doc/format.md` §6).
    old_content: String,
    new_content: String,
    replacement_count: usize,
    /// The 1-based inclusive line range the edits touched, for `mode = "edit"`
    /// formatting. `None` when nothing actually changed.
    edited_lines: Option<(u32, u32)>,
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
                        &not_found_message(rel_path, &lines, entry),
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

        // The 1-based inclusive line range the edits touch, in the NEW
        // content's coordinates (for `mode = "edit"` formatting). Computed
        // from the splices BEFORE they are consumed below.
        let edited_lines = edited_line_range(&splices);

        // Apply high-index first so earlier splices' indices stay valid.
        let mut out: Vec<String> = lines.iter().map(|s| (*s).to_owned()).collect();
        for (start, end, entry) in splices.into_iter().rev() {
            out.splice(start..end, entry.new.iter().cloned());
        }

        let mut new_content = out.join("\n");
        if trailing_newline && !new_content.is_empty() {
            new_content.push('\n');
        }

        // `edited_lines` is meaningful only when the edit actually changed
        // something — an identical old/new leaves nothing to format locally.
        let changed = new_content != content;

        Ok(PlannedWrite {
            abs_path: abs_path.to_path_buf(),
            rel_path: rel_path.to_owned(),
            old_content: content,
            new_content,
            replacement_count,
            edited_lines: if changed { edited_lines } else { None },
        })
    }
}

/// The 1-based inclusive line range a set of splices touches, in the NEW
/// content's coordinates (for `mode = "edit"` formatting, `doc/format.md`
/// §5). Walks the sorted splices tracking the cumulative line shift from old
/// to new coordinates. Coordinates stay in `isize` (a pure deletion can pull
/// a later splice's new-start below its old one) and convert to `u32` once;
/// line counts here are file lengths, far below any wrap concern.
#[allow(clippy::cast_possible_wrap)]
fn edited_line_range(splices: &[(usize, usize, &Entry)]) -> Option<(u32, u32)> {
    if splices.is_empty() {
        return None;
    }
    let mut first_new_start = isize::MAX;
    let mut last_new_end = 0isize;
    let mut shift = 0isize;
    for (start, end, entry) in splices {
        let new_start = *start as isize + shift;
        first_new_start = first_new_start.min(new_start);
        last_new_end = last_new_end.max(new_start + entry.new.len() as isize);
        shift += entry.new.len() as isize - (*end - *start) as isize;
    }
    // 1-based inclusive [start, end]; a pure insertion has new.len() lines, a
    // pure deletion touches the line it collapsed into.
    let to_u32 = |v: isize| u32::try_from(v).unwrap_or(u32::MAX);
    let start_line = to_u32(first_new_start + 1);
    let end_line = to_u32(last_new_end.max(first_new_start + 1));
    Some((start_line, end_line))
}

/// Every start index (0-based) at which `needle` occurs as a contiguous run in
/// `haystack`, scanning left to right and skipping past a match so overlapping
/// occurrences are not double-counted under `replace_all`.
pub fn find_matches(haystack: &[&str], needle: &[String]) -> Vec<usize> {
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

/// Build the `not_found` message with a located diagnosis that mirrors
/// `find_matches`' contiguous-block semantics: from the leftmost start with
/// the longest verbatim prefix of `old`, report the first line where the
/// block breaks, `old` vs file side by side, plus the first differing
/// character. Turns "no match" into "line N differs from file line M like
/// this", so a one-character misquote costs one retry, not a full re-read.
fn not_found_message(rel_path: &str, lines: &[&str], entry: &Entry) -> String {
    let header = format!(
        "{rel_path}: no match for the given `old` lines (entry {})",
        entry.ordinal
    );

    // From every candidate start line, how many leading lines of `old` match
    // verbatim? Keep the leftmost attempt with the longest prefix. (The
    // greedy "earliest match per line" walk this replaced could anchor on a
    // scattered match and blame the wrong `old` line.) The Z algorithm gives
    // every start's prefix length in O(M+N) instead of a naive O(MN) — same
    // matched-region reuse idea as KMP, but Z answers "prefix length at each
    // start" where KMP answers "where full matches end".
    let prefix_lens = z_prefix_lengths(&entry.old, lines);
    // Empty `lines` yields no start; (0, 0) still reports `old` line #1.
    let (best_start, best_len) = prefix_lens
        .iter()
        .enumerate()
        .max_by_key(|&(_, &len)| len)
        .map_or((0, 0), |(start, &len)| (start, len));

    let breaking_old = &entry.old[best_len];
    let mut msg = format!(
        "{header}\nfirst unmatched `old` line is #{}: {breaking_old:?}",
        best_len + 1
    );
    let break_file_idx = best_start + best_len;
    if break_file_idx < lines.len() {
        // The file has a line exactly where the block breaks — that is the
        // closest look-alike by construction, no fuzzy search needed.
        use std::fmt::Write;
        let _ = write!(
            msg,
            "\nclosest file line is #{}: {:?}{}",
            break_file_idx + 1,
            lines[break_file_idx],
            char_hint(breaking_old, lines[break_file_idx])
        );
    } else {
        msg.push_str("\nthe file ends there; the quoted `old` has extra lines");
    }
    msg
}

/// For every start line in `text`, the number of leading `pattern` lines
/// that match verbatim — via the Z algorithm on `pattern ++ [sentinel] ++
/// text`, so `z[pattern.len() + 1 + start]` is exactly the prefix length at
/// `start`. O(M+N) line comparisons, all inside the Z-box when possible.
/// Only invoked on the not-found path, so `pattern` never fully matches.
fn z_prefix_lengths(pattern: &[String], text: &[&str]) -> Vec<usize> {
    let total = pattern.len() + 1 + text.len();
    // None is the sentinel: pattern/text lines are all Some, so the sentinel
    // can never match one, keeping z[i] from overrunning a segment.
    let line_at = |i: usize| -> Option<&str> {
        match i.cmp(&pattern.len()) {
            std::cmp::Ordering::Less => Some(&pattern[i]),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(text[i - pattern.len() - 1]),
        }
    };
    let mut z = vec![0usize; total];
    let (mut l, mut r) = (0usize, 0usize); // rightmost Z-box [l, r)
    for i in 1..total {
        if i < r {
            z[i] = (r - i).min(z[i - l]);
        }
        while i + z[i] < total && line_at(z[i]) == line_at(i + z[i]) {
            z[i] += 1;
        }
        if i + z[i] > r {
            l = i;
            r = i + z[i];
        }
    }
    text.iter()
        .enumerate()
        .map(|(start, _)| z[pattern.len() + 1 + start].min(pattern.len()))
        .collect()
}

/// Pinpoint the first differing character between the quoted line and the
/// candidate file line, when they share a prefix. Empty when no useful hint
/// (identical prefix length 0, or one is a prefix of the other without
/// extra context worth citing).
fn char_hint(old_line: &str, file_line: &str) -> String {
    let mut old_chars = old_line.chars();
    let mut file_chars = file_line.chars();
    let mut col = 0usize;
    loop {
        match (old_chars.next(), file_chars.next()) {
            (Some(a), Some(b)) if a == b => col += 1,
            (a, b) => {
                use std::fmt::Write;
                let mut hint = format!("\nfirst difference at char {}", col + 1);
                match (a, b) {
                    (Some(x), Some(y)) => {
                        let _ = write!(hint, ": `old` has {x:?}, file has {y:?}");
                    }
                    (Some(x), None) => {
                        let _ = write!(hint, ": `old` continues with {x:?}, file line ends here");
                    }
                    (None, Some(y)) => {
                        let _ = write!(hint, ": `old` line ends here, file continues with {y:?}");
                    }
                    (None, None) => return String::new(),
                }
                return hint;
            }
        }
    }
}

/// Validate and normalize the parsed entries: non-empty `path`, non-empty
/// `old` after normalization. Normalization SPLITS any array item containing
/// an embedded newline into one item per line — a model frequently pastes a
/// whole multi-line block as a single `old`/`new` element, and rejecting that
/// taught callers to route around the tool instead of fixing the call
/// (`doc/lsp.md`-era lesson: a tool people bypass is worse than a tolerant
/// one). The canonical form stays "one file line per array item"; the split
/// just stops a malformed-but-unambiguous call from failing. Only structural
/// problems remain protocol errors.
fn validate_entries(edits: Vec<EditEntryArg>) -> Result<Vec<Entry>, String> {
    edits
        .into_iter()
        .enumerate()
        .map(|(idx, e)| {
            let ctx = format!("edits[{idx}]");
            if e.path.is_empty() {
                return Err(format!("{ctx}: empty `path`"));
            }
            let old = split_lines(e.old);
            let new = split_lines(e.new);
            if old.is_empty() {
                return Err(format!(
                    "{ctx} ({}): empty `old` — edit requires existing content to replace; use `write` to create a new file",
                    e.path
                ));
            }
            Ok(Entry {
                path: e.path,
                ordinal: idx + 1,
                old,
                new,
                replace_all: e.replace_all,
            })
        })
        .collect()
}

/// Flatten an `old`/`new` array to one element per line: items containing
/// `\n` are split, and a trailing `\r` on each resulting line is stripped so
/// CRLF-pasted content matches the file's lines (which `str::lines` already
/// yields without a `\r`). Blank pieces are preserved — a blank line is real
/// content; only the array itself being empty is meaningful. A multi-line
/// block pasted as one item is split for you, but one-item-per-line stays
/// the canonical form this module's docs and schema describe.
fn split_lines(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .flat_map(|item| {
            let mut pieces: Vec<&str> = item.split('\n').collect();
            // An item ENDING in a newline ("a\nb\n") splits into a trailing
            // empty piece that is an artifact of the terminator, not a real
            // blank line — drop it. (A blank line in the MIDDLE is real
            // content and is preserved.)
            if pieces.last() == Some(&"") {
                pieces.pop();
            }
            pieces
                .into_iter()
                .map(|piece| piece.strip_suffix('\r').unwrap_or(piece).to_owned())
                .collect::<Vec<_>>()
        })
        .collect()
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
            progress: None,
        }
    }

    fn text(out: &ToolOutput) -> String {
        match &out.content[0] {
            Content::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// The `TextView` block of a successful edit, if it produced one.
    fn view(out: &ToolOutput) -> Option<&str> {
        out.content.iter().find_map(|c| match c {
            Content::TextView { text, audience } if audience == "ui" => Some(text.as_str()),
            _ => None,
        })
    }

    /// A single-line replacement yields the model-facing summary PLUS a
    /// `TextView` with the exact unified diff (headers + hunk), and the view
    /// never leaks into the model-facing `Text` (`doc/tool-view.md` §2–§3).
    #[tokio::test]
    async fn successful_edit_carries_a_ui_diff_view() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("f.txt"),
            "a
b
c
d
e
",
        )
        .unwrap();
        let out = tool(dir.path().to_path_buf())
            .invoke(call(
                serde_json::json!([{ "path": "f.txt", "old": ["c"], "new": ["C"] }]),
            ))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(text(&out), "edited f.txt (1 replacement)");
        // The view is a JSON envelope `{ kind: "diff", files: [{ path, patch }] }`.
        let view_json: serde_json::Value = serde_json::from_str(view(&out).unwrap()).unwrap();
        assert_eq!(view_json["kind"], "diff");
        assert_eq!(view_json["files"][0]["path"], "f.txt");
        assert_eq!(
            view_json["files"][0]["patch"].as_str().unwrap(),
            "@@ -1,5 +1,5 @@\n a\n b\n-c\n+C\n d\n e"
        );
    }

    /// A business failure (no match) carries only the error text — no view
    /// (the error brief is the whole story; the debug fold shows it).
    #[tokio::test]
    async fn failed_edit_has_no_view() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("f.txt"),
            "a
b
",
        )
        .unwrap();
        let out = tool(dir.path().to_path_buf())
            .invoke(call(
                serde_json::json!([{ "path": "f.txt", "old": ["zzz"], "new": ["Z"] }]),
            ))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(view(&out).is_none());
    }

    /// An identical old→new (a no-op edit) produces no view block: emitting
    /// an empty diff would claim a change where none happened.
    #[tokio::test]
    async fn noop_edit_has_no_view() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("f.txt"),
            "a
b
",
        )
        .unwrap();
        let out = tool(dir.path().to_path_buf())
            .invoke(call(
                serde_json::json!([{ "path": "f.txt", "old": ["b"], "new": ["b"] }]),
            ))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(view(&out).is_none());
    }

    // --- auto-format integration (`doc/format.md`) --------------------------

    /// A `FormatManager` whose only formatter strips trailing whitespace via
    /// `sed` — a deterministic *whitespace-only* change, so it passes the
    /// fail-closed consistency check (non-whitespace content is unchanged).
    fn fmt_manager() -> std::sync::Arc<crate::format::FormatManager> {
        let config = crate::format::FormatConfig {
            mode: Some(crate::format::FormatMode::File),
            formatters: vec![crate::format::FormatterConfig {
                name: "trim-ws".to_owned(),
                command: "sed".to_owned(),
                args: vec!["s/[[:space:]]*$//".to_owned()],
                env: std::collections::HashMap::new(),
                extensions: vec!["txt".to_owned()],
                enabled: true,
                supports_line_range: false,
                format_timeout_ms: 5_000,
            }],
        };
        crate::format::FormatManager::new(config, std::collections::BTreeMap::new()).unwrap()
    }

    /// When the formatter changes the text, the edit writes the FORMATTED
    /// text and the diff view reflects it (annotated `formatted_by`), so the
    /// model's next edit anchors to the real on-disk state (`doc/format.md`
    /// §2/§6). The model's `new` here carries trailing whitespace; the
    /// formatter strips it, and the diff shows the stripped line.
    #[tokio::test]
    async fn formatted_edit_diff_anchors_to_formatted_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let t = EditTool::new(dir.path().to_path_buf()).with_format(Some(fmt_manager()));

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["b"], "new": ["B   "] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        // The on-disk text is formatted (trailing whitespace stripped).
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nB\nc\n"
        );
        // The diff shows the formatted line (`+B`, not `+B   `) and names the
        // formatter.
        let view_json: serde_json::Value = serde_json::from_str(view(&out).unwrap()).unwrap();
        assert_eq!(view_json["files"][0]["formatted_by"], "trim-ws");
        // The formatter stripped trailing whitespace on one line — one change
        // region, attributed to it (not to the model).
        assert_eq!(view_json["files"][0]["format_adjustments"], 1);
        let patch = view_json["files"][0]["patch"].as_str().unwrap();
        assert!(
            patch.contains("\n+B\n"),
            "patch should show stripped +B: {patch}"
        );
        assert!(
            !patch.contains("+B   "),
            "patch must not show the pre-format text"
        );
    }

    /// A file the formatter leaves unchanged carries no `formatted_by`
    /// annotation — an already-clean file must not claim it was reformatted.
    #[tokio::test]
    async fn unchanged_format_has_no_annotation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let t = EditTool::new(dir.path().to_path_buf()).with_format(Some(fmt_manager()));

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["b"], "new": ["B"] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        let view_json: serde_json::Value = serde_json::from_str(view(&out).unwrap()).unwrap();
        assert!(view_json["files"][0].get("formatted_by").is_none());
    }

    /// Fail-closed end to end: a formatter whose binary is missing skips
    /// formatting, so the edit writes the model's ORIGINAL text (trailing
    /// whitespace and all) and no annotation appears — never a broken result.
    #[tokio::test]
    async fn missing_formatter_keeps_original_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let config = crate::format::FormatConfig {
            mode: Some(crate::format::FormatMode::File),
            formatters: vec![crate::format::FormatterConfig {
                name: "gone".to_owned(),
                command: "definitely-not-a-real-formatter-xyz".to_owned(),
                args: vec![],
                env: std::collections::HashMap::new(),
                extensions: vec!["txt".to_owned()],
                enabled: true,
                supports_line_range: false,
                format_timeout_ms: 5_000,
            }],
        };
        let fmt =
            crate::format::FormatManager::new(config, std::collections::BTreeMap::new()).unwrap();
        let t = EditTool::new(dir.path().to_path_buf()).with_format(Some(fmt));

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["b"], "new": ["B   "] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        // Original (unstripped) text is on disk; no annotation.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nB   \nc\n"
        );
        let view_json: serde_json::Value = serde_json::from_str(view(&out).unwrap()).unwrap();
        assert!(view_json["files"][0].get("formatted_by").is_none());
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

    /// A multi-line block pasted as ONE `old`/`new` array item (the most
    /// common malformed call) is normalized by splitting on newlines, so the
    /// edit applies exactly as if the caller had passed one item per line.
    #[tokio::test]
    async fn embedded_newlines_in_items_are_split_not_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["a\nb"], "new": ["x\ny\nz"] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "x\ny\nz\nc\n"
        );
    }

    /// CRLF-pasted content: a trailing `\r` per split line is stripped, so a
    /// block copied from a Windows-style source still matches the file's
    /// lines (which `str::lines` yields without `\r`).
    #[tokio::test]
    async fn crlf_pasted_block_matches_lf_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["a\r\nb\r\n"], "new": ["X"] }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "X\n"
        );
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

    // --- tolerant parsing (schema-canonical forms still win) ---------------

    /// A bare single object for `edits` (not wrapped in an array) is
    /// unambiguous — normalize it instead of failing the call.
    #[tokio::test]
    async fn bare_object_edits_is_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let input = ToolInput {
            call_id: "c1".to_owned(),
            input: serde_json::json!({ "edits": { "path": "f.txt", "old": ["b"], "new": ["B"] } }),
            timeout: Duration::from_secs(5),
            progress: None,
        };
        let out = t.invoke(input).await.unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nB\n"
        );
    }

    /// A single string for `old`/`new` (not an array) splits on newlines and
    /// otherwise behaves identically to the canonical array form.
    #[tokio::test]
    async fn string_old_new_split_into_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": "b", "new": "B1\nB2" }
            ])))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nB1\nB2\nc\n"
        );
    }

    // --- not_found diagnostics ---------------------------------------------

    /// A one-character misquote must be located, not just "no match": the
    /// message names the breaking `old` line, the closest file line, and the
    /// first differing character — so a retry fixes the quote instead of
    /// re-reading the whole file.
    #[tokio::test]
    async fn not_found_locates_first_differing_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["alpha", "bete", "gamma"], "new": ["x"] }
            ])))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("not_found"));
        let msg = text(&out);
        assert!(
            msg.contains("first unmatched `old` line is #2: \"bete\""),
            "{msg}"
        );
        assert!(msg.contains("closest file line is #2: \"beta\""), "{msg}");
        assert!(msg.contains("first difference at char 4"), "{msg}");
        // The failed entry must leave the file untouched.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "alpha\nbeta\ngamma\n"
        );
    }

    /// When quoted lines exist in the file but are not adjacent, the
    /// diagnosis must name the line where the contiguous block breaks — not
    /// skip ahead to a later match and blame a line further down.
    #[tokio::test]
    async fn not_found_names_the_breaking_line_not_a_scattered_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nx\nb\ny\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["a", "b", "c"], "new": ["z"] }
            ])))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("not_found"));
        let msg = text(&out);
        assert!(
            msg.contains("first unmatched `old` line is #2: \"b\""),
            "{msg}"
        );
        assert!(msg.contains("closest file line is #2: \"x\""), "{msg}");
    }

    /// When the quoted `old` is a verbatim prefix of the file's tail but has
    /// extra trailing lines, the message says the file ran out rather than
    /// blaming a nonexistent line.
    #[tokio::test]
    async fn not_found_reports_file_end_when_old_runs_past_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "alpha\nbeta\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(call(serde_json::json!([
                { "path": "f.txt", "old": ["beta", "gamma"], "new": ["z"] }
            ])))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("not_found"));
        let msg = text(&out);
        assert!(
            msg.contains("first unmatched `old` line is #2: \"gamma\""),
            "{msg}"
        );
        assert!(msg.contains("file ends there"), "{msg}");
    }

    /// The Z-based prefix lengths must agree with the naive per-start count
    /// on adversarial shapes: highly repetitive lines are exactly where the
    /// naive scan degrades to O(MN) and where Z-box reuse is easiest to get
    /// wrong.
    #[test]
    fn z_prefix_lengths_matches_naive_on_repetitive_inputs() {
        let alphabet = ["a", "b", " "];
        // Deterministic LCG so the test is reproducible without a dev-dep.
        let mut state = 0x1234_5678_u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (state >> 33) as usize
        };
        for _case in 0..50 {
            let pat_len = 1 + next() % 5;
            let text_len = 1 + next() % 30;
            let pattern: Vec<String> = (0..pat_len)
                .map(|_| alphabet[next() % alphabet.len()].to_string())
                .collect();
            let text: Vec<&str> = (0..text_len)
                .map(|_| alphabet[next() % alphabet.len()])
                .collect();
            let naive: Vec<usize> = (0..text.len())
                .map(|start| {
                    let mut m = 0;
                    while m < pattern.len()
                        && start + m < text.len()
                        && text[start + m] == pattern[m]
                    {
                        m += 1;
                    }
                    m
                })
                .collect();
            assert_eq!(
                z_prefix_lengths(&pattern, &text),
                naive,
                "pattern={pattern:?} text={text:?}"
            );
        }
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
