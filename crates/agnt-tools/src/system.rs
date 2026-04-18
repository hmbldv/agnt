//! System information tools (feature = "system-tools").
//!
//! These tools run read-only system commands and return their output. They have
//! no filesystem sandbox because they do not accept user-supplied paths.
//! All commands are hardcoded; there is no command-injection surface.
//!
//! Gate with `#[cfg(feature = "system-tools")]` at call sites.

use agnt_core::Tool;
use serde_json::{json, Value};
use std::process::Command;

fn run(argv0: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(argv0)
        .args(args)
        .output()
        .map_err(|e| format!("{} not available: {}", argv0, e))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        return Err(format!(
            "exit {}: {}{}",
            out.status.code().unwrap_or(-1),
            stdout,
            stderr
        ));
    }
    Ok(stdout)
}

// ------------------------------------------------------------------------------------------------
// DiskUsage
// ------------------------------------------------------------------------------------------------

/// Report disk usage for all mounted filesystems (`df -h`).
pub struct DiskUsage;

impl Tool for DiskUsage {
    fn name(&self) -> &str { "disk_usage" }
    fn description(&self) -> &str {
        "Return human-readable disk usage for all mounted filesystems (df -h). No arguments."
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn call(&self, _args: Value) -> Result<String, String> {
        run("df", &["-h"])
    }
}

// ------------------------------------------------------------------------------------------------
// SystemInfo
// ------------------------------------------------------------------------------------------------

/// Report OS, hostname, and uptime.
pub struct SystemInfo;

impl Tool for SystemInfo {
    fn name(&self) -> &str { "system_info" }
    fn description(&self) -> &str {
        "Return OS kernel info (uname -a), hostname, and uptime. No arguments."
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn call(&self, _args: Value) -> Result<String, String> {
        let uname = run("uname", &["-a"])?;
        let hostname = run("hostname", &[])?;
        let uptime = run("uptime", &[])?;
        Ok(format!(
            "uname: {}hostname: {}uptime: {}",
            uname, hostname, uptime
        ))
    }
}

// ------------------------------------------------------------------------------------------------
// DockerPs
// ------------------------------------------------------------------------------------------------

/// List running Docker containers as JSON.
pub struct DockerPs;

impl Tool for DockerPs {
    fn name(&self) -> &str { "docker_ps" }
    fn description(&self) -> &str {
        "List running Docker containers in JSON format (docker ps --format json). No arguments."
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn call(&self, _args: Value) -> Result<String, String> {
        run("docker", &["ps", "--format", "json"])
    }
}

// ------------------------------------------------------------------------------------------------
// NvidiaSmi
// ------------------------------------------------------------------------------------------------

/// Query NVIDIA GPU status.
pub struct NvidiaSmi;

impl Tool for NvidiaSmi {
    fn name(&self) -> &str { "nvidia_smi" }
    fn description(&self) -> &str {
        "Return NVIDIA GPU status: name, total/used/free memory (MiB), GPU utilization (%), temperature (°C). No arguments."
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn call(&self, _args: Value) -> Result<String, String> {
        run(
            "nvidia-smi",
            &[
                "--query-gpu=name,memory.total,memory.used,memory.free,utilization.gpu,temperature.gpu",
                "--format=csv,noheader",
            ],
        )
    }
}
