//! Built-in tools for agnt agents.
//!
//! All filesystem tools optionally hold an `Arc<FilesystemRoot>`; when set,
//! every user-supplied path is resolved through the sandbox before touching
//! `std::fs`. Default (unsandboxed) constructors are still provided for
//! development / REPL use, but their rustdoc carries an explicit warning.
//!
//! The [`Shell`] tool is gated behind the `shell` cargo feature and is
//! documented as CVE-class dangerous. See [`Shell::new_sandboxed`].

use agnt_core::Tool;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::sandbox::FilesystemRoot;

// ------------------------------------------------------------------------------------------------
// ReadFile
// ------------------------------------------------------------------------------------------------

const READ_FILE_MAX: usize = 256 * 1024;

/// Read a UTF-8 text file.
///
/// **Unsandboxed by default.** Without [`ReadFile::with_sandbox`] this tool
/// can read any file the process has access to. Pair with a
/// [`FilesystemRoot`] when exposing to untrusted LLM output.
pub struct ReadFile {
    sandbox: Option<Arc<FilesystemRoot>>,
}

impl Default for ReadFile {
    fn default() -> Self { Self::new() }
}

impl ReadFile {
    /// Unsandboxed constructor — full-host read access. Use only in trusted
    /// contexts.
    pub fn new() -> Self { Self { sandbox: None } }

    /// Sandboxed constructor — paths are resolved against `sandbox` and
    /// rejected if they escape the root.
    pub fn with_sandbox(sandbox: Arc<FilesystemRoot>) -> Self {
        Self { sandbox: Some(sandbox) }
    }
}

fn resolve_path(sandbox: &Option<Arc<FilesystemRoot>>, input: &str) -> Result<PathBuf, String> {
    match sandbox {
        Some(s) => s.resolve(input),
        None => Ok(PathBuf::from(input)),
    }
}

impl Tool for ReadFile {
    fn name(&self) -> &str { "read_file" }
    fn description(&self) -> &str {
        "Read a UTF-8 text file and return its contents. Truncated at 256KB. Prefer this over 'shell cat' — it is deterministic and cheaper."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "file path (must be under the agent sandbox root if one is configured)" }
            },
            "required": ["path"]
        })
    }
    fn call(&self, args: Value) -> Result<String, String> {
        let path = args["path"].as_str().ok_or("missing path")?;
        let resolved = resolve_path(&self.sandbox, path)?;
        let content = fs::read_to_string(&resolved)
            .map_err(|e| format!("read {}: {}", resolved.display(), e))?;
        if content.len() <= READ_FILE_MAX {
            return Ok(content);
        }
        let mut cut = READ_FILE_MAX;
        while cut > 0 && !content.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut out = content[..cut].to_string();
        out.push_str(&format!(
            "\n...(truncated at {} bytes; file is {} bytes total)",
            cut,
            content.len()
        ));
        Ok(out)
    }
}

// ------------------------------------------------------------------------------------------------
// EditFile — atomic (S6)
// ------------------------------------------------------------------------------------------------

/// Targeted file edit. Locks the file, re-reads under lock, verifies the
/// unique-match invariant, writes to a temp sibling, and atomically renames
/// into place — fixing the v0.1 TOCTOU race between read and write.
///
/// **Unsandboxed by default.** Use [`EditFile::with_sandbox`] when exposed to
/// hostile LLM output.
pub struct EditFile {
    sandbox: Option<Arc<FilesystemRoot>>,
}

impl Default for EditFile {
    fn default() -> Self { Self::new() }
}

impl EditFile {
    pub fn new() -> Self { Self { sandbox: None } }
    pub fn with_sandbox(sandbox: Arc<FilesystemRoot>) -> Self {
        Self { sandbox: Some(sandbox) }
    }
}

