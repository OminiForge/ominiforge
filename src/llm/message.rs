//! Provider-neutral message types the agent loop works with.
//!
//! These are the conversation primitives sent to a [`super::Provider`]. Each
//! provider adapter converts them to and from its own wire format; nothing
//! here is OpenAI- or Anthropic-specific.

use serde::{Deserialize, Serialize};

/// One message in a conversation, in provider-neutral form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// The system prompt / agent identity.
    System { content: String },
    /// User input.
    User { content: String },
    /// A model turn: free-text content and/or tool calls.
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    /// The result of running a tool, fed back to the model.
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// A tool invocation requested by the model. `arguments` is the raw JSON string
/// as produced by the model (parsed by the tool dispatcher, not here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// The schema of a tool advertised to the model. `parameters` is a JSON Schema
/// object. See `doc/tool-protocol.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A single request to a model: the conversation plus generation parameters.
///
/// Tool schemas are kept sorted by name by the caller (the agent loop) to
/// preserve prefix-cache hits; this type does not reorder them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSchema>,
    pub temperature: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}
