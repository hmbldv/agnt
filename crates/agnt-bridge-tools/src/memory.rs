//! `memctl_recall` + `memctl_ingest` — bind the agent to the user's
//! persistent memory store via the `memctl` CLI.
//!
//! `memctl` is a separate binary (`~/.local/bin/memctl`); these tools just
//! shell out with structured arguments.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::shell::{run_blocking, DEFAULT_TIMEOUT};

const RECALL_TIMEOUT: Duration = DEFAULT_TIMEOUT;
const INGEST_TIMEOUT: Duration = DEFAULT_TIMEOUT;

const ALLOWED_KINDS: &[&str] = &[
    "decision",
    "fact",
    "error",
    "correction",
    "pattern",
    "insight",
];
const ALLOWED_SCOPES: &[&str] = &["project", "global"];

// ─────────────────────────────────────────────────────────────────────────────
// memctl_recall
// ─────────────────────────────────────────────────────────────────────────────

pub struct MemctlRecall {
    bin: PathBuf,
}

impl MemctlRecall {
    pub fn new(bin: PathBuf) -> Self {
        Self { bin }
    }
}

impl agnt::Tool for MemctlRecall {
    fn name(&self) -> &str {
        "memctl_recall"
    }

    fn description(&self) -> &str {
        "Search the user's persistent memory store (memctl). Returns scored \
         memories — decisions, facts, corrections, patterns the user has \
         confirmed in past sessions. Use BEFORE re-deriving context that \
         might already be settled, especially on session start or topic \
         switch."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Free-text search query — keywords work best."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max memories to return (default 5, max 20).",
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or("missing 'query' (string)")?
            .trim()
            .to_string();
        if query.is_empty() {
            return Err("query must not be empty".into());
        }
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or(5)
            .clamp(1, 20);

        let bin = self
            .bin
            .to_str()
            .ok_or("memctl_bin contained non-UTF-8 bytes")?;
        let limit_str = limit.to_string();
        let argv: Vec<&str> = vec!["recall", "--limit", &limit_str, &query];
        let out = run_blocking(bin, argv, RECALL_TIMEOUT)?;
        if !out.status_ok {
            return Err(format!(
                "memctl recall failed: {}",
                if out.stderr.trim().is_empty() {
                    out.stdout.trim()
                } else {
                    out.stderr.trim()
                }
            ));
        }
        let stdout = out.stdout.trim();
        if stdout.is_empty() {
            Ok("(no memories matched)".to_string())
        } else {
            Ok(stdout.to_string())
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// memctl_ingest
// ─────────────────────────────────────────────────────────────────────────────

pub struct MemctlIngest {
    bin: PathBuf,
}

impl MemctlIngest {
    pub fn new(bin: PathBuf) -> Self {
        Self { bin }
    }
}

impl agnt::Tool for MemctlIngest {
    fn name(&self) -> &str {
        "memctl_ingest"
    }

    fn description(&self) -> &str {
        "Persist a new memory (decision/fact/correction/pattern/insight/error) \
         into the user's memctl store. Use AFTER confirming a non-trivial \
         decision, discovering a non-obvious pattern, or being corrected on \
         something. Don't ingest routine code changes or anything derivable \
         from git history."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The memory text — one or two sentences." },
                "kind": {
                    "type": "string",
                    "enum": ALLOWED_KINDS,
                    "description": "Memory type."
                },
                "scope": {
                    "type": "string",
                    "enum": ALLOWED_SCOPES,
                    "description": "Storage scope."
                }
            },
            "required": ["content", "kind", "scope"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or("missing 'content' (string)")?
            .to_string();
        if content.trim().is_empty() {
            return Err("content must not be empty".into());
        }
        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or("missing 'kind' (string)")?;
        let scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .ok_or("missing 'scope' (string)")?;
        validate_kind(kind)?;
        validate_scope(scope)?;

        let bin = self
            .bin
            .to_str()
            .ok_or("memctl_bin contained non-UTF-8 bytes")?;
        let argv: Vec<&str> = vec!["ingest", "-t", kind, "-s", scope, &content];
        let out = run_blocking(bin, argv, INGEST_TIMEOUT)?;
        if !out.status_ok {
            return Err(format!(
                "memctl ingest failed: {}",
                if out.stderr.trim().is_empty() {
                    out.stdout.trim()
                } else {
                    out.stderr.trim()
                }
            ));
        }
        Ok(format!(
            "ingested ({}/{}): {}",
            kind,
            scope,
            out.stdout.trim()
        ))
    }
}

pub fn validate_kind(kind: &str) -> Result<(), String> {
    if ALLOWED_KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(format!(
            "kind must be one of {:?}, got '{kind}'",
            ALLOWED_KINDS
        ))
    }
}

pub fn validate_scope(scope: &str) -> Result<(), String> {
    if ALLOWED_SCOPES.contains(&scope) {
        Ok(())
    } else {
        Err(format!(
            "scope must be one of {:?}, got '{scope}'",
            ALLOWED_SCOPES
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnt::Tool;

    #[test]
    fn allowed_kinds_match_spec() {
        for k in [
            "decision",
            "fact",
            "error",
            "correction",
            "pattern",
            "insight",
        ] {
            assert!(validate_kind(k).is_ok(), "kind {k} should be allowed");
        }
    }

    #[test]
    fn rejects_bad_kind() {
        let err = validate_kind("nope").unwrap_err();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn rejects_bad_scope() {
        let err = validate_scope("session").unwrap_err();
        assert!(err.contains("session"), "{err}");
    }

    #[test]
    fn allowed_scopes_match_spec() {
        for s in ["project", "global"] {
            assert!(validate_scope(s).is_ok());
        }
    }

    #[test]
    fn ingest_rejects_missing_content() {
        let bin = PathBuf::from("/bin/true");
        let tool = MemctlIngest::new(bin);
        let err = tool
            .call(json!({"kind": "fact", "scope": "project"}))
            .unwrap_err();
        assert!(err.contains("content"), "{err}");
    }

    #[test]
    fn ingest_rejects_bad_kind_via_call() {
        let tool = MemctlIngest::new(PathBuf::from("/bin/true"));
        let err = tool
            .call(json!({"content": "x", "kind": "junk", "scope": "project"}))
            .unwrap_err();
        assert!(err.contains("junk") || err.contains("kind"), "{err}");
    }

    #[test]
    fn ingest_rejects_bad_scope_via_call() {
        let tool = MemctlIngest::new(PathBuf::from("/bin/true"));
        let err = tool
            .call(json!({"content": "x", "kind": "fact", "scope": "session"}))
            .unwrap_err();
        assert!(err.contains("session") || err.contains("scope"), "{err}");
    }

    #[test]
    fn recall_rejects_empty_query() {
        let tool = MemctlRecall::new(PathBuf::from("/bin/true"));
        let err = tool.call(json!({"query": "  "})).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }
}
