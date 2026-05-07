//! Bridge config schema (`~/.config/voicectl/cc/<bridge>.toml`).
//!
//! One bridge process can serve multiple **personas**, each a (host + cwd +
//! permission_mode + optional system prompt + optional daily cost limit)
//! tuple. The persona is selected by the dispatch subject suffix:
//!
//! ```text
//! cc.dispatch.<persona_name>
//! ```
//!
//! See the example configs under `config/cc/*.toml` for ready-to-use seeds.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level bridge config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CcBridgeConfig {
    pub bridge: BridgeSection,
    pub bus: BusSection,
    /// One or more personas. Empty → bridge fails fast at startup.
    #[serde(default, rename = "personas")]
    pub personas: Vec<Persona>,
}

/// Bridge identity. The `name` is used as the persisted-state file stem
/// and in service-instance names (`cc-bridge@<name>.service`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeSection {
    pub name: String,
    /// Subject root. Conventionally `cc` (subjects become `cc.dispatch.*`,
    /// `cc.cancel.*`, `cc.event.<persona>.cost`). Defaults to `cc`.
    #[serde(default = "default_subject_root")]
    pub subject_root: String,
}

fn default_subject_root() -> String {
    "cc".into()
}

/// NATS connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusSection {
    pub nats_url: String,
    /// Env var name to read NATS user from. Bridge fails fast if the
    /// referenced env var is missing AND no anonymous connection succeeds.
    #[serde(default = "default_user_env")]
    pub user_env: String,
    /// Env var name to read NATS password from.
    #[serde(default = "default_password_env")]
    pub password_env: String,
}

fn default_user_env() -> String {
    "NATS_USER".into()
}

fn default_password_env() -> String {
    "NATS_PASSWORD".into()
}

/// One Claude Code persona.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    /// Persona name. Used as the dispatch-subject suffix
    /// (`<subject_root>.dispatch.<name>`).
    pub name: String,
    /// SSH target — anything `ssh` can resolve. Use `"localhost"` (or `""`)
    /// to invoke `claude` directly without ssh; otherwise the bridge runs
    /// `ssh <host> claude …`.
    pub host: String,
    /// Working directory the remote `claude` invocation `cd`s into before
    /// running. Tilde-expanded only on the local side; the remote shell
    /// resolves whatever you put here. Use absolute paths to be safe.
    pub cwd: String,
    /// Claude Code permission mode. One of `default`, `acceptEdits`,
    /// `bypassPermissions`, `plan`. Validated at startup.
    pub permission_mode: String,
    /// Optional path to a markdown file whose contents are prepended (with
    /// a `\n---\n` separator) to every `user_input` before being passed as
    /// the prompt argument. Tilde-expanded.
    #[serde(default)]
    pub system_prompt_file: Option<PathBuf>,
    /// Daily cost ceiling in USD. When the persona's cumulative cost for
    /// the current local day reaches this value, further dispatches are
    /// rejected with `error="daily cost limit reached for <persona>"` until
    /// midnight. `None` = unbounded (bridge logs cumulative for telemetry
    /// only).
    #[serde(default)]
    pub daily_cost_limit_usd: Option<f64>,
    /// Default per-dispatch wall timeout in seconds. The dispatch's
    /// `context.timeout_sec` overrides this when present. Defaults to 600.
    #[serde(default = "default_timeout_sec")]
    pub timeout_sec: u64,
    /// Optional override of the remote `claude` binary path. Defaults to
    /// `claude` (resolved via `$PATH` on the remote shell). Set this when
    /// the binary is at a non-standard location and the remote shell's
    /// non-interactive `$PATH` doesn't include it.
    #[serde(default)]
    pub claude_bin: Option<String>,
}

fn default_timeout_sec() -> u64 {
    600
}

impl Persona {
    /// Returns true iff the persona is configured to invoke `claude`
    /// locally (no `ssh` wrapper). We treat empty string and "localhost"
    /// equivalently since both are common ways to express "this machine".
    pub fn is_local(&self) -> bool {
        self.host.is_empty() || self.host == "localhost" || self.host == "127.0.0.1"
    }

    /// Validate the persona: permission_mode is one of the documented
    /// values, name is a valid NATS subject token, paths exist where they
    /// must.
    pub fn validate(&self) -> Result<(), String> {
        if !is_valid_subject_token(&self.name) {
            return Err(format!(
                "persona name '{}' is not a valid NATS subject token \
                 (alnum + '_' + '-' only, no '.' or '*' or '>')",
                self.name
            ));
        }
        match self.permission_mode.as_str() {
            "default" | "acceptEdits" | "bypassPermissions" | "plan" => {}
            other => {
                return Err(format!(
                    "persona '{}' has invalid permission_mode '{}'; \
                     must be one of: default, acceptEdits, \
                     bypassPermissions, plan",
                    self.name, other
                ));
            }
        }
        if self.cwd.trim().is_empty() {
            return Err(format!("persona '{}' has empty cwd", self.name));
        }
        Ok(())
    }
}

