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
//! - `Fetch` has a built-in SSRF guard that runs *atomically* with DNS
//!   resolution via a custom [`ureq::Resolver`] ([`ssrf::SsrfResolver`]).
//!   http/https only, IPv4/IPv6 private / loopback / link-local /
//!   multicast / metadata addresses rejected in the same lookup that
//!   `ureq` then uses to connect — no DNS-rebinding TOCTOU. Redirects
//!   are disabled on the per-instance agent.
//! - `Shell` is gated behind the `shell` cargo feature; it has no
//!   unsandboxed constructor. On Linux, the `bwrap-shell` feature adds
//!   a bubblewrap namespace on top of the argv allowlist for defense
//!   in depth.
//!
//! See `THREAT_MODEL.md` in the repo root for the current threat model
//! (updated for v0.3.1).

pub mod builtins;
pub mod http;
pub mod sandbox;
pub mod ssrf;

pub use builtins::{EditFile, Fetch, Glob, Grep, ListDir, ReadFile, WriteFile};
pub use sandbox::{FilesystemRoot, SandboxedPath};

/// The CVE-class `Shell` tool. Only available when the `shell` cargo feature
/// is enabled. See [`builtins::Shell`] for the full threat-model rustdoc.
#[cfg(feature = "shell")]
pub use builtins::Shell;

/// Read-only system information tools. Available when the `system-tools` cargo
/// feature is enabled. All commands are hardcoded — no injection surface.
#[cfg(feature = "system-tools")]
pub mod system;
#[cfg(feature = "system-tools")]
pub use system::{DiskUsage, DockerPs, NvidiaSmi, SystemInfo};
