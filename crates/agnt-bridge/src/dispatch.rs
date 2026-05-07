//! Dispatch handler — the core request loop.
//!
//! `BridgeContext` owns the agent (behind a sync mutex — agnt is sync-first;
//! we lock during the spawn_blocking call), the NATS client, the
//! observer-collected tool-call list, and the in-process session state.
//! `handle_dispatch` consumes one `AgentDispatch` envelope, runs the agent,
//! and publishes:
//!
//! - zero-or-more `AgentToken` frames on `agent.token.<name>.<request_id>`
//!   (only when streaming is enabled — see `[streaming]` config), then
//! - exactly one `AgentReply` on the dispatch's `reply_to` subject.
//!
//! Concurrency: v0 is **one in-flight request per bridge**. The mutex is held
//! across the entire spawn_blocking call, so a second dispatch arriving while
//! the first is still running will simply queue. Future revs may pool agents
//! per-bridge, but the dispatch protocol is unchanged either way.
//!
//! Cancellation: a spawn_blocking call cannot itself be aborted from outside
//! (agnt's step is a sync HTTP loop with its own internal timeout), so the
//! bridge's cancel handling is **best-effort**: we drop the reply if the
//! cancel arrives before completion and reply with `error: "cancelled"`.
//! The agent itself keeps running in the background until its current
//! inference call returns. This is documented as a v0 limitation — see the
//! followups list in the README.
//!
//! Token streaming: agnt-core's `Agent::on_token` callback fires
//! synchronously from inside the sync `step()` call. The bridge plumbs that
//! callback through a tokio mpsc channel into an async publisher task —
//! since `step()` runs in `spawn_blocking`, doing a `try_send` from the
//! callback is the cheapest way to bridge sync↔async without holding any
//! tokio runtime handle inside the hot loop.
//!
//! Session lifecycle: each bridge maintains one `(session_id,
//! last_message_at)` pair behind a sync mutex. On each dispatch we check
//! `now - last_message_at`; if it exceeds `conversation.session_timeout_ms`
//! we mint a fresh session UUID, clear the in-memory message history
//! (preserving the system prompt at index 0), and update the agent's
//! `session` field so subsequent `step()` calls persist to the new session
//! row in the SQLite store. The old session's history stays on disk for
//! offline analysis.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use async_nats::Client as NatsClient;
use serde::Serialize;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::harmony::HarmonyStripper;

use voicectl_core::config::ConversationConfig;
use agnt_core::wire::{AgentDispatch, AgentReply, AgentToken, RequestId};

use crate::config::{AgentBridgeConfig, BackendKind};

// ── ToolObserver — collects tool-call names for observability ────────────────

/// Lifecycle observer that just appends each invoked tool name to a shared
/// vector. The dispatch handler drains this between requests so each reply
/// carries only the calls that happened during *its* step.
pub struct ToolObserver {
    pub log: Arc<Mutex<Vec<String>>>,
}

impl ToolObserver {
    pub fn new() -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let obs = Arc::new(Self {
            log: Arc::clone(&log),
        });
        (obs, log)
    }
}

impl Default for ToolObserver {
    fn default() -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl agnt::Observer for ToolObserver {
    fn on_tool_start(&self, call: &agnt::ToolCall) {
        if let Ok(mut g) = self.log.lock() {
            g.push(call.function.name.clone());
        }
    }
}

// ── Session state ───────────────────────────────────────────────────────────

/// Per-bridge conversational state.
///
/// The bridge keeps exactly one in-memory session at a time; rolling to a new
/// session only happens at dispatch boundaries. The `clock` indirection is
/// here purely so unit tests can pin time without monkeypatching the world.
#[derive(Clone)]
pub struct SessionState {
    pub session_id: String,
    /// Wall time of the most recent successful `agent.step` (the moment the
    /// reply was produced, not when the dispatch arrived). `None` until the
    /// first turn lands.
    pub last_message_at: Option<Instant>,
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            session_id: new_session_id(),
            last_message_at: None,
        }
    }
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

fn new_session_id() -> String {
    Uuid::new_v4().to_string()
}

/// Decision returned by [`SessionState::decide`]: either keep the current
/// session id or start a new one. Pure function so unit tests can drive every
/// branch without spinning up a bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionDecision {
    /// Use this id; agent already has matching history.
    Reuse(String),
    /// Mint this new id; bridge must clear in-memory history.
    Rotate(String),
}

