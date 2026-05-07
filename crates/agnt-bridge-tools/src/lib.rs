//! agnt-bridge-tools — system-level tools for agnt-bridge.
//!
//! These tools let an agent driven by `agnt-bridge` actually do things on
//! the host: launch apps, read the clipboard, take screenshots, search the
//! web, recall persistent memory, and dispatch tasks to other agents on
//! the bus.
//!
//! ## Safety posture
//!
//! - **Read-only / additive tools** (`open_app`, `notification`,
//!   `current_window`, `screenshot`, `clipboard_get`, `web_search`, etc.)
//!   are unconditionally available. They cannot overwrite user state.
//! - **Destructive computer-use tools** (`click`, `type_text`, `key_combo`,
//!   `scroll`, `focus_window`) live in [`computer`] and route every call
//!   through a [`computer::SafetyPolicy`] gate that publishes a
//!   `voice.system.confirm.request` and waits for an explicit
//!   `voice.system.confirm.reply` before executing. Default mode is
//!   `confirm`; never ship `off`.
//! - **Read-only computer-use** (`get_mouse`) is NOT gated.
//!
//! ## Registration
//!
//! Each tool is exposed through [`build_tools`] which returns
//! `Box<dyn agnt::Tool>` instances ready to hand to `agnt::AgentBuilder`.
//! The bridge enables a subset by name from its TOML config.
//!
//! ## Async vs sync
//!
//! agnt-rs's `Tool` trait is sync. Most of these tools shell out to a
//! subprocess; we use `tokio::process::Command` and block on the current
//! tokio runtime via `Handle::current().block_on(…)`. The bridge always
//! invokes tools from inside `spawn_blocking`, so this is safe.

pub mod computer;
pub mod desktop;
pub mod dispatch;
pub mod memory;
pub mod search;
pub mod shell;
pub mod vision;

use std::sync::Arc;

pub use computer::{
    Click, FocusWindow, GetMouse, KeyCombo, SafetyMode, SafetyPolicy, Scroll, TypeText,
};
pub use desktop::{ClipboardGet, CurrentWindow, Notification, OpenApp, OpenUrl, Screenshot};
pub use dispatch::DispatchAgent;
pub use memory::{MemctlIngest, MemctlRecall};
pub use search::{SearchConfig, WebSearch};
pub use vision::{LookAtScreen, VisionConfig};

/// Names of every tool exposed by this crate. Useful for config validation.
pub const ALL_TOOLS: &[&str] = &[
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
    "click",
    "type_text",
    "key_combo",
    "scroll",
    "focus_window",
    "get_mouse",
    "look_at_screen",
];

/// Configuration for the system-tool surface.
///
/// Defaults work on a stock ubu desktop. `searxng_url` and the dispatch
/// bus only matter if you enable the relevant tools.
#[derive(Clone, Debug)]
pub struct SystemToolsConfig {
    /// Where SearXNG lives. Used by `web_search`. Default: `http://lnx-rig:8888`.
    pub searxng_url: String,
    /// Path to memctl. Default: `~/.local/bin/memctl`.
    pub memctl_bin: std::path::PathBuf,
    /// Cache directory used by tools that write artefacts (`screenshot`,
    /// `look_at_screen`).
    /// Default: `~/.cache/voicectl`.
    pub cache_dir: std::path::PathBuf,
    /// Safety policy applied to destructive computer-use tools (click,
    /// type_text, key_combo, scroll, focus_window). Defaults to
    /// `SafetyMode::Confirm` — every destructive call publishes a
    /// confirmation request and waits for explicit approval.
    pub computer_use_safety: SafetyPolicy,
}

impl Default for SystemToolsConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
        Self {
            searxng_url: "http://lnx-rig:8888".into(),
            memctl_bin: home.join(".local/bin/memctl"),
            cache_dir: home.join(".cache/voicectl"),
            computer_use_safety: SafetyPolicy::default(),
        }
    }
}

