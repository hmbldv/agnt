//! Shared daemon state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agnt_core::{Agent, AgentBuilder, LlmBackend, Registry};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::Config;

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
            .system(prompt);

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
    pub fn get_or_create_session(&self, session_id: &str) -> String {
        let mut sessions = self.sessions.lock().unwrap();
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
        session_id.to_string()
    }
}
