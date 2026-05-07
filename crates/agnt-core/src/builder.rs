//! Builder pattern for [`Agent`] and backend construction.
//!
//! v0.1 exposed public fields on `Agent` (`stream`, `max_steps`, `max_window`,
//! etc.) which was ergonomic but leaky — users had to know the internals to
//! configure the agent. v0.2 adds [`AgentBuilder`] as the preferred
//! construction path while keeping the public fields accessible for backward
//! compatibility and advanced use.

use crate::agent::Agent;
use crate::backend_trait::LlmBackend;
use crate::observer::{NoOpObserver, Observer};
use crate::store_trait::MessageStore;
use crate::tool::Tool;
use std::sync::Arc;

/// Fluent builder for [`Agent`].
///
/// # Example
///
/// ```ignore
/// use agnt_core::AgentBuilder;
/// use my_backend::Backend;
///
/// let agent = AgentBuilder::new(Backend::ollama("gemma4:e4b"))
///     .system("You are a helpful assistant.")
///     .max_steps(5)
///     .max_window(20)
///     .max_tool_result_bytes(32 * 1024)
///     .on_token(Box::new(|tok| print!("{}", tok)))
///     .build();
/// ```
pub struct AgentBuilder<B: LlmBackend> {
    backend: B,
    system: String,
    tools: Vec<Box<dyn Tool>>,
    max_steps: Option<usize>,
    max_window: Option<usize>,
    max_tool_result_bytes: Option<usize>,
    max_tool_output_chars: Option<usize>,
    store: Option<Arc<dyn MessageStore>>,
    session: Option<String>,
    observer: Option<Arc<dyn Observer>>,
    on_token: Option<Box<dyn FnMut(&str) + Send>>,
    max_step_duration: Option<std::time::Duration>,
}

impl<B: LlmBackend> AgentBuilder<B> {
    /// Start a new builder with the given backend. The system prompt defaults
    /// to the empty string and can be set with [`AgentBuilder::system`].
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            system: String::new(),
            tools: Vec::new(),
            max_steps: None,
            max_window: None,
            max_tool_result_bytes: None,
            max_tool_output_chars: None,
            store: None,
            session: None,
            observer: None,
            on_token: None,
            max_step_duration: None,
        }
    }

    /// Set the system prompt.
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = system.into();
        self
    }

    /// Register a single tool.
    pub fn tool(mut self, tool: Box<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Register many tools at once.
    pub fn tools(mut self, tools: Vec<Box<dyn Tool>>) -> Self {
        self.tools.extend(tools);
        self
    }

    /// Maximum inference turns per [`Agent::step`] call. Default: 10.
    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps = Some(n);
        self
    }

    /// Maximum messages sent to the backend per turn. Default: 40.
    pub fn max_window(mut self, n: usize) -> Self {
        self.max_window = Some(n);
        self
    }

    /// Cap on raw bytes per tool result before envelope framing. Default: 64KB.
    pub fn max_tool_result_bytes(mut self, n: usize) -> Self {
        self.max_tool_result_bytes = Some(n);
        self
    }

    /// Cap on Unicode characters per tool result. Default: 8 000.
    pub fn max_tool_output_chars(mut self, n: usize) -> Self {
        self.max_tool_output_chars = Some(n);
        self
    }

    /// Attach a persistent message store and session id.
    pub fn store(mut self, store: Arc<dyn MessageStore>, session: impl Into<String>) -> Self {
        self.store = Some(store);
        self.session = Some(session.into());
        self
    }

    /// Attach a lifecycle observer.
    pub fn observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Install a token callback. Each streamed delta from the backend is
    /// passed to this closure during inference.
    pub fn on_token(mut self, sink: Box<dyn FnMut(&str) + Send>) -> Self {
        self.on_token = Some(sink);
        self
    }

    /// Set a wall-clock deadline for a single [`Agent::step`] call.
    ///
    /// See [`Agent::max_step_duration`] for the enforcement semantics.
    /// Unset (the default) preserves the unbounded v0.3 behavior.
    pub fn max_step_duration(mut self, d: std::time::Duration) -> Self {
        self.max_step_duration = Some(d);
        self
    }

    /// Finalize the builder and produce an [`Agent`].
    ///
    /// If a store was provided, this calls [`Agent::attach_store`]. Any error
    /// from attaching the store is returned.
    pub fn build(self) -> Result<Agent<B>, String> {
        let mut agent = Agent::new(self.backend, &self.system);
        for tool in self.tools {
            agent.tools.register(tool);
        }
        if let Some(n) = self.max_steps {
            agent.max_steps = n;
        }
        if let Some(n) = self.max_window {
            agent.max_window = n;
        }
        if let Some(n) = self.max_tool_result_bytes {
            agent.max_tool_result_bytes = n;
        }
        if let Some(n) = self.max_tool_output_chars {
            agent.max_tool_output_chars = n;
        }
        if let Some(obs) = self.observer {
            agent.observer = obs;
        } else {
            agent.observer = Arc::new(NoOpObserver);
        }
        agent.on_token = self.on_token;
        agent.max_step_duration = self.max_step_duration;
        if let Some(store) = self.store {
            let session = self.session.unwrap_or_else(|| "default".into());
            agent.attach_store(store, &session)?;
        }
        Ok(agent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend_trait::BackendError;
    use crate::message::Message;
    use serde_json::Value;

    struct MockBackend;
    impl LlmBackend for MockBackend {
        fn model(&self) -> &str {
            "mock"
        }
        fn chat(
            &self,
            _messages: &[Message],
            _tools: &Value,
            _on_token: Option<&mut dyn FnMut(&str)>,
        ) -> Result<Message, BackendError> {
            Ok(Message {
                role: "assistant".into(),
                content: Some("ok".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
                usage: None,
            })
        }
    }

    #[test]
    fn builder_sets_defaults() {
        let agent = AgentBuilder::new(MockBackend)
            .system("sys")
            .build()
            .unwrap();
        assert_eq!(agent.max_steps, 25);
        assert_eq!(agent.max_window, 40);
        assert_eq!(agent.messages[0].role, "system");
        assert_eq!(agent.messages[0].content.as_deref(), Some("sys"));
    }

    #[test]
    fn builder_overrides() {
        let agent = AgentBuilder::new(MockBackend)
            .system("sys")
            .max_steps(3)
            .max_window(5)
            .max_tool_result_bytes(1024)
            .build()
            .unwrap();
        assert_eq!(agent.max_steps, 3);
        assert_eq!(agent.max_window, 5);
        assert_eq!(agent.max_tool_result_bytes, 1024);
    }

    #[test]
    fn builder_accepts_multiple_tools() {
        use crate::tool::Tool;
        struct Dummy(&'static str);
        impl Tool for Dummy {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "dummy"
            }
            fn schema(&self) -> Value {
                serde_json::json!({ "type": "object" })
            }
            fn call(&self, _args: Value) -> Result<String, String> {
                Ok("".into())
            }
        }
        let agent = AgentBuilder::new(MockBackend)
            .tool(Box::new(Dummy("a")))
            .tool(Box::new(Dummy("b")))
            .tools(vec![Box::new(Dummy("c")), Box::new(Dummy("d"))])
            .build()
            .unwrap();
        assert_eq!(agent.tools.names(), vec!["a", "b", "c", "d"]);
    }
}
