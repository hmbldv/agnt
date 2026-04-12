//! The [`MessageStore`] trait — abstract session persistence.
//!
//! Implementors live in `agnt-store` (SQLite) or can be user-provided
//! (in-memory for tests, Postgres, Redis, key-value store, etc.).

use crate::message::Message;

/// Error returned by [`MessageStore`] operations.
#[derive(Debug, Clone)]
pub enum StoreError {
    /// Failed to open or connect to the store.
    Open(String),
    /// IO or query failure at runtime.
    Io(String),
    /// Serialization failure encoding/decoding a message.
    Serde(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(e) => write!(f, "store open: {}", e),
            Self::Io(e) => write!(f, "store io: {}", e),
            Self::Serde(e) => write!(f, "store serde: {}", e),
        }
    }
}

impl std::error::Error for StoreError {}

/// A tool execution record to persist.
#[derive(Debug, Clone)]
pub struct ToolLog<'a> {
    pub name: &'a str,
    pub args: &'a str,
    pub result: &'a str,
    pub duration_us: u64,
}

/// Abstract session persistence.
///
/// Implementors provide durable storage for conversation history and tool
/// execution logs, keyed by a session identifier.
pub trait MessageStore: Send + Sync {
    /// Load all messages for a session in insertion order.
    fn load(&self, session: &str) -> Result<Vec<Message>, StoreError>;

    /// Append one message to the session log.
    fn append(&self, session: &str, message: &Message) -> Result<(), StoreError>;

    /// Record a tool execution with its args, result, and duration.
    fn log_tool(&self, session: &str, log: &ToolLog<'_>) -> Result<(), StoreError>;

    /// Wipe all messages and tool logs for a session.
    fn clear(&self, session: &str) -> Result<(), StoreError>;
}
