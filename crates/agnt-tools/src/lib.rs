//! # agnt-tools
//!
//! Built-in tools for the agnt agent runtime.
//!
//! Ships eight ready-to-use tools that implement [`agnt_core::Tool`]:
//!
//! - **Filesystem**: `ReadFile`, `WriteFile`, `EditFile`, `ListDir`
//! - **Search**: `Glob`, `Grep`
//! - **Network**: `Fetch`
//! - **Shell** (`shell` feature, opt-in, CVE-class): `Shell`
//!
//! ## Security notes
//!
//! The v0.2 hardening pass adds a `filesystem_root` sandbox, an SSRF guard
//! on `Fetch`, and makes `Shell` opt-in via the `shell` feature. See the
//! v0.2 plan doc and threat model for details.

pub mod builtins;
pub mod http;

// Phase 0: re-exports mirror v0.1 surface. Phase 1 Agent D gates Shell
// behind the `shell` feature (plan item S1).
pub use builtins::{EditFile, Fetch, Glob, Grep, ListDir, ReadFile, Shell, WriteFile};
