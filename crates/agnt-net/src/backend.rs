use agnt_core::{BackendError, FunctionCall, LlmBackend, Message, ToolCall};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Openai,
    Anthropic,
}

/// A multi-provider LLM backend.
///
/// Supports Ollama (via OpenAI-compatible API), OpenAI, and Anthropic.
/// All three providers use the same internal `Message`/`ToolCall` format —
/// Anthropic's content-block schema is translated at the wire boundary.
///
/// # Example
///
/// ```no_run
/// use agnt_net::Backend;
///
/// let ollama = Backend::ollama("gemma4:e4b");
/// let openai = Backend::openai("gpt-4o-mini", "sk-...");
/// let anthropic = Backend::anthropic("claude-sonnet-4-6", "sk-ant-...");
/// ```
#[derive(Clone)]
pub struct Backend {
    /// Which provider schema to use on the wire.
    pub kind: Kind,
    /// Base URL for the provider's API.
    pub base_url: String,
    /// Optional API key. `None` for local Ollama.
    pub api_key: Option<String>,
    /// Model identifier passed in every request.
    pub model: String,
}

impl Backend {
    /// Create a backend pointing at a local Ollama server.
    ///
    /// Uses `http://localhost:11434/v1` by default (the OpenAI-compatible endpoint).
    pub fn ollama(model: &str) -> Self {
        Self {
            kind: Kind::Openai,
            base_url: "http://localhost:11434/v1".into(),
            api_key: None,
            model: model.into(),
        }
    }
    /// Create a backend for OpenAI's API.
    pub fn openai(model: &str, api_key: &str) -> Self {
        Self {
            kind: Kind::Openai,
            base_url: "https://api.openai.com/v1".into(),
            api_key: Some(api_key.into()),
            model: model.into(),
        }
    }
    /// Create a backend for Anthropic's native API.
    ///
    /// Message format is automatically translated to Anthropic's content-block
    /// schema at the wire boundary — you still work with the OpenAI-style
    /// [`Message`] type internally.
    pub fn anthropic(model: &str, api_key: &str) -> Self {
        Self {
            kind: Kind::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: Some(api_key.into()),
            model: model.into(),
        }
    }

    pub fn chat(
        &self,
        messages: &[Message],
        tools: &Value,
        sink: Option<&mut dyn FnMut(&str)>,
    ) -> Result<Message, String> {
        match self.kind {
            Kind::Openai => self.chat_openai(messages, tools, sink),
            Kind::Anthropic => self.chat_anthropic(messages, tools, sink),
        }
    }

    fn build_request(&self, url: &str) -> ureq::Request {
        let mut req = crate::http::agent()
            .post(url)
            .set("Content-Type", "application/json");
        if let Some(k) = &self.api_key {
            match self.kind {
                Kind::Openai => {
                    req = req.set("Authorization", &format!("Bearer {}", k));
                }
                Kind::Anthropic => {
                    req = req
                        .set("x-api-key", k)
                        .set("anthropic-version", "2023-06-01");
                }
            }
        }
        req
    }

    fn chat_openai(
        &self,
        messages: &[Message],
        tools: &Value,
        sink: Option<&mut dyn FnMut(&str)>,
    ) -> Result<Message, String> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut body = json!({ "model": self.model, "messages": messages });
        if let Some(arr) = tools.as_array() {
            if !arr.is_empty() {
                body["tools"] = tools.clone();
            }
        }
        if sink.is_some() {
            body["stream"] = Value::Bool(true);
        }

        let resp = with_retry(5, || self.build_request(&url).send_json(body.clone()))?;

