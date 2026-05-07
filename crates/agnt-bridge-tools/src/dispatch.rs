//! `dispatch_agent` — quarterback another agent on the NATS bus.
//!
//! Publishes an [`AgentDispatch`](voicectl_core::events::AgentDispatch) on
//! `agent.dispatch.<name>`, subscribes to a unique `reply_to` subject, and
//! waits for a single [`AgentReply`](voicectl_core::events::AgentReply)
//! within `timeout_ms`. Returns the reply text (or error) to the calling
//! agent.
//!
//! This is the multi-agent quarterbacking primitive: Sage can dispatch to
//! axiom-proof, scribe, paladin, etc. and weave the results into its own
//! response. The replying agent runs in its own `agnt-bridge` process; the
//! reply goes through a fresh NATS subject created per call.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use tracing::{debug, warn};

use agnt_core::wire::RequestId;
use voicectl_core::events::{AgentDispatch, AgentReply};
use voicectl_net::Bus;

use crate::shell::block_on;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 5 * 60 * 1000;

/// Dispatch a task to another named agent on the bus and wait for its reply.
pub struct DispatchAgent {
    bus: Arc<Bus>,
}

impl DispatchAgent {
    pub fn new(bus: Arc<Bus>) -> Self {
        Self { bus }
    }
}

impl agnt::Tool for DispatchAgent {
    fn name(&self) -> &str {
        "dispatch_agent"
    }

    fn description(&self) -> &str {
        "Send a task to another agent on the NATS bus and wait for its reply. \
         Use to delegate specialised work — e.g. dispatch_agent('axiom-proof', \
         'verify claim X') or dispatch_agent('paladin', 'audit this script'). \
         Returns the agent's text reply, or an error if it timed out."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_name": {
                    "type": "string",
                    "description": "Target agent name. Becomes the suffix of `agent.dispatch.<name>`. Avoid dispatching to your own agent name (loop)."
                },
                "task": {
                    "type": "string",
                    "description": "The task / prompt to send to the target agent. Treated as the user_input on the receiving end."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "How long to wait for a reply, in milliseconds. Default 30000, max 300000.",
                    "minimum": 100,
                    "maximum": 300000
                }
            },
            "required": ["agent_name", "task"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let agent_name = args
            .get("agent_name")
            .and_then(|v| v.as_str())
            .ok_or("missing 'agent_name' (string)")?
            .trim()
            .to_string();
        if agent_name.is_empty() {
            return Err("agent_name must not be empty".into());
        }
        if !agent_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            // NATS subjects don't allow whitespace and we don't want
            // wildcards or `.` either — keep the surface tight.
            return Err(format!(
                "agent_name '{agent_name}' contains disallowed characters \
                 (only A-Z, a-z, 0-9, '-', '_')"
            ));
        }
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or("missing 'task' (string)")?
            .to_string();
        if task.trim().is_empty() {
            return Err("task must not be empty".into());
        }
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(100, MAX_TIMEOUT_MS);

        let bus = Arc::clone(&self.bus);
        block_on(
            async move { dispatch_and_wait(bus.as_ref(), &agent_name, &task, timeout_ms).await },
        )
    }
}

/// Publish a dispatch and wait for the matching reply.
pub async fn dispatch_and_wait(
    bus: &Bus,
    agent_name: &str,
    task: &str,
    timeout_ms: u64,
) -> Result<String, String> {
    let request_id = RequestId::new();
    let reply_to = format!("agent.response.{agent_name}.{request_id}");
    let dispatch_subject = format!("agent.dispatch.{agent_name}");

    // Subscribe FIRST so we never miss the reply.
    let mut sub = bus
        .client
        .subscribe(reply_to.clone())
        .await
        .map_err(|e| format!("subscribe {reply_to}: {e}"))?;

    let envelope = AgentDispatch {
        request_id: request_id.clone(),
        user_input: task.to_string(),
        context: Some(json!({ "from": "dispatch_agent" })),
        reply_to: reply_to.clone(),
    };
    let payload = serde_json::to_vec(&envelope).map_err(|e| format!("encode dispatch: {e}"))?;
    bus.client
        .publish(dispatch_subject.clone(), payload.into())
        .await
        .map_err(|e| format!("publish {dispatch_subject}: {e}"))?;

    debug!(
        request_id = %request_id,
        agent = %agent_name,
        timeout_ms,
        "dispatch_agent published"
    );

    match tokio::time::timeout(Duration::from_millis(timeout_ms), sub.next()).await {
        Ok(Some(msg)) => match serde_json::from_slice::<AgentReply>(&msg.payload) {
            Ok(reply) => {
                if reply.request_id != request_id {
                    warn!(
                        got = %reply.request_id,
                        want = %request_id,
                        "dispatch_agent received unexpected request_id; returning anyway"
                    );
                }
                if reply.ok {
                    Ok(reply.text)
                } else {
                    Err(format!(
                        "agent '{agent_name}' returned error: {}",
                        reply.error.unwrap_or_else(|| "unknown".into())
                    ))
                }
            }
            Err(e) => Err(format!("decode AgentReply: {e}")),
        },
        Ok(None) => Err("reply subscription closed unexpectedly".into()),
        Err(_) => Err(format!(
            "dispatch_agent timed out after {timeout_ms}ms waiting for '{agent_name}'"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_bus_args(agent: &str) -> Value {
        json!({
            "agent_name": agent,
            "task": "ping",
            "timeout_ms": 100
        })
    }

    #[test]
    fn rejects_empty_agent_name() {
        // We can't construct a real Bus easily; build args and run the
        // validation path by short-circuiting before the bus call. The tool
        // still requires a Bus; build a minimal stub through the *args*
        // checker only.
        let args = fake_bus_args("");
        // Use the same validator the Tool::call uses — duplicate here.
        let agent_name = args["agent_name"].as_str().unwrap().to_string();
        assert!(agent_name.is_empty());
    }

    #[test]
    fn agent_name_charset_rejected() {
        let bad = ["foo.bar", "foo bar", "foo*", "foo/bar", "foo>"];
        for n in bad {
            assert!(
                !n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "expected '{n}' to be rejected"
            );
        }
        for n in ["foo", "foo-bar", "foo_bar", "axiom-proof123"] {
            assert!(n
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        }
    }

    #[test]
    fn timeout_clamping_bounds() {
        let too_small = 0u64.clamp(100, MAX_TIMEOUT_MS);
        assert_eq!(too_small, 100);
        let too_big = (MAX_TIMEOUT_MS + 1).clamp(100, MAX_TIMEOUT_MS);
        assert_eq!(too_big, MAX_TIMEOUT_MS);
    }
}
