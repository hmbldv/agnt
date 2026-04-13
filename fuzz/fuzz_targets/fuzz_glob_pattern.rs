#![no_main]
//! Fuzz the `Glob` tool with pathological glob patterns. Targets the
//! `glob` crate's parser plus the sandbox-relative-path logic in
//! `agnt_tools::builtins::Glob::call`.

use agnt_core::Tool;
use agnt_tools::{FilesystemRoot, Glob};
use libfuzzer_sys::fuzz_target;
use std::sync::{Arc, OnceLock};

static SBX: OnceLock<Arc<FilesystemRoot>> = OnceLock::new();

fn sandbox() -> Arc<FilesystemRoot> {
    SBX.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "agnt-fuzz-glob-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create fuzz glob sandbox");
        Arc::new(FilesystemRoot::new(dir).expect("canonicalize fuzz glob sandbox"))
    })
    .clone()
}

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    if s.len() > 2048 {
        return;
    }
    // Exercise both sandboxed and unsandboxed paths.
    let unsandboxed = Glob::new();
    let _ = unsandboxed.call(serde_json::json!({ "pattern": s }));

    let sandboxed = Glob::with_sandbox(sandbox());
    let _ = sandboxed.call(serde_json::json!({ "pattern": s }));
});
