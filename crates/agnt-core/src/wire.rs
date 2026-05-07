//! Wire types for the agntc NATS bus protocol.
//!
//! Cross-crate contract between any publisher and any subscriber on the agent
//! dispatch and computer-use confirmation subjects. Zero I/O, zero async, no
//! NATS dependency — this module compiles to WASM as-is.
//!
//! Subject strings live in [`subjects`].

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── WireError ─────────────────────────────────────────────────────────────────

/// Errors returned by wire-layer helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The agent name contains a character illegal in a NATS subject token
    /// (`.`, `>`, `*`, or any whitespace).
    InvalidAgentName(String),
}

// ── RequestId ────────────────────────────────────────────────────────────────

/// Stable identifier for a single agent dispatch request.
///
/// Wraps a UUID for type safety. Callers allocate at dispatch time and echo it
/// back in replies, token frames, and cancel messages to correlate fan-out
/// responses.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub Uuid);

impl RequestId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for RequestId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ── Agent dispatch ────────────────────────────────────────────────────────────

/// Request from any client to a specific named agent.
///
/// Published on `agent.dispatch.<agent_name>` — see [`subjects::dispatch_for`].
///
/// Any client (voicectl, web UI, CLI) can publish this. The receiving
/// `agnt-bridge` process echoes `request_id` in every reply and token frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDispatch {
    /// Caller-allocated identifier. Bridge echoes this in [`AgentReply`],
    /// [`AgentToken`], and on the `reply_to` subject.
    pub request_id: RequestId,
    /// Free-form text the agent treats as user input.
    pub user_input: String,
    /// Optional caller-provided metadata. voicectl populates `utterance_id`
    /// and `from`; bridges log it but are not required to act on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
    /// Subject the bridge publishes the single [`AgentReply`] on.
    /// Conventionally `agent.response.<name>.<request_id>` — see
    /// [`subjects::response_for`] — but the bridge treats it as opaque.
    pub reply_to: String,
}

/// One-shot reply from an agnt-bridge for a previously-dispatched request.
///
/// Published on whatever `AgentDispatch::reply_to` was.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentReply {
    pub request_id: RequestId,
    /// `true` on a successful `agent.step`; `false` on any error including
    /// cancellation and timeout.
    pub ok: bool,
    /// Final assistant text on success. Empty on error.
    #[serde(default)]
    pub text: String,
    /// Completion-token count. `0` if the backend didn't surface usage;
    /// bridges may approximate by word count.
    #[serde(default)]
    pub tokens: u32,
    /// Wall time spent in `agent.step`, excluding NATS round-trip.
    #[serde(default)]
    pub duration_ms: u32,
    /// Names of every tool the agent invoked, in invocation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<String>,
    /// Human-readable error message when `ok = false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Streaming token delta published by agnt-bridge while `agent.step` runs.
///
/// Published on `agent.token.<agent_name>.<request_id>` — see
/// [`subjects::token_for`].
///
/// One frame per backend delta. The final frame has `is_final: true` and may
/// carry an empty `text` — treat `is_final` as the end-of-stream marker.
///
/// The complete reply still arrives via [`AgentReply`]. The token stream is
/// purely additive — consumers that only need the final text can ignore it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToken {
    pub request_id: RequestId,
    /// Zero-based delta index. Useful for re-ordering on rare out-of-order
    /// NATS delivery.
    pub idx: u32,
    /// The delta payload — typically one token or a small word fragment.
    pub text: String,
    /// `true` only on the synthetic terminal frame emitted after the backend
    /// returns. Consumers should flush their accumulator on this.
    #[serde(default)]
    pub is_final: bool,
}

/// Cancel an in-flight or queued agent dispatch.
///
/// Published on `agent.cancel.<agent_name>` — see [`subjects::cancel_for`].
///
/// If `request_id` is `Some`, cancels only that request. If `all` is `true`,
/// aborts the running request and clears any queued ones.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentCancel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    #[serde(default)]
    pub all: bool,
}

// ── Computer-use safety gate ──────────────────────────────────────────────────

/// Published by a destructive computer-use tool before it executes.
///
/// Published on [`subjects::CONFIRM_REQUEST`] (`agnt.confirm.request`).
///
/// The tool waits up to 30 s for a matching [`ConfirmReply`]. Any subscriber
/// (CLI, tray, voicectl, web UI) can approve or deny.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmRequest {
    /// Tool-allocated UUID. The reply must echo this, or use `"*"` / empty
    /// string to match the most recent request.
    pub request_id: RequestId,
    /// Tool name as registered in the bridge (e.g. `click`, `type_text`).
    pub tool: String,
    /// Tool args as JSON — informational only. The tool has already validated
    /// its own parsed copy.
    pub args: serde_json::Value,
    /// Short human-readable preview, e.g. `"type 'rm -rf /' into Terminal"`.
    pub preview: String,
    /// Unix nanoseconds at which the tool gives up and fails.
    pub expires_at_ns: u64,
}

