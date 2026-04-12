//! The [`LlmBackend`] trait — abstract LLM inference provider.
//!
//! Implementors live in `agnt-net` (HTTP backends like Ollama/OpenAI/Anthropic)
//! or can be user-provided (WASM-side fetch, local Candle, mock for testing).

use crate::message::Message;
use serde_json::Value;

/// Error returned by [`LlmBackend::chat`].
#[derive(Debug, Clone)]
pub enum BackendError {
    /// Transport-level failure (network, DNS, TLS handshake).
    Transport(String),
    /// HTTP status error from an upstream API.
    Http { code: u16, body: String },
    /// Response parsing failure (malformed JSON, missing fields).
    Parse(String),
    /// All retries exhausted.
    Retry(String),
    /// Provider returned a structured error (rate limit, auth, etc.).
    Provider(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {}", e),
            Self::Http { code, body } => write!(f, "http {}: {}", code, body),
            Self::Parse(e) => write!(f, "parse: {}", e),
            Self::Retry(e) => write!(f, "retry: {}", e),
            Self::Provider(e) => write!(f, "provider: {}", e),
        }
    }
}

impl std::error::Error for BackendError {}

/// Abstract LLM backend.
///
/// Implementors translate the internal OpenAI-flavored [`Message`] format to
/// whatever wire format the provider expects, stream the response, parse it
/// back into a [`Message`], and return. The optional `on_token` callback is
/// invoked as text tokens arrive so callers can render progressively.
pub trait LlmBackend: Send + Sync {
    /// The model identifier this backend is configured for (e.g. `gemma4:e4b`).
    fn model(&self) -> &str;

    /// Run one inference turn with the given conversation and tool schemas.
    ///
    /// `tools` is a JSON array in OpenAI's `function` tool format; backends
    /// translate to provider-native format as needed.
    ///
    /// If `on_token` is `Some`, text deltas are pushed into it as they arrive
    /// from the wire.
    fn chat(
        &self,
        messages: &[Message],
        tools: &Value,
        on_token: Option<&mut dyn FnMut(&str)>,
    ) -> Result<Message, BackendError>;
}