impl SessionState {
    /// Given the current state, the timeout, and "now", decide what session id
    /// the next dispatch should use. Mutates `self` so subsequent calls see
    /// the rotation.
    pub fn decide(&mut self, timeout: Duration, now: Instant) -> SessionDecision {
        match self.last_message_at {
            None => SessionDecision::Reuse(self.session_id.clone()),
            Some(t) if now.duration_since(t) < timeout => {
                SessionDecision::Reuse(self.session_id.clone())
            }
            Some(_) => {
                self.session_id = new_session_id();
                self.last_message_at = None;
                SessionDecision::Rotate(self.session_id.clone())
            }
        }
    }

    /// Record that a turn just completed.
    pub fn mark_turn(&mut self, now: Instant) {
        self.last_message_at = Some(now);
    }
}

// ── Runner abstraction ───────────────────────────────────────────────────────

/// Outcome of a single agent step. Mirrors the relevant fields of `AgentReply`
/// minus the `request_id` (filled in by the dispatcher).
pub struct ReplyOutcome {
    pub ok: bool,
    pub text: String,
    pub tokens: u32,
    pub duration_ms: u32,
    pub tool_calls: Vec<String>,
    pub error: Option<String>,
}

/// Per-step inputs the runner needs from the dispatcher to publish tokens
/// while inference is happening, and to manage session rotation.
pub struct StepInputs<'a> {
    pub user_input: &'a str,
    pub on_token: Option<TokenSink>,
    pub session: SessionDirective,
    /// Cap on retained user/assistant turns. Applied right before `step()`.
    pub max_history_turns: u32,
}

/// Action to take on the agent's session/history before calling `step()`.
#[derive(Debug, Clone)]
pub enum SessionDirective {
    /// Continue the current session — no manipulation.
    Reuse,
    /// Start fresh: clear messages (preserve system prompt) and set the
    /// agent's `session` field to this id.
    Rotate(String),
}

/// Sync sink the runner hands to `on_token`. Boxed once per dispatch so the
/// agent's `FnMut(&str) + Send` requirement is satisfied without unsafe.
pub type TokenSink = Box<dyn FnMut(&str) + Send>;

/// Erased agent-runner. Real agents wrap an `agnt::Agent<Backend>`; the echo
/// stub bypasses LLM entirely. The trait is sync — invoke from inside
/// `spawn_blocking`.
pub trait AgentRunner: Send + Sync {
    fn run_step(&self, inputs: StepInputs<'_>) -> ReplyOutcome;
}

/// Real runner: holds a real `agnt::Agent` behind a sync mutex.
pub struct AgntRunner {
    agent: Arc<Mutex<agnt::Agent<agnt::Backend>>>,
    tool_log: Arc<Mutex<Vec<String>>>,
}

impl AgntRunner {
    pub fn new(
        agent: Arc<Mutex<agnt::Agent<agnt::Backend>>>,
        tool_log: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self { agent, tool_log }
    }
}

impl AgentRunner for AgntRunner {
    fn run_step(&self, inputs: StepInputs<'_>) -> ReplyOutcome {
        let t0 = Instant::now();
        // Drain any pre-existing entries so this reply only reports calls
        // from this dispatch.
        if let Ok(mut g) = self.tool_log.lock() {
            g.clear();
        }
        // Lock-and-call. Holding the mutex for the duration of the inference
        // is fine for v0 because we only allow one in-flight request anyway.
        let mut guard = match self.agent.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                // Agent state was corrupted by a previous panic. Recover the
                // guard and try anyway — agnt's step is internally tolerant
                // of partial history. Logging is enough; we don't refuse.
                warn!("agnt mutex was poisoned — recovering and continuing");
                poisoned.into_inner()
            }
        };

        // Apply session directive BEFORE installing the token sink so a
        // rotation never observes a stale callback.
        if let SessionDirective::Rotate(id) = &inputs.session {
            rotate_agent_session(&mut guard, id);
        }
        // Trim oldest turns past the configured cap. Always preserve message
        // 0 (the system prompt) and never split a tool_use/tool_result pair.
        trim_history_to_turns(&mut guard, inputs.max_history_turns);

