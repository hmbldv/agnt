//! # agnt
//!
//! A dense, sync-first Rust agent engine. Multi-backend LLM inference with streaming,
//! parallel tool dispatch, SQLite session persistence, and microsecond-level tool
//! profiling — in under 1,500 lines of code with no async runtime required.
//!
//! ## Quick start
//!
//! ```no_run
//! use agnt::{Agent, Backend};
//!
//! let backend = Backend::ollama("gemma4:e4b");
//! let mut agent = Agent::new(backend, "You are a helpful assistant.");
//! # #[cfg(feature = "tools")]
//! agent.tools.register(Box::new(agnt::builtins::ReadFile));
//!
//! let reply = agent.step("Read /etc/hostname and tell me the hostname.").unwrap();
//! println!("{}", reply);
//! ```
//!
//! ## Architecture (v0.2 — multi-crate workspace)
//!
//! The flagship `agnt` crate is a thin re-export over four underlying crates:
//!
//! - [`agnt-core`](https://crates.io/crates/agnt-core) — traits, types, and
//!   the sync agent loop. Zero I/O dependencies; compiles to WASM.
//! - [`agnt-net`](https://crates.io/crates/agnt-net) — HTTP backend
//!   implementation (Ollama / OpenAI / Anthropic). Behind the `net` feature.
//! - [`agnt-store`](https://crates.io/crates/agnt-store) — SQLite message
//!   store. Behind the `store` feature.
//! - [`agnt-tools`](https://crates.io/crates/agnt-tools) — built-in tools
//!   (filesystem, search, fetch). Behind the `tools` feature. Shell is
//!   opt-in via `tools-shell`.
//!
//! All three feature flags are on by default so `cargo add agnt` gives you
//! the full runtime. Opt out for minimal / embedded / WASM use.
//!
//! ## Design principles
//!
//! 1. **Sync-first.** No tokio required. Tool dispatch uses
//!    [`std::thread::scope`] for parallelism without an async runtime.
//! 2. **Dense.** Every module is small, focused, and auditable.
//! 3. **Multi-backend from day one.** One internal `Message` type; providers
//!    translate at the wire boundary.
//! 4. **Structurally sandboxed.** v0.2 adds filesystem root, SSRF guards, and
//!    opt-in Shell so adversarial LLM output can't escape the probe.
//!
//! See the [README](https://github.com/hmbldv/agnt) for benchmarks, the
//! v0.2 threat model, and the roadmap.

// Core types and traits — always re-exported.
pub use agnt_core::{
    Agent, BackendError, FunctionCall, LlmBackend, Message, MessageStore, Observer, Registry,
    StoreError, Tool, ToolCall, ToolLog, ToolResult,
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
