# Changelog

All notable changes to `agnt` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] — 2026-04-12

Ergonomics + extensibility release. No new Critical-severity findings; v0.3
is additive over v0.2 and existing code keeps compiling. Two new crates join
the workspace, and the agent loop gains per-tool quotas plus an observer
veto hook so embedders can enforce policy without forking the dispatch path.

### ✨ New

- **`agnt-macros` crate — `#[tool]` attribute macro.** Turns an ordinary
  `fn` into a `TypedTool` impl, lifting the first-line doc comment into the
  tool description and generating a PascalCase wrapper struct. Behind the
  `macros` feature on the flagship crate (on by default). Schema generation
  is a placeholder object in v0.3; schemars-derived JSON Schema is planned
  for v0.4.
- **`agnt-mcp` crate — MCP stdio client.** Spawns an MCP server subprocess,
  runs the `2024-11-05` protocol handshake, and bridges each remote tool
  into the existing `agnt_core::Tool` trait via `McpTool`. No async
  runtime; the reader is a dedicated std thread draining stdout into an
  `mpsc` channel with a 30-second request timeout. Behind the `mcp`
  feature (off by default — `cargo add agnt --features mcp`).
- **`ToolQuota` — per-tool rate limits inside a single `step()`.** Cap
  `max_calls`, `max_duration_us`, and `max_result_bytes` per tool name.
  Quota violations become refused tool messages that the model sees on
  the next turn, so the agent can notice and adapt.
- **`Observer::should_dispatch(&ToolCall) -> Disposition`.** New default
  method on the `Observer` trait (`Disposition::Allow` / `Refused(msg)`)
  that lets policy layers veto a tool call before it runs. Refusals are
  surfaced as tool results so the model stays in the loop.
- **`bwrap-shell` feature — Linux bubblewrap sandbox for `Shell`.**
  Stacks on top of the v0.2 argv allowlist: when enabled, allowed commands
  execute inside a `bwrap --unshare-all --die-with-parent` namespace with
  a tmpfs `/tmp`, a read-only `/usr` + `/bin`, and cwd bound at the same
  path. Opt-in via `agnt-tools/bwrap-shell` or the flagship
  `tools-bwrap-shell` feature. Non-Linux builds return a clear error from
  the `new_bwrap` constructor.
- **`fuzz/` workspace — 4 libfuzzer targets.** Covers `FilesystemRoot`
  resolution, Fetch SSRF guard, glob pattern parsing, and tool-call
  dispatch. Isolated via a nested `[workspace]` table so the main crates
  stay on stable Rust. Stream parsers will be fuzzed in v0.4 once the
  targets gain a non-private entry point.
- **Release-grade criterion benches** with real numbers captured in
  `benches/README.md`.

### 🧰 Flagship features

- New: `macros` (default on), `mcp`, `tools-bwrap-shell`.
- Unchanged: `net`, `store`, `tools`, `tools-shell`.

### 🧪 Tests

- +12 unit tests across the new crates and the quota / observer veto paths.
- Workspace test count: 77 passing with default features, 83 with
  `shell bwrap-shell` enabled. Zero `#[ignore]`-by-default tests outside
  of the bwrap integration test that requires `bwrap` on `$PATH`.
- One flaky MCP stdio test stabilized by aligning the mock-server script
  with the post-handshake `notifications/initialized` notification.

### 🛠 Internal

- Workspace version bumped to `0.3.0`; all path-dep version constraints
  bumped in lockstep.
- `Agent::step` now runs a sequential pre-dispatch decision pass
  (observer veto + quota check) before the parallel `thread::scope`
  dispatch block, so quota counters remain coherent under concurrent
  tool calls within one turn.

## [0.2.0] — 2026-04-12

A hardening + restructuring release. v0.1.0 has been yanked from crates.io;
upgrade to v0.2 is strongly recommended due to three Critical-severity
findings in the default tool set.

### ⚠ Breaking changes

- **`Shell` tool is now opt-in.** Gated behind the `shell` cargo feature in
  `agnt-tools`. The v0.1 `SHELL_DENYLIST` approach was trivially bypassable
  and has been deleted. The new `Shell::new_sandboxed(allowed_argv0, cwd)`
  is the only constructor and requires an explicit argv allowlist. Commands
  are parsed via `shell-words` and executed directly via `Command::new(argv[0])`
  — **no more `sh -c`**. The `unsafe_mode` field is gone.
