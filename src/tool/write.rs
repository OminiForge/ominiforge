//! The `write` built-in tool: write a UTF-8 file within the workspace.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;

use super::{
    Tool, ToolDescriptor, ToolError, ToolInput, ToolResult, append_diagnostics,
    resolve_in_workspace,
};
use crate::core::payload::{Content, ToolOutput};
use crate::format::FormatManager;
use crate::lsp::LspManager;

/// Writes a text file relative to the session workspace, creating parent
/// directories as needed.
#[derive(Clone)]
pub struct WriteTool {
    workspace: PathBuf,
    /// Optional LSP assist: when set, a successful write appends the touched
    /// file's diagnostics to the result (`doc/lsp.md`).
    lsp: Option<Arc<LspManager>>,
    /// Optional auto-format: when set, the written content is formatted before
    /// the diff/diagnostics are produced (`doc/format.md`).
    format: Option<Arc<FormatManager>>,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

impl WriteTool {
    /// Create a `write` tool rooted at `workspace`.
    #[must_use]
    pub const fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            lsp: None,
            format: None,
        }
    }

    /// Attach an [`LspManager`] so successful writes carry diagnostics.
    #[must_use]
    pub fn with_lsp(mut self, lsp: Option<Arc<LspManager>>) -> Self {
        self.lsp = lsp;
        self
    }

    /// Attach a [`FormatManager`] so successful writes are formatted before
    /// their diff/diagnostics are produced (`doc/format.md`).
    #[must_use]
    pub fn with_format(mut self, format: Option<Arc<FormatManager>>) -> Self {
        self.format = format;
        self
    }
}

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "write".to_owned(),
            description: "Write a UTF-8 text file, relative to the workspace root. \
                          Creates parent directories and overwrites existing files. \
                          Output `path` FIRST, then `content` (streaming renders the \
                          file as soon as `path` arrives)."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the workspace root. \
                                        Emit this field FIRST (before `content`) so \
                                        streaming can render the file immediately."
                    },
                    "content": {
                        "type": "string",
                        "description": "Full file contents to write."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, input: ToolInput) -> ToolResult {
        let args: WriteArgs = serde_json::from_value(input.input)
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let path = resolve_in_workspace(&self.workspace, &args.path)?;

        // Snapshot the prior content (if any) BEFORE writing, so an overwrite can
        // diff old→new. A missing/unreadable file is treated as a new file.
        let old = tokio::fs::read_to_string(&path).await.ok();

        // Auto-format the content BEFORE it lands (`doc/format.md` §6): a
        // `write` replaces the whole file, so it always formats whole-file
        // (`edited_lines = None`). The FINAL text is written once, and the
        // diff/diagnostics below are anchored to it. Fail-closed: a skip keeps
        // the model's content.
        let outcome = match &self.format {
            Some(fmt) => fmt.format(&path, &args.content, None).await,
            None => crate::format::FormatOutcome::Skipped {
                text: args.content.clone(),
            },
        };
        // Record the formatter only when it actually changed the content,
        // plus how many change regions it made (model content → formatted).
        let formatter = match &outcome {
            crate::format::FormatOutcome::Formatted { formatter, text }
                if *text != args.content =>
            {
                Some((
                    formatter.clone(),
                    super::diffview::change_region_count(&args.content, text),
                ))
            }
            _ => None,
        };
        let content = outcome.into_text();

        if let Some(parent) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(business_error(&args.path, &e));
        }
        match tokio::fs::write(&path, content.as_bytes()).await {
            Ok(()) => {
                let mut output = ToolOutput {
                    content: vec![Content::Text(write_summary(
                        &args.path,
                        old.as_deref(),
                        &content,
                    ))],
                    is_error: false,
                    error_code: None,
                };
                // UI view: an overwrite diffs old→new (`similar`, same engine
                // `write_summary` counts with); a new file's view is its full
                // content. Never model input (`doc/tool-view.md`). When a
                // formatter changed the text the diff carries `formatted_by`.
                let view = write_view(
                    &args.path,
                    old.as_deref(),
                    &content,
                    formatter.as_ref().map(|(n, c)| (n.as_str(), *c)),
                );
                if let Some(text) = view {
                    output.content.push(Content::TextView {
                        text,
                        audience: crate::core::payload::AUDIENCE_UI.to_owned(),
                    });
                }
                append_diagnostics(self.lsp.as_ref(), &mut output, &path, &args.path, &content)
                    .await;
                Ok(output)
            }
            Err(e) => Ok(business_error(&args.path, &e)),
        }
    }

    /// Stage-2 streaming (`doc/tool-streaming.md`): a per-call presenter that
    /// grows a code view (new file) or a live old→new diff (overwrite) as the
    /// `content` arg streams in.
    fn stream_presenter(&self) -> Option<Box<dyn super::StreamPresenter>> {
        Some(Box::new(super::write_stream::WriteStreamPresenter::new(
            self.workspace.clone(),
        )))
    }
}

