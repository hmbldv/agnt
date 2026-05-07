//! Computer-use tools — click, type, key combos, scroll, window focus.
//!
//! These are the **first destructive tools** in the agnt-bridge surface.
//! Voice-driven xdotool can wreck a session if Sage misinterprets a request,
//! so every destructive call goes through a [`SafetyPolicy`] gate that:
//!
//! 1. Builds a one-line preview of the action (`"type 'rm -rf' into Terminal"`).
//! 2. Publishes a [`ConfirmRequest`] on `agnt.confirm.request`.
//! 3. Subscribes to `agnt.confirm.reply` and waits up to 30 s for an
//!    approval message with the matching `request_id` (or the wildcard `"*"`,
//!    which always targets the most-recent request — the path used by the
//!    `voicectl confirm` CLI and voice approval).
//! 4. Either runs the xdotool command (approved) or returns `"user denied"`
//!    / `"no confirmation in 30s"`.
//!
//! [`SafetyMode::Smart`] adds two allow-list bypasses:
//!
//! - The combo argument of `key_combo` matches an entry in
//!   `computer_use_safe_keys` (e.g. `"Escape"`, `"alt+Tab"`).
//! - The currently-focused window's WM class is in
//!   `computer_use_safe_focus_apps` AND the action is `type_text` or
//!   `key_combo` (clicks and scrolls always confirm in smart mode).
//!
//! [`SafetyMode::Off`] executes immediately — testing only. The bridge
//! warn-logs at startup whenever it sees `off`.
//!
//! All shell-out paths use structured `tokio::process::Command` args (no
//! shell string interpolation), and the type/key arguments are validated for
//! injection markers before being passed to xdotool.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{debug, warn};

use agnt_core::wire::{subjects as wire_subjects, ConfirmReply, ConfirmRequest, RequestId};
use voicectl_net::Bus;

use crate::shell::{block_on, run_blocking, DEFAULT_TIMEOUT};

// ─────────────────────────────────────────────────────────────────────────────
// SafetyPolicy
// ─────────────────────────────────────────────────────────────────────────────

/// Wall-clock confirmation timeout. 30 s is long enough for the user to
/// glance at a tray notification or finish a sentence; short enough that a
/// stuck request doesn't pin the agent loop indefinitely.
pub const CONFIRM_TIMEOUT_SECS: u64 = 30;

/// Canonical subject for confirm requests (`agnt.confirm.request`).
pub fn confirm_request_subject() -> &'static str {
    wire_subjects::CONFIRM_REQUEST
}

/// Canonical subject for confirm replies (`agnt.confirm.reply`).
pub fn confirm_reply_subject() -> &'static str {
    wire_subjects::CONFIRM_REPLY
}

/// Three modes for the destructive tool gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyMode {
    /// Execute immediately — testing only. Bridge warn-logs at startup.
    Off,
    /// Every destructive call publishes a request and waits for approval.
    /// **Default.**
    #[default]
    Confirm,
    /// Confirm unless: (a) `key_combo` arg is in the safe-key allowlist, OR
    /// (b) the active window class is in the safe-focus-apps list AND the
    /// action is `type_text` / `key_combo`. Click and scroll always confirm.
    Smart,
}

impl SafetyMode {
    /// Parse from a TOML string value. Unknown strings fall back to the
    /// default (`confirm`) and are warn-logged by the caller.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "confirm" => Ok(Self::Confirm),
            "smart" => Ok(Self::Smart),
            other => Err(format!(
                "unknown computer_use_safety mode '{other}' (expected off/confirm/smart)"
            )),
        }
    }
}

/// Resolved safety configuration handed to every destructive tool.
#[derive(Clone, Debug)]
pub struct SafetyPolicy {
    pub mode: SafetyMode,
    /// Combos that bypass confirmation in `Smart` mode. Compared
    /// case-sensitively against the validated combo string.
    pub safe_keys: Vec<String>,
    /// WM-class names whose windows can receive `type_text` / `key_combo`
    /// without confirmation in `Smart` mode. Compared case-insensitively.
    pub safe_focus_apps: Vec<String>,
}

impl Default for SafetyPolicy {
    fn default() -> Self {
        Self {
            mode: SafetyMode::Confirm,
            safe_keys: default_safe_keys(),
            safe_focus_apps: default_safe_focus_apps(),
        }
    }
}

