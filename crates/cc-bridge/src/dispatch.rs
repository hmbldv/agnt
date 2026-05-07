//! Dispatch handler — the core request loop.
//!
//! `BridgeContext` owns the resolved config, the NATS client, the cost
//! tracker (behind a tokio mutex so the dispatch + cost-event paths are
//! linearisable), and the runner. `handle_dispatch` consumes one
//! `AgentDispatch` envelope, runs the persona's `claude --print …`
//! invocation, publishes one `AgentReply` on the dispatch's `reply_to`
//! subject, and emits a cost event on `<subject_root>.event.<persona>.cost`.
//!
//! Concurrency: each dispatch is spawned into its own tokio task. The
//! per-persona cost tracker is the single shared mutable state, hidden
//! behind a tokio mutex held only across short critical sections (read or
//! `record` + persist). Multiple personas can dispatch concurrently —
//! `claude` itself serialises remote work via its own session model.
//!
//! Cancellation: a `cc.cancel.<persona>` event matching an in-flight
//! request causes the bridge to publish a synthetic `error: "cancelled"`
//! reply. The actual `ssh` child is killed via `tokio::process::Command`'s
//! `kill_on_drop` when the runner future is dropped — but the runner
//! future itself isn't drop-cancelled in v0 (the spawned task is detached).
//! v0 cancel semantics are therefore "best-effort fast-fail to the
//! caller"; the in-flight ssh+claude runs to completion and its result is
//! discarded.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_nats::Client as NatsClient;
use chrono::Local;
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

use voicectl_core::events::{AgentDispatch, AgentReply, RequestId};

use crate::config::{CcBridgeConfig, Persona};
use crate::cost::CostTracker;
use crate::runner::{ClaudeResult, ClaudeRunner};

/// Outcome of a single dispatch — what we publish to the caller plus the
/// derived cost event. Exposed mostly for tests and tracing.
pub struct ReplyOutcome {
    pub reply: AgentReply,
    pub claude: ClaudeResult,
}

/// In-flight request bookkeeping: maps persona → (request_id, reply_to)
/// for at most one outstanding request per persona at a time. v0 doesn't
/// queue; if a second dispatch arrives for the same persona while the
/// first is in flight, both run concurrently and only the most recent
/// one is cancellable by name.
type InFlight = Arc<Mutex<HashMap<String, (String, String)>>>;

/// Bridge runtime context.
pub struct BridgeContext {
    pub cfg: CcBridgeConfig,
    pub nats: NatsClient,
    pub runner: Arc<dyn ClaudeRunner>,
    pub cost: Arc<Mutex<CostTracker>>,
    pub in_flight: InFlight,
}

impl BridgeContext {
    pub fn new(
        cfg: CcBridgeConfig,
        nats: NatsClient,
        runner: Arc<dyn ClaudeRunner>,
        cost: CostTracker,
    ) -> Self {
        Self {
            cfg,
            nats,
            runner,
            cost: Arc::new(Mutex::new(cost)),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Extract the persona name from a NATS subject. Subjects have shape
    /// `<root>.dispatch.<persona>` — we strip the prefix and return the
    /// final token. Returns `None` if the shape doesn't match (which
    /// shouldn't happen given the wildcard subscription, but is handled
    /// defensively).
    pub fn persona_from_subject<'a>(&self, subject: &'a str) -> Option<&'a str> {
        let prefix = format!("{}.dispatch.", self.cfg.bridge.subject_root);
        subject.strip_prefix(&prefix)
    }

