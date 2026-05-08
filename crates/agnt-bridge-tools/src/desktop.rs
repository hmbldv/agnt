//! Desktop integration tools — open apps/URLs, send notifications, query the
//! active window, screenshot the screen, and read the clipboard.
//!
//! All implementations are read-only or additive; nothing here can overwrite
//! user state. URL validation rejects `file://` to keep the LLM from
//! exfiltrating local files via `xdg-open`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agnt::TypedTool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use crate::shell::{run_blocking, DEFAULT_TIMEOUT};

// ─────────────────────────────────────────────────────────────────────────────
// open_app
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OpenAppArgs {
    /// App name. Tries exact `.desktop` filename first, then a fuzzy match
    /// against `~/.local/share/applications` and `/usr/share/applications`.
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct OpenAppResult {
    pub launched: String,
    pub via: String,
}

/// Launch a desktop application by name.
pub struct OpenApp;

impl TypedTool for OpenApp {
    type Args = OpenAppArgs;
    type Output = OpenAppResult;
    type Error = String;
    const NAME: &'static str = "open_app";
    const DESCRIPTION: &'static str = "Launch a desktop application by name. \
         Tries the literal `<name>.desktop` first, then a fuzzy match against \
         the user's and system .desktop directories. Returns the desktop ID \
         that was launched.";

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "App name — e.g. 'firefox', 'kitty', 'Obsidian'. Case-insensitive fuzzy match."
                }
            },
            "required": ["name"]
        })
    }

    fn call(&self, args: OpenAppArgs) -> Result<OpenAppResult, String> {
        let trimmed = args.name.trim();
        if trimmed.is_empty() {
            return Err("name must not be empty".into());
        }
        tracing::info!(arg = %trimmed, "open_app: invoked");

        // Helper: try one .desktop id via gtk-launch. Returns:
        //   Ok(launched) — gtk-launch returned success in time
        //   Ok(launched_but_slow) — gtk-launch timed out waiting for the app's
        //       StartupNotify ack, but the spawn itself happened. We treat
        //       this as success because the app is launching; many desktops
        //       (kitty, electron apps) take >10s to send the ack despite
        //       being on-screen instantly.
        //   Err(stderr) — gtk-launch returned non-zero (e.g. unknown name).
        fn try_launch(id: &str) -> Result<bool, String> {
            // 3-second timeout: enough for gtk-launch to either exit cleanly
            // or commit the spawn. If it's still running after 3s, the app
            // has been spawned and we don't care about the ack.
            let short = std::time::Duration::from_secs(3);
            match run_blocking("gtk-launch", [id], short) {
                Ok(out) => {
                    if out.status_ok {
                        Ok(true)
                    } else {
                        Err(out.stderr.trim().to_string())
                    }
                }
                Err(e) if e.contains("timed out") => {
                    // Treat timeout as launched-but-slow.
                    Ok(true)
                }
                Err(e) => Err(e),
            }
        }

        // 1) Direct gtk-launch with the exact ID.
        match try_launch(trimmed) {
            Ok(_) => {
                tracing::info!(arg = %trimmed, "open_app: direct gtk-launch ok");
                return Ok(OpenAppResult {
                    launched: trimmed.to_string(),
                    via: "gtk-launch".into(),
                });
            }
            Err(stderr) => {
                tracing::info!(
                    arg = %trimmed,
                    stderr = %stderr,
                    "open_app: direct gtk-launch failed, trying fuzzy match"
                );
            }
        }

        // 2) Fuzzy match against .desktop files.
        if let Some(id) = fuzzy_desktop_id(trimmed) {
            tracing::info!(arg = %trimmed, matched = %id, "open_app: fuzzy match");
            match try_launch(&id) {
                Ok(_) => {
                    tracing::info!(
                        arg = %trimmed,
                        matched = %id,
                        "open_app: fuzzy gtk-launch ok"
                    );
                    return Ok(OpenAppResult {
                        launched: id,
                        via: "gtk-launch (fuzzy)".into(),
                    });
                }
                Err(stderr) => return Err(format!("gtk-launch {id} failed: {stderr}")),
            }
        }
        Err(format!(
            "no .desktop entry matched '{trimmed}' (tried gtk-launch and \
             ~/.local/share/applications + /usr/share/applications)"
        ))
    }
}

