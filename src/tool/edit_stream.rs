//! Stage-2 streaming presenter for the `edit` tool (`doc/tool-streaming.md`).
//!
//! CUMULATIVE streaming: as each `edits[i]` entry completes, its replacement
//! is located against the original file (the same `find_matches` the real
//! execute uses) and ADDED to a per-file splice set; the entry still streaming
//! grows its own splice on each frame. Because every entry anchors to the same
//! original snapshot — exactly as `plan_path` does — the accumulated view
//! converges to precisely the settled `plan_view` diff once the args close
//! (provided the file didn't change mid-stream; stage 3 re-plans against the
//! live file regardless, so an external change never yields a wrong result).
//!
//! Entries are grouped by RESOLVED absolute path, preserving first-seen order
//! — mirroring `plan_all`, so `A B A` renders as two files (A holding both
//! entries' splices merged, then B), never `A B A` with a repeated A. This is
//! not just cosmetic: two entries on one file are ONE set of splices applied
//! together, and splitting them would imply two independent writes.
//!
//! Failure tolerance: an `old` that is ambiguous mid-stream renders its first
//! match, and overlapping entries render as accumulated — the presenter is
//! optimistic. The settled execute is the authority on `not_found` /
//! `ambiguous` / `overlapping_edits` (stage 3); a failed edit carries no view.

use std::path::PathBuf;

use super::stream_args::PartialArgs;
use super::{StreamPresenter, resolve_in_workspace};

/// The `edit` streaming presenter. Created per call by
/// [`super::EditTool::stream_presenter`].
pub struct EditStreamPresenter {
    workspace: PathBuf,
    /// One group per distinct resolved path, in first-seen order. Each holds
    /// the file's original lines and the splices accumulated so far.
    groups: Vec<FileGroup>,
    /// How many closed entries have been folded into `groups`. The entries
    /// beyond this index (at most one — the streaming one) are re-read each
    /// frame.
    folded: usize,
}

/// A located replacement: `(start, end, new-lines)` against a file's original
/// lines — the same triple `render_hunks` consumes.
type Splice = (usize, usize, Vec<String>);

/// A transient splice tagged with its group index (the streaming entry's
/// contribution, re-computed each frame until it closes and folds).
type PendingSplice = (usize, Splice);

/// Accumulated state for one target file.
struct FileGroup {
    /// The workspace-relative path as first spelled (drives the diff header).
    rel_path: String,
    /// The resolved absolute path — the grouping key, so `"src/a.rs"` and
    /// `"./src/a.rs"` merge into one group (mirroring `plan_all`).
    abs: PathBuf,
    /// The raw file content as read, kept for byte-level `find_matches`.
    content: String,
    /// `content` split into lines (without terminators) for `render_hunks`.
    lines: Vec<String>,
    /// `(start, end, new)` splices located so far, in LINE coordinates
    /// (converted from the byte offsets `find_matches` returns). Insertion
    /// order; `render_hunks` sorts and merges them, so order doesn't matter.
    splices: Vec<Splice>,
}

/// Convert a byte offset in `content` to a 0-based line index (count of `\n`
/// before it). Used to express a byte-level match in the line coordinates
/// `render_hunks` consumes.
fn line_of(content: &str, byte: usize) -> usize {
    content[..byte].matches('\n').count()
}

