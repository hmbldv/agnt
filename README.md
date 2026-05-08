# agnt

**Production Rust agent runtime — zero-I/O kernel, streaming backends, sandboxed tools.**

[![crates.io](https://img.shields.io/crates/v/agnt.svg)](https://crates.io/crates/agnt)
[![docs.rs](https://img.shields.io/docsrs/agnt)](https://docs.rs/agnt)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`agnt` is a modular, production-grade agent runtime for Rust. It ships a zero-I/O kernel that composes cleanly with any HTTP backend, an observer model that enforces policy at every tool invocation, and built-in tools that are sandbox-first by design — not by configuration. 9/9 on the end-to-end eval suite, including multi-step arithmetic pipelines, multi-file projects, iterative refinement loops, and a 27-turn coherence probe.

---

## Features

- **`FilesystemRoot` sandbox** — path traversal is rejected at the type level; there is no runtime policy check to bypass
- **`should_dispatch` observer gate** — fires before every tool call; the canonical hook for trust-tier enforcement, HITL approval, and content policy
- **Loop detection** — per-step `(tool_name, args_json)` fingerprints; 3+ repeats injects a synthetic refusal before the LLM can spiral
- **`UsageStats` on every message** — `prompt_tokens` + `completion_tokens`, populated from backend responses; every step is auditable
- **`on_step_usage` observer hook** — fires at every step exit (success, error, deadline, max-steps) with cumulative token count
- **SSRF guard** — atomic resolver baked into the `Fetch` tool; no TOCTOU window
- **Streaming + retry** — SSE streaming with back-off across OpenAI, Anthropic, Ollama, and any OpenAI-compatible endpoint
- **Sliding context window** — `max_window = 40` messages; `max_steps = 25` per run
- **`#[tool]` macro** — attribute macro to implement `Tool` without boilerplate
- **MCP stdio client** — `agnt-mcp` for tool delegation to external servers

---

## Crate Layout

| Crate | Role | Notes |
|-------|------|-------|
| `agnt-core` | Zero-I/O kernel | Agent loop, message types, `Tool` trait, observer hooks, `Store` trait |
| `agnt-net` | HTTP backends | OpenAI / Anthropic / Ollama / any OpenAI-compat; streaming + retry |
| `agnt-store` | Message persistence | SQLite, WAL mode, µs-precision tool log |
| `agnt-tools` | Built-in tools | ReadFile, WriteFile, EditFile, ListDir, Glob, Grep, Fetch, Shell (+bwrap) |
| `agnt-macros` | Proc-macro | `#[tool]` attribute macro |
| `agnt-mcp` | MCP client | stdio transport; delegates tool calls to external MCP servers |
| `agnt` | Meta-crate | Re-exports everything; this is the entry point |

~7,000 lines across seven crates.

---

## Quick Start

```toml
[dependencies]
agnt = "0.3"
```

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

---

## Security Architecture

### `FilesystemRoot` — Type-Level Path Containment

All filesystem tools (`ReadFile`, `WriteFile`, `EditFile`, `ListDir`, `Glob`, `Grep`) accept a `FilesystemRoot` rather than a raw path. The type normalises and validates the requested path against the root at construction time. There is no separate runtime guard that a misconfigured caller could skip — if the path escapes the root, the type cannot be constructed.

```rust
let root = FilesystemRoot::new("/workspace")?;
let tool = ReadFile::with_root(root);
```

Attempts to traverse above the root (`../../etc/passwd`, symlink chains, etc.) return an error at the type boundary, not inside tool execution.

### `should_dispatch` — Pre-Call Observer Gate

Every tool call passes through the `should_dispatch(tool_name, args)` observer hook before execution. Return `false` to halt the call and inject a synthetic refusal into the agent's context. This is the canonical integration point for:

- **Trust-tier enforcement** — gate calls based on the agent's assigned trust level
- **HITL approval** — block and await human confirmation before sensitive operations
- **Content policy** — inspect arguments before they reach I/O

```rust
agent.on_should_dispatch(|name, args| {
    if name == "Shell" {
        return ApprovalGate::require_human(args);
    }
    true
});
```

### Loop Detection

At each step, `agnt-core` computes a `(tool_name, args_json)` fingerprint and tracks repetitions in a per-step map. When any fingerprint appears three or more times within a run, the runtime injects a synthetic refusal message into the conversation and terminates the loop. The LLM never gets an additional turn — the guard is in the kernel, not in the prompt.

### Token Tracking and Step Auditing

`UsageStats { prompt_tokens, completion_tokens }` is attached to every `Message`, populated directly from backend responses. The `on_step_usage(UsageStats)` observer hook fires at every step exit — whether the step succeeds, hits an error, reaches the deadline, or is stopped by `max_steps`. The cumulative token count is always available, enabling per-run billing attribution, rate-limit enforcement, and audit logging without instrumenting the backend.

### SSRF Guard

The built-in `Fetch` tool uses an atomic resolver: DNS resolution and the connection decision are made in a single operation with no window between them. Requests to RFC 1918 addresses, loopback, link-local, and metadata endpoints (e.g. `169.254.169.254`) are rejected before the socket opens.

### Bounded Execution

- `max_steps = 25` — hard ceiling on LLM turns per `run()` call
- `max_window = 40` — sliding message window; older messages are evicted to keep context bounded
- Both limits are enforced in `agnt-core` independent of the backend

---

## Observer Hooks

Observers are the extension surface for policy, monitoring, and control. All hooks are registered on the `AgentBuilder`:

| Hook | When it fires | Typical use |
|------|---------------|-------------|
| `on_step_start()` | Before each LLM call | Logging, rate limiting |
| `on_step_end(message)` | After each LLM response | Response logging, content filtering |
| `should_dispatch(name, args)` | Before every tool call | Trust enforcement, HITL, policy |
| `on_tool_result(name, result)` | After every tool returns | Audit logging, result scrubbing |
| `on_step_usage(UsageStats)` | At every step exit | Token accounting, billing attribution |
| `on_loop_detected(name, args)` | When a fingerprint repeats ≥ 3× | Alerting, telemetry |

---

## Stack

![Rust](https://img.shields.io/badge/Rust-1.80%2B-orange?logo=rust)
![SQLite](https://img.shields.io/badge/SQLite-WAL-blue?logo=sqlite)
![OpenAI](https://img.shields.io/badge/OpenAI-compatible-412991?logo=openai)
![Anthropic](https://img.shields.io/badge/Anthropic-Claude-c45700)
![Ollama](https://img.shields.io/badge/Ollama-local-444)
![MCP](https://img.shields.io/badge/MCP-stdio-lightgrey)
