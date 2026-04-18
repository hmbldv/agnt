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

use crate::sandbox::{FilesystemRoot, SandboxedPath};

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
    sandbox: SandboxedPath,
}

impl Default for ReadFile {
    fn default() -> Self { Self::new() }
}

impl ReadFile {
    /// Unsandboxed constructor — full-host read access. Use only in trusted
    /// contexts.
    pub fn new() -> Self { Self { sandbox: SandboxedPath::new() } }

    /// Sandboxed constructor — paths are resolved against `sandbox` and
    /// rejected if they escape the root.
    pub fn with_sandbox(sandbox: Arc<FilesystemRoot>) -> Self {
        Self { sandbox: SandboxedPath::with_root(sandbox) }
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
        let resolved = self.sandbox.resolve(path)?;
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
///
/// ## Lockfile name is predictable
///
/// The sidecar lock lives at `.<filename>.agnt-edit.lock` in the same
/// directory as the target. The name is deterministic by design — it
/// has to be, so two agnt processes editing the same file on the same
/// host coordinate correctly. The tradeoff is that a *different* local
/// process on the same machine can pre-create the lockfile and hold
/// the exclusive lock, causing every `EditFile` call on that target
/// to block or fail. That is a local-user DoS, not a sandbox escape:
/// it requires write access to the target's parent directory, which
/// is already out of the agent's threat model (v0.2 Threat Model §
/// "local untrusted users"). If you need multi-tenant isolation, put
/// each agent in its own bwrap/container/landlock view — the lockfile
/// pattern is designed for the single-tenant case.
pub struct EditFile {
    sandbox: SandboxedPath,
}

impl Default for EditFile {
    fn default() -> Self { Self::new() }
}

impl EditFile {
    pub fn new() -> Self { Self { sandbox: SandboxedPath::new() } }
    pub fn with_sandbox(sandbox: Arc<FilesystemRoot>) -> Self {
        Self { sandbox: SandboxedPath::with_root(sandbox) }
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

        let resolved = self.sandbox.resolve(path)?;

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
    sandbox: SandboxedPath,
}

impl Default for WriteFile {
    fn default() -> Self { Self::new() }
}

impl WriteFile {
    pub fn new() -> Self { Self { sandbox: SandboxedPath::new() } }
    pub fn with_sandbox(sandbox: Arc<FilesystemRoot>) -> Self {
        Self { sandbox: SandboxedPath::with_root(sandbox) }
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
        let resolved = self.sandbox.resolve(path)?;
        fs::write(&resolved, content)
            .map_err(|e| format!("write {}: {}", resolved.display(), e))?;
        Ok(format!("wrote {} bytes to {}", content.len(), resolved.display()))
    }
}

// ------------------------------------------------------------------------------------------------
// ListDir
// ------------------------------------------------------------------------------------------------

pub struct ListDir {
    sandbox: SandboxedPath,
}

impl Default for ListDir {
    fn default() -> Self { Self::new() }
}

impl ListDir {
    pub fn new() -> Self { Self { sandbox: SandboxedPath::new() } }
    pub fn with_sandbox(sandbox: Arc<FilesystemRoot>) -> Self {
        Self { sandbox: SandboxedPath::with_root(sandbox) }
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
        let resolved = self.sandbox.resolve(path)?;
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
    /// v0.3 C1: optional bubblewrap configuration. When set, the call
    /// wraps the spawned command in `bwrap` with a read-only rootfs and a
    /// scoped bind mount of `cwd`. Linux only.
    #[cfg(all(feature = "bwrap-shell", target_os = "linux"))]
    bwrap: Option<BwrapConfig>,
}

/// Bubblewrap configuration for the Shell tool (v0.3 C1).
///
/// Wraps the allowed command in `bwrap` with:
/// - `--ro-bind /usr /usr`, `--ro-bind /bin /bin`, `--ro-bind /lib /lib`,
///   `--ro-bind /lib64 /lib64`, `--ro-bind /etc /etc` (when present)
/// - `--bind <cwd> <cwd>` (read-write bind of the sandboxed working dir)
/// - `--tmpfs /tmp`, `--proc /proc`, `--dev /dev`
/// - `--unshare-all` optionally modified by `share_net`
/// - `--die-with-parent`
/// - `--chdir <cwd>`
#[cfg(all(feature = "bwrap-shell", target_os = "linux"))]
#[derive(Debug, Clone)]
pub struct BwrapConfig {
    /// Whether to share the host's network namespace with the sandboxed
    /// process. Set to `false` to deny network access entirely — at which
    /// point every network-reaching command (`curl`, `git fetch`, etc.)
    /// will fail inside the sandbox.
    pub share_net: bool,
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
        Self {
            allowed_argv0,
            cwd,
            #[cfg(all(feature = "bwrap-shell", target_os = "linux"))]
            bwrap: None,
        }
    }

    /// v0.3 C1: construct a Shell wrapped in bubblewrap for OS-level
    /// defense-in-depth on top of the argv allowlist.
    ///
    /// Returns an error if `bwrap` is not available on the host PATH.
    /// Only available when the `bwrap-shell` feature is enabled AND the
    /// target is Linux — `bwrap` is a Linux-only tool.
    ///
    /// The sandbox bind-mounts `cwd` read-write; everything else under
    /// `/usr`, `/bin`, `/lib`, `/lib64`, `/etc` is read-only. `/tmp` is a
    /// tmpfs. No `/home`, no `/var`, no `/root` — set `cwd` to a
    /// scratch directory and bind-mount additional paths manually by
    /// extending [`BwrapConfig`] in your fork if you need more.
    ///
    /// This is CVE-class and must be paired with the v0.2 argv allowlist.
    /// The sandbox is defense in depth, NOT a primary boundary.
    #[cfg(all(feature = "bwrap-shell", target_os = "linux"))]
    pub fn new_bwrap(
        allowed_argv0: Vec<String>,
        cwd: PathBuf,
        share_net: bool,
    ) -> Result<Self, String> {
        // Probe for bwrap on PATH.
        let probe = std::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .map_err(|e| format!("bwrap not available: {}", e))?;
        if !probe.status.success() {
            return Err(format!(
                "bwrap --version exited {}",
                probe.status.code().unwrap_or(-1)
            ));
        }
        Ok(Self {
            allowed_argv0,
            cwd,
            bwrap: Some(BwrapConfig { share_net }),
        })
    }

    /// Non-Linux stub for `new_bwrap` — compile-error at call time via a
    /// clear message rather than hiding the method entirely. Only compiled
    /// when the `bwrap-shell` feature is enabled on a non-Linux target.
    #[cfg(all(feature = "bwrap-shell", not(target_os = "linux")))]
    pub fn new_bwrap(
        _allowed_argv0: Vec<String>,
        _cwd: PathBuf,
        _share_net: bool,
    ) -> Result<Self, String> {
        Err("bwrap sandbox is Linux-only".into())
    }

    /// Build the bwrap argv vector that wraps `argv` with this shell's
    /// bubblewrap config. Pure function, no I/O — used by both `call()`
    /// and the unit tests.
    #[cfg(all(feature = "bwrap-shell", target_os = "linux"))]
    fn build_bwrap_argv(
        cfg: &BwrapConfig,
        cwd: &std::path::Path,
        argv: &[String],
    ) -> Vec<String> {
        let cwd_str = cwd.to_string_lossy().into_owned();
        let mut out: Vec<String> = vec![
            "--ro-bind".into(), "/usr".into(), "/usr".into(),
            "--ro-bind".into(), "/bin".into(), "/bin".into(),
            "--ro-bind-try".into(), "/lib".into(), "/lib".into(),
            "--ro-bind-try".into(), "/lib64".into(), "/lib64".into(),
            "--ro-bind-try".into(), "/etc".into(), "/etc".into(),
            "--bind".into(), cwd_str.clone(), cwd_str.clone(),
            "--tmpfs".into(), "/tmp".into(),
            "--proc".into(), "/proc".into(),
            "--dev".into(), "/dev".into(),
            "--unshare-all".into(),
        ];
        if cfg.share_net {
            out.push("--share-net".into());
        }
        out.push("--die-with-parent".into());
        out.push("--chdir".into());
        out.push(cwd_str);
        out.push("--".into());
        out.extend(argv.iter().cloned());
        out
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

        // v0.3 C1: when bwrap is configured, wrap the command. Otherwise
        // spawn directly with the argv allowlist constraint only.
        #[cfg(all(feature = "bwrap-shell", target_os = "linux"))]
        let out = if let Some(cfg) = &self.bwrap {
            let bwrap_argv = Self::build_bwrap_argv(cfg, &self.cwd, &argv);
            std::process::Command::new("bwrap")
                .args(&bwrap_argv)
                .output()
                .map_err(|e| format!("bwrap spawn: {}", e))?
        } else {
            std::process::Command::new(argv0)
                .args(&argv[1..])
                .current_dir(&self.cwd)
                .output()
                .map_err(|e| format!("spawn: {}", e))?
        };

        #[cfg(not(all(feature = "bwrap-shell", target_os = "linux")))]
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
    sandbox: SandboxedPath,
}

impl Default for Glob {
    fn default() -> Self { Self::new() }
}

impl Glob {
    pub fn new() -> Self { Self { sandbox: SandboxedPath::new() } }
    pub fn with_sandbox(sandbox: Arc<FilesystemRoot>) -> Self {
        Self { sandbox: SandboxedPath::with_root(sandbox) }
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
        let (effective_pattern, root_strip): (String, Option<PathBuf>) = match self.sandbox.root() {
            Some(root) => {
                if Path::new(pattern).is_absolute() {
                    return Err(format!(
                        "glob pattern must be relative when sandboxed: {}",
                        pattern
                    ));
                }
                if pattern.split('/').any(|seg| seg == "..") {
                    return Err(format!("glob pattern contains '..': {}", pattern));
                }
                let joined = root.join(pattern);
                let eff = joined.to_string_lossy().into_owned();
                (eff, Some(root.to_path_buf()))
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
    sandbox: SandboxedPath,
}

impl Default for Grep {
    fn default() -> Self { Self::new() }
}

impl Grep {
    pub fn new() -> Self { Self { sandbox: SandboxedPath::new() } }
    pub fn with_sandbox(sandbox: Arc<FilesystemRoot>) -> Self {
        Self { sandbox: SandboxedPath::with_root(sandbox) }
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
        let resolved = self.sandbox.resolve(path)?;
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
            if let Some(root) = self.sandbox.root() {
                if let Ok(canonical) = std::fs::canonicalize(entry.path()) {
                    if !canonical.starts_with(root) {
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

/// HTTP GET a URL with an atomic SSRF guard.
///
/// v0.3.1 closes the v0.2/v0.3 two-phase TOCTOU by installing a custom
/// [`ureq::Resolver`] ([`crate::ssrf::SsrfResolver`]) on the underlying
/// agent. ureq calls the resolver exactly once per connection, uses the
/// exact addresses it returns, and never performs a second DNS lookup.
/// That removes the DNS-rebinding window a short-TTL authority could
/// previously use to flip a public check-time IP to a private
/// request-time IP.
///
/// Each `Fetch` instance lazily builds its own `ureq::Agent` on first
/// call, so a per-instance `allow_hosts` allowlist composes cleanly.
/// Redirects are disabled (`redirects(0)`) so a `302 Location:` hop
/// cannot bypass the resolver.
///
/// URL-shape validation (scheme allowlist, parsing) still happens
/// up-front in `Fetch::call` because the resolver only sees the
/// `host:port` netloc, not the scheme.
pub struct Fetch {
    allow_hosts: Option<Vec<String>>,
    max_bytes: usize,
    // Lazily initialised so the tool is cheap to construct and so the
    // agent's resolver captures the final allow_hosts configured via the
    // builder-style setters.
    agent: std::sync::OnceLock<ureq::Agent>,
}

const FETCH_DEFAULT_MAX: usize = 64 * 1024;

impl Default for Fetch {
    fn default() -> Self { Self::new() }
}

impl Fetch {
    pub fn new() -> Self {
        Self {
            allow_hosts: None,
            max_bytes: FETCH_DEFAULT_MAX,
            agent: std::sync::OnceLock::new(),
        }
    }

    /// Restrict fetches to an explicit host allowlist. Case-insensitive.
    ///
    /// The allowlist is enforced inside the custom resolver before any
    /// DNS query is issued, so a rejected host never triggers a lookup.
    pub fn with_allow_hosts(mut self, hosts: Vec<String>) -> Self {
        self.allow_hosts = Some(hosts.into_iter().map(|h| h.to_lowercase()).collect());
        self
    }

    /// Set the maximum number of response bytes to read. Defaults to 64KB.
    pub fn with_max_bytes(mut self, n: usize) -> Self {
        self.max_bytes = n;
        self
    }

    /// Build the ureq agent for this Fetch instance. Installs
    /// [`SsrfResolver`] so DNS resolution is atomic with validation.
    ///
    /// [`SsrfResolver`]: crate::ssrf::SsrfResolver
    fn agent(&self) -> &ureq::Agent {
        self.agent.get_or_init(|| {
            let resolver = match &self.allow_hosts {
                Some(list) => crate::ssrf::SsrfResolver::with_allow_hosts(list.clone()),
                None => crate::ssrf::SsrfResolver::new(),
            };
            let builder = ureq::AgentBuilder::new()
                .resolver(resolver)
                .redirects(0);
            match native_tls::TlsConnector::new() {
                Ok(connector) => builder.tls_connector(Arc::new(connector)).build(),
                Err(_) => builder.build(),
            }
        })
    }
}

/// Upfront URL-shape validation. The atomic IP / host check lives in
/// [`crate::ssrf::SsrfResolver`]; this function only catches things the
/// resolver cannot see from a netloc alone — primarily the scheme and
/// malformed URLs.
fn fetch_url_shape_check(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("url parse: {}", e))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("rejected scheme: {}", scheme));
    }
    if parsed.host_str().is_none() {
        return Err("url has no host".to_string());
    }
    Ok(())
}

impl Tool for Fetch {
    fn name(&self) -> &str { "fetch" }
    fn description(&self) -> &str {
        "HTTP GET a URL and return the response body (first 64KB by default). Rejects loopback / private / link-local / metadata hosts atomically via a custom DNS resolver."
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
        fetch_url_shape_check(url)?;
        let resp = self
            .agent()
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

    #[test]
    fn fetch_uses_ssrf_resolver_atomically() {
        // v0.3.1: ureq's custom Resolver is the ONLY DNS path the agent
        // uses. This test verifies the wired-up agent rejects a private
        // IP via the resolver (not the old pre-check) by ensuring the
        // returned error carries the resolver's message.
        //
        // 10.0.0.1 isn't actually resolved by the system; we use a raw
        // IP so ToSocketAddrs skips DNS and hits validate_addrs directly,
        // proving the resolver is on the code path.
        let tool = Fetch::new();
        let err = tool.call(json!({"url":"http://10.0.0.1/"})).unwrap_err();
        assert!(
            err.contains("IPv4") || err.contains("10.0.0.1") || err.contains("private"),
            "error should come from SsrfResolver: {}",
            err
        );
    }

    #[test]
    fn fetch_ipv6_literal_loopback_rejected() {
        let tool = Fetch::new();
        let err = tool.call(json!({"url":"http://[::1]/"})).unwrap_err();
        assert!(err.contains("loopback") || err.contains("::1"), "got: {}", err);
    }

    #[test]
    fn fetch_ipv6_literal_ula_rejected() {
        let tool = Fetch::new();
        let err = tool.call(json!({"url":"http://[fc00::1]/"})).unwrap_err();
        assert!(err.contains("IPv6") || err.contains("fc00"), "got: {}", err);
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

    // ---- C1: bubblewrap sandbox ---------------------------------------------------------

    #[cfg(all(feature = "bwrap-shell", target_os = "linux"))]
    #[test]
    fn bwrap_argv_contains_core_ro_binds_and_unshare() {
        let cfg = BwrapConfig { share_net: false };
        let cwd = PathBuf::from("/tmp/workdir-xyz");
        let argv = vec!["echo".to_string(), "hi".to_string()];
        let out = Shell::build_bwrap_argv(&cfg, &cwd, &argv);
        // Core read-only system binds
        assert!(out.windows(3).any(|w| w == ["--ro-bind", "/usr", "/usr"]));
        assert!(out.windows(3).any(|w| w == ["--ro-bind", "/bin", "/bin"]));
        // Isolation
        assert!(out.iter().any(|s| s == "--unshare-all"));
        assert!(out.iter().any(|s| s == "--die-with-parent"));
        assert!(out.iter().any(|s| s == "--tmpfs"));
        // cwd gets bound and chdir'd
        assert!(out.windows(3).any(|w| w[0] == "--bind" && w[1] == "/tmp/workdir-xyz"));
        let chdir_pos = out.iter().position(|s| s == "--chdir").expect("chdir");
        assert_eq!(out[chdir_pos + 1], "/tmp/workdir-xyz");
        // `--` separator then the wrapped argv at the tail
        let sep = out.iter().rposition(|s| s == "--").expect("-- sep");
        assert_eq!(&out[sep + 1..], &["echo".to_string(), "hi".to_string()][..]);
    }

    #[cfg(all(feature = "bwrap-shell", target_os = "linux"))]
    #[test]
    fn bwrap_share_net_flag_toggles() {
        let cwd = PathBuf::from("/tmp/nw");
        let argv = vec!["echo".to_string()];
        let off = Shell::build_bwrap_argv(&BwrapConfig { share_net: false }, &cwd, &argv);
        assert!(!off.iter().any(|s| s == "--share-net"));
        let on = Shell::build_bwrap_argv(&BwrapConfig { share_net: true }, &cwd, &argv);
        assert!(on.iter().any(|s| s == "--share-net"));
    }

    #[cfg(all(feature = "bwrap-shell", target_os = "linux"))]
    #[test]
    #[ignore = "requires bwrap installed locally"]
    fn bwrap_echo_runs_under_sandbox() {
        let s = Shell::new_bwrap(vec!["echo".into()], std::env::temp_dir(), false)
            .expect("bwrap must be installed to run this test");
        let out = s.call(json!({"cmd":"echo sandboxed"})).unwrap();
        assert!(out.contains("sandboxed"));
    }
}
