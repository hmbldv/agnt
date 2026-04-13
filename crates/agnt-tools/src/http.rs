//! Deprecated shared HTTP client.
//!
//! v0.3.1 moved `Fetch` to a per-instance `ureq::Agent` with a custom
//! [`crate::ssrf::SsrfResolver`] so DNS resolution and SSRF validation
//! happen atomically — closing the v0.2/v0.3 TOCTOU window where the
//! pre-request `ssrf_check` and ureq's internal lookup could see
//! different addresses (DNS rebinding).
//!
//! The process-wide shared agent that used to live here had no way to
//! carry a per-`Fetch` allowlist, so keeping it as the `Fetch` backend
//! blocked the security fix. The shim is preserved *only* so external
//! crates that called `agnt_tools::http::agent()` directly keep
//! compiling; it is now marked `#[deprecated]` and will be removed in
//! v0.4.
//!
//! **Do not use this for new code.** It has no SSRF guard — build your
//! own `ureq::Agent` (with an `SsrfResolver` if the URL is attacker-
//! influenced) or use [`crate::Fetch`] directly.

use std::sync::{Arc, OnceLock};

static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

/// Deprecated: returns a shared ureq Agent with `redirects(0)` and the
/// system TLS store, but *without* the SSRF resolver. Use
/// [`crate::Fetch`] for any attacker-influenced URL.
#[deprecated(
    since = "0.3.1",
    note = "use agnt_tools::Fetch directly; this shim has no SSRF guard and will be removed in v0.4"
)]
pub fn agent() -> &'static ureq::Agent {
    AGENT.get_or_init(|| {
        match native_tls::TlsConnector::new() {
            Ok(connector) => ureq::AgentBuilder::new()
                .tls_connector(Arc::new(connector))
                .redirects(0)
                .build(),
            Err(_) => ureq::AgentBuilder::new().redirects(0).build(),
        }
    })
}
