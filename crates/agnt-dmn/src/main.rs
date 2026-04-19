//! dmn — edge daemon wrapping agnt-core in an HTTP server.
//!
//! Provides REST endpoints for agentic turns, tool dispatch, session
//! management, and health checks. Designed to run one per machine
//! in a Tailscale-connected mesh.

mod config;
mod handlers;
mod middleware;
mod state;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::middleware as axum_middleware;
use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use agnt_core::Registry;
use agnt_net::Backend;
use agnt_tools::builtins;

use config::Config;
use state::{AgentFactory, DmnState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("dmn=info".parse()?))
        .init();

    let config = Config::load();

    // Refuse to start without an auth token — running unauthenticated would
    // expose tool dispatch and agent sessions to any local or network caller.
    if config.auth_token.is_empty() {
        eprintln!("error: auth_token must be set in dmn.toml — daemon refusing to start");
        std::process::exit(1);
    }

    // Warn if tool results are injected as user-role messages; this widens
    // the prompt-injection surface because tool output is trusted as user input.
    if config.tool_result_as_user {
        tracing::warn!(
            "tool_result_as_user=true: tool results injected as user-role messages — \
             elevated prompt injection risk. See THREAT_MODEL.md."
        );
    }

    let addr = config.addr();

    tracing::info!("dmn starting on {}", addr);
    tracing::info!("model: {} via {}", config.model, config.provider);

    // Build the LLM backend
    let backend = build_backend(&config)?;

    // Build the tool registry with builtins
    let mut registry = Registry::new();
    registry.register(Box::new(builtins::ReadFile::new()));
    registry.register(Box::new(builtins::ListDir::new()));
    registry.register(Box::new(builtins::Glob::new()));
    registry.register(Box::new(builtins::Grep::new()));
    registry.register(Box::new(builtins::Fetch::new()));

    let machine = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".into());

    // Store path
    let store_path = config.store_db_path();
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let state = Arc::new(DmnState {
        config: config.clone(),
        machine: machine.clone(),
        started_at: chrono::Utc::now(),
        agent_factory: AgentFactory {
            backend,
            registry: Arc::new(registry),
            store_path: Some(store_path.to_string_lossy().to_string()),
        },
        sessions: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/health", get(handlers::health::<Backend>))
        .route("/step", post(handlers::step::<Backend>))
        .route("/step/stream", post(handlers::step_stream::<Backend>))
        .route("/sessions", get(handlers::sessions::<Backend>))
        .route("/tools", get(handlers::tools::<Backend>));

    #[cfg(feature = "engine")]
    let app = app.route("/engine", post(handlers::run_engine::<Backend>));

    // Auth middleware must wrap the full app so every endpoint is protected.
    // The token is cloned into the middleware state separately from DmnState.
    let app = app
        .layer(axum_middleware::from_fn_with_state(
            config.auth_token.clone(),
            middleware::require_auth,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("dmn listening on http://{}", addr);
    tracing::info!("machine: {}", machine);

    axum::serve(listener, app).await?;
    Ok(())
}

fn build_backend(config: &Config) -> anyhow::Result<Backend> {
    let mut backend = match config.provider.as_str() {
        "ollama" => Backend::ollama(&config.model),
        "openai" => {
            let key = config.api_key.clone()
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("OpenAI API key required"))?;
            Backend::openai(&config.model, &key)
        }
        "anthropic" => {
            let key = config.api_key.clone()
                .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("Anthropic API key required"))?;
            Backend::anthropic(&config.model, &key)
        }
        other => anyhow::bail!("unknown provider: {}", other),
    };
    if let Some(ref url) = config.base_url {
        backend.base_url = url.clone();
    }
    backend.tool_result_as_user = config.tool_result_as_user;
    Ok(backend)
}
