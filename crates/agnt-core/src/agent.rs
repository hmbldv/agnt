//! The agent loop — message → inference → parallel tool dispatch → loop.
//!
//! [`Agent`] is generic over a backend (`B: LlmBackend`). A persistent
//! message store is optional and passed as `Option<Box<dyn MessageStore>>`
//! to keep the type surface small.
//!
//! This module contains no I/O — all network and disk access goes through
//! the trait abstractions. That means `agnt-core` compiles to WASM as-is
//! and can be embedded in environments where you bring your own backend.

use crate::backend_trait::LlmBackend;
use crate::message::Message;
use crate::observer::{NoOpObserver, Observer, StepContext, ToolResult};
use crate::store_trait::{MessageStore, ToolLog};
use crate::tool::Registry;
use std::io::Write;
use std::sync::Arc;

/// The agent loop.
pub struct Agent<B: LlmBackend> {
    /// LLM backend used for inference.
    pub backend: B,
    /// Full conversation history (system + user + assistant + tool messages).
    pub messages: Vec<Message>,
    /// Tool registry — tools the model may call.
    pub tools: Registry,
    /// Maximum number of inference turns per [`Agent::step`] call. Defaults to 10.
    pub max_steps: usize,
    /// Maximum messages sent to the backend per turn. Truncation advances to
    /// a user-message boundary so tool_use/tool_result pairs are never split.
    /// Defaults to 40.
    pub max_window: usize,
    /// Optional persistence layer.
    pub store: Option<Arc<dyn MessageStore>>,
    /// Session identifier for the store (defaults to "default").
    pub session: String,
    /// Lifecycle observer. Defaults to `NoOpObserver`.
    pub observer: Arc<dyn Observer>,
    /// Whether to stream output tokens to stdout. Defaults to true.
    ///
    /// NOTE: scheduled for removal in Phase 1 A8 — replaced with an
    /// `on_token` callback for flexible sinks.
    pub stream: bool,
}