/// The write UI view as a JSON envelope (`doc/tool-view.md`): an overwrite is a
/// `diff` of old→new; a new file is its full `code` content. `None` for a
/// no-change write (empty diff = no block) or an empty new file. Shared by
/// `invoke` (executed `TextView`) and `preview` (approval gate), so the gate
/// shows exactly what the executed card will.
fn write_view(
    path: &str,
    old: Option<&str>,
    content: &str,
    formatted: Option<(&str, usize)>,
) -> Option<String> {
    match old {
        Some(old) if old != content => {
            let body = super::diffview::write_diff_json(
                path,
                old,
                content,
                super::diffview::default_context(),
                formatted,
            );
            (!body.is_empty()).then_some(body)
        }
        None if !content.is_empty() => Some(
            serde_json::json!({
                "kind": "code",
                "path": path,
                "content": content,
            })
            .to_string(),
        ),
        _ => None,
    }
}

/// Build the write result: a single header line, no diff body. The model
/// already has the full new content in this call's own `content` argument, so
/// echoing it back as a diff would be redundant; the visual diff is the
/// frontend's job (it reconstructs it from the args plus its file cache). See
/// `doc/tool-protocol.md` §11.
///
/// - New file (`old` is `None`) → `wrote PATH (new, N lines)`.
/// - Overwrite (`old` is `Some`) → `wrote PATH (~, +A -B)` with the added/
///   removed line counts, or `wrote PATH (no change)` for identical content.
fn write_summary(path: &str, old: Option<&str>, new: &str) -> String {
    let Some(old) = old else {
        let n = new.lines().count();
        return format!("wrote {path} (new, {n} lines)");
    };

    if old == new {
        return format!("wrote {path} (no change)");
    }

    let diff = similar::TextDiff::from_lines(old, new);
    let (mut added, mut removed) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    format!("wrote {path} (~, +{added} -{removed})")
}

