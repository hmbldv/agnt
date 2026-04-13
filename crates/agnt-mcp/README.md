# agnt-mcp

Synchronous [Model Context Protocol](https://modelcontextprotocol.io)
stdio client for the [`agnt`](https://crates.io/crates/agnt) agent
runtime. Spawns an MCP server as a child process, runs the
`2024-11-05` initialize handshake, and bridges every remote tool into
the native `agnt_core::Tool` trait.

```rust,no_run
use std::sync::{Arc, Mutex};
use agnt_core::Tool;
use agnt_mcp::{McpClient, McpTool};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mut client = McpClient::start("mcp-server-filesystem", &["/tmp"])?;
let tools = client.list_tools()?;
let shared = Arc::new(Mutex::new(client));

let bridged: Vec<Box<dyn Tool>> = tools
    .into_iter()
    .map(|info| Box::new(McpTool::new(Arc::clone(&shared), info)) as Box<dyn Tool>)
    .collect();

// Register each `bridged` tool on your Agent's Registry as usual.
# Ok(()) }
```

## Design notes

- **No async runtime.** The stdout reader runs in a dedicated
  `std::thread`, draining JSON-RPC frames into an `mpsc::Receiver`.
  Requests time out at 30 seconds (`REQUEST_TIMEOUT`).
- **Bounded reader.** v0.3.1 caps the inner reader at `MAX_LINE_BYTES`
  (4 MiB) so a hostile or buggy server cannot OOM the agent with a
  multi-gigabyte line. Overflow surfaces as `McpError::Protocol` and
  closes the stream.
- **Typed errors.** `McpError` splits into `Io`, `Protocol`, `Timeout`,
  and `Closed` so callers can tell DoS from protocol bugs from an
  exited child.
- **Off by default in the flagship crate.** `cargo add agnt --features mcp`
  pulls this in; without the feature the flagship stays lean.

See the [flagship `agnt` crate](https://crates.io/crates/agnt) for the
agent runtime this plugs into.

## License

Dual-licensed under MIT OR Apache-2.0.
