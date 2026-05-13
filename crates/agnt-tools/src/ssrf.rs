//! Atomic SSRF-guarded DNS resolver for the `Fetch` tool.
//!
//! ## Why this exists
//!
//! v0.2 shipped a two-phase SSRF guard: `ssrf_check` called `ToSocketAddrs`,
//! validated the returned IPs, and then handed the raw URL to `ureq`. The
//! problem — first identified in v0.3's adversarial review — is that `ureq`
//! does its **own** DNS lookup when it actually makes the request. A hostile
//! authority with a short TTL can return a safe public IP at check time and
//! flip the record to `169.254.169.254` (or any RFC1918 address) before the
//! second lookup lands. Classic DNS rebinding. Classic TOCTOU.
//!
//! ## The fix
//!
//! `ureq::AgentBuilder::resolver` installs a custom [`ureq::Resolver`] that
//! is the *only* DNS path the agent uses. If we validate inside the resolver
//! itself and return the validated `SocketAddr`s directly, `ureq` connects
//! to those exact addresses — no second lookup, no gap. Atomic.
//!
//! That is what [`SsrfResolver`] does:
//!
//! 1. Parse the netloc (`host:port`).
//! 2. Reject the metadata hostname blocklist.
//! 3. If an explicit `allow_hosts` list is configured, reject non-members.
//! 4. Resolve once.
//! 5. Reject any returned IP in the loopback / private / link-local /
//!    broadcast / unspecified / multicast ranges, plus the explicit AWS
//!    metadata IP (`169.254.169.254`) as belt-and-suspenders on top of
//!    `is_link_local`.
//! 6. Return the survivors. `ureq` uses *these* addresses.
//!
//! The old `ssrf_check` wrapper still exists in `builtins.rs` for the
//! upfront scheme / URL-shape check — the resolver can't see the scheme,
//! only the netloc. Splitting responsibilities keeps each layer minimal.
//!
//! ## What this still does not defend against
//!
//! - **Dual-stack trickery.** If a host resolves to both an IPv4 and an
//!   IPv6 address, and one is public and one is private, this rejects the
//!   whole batch — the old implementation did the same. Correct behavior
//!   but worth noting.
//! - **IPv6 ULA boundaries.** `std::net::Ipv6Addr::is_private` is still
//!   unstable on the crate's MSRV (1.75), so we hand-check the `fc00::/7`
//!   and `fe80::/10` prefixes. If a future IPv6 reservation lands outside
//!   those, it won't be blocked.
//! - **Public-but-sensitive internal APIs.** If you run a production service
//!   on a public IP you own, SSRF-by-IP alone won't save you. Use
//!   `allow_hosts` as the positive gate for those cases.

use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

/// Lowercased host names that must never be resolved, regardless of what
/// DNS would return. Covers the GCP metadata alias that resolves to a
/// public-looking IP from outside a VM but exposes credentials from inside.
const METADATA_HOST_BLOCKLIST: &[&str] = &[
    "metadata.google.internal",
    "metadata",
    "metadata.goog",
    // AWS IMDS IP written as a hostname — belt-and-suspenders against a
    // clever URL parser.
    "169.254.169.254",
];

/// A [`ureq::Resolver`] that performs DNS and SSRF validation atomically.
///
/// Install on a `ureq::Agent` via `AgentBuilder::resolver` and ureq will
/// call [`SsrfResolver::resolve`] exactly once per connection attempt,
/// using whatever socket addresses we return. There is no second lookup
/// inside `ureq`, so a DNS rebinding flip between check and use is
/// structurally impossible.
#[derive(Debug, Clone, Default)]
pub struct SsrfResolver {
    /// Optional positive allowlist. When `Some`, every host must match
    /// (case-insensitive) or the resolver rejects with `PermissionDenied`.
    /// Compared *before* DNS so the agent never issues a lookup for a
    /// rejected host.
    pub allow_hosts: Option<Vec<String>>,
}

impl SsrfResolver {
    /// Build a resolver with no allowlist (all hosts pass except the
    /// metadata blocklist and private IP ranges).
    pub fn new() -> Self {
        Self { allow_hosts: None }
    }

    /// Build a resolver with an explicit host allowlist.
    pub fn with_allow_hosts(hosts: Vec<String>) -> Self {
        Self {
            allow_hosts: Some(hosts.into_iter().map(|h| h.to_lowercase()).collect()),
        }
    }

