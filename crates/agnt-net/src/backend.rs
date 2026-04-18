use agnt_core::{BackendError, FunctionCall, LlmBackend, Message, ToolCall};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    api_key: Option<String>,
    /// Model identifier passed in every request.
    pub model: String,
    /// Optional dedicated ureq Agent. When `None`, the process-wide shared
    /// Agent (with default timeouts) is used.
    agent: Option<Arc<ureq::Agent>>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.kind {
            Kind::Openai => "Openai",
            Kind::Anthropic => "Anthropic",
        };
        f.debug_struct("Backend")
            .field("kind", &kind)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("agent", &self.agent.as_ref().map(|_| "<custom>"))
            .finish()
    }
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
            agent: None,
        }
    }
    /// Create a backend for OpenAI's API.
    pub fn openai(model: &str, api_key: &str) -> Self {
        Self {
            kind: Kind::Openai,
            base_url: "https://api.openai.com/v1".into(),
            api_key: Some(api_key.into()),
            model: model.into(),
            agent: None,
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
            agent: None,
        }
    }

    /// Override the HTTP timeouts for this backend instance.
    ///
    /// Builds a fresh ureq Agent with the supplied connect/read timeouts and
    /// attaches it to this [`Backend`]. Subsequent requests made via this
    /// instance will use the custom Agent instead of the process-wide shared
    /// one.
    ///
    /// Returns an error if TLS initialization fails.
    pub fn with_timeouts(mut self, connect: Duration, read: Duration) -> Result<Self, String> {
        let agent = crate::http::build_agent(connect, read)?;
        self.agent = Some(Arc::new(agent));
        Ok(self)
    }

    #[tracing::instrument(
        skip(self, messages, tools, sink),
        fields(
            kind = ?self.kind,
            model = %self.model,
            message_count = messages.len(),
            streaming = sink.is_some(),
        )
    )]
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

    fn build_request(&self, url: &str) -> Result<ureq::Request, String> {
        let agent: &ureq::Agent = match &self.agent {
            Some(a) => a.as_ref(),
            None => crate::http::agent()?,
        };
        let mut req = agent.post(url).set("Content-Type", "application/json");
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
        Ok(req)
    }

    fn chat_openai(
        &self,
        messages: &[Message],
        tools: &Value,
        sink: Option<&mut dyn FnMut(&str)>,
    ) -> Result<Message, String> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut body = json!({ "model": self.model, "messages": to_openai_messages(messages) });
        if let Some(arr) = tools.as_array() {
            if !arr.is_empty() {
                body["tools"] = tools.clone();
            }
        }
        if sink.is_some() {
            body["stream"] = Value::Bool(true);
        }

        // Serialize the body exactly once before entering the retry loop so
        // we don't clone a fresh JSON `Value` on every attempt.
        let body_bytes =
            serde_json::to_vec(&body).map_err(|e| format!("encode body: {}", e))?;
        let body_slice: &[u8] = &body_bytes;

        let resp = with_retry(5, || {
            self.build_request(&url)?
                .send_bytes(body_slice)
                .map_err(RetryError::from)
        })?;

        if let Some(sink) = sink {
            parse_openai_stream(resp.into_reader(), sink)
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

        let body_bytes =
            serde_json::to_vec(&body).map_err(|e| format!("encode body: {}", e))?;
        let body_slice: &[u8] = &body_bytes;

        let resp = with_retry(5, || {
            self.build_request(&url)?
                .send_bytes(body_slice)
                .map_err(RetryError::from)
        })?;

        if let Some(sink) = sink {
            parse_anthropic_stream(resp.into_reader(), sink)
        } else {
            let v: Value = resp.into_json().map_err(|e| format!("decode: {}", e))?;
            from_anthropic_response(&v)
        }
    }
}

/// Internal error type for the retry loop — distinguishes a failure to build
/// the request (e.g. TLS init) from a ureq transport/status error.
enum RetryError {
    Build(String),
    Ureq(ureq::Error),
}

impl From<ureq::Error> for RetryError {
    fn from(e: ureq::Error) -> Self {
        RetryError::Ureq(e)
    }
}

impl From<String> for RetryError {
    fn from(e: String) -> Self {
        RetryError::Build(e)
    }
}