impl Tool for EditFile {
    fn name(&self) -> &str { "edit_file" }
    fn description(&self) -> &str {
        "Targeted file edit. Replaces one exact occurrence of 'old' with 'new' in the file. Fails if 'old' is not found or appears more than once — in that case pass more surrounding context in 'old' to make it unique. Prefer this over write_file when changing a small part of an existing file."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old":  { "type": "string", "description": "exact text to find (must be unique in the file)" },
                "new":  { "type": "string", "description": "replacement text" }
            },
            "required": ["path", "old", "new"]
        })
    }
    fn call(&self, args: Value) -> Result<String, String> {
        use fs2::FileExt;
        use std::io::Write;

        let path = args["path"].as_str().ok_or("missing path")?;
        let old = args["old"].as_str().ok_or("missing old")?;
        let new_s = args["new"].as_str().ok_or("missing new")?;
        if old.is_empty() {
            return Err("'old' must not be empty".into());
        }

        let resolved = resolve_path(&self.sandbox, path)?;

        // Lock a stable sibling lockfile. Locking the target file directly
        // does not work because atomic-rename swaps the inode — other waiters
        // would hold locks on the orphaned pre-rename file descriptor and
        // race past each other, clobbering the winner. The lockfile path is
        // derived from the target filename and stays put across renames.
        let lock_name = format!(
            ".{}.agnt-edit.lock",
            resolved
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("edit")
        );
        let lock_path = resolved
            .parent()
            .map(|p| p.join(&lock_name))
            .unwrap_or_else(|| PathBuf::from(&lock_name));

        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("lock open {}: {}", lock_path.display(), e))?;

        lock_file
            .lock_exclusive()
            .map_err(|e| format!("lock {}: {}", lock_path.display(), e))?;

        // Re-read the target under the lock. Any other writer that held the
        // lock before us has already renamed their result into place; we now
        // see their updated bytes.
        let perform = || -> Result<(String, String), String> {
            let content = std::fs::read_to_string(&resolved)
                .map_err(|e| format!("read {}: {}", resolved.display(), e))?;
            let count = content.matches(old).count();
            if count == 0 {
                return Err(format!("'old' not found in {}", resolved.display()));
            }
            if count > 1 {
                return Err(format!(
                    "'old' appears {} times in {}; pass more surrounding context to make it unique",
                    count,
                    resolved.display()
                ));
            }
            let updated = content.replacen(old, new_s, 1);

            // Write to sibling .tmp and atomically rename.
            let mut tmp = resolved.clone();
            let tmp_name = format!(
                "{}.agnt-edit-tmp.{}.{:?}",
                resolved
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("edit"),
                std::process::id(),
                std::thread::current().id()
            );
            tmp.set_file_name(tmp_name);
            {
                let mut tmpf = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&tmp)
                    .map_err(|e| format!("tmp open {}: {}", tmp.display(), e))?;
                tmpf.write_all(updated.as_bytes())
                    .map_err(|e| format!("tmp write: {}", e))?;
                tmpf.sync_all().map_err(|e| format!("tmp sync: {}", e))?;
            }
            std::fs::rename(&tmp, &resolved)
                .map_err(|e| format!("rename {} -> {}: {}", tmp.display(), resolved.display(), e))?;

            Ok((content, updated))
        };

        let res = perform();
        // Release lock before dropping file (drop would also release, but be explicit).
        let _ = lock_file.unlock();
        drop(lock_file);

        let (before, after) = res?;
        Ok(format!(
            "edited {} ({} bytes → {} bytes)",
            resolved.display(),
            before.len(),
            after.len()
        ))
    }
}

// ------------------------------------------------------------------------------------------------
// WriteFile
// ------------------------------------------------------------------------------------------------

pub struct WriteFile {
    sandbox: Option<Arc<FilesystemRoot>>,
}

impl Default for WriteFile {
    fn default() -> Self { Self::new() }
}

impl WriteFile {
    pub fn new() -> Self { Self { sandbox: None } }
    pub fn with_sandbox(sandbox: Arc<FilesystemRoot>) -> Self {
        Self { sandbox: Some(sandbox) }
    }
}

