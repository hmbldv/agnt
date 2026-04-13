# agnt fuzz targets

`cargo-fuzz` harness for the agnt workspace. Lives outside the parent
workspace (its `Cargo.toml` declares an empty `[workspace]` of its own) so
it doesn't perturb `cargo test --workspace`.

## Prerequisites

```bash
cargo install cargo-fuzz           # one-time
rustup toolchain install nightly   # cargo-fuzz requires nightly for sanitizers
```

## Run

From the repo root:

```bash
cd fuzz
cargo +nightly fuzz list
cargo +nightly fuzz run fuzz_filesystem_root_resolve -- -runs=10000
cargo +nightly fuzz run fuzz_fetch_ssrf             -- -runs=10000
cargo +nightly fuzz run fuzz_glob_pattern           -- -runs=10000
cargo +nightly fuzz run fuzz_tool_call_dispatch     -- -runs=10000
```

A longer run (e.g. 5 minutes, matching the v0.3 release criterion):

```bash
cargo +nightly fuzz run fuzz_filesystem_root_resolve -- -max_total_time=300
```

## Targets

| Target                            | Surface                                   |
|-----------------------------------|-------------------------------------------|
| `fuzz_filesystem_root_resolve`    | `FilesystemRoot::resolve` — path traversal, symlink escapes, pathological UTF-8 |
| `fuzz_fetch_ssrf`                 | `Fetch::call` → internal `ssrf_check` — URL parser attacks, metadata IPs, host allowlist |
| `fuzz_glob_pattern`               | `Glob::call` — `glob` crate parser + sandboxed-relative-path rejection |
| `fuzz_tool_call_dispatch`         | `Registry::dispatch` + `ErasedAdapter` — JSON deserialize/serialize boundary |

### Deferred to v0.4

- `fuzz_openai_stream` — targets the private `parse_openai_stream` in
  `agnt-net::backend`. Exposing it would require modifying `crates/agnt-net/`,
  which is off-limits to the S1 fuzzing sprint (another agent owns it).
  Pick this up in v0.4 by adding a `#[doc(hidden)] pub` test hook or a
  `fuzz` feature on `agnt-net`.
- `fuzz_anthropic_stream` — same rationale.

Both stream parsers are also covered indirectly by unit tests in
`crates/agnt-net/src/backend.rs`; the fuzz targets are a hardening follow-up.

## Triage

Crashes are written under `fuzz/artifacts/<target>/crash-*`. To minimize and
reproduce:

```bash
cargo +nightly fuzz tmin <target> fuzz/artifacts/<target>/crash-XXXX
cargo +nightly fuzz run  <target> fuzz/artifacts/<target>/crash-XXXX
```

Do **not** commit artifacts into git; the `.gitignore` excludes them.

## CI

Short smoke runs (1000 iterations each, well under 30 s wall clock) are
intended to run on every push. A nightly job runs each target for
`-max_total_time=3600`.
