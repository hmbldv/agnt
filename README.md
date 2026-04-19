# agnt

**A dense, sync-first Rust agent runtime — module-level auditable, structurally sandboxed against adversarial LLM output, and composable across async and sync callers without forcing a runtime choice on either.**

[![Crates.io](https://img.shields.io/crates/v/agnt.svg)](https://crates.io/crates/agnt)
[![Documentation](https://docs.rs/agnt/badge.svg)](https://docs.rs/agnt)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

```toml
[dependencies]
agnt = "0.3"
```

## Repository layout (v0.3.2 — nine-crate workspace)

Around 7,500 LOC across nine crates: seven published library crates, an
edge daemon binary (`agnt-dmn`), and an async task engine (`agnt-engine`).
Each security-critical path lives in a single small file (agent loop ~950
LOC, tools ~1,250 LOC, MCP framing ~750 LOC, SSRF resolver ~250 LOC) so
reviewers can read one layer at a time.

| Path | Crate | Purpose |
|---|---|---|
| `crates/agnt/` | `agnt` | Flagship meta-crate — what you `cargo add` |
| `crates/agnt-core/` | `agnt-core` | Traits + message types + Agent loop. Zero I/O deps. WASM-ready. |
| `crates/agnt-net/` | `agnt-net` | HTTP backend (Ollama / OpenAI / Anthropic / vLLM) with streaming + retry |
| `crates/agnt-store/` | `agnt-store` | SQLite message store (bundled, WAL mode, prepared-statement cache) |
| `crates/agnt-tools/` | `agnt-tools` | Built-in tools with filesystem sandbox, SSRF guard, opt-in shell, opt-in system-tools |
| `crates/agnt-macros/` | `agnt-macros` | `#[tool]` attribute macro — turn a `fn` into a `TypedTool` with zero boilerplate |
| `crates/agnt-mcp/` | `agnt-mcp` | MCP stdio client — bridges remote MCP tools into `agnt_core::Tool` |
| `crates/agnt-dmn/` | `agnt-dmn` | Edge daemon — HTTP server wrapping agnt-core with persistent sessions + SSE streaming |
| `crates/agnt-engine/` | `agnt-engine` | Async task engine — retry, budget, execution modes, cron scheduling |
| `fuzz/` | — | libfuzzer targets for sandbox, SSRF, glob, dispatch, SSE parsers |
| `src/` | `agnt-rs` | REPL binary example consumer |

All seven library crates publish independently; `cargo add agnt` pulls the
default stack via `default = ["net", "store", "tools", "macros"]`. Opt in
to `mcp` and `tools-bwrap-shell` as needed. `agnt-dmn` and `agnt-engine`
are binaries/internal crates used for deployment.

## Quick start

```rust
use agnt::{AgentBuilder, Backend};
use agnt::builtins::{ReadFile, Grep};

fn main() -> Result<(), String> {
    let backend = Backend::ollama("gemma4:e4b");

    let mut agent = AgentBuilder::new(backend)
        .system("You are a helpful assistant.")
        .tool(Box::new(ReadFile::new()))
        .tool(Box::new(Grep::new()))
        .build()?;

    println!("{}", agent.step("Find TODOs in src/")?);
    Ok(())
}
```

## Running the REPL binary

```bash
ollama pull gemma4:e4b
cargo run --release

> find all .rs files under src/ and tell me which is largest
> /stats
```

The REPL is a thin wrapper over `agnt` with CLI flags for session
management, tool allowlisting, and a `/stats` command that prints µs-level
tool latency breakdowns from the SQLite log.

## Agent loop internals (v0.3.2)

`Agent::step` is decomposed into three private helpers to keep the main
loop readable:

| Method | Responsibility |
|---|---|
| `build_send_window()` | Slice the message history to `max_window`, aligning cuts to user-message boundaries so tool_use/tool_result pairs are never split |
| `dispatch_calls(calls, quota_usage)` | Sequential pre-dispatch decision pass (observer veto + quota check), then parallel `thread::scope` dispatch; returns one `ToolOutcome` per call |
| `frame_results(results, quota_usage, legacy_stream)` | Accumulate post-dispatch durations into quota counters, apply per-tool byte caps, wrap each result in `<tool_output>` envelope, append to message history |

`Agent::max_tool_result_bytes` (default 64 KB) caps raw tool output before
envelope framing. Truncation is UTF-8-boundary-safe and annotates the
envelope with `truncated="true"` and `raw_bytes="N"` so the model can
detect and react to truncation.

## Tool output sandboxing

`agnt-tools` exports `SandboxedPath` alongside `FilesystemRoot`:

```rust
use agnt::{FilesystemRoot, builtins::ReadFile};
use std::sync::Arc;

let root = Arc::new(FilesystemRoot::new("/home/user/workspace")?);
let read_file = ReadFile::with_sandbox(root.clone());
```

`SandboxedPath` is the resolved, validated path type returned by
`FilesystemRoot::resolve`. You can hold one as a verified capability
and pass it to lower-level code without repeating the sandbox check.

### System-info tools (opt-in)

The `system-tools` feature enables a set of read-only system inspection
tools whose commands are hardcoded (no injection surface):

```toml
agnt-tools = { version = "0.3", features = ["system-tools"] }
```

Adds: `SystemInfo`, `DiskUsage`, `NvidiaSmi`, `DockerPs`.

## Backend configuration

### Custom base URL (vLLM / LiteLLM / local proxy)

```rust
let backend = Backend::openai("gemma-4b", "")
    .with_base_url("http://localhost:8000/v1");
```

`with_base_url` overrides the default provider URL. Use it to point the
OpenAI-compatible backend at a local vLLM server, LiteLLM proxy, or any
other OpenAI-API-compatible endpoint.

### `tool_result_as_user` — Gemma 4 on vLLM

Gemma 4's chat template embeds tool results inside the model turn and closes
it, which leaves no generation prompt for the follow-up turn. Sending tool
results as `role: "user"` instead of `role: "tool"` produces the correct
`<|turn>user … <|turn>model` sequence:

```rust
let mut backend = Backend::openai("gemma4-e4b", "")
    .with_base_url("http://localhost:8000/v1");
backend.tool_result_as_user = true;
```

In `agnt-dmn` this is a config field:

```toml
# ~/.config/dmn/dmn.toml
provider = "openai"
model = "gemma4-E4B"
base_url = "http://localhost:8000/v1"
tool_result_as_user = true
```

## vLLM deployment (Gemma 4)

Recommended setup for running `agnt` or `agnt-dmn` against a local vLLM
instance:

```bash
# 1. Start vLLM with Gemma 4 and tool-call support
vllm serve google/gemma-4-e4b-it \
  --enable-auto-tool-choice \
  --tool-call-parser gemma4 \
  --port 8000

# 2. Configure dmn (or set fields directly in your Rust code)
cat > ~/.config/dmn/dmn.toml <<'EOF'
provider     = "openai"
model        = "gemma4-E4B"
base_url     = "http://localhost:8000/v1"
tool_result_as_user = true
EOF

# 3. Start the daemon
cargo run -p agnt-dmn --release
```

The `--tool-call-parser gemma4` flag tells vLLM to parse Gemma 4's
function-calling format. `--enable-auto-tool-choice` allows the model to
choose tools dynamically. The `tool_result_as_user = true` config ensures
the follow-up generation prompt is correctly positioned after each tool
result.

## `agnt-dmn` — edge daemon

`agnt-dmn` wraps the agent runtime in an HTTP server designed to run one
instance per machine in a Tailscale-connected mesh.

```
GET  /health           — liveness + session count + uptime
POST /step             — agentic turn (blocking, returns full result)
POST /step/stream      — agentic turn via SSE (tokens + tool events)
POST /tool             — direct tool dispatch, no inference
GET  /sessions         — list active sessions, sorted by last_activity
GET  /tools            — list registered tools
POST /engine           — run an agnt-engine Task (requires `engine` feature)
```

### Persistent sessions

Sessions are tracked by `session_id` in a `Mutex<HashMap>` on `DmnState`.
Each `POST /step` request carries a `session_id`; if absent, a UUID is
generated. The SQLite message store is keyed by `session_id`, so
conversation history survives daemon restarts.

### SSE streaming (`POST /step/stream`)

`/step/stream` returns an SSE stream with five event types:

| Event | Payload |
|---|---|
| `session_start` | `{ session_id, model }` |
| `token` | `{ content }` — streamed inference tokens |
| `tool_call` | `{ name, args }` — fired before dispatch |
| `tool_result` | `{ name, result, duration_ms }` — fired after dispatch |
| `complete` | `{ session_id, response }` — final assistant message |
| `error` | `{ message }` — step-level error |

The stream closes when the blocking task exits (agent drop closes both the
observer and token senders). A 15-second SSE keep-alive ping prevents proxy
timeouts on long-running inference turns.

### `agnt-engine` feature

Enable with `--features engine` (or `features = ["engine"]` in `Cargo.toml`).
The `/engine` endpoint accepts an `agnt_engine::Task` and executes it with
full retry, budget, and execution-mode support.

## `agnt-engine` — async task runner

`agnt-engine` wraps `agnt-core`'s sync Agent loop in an async execution
layer with pluggable retry and budget policies.

### Execution modes

| Mode | Behaviour |
|---|---|
| `OneShot` | Run once; succeed or fail |
| `UntilSuccess { max_attempts }` | Retry up to N times until the step returns a non-error result |
| `Loop { interval_secs, max_iterations }` | Run on an interval with optional iteration cap and shutdown signal |
| `Pipeline { steps }` | Execute a sequence of `TaskPayload`s; configurable on-failure action (abort / skip / retry) |

### Terminal reasons

`BudgetExhausted`, `TtlExpired`, `MaxRetries`, `Aborted`, `DiminishingReturns`,
`MaxIterations`, `PipelineCompleted`, `Completed`, `ModelError`, `PolicyBlocked`.

### Tool policy

`EngineObserver` enforces a permit/deny list via `Observer::should_dispatch`:

```rust
let config = EngineConfig {
    permitted_tools: vec!["read_file".into(), "grep".into()],
    denied_tools:    vec!["shell".into()],
    // ...
};
```

Denied tools take precedence. Empty `permitted_tools` means all tools are
allowed (subject to the deny list).

## Observer hooks

Every step has six lifecycle hooks:

```rust
use agnt::{Observer, Message, ToolCall, ToolResult, Disposition};

struct AuditLog;
impl Observer for AuditLog {
    fn on_step_start(&self, ctx: &StepContext) { /* ... */ }
    fn on_tool_start(&self, call: &ToolCall) { /* ... */ }
    fn on_tool_end(&self, call: &ToolCall, result: &ToolResult) { /* ... */ }
    fn on_step_end(&self, response: &Message) { /* ... */ }
    fn on_step_error(&self, error: &str) { /* ... */ }

    // Policy gate — return Refused to block a tool call before dispatch.
    fn should_dispatch(&self, call: &ToolCall) -> Disposition {
        Disposition::Allow
    }
}
```

Attach via `AgentBuilder::observer(Arc::new(AuditLog))`. All methods have
no-op defaults so you only implement what you need.

## Documentation

- **[Library README](crates/agnt/README.md)** — Full feature matrix, typed
  tools, observer hooks, benchmarks, and comparison against rig-core,
  llm, langchain-rust.
- **[Threat model](THREAT_MODEL.md)** — Current security posture: what
  the sandbox, SSRF resolver, and opt-in Shell defend against, what's
  partially mitigated, and what's out of scope.
- **[Changelog](CHANGELOG.md)** — v0.1 → v0.2 → v0.3 → v0.3.1 → v0.3.2
  notes, every breaking change, and the full list of security and
  performance fixes.

## License

Dual-licensed under either:

- MIT License ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
