//! # agnt-core
//!
//! The zero-I/O kernel of the agnt agent runtime. Defines the message types,
//! tool trait, backend abstraction, persistence abstraction, observer hooks,
//! and the synchronous agent loop itself — with no HTTP, no SQLite, and no
//! async runtime dependencies.
//!
//! Users who want a working agent typically depend on the flagship [`agnt`]
//! meta-crate which pulls in `agnt-net`, `agnt-store`, and `agnt-tools` as
//! well. Users building custom backends, custom stores, or WASM targets can
//! depend on `agnt-core` alone and wire up their own implementations of
//! [`LlmBackend`] and [`MessageStore`].
//!
//! ## Architecture
//!
//! ```text
//! agnt (flagship re-export)
//!   ├── agnt-core      ← you are here
//!   ├── agnt-net       (HTTP backend implementation)
//!   ├── agnt-store     (SQLite message store implementation)
//!   └── agnt-tools     (built-in tools: read_file, grep, fetch, etc.)
//! ```
//!
//! [`agnt`]: https://crates.io/crates/agnt

pub mod agent;
pub mod backend_trait;
pub mod message;
pub mod observer;
pub mod store_trait;
pub mod tool;

pub use agent::Agent;
pub use backend_trait::{BackendError, LlmBackend};
pub use message::{FunctionCall, Message, ToolCall};
pub use observer::{Observer, ToolResult};
pub use store_trait::{MessageStore, StoreError, ToolLog};
pub use tool::{Registry, Tool};
