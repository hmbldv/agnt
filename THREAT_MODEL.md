# agnt Threat Model (current: v0.3.1)

This document describes what `agnt` defends against, what it does not,
and the assumptions behind its security posture. Updated for v0.3.1.

## Changelog

- **v0.3.1** — Closes the DNS rebinding TOCTOU in `Fetch` by moving the
  SSRF guard into a custom `ureq::Resolver` so validation and lookup
  are the same operation. Bounds the MCP stdio reader at 4 MiB per
  line to eliminate the unbounded-allocation DoS. Adds
  `Agent::max_step_duration` for a coarse per-step wall-clock deadline.
  Adds SSE stream parser and SSRF resolver fuzz targets. Preserves
  panicked-tool attribution through the dispatch boundary.
- **v0.3.0** — Adds `#[tool]` proc-macro, MCP stdio client, per-tool
  quotas, `Observer::should_dispatch` policy hook, bubblewrap sandbox
  on top of the Shell argv allowlist. No new Critical findings vs
  v0.2.
- **v0.2.0** — Hardening release: Shell opt-in with argv allowlist
  (was denylist-bypassable), FilesystemRoot sandbox (was unbounded),
  SSRF guard on Fetch (was absent). v0.1.0 yanked the same day it
  was published because of three Critical findings.

## Adversary

The **primary adversary is the LLM itself.** Any tool call issued by the
model should be assumed to be either:

1. A reasonable action in response to user input, or
2. An adversarial payload injected into the model via:
   - A hostile webpage fetched by `Fetch`
   - A malicious file read by `ReadFile` or `Grep`
   - A crafted tool output from an earlier turn (cross-tool injection)
   - Prompt injection in the user's own input from an untrusted source

`agnt` does **not** distinguish these cases at dispatch time. Every tool call
is treated as potentially adversarial and constrained accordingly.

## In scope

These attack classes are structurally blocked in v0.2:

### Command execution

