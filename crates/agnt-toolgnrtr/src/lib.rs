//! # agnt-toolgnrtr
//!
//! Phase 1 tool factory: ask an LLM for Rust source, compile it to
//! `wasm32-wasip1` via cargo, version the artifact in SQLite, and execute it
//! through wasmtime with WASI preview 1.
//!
//! ## Execution model
//!
//! - The generated tool is a normal WASI command. It reads JSON args from
//!   stdin and writes a JSON (or string) result to stdout.
//! - wasmtime runs the module with stdin/stdout/stderr piped through
//!   in-memory buffers. There is no filesystem and no network — capabilities
//!   default to none and the [`SandboxConfig`] surface is the only knob.
//! - Wall-clock deadlines are enforced via wasmtime's epoch interruption.
//!
//! ## High-level flow
//!
//! 1. [`ToolGenerator::generate`] asks the LLM for a JSON spec containing a
//!    Rust source file.
//! 2. [`crate::wasm_compile::WasmCompiler`] writes the source into the
//!    persistent build workspace and runs `cargo build --release --target
//!    wasm32-wasip1 --bin <name>`.
//! 3. The resulting `.wasm` bytes are persisted alongside the source.
//! 4. [`ToolGenerator::test`] reloads the module, calls it, and records
//!    success/failure in the `tool_calls` table.

pub mod generator;
pub mod script_tool;
pub mod store;
pub mod wasm_compile;

pub use generator::{GeneratedSpec, Generator};
pub use script_tool::{SandboxConfig, ScriptTool};
pub use store::{TestCase, ToolRecord, ToolStats, ToolStore, ToolSummary};
pub use wasm_compile::WasmCompiler;

use agnt_core::Tool;
use agnt_net::Backend;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Instant;

/// Top-level API combining generation, compilation, persistence, and dispatch.
pub struct ToolGenerator {
    backend: Backend,
    store: ToolStore,
    compiler: WasmCompiler,
}

impl ToolGenerator {
    /// Open the store at `db_path` and use the default build workspace
    /// (`~/.cache/agnt-toolgnrtr/build/`).
    pub fn new(backend: Backend, db_path: &str) -> Result<Self, String> {
        let workspace = WasmCompiler::default_workspace()?;
        Self::with_workspace(backend, db_path, workspace)
    }

    pub fn with_workspace(
        backend: Backend,
        db_path: &str,
        workspace: PathBuf,
    ) -> Result<Self, String> {
        let store = ToolStore::open(db_path)?;
        let compiler = WasmCompiler::new(workspace)?;
        Ok(Self {
            backend,
            store,
            compiler,
        })
    }

    pub fn store(&self) -> &ToolStore {
        &self.store
    }

    pub fn compiler(&self) -> &WasmCompiler {
        &self.compiler
    }

    /// Generate, compile, and persist a tool. Returns the runnable
    /// [`ScriptTool`].
    pub fn generate(
        &self,
        description: &str,
        sandbox: SandboxConfig,
    ) -> Result<ScriptTool, String> {
        let gen = Generator::new(&self.backend);
        let spec = gen.generate(description)?;
        let wasm = self.compiler.compile(&spec.name, &spec.source)?;
        let tool = ScriptTool::new(
            spec.name.clone(),
            spec.description,
            spec.schema,
            spec.source,
            wasm,
        )
        .with_sandbox(sandbox);
        let version = self.store.save_tool(&tool)?;
        if let Some(input) = spec.test_input {
            let _ = self
                .store
                .save_test_case(&spec.name, version, &input, None, None);
        }
        Ok(tool)
    }

    /// Compile and persist a tool from a known Rust source — used to register
    /// hand-written tools or test fixtures without the LLM.
    pub fn compile_and_save(
        &self,
        name: &str,
        description: &str,
        schema: Value,
        source: &str,
        sandbox: SandboxConfig,
    ) -> Result<ScriptTool, String> {
        let wasm = self.compiler.compile(name, source)?;
        let tool = ScriptTool::new(
            name.to_string(),
            description.to_string(),
            schema,
            source.to_string(),
            wasm,
        )
        .with_sandbox(sandbox);
        self.store.save_tool(&tool)?;
        Ok(tool)
    }

    pub fn evolve(&self, tool_name: &str, feedback: &str) -> Result<ScriptTool, String> {
        let prev = self
            .store
            .load_tool(tool_name)?
            .ok_or_else(|| format!("no such tool: {tool_name}"))?;
        let sandbox = prev.sandbox_config.clone();
        let gen = Generator::new(&self.backend);
        let spec = gen.evolve(&prev.name, &prev.source, feedback)?;
        let wasm = self.compiler.compile(&spec.name, &spec.source)?;
        let tool = ScriptTool::new(
            spec.name,
            spec.description,
            spec.schema,
            spec.source,
            wasm,
        )
        .with_sandbox(sandbox);
        self.store.save_tool(&tool)?;
        Ok(tool)
    }

    pub fn search(&self, query: &str) -> Result<Vec<ToolSummary>, String> {
        self.store.search(query)
    }

    pub fn list(&self) -> Result<Vec<ToolSummary>, String> {
        self.store.list()
    }

    pub fn test(&self, tool_name: &str, input: Value) -> Result<String, String> {
        let rec = self
            .store
            .load_tool(tool_name)?
            .ok_or_else(|| format!("no such tool: {tool_name}"))?;
        let version = rec.version;
        let tool = ToolStore::record_to_tool(rec);
        let started = Instant::now();
        let result = tool.call(input.clone());
        let elapsed_us = started.elapsed().as_micros() as u64;
        let (ok, payload) = match &result {
            Ok(s) => (true, s.clone()),
            Err(e) => (false, e.clone()),
        };
        let _ = self
            .store
            .record_call(tool_name, version, &input, &payload, ok, elapsed_us);
        result
    }

    pub fn stats(&self, tool_name: Option<&str>) -> Result<ToolStats, String> {
        self.store.stats(tool_name)
    }

    /// Return the latest version of every stored tool as boxed [`Tool`]
    /// objects ready to register into an [`agnt_core::Registry`].
    pub fn into_tools(&self) -> Result<Vec<Box<dyn Tool>>, String> {
        let summaries = self.store.list()?;
        let mut out: Vec<Box<dyn Tool>> = Vec::with_capacity(summaries.len());
        for s in summaries {
            if let Some(rec) = self.store.load_tool(&s.name)? {
                out.push(Box::new(ToolStore::record_to_tool(rec)));
            }
        }
        Ok(out)
    }
}
