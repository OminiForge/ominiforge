//! The `write` built-in tool: write a UTF-8 file within the workspace.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;

use super::{
    Tool, ToolDescriptor, ToolError, ToolInput, ToolResult, append_diagnostics,
    resolve_in_workspace,
};
use crate::core::payload::{Content, ToolOutput};
use crate::format::FormatService;
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
    /// the diff/diagnostics are produced (`doc/lsp.md`).
    format: Option<Arc<dyn FormatService>>,
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

    /// Attach a [`FormatService`] so successful writes are formatted before
    /// their diff/diagnostics are produced (`doc/lsp.md`).
    #[must_use]
    pub fn with_format(mut self, format: Option<Arc<dyn FormatService>>) -> Self {
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

        // Line-ending preservation: overwriting a CRLF file with bare-LF
        // content must not silently rewrite every line ending to LF. Normalize
        // the model's content to the file's existing convention (CRLF → the
        // whole file), so a `write` keeps the file's own style. A NEW file has
        // no convention to preserve, so the model's content lands as given.
        let model_content = match &old {
            Some(old) if super::edit::detect_line_ending(old) == super::edit::LineEnding::Crlf => {
                normalize_to_crlf(&args.content)
            }
            _ => args.content.clone(),
        };

        // Auto-format the content BEFORE it lands (`doc/lsp.md` §6): a
        // `write` replaces the whole file, so it always formats whole-file
        // (`edited_lines = None`). The FINAL text is written once, and the
        // diff/diagnostics below are anchored to it. Fail-closed: a skip keeps
        // the model's content. Formatting runs on the line-ending-normalized
        // content so the on-disk result keeps the file's convention.
        let outcome = match &self.format {
            Some(fmt) => fmt.format(&path, &model_content, None).await,
            None => crate::format::FormatOutcome::Skipped {
                text: model_content.clone(),
            },
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
                append_diagnostics(self.lsp.as_ref(), &mut output, &path, &args.path, &content)
                    .await;
                Ok(output)
            }
            Err(e) => Ok(business_error(&args.path, &e)),
        }
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

/// Normalize bare LF line endings to CRLF, preserving any existing `\r\n`.
/// Used when overwriting a CRLF file so the model's LF content adopts the
/// file's own convention instead of silently rewriting every line to LF.
fn normalize_to_crlf(content: &str) -> String {
    // Split on `\n`, strip a trailing `\r` from each piece (so an already-CRLF
    // line isn't doubled to `\r\r\n`), then rejoin with `\r\n`.
    content
        .split('\n')
        .map(|piece| piece.strip_suffix('\r').unwrap_or(piece))
        .collect::<Vec<_>>()
        .join("\r\n")
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

    /// Overwriting a CRLF file with bare-LF content keeps the file's CRLF
    /// convention — the write must not silently rewrite every line to LF.
    #[tokio::test]
    async fn overwrite_crlf_file_preserves_crlf() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\r\nb\r\n").unwrap();
        let tool = WriteTool::new(dir.path().to_path_buf());

        let out = tool.invoke(input("f.txt", "x\ny\n")).await.unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"x\r\ny\r\n",
            "LF content normalized to the file's CRLF convention"
        );
    }

    /// Overwriting an LF file keeps LF (no spurious `\r` introduced), and a
    /// brand-new file lands exactly as the model wrote it.
    #[tokio::test]
    async fn overwrite_lf_file_and_new_file_keep_lf() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lf.txt"), "a\nb\n").unwrap();
        let tool = WriteTool::new(dir.path().to_path_buf());

        let out = tool.invoke(input("lf.txt", "x\ny\n")).await.unwrap();
        assert!(!out.is_error);
        assert_eq!(std::fs::read(dir.path().join("lf.txt")).unwrap(), b"x\ny\n");

        let out = tool.invoke(input("new.txt", "p\nq\n")).await.unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read(dir.path().join("new.txt")).unwrap(),
            b"p\nq\n",
            "a new file has no convention to preserve"
        );
    }

    #[test]
    fn normalize_to_crlf_does_not_double_existing_cr() {
        assert_eq!(normalize_to_crlf("a\nb\n"), "a\r\nb\r\n");
        assert_eq!(normalize_to_crlf("a\r\nb\r\n"), "a\r\nb\r\n");
        assert_eq!(normalize_to_crlf("a\nb"), "a\r\nb");
    }

    // --- auto-format integration (`doc/lsp.md`) --------------------------

    /// A `FormatService` whose only formatter strips trailing whitespace via
    /// `sed` (a whitespace-only change that passes the fail-closed check).
    fn fmt_manager() -> std::sync::Arc<dyn crate::format::FormatService> {
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
        crate::format::ProcessFormatService::new(config, std::collections::BTreeMap::new()).unwrap()
    }

    /// An overwrite whose content carries trailing whitespace is written
    /// FORMATTED (`doc/lsp.md` §6) — the on-disk change is the formatter's.
    #[tokio::test]
    async fn formatted_write_is_applied() {
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
    }

    /// `mode = "off"` produces no `FormatService` at all (`ProcessFormatService::new`
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
            crate::format::ProcessFormatService::new(config, std::collections::BTreeMap::new())
                .is_none()
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
