//! Generated tool: a compiled WASM module that reads JSON from stdin and
//! writes a result to stdout.
//!
//! Each [`ScriptTool`] carries the original Rust source (for evolution and
//! debugging) and the compiled `wasm32-wasip1` bytes. Calling the tool loads
//! the module into wasmtime, hands it a freshly built WASI context with the
//! JSON args piped to stdin, captures stdout, and returns it.
//!
//! No subprocess, no platform-specific sandbox: capabilities default to none
//! and are added explicitly when [`SandboxConfig`] flags them on.

use agnt_core::Tool;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tracing::debug;
use wasmtime::{Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::WasiCtxBuilder;

/// A tool whose body is a compiled WASM module.
pub struct ScriptTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
    /// Original source the module was compiled from (for evolution).
    pub source: String,
    /// Compiled `wasm32-wasip1` bytes.
    pub wasm: Vec<u8>,
    pub sandbox_config: SandboxConfig,
}

/// Capability budget for a single invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Inherit the host network. Currently no-op — WASI preview 1 doesn't
    /// expose sockets through `wasmtime-wasi` — but kept so the config
    /// surface is stable for Phase 2 (preview 2 wasi-sockets).
    pub allow_network: bool,
    pub max_runtime_ms: u64,
    pub max_output_bytes: usize,
    /// Hard memory cap for the WASM linear memory, in bytes.
    pub max_memory_bytes: usize,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            allow_network: false,
            max_runtime_ms: 30_000,
            max_output_bytes: 65_536,
            max_memory_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Combined WASI ctx + per-store memory limiter. Stored as the wasmtime
/// `Store`'s data so both can be referenced through the linker without
/// fighting the borrow checker.
struct Host {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

impl ScriptTool {
    pub fn new(
        name: String,
        description: String,
        schema: Value,
        source: String,
        wasm: Vec<u8>,
    ) -> Self {
        Self {
            name,
            description,
            schema,
            source,
            wasm,
            sandbox_config: SandboxConfig::default(),
        }
    }

    pub fn with_sandbox(mut self, cfg: SandboxConfig) -> Self {
        self.sandbox_config = cfg;
        self
    }

    fn execute(&self, args_json: &str) -> Result<Vec<u8>, String> {
        let mut config = wasmtime::Config::new();
        // Epoch-based interruption gives us a soft deadline that doesn't
        // require spawning a thread per invocation.
        config.epoch_interruption(true);
        config.consume_fuel(false);
        let engine = Engine::new(&config).map_err(|e| format!("engine: {e}"))?;

        let module = Module::from_binary(&engine, &self.wasm)
            .map_err(|e| format!("load module: {e}"))?;

        let stdin = MemoryInputPipe::new(args_json.as_bytes().to_vec());
        let stdout = MemoryOutputPipe::new(self.sandbox_config.max_output_bytes);
        let stderr = MemoryOutputPipe::new(self.sandbox_config.max_output_bytes);

        let wasi = WasiCtxBuilder::new()
            .stdin(stdin)
            .stdout(stdout.clone())
            .stderr(stderr.clone())
            .build_p1();

        let limits: StoreLimits = StoreLimitsBuilder::new()
            .memory_size(self.sandbox_config.max_memory_bytes)
            .build();

        let host = Host { wasi, limits };
        let mut store = Store::new(&engine, host);
        store.limiter(|h| &mut h.limits);
        store.set_epoch_deadline(1);

        let mut linker: Linker<Host> = Linker::new(&engine);
        preview1::add_to_linker_sync(&mut linker, |h: &mut Host| &mut h.wasi)
            .map_err(|e| format!("link wasi: {e}"))?;

        // Spawn a watchdog thread that increments the engine's epoch after
        // `max_runtime_ms`. The wasm execution observes this on the next
        // function-entry/loop-header and traps.
        let watchdog_engine = engine.clone();
        let deadline = Duration::from_millis(self.sandbox_config.max_runtime_ms);
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            if cancel_rx.recv_timeout(deadline).is_err() {
                watchdog_engine.increment_epoch();
            }
        });

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| format!("instantiate: {e}"))?;
        let start = instance
            .get_typed_func::<(), ()>(&mut store, "_start")
            .map_err(|e| format!("missing _start (is this a WASI command?): {e}"))?;

        let exec_result = start.call(&mut store, ());

        // Cancel the watchdog whether we succeeded or not.
        let _ = cancel_tx.send(());
        let _ = watchdog.join();

        if let Err(e) = exec_result {
            // `Trap::Interrupt` becomes part of the chain on epoch deadline.
            let msg = format!("{e:#}");
            if msg.contains("interrupt") || msg.contains("epoch") {
                return Err(format!(
                    "tool timed out after {}ms",
                    self.sandbox_config.max_runtime_ms
                ));
            }
            let stderr_bytes = stderr.contents();
            let stderr_s = String::from_utf8_lossy(&stderr_bytes);
            if stderr_s.is_empty() {
                return Err(format!("tool trapped: {msg}"));
            }
            return Err(format!("tool failed: {msg}; stderr: {stderr_s}"));
        }

        let bytes = stdout.contents().to_vec();
        debug!(tool = %self.name, bytes = bytes.len(), "tool executed");
        Ok(bytes)
    }
}

impl Tool for ScriptTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let args_json = serde_json::to_string(&args).map_err(|e| format!("encode args: {e}"))?;
        let bytes = self.execute(&args_json)?;
        let out = String::from_utf8(bytes)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        let trimmed = out.trim_end().to_string();
        if trimmed.len() > self.sandbox_config.max_output_bytes {
            let cap = self.sandbox_config.max_output_bytes;
            let mut end = cap.min(trimmed.len());
            while end > 0 && !trimmed.is_char_boundary(end) {
                end -= 1;
            }
            Ok(format!("{}... [truncated]", &trimmed[..end]))
        } else {
            Ok(trimmed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_config_plumbs_through_builder() {
        let tool = ScriptTool::new(
            "x".into(),
            "x".into(),
            serde_json::json!({}),
            String::new(),
            Vec::new(),
        )
        .with_sandbox(SandboxConfig {
            max_output_bytes: 16,
            max_runtime_ms: 250,
            max_memory_bytes: 4 * 1024 * 1024,
            allow_network: false,
        });
        assert_eq!(tool.sandbox_config.max_output_bytes, 16);
        assert_eq!(tool.sandbox_config.max_runtime_ms, 250);
        assert_eq!(tool.sandbox_config.max_memory_bytes, 4 * 1024 * 1024);
    }

    #[test]
    fn empty_wasm_call_returns_error_not_panic() {
        // No magic header — wasmtime should reject this cleanly.
        let tool = ScriptTool::new(
            "x".into(),
            "x".into(),
            serde_json::json!({}),
            String::new(),
            Vec::new(),
        );
        let err = tool.call(serde_json::json!({})).unwrap_err();
        assert!(!err.is_empty());
    }
}
