# agnt-bridge-tools

System-level tools for `agnt-bridge` — opens apps, reads the clipboard,
takes screenshots, searches the web, recalls / writes persistent memory,
and dispatches tasks to other agents on the NATS bus.

## What's in this crate

| Tool             | What it does                                                                                  |
|------------------|-----------------------------------------------------------------------------------------------|
| `open_app`       | Launch a desktop app via `gtk-launch` with fuzzy `.desktop`-name matching.                    |
| `open_url`       | `xdg-open <url>`. Only http/https/mailto are accepted; `file://` is rejected.                 |
| `notification`   | `notify-send <title> [body]` — non-blocking status updates.                                   |
| `current_window` | `xdotool getactivewindow getwindow{name,classname}` — what is the user looking at right now.  |
| `screenshot`     | Capture full screen to `~/.cache/voicectl/screenshots/<ts>.png`. Returns the path only.       |
| `clipboard_get`  | Read X11 clipboard (`xclip -selection clipboard -o`). Capped at 4 KB.                         |
| `web_search`     | GET `<searxng_url>/search?q=…&format=json`. Returns a numbered list with snippets.            |
| `memctl_recall`  | `memctl recall --limit N <query>` — scored persistent memories.                               |
| `memctl_ingest`  | `memctl ingest -t <kind> -s <scope> <content>` (kinds + scopes are validated).                |
| `dispatch_agent` | Publish on `agent.dispatch.<name>`, await one `AgentReply`. Multi-agent quarterbacking.       |

## What's deliberately NOT in this crate (Day-7+)

These need a confirmation pattern, allowlist, or sandbox before they can
ride the same bus:

- **`click(x, y)`**, **`type_text(text)`**, **`key_combo(combo)`** —
  destructive computer use; need user confirmation, scoped activation,
  and a record of what got injected.
- **`clipboard_set`** — could overwrite a copy the user has in flight.
- **`run_command`** — too broad; needs an allowlist and a sandbox
  (bubblewrap or similar).
- **File-write tools** outside `~/.cache/voicectl/`.

## Configuration

Each agent's TOML config picks a subset of tools by name:

```toml
[tools]
vault_root = "~/Documents/Squinks"
enabled = [
  # vault tools (agnt-bridge builtins)
  "read_file",
  "grep",
  # system tools (this crate)
  "open_app",
  "open_url",
  "notification",
  "current_window",
  "screenshot",
  "clipboard_get",
  "web_search",
  "memctl_recall",
  "memctl_ingest",
  "dispatch_agent",
]
```

Anything not in the list is omitted from the registry. Unknown tool
names are warn-logged at startup (don't panic — keeps configs forward-
and backward-compatible across agnt-bridge versions).

## Safety posture

- All tools shell out via `tokio::process::Command` with **structured
  arguments** — never `sh -c "<string>"`. There is no command-injection
  surface.
- `open_url` only allows http/https/mailto.
- `screenshot` writes only under `~/.cache/voicectl/screenshots/`.
- `memctl_ingest` validates `kind` ∈ {decision, fact, error, correction,
  pattern, insight} and `scope` ∈ {project, global}.
- `dispatch_agent` validates the agent name's character set so the LLM
  can't subscribe to wildcard subjects.
- Tools run inside `tokio::task::spawn_blocking`, so a misbehaving tool
  can't stall the bridge's event loop.

## Async model

The agnt-rs `Tool` trait is sync. Tools that need network or
subprocess I/O block on the surrounding tokio runtime via
`Handle::current().block_on(…)`. This works because `agnt-bridge`
always invokes tools from inside `spawn_blocking`.

## License

MIT OR Apache-2.0.