impl Tool for WriteFile {
    fn name(&self) -> &str { "write_file" }
    fn description(&self) -> &str { "Write UTF-8 content to a file, creating or overwriting it." }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }
    fn call(&self, args: Value) -> Result<String, String> {
        let path = args["path"].as_str().ok_or("missing path")?;
        let content = args["content"].as_str().ok_or("missing content")?;
        let resolved = resolve_path(&self.sandbox, path)?;
        fs::write(&resolved, content)
            .map_err(|e| format!("write {}: {}", resolved.display(), e))?;
        Ok(format!("wrote {} bytes to {}", content.len(), resolved.display()))
    }
}

// ------------------------------------------------------------------------------------------------
// ListDir
// ------------------------------------------------------------------------------------------------

pub struct ListDir {
    sandbox: Option<Arc<FilesystemRoot>>,
}

impl Default for ListDir {
    fn default() -> Self { Self::new() }
}

impl ListDir {
    pub fn new() -> Self { Self { sandbox: None } }
    pub fn with_sandbox(sandbox: Arc<FilesystemRoot>) -> Self {
        Self { sandbox: Some(sandbox) }
    }
}

impl Tool for ListDir {
    fn name(&self) -> &str { "list_dir" }
    fn description(&self) -> &str {
        "List a directory. One entry per line as 'TYPE NAME' where TYPE is F (file), D (dir), or L (symlink)."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "required": ["path"]
        })
    }
    fn call(&self, args: Value) -> Result<String, String> {
        let path = args["path"].as_str().ok_or("missing path")?;
        let resolved = resolve_path(&self.sandbox, path)?;
        let mut out = String::new();
        for entry in fs::read_dir(&resolved)
            .map_err(|e| format!("read_dir {}: {}", resolved.display(), e))?
        {
            let e = entry.map_err(|e| e.to_string())?;
            let ft = e.file_type().map_err(|e| e.to_string())?;
            let tag = if ft.is_dir() { 'D' } else if ft.is_symlink() { 'L' } else { 'F' };
            out.push_str(&format!("{} {}\n", tag, e.file_name().to_string_lossy()));
        }
        Ok(out)
    }
}

// ------------------------------------------------------------------------------------------------
// Shell (feature = "shell") — CVE-class dangerous, opt-in only
// ------------------------------------------------------------------------------------------------

/// Execute a shell-like command **without** invoking `sh -c`.
///
/// # !!! CVE-class dangerous !!!
///
/// This tool can execute arbitrary commands the LLM chooses. It is CVE-class
/// dangerous and must be paired with OS-level isolation (containers, seccomp,
/// bubblewrap, unshare, VMs — whatever is appropriate for the host). The
/// argv[0] allowlist implemented here is defense-in-depth, **not** a primary
/// security boundary.
///
/// Construction requires [`Shell::new_sandboxed`] — there is no "default"
/// constructor because there is no safe default. The allowlist and working
/// directory must be explicit.
///
/// ## What this tool guarantees
///
/// - `cmd` is parsed with `shell-words` (POSIX word splitting).
/// - `argv[0]` must appear in the caller-supplied `allowed_argv0` list.
/// - Any token containing `$`, `` ` ``, `|`, `;`, `&`, `>`, `<`, `(`, `)`, or
///   a newline is rejected (defense-in-depth against unexpected shell-ish
///   metacharacters that `shell-words` happens to pass through as literal
///   tokens).
/// - Execution uses `std::process::Command::new(argv[0]).args(&argv[1..])` —
///   **no `sh -c`**. There is no command-substitution / glob-expansion /
///   env-expansion surface inside this process.
/// - Working directory is pinned via `current_dir(&self.cwd)`.
///
/// ## What this tool does NOT guarantee
///
/// - The executed binary itself may be dangerous (e.g. `git clean -fdx`).
/// - The binary may spawn subprocesses or shells of its own.
/// - File-descriptor inheritance, environment variables, and kernel syscalls
///   are unrestricted. Pair with OS-level isolation.
#[cfg(feature = "shell")]
pub struct Shell {
    allowed_argv0: Vec<String>,
    cwd: PathBuf,
}

