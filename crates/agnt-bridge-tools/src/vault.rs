//! Vault-specific read tools for the Sage agent.
//!
//! These tools operate within the vault_root sandbox set in sage.toml.
//! They walk the Obsidian Markdown vault and surface files by recency or by
//! frontmatter field, saving the agent from needing to glob + stat + parse
//! YAML itself.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};

// ─────────────────────────────────────────────────────────────────────────────
// Shared helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Walk `root` recursively, collecting all `.md` files with their mtime.
fn collect_md_files(root: &Path) -> Vec<(PathBuf, SystemTime)> {
    let mut out = Vec::new();
    walk_dir(root, root, &mut out);
    out
}

fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, SystemTime)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Skip hidden files and directories (e.g. .obsidian, .trash)
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with('.'))
            .unwrap_or(false)
        {
            continue;
        }
        if path.is_dir() {
            walk_dir(root, &path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((path, mtime));
        }
    }
}

/// Parse YAML frontmatter from a Markdown file. Returns a map of key → value
/// (both as strings). Returns an empty map if no frontmatter is found or
/// parsing fails.
fn parse_frontmatter(path: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return map,
    };
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return map;
    }
    // Find closing ---
    let after_open = match trimmed.find('\n') {
        Some(i) => &trimmed[i + 1..],
        None => return map,
    };
    let end = match after_open.find("\n---") {
        Some(i) => i,
        None => return map,
    };
    let yaml_block = &after_open[..end];
    for line in yaml_block.lines() {
        if let Some((key, val)) = line.split_once(':') {
            let k = key.trim().to_string();
            let v = val.trim().trim_matches('"').trim_matches('\'').to_string();
            if !k.is_empty() {
                map.insert(k, v);
            }
        }
    }
    map
}

