//! HTTP request handlers.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use agnt_core::LlmBackend;
use crate::state::DmnState;

// --- Health & Status ---

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub machine: String,
    pub uptime_seconds: i64,
    pub model: String,
    pub sessions: usize,
}

pub async fn health<B: LlmBackend + Clone + 'static>(
    State(state): State<Arc<DmnState<B>>>,
) -> Json<HealthResponse> {
    let uptime = (chrono::Utc::now() - state.started_at).num_seconds();
    let session_count = state.sessions.lock().unwrap().len();
    Json(HealthResponse {
        status: "ok".into(),
        machine: state.machine.clone(),
        uptime_seconds: uptime,
        model: state.config.model.clone(),
        sessions: session_count,
    })
}

// --- Step (agentic turn) ---

#[derive(Deserialize)]
pub struct StepRequest {
    pub prompt: String,
    #[serde(default = "default_session")]
    pub session_id: String,
    pub system_prompt: Option<String>,
}

fn default_session() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Serialize)]
pub struct StepResponse {
    pub session_id: String,
    pub response: String,
}

pub async fn step<B: LlmBackend + Clone + 'static>(
    State(state): State<Arc<DmnState<B>>>,
    Json(req): Json<StepRequest>,
) -> Result<Json<StepResponse>, (StatusCode, String)> {
    let session = state.get_or_create_session(&req.session_id);

    // Create agent in a blocking task (agnt-core is sync)
    let state_clone = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut agent = state_clone.agent_factory.create(
            &session.id,
            req.system_prompt.as_deref(),
        );

        match agent.step(&req.prompt) {
            Ok(response) => Ok(StepResponse {
                session_id: session.id,
                response,
            }),
            Err(e) => Err(format!("agent step failed: {}", e)),
        }
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task join: {}", e)))?;

    result.map(Json).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))
}

// --- Tool dispatch (direct, no inference) ---

#[derive(Deserialize)]
pub struct ToolRequest {
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Serialize)]
pub struct ToolResponse {
    pub name: String,
    pub result: String,
    pub is_error: bool,
}

pub async fn tool<B: LlmBackend + Clone + 'static>(
    State(state): State<Arc<DmnState<B>>>,
    Json(req): Json<ToolRequest>,
) -> Result<Json<ToolResponse>, (StatusCode, String)> {
    let registry = state.agent_factory.registry.clone();

    let result = tokio::task::spawn_blocking(move || {
        match registry.dispatch(&req.name, req.args) {
            Ok(output) => ToolResponse {
                name: req.name,
                result: output,
                is_error: false,
            },
            Err(err) => ToolResponse {
                name: req.name,
                result: err,
                is_error: true,
            },
        }
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task join: {}", e)))?;

    Ok(Json(result))
}

// --- Session info ---

pub async fn sessions<B: LlmBackend + Clone + 'static>(
    State(state): State<Arc<DmnState<B>>>,
) -> Json<Vec<crate::state::SessionInfo>> {
    let sessions = state.sessions.lock().unwrap();
    let mut list: Vec<_> = sessions.values().cloned().collect();
    list.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
    Json(list)
}

// --- Tool listing ---

#[derive(Serialize)]
pub struct ToolInfo {
    pub name: String,
}

pub async fn tools<B: LlmBackend + Clone + 'static>(
    State(state): State<Arc<DmnState<B>>>,
) -> Json<Vec<ToolInfo>> {
    let list: Vec<ToolInfo> = state.agent_factory.registry
        .names()
        .iter()
        .map(|name| ToolInfo { name: name.to_string() })
        .collect();
    Json(list)
}