#[cfg(feature = "shell")]
impl Shell {
    /// Construct a sandboxed Shell tool.
    ///
    /// - `allowed_argv0`: the exact list of program names that may appear as
    ///   `argv[0]`. Matched as a literal string — no path resolution. Put
    ///   full paths here if you want to pin to `/usr/bin/git`.
    /// - `cwd`: the working directory every spawned process runs in. The
    ///   tool does not honour relative-path arguments from the model.
    ///
    /// # Safety
    ///
    /// This constructor is not `unsafe` in the Rust sense but carries the
    /// CVE-class warning from the struct-level docs. Do not call it without
    /// OS-level isolation in place.
    pub fn new_sandboxed(allowed_argv0: Vec<String>, cwd: PathBuf) -> Self {
        Self { allowed_argv0, cwd }
    }
}

#[cfg(feature = "shell")]
const SHELL_FORBIDDEN_CHARS: &[char] =
    &['$', '`', '|', ';', '&', '>', '<', '(', ')', '\n'];

#[cfg(feature = "shell")]
impl Tool for Shell {
    fn name(&self) -> &str { "shell" }
    fn description(&self) -> &str {
        "Run a program with arguments. The command is parsed with shell-words; argv[0] must be in the caller's allowlist; no sh -c, no command substitution, no pipes. Prefer specialized tools (read_file, grep, glob, fetch) over this."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "command line (e.g. 'git status' or 'cargo build --release')" }
            },
            "required": ["cmd"]
        })
    }
    fn call(&self, args: Value) -> Result<String, String> {
        let cmd = args["cmd"].as_str().ok_or("missing cmd")?;
        let argv = shell_words::split(cmd)
            .map_err(|e| format!("shell parse: {}", e))?;
        if argv.is_empty() {
            return Err("empty command".into());
        }
        for tok in &argv {
            if let Some(bad) = tok.chars().find(|c| SHELL_FORBIDDEN_CHARS.contains(c)) {
                return Err(format!(
                    "token contains forbidden character {:?}: {}",
                    bad, tok
                ));
            }
        }
        let argv0 = &argv[0];
        if !self.allowed_argv0.iter().any(|a| a == argv0) {
            return Err(format!(
                "argv[0] {:?} not in allowlist {:?}",
                argv0, self.allowed_argv0
            ));
        }

        let out = std::process::Command::new(argv0)
            .args(&argv[1..])
            .current_dir(&self.cwd)
            .output()
            .map_err(|e| format!("spawn: {}", e))?;
        let status = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        Ok(format!(
            "exit: {}\n--- stdout ---\n{}--- stderr ---\n{}",
            status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ))
    }
}

// ------------------------------------------------------------------------------------------------
// Glob
// ------------------------------------------------------------------------------------------------

pub struct Glob {
    sandbox: Option<Arc<FilesystemRoot>>,
}

impl Default for Glob {
    fn default() -> Self { Self::new() }
}

impl Glob {
    pub fn new() -> Self { Self { sandbox: None } }
    pub fn with_sandbox(sandbox: Arc<FilesystemRoot>) -> Self {
        Self { sandbox: Some(sandbox) }
    }
}