        // Install the streaming token sink (if any) for the duration of
        // step(). We always restore the previous value (`None` for now —
        // there is no other token consumer in the bridge) before returning
        // so the agent doesn't hold a dangling closure across dispatches.
        let prev_sink = guard.on_token.take();
        guard.on_token = inputs.on_token;
        let result = guard.step(inputs.user_input);
        // Always reset to the prior sink so a panic in step() doesn't leave
        // a closure pointing at a freed channel.
        guard.on_token = prev_sink;
        drop(guard);

        let duration_ms = t0.elapsed().as_millis() as u32;
        let tool_calls = self.tool_log.lock().map(|g| g.clone()).unwrap_or_default();

        match result {
            Ok(text) => {
                let tokens = approximate_tokens(&text);
                ReplyOutcome {
                    ok: true,
                    text,
                    tokens,
                    duration_ms,
                    tool_calls,
                    error: None,
                }
            }
            Err(e) => ReplyOutcome {
                ok: false,
                text: String::new(),
                tokens: 0,
                duration_ms,
                tool_calls,
                error: Some(format!("agent.step failed: {e}")),
            },
        }
    }
}

/// Reset the agent for a new session: clear messages except the system
/// prompt, write the new session id. The Store rows for the previous session
/// are intentionally left in place so analysis tools can still see them.
fn rotate_agent_session(agent: &mut agnt::Agent<agnt::Backend>, new_id: &str) {
    // The system prompt always lives at index 0 in the v0.3 agent. Slice
    // surgically to drop turn history without re-cloning when we don't have
    // to. If somehow the message vec is empty (shouldn't happen — the
    // builder seeds a system message even when the prompt string is empty),
    // we leave it alone and just overwrite the session id.
    if !agent.messages.is_empty() {
        agent.messages.truncate(1);
    }
    agent.session = new_id.to_string();
    debug!(
        new_session = %new_id,
        retained_messages = agent.messages.len(),
        "rotated agent session"
    );
}

/// Walk the in-memory message log and drop the oldest turns until the
/// retained user-message count is `<= max_turns`. A "turn" is one user
/// message; assistant + tool messages following it are kept together so a
/// `tool_use`/`tool_result` pair can never be split across the cut point.
fn trim_history_to_turns(agent: &mut agnt::Agent<agnt::Backend>, max_turns: u32) {
    if max_turns == 0 || agent.messages.len() <= 1 {
        return;
    }
    // Index of every user message after the system prompt.
    let user_indices: Vec<usize> = agent
        .messages
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, m)| m.role == "user")
        .map(|(i, _)| i)
        .collect();
    let max = max_turns as usize;
    if user_indices.len() <= max {
        return;
    }
    // Drop everything before the (len - max)-th user message.
    let drop_before = user_indices[user_indices.len() - max];
    // Replace messages[1..drop_before] with the system message preserved.
    let mut new_msgs = Vec::with_capacity(1 + agent.messages.len() - drop_before);
    new_msgs.push(agent.messages[0].clone());
    new_msgs.extend(agent.messages[drop_before..].iter().cloned());
    let dropped = agent.messages.len() - new_msgs.len();
    agent.messages = new_msgs;
    debug!(
        dropped,
        retained = agent.messages.len(),
        max_turns,
        "trimmed agent history"
    );
}

/// Stub runner that just echoes the user input. Used for the
/// `kind = "echo"` backend; lets you stand up a second bridge
/// (e.g. `agnt-bridge@echo.service`) without burning a GPU. Useful for the
/// "many agnts" demo and as a smoke target.
pub struct EchoRunner;

impl AgentRunner for EchoRunner {
    fn run_step(&self, inputs: StepInputs<'_>) -> ReplyOutcome {
        let text = format!("echo: {}", inputs.user_input);
        // Honour the streaming sink contract for parity with the real
        // runner — split on whitespace and feed one piece at a time so the
        // sentence-boundary aggregator on the consumer side has something to
        // chew on. We don't bother adding artificial latency.
        if let Some(mut sink) = inputs.on_token {
            for (i, w) in text.split_whitespace().enumerate() {
                if i > 0 {
                    sink(" ");
                }
                sink(w);
            }
        }
        let tokens = approximate_tokens(&text);
        ReplyOutcome {
            ok: true,
            text,
            tokens,
            duration_ms: 0,
            tool_calls: Vec::new(),
            error: None,
        }
    }
}