impl EditStreamPresenter {
    #[must_use]
    pub const fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            groups: Vec::new(),
            folded: 0,
        }
    }

    /// The group index for `rel_path`, creating (and reading) the group on
    /// first sight. Returns `None` if the path is unresolvable or the file
    /// unreadable — the settled execute reports the real error at stage 3.
    fn group_for(&mut self, rel_path: &str) -> Option<usize> {
        let abs = resolve_in_workspace(&self.workspace, rel_path).ok()?;
        if let Some(idx) = self
            .groups
            .iter()
            .position(|g| g.rel_path == rel_path || g.same_file(&abs))
        {
            return Some(idx);
        }
        let content = std::fs::read_to_string(&abs).ok()?;
        self.groups.push(FileGroup {
            rel_path: rel_path.to_owned(),
            abs,
            lines: content.lines().map(str::to_owned).collect(),
            content,
            splices: Vec::new(),
        });
        Some(self.groups.len() - 1)
    }

    /// Locate one entry's `old` against a group's lines and append its
    /// splices. `replace_all` mirrors the execute's matching; ambiguous
    /// without it renders the first match (optimistic — see module docs).
    fn add_splices(group: &mut FileGroup, old: &[String], new: &[String], replace_all: bool) {
        if old.is_empty() {
            return;
        }
        // Byte-level match against the raw content, then expressed in line
        // coordinates for `render_hunks`. The presenter is optimistic (module
        // docs): it matches the model's LF-joined `old` directly, without the
        // execute's CRLF adaptation — stage 3 re-plans against the live file
        // and is the authority on the exact span.
        let needle = old.join("\n");
        let matches = super::edit::find_matches(&group.content, &needle);
        let starts: &[usize] = if replace_all {
            &matches
        } else {
            &matches[..1.min(matches.len())]
        };
        for &start in starts {
            let line_start = line_of(&group.content, start);
            // Absorb the line terminator after the match, mirroring the
            // execute's `locate_matches`, so a whole-line `old` renders as a
            // replacement of that line (not an insertion before it).
            let mut end = start + needle.len();
            if group.content[end..].starts_with('\n') {
                end += 1;
            }
            let line_end = line_of(&group.content, end);
            group.splices.push((line_start, line_end, new.to_vec()));
        }
    }
}

#[async_trait::async_trait]
impl StreamPresenter for EditStreamPresenter {
    async fn render(&mut self, accumulated_args: &str) -> Option<String> {
        let args = PartialArgs::new(accumulated_args);
        let (closed, open) = args.edit_entries()?;

        // Fold newly-closed entries into their groups (each exactly once).
        while self.folded < closed.len() {
            let entry = PartialArgs::new(closed[self.folded]);
            self.fold_entry(&entry);
            self.folded += 1;
        }

        // The streaming entry contributes a transient splice (its `new` may
        // still be growing); it is folded for real once it closes.
        let pending: Option<PendingSplice> = open.as_ref().and_then(|o| self.transient_splice(o));

        // Render every group that has anything to show, in first-seen order.
        let mut files = Vec::new();
        for (idx, group) in self.groups.iter().enumerate() {
            let mut splices: Vec<(usize, usize, &[String])> = group
                .splices
                .iter()
                .map(|(s, e, n)| (*s, *e, n.as_slice()))
                .collect();
            if let Some((gidx, (s, e, ref n))) = pending
                && gidx == idx
            {
                splices.push((s, e, n.as_slice()));
            }
            if splices.is_empty() {
                continue;
            }
            let patch = super::diffview::render_hunks(
                &group.lines,
                &splices,
                super::diffview::default_context(),
            );
            if !patch.is_empty() {
                files.push(serde_json::json!({ "path": group.rel_path, "patch": patch }));
            }
        }
        if files.is_empty() {
            return None;
        }
        Some(serde_json::json!({ "kind": "diff", "files": files }).to_string())
    }
}

impl EditStreamPresenter {
    /// Fold one CLOSED entry into its group's splice set (permanent).
    fn fold_entry(&mut self, entry: &PartialArgs<'_>) {
        let Some(path) = entry.complete_string("path") else {
            return;
        };
        let Some(old) = entry.complete_lines("old") else {
            return;
        };
        let new = entry.complete_lines("new").unwrap_or_default();
        let replace_all = entry.complete_bool("replace_all").unwrap_or(false);
        if let Some(idx) = self.group_for(&path) {
            Self::add_splices(&mut self.groups[idx], &old, &new, replace_all);
        }
    }

    /// The streaming entry's transient splice: its located `old` against the
    /// received prefix of `new`. `None` until `path` and `old` are complete
    /// and the anchor matches. Read fresh every frame (it is NOT folded).
    fn transient_splice(&mut self, entry: &PartialArgs<'_>) -> Option<PendingSplice> {
        let path = entry.complete_string("path")?;
        let old = entry.complete_lines("old")?;
        let idx = self.group_for(&path)?;
        let group = &self.groups[idx];
        if old.is_empty() {
            return None;
        }
        let needle = old.join("\n");
        let start = super::edit::find_matches(&group.content, &needle)
            .into_iter()
            .next()?;
        let new = entry.streaming_lines("new").unwrap_or_default();
        let line_start = line_of(&group.content, start);
        let mut end = start + needle.len();
        if group.content[end..].starts_with('\n') {
            end += 1;
        }
        let line_end = line_of(&group.content, end);
        Some((idx, (line_start, line_end, new)))
    }
}

