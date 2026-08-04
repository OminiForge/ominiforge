//! Stage-2 streaming presenter for the `write` tool (`doc/tool-streaming.md`).
//!
//! `write` is the highest-value streaming tool: its `content` arg is large and
//! arrives slowly, so a live view turns a multi-second silent wait into a
//! visible, growing file. Generation order works for us — the model emits
//! `path` before `content` — so the presenter can read the pre-write file as
//! soon as `path` closes, then render `content` as it grows.
//!
//! View strategy (mirrors [`super::write`]'s settled view, so the front-end
//! uses one render path for stage 2 and stage 3):
//! - **New file** (no pre-existing content): a `code` envelope whose `content`
//!   is the received prefix — the file simply grows on screen.
//! - **Overwrite**: a `diff` envelope of the pre-write content vs the received
//!   prefix. Early frames show only the head of the file changing; the settled
//!   view at stage 3 replaces the partial diff with the complete one.
//!
//! The pre-write file content is read ONCE (lazily, on the first frame after
//! `path` closes) and cached — a presenter instance serves exactly one call.

use std::path::PathBuf;

use super::stream_args::PartialArgs;
use super::{StreamPresenter, resolve_in_workspace};

/// The pre-write file state, resolved once `path` closes.
enum PreWrite {
    /// Not yet looked up (`path` still streaming or unresolvable).
    Unknown,
    /// Looked up: `Some(content)` = an overwrite of an existing file, `None` =
    /// a new file (no pre-existing content).
    Resolved(Option<String>),
}

/// The `write` streaming presenter. Created per call by
/// [`super::WriteTool::stream_presenter`].
pub struct WriteStreamPresenter {
    workspace: PathBuf,
    /// Cached pre-write file content, filled once `path` closes.
    old: PreWrite,
    /// The resolved relative path, once `path` closes (drives the envelope).
    path: Option<String>,
}

impl WriteStreamPresenter {
    #[must_use]
    pub const fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            old: PreWrite::Unknown,
            path: None,
        }
    }

    /// Resolve `path` and load the pre-write content, once. No-op until `path`
    /// closes; subsequent calls reuse the cache. An unresolvable path leaves
    /// both caches empty (the settled execute reports the real error later).
    /// Synchronous: the read is small and runs on the throttled frame, not per
    /// token.
    fn ensure_file(&mut self, args: &PartialArgs<'_>) {
        if matches!(self.old, PreWrite::Resolved(_)) {
            return;
        }
        let Some(path) = args.complete_string("path") else {
            return;
        };
        let old = resolve_in_workspace(&self.workspace, &path)
            .ok()
            .and_then(|abs| std::fs::read_to_string(abs).ok());
        self.path = Some(path);
        self.old = PreWrite::Resolved(old);
    }
}