fn approximate_tokens(text: &str) -> u32 {
    if text.is_empty() {
        0
    } else {
        text.split_whitespace().count() as u32
    }
}

// ── Agent construction from config ───────────────────────────────────────────

/// Wrapper holding the runner + (for real agents) the underlying mutex so the
/// caller can keep a strong reference for diagnostics. The runner is the
/// public-facing handle the bridge actually uses.
pub struct AgentHandle {
    pub runner: Arc<dyn AgentRunner>,
}

impl AgentHandle {
    /// Build from config. Reads the system-prompt file (with fallback),
    /// resolves the filesystem sandbox, and registers any enabled builtins
    /// + system tools.
    ///
    /// `dispatch_bus` is an optional NATS bus handle used by the
    /// `dispatch_agent` system tool. Pass `None` if the bridge cannot
    /// reach the bus yet — `dispatch_agent` will then be silently
    /// dropped from the registry (and warn-logged).
    pub fn from_config(
        cfg: &AgentBridgeConfig,
        dispatch_bus: Option<Arc<voicectl_net::Bus>>,
    ) -> anyhow::Result<Self> {
        match cfg.backend.kind {
            BackendKind::Echo => Ok(Self {
                runner: Arc::new(EchoRunner),
            }),
            BackendKind::OpenaiCompat => Self::build_openai(cfg, dispatch_bus),
        }
    }

    fn build_openai(
        cfg: &AgentBridgeConfig,
        dispatch_bus: Option<Arc<voicectl_net::Bus>>,
    ) -> anyhow::Result<Self> {
        let api_key = std::env::var(&cfg.backend.api_key_env).with_context(|| {
            format!(
                "env var '{}' (backend.api_key_env) is not set; agnt-bridge \
                 refuses to default to a baked-in key.",
                cfg.backend.api_key_env
            )
        })?;

        let backend =
            agnt::Backend::openai(&cfg.backend.model, &api_key).with_base_url(&cfg.backend.url);

        // System prompt — try the configured path, then the fallback.
        let system_prompt = read_prompt(cfg)?;

        let (observer, tool_log) = ToolObserver::new();

        // Sandbox for filesystem-aware tools. agnt::FilesystemRoot::new
        // canonicalises the path and rejects unreadable / nonexistent roots,
        // so we surface its error verbatim rather than silently degrading
        // to unsandboxed tools.
        let sandbox = match &cfg.tools.vault_root {
            Some(root) => {
                Some(Arc::new(agnt::FilesystemRoot::new(root).map_err(|e| {
                    anyhow::anyhow!("vault_root sandbox init: {e}")
                })?))
            }
            None => None,
        };

        let mut builder = agnt::AgentBuilder::new(backend)
            .system(system_prompt)
            .observer(observer);

        // Build the system-tools config once; reused per matching name.
        let system_cfg = build_system_tools_config(cfg);

        for name in &cfg.tools.enabled {
            match name.as_str() {
                "read_file" => {
                    let tool: Box<dyn agnt::Tool> = match &sandbox {
                        Some(s) => Box::new(agnt::builtins::ReadFile::with_sandbox(Arc::clone(s))),
                        None => Box::new(agnt::builtins::ReadFile::new()),
                    };
                    builder = builder.tool(tool);
                }
                "grep" => {
                    let tool: Box<dyn agnt::Tool> = match &sandbox {
                        Some(s) => Box::new(agnt::builtins::Grep::with_sandbox(Arc::clone(s))),
                        None => Box::new(agnt::builtins::Grep::new()),
                    };
                    builder = builder.tool(tool);
                }
                other => {
                    if let Some(tool) =
                        agnt_bridge_tools::build_tool(other, &system_cfg, dispatch_bus.clone())
                    {
                        builder = builder.tool(tool);
                    } else if other == "dispatch_agent" && dispatch_bus.is_none() {
                        warn!(
                            "config enables 'dispatch_agent' but no NATS bus was \
                             provided to AgentHandle::from_config — tool omitted"
                        );
                    } else {
                        warn!(
                            tool = %other,
                            "ignoring unknown tool name in config (not a vault \
                             builtin and not in agnt_bridge_tools::ALL_TOOLS)"
                        );
                    }
                }
            }
        }

        // Optional persistent message store. When set we hand the agent a
        // starting session id; the dispatcher will rotate it as the
        // conversation expires.
        let initial_session = new_session_id();
        if let Some(store_section) = &cfg.store {
            // Make parent dir if needed — Store::open won't mkdir for us.
            if let Some(parent) = store_section.db_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let path = store_section.db_path.to_string_lossy().to_string();
            let store = agnt::Store::open(&path).map_err(|e| {
                anyhow::anyhow!(
                    "agnt::Store::open({}) failed: {e}",
                    store_section.db_path.display()
                )
            })?;
            // The MessageStore-trait Arc the builder wants needs `dyn`,
            // not the concrete type — wrap explicitly.
            let store_arc: Arc<dyn agnt::MessageStore> = Arc::new(store);
            builder = builder.store(store_arc, initial_session.clone());
            info!(
                db = %store_section.db_path.display(),
                session = %initial_session,
                "agnt-store attached"
            );
        }

        let agent = builder
            .build()
            .map_err(|e| anyhow::anyhow!("agnt::AgentBuilder::build: {e}"))?;
        let agent = Arc::new(Mutex::new(agent));

        Ok(Self {
            runner: Arc::new(AgntRunner::new(agent, tool_log)),
        })
    }
}

