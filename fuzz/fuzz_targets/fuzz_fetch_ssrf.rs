#![no_main]
//! Fuzz the SSRF guard on the `Fetch` tool. The private `ssrf_check` function
//! is exercised indirectly through the public `Tool::call` path: we hand the
//! fuzz input in as a `url` argument and confirm no panic is reachable.
//!
//! NOTE: this target intentionally does NOT assert reject-vs-accept. The
//! libfuzzer harness enforces a per-run time budget; a rare accidental
//! successful fetch would still be counted as "no crash" here. For semantic
//! correctness tests see the unit tests in `agnt-tools::builtins`.

use agnt_core::Tool;
use agnt_tools::Fetch;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    // Bound the input size — Fetch::call feeds this to url::Url::parse which
    // is O(n) but libfuzzer can still waste budget on megabyte-sized strings.
    if s.len() > 4096 {
        return;
    }
    let fetch = Fetch::new();
    let _ = fetch.call(serde_json::json!({ "url": s }));
});
