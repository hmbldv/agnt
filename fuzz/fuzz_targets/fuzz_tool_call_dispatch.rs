#![no_main]
//! Fuzz `Registry::dispatch` with arbitrary JSON arg payloads against a
//! dummy typed tool. Exercises the `ErasedAdapter` deserialize-call-serialize
//! path with adversarial input — the JSON parser, the schema-less boundary,
//! and the error string flattening.

use agnt_core::{Registry, Tool, TypedTool};
use libfuzzer_sys::fuzz_target;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Args {
    #[allow(dead_code)]
    a: i64,
    #[allow(dead_code)]
    b: i64,
}

#[derive(Serialize)]
struct Out {
    sum: i64,
}

struct Add;
impl TypedTool for Add {
    type Args = Args;
    type Output = Out;
    type Error = String;
    const NAME: &'static str = "add";
    const DESCRIPTION: &'static str = "Add two integers.";
    fn schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "a": {"type": "integer"},
                "b": {"type": "integer"}
            },
            "required": ["a", "b"]
        })
    }
    fn call(&self, args: Args) -> Result<Out, String> {
        Ok(Out { sum: args.a.wrapping_add(args.b) })
    }
}

// Also register a raw erased tool whose `call` echoes its args back, so the
// dispatch path itself is exercised even when JSON deserialization would
// fail on the typed tool.
struct Echo;
impl Tool for Echo {
    fn name(&self) -> &str { "echo" }
    fn description(&self) -> &str { "Echo raw JSON" }
    fn schema(&self) -> Value { serde_json::json!({"type": "object"}) }
    fn call(&self, args: Value) -> Result<String, String> {
        Ok(args.to_string())
    }
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| {
        let mut r = Registry::new();
        r.register_typed(Add);
        r.register(Box::new(Echo));
        r
    })
}

fuzz_target!(|data: &[u8]| {
    // Treat the fuzz input as a JSON payload. Skip non-UTF8 and oversize.
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    if s.len() > 8192 {
        return;
    }
    let parsed: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(_) => Value::Null,
    };

    // Pick a tool name from the first byte so both branches are exercised.
    let name = match data.first().copied().unwrap_or(0) % 3 {
        0 => "add",
        1 => "echo",
        _ => "nonexistent",
    };

    let _ = registry().dispatch(name, parsed);
});