/// Conservative default allowlist — purely navigation, no destructive shortcuts.
pub fn default_safe_keys() -> Vec<String> {
    [
        "Escape",
        "Return",
        "Tab",
        "alt+Tab",
        "super+a",
        "super+s",
        "Page_Down",
        "Page_Up",
        "Home",
        "End",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Default safe-focus apps — terminals and editors where the user is already
/// expecting keyboard input. Comparing case-insensitively, but the canonical
/// WM_CLASS spelling is preserved here for documentation.
pub fn default_safe_focus_apps() -> Vec<String> {
    [
        "kitty",
        "Alacritty",
        "VSCode",
        "Code",
        "obsidian",
        "Obsidian",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Confirmation gate — shared across all destructive tools
// ─────────────────────────────────────────────────────────────────────────────

/// Final decision returned to the calling tool after the smart pre-check
/// and (if necessary) the NATS round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Run the action — either pre-approved by the allowlist or explicitly
    /// confirmed by the user.
    Allowed,
    /// The user explicitly denied the action.
    Denied,
    /// No reply within `CONFIRM_TIMEOUT_SECS`.
    Timeout,
    /// NATS publish/subscribe failed; treat as a soft failure.
    PublishFailed(String),
}

/// What kind of action we're gating. Used for smart-mode rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Click,
    Type,
    Key,
    Scroll,
    FocusWindow,
}

impl ActionKind {
    fn is_keyboard_like(self) -> bool {
        matches!(self, Self::Type | Self::Key)
    }
}

/// Pre-gate decision returned by [`evaluate_smart`] BEFORE NATS round-trip.
///
/// - `Bypass` — execute without confirmation (off mode, or smart-mode allowlist).
/// - `MustConfirm` — caller must publish the request and wait for a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmartDecision {
    Bypass(&'static str),
    MustConfirm,
}

/// Outcome of evaluating smart-mode policy without going to NATS. Pure
/// function so tests can drive every branch deterministically.
pub fn evaluate_smart(
    policy: &SafetyPolicy,
    action: ActionKind,
    combo: Option<&str>,
    focused_class: Option<&str>,
) -> SmartDecision {
    match policy.mode {
        SafetyMode::Off => SmartDecision::Bypass("safety mode = off"),
        SafetyMode::Confirm => SmartDecision::MustConfirm,
        SafetyMode::Smart => {
            if action == ActionKind::Key {
                if let Some(c) = combo {
                    if policy.safe_keys.iter().any(|k| k == c) {
                        return SmartDecision::Bypass("key in safe-key allowlist");
                    }
                }
            }
            if action.is_keyboard_like() {
                if let Some(class) = focused_class {
                    let lower = class.trim().to_ascii_lowercase();
                    if policy
                        .safe_focus_apps
                        .iter()
                        .any(|c| c.to_ascii_lowercase() == lower)
                    {
                        return SmartDecision::Bypass("focused app in safe list");
                    }
                }
            }
            // Click and scroll always confirm in smart mode (per spec).
            SmartDecision::MustConfirm
        }
    }
}

/// Publish a [`ConfirmRequest`] on the bus and wait for a matching
/// [`ConfirmReply`]. Returns the gate outcome.
///
/// `*`-wildcard reply: a reply with `request_id == "*"` is accepted and
/// treated as a match. This is the path the `voicectl confirm` CLI and voice
/// approval take — they don't know the active request id, so they target
/// "the most recent".
pub async fn publish_and_wait(bus: &Bus, tool: &str, args: Value, preview: String) -> GateOutcome {
    let request_id = RequestId::new();
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let expires_at_ns = now_ns.saturating_add(CONFIRM_TIMEOUT_SECS * 1_000_000_000);

    let req = ConfirmRequest {
        request_id: request_id.clone(),
        tool: tool.to_string(),
        args,
        preview,
        expires_at_ns,
    };

    // Subscribe FIRST so we never miss a fast reply.
    let reply_subj = confirm_reply_subject();
    let mut sub = match bus.client.subscribe(reply_subj.to_owned()).await {
        Ok(s) => s,
        Err(e) => {
            return GateOutcome::PublishFailed(format!("subscribe {reply_subj}: {e}"));
        }
    };

    let req_subj = confirm_request_subject();
    let payload = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => return GateOutcome::PublishFailed(format!("encode ConfirmRequest: {e}")),
    };
    if let Err(e) = bus.client.publish(req_subj.to_owned(), payload.into()).await {
        return GateOutcome::PublishFailed(format!("publish {req_subj}: {e}"));
    }
    debug!(request_id = %request_id, tool, "confirm.request published");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(CONFIRM_TIMEOUT_SECS);
    loop {
        match tokio::time::timeout_at(deadline, sub.next()).await {
            Ok(Some(msg)) => match serde_json::from_slice::<ConfirmReply>(&msg.payload) {
                Ok(reply) => {
                    // Deny-wins: any denial (matching or wildcard) terminates
                    // immediately without waiting for the window to close.
                    if !reply.approved
                        && (reply.request_id == request_id.to_string()
                            || reply.request_id == "*")
                    {
                        debug!(
                            request_id = %request_id,
                            source = ?reply.source,
                            "confirm.reply denied — deny-wins gate"
                        );
                        return GateOutcome::Denied;
                    }
                    if reply.request_id != request_id.to_string() && reply.request_id != "*" {
                        // Reply for a different request — ignore and keep waiting.
                        continue;
                    }
                    debug!(
                        request_id = %request_id,
                        approved = reply.approved,
                        source = ?reply.source,
                        "confirm.reply matched"
                    );
                    return GateOutcome::Allowed;
                }
                Err(e) => {
                    warn!(error = %e, "confirm.reply decode failed; ignoring frame");
                    continue;
                }
            },
            Ok(None) => return GateOutcome::Timeout,
            Err(_) => return GateOutcome::Timeout,
        }
    }
}

/// Bundle of gate inputs. Lifted into a struct to keep the public
/// `gate_action` arity manageable as we add more action context (e.g. AT-SPI
/// roles in a future iteration).
pub struct GateRequest<'a> {
    pub policy: &'a SafetyPolicy,
    pub bus: &'a Bus,
    pub action: ActionKind,
    pub combo: Option<&'a str>,
    pub focused_class: Option<&'a str>,
    pub tool: &'a str,
    pub args: Value,
    pub preview: String,
}