/// Strip sensitive headers from an error body before it bubbles up.
///
/// Upstream providers sometimes echo request headers back in verbose error
/// payloads. We redact the two we know carry secrets so they never end up in
/// logs.
fn redact_secrets(s: &str) -> String {
    // Line-based replacement is good enough for SSE-style payloads; we also
    // handle single-line JSON with inline header strings.
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        let lower = line.to_ascii_lowercase();
        if lower.contains("authorization") || lower.contains("x-api-key") {
            // Drop bearer token / api key values after the header name.
            // A conservative redaction: replace the whole line.
            out.push_str("[redacted header]\n");
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Simple xorshift64* PRNG seeded from the wall clock. We only use this for
/// retry jitter so quality is not critical — we just want each process (and
/// ideally each retry) to pick a different multiplier.
fn xorshift_jitter(state: &mut u64) -> f64 {
    if *state == 0 {
        *state = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15)
            .wrapping_add(1);
    }
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    // Map into [-1.0, 1.0).
    let frac = ((x >> 11) as f64) / ((1u64 << 53) as f64);
    frac * 2.0 - 1.0
}

/// Apply ±20% jitter to a base delay in milliseconds.
fn jittered(base_ms: u64, rng_state: &mut u64) -> u64 {
    let j = xorshift_jitter(rng_state); // [-1, 1)
    let delta = (base_ms as f64) * 0.20 * j;
    let adjusted = (base_ms as f64 + delta).max(0.0);
    adjusted as u64
}

fn with_retry<F>(max: u32, mut f: F) -> Result<ureq::Response, String>
where
    F: FnMut() -> Result<ureq::Response, RetryError>,
{
    if max == 0 {
        return Err("with_retry: max must be >= 1".into());
    }
    let mut base_delay = 500u64;
    let mut rng_state = 0u64;
    let mut last_err: Option<String> = None;
    for i in 0..max {
        match f() {
            Ok(r) => return Ok(r),
            Err(RetryError::Build(e)) => {
                // A build failure (e.g. TLS init) is not worth retrying.
                return Err(e);
            }
            Err(RetryError::Ureq(ureq::Error::Status(code, r))) => {
                let retryable = code == 429 || code >= 500;
                if retryable && i + 1 < max {
                    let sleep_ms = jittered(base_delay, &mut rng_state);
                    thread::sleep(Duration::from_millis(sleep_ms));
                    base_delay = (base_delay * 2).min(8000);
                    continue;
                }
                let body = r.into_string().unwrap_or_default();
                return Err(redact_secrets(&format!("http {}: {}", code, body)));
            }
            Err(RetryError::Ureq(ureq::Error::Transport(t))) => {
                last_err = Some(format!("transport: {}", t));
                if i + 1 < max {
                    let sleep_ms = jittered(base_delay, &mut rng_state);
                    thread::sleep(Duration::from_millis(sleep_ms));
                    base_delay = (base_delay * 2).min(8000);
                    continue;
                }
                return Err(redact_secrets(last_err.as_deref().unwrap_or("transport: unknown")));
            }
        }
    }
    Err(redact_secrets(
        last_err.as_deref().unwrap_or("with_retry: exhausted"),
    ))
}

/// Serialize messages for the OpenAI-compatible wire format.
///
/// Ollama rejects assistant messages whose `content` field is absent or null —
/// it requires the field to be a string even when the response contains only
/// tool_calls. Same constraint applies to `tool` role messages whose result
/// content may be absent.
fn to_openai_messages(msgs: &[Message]) -> Vec<Value> {
    msgs.iter()
        .map(|m| {
            let mut obj = serde_json::to_value(m).unwrap_or(json!({}));
            if (m.role == "assistant" || m.role == "tool")
                && (obj.get("content").is_none() || obj["content"].is_null())
            {
                obj["content"] = json!("");
            }
            obj
        })
        .collect()
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

/// Read one line from the reader into `buf` (cleared first), stripping a
/// trailing `\n` and optional `\r`. Returns `Ok(false)` at EOF.
fn read_sse_line<R: BufRead>(reader: &mut R, buf: &mut String) -> std::io::Result<bool> {
    buf.clear();
    let n = reader.read_line(buf)?;
    if n == 0 {
        return Ok(false);
    }
    if buf.ends_with('\n') {
        buf.pop();
        if buf.ends_with('\r') {
            buf.pop();
        }
    }
    Ok(true)
}

/// Fuzzing / integration-test hook for the OpenAI SSE parser.
///
/// Not part of the stable API — `#[doc(hidden)]` and behind the
/// `fuzz-api` feature so libfuzzer targets can exercise the stream
/// parser without the parser itself becoming `pub`. See
/// `fuzz/fuzz_targets/fuzz_openai_sse.rs`.
#[doc(hidden)]
#[cfg(feature = "fuzz-api")]
pub fn _fuzz_parse_openai_stream(bytes: &[u8]) -> Result<Message, String> {
    let mut sink = |_s: &str| {};
    parse_openai_stream(bytes, &mut sink)
}

/// Fuzzing hook for the Anthropic SSE parser. See
/// [`_fuzz_parse_openai_stream`].
#[doc(hidden)]
#[cfg(feature = "fuzz-api")]
pub fn _fuzz_parse_anthropic_stream(bytes: &[u8]) -> Result<Message, String> {
    let mut sink = |_s: &str| {};
    parse_anthropic_stream(bytes, &mut sink)
}

fn parse_openai_stream<R: Read>(
    resp: R,
    sink: &mut dyn FnMut(&str),
) -> Result<Message, String> {
    // Generic over `R: Read` so tests can feed a `&[u8]` and production can
    // pass `ureq::Response::into_reader()`.
    let mut reader = BufReader::new(resp);
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut line = String::new();

    while read_sse_line(&mut reader, &mut line).map_err(|e| format!("stream: {}", e))? {
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

fn parse_anthropic_stream<R: Read>(
    resp: R,
    sink: &mut dyn FnMut(&str),
) -> Result<Message, String> {
    let mut reader = BufReader::new(resp);
    let mut text = String::new();
    let mut blocks: Vec<(String, String, String, String)> = Vec::new();
    let mut line = String::new();

    while read_sse_line(&mut reader, &mut line).map_err(|e| format!("stream: {}", e))? {
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
                // Borrow the content_block in place instead of cloning it.
                let block = ev.get("content_block");
                let btype = block
                    .and_then(|b| b.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                let id = block
                    .and_then(|b| b.get("id"))
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .and_then(|b| b.get("name"))
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
                // Borrow the delta in place instead of cloning it.
                let delta = ev.get("delta");
                let dtype = delta
                    .and_then(|d| d.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                match dtype {
                    "text_delta" => {
                        if let Some(t) =
                            delta.and_then(|d| d.get("text")).and_then(|t| t.as_str())
                        {
                            text.push_str(t);
                            sink(t);
                        }
                    }
                    "input_json_delta" => {
                        if let Some(p) = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(|p| p.as_str())
                        {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_impl_redacts_api_key() {
        let b = Backend::openai("gpt-4o-mini", "sk-super-secret-key");
        let s = format!("{:?}", b);
        assert!(s.contains("<redacted>"), "debug output: {}", s);
        assert!(
            !s.contains("sk-super-secret-key"),
            "secret leaked in debug output: {}",
            s
        );
    }

    #[test]
    fn redact_secrets_strips_auth_headers() {
        let raw = "line1\nAuthorization: Bearer sk-xyz\nx-api-key: abc\nother\n";
        let out = redact_secrets(raw);
        assert!(!out.contains("sk-xyz"));
        assert!(!out.contains("abc"));
        assert!(out.contains("line1"));
        assert!(out.contains("other"));
    }

    #[test]
    fn with_retry_zero_max_returns_err_not_panic() {
        let r: Result<ureq::Response, String> =
            with_retry(0, || unreachable!("should not be called"));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("max must be >= 1"));
    }

    #[test]
    fn with_retry_build_error_is_not_retried() {
        let mut calls = 0u32;
        let r: Result<ureq::Response, String> = with_retry(5, || {
            calls += 1;
            Err(RetryError::Build("tls init blew up".into()))
        });
        assert!(r.is_err());
        assert_eq!(calls, 1, "build errors must not be retried");
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let mut state = 1u64;
        for _ in 0..1000 {
            let j = jittered(1000, &mut state);
            assert!(j <= 1200, "j={}", j);
            // Lower bound: 1000 - 200 = 800, but floor is 0.
            assert!(j >= 800, "j={}", j);
        }
    }

    #[test]
    fn openai_stream_parses_content_and_tool_call() {
        let data = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"x\\\":1}\"}}]}}]}\n",
            "data: [DONE]\n",
        );
        let mut captured = String::new();
        let msg = {
            let mut sink = |s: &str| captured.push_str(s);
            parse_openai_stream(data.as_bytes(), &mut sink).unwrap()
        };
        assert_eq!(captured, "hello");
        assert_eq!(msg.content.as_deref(), Some("hello"));
        let tcs = msg.tool_calls.expect("tool_calls");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].id, "call_1");
        assert_eq!(tcs[0].function.name, "f");
        assert_eq!(tcs[0].function.arguments, "{\"x\":1}");
    }

    #[test]
    fn to_openai_messages_content_never_null_for_assistant() {
        let msgs = vec![
            Message {
                role: "user".into(),
                content: Some("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            Message {
                role: "assistant".into(),
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "list_dir".into(),
                        arguments: "{}".into(),
                    },
                }]),
                tool_call_id: None,
                name: None,
            },
            Message {
                role: "tool".into(),
                content: None,
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                name: None,
            },
        ];
        let out = to_openai_messages(&msgs);
        // user message: content should be present as-is
        assert_eq!(out[0]["content"], json!("hi"));
        // assistant with tool_calls, no text: content must be "" not null/missing
        assert_eq!(out[1]["content"], json!(""), "assistant content must be empty string, not null");
        // tool message with no content: must be "" not null/missing
        assert_eq!(out[2]["content"], json!(""), "tool content must be empty string, not null");
    }

    #[test]
    fn anthropic_stream_parses_text_and_tool_use() {
        let data = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"lookup\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\\\"x\\\"}\"}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        );
        let mut captured = String::new();
        let msg = {
            let mut sink = |s: &str| captured.push_str(s);
            parse_anthropic_stream(data.as_bytes(), &mut sink).unwrap()
        };
        assert_eq!(captured, "hi");
        assert_eq!(msg.content.as_deref(), Some("hi"));
        let tcs = msg.tool_calls.expect("tool_calls");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].id, "t1");
        assert_eq!(tcs[0].function.name, "lookup");
        assert_eq!(tcs[0].function.arguments, "{\"q\":\"x\"}");
    }
}
