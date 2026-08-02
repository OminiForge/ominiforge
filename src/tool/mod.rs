//! The tool abstraction: a uniform interface over built-in (Rust) and, later,
//! MCP tools, plus the registry the agent loop queries.
//!
//! The agent loop treats every tool identically through [`Tool`]; the source
//! (built-in vs MCP) only matters for monitoring. Tools are stateless
//! request/response operations — no streaming. Output over 64 KB will spill to
//! the artifact store once that exists (Phase 2); for now it is returned
//! inline. See `doc/tool-protocol.md`.

pub(crate) mod diffview;
pub(crate) mod edit;
mod edit_stream;
mod error;
mod find;
mod read;
mod search;
mod shell;
pub mod stream_args;
mod terminal;
pub mod web;
mod web_fetch;
mod write;
mod write_stream;

pub use edit::EditTool;
pub use error::ToolError;
pub use find::FindTool;
pub use read::ReadTool;
pub use search::SearchTool;
pub use shell::ShellTool;
pub use web::WebFetchPolicy;
pub use web_fetch::WebFetchTool;
pub use write::WriteTool;

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::core::payload::{Content, ToolOutput, ToolSource};
use crate::lsp::LspManager;

/// Append LSP diagnostics for `abs_path` (with current `text`) to `output`, if a
/// language server handles it. Best-effort: a missing manager, unsupported
/// extension, or a server that yields nothing all leave `output` untouched. The
/// diagnostics ride on a *successful* file op — see the assist design in
/// `doc/lsp.md`. `label` is the workspace-relative path the tool
/// already prints, so the diagnostics block names the same file the model sees.
async fn append_diagnostics(
    lsp: Option<&Arc<LspManager>>,
    output: &mut ToolOutput,
    abs_path: &Path,
    label: &str,
    text: &str,
) {
    let Some(lsp) = lsp else { return };
    if let Some(diagnostics) = lsp.diagnostics(abs_path, text).await {
        let block = crate::lsp::render_diagnostics(label, &diagnostics);
        if !block.is_empty() {
            output.content.push(Content::Text(block));
        }
    }
}

/// The outcome of a tool invocation: either a [`ToolOutput`] (possibly a
/// business-level error) or a protocol-level [`ToolError`].
pub type ToolResult = Result<ToolOutput, ToolError>;

/// A callable tool. Built-in tools implement this directly; the MCP adapter
/// implements it over a JSON-RPC server.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// The schema advertised to the model.
    fn descriptor(&self) -> ToolDescriptor;

    /// Where the tool comes from, for source-aware monitoring. Defaults to
    /// [`ToolSource::Builtin`]; the MCP adapter overrides it with the server
    /// name (`doc/tool-protocol.md` §9).
    fn source(&self) -> ToolSource {
        ToolSource::Builtin
    }

    /// Execute the tool to completion.
    async fn invoke(&self, input: ToolInput) -> ToolResult;

    /// A streaming presenter for stage 2 of the tool-call pipeline
    /// (`doc/tool-streaming.md`): turns this call's args into render-ready view
    /// snapshots as they stream in. The default is `None` — the card shows the
    /// block-start skeleton and jumps straight to the settled view at stage 3,
    /// which is the correct behavior for most tools (small args: read/find/
    /// shell; non-renderable: todo/MCP). Override ONLY for tools with large
    /// streamed args where a live view adds real value (currently: `write`).
    ///
    /// A fresh presenter is created per tool call (it may hold per-call state,
    /// e.g. a cached pre-edit file snapshot).
    fn stream_presenter(&self) -> Option<Box<dyn StreamPresenter>> {
        None
    }
}

