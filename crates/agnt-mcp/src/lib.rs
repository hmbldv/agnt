//! # agnt-mcp
//!
//! Minimal Model Context Protocol client for [agnt](https://crates.io/crates/agnt).
//!
//! v0.3 scope:
//!
//! - **stdio transport only** — spawns an MCP server as a child process and
//!   speaks line-delimited JSON-RPC 2.0 over its stdin/stdout.
//! - **Synchronous, one-shot request/response** — a single in-flight request
//!   at a time, serialized through a [`std::sync::Mutex`] when shared across
//!   threads.
//! - **Tool discovery** via `tools/list`, tool invocation via `tools/call`.
//! - **`agnt_core::Tool` bridge** via [`McpTool`], so discovered MCP tools
//!   slot directly into an `agnt_core::Registry`.
//!
//! Protocol version targeted: `2024-11-05`.
//!
//! ## Example
//!
//! ```no_run
//! use agnt_mcp::{McpClient, McpTool};
//! use std::sync::{Arc, Mutex};
//!
//! let mut client = McpClient::start("my-mcp-server", &["--flag"]).unwrap();
//! let infos = client.list_tools().unwrap();
//! let shared = Arc::new(Mutex::new(client));
//! let tools: Vec<McpTool> = infos
//!     .into_iter()
//!     .map(|info| McpTool::new(Arc::clone(&shared), info))
//!     .collect();
//! ```

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Protocol version this client advertises in its `initialize` handshake.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Per-request timeout. Hard-coded for v0.3.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors surfaced by [`McpClient`] operations.
#[derive(Debug)]
pub enum McpError {
    /// OS / pipe / spawn failure.
    Io(String),
    /// Protocol-level failure — malformed JSON, JSON-RPC error, missing field.
    Protocol(String),
    /// Request timed out waiting for a response.
    Timeout,
    /// Child stdout/stdin closed unexpectedly.
    Closed,
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpError::Io(m) => write!(f, "mcp io: {}", m),
            McpError::Protocol(m) => write!(f, "mcp protocol: {}", m),
            McpError::Timeout => write!(f, "mcp timeout"),
            McpError::Closed => write!(f, "mcp channel closed"),
        }
    }
}

impl std::error::Error for McpError {}