    /// Standalone validation for a list of resolved addresses. Exposed so
    /// tests can exercise the decision logic without going through a ureq
    /// agent, and so `Fetch::call` can reuse the same predicate for the
    /// early scheme/shape check.
    pub fn validate_addrs(host: &str, addrs: &[SocketAddr]) -> io::Result<()> {
        if addrs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no addresses for {}", host),
            ));
        }
        for sa in addrs {
            let ip = sa.ip();
            if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("rejected IP {} for {}", ip, host),
                ));
            }
            match ip {
                IpAddr::V4(v4) => {
                    if v4.is_private() || v4.is_link_local() || v4.is_broadcast() {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!("rejected IPv4 {} for {}", v4, host),
                        ));
                    }
                    // 169.254.169.254 is already caught by is_link_local;
                    // explicit match documents the intent and guards against
                    // std ever dropping link-local from the check.
                    if v4.octets() == [169, 254, 169, 254] {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!("rejected AWS metadata IP for {}", host),
                        ));
                    }
                }
                IpAddr::V6(v6) => {
                    let seg0 = v6.segments()[0];
                    // fc00::/7 (ULA) and fe80::/10 (link-local).
                    if (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80 {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!("rejected IPv6 {} for {}", v6, host),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

impl ureq::Resolver for SsrfResolver {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        let (raw_host, _) = netloc.rsplit_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad netloc: {}", netloc),
            )
        })?;
        // IPv6 literals arrive bracketed: "[::1]:443". Strip for comparison.
        let host = raw_host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_lowercase();

        if METADATA_HOST_BLOCKLIST.iter().any(|&h| h == host) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("rejected metadata host: {}", host),
            ));
        }

        if let Some(allow) = &self.allow_hosts {
            if !allow.iter().any(|h| h == &host) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("host {} not in allowlist", host),
                ));
            }
        }

        let addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
        Self::validate_addrs(&host, &addrs)?;
        Ok(addrs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ureq::Resolver;

    #[test]
    fn rejects_metadata_host_before_dns() {
        let r = SsrfResolver::new();
        let err = r.resolve("metadata.google.internal:80").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("metadata"));
    }

    #[test]
    fn rejects_aws_metadata_ip_as_hostname() {
        let r = SsrfResolver::new();
        let err = r.resolve("169.254.169.254:80").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn rejects_non_allowlist_host_before_dns() {
        let r = SsrfResolver::with_allow_hosts(vec!["example.com".into()]);
        let err = r.resolve("not-on-list.invalid:80").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("allowlist"));
    }

    #[test]
    fn validate_addrs_rejects_loopback() {
        let sa: SocketAddr = "127.0.0.1:80".parse().unwrap();
        let err = SsrfResolver::validate_addrs("localhost", &[sa]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn validate_addrs_rejects_private_ipv4() {
        let sa: SocketAddr = "10.0.0.5:80".parse().unwrap();
        let err = SsrfResolver::validate_addrs("internal.corp", &[sa]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("IPv4"));
    }

    #[test]
    fn validate_addrs_rejects_link_local_ipv4() {
        let sa: SocketAddr = "169.254.169.254:80".parse().unwrap();
        let err = SsrfResolver::validate_addrs("anywhere", &[sa]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn validate_addrs_rejects_ipv6_ula() {
        let sa: SocketAddr = "[fc00::1]:80".parse().unwrap();
        let err = SsrfResolver::validate_addrs("anywhere", &[sa]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn validate_addrs_rejects_ipv6_link_local() {
        let sa: SocketAddr = "[fe80::1]:80".parse().unwrap();
        let err = SsrfResolver::validate_addrs("anywhere", &[sa]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn validate_addrs_rejects_empty_list() {
        let err = SsrfResolver::validate_addrs("anywhere", &[]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn validate_addrs_accepts_public_ipv4() {
        let sa: SocketAddr = "93.184.216.34:80".parse().unwrap(); // example.com
        SsrfResolver::validate_addrs("example.com", &[sa]).unwrap();
    }

    #[test]
    fn validate_addrs_rejects_batch_if_any_private() {
        // Dual-stack case where one address is public and one is private.
        // We reject the whole batch — better safe than routing-dependent.
        let public: SocketAddr = "93.184.216.34:80".parse().unwrap();
        let private: SocketAddr = "10.0.0.1:80".parse().unwrap();
        let err = SsrfResolver::validate_addrs("dual.example", &[public, private]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn ipv6_literal_netloc_strips_brackets() {
        let r = SsrfResolver::new();
        // ::1 is loopback — this should get rejected by the IP check, not
        // misparsed as a weird hostname.
        let err = r.resolve("[::1]:80").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }
}