/// Translate the bridge's `[tools]` section into a
/// [`agnt_bridge_tools::SystemToolsConfig`]. Unset fields fall back to the
/// crate's defaults (SearXNG at lnx-rig, memctl at `~/.local/bin/memctl`,
/// cache dir at `~/.cache/voicectl`, `SafetyMode::Confirm`, etc.).
///
/// The safety mode parsing is permissive: an unknown value warn-logs and
/// falls back to the default `confirm` rather than panicking — never trade a
/// crashed bridge for a guaranteed safety mode, but never silently downgrade
/// either.
fn build_system_tools_config(cfg: &AgentBridgeConfig) -> agnt_bridge_tools::SystemToolsConfig {
    let mut sc = agnt_bridge_tools::SystemToolsConfig::default();
    if let Some(url) = &cfg.tools.searxng_url {
        sc.searxng_url = url.clone();
    }
    if let Some(p) = &cfg.tools.memctl_bin {
        sc.memctl_bin = p.clone();
    }
    if let Some(p) = &cfg.tools.cache_dir {
        sc.cache_dir = p.clone();
    }
    if let Some(s) = &cfg.tools.computer_use_safety {
        match agnt_bridge_tools::SafetyMode::parse(s) {
            Ok(mode) => {
                if mode == agnt_bridge_tools::SafetyMode::Off {
                    warn!(
                        agent = %cfg.agent.name,
                        "computer_use_safety = off — destructive tools will execute without \
                         confirmation. This is for testing only; do not ship."
                    );
                }
                sc.computer_use_safety.mode = mode;
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "invalid computer_use_safety value; falling back to 'confirm'"
                );
            }
        }
    }
    if let Some(keys) = &cfg.tools.computer_use_safe_keys {
        sc.computer_use_safety.safe_keys = keys.clone();
    }
    if let Some(apps) = &cfg.tools.computer_use_safe_focus_apps {
        sc.computer_use_safety.safe_focus_apps = apps.clone();
    }
    if let Some(url) = &cfg.tools.vision_url {
        sc.vision_url = url.clone();
    }
    if let Some(m) = &cfg.tools.vision_model {
        sc.vision_model = m.clone();
    }
    sc
}

fn read_prompt(cfg: &AgentBridgeConfig) -> anyhow::Result<String> {
    let primary = &cfg.prompt.system_file;
    if primary.exists() {
        return std::fs::read_to_string(primary)
            .map(|s| s.trim().to_owned())
            .with_context(|| format!("read system_file {}", primary.display()));
    }
    if let Some(fallback) = &cfg.prompt.fallback_file {
        if fallback.exists() {
            info!(
                primary = %primary.display(),
                fallback = %fallback.display(),
                "primary system prompt missing, using fallback"
            );
            return std::fs::read_to_string(fallback)
                .map(|s| s.trim().to_owned())
                .with_context(|| format!("read fallback_file {}", fallback.display()));
        }
    }
    warn!(
        primary = %primary.display(),
        "no system prompt file found; running with empty prompt"
    );
    Ok(String::new())
}

// ── Bridge runtime context ───────────────────────────────────────────────────