/// Approval or denial for a pending [`ConfirmRequest`].
///
/// Published on [`subjects::CONFIRM_REPLY`] (`agnt.confirm.reply`).
///
/// `request_id` `"*"` matches the most recent pending request — the path used
/// by voice approval and `voicectl confirm` shortcut.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmReply {
    /// UUID from [`ConfirmRequest::request_id`], or `"*"` to match the latest
    /// request.
    pub request_id: String,
    pub approved: bool,
    /// Optional caller identity for debug logging (`"cli"`, `"tray"`,
    /// `"voice"`, `"web"`). Not used for routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

// ── Subject constants and builders ────────────────────────────────────────────

pub mod subjects {
    use super::{RequestId, WireError};

    // Agent dispatch

    /// `agent.dispatch.<name>` prefix
    pub const AGENT_DISPATCH_PREFIX: &str = "agent.dispatch";
    /// `agent.token.<name>.<request_id>` prefix
    pub const AGENT_TOKEN_PREFIX: &str = "agent.token";
    /// `agent.cancel.<name>` prefix
    pub const AGENT_CANCEL_PREFIX: &str = "agent.cancel";
    /// `agent.response.<name>.<request_id>` prefix
    pub const AGENT_RESPONSE_PREFIX: &str = "agent.response";

    // Computer-use gate — note: NOT under voice.* namespace

    /// Destructive tool publishes here before executing.
    /// Payload: [`super::ConfirmRequest`]
    pub const CONFIRM_REQUEST: &str = "agnt.confirm.request";

    /// Any approver publishes here.
    /// Payload: [`super::ConfirmReply`]
    pub const CONFIRM_REPLY: &str = "agnt.confirm.reply";

    /// Validate that `name` is a legal NATS subject token for an agent name.
    ///
    /// Rejects any string containing `.`, `>`, `*`, or ASCII/Unicode whitespace,
    /// all of which are reserved or ambiguous in NATS subject hierarchies.
    pub fn validate_nats_token(name: &str) -> Result<(), WireError> {
        let illegal = |c: char| matches!(c, '.' | '>' | '*') || c.is_whitespace();
        if name.chars().any(illegal) {
            Err(WireError::InvalidAgentName(name.to_owned()))
        } else {
            Ok(())
        }
    }

    /// `agent.dispatch.<name>`
    pub fn dispatch_for(name: &str) -> Result<String, WireError> {
        validate_nats_token(name)?;
        Ok(format!("{AGENT_DISPATCH_PREFIX}.{name}"))
    }

    /// `agent.token.<name>.<request_id>`
    pub fn token_for(name: &str, request_id: &RequestId) -> Result<String, WireError> {
        validate_nats_token(name)?;
        Ok(format!("{AGENT_TOKEN_PREFIX}.{name}.{request_id}"))
    }

    /// `agent.cancel.<name>`
    pub fn cancel_for(name: &str) -> Result<String, WireError> {
        validate_nats_token(name)?;
        Ok(format!("{AGENT_CANCEL_PREFIX}.{name}"))
    }

