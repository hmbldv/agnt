//! Claude Code runner — invokes `ssh <host> claude --print …` (or just
//! `claude --print …` when the persona is local) and parses the JSON result.
//!
//! The runner returns `ClaudeResult { ok, text, total_cost_usd, duration_ms,
//! session_id, error }`. The dispatch layer translates that into an
//! `AgentReply`.
//!
//! ### Verified protocol (from earlier session)
//!
//! ```bash
//! ssh lnx-rig "claude --print --permission-mode bypassPermissions \
//!     --output-format json '<task>'"
//! ```
//!
//! Returns:
//! ```json
//! {"type":"result","subtype":"success","result":"...",
//!  "duration_ms":4200,"total_cost_usd":0.27,"session_id":"...",...}
//! ```
//!
//! On error / non-success subtypes the bridge surfaces `subtype` in the
//! `AgentReply.error` field.

use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{expand_tilde, Persona};

/// Outcome of a single claude invocation. Mirrors the shape of `AgentReply`
/// minus the `request_id` field (which the dispatch layer fills in).
#[derive(Debug, Clone)]
pub struct ClaudeResult {
    pub ok: bool,
    pub text: String,
    pub total_cost_usd: f64,
    pub duration_ms: u32,
    pub session_id: Option<String>,
    pub error: Option<String>,
}

/// Sub-shape parsed out of `claude --output-format json` stdout.
///
/// Claude Code surfaces extra fields (usage, num_turns, …); we only pluck
/// what the bridge actually needs and accept everything else as flatten-skip.
#[derive(Debug, Deserialize)]
struct ClaudeJson {
    #[serde(rename = "type")]
    _type: Option<String>,
    subtype: Option<String>,
    result: Option<String>,
    duration_ms: Option<u64>,
    total_cost_usd: Option<f64>,
    session_id: Option<String>,
    is_error: Option<bool>,
    error: Option<String>,
}

/// Runner abstraction so unit tests can substitute a stub. Real runners
/// shell out via `ssh` (or directly when the persona is local). The extra
/// `request_id` parameter is used purely for tracing.
#[async_trait::async_trait]
pub trait ClaudeRunner: Send + Sync {
    async fn run(
        &self,
        persona: &Persona,
        user_input: &str,
        request_id: &str,
        timeout: Duration,
    ) -> ClaudeResult;
}

/// Real runner: spawns `ssh <host> sh -c '…'` (or `sh -c '…'` locally) with
/// the rendered claude command, with a wall-clock timeout. The full command
/// vector is logged at info level for debugging.
pub struct RealClaudeRunner;