/// Semantic aliases for category-based matching. The user's words "terminal",
/// "browser", "editor" map to .desktop `Categories=...` flags. Used only as a
/// fallback when the literal name doesn't match anything (score 5 in the
/// scoring table inside `fuzzy_desktop_id`). Exact `Name=` matches still win.
const ALIAS_TO_CATEGORY: &[(&str, &[&str])] = &[
    ("terminal", &["TerminalEmulator"]),
    ("browser", &["WebBrowser"]),
    ("editor", &["TextEditor", "IDE", "Development"]),
    ("file manager", &["FileManager"]),
    ("files", &["FileManager"]),
    ("calculator", &["Calculator"]),
    ("video", &["AudioVideo", "Video", "Player"]),
    ("music", &["AudioVideo", "Audio", "Player"]),
    ("image", &["Graphics", "RasterGraphics", "Photography"]),
    ("settings", &["Settings"]),
];

fn fuzzy_desktop_id(name: &str) -> Option<String> {
    let lower = name.trim().to_lowercase();
    let dirs: Vec<PathBuf> = {
        let mut v = Vec::new();
        if let Some(home) = dirs::home_dir() {
            v.push(home.join(".local/share/applications"));
        }
        v.push(PathBuf::from("/usr/share/applications"));
        v
    };

    // Look up category aliases — "terminal" → TerminalEmulator, etc.
    let aliased_cats: Vec<&str> = ALIAS_TO_CATEGORY
        .iter()
        .find(|(alias, _)| *alias == lower)
        .map(|(_, cats)| cats.to_vec())
        .unwrap_or_default();

    // Score:
    //   1 = exact filename stem match
    //   2 = exact Name= field match (case-insensitive)
    //   3 = filename stem contains query
    //   4 = Name= field contains query
    //   5 = matches one of the aliased categories (only if alias hit above)
    // Lower number = better. On tie, shorter stem wins (less specific name).
    let mut best: Option<(u8, usize, String)> = None;
    let mut consider = |score: u8, stem: &str| {
        let stem_len = stem.len();
        match &best {
            Some((s, _, _)) if *s < score => {}
            Some((s, l, _)) if *s == score && *l <= stem_len => {}
            _ => best = Some((score, stem_len, stem.to_string())),
        }
    };

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let stem_lower = stem.to_lowercase();

            // Score 1: exact filename stem.
            if stem_lower == lower {
                return Some(stem.to_string());
            }

            // Parse Name= and Categories= cheaply (single pass).
            let contents = std::fs::read_to_string(&path).unwrap_or_default();
            let mut name_field: Option<String> = None;
            let mut categories: Vec<String> = Vec::new();
            let mut nodisplay = false;
            for line in contents.lines() {
                if let Some(rest) = line.strip_prefix("Name=") {
                    if name_field.is_none() {
                        name_field = Some(rest.to_string());
                    }
                } else if let Some(rest) = line.strip_prefix("Categories=") {
                    categories = rest
                        .split(';')
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect();
                } else if line.trim() == "NoDisplay=true" {
                    nodisplay = true;
                }
            }
            if nodisplay {
                continue; // skip hidden entries (control panels, helpers)
            }
            let name_lower = name_field.as_deref().unwrap_or("").to_lowercase();

            // Score 2: exact Name= match.
            if name_lower == lower {
                consider(2, stem);
                continue;
            }
            // Score 3: stem contains query.
            if stem_lower.contains(&lower) {
                consider(3, stem);
                continue;
            }
            // Score 4: Name field contains query.
            if !name_lower.is_empty() && name_lower.contains(&lower) {
                consider(4, stem);
                continue;
            }
            // Score 5: category alias.
            if !aliased_cats.is_empty()
                && categories
                    .iter()
                    .any(|c| aliased_cats.iter().any(|a| a == c))
            {
                consider(5, stem);
            }
        }
    }
    best.map(|(_, _, s)| s)
}