pub struct BridgeContext {
    pub cfg: AgentBridgeConfig,
    pub nats: NatsClient,
    pub agent: AgentHandle,
    /// In-memory session state, rotated lazily on dispatch.
    pub session: Mutex<SessionState>,
}

impl BridgeContext {
    pub fn new(cfg: AgentBridgeConfig, nats: NatsClient, agent: AgentHandle) -> Self {
        Self {
            cfg,
            nats,
            agent,
            session: Mutex::new(SessionState::new()),
        }
    }

    /// Build the streaming subject for a request. Public so tests can assert.
    pub fn token_subject(&self, request_id: &RequestId) -> String {
        format!("agent.token.{}.{}", self.cfg.agent.name, request_id)
    }

    /// Handle one `AgentDispatch`: run the agent in spawn_blocking, publish
    /// the reply (and per-token frames if streaming is enabled).
    pub async fn handle_dispatch(self: &Arc<Self>, dispatch: AgentDispatch) {
        let request_id = dispatch.request_id.clone();
        let reply_to = dispatch.reply_to.clone();
        info!(
            request_id = %request_id,
            reply_to = %reply_to,
            user_chars = dispatch.user_input.len(),
            streaming = self.cfg.streaming.enabled,
            "agent.dispatch received"
        );

        // Validate reply_to before starting the agent — reject subjects that
        // don't match the expected `agent.response.<name>.<uuid>` pattern.
        // This prevents a malformed or adversarial dispatch from causing the
        // reply to be published to an arbitrary subject.
        if !reply_to.starts_with("agent.response.") {
            warn!(
                request_id = %request_id,
                reply_to = %reply_to,
                "dispatch rejected: reply_to does not match agent.response.<name>.<uuid> pattern"
            );
            // Publish an error to the dead-letter subject so the caller has
            // some signal rather than silently dropping the dispatch.
            let fallback_subject = format!(
                "agent.response.error.{}",
                request_id
            );
            let err_reply = AgentReply {
                request_id: request_id.clone(),
                ok: false,
                text: String::new(),
                tokens: 0,
                duration_ms: 0,
                tool_calls: Vec::new(),
                error: Some(format!(
                    "invalid reply_to subject '{reply_to}': must start with 'agent.response.'"
                )),
            };
            let _ = publish_json(&self.nats, &fallback_subject, &err_reply).await;
            return;
        }

        // Decide session before entering the blocking section so the new id
        // (if rotation fires) is visible in logs immediately.
        let directive = {
            let timeout = Duration::from_millis(self.cfg.conversation.session_timeout_ms);
            let mut g = self.session.lock().unwrap_or_else(|p| p.into_inner());
            match g.decide(timeout, Instant::now()) {
                SessionDecision::Reuse(_) => SessionDirective::Reuse,
                SessionDecision::Rotate(id) => {
                    info!(new_session = %id, "starting new conversation session");
                    SessionDirective::Rotate(id)
                }
            }
        };

        // Streaming pump. We spawn it unconditionally when enabled — the
        // closure does a `try_send` so the cost in the disabled case is
        // exactly the channel allocation we skip below.
        let streaming = self.cfg.streaming.enabled;
        let token_subject = self.token_subject(&request_id);
        // Harmony channel-marker stripper, shared between the sink and the
        // outer scope so we can flush any held-back tail bytes after
        // `run_step` returns. Without this, models that emit tokens like
        // `<|channel>thought\n<channel|>` (gemma4-26b on vLLM, gpt-oss
        // variants) cause the user to literally hear "channel … channel"
        // through the TTS pipeline.
        let stripper: Arc<Mutex<HarmonyStripper>> = Arc::new(Mutex::new(HarmonyStripper::new()));
        let (token_sink, pump_handle, sink_tx) = if streaming {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);
            let nats = self.nats.clone();
            let request_id_for_pump = request_id.clone();
            let subj = token_subject.clone();
            let h = tokio::spawn(async move {
                let mut idx: u32 = 0;
                while let Some(text) = rx.recv().await {
                    let frame = AgentToken {
                        request_id: request_id_for_pump.clone(),
                        idx,
                        text,
                        is_final: false,
                    };
                    if let Err(e) = publish_json(&nats, &subj, &frame).await {
                        warn!(error = %e, "publish agent.token failed");
                    }
                    idx = idx.saturating_add(1);
                }
                idx
            });
            // Build the FnMut sink that the agent will call. `try_send` is
            // important: a slow subscriber must not stall the inference loop.
            let stripper_for_sink = Arc::clone(&stripper);
            let tx_for_sink = tx.clone();
            let sink: TokenSink = Box::new(move |delta: &str| {
                let cleaned = stripper_for_sink
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .process(delta);
                if !cleaned.is_empty() {
                    let _ = tx_for_sink.try_send(cleaned);
                }
            });
            (Some(sink), Some(h), Some(tx))
        } else {
            (None, None, None)
        };

