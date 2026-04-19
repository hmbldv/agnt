//! Shared daemon state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agnt_core::{Agent, AgentBuilder, LlmBackend, Registry};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::Config;

/// Maximum byte length of a session ID. Prevents excessively long IDs from
/// consuming HashMap key memory or leaking into logs without bound.
const MAX_SESSION_ID_LEN: usize = 64;

/// Hard cap on concurrent sessions. Prevents a session-ID flood from growing
/// the HashMap without bound and exhausting memory.
const MAX_SESSIONS: usize = 10_000;

/// Validate a session ID. Allowed characters: ASCII alphanumeric, hyphen, underscore.
/// Returns `false` for empty strings or IDs that exceed `MAX_SESSION_ID_LEN`.
fn validate_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_LEN
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Shared state across all HTTP handlers.
pub struct DmnState<B: LlmBackend> {
    pub config: Config,
    pub machine: String,
    pub started_at: DateTime<Utc>,
    pub agent_factory: AgentFactory<B>,
    pub sessions: Mutex<HashMap<String, SessionInfo>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub request_count: u64,
}

/// Factory for creating configured Agent instances.
pub struct AgentFactory<B: LlmBackend> {
    pub backend: B,
    pub registry: Arc<Registry>,
    pub store_path: Option<String>,
}

impl<B: LlmBackend + Clone + 'static> AgentFactory<B> {
    /// Create a new agent for a session, optionally with persistence.
    pub fn create(&self, session_id: &str, system_prompt: Option<&str>) -> Agent<B> {
        let prompt = system_prompt.unwrap_or("You are a helpful assistant.");
        let mut builder = AgentBuilder::new(self.backend.clone())
            .system(prompt)
            .tools(self.registry.make_proxies());

        // Attach store if configured
        if let Some(ref path) = self.store_path {
            if let Ok(store) = agnt_store::Store::open(path) {
                builder = builder.store(Arc::new(store), session_id);
            }
        }

        builder.build().expect("failed to build agent")
    }
}

impl<B: LlmBackend> DmnState<B> {
    /// Retrieve or create a session by ID.
    ///
    /// Returns `Err(BAD_REQUEST)` if the ID is invalid (empty, too long, or
    /// contains non-alphanumeric/hyphen/underscore characters).
    ///
    /// Returns `Err(TOO_MANY_REQUESTS)` if the session cap has been reached
    /// and the requested ID does not already exist.
    pub fn get_or_create_session(&self, session_id: &str) -> Result<String, StatusCode> {
        if !validate_session_id(session_id) {
            return Err(StatusCode::BAD_REQUEST);
        }

        let mut sessions = self.sessions.lock().unwrap();

        // Enforce cap only for *new* sessions — existing ones may continue.
        if sessions.len() >= MAX_SESSIONS && !sessions.contains_key(session_id) {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }

        let now = Utc::now();
        sessions
            .entry(session_id.to_string())
            .and_modify(|s| {
                s.last_activity = now;
                s.request_count += 1;
            })
            .or_insert_with(|| SessionInfo {
                id: session_id.to_string(),
                created_at: now,
                last_activity: now,
                request_count: 1,
            });
        Ok(session_id.to_string())
    }
}