#[async_trait::async_trait]
impl StreamPresenter for WriteStreamPresenter {
    async fn render(&mut self, accumulated_args: &str) -> Option<String> {
        let args = PartialArgs::new(accumulated_args);
        self.ensure_file(&args);
        let path = self.path.as_deref()?;
        let content = args.streaming_string("content")?;
        if content.is_empty() {
            return None;
        }
        let PreWrite::Resolved(old) = &self.old else {
            return None;
        };
        match old.as_deref() {
            // New file: grow a code view with the received prefix.
            None => Some(
                serde_json::json!({
                    "kind": "code",
                    "path": path,
                    "content": content,
                })
                .to_string(),
            ),
            // Overwrite: diff the pre-write content against the prefix so far.
            // A partial prefix would diff the old file's UNREACHED tail as a
            // spurious deletion, so the old side is truncated to the prefix's
            // line count first — an unchanged-so-far prefix then yields an
            // empty diff (no frame yet), and only real changes render.
            Some(old) => {
                // Diff only the COMPLETE lines received so far: the trailing
                // line may be cut mid-way by the stream, and diffing it would
                // show a phantom in-line change against the old file. Dropping
                // it also keeps the frame stable until a full line lands.
                let complete = match content.strip_suffix('\n') {
                    // Ends on a newline: every line is complete.
                    Some(head) => head,
                    // Mid-line: keep up to the last newline (None if the very
                    // first line isn't finished yet → nothing stable to show).
                    None => match content.rsplit_once('\n') {
                        Some((head, _)) => head,
                        None => return None,
                    },
                };
                if complete.is_empty() {
                    return None;
                }
                let prefix_lines = complete.lines().count();
                let old_head: String = old
                    .lines()
                    .take(prefix_lines)
                    .collect::<Vec<_>>()
                    .join("\n");
                let body = super::diffview::write_diff_json(
                    path,
                    &old_head,
                    complete,
                    super::diffview::default_context(),
                    // Streaming preview: no formatter has run yet, so no
                    // `formatted_by` annotation (`doc/format.md` §6).
                    None,
                );
                (!body.is_empty()).then_some(body)
            }
        }
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
    async fn nothing_until_path_then_content() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = WriteStreamPresenter::new(dir.path().to_path_buf());
        // Path not closed yet → None.
        assert!(p.render(r#"{"path": "new"#).await.is_none());
        // Path closed, content not started → None.
        assert!(p.render(r#"{"path": "new.txt""#).await.is_none());
        // Content streaming → a growing code view.
        let v = p
            .render(r#"{"path": "new.txt", "content": "hello"#)
            .await
            .unwrap();
        let v = env(&v);
        assert_eq!(v["kind"], "code");
        assert_eq!(v["path"], "new.txt");
        assert_eq!(v["content"], "hello");
    }

    #[tokio::test]
    async fn new_file_grows_a_code_view() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = WriteStreamPresenter::new(dir.path().to_path_buf());
        let v1 = p
            .render(r#"{"path": "n.rs", "content": "fn main() {"#)
            .await
            .unwrap();
        let v2 = p
            .render(r#"{"path": "n.rs", "content": "fn main() {\n    run();"#)
            .await
            .unwrap();
        assert_eq!(env(&v1)["content"], "fn main() {");
        assert_eq!(env(&v2)["content"], "fn main() {\n    run();");
    }

    #[tokio::test]
    async fn overwrite_diffs_against_the_pre_write_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let mut p = WriteStreamPresenter::new(dir.path().to_path_buf());
        let v = p
            .render(r#"{"path": "f.txt", "content": "a\nB\nc\n"}"#)
            .await
            .unwrap();
        let v = env(&v);
        assert_eq!(v["kind"], "diff");
        assert_eq!(v["files"][0]["path"], "f.txt");
        // Trailing newline → all three lines complete → full old-vs-new diff.
        assert_eq!(
            v["files"][0]["patch"].as_str().unwrap(),
            "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c"
        );
    }

    #[tokio::test]
    async fn overwrite_emits_nothing_while_prefix_is_identical() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let mut p = WriteStreamPresenter::new(dir.path().to_path_buf());
        // Received prefix still matches the old file (and ends mid-line, so
        // only the one complete line "a" is diffed) → no diff yet → None.
        assert!(
            p.render(r#"{"path": "f.txt", "content": "a\nb"#)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn file_is_read_once_and_cached() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "old\n").unwrap();
        let mut p = WriteStreamPresenter::new(dir.path().to_path_buf());
        let _ = p.render(r#"{"path": "f.txt", "content": "o"#).await;
        // Mutate the file after the first frame; the cached pre-write content
        // must still drive the next frame (a presenter snapshots once).
        std::fs::write(dir.path().join("f.txt"), "CHANGED\n").unwrap();
        let v = p
            .render(r#"{"path": "f.txt", "content": "old\nnew\n"}"#)
            .await
            .unwrap();
        let patch = env(&v)["files"][0]["patch"].as_str().unwrap().to_owned();
        assert!(
            patch.contains("+new"),
            "diff vs cached old, not the live file"
        );
    }
}