        let runner = Arc::clone(&self.agent.runner);
        let user_input = dispatch.user_input.clone();
        let max_history_turns = self.cfg.conversation.max_history_turns;
        let outcome = tokio::task::spawn_blocking(move || {
            runner.run_step(StepInputs {
                user_input: &user_input,
                on_token: token_sink,
                session: directive,
                max_history_turns,
            })
        })
        .await;

        // Flush any tail bytes the harmony stripper was holding back in
        // case they were the prefix of a marker. Send them through the
        // existing token channel so the pump publishes them as a regular
        // `agent.token` frame, then drop our sender so the pump exits.
        if let Some(tx) = sink_tx {
            let tail = stripper
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .flush();
            if !tail.is_empty() {
                let _ = tx.try_send(tail);
            }
            drop(tx);
        }

        // Drain the pump (if any) BEFORE publishing the synthetic terminal
        // frame so subscribers see ordered idx values. The closure inside
        // the agent has been dropped by now (run_step returned), so all tx
        // clones are released and the pump's rx will see EOF promptly.
        let final_idx = if let Some(handle) = pump_handle {
            handle.await.unwrap_or(0)
        } else {
            0
        };
        if streaming {
            let final_frame = AgentToken {
                request_id: request_id.clone(),
                idx: final_idx,
                text: String::new(),
                is_final: true,
            };
            if let Err(e) = publish_json(&self.nats, &token_subject, &final_frame).await {
                warn!(error = %e, "publish agent.token (final) failed");
            }
        }

        let reply = match outcome {
            Ok(o) => {
                if o.ok {
                    let mut g = self.session.lock().unwrap_or_else(|p| p.into_inner());
                    g.mark_turn(Instant::now());
                }
                AgentReply {
                    request_id: request_id.clone(),
                    ok: o.ok,
                    // The final text may contain harmony channel markers if
                    // the model leaks them inline (see `harmony.rs`). Strip
                    // them here so non-streaming subscribers — and the
                    // logged AgentReply itself — see clean assistant text.
                    text: HarmonyStripper::strip_full(&o.text),
                    tokens: o.tokens,
                    duration_ms: o.duration_ms,
                    tool_calls: o.tool_calls,
                    error: o.error,
                }
            }
            Err(join_err) => AgentReply {
                request_id: request_id.clone(),
                ok: false,
                text: String::new(),
                tokens: 0,
                duration_ms: 0,
                tool_calls: Vec::new(),
                error: Some(format!("worker panicked: {join_err}")),
            },
        };

        if let Err(e) = publish_json(&self.nats, &reply_to, &reply).await {
            warn!(error = %e, reply_to = %reply_to, "failed to publish reply");
        } else {
            info!(
                request_id = %request_id,
                ok = reply.ok,
                tokens = reply.tokens,
                duration_ms = reply.duration_ms,
                tools = ?reply.tool_calls,
                "agent.reply published"
            );
        }
    }

    /// Publish a synthetic "cancelled" reply for an in-flight request whose
    /// cancel notification arrived. This is best-effort: the actual sync
    /// `agent.step` call cannot be aborted from outside agnt-rs as of v0.3.
    pub async fn publish_cancel_reply(&self, request_id: &RequestId, reply_to: &str) {
        let reply = AgentReply {
            request_id: request_id.clone(),
            ok: false,
            text: String::new(),
            tokens: 0,
            duration_ms: 0,
            tool_calls: Vec::new(),
            error: Some("cancelled".into()),
        };
        let _ = publish_json(&self.nats, reply_to, &reply).await;
    }
}

async fn publish_json<T: Serialize>(
    nats: &NatsClient,
    subject: &str,
    payload: &T,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(payload).map_err(|e| format!("encode: {e}"))?;
    nats.publish(subject.to_string(), bytes.into())
        .await
        .map_err(|e| format!("publish: {e}"))
}

