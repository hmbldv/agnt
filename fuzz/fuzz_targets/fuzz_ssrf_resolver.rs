#![no_main]
// Fuzz target: feed arbitrary netloc strings through `SsrfResolver`.
//
// The resolver is the single chokepoint protecting `Fetch` from
// DNS-rebinding / TOCTOU after the v0.3.1 refactor. Mutating the
// `host:port` string (including IPv6 literals, weird separators,
// null bytes, giant hostnames) should never panic — only return
// io::Error.

use libfuzzer_sys::fuzz_target;
use ureq::Resolver;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return; };
    // Clamp length so we don't spend fuzz time on multi-megabyte hostnames.
    if s.len() > 4096 { return; }
    let r = agnt_tools::ssrf::SsrfResolver::new();
    let _ = r.resolve(s);
});
