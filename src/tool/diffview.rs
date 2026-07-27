//! Backend construction of the UI diff view for `edit`/`write` results.
//!
//! This is the `doc/tool-view.md` home for what used to be the front-end's
//! `diff-builder.ts` (since deleted): the same unified-diff rendering, now
//! living next to the matching that produced it, so the diff is exact — built
//! from the real pre-edit content the tool already read, not a cache the
//! front-end reconstructed from event fragments.
//!
//! Everything here is pure (line slices in, diff text out); the tools call it
//! at execution time and attach the result as a `Content::TextView` block.

/// Unchanged context lines shown on each side of a hunk (matches the old
/// front-end renderer).
const CONTEXT: usize = 3;

/// A resolved edit against a file: a half-open `[start, end)` old range and
/// the replacement payload.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Splice {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) payload: Vec<String>,
}

/// Strip the common leading/trailing lines an edit shares between its old
/// range and its replacement, returning the narrowed core. A model routinely
/// pads both with extra context lines for anchoring; rendered whole, every
/// padded line shows as a `-`/`+` pair and the real change is visually buried.
/// The strip is purely presentational: matching already ran against the FULL
/// old (the splice lands where the tool applied it), and the stripped lines
/// simply fall back to ordinary context in the hunk.
fn strip_common(start: usize, old_len: usize, replacement: &[String], lines: &[String]) -> Splice {
    let old = &lines[start..start + old_len];
    let mut pre = 0;
    let max_pre = old_len.min(replacement.len());
    while pre < max_pre && old[pre] == replacement[pre] {
        pre += 1;
    }
    let mut suf = 0;
    let max_suf = (old_len - pre).min(replacement.len() - pre);
    while suf < max_suf && old[old_len - 1 - suf] == replacement[replacement.len() - 1 - suf] {
        suf += 1;
    }
    Splice {
        start: start + pre,
        end: start + old_len - suf,
        payload: replacement[pre..replacement.len() - suf].to_vec(),
    }
}

/// Render sorted splices against the pre-edit `lines` into unified-diff hunk
/// text with [`CONTEXT`] lines on each side, merging hunks whose context
/// windows touch. `splices` are the tool's exact resolved edits
/// (`(start, end, entry)`); each is narrowed by [`strip_common`] first.
pub fn render_hunks(
    lines: &[String],
    splices: &[(usize, usize, &[String])],
    context: usize,
) -> String {
    let mut narrowed: Vec<Splice> = splices
        .iter()
        .map(|&(start, end, payload)| strip_common(start, end - start, payload, lines))
        // Drop no-op splices (old identical to new after stripping): nothing
        // changed, so there is nothing to diff — rendering one would emit a
        // hunk of pure context with a misleading `@@` header.
        .filter(|s| s.end > s.start || !s.payload.is_empty())
        .collect();
    narrowed.sort_by_key(|s| s.start);
    if narrowed.is_empty() {
        return String::new();
    }
    let n = lines.len();

    // Group edits whose context windows overlap into shared hunks.
    let mut groups: Vec<Vec<Splice>> = Vec::new();
    for s in narrowed {
        match groups.last_mut() {
            Some(last) if s.start <= last[last.len() - 1].end + context * 2 => last.push(s),
            _ => groups.push(vec![s]),
        }
    }

    let mut out: Vec<String> = Vec::new();
    let mut cum_added = 0usize;
    let mut cum_removed = 0usize;
    for group in groups {
        let first_start = group[0].start;
        let last_end = group[group.len() - 1].end;
        let ctx_start = first_start.saturating_sub(context);
        let ctx_end = (last_end + context).min(n);

        let old_len = ctx_end - ctx_start;
        let mut new_len = old_len;
        for s in &group {
            new_len = new_len - (s.end - s.start) + s.payload.len();
        }
        let old_start = ctx_start + 1;
        let new_start = ctx_start
            .saturating_add(cum_added)
            .saturating_sub(cum_removed)
            + 1;
        out.push(format!(
            "@@ -{old_start},{old_len} +{new_start},{new_len} @@"
        ));

        let mut cursor = ctx_start;
        for s in &group {
            for line in &lines[cursor..s.start] {
                out.push(format!(" {line}"));
            }
            for line in &lines[s.start..s.end] {
                out.push(format!("-{line}"));
            }
            for p in &s.payload {
                out.push(format!("+{p}"));
            }
            cursor = s.end;
            cum_added += s.payload.len();
            cum_removed += s.end - s.start;
        }
        for line in &lines[cursor..ctx_end] {
            out.push(format!(" {line}"));
        }
    }
    out.join("\n")
}