// ── Free helpers used by tests ───────────────────────────────────────────────

/// For tests: synthesise a session decision against fixed timestamps.
#[cfg(test)]
pub fn _test_decide(state: &mut SessionState, timeout_ms: u64, now: Instant) -> SessionDecision {
    state.decide(Duration::from_millis(timeout_ms), now)
}

// Suppress unused warning when ConversationConfig is read only via field
// access in BridgeContext.
#[allow(dead_code)]
fn _conversation_marker(_: &ConversationConfig) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_runner_bounces_input() {
        let r = EchoRunner;
        let out = r.run_step(StepInputs {
            user_input: "hello world",
            on_token: None,
            session: SessionDirective::Reuse,
            max_history_turns: 20,
        });
        assert!(out.ok);
        assert_eq!(out.text, "echo: hello world");
        // approximate_tokens just splits on whitespace — "echo:", "hello",
        // "world" = 3.
        assert_eq!(out.tokens, 3);
        assert!(out.error.is_none());
        assert!(out.tool_calls.is_empty());
    }

    #[test]
    fn echo_runner_streams_tokens_through_sink() {
        let r = EchoRunner;
        let collected = Arc::new(Mutex::new(Vec::<String>::new()));
        let cloned = collected.clone();
        let sink: TokenSink = Box::new(move |delta: &str| {
            cloned.lock().unwrap().push(delta.to_string());
        });
        let _ = r.run_step(StepInputs {
            user_input: "alpha beta",
            on_token: Some(sink),
            session: SessionDirective::Reuse,
            max_history_turns: 20,
        });
        let pieces = collected.lock().unwrap().clone();
        // "echo: alpha beta" → ["echo:", " ", "alpha", " ", "beta"]
        assert_eq!(pieces, vec!["echo:", " ", "alpha", " ", "beta"]);
    }

    #[test]
    fn approximate_tokens_counts_words() {
        assert_eq!(approximate_tokens(""), 0);
        assert_eq!(approximate_tokens("a"), 1);
        assert_eq!(approximate_tokens("a b c"), 3);
        assert_eq!(approximate_tokens("  multiple   spaces "), 2);
    }

    // ── session lifecycle ─────────────────────────────────────────────────

    #[test]
    fn session_first_dispatch_reuses_initial_id() {
        let mut s = SessionState::new();
        let initial = s.session_id.clone();
        let now = Instant::now();
        match s.decide(Duration::from_secs(60), now) {
            SessionDecision::Reuse(id) => assert_eq!(id, initial),
            SessionDecision::Rotate(_) => panic!("first dispatch must reuse"),
        }
    }

    #[test]
    fn session_within_window_reuses() {
        let mut s = SessionState::new();
        let t0 = Instant::now();
        s.mark_turn(t0);
        let still_in = t0 + Duration::from_secs(5);
        match s.decide(Duration::from_secs(60), still_in) {
            SessionDecision::Reuse(_) => {}
            SessionDecision::Rotate(_) => panic!("inside window must reuse"),
        }
    }

    #[test]
    fn session_after_timeout_rotates() {
        let mut s = SessionState::new();
        let initial = s.session_id.clone();
        let t0 = Instant::now();
        s.mark_turn(t0);
        let later = t0 + Duration::from_secs(120);
        match s.decide(Duration::from_secs(60), later) {
            SessionDecision::Rotate(new_id) => {
                assert_ne!(new_id, initial);
                assert_eq!(s.session_id, new_id);
                assert!(s.last_message_at.is_none(), "rotation clears clock");
            }
            SessionDecision::Reuse(_) => panic!("must rotate past timeout"),
        }
    }

    #[test]
    fn session_zero_timeout_means_always_rotate_after_first_turn() {
        let mut s = SessionState::new();
        let t0 = Instant::now();
        s.mark_turn(t0);
        // Even a zero-duration timeout shouldn't fire on the same instant
        // because we use `<` (strict less-than). Bumping by 1ns suffices.
        let later = t0 + Duration::from_nanos(1);
        match s.decide(Duration::ZERO, later) {
            SessionDecision::Rotate(_) => {}
            SessionDecision::Reuse(_) => panic!("zero timeout should rotate"),
        }
    }
}
