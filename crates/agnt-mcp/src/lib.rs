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

/// Maximum bytes accepted on a single JSON-RPC line from an MCP server.
///
/// The v0.3 release shipped with an unbounded `BufReader::read_line` which
/// meant a hostile or buggy server could stream a multi-gigabyte line and
/// OOM the agent process. v0.3.1 caps the reader at 4 MiB: enough headroom
/// for any well-behaved tool response (the MCP examples peak in the tens
/// of KiB) while keeping the blast radius of a broken peer finite. On
/// overflow the reader emits an `McpError::Protocol("line too long")`
/// via the message channel and closes — the client cannot recover the
/// stream because stdio framing is no longer reliable.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

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
// Bounded line reader (v0.3.1 DoS fix)
// ---------------------------------------------------------------------------

/// Outcome of a single bounded-line read. Separated from the error
/// variant so the reader thread can distinguish a clean EOF from a
/// completed line without unwrapping a union.
enum BoundedRead {
    /// The reader returned 0 bytes before hitting a newline. EOF.
    Eof,
    /// One full line (without the trailing `\n`) is now in the buffer.
    Line,
}

/// Failure modes for [`read_bounded_line`].
#[derive(Debug)]
enum BoundedReadError {
    /// Peer exceeded [`MAX_LINE_BYTES`] without a newline. Treated as a
    /// hard close because stdio framing is no longer reliable.
    Overflow,
    /// Underlying `std::io` read error. Caller converts to Eof for the
    /// mpsc channel.
    #[allow(dead_code)]
    Io(std::io::Error),
}

