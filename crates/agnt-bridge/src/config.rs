//! Bridge config schema (`~/.config/voicectl/agents/<name>.toml`).
//!
//! The schema is intentionally agent-shaped, not voice-shaped: the bridge has
//! no idea STT/TTS/voicectld even exist. Voicectld is just one client of the
//! `agent.dispatch.*` subjects.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use voicectl_core::config::ConversationConfig;

/// Top-level bridge config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBridgeConfig {
    pub agent: AgentSection,
    pub backend: BackendSection,
    pub prompt: PromptSection,
    #[serde(default)]
    pub tools: ToolsSection,
    pub bus: BusSection,
    #[serde(default)]
    pub store: Option<StoreSection>,
    /// Token-streaming policy. The bridge always *can* stream — this flag
    /// just toggles whether it actually publishes the
    /// `agent.token.<name>.<request_id>` subject. Off by default; voicectld
    /// flips it on for low-latency TTS.
    #[serde(default)]
    pub streaming: StreamingSection,
    /// Multi-turn conversation policy. Borrowed from voicectl-core so any
    /// future tool can introspect the same schema.
    #[serde(default)]
    pub conversation: ConversationConfig,
}

/// Agent identity. The `name` is also the NATS subject suffix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSection {
    pub name: String,
}

/// Inference backend. v0 is OpenAI-compatible only — Ollama / Anthropic
/// can be added behind the same enum without breaking the schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendSection {
    pub kind: BackendKind,
    pub url: String,
    pub model: String,
    /// Env var name to read the API key from (e.g. `LITELLM_API_KEY`). The
    /// bridge does **not** accept inline keys — env-only by policy.
    pub api_key_env: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// `Backend::openai(model, key).with_base_url(url)` — works for vLLM,
    /// litellm, OpenAI, anything else that speaks `/v1/chat/completions`.
    OpenaiCompat,
    /// Built-in echo backend — bounces `user_input` back without any LLM
    /// call. Useful for stub agents and the "many agnts" demo. The
    /// `url` / `model` / `api_key_env` fields are ignored when this is set.
    Echo,
}

/// System prompt source. We read once at startup — reload requires a
/// service restart (matches voicectld behavior).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptSection {
    /// Path to a UTF-8 markdown file. Tilde-expanded by the bridge.
    pub system_file: PathBuf,
    /// Fallback path if `system_file` doesn't exist. Useful for shared base
    /// prompts when an individual agent hasn't written its own yet.
    #[serde(default)]
    pub fallback_file: Option<PathBuf>,
}

/// Tool surface. `vault_root` is the structural sandbox passed to every
/// path-aware builtin.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsSection {
    /// Filesystem root the agent is allowed to read from. Resolved via
    /// `agnt::FilesystemRoot::new`. Tilde-expanded.
    #[serde(default)]
    pub vault_root: Option<PathBuf>,
    /// Subset of tools to enable. Names accepted:
    ///
    /// - vault: `read_file`, `grep` (agnt-bridge builtins)
    /// - system: anything in `agnt_bridge_tools::ALL_TOOLS` —
    ///   `open_app`, `open_url`, `notification`, `current_window`,
    ///   `screenshot`, `clipboard_get`, `web_search`, `memctl_recall`,
    ///   `memctl_ingest`, `dispatch_agent`,
    ///   `click`, `type_text`, `key_combo`, `scroll`, `focus_window`,
    ///   `get_mouse`.
    ///
    /// Anything not in this list is omitted from the registry. Unknown
    /// names are warn-logged at startup, not fatal.
    #[serde(default)]
    pub enabled: Vec<String>,
    /// SearXNG base URL for `web_search`. Default: `http://lnx-rig:8888`.
    #[serde(default)]
    pub searxng_url: Option<String>,
    /// Path to `memctl`. Default: `~/.local/bin/memctl`.
    #[serde(default)]
    pub memctl_bin: Option<PathBuf>,
    /// Cache directory for tools that write artefacts (`screenshot`,
    /// `look_at_screen`). Default: `~/.cache/voicectl`.
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
    /// Safety mode for destructive computer-use tools. One of `off`,
    /// `confirm`, `smart`. Default: `confirm` (every destructive call waits
    /// for explicit user approval). `off` is testing-only and is warn-logged
    /// at startup.
    #[serde(default)]
    pub computer_use_safety: Option<String>,
    /// Key combos that bypass confirmation in `smart` mode. Default: a
    /// conservative navigation set — see
    /// [`agnt_bridge_tools::computer::default_safe_keys`].
    #[serde(default)]
    pub computer_use_safe_keys: Option<Vec<String>>,
    /// WM-class names whose windows can receive `type_text` / `key_combo`
    /// without confirmation in `smart` mode. Compared case-insensitively.
    /// Default: `["kitty", "Alacritty", "VSCode", "Code", "obsidian",
    /// "Obsidian"]`.
    #[serde(default)]
    pub computer_use_safe_focus_apps: Option<Vec<String>>,
    /// Vision-LLM endpoint for `look_at_screen` (OpenAI-compatible
    /// chat-completions URL). Default: `http://lnx-rig:8002/v1/chat/completions`.
    /// Retained for backward-compat but silently ignored — look_at_screen now
    /// dispatches over NATS to vznd, not directly to a model.
    #[serde(default)]
    pub vision_url: Option<String>,
    /// Vision-LLM model name advertised by the endpoint. Retained for
    /// backward-compat but silently ignored — use `vision_analyze_model` instead.
    #[serde(default)]
    pub vision_model: Option<String>,
    /// Model override for vznd enhanced pass-1 bbox localisation.
    /// When set, vznd uses this model for the thumbnail → bbox step.
    /// Good choice: `"qwen2-vl-2b"` (fast, local). Default: vznd's configured default.
    #[serde(default)]
    pub vision_localize_model: Option<String>,
    /// Model override for vznd standard single-pass and enhanced pass-2 analysis.
    /// When set, vznd uses this model when analysing the (cropped) screenshot.
    /// Default: `"gemma4-quality"`. Set to `None` in config to let vznd decide.
    #[serde(default = "default_vision_analyze_model")]
    pub vision_analyze_model: Option<String>,
}

