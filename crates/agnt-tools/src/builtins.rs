use agnt_core::Tool;
use serde_json::{json, Value};
use std::fs;
use std::process::Command;

pub struct ReadFile;

const READ_FILE_MAX: usize = 256 * 1024;

impl Tool for ReadFile {
    fn name(&self) -> &str { "read_file" }
    fn description(&self) -> &str {
        "Read a UTF-8 text file and return its contents. Truncated at 256KB. Prefer this over 'shell cat' — it is deterministic and cheaper."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "absolute or relative file path" }
            },
            "required": ["path"]
        })
    }
    fn call(&self, args: Value) -> Result<String, String> {
        let path = args["path"].as_str().ok_or("missing path")?;
        let content = fs::read_to_string(path).map_err(|e| format!("read {}: {}", path, e))?;
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

pub struct EditFile;
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
        let path = args["path"].as_str().ok_or("missing path")?;
        let old = args["old"].as_str().ok_or("missing old")?;
        let new_s = args["new"].as_str().ok_or("missing new")?;
        if old.is_empty() {
            return Err("'old' must not be empty".into());
        }
        let content = fs::read_to_string(path).map_err(|e| format!("read {}: {}", path, e))?;
        let count = content.matches(old).count();
        if count == 0 {
            return Err(format!("'old' not found in {}", path));
        }
        if count > 1 {
            return Err(format!(
                "'old' appears {} times in {}; pass more surrounding context to make it unique",
                count, path
            ));
        }
        let updated = content.replacen(old, new_s, 1);
        fs::write(path, &updated).map_err(|e| format!("write {}: {}", path, e))?;
        Ok(format!(
            "edited {} ({} bytes → {} bytes)",
            path,
            content.len(),
            updated.len()
        ))
    }
}

pub struct WriteFile;
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
        fs::write(path, content).map_err(|e| format!("write {}: {}", path, e))?;
        Ok(format!("wrote {} bytes to {}", content.len(), path))
    }
}

pub struct ListDir;
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
        let mut out = String::new();
        for entry in fs::read_dir(path).map_err(|e| format!("read_dir {}: {}", path, e))? {
            let e = entry.map_err(|e| e.to_string())?;
            let ft = e.file_type().map_err(|e| e.to_string())?;
            let tag = if ft.is_dir() { 'D' } else if ft.is_symlink() { 'L' } else { 'F' };
            out.push_str(&format!("{} {}\n", tag, e.file_name().to_string_lossy()));
        }
        Ok(out)
    }
}

pub struct Shell {
    pub unsafe_mode: bool,
}

const SHELL_DENYLIST: &[&str] = &[
    "rm -rf /",
    "rm -rf ~",
    "rm -rf ..",
    "sudo ",
    "dd if=",
    "mkfs",
    ":(){:|:&};:",
    "> /dev/sda",
    "> /dev/nvme",
    "chmod -r 777 /",
    "chown -r",
    "shutdown",
    "reboot",
    "halt",
    "init 0",
    "init 6",
];

fn is_dangerous(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    SHELL_DENYLIST.iter().any(|p| lower.contains(p))
}

impl Tool for Shell {
    fn name(&self) -> &str { "shell" }
    fn description(&self) -> &str {
        "Run a shell command via 'sh -c'. Use as a LAST RESORT when no specialized tool fits — for tasks like 'git status', 'cargo build', or one-off pipelines. For reading files use read_file; for writing use write_file; for edits use edit_file; for searching contents use grep; for finding files use glob; for listing a directory use list_dir; for HTTP use fetch. Dangerous commands (rm -rf /, sudo, dd, mkfs, shutdown) are refused."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "command line to execute" }
            },
            "required": ["cmd"]
        })
    }
    fn call(&self, args: Value) -> Result<String, String> {
        let cmd = args["cmd"].as_str().ok_or("missing cmd")?;
        if !self.unsafe_mode && is_dangerous(cmd) {
            return Err(format!("refused dangerous command: {}", cmd));
        }
        let out = Command::new("sh")
            .arg("-c")
            .arg(cmd)
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

pub struct Glob;
impl Tool for Glob {
    fn name(&self) -> &str { "glob" }
    fn description(&self) -> &str {
        "Find files matching a shell-style glob pattern (e.g. 'src/**/*.rs', '**/Cargo.toml'). Returns one path per line. Prefer this over 'shell find' — it is faster, portable across OSes, and has no command-injection surface."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "glob pattern" }
            },
            "required": ["pattern"]
        })
    }
    fn call(&self, args: Value) -> Result<String, String> {
        let pattern = args["pattern"].as_str().ok_or("missing pattern")?;
        let mut out = String::new();
        let mut count = 0usize;
        for entry in glob::glob(pattern).map_err(|e| format!("glob: {}", e))? {
            if let Ok(p) = entry {
                out.push_str(&p.to_string_lossy());
                out.push('\n');
                count += 1;
                if count >= 2000 {
                    out.push_str("(truncated at 2000)\n");
                    break;
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

pub struct Grep;
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
        let re = regex::Regex::new(pattern).map_err(|e| format!("regex: {}", e))?;
        let mut out = String::new();
        let mut count = 0usize;
        for entry in walkdir::WalkDir::new(path)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() { continue; }
            if let Some(e) = ext {
                if entry.path().extension().and_then(|s| s.to_str()) != Some(e) { continue; }
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

pub struct Fetch;
impl Tool for Fetch {
    fn name(&self) -> &str { "fetch" }
    fn description(&self) -> &str {
        "HTTP GET a URL and return the response body (first 50KB). Use for fetching docs or raw text."
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
        let resp = crate::http::agent()
            .get(url)
            .call()
            .map_err(|e| format!("fetch: {}", e))?;
        let status = resp.status();
        let mut body = String::new();
        resp.into_reader()
            .take(50_000)
            .read_to_string(&mut body)
            .map_err(|e| format!("read: {}", e))?;
        Ok(format!("HTTP {}\n{}", status, body))
    }
}
