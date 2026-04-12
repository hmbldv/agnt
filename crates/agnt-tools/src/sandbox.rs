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

/// A canonicalized sandbox root. All paths resolved through this instance are
/// guaranteed to live under the root directory on the local filesystem.
#[derive(Debug, Clone)]
pub struct FilesystemRoot {
    root: PathBuf,
}

impl FilesystemRoot {
    /// Construct a sandbox rooted at `root`. The directory must exist and be
    /// canonicalizable (symlinks resolved). Returns `Err` if the root does not
    /// exist or is not accessible.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, String> {
        let root = root.into();
        let canonical = std::fs::canonicalize(&root)
            .map_err(|e| format!("sandbox root {}: {}", root.display(), e))?;
        Ok(Self { root: canonical })
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
    pub fn resolve(&self, input: &str) -> Result<PathBuf, String> {
        if input.is_empty() {
            return Err("empty path".into());
        }
        let raw = Path::new(input);

        // Reject explicit `..` components before touching the filesystem — this
        // avoids relying on canonicalize() alone (which can still be tricked by
        // symlink chains that happen to land back under the root).
        for comp in raw.components() {
            if matches!(comp, Component::ParentDir) {
                return Err(format!("path contains '..': {}", input));
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
            return Ok(canonical);
        }

        // For paths that don't exist yet (e.g. new file in WriteFile),
        // canonicalize the parent and rejoin the filename.
        let parent = joined
            .parent()
            .ok_or_else(|| format!("path has no parent: {}", input))?;
        let file_name = joined
            .file_name()
            .ok_or_else(|| format!("path has no filename: {}", input))?;

        let parent_canonical = std::fs::canonicalize(parent).map_err(|e| {
            format!(
                "sandbox parent {} of {}: {}",
                parent.display(),
                input,
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
}