impl Tool for Glob {
    fn name(&self) -> &str { "glob" }
    fn description(&self) -> &str {
        "Find files matching a shell-style glob pattern (e.g. 'src/**/*.rs', '**/Cargo.toml'). Returns one path per line. Prefer this over 'shell find' — it is faster, portable across OSes, and has no command-injection surface."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "glob pattern (must be relative to the sandbox root when sandboxed)" }
            },
            "required": ["pattern"]
        })
    }
    fn call(&self, args: Value) -> Result<String, String> {
        let pattern = args["pattern"].as_str().ok_or("missing pattern")?;

        // When sandboxed, require the pattern to be a relative path under the
        // root. We anchor by joining the pattern onto the root and checking
        // that it stays under the root (which also rejects `..`).
        let (effective_pattern, root_strip): (String, Option<PathBuf>) = match &self.sandbox {
            Some(s) => {
                if Path::new(pattern).is_absolute() {
                    return Err(format!(
                        "glob pattern must be relative when sandboxed: {}",
                        pattern
                    ));
                }
                if pattern.split('/').any(|seg| seg == "..") {
                    return Err(format!("glob pattern contains '..': {}", pattern));
                }
                let joined = s.root().join(pattern);
                let eff = joined.to_string_lossy().into_owned();
                (eff, Some(s.root().to_path_buf()))
            }
            None => (pattern.to_string(), None),
        };

        let mut out = String::new();
        let mut count = 0usize;
        for entry in glob::glob(&effective_pattern).map_err(|e| format!("glob: {}", e))? {
            let p = match entry {
                Ok(p) => p,
                Err(_) => continue,
            };
            // Double-check sandbox containment post-expansion (defensive —
            // glob should never escape, but symlinks could surface).
            if let Some(root) = &root_strip {
                if let Ok(canonical) = std::fs::canonicalize(&p) {
                    if !canonical.starts_with(root) {
                        continue;
                    }
                }
            }
            out.push_str(&p.to_string_lossy());
            out.push('\n');
            count += 1;
            if count >= 2000 {
                out.push_str("(truncated at 2000)\n");
                break;
            }
        }
        if out.is_empty() {
            Ok("(no matches)".into())
        } else {
            Ok(out)
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Grep
// ------------------------------------------------------------------------------------------------

pub struct Grep {
    sandbox: Option<Arc<FilesystemRoot>>,
}

impl Default for Grep {
    fn default() -> Self { Self::new() }
}

impl Grep {
    pub fn new() -> Self { Self { sandbox: None } }
    pub fn with_sandbox(sandbox: Arc<FilesystemRoot>) -> Self {
        Self { sandbox: Some(sandbox) }
    }
}

impl Tool for Grep {
    fn name(&self) -> &str { "grep" }
    fn description(&self) -> &str {
        "Search text files under a directory for a regex pattern. Returns 'path:line:text' per match. Optional 'ext' filter (e.g. 'rs', 'md'). Prefer this over 'shell grep' — it is native, typically under 1ms for a source tree, and avoids quoting pitfalls."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "regex pattern" },
                "path":    { "type": "string", "description": "root directory to walk" },
                "ext":     { "type": "string", "description": "optional file extension filter without dot" }
            },
            "required": ["pattern", "path"]
        })
    }
    fn call(&self, args: Value) -> Result<String, String> {
        let pattern = args["pattern"].as_str().ok_or("missing pattern")?;
        let path = args["path"].as_str().ok_or("missing path")?;
        let ext = args["ext"].as_str();
        let resolved = resolve_path(&self.sandbox, path)?;
        let re = regex::Regex::new(pattern).map_err(|e| format!("regex: {}", e))?;
        let mut out = String::new();
        let mut count = 0usize;
        for entry in walkdir::WalkDir::new(&resolved)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() { continue; }
            if let Some(e) = ext {
                if entry.path().extension().and_then(|s| s.to_str()) != Some(e) { continue; }
            }
            // When sandboxed, skip any file whose canonical path escapes the root.
            if let Some(sbx) = &self.sandbox {
                if let Ok(canonical) = std::fs::canonicalize(entry.path()) {
                    if !canonical.starts_with(sbx.root()) {
                        continue;
                    }
                }
            }
            let content = match fs::read_to_string(entry.path()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (i, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    out.push_str(&format!("{}:{}:{}\n", entry.path().display(), i + 1, line));
                    count += 1;
                    if count >= 500 {
                        out.push_str("(truncated at 500 matches)\n");
                        return Ok(out);
                    }
                }
            }
        }
        if out.is_empty() {
            Ok("(no matches)".into())
        } else {
            Ok(out)
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Fetch — SSRF guarded (S3)
// ------------------------------------------------------------------------------------------------

/// HTTP GET a URL with an SSRF guard.
///
/// Rejects non-http(s) schemes, rejects URLs whose DNS resolution returns
/// any IP in the loopback / private / link-local / unspecified / multicast
/// ranges, and explicitly blocklists the cloud metadata endpoints. Redirects
/// are disabled on the underlying ureq agent so attackers cannot bypass the
/// resolved-IP check via `302 Location: http://169.254.169.254/…`.
pub struct Fetch {
    allow_hosts: Option<Vec<String>>,
    max_bytes: usize,
}

const FETCH_DEFAULT_MAX: usize = 64 * 1024;

impl Default for Fetch {
    fn default() -> Self { Self::new() }
}

impl Fetch {
    pub fn new() -> Self {
        Self { allow_hosts: None, max_bytes: FETCH_DEFAULT_MAX }
    }

    /// Restrict fetches to an explicit host allowlist. When set, any URL
    /// whose host (case-insensitive) is not in the list is rejected before
    /// DNS resolution.
    pub fn with_allow_hosts(mut self, hosts: Vec<String>) -> Self {
        self.allow_hosts = Some(hosts.into_iter().map(|h| h.to_lowercase()).collect());
        self
    }

    /// Set the maximum number of response bytes to read. Defaults to 64KB.
    pub fn with_max_bytes(mut self, n: usize) -> Self {
        self.max_bytes = n;
        self
    }
}

fn ssrf_check(url: &str, allow_hosts: &Option<Vec<String>>) -> Result<(), String> {
    use std::net::ToSocketAddrs;

    let parsed = url::Url::parse(url).map_err(|e| format!("url parse: {}", e))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("rejected scheme: {}", scheme));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "url has no host".to_string())?
        .to_lowercase();

    // Explicit metadata blocklist (covers the name-based GCP endpoint that
    // would otherwise resolve to a non-private IP that happens to be routable
    // only from inside the VM).
    if host == "metadata.google.internal" || host == "169.254.169.254" {
        return Err(format!("rejected metadata host: {}", host));
    }

    if let Some(allow) = allow_hosts {
        if !allow.iter().any(|h| h == &host) {
            return Err(format!("host {} not in allowlist", host));
        }
    }

    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {}: {}", host, e))?;

    let mut any = false;
    for sa in addrs {
        any = true;
        let ip = sa.ip();
        if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
            return Err(format!("rejected IP {} for {}", ip, host));
        }
        match ip {
            std::net::IpAddr::V4(v4) => {
                if v4.is_private() || v4.is_link_local() || v4.is_broadcast() {
                    return Err(format!("rejected IPv4 {} for {}", v4, host));
                }
                // 169.254.169.254 is already is_link_local; explicit belt-and-suspenders:
                if v4.octets() == [169, 254, 169, 254] {
                    return Err(format!("rejected AWS metadata IP for {}", host));
                }
            }
            std::net::IpAddr::V6(v6) => {
                // No stable is_private / is_unique_local on stable std yet.
                // Reject ULA (fc00::/7) and link-local (fe80::/10) by prefix.
                let seg0 = v6.segments()[0];
                if (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80 {
                    return Err(format!("rejected IPv6 {} for {}", v6, host));
                }
            }
        }
    }
    if !any {
        return Err(format!("no addresses for {}", host));
    }
    Ok(())
}