impl FileGroup {
    /// Whether this group already covers `abs` (catches `"src/a.rs"` vs
    /// `"./src/a.rs"` resolving to the same file, like `plan_all`).
    fn same_file(&self, abs: &std::path::Path) -> bool {
        self.abs == abs
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn env(view: &str) -> serde_json::Value {
        serde_json::from_str(view).unwrap()
    }

    #[tokio::test]
    async fn accumulates_entries_monotonically() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\ne\n").unwrap();
        let mut p = EditStreamPresenter::new(dir.path().to_path_buf());

        // Entry 1 streaming: only its diff shows.
        let v = p
            .render(r#"{"edits": [{"path":"f.txt","old":["b"],"new":["B"]"#)
            .await
            .unwrap();
        let patch = env(&v)["files"][0]["patch"].as_str().unwrap().to_owned();
        assert!(patch.contains("-b") && patch.contains("+B"), "{patch}");
        assert!(!patch.contains("-d"), "entry 2 not here yet: {patch}");

        // Entry 1 closed, entry 2 streaming: BOTH show — entry 1 didn't vanish.
        let v = p
            .render(
                r#"{"edits": [{"path":"f.txt","old":["b"],"new":["B"]}, {"path":"f.txt","old":["d"],"new":["D"]"#,
            )
            .await
            .unwrap();
        let patch = env(&v)["files"][0]["patch"].as_str().unwrap().to_owned();
        assert!(patch.contains("+B"), "entry 1 persists: {patch}");
        assert!(patch.contains("+D"), "entry 2 joined: {patch}");
    }

    #[tokio::test]
    async fn a_b_a_merges_into_one_file_group() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a1\na2\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b1\n").unwrap();
        let mut p = EditStreamPresenter::new(dir.path().to_path_buf());
        // A, B, then A again — the third entry must merge into A's group, not
        // appear as a second A after B.
        let v = p
            .render(
                r#"{"edits": [
                    {"path":"a.txt","old":["a1"],"new":["A1"]},
                    {"path":"b.txt","old":["b1"],"new":["B1"]},
                    {"path":"a.txt","old":["a2"],"new":["A2"]}
                ]}"#,
            )
            .await
            .unwrap();
        let v = env(&v);
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 2, "A and B only — no second A: {v}");
        assert_eq!(files[0]["path"], "a.txt");
        assert_eq!(files[1]["path"], "b.txt");
        let a_patch = files[0]["patch"].as_str().unwrap();
        assert!(
            a_patch.contains("+A1") && a_patch.contains("+A2"),
            "{a_patch}"
        );
    }

    #[tokio::test]
    async fn accumulated_final_matches_a_full_render() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\ne\n").unwrap();
        let mut p = EditStreamPresenter::new(dir.path().to_path_buf());
        // Feed the whole thing in one shot (as at BlockStop): the accumulated
        // view equals the settled plan_view's diff.
        let v = p
            .render(
                r#"{"edits": [{"path":"f.txt","old":["b"],"new":["B"]},{"path":"f.txt","old":["d"],"new":["D"]}]}"#,
            )
            .await
            .unwrap();
        let patch = env(&v)["files"][0]["patch"].as_str().unwrap().to_owned();
        // Same single merged hunk shape the settled view produces.
        assert!(patch.contains("-b") && patch.contains("+B"));
        assert!(patch.contains("-d") && patch.contains("+D"));
    }

    #[tokio::test]
    async fn unmatched_old_yields_no_frame() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\n").unwrap();
        let mut p = EditStreamPresenter::new(dir.path().to_path_buf());
        assert!(
            p.render(r#"{"edits": [{"path": "f.txt", "old": ["zzz"], "new": ["Z"]}]}"#)
                .await
                .is_none()
        );
    }
}