/// Read from `reader` into `buf` up to (and including) the next `\n`,
/// refusing to grow past `limit` bytes. On success the trailing newline
/// is stripped, so callers can `str::from_utf8(&buf)` directly.
///
/// This is intentionally a one-byte-at-a-time loop rather than
/// `read_until` because `std::io::BufRead::read_until` has no way to
/// cap its growth — it will happily allocate a gigabyte if that's what
/// the peer sends. Byte-level reads are fine here: the child process
/// stdout is already line-buffered by convention (one JSON-RPC frame
/// per line) and the inner `BufReader` amortises the syscalls.
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    limit: usize,
) -> Result<BoundedRead, BoundedReadError> {
    loop {
        let available = match reader.fill_buf() {
            Ok(b) => {
                if b.is_empty() {
                    // Peer closed. Whether we had a partial line or
                    // not, the stream is unrecoverable — frame the
                    // result as EOF so the caller reports Closed.
                    return Ok(BoundedRead::Eof);
                }
                b
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(BoundedReadError::Io(e)),
        };
        let (chunk, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (&available[..=i], true),
            None => (available, false),
        };
        if buf.len() + chunk.len() > limit.saturating_add(1) {
            // +1 for the trailing newline which we strip before the cap
            // check would otherwise trigger on a line exactly at `limit`.
            let take = limit.saturating_add(1).saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..take]);
            let consumed = take;
            reader.consume(consumed);
            return Err(BoundedReadError::Overflow);
        }
        buf.extend_from_slice(chunk);
        let consumed = chunk.len();
        reader.consume(consumed);
        if done {
            // Strip the trailing '\n' and any preceding '\r' (CRLF hosts).
            if buf.last() == Some(&b'\n') {
                buf.pop();
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(BoundedRead::Line);
        }
    }
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
    /// A framing-level failure the client cannot recover from — e.g. a
    /// line longer than [`MAX_LINE_BYTES`] or a non-UTF-8 byte sequence
    /// on a supposedly JSON-encoded stream. Treated as a hard close by
    /// the main loop.
    Error(String),
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
            // Bounded, byte-level read loop. `BufReader::read_until` lets
            // us enforce [`MAX_LINE_BYTES`] before we ever allocate a full
            // line into memory — a hostile MCP server streaming 1 GB of
            // garbage terminates on overflow instead of OOMing the agent.
            let mut buf: Vec<u8> = Vec::with_capacity(4096);
            loop {
                buf.clear();
                match read_bounded_line(&mut reader, &mut buf, MAX_LINE_BYTES) {
                    Ok(BoundedRead::Eof) => {
                        let _ = tx.send(ReaderMsg::Eof);
                        return;
                    }
                    Ok(BoundedRead::Line) => match std::str::from_utf8(&buf) {
                        Ok(s) => {
                            if tx.send(ReaderMsg::Line(s.to_string())).is_err() {
                                return;
                            }
                        }
                        Err(_) => {
                            let _ =
                                tx.send(ReaderMsg::Error("non-utf8 bytes on mcp stdout".into()));
                            return;
                        }
                    },
                    Err(BoundedReadError::Overflow) => {
                        let _ = tx.send(ReaderMsg::Error(format!(
                            "mcp line exceeded {} bytes",
                            MAX_LINE_BYTES
                        )));
                        return;
                    }
                    Err(BoundedReadError::Io(_)) => {
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
            "clientInfo": { "name": "agnt-mcp", "version": "0.3.1" }
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
            let stdin = self.stdin.as_mut().ok_or(McpError::Closed)?;
            stdin
                .write_all(line.as_bytes())
                .map_err(|e| McpError::Io(format!("write: {}", e)))?;
            stdin
                .flush()
                .map_err(|e| McpError::Io(format!("flush: {}", e)))?;
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
                Ok(ReaderMsg::Error(msg)) => return Err(McpError::Protocol(msg)),
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
            matches!(
                err,
                McpError::Closed | McpError::Io(_) | McpError::Protocol(_)
            ),
            "got {:?}",
            err
        );
    }

    #[test]
    fn spawn_nonexistent_binary_is_io_error() {
        let err =
            McpClient::start("/definitely/not/a/real/binary-xyz", &[]).expect_err("should fail");
        assert!(matches!(err, McpError::Io(_)), "got {:?}", err);
    }

    #[test]
    fn mcp_tool_bridges_to_agnt_core_tool_trait() {
        use agnt_core::tool::Tool;
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#;
        let call =
            r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"routed"}]}}"#;
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
        assert!(McpError::Protocol("x".into())
            .to_string()
            .contains("protocol"));
    }

    // ---- v0.3.1 bounded reader DoS fix -----------------------------------

    #[test]
    fn bounded_reader_accepts_short_line() {
        let input: &[u8] = b"hello\n";
        let mut r = std::io::BufReader::new(input);
        let mut buf = Vec::new();
        let outcome = read_bounded_line(&mut r, &mut buf, 1024)
            .unwrap_or_else(|_| panic!("should accept short line"));
        assert!(matches!(outcome, BoundedRead::Line));
        assert_eq!(buf, b"hello");
    }

    #[test]
    fn bounded_reader_strips_crlf() {
        let input: &[u8] = b"crlf\r\n";
        let mut r = std::io::BufReader::new(input);
        let mut buf = Vec::new();
        read_bounded_line(&mut r, &mut buf, 1024).expect("ok");
        assert_eq!(buf, b"crlf");
    }

    #[test]
    fn bounded_reader_reports_eof_on_empty() {
        let input: &[u8] = b"";
        let mut r = std::io::BufReader::new(input);
        let mut buf = Vec::new();
        match read_bounded_line(&mut r, &mut buf, 1024).expect("ok") {
            BoundedRead::Eof => {}
            BoundedRead::Line => panic!("expected EOF"),
        }
    }

    #[test]
    fn bounded_reader_rejects_oversized_line() {
        // 32 KB of 'x' with no newline should hit the limit.
        let big: Vec<u8> = vec![b'x'; 32 * 1024];
        let mut r = std::io::BufReader::new(&big[..]);
        let mut buf = Vec::new();
        let err = read_bounded_line(&mut r, &mut buf, 8 * 1024);
        assert!(matches!(err, Err(BoundedReadError::Overflow)));
    }

    #[test]
    fn bounded_reader_rejects_line_just_over_limit() {
        // N bytes + '\n' where N == limit + 1 should overflow.
        let mut big: Vec<u8> = vec![b'a'; 1025];
        big.push(b'\n');
        let mut r = std::io::BufReader::new(&big[..]);
        let mut buf = Vec::new();
        let err = read_bounded_line(&mut r, &mut buf, 1024);
        assert!(matches!(err, Err(BoundedReadError::Overflow)));
    }

    #[test]
    fn bounded_reader_handles_multi_line_stream() {
        let input: &[u8] = b"one\ntwo\nthree\n";
        let mut r = std::io::BufReader::new(input);
        let mut buf = Vec::new();
        read_bounded_line(&mut r, &mut buf, 1024).expect("one");
        assert_eq!(buf, b"one");
        buf.clear();
        read_bounded_line(&mut r, &mut buf, 1024).expect("two");
        assert_eq!(buf, b"two");
        buf.clear();
        read_bounded_line(&mut r, &mut buf, 1024).expect("three");
        assert_eq!(buf, b"three");
        buf.clear();
        match read_bounded_line(&mut r, &mut buf, 1024).expect("eof") {
            BoundedRead::Eof => {}
            BoundedRead::Line => panic!("expected EOF after exhausting input"),
        }
    }
}
