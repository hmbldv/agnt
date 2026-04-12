use crate::backend::{Backend, Message};
use crate::store::Store;
use crate::tool::Registry;
use std::io::Write;

/// The core agent loop: message → inference → parallel tool dispatch → loop.
///
/// An `Agent` owns a [`Backend`] for LLM inference, a [`Registry`] of tools,
/// a conversation history, and optional [`Store`] for persistence. Calling
/// [`Agent::step`] with user input runs the loop until the model returns
/// without requesting tools, or until `max_steps` is hit.
///
/// # Example
///
/// ```no_run
/// use agnt::{Agent, Backend};
///
/// let backend = Backend::ollama("gemma4:e4b");
/// let mut agent = Agent::new(backend, "You are concise.");
/// agent.tools.register(Box::new(agnt::builtins::Grep));
///
/// let answer = agent.step("Find TODOs in src/").unwrap();
/// println!("{}", answer);
/// ```
pub struct Agent {
    /// LLM backend used for inference.
    pub backend: Backend,
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
    /// Optional SQLite session store for persistence across processes.
    pub store: Option<Store>,
    /// Session identifier for the store (defaults to "default").
    pub session: String,
    /// Whether to stream output tokens to stdout. Defaults to true.
    pub stream: bool,
}

impl Agent {
    /// Create a new agent with the given backend and system prompt.
    ///
    /// The agent starts with a fresh message history containing only the system
    /// prompt. No tools are registered and no persistence is attached.
    pub fn new(backend: Backend, system: &str) -> Self {
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
            stream: true,
        }
    }

    pub fn attach_store(&mut self, store: Store, session: &str) -> Result<(), String> {
        let loaded = store.load(session)?;
        if loaded.is_empty() {
            for m in &self.messages {
                store.append(session, m)?;
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

    /// Return a send-window: system prompt + trailing N messages,
    /// advancing the cut point forward until it lands on a user message
    /// so we never split a tool_use/tool_result pair.
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

    pub fn step(&mut self, user_input: &str) -> Result<String, String> {
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
                let r = self.backend.chat(&send, &tools, Some(&mut sink))?;
                println!();
                r
            } else {
                self.backend.chat(&send, &tools, None)?
            };

            let tool_calls = resp.tool_calls.clone();
            self.persist(&resp);
            self.messages.push(resp.clone());

            let calls = match tool_calls {
                Some(c) if !c.is_empty() => c,
                _ => return Ok(resp.content.unwrap_or_default()),
            };

            let registry = &self.tools;
            let results: Vec<(String, String, String, String, u64)> = std::thread::scope(|s| {
                let handles: Vec<_> = calls
                    .iter()
                    .map(|call| {
                        let name = call.function.name.clone();
                        let id = call.id.clone();
                        let args_str = call.function.arguments.clone();
                        s.spawn(move || {
                            let args: serde_json::Value =
                                serde_json::from_str(&args_str).unwrap_or(serde_json::Value::Null);
                            let t0 = std::time::Instant::now();
                            let result = registry
                                .dispatch(&name, args)
                                .unwrap_or_else(|e| format!("error: {}", e));
                            let dur = t0.elapsed().as_micros() as u64;
                            (id, name, args_str, result, dur)
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });

            for (id, name, args_str, result, dur_us) in results {
                if self.stream {
                    println!("[tool: {} ({:.2}ms)]", name, dur_us as f64 / 1000.0);
                }
                if let Some(s) = &self.store {
                    if let Err(e) =
                        s.log_tool(&self.session, &name, &args_str, &result, dur_us)
                    {
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

        Err("max steps exceeded".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Backend, FunctionCall, Message, ToolCall};

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
        let mut a = Agent::new(Backend::ollama("m"), "sys");
        a.max_window = 10;
        a.messages.push(msg("user", "hi"));
        a.messages.push(msg("assistant", "hello"));
        let w = a.windowed();
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].role, "system");
    }

    #[test]
    fn windowing_preserves_system_and_starts_at_user() {
        let mut a = Agent::new(Backend::ollama("m"), "sys");
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
        // Simulate: system, user, assistant(tool_call), tool, assistant, user, assistant, ...
        let mut a = Agent::new(Backend::ollama("m"), "sys");
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
        // No orphaned tool_result should appear as the first non-system message.
        assert_eq!(w[1].role, "user");
    }
}