/// Turns a tool call's accumulated raw args into a render-ready view snapshot.
///
/// The args arrive as a growing JSON string, possibly truncated mid-token. One
/// presenter instance per tool call, driven by the collector under throttle
/// (`doc/tool-streaming.md` §4). Async so a presenter may read the pre-edit
/// file once (lazily, cached) before it can diff.
///
/// Contract:
/// - `accumulated_args` is the FULL args text so far, never a delta — snapshots
///   are self-contained so the gateway may coalesce (drop stale, keep newest).
/// - The output is the SAME `TextView` envelope as stage 3's
///   `preview()`/`invoke` view, so the front-end renders stage 2 and stage 3
///   with one code path.
/// - Return `None` when the args aren't yet renderable (e.g. `path` still
///   streaming); the caller keeps the last good snapshot.
/// - Must be cheap: this runs on the model stream's hot path under throttle.
#[async_trait::async_trait]
pub trait StreamPresenter: Send {
    /// Render a snapshot from the accumulated args, or `None` if not yet
    /// renderable.
    async fn render(&mut self, accumulated_args: &str) -> Option<String>;
}

/// Static dispatch for `summarize` by tool name.
///
/// Used by the collector (which has no tool instance) to fill
/// `BlockContent::ToolCall.summary`. Built-ins declare their primary field;
/// MCP tools fall back to the default truncated JSON dump.
#[must_use]
pub fn summarize_by_name(name: &str, input: &serde_json::Value) -> String {
    match name {
        "read" | "write" => input
            .get("path")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_owned(),
        "edit" => {
            if let Some(path) = input.get("path").and_then(|p| p.as_str()) {
                return path.to_owned();
            }
            if let Some(path) = input
                .get("edits")
                .and_then(|e| e.as_array())
                .and_then(|edits| edits.first())
                .and_then(|first| first.get("path"))
                .and_then(|p| p.as_str())
            {
                return path.to_owned();
            }
            String::new()
        }
        "shell" => input
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_owned(),
        "find" => input
            .get("patterns")
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        "search" => input
            .get("patterns")
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        "web_fetch" => input
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_owned(),
        _ => {
            let s = input.to_string();
            if s.len() > 80 {
                // Byte-slice at a char boundary: tool input can be arbitrary
                // UTF-8 (e.g. CJK todo steps), and `&s[..80]` panics when 80
                // lands inside a multi-byte char.
                let end = s
                    .char_indices()
                    .map(|(i, _)| i)
                    .take_while(|&i| i <= 80)
                    .last()
                    .unwrap_or(0);
                format!("{}…", &s[..end])
            } else {
                s
            }
        }
    }
}

/// What the model is told about a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input object.
    pub input_schema: serde_json::Value,
}

/// A human-facing description of a tool for the permission-config UI.
///
/// This is NOT what the model sees (that is [`ToolDescriptor`]) — it is the
/// metadata the front-end turns into a per-tool gating card (`doc/permission.md`
/// §3.2): a friendly label, the one-line purpose, and which input fields a rule
/// may target (so the user picks a field from a dropdown instead of typing a
/// JSON path they cannot know).
///
/// Serialized to the front-end via `GET /tools`; only built-in tools have a
/// hand-authored catalog entry today. A tool with no entry (e.g. an MCP tool)
/// still gates fine — the UI falls back to a generic "whole input" card.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct ToolInfo {
    /// The tool name a [`crate::permission::Rule`] targets (its `tool` field).
    pub name: String,
    /// A short human label for the card header (e.g. "运行命令"). Falls back to
    /// `name` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// One-line purpose, shown under the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The input fields a rule may scope to. Empty = only whole-input rules make
    /// sense for this tool (the UI shows no field dropdown).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<ToolField>,
}

/// One targetable input field of a tool, for the config UI's field dropdown.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct ToolField {
    /// The JSON key a rule's `field` targets (e.g. `"command"`, `"path"`).
    pub key: String,
    /// A human label for the field in the dropdown (e.g. "命令", "路径").
    pub label: String,
    /// Whether this field holds a filesystem path — the UI offers prefix
    /// (directory allow/deny-list) controls for it, not just substring.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_path: bool,
}

