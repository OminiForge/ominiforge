//! JSON-RPC 2.0 envelopes and the subset of MCP message shapes we use:
//! `initialize`, `tools/list`, `tools/call` (`doc/tool-protocol.md` §5.3).
//!
//! These are wire types only — serde shapes for what crosses the stdio pipe.
//! The client ([`super::client`]) owns framing, id matching, and error mapping.

use serde::{Deserialize, Serialize};

/// The MCP protocol revision we advertise in `initialize`
/// (latest spec: <https://modelcontextprotocol.io/specification/2025-11-25>).
/// The server echoes its own supported version in the handshake response; we
/// send the newest and tolerate an older reply (we only use the stable
/// `tools/list` + `tools/call` surface, unchanged across revisions).
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// A JSON-RPC request (has an `id`; expects a response).
#[derive(Debug, Serialize)]
pub struct Request<'a> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl<'a> Request<'a> {
    /// A request with the `2.0` version stamped.
    pub const fn new(id: u64, method: &'a str, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// A JSON-RPC notification (no `id`; no response expected).
#[derive(Debug, Serialize)]
pub struct Notification<'a> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl<'a> Notification<'a> {
    pub const fn new(method: &'a str, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

/// A JSON-RPC response: exactly one of `result` / `error` is present, keyed to
/// a request by `id`. `id` is optional so a malformed/notification line decodes
/// without erroring (the client skips non-matching lines).
#[derive(Debug, Deserialize)]
pub struct Response {
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// A JSON-RPC error object.
#[derive(Debug, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// One tool as returned by `tools/list`.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema for the tool's arguments. MCP calls it `inputSchema`.
    #[serde(rename = "inputSchema", default)]
    pub input_schema: serde_json::Value,
}

/// The `tools/list` result envelope.
#[derive(Debug, Deserialize)]
pub struct ToolsListResult {
    #[serde(default)]
    pub tools: Vec<ToolDef>,
}

/// The `tools/call` result envelope: a content array plus an error flag.
#[derive(Debug, Deserialize)]
pub struct ToolCallResult {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    /// MCP marks a business-level failure with `isError: true`.
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// One block of `tools/call` content. Only text is mapped richly; other kinds
/// are preserved as their JSON text so nothing is silently dropped.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    /// Any non-text block (image, resource, ...) — kept as raw JSON for now.
    #[serde(other)]
    Other,
}