/// Build a single tool by name. Unknown names return `None`.
///
/// `dispatch_bus` is an optional handle to the NATS bus. Without it,
/// `dispatch_agent` AND every destructive computer-use tool is omitted —
/// destructive tools require a bus to publish their confirmation requests.
/// Read-only and additive tools work regardless.
pub fn build_tool(
    name: &str,
    cfg: &SystemToolsConfig,
    dispatch_bus: Option<Arc<voicectl_net::Bus>>,
) -> Option<Box<dyn agnt::Tool>> {
    let policy = Arc::new(cfg.computer_use_safety.clone());
    match name {
        "open_app" => Some(Box::new(agnt::ErasedAdapter::new(OpenApp))),
        "open_url" => Some(Box::new(agnt::ErasedAdapter::new(OpenUrl))),
        "notification" => Some(Box::new(agnt::ErasedAdapter::new(Notification))),
        "current_window" => Some(Box::new(agnt::ErasedAdapter::new(CurrentWindow))),
        "screenshot" => Some(Box::new(Screenshot::new(cfg.cache_dir.clone()))),
        "clipboard_get" => Some(Box::new(agnt::ErasedAdapter::new(ClipboardGet))),
        "web_search" => Some(Box::new(WebSearch::new(SearchConfig {
            searxng_url: cfg.searxng_url.clone(),
        }))),
        "memctl_recall" => Some(Box::new(MemctlRecall::new(cfg.memctl_bin.clone()))),
        "memctl_ingest" => Some(Box::new(MemctlIngest::new(cfg.memctl_bin.clone()))),
        "dispatch_agent" => dispatch_bus
            .clone()
            .map(|bus| Box::new(DispatchAgent::new(bus)) as Box<dyn agnt::Tool>),
        // Read-only computer use — never gated.
        "get_mouse" => Some(Box::new(GetMouse)),
        // Destructive computer use — require a bus for the confirm channel.
        "click" => dispatch_bus
            .clone()
            .map(|bus| Box::new(Click::new(bus, Arc::clone(&policy))) as Box<dyn agnt::Tool>),
        "type_text" => dispatch_bus
            .clone()
            .map(|bus| Box::new(TypeText::new(bus, Arc::clone(&policy))) as Box<dyn agnt::Tool>),
        "key_combo" => dispatch_bus
            .clone()
            .map(|bus| Box::new(KeyCombo::new(bus, Arc::clone(&policy))) as Box<dyn agnt::Tool>),
        "scroll" => dispatch_bus
            .clone()
            .map(|bus| Box::new(Scroll::new(bus, Arc::clone(&policy))) as Box<dyn agnt::Tool>),
        "focus_window" => dispatch_bus
            .map(|bus| Box::new(FocusWindow::new(bus, Arc::clone(&policy))) as Box<dyn agnt::Tool>),
        // look_at_screen dispatches over NATS — requires a bus.
        "look_at_screen" => dispatch_bus
            .clone()
            .map(|bus| {
                Box::new(LookAtScreen::new(
                    VisionConfig {
                        cache_dir: cfg.cache_dir.clone(),
                    },
                    bus,
                )) as Box<dyn agnt::Tool>
            }),
        _ => None,
    }
}

/// Build every requested tool. Names that don't match a known tool are
/// returned in the second slot so the caller can warn-log them.
pub fn build_tools(
    names: &[String],
    cfg: &SystemToolsConfig,
    dispatch_bus: Option<Arc<voicectl_net::Bus>>,
) -> (Vec<Box<dyn agnt::Tool>>, Vec<String>) {
    let mut tools = Vec::new();
    let mut unknown = Vec::new();
    for name in names {
        match build_tool(name, cfg, dispatch_bus.clone()) {
            Some(t) => tools.push(t),
            None => unknown.push(name.clone()),
        }
    }
    (tools, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_tools_constants_match_build_tool() {
        let cfg = SystemToolsConfig::default();
        // Bus-dependent tools — confirm publish channel, agent dispatch, or vzn vision.
        let needs_bus = [
            "dispatch_agent",
            "click",
            "type_text",
            "key_combo",
            "scroll",
            "focus_window",
            "look_at_screen",
        ];
        for name in ALL_TOOLS {
            if needs_bus.contains(name) {
                continue;
            }
            assert!(
                build_tool(name, &cfg, None).is_some(),
                "build_tool({name}) returned None — ALL_TOOLS is out of sync"
            );
        }
    }

    #[test]
    fn bus_required_tools_refuse_without_bus() {
        let cfg = SystemToolsConfig::default();
        for name in [
            "click",
            "type_text",
            "key_combo",
            "scroll",
            "focus_window",
            "look_at_screen",
        ] {
            assert!(
                build_tool(name, &cfg, None).is_none(),
                "{name} should refuse to build without a Bus"
            );
        }
        // get_mouse is read-only and works without a bus.
        assert!(build_tool("get_mouse", &cfg, None).is_some());
    }

    #[test]
    fn unknown_tool_is_reported() {
        let cfg = SystemToolsConfig::default();
        let names = vec!["open_app".to_string(), "nope".to_string()];
        let (tools, unknown) = build_tools(&names, &cfg, None);
        assert_eq!(tools.len(), 1);
        assert_eq!(unknown, vec!["nope".to_string()]);
    }
}