- **`Shell` RCE via denylist bypass** — fixed by removing the denylist
  entirely. `Shell` is now opt-in (`features = ["tools-shell"]`), requires
  an explicit argv allowlist at construction, and spawns commands directly
  via `Command::new(argv[0])` — never `sh -c`. Tokens containing shell
  metacharacters (`$`, `` ` ``, `|`, `;`, `&`, `>`, `<`, `(`, `)`) are
  rejected.
- **Command substitution via `$(...)` or backticks** — fixed by not invoking
  a shell at all. The tokens never reach a shell interpreter.
- **Pipe chains like `echo x | tee /etc/sudoers`** — fixed by rejecting `|`
  at the token level and by not using `sh -c`.

### Filesystem access

- **Path traversal via `..`** — fixed by the `FilesystemRoot` sandbox.
  `resolve()` rejects any input whose canonical path escapes the root.
  Components containing `..` are rejected explicitly.
- **Symlink escape** — fixed by canonicalizing every input path. A symlink
  pointing outside the root is rejected after canonicalization.
- **Reading `/etc/shadow`, `~/.ssh/id_rsa`, etc.** — blocked when a
  `FilesystemRoot` is attached to the tool. Tools without a sandbox are
  unconstrained and documented as such in their rustdoc.
- **Writing to `~/.bashrc` or `~/.ssh/authorized_keys`** — blocked by the
  same sandbox check; also blocked structurally because `WriteFile` /
  `EditFile` paths must canonicalize to a subpath of the root.

### Network access

- **SSRF to AWS IMDS (169.254.169.254) or GCP metadata** — blocked
  atomically inside `agnt_tools::ssrf::SsrfResolver`. Both addresses
  are rejected by name and by IP.
- **SSRF to loopback / localhost** — blocked. `127.0.0.0/8` and `::1`
  rejected.
- **SSRF to private IPv4 ranges** — blocked. `10.0.0.0/8`, `172.16.0.0/12`,
  `192.168.0.0/16` rejected via `Ipv4Addr::is_private()`.
- **SSRF to link-local and unique local IPv6** — blocked. `fe80::/10` and
  `fc00::/7` rejected.
- **DNS rebinding / check-vs-use TOCTOU** — blocked as of v0.3.1.
  `Fetch` installs `SsrfResolver` as its `ureq::Agent`'s resolver, so
  validation happens inside the same DNS lookup whose result ureq then
  uses to connect. There is no second lookup to flip. A hostile
  authority cannot serve a safe IP at check time and a private IP at
  request time because there is only one resolution. This closes the
  v0.2/v0.3 two-phase gap where `ssrf_check` ran ahead of `ureq.get`.
- **Redirect-based SSRF bypass** — blocked. `Fetch`'s per-instance
  ureq agent has `redirects(0)` set, so a hostile server cannot
  bounce the client to an internal target via a `302 Location:`
  header. The model must re-call `Fetch` with the redirect target
  explicitly, re-triggering resolver validation.
- **Non-HTTP schemes (`file://`, `ftp://`, `gopher://`)** — blocked. Only
  `http` and `https` are accepted. Scheme check runs in
  `fetch_url_shape_check` before the ureq agent touches the URL,
  because the resolver only sees the netloc, not the scheme.

### Concurrency and atomicity

- **`EditFile` TOCTOU race** — fixed with a sidecar lockfile pattern.
  Multiple concurrent edits to the same file are serialized exclusively
  via `fs2::FileExt::lock_exclusive`. Writes go to a temp file and are
  atomically renamed. Verified with a 4-thread × 100-round stress test —
  exactly one winner per round.

### Prompt injection via tool output

- **Injection via fetched web content / file contents** — **partially**
  mitigated by the `<tool_output name="..." id="..." truncated="...">...</tool_output>`
  envelope. Every tool result is wrapped before being persisted or fed back
  to the model. The system prompt should explicitly instruct the model that
  content inside these envelopes is data, not operator instructions.
  Truncation at 64KB limits the blast radius of large injection payloads.
  **This is a mitigation, not a guarantee.** The model may still follow
  instructions embedded in tool output. See "Out of scope" below.

### Secrets and information leakage

- **API key leaked in `Debug` output** — fixed. `Backend` has a manual
  `Debug` impl that prints `api_key: <redacted>`. The field is private;
  construction goes through the provider-specific constructors.
- **API key leaked in error messages** — fixed. `redact_secrets()` scrubs
  `Authorization` and `x-api-key` headers from upstream error bodies before
  they bubble up through `BackendError`.

### Denial of service via panics

- **`.expect()` / `.unwrap()` panics crashing the agent** — fixed. Every
  library path panic site has been replaced with proper `Result` return.
  TLS init is lazy and fallible. Scoped-thread panics during tool dispatch
  are caught and converted to error strings; the loop continues. As of
  v0.3.1 the panicked-tool path also preserves `tool_call_id` and
  `Tool::name()` through the join-fallback, so SQLite tool logs and
  downstream observers keep attribution instead of seeing an empty
  `<panicked>` placeholder.
- **Integer overflow in retry backoff math** — checked. Backoff is capped
  at 8 seconds, so the math never overflows.
- **MCP server flooding the reader with an unbounded line** — fixed in
  v0.3.1. `agnt_mcp` uses a bounded `read_bounded_line` helper capped
  at `MAX_LINE_BYTES` (4 MiB). Overflow emits a protocol error on the
  channel and closes the reader; the stream is unrecoverable but the
  agent process does not OOM.
- **Runaway step() tied to a slow tool or slow backend** — bounded in
  v0.3.1 via `Agent::max_step_duration`. When set, the step loop
  refuses to begin a new backend call or tool dispatch past the
  deadline. Granularity is between operations — a single tool that
  has already started runs to its own timeout — but combined with
  `Fetch`'s 10s connect / 120s read defaults, the worst-case hang
  is bounded.

### Transport security

- **TLS certificate verification** — confirmed correct. `agnt-net::http`
  uses `native-tls` with the system trust store. No `danger_accept_invalid_certs`
  or custom verifier.
- **HTTP timeouts** — default 10s connect, 120s read. Previously unbounded.
  A hung upstream cannot block the agent loop forever.

## Partially mitigated

These have reduced impact but are not fully solved:

### Prompt injection inside tool output

The `<tool_output>` envelope signals to the model that content is data.
A well-instructed model should treat it accordingly. A poorly-instructed
model, or a model that's been jailbroken by a sufficiently clever payload,
may still follow instructions embedded in tool output.

**Recommended mitigations for consumers:**

- Use a strong system prompt that explicitly instructs the model to treat
  `<tool_output>` content as untrusted data
- Register an `Observer` that rejects tool calls whose args contain content
  drawn from recent tool outputs
- For production use, gate sensitive tools (write, edit, fetch to
  allowlisted hosts) behind HITL approval via the observer

### Tool result size attacks

Tool outputs are capped at 64KB per call by default
(`Agent::max_tool_result_bytes`). A sufficiently adversarial tool could
still chain many small outputs together. The context window limit is the
ultimate backstop; set `Agent::max_window` conservatively.

## Out of scope

These are explicitly **not** defended against in v0.2:

### The compromised operator

`agnt` assumes the operator (the human running the agent) is trusted.
An operator can bypass every sandbox by constructing the tools without a
`FilesystemRoot`, enabling the `shell` feature without an allowlist, or
providing an allowlist that includes dangerous binaries. The library can
make the unsafe path inconvenient and well-documented, but cannot prevent
it.

### Side-channel data exfiltration

A tool that legitimately has network access (`Fetch` with an allowlist
containing, e.g., an attacker-controlled domain) can exfiltrate secrets
it has access to. `agnt` does not prevent this. Use the observer trait
to detect suspicious patterns if this is in your threat model.

### Resource exhaustion via the LLM

A model stuck in a loop of tool calls will consume CPU, memory, tokens
(API cost), and GPU (local inference) up to `Agent::max_steps`. The
default is 10. Set it conservatively. v0.3 added per-tool quotas
(`ToolQuota::max_calls` / `max_duration_us` / `max_result_bytes`),
and v0.3.1 added `Agent::max_step_duration` for a wall-clock ceiling
on a whole `step()` call. Note that `max_duration_us` is enforced at
*turn boundaries* within a step — concurrent calls to the same
quotaed tool in one turn all pass the duration check (they see a
zero counter); use `max_calls = 1` to serialize them if you need
strict per-turn wall time. Session-wide quotas across multiple
`step()` calls are not provided in v0.3.x; the operator is
responsible.

### Race conditions in third-party tools

v0.2 guards `EditFile` specifically. Tool authors writing their own
filesystem tools are responsible for their own atomicity.

### Cryptographic attacks on the trust model

`agnt` has no intrinsic trust model. If you need signed agent-to-agent
calls, use the `Observer` trait to verify signatures externally. The
trust-claim envelope work is at the substrate layer (e.g. SOLA, PLYGLT),
not in `agnt` itself.

### Sandbox escape via `/proc` or `/sys`

Even with a `FilesystemRoot` of e.g. `/home/user/work`, a symlink within
that directory pointing to `/proc/self/mem` could expose process memory
if the model writes to it. `agnt`'s sandbox does a symlink-containment
check, but a determined model + a permissive root may find edge cases.
For true isolation, pair `agnt` with OS-level sandboxing (containers,
namespaces, seccomp, bubblewrap).

### Compromised dependencies

`agnt` depends on `ureq`, `native-tls`, `rusqlite`, `walkdir`, `regex`,
`glob`, `fs2`, `url`, `shell-words`, `tracing`, `syn`/`quote`/`proc-macro2`
(via `agnt-macros`), and `serde*`. A supply-chain compromise in any of
these affects `agnt`. Run `cargo audit` before each release.

## Reporting vulnerabilities

If you find a security issue in `agnt`, please open a GitHub issue marked
**"security"** at https://github.com/hmbldv/agnt/issues or email the
maintainer directly. Do not post exploits publicly before a fix is released.

v0.1.0 was yanked the same day it was published because of three Critical
findings (Shell RCE, path traversal, SSRF). The v0.2 hardening pass
addressed all three. v0.3.1 closed a High-severity DNS rebinding
TOCTOU in `Fetch` that the v0.3 adversarial review identified. New
findings will be triaged with the same urgency.
