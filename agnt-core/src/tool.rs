use serde_json::Value;

/// A tool the agent can invoke.
///
/// Implement this trait to give the agent a capability. Each tool has a name,
/// a human-readable description (shown to the model), a JSON schema for its
/// arguments, and a synchronous `call` method.
///
/// # Example
///
/// ```
/// use agnt::Tool;
/// use serde_json::{json, Value};
///
/// struct UpperCase;
/// impl Tool for UpperCase {
///     fn name(&self) -> &str { "uppercase" }
///     fn description(&self) -> &str { "Uppercase a string." }
///     fn schema(&self) -> Value {
///         json!({
///             "type": "object",
///             "properties": { "text": { "type": "string" } },
///             "required": ["text"]
///         })
///     }
///     fn call(&self, args: Value) -> Result<String, String> {
///         Ok(args["text"].as_str().unwrap_or("").to_uppercase())
///     }
/// }
/// ```
pub trait Tool: Send + Sync {
    /// The tool's name — used by the model to invoke it and for dispatch.
    fn name(&self) -> &str;
    /// Human-readable description sent to the model. This is the primary way
    /// to steer tool selection; a good description dramatically improves
    /// model behavior.
    fn description(&self) -> &str;
    /// JSON Schema describing the tool's arguments.
    fn schema(&self) -> Value;
    /// Execute the tool. Return a string result or an error message.
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