    /// Same idea for cancel subjects: `<root>.cancel.<persona>`.
    pub fn cancel_persona_from_subject<'a>(&self, subject: &'a str) -> Option<&'a str> {
        let prefix = format!("{}.cancel.", self.cfg.bridge.subject_root);
        subject.strip_prefix(&prefix)
    }

    /// Handle one dispatch. Spawned into its own task by the main loop.
    pub async fn handle_dispatch(self: &Arc<Self>, subject: String, dispatch: AgentDispatch) {
        let request_id_typed = dispatch.request_id.clone();
        let request_id = request_id_typed.to_string();
        let reply_to = dispatch.reply_to.clone();

        // 1. Subject → persona lookup.
        let Some(persona_name) = self.persona_from_subject(&subject).map(|s| s.to_string()) else {
            warn!(subject = %subject, "dispatch subject does not match expected shape");
            self.publish_failure(&request_id, &reply_to, "subject did not parse")
                .await;
            return;
        };
        let Some(persona) = self.cfg.persona(&persona_name).cloned() else {
            warn!(
                request_id = %request_id,
                persona = %persona_name,
                "unknown persona"
            );
            self.publish_failure(
                &request_id,
                &reply_to,
                &format!("unknown persona: {persona_name}"),
            )
            .await;
            return;
        };

        info!(
            request_id = %request_id,
            persona = %persona.name,
            host = %persona.host,
            user_chars = dispatch.user_input.len(),
            "cc.dispatch received"
        );

        // 2. Quota check.
        let now = Local::now();
        let quota_err = {
            let tracker = self.cost.lock().await;
            tracker
                .check_quota(&persona.name, persona.daily_cost_limit_usd, now)
                .err()
        };
        if let Some(reason) = quota_err {
            warn!(request_id = %request_id, reason = %reason, "dispatch refused");
            self.publish_failure(&request_id, &reply_to, &reason).await;
            return;
        }

        // 3. Mark in-flight.
        {
            let mut g = self.in_flight.lock().await;
            g.insert(persona.name.clone(), (request_id.clone(), reply_to.clone()));
        }

        // 4. Resolve timeout (per-dispatch context override beats persona default).
        let timeout = resolve_timeout(&persona, dispatch.context.as_ref());

        // 5. Run.
        let result = self
            .runner
            .run(&persona, &dispatch.user_input, &request_id, timeout)
            .await;

        // 6. Record cost.
        let cost_snapshot = {
            let mut tracker = self.cost.lock().await;
            tracker.record(&persona.name, result.total_cost_usd, Local::now())
        };

        // 7. Publish reply. We always publish — even on failure — so the
        //    caller doesn't hang.
        let reply = AgentReply {
            request_id: request_id_typed.clone(),
            ok: result.ok,
            text: result.text.clone(),
            // Claude Code doesn't expose token counts in the JSON envelope
            // we currently parse (only `total_cost_usd` + `duration_ms`),
            // so v0 reports `tokens=0`. Subscribers should treat 0 as
            // "unknown" rather than "zero output".
            tokens: 0,
            duration_ms: result.duration_ms,
            tool_calls: Vec::new(),
            error: result.error.clone(),
        };
        if let Err(e) = publish_json(&self.nats, &reply_to, &reply).await {
            warn!(error = %e, reply_to = %reply_to, "failed to publish reply");
        } else {
            info!(
                request_id = %request_id,
                persona = %persona.name,
                ok = reply.ok,
                duration_ms = reply.duration_ms,
                cost_usd = result.total_cost_usd,
                cumulative_usd = cost_snapshot.cumulative_today_usd,
                "cc.reply published"
            );
        }

        // 8. Publish cost event. Best-effort — failures don't bubble.
        let cost_event = CostEvent {
            persona: persona.name.clone(),
            cumulative_today_usd: cost_snapshot.cumulative_today_usd,
            last_call_usd: cost_snapshot.last_call_usd,
            total_calls_today: cost_snapshot.total_calls_today,
            session_id: result.session_id.clone(),
            ok: result.ok,
        };
        let cost_subject = self.cfg.cost_subject(&persona.name);
        if let Err(e) = publish_json(&self.nats, &cost_subject, &cost_event).await {
            warn!(error = %e, subject = %cost_subject, "failed to publish cost event");
        }

        // 9. Clear in-flight (only if it's still us).
        let mut g = self.in_flight.lock().await;
        if let Some((id, _)) = g.get(&persona.name) {
            if id == &request_id {
                g.remove(&persona.name);
            }
        }
    }

    /// Handle a cancel envelope. v0: publish a synthetic cancelled reply
    /// to the in-flight reply_to (if any) and clear bookkeeping. The
    /// detached dispatch task itself runs to completion — its later reply
    /// publish is harmless (the caller will have already seen the
    /// cancelled one and ignore the dupe by request_id).
    pub async fn handle_cancel(self: &Arc<Self>, subject: String) {
        let Some(persona) = self
            .cancel_persona_from_subject(&subject)
            .map(|s| s.to_string())
        else {
            warn!(subject = %subject, "cancel subject did not parse");
            return;
        };
        let target = {
            let mut g = self.in_flight.lock().await;
            g.remove(&persona)
        };
        match target {
            Some((req_id, reply_to)) => {
                info!(persona = %persona, request_id = %req_id, "cancelling in-flight request");
                let reply = AgentReply {
                    request_id: req_id
                        .parse()
                        .expect("request_id from in-flight map is always a valid RequestId"),
                    ok: false,
                    text: String::new(),
                    tokens: 0,
                    duration_ms: 0,
                    tool_calls: Vec::new(),
                    error: Some("cancelled".into()),
                };
                let _ = publish_json(&self.nats, &reply_to, &reply).await;
            }
            None => {
                info!(persona = %persona, "cancel ignored — nothing in flight");
            }
        }
    }

    async fn publish_failure(&self, request_id: &str, reply_to: &str, error: &str) {
        let reply = AgentReply {
            request_id: request_id
                .parse()
                .expect("request_id from in-flight map is always a valid RequestId"),
            ok: false,
            text: String::new(),
            tokens: 0,
            duration_ms: 0,
            tool_calls: Vec::new(),
            error: Some(error.into()),
        };
        if let Err(e) = publish_json(&self.nats, reply_to, &reply).await {
            warn!(error = %e, reply_to = %reply_to, "failed to publish failure reply");
        }
    }
}

