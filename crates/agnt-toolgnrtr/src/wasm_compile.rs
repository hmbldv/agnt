//! Compile generated Rust source to `wasm32-wasip1` via cargo.
//!
//! We keep a persistent Cargo project directory (default
//! `~/.cache/agnt-toolgnrtr/build/`) whose `Cargo.toml` pins serde +
//! serde_json. Each tool becomes a binary target under `src/bin/<name>.rs`.
//! Cargo's incremental build + crate cache means the first compile is slow
//! (~30s cold) but subsequent generations are seconds.
//!
//! The compile step is fail-loud: stderr from rustc is returned verbatim so
//! callers (and the `evolve` path) can show the model what went wrong.

use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, info};

/// Workspace template — minimal `Cargo.toml` that declares the deps the
/// generated tools are allowed to use. Kept tiny on purpose: every extra
/// dep is wasm-compiled the first time and added to the bin's footprint.
const CARGO_TOML: &str = r#"[package]
name = "toolgnrtr-workspace"
version = "0.0.0"
edition = "2021"
publish = false

[[bin]]
name = "_placeholder"
path = "src/bin/_placeholder.rs"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[profile.release]
opt-level = "s"
lto = false
codegen-units = 1
strip = true
"#;

const PLACEHOLDER_BIN: &str = "fn main() {}\n";

pub struct WasmCompiler {
    workspace: PathBuf,
}

impl WasmCompiler {
    /// Create a compiler rooted at `workspace`. The directory is created and
    /// seeded with a `Cargo.toml` + placeholder bin on first use.
    pub fn new(workspace: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(&workspace)
            .map_err(|e| format!("create workspace {}: {e}", workspace.display()))?;
        std::fs::create_dir_all(workspace.join("src").join("bin"))
            .map_err(|e| format!("create src/bin: {e}"))?;

        let cargo_toml = workspace.join("Cargo.toml");
        if !cargo_toml.exists() {
            std::fs::write(&cargo_toml, CARGO_TOML)
                .map_err(|e| format!("write Cargo.toml: {e}"))?;
        }
        let placeholder = workspace.join("src").join("bin").join("_placeholder.rs");
        if !placeholder.exists() {
            std::fs::write(&placeholder, PLACEHOLDER_BIN)
                .map_err(|e| format!("write placeholder: {e}"))?;
        }
        Ok(Self { workspace })
    }

    /// Default workspace at `$XDG_CACHE_HOME/agnt-toolgnrtr/build` or
    /// `~/.cache/agnt-toolgnrtr/build`.
    pub fn default_workspace() -> Result<PathBuf, String> {
        if let Ok(cache) = std::env::var("XDG_CACHE_HOME") {
            return Ok(PathBuf::from(cache).join("agnt-toolgnrtr").join("build"));
        }
        if let Ok(home) = std::env::var("HOME") {
            return Ok(PathBuf::from(home)
                .join(".cache")
                .join("agnt-toolgnrtr")
                .join("build"));
        }
        Err("no $HOME or $XDG_CACHE_HOME set".into())
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Compile `source` as the bin named `name` and return the resulting
    /// `wasm32-wasip1` bytes. The bin file is left on disk under
    /// `src/bin/<name>.rs` so cargo can incrementally rebuild it.
    pub fn compile(&self, name: &str, source: &str) -> Result<Vec<u8>, String> {
        validate_bin_name(name)?;

        let bin_path = self.workspace.join("src").join("bin").join(format!("{name}.rs"));
        std::fs::write(&bin_path, source).map_err(|e| format!("write bin: {e}"))?;

        info!(name = %name, "compiling tool to wasm32-wasip1");
        let started = std::time::Instant::now();
        let output = Command::new("cargo")
            .arg("build")
            .arg("--release")
            .arg("--target")
            .arg("wasm32-wasip1")
            .arg("--bin")
            .arg(name)
            // Quieter output. Errors still land on stderr.
            .arg("--quiet")
            .current_dir(&self.workspace)
            .output()
            .map_err(|e| format!("invoke cargo: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Remove the source so a failed compile doesn't pollute the
            // workspace and bias the next `cargo build` toward stale errors.
            let _ = std::fs::remove_file(&bin_path);
            return Err(format!("cargo build failed:\n{stderr}"));
        }

        let artifact = self
            .workspace
            .join("target")
            .join("wasm32-wasip1")
            .join("release")
            .join(format!("{name}.wasm"));
        let bytes = std::fs::read(&artifact)
            .map_err(|e| format!("read artifact {}: {e}", artifact.display()))?;
        debug!(
            name = %name,
            bytes = bytes.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "tool compiled"
        );
        Ok(bytes)
    }
}

/// Bin names map to filenames and rustc identifiers, so be strict.
fn validate_bin_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("bin name is empty".into());
    }
    if name.starts_with('_') {
        return Err("bin name must not start with underscore (reserved)".into());
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() {
        return Err(format!("bin name must start with lowercase letter: {name}"));
    }
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return Err(format!("bin name has invalid char {c:?}: {name}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation_rejects_garbage() {
        assert!(validate_bin_name("").is_err());
        assert!(validate_bin_name("_placeholder").is_err());
        assert!(validate_bin_name("Foo").is_err());
        assert!(validate_bin_name("foo-bar").is_err());
        assert!(validate_bin_name("foo_bar9").is_ok());
    }

    #[test]
    fn new_seeds_workspace_files() {
        let dir = std::env::temp_dir().join(format!("toolgnrtr-ws-{}", uuid::Uuid::new_v4()));
        let _ = WasmCompiler::new(dir.clone()).unwrap();
        assert!(dir.join("Cargo.toml").exists());
        assert!(dir.join("src").join("bin").join("_placeholder.rs").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