fn business_error(path: &str, e: &std::io::Error) -> ToolOutput {
    ToolOutput {
        content: vec![Content::Text(format!("failed to write {path}: {e}"))],
        is_error: true,
        error_code: Some("write_failed".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::time::Duration;

    fn input(path: &str, content: &str) -> ToolInput {
        ToolInput {
            call_id: "c1".to_owned(),
            input: serde_json::json!({ "path": path, "content": content }),
            timeout: Duration::from_secs(5),
            progress: None,
        }
    }

    #[tokio::test]
    async fn writes_file_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(dir.path().to_path_buf());

        let out = tool.invoke(input("nested/dir/a.txt", "hi")).await.unwrap();
        assert!(!out.is_error);
        let written = std::fs::read_to_string(dir.path().join("nested/dir/a.txt")).unwrap();
        assert_eq!(written, "hi");
    }

    #[tokio::test]
    async fn escaping_path_is_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(dir.path().to_path_buf());
        assert!(matches!(
            tool.invoke(input("../escape", "x")).await,
            Err(ToolError::InvalidInput(_))
        ));
    }

    fn view(out: &ToolOutput) -> Option<&str> {
        out.content.iter().find_map(|c| match c {
            Content::TextView { text, audience } if audience == "ui" => Some(text.as_str()),
            _ => None,
        })
    }

    /// A new file's view is its full content (the front-end renders it as a
    /// code view, not a diff — there is no "before" side).
    #[tokio::test]
    async fn new_file_view_is_the_full_content() {
        let dir = tempfile::tempdir().unwrap();
        let out = WriteTool::new(dir.path().to_path_buf())
            .invoke(input(
                "n.rs",
                "fn main() {}
",
            ))
            .await
            .unwrap();
        assert!(!out.is_error);
        // The view is a JSON envelope `{ kind: "code", path, content }`.
        let view_json: serde_json::Value = serde_json::from_str(view(&out).unwrap()).unwrap();
        assert_eq!(view_json["kind"], "code");
        assert_eq!(view_json["path"], "n.rs");
        assert_eq!(view_json["content"], "fn main() {}\n");
    }

    /// An overwrite's view is the exact old→new unified diff, built from the
    /// real pre-write content — never a front-end reconstruction
    /// (`doc/tool-view.md` §4).
    #[tokio::test]
    async fn overwrite_view_is_the_diff() {
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
        let out = WriteTool::new(dir.path().to_path_buf())
            .invoke(input(
                "f.txt",
                "a
b
C
d
e
",
            ))
            .await
            .unwrap();
        assert!(!out.is_error);
        // The view is a JSON envelope `{ kind: "diff", files: [{ path, patch }] }`.
        let view_json: serde_json::Value = serde_json::from_str(view(&out).unwrap()).unwrap();
        assert_eq!(view_json["kind"], "diff");
        assert_eq!(view_json["files"][0]["path"], "f.txt");
        assert_eq!(
            view_json["files"][0]["patch"].as_str().unwrap(),
            "@@ -1,5 +1,5 @@\n a\n b\n-c\n+C\n d\n e"
        );
    }

    /// A no-change write produces no view (the "no change" summary is the
    /// whole story), and a failed write (escaping path is a protocol error,
    /// but a business failure likewise) never carries one.
    #[tokio::test]
    async fn no_change_write_has_no_view() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("f.txt"),
            "same
",
        )
        .unwrap();
        let out = WriteTool::new(dir.path().to_path_buf())
            .invoke(input(
                "f.txt", "same
",
            ))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(view(&out).is_none());
    }

    // --- auto-format integration (`doc/format.md`) --------------------------

    /// A `FormatManager` whose only formatter strips trailing whitespace via
    /// `sed` (a whitespace-only change that passes the fail-closed check).
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

    /// An overwrite whose content carries trailing whitespace is written
    /// FORMATTED, and the diff view (old → formatted) is annotated
    /// `formatted_by` (`doc/format.md` §6) — the model sees the real on-disk
    /// change, part of which is the formatter's.
    #[tokio::test]
    async fn formatted_write_diff_is_annotated() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-write content differs from the formatted result, so the diff is
        // non-empty; the model's content carries trailing whitespace that the
        // formatter strips.
        std::fs::write(dir.path().join("f.txt"), "x\ny\n").unwrap();
        let out = WriteTool::new(dir.path().to_path_buf())
            .with_format(Some(fmt_manager()))
            .invoke(input("f.txt", "a   \nb\n"))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nb\n"
        );
        let view_json: serde_json::Value = serde_json::from_str(view(&out).unwrap()).unwrap();
        assert_eq!(view_json["files"][0]["formatted_by"], "trim-ws");
    }

    /// `mode = "off"` produces no `FormatManager` at all (`FormatManager::new`
    /// returns `None`), so a write is never touched.
    #[tokio::test]
    async fn mode_off_writes_content_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::format::FormatConfig {
            mode: Some(crate::format::FormatMode::Off),
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
        // `None` manager → the tool is constructed without a formatter.
        assert!(
            crate::format::FormatManager::new(config, std::collections::BTreeMap::new()).is_none()
        );
        let out = WriteTool::new(dir.path().to_path_buf())
            .invoke(input("f.txt", "a   \n"))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a   \n"
        );
    }

    // --- write_summary -------------------------------------------------------

    /// A brand-new file reports line count in a one-line header, no body — the
    /// model already has the full content in its own `content` arg.
    #[test]
    fn summary_new_file_is_header_only() {
        let out = write_summary("src/new.rs", None, "a\nb\n");
        assert_eq!(out, "wrote src/new.rs (new, 2 lines)");
    }

    /// An overwrite reports +added/-removed counts in a one-line header, no
    /// diff body (the frontend rebuilds the visual diff from args + its cache).
    #[test]
    fn summary_overwrite_is_header_only_with_counts() {
        let out = write_summary("f.txt", Some("a\nb\nc\n"), "a\nB\nc\n");
        assert_eq!(out, "wrote f.txt (~, +1 -1)");
    }

    /// Rewriting identical content is a no-op the reader should see as such, not
    /// an empty diff that looks like a failure.
    #[test]
    fn summary_no_change_is_flagged() {
        let out = write_summary("f.txt", Some("same\n"), "same\n");
        assert_eq!(out, "wrote f.txt (no change)");
    }
}
