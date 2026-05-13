//! Integration tests for the `#[tool]` proc-macro.
//!
//! These are plain `#[test]` functions — no `trybuild`. They verify that the
//! macro expansion produces a struct implementing `agnt_core::TypedTool`
//! correctly, including name/description/schema/call semantics.

use agnt_core::{Registry, TypedTool};
use agnt_macros::tool;
use serde::{Deserialize, Serialize};

// ---------- fixture 1: the canonical `add` example ----------

#[derive(Deserialize)]
struct AddArgs {
    a: i64,
    b: i64,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
struct AddOut {
    sum: i64,
}

/// Add two integers and return their sum.
#[tool]
fn add(args: AddArgs) -> Result<AddOut, String> {
    Ok(AddOut {
        sum: args.a + args.b,
    })
}

#[test]
fn add_name_and_description() {
    assert_eq!(<Add as TypedTool>::NAME, "add");
    assert_eq!(
        <Add as TypedTool>::DESCRIPTION,
        "Add two integers and return their sum."
    );
}

#[test]
fn add_schema_is_placeholder_object() {
    let schema = <Add as TypedTool>::schema();
    assert_eq!(schema, serde_json::json!({ "type": "object" }));
}

#[test]
fn add_call_roundtrips_typed() {
    let out = Add.call(AddArgs { a: 2, b: 3 }).unwrap();
    assert_eq!(out, AddOut { sum: 5 });
}

#[test]
fn add_registers_into_registry_and_dispatches() {
    let mut reg = Registry::new();
    reg.register_typed(Add);
    let out = reg
        .dispatch("add", serde_json::json!({"a": 10, "b": 20}))
        .unwrap();
    assert_eq!(out, r#"{"sum":30}"#);
}

// ---------- fixture 2: snake_case name → PascalCase struct ----------

#[derive(Deserialize)]
struct ReadFileArgs {
    path: String,
}

#[derive(Serialize)]
struct ReadFileOut {
    bytes: usize,
}

/// Read a file at the given path.
/// Returns the number of bytes read (mocked).
#[tool]
fn read_file(args: ReadFileArgs) -> Result<ReadFileOut, String> {
    Ok(ReadFileOut {
        bytes: args.path.len(),
    })
}

#[test]
fn read_file_has_pascal_struct_name() {
    // If the struct wasn't generated as `ReadFile`, this wouldn't compile.
    assert_eq!(<ReadFile as TypedTool>::NAME, "read_file");
    // Multi-line doc comments are joined with spaces.
    assert_eq!(
        <ReadFile as TypedTool>::DESCRIPTION,
        "Read a file at the given path. Returns the number of bytes read (mocked)."
    );
}

#[test]
fn read_file_calls_through() {
    let out = ReadFile
        .call(ReadFileArgs {
            path: "hello".to_string(),
        })
        .unwrap();
    assert_eq!(out.bytes, 5);
}

// ---------- fixture 3: missing doc comment falls back to fn name ----------

#[derive(Deserialize)]
struct NoopArgs {}

#[derive(Serialize)]
struct NoopOut {}

#[tool]
fn noop(_args: NoopArgs) -> Result<NoopOut, String> {
    Ok(NoopOut {})
}

#[test]
fn noop_description_falls_back_to_fn_name() {
    assert_eq!(<Noop as TypedTool>::NAME, "noop");
    assert_eq!(<Noop as TypedTool>::DESCRIPTION, "noop");
}

// ---------- fixture 4: custom error type (not just String) ----------

#[derive(Debug)]
struct MyErr(&'static str);
impl std::fmt::Display for MyErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Deserialize)]
struct DivArgs {
    a: i64,
    b: i64,
}

#[derive(Serialize)]
struct DivOut {
    quot: i64,
}

/// Divide a by b.
#[tool]
fn divide(args: DivArgs) -> Result<DivOut, MyErr> {
    if args.b == 0 {
        Err(MyErr("division by zero"))
    } else {
        Ok(DivOut {
            quot: args.a / args.b,
        })
    }
}

#[test]
fn divide_error_path_flows_through_erased_adapter() {
    let mut reg = Registry::new();
    reg.register_typed(Divide);
    let err = reg
        .dispatch("divide", serde_json::json!({"a": 1, "b": 0}))
        .unwrap_err();
    assert!(err.contains("division by zero"), "got: {}", err);
}