/// Top-level gate the tool implementations call. Combines the synchronous
/// smart pre-check with (if needed) a NATS confirm round-trip. Always
/// returns a definitive [`GateOutcome`].
pub fn gate_action(req: GateRequest<'_>) -> GateOutcome {
    match evaluate_smart(req.policy, req.action, req.combo, req.focused_class) {
        SmartDecision::Bypass(reason) => {
            debug!(
                reason,
                tool = req.tool,
                "computer-use bypass — no confirmation needed"
            );
            GateOutcome::Allowed
        }
        SmartDecision::MustConfirm => {
            block_on(publish_and_wait(req.bus, req.tool, req.args, req.preview))
        }
    }
}

/// Look up the WM class of the currently-focused window, or `None` if
/// xdotool fails.
fn active_window_class() -> Option<String> {
    let out = run_blocking(
        "xdotool",
        ["getactivewindow", "getwindowclassname"],
        DEFAULT_TIMEOUT,
    )
    .ok()?;
    if !out.status_ok {
        return None;
    }
    let s = out.stdout.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Look up the window class at a screen coordinate by mousemoving --sync and
/// reading getwindowclassname on the resulting WINDOW. Side-effect: this
/// moves the mouse pointer. Used by `click`'s preview, so the preview is
/// honest about where the click will land.
fn class_at_coord(x: u32, y: u32) -> Option<String> {
    let _ = run_blocking(
        "xdotool",
        ["mousemove", "--sync", &x.to_string(), &y.to_string()],
        DEFAULT_TIMEOUT,
    )
    .ok()?;
    let out = run_blocking("xdotool", ["getmouselocation", "--shell"], DEFAULT_TIMEOUT).ok()?;
    if !out.status_ok {
        return None;
    }
    let win_id = out.stdout.lines().find_map(|l| l.strip_prefix("WINDOW="))?;
    let class = run_blocking("xdotool", ["getwindowclassname", win_id], DEFAULT_TIMEOUT).ok()?;
    if !class.status_ok {
        return None;
    }
    let s = class.stdout.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// click
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ClickArgs {
    pub x: u32,
    pub y: u32,
    #[serde(default)]
    pub button: Option<String>,
    #[serde(default)]
    pub double: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ClickResult {
    clicked: String,
    over: Option<String>,
    button: String,
    double: bool,
}

/// `click(x, y, button="left", double=false)` — move the mouse and click.
pub struct Click {
    bus: Arc<Bus>,
    policy: Arc<SafetyPolicy>,
}

impl Click {
    pub fn new(bus: Arc<Bus>, policy: Arc<SafetyPolicy>) -> Self {
        Self { bus, policy }
    }
}

impl agnt::Tool for Click {
    fn name(&self) -> &str {
        "click"
    }

    fn description(&self) -> &str {
        "Move the mouse to (x, y) and click. button='left'|'right'|'middle' \
         (default left). Set double=true for a double-click. DESTRUCTIVE — \
         will request user confirmation unless the safety mode is configured \
         to bypass it."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "x": { "type": "integer", "minimum": 0, "description": "X screen coordinate." },
                "y": { "type": "integer", "minimum": 0, "description": "Y screen coordinate." },
                "button": {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "description": "Mouse button (default 'left')."
                },
                "double": {
                    "type": "boolean",
                    "description": "If true, performs a double-click."
                }
            },
            "required": ["x", "y"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let parsed: ClickArgs =
            serde_json::from_value(args.clone()).map_err(|e| format!("args: {e}"))?;
        let button = parsed.button.as_deref().unwrap_or("left");
        let button_num = match button {
            "left" => "1",
            "middle" => "2",
            "right" => "3",
            other => return Err(format!("unknown button '{other}'")),
        };
        let double = parsed.double.unwrap_or(false);

        let class = class_at_coord(parsed.x, parsed.y);
        let preview = format!(
            "{}{}-click at ({}, {}){}",
            if double { "double " } else { "" },
            button,
            parsed.x,
            parsed.y,
            class
                .as_deref()
                .map(|c| format!(" over {c}"))
                .unwrap_or_default()
        );

        let outcome = gate_action(GateRequest {
            policy: &self.policy,
            bus: &self.bus,
            action: ActionKind::Click,
            combo: None,
            focused_class: class.as_deref(),
            tool: "click",
            args,
            preview,
        });
        match outcome {
            GateOutcome::Allowed => {}
            GateOutcome::Denied => return Err("user denied click".into()),
            GateOutcome::Timeout => return Err("no confirmation in 30s".into()),
            GateOutcome::PublishFailed(e) => return Err(format!("confirm publish failed: {e}")),
        }

        // Move + click. We always do mousemove --sync first so the click is
        // deterministic; xdotool's `click` alone clicks wherever the pointer
        // happens to be.
        let mv = run_blocking(
            "xdotool",
            [
                "mousemove",
                "--sync",
                &parsed.x.to_string(),
                &parsed.y.to_string(),
            ],
            DEFAULT_TIMEOUT,
        )?;
        if !mv.status_ok {
            return Err(format!("xdotool mousemove: {}", mv.stderr.trim()));
        }
        let click_args: Vec<String> = if double {
            vec![
                "click".into(),
                "--repeat".into(),
                "2".into(),
                button_num.into(),
            ]
        } else {
            vec!["click".into(), button_num.into()]
        };
        let cl = run_blocking("xdotool", &click_args, DEFAULT_TIMEOUT)?;
        if !cl.status_ok {
            return Err(format!("xdotool click: {}", cl.stderr.trim()));
        }

        let result = ClickResult {
            clicked: format!("({}, {})", parsed.x, parsed.y),
            over: class,
            button: button.to_string(),
            double,
        };
        serde_json::to_string(&result).map_err(|e| format!("encode result: {e}"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// type_text
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TypeTextArgs {
    pub text: String,
}

#[derive(Debug, Serialize)]
struct TypeTextResult {
    typed_chars: usize,
    over: Option<String>,
}

const TYPE_TEXT_MAX: usize = 4096;

/// `type_text(text)` — synthesise typing into the focused window.
pub struct TypeText {
    bus: Arc<Bus>,
    policy: Arc<SafetyPolicy>,
}

impl TypeText {
    pub fn new(bus: Arc<Bus>, policy: Arc<SafetyPolicy>) -> Self {
        Self { bus, policy }
    }
}

/// Validate text for xdotool flag-injection markers. We pass `--` before the
/// text so it can never be interpreted as a flag, but we still reject the
/// `--clearmodifiers` literal and a bare leading `-` to make accidental
/// misuse loud rather than silent.
pub fn validate_typed_text(text: &str) -> Result<(), String> {
    if text.contains("--clearmodifiers") {
        return Err("text must not contain '--clearmodifiers'".into());
    }
    if text.starts_with('-') {
        return Err(
            "text must not start with '-' (would look like an xdotool flag despite -- guard)"
                .into(),
        );
    }
    Ok(())
}

impl agnt::Tool for TypeText {
    fn name(&self) -> &str {
        "type_text"
    }

    fn description(&self) -> &str {
        "Synthesise keyboard typing into the currently-focused window. Capped \
         at 4096 characters. DESTRUCTIVE — will request user confirmation \
         unless the focused window's WM class is in the safe-focus-apps \
         allowlist (smart mode) or safety mode is off."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "Text to type. Max 4096 chars. Cannot start with '-' or contain '--clearmodifiers'."
                }
            },
            "required": ["text"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let parsed: TypeTextArgs =
            serde_json::from_value(args.clone()).map_err(|e| format!("args: {e}"))?;
        if parsed.text.is_empty() {
            return Err("text must not be empty".into());
        }
        if parsed.text.chars().count() > TYPE_TEXT_MAX {
            return Err(format!("text exceeds {TYPE_TEXT_MAX}-char cap"));
        }
        validate_typed_text(&parsed.text)?;

        let class = active_window_class();
        let preview = format!(
            "type {} chars{}",
            parsed.text.chars().count(),
            class
                .as_deref()
                .map(|c| format!(" into {c}"))
                .unwrap_or_default()
        );

        let outcome = gate_action(GateRequest {
            policy: &self.policy,
            bus: &self.bus,
            action: ActionKind::Type,
            combo: None,
            focused_class: class.as_deref(),
            tool: "type_text",
            args,
            preview,
        });
        match outcome {
            GateOutcome::Allowed => {}
            GateOutcome::Denied => return Err("user denied type_text".into()),
            GateOutcome::Timeout => return Err("no confirmation in 30s".into()),
            GateOutcome::PublishFailed(e) => return Err(format!("confirm publish failed: {e}")),
        }

        // `--` separates options from positional args so the text can never
        // be parsed as a flag.
        let out = run_blocking(
            "xdotool",
            ["type", "--delay", "30", "--", &parsed.text],
            Duration::from_secs(30),
        )?;
        if !out.status_ok {
            return Err(format!("xdotool type: {}", out.stderr.trim()));
        }

        let result = TypeTextResult {
            typed_chars: parsed.text.chars().count(),
            over: class,
        };
        serde_json::to_string(&result).map_err(|e| format!("encode result: {e}"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// key_combo
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct KeyComboArgs {
    pub combo: String,
}

#[derive(Debug, Serialize)]
struct KeyComboResult {
    combo: String,
    over: Option<String>,
}

/// `key_combo(combo)` — press a key or combination via `xdotool key`.
pub struct KeyCombo {
    bus: Arc<Bus>,
    policy: Arc<SafetyPolicy>,
}

impl KeyCombo {
    pub fn new(bus: Arc<Bus>, policy: Arc<SafetyPolicy>) -> Self {
        Self { bus, policy }
    }
}

/// Combo charset: A-Z, a-z, 0-9, `_` and `+`. xdotool key syms (`Return`,
/// `Page_Down`, `ctrl+c`) all fit. Anything else (spaces, semicolons, dots,
/// shell metachars) is rejected.
pub fn validate_combo(combo: &str) -> Result<(), String> {
    if combo.is_empty() {
        return Err("combo must not be empty".into());
    }
    if !combo
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '+')
    {
        return Err(format!(
            "combo '{combo}' contains disallowed characters (only A-Z, a-z, 0-9, '_', '+')"
        ));
    }
    Ok(())
}

impl agnt::Tool for KeyCombo {
    fn name(&self) -> &str {
        "key_combo"
    }

    fn description(&self) -> &str {
        "Press a single key or modifier combination via xdotool. Examples: \
         'Return', 'Escape', 'ctrl+c', 'alt+Tab', 'super+l'. The combo string \
         is validated for shell-metachar injection (only A-Z, a-z, 0-9, '_', \
         '+'). DESTRUCTIVE — confirmation required unless the combo is in \
         the safe-key allowlist (smart mode) or safety is off."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "combo": {
                    "type": "string",
                    "description": "Key combo string. Only A-Z, a-z, 0-9, '_', '+' allowed."
                }
            },
            "required": ["combo"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let parsed: KeyComboArgs =
            serde_json::from_value(args.clone()).map_err(|e| format!("args: {e}"))?;
        validate_combo(&parsed.combo)?;

        let class = active_window_class();
        let preview = format!(
            "press {}{}",
            parsed.combo,
            class
                .as_deref()
                .map(|c| format!(" over {c}"))
                .unwrap_or_default()
        );

        let outcome = gate_action(GateRequest {
            policy: &self.policy,
            bus: &self.bus,
            action: ActionKind::Key,
            combo: Some(&parsed.combo),
            focused_class: class.as_deref(),
            tool: "key_combo",
            args,
            preview,
        });
        match outcome {
            GateOutcome::Allowed => {}
            GateOutcome::Denied => return Err("user denied key_combo".into()),
            GateOutcome::Timeout => return Err("no confirmation in 30s".into()),
            GateOutcome::PublishFailed(e) => return Err(format!("confirm publish failed: {e}")),
        }

        let out = run_blocking(
            "xdotool",
            ["key", "--clearmodifiers", &parsed.combo],
            DEFAULT_TIMEOUT,
        )?;
        if !out.status_ok {
            return Err(format!("xdotool key: {}", out.stderr.trim()));
        }

        let result = KeyComboResult {
            combo: parsed.combo,
            over: class,
        };
        serde_json::to_string(&result).map_err(|e| format!("encode result: {e}"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// scroll
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ScrollArgs {
    pub direction: String,
    #[serde(default)]
    pub amount: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ScrollResult {
    direction: String,
    amount: u32,
}

/// `scroll(direction, amount=3)` — scroll wheel via xdotool button repeats.
pub struct Scroll {
    bus: Arc<Bus>,
    policy: Arc<SafetyPolicy>,
}

impl Scroll {
    pub fn new(bus: Arc<Bus>, policy: Arc<SafetyPolicy>) -> Self {
        Self { bus, policy }
    }
}

impl agnt::Tool for Scroll {
    fn name(&self) -> &str {
        "scroll"
    }

    fn description(&self) -> &str {
        "Scroll the wheel in a direction. direction='up'|'down'|'left'|'right', \
         amount is the number of click ticks (default 3). DESTRUCTIVE — \
         scroll always confirms in smart and confirm modes."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "direction": {
                    "type": "string",
                    "enum": ["up", "down", "left", "right"]
                },
                "amount": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "description": "Click ticks. Default 3, max 50."
                }
            },
            "required": ["direction"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let parsed: ScrollArgs =
            serde_json::from_value(args.clone()).map_err(|e| format!("args: {e}"))?;
        let amount = parsed.amount.unwrap_or(3).clamp(1, 50);
        let button = match parsed.direction.as_str() {
            "up" => "4",
            "down" => "5",
            "left" => "6",
            "right" => "7",
            other => return Err(format!("unknown direction '{other}'")),
        };

        let class = active_window_class();
        let preview = format!(
            "scroll {} x{}{}",
            parsed.direction,
            amount,
            class
                .as_deref()
                .map(|c| format!(" over {c}"))
                .unwrap_or_default()
        );

        let outcome = gate_action(GateRequest {
            policy: &self.policy,
            bus: &self.bus,
            action: ActionKind::Scroll,
            combo: None,
            focused_class: class.as_deref(),
            tool: "scroll",
            args,
            preview,
        });
        match outcome {
            GateOutcome::Allowed => {}
            GateOutcome::Denied => return Err("user denied scroll".into()),
            GateOutcome::Timeout => return Err("no confirmation in 30s".into()),
            GateOutcome::PublishFailed(e) => return Err(format!("confirm publish failed: {e}")),
        }

        let out = run_blocking(
            "xdotool",
            ["click", "--repeat", &amount.to_string(), button],
            DEFAULT_TIMEOUT,
        )?;
        if !out.status_ok {
            return Err(format!("xdotool scroll: {}", out.stderr.trim()));
        }

        let result = ScrollResult {
            direction: parsed.direction,
            amount,
        };
        serde_json::to_string(&result).map_err(|e| format!("encode result: {e}"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// focus_window
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct FocusWindowArgs {
    pub query: String,
}

#[derive(Debug, Serialize)]
struct FocusWindowResult {
    focused: String,
}

/// `focus_window(query)` — find and activate a window by title substring.
pub struct FocusWindow {
    bus: Arc<Bus>,
    policy: Arc<SafetyPolicy>,
}

impl FocusWindow {
    pub fn new(bus: Arc<Bus>, policy: Arc<SafetyPolicy>) -> Self {
        Self { bus, policy }
    }
}

impl agnt::Tool for FocusWindow {
    fn name(&self) -> &str {
        "focus_window"
    }

    fn description(&self) -> &str {
        "Find a window by title substring (xdotool search --name) and \
         activate the first match. The query is treated as a regex by \
         xdotool, so anchor it or use literal substrings. DESTRUCTIVE — \
         confirmation required unless safety is off."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Window-title regex. Plain substrings work — they're treated as literal text."
                }
            },
            "required": ["query"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let parsed: FocusWindowArgs =
            serde_json::from_value(args.clone()).map_err(|e| format!("args: {e}"))?;
        let query = parsed.query.trim();
        if query.is_empty() {
            return Err("query must not be empty".into());
        }
        // The query is passed straight to xdotool as a positional arg, never
        // a shell — it only needs to be non-empty and reasonably bounded.
        if query.len() > 256 {
            return Err("query exceeds 256-char cap".into());
        }

        let preview = format!("focus window matching '{query}'");

        let outcome = gate_action(GateRequest {
            policy: &self.policy,
            bus: &self.bus,
            action: ActionKind::FocusWindow,
            combo: None,
            focused_class: None,
            tool: "focus_window",
            args,
            preview,
        });
        match outcome {
            GateOutcome::Allowed => {}
            GateOutcome::Denied => return Err("user denied focus_window".into()),
            GateOutcome::Timeout => return Err("no confirmation in 30s".into()),
            GateOutcome::PublishFailed(e) => return Err(format!("confirm publish failed: {e}")),
        }

        let out = run_blocking(
            "xdotool",
            ["search", "--name", query, "windowactivate", "%1"],
            DEFAULT_TIMEOUT,
        )?;
        if !out.status_ok {
            return Err(format!(
                "xdotool search/activate: {} {}",
                out.stdout.trim(),
                out.stderr.trim()
            ));
        }
        let result = FocusWindowResult {
            focused: query.to_string(),
        };
        serde_json::to_string(&result).map_err(|e| format!("encode result: {e}"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// get_mouse — read-only, NOT gated by safety
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct GetMouseResult {
    x: i64,
    y: i64,
    window_class: Option<String>,
    window_title: Option<String>,
}

/// `get_mouse()` — return the current pointer coordinate + the window the
/// pointer is currently over. Read-only; never confirmed.
pub struct GetMouse;

impl agnt::Tool for GetMouse {
    fn name(&self) -> &str {
        "get_mouse"
    }

    fn description(&self) -> &str {
        "Return the current mouse pointer coordinate and the window class + \
         title the pointer is currently over. Read-only; safe to call any time."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn call(&self, _args: Value) -> Result<String, String> {
        let out = run_blocking("xdotool", ["getmouselocation", "--shell"], DEFAULT_TIMEOUT)?;
        if !out.status_ok {
            return Err(format!("xdotool getmouselocation: {}", out.stderr.trim()));
        }
        let mut x: i64 = 0;
        let mut y: i64 = 0;
        let mut win_id: Option<String> = None;
        for line in out.stdout.lines() {
            if let Some(v) = line.strip_prefix("X=") {
                x = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("Y=") {
                y = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("WINDOW=") {
                win_id = Some(v.trim().to_string());
            }
        }
        let (window_class, window_title) = match &win_id {
            Some(id) => (
                run_blocking("xdotool", ["getwindowclassname", id], DEFAULT_TIMEOUT)
                    .ok()
                    .filter(|o| o.status_ok)
                    .map(|o| o.stdout.trim().to_string())
                    .filter(|s| !s.is_empty()),
                run_blocking("xdotool", ["getwindowname", id], DEFAULT_TIMEOUT)
                    .ok()
                    .filter(|o| o.status_ok)
                    .map(|o| o.stdout.trim().to_string())
                    .filter(|s| !s.is_empty()),
            ),
            None => (None, None),
        };
        let result = GetMouseResult {
            x,
            y,
            window_class,
            window_title,
        };
        serde_json::to_string(&result).map_err(|e| format!("encode result: {e}"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safety_mode_parses_known_strings() {
        assert_eq!(SafetyMode::parse("off").unwrap(), SafetyMode::Off);
        assert_eq!(SafetyMode::parse("confirm").unwrap(), SafetyMode::Confirm);
        assert_eq!(SafetyMode::parse("smart").unwrap(), SafetyMode::Smart);
        assert_eq!(SafetyMode::parse("  Smart  ").unwrap(), SafetyMode::Smart);
        assert_eq!(SafetyMode::parse("CONFIRM").unwrap(), SafetyMode::Confirm);
    }

    #[test]
    fn safety_mode_rejects_unknown_strings() {
        assert!(SafetyMode::parse("paranoid").is_err());
        assert!(SafetyMode::parse("").is_err());
        assert!(SafetyMode::parse("yolo").is_err());
    }

    #[test]
    fn safety_mode_default_is_confirm() {
        assert_eq!(SafetyMode::default(), SafetyMode::Confirm);
    }

    #[test]
    fn validate_combo_rejects_shell_injection() {
        let bad = [
            "Return; rm -rf /",
            "Return && curl evil",
            "ctrl+c | nc",
            "key with space",
            "key.with.dot",
            "",
            "key/with/slash",
            "key`backtick`",
            "key$(cmd)",
        ];
        for c in bad {
            assert!(validate_combo(c).is_err(), "expected '{c}' rejected");
        }
    }

    #[test]
    fn validate_combo_accepts_real_combos() {
        let good = [
            "Return",
            "Escape",
            "Tab",
            "ctrl+c",
            "alt+Tab",
            "super+l",
            "Page_Down",
        ];
        for c in good {
            validate_combo(c).expect(c);
        }
    }

    #[test]
    fn validate_typed_text_rejects_clearmodifiers_marker() {
        assert!(validate_typed_text("hello --clearmodifiers world").is_err());
    }

    #[test]
    fn validate_typed_text_rejects_leading_dash() {
        assert!(validate_typed_text("-rm -rf").is_err());
    }

    #[test]
    fn validate_typed_text_accepts_normal_text() {
        validate_typed_text("hello world").unwrap();
        validate_typed_text("rm -rf").unwrap(); // not leading
        validate_typed_text("SELECT * FROM users").unwrap();
    }

    // ── Smart-mode evaluation matrix ────────────────────────────────────────

    fn smart_policy() -> SafetyPolicy {
        SafetyPolicy {
            mode: SafetyMode::Smart,
            safe_keys: vec!["Escape".into(), "Return".into(), "alt+Tab".into()],
            safe_focus_apps: vec!["kitty".into(), "Code".into()],
        }
    }

    #[test]
    fn smart_safe_key_bypasses_confirm() {
        let p = smart_policy();
        assert!(matches!(
            evaluate_smart(&p, ActionKind::Key, Some("Escape"), None),
            SmartDecision::Bypass(_)
        ));
        assert!(matches!(
            evaluate_smart(&p, ActionKind::Key, Some("alt+Tab"), Some("Slack")),
            SmartDecision::Bypass(_)
        ));
    }

    #[test]
    fn smart_unsafe_key_in_unsafe_app_requires_confirm() {
        let p = smart_policy();
        let outcome = evaluate_smart(&p, ActionKind::Key, Some("ctrl+w"), Some("Slack"));
        assert_eq!(outcome, SmartDecision::MustConfirm);
    }

    #[test]
    fn smart_safe_app_bypasses_type() {
        let p = smart_policy();
        let outcome = evaluate_smart(&p, ActionKind::Type, None, Some("kitty"));
        assert!(matches!(outcome, SmartDecision::Bypass(_)));
    }

    #[test]
    fn smart_safe_app_bypasses_key_too() {
        let p = smart_policy();
        let outcome = evaluate_smart(&p, ActionKind::Key, Some("ctrl+w"), Some("Code"));
        assert!(matches!(outcome, SmartDecision::Bypass(_)));
    }

    #[test]
    fn smart_unsafe_app_with_unsafe_key_requires_confirm() {
        let p = smart_policy();
        let outcome = evaluate_smart(&p, ActionKind::Type, None, Some("Slack"));
        assert_eq!(outcome, SmartDecision::MustConfirm);
    }

    #[test]
    fn smart_click_always_requires_confirm() {
        let p = smart_policy();
        // Even over a safe app, click never bypasses.
        let outcome = evaluate_smart(&p, ActionKind::Click, None, Some("kitty"));
        assert_eq!(outcome, SmartDecision::MustConfirm);
    }

    #[test]
    fn smart_scroll_always_requires_confirm() {
        let p = smart_policy();
        let outcome = evaluate_smart(&p, ActionKind::Scroll, None, Some("kitty"));
        assert_eq!(outcome, SmartDecision::MustConfirm);
    }

    #[test]
    fn off_mode_bypasses_everything() {
        let p = SafetyPolicy {
            mode: SafetyMode::Off,
            safe_keys: vec![],
            safe_focus_apps: vec![],
        };
        let outcome = evaluate_smart(&p, ActionKind::Click, None, None);
        assert!(matches!(outcome, SmartDecision::Bypass(_)));
    }

    #[test]
    fn confirm_mode_always_requires_confirm() {
        let p = SafetyPolicy {
            mode: SafetyMode::Confirm,
            safe_keys: vec!["Escape".into()],
            safe_focus_apps: vec!["kitty".into()],
        };
        // Even with the key in the safe list and a safe app, confirm mode
        // always requires confirmation.
        let outcome = evaluate_smart(&p, ActionKind::Key, Some("Escape"), Some("kitty"));
        assert_eq!(outcome, SmartDecision::MustConfirm);
    }

    #[test]
    fn smart_safe_app_match_is_case_insensitive() {
        let p = smart_policy();
        let outcome = evaluate_smart(&p, ActionKind::Type, None, Some("KITTY"));
        assert!(matches!(outcome, SmartDecision::Bypass(_)));
    }

    #[test]
    fn confirm_subjects_use_canonical_agnt_namespace() {
        assert_eq!(confirm_request_subject(), "agnt.confirm.request");
        assert_eq!(confirm_reply_subject(), "agnt.confirm.reply");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Integration tests — real NATS confirm flow. Marked #[ignore]; run with
// `cargo test --workspace -- --ignored` against a local nats-server.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod integration_tests {
    use super::*;

    /// Connect to the local NATS, otherwise skip.
    async fn try_bus() -> Option<Arc<Bus>> {
        match Bus::connect("nats://127.0.0.1:4222", "voice").await {
            Ok(b) => Some(Arc::new(b)),
            Err(_) => None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires local nats-server on 127.0.0.1:4222"]
    async fn full_confirm_flow_approves() {
        let bus = match try_bus().await {
            Some(b) => b,
            None => return, // skip — no NATS available
        };
        let bus_for_approver = Arc::clone(&bus);
        // Spawn an approver that listens for the request and sends approve.
        let approver = tokio::spawn(async move {
            let mut sub = bus_for_approver
                .client
                .subscribe(confirm_request_subject().to_owned())
                .await
                .expect("subscribe");
            let msg = sub.next().await.expect("got request");
            let req: ConfirmRequest = serde_json::from_slice(&msg.payload).expect("decode");
            let reply = ConfirmReply {
                request_id: req.request_id.to_string(),
                approved: true,
                source: Some("test".into()),
            };
            let bytes = serde_json::to_vec(&reply).unwrap();
            bus_for_approver
                .client
                .publish(confirm_reply_subject().to_owned(), bytes.into())
                .await
                .unwrap();
        });

        let outcome = publish_and_wait(
            &bus,
            "click",
            json!({"x": 0, "y": 0}),
            "click at (0, 0)".into(),
        )
        .await;
        approver.await.unwrap();
        assert_eq!(outcome, GateOutcome::Allowed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires local nats-server on 127.0.0.1:4222"]
    async fn full_confirm_flow_denies() {
        let bus = match try_bus().await {
            Some(b) => b,
            None => return,
        };
        let bus_for_denier = Arc::clone(&bus);
        let denier = tokio::spawn(async move {
            let mut sub = bus_for_denier
                .client
                .subscribe(confirm_request_subject().to_owned())
                .await
                .expect("subscribe");
            let msg = sub.next().await.expect("got request");
            let req: ConfirmRequest = serde_json::from_slice(&msg.payload).expect("decode");
            let reply = ConfirmReply {
                request_id: req.request_id.to_string(),
                approved: false,
                source: Some("test".into()),
            };
            let bytes = serde_json::to_vec(&reply).unwrap();
            bus_for_denier
                .client
                .publish(confirm_reply_subject().to_owned(), bytes.into())
                .await
                .unwrap();
        });
        let outcome = publish_and_wait(
            &bus,
            "type_text",
            json!({"text": "rm -rf /"}),
            "type 'rm -rf /'".into(),
        )
        .await;
        denier.await.unwrap();
        assert_eq!(outcome, GateOutcome::Denied);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires local nats-server on 127.0.0.1:4222 — slow (waits CONFIRM_TIMEOUT_SECS)"]
    async fn full_confirm_flow_times_out() {
        let bus = match try_bus().await {
            Some(b) => b,
            None => return,
        };
        // No approver — should time out.
        let outcome = publish_and_wait(
            &bus,
            "click",
            json!({"x": 0, "y": 0}),
            "click at (0, 0)".into(),
        )
        .await;
        assert_eq!(outcome, GateOutcome::Timeout);
    }
}
