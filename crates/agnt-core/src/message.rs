//! Message types shared between backend, agent loop, and store.

use serde::{Deserialize, Serialize};

/// Token usage reported by the backend for a single inference call.
///
/// Fields are zero when the backend didn't surface usage data (e.g. when
/// streaming is enabled and the provider omits the final usage event).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageStats {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

impl UsageStats {
    pub fn total(&self) -> u32 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// A conversation message in the OpenAI-flavored internal format.
///
/// Backends that use a different wire format (e.g. Anthropic's content blocks)
/// translate to/from this type at the wire boundary.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Token usage for this inference call. Not serialized into chat history —
    /// only present on assistant messages returned directly from the backend.
    #[serde(skip)]
    pub usage: Option<UsageStats>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_tc_type")]
    pub call_type: String,
    pub function: FunctionCall,
}

fn default_tc_type() -> String {
    "function".into()
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}
