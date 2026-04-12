# agnt

**The smallest Rust agent runtime that's auditable as a single binary, structurally sandboxed against adversarial LLM output, and composable across async and sync callers without forcing a runtime choice on either.**

```toml
[dependencies]
agnt = "0.2"
```

[![Crates.io](https://img.shields.io/crates/v/agnt.svg)](https://crates.io/crates/agnt)
[![Documentation](https://docs.rs/agnt/badge.svg)](https://docs.rs/agnt)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

## Why agnt

Most Rust LLM agent libraries are thick wrappers around Python concepts: tokio everywhere, trait-object soup, opinionated orchestration frameworks. `agnt` is the opposite — a small, sync-first library that gives you a working agent loop, typed tool trait, and persistence layer, and gets out of your way.

| | `agnt` v0.2 | rig-core | graniet/llm | langchain-rust |
|---|---|---|---|---|
| **LOC (library path)** | ~2,200 | ~49,000 | ~20,000 | ~30,000 |
| **Async runtime required** | ❌ | ✅ tokio | ✅ tokio | ✅ tokio |
| **WASM-capable kernel** | ✅ `agnt-core` | ⚠ partial | ❌ | ❌ |
| **Multi-backend** | ✅ Ollama, OpenAI, Anthropic | ✅ 20+ | ✅ 12+ | ✅ |
| **Parallel tool dispatch** | ✅ `thread::scope` | ✅ tokio | ❌ | ❌ |
| **Typed tool trait** | ✅ `TypedTool` (macro-free) | ✅ + macro | ✅ | ⚠ |
| **SQLite persistence** | ✅ bundled, WAL mode | ❌ | ⚠ partial | ⚠ vector-only |
| **µs tool profiling** | ✅ built-in | ❌ | ❌ | ❌ |
| **Structural sandbox** | ✅ filesystem root + SSRF guard | ❌ | ❌ | ❌ |
| **Tool output framing** | ✅ `<tool_output>` envelope | ❌ | ❌ | ❌ |
| **Lifecycle observer trait** | ✅ | ⚠ OTel only | ⚠ hooks | ⚠ callbacks |

`agnt` is ~22× denser than the dominant Rust agent framework. The tradeoff: no tokio means no async I/O parallelism beyond what `std::thread::scope` gives you. For agents that spend most of their wall time waiting on LLM inference, this is a non-issue.

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
        .on_token(Box::new(|tok| {
            use std::io::Write;
            print!("{}", tok);
            std::io::stdout().flush().ok();
        }))
        .build()?;

    let reply = agent.step("Find TODOs in src/")?;
    println!("\n{}", reply);
    Ok(())
}
```

## The v0.2 security model

`agnt` v0.2 treats the LLM as an adversary. Every tool in the default set is structurally constrained:

- **`Shell` is off by default.** Opt in via `features = ["tools-shell"]`. The only constructor requires an explicit argv allowlist. Commands parse via `shell-words` and run directly via `Command::new(argv[0])` — **never `sh -c`**. Tokens containing `$`, `` ` ``, `|`, `;`, `&`, `>`, `<`, `(`, `)` are rejected.

- **Filesystem tools are sandbox-aware.** `ReadFile`, `WriteFile`, `EditFile`, `ListDir`, `Glob`, `Grep` all accept an optional `FilesystemRoot` that canonicalizes paths, rejects `..` components, and follows symlinks before checking containment. Without a sandbox the tool docs explicitly warn about full host access.

- **`Fetch` blocks SSRF.** Scheme allowlist (`http`/`https`), DNS resolution, and rejection of any IP that is loopback / private / link-local / multicast / unspecified / broadcast / AWS IMDS / GCP metadata. HTTP redirects are disabled on the shared agent so a `302 Location: http://169.254.169.254/` cannot bypass the check.

- **Tool outputs are framed as data.** Every tool result is wrapped in a `<tool_output name="..." id="..." truncated="...">...</tool_output>` envelope with 64KB cap before being persisted or fed back to the model. The system prompt should explicitly instruct the model that content inside these envelopes is data, not instructions.

- **`EditFile` is atomic.** Sidecar lockfile (target-file locking can't survive the atomic rename), re-read under lock, temp write + rename. Verified with a 4-thread × 100-round stress test.

- **No panics.** Every `.unwrap()` / `.expect()` in the library path has been removed. Scoped-thread panics during tool dispatch are caught and converted to error strings.

- **Secrets don't leak.** `Backend::api_key` is private. Manual `Debug` impl prints `api_key: <redacted>`. Upstream error bodies are scrubbed of `Authorization` and `x-api-key` headers before bubbling up.

Read the full [threat model](https://github.com/hmbldv/agnt/blob/main/THREAT_MODEL.md) for what's covered and what isn't.

## Performance posture (v0.2)

| Operation | v0.1 | v0.2 | Notes |
|---|---|---|---|
| `Agent::step` (short history) | full clone of all messages | zero clones, borrows directly | P1 |
| HTTP request body on retry | cloned per attempt | serialized once, `send_bytes(&[u8])` | P2 |
| SQLite `append` | 2 roundtrips, 2+N fsyncs | 1 roundtrip, 1 fsync via WAL + txn | P3 |
| SSE stream parser | `String` allocated per line | single reused buffer | P4 |
| Retry backoff | deterministic 500/1000/2000ms | ±20% jitter (xorshift64*) | P5 |
| HTTP timeouts | unbounded | 10s connect / 120s read default | P6 |

## Crate split

The v0.2 workspace ships five published crates:

| Crate | Purpose | Deps pulled |
|---|---|---|
| **`agnt`** | Flagship meta-crate — what you `cargo add` | re-exports from the rest |
| **`agnt-core`** | Traits + types + Agent loop | `serde`, `serde_json`, `tracing` |
| **`agnt-net`** | HTTP backend (Ollama/OpenAI/Anthropic) | `ureq`, `native-tls` |
| **`agnt-store`** | Bundled-SQLite persistence | `rusqlite/bundled` |
| **`agnt-tools`** | Built-in sandboxed tools | `walkdir`, `regex`, `glob`, `fs2`, `url`, `shell-words` |

**Minimal build** (~1MB, WASM-compatible):
```toml
agnt = { version = "0.2", default-features = false, features = ["net"] }
```

**Full build** (same as v0.1):
```toml
agnt = "0.2"  # net + store + tools
```

**Full build with shell (opt-in, CVE-class):**
```toml
agnt = { version = "0.2", features = ["tools-shell"] }
```

## Typed tools (v0.2)

v0.2 adds a typed `TypedTool` trait alongside the existing erased `Tool`:

```rust
use agnt::{TypedTool, Registry};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)] struct Args { a: i64, b: i64 }
#[derive(Serialize)] struct Out { sum: i64 }

struct Add;
impl TypedTool for Add {
    type Args = Args;
    type Output = Out;
    type Error = String;
    const NAME: &'static str = "add";
    const DESCRIPTION: &'static str = "Add two integers.";
    fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "a": { "type": "integer" },
                "b": { "type": "integer" }
            },
            "required": ["a", "b"]
        })
    }
    fn call(&self, args: Args) -> Result<Out, String> {
        Ok(Out { sum: args.a + args.b })
    }
}

let mut reg = Registry::new();
reg.register_typed(Add);  // auto-wraps in ErasedAdapter
```

No `from_str` dance inside `call`; no stringification on the output path. Existing erased `Tool` impls keep working unchanged.

## Observer hooks

Every step has five lifecycle hooks you can observe without forking the loop:

```rust
use agnt::{Observer, Message, ToolCall, ToolResult};

struct AuditLog;
impl Observer for AuditLog {
    fn on_tool_start(&self, call: &ToolCall) {
        println!("→ {}", call.function.name);
    }
    fn on_tool_end(&self, call: &ToolCall, result: &ToolResult) {
        println!("← {} ({}µs)", call.function.name, result.duration_us);
    }
    // on_step_start, on_step_end, on_step_error also available
}
```

Attach via `AgentBuilder::observer(Arc::new(AuditLog))`. This is the integration point for HITL approval, NATS event publishing, OpenTelemetry spans via `tracing-opentelemetry`, or anything else you want to hang off the loop.

## Observability

`agnt` emits `tracing` spans and events at key boundaries:

- `agnt.step { session, input_len }` — every call to `Agent::step`
- `agnt.backend.chat { kind, model, message_count, streaming }` — every LLM inference call
- `agnt.tool { name, id }` — every tool dispatch

Zero dependency on `opentelemetry` — use `tracing-opentelemetry` externally to export to Jaeger / Honeycomb / Datadog / Tempo if you want.

## Roadmap

- **v0.2** (current) — hardening + restructuring pass, typed tools, crate split, tracing
- **v0.3** — `#[tool]` proc-macro, MCP client, trust tier gating, fuzzing targets
- **v1.0** — API freeze

## License

Dual-licensed under either:

- MIT License ([LICENSE-MIT](../../LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](../../LICENSE-APACHE))

at your option.
