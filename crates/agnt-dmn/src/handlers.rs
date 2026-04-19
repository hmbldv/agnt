//! HTTP request handlers.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt as _;

use agnt_core::{LlmBackend, Message, Observer, ToolCall, ToolResult};
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
    let session = state
        .get_or_create_session(&req.session_id)
        .map_err(|sc| (sc, "invalid or rate-limited session id".into()))?;

    // Create agent in a blocking task (agnt-core is sync)
    let state_clone = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut agent = state_clone.agent_factory.create(
            &session,
            req.system_prompt.as_deref(),
        );

        match agent.step(&req.prompt) {
            Ok(response) => Ok(StepResponse {
                session_id: session,
                response,
            }),
            Err(e) => Err(format!("agent step failed: {}", e)),
        }
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task join: {}", e)))?;

    result.map(Json).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))
}

// --- SSE streaming step ---

struct SseItem {
    event: &'static str,
    json: String,
}

struct SseObserver {
    tx: mpsc::Sender<SseItem>,
    session_id: String,
}

impl SseObserver {
    /// Send an item on the bounded channel, logging if the channel is full.
    /// Events are dropped rather than blocking the agent thread.
    fn send(&self, item: SseItem) {
        if self.tx.try_send(item).is_err() {
            tracing::debug!("sse channel full, dropping event");
        }
    }
}

impl Observer for SseObserver {
    fn on_tool_start(&self, call: &ToolCall) {
        let data = serde_json::json!({
            "name": call.function.name,
            "args": call.function.arguments,
        });
        self.send(SseItem { event: "tool_call", json: data.to_string() });
    }

    fn on_tool_end(&self, call: &ToolCall, result: &ToolResult) {
        let result_str = match &result.output {
            Ok(s) => s.as_str(),
            Err(s) => s.as_str(),
        };
        const RESULT_CAP: usize = 2048;
        let display = if result_str.len() > RESULT_CAP {
            // Truncate on a valid UTF-8 boundary
            let mut end = RESULT_CAP;
            while end > 0 && !result_str.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}… [{} bytes truncated]", &result_str[..end], result_str.len() - end)
        } else {
            result_str.to_string()
        };
        let data = serde_json::json!({
            "name": call.function.name,
            "result": display,
            "duration_ms": result.duration_us as f64 / 1000.0,
        });
        self.send(SseItem { event: "tool_result", json: data.to_string() });
    }

    fn on_step_end(&self, response: &Message) {
        let data = serde_json::json!({
            "session_id": self.session_id,
            "response": response.content.as_deref().unwrap_or(""),
        });
        self.send(SseItem { event: "complete", json: data.to_string() });
    }

    fn on_step_error(&self, error: &str) {
        let data = serde_json::json!({ "message": error });
        self.send(SseItem { event: "error", json: data.to_string() });
    }
}

pub async fn step_stream<B: LlmBackend + Clone + 'static>(
    State(state): State<Arc<DmnState<B>>>,
    Json(req): Json<StepRequest>,
) -> Response {
    let session = match state.get_or_create_session(&req.session_id) {
        Ok(s) => s,
        Err(sc) => return sc.into_response(),
    };

    // Bounded channel — callers that can't keep up receive dropped events
    // rather than causing unbounded memory growth. 256 is generous for typical
    // agent runs (tool calls + tokens) while still capping exposure.
    let (tx, rx) = mpsc::channel::<SseItem>(256);

    let _ = tx.try_send(SseItem {
        event: "session_start",
        json: serde_json::json!({
            "session_id": session,
            "model": state.config.model,
        })
        .to_string(),
    });

    // Two senders: one for the observer, one for the on_token callback.
    // Both are dropped when the blocking task exits, which closes the channel.
    let tx_obs = tx.clone();
    let tx_tok = tx.clone();
    // Clone for the panic-surface watcher.
    let tx_err = tx.clone();
    drop(tx);

    let observer = Arc::new(SseObserver {
        tx: tx_obs,
        session_id: session.clone(),
    });

    let state_clone = state.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let mut agent = state_clone.agent_factory.create(&session, req.system_prompt.as_deref());
        agent.observer = observer as Arc<dyn agnt_core::Observer>;
        agent.on_token = Some(Box::new(move |tok| {
            let data = serde_json::json!({ "content": tok });
            if tx_tok.try_send(SseItem { event: "token", json: data.to_string() }).is_err() {
                tracing::debug!("sse channel full, dropping token event");
            }
        }));
        let _ = agent.step(&req.prompt);
        // agent drops here → observer (tx_obs) and on_token (tx_tok) drop → channel closes
    });

    // Surface panics in the blocking task back to the SSE client so the
    // connection doesn't just silently stall.
    tokio::spawn(async move {
        if handle.await.is_err() {
            let data = serde_json::json!({ "message": "internal agent task panicked" });
            let _ = tx_err.try_send(SseItem { event: "error", json: data.to_string() });
        }
    });

    let stream = ReceiverStream::new(rx).map(|item| {
        Ok::<Event, Infallible>(Event::default().event(item.event).data(item.json))
    });

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("ping"),
        )
        .into_response()
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

// --- Engine (optional feature) ---

#[cfg(feature = "engine")]
#[derive(Deserialize)]
pub struct EngineRequest {
    pub task: agnt_engine::Task,
    pub system_prompt: Option<String>,
    pub max_steps: Option<usize>,
    pub credits_per_step: Option<u64>,
    pub budget_allocated: Option<u64>,
    pub ttl_seconds: Option<i64>,
    pub permitted_tools: Option<Vec<String>>,
    pub denied_tools: Option<Vec<String>>,
}

#[cfg(feature = "engine")]
pub async fn run_engine<B: LlmBackend + Clone + 'static>(
    State(state): State<Arc<DmnState<B>>>,
    Json(req): Json<EngineRequest>,
) -> Result<Json<agnt_engine::EngineResult>, (StatusCode, String)> {
    use agnt_engine::{run_agent, EngineConfig};
    use tokio::sync::Notify;

    let config = EngineConfig {
        backend: state.agent_factory.backend.clone(),
        tools: vec![],
        system_prompt: req.system_prompt.unwrap_or_else(|| "You are a helpful assistant.".into()),
        max_steps: req.max_steps.unwrap_or(50),
        credits_per_step: req.credits_per_step.unwrap_or(1),
        budget_allocated: req.budget_allocated.unwrap_or(0),
        ttl_expires_at: chrono::Utc::now() + chrono::TimeDelta::seconds(req.ttl_seconds.unwrap_or(3600)),
        shutdown: Arc::new(Notify::new()),
        permitted_tools: req.permitted_tools.unwrap_or_default(),
        denied_tools: req.denied_tools.unwrap_or_default(),
    };

    let result = run_agent(config, req.task).await;
    Ok(Json(result))
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