impl From<std::io::Error> for McpError {
    fn from(e: std::io::Error) -> Self {
        McpError::Io(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcNotification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcErrorPayload>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcErrorPayload {
    code: i64,
    message: String,
    #[serde(default)]
    #[allow(dead_code)]
    data: Option<Value>,
}

/// Metadata for a tool exposed by an MCP server.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default = "default_schema")]
    pub input_schema: Value,
}

fn default_schema() -> Value {
    json!({"type": "object"})
}

// ---------------------------------------------------------------------------
// McpClient
// ---------------------------------------------------------------------------

/// Synchronous MCP stdio client.
///
/// Spawns a child process on construction, drives the MCP initialize
/// handshake, and exposes `list_tools` / `call_tool` / `shutdown`.
/// Serialize access via `Mutex<McpClient>` if sharing across threads.
pub struct McpClient {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    rx: Option<Receiver<ReaderMsg>>,
    reader_thread: Option<JoinHandle<()>>,
    next_id: u64,
}

/// Message pushed from the stdout reader thread back to the client.
enum ReaderMsg {
    /// A raw line of JSON from the server.
    Line(String),
    /// Stdout hit EOF.
    Eof,
}

impl McpClient {
    /// Spawn an MCP server and run the `initialize` handshake.
    pub fn start(command: &str, args: &[&str]) -> Result<Self, McpError> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| McpError::Io(format!("spawn {}: {}", command, e)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Io("missing child stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Io("missing child stdout".into()))?;

        let (tx, rx) = mpsc::channel();
        let reader_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = tx.send(ReaderMsg::Eof);
                        return;
                    }
                    Ok(_) => {
                        if tx.send(ReaderMsg::Line(line.clone())).is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(ReaderMsg::Eof);
                        return;
                    }
                }
            }
        });

        let mut this = McpClient {
            child: Some(child),
            stdin: Some(stdin),
            rx: Some(rx),
            reader_thread: Some(reader_thread),
            next_id: 0,
        };

        // Initialize handshake.
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "agnt-mcp", "version": "0.3.0" }
        });
        let _ = this.request("initialize", Some(params))?;

        // Best-effort: send `notifications/initialized`. Non-fatal if it fails.
        let _ = this.notify("notifications/initialized", None);

        Ok(this)
    }

    /// List tools exposed by the server.
    pub fn list_tools(&mut self) -> Result<Vec<McpToolInfo>, McpError> {
        let result = self.request("tools/list", None)?;
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .ok_or_else(|| McpError::Protocol("tools/list: missing tools array".into()))?;
        let mut out = Vec::with_capacity(tools.len());
        for t in tools {
            let info: McpToolInfo = serde_json::from_value(t.clone())
                .map_err(|e| McpError::Protocol(format!("tools/list entry: {}", e)))?;
            out.push(info);
        }
        Ok(out)
    }

    /// Invoke a tool on the server. The returned string is the joined text
    /// content blocks of the response.
    pub fn call_tool(&mut self, name: &str, args: Value) -> Result<String, McpError> {
        let span = tracing::info_span!("mcp.call", name = %name);
        let _enter = span.enter();
        let params = json!({ "name": name, "arguments": args });
        let result = self.request("tools/call", Some(params))?;

        if result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Err(McpError::Protocol(format!(
                "tools/call isError: {}",
                result
            )));
        }

        let content = result
            .get("content")
            .and_then(|v| v.as_array())
            .ok_or_else(|| McpError::Protocol("tools/call: missing content".into()))?;

        let mut buf = String::new();
        for block in content {
            if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(text);
                }
            }
        }
        Ok(buf)
    }

    /// Shut down the server child process. Sends a best-effort shutdown
    /// notification, then waits for the child to exit (killing it if necessary).
    pub fn shutdown(mut self) -> Result<(), McpError> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Result<(), McpError> {
        // Best-effort shutdown notification.
        let _ = self.notify("shutdown", None);

        // Drop stdin so the child's stdin read loop exits.
        drop(self.stdin.take());

        if let Some(mut child) = self.child.take() {
            // Give it a beat to exit cleanly.
            match child.try_wait() {
                Ok(Some(_)) => {}
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        // Join reader thread — it will exit on EOF.
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        self.rx.take();
        Ok(())
    }

    // -------------------------------------------------------------------
    // Internals
    // -------------------------------------------------------------------

    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        let id = self.alloc_id();
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let mut line = serde_json::to_string(&req)
            .map_err(|e| McpError::Protocol(format!("serialize request: {}", e)))?;
        line.push('\n');

        {
            let stdin = self
                .stdin
                .as_mut()
                .ok_or(McpError::Closed)?;
            stdin
                .write_all(line.as_bytes())
                .map_err(|e| McpError::Io(format!("write: {}", e)))?;
            stdin.flush().map_err(|e| McpError::Io(format!("flush: {}", e)))?;
        }

        self.await_response(id)
    }

    fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        let n = JsonRpcNotification {
            jsonrpc: "2.0",
            method,
            params,
        };
        let mut line = serde_json::to_string(&n)
            .map_err(|e| McpError::Protocol(format!("serialize notify: {}", e)))?;
        line.push('\n');
        let stdin = self.stdin.as_mut().ok_or(McpError::Closed)?;
        stdin
            .write_all(line.as_bytes())
            .map_err(|e| McpError::Io(format!("write notify: {}", e)))?;
        stdin
            .flush()
            .map_err(|e| McpError::Io(format!("flush notify: {}", e)))?;
        Ok(())
    }

    fn await_response(&mut self, id: u64) -> Result<Value, McpError> {
        let rx = self.rx.as_ref().ok_or(McpError::Closed)?;
        loop {
            match rx.recv_timeout(REQUEST_TIMEOUT) {
                Ok(ReaderMsg::Line(line)) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let resp: JsonRpcResponse = match serde_json::from_str(trimmed) {
                        Ok(r) => r,
                        Err(e) => {
                            return Err(McpError::Protocol(format!(
                                "parse response: {} (line: {})",
                                e, trimmed
                            )));
                        }
                    };
                    // Skip notifications (no id) or id mismatches (late replies).
                    let resp_id = match &resp.id {
                        Some(Value::Number(n)) => n.as_u64(),
                        _ => None,
                    };
                    if resp_id != Some(id) {
                        continue;
                    }
                    if let Some(err) = resp.error {
                        return Err(McpError::Protocol(format!(
                            "jsonrpc error {}: {}",
                            err.code, err.message
                        )));
                    }
                    return Ok(resp.result.unwrap_or(Value::Null));
                }
                Ok(ReaderMsg::Eof) => return Err(McpError::Closed),
                Err(RecvTimeoutError::Timeout) => return Err(McpError::Timeout),
                Err(RecvTimeoutError::Disconnected) => return Err(McpError::Closed),
            }
        }
    }
}

