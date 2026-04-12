use std::sync::{Arc, OnceLock};

static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

/// Single shared ureq Agent wired to a native-tls connector so HTTPS
/// verifies against the system CA store (not the baked-in webpki-roots).
pub fn agent() -> &'static ureq::Agent {
    AGENT.get_or_init(|| {
        let connector = native_tls::TlsConnector::new()
            .expect("native-tls connector");
        ureq::AgentBuilder::new()
            .tls_connector(Arc::new(connector))
            .build()
    })
}
