//! # agnt-net
//!
//! HTTP backend implementations for the agnt agent runtime.
//!
//! Provides concrete [`Backend`] implementations for:
//! - Ollama (via its OpenAI-compatible API)
//! - OpenAI
//! - Anthropic (with automatic content-block translation)
//!
//! All three use the same internal [`agnt_core::Message`] format — Anthropic's
//! content blocks are translated at the wire boundary.
//!
//! ## Example
//!
//! ```no_run
//! use agnt_net::Backend;
//! use agnt_core::LlmBackend;
//!
//! let backend = Backend::ollama("gemma4:e4b");
//! assert_eq!(backend.model(), "gemma4:e4b");
//! ```

pub mod backend;
pub mod http;

pub use backend::{Backend, Kind};
