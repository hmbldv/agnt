//! # agnt
//!
//! A dense, sync-first Rust agent engine. Multi-backend LLM inference
//! with streaming, parallel tool dispatch, SQLite session persistence,
//! and microsecond-level tool profiling — no async runtime required.
//! Around 6,000 LOC across seven crates as of v0.3.1; see the repository
//! README for the live breakdown.
//!
//! ## Quick start
//!
//! ```no_run
//! use agnt::{Agent, Backend};
//!
//! let backend = Backend::ollama("gemma4:e4b");
//! let mut agent = Agent::new(backend, "You are a helpful assistant.");
//! # #[cfg(feature = "tools")]
//! agent.tools.register(Box::new(agnt::builtins::ReadFile::new()));
//!
//! let reply = agent.step("Read /etc/hostname and tell me the hostname.").unwrap();
//! println!("{}", reply);
//! ```
//!
//! ## Architecture (v0.3.1 — seven-crate workspace)
//!
//! The flagship `agnt` crate is a thin re-export over six underlying
//! library crates. Everything is feature-gated so consumers can pick
//! the slice they need — WASM / embedded callers depend only on
//! `agnt-core`.
//!
//! - [`agnt-core`](https://crates.io/crates/agnt-core) — traits, types,
//!   agent loop, quotas, observer hooks. Zero I/O dependencies.
//! - [`agnt-net`](https://crates.io/crates/agnt-net) — HTTP backend
//!   implementation (Ollama / OpenAI / Anthropic). `net` feature.
//! - [`agnt-store`](https://crates.io/crates/agnt-store) — SQLite
//!   message store with µs-precision tool log. `store` feature.
//! - [`agnt-tools`](https://crates.io/crates/agnt-tools) — built-in
//!   tools with filesystem sandbox, atomic SSRF-guarded Fetch, and
//!   opt-in Shell (plus `bwrap-shell` on Linux). `tools` feature.
//! - [`agnt-macros`](https://crates.io/crates/agnt-macros) — `#[tool]`
//!   attribute macro. `macros` feature (default on).
//! - [`agnt-mcp`](https://crates.io/crates/agnt-mcp) — MCP stdio
//!   client. `mcp` feature (off by default).
//!
//! `default = ["net", "store", "tools", "macros"]` gives you the
//! working runtime from a single `cargo add agnt`. Opt in to `mcp`
//! and `tools-bwrap-shell` as needed.
//!
//! ## Design principles
//!
//! 1. **Sync-first.** No tokio required. Tool dispatch uses
//!    [`std::thread::scope`] for parallelism without an async runtime.
//! 2. **Structurally sandboxed.** Filesystem root, atomic SSRF
//!    resolver, opt-in Shell, optional bubblewrap — each layer is
//!    designed assuming the LLM output is hostile.
//! 3. **Multi-backend from day one.** One internal `Message` type;
//!    providers translate at the wire boundary.
//! 4. **Auditable by module.** Security-critical paths (agent loop,
//!    tools, sandbox, SSRF resolver, MCP framing) each live in a
//!    single small file so reviewers can read them in isolation.
//!
//! See the [README](https://github.com/hmbldv/agnt) for benchmarks, the
//! current threat model, and the roadmap.

// Core types and traits — always re-exported.
pub use agnt_core::agent::ToolQuota;
pub use agnt_core::{
    Agent, AgentBuilder, BackendError, Disposition, ErasedAdapter, FunctionCall, LlmBackend,
    Message, MessageStore, Observer, Registry, StoreError, Tool, ToolCall, ToolLog, ToolResult,
    TypedTool, UsageStats,
};

/// Alias the agent loop module for explicit access.
pub mod agent {
    pub use agnt_core::agent::*;
}

/// Alias the tool module for explicit access.
pub mod tool {
    pub use agnt_core::tool::*;
}

// Network backend — feature-gated.
#[cfg(feature = "net")]
pub use agnt_net::Backend;

#[cfg(feature = "net")]
pub mod backend {
    pub use agnt_net::backend::*;
}

#[cfg(feature = "net")]
pub mod http {
    pub use agnt_net::http::*;
}

// Persistence — feature-gated.
#[cfg(feature = "store")]
pub use agnt_store::Store;

#[cfg(feature = "store")]
pub mod store {
    pub use agnt_store::store::*;
}

// Built-in tools — feature-gated.
#[cfg(feature = "tools")]
pub mod builtins {
    pub use agnt_tools::builtins::*;
}

// Sandbox primitive, re-exported at the flagship crate root so consumers
// don't have to pull `agnt-tools` directly for the most common
// sandbox-aware tool construction pattern (`ReadFile::with_sandbox(Arc<…>)`).
// Added in v0.3.2 after SOLA became the first real downstream consumer
// and surfaced the ergonomic gap.
#[cfg(feature = "tools")]
pub use agnt_tools::FilesystemRoot;

// v0.3: #[tool] proc-macro. Feature-gated so crates that want a minimal
// agnt-core footprint can skip the proc-macro compile cost.
#[cfg(feature = "macros")]
pub use agnt_macros::tool;

// v0.3: MCP stdio client + Tool bridge. Feature-gated because the average
// user doesn't need it and it pulls in child-process plumbing.
#[cfg(feature = "mcp")]
pub mod mcp {
    pub use agnt_mcp::*;
}

// Async execution runtime. Wraps agnt-core's sync Agent<B> with retry,
// recovery cascade, budget tracking, and execution modes (OneShot, Loop,
// UntilSuccess, Pipeline). Requires tokio.
#[cfg(feature = "engine")]
pub mod engine {
    pub use agnt_engine::*;
}

// WASM-sandboxed tool generation: ask the LLM for Rust source, compile to
// wasm32-wasip1, version in SQLite, execute through wasmtime. Generated tools
// implement agnt_core::Tool and slot into any Registry.
#[cfg(feature = "toolgen")]
pub mod toolgen {
    pub use agnt_toolgnrtr::{
        GeneratedSpec, Generator, SandboxConfig, ScriptTool, TestCase, ToolGenerator, ToolRecord,
        ToolStats, ToolStore, ToolSummary, WasmCompiler,
    };
}
