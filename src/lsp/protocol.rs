//! JSON-RPC 2.0 envelopes and the subset of LSP message shapes we use:
//! `initialize` and `textDocument/publishDiagnostics`.
//!
//! These are wire types only — serde shapes for what crosses the
//! `Content-Length`-framed stdio pipe. The client ([`super::client`]) owns
//! framing, message-kind classification, and id matching.
//!
//! Unlike MCP (request → response, always initiated by us), an LSP server also
//! sends us unsolicited **notifications** (`publishDiagnostics`, ...) and
//! occasionally **requests** of its own (`workspace/configuration`,
//! `client/registerCapability`, ...) that we must answer or it may stall
//! waiting for a reply. [`Incoming`] captures a message generically so the
//! client can tell the three kinds apart before decoding further; deliberately
//! minimal so later ops (`definition`, `references`, `hover`, ...) add fields
//! without breaking this shape.

use serde::{Deserialize, Serialize};

/// An outgoing JSON-RPC request we originate (id is one of ours, an
/// ever-increasing `u64`).
#[derive(Debug, Serialize)]
pub struct Request<'a> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl<'a> Request<'a> {
    pub const fn new(id: u64, method: &'a str, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// An outgoing JSON-RPC notification (no `id`; no response expected) —
/// `initialized`, `textDocument/didOpen`, `textDocument/didChange`.
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

/// An outgoing reply to a request **the server sent us** (e.g.
/// `workspace/configuration`). We don't implement any server-initiated request
/// today, so replies are empty: `result: null` — or, for
/// `workspace/configuration`, whose spec'd response shape is one entry per
/// requested item, an array of nulls. Just enough for the server to stop
/// waiting on us.
#[derive(Debug, Serialize)]
pub struct OutgoingResponse {
    pub jsonrpc: &'static str,
    pub id: serde_json::Value,
    pub result: serde_json::Value,
}

impl OutgoingResponse {
    /// A reply carrying `result`, echoing back the server's own `id` verbatim
    /// (number or string — not ours to interpret).
    pub const fn with_result(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result,
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// One inbound line, decoded loosely enough to classify before acting on it.
/// Which fields are present distinguishes the three LSP message kinds:
///
/// | `id` | `method` | kind |
/// |---|---|---|
/// | present | present | a request **from the server** — must be answered |
/// | absent | present | a notification (`publishDiagnostics`, ...) |
/// | present | absent | a response to one of **our** requests |
#[derive(Debug, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

impl Incoming {
    /// The `id` as one of ours (a `u64`), for correlating a response to the
    /// request we sent. `None` for a notification or a server-initiated
    /// request (whose `id`, if any, is not ours to match against).
    #[must_use]
    pub fn our_request_id(&self) -> Option<u64> {
        self.id.as_ref()?.as_u64()
    }
}

/// The subset of the `initialize` result we read: the server's negotiated
/// position encoding (`doc/lsp.md` §4 performance model — we request
/// `utf-8` to avoid UTF-16 offset math; a server that ignores the request
/// falls back to its LSP-mandated default of `utf-16`).
#[derive(Debug, Default, Deserialize)]
pub struct InitializeResult {
    #[serde(default)]
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Default, Deserialize)]
pub struct ServerCapabilities {
    #[serde(default, rename = "positionEncoding")]
    pub position_encoding: Option<String>,
}

/// `textDocument/publishDiagnostics` params.
#[derive(Debug, Clone, Deserialize)]
pub struct PublishDiagnosticsParams {
    pub uri: String,
    /// The document version these diagnostics were computed against, when the
    /// server sends one — used to reject diagnostics older than our last edit
    /// (`doc/lsp.md` §4 performance model, version gating).
    #[serde(default)]
    pub version: Option<i64>,
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
}

/// One diagnostic (a subset of the LSP `Diagnostic` shape — just what we
/// render).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range,
    #[serde(default)]
    pub severity: Option<DiagnosticSeverity>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// 0-based line/character, per LSP. Character units follow the negotiated
/// position encoding (`utf-8` when the server honors our request).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// LSP encodes severity as `1..=4`; hand-rolled since we don't depend on
/// `serde_repr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl DiagnosticSeverity {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "info",
            Self::Hint => "hint",
        }
    }
}

impl<'de> Deserialize<'de> for DiagnosticSeverity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match u8::deserialize(deserializer)? {
            1 => Ok(Self::Error),
            2 => Ok(Self::Warning),
            3 => Ok(Self::Information),
            4 => Ok(Self::Hint),
            other => Err(serde::de::Error::custom(format!(
                "invalid LSP diagnostic severity: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// `Incoming` correctly classifies all three message kinds by which of
    /// `id`/`method` are present — the client's whole dispatch hinges on this.
    #[test]
    fn incoming_classifies_by_id_and_method_presence() {
        let request: Incoming = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":7,"method":"workspace/configuration","params":{}}"#,
        )
        .unwrap();
        assert!(request.method.is_some() && request.id.is_some());

        let notification: Incoming = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{}}"#,
        )
        .unwrap();
        assert!(notification.method.is_some() && notification.id.is_none());

        let response: Incoming =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#).unwrap();
        assert!(response.method.is_none());
        assert_eq!(response.our_request_id(), Some(3));
    }

    /// A full `publishDiagnostics` payload decodes, including the numeric
    /// severity mapping to our enum.
    #[test]
    fn publish_diagnostics_decodes() {
        let params: PublishDiagnosticsParams = serde_json::from_str(
            r#"{
                "uri": "file:///tmp/lib.rs",
                "version": 2,
                "diagnostics": [
                    {
                        "range": {"start": {"line": 4, "character": 5}, "end": {"line": 4, "character": 9}},
                        "severity": 1,
                        "message": "mismatched types"
                    }
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(params.uri, "file:///tmp/lib.rs");
        assert_eq!(params.version, Some(2));
        assert_eq!(params.diagnostics.len(), 1);
        assert_eq!(
            params.diagnostics[0].severity,
            Some(DiagnosticSeverity::Error)
        );
        assert_eq!(params.diagnostics[0].range.start.line, 4);
    }

    /// A diagnostic with no `severity` (optional per spec) still decodes.
    #[test]
    fn diagnostic_severity_is_optional() {
        let d: Diagnostic = serde_json::from_str(
            r#"{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "message": "note"}"#,
        )
        .unwrap();
        assert!(d.severity.is_none());
    }
}