fn default_vision_analyze_model() -> Option<String> {
    Some("gemma4-quality".to_string())
}

/// NATS connection + subject naming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusSection {
    pub nats_url: String,
    /// Env var name to read NATS user from. Bridge fails fast if the
    /// referenced env var is missing.
    #[serde(default = "default_user_env")]
    pub user_env: String,
    /// Env var name to read NATS password from.
    #[serde(default = "default_password_env")]
    pub password_env: String,
    /// Subject the bridge subscribes to. Conventionally
    /// `agent.dispatch.<name>`.
    pub subscribe_subject: String,
    /// Prefix used when publishing observability events
    /// (`<publish_prefix>.tool_call`). Replies always go to whatever
    /// `reply_to` the dispatch carried.
    pub publish_prefix: String,
}

fn default_user_env() -> String {
    "NATS_USER".into()
}

fn default_password_env() -> String {
    "NATS_PASSWORD".into()
}

/// Optional persistent message store. When set the agent's history survives
/// restarts. v0 uses agnt-store SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreSection {
    pub db_path: PathBuf,
}

/// Token-streaming flag. A struct (not a bool) so future fields like batch
/// size or sentence-boundary punctuation list can be added without breaking
/// the schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingSection {
    /// When true, the bridge publishes `agent.token.<name>.<request_id>`
    /// frames as the backend emits them, plus a synthetic `is_final=true`
    /// frame after the agent step completes.
    #[serde(default = "default_streaming_enabled")]
    pub enabled: bool,
}

impl Default for StreamingSection {
    fn default() -> Self {
        Self {
            enabled: default_streaming_enabled(),
        }
    }
}

fn default_streaming_enabled() -> bool {
    // On by default — the streaming path falls back gracefully if no
    // subscriber is listening (NATS publishes are fire-and-forget).
    true
}

impl AgentBridgeConfig {
    /// Parse a TOML string. Tilde expansion happens later (per-field).
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Default config path: `~/.config/voicectl/agents/<name>.toml`.
    /// Useful for `--name` argless launches (not currently exposed by main).
    pub fn default_path(agent_name: &str) -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("voicectl/agents").join(format!("{agent_name}.toml")))
    }

    /// Resolve `~/`-prefixed paths against `$HOME`. Mutates in place. We
    /// expand on every path field that the bridge will open.
    pub fn expand_tildes(&mut self) {
        if let Some(p) = self.tools.vault_root.as_mut() {
            *p = expand_tilde(p);
        }
        if let Some(p) = self.tools.memctl_bin.as_mut() {
            *p = expand_tilde(p);
        }
        if let Some(p) = self.tools.cache_dir.as_mut() {
            *p = expand_tilde(p);
        }
        self.prompt.system_file = expand_tilde(&self.prompt.system_file);
        if let Some(p) = self.prompt.fallback_file.as_mut() {
            *p = expand_tilde(p);
        }
        if let Some(s) = self.store.as_mut() {
            s.db_path = expand_tilde(&s.db_path);
        }
    }
}

