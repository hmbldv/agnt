# agnt

**A dense, sync-first Rust agent engine.** Multi-backend LLM inference with streaming, parallel tool dispatch, SQLite session persistence, and microsecond-level tool profiling — in under 1,500 lines of code with no async runtime required.

[![Crates.io](https://img.shields.io/crates/v/agnt.svg)](https://crates.io/crates/agnt)
[![Documentation](https://docs.rs/agnt/badge.svg)](https://docs.rs/agnt)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

```toml
[dependencies]
agnt = "0.1"
```

## Repository layout

This repo is a Cargo workspace with two crates:

- **`agnt-core/`** — the `agnt` library crate ([README](agnt-core/README.md) · [crates.io](https://crates.io/crates/agnt) · [docs.rs](https://docs.rs/agnt))
- **`src/`** — the `agnt-rs` binary, a REPL example consumer of the library

## Quick start

```rust
use agnt::{Agent, Backend};

let backend = Backend::ollama("gemma4:e4b");
let mut agent = Agent::new(backend, "You are a helpful assistant.");
agent.tools.register(Box::new(agnt::builtins::Grep));

let reply = agent.step("Find TODOs in src/")?;
println!("{}", reply);
```

## Running the binary

```bash
ollama pull gemma4:e4b
cargo run --release

> find all .rs files under src/ and tell me which is largest
> /stats
```

The included `agnt-rs` binary is a 137-line REPL wrapper around `agnt` with CLI flags for session management, tool allowlisting, streaming toggles, and a `/stats` command that prints µs-level tool latency breakdowns from the SQLite log.

See **[agnt-core/README.md](agnt-core/README.md)** for full library documentation, feature comparison against `rig-core` / `llm` / `langchain-rust`, benchmarks, and roadmap.

## License

Dual-licensed under either:

- MIT License ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
