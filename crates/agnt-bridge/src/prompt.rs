//! RASP (Retrieval-Augmented System Prompt) builder.
//!
//! Produces a two-layer system prompt and an optional per-dispatch context
//! prefix. Total overhead: ~120t static + up to ~300t live = ~420t.
//!
//! ## Layout
//!
//! ```text
//! Layer 1 — identity (~30t)   agents/<name>/system.md rendered with agent.toml fields
//! Layer 2 — principles (~90t) agents/<name>/principles.md (terse imperative rules)
//! ```
//!
//! Layer 3 (live context) is injected at dispatch time as a `[Context]` block
//! prepended to the user message, not baked into the static system prompt.
//!
//! ## Fallback
//!
//! If `agents_dir/<name>/` is absent or malformed, both functions return
//! `None`/unchanged-input so the caller can fall back to the static
//! `system_file` approach.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::{debug, warn};

// ── AgentDef ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct AgentDef {
    pub name: String,
    pub role: String,
    pub team: String,
    pub focus: String,
    #[serde(default = "default_trust_tier")]
    pub trust_tier: String,
    #[serde(default = "default_confirm_mode")]
    pub confirm_mode: String,
}

fn default_trust_tier() -> String {
    "standard".into()
}

fn default_confirm_mode() -> String {
    "confirm".into()
}

// ── Static prompt (startup) ──────────────────────────────────────────────────

/// Build the static system prompt from `agents_dir/<agent_name>/`.
/// Returns `None` if the directory is missing or agent.toml cannot be parsed.
pub fn build_static_prompt(agents_dir: &Path, agent_name: &str) -> Option<String> {
    let dir = agents_dir.join(agent_name);
    if !dir.exists() {
        return None;
    }

    let def = load_agent_def(&dir.join("agent.toml"))?;

    let system_raw = read_trimmed(&dir.join("system.md")).unwrap_or_default();
    let identity = render_template(&system_raw, &def);

    let principles = read_trimmed(&dir.join("principles.md")).unwrap_or_default();

    if identity.is_empty() && principles.is_empty() {
        return None;
    }

    let mut parts = Vec::new();
    if !identity.is_empty() {
        parts.push(identity);
    }
    if !principles.is_empty() {
        parts.push(principles);
    }
    Some(parts.join("\n\n"))
}

// ── Live context (dispatch time, blocking) ───────────────────────────────────

/// Query memctl for the top-3 recalls relevant to the pending user input.
/// Returns `None` if memctl is unreachable or returns no results.
///
/// Must be called from a blocking context (inside `spawn_blocking`).
pub fn recall_context(memctl_bin: &Path, agent_name: &str, user_input: &str) -> Option<String> {
    // Blend agent name with first 60 chars of the request for relevance.
    let query = format!(
        "{} {}",
        agent_name,
        &user_input[..user_input.len().min(60)]
    );

    let output = std::process::Command::new(memctl_bin)
        .args(["recall", &query, "--limit", "3"])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            if text.is_empty() {
                None
            } else {
                debug!(bytes = text.len(), "memctl recall returned context");
                Some(text)
            }
        }
        Ok(o) => {
            warn!(
                stderr = %String::from_utf8_lossy(&o.stderr),
                "memctl recall non-zero exit"
            );
            None
        }
        Err(e) => {
            warn!(error = %e, "memctl recall failed to execute");
            None
        }
    }
}

/// Prepend a context block to `user_input` when `context` is non-empty.
pub fn augment_user_input(user_input: &str, context: Option<&str>) -> String {
    match context {
        Some(ctx) if !ctx.is_empty() => format!("[Context]\n{ctx}\n\n---\n{user_input}"),
        _ => user_input.to_owned(),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn load_agent_def(path: &Path) -> Option<AgentDef> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "could not read agent.toml");
            return None;
        }
    };
    match toml::from_str::<AgentDef>(&raw) {
        Ok(def) => Some(def),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to parse agent.toml");
            None
        }
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn render_template(template: &str, def: &AgentDef) -> String {
    let mut vars: HashMap<&str, &str> = HashMap::new();
    vars.insert("name", &def.name);
    vars.insert("role", &def.role);
    vars.insert("team", &def.team);
    vars.insert("focus", &def.focus);
    vars.insert("trust_tier", &def.trust_tier);
    vars.insert("confirm_mode", &def.confirm_mode);

    let mut out = template.to_owned();
    for (k, v) in &vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn make_agent_dir(dir: &Path, name: &str) -> std::path::PathBuf {
        let agent_dir = dir.join(name);
        std::fs::create_dir_all(&agent_dir).unwrap();

        let toml = format!(
            r#"name = "{name}"
role = "test role"
team = "TestTeam"
focus = "testing"
trust_tier = "standard"
confirm_mode = "confirm"
"#
        );
        std::fs::write(agent_dir.join("agent.toml"), toml).unwrap();
        std::fs::write(
            agent_dir.join("system.md"),
            "You are {name}, {role}. Team: {team}.",
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("principles.md"),
            "- Rule one.\n- Rule two.",
        )
        .unwrap();
        agent_dir
    }

    #[test]
    fn build_static_prompt_renders_template() {
        let tmp = TempDir::new().unwrap();
        make_agent_dir(tmp.path(), "myagent");

        let prompt = build_static_prompt(tmp.path(), "myagent").unwrap();
        assert!(prompt.contains("You are myagent, test role."));
        assert!(prompt.contains("Team: TestTeam."));
        assert!(prompt.contains("Rule one."));
        assert!(prompt.contains("Rule two."));
    }

    #[test]
    fn build_static_prompt_returns_none_for_missing_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(build_static_prompt(tmp.path(), "ghost").is_none());
    }

    #[test]
    fn augment_user_input_prepends_context() {
        let aug = augment_user_input("what is X?", Some("past: X is Y"));
        assert!(aug.starts_with("[Context]\npast: X is Y\n\n---\n"));
        assert!(aug.ends_with("what is X?"));
    }

    #[test]
    fn augment_user_input_passthrough_when_no_context() {
        let aug = augment_user_input("hello", None);
        assert_eq!(aug, "hello");
    }

    #[test]
    fn augment_user_input_passthrough_when_empty_context() {
        let aug = augment_user_input("hello", Some(""));
        assert_eq!(aug, "hello");
    }

    #[test]
    fn render_template_all_placeholders() {
        let def = AgentDef {
            name: "sage".into(),
            role: "vault helper".into(),
            team: "Utility".into(),
            focus: "filing".into(),
            trust_tier: "standard".into(),
            confirm_mode: "smart".into(),
        };
        let tmpl = "I am {name} ({role}), team {team}, focus {focus}, tier {trust_tier}.";
        let out = render_template(tmpl, &def);
        assert_eq!(
            out,
            "I am sage (vault helper), team Utility, focus filing, tier standard."
        );
    }
}