/// `(tag, old_idx, new_idx)` per line; `-1` for a line present on one side only.
enum Tag {
    Eq,
    Add,
    Del,
}
struct Flat {
    tag: Tag,
    text: String,
    old_idx: i64,
    new_idx: i64,
}

/// Build a JSON envelope `{ kind: "diff", files: [{ path, patch }] }` for a
/// `write` overwrite, given the pre-write file content and the new content.
/// Unlike [`render_hunks_json`] (which renders the tool's already-anchored
/// splices), a `write` has only the new full content with no anchor
/// correspondence to the old, so this runs a real line-level diff (`similar`,
/// Myers) and windows it into hunks.
///
/// Returns `""` when the contents are identical (a no-change write renders no
/// diff block, same as the old front-end).
pub fn write_diff_json(path: &str, old: &str, new: &str, context: usize) -> String {
    let body = write_diff(old, new, context);
    if body.is_empty() {
        return String::new();
    }
    serde_json::json!({
        "kind": "diff",
        "files": [{
            "path": path,
            "patch": body,
        }],
    })
    .to_string()
}

/// Build a unified-diff hunk text for a `write` overwrite, given the pre-write
/// file content and the new content. Unlike [`render_hunks`] (which renders
/// the tool's already-anchored splices), a `write` has only the new full
/// content with no anchor correspondence to the old, so this runs a real
/// line-level diff (`similar`, Myers) and windows it into hunks.
///
/// Returns `""` when the contents are identical (a no-change write renders no
/// diff block, same as the old front-end).
pub fn write_diff(old: &str, new: &str, context: usize) -> String {
    let diff = similar::TextDiff::from_lines(old, new);

    let mut flat: Vec<Flat> = Vec::new();
    let (mut oi, mut ni) = (0i64, 0i64);
    for change in diff.iter_all_changes() {
        let text = change.value().trim_end_matches('\n').to_owned();
        match change.tag() {
            similar::ChangeTag::Equal => {
                flat.push(Flat {
                    tag: Tag::Eq,
                    text,
                    old_idx: oi,
                    new_idx: ni,
                });
                oi += 1;
                ni += 1;
            }
            similar::ChangeTag::Delete => {
                flat.push(Flat {
                    tag: Tag::Del,
                    text,
                    old_idx: oi,
                    new_idx: -1,
                });
                oi += 1;
            }
            similar::ChangeTag::Insert => {
                flat.push(Flat {
                    tag: Tag::Add,
                    text,
                    old_idx: -1,
                    new_idx: ni,
                });
                ni += 1;
            }
        }
    }

    let changed: Vec<usize> = (0..flat.len())
        .filter(|&i| !matches!(flat[i].tag, Tag::Eq))
        .collect();
    if changed.is_empty() {
        return String::new();
    }

    // Expand each changed index into a context window, merging overlaps.
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for ci in changed {
        let lo = ci.saturating_sub(context);
        let hi = (ci + context + 1).min(flat.len());
        match windows.last_mut() {
            Some(last) if last.1 >= lo => last.1 = last.1.max(hi),
            _ => windows.push((lo, hi)),
        }
    }

    let mut out: Vec<String> = Vec::new();
    for (lo, hi) in windows {
        let hunk = &flat[lo..hi];
        let first_old = hunk.iter().find(|l| l.old_idx != -1);
        let first_new = hunk.iter().find(|l| l.new_idx != -1);
        let old_start = first_old.map_or(0, |l| l.old_idx + 1);
        let new_start = first_new.map_or(0, |l| l.new_idx + 1);
        let old_len = hunk.iter().filter(|l| !matches!(l.tag, Tag::Add)).count();
        let new_len = hunk.iter().filter(|l| !matches!(l.tag, Tag::Del)).count();
        out.push(format!(
            "@@ -{old_start},{old_len} +{new_start},{new_len} @@"
        ));
        for l in hunk {
            let prefix = match l.tag {
                Tag::Add => '+',
                Tag::Del => '-',
                Tag::Eq => ' ',
            };
            out.push(format!("{prefix}{}", l.text));
        }
    }
    out.join("\n")
}

