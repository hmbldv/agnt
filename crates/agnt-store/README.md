# agnt-store

**SQLite message store for the [agnt](https://crates.io/crates/agnt) agent runtime.**

Bundled SQLite persistence that implements `agnt_core::MessageStore`.
WAL mode, prepared-statement cache, batch transactions, microsecond tool
profiling, and token usage accounting built in.

```toml
[dependencies]
agnt-store = "0.2"
```

## When to depend on this

Most users should `cargo add agnt` — the flagship crate re-exports this
one under the `store` feature.

Depend directly on `agnt-store` when you want bundled SQLite persistence
without `agnt-core`'s Agent loop — for example, to persist messages from
a custom agent implementation that uses the same on-disk schema.

## What it provides

- `Store::open(path)` — open a SQLite store, auto-creates schema
- `Store::load(session)` — load all messages for a session
- `Store::append(session, msg)` — single message, single roundtrip
- `Store::append_many(session, &[msg])` — batch in one transaction,
  one fsync
- `Store::with_transaction(f)` — arbitrary batching with automatic rollback
- `Store::log_tool(session, name, args, result, duration_us)` — persist
  a tool execution record
- `Store::stats(session)` — per-tool latency histograms (count, avg, max
  µs)
- `Store::log_usage(session, idx, prompt, completion)` — record token
  usage for an assistant turn
- `Store::usage_total(session)` — prompt/completion/total token totals
- `Store::clear(session)` — wipe a session
- Implements `agnt_core::MessageStore`

## Performance

v0.2 improvements over v0.1:

- `PRAGMA journal_mode=WAL` + `synchronous=NORMAL` — batched fsync
- `prepare_cached` for every statement — no per-call SQL reparse
- `append` collapsed to one `INSERT … SELECT COALESCE(MAX(idx),-1)+1 …`
  (one roundtrip instead of two)
- `append_many` wraps N inserts in one transaction — N+2 fsyncs → 1

## License

Dual-licensed under MIT OR Apache-2.0.
