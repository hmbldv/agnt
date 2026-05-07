# agent.toml schema

Every agent directory (`agents/<name>/`) must contain:

| File | Purpose |
|------|---------|
| `agent.toml` | Identity metadata — maps to FalkorDB node fields |
| `system.md` | Identity template — rendered with agent.toml fields |
| `principles.md` | Behavioral rules — terse imperatives, one per line |

## agent.toml fields

```toml
# Required
name = "sage"               # Must match directory name and FalkorDB node name
role = "vault co-author"    # One-line role description
team = "Utility"            # Team name from FalkorDB (BARAKA, CRBRS, Axiom, etc.)
focus = "knowledge, filing" # Comma-separated focus areas

# Optional
trust_tier = "standard"     # observer | restricted | standard | elevated | autonomous
confirm_mode = "smart"      # off | confirm | smart
description = "..."         # Longer description for FalkorDB sync (not in prompt)
```

## system.md templating

Placeholders: `{name}`, `{role}`, `{team}`, `{focus}`, `{trust_tier}`, `{confirm_mode}`

Template is rendered once at startup. Keep it ≤ 30 tokens (one identity sentence + one
sentence of key context). principles.md handles behavioral rules separately.

## principles.md format

One rule per line starting with `-`. Each rule should be ≤ 15 tokens.
Target: 6-10 rules = 80-120 tokens total.

## Token budget (RASP pattern)

| Layer | Source | Budget |
|-------|--------|--------|
| 1 — Identity | system.md rendered | ~30t |
| 2 — Principles | principles.md | ~90t |
| 3 — Live context | memctl recall (3 results) | ~300t |
| **Total** | | **~420t** |

Layer 3 is injected at dispatch time as a context prefix on the user message,
not as part of the static system prompt.