/// The default context, exposed so the tools don't name the constant.
pub const fn default_context() -> usize {
    CONTEXT
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    // Ported from the deleted front-end `diff-builder.test.ts` fixtures — the
    // rendering contract the UI's `Diff.svelte` was built against, now pinned
    // on the side that owns the matching.

    #[test]
    fn one_hunk_with_context_for_single_line_replace() {
        let file = lines(&["a", "b", "c", "d", "e"]);
        let new = lines(&["C"]);
        assert_eq!(
            render_hunks(&file, &[(2, 3, &new)], 1),
            "@@ -2,3 +2,3 @@\n b\n-c\n+C\n d"
        );
    }

    #[test]
    fn distant_edits_split_into_two_hunks() {
        let file = lines(
            &(1..=12)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        let a = lines(&["A"]);
        let b = lines(&["B"]);
        assert_eq!(
            render_hunks(&file, &[(1, 2, &a), (10, 11, &b)], 1),
            "@@ -1,3 +1,3 @@\n l1\n-l2\n+A\n l3\n@@ -10,3 +10,3 @@\n l10\n-l11\n+B\n l12"
        );
    }

    #[test]
    fn close_edits_merge_into_one_hunk() {
        let file = lines(&["a", "b", "c", "d", "e"]);
        let b = lines(&["B"]);
        let d = lines(&["D"]);
        assert_eq!(
            render_hunks(&file, &[(1, 2, &b), (3, 4, &d)], 3),
            "@@ -1,5 +1,5 @@\n a\n-b\n+B\n c\n-d\n+D\n e"
        );
    }

    #[test]
    fn insert_keeps_anchor_as_context_not_remove_add_pair() {
        let file = lines(&["a", "b"]);
        let new = lines(&["a", "A1"]);
        assert_eq!(
            render_hunks(&file, &[(0, 1, &new)], 1),
            "@@ -1,2 +1,3 @@\n a\n+A1\n b"
        );
    }

    #[test]
    fn common_prefix_suffix_renders_as_context() {
        let file = lines(&["p1", "p2", "old-mid", "p3", "p4"]);
        let new = lines(&["p1", "p2", "new-mid", "p3", "p4"]);
        assert_eq!(
            render_hunks(&file, &[(0, 5, &new)], 1),
            "@@ -2,3 +2,3 @@\n p2\n-old-mid\n+new-mid\n p3"
        );
    }

    #[test]
    fn shared_suffix_strips_tail_not_head() {
        let file = lines(&["x", "anchor"]);
        let new = lines(&["X1", "X2", "anchor"]);
        assert_eq!(
            render_hunks(&file, &[(0, 2, &new)], 1),
            "@@ -1,2 +1,3 @@\n-x\n+X1\n+X2\n anchor"
        );
    }

    #[test]
    fn stripping_never_moves_the_hunk_anchor() {
        let file = lines(&["pre", "mid-old", "post", "other"]);
        let new = lines(&["pre", "mid-new", "post"]);
        assert_eq!(
            render_hunks(&file, &[(0, 3, &new)], 0),
            "@@ -2,1 +2,1 @@\n-mid-old\n+mid-new"
        );
    }

    #[test]
    fn identical_old_new_renders_no_diff() {
        let file = lines(&["a", "b", "c"]);
        let new = lines(&["b"]);
        assert_eq!(render_hunks(&file, &[(1, 2, &new)], 1), "");
    }

    #[test]
    fn replace_all_occurrences_merge_into_one_hunk() {
        let file = lines(&["x", "y", "x", "z", "x"]);
        let new = lines(&["X"]);
        let out = render_hunks(&file, &[(0, 1, &new), (2, 3, &new), (4, 5, &new)], 1);
        assert_eq!(out.matches("@@ ").count(), 1);
        assert!(out.contains("-x") && out.contains("+X"));
    }

    // --- write_diff ----------------------------------------------------------

    #[test]
    fn write_identical_content_renders_no_hunk() {
        assert_eq!(write_diff("a\nb\nc\n", "a\nb\nc\n", 3), "");
    }

    #[test]
    fn write_single_line_change() {
        assert_eq!(
            write_diff("a\nb\nc\nd\ne\n", "a\nb\nC\nd\ne\n", 1),
            "@@ -2,3 +2,3 @@\n b\n-c\n+C\n d"
        );
    }

    #[test]
    fn write_distant_changes_split() {
        let old: String = (1..=12).fold(String::new(), |mut acc, i| {
            use std::fmt::Write;
            let _ = writeln!(acc, "l{i}");
            acc
        });
        let new = old.replace("l2\n", "A\n").replace("l11\n", "B\n");
        assert_eq!(
            write_diff(&old, &new, 1),
            "@@ -1,3 +1,3 @@\n l1\n-l2\n+A\n l3\n@@ -10,3 +10,3 @@\n l10\n-l11\n+B\n l12"
        );
    }

    #[test]
    fn write_pure_insertion_at_start() {
        assert_eq!(
            write_diff("a\nb\n", "x\na\nb\n", 1),
            "@@ -1,1 +1,2 @@\n+x\n a"
        );
    }
}
