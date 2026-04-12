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
//! agent.tools.register(Box::new(agnt::builtins::ReadFile));
//!
//! let reply = agent.step("Read /etc/hostname and tell me the hostname.").unwrap();
//! println!("{}", reply);
//! ```
//!
//! ## Architecture
//!
//! - [`Backend`] — multi-provider LLM client (Ollama, OpenAI, Anthropic) with streaming and retry
//! - [`Agent`] — the core loop: message → inference → parallel tool dispatch → loop
//! - [`Tool`] — trait for extending the agent with capabilities
//! - [`Registry`] — collection of tools with name-based dispatch
//! - [`Store`] — SQLite session persistence with µs tool-call profiling
//! - [`builtins`] — eight ready-to-use tools for files, search, HTTP, and shell
//!
//! ## Design principles
//!
//! 1. **Sync-first.** No tokio. The agent loop is synchronous. Tool dispatch uses
//!    [`std::thread::scope`] for parallel execution without an async runtime.
//! 2. **Dense.** Every module is small, focused, and auditable. No framework ceremony.
//! 3. **Multi-backend from day one.** The `Message` and `ToolCall` types are
//!    OpenAI-flavored internally; Anthropic content blocks are translated at the
//!    wire boundary. Switching backends is one line.
//! 4. **Persistence is free.** SQLite ships with the crate (`rusqlite/bundled`)
//!    so you don't need system libsqlite. Every tool call is timed in microseconds
//!    and written to the session log.
//!
//! See the [README](https://github.com/hmbldv/agnt) for benchmarks, feature comparison
//! against other Rust agent libraries, and roadmap.

pub mod agent;
pub mod backend;
pub mod builtins;
pub mod http;
pub mod store;
pub mod tool;

pub use agent::Agent;
pub use backend::{Backend, Message};
pub use store::Store;
pub use tool::{Registry, Tool};