fn is_valid_subject_token(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl CcBridgeConfig {
    /// Parse a TOML string. Tilde expansion happens later (per-field).
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Default config path: `~/.config/voicectl/cc/<bridge>.toml`.
    pub fn default_path(bridge_name: &str) -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("voicectl/cc").join(format!("{bridge_name}.toml")))
    }

    /// Resolve `~/`-prefixed paths against `$HOME`. Mutates in place.
    pub fn expand_tildes(&mut self) {
        for p in self.personas.iter_mut() {
            if let Some(spf) = p.system_prompt_file.as_mut() {
                *spf = expand_tilde(spf);
            }
        }
    }

    /// Validate the whole config; returns the first violation found.
    pub fn validate(&self) -> Result<(), String> {
        if self.personas.is_empty() {
            return Err("config has zero personas; nothing to dispatch to".into());
        }
        // Names must be unique within a bridge — otherwise subject->persona
        // resolution is ambiguous.
        for (i, a) in self.personas.iter().enumerate() {
            a.validate()?;
            for b in &self.personas[i + 1..] {
                if a.name == b.name {
                    return Err(format!("duplicate persona name '{}'", a.name));
                }
            }
        }
        if !is_valid_subject_token(&self.bridge.subject_root)
            && !self.bridge.subject_root.is_empty()
        {
            return Err(format!(
                "bridge.subject_root '{}' is not a valid NATS token",
                self.bridge.subject_root
            ));
        }
        Ok(())
    }

    /// Look up a persona by name (case-sensitive).
    pub fn persona(&self, name: &str) -> Option<&Persona> {
        self.personas.iter().find(|p| p.name == name)
    }

    /// Subject pattern for the wildcard subscribe. Conventionally
    /// `<subject_root>.dispatch.*`.
    pub fn dispatch_wildcard(&self) -> String {
        format!("{}.dispatch.*", self.bridge.subject_root)
    }

    /// Subject pattern for cancel events. Conventionally
    /// `<subject_root>.cancel.*`.
    pub fn cancel_wildcard(&self) -> String {
        format!("{}.cancel.*", self.bridge.subject_root)
    }

    /// Subject for cost-event publication.
    pub fn cost_subject(&self, persona: &str) -> String {
        format!("{}.event.{}.cost", self.bridge.subject_root, persona)
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
[bridge]
name = "codex"
subject_root = "cc"

[bus]
nats_url = "nats://lnx-rig:4222"
user_env = "NATS_USER"
password_env = "NATS_PASSWORD"

[[personas]]
name = "archon"
host = "lnx-rig"
cwd = "/home/squinks/projects"
permission_mode = "bypassPermissions"
system_prompt_file = "~/.config/voicectl/cc/prompts/archon.md"
daily_cost_limit_usd = 5.00

[[personas]]
name = "scalpel"
host = "ubu"
cwd = "/home/doop/Repositories"
permission_mode = "acceptEdits"
"#;

    #[test]
    fn parses_full_sample() {
        let cfg = CcBridgeConfig::from_toml_str(SAMPLE).expect("parse");
        assert_eq!(cfg.bridge.name, "codex");
        assert_eq!(cfg.bridge.subject_root, "cc");
        assert_eq!(cfg.personas.len(), 2);
        let archon = cfg.persona("archon").unwrap();
        assert_eq!(archon.host, "lnx-rig");
        assert_eq!(archon.permission_mode, "bypassPermissions");
        assert_eq!(archon.daily_cost_limit_usd, Some(5.00));
        assert_eq!(archon.timeout_sec, 600);
        let scalpel = cfg.persona("scalpel").unwrap();
        assert_eq!(scalpel.host, "ubu");
        assert!(scalpel.daily_cost_limit_usd.is_none());
    }

    #[test]
    fn validates_permission_mode() {
        let mut cfg = CcBridgeConfig::from_toml_str(SAMPLE).expect("parse");
        cfg.personas[0].permission_mode = "yolo".into();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("invalid permission_mode"), "{err}");
    }

    #[test]
    fn validates_unique_persona_names() {
        let mut cfg = CcBridgeConfig::from_toml_str(SAMPLE).expect("parse");
        cfg.personas[1].name = "archon".into();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("duplicate persona name"), "{err}");
    }

    #[test]
    fn validates_subject_token() {
        let mut cfg = CcBridgeConfig::from_toml_str(SAMPLE).expect("parse");
        cfg.personas[0].name = "with.dot".into();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("valid NATS subject token"), "{err}");
    }

    #[test]
    fn validates_empty_personas_list() {
        let toml = r#"
[bridge]
name = "empty"

[bus]
nats_url = "nats://lnx-rig:4222"
"#;
        let cfg = CcBridgeConfig::from_toml_str(toml).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("zero personas"), "{err}");
    }

    #[test]
    fn dispatch_wildcard_matches_root() {
        let cfg = CcBridgeConfig::from_toml_str(SAMPLE).expect("parse");
        assert_eq!(cfg.dispatch_wildcard(), "cc.dispatch.*");
        assert_eq!(cfg.cancel_wildcard(), "cc.cancel.*");
        assert_eq!(cfg.cost_subject("archon"), "cc.event.archon.cost");
    }

    #[test]
    fn expand_tildes_resolves_home() {
        let mut cfg = CcBridgeConfig::from_toml_str(SAMPLE).expect("parse");
        cfg.expand_tildes();
        let p = cfg.personas[0]
            .system_prompt_file
            .as_ref()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(!p.starts_with('~'), "tilde should be expanded: {p}");
        assert!(
            p.contains("/.config/voicectl/cc/prompts/archon.md"),
            "unexpected expansion: {p}"
        );
    }

    #[test]
    fn is_local_recognises_localhost_variants() {
        let mk = |host: &str| Persona {
            name: "p".into(),
            host: host.into(),
            cwd: "/tmp".into(),
            permission_mode: "default".into(),
            system_prompt_file: None,
            daily_cost_limit_usd: None,
            timeout_sec: 600,
            claude_bin: None,
        };
        assert!(mk("").is_local());
        assert!(mk("localhost").is_local());
        assert!(mk("127.0.0.1").is_local());
        assert!(!mk("lnx-rig").is_local());
        assert!(!mk("ubu").is_local());
    }

    #[test]
    fn missing_required_section_fails_clearly() {
        let toml = r#"
[bridge]
name = "x"
"#;
        let err = CcBridgeConfig::from_toml_str(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bus") || msg.contains("missing field"),
            "expected field-level error, got: {msg}"
        );
    }
}