impl RealClaudeRunner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RealClaudeRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ClaudeRunner for RealClaudeRunner {
    async fn run(
        &self,
        persona: &Persona,
        user_input: &str,
        request_id: &str,
        timeout: Duration,
    ) -> ClaudeResult {
        let prompt = build_prompt(persona, user_input);
        let (program, args) = build_command(persona, &prompt);
        info!(
            request_id = %request_id,
            persona = %persona.name,
            host = %persona.host,
            cwd = %persona.cwd,
            mode = %persona.permission_mode,
            program = %program,
            args = ?args,
            timeout_secs = timeout.as_secs(),
            "ssh+claude dispatching"
        );
        let t0 = Instant::now();

        let mut cmd = Command::new(&program);
        cmd.args(&args);
        cmd.stdin(std::process::Stdio::null());
        cmd.kill_on_drop(true);

        let invocation = tokio::time::timeout(timeout, cmd.output()).await;
        let elapsed_ms = t0.elapsed().as_millis() as u32;

        let output = match invocation {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return ClaudeResult {
                    ok: false,
                    text: String::new(),
                    total_cost_usd: 0.0,
                    duration_ms: elapsed_ms,
                    session_id: None,
                    error: Some(format!("spawn failed: {e}")),
                };
            }
            Err(_) => {
                return ClaudeResult {
                    ok: false,
                    text: String::new(),
                    total_cost_usd: 0.0,
                    duration_ms: elapsed_ms,
                    session_id: None,
                    error: Some(format!(
                        "claude invocation exceeded {}s timeout",
                        timeout.as_secs()
                    )),
                };
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            warn!(
                request_id = %request_id,
                code = ?output.status.code(),
                stderr = %stderr,
                "claude exited non-zero"
            );
            return ClaudeResult {
                ok: false,
                text: String::new(),
                total_cost_usd: 0.0,
                duration_ms: elapsed_ms,
                session_id: None,
                error: Some(format!(
                    "claude exited with status {:?}: {}",
                    output.status.code(),
                    truncate(&stderr, 512),
                )),
            };
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        debug!(request_id = %request_id, bytes = stdout.len(), "claude stdout received");
        parse_claude_json(&stdout, elapsed_ms)
    }
}

/// Build the prompt by prepending an optional system seed (read from the
/// persona's `system_prompt_file`) to the user input. Reads happen on the
/// dispatch path (not at startup) so editing the prompt file is picked up
/// on the next dispatch without restarting the bridge — matches Claude
/// Code's `--append-system-prompt` ergonomics with one less moving part.
fn build_prompt(persona: &Persona, user_input: &str) -> String {
    let seed = persona
        .system_prompt_file
        .as_ref()
        .map(|p| expand_tilde(p))
        .and_then(|p| match std::fs::read_to_string(&p) {
            Ok(s) => Some(s.trim().to_owned()),
            Err(e) => {
                warn!(
                    persona = %persona.name,
                    path = %p.display(),
                    error = %e,
                    "system_prompt_file unreadable; dispatching without seed"
                );
                None
            }
        });
    match seed {
        Some(s) if !s.is_empty() => format!("{s}\n---\n{user_input}"),
        _ => user_input.to_owned(),
    }
}

/// Render the (program, args) tuple. Pure function — no I/O — so unit-testable.
///
/// We always wrap the remote claude invocation in a `cd <cwd> && claude …`
/// shell command. That:
/// 1. Lets us specify a working directory for the remote process without
///    relying on a per-host `claude` flag (claude itself has `--cwd` but
///    we want the *shell* to be in cwd for any tools claude spawns).
/// 2. Keeps the local-vs-ssh code paths symmetric: the only difference is
///    whether `sh -c …` runs through ssh.
///
/// Quoting strategy: we shell-quote the cwd path, the prompt, and any
/// extra-args via `single_quote` (which closes/escapes/reopens single
/// quotes). The remote shell sees a single quoted argument, so newlines /
/// special chars in the prompt are passed through cleanly.
pub(crate) fn build_command(persona: &Persona, prompt: &str) -> (String, Vec<String>) {
    let claude_bin = persona.claude_bin.as_deref().unwrap_or("claude");
    let remote_cmd = format!(
        "cd {cwd} && {bin} --print --permission-mode {mode} --output-format json {prompt}",
        cwd = single_quote(&persona.cwd),
        bin = claude_bin,
        mode = persona.permission_mode,
        prompt = single_quote(prompt),
    );
    if persona.is_local() {
        // Use sh -c so the cd-and-pipeline string is interpreted the same
        // way it would be over ssh.
        ("sh".into(), vec!["-c".into(), remote_cmd])
    } else {
        // ssh -o BatchMode=yes — fail fast if the host needs a password
        // (we never want the bridge to hang on an interactive prompt).
        // ServerAliveInterval keeps long-running claude calls from being
        // killed by an idle middlebox.
        (
            "ssh".into(),
            vec![
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "ServerAliveInterval=30".into(),
                persona.host.clone(),
                remote_cmd,
            ],
        )
    }
}

/// POSIX-shell single-quote: wrap the argument in `'…'`, escaping any
/// embedded `'` by closing the quote, emitting `\'`, and reopening.
fn single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        // Try not to chop a UTF-8 boundary mid-codepoint.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut out = s[..end].to_owned();
        out.push('…');
        out
    }
}

