# agnt-core

**The zero-I/O kernel of the [agnt](https://crates.io/crates/agnt) agent runtime.**

Defines the message types, tool trait, backend abstraction, persistence
abstraction, observer hooks, and the synchronous agent loop itself — with
no HTTP, no SQLite, and no async runtime dependencies.

```toml
[dependencies]
agnt-core = "0.2"
```

## When to depend on this

Most users should `cargo add agnt` instead — the flagship crate pulls in
`agnt-core` plus `agnt-net`, `agnt-store`, and `agnt-tools` as optional
features and gives you a working agent runtime.

Depend directly on `agnt-core` when:

- You want the minimum possible dependency footprint (only `serde`,
  `serde_json`, `tracing`)
- You're targeting WASM — `agnt-core` has no filesystem, TLS, or
  process-spawning deps that would block `wasm32-*` builds
- You're bringing your own backend (implementing [`LlmBackend`]) and your
  own persistence (implementing [`MessageStore`])
- You're embedding the agent loop into a larger system with pre-existing
  HTTP and persistence layers

## What it provides

- [`Agent<B: LlmBackend>`] — the core loop (message → inference → parallel
  tool dispatch → loop)
- [`AgentBuilder`] — fluent construction
- [`Tool`] (erased) and [`TypedTool`] (typed `Args`/`Output`/`Error`) traits,
  with [`ErasedAdapter`] bridging typed impls into the erased dispatch path
- [`Registry`] — name-based tool dispatch
- [`Message`], [`ToolCall`], [`FunctionCall`] — the internal wire format
- [`LlmBackend`] and [`BackendError`] — trait any backend implements
- [`MessageStore`], [`StoreError`], [`ToolLog`] — trait any store implements
- [`Observer`] — lifecycle hook trait for HITL approval, audit, metrics
- `tracing` instrumentation at `agnt.step`, `agnt.backend.chat`, `agnt.tool`
  span boundaries

## Security

See the [v0.2 threat model](https://github.com/hmbldv/agnt/blob/main/THREAT_MODEL.md).
`agnt-core` handles tool output envelope framing (`<tool_output>`) and
enforces the 64KB per-result byte cap. Filesystem sandboxing and SSRF
guards live in `agnt-tools`.

## License

Dual-licensed under MIT OR Apache-2.0.