        if let Some(sink) = sink {
            parse_openai_stream(resp, sink)
        } else {
            let v: Value = resp.into_json().map_err(|e| format!("decode: {}", e))?;
            let msg = v
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .ok_or_else(|| format!("no message: {}", v))?;
            serde_json::from_value(msg.clone()).map_err(|e| format!("parse: {}", e))
        }
    }

    fn chat_anthropic(
        &self,
        messages: &[Message],
        tools: &Value,
        sink: Option<&mut dyn FnMut(&str)>,
    ) -> Result<Message, String> {
        let url = format!("{}/messages", self.base_url);
        let (system, msgs) = to_anthropic_messages(messages);
        let mut body = json!({
            "model": self.model,
            "messages": msgs,
            "max_tokens": 4096,
        });
        if !system.is_empty() {
            body["system"] = Value::String(system);
        }
        if let Some(arr) = tools.as_array() {
            if !arr.is_empty() {
                let conv: Vec<Value> = arr
                    .iter()
                    .map(|t| {
                        let f = &t["function"];
                        json!({
                            "name": f["name"],
                            "description": f["description"],
                            "input_schema": f["parameters"],
                        })
                    })
                    .collect();
                body["tools"] = Value::Array(conv);
            }
        }
        if sink.is_some() {
            body["stream"] = Value::Bool(true);
        }

        let resp = with_retry(5, || self.build_request(&url).send_json(body.clone()))?;

        if let Some(sink) = sink {
            parse_anthropic_stream(resp, sink)
        } else {
            let v: Value = resp.into_json().map_err(|e| format!("decode: {}", e))?;
            from_anthropic_response(&v)
        }
    }
}

fn with_retry(
    max: u32,
    f: impl Fn() -> Result<ureq::Response, ureq::Error>,
) -> Result<ureq::Response, String> {
    let mut delay = 500u64;
    for i in 0..max {
        match f() {
            Ok(r) => return Ok(r),
            Err(ureq::Error::Status(code, r)) => {
                if (code == 429 || code >= 500) && i + 1 < max {
                    thread::sleep(Duration::from_millis(delay));
                    delay = (delay * 2).min(8000);
                    continue;
                }
                let body = r.into_string().unwrap_or_default();
                return Err(format!("http {}: {}", code, body));
            }
            Err(ureq::Error::Transport(t)) => {
                if i + 1 < max {
                    thread::sleep(Duration::from_millis(delay));
                    delay = (delay * 2).min(8000);
                    continue;
                }
                return Err(format!("transport: {}", t));
            }
        }
    }
    unreachable!()
}

fn to_anthropic_messages(msgs: &[Message]) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut out: Vec<Value> = Vec::new();
    for m in msgs {
        match m.role.as_str() {
            "system" => {
                if !system.is_empty() {
                    system.push('\n');
                }
                if let Some(c) = &m.content {
                    system.push_str(c);
                }
            }
            "user" => {
                out.push(json!({
                    "role": "user",
                    "content": m.content.clone().unwrap_or_default()
                }));
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(c) = &m.content {
                    if !c.is_empty() {
                        blocks.push(json!({"type":"text","text": c}));
                    }
                }
                if let Some(tcs) = &m.tool_calls {
                    for tc in tcs {
                        let input: Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or(json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": input,
                        }));
                    }
                }
                out.push(json!({ "role": "assistant", "content": blocks }));
            }
            "tool" => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content.clone().unwrap_or_default(),
                });
                if let Some(last) = out.last_mut() {
                    if last["role"] == "user" {
                        if last["content"].is_array() {
                            last["content"].as_array_mut().unwrap().push(block);
                            continue;
                        } else {
                            let existing = last["content"].clone();
                            let mut arr: Vec<Value> = Vec::new();
                            if existing.is_string()
                                && !existing.as_str().unwrap_or("").is_empty()
                            {
                                arr.push(json!({"type":"text","text": existing}));
                            }
                            arr.push(block);
                            last["content"] = Value::Array(arr);
                            continue;
                        }
                    }
                }
                out.push(json!({ "role": "user", "content": [block] }));
            }
            _ => {}
        }
    }
    (system, out)
}