// ─────────────────────────────────────────────────────────────────────────────
// open_url
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OpenUrlArgs {
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct OpenUrlResult {
    pub opened: String,
}

/// Open a URL in the default browser.
pub struct OpenUrl;

impl TypedTool for OpenUrl {
    type Args = OpenUrlArgs;
    type Output = OpenUrlResult;
    type Error = String;
    const NAME: &'static str = "open_url";
    const DESCRIPTION: &'static str = "Open a URL in the user's default browser. \
         Only http://, https://, and mailto: URLs are accepted — file:// and \
         other schemes are rejected to prevent local file exfiltration.";

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL to open. Must start with http://, https://, or mailto:"
                }
            },
            "required": ["url"]
        })
    }

    fn call(&self, args: OpenUrlArgs) -> Result<OpenUrlResult, String> {
        validate_url(&args.url)?;
        let out = run_blocking("xdg-open", [&args.url], DEFAULT_TIMEOUT)?;
        if !out.status_ok {
            return Err(format!("xdg-open failed: {}", out.stderr.trim()));
        }
        Ok(OpenUrlResult { opened: args.url })
    }
}

/// Reject anything that isn't `http://`, `https://`, or `mailto:`.
pub fn validate_url(s: &str) -> Result<(), String> {
    let u = url::Url::parse(s).map_err(|e| format!("invalid URL: {e}"))?;
    match u.scheme() {
        "http" | "https" | "mailto" => Ok(()),
        other => Err(format!(
            "scheme '{other}' is not allowed (only http/https/mailto)"
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// notification
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct NotificationArgs {
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NotificationResult {
    pub sent: bool,
}

/// Show a desktop notification via `notify-send`.
pub struct Notification;

impl TypedTool for Notification {
    type Args = NotificationArgs;
    type Output = NotificationResult;
    type Error = String;
    const NAME: &'static str = "notification";
    const DESCRIPTION: &'static str = "Send a desktop notification (notify-send). \
         Use for non-blocking status updates the user can glance at — e.g. \
         'meeting in 5 minutes' or 'pipeline finished'. Always safe.";

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Short title shown in bold." },
                "body": { "type": ["string", "null"], "description": "Optional body text." }
            },
            "required": ["title"]
        })
    }

    fn call(&self, args: NotificationArgs) -> Result<NotificationResult, String> {
        if args.title.trim().is_empty() {
            return Err("title must not be empty".into());
        }
        let mut argv: Vec<String> = vec![args.title];
        if let Some(b) = args.body {
            argv.push(b);
        }
        let out = run_blocking("notify-send", &argv, DEFAULT_TIMEOUT)?;
        if !out.status_ok {
            return Err(format!("notify-send failed: {}", out.stderr.trim()));
        }
        Ok(NotificationResult { sent: true })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// current_window
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CurrentWindowArgs {}

#[derive(Debug, Serialize)]
pub struct CurrentWindowResult {
    pub title: String,
    pub wm_class: String,
}

/// Return the currently-focused window's title + WM class.
pub struct CurrentWindow;

impl TypedTool for CurrentWindow {
    type Args = CurrentWindowArgs;
    type Output = CurrentWindowResult;
    type Error = String;
    const NAME: &'static str = "current_window";
    const DESCRIPTION: &'static str = "Return the currently focused X11 window's \
         title and WM class. Useful for situational awareness — 'what is the \
         user looking at right now'.";

    fn schema() -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn call(&self, _args: CurrentWindowArgs) -> Result<CurrentWindowResult, String> {
        let title_out = run_blocking(
            "xdotool",
            ["getactivewindow", "getwindowname"],
            DEFAULT_TIMEOUT,
        )?;
        let class_out = run_blocking(
            "xdotool",
            ["getactivewindow", "getwindowclassname"],
            DEFAULT_TIMEOUT,
        )?;
        Ok(CurrentWindowResult {
            title: title_out.stdout.trim().to_string(),
            wm_class: class_out.stdout.trim().to_string(),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// screenshot — needs a cache_dir, so it's a struct (not a unit type) and we
// hand-roll Tool directly rather than going through TypedTool.
// ─────────────────────────────────────────────────────────────────────────────

/// Capture the full screen and save it under `<cache_dir>/screenshots/`.
///
/// Returns only the absolute path; the image content is **never** read or
/// transmitted by this tool. Vision integration is a separate Day-7 task.
pub struct Screenshot {
    cache_dir: PathBuf,
}

impl Screenshot {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    fn screenshots_dir(&self) -> PathBuf {
        self.cache_dir.join("screenshots")
    }
}

impl agnt::Tool for Screenshot {
    fn name(&self) -> &str {
        "screenshot"
    }

    fn description(&self) -> &str {
        "Capture the full screen to a PNG file. Returns the absolute file path; \
         the image is NOT read back into the tool result. Use when the user \
         asks you to 'take a screenshot' or 'save what's on screen'."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn call(&self, _args: Value) -> Result<String, String> {
        let dir = self.screenshots_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Err(format!("create {}: {e}", dir.display()));
        }
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let path = dir.join(format!("{ts}.png"));
        // gnome-screenshot is the GNOME default; on some systems it may be
        // missing — fall back to ImageMagick `import`.
        if try_screenshot("gnome-screenshot", &["-f", path_str(&path)?]).is_ok() && path.exists() {
            return Ok(format!("{{\"path\":\"{}\"}}", path.display()));
        }
        if try_screenshot("import", &["-window", "root", path_str(&path)?]).is_ok() && path.exists()
        {
            return Ok(format!("{{\"path\":\"{}\"}}", path.display()));
        }
        Err("no screenshot tool available (tried gnome-screenshot, import)".into())
    }
}

fn path_str(p: &Path) -> Result<&str, String> {
    p.to_str()
        .ok_or_else(|| "screenshot path contained non-UTF-8 bytes".into())
}

fn try_screenshot(program: &str, args: &[&str]) -> Result<(), String> {
    let out = run_blocking(program, args.iter().copied(), Duration::from_secs(15))?;
    if out.status_ok {
        Ok(())
    } else {
        warn!(
            program,
            stderr = %out.stderr.trim(),
            "screenshot fallback step failed"
        );
        Err(out.stderr)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// clipboard_get
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ClipboardGetArgs {}

#[derive(Debug, Serialize)]
pub struct ClipboardGetResult {
    pub text: String,
    pub truncated: bool,
}

/// Read the text contents of the X11 clipboard.
pub struct ClipboardGet;

const CLIPBOARD_MAX: usize = 4 * 1024;

impl TypedTool for ClipboardGet {
    type Args = ClipboardGetArgs;
    type Output = ClipboardGetResult;
    type Error = String;
    const NAME: &'static str = "clipboard_get";
    const DESCRIPTION: &'static str = "Read the text content of the user's X11 \
         clipboard (xclip -selection clipboard). Capped at 4KB; longer content \
         is truncated with a marker. The clipboard is NOT modified.";

    fn schema() -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn call(&self, _args: ClipboardGetArgs) -> Result<ClipboardGetResult, String> {
        let out = run_blocking("xclip", ["-selection", "clipboard", "-o"], DEFAULT_TIMEOUT)?;
        if !out.status_ok {
            // xclip exits non-zero when clipboard is empty; surface stderr.
            let msg = out.stderr.trim();
            if msg.is_empty() {
                return Ok(ClipboardGetResult {
                    text: String::new(),
                    truncated: false,
                });
            }
            return Err(format!("xclip failed: {msg}"));
        }
        let raw = out.stdout;
        if raw.len() <= CLIPBOARD_MAX {
            return Ok(ClipboardGetResult {
                text: raw,
                truncated: false,
            });
        }
        // Truncate at a char boundary to keep it valid UTF-8.
        let mut cut = CLIPBOARD_MAX;
        while cut > 0 && !raw.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut text = raw[..cut].to_string();
        text.push_str("\n…[truncated; clipboard exceeded 4KB cap]");
        Ok(ClipboardGetResult {
            text,
            truncated: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_validator_accepts_http_and_https_and_mailto() {
        assert!(validate_url("http://example.com").is_ok());
        assert!(validate_url("https://example.com/path?q=1").is_ok());
        assert!(validate_url("mailto:foo@example.com").is_ok());
    }

    #[test]
    fn url_validator_rejects_file_scheme() {
        let err = validate_url("file:///etc/passwd").unwrap_err();
        assert!(err.contains("file") || err.contains("not allowed"), "{err}");
    }

    #[test]
    fn url_validator_rejects_javascript_and_data() {
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("data:text/html,foo").is_err());
        assert!(validate_url("ftp://example.com").is_err());
    }

    #[test]
    fn url_validator_rejects_garbage() {
        assert!(validate_url("not a url").is_err());
    }

    #[test]
    fn open_app_rejects_empty_name() {
        let err = OpenApp.call(OpenAppArgs { name: "  ".into() }).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn fuzzy_desktop_id_returns_none_for_garbage() {
        // Random string nothing in /usr/share/applications should match.
        let id = fuzzy_desktop_id("zzzzz-no-such-app-zzzzz-zzzzz");
        assert!(id.is_none(), "unexpected match: {id:?}");
    }
}
