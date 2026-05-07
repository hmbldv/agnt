//! The [`Observer`] trait — single extension point for lifecycle hooks.
//!
//! Observers see every step start/end and every tool start/end during an
//! agent's execution. They are the designated integration point for:
//!
//! - Audit logging (persist everything to an external system)
//! - Human-in-the-loop approval (block or deny tool calls)
//! - Event bus publishing (stream to NATS, Kafka, Redis)
//! - Metrics collection (latency histograms, error rates)
//! - OpenTelemetry spans (via a `tracing-opentelemetry` bridge)
//!
//! There is exactly ONE observer per [`Agent`](crate::Agent), not a Vec.
//! Users who want to fan out to multiple destinations can wrap their
//! concerns in a single composite `Observer` impl.

use crate::message::{Message, ToolCall, UsageStats};

/// Result of a tool execution, passed to observers after dispatch.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub name: String,
    pub output: Result<String, String>,
    pub duration_us: u64,
}

/// Context for a step lifecycle event.
///
/// Carries the session id and the user input that triggered this step.
/// Expands in v0.3 to include step number and deadline.
#[derive(Debug, Clone)]
pub struct StepContext {
    pub session: String,
    pub user_input: String,
}

/// Disposition returned by [`Observer::should_dispatch`] — whether a tool
/// call should proceed, be refused, or be intercepted.
///
/// v0.3 C2. Added as the canonical extension point for trust tier enforcement,
/// human-in-the-loop approval, and policy gating. The agent treats
/// [`Disposition::Refused`] as a synthetic tool result (fed back to the model
/// as an error) so the loop continues instead of aborting.
#[derive(Debug, Clone)]
pub enum Disposition {
    /// The tool call may proceed normally.
    Allow,
    /// The tool call is refused. The provided message becomes the tool
    /// result passed back to the model ("wrapped" in the standard
    /// `<tool_output>` envelope by the agent).
    Refused(String),
}

/// Lifecycle observer. Every method has a default no-op implementation so
/// implementors override only the hooks they care about.
pub trait Observer: Send + Sync {
    /// Called when [`Agent::step`](crate::Agent::step) begins.
    fn on_step_start(&self, _ctx: &StepContext) {}

    /// Called before each tool dispatch inside the step loop.
    fn on_tool_start(&self, _call: &ToolCall) {}

    /// Called after each tool dispatch completes, with the result.
    fn on_tool_end(&self, _call: &ToolCall, _result: &ToolResult) {}

    /// Called when the step loop terminates with a final assistant message.
    fn on_step_end(&self, _response: &Message) {}

    /// Called if the step loop errors out before producing a final message.
    fn on_step_error(&self, _error: &str) {}

    /// Called at step completion (success or error) with cumulative token usage
    /// for all inference turns in this step. Tokens are zero when the backend
    /// didn't surface usage data (e.g. streaming without a usage event).
    fn on_step_usage(&self, _usage: UsageStats) {}

    /// v0.3 C2 — policy gate fired BEFORE every tool dispatch.
    ///
    /// Returning [`Disposition::Allow`] (the default) lets the call proceed.
    /// Returning [`Disposition::Refused`] causes the agent to skip the actual
    /// tool call and return the provided message to the model as a synthetic
    /// tool result. The loop continues — the model may choose to call a
    /// different tool, retry with different arguments, or stop.
    ///
    /// This is the canonical extension point for:
    /// - Trust tier enforcement (deny by policy)
    /// - Human-in-the-loop approval (block until a human clicks "allow")
    /// - Quota accounting layered on top of the built-in `Agent::tool_quotas`
    /// - Content filtering on tool arguments
    ///
    /// Default impl always allows. Existing `Observer` implementations do
    /// not need to change.
    fn should_dispatch(&self, _call: &ToolCall) -> Disposition {
        Disposition::Allow
    }
}

/// A no-op observer used as the default when the agent is constructed
/// without one.
pub struct NoOpObserver;

impl Observer for NoOpObserver {}