/// The permission-config catalog for the built-in tools.
///
/// Static and always available (`doc/permission.md` §3.2) — unlike MCP tools,
/// the built-ins need no subprocess to enumerate, so `GET /tools` can serve this
/// without a workspace context. Ordered find → read → write → edit → shell
/// (roughly least → most powerful), the order the cards render in.
#[must_use]
pub fn builtin_catalog() -> Vec<ToolInfo> {
    let path_field = |desc: &str| ToolField {
        key: "path".to_owned(),
        label: format!("路径（{desc}）"),
        is_path: true,
    };
    vec![
        ToolInfo {
            name: "find".to_owned(),
            label: Some("找文件".to_owned()),
            description: Some("按 glob 通配符查找工作区内的文件（遵循 .gitignore）".to_owned()),
            fields: vec![ToolField {
                key: "patterns".to_owned(),
                label: "通配符".to_owned(),
                is_path: false,
            }],
        },
        ToolInfo {
            name: "search".to_owned(),
            label: Some("搜内容".to_owned()),
            description: Some(
                "按正则搜索工作区内文件的文本内容（遵循 .gitignore，跳过二进制）".to_owned(),
            ),
            fields: vec![
                ToolField {
                    key: "patterns".to_owned(),
                    label: "正则".to_owned(),
                    is_path: false,
                },
                path_field("限定的目录"),
            ],
        },
        ToolInfo {
            name: "read".to_owned(),
            label: Some("读文件".to_owned()),
            description: Some("读取工作区内的文本文件或列目录".to_owned()),
            fields: vec![path_field("读取的文件")],
        },
        ToolInfo {
            name: "write".to_owned(),
            label: Some("写文件".to_owned()),
            description: Some("写入（覆盖）工作区内的文本文件".to_owned()),
            fields: vec![
                path_field("写入的文件"),
                ToolField {
                    key: "content".to_owned(),
                    label: "内容".to_owned(),
                    is_path: false,
                },
            ],
        },
        ToolInfo {
            name: "edit".to_owned(),
            label: Some("改文件".to_owned()),
            description: Some("按行编辑工作区内的文件".to_owned()),
            fields: vec![path_field("编辑的文件")],
        },
        ToolInfo {
            name: "shell".to_owned(),
            label: Some("运行命令".to_owned()),
            description: Some("在工作区目录执行 shell 命令".to_owned()),
            fields: vec![ToolField {
                key: "command".to_owned(),
                label: "命令".to_owned(),
                is_path: false,
            }],
        },
        ToolInfo {
            name: "web_fetch".to_owned(),
            label: Some("抓网页".to_owned()),
            description: Some(
                "抓取 URL 并提取正文为 markdown（出网访问，SSRF 防护内置）".to_owned(),
            ),
            fields: vec![ToolField {
                key: "url".to_owned(),
                label: "URL".to_owned(),
                is_path: false,
            }],
        },
    ]
}

/// A single invocation request. Not `Clone`: the optional `progress` sink is
/// a one-shot callback. `Debug` is manual to skip that sink.
pub struct ToolInput {
    /// The model-assigned tool-call id (correlates result back to the call).
    pub call_id: String,
    /// The decoded arguments object.
    pub input: serde_json::Value,
    /// Wall-clock budget for this invocation.
    pub timeout: Duration,
    /// Optional live-progress sink for tools that stream RESULTS mid-execution
    /// (currently `shell` output, `doc/tool-streaming.md` §5). The tool calls it
    /// with self-contained view snapshots (the same envelope as the settled
    /// view); the agent wires it to `StreamSink::on_tool_call_progress`. `None`
    /// (tests, headless) means no live frames — the settled view at stage 3 is
    /// unaffected. Distinct from `stream_presenter`, which streams ARGS before
    /// execution; this streams the RESULT during it.
    pub progress: Option<Box<dyn FnMut(String) + Send>>,
}

impl std::fmt::Debug for ToolInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolInput")
            .field("call_id", &self.call_id)
            .field("input", &self.input)
            .field("timeout", &self.timeout)
            .field("progress", &self.progress.as_ref().map(|_| "<sink>"))
            .finish()
    }
}