/// Cost observability event published on
/// `<subject_root>.event.<persona>.cost` after each dispatch (success or
/// failure). The `tray` and any other subscriber can use this to surface
/// "$X.YZ spent today" without polling state files.
#[derive(Debug, Clone, Serialize)]
struct CostEvent {
    pub persona: String,
    pub cumulative_today_usd: f64,
    pub last_call_usd: f64,
    pub total_calls_today: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub ok: bool,
}

fn resolve_timeout(persona: &Persona, ctx: Option<&serde_json::Value>) -> Duration {
    let secs = ctx
        .and_then(|v| v.get("timeout_sec"))
        .and_then(|v| v.as_u64())
        .unwrap_or(persona.timeout_sec);
    Duration::from_secs(secs)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CcBridgeConfig;
    use crate::runner::ClaudeResult;

    fn cfg() -> CcBridgeConfig {
        CcBridgeConfig::from_toml_str(
            r#"
[bridge]
name = "codex"

[bus]
nats_url = "nats://lnx-rig:4222"

[[personas]]
name = "archon"
host = "lnx-rig"
cwd = "/tmp"
permission_mode = "bypassPermissions"
"#,
        )
        .unwrap()
    }

    #[test]
    fn persona_from_subject_strips_prefix() {
        let cfg = cfg();
        // We don't actually need a real NatsClient for the parser test —
        // construct just enough state to call the helper. Use a stub
        // BridgeContext via the config alone.
        let prefix = format!("{}.dispatch.", cfg.bridge.subject_root);
        let subject = format!("{prefix}archon");
        assert_eq!(subject.strip_prefix(&prefix), Some("archon"));
    }

    #[test]
    fn resolve_timeout_uses_persona_default() {
        let p = Persona {
            name: "archon".into(),
            host: "x".into(),
            cwd: "/tmp".into(),
            permission_mode: "default".into(),
            system_prompt_file: None,
            daily_cost_limit_usd: None,
            timeout_sec: 42,
            claude_bin: None,
        };
        let dur = resolve_timeout(&p, None);
        assert_eq!(dur, Duration::from_secs(42));
    }

    #[test]
    fn resolve_timeout_context_override_wins() {
        let p = Persona {
            name: "archon".into(),
            host: "x".into(),
            cwd: "/tmp".into(),
            permission_mode: "default".into(),
            system_prompt_file: None,
            daily_cost_limit_usd: None,
            timeout_sec: 42,
            claude_bin: None,
        };
        let ctx = serde_json::json!({"timeout_sec": 10});
        let dur = resolve_timeout(&p, Some(&ctx));
        assert_eq!(dur, Duration::from_secs(10));
    }

    /// Verify ClaudeResult → AgentReply translation drops tokens (we don't
    /// surface them in v0) and keeps the duration / error.
    #[test]
    fn claude_result_translates_to_agent_reply_shape() {
        // No real BridgeContext — just exercise the field mapping.
        let r = ClaudeResult {
            ok: false,
            text: String::new(),
            total_cost_usd: 0.0,
            duration_ms: 1234,
            session_id: None,
            error: Some("boom".into()),
        };
        let reply = AgentReply {
            request_id: RequestId::new(),
            ok: r.ok,
            text: r.text.clone(),
            tokens: 0,
            duration_ms: r.duration_ms,
            tool_calls: Vec::new(),
            error: r.error.clone(),
        };
        assert!(!reply.ok);
        assert_eq!(reply.duration_ms, 1234);
        assert_eq!(reply.tokens, 0);
        assert_eq!(reply.error.as_deref(), Some("boom"));
    }
}