fn from_anthropic_response(v: &Value) -> Result<Message, String> {
    let content = v
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| format!("no content: {}", v))?;
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                tool_calls.push(ToolCall {
                    id,
                    call_type: "function".into(),
                    function: FunctionCall {
                        name,
                        arguments: input.to_string(),
                    },
                });
            }
            _ => {}
        }
    }
    Ok(Message {
        role: "assistant".into(),
        content: if text.is_empty() { None } else { Some(text) },
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
        name: None,
    })
}

fn parse_openai_stream(
    resp: ureq::Response,
    sink: &mut dyn FnMut(&str),
) -> Result<Message, String> {
    let reader = BufReader::new(resp.into_reader());
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for line in reader.lines() {
        let line = line.map_err(|e| format!("stream: {}", e))?;
        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => continue,
        };
        if data == "[DONE]" {
            break;
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let delta = match chunk
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
        {
            Some(d) => d,
            None => continue,
        };
        if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
            text.push_str(c);
            sink(c);
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                while tool_calls.len() <= idx {
                    tool_calls.push(ToolCall {
                        id: String::new(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: String::new(),
                            arguments: String::new(),
                        },
                    });
                }
                let slot = &mut tool_calls[idx];
                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                    if !id.is_empty() {
                        slot.id = id.to_string();
                    }
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                        slot.function.name.push_str(n);
                    }
                    if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                        slot.function.arguments.push_str(a);
                    }
                }
            }
        }
    }

    Ok(Message {
        role: "assistant".into(),
        content: if text.is_empty() { None } else { Some(text) },
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
        name: None,
    })
}

fn parse_anthropic_stream(
    resp: ureq::Response,
    sink: &mut dyn FnMut(&str),
) -> Result<Message, String> {
    let reader = BufReader::new(resp.into_reader());
    let mut text = String::new();
    let mut blocks: Vec<(String, String, String, String)> = Vec::new();

    for line in reader.lines() {
        let line = line.map_err(|e| format!("stream: {}", e))?;
        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => continue,
        };
        let ev: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let t = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match t {
            "content_block_start" => {
                let idx = ev.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let block = ev.get("content_block").cloned().unwrap_or(Value::Null);
                let btype = block
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                let id = block
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                while blocks.len() <= idx {
                    blocks.push((String::new(), String::new(), String::new(), String::new()));
                }
                blocks[idx] = (btype, id, name, String::new());
            }
            "content_block_delta" => {
                let idx = ev.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let delta = ev.get("delta").cloned().unwrap_or(Value::Null);
                let dtype = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match dtype {
                    "text_delta" => {
                        if let Some(t) = delta.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                            sink(t);
                        }
                    }
                    "input_json_delta" => {
                        if let Some(p) = delta.get("partial_json").and_then(|p| p.as_str()) {
                            if let Some(slot) = blocks.get_mut(idx) {
                                slot.3.push_str(p);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "message_stop" => break,
            _ => {}
        }
    }

    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for (btype, id, name, args) in blocks {
        if btype == "tool_use" {
            tool_calls.push(ToolCall {
                id,
                call_type: "function".into(),
                function: FunctionCall {
                    name,
                    arguments: if args.is_empty() {
                        "{}".into()
                    } else {
                        args
                    },
                },
            });
        }
    }

    Ok(Message {
        role: "assistant".into(),
        content: if text.is_empty() { None } else { Some(text) },
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
        name: None,
    })
}

impl LlmBackend for Backend {
    fn model(&self) -> &str {
        &self.model
    }

    fn chat(
        &self,
        messages: &[Message],
        tools: &Value,
        on_token: Option<&mut dyn FnMut(&str)>,
    ) -> Result<Message, BackendError> {
        // Delegate to the inherent method and map String errors into
        // BackendError::Provider. Leg 2 refinements (error taxonomy at
        // source) land in v0.2 Phase 1.
        Backend::chat(self, messages, tools, on_token).map_err(BackendError::Provider)
    }
}