/// A name-indexed set of tools.
///
/// [`descriptors`](Self::descriptors) returns them sorted by name so the tool
/// schema block sent to the model is stable, preserving prefix-cache hits
/// (`doc/context-management.md` §3).
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool under its descriptor name, replacing any prior tool of
    /// the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.descriptor().name;
        self.tools.insert(name, tool);
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// A streaming presenter for `name`'s next call, or `None` if the tool has
    /// no stage-2 streaming (the default — `doc/tool-streaming.md` §4).
    #[must_use]
    pub fn stream_presenter(&self, name: &str) -> Option<Box<dyn StreamPresenter>> {
        self.tools.get(name).and_then(|t| t.stream_presenter())
    }

    /// The [`ToolSource`] of a registered tool, or [`ToolSource::Builtin`] if
    /// the name is unknown (the loop reports the started event before confirming
    /// the tool exists; an unknown name is treated as builtin for that event).
    #[must_use]
    pub fn source_of(&self, name: &str) -> ToolSource {
        self.tools
            .get(name)
            .map_or(ToolSource::Builtin, |t| t.source())
    }

    /// Whether the registry holds no tools.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Tool descriptors, sorted by name for prefix-cache stability.
    #[must_use]
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut descriptors: Vec<ToolDescriptor> =
            self.tools.values().map(|t| t.descriptor()).collect();
        descriptors.sort_by(|a, b| a.name.cmp(&b.name));
        descriptors
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&String> = self.tools.keys().collect();
        names.sort();
        f.debug_struct("ToolRegistry")
            .field("tools", &names)
            .finish()
    }
}

/// Resolve a model-supplied path against the workspace, refusing anything that
/// escapes it.
///
/// Phase 1 has no OS sandbox (`doc/sandbox.md`), so this lexical check is the
/// guard rail for the file tools: components are normalized without touching
/// the filesystem (so it works for not-yet-created files), and any `..` that
/// would climb above the workspace root is rejected.
pub(crate) fn resolve_in_workspace(
    workspace: &Path,
    requested: &str,
) -> Result<PathBuf, ToolError> {
    let joined = workspace.join(requested);
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(ToolError::InvalidInput(format!(
                        "path escapes workspace: {requested}"
                    )));
                }
            }
            Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    if !normalized.starts_with(workspace) {
        return Err(ToolError::InvalidInput(format!(
            "path escapes workspace: {requested}"
        )));
    }
    Ok(normalized)
}

/// Register the built-in tools (find, search, read, write, edit, shell,
/// `web_fetch`), all scoped to `workspace`.
pub fn register_builtin(registry: &mut ToolRegistry, workspace: PathBuf) {
    registry.register(Arc::new(FindTool::new(workspace.clone())));
    registry.register(Arc::new(SearchTool::new(workspace.clone())));
    registry.register(Arc::new(ReadTool::new(workspace.clone())));
    registry.register(Arc::new(WriteTool::new(workspace.clone())));
    registry.register(Arc::new(EditTool::new(workspace.clone())));
    registry.register(Arc::new(ShellTool::new(Arc::new(
        crate::sandbox::passthrough::PassthroughSandbox::new(workspace.clone(), BTreeMap::new()),
    ))));
    registry.register(Arc::new(WebFetchTool::new(workspace)));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn descriptors_are_sorted_by_name() {
        let mut reg = ToolRegistry::new();
        register_builtin(&mut reg, PathBuf::from("/tmp/ws"));
        let names: Vec<String> = reg.descriptors().into_iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec![
                "edit",
                "find",
                "read",
                "search",
                "shell",
                "web_fetch",
                "write"
            ]
        );
    }

    #[test]
    fn resolve_rejects_parent_escape() {
        let ws = Path::new("/home/user/project");
        assert!(resolve_in_workspace(ws, "../secret").is_err());
        assert!(resolve_in_workspace(ws, "src/../../etc/passwd").is_err());
    }

    #[test]
    fn summarize_truncates_multibyte_without_panic() {
        // Regression: `&s[..80]` panicked when byte 80 fell inside a CJK char
        // ("end byte index 80 is not a char boundary"). The summary is a
        // human-facing label; it must degrade gracefully, not kill the task.
        let input = serde_json::json!({ "steps": "测试测试测试测试测试测试测试测试测试测试测试测试测试测试测试测试测试测试测试测试" });
        let summary = summarize_by_name("todo", &input);
        assert!(summary.len() <= 80 + '…'.len_utf8());
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn resolve_allows_paths_within_workspace() {
        let ws = Path::new("/home/user/project");
        let resolved = resolve_in_workspace(ws, "src/main.rs").unwrap();
        assert_eq!(resolved, Path::new("/home/user/project/src/main.rs"));
    }
}
