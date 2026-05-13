//! LLM-driven Rust source generation for tools that will be compiled to WASM.
//!
//! The generator asks the configured backend for a JSON spec containing a
//! Rust source file. The source MUST compile against the workspace's
//! `Cargo.toml` (serde + serde_json, edition 2021) and target
//! `wasm32-wasip1`. At runtime, the compiled binary reads JSON args from
//! stdin and writes a result to stdout.

use agnt_core::Message;
use agnt_net::Backend;
use serde::Deserialize;
use serde_json::{json, Value};

const SYSTEM_PROMPT: &str = r#"You are a tool generator. The user describes a capability they need. You produce a SINGLE Rust source file that will be compiled to wasm32-wasip1 and executed in a wasmtime sandbox.

Output format:
- Reply with a SINGLE JSON object and nothing else (no prose, no markdown fences).
- Fields:
  - "name": short snake_case identifier matching /^[a-z][a-z0-9_]*$/ (no leading underscore)
  - "description": one sentence describing what the tool does
  - "schema": JSON Schema object describing the args dict
  - "source": full Rust source of the tool (see template below)
  - "test_input": a sample JSON object satisfying the schema, used for smoke-testing

Runtime contract (the source MUST follow this template):
```rust
use std::io::{Read, Write};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Args { /* tool-specific fields */ }

#[derive(Serialize)]
struct Output { /* tool-specific fields */ }

fn main() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("failed to read stdin");
        std::process::exit(1);
    }
    let args: Args = match serde_json::from_str(&input) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("invalid args: {e}");
            std::process::exit(1);
        }
    };
    let output: Output = run(args);
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, &output).ok();
    let _ = lock.write_all(b"\n");
}

fn run(args: Args) -> Output {
    // <implementation>
}
```

Constraints:
- Allowed crates: `std`, `serde`, `serde_json`. NO other dependencies.
- No `unsafe`. No `std::process::Command`. No FFI.
- The WASI sandbox provides stdin/stdout/stderr only — no filesystem, no network.
- Keep it small. Most tools are <100 lines.
"#;

const EVOLVE_PROMPT: &str = r#"You are revising an existing Rust tool. The user gives you the previous source plus feedback (a compile error, a test failure, or a behavior change). Produce a new version that addresses the feedback while preserving the tool's intent.

Output rules are identical to initial generation: a SINGLE JSON object with fields name, description, schema, source, test_input. Keep the `name` stable across versions unless the user explicitly asks to rename.
"#;

#[derive(Debug, Deserialize)]
struct LlmToolSpec {
    name: String,
    description: String,
    schema: Value,
    source: String,
    #[serde(default)]
    test_input: Option<Value>,
}

/// The parsed output of a generation step. The Rust source has not yet been
/// compiled — call into [`crate::wasm_compile::WasmCompiler`] to produce
/// the wasm bytes.
#[derive(Debug, Clone)]
pub struct GeneratedSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub source: String,
    pub test_input: Option<Value>,
}

pub struct Generator<'a> {
    backend: &'a Backend,
}

impl<'a> Generator<'a> {
    pub fn new(backend: &'a Backend) -> Self {
        Self { backend }
    }

    pub fn generate(&self, description: &str) -> Result<GeneratedSpec, String> {
        let messages = vec![
            Message {
                role: "system".into(),
                content: Some(SYSTEM_PROMPT.into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            Message {
                role: "user".into(),
                content: Some(description.into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        ];
        let reply = self
            .backend
            .chat(&messages, &json!([]), None)
            .map_err(|e| format!("backend chat: {e}"))?;
        let raw = reply
            .content
            .ok_or_else(|| "model returned no content".to_string())?;
        let spec = parse_spec(&raw)?;
        validate_name(&spec.name)?;
        Ok(GeneratedSpec {
            name: spec.name,
            description: spec.description,
            schema: spec.schema,
            source: spec.source,
            test_input: spec.test_input,
        })
    }

    pub fn evolve(
        &self,
        previous_name: &str,
        previous_source: &str,
        feedback: &str,
    ) -> Result<GeneratedSpec, String> {
        let user_msg = format!(
            "Previous tool name: {previous_name}\nPrevious source:\n```rust\n{previous_source}\n```\n\nFeedback:\n{feedback}\n\nProduce the revised tool as JSON."
        );
        let messages = vec![
            Message {
                role: "system".into(),
                content: Some(EVOLVE_PROMPT.into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            Message {
                role: "user".into(),
                content: Some(user_msg),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        ];
        let reply = self
            .backend
            .chat(&messages, &json!([]), None)
            .map_err(|e| format!("backend chat: {e}"))?;
        let raw = reply
            .content
            .ok_or_else(|| "model returned no content".to_string())?;
        let spec = parse_spec(&raw)?;
        validate_name(&spec.name)?;
        Ok(GeneratedSpec {
            name: spec.name,
            description: spec.description,
            schema: spec.schema,
            source: spec.source,
            test_input: spec.test_input,
        })
    }
}

fn parse_spec(raw: &str) -> Result<LlmToolSpec, String> {
    let body = strip_fences(raw.trim());
    if let Ok(spec) = serde_json::from_str::<LlmToolSpec>(body) {
        return Ok(spec);
    }
    if let (Some(start), Some(end)) = (body.find('{'), body.rfind('}')) {
        if end > start {
            let slice = &body[start..=end];
            return serde_json::from_str::<LlmToolSpec>(slice)
                .map_err(|e| format!("parse model JSON: {e}; body: {body}"));
        }
    }
    Err(format!("no JSON object found in model output: {raw}"))
}

fn strip_fences(s: &str) -> &str {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        return rest.trim_end_matches("```").trim();
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        return rest.trim_end_matches("```").trim();
    }
    trimmed
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("tool name is empty".into());
    }
    if name.starts_with('_') {
        return Err("tool name must not start with underscore".into());
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() {
        return Err(format!("tool name must start with a lowercase letter: {name}"));
    }
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return Err(format!("tool name has invalid char {c:?}: {name}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fences_handles_plain_and_fenced() {
        assert_eq!(strip_fences("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("```\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    #[test]
    fn parse_spec_accepts_raw_json() {
        let raw = r#"{"name":"x","description":"y","schema":{},"source":"fn main(){}","test_input":{"a":1}}"#;
        let spec = parse_spec(raw).unwrap();
        assert_eq!(spec.name, "x");
        assert!(spec.test_input.is_some());
    }

    #[test]
    fn parse_spec_recovers_from_preamble() {
        let raw = r#"Sure!
```json
{"name":"x","description":"y","schema":{},"source":"fn main(){}"}
```
"#;
        let spec = parse_spec(raw).unwrap();
        assert_eq!(spec.name, "x");
    }

    #[test]
    fn validate_name_rejects_bad_inputs() {
        assert!(validate_name("").is_err());
        assert!(validate_name("Foo").is_err());
        assert!(validate_name("9foo").is_err());
        assert!(validate_name("foo-bar").is_err());
        assert!(validate_name("_x").is_err());
        assert!(validate_name("foo_bar9").is_ok());
    }
}