- **Filesystem tools (`ReadFile`, `WriteFile`, `EditFile`, `ListDir`,
  `Glob`, `Grep`) are no longer unit structs.** They gained private
  `sandbox: Option<Arc<FilesystemRoot>>` fields and must be constructed via
  `::new()` or `::with_sandbox(root)`. Old `Box::new(ReadFile)` call sites
  become `Box::new(ReadFile::new())`.
- **`Backend::api_key` is now private.** Use the `ollama` / `openai` /
  `anthropic` constructors as before, or build one with `Backend::with_timeouts`.
  `Backend` now has a manual `Debug` impl that prints `api_key: <redacted>`.
- **`Agent::stream: bool` is deprecated.** Prefer
  `agent.on_token = Some(Box::new(|tok| ...))` as the token sink. The legacy
  `stream = true` path still works (prints to stdout) but will be removed in
  v0.3.
- **`Agent::new` takes a backend `B: LlmBackend` generic.** Previously tied
  to the concrete `Backend`. This is a compile-error only at API boundaries
  that stored `Agent` as a bare type — `Agent<Backend>` works.
- **`Tool::call` signature unchanged** for backward compatibility. v0.2 adds
  a new `TypedTool` trait alongside with associated `Args` / `Output` /
  `Error` types, plus an `ErasedAdapter<T: TypedTool>` that bridges typed
  impls into the existing erased `Tool` dispatch path. Existing `Tool`
  implementations keep working unchanged.

### 🔒 Security (Critical)

- **S1** — Remove default `Shell` tool; new sandboxed constructor with
  argv allowlist and token-level metacharacter rejection.
- **S2** — Add `FilesystemRoot` sandbox type in `agnt-tools::sandbox`.
  All filesystem-touching tools now accept an optional sandbox that
  canonicalizes paths, rejects `..` components, and follows symlinks
  before containment check.
- **S3** — `Fetch` now parses URLs with `url::Url`, enforces a
  `http`/`https` scheme allowlist, rejects loopback / private / link-local
  / multicast / unspecified / broadcast / AWS IMDS / GCP metadata targets
  (both IPv4 and IPv6), and disables HTTP redirects on the shared ureq
  agent so the check cannot be bypassed via a 302.

### 🔒 Security (High)

- **S4** — Tool results are now wrapped in a
  `<tool_output name="..." id="..." truncated="...">...</tool_output>`
  envelope before being persisted or fed back to the model. Raw bytes are
  capped at `Agent::max_tool_result_bytes` (default 64KB) with
  UTF-8-boundary-safe truncation. System prompts should explicitly instruct
  the model that tool output is data, not operator instructions.
- **S5** — Every `.unwrap()` / `.expect()` in the library path has been
  removed. Scoped-thread panics during tool dispatch are caught and
  converted to error strings; the loop continues instead of aborting.
  `http::agent()` now returns `Result` with lazy TLS init.
- **S6** — `EditFile` is now atomic: sidecar-lockfile via
  `fs2::FileExt::lock_exclusive`, re-read under lock, temp write + rename.
  Verified with a 4-thread × 100-round stress test.
- **S7** — `Backend::api_key` private, manual `Debug` impl redacts,
  `redact_secrets()` scrubs `Authorization` and `x-api-key` from upstream
  error bodies before they bubble up.

### ⚡ Performance

- **P1** — `Agent::step` avoids cloning the full message history when the
  conversation fits in `max_window`. Truncated path clones only the
  messages actually sent. `resp.clone()` removed.
- **P2** — Request bodies are serialized to `Vec<u8>` once before
  `with_retry`, not cloned on every retry attempt.
- **P3** — SQLite store now opens in `journal_mode=WAL` with
  `synchronous=NORMAL`. All queries use `prepare_cached`. `append`
  collapsed to a single `INSERT … SELECT COALESCE(MAX(idx),-1)+1`
  statement. New `Store::append_many` and `Store::with_transaction`
  batch multiple writes into one fsync. Expected impact: 2+N fsyncs per
  turn → 1.
