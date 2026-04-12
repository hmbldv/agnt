use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Default connect timeout for the shared Agent.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default read timeout for the shared Agent.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(120);

// Cache the result of the shared agent build so we only attempt TLS init once,
// but surface a `Result` to callers instead of panicking on failure.
static AGENT: OnceLock<Result<ureq::Agent, String>> = OnceLock::new();

/// Build a ureq Agent with a native-tls connector so HTTPS verifies against
/// the system CA store. Returns an error if TLS initialization fails.
pub fn build_agent(connect_timeout: Duration, read_timeout: Duration) -> Result<ureq::Agent, String> {
    let connector = native_tls::TlsConnector::new()
        .map_err(|e| format!("native-tls connector init failed: {}", e))?;
    Ok(ureq::AgentBuilder::new()
        .tls_connector(Arc::new(connector))
        .timeout_connect(connect_timeout)
        .timeout_read(read_timeout)
        .build())
}

/// Return the process-wide shared Agent with default timeouts.
///
/// TLS is initialized lazily on first call. If initialization fails the error
/// is cached and returned to every caller — callers must handle it rather
/// than panicking.
pub fn agent() -> Result<&'static ureq::Agent, String> {
    let cached = AGENT.get_or_init(|| build_agent(DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT));
    match cached {
        Ok(a) => Ok(a),
        Err(e) => Err(e.clone()),
    }
}