    /// `agent.response.<name>.<request_id>`
    pub fn response_for(name: &str, request_id: &RequestId) -> Result<String, WireError> {
        validate_nats_token(name)?;
        Ok(format!("{AGENT_RESPONSE_PREFIX}.{name}.{request_id}"))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_roundtrip() {
        let id = RequestId::new();
        let s = serde_json::to_string(&id).unwrap();
        let back: RequestId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn request_id_display_is_uuid_string() {
        let id = RequestId::new();
        let display = id.to_string();
        assert_eq!(display.len(), 36); // UUID hyphenated form
        assert_eq!(display, id.0.to_string());
    }

    #[test]
    fn request_id_fromstr_roundtrip() {
        let id = RequestId::new();
        let s = id.to_string();
        let parsed: RequestId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn agent_dispatch_roundtrip() {
        let req = AgentDispatch {
            request_id: RequestId::new(),
            user_input: "what folders does my vault have?".into(),
            context: Some(serde_json::json!({"utterance_id": "u-1", "from": "voicectld"})),
            reply_to: "agent.response.sage.abc-123".into(),
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: AgentDispatch = serde_json::from_str(&s).unwrap();
        assert_eq!(back.request_id, req.request_id);
        assert_eq!(back.user_input, req.user_input);
        assert_eq!(back.reply_to, req.reply_to);
        assert!(back.context.is_some());
    }

    #[test]
    fn agent_dispatch_context_optional() {
        let req = AgentDispatch {
            request_id: RequestId::new(),
            user_input: "ping".into(),
            context: None,
            reply_to: "r".into(),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("context"));
        let back: AgentDispatch = serde_json::from_str(&s).unwrap();
        assert!(back.context.is_none());
    }

    #[test]
    fn agent_reply_success_roundtrip() {
        let r = AgentReply {
            request_id: RequestId::new(),
            ok: true,
            text: "vault folders are 00, 01, …".into(),
            tokens: 47,
            duration_ms: 820,
            tool_calls: vec!["grep".into(), "read_file".into()],
            error: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: AgentReply = serde_json::from_str(&s).unwrap();
        assert!(back.ok);
        assert_eq!(back.tokens, 47);
        assert_eq!(back.tool_calls.len(), 2);
        assert!(back.error.is_none());
        // error field should be omitted from JSON when None
        assert!(!s.contains("error"));
    }

    #[test]
    fn agent_reply_error_roundtrip() {
        let r = AgentReply {
            request_id: RequestId::new(),
            ok: false,
            text: String::new(),
            tokens: 0,
            duration_ms: 0,
            tool_calls: Vec::new(),
            error: Some("backend timeout".into()),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: AgentReply = serde_json::from_str(&s).unwrap();
        assert!(!back.ok);
        assert_eq!(back.error.as_deref(), Some("backend timeout"));
        // tool_calls empty — should be omitted
        assert!(!s.contains("tool_calls"));
    }

    #[test]
    fn agent_token_roundtrip() {
        let t = AgentToken {
            request_id: RequestId::new(),
            idx: 7,
            text: "hello".into(),
            is_final: false,
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: AgentToken = serde_json::from_str(&s).unwrap();
        assert_eq!(back.idx, 7);
        assert!(!back.is_final);
    }

    #[test]
    fn agent_token_final_frame_empty_text() {
        let t = AgentToken {
            request_id: RequestId::new(),
            idx: 99,
            text: String::new(),
            is_final: true,
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: AgentToken = serde_json::from_str(&s).unwrap();
        assert!(back.is_final);
        assert!(back.text.is_empty());
    }

    #[test]
    fn agent_cancel_default_all_false() {
        let c = AgentCancel::default();
        let s = serde_json::to_string(&c).unwrap();
        let back: AgentCancel = serde_json::from_str(&s).unwrap();
        assert!(back.request_id.is_none());
        assert!(!back.all);
        assert!(!s.contains("request_id"));
    }

    #[test]
    fn confirm_request_roundtrip() {
        let r = ConfirmRequest {
            request_id: RequestId::new(),
            tool: "click".into(),
            args: serde_json::json!({"x": 100, "y": 200}),
            preview: "click at (100, 200) over Terminal".into(),
            expires_at_ns: 9999999999,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: ConfirmRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tool, "click");
        assert_eq!(back.expires_at_ns, 9999999999);
    }

    #[test]
    fn confirm_reply_wildcard() {
        let r = ConfirmReply {
            request_id: "*".into(),
            approved: true,
            source: Some("voice".into()),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: ConfirmReply = serde_json::from_str(&s).unwrap();
        assert_eq!(back.request_id, "*");
        assert!(back.approved);
    }

    #[test]
    fn confirm_reply_source_omitted_when_none() {
        let r = ConfirmReply {
            request_id: "some-id".into(),
            approved: false,
            source: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("source"));
    }

    #[test]
    fn subjects_dispatch_for() {
        assert_eq!(
            subjects::dispatch_for("sage").unwrap(),
            "agent.dispatch.sage"
        );
    }

    #[test]
    fn subjects_token_for() {
        let id = RequestId::new();
        let subj = subjects::token_for("sage", &id).unwrap();
        assert!(subj.starts_with("agent.token.sage."));
        assert!(subj.ends_with(&id.to_string()));
    }

    #[test]
    fn subjects_cancel_for() {
        assert_eq!(
            subjects::cancel_for("nexus").unwrap(),
            "agent.cancel.nexus"
        );
    }

    #[test]
    fn subjects_response_for() {
        let id = RequestId::new();
        let subj = subjects::response_for("sage", &id).unwrap();
        assert!(subj.starts_with("agent.response.sage."));
    }

    #[test]
    fn validate_nats_token_rejects_dot() {
        assert_eq!(
            subjects::validate_nats_token("bad.name"),
            Err(WireError::InvalidAgentName("bad.name".into()))
        );
    }

    #[test]
    fn validate_nats_token_rejects_gt() {
        assert!(subjects::validate_nats_token("bad>name").is_err());
    }

    #[test]
    fn validate_nats_token_rejects_star() {
        assert!(subjects::validate_nats_token("bad*name").is_err());
    }

    #[test]
    fn validate_nats_token_rejects_whitespace() {
        assert!(subjects::validate_nats_token("bad name").is_err());
        assert!(subjects::validate_nats_token("bad\tname").is_err());
    }

    #[test]
    fn validate_nats_token_accepts_valid_name() {
        assert!(subjects::validate_nats_token("sage").is_ok());
        assert!(subjects::validate_nats_token("NEXUS-2").is_ok());
        assert!(subjects::validate_nats_token("agent_42").is_ok());
    }

    #[test]
    fn dispatch_for_rejects_invalid_name() {
        assert!(subjects::dispatch_for("bad.name").is_err());
    }

    #[test]
    fn confirm_subjects_not_under_voice_namespace() {
        assert!(!subjects::CONFIRM_REQUEST.starts_with("voice."));
        assert!(!subjects::CONFIRM_REPLY.starts_with("voice."));
        assert_eq!(subjects::CONFIRM_REQUEST, "agnt.confirm.request");
        assert_eq!(subjects::CONFIRM_REPLY, "agnt.confirm.reply");
    }
}
