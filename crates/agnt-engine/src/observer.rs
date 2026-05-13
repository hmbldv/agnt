use agnt_core::{Disposition, Observer, StepContext, ToolCall, ToolResult};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::debug;

/// Observer that bridges agnt-core's sync step loop with the engine's
/// async execution layer. Enforces tool permissions and meters budget.
pub struct EngineObserver {
    /// Accumulated credits consumed (read by the engine between steps).
    credits: Arc<AtomicU64>,
    /// Tools this agent is allowed to call (empty = all allowed).
    permitted_tools: HashSet<String>,
    /// Tools explicitly denied.
    denied_tools: HashSet<String>,
    /// Credits per millisecond of tool execution.
    credits_per_ms: u64,
}

impl EngineObserver {
    pub fn new(
        credits: Arc<AtomicU64>,
        permitted_tools: Vec<String>,
        denied_tools: Vec<String>,
    ) -> Self {
        Self {
            credits,
            permitted_tools: permitted_tools.into_iter().collect(),
            denied_tools: denied_tools.into_iter().collect(),
            credits_per_ms: 1, // 1 credit per ms of tool execution
        }
    }

    /// Read the current accumulated credits.
    pub fn credits_consumed(credits: &Arc<AtomicU64>) -> u64 {
        credits.load(Ordering::Relaxed)
    }

    /// Create a shared credits counter.
    pub fn credits_counter() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }
}

impl Observer for EngineObserver {
    fn on_step_start(&self, ctx: &StepContext) {
        debug!(session = %ctx.session, "step start");
    }

    fn should_dispatch(&self, call: &ToolCall) -> Disposition {
        let tool_name = &call.function.name;

        // Deny list takes precedence.
        if self.denied_tools.contains(tool_name) {
            return Disposition::Refused(format!("tool '{}' is denied by policy", tool_name));
        }

        // If permit list is non-empty, tool must be in it.
        if !self.permitted_tools.is_empty() && !self.permitted_tools.contains(tool_name) {
            return Disposition::Refused(format!(
                "tool '{}' is not in the permitted set",
                tool_name
            ));
        }

        Disposition::Allow
    }

    fn on_tool_end(&self, _call: &ToolCall, result: &ToolResult) {
        // Meter: convert tool duration to credits.
        let credits = (result.duration_us / 1000) * self.credits_per_ms;
        self.credits.fetch_add(credits, Ordering::Relaxed);
    }

    fn on_step_error(&self, error: &str) {
        debug!(error = %error, "step error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_allowed_tool() {
        let credits = EngineObserver::credits_counter();
        let obs = EngineObserver::new(credits, vec!["read_file".into()], vec![]);
        let call = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: agnt_core::FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        };
        assert!(matches!(obs.should_dispatch(&call), Disposition::Allow));
    }

    #[test]
    fn refuses_unlisted_tool() {
        let credits = EngineObserver::credits_counter();
        let obs = EngineObserver::new(credits, vec!["read_file".into()], vec![]);
        let call = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: agnt_core::FunctionCall {
                name: "shell".into(),
                arguments: "{}".into(),
            },
        };
        assert!(matches!(
            obs.should_dispatch(&call),
            Disposition::Refused(_)
        ));
    }

    #[test]
    fn deny_overrides_permit() {
        let credits = EngineObserver::credits_counter();
        let obs = EngineObserver::new(credits, vec!["shell".into()], vec!["shell".into()]);
        let call = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: agnt_core::FunctionCall {
                name: "shell".into(),
                arguments: "{}".into(),
            },
        };
        assert!(matches!(
            obs.should_dispatch(&call),
            Disposition::Refused(_)
        ));
    }

    #[test]
    fn meters_credits() {
        let credits = EngineObserver::credits_counter();
        let obs = EngineObserver::new(credits.clone(), vec![], vec![]);
        let call = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: agnt_core::FunctionCall {
                name: "test".into(),
                arguments: "{}".into(),
            },
        };
        let result = ToolResult {
            name: "test".into(),
            output: Ok("ok".into()),
            duration_us: 5000, // 5ms = 5 credits
        };
        obs.on_tool_end(&call, &result);
        assert_eq!(EngineObserver::credits_consumed(&credits), 5);
    }
}