impl Tool for Fetch {
    fn name(&self) -> &str { "fetch" }
    fn description(&self) -> &str {
        "HTTP GET a URL and return the response body (first 64KB by default). Rejects loopback / private / link-local / metadata hosts."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" }
            },
            "required": ["url"]
        })
    }
    fn call(&self, args: Value) -> Result<String, String> {
        use std::io::Read;
        let url = args["url"].as_str().ok_or("missing url")?;
        ssrf_check(url, &self.allow_hosts)?;
        let resp = crate::http::agent()
            .get(url)
            .call()
            .map_err(|e| format!("fetch: {}", e))?;
        let status = resp.status();
        let mut body = String::new();
        resp.into_reader()
            .take(self.max_bytes as u64)
            .read_to_string(&mut body)
            .map_err(|e| format!("read: {}", e))?;
        Ok(format!("HTTP {}\n{}", status, body))
    }
}

// ------------------------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agnt-tools-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    // ---- S2: sandbox enforcement ----------------------------------------------------------

    #[test]
    fn sandbox_blocks_read_of_etc_shadow() {
        let dir = tmpdir("sbx-read");
        let sbx = Arc::new(FilesystemRoot::new(&dir).unwrap());
        let tool = ReadFile::with_sandbox(sbx);
        let res = tool.call(json!({"path":"/etc/shadow"}));
        assert!(res.is_err(), "expected sandbox rejection");
    }

    #[test]
    fn sandbox_blocks_write_outside_root() {
        let dir = tmpdir("sbx-write");
        let sbx = Arc::new(FilesystemRoot::new(&dir).unwrap());
        let tool = WriteFile::with_sandbox(sbx);
        let res = tool.call(json!({"path":"../escape.txt","content":"x"}));
        assert!(res.is_err());
    }

    #[test]
    fn sandbox_allows_read_under_root() {
        let dir = tmpdir("sbx-ok");
        fs::write(dir.join("hello.txt"), "world").unwrap();
        let sbx = Arc::new(FilesystemRoot::new(&dir).unwrap());
        let tool = ReadFile::with_sandbox(sbx);
        let out = tool.call(json!({"path":"hello.txt"})).unwrap();
        assert_eq!(out, "world");
    }

    #[test]
    fn sandbox_blocks_listdir_of_root() {
        let dir = tmpdir("sbx-ls");
        let sbx = Arc::new(FilesystemRoot::new(&dir).unwrap());
        let tool = ListDir::with_sandbox(sbx);
        assert!(tool.call(json!({"path":"/"})).is_err());
    }

    #[test]
    fn sandbox_blocks_glob_absolute() {
        let dir = tmpdir("sbx-glob");
        let sbx = Arc::new(FilesystemRoot::new(&dir).unwrap());
        let tool = Glob::with_sandbox(sbx);
        assert!(tool.call(json!({"pattern":"/etc/*"})).is_err());
    }

    #[test]
    fn sandbox_blocks_glob_parent_traversal() {
        let dir = tmpdir("sbx-glob2");
        let sbx = Arc::new(FilesystemRoot::new(&dir).unwrap());
        let tool = Glob::with_sandbox(sbx);
        assert!(tool.call(json!({"pattern":"../*"})).is_err());
    }

    #[test]
    fn sandbox_blocks_grep_root() {
        let dir = tmpdir("sbx-grep");
        let sbx = Arc::new(FilesystemRoot::new(&dir).unwrap());
        let tool = Grep::with_sandbox(sbx);
        assert!(tool.call(json!({"pattern":"root:","path":"/etc"})).is_err());
    }

    // ---- S3: Fetch SSRF guard -------------------------------------------------------------

    #[test]
    fn fetch_rejects_aws_metadata_ip() {
        let tool = Fetch::new();
        let err = tool
            .call(json!({"url":"http://169.254.169.254/latest/meta-data/"}))
            .unwrap_err();
        assert!(err.contains("metadata") || err.contains("link") || err.contains("169.254"));
    }

    #[test]
    fn fetch_rejects_gcp_metadata_name() {
        let tool = Fetch::new();
        let err = tool
            .call(json!({"url":"http://metadata.google.internal/"}))
            .unwrap_err();
        assert!(err.contains("metadata"));
    }

    #[test]
    fn fetch_rejects_loopback() {
        let tool = Fetch::new();
        let err = tool.call(json!({"url":"http://127.0.0.1:11434/"})).unwrap_err();
        assert!(err.contains("IP") || err.contains("loopback") || err.contains("127"));
    }

    #[test]
    fn fetch_rejects_private_ipv4() {
        let tool = Fetch::new();
        let err = tool.call(json!({"url":"http://192.168.1.1/"})).unwrap_err();
        assert!(err.contains("IPv4") || err.contains("192.168") || err.contains("private"));
    }

    #[test]
    fn fetch_rejects_file_scheme() {
        let tool = Fetch::new();
        let err = tool.call(json!({"url":"file:///etc/passwd"})).unwrap_err();
        assert!(err.contains("scheme"));
    }

    #[test]
    fn fetch_rejects_localhost_name() {
        let tool = Fetch::new();
        let err = tool.call(json!({"url":"http://localhost:6379/"})).unwrap_err();
        assert!(err.contains("IP") || err.contains("loopback") || err.contains("127"));
    }

    #[test]
    fn fetch_allowlist_blocks_non_matching_host_before_dns() {
        let tool = Fetch::new().with_allow_hosts(vec!["example.com".into()]);
        let err = tool.call(json!({"url":"http://metadata.google.internal/"})).unwrap_err();
        // metadata is blocked by explicit list first; check allowlist on benign host
        assert!(err.contains("metadata"));
        let tool2 = Fetch::new().with_allow_hosts(vec!["example.com".into()]);
        let err2 = tool2.call(json!({"url":"http://not-on-list.invalid/"})).unwrap_err();
        assert!(err2.contains("allowlist") || err2.contains("not-on-list"));
    }

    // ---- S6: EditFile atomicity ----------------------------------------------------------

    #[test]
    fn edit_file_unique_match() {
        let dir = tmpdir("edit-unique");
        let p = dir.join("f.txt");
        fs::write(&p, "hello world").unwrap();
        let tool = EditFile::new();
        tool.call(json!({"path": p.to_str().unwrap(), "old":"world", "new":"agnt"})).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello agnt");
    }

    #[test]
    fn edit_file_concurrent_stress() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

        let dir = tmpdir("edit-stress");
        let path = dir.join("race.txt");
        // Run 100 race-rounds, 4 threads each trying to replace the same
        // unique marker with their own value. Under the lock + atomic-rename
        // semantics, exactly one winner should be observed per round.
        for round in 0..100 {
            fs::write(&path, format!("start-{}-MARK-end", round)).unwrap();
            let winners = Arc::new(AtomicUsize::new(0));
            thread::scope(|s| {
                for tid in 0..4 {
                    let path = path.clone();
                    let winners = winners.clone();
                    s.spawn(move || {
                        let tool = EditFile::new();
                        let res = tool.call(json!({
                            "path": path.to_str().unwrap(),
                            "old": "MARK",
                            "new": format!("T{}", tid),
                        }));
                        if res.is_ok() {
                            winners.fetch_add(1, Ordering::SeqCst);
                        }
                    });
                }
            });
            assert_eq!(
                winners.load(Ordering::SeqCst),
                1,
                "expected exactly one winner per round, got {} on round {}",
                winners.load(Ordering::SeqCst),
                round
            );
            let final_content = fs::read_to_string(&path).unwrap();
            assert!(!final_content.contains("MARK"), "marker should be replaced");
        }
    }

    // ---- S1: Shell feature gate (only runs when shell feature enabled) ------------------

    #[cfg(feature = "shell")]
    #[test]
    fn shell_rejects_unknown_argv0() {
        let s = Shell::new_sandboxed(vec!["echo".into()], std::env::temp_dir());
        assert!(s.call(json!({"cmd":"rm -rf /"})).is_err());
    }

    #[cfg(feature = "shell")]
    #[test]
    fn shell_rejects_command_substitution() {
        let s = Shell::new_sandboxed(vec!["echo".into()], std::env::temp_dir());
        // shell-words will keep $(...) as a single token; our char filter rejects $
        let err = s.call(json!({"cmd":"echo $(whoami)"})).unwrap_err();
        assert!(err.contains("forbidden"));
    }

    #[cfg(feature = "shell")]
    #[test]
    fn shell_rejects_pipe() {
        let s = Shell::new_sandboxed(vec!["echo".into()], std::env::temp_dir());
        let err = s.call(json!({"cmd":"echo hi | cat"})).unwrap_err();
        assert!(err.contains("forbidden") || err.contains("allowlist"));
    }

    #[cfg(feature = "shell")]
    #[test]
    fn shell_allowlisted_echo_runs() {
        let s = Shell::new_sandboxed(vec!["echo".into()], std::env::temp_dir());
        let out = s.call(json!({"cmd":"echo hello"})).unwrap();
        assert!(out.contains("hello"));
    }
}
