//! Shared HTTP client for tool use (Fetch, etc.).
//!
//! Separate from `agnt-net`'s LLM backend client so the two have independent
//! connection pools. Tool fetches go to arbitrary user URLs; LLM calls go to
//! a single provider. Different traffic patterns, different retry posture.

use std::sync::{Arc, OnceLock};

static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

/// Shared ureq Agent wired to a native-tls connector so HTTPS verifies
/// against the system CA store (not the baked-in webpki-roots).
pub fn agent() -> &'static ureq::Agent {
    AGENT.get_or_init(|| {
        let connector = native_tls::TlsConnector::new().expect("native-tls connector");
        ureq::AgentBuilder::new()
            .tls_connector(Arc::new(connector))
            .build()
    })
}
