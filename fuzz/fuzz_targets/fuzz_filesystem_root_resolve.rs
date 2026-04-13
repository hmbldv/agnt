#![no_main]
//! Fuzz `FilesystemRoot::resolve` — the primary path-safety boundary between
//! hostile LLM tool calls and the host filesystem. Treats fuzz input as a
//! UTF-8 path string; asserts the function never panics, regardless of input.

use agnt_tools::FilesystemRoot;
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

static ROOT: OnceLock<FilesystemRoot> = OnceLock::new();

fn root() -> &'static FilesystemRoot {
    ROOT.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "agnt-fuzz-sandbox-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create fuzz sandbox");
        FilesystemRoot::new(dir).expect("canonicalize fuzz sandbox")
    })
}

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    // Discard the result — we only care that resolve() does not panic.
    let _ = root().resolve(s);
});
