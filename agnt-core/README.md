# agnt

**A dense, sync-first Rust agent engine.** Multi-backend LLM inference with streaming, parallel tool dispatch, SQLite session persistence, and microsecond-level tool profiling — in under 1,500 lines of code with no async runtime required.

```toml
[dependencies]
agnt = "0.1"
```

## Why agnt

Most Rust LLM agent libraries are thick wrappers around Python concepts: tokio everywhere, trait-object soup, opinionated orchestration frameworks. `agnt` is the opposite — a small, sync-first library that gives you a working agent loop, tool trait, and persistence layer, and gets out of your way.

| | `agnt` | rig-core | llm | langchain-rust |
|---|---|---|---|---|
| **LOC** | ~1,500 | ~49,000 | ~3,500 | ~20,000 |
| **Async runtime required** | ❌ | ✅ tokio | ✅ tokio | ✅ tokio |
| **Binary size (release + LTO)** | 4.5 MB | 12+ MB | 8+ MB | 15+ MB |
| **Multi-backend** | ✅ Ollama, OpenAI, Anthropic | ✅ 20+ providers | ✅ | ✅ |
| **Parallel tool dispatch** | ✅ `thread::scope` | ✅ tokio | ❌ | ❌ |
| **SQLite persistence built-in** | ✅ | ❌ | ❌ | ❌ |
| **µs tool profiling** | ✅ | ❌ | ❌ | ❌ |

`agnt` is ~30× denser than the dominant Rust agent framework and adds SQLite persistence that nobody else ships. The tradeoff: no tokio means no async I/O parallelism beyond what `std::thread::scope` gives you. For agents that spend most of their wall time waiting on LLM inference, this is a non-issue.

## Quick start

```rust
use agnt::{Agent, Backend, Tool, Registry};
use serde_json::{json, Value};

// 1. Define a tool
struct Echo;
impl Tool for Echo {
    fn name(&self) -> &str { "echo" }
    fn description(&self) -> &str { "Echo back the input" }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        })
    }
    fn call(&self, args: Value) -> Result<String, String> {
        Ok(args["text"].as_str().unwrap_or("").to_string())
    }
}

fn main() -> Result<(), String> {
    // 2. Pick a backend
    let backend = Backend::ollama("gemma4:e4b");
    // or: Backend::openai("gpt-4o-mini", &api_key)
    // or: Backend::anthropic("claude-sonnet-4-6", &api_key)

    // 3. Build an agent
    let mut agent = Agent::new(backend, "You are a helpful assistant.");
    agent.tools.register(Box::new(Echo));

    // 4. Run a step
    let reply = agent.step("Use the echo tool to say hello")?;
    println!("{}", reply);
    Ok(())
}
```

## Features

### Multi-backend inference

```rust
Backend::ollama("gemma4:e4b")           // Local, via OpenAI-compatible API
Backend::openai("gpt-4o-mini", key)     // OpenAI
Backend::anthropic("claude-sonnet-4-6", key) // Anthropic native API with schema translation
```

All three use the same internal `Message`/`ToolCall` format. Switching backends is one line.

### Streaming with a callback sink

```rust
let mut sink = |token: &str| {
    print!("{}", token);
    std::io::stdout().flush().ok();
};
let msg = backend.chat(&messages, &tools, Some(&mut sink))?;
```

Works with both OpenAI-format SSE and Anthropic's content-block streaming protocol, including fragmented tool call reassembly by index.

### Parallel tool dispatch via `thread::scope`

When a model returns multiple tool calls in one turn, `agnt` executes them concurrently on OS threads and joins before sending results back. No `tokio`, no `Arc<Mutex<_>>` — just `std::thread::scope`.

### Built-in tools (optional)

`agnt` ships 8 battle-tested tools you can register with one line each:

- `ReadFile`, `WriteFile`, `EditFile` — file I/O with UTF-8-safe truncation and unique-match edit semantics
- `ListDir`, `Glob`, `Grep` — filesystem search via `walkdir` and `regex`
- `Fetch` — HTTP GET with native-tls, 50KB response cap
- `Shell` — `sh -c` with a denylist for destructive commands

```rust
agent.tools.register(Box::new(agnt::builtins::ReadFile));
agent.tools.register(Box::new(agnt::builtins::Grep));
// ...
```

### SQLite session persistence

Two-table schema: `messages` (full conversation log) and `tool_calls` (every dispatch with µs timing).

```rust
let store = agnt::Store::open("~/.agnt.db")?;
agent.attach_store(store, "session-id")?;
// Every step now persists, and new processes can resume the session.
```

### Context windowing without orphan tool results

`agnt` keeps your message history bounded by an `max_window` count but never cuts between a `tool_use` and its `tool_result` — the window always advances to a user-message boundary so the backend never rejects your request for a malformed tool sequence.

### Retry and backoff

HTTP calls automatically retry on 429/5xx with exponential backoff capped at 8 seconds, up to 5 attempts.

## Benchmarks

Measured on an RTX 5090 with `gemma4:e4b` via Ollama, single agent with exclusive GPU:

| Operation | Latency |
|---|---|
| `read_file` tool | 11 µs |
| `glob` tool (src/**/*.rs) | 24 µs |
| `grep` tool (regex over source tree) | 139 µs |
| `fetch` tool (HTTPS roundtrip) | 563 ms |
| Simple chat turn (no tools) | 137 ms |
| Tool-calling turn | ~2 s |

The tool dispatch overhead is effectively zero — the LLM inference is always the bottleneck. `agnt`'s dense design means you're spending cycles on inference, not on framework overhead.

## Example: agnt-rs

The included `agnt-rs` binary is a 137-line REPL wrapper around `agnt` with CLI flags for session management, tool allowlisting, streaming toggles, and a `/stats` command that prints µs-level tool latency breakdowns from the SQLite log:

```bash
cargo run --release -- --session work
> find all .rs files under src/ and tell me which is largest
> /stats

tool          count       avg_us       max_us
grep              3          139          187
read_file         1           11           11
glob              1           24           24
```

See `src/main.rs` in the repository.

## Roadmap

- **v0.1** (current): core engine + builtin tools + SQLite persistence
- **v0.2**: golden-image feature flags (compile-time tool gating for trust tiers)
- **v0.3**: NATS observability module (publish lifecycle events to an event bus)
- **v0.4**: session contracts (spawn-time lifecycle bounds for ephemeral agents)
- **v1.0**: API freeze

## License

Dual-licensed under MIT OR Apache-2.0, at your option.
