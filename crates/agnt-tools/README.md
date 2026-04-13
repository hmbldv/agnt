# agnt-tools

**Built-in sandboxed tools for the [agnt](https://crates.io/crates/agnt) agent runtime.**

Ships seven default tools that implement `agnt_core::Tool`, plus one
opt-in CVE-class tool behind a cargo feature flag.

```toml
[dependencies]
agnt-tools = "0.2"

# Opt in to the Shell tool (default off for security reasons):
# agnt-tools = { version = "0.2", features = ["shell"] }
```

## When to depend on this

Most users should `cargo add agnt` — the flagship crate re-exports this
one under the `tools` feature.

Depend directly on `agnt-tools` when you want the built-in tools without
pulling in the full Agent loop — for example, when embedding the tools
into a larger agent framework of your own.

## What it provides

### Default tools

- **`ReadFile`** — read a UTF-8 file with optional `FilesystemRoot` sandbox
- **`WriteFile`** — write a file, sandboxed
- **`EditFile`** — atomic edit via sidecar lockfile + temp-rename
- **`ListDir`** — directory listing, sandboxed
- **`Glob`** — shell-style glob patterns, sandboxed
- **`Grep`** — ripgrep-style regex search via `walkdir`, sandboxed
- **`Fetch`** — HTTP GET with SSRF guard, host allowlist, byte cap
- **`FilesystemRoot`** — the sandbox type all filesystem tools accept

### Opt-in (CVE-class)

- **`Shell`** (`shell` feature) — arbitrary command execution with an
  explicit argv allowlist, token-level metacharacter rejection, direct
  `Command::new(argv[0])` spawn (never `sh -c`). **Default-off.**
  Requires an explicit `Shell::new_sandboxed(allowed_argv0, cwd)`
  constructor call.

## Security

The entire security story of `agnt-tools` lives in the
[threat model](https://github.com/hmbldv/agnt/blob/main/THREAT_MODEL.md).
Summary:

- Filesystem tools use `FilesystemRoot` for symlink-aware containment
  checks. Without a sandbox the tool is explicitly documented as full-host.
- `Fetch` blocks loopback / private / link-local / AWS IMDS / GCP metadata
  (IPv4 and IPv6) *atomically* with DNS resolution via the custom
  [`ssrf::SsrfResolver`] installed on a per-instance `ureq::Agent`.
  v0.3.1 closed the two-phase TOCTOU that v0.2/v0.3 had — ureq no longer
  performs a second DNS lookup after validation. Redirects are disabled.
- `EditFile` is race-free via an exclusive sidecar lockfile.
- `Shell` has no unsafe constructor — the caller must explicitly opt in
  to both the cargo feature AND provide an argv allowlist. On Linux the
  `bwrap-shell` feature adds a bubblewrap namespace wrapper on top.

[`ssrf::SsrfResolver`]: https://docs.rs/agnt-tools/latest/agnt_tools/ssrf/struct.SsrfResolver.html

## License

Dual-licensed under MIT OR Apache-2.0.
