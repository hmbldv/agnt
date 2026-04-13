# agnt-macros

Proc-macros for the [`agnt`](https://crates.io/crates/agnt) agent
runtime. Currently provides one attribute: `#[tool]`.

```rust
use agnt_macros::tool;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct AddArgs { a: i64, b: i64 }
#[derive(Serialize)]
struct AddOut { sum: i64 }

/// Add two integers and return their sum.
#[tool]
fn add(args: AddArgs) -> Result<AddOut, String> {
    Ok(AddOut { sum: args.a + args.b })
}

// Generates:
//   pub struct Add;
//   impl agnt_core::TypedTool for Add { const NAME = "add"; ... }
```

The first-line doc comment becomes the tool description. The PascalCase
struct name is derived from the snake_case fn name. The generated impl
forwards to the original function, which is left in place so you can
still call it directly from Rust.

## ⚠️ v0.3.x limitation

The generated `schema()` returns a bare `{"type": "object"}` with no
field metadata — the model cannot see your argument names or types
from the schema alone and must infer them from the description. For
any non-trivial tool, hand-writing a `TypedTool` impl where you
control `schema()` is currently still better UX.

Real JSON Schema derivation (via `schemars`) is planned for v0.4
behind an opt-in `#[tool(schema = schemars)]` attribute.

## Requirements

- Exactly one argument whose type becomes `TypedTool::Args`
- Return type must be `Result<Output, Error>`
- A doc comment is strongly recommended (becomes the description)

See the [flagship `agnt` crate](https://crates.io/crates/agnt) for the
agent runtime this plugs into, or the
[repository](https://github.com/hmbldv/agnt) for the broader project.

## License

Dual-licensed under MIT OR Apache-2.0.
