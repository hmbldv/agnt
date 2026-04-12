//! # agnt-tools
//!
//! Built-in tools for the agnt agent runtime.
//!
//! Ships seven default tools that implement [`agnt_core::Tool`]:
//!
//! - **Filesystem**: `ReadFile`, `WriteFile`, `EditFile`, `ListDir`
//! - **Search**: `Glob`, `Grep`
//! - **Network**: `Fetch`
//!
//! Plus one **opt-in CVE-class** tool behind the `shell` feature:
//!
//! - **Shell** (`shell` feature): [`Shell`] — arbitrary command execution,
//!   default-OFF, requires an explicit sandbox config at construction.
//!
//! ## Security notes
//!
//! - All filesystem tools accept an optional [`sandbox::FilesystemRoot`] via
//!   `with_sandbox`. Without a sandbox they can read / write / list anywhere
//!   the process has access; with one, every path is canonicalized and
//!   rejected if it escapes the root.
//! - `Fetch` has a built-in SSRF guard: http/https only, IPv4/IPv6
//!   private / loopback / link-local / multicast / metadata addresses
//!   rejected, redirects disabled on the shared ureq agent.
//! - `Shell` is gated behind the `shell` cargo feature; it has no
//!   unsandboxed constructor. See its rustdoc for the threat model.
//!
//! See the v0.2 threat model (`agnt-v0.2-plan.md` Part 2 S1–S7) for
//! details.

pub mod builtins;
pub mod http;
pub mod sandbox;

pub use builtins::{EditFile, Fetch, Glob, Grep, ListDir, ReadFile, WriteFile};
pub use sandbox::FilesystemRoot;

/// The CVE-class `Shell` tool. Only available when the `shell` cargo feature
/// is enabled. See [`builtins::Shell`] for the full threat-model rustdoc.
#[cfg(feature = "shell")]
pub use builtins::Shell;
