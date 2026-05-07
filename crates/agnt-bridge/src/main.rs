//! agnt-bridge daemon entry point.
//!
//! Usage:
//! ```text
//! agnt-bridge --config ~/.config/voicectl/agents/sage.toml
//! ```
//!
//! The daemon:
//! 1. Loads + validates the config (panics with a clear message on schema /
//!    env-var problems).
//! 2. Connects to NATS using `voicectl_net::Bus::from_config`.
//! 3. Subscribes to `cfg.bus.subscribe_subject` and `agent.cancel.<name>`.
//! 4. For each dispatch, spawns a tokio task that runs `agent.step` inside
//!    `spawn_blocking` and publishes one reply.
//!
//! Cancel handling: v0 publishes a synthetic `ok=false, error=cancelled`
//! reply to the in-flight request's reply_to subject. The agnt-rs sync step
//! call itself cannot be aborted mid-flight (no async cancellation primitive
//! across the FFI boundary); the in-flight inference will complete in the
//! background and any further reply is dropped because the cancel channel
//! already produced one.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use futures::StreamExt;
use tracing::{info, warn};

use agnt_bridge::{AgentBridgeConfig, AgentHandle, BridgeContext};
use agnt_core::wire::RequestId;
use voicectl_core::config::BusConfig;
use voicectl_core::events::{AgentCancel, AgentDispatch};
use voicectl_net::Bus;

#[derive(Parser, Debug)]
#[command(
    name = "agnt-bridge",
    version,
    about = "NATS↔agnt-rs bridge daemon — one process per agent."
)]
struct Args {
    /// Path to the agent's TOML config. Tilde-expanded.
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,agnt_bridge=debug".into()),
        )
        .init();

    let args = Args::parse();
    let cfg_path = expand_tilde(&args.config);
    info!(config = %cfg_path.display(), "agnt-bridge starting");

    let cfg_str = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("read config {}", cfg_path.display()))?;
    let mut cfg = AgentBridgeConfig::from_toml_str(&cfg_str)
        .with_context(|| format!("parse config {}", cfg_path.display()))?;
    cfg.expand_tildes();

    info!(
        agent = %cfg.agent.name,
        backend = ?cfg.backend.kind,
        model = %cfg.backend.model,
        url = %cfg.backend.url,
        subject = %cfg.bus.subscribe_subject,
        "config loaded"
    );

    // Connect to NATS first — the `dispatch_agent` system tool needs a bus
    // handle at agent-construction time. Misconfigured prompts / sandboxes
    // still surface inside `from_config`, just after a NATS round-trip
    // instead of before; on a healthy network this adds <100ms.
    let bus_cfg = BusConfig {
        nats_url: cfg.bus.nats_url.clone(),
        subject_prefix: String::new(),
        user_env: cfg.bus.user_env.clone(),
        password_env: cfg.bus.password_env.clone(),
    };
    let bus = Bus::from_config(&bus_cfg).await.context("connect NATS")?;
    info!(url = %bus_cfg.nats_url, "NATS connected");
    let bus_arc = std::sync::Arc::new(bus);

    // Build the agent now that we have a bus handle to thread into the
    // `dispatch_agent` tool (other tools don't need it).
    let agent = AgentHandle::from_config(&cfg, Some(Arc::clone(&bus_arc)))
        .context("AgentHandle::from_config")?;

    let dispatch_subject = cfg.bus.subscribe_subject.clone();
    let cancel_subject = format!("agent.cancel.{}", cfg.agent.name);

    let mut dispatch_sub = bus_arc
        .client
        .subscribe(dispatch_subject.clone())
        .await
        .with_context(|| format!("subscribe {dispatch_subject}"))?;
    let mut cancel_sub = bus_arc
        .client
        .subscribe(cancel_subject.clone())
        .await
        .with_context(|| format!("subscribe {cancel_subject}"))?;
    info!(
        dispatch = %dispatch_subject,
        cancel = %cancel_subject,
        "subscribed"
    );

    let ctx = Arc::new(BridgeContext::new(cfg, bus_arc.client.clone(), agent));

    // Track the currently-in-flight request so a cancel can publish a
    // synthetic reply on its reply_to subject. v0 = at most one in-flight.
    let in_flight: Arc<tokio::sync::Mutex<Option<(RequestId, String)>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    // SIGTERM / SIGINT.
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;

            _ = &mut shutdown => {
                info!("shutdown signal received");
                break;
            }

            msg = dispatch_sub.next() => {
                let Some(msg) = msg else {
                    warn!("dispatch subscription closed; exiting");
                    break;
                };
                match serde_json::from_slice::<AgentDispatch>(&msg.payload) {
                    Ok(dispatch) => {
                        let req_id = dispatch.request_id.clone();
                        let reply_to = dispatch.reply_to.clone();
                        {
                            let mut g = in_flight.lock().await;
                            *g = Some((req_id.clone(), reply_to.clone()));
                        }
                        let ctx = Arc::clone(&ctx);
                        let in_flight = Arc::clone(&in_flight);
                        // We could spawn-and-forget for concurrency; v0
                        // serialises through the agent mutex anyway, so
                        // spawning still gives us the right semantics
                        // without risking the runtime queue stalling on a
                        // stuck step.
                        tokio::spawn(async move {
                            ctx.handle_dispatch(dispatch).await;
                            let mut g = in_flight.lock().await;
                            // Only clear if it's still us.
                            if g.as_ref().map(|(id, _)| id == &req_id).unwrap_or(false) {
                                *g = None;
                            }
                        });
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            subject = %msg.subject,
                            "failed to decode AgentDispatch payload — dropping"
                        );
                    }
                }
            }

            msg = cancel_sub.next() => {
                let Some(msg) = msg else {
                    warn!("cancel subscription closed; ignoring");
                    continue;
                };
                let cancel: AgentCancel = serde_json::from_slice(&msg.payload).unwrap_or_default();
                let target = {
                    let g = in_flight.lock().await;
                    g.clone()
                };
                match target {
                    Some((req_id, reply_to))
                        if cancel.all
                            || cancel
                                .request_id
                                .as_ref()
                                .map(|r| r == &req_id)
                                .unwrap_or(true) =>
                    {
                        info!(request_id = %req_id, "cancelling in-flight request");
                        ctx.publish_cancel_reply(&req_id, &reply_to).await;
                    }
                    Some(_) => {
                        info!("cancel ignored — request_id mismatch");
                    }
                    None => {
                        info!("cancel ignored — nothing in flight");
                    }
                }
            }
        }
    }

    info!("agnt-bridge stopped");
    Ok(())
}

fn expand_tilde(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.to_path_buf()
}