impl<B: LlmBackend> Agent<B> {
    /// Create a new agent with the given backend and system prompt.
    ///
    /// The agent starts with a fresh message history containing only the
    /// system prompt. No tools are registered and no persistence is attached.
    pub fn new(backend: B, system: &str) -> Self {
        Self {
            backend,
            messages: vec![Message {
                role: "system".into(),
                content: Some(system.into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            tools: Registry::new(),
            max_steps: 10,
            max_window: 40,
            store: None,
            session: "default".into(),
            observer: Arc::new(NoOpObserver),
            stream: true,
        }
    }

    /// Attach a persistent message store and resume the named session.
    ///
    /// If the session has no prior history, the current in-memory messages
    /// (i.e. the system prompt) are persisted. Otherwise, the agent's
    /// history is replaced with the loaded session.
    pub fn attach_store(
        &mut self,
        store: Arc<dyn MessageStore>,
        session: &str,
    ) -> Result<(), String> {
        let loaded = store.load(session).map_err(|e| e.to_string())?;
        if loaded.is_empty() {
            for m in &self.messages {
                store.append(session, m).map_err(|e| e.to_string())?;
            }
        } else {
            self.messages = loaded;
        }
        self.store = Some(store);
        self.session = session.into();
        Ok(())
    }

    fn persist(&self, msg: &Message) {
        if let Some(s) = &self.store {
            if let Err(e) = s.append(&self.session, msg) {
                eprintln!("persist: {}", e);
            }
        }
    }

    /// Return a send-window: system prompt + trailing N messages, advancing
    /// the cut point forward until it lands on a user message so we never
    /// split a tool_use / tool_result pair.
    ///
    /// NOTE: P1 in Phase 1 eliminates the clone here.
    fn windowed(&self) -> Vec<Message> {
        if self.messages.len() <= self.max_window {
            return self.messages.clone();
        }
        let n = self.max_window;
        let mut start = self.messages.len() - (n - 1);
        while start < self.messages.len() && self.messages[start].role != "user" {
            start += 1;
        }
        let mut out = Vec::with_capacity(n);
        out.push(self.messages[0].clone());
        out.extend(self.messages[start..].iter().cloned());
        out
    }

    /// Run the agent loop on a new user input.
    ///
    /// Iterates up to `max_steps` times:
    ///  1. Call the backend with the current message window
    ///  2. If the response has no tool calls, return the assistant text
    ///  3. Otherwise dispatch every tool call in parallel via
    ///     `std::thread::scope`, append the results to the message history,
    ///     and loop.
    pub fn step(&mut self, user_input: &str) -> Result<String, String> {
        let ctx = StepContext {
            session: self.session.clone(),
            user_input: user_input.into(),
        };
        self.observer.on_step_start(&ctx);

        let user = Message {
            role: "user".into(),
            content: Some(user_input.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        self.persist(&user);
        self.messages.push(user);

        let tools = self.tools.as_openai_tools();

        for _ in 0..self.max_steps {
            let send = self.windowed();
            let resp = if self.stream {
                let mut sink = |s: &str| {
                    print!("{}", s);
                    std::io::stdout().flush().ok();
                };
                let r = self
                    .backend
                    .chat(&send, &tools, Some(&mut sink))
                    .map_err(|e| e.to_string())?;
                println!();
                r
            } else {
                self.backend
                    .chat(&send, &tools, None)
                    .map_err(|e| e.to_string())?
            };

            let tool_calls = resp.tool_calls.clone();
            self.persist(&resp);
            self.messages.push(resp.clone());

            let calls = match tool_calls {
                Some(c) if !c.is_empty() => c,
                _ => {
                    let out = resp.content.unwrap_or_default();
                    let final_msg = Message {
                        role: "assistant".into(),
                        content: Some(out.clone()),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    };
                    self.observer.on_step_end(&final_msg);
                    return Ok(out);
                }
            };

            let registry = &self.tools;
            let observer = self.observer.clone();
            let results: Vec<(String, String, String, String, u64)> =
                std::thread::scope(|s| {
                    let handles: Vec<_> = calls
                        .iter()
                        .map(|call| {
                            let name = call.function.name.clone();
                            let id = call.id.clone();
                            let args_str = call.function.arguments.clone();
                            let observer = observer.clone();
                            let call_clone = call.clone();
                            s.spawn(move || {
                                observer.on_tool_start(&call_clone);
                                let args: serde_json::Value = serde_json::from_str(&args_str)
                                    .unwrap_or(serde_json::Value::Null);
                                let t0 = std::time::Instant::now();
                                let result = registry
                                    .dispatch(&name, args)
                                    .unwrap_or_else(|e| format!("error: {}", e));
                                let dur = t0.elapsed().as_micros() as u64;
                                let tool_result = ToolResult {
                                    name: name.clone(),
                                    output: Ok(result.clone()),
                                    duration_us: dur,
                                };
                                observer.on_tool_end(&call_clone, &tool_result);
                                (id, name, args_str, result, dur)
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().unwrap())
                        .collect()
                });

            for (id, name, args_str, result, dur_us) in results {
                if self.stream {
                    println!("[tool: {} ({:.2}ms)]", name, dur_us as f64 / 1000.0);
                }
                if let Some(s) = &self.store {
                    let log = ToolLog {
                        name: &name,
                        args: &args_str,
                        result: &result,
                        duration_us: dur_us,
                    };
                    if let Err(e) = s.log_tool(&self.session, &log) {
                        eprintln!("log_tool: {}", e);
                    }
                }
                let msg = Message {
                    role: "tool".into(),
                    content: Some(result),
                    tool_calls: None,
                    tool_call_id: Some(id),
                    name: Some(name),
                };
                self.persist(&msg);
                self.messages.push(msg);
            }
        }

        let err = "max steps exceeded".to_string();
        self.observer.on_step_error(&err);
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend_trait::BackendError;
    use crate::message::{FunctionCall, ToolCall};
    use serde_json::Value;

    /// Mock backend for agent loop unit tests.
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
                content: Some("mock response".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            })
        }
    }

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn windowing_empty_session_returns_all() {
        let mut a = Agent::new(MockBackend, "sys");
        a.max_window = 10;
        a.messages.push(msg("user", "hi"));
        a.messages.push(msg("assistant", "hello"));
        let w = a.windowed();
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].role, "system");
    }

    #[test]
    fn windowing_preserves_system_and_starts_at_user() {
        let mut a = Agent::new(MockBackend, "sys");
        a.max_window = 5;
        for i in 0..20 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            a.messages.push(msg(role, &format!("m{}", i)));
        }
        let w = a.windowed();
        assert_eq!(w[0].role, "system", "system slot preserved");
        assert!(w.len() <= 5, "window respects max_window: {}", w.len());
        assert_eq!(w[1].role, "user", "first post-system must be user");
    }

    #[test]
    fn windowing_skips_orphan_tool_results() {
        let mut a = Agent::new(MockBackend, "sys");
        a.max_window = 4;
        a.messages.push(msg("user", "do thing"));
        a.messages.push(Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "c1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "t".into(),
                    arguments: "{}".into(),
                },
            }]),
            tool_call_id: None,
            name: None,
        });
        a.messages.push(Message {
            role: "tool".into(),
            content: Some("result".into()),
            tool_calls: None,
            tool_call_id: Some("c1".into()),
            name: Some("t".into()),
        });
        a.messages.push(msg("assistant", "done"));
        a.messages.push(msg("user", "next"));
        a.messages.push(msg("assistant", "ok"));
        let w = a.windowed();
        assert_eq!(w[0].role, "system");
        assert_eq!(w[1].role, "user");
    }
}