impl fmt::Debug for McpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpClient")
            .field("next_id", &self.next_id)
            .field("alive", &self.child.is_some())
            .finish()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

// ---------------------------------------------------------------------------
// McpTool — bridge to agnt_core::Tool
// ---------------------------------------------------------------------------

/// An [`agnt_core::Tool`] implementation that routes calls to a specific tool
/// on a shared [`McpClient`].
///
/// Multiple `McpTool` instances share a single client via `Arc<Mutex<_>>`.
pub struct McpTool {
    client: Arc<Mutex<McpClient>>,
    name: String,
    description: String,
    schema: Value,
}

impl McpTool {
    pub fn new(client: Arc<Mutex<McpClient>>, info: McpToolInfo) -> Self {
        Self {
            client,
            name: info.name,
            description: info.description,
            schema: info.input_schema,
        }
    }
}

impl agnt_core::tool::Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let span = tracing::info_span!("mcp.call", name = %self.name);
        let _enter = span.enter();
        let mut guard = self
            .client
            .lock()
            .map_err(|e| format!("mcp client mutex poisoned: {}", e))?;
        guard.call_tool(&self.name, args).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny shell-based mock MCP server that reads JSON-RPC requests
    /// line by line and replies with canned responses. Uses only standard Unix
    /// utilities so the test runs anywhere with `/bin/sh`.
    ///
    /// The mock advances one line of stdin per response to keep `id` ordering
    /// aligned with the real client.
    fn mock_server_script(responses: &[&str]) -> String {
        // Flow: the real client sends `initialize`, then a
        // `notifications/initialized` notification, then one request per
        // subsequent response. The mock reads one line per request and the
        // extra notification line immediately after the initialize reply so
        // response ids stay in lockstep. Without that extra read the test is
        // flaky — the notification race-writes past the script and closes
        // the pipe before the next request lands.
        let mut s = String::new();
        for (i, r) in responses.iter().enumerate() {
            let escaped = r.replace('\'', "'\\''");
            s.push_str(&format!("read line; printf '%s\\n' '{}'\n", escaped));
            if i == 0 {
                s.push_str("read line\n");
            }
        }
        // Keep the mock alive long enough for the test to finish draining
        // the last response before the pipe closes.
        s.push_str("sleep 0.2\n");
        s
    }

    fn start_mock(responses: &[&str]) -> McpClient {
        let script = mock_server_script(responses);
        McpClient::start("/bin/sh", &["-c", &script]).expect("start mock")
    }

    #[test]
    fn initialize_handshake_completes() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#;
        let client = start_mock(&[init]);
        // If start() returned Ok, the handshake succeeded.
        drop(client);
    }

    #[test]
    fn list_tools_parses_server_response() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#;
        let list = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}"#;
        let mut client = start_mock(&[init, list]);
        let tools = client.list_tools().expect("list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "Echo text");
        assert_eq!(
            tools[0].input_schema,
            serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}})
        );
    }

    #[test]
    fn call_tool_joins_text_content_blocks() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#;
        let call = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"hello"},{"type":"text","text":"world"}],"isError":false}}"#;
        let mut client = start_mock(&[init, call]);
        let out = client
            .call_tool("echo", serde_json::json!({"text":"hi"}))
            .expect("call");
        assert_eq!(out, "hello\nworld");
    }

    #[test]
    fn call_tool_is_error_surfaces_protocol_error() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#;
        let call = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"bad"}],"isError":true}}"#;
        let mut client = start_mock(&[init, call]);
        let err = client
            .call_tool("echo", serde_json::json!({}))
            .expect_err("should error");
        assert!(matches!(err, McpError::Protocol(_)), "got {:?}", err);
    }

    #[test]
    fn jsonrpc_error_response_maps_to_protocol_error() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#;
        let err_resp =
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"method not found"}}"#;
        let mut client = start_mock(&[init, err_resp]);
        let err = client.list_tools().expect_err("should error");
        match err {
            McpError::Protocol(m) => assert!(m.contains("method not found"), "got: {}", m),
            other => panic!("expected Protocol, got {:?}", other),
        }
    }

    #[test]
    fn closed_pipe_yields_closed_error() {
        // `true` exits immediately with no stdout — the reader hits EOF before
        // the initialize reply can arrive.
        let err = McpClient::start("/bin/sh", &["-c", "exit 0"]).expect_err("should fail");
        assert!(
            matches!(err, McpError::Closed | McpError::Io(_) | McpError::Protocol(_)),
            "got {:?}",
            err
        );
    }

    #[test]
    fn spawn_nonexistent_binary_is_io_error() {
        let err = McpClient::start("/definitely/not/a/real/binary-xyz", &[])
            .expect_err("should fail");
        assert!(matches!(err, McpError::Io(_)), "got {:?}", err);
    }

    #[test]
    fn mcp_tool_bridges_to_agnt_core_tool_trait() {
        use agnt_core::tool::Tool;
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#;
        let call = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"routed"}]}}"#;
        let client = start_mock(&[init, call]);
        let shared = Arc::new(Mutex::new(client));
        let info = McpToolInfo {
            name: "echo".into(),
            description: "Echo text".into(),
            input_schema: serde_json::json!({"type":"object"}),
        };
        let tool = McpTool::new(Arc::clone(&shared), info);
        assert_eq!(tool.name(), "echo");
        assert_eq!(tool.description(), "Echo text");
        assert_eq!(tool.schema(), serde_json::json!({"type":"object"}));
        let out = tool.call(serde_json::json!({})).expect("call");
        assert_eq!(out, "routed");
    }

    #[test]
    fn mcp_tool_info_deserializes_with_missing_description() {
        let info: McpToolInfo = serde_json::from_value(serde_json::json!({
            "name": "bare",
            "inputSchema": {"type":"object"}
        }))
        .expect("deserialize");
        assert_eq!(info.name, "bare");
        assert_eq!(info.description, "");
    }

    #[test]
    fn mcp_tool_info_deserializes_with_missing_schema() {
        let info: McpToolInfo = serde_json::from_value(serde_json::json!({
            "name": "bare"
        }))
        .expect("deserialize");
        assert_eq!(info.input_schema, serde_json::json!({"type":"object"}));
    }

    #[test]
    fn mcp_error_display_is_stable() {
        assert_eq!(McpError::Timeout.to_string(), "mcp timeout");
        assert_eq!(McpError::Closed.to_string(), "mcp channel closed");
        assert!(McpError::Io("x".into()).to_string().contains("io"));
        assert!(McpError::Protocol("x".into()).to_string().contains("protocol"));
    }
}
