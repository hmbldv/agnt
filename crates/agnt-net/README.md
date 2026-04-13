# agnt-net

**HTTP backend for the [agnt](https://crates.io/crates/agnt) agent runtime.**

Provides multi-provider LLM inference (Ollama, OpenAI, Anthropic) with
streaming SSE, retry with jitter, and default timeouts — on top of
blocking `ureq`. No async runtime required.

```toml
[dependencies]
agnt-net = "0.2"
```

## When to depend on this

Most users should `cargo add agnt` — the flagship crate re-exports this
one under the `net` feature.

Depend directly on `agnt-net` when you want the backend layer without
`agnt-core`'s Agent loop — for example, if you're using the `Backend`
struct as a thin streaming LLM client from some other framework.

## What it provides

- `Backend::ollama(model)` — local Ollama via its OpenAI-compat API
- `Backend::openai(model, api_key)` — OpenAI
- `Backend::anthropic(model, api_key)` — Anthropic (with content-block
  translation at the wire boundary)
- `Backend::with_timeouts(connect, read)` — per-instance HTTP timeouts
- Implements `agnt_core::LlmBackend`

## Security notes

- `Backend::api_key` is private. Manual `Debug` impl prints `api_key: <redacted>`.
- Upstream error bodies have `Authorization` / `x-api-key` headers scrubbed
  before bubbling up.
- HTTP redirects are enabled on the default shared agent but disabled in
  `agnt-tools::Fetch` (for SSRF reasons).
- TLS init is lazy and fallible via `OnceLock<Result<_>>` — no first-call
  panic.
- See the [threat model](https://github.com/hmbldv/agnt/blob/main/THREAT_MODEL.md).

## License

Dual-licensed under MIT OR Apache-2.0.
