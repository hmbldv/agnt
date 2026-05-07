//! cc-bridge daemon entry point.
//!
//! ```text
//! cc-bridge --config ~/.config/voicectl/cc/codex.toml
//! ```
//!
//! The daemon:
//! 1. Loads + validates the config (panics with a clear message on schema
//!    or persona-validation problems).
//! 2. Connects to NATS using `voicectl_net::Bus::from_config`.
//! 3. Subscribes to `<subject_root>.dispatch.*` and `<subject_root>.cancel.*`.
//! 4. For each dispatch, spawns a tokio task that runs `ssh <host> claude
//!    --print …`, publishes the AgentReply, and emits a cost event.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use futures::StreamExt;
use tracing::{info, warn};

use cc_bridge::{BridgeContext, CcBridgeConfig, CostTracker, RealClaudeRunner};
use voicectl_core::config::BusConfig;
use voicectl_core::events::AgentDispatch;
use voicectl_net::Bus;

#[derive(Parser, Debug)]
#[command(
    name = "cc-bridge",
    version,
    about = "NATS↔Claude Code bridge daemon — one process, many personas."
)]
struct Args {
    /// Path to the bridge's TOML config. Tilde-expanded.
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cc_bridge=debug".into()),
        )
        .init();

    let args = Args::parse();
    let cfg_path = expand_tilde(&args.config);
    info!(config = %cfg_path.display(), "cc-bridge starting");

    let cfg_str = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("read config {}", cfg_path.display()))?;
    let mut cfg = CcBridgeConfig::from_toml_str(&cfg_str)
        .with_context(|| format!("parse config {}", cfg_path.display()))?;
    cfg.expand_tildes();
    cfg.validate()
        .map_err(|e| anyhow::anyhow!("config validation failed: {e}"))?;

    info!(
        bridge = %cfg.bridge.name,
        subject_root = %cfg.bridge.subject_root,
        personas = ?cfg.personas.iter().map(|p| &p.name).collect::<Vec<_>>(),
        "config loaded"
    );

    // NATS connection. We pass an empty subject_prefix because cc-bridge
    // subjects are absolute (`cc.…`) — the prefix machinery is voice-only.
    let bus_cfg = BusConfig {
        nats_url: cfg.bus.nats_url.clone(),
        subject_prefix: String::new(),
        user_env: cfg.bus.user_env.clone(),
        password_env: cfg.bus.password_env.clone(),
    };
    let bus = Bus::from_config(&bus_cfg).await.context("connect NATS")?;
    info!(url = %bus_cfg.nats_url, "NATS connected");

    let dispatch_subject = cfg.dispatch_wildcard();
    let cancel_subject = cfg.cancel_wildcard();

    let mut dispatch_sub = bus
        .client
        .subscribe(dispatch_subject.clone())
        .await
        .with_context(|| format!("subscribe {dispatch_subject}"))?;
    let mut cancel_sub = bus
        .client
        .subscribe(cancel_subject.clone())
        .await
        .with_context(|| format!("subscribe {cancel_subject}"))?;
    info!(
        dispatch = %dispatch_subject,
        cancel = %cancel_subject,
        "subscribed"
    );

    let tracker = CostTracker::load_or_default(CostTracker::default_path(&cfg.bridge.name));
    let runner = Arc::new(RealClaudeRunner::new());
    let ctx = Arc::new(BridgeContext::new(cfg, bus.client.clone(), runner, tracker));

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
                let subject = msg.subject.to_string();
                match serde_json::from_slice::<AgentDispatch>(&msg.payload) {
                    Ok(dispatch) => {
                        let ctx = Arc::clone(&ctx);
                        tokio::spawn(async move {
                            ctx.handle_dispatch(subject, dispatch).await;
                        });
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            subject = %subject,
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
                let subject = msg.subject.to_string();
                let ctx = Arc::clone(&ctx);
                tokio::spawn(async move {
                    ctx.handle_cancel(subject).await;
                });
            }
        }
    }

    info!("cc-bridge stopped");
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