pub fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[agent]
name = "sage"

[backend]
kind = "openai_compat"
url = "http://localhost:8001/v1"
model = "gemma4-26b"
api_key_env = "LITELLM_API_KEY"

[prompt]
system_file = "~/.config/voicectl/prompts/sage.md"
fallback_file = "~/.config/voicectl/prompts/system.md"

[tools]
vault_root = "~/Documents/Squinks"
enabled = ["read_file", "grep"]

[bus]
nats_url = "nats://lnx-rig:4222"
user_env = "NATS_USER"
password_env = "NATS_PASSWORD"
subscribe_subject = "agent.dispatch.sage"
publish_prefix = "agent.event.sage"

[store]
db_path = "~/.local/state/agnt-bridge/sage.db"

[streaming]
enabled = true

[conversation]
session_timeout_ms = 900000
max_history_turns = 30
"#;

    #[test]
    fn parses_full_sample() {
        let cfg = AgentBridgeConfig::from_toml_str(SAMPLE).expect("parse");
        assert_eq!(cfg.agent.name, "sage");
        assert_eq!(cfg.backend.kind, BackendKind::OpenaiCompat);
        assert_eq!(cfg.backend.model, "gemma4-26b");
        assert_eq!(cfg.backend.api_key_env, "LITELLM_API_KEY");
        assert!(cfg.tools.enabled.contains(&"read_file".to_string()));
        assert!(cfg.tools.enabled.contains(&"grep".to_string()));
        assert_eq!(cfg.bus.subscribe_subject, "agent.dispatch.sage");
        assert!(cfg.store.is_some());
        assert!(cfg.streaming.enabled);
        assert_eq!(cfg.conversation.session_timeout_ms, 900_000);
        assert_eq!(cfg.conversation.max_history_turns, 30);
    }

    #[test]
    fn streaming_and_conversation_default_when_omitted() {
        // Ensure older configs still parse — both new sections must be
        // optional with sensible defaults.
        let toml = r#"
[agent]
name = "sage"

[backend]
kind = "echo"
url = "unused"
model = "unused"
api_key_env = "UNUSED"

[prompt]
system_file = "/dev/null"

[bus]
nats_url = "nats://lnx-rig:4222"
subscribe_subject = "agent.dispatch.sage"
publish_prefix = "agent.event.sage"
"#;
        let cfg = AgentBridgeConfig::from_toml_str(toml).expect("parse");
        // Streaming default is `true` so existing call sites stream by default.
        assert!(cfg.streaming.enabled);
        assert_eq!(cfg.conversation.session_timeout_ms, 600_000);
        assert_eq!(cfg.conversation.max_history_turns, 20);
    }

    #[test]
    fn parses_echo_backend() {
        let toml = r#"
[agent]
name = "echo"

[backend]
kind = "echo"
url = "unused"
model = "unused"
api_key_env = "UNUSED"

[prompt]
system_file = "/dev/null"

[bus]
nats_url = "nats://lnx-rig:4222"
subscribe_subject = "agent.dispatch.echo"
publish_prefix = "agent.event.echo"
"#;
        let cfg = AgentBridgeConfig::from_toml_str(toml).expect("parse");
        assert_eq!(cfg.backend.kind, BackendKind::Echo);
        // Defaults filled in for env-vars + tools.
        assert_eq!(cfg.bus.user_env, "NATS_USER");
        assert!(cfg.tools.enabled.is_empty());
    }

    #[test]
    fn expand_tildes_resolves_home() {
        let mut cfg = AgentBridgeConfig::from_toml_str(SAMPLE).expect("parse");
        cfg.expand_tildes();
        let prompt = cfg.prompt.system_file.to_string_lossy().to_string();
        assert!(
            !prompt.starts_with("~"),
            "tilde should have been expanded, got {prompt}"
        );
        assert!(
            prompt.contains("/.config/voicectl/prompts/sage.md"),
            "unexpected expansion: {prompt}"
        );
    }

    #[test]
    fn missing_required_section_fails_clearly() {
        let toml = r#"
[agent]
name = "x"
"#;
        let err = AgentBridgeConfig::from_toml_str(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("backend") || msg.contains("missing field"),
            "expected field-level error, got: {msg}"
        );
    }
}
