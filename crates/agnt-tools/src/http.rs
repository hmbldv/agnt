//! Shared HTTP client for tool use (Fetch, etc.).
//!
//! Separate from `agnt-net`'s LLM backend client so the two have independent
//! connection pools. Tool fetches go to arbitrary user URLs; LLM calls go to
//! a single provider. Different traffic patterns, different retry posture.

use std::sync::{Arc, OnceLock};

static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

/// Shared ureq Agent wired to a native-tls connector so HTTPS verifies
/// against the system CA store (not the baked-in webpki-roots).
///
/// `redirects(0)` is set so the SSRF guard in `Fetch` cannot be bypassed by
/// a `302 Location: http://169.254.169.254/…` hop — the model must re-fetch
/// any redirect target explicitly, re-triggering validation.
///
/// Falls back to a plain ureq Agent (with TLS config) if `native-tls` init
/// fails at runtime instead of panicking — addresses the S5 panic-removal
/// pass for the first-HTTPS-call panic site.
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