- **P4** — SSE stream parsers use a single reused `String` buffer via
  `read_sse_line` helper. Parsers are now generic over `R: Read` which
  also enabled two new unit tests against recorded SSE blobs. Anthropic
  parser borrows `content_block`/`delta` instead of cloning.
- **P5** — Retry backoff has ±20% jitter via a hand-rolled xorshift64*
  (no new dep). `max == 0` returns an error instead of `unreachable!()`.
- **P6** — HTTP client has default timeouts: `timeout_connect=10s`,
  `timeout_read=120s`. Override via `Backend::with_timeouts(connect, read)`.

### ✨ New features

- **A1** — Typed `TypedTool` trait with associated `Args` / `Output` /
  `Error` types, constant `NAME` / `DESCRIPTION`, and `schema()` method.
  Bridge via `ErasedAdapter<T: TypedTool>` to register into the existing
  `Registry`. Use `Registry::register_typed(tool)` as a convenience.
- **A2** — Multi-crate workspace. The monolithic v0.1 crate is now split
  into:
  - `agnt-core` — traits, message types, Agent loop. **Zero I/O deps.**
    Compiles to WASM as-is if you bring your own backend.
  - `agnt-net` — HTTP backend implementation (Ollama / OpenAI / Anthropic).
  - `agnt-store` — SQLite-bundled message store.
  - `agnt-tools` — built-in tools with the sandboxes above.
  - `agnt` — flagship meta-crate with `default = ["net", "store", "tools"]`
    that re-exports everything under the v0.1 paths. `cargo add agnt`
    continues to give the full runtime.
- **A3** — Feature flags on the `agnt` flagship crate:
  `net`, `store`, `tools`, `tools-shell`. Minimal build:
  `agnt = { version = "0.2", default-features = false, features = ["net"] }`
- **A4** — `tracing` instrumentation. `agnt.step`, `agnt.backend.chat`,
  `agnt.tool` spans, plus `event!`s at tool dispatch and persistence.
  No direct `opentelemetry` dep — bridge externally via
  `tracing-opentelemetry` if you want OTel export.
- **A5** — `Observer` trait with `on_step_start`, `on_tool_start`,
  `on_tool_end`, `on_step_end`, `on_step_error` hooks. Single extension
  point for HITL approval, audit logging, metrics. Attach via
  `AgentBuilder::observer`.
- **A6** — Token usage accounting. New `usage` table in `agnt-store`
  tracking `prompt_tokens` / `completion_tokens` / `total_tokens` per
  assistant message. `Store::log_usage` and `Store::usage_total` methods.
  Agent loop wiring lands in v0.3.
- **A7** — `AgentBuilder` for fluent construction:
  ```rust
  let agent = AgentBuilder::new(backend)
      .system("You are helpful.")
      .tool(Box::new(ReadFile::new()))
      .max_steps(5)
      .on_token(Box::new(|t| print!("{}", t)))
      .build()?;
  ```
- **A8** — `Agent::on_token: Option<Box<dyn FnMut(&str) + Send>>` replaces
  the hardcoded stdout streaming in v0.1. The old `stream` field remains
  (deprecated) for backward compatibility.

### 🧪 Tests

- 53 tests across the workspace passing (11 agnt-core, 7 agnt-net +
  2 doctests, 9 agnt-store, 21 agnt-tools default, 25 agnt-tools with
  `shell` feature, 3 agnt builder).
- `cargo test --workspace` clean.
- `cargo build --workspace` clean with zero warnings.
- Minimal-feature build (`--no-default-features --features net`) compiles.
- `cargo audit` clean (0 advisories).

### 📦 Crate layout (published)

| Crate | Purpose | Version |
|---|---|---|
| `agnt` | Flagship meta-crate | 0.2.0 |
| `agnt-core` | Zero-I/O kernel | 0.2.0 |
| `agnt-net` | HTTP backends | 0.2.0 |
| `agnt-store` | SQLite persistence | 0.2.0 |
| `agnt-tools` | Built-in tools | 0.2.0 |

## [0.1.0] — 2026-04-12 (YANKED)

Initial release. Yanked the same day due to three Critical security
findings in `agnt-tools` (Shell denylist bypass, path traversal, SSRF).
Upgrade to v0.2 immediately.