/// Return `path` relative to `root`, or the full path as a string if
/// stripping fails.
fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// Format a SystemTime as a human-readable ISO-8601 date.
fn fmt_mtime(t: SystemTime) -> String {
    use std::time::UNIX_EPOCH;
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    // Manual ISO-8601 from epoch — avoids pulling in chrono here.
    // Compute date using proleptic Gregorian calendar algorithm.
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;
    // Days-to-date (civil calendar algorithm, Linden Windels / Howard Hinnant variant)
    let z = days_since_epoch as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

// ─────────────────────────────────────────────────────────────────────────────
// vault_recent
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the N most recently modified Markdown files in the vault, with
/// their frontmatter title, type, project, and modification timestamp.
pub struct VaultRecent {
    vault_root: PathBuf,
}

impl VaultRecent {
    pub fn new(vault_root: PathBuf) -> Self {
        Self { vault_root }
    }
}

impl agnt::Tool for VaultRecent {
    fn name(&self) -> &str {
        "vault_recent"
    }

    fn description(&self) -> &str {
        "Return the N most recently modified files in the Obsidian vault. \
         Each entry includes relative path, last-modified timestamp, and key \
         frontmatter fields (title, type, project, status). Use this to \
         answer 'what did I last work on?' or 'what was created recently?'"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "n": {
                    "type": "integer",
                    "description": "Number of files to return (default 10, max 50).",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": []
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let n = args
            .get("n")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(10)
            .clamp(1, 50);

        let root = &self.vault_root;
        if !root.is_dir() {
            return Err(format!("vault_root is not a directory: {}", root.display()));
        }

        let mut files = collect_md_files(root);
        files.sort_by(|a, b| b.1.cmp(&a.1));
        files.truncate(n);

        if files.is_empty() {
            return Ok("(no Markdown files found in vault)".into());
        }

        let mut lines = Vec::with_capacity(files.len());
        for (path, mtime) in &files {
            let fm = parse_frontmatter(path);
            let rel_path = rel(root, path);
            let ts = fmt_mtime(*mtime);
            let title = fm.get("title").map(String::as_str).unwrap_or("—");
            let kind = fm.get("type").map(String::as_str).unwrap_or("—");
            let project = fm.get("project").map(String::as_str).unwrap_or("—");
            let status = fm.get("status").map(String::as_str).unwrap_or("—");
            lines.push(format!(
                "{ts}  {rel_path}\n  title={title}  type={kind}  project={project}  status={status}"
            ));
        }
        Ok(lines.join("\n"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// vault_find
// ─────────────────────────────────────────────────────────────────────────────

/// Filter vault files by frontmatter fields.
pub struct VaultFind {
    vault_root: PathBuf,
}

impl VaultFind {
    pub fn new(vault_root: PathBuf) -> Self {
        Self { vault_root }
    }
}

impl agnt::Tool for VaultFind {
    fn name(&self) -> &str {
        "vault_find"
    }

    fn description(&self) -> &str {
        "Search vault files by frontmatter field values. All provided filters \
         are AND-ed. Returns matching files with their relative path and \
         frontmatter fields. Use for queries like 'find all active projects' \
         or 'what files are tagged meeting and belong to SOLA?'"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Match files with this frontmatter `project` value (substring, case-insensitive)."
                },
                "tags": {
                    "type": "string",
                    "description": "Comma-separated tags to require (frontmatter `tags` field, substring match each)."
                },
                "type": {
                    "type": "string",
                    "description": "Match files with this frontmatter `type` value (substring, case-insensitive)."
                },
                "status": {
                    "type": "string",
                    "description": "Match files with this frontmatter `status` value (exact, case-insensitive)."
                },
                "title_contains": {
                    "type": "string",
                    "description": "Substring to match against frontmatter `title` (case-insensitive)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default 20, max 100).",
                    "minimum": 1,
                    "maximum": 100
                }
            },
            "required": []
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        if args.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            return Err(
                "at least one filter is required (project, tags, type, status, or title_contains)"
                    .into(),
            );
        }

        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(20)
            .clamp(1, 100);

        let filter_project = args
            .get("project")
            .and_then(|v| v.as_str())
            .map(|s| s.to_lowercase());
        let filter_type = args
            .get("type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_lowercase());
        let filter_status = args
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.to_lowercase());
        let filter_title = args
            .get("title_contains")
            .and_then(|v| v.as_str())
            .map(|s| s.to_lowercase());
        let filter_tags: Vec<String> = args
            .get("tags")
            .and_then(|v| v.as_str())
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_lowercase())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let root = &self.vault_root;
        if !root.is_dir() {
            return Err(format!("vault_root is not a directory: {}", root.display()));
        }

        let mut files = collect_md_files(root);
        // Sort newest-first so the most relevant hits tend to appear first.
        files.sort_by(|a, b| b.1.cmp(&a.1));

        let mut results = Vec::new();
        for (path, mtime) in &files {
            if results.len() >= limit {
                break;
            }
            let fm = parse_frontmatter(path);

            if let Some(ref p) = filter_project {
                let v = fm.get("project").map(|s| s.to_lowercase()).unwrap_or_default();
                if !v.contains(p.as_str()) {
                    continue;
                }
            }
            if let Some(ref t) = filter_type {
                let v = fm.get("type").map(|s| s.to_lowercase()).unwrap_or_default();
                if !v.contains(t.as_str()) {
                    continue;
                }
            }
            if let Some(ref s) = filter_status {
                let v = fm.get("status").map(|s| s.to_lowercase()).unwrap_or_default();
                if v != s.as_str() {
                    continue;
                }
            }
            if let Some(ref ti) = filter_title {
                let v = fm.get("title").map(|s| s.to_lowercase()).unwrap_or_default();
                if !v.contains(ti.as_str()) {
                    continue;
                }
            }
            if !filter_tags.is_empty() {
                let raw_tags = fm.get("tags").map(String::as_str).unwrap_or("").to_lowercase();
                if !filter_tags.iter().all(|tag| raw_tags.contains(tag.as_str())) {
                    continue;
                }
            }

            let rel_path = rel(root, path);
            let ts = fmt_mtime(*mtime);
            let title = fm.get("title").map(String::as_str).unwrap_or("—");
            let kind = fm.get("type").map(String::as_str).unwrap_or("—");
            let project = fm.get("project").map(String::as_str).unwrap_or("—");
            let status = fm.get("status").map(String::as_str).unwrap_or("—");
            results.push(format!(
                "{ts}  {rel_path}\n  title={title}  type={kind}  project={project}  status={status}"
            ));
        }

        if results.is_empty() {
            Ok("(no files matched the given filters)".into())
        } else {
            Ok(results.join("\n"))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use agnt::Tool;
    use std::fs;
    use tempfile::TempDir;

    fn make_md(dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).unwrap();
    }

    fn vault_with_files() -> TempDir {
        let tmp = TempDir::new().unwrap();
        make_md(
            tmp.path(),
            "note1.md",
            "---\ntitle: Alpha Note\ntype: note\nproject: SOLA\nstatus: active\n---\nBody.",
        );
        make_md(
            tmp.path(),
            "note2.md",
            "---\ntitle: Beta Meeting\ntype: meeting\nproject: PLYGLT\nstatus: done\ntags: meeting, standup\n---\nBody.",
        );
        make_md(
            tmp.path(),
            "note3.md",
            "No frontmatter here.",
        );
        tmp
    }

    #[test]
    fn vault_recent_returns_files() {
        let tmp = vault_with_files();
        let tool = VaultRecent::new(tmp.path().to_path_buf());
        let out = tool.call(json!({"n": 10})).unwrap();
        assert!(out.contains("note1.md") || out.contains("note2.md") || out.contains("note3.md"));
    }

    #[test]
    fn vault_recent_defaults_to_10() {
        let tmp = vault_with_files();
        let tool = VaultRecent::new(tmp.path().to_path_buf());
        let out = tool.call(json!({})).unwrap();
        assert!(!out.is_empty());
    }

    #[test]
    fn vault_recent_missing_root_is_error() {
        let tool = VaultRecent::new(PathBuf::from("/nonexistent/vault"));
        assert!(tool.call(json!({})).is_err());
    }

    #[test]
    fn vault_find_by_project() {
        let tmp = vault_with_files();
        let tool = VaultFind::new(tmp.path().to_path_buf());
        let out = tool.call(json!({"project": "sola"})).unwrap();
        assert!(out.contains("Alpha Note"), "{out}");
        assert!(!out.contains("PLYGLT"), "{out}");
    }

    #[test]
    fn vault_find_by_type() {
        let tmp = vault_with_files();
        let tool = VaultFind::new(tmp.path().to_path_buf());
        let out = tool.call(json!({"type": "meeting"})).unwrap();
        assert!(out.contains("Beta Meeting"), "{out}");
    }

    #[test]
    fn vault_find_by_status_exact() {
        let tmp = vault_with_files();
        let tool = VaultFind::new(tmp.path().to_path_buf());
        let out = tool.call(json!({"status": "active"})).unwrap();
        assert!(out.contains("Alpha Note"), "{out}");
        assert!(!out.contains("Beta Meeting"), "{out}");
    }

    #[test]
    fn vault_find_no_filters_is_error() {
        let tmp = vault_with_files();
        let tool = VaultFind::new(tmp.path().to_path_buf());
        assert!(tool.call(json!({})).is_err());
    }

    #[test]
    fn vault_find_no_match_returns_message() {
        let tmp = vault_with_files();
        let tool = VaultFind::new(tmp.path().to_path_buf());
        let out = tool.call(json!({"project": "zzz-nonexistent"})).unwrap();
        assert!(out.contains("no files matched"), "{out}");
    }

    #[test]
    fn parse_frontmatter_missing_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bare.md");
        fs::write(&p, "Just content, no frontmatter.").unwrap();
        let fm = parse_frontmatter(&p);
        assert!(fm.is_empty());
    }

    #[test]
    fn fmt_mtime_epoch_is_valid() {
        let s = fmt_mtime(SystemTime::UNIX_EPOCH);
        assert_eq!(s, "1970-01-01T00:00:00Z");
    }
}
