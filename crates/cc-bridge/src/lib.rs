//! cc-bridge — NATS↔Claude Code dispatcher.
//!
//! One `cc-bridge` process serves multiple **personas**, each backed by a
//! `claude --print` invocation against a configured host (via `ssh`) and
//! working directory. The persona's identity, target host, permission mode,
//! optional system-prompt seed, and daily cost ceiling are all read from a
//! TOML config file at startup; from then on the bridge:
//!
//! 1. Subscribes to `<subject_root>.dispatch.*` (default subject_root = `cc`).
//! 2. For each [`AgentDispatch`](voicectl_core::events::AgentDispatch),
//!    extracts the persona suffix from the subject, looks it up in the
//!    config, runs `ssh <host> claude --print --permission-mode <mode>
//!    --output-format json '<system + user>'` via [`tokio::process::Command`]
//!    inside a spawned task.
//! 3. Parses the resulting JSON ([`runner::ClaudeResult`]) and publishes a
//!    single [`AgentReply`](voicectl_core::events::AgentReply) on the
//!    dispatch's `reply_to` subject.
//! 4. Tracks cumulative `total_cost_usd` per persona per local day and
//!    refuses dispatch when a persona's `daily_cost_limit_usd` is exceeded.
//!    State persists to `~/.local/state/cc-bridge/<bridge_name>.json`.
//! 5. Publishes a `<subject_root>.event.<persona>.cost` observability event
//!    after each successful completion.
//! 6. On `<subject_root>.cancel.<persona>` events, kills the in-flight `ssh`
//!    child and replies `ok=false, error=cancelled`.
//!
//! See `crates/cc-bridge/src/config.rs` for the schema,
//! `crates/cc-bridge/src/runner.rs` for the ssh+claude invocation, and
//! `crates/cc-bridge/src/dispatch.rs` for the request handler.
//!
//! ## "Many bridges" pattern
//!
//! Run one `cc-bridge@<config_name>.service` per **bridge config**. A bridge
//! config bundles related personas (e.g. all CODEX-team CC personas in one
//! file). Adding a new persona is a config-file edit + service restart;
//! adding a new team is a fresh config file + a new templated unit instance.

pub mod config;
pub mod cost;
pub mod dispatch;
pub mod runner;

pub use config::{CcBridgeConfig, Persona};
pub use cost::{CostState, CostTracker};
pub use dispatch::{BridgeContext, ReplyOutcome};
pub use runner::{ClaudeResult, ClaudeRunner, RealClaudeRunner};
