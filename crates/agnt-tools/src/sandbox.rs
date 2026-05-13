//! Filesystem root sandbox for agnt-tools.
//!
//! Resolves user-supplied paths against a fixed root directory, rejecting any
//! path that escapes the root via `..`, absolute-path rewrites, or symlink
//! traversal. This is the primary boundary between a hostile LLM's tool calls
//! and the host filesystem.
//!
//! ## Threat model
//!
//! - Adversary: LLM output that issues `read_file {"path":"/etc/shadow"}`-class
//!   tool calls.
//! - Defense: every filesystem-touching tool holds an `Arc<FilesystemRoot>`
//!   (when sandboxed) and routes input paths through [`FilesystemRoot::resolve`]
//!   before touching `std::fs`.
//! - Non-goal: defending against OS-level privilege escalation; this is a
//!   path-safety layer, not a chroot.
//!
//! Constructing a [`FilesystemRoot`] canonicalizes the root and stores it; all
//! subsequent resolutions are compared against that canonical prefix.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// A canonicalized sandbox root. All paths resolved through this instance are
/// guaranteed to live under the root directory on the local filesystem.
#[derive(Debug, Clone)]
pub struct FilesystemRoot {
    root: PathBuf,
    /// Paths (canonicalized at construction time) that are explicitly denied,
    /// regardless of whether they fall under the sandbox root. Checked after
    /// the root-containment guard so operators can lock out sensitive sub-trees
    /// (e.g. `.ssh/`, `.gnupg/`) even when those trees are inside the root.
    denylist: Vec<PathBuf>,
}

impl FilesystemRoot {
    /// Construct a sandbox rooted at `root`. The directory must exist and be
    /// canonicalizable (symlinks resolved). Returns `Err` if the root does not
    /// exist or is not accessible.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, String> {
        let root = root.into();
        let canonical = std::fs::canonicalize(&root)
            .map_err(|e| format!("sandbox root {}: {}", root.display(), e))?;
        Ok(Self { root: canonical, denylist: Vec::new() })
    }

    /// Reject any resolved path that falls under one of the given entries,
    /// regardless of where the sandbox root sits. Entries are canonicalized at
    /// construction time; non-existent entries use the raw path.
    pub fn with_denylist(mut self, entries: Vec<impl AsRef<Path>>) -> Result<Self, String> {
        for entry in entries {
            let p = entry.as_ref();
            let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            self.denylist.push(canonical);
        }
        Ok(self)
    }

    /// Return the canonical root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a user-supplied path against the sandbox root.
    ///
    /// Returns the absolute, canonical path if it is (or — for not-yet-existing
    /// paths — would be) under the root. Rejects:
    ///
    /// - any path whose components contain `..`
    /// - any path that canonicalizes outside the root (symlink escape)
    /// - for new files: any parent path that escapes the root
    /// - any path that falls under a denylist entry (see [`with_denylist`])
    ///
    /// [`with_denylist`]: FilesystemRoot::with_denylist
    pub fn resolve(&self, input: impl AsRef<Path>) -> Result<PathBuf, String> {
        let input = input.as_ref();
        if input.as_os_str().is_empty() {
            return Err("empty path".into());
        }
        let raw = input;

        // Reject explicit `..` components before touching the filesystem — this
        // avoids relying on canonicalize() alone (which can still be tricked by
        // symlink chains that happen to land back under the root).
        for comp in raw.components() {
            if matches!(comp, Component::ParentDir) {
                return Err(format!("path contains '..': {}", input.display()));
            }
        }

        // Join relative paths onto the root so `"foo.txt"` resolves to
        // `<root>/foo.txt`. Absolute paths are taken as-is but will still be
        // checked for containment below.
        let joined: PathBuf = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.root.join(raw)
        };

        // For existing paths, canonicalize directly.
        if let Ok(canonical) = std::fs::canonicalize(&joined) {
            if !canonical.starts_with(&self.root) {
                return Err(format!(
                    "path escapes sandbox root: {} not under {}",
                    canonical.display(),
                    self.root.display()
                ));
            }
            // Check denylist after confirming the path is under the root so
            // operators can lock out sensitive sub-trees (e.g. .ssh/) without
            // touching the root-containment logic. The "denied:" prefix lets
            // log consumers distinguish this error from escape-sandbox errors.
            for denied in &self.denylist {
                if canonical.starts_with(denied) {
                    return Err(format!(
                        "denied: path is under denylist entry {}",
                        denied.display()
                    ));
                }
            }
            return Ok(canonical);
        }

        // For paths that don't exist yet (e.g. new file in WriteFile),
        // canonicalize the parent and rejoin the filename.
        let parent = joined
            .parent()
            .ok_or_else(|| format!("path has no parent: {}", input.display()))?;
        let file_name = joined
            .file_name()
            .ok_or_else(|| format!("path has no filename: {}", input.display()))?;

        let parent_canonical = std::fs::canonicalize(parent).map_err(|e| {
            format!(
                "sandbox parent {} of {}: {}",
                parent.display(),
                input.display(),
                e
            )
        })?;

        if !parent_canonical.starts_with(&self.root) {
            return Err(format!(
                "path escapes sandbox root: {} not under {}",
                parent_canonical.display(),
                self.root.display()
            ));
        }

        Ok(parent_canonical.join(file_name))
    }
}

