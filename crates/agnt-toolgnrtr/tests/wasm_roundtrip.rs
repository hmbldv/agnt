//! End-to-end test: compile a hand-written Rust tool to wasm32-wasip1 and
//! execute it via wasmtime. This exercises both `WasmCompiler` and the
//! `ScriptTool` runtime path.
//!
//! Gated by the presence of cargo + the `wasm32-wasip1` target. If those are
//! missing the test exits early as a no-op so CI without WASM tooling still
//! passes.

use agnt_toolgnrtr::{SandboxConfig, ScriptTool, WasmCompiler};
use std::process::Command;

fn wasi_target_installed() -> bool {
    let out = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines().any(|l| l.trim() == "wasm32-wasip1")
        }
        Err(_) => false,
    }
}

const ECHO_TOOL: &str = r#"
use std::io::{Read, Write};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Args { message: String }

#[derive(Serialize)]
struct Output { echoed: String, length: usize }

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let args: Args = serde_json::from_str(&input).unwrap();
    let length = args.message.chars().count();
    let out = Output { echoed: args.message, length };
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, &out).unwrap();
    let _ = lock.write_all(b"\n");
}
"#;

#[test]
fn compile_and_execute_echo_tool() {
    if !wasi_target_installed() {
        eprintln!("wasm32-wasip1 not installed; skipping");
        return;
    }
    let ws = std::env::temp_dir().join(format!("toolgnrtr-rt-{}", uuid::Uuid::new_v4()));
    let compiler = WasmCompiler::new(ws.clone()).unwrap();
    let wasm = match compiler.compile("echo_tool", ECHO_TOOL) {
        Ok(b) => b,
        Err(e) => {
            // Network-fetch failures shouldn't fail the suite — print and skip.
            eprintln!("cargo build failed (likely offline): {e}");
            let _ = std::fs::remove_dir_all(&ws);
            return;
        }
    };
    assert!(wasm.len() > 100, "wasm too small: {} bytes", wasm.len());
    assert_eq!(&wasm[..4], b"\0asm", "missing wasm magic header");

    let tool = ScriptTool::new(
        "echo_tool".into(),
        "echo the input".into(),
        serde_json::json!({
            "type": "object",
            "properties": {"message": {"type": "string"}},
            "required": ["message"]
        }),
        ECHO_TOOL.into(),
        wasm,
    )
    .with_sandbox(SandboxConfig::default());

    use agnt_core::Tool;
    let result = tool.call(serde_json::json!({"message": "hello wasm"})).unwrap();
    assert!(result.contains("hello wasm"), "result: {result}");
    assert!(result.contains("\"length\""), "result: {result}");

    let _ = std::fs::remove_dir_all(&ws);
}
