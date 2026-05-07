//! agnt-bridge — generic NATS↔agnt-rs bridge daemon.
//!
//! One `agnt-bridge` process serves exactly one agent. The agent's identity,
//! backend, prompt, and tool surface are all read from a TOML config file
//! at startup; from then on the bridge:
//!
//! 1. Subscribes to `<subject_root>.dispatch.<name>`.
//! 2. For each [`AgentDispatch`](voicectl_core::events::AgentDispatch),
//!    runs `agent.step(user_input)` inside `tokio::task::spawn_blocking`
//!    (agnt is sync; we hold an `Arc<std::sync::Mutex<Agent>>`).
//! 3. Publishes a single [`AgentReply`](voicectl_core::events::AgentReply)
//!    on the dispatch's `reply_to` subject.
//! 4. On `<subject_root>.cancel.<name>` events, aborts whatever request is
//!    currently in flight and replies with `ok=false, error=cancelled`.
//!
//! See `crates/agnt-bridge/src/config.rs` for the schema and
//! `crates/agnt-bridge/src/dispatch.rs` for the request handler.
//!
//! ## "Many agnts" pattern
//!
//! Run one `agnt-bridge@<name>.service` per agent. Each instance reads a
//! config file at `~/.config/voicectl/agents/<name>.toml`. Adding a new
//! agent is a config file plus a `systemctl --user enable --now …`. The
//! voicectld pipeline is told which agent to address via
//! `[agent_dispatch].target_agent` in `voicectl.toml`.

pub mod config;
pub mod dispatch;
pub mod harmony;
pub mod prompt;

pub use config::AgentBridgeConfig;
pub use dispatch::{AgentHandle, BridgeContext, RaspConfig, ReplyOutcome};
