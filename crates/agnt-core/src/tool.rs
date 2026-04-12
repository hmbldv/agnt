//! The [`Tool`] trait — agent-callable capabilities.
//!
//! The current (v0.1-compatible) signature uses `serde_json::Value` for args
//! and `String` for output. v0.2 Phase 1 will introduce a typed variant
//! (`TypedTool` with associated `Args`/`Output`/`Error` types) alongside this
//! one, with an `ErasedTool` adapter bridging typed impls into the dyn path.
//!
//! See v0.2 plan doc Work Item A1.

use serde_json::Value;

/// A tool the agent can invoke.
pub trait Tool: Send + Sync {
    /// The tool's name — used by the model to invoke it and for dispatch.
    fn name(&self) -> &str;

    /// Human-readable description sent to the model as part of the tool list.
    /// This is the primary steering mechanism for tool selection.
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's arguments.
    fn schema(&self) -> Value;

    /// Execute the tool synchronously. Return a string result or an error
    /// message. Callers must enforce result-byte caps and envelope framing
    /// before persisting or feeding back to the model.
    fn call(&self, args: Value) -> Result<String, String>;
}

/// A collection of tools with name-based dispatch.
///
/// The [`Agent`](crate::Agent) holds a `Registry` and uses it to dispatch
/// tool calls from the model. Tools can be registered at any time before
/// or between calls to [`Agent::step`](crate::Agent::step).
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn dispatch(&self, name: &str, args: Value) -> Result<String, String> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| format!("unknown tool: {}", name))?
            .call(args)
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    pub fn as_openai_tools(&self) -> Value {
        Value::Array(
            self.tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name(),
                            "description": t.description(),
                            "parameters": t.schema(),
                        }
                    })
                })
                .collect(),
        )
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
