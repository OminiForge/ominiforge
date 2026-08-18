//! The tool abstraction: a uniform interface over built-in (Rust) and, later,
//! MCP tools, plus the registry the agent loop queries.
//!
//! The agent loop treats every tool identically through [`Tool`]; the source
//! (built-in vs MCP) only matters for monitoring. Tools are stateless
//! request/response operations — no streaming. Output over 64 KB will spill to
//! the artifact store once that exists (Phase 2); for now it is returned
//! inline. See `doc/tool-protocol.md`.

pub(crate) mod edit;
mod error;
mod find;
mod read;
mod search;
mod shell;
pub mod web;
mod web_fetch;
mod write;

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
pub struct ToolInfo {
    /// The tool name a [`crate::permission::Rule`] targets (its `tool` field).
    pub name: String,
    /// A short human label for the card header (e.g. "Run command"). Falls back to
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
pub struct ToolField {
    /// The JSON key a rule's `field` targets (e.g. `"command"`, `"path"`).
    pub key: String,
    /// A human label for the field in the dropdown (e.g. "Command", "Path").
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
        label: format!("Path ({desc})"),
        is_path: true,
    };
    vec![
        ToolInfo {
            name: "find".to_owned(),
            label: Some("Find files".to_owned()),
            description: Some(
                "Find files in the workspace by glob pattern (respects .gitignore)".to_owned(),
            ),
            fields: vec![ToolField {
                key: "patterns".to_owned(),
                label: "Patterns".to_owned(),
                is_path: false,
            }],
        },
        ToolInfo {
            name: "search".to_owned(),
            label: Some("Search content".to_owned()),
            description: Some(
                "Search file text in the workspace by regex (respects .gitignore, skips binary)"
                    .to_owned(),
            ),
            fields: vec![
                ToolField {
                    key: "patterns".to_owned(),
                    label: "Regex".to_owned(),
                    is_path: false,
                },
                path_field("Directory to scope to"),
            ],
        },
        ToolInfo {
            name: "read".to_owned(),
            label: Some("Read file".to_owned()),
            description: Some("Read a text file in the workspace or list a directory".to_owned()),
            fields: vec![path_field("File to read")],
        },
        ToolInfo {
            name: "write".to_owned(),
            label: Some("Write file".to_owned()),
            description: Some("Write (overwrite) a text file in the workspace".to_owned()),
            fields: vec![
                path_field("File to write"),
                ToolField {
                    key: "content".to_owned(),
                    label: "Content".to_owned(),
                    is_path: false,
                },
            ],
        },
        ToolInfo {
            name: "edit".to_owned(),
            label: Some("Edit file".to_owned()),
            description: Some("Edit a file in the workspace line by line".to_owned()),
            fields: vec![path_field("File to edit")],
        },
        ToolInfo {
            name: "shell".to_owned(),
            label: Some("Run command".to_owned()),
            description: Some("Run a shell command in the workspace directory".to_owned()),
            fields: vec![ToolField {
                key: "command".to_owned(),
                label: "Command".to_owned(),
                is_path: false,
            }],
        },
        ToolInfo {
            name: "web_fetch".to_owned(),
            label: Some("Fetch web page".to_owned()),
            description: Some(
                "Fetch a URL and extract the body as markdown (egress; SSRF guard built in)"
                    .to_owned(),
            ),
            fields: vec![ToolField {
                key: "url".to_owned(),
                label: "URL".to_owned(),
                is_path: false,
            }],
        },
    ]
}

/// A single invocation request.
pub struct ToolInput {
    /// The model-assigned tool-call id (correlates result back to the call).
    pub call_id: String,
    /// The decoded arguments object.
    pub input: serde_json::Value,
    /// Wall-clock budget for this invocation.
    pub timeout: Duration,
}

/// A name-indexed set of tools.
///
/// [`descriptors`](Self::descriptors) returns them sorted by name so the tool
/// schema block sent to the model is stable, preserving prefix-cache hits
/// (`doc/design/runtime-architecture.md` §3).
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
/// Phase 1 has no OS sandbox (`doc/design/runtime-architecture.md`), so this lexical check is the
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
        // lint-english: allow — long CJK string is intentional streaming test input.
        let input = serde_json::json!({ "steps": "测试测试测试测试测试测试测试测试测试测试测试测试测试测试测试测试测试测试测试测试" }); // lint-english: allow
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
