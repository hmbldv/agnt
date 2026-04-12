//! # agnt-store
//!
//! SQLite-backed message store for the agnt agent runtime.
//!
//! Provides a bundled SQLite persistence layer that implements
//! [`agnt_core::MessageStore`] with session-scoped message logs and
//! microsecond-level tool call profiling.
//!
//! Uses `rusqlite/bundled` so there's no system libsqlite dependency —
//! builds everywhere Rust builds.

pub mod store;

pub use store::Store;