/// Parse the JSON envelope claude emits when `--output-format json` is set.
/// `elapsed_ms` is the bridge-measured wall time; we prefer the
/// claude-reported `duration_ms` when it's present, otherwise fall back to
/// the bridge measurement. Lower-bound the reported value to 1 so the
/// dispatch layer's "duration_ms > 0" sanity check stays meaningful.
pub(crate) fn parse_claude_json(stdout: &str, elapsed_ms: u32) -> ClaudeResult {
    // Claude can sometimes emit additional log lines on stdout when
    // `--debug` is on; we never set that flag, but be defensive: take the
    // last non-empty line as the JSON envelope.
    let line = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(stdout.trim());
    let parsed: Result<ClaudeJson, _> = serde_json::from_str(line);
    match parsed {
        Ok(j) => {
            let subtype = j.subtype.as_deref().unwrap_or("success");
            let is_error = j.is_error.unwrap_or(false);
            let ok = !is_error && subtype == "success";
            let duration = j.duration_ms.map(|d| d as u32).unwrap_or(elapsed_ms).max(1);
            ClaudeResult {
                ok,
                text: j.result.unwrap_or_default(),
                total_cost_usd: j.total_cost_usd.unwrap_or(0.0),
                duration_ms: duration,
                session_id: j.session_id,
                error: if ok {
                    None
                } else {
                    Some(format!(
                        "claude reported subtype={} is_error={} error={:?}",
                        subtype, is_error, j.error
                    ))
                },
            }
        }
        Err(e) => ClaudeResult {
            ok: false,
            text: String::new(),
            total_cost_usd: 0.0,
            duration_ms: elapsed_ms,
            session_id: None,
            error: Some(format!(
                "could not decode claude JSON output: {e}; raw: {}",
                truncate(line, 512)
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_persona() -> Persona {
        Persona {
            name: "archon".into(),
            host: "lnx-rig".into(),
            cwd: "/home/squinks/projects".into(),
            permission_mode: "bypassPermissions".into(),
            system_prompt_file: None,
            daily_cost_limit_usd: Some(5.0),
            timeout_sec: 600,
            claude_bin: None,
        }
    }

    #[test]
    fn single_quote_wraps_simple() {
        assert_eq!(single_quote("hello world"), "'hello world'");
    }

    #[test]
    fn single_quote_escapes_internal_quotes() {
        // foo'bar  ->  'foo'\''bar'
        assert_eq!(single_quote("foo'bar"), "'foo'\\''bar'");
    }

    #[test]
    fn build_command_remote_uses_ssh() {
        let p = fixture_persona();
        let (prog, args) = build_command(&p, "say hi");
        assert_eq!(prog, "ssh");
        assert_eq!(args[0], "-o");
        assert_eq!(args[1], "BatchMode=yes");
        // Host is the 5th arg (after the two -o pairs).
        assert_eq!(args[4], "lnx-rig");
        let remote_cmd = &args[5];
        assert!(remote_cmd.contains("cd '/home/squinks/projects' &&"));
        assert!(remote_cmd.contains(" claude --print"));
        assert!(remote_cmd.contains("--permission-mode bypassPermissions"));
        assert!(remote_cmd.contains("--output-format json"));
        assert!(remote_cmd.contains("'say hi'"));
    }

    #[test]
    fn build_command_local_uses_sh() {
        let mut p = fixture_persona();
        p.host = "localhost".into();
        let (prog, args) = build_command(&p, "say hi");
        assert_eq!(prog, "sh");
        assert_eq!(args[0], "-c");
        assert!(args[1].starts_with("cd '/home/squinks/projects' && claude"));
    }

    #[test]
    fn build_command_respects_custom_claude_bin() {
        let mut p = fixture_persona();
        p.claude_bin = Some("/usr/local/bin/claude".into());
        let (_prog, args) = build_command(&p, "x");
        let remote = args.last().unwrap();
        assert!(
            remote.contains("/usr/local/bin/claude --print"),
            "expected custom binary path in remote command, got: {remote}"
        );
    }

    #[test]
    fn build_command_quotes_user_input_with_special_chars() {
        let p = fixture_persona();
        let (_prog, args) = build_command(&p, "what's up?");
        let remote = args.last().unwrap();
        // POSIX-quoted: 'what'\''s up?'
        assert!(
            remote.contains("'what'\\''s up?'"),
            "user_input not POSIX-quoted: {remote}"
        );
    }

    #[test]
    fn parse_claude_json_success() {
        // Real claude --output-format json emits single-line JSON; we
        // parse the last non-empty stdout line as the envelope.
        let raw = r#"{"type":"result","subtype":"success","result":"OK","duration_ms":4200,"total_cost_usd":0.27,"session_id":"abc"}"#;
        let r = parse_claude_json(raw, 5000);
        assert!(r.ok);
        assert_eq!(r.text, "OK");
        assert_eq!(r.total_cost_usd, 0.27);
        // Should prefer the claude-reported duration over the bridge measurement.
        assert_eq!(r.duration_ms, 4200);
        assert_eq!(r.session_id.as_deref(), Some("abc"));
    }

    #[test]
    fn parse_claude_json_failure_subtype() {
        let raw = r#"{"type":"result","subtype":"error","is_error":true,"error":"rate limit"}"#;
        let r = parse_claude_json(raw, 5000);
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("rate limit"));
    }

    #[test]
    fn parse_claude_json_garbage_input() {
        let r = parse_claude_json("not json at all", 1234);
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("could not decode"));
        assert_eq!(r.duration_ms, 1234);
    }

    #[test]
    fn parse_claude_json_missing_duration_uses_elapsed() {
        let raw = r#"{"type":"result","subtype":"success","result":"x"}"#;
        let r = parse_claude_json(raw, 999);
        assert_eq!(r.duration_ms, 999);
    }

    #[test]
    fn parse_claude_json_takes_last_line_when_extra_log_output() {
        let raw = "warn: deprecated flag\n\
                   {\"type\":\"result\",\"subtype\":\"success\",\"result\":\"hi\"}\n";
        let r = parse_claude_json(raw, 100);
        assert!(r.ok);
        assert_eq!(r.text, "hi");
    }

    #[test]
    fn build_prompt_no_seed_returns_user_input() {
        let p = fixture_persona();
        assert_eq!(build_prompt(&p, "hello"), "hello");
    }
}