/// Wraps an optional [`FilesystemRoot`] sandbox. Each filesystem tool holds one;
/// methods centralize the `None` → unrestricted / `Some` → sandboxed dispatch
/// that was previously duplicated across every tool.
pub struct SandboxedPath(pub(crate) Option<Arc<FilesystemRoot>>);

impl SandboxedPath {
    /// No sandbox — tool has unrestricted path access.
    pub fn new() -> Self { Self(None) }

    /// Sandbox-restricted — all paths must resolve under `root`.
    pub fn with_root(root: Arc<FilesystemRoot>) -> Self { Self(Some(root)) }

    /// Resolve `input` against the sandbox, or pass it through when unsandboxed.
    pub fn resolve(&self, input: &str) -> Result<PathBuf, String> {
        match &self.0 {
            Some(s) => s.resolve(input),
            None => Ok(PathBuf::from(input)),
        }
    }

    /// Return the sandbox root path, or `None` when unsandboxed.
    pub fn root(&self) -> Option<&Path> {
        self.0.as_ref().map(|r| r.root())
    }
}

impl Default for SandboxedPath {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agnt-sandbox-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn resolves_relative_under_root() {
        let dir = tmpdir();
        let sandbox = FilesystemRoot::new(&dir).unwrap();
        fs::write(dir.join("a.txt"), "x").unwrap();
        let resolved = sandbox.resolve("a.txt").unwrap();
        assert!(resolved.starts_with(sandbox.root()));
    }

    #[test]
    fn rejects_parent_escape() {
        let dir = tmpdir();
        let sandbox = FilesystemRoot::new(&dir).unwrap();
        let err = sandbox.resolve("../etc/shadow").unwrap_err();
        assert!(err.contains(".."), "expected .. rejection, got {}", err);
    }

    #[test]
    fn rejects_absolute_outside_root() {
        let dir = tmpdir();
        let sandbox = FilesystemRoot::new(&dir).unwrap();
        let err = sandbox.resolve("/etc/passwd").unwrap_err();
        assert!(err.contains("sandbox") || err.contains("escape"));
    }

    #[test]
    fn allows_new_file_under_root() {
        let dir = tmpdir();
        let sandbox = FilesystemRoot::new(&dir).unwrap();
        let resolved = sandbox.resolve("new.txt").unwrap();
        assert!(resolved.starts_with(sandbox.root()));
    }

    #[test]
    fn rejects_symlink_escape() {
        #[cfg(unix)]
        {
            let dir = tmpdir();
            let outside = tmpdir();
            fs::write(outside.join("secret.txt"), "pw").unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
            let sandbox = FilesystemRoot::new(&dir).unwrap();
            let err = sandbox.resolve("link/secret.txt").unwrap_err();
            assert!(err.contains("escape") || err.contains("sandbox"));
        }
    }

    // ---- Denylist tests -------------------------------------------------------------------

    #[test]
    fn denylist_rejects_path_under_denied_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let deny = tmp.path().join(".ssh");
        std::fs::create_dir_all(&deny).unwrap();
        std::fs::write(deny.join("id_rsa"), b"fake").unwrap();
        let root = FilesystemRoot::new(tmp.path())
            .unwrap()
            .with_denylist(vec![deny.clone()])
            .unwrap();
        let err = root.resolve(deny.join("id_rsa")).unwrap_err();
        assert!(err.contains("denied"), "got: {}", err);
    }

    #[test]
    fn denylist_allows_sibling_of_denied_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let deny = tmp.path().join(".ssh");
        let ok = tmp.path().join("notes");
        std::fs::create_dir_all(&deny).unwrap();
        std::fs::create_dir_all(&ok).unwrap();
        std::fs::write(ok.join("hello.md"), b"hi").unwrap();
        let root = FilesystemRoot::new(tmp.path())
            .unwrap()
            .with_denylist(vec![deny])
            .unwrap();
        assert!(root.resolve(ok.join("hello.md")).is_ok());
    }

    #[test]
    fn denylist_with_symlink_entering_denied_area_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let deny = tmp.path().join(".secrets");
        std::fs::create_dir_all(&deny).unwrap();
        std::fs::write(deny.join("key"), b"k").unwrap();
        let link = tmp.path().join("shortcut");
        std::os::unix::fs::symlink(&deny, &link).unwrap();
        let root = FilesystemRoot::new(tmp.path())
            .unwrap()
            .with_denylist(vec![deny])
            .unwrap();
        let err = root.resolve(link.join("key")).unwrap_err();
        assert!(err.contains("denied"), "got: {}", err);
    }
}
