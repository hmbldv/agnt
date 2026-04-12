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

use crate::message::{Message, ToolCall};

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
}

/// A no-op observer used as the default when the agent is constructed
/// without one.
pub struct NoOpObserver;

impl Observer for NoOpObserver {}
