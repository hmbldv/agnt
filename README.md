# agnt

**A dense, sync-first Rust agent runtime — module-level auditable, structurally sandboxed against adversarial LLM output, and composable across async and sync callers without forcing a runtime choice on either.**

[![Crates.io](https://img.shields.io/crates/v/agnt.svg)](https://crates.io/crates/agnt)
[![Documentation](https://docs.rs/agnt/badge.svg)](https://docs.rs/agnt)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

```toml
[dependencies]
agnt = "0.3"
```

## Repository layout (v0.3.1 — seven-crate workspace)

Around 6,200 LOC across seven published library crates plus a REPL
binary and a fuzz workspace. Each security-critical path lives in a
single small file (agent loop 947 LOC, tools 1,196 LOC, MCP framing
~750 LOC, SSRF resolver ~250 LOC) so reviewers can read one layer at
a time without holding the whole runtime in their head.

| Path | Crate | Purpose |
|---|---|---|
| `crates/agnt/` | `agnt` | Flagship meta-crate — what you `cargo add` |
| `crates/agnt-core/` | `agnt-core` | Traits + message types + Agent loop. Zero I/O deps. WASM-ready. |
| `crates/agnt-net/` | `agnt-net` | HTTP backend (Ollama / OpenAI / Anthropic) with streaming + retry |
| `crates/agnt-store/` | `agnt-store` | SQLite message store (bundled, WAL mode, prepared-statement cache) |
| `crates/agnt-tools/` | `agnt-tools` | Built-in tools with filesystem sandbox, SSRF guard, opt-in shell (+ bubblewrap on Linux) |
| `crates/agnt-macros/` | `agnt-macros` | `#[tool]` attribute macro — turn a `fn` into a `TypedTool` with zero boilerplate |
| `crates/agnt-mcp/` | `agnt-mcp` | MCP stdio client — bridges remote MCP tools into `agnt_core::Tool` |
| `fuzz/` | — | libfuzzer targets for sandbox, SSRF, glob, dispatch |
| `src/` | `agnt-rs` | REPL binary example consumer |

All seven library crates publish independently; `cargo add agnt` pulls the
default stack via `default = ["net", "store", "tools", "macros"]`. Opt in
to `mcp` and `tools-bwrap-shell` as needed.

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

## Documentation

- **[Library README](crates/agnt/README.md)** — Full feature matrix, typed
  tools, observer hooks, benchmarks, and comparison against rig-core,
  llm, langchain-rust.
- **[Threat model](THREAT_MODEL.md)** — Current security posture: what
  the sandbox, SSRF resolver, and opt-in Shell defend against, what's
  partially mitigated, and what's out of scope.
- **[Changelog](CHANGELOG.md)** — v0.1 → v0.2 → v0.3 → v0.3.1 notes,
  every breaking change, and the full list of security and performance
  fixes.

## License

Dual-licensed under either:

- MIT License ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
