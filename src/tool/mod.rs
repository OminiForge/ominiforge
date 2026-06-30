//! The tool abstraction: a uniform interface over built-in (Rust) and, later,
//! MCP tools, plus the registry the agent loop queries.
//!
//! The agent loop treats every tool identically through [`Tool`]; the source
//! (built-in vs MCP) only matters for monitoring. Tools are stateless
//! request/response operations — no streaming. Output over 64 KB will spill to
//! the artifact store once that exists (Phase 2); for now it is returned
//! inline. See `doc/tool-protocol.md`.

mod edit;
mod error;
mod read;
mod shell;
mod snapshot;
mod write;

pub use edit::EditTool;
pub use error::ToolError;
pub use read::ReadTool;
pub use shell::ShellTool;
pub use snapshot::SnapshotStore;
pub use write::WriteTool;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::core::payload::{ToolOutput, ToolSource};

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

/// What the model is told about a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input object.
    pub input_schema: serde_json::Value,
}

/// A single invocation request.
#[derive(Debug, Clone)]
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
fn resolve_in_workspace(workspace: &Path, requested: &str) -> Result<PathBuf, ToolError> {
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

/// Register the built-in tools (read, write, edit, shell), all scoped to `workspace`.
///
/// `read` and `edit` share one [`SnapshotStore`] so an `edit` patch is verified
/// against the snapshot the preceding `read` recorded.
///
/// TODO: The `SnapshotStore` wiring here mirrors `register_profile_tools` in
/// `app.rs`. If a third tool needs the store, extract a shared helper rather
/// than duplicating the wiring a third time.
pub fn register_builtin(registry: &mut ToolRegistry, workspace: PathBuf) {
    let snapshots = SnapshotStore::new();
    registry.register(Arc::new(ReadTool::new(
        workspace.clone(),
        snapshots.clone(),
    )));
    registry.register(Arc::new(WriteTool::new(workspace.clone())));
    registry.register(Arc::new(EditTool::new(workspace.clone(), snapshots)));
    registry.register(Arc::new(ShellTool::new(workspace)));
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
        assert_eq!(names, vec!["edit", "read", "shell", "write"]);
    }

    #[test]
    fn resolve_rejects_parent_escape() {
        let ws = Path::new("/home/user/project");
        assert!(resolve_in_workspace(ws, "../secret").is_err());
        assert!(resolve_in_workspace(ws, "src/../../etc/passwd").is_err());
    }

    #[test]
    fn resolve_allows_paths_within_workspace() {
        let ws = Path::new("/home/user/project");
        let resolved = resolve_in_workspace(ws, "src/main.rs").unwrap();
        assert_eq!(resolved, Path::new("/home/user/project/src/main.rs"));
    }
}
