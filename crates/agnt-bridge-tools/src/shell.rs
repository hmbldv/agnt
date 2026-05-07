//! Subprocess helpers.
//!
//! All system tools shell out via [`tokio::process::Command`] with **structured
//! arguments only** — never a shell string — so untrusted LLM output can never
//! lead to command injection.
//!
//! Tools live in a sync `Tool::call`, so we block on the current tokio runtime.
//! This relies on the bridge invoking tools from inside `spawn_blocking`,
//! which is how `agnt-bridge::dispatch::AgntRunner` is wired.

use std::ffi::OsStr;
use std::time::Duration;

use tokio::process::Command;
use tokio::runtime::Handle;

/// Default per-tool subprocess timeout. Long enough for `gnome-screenshot`,
/// short enough that a stuck `xdotool` doesn't wedge the agent loop.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Output of a structured subprocess invocation.
pub struct CmdOutput {
    pub status_ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run `program` with `args`, capturing stdout + stderr. Blocks the current
/// thread on the surrounding tokio runtime; do not call from async context.
pub fn run_blocking<I, S>(program: &str, args: I, timeout: Duration) -> Result<CmdOutput, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.kill_on_drop(true);
    let fut = async move {
        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(out)) => Ok(CmdOutput {
                status_ok: out.status.success(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
            Ok(Err(e)) => Err(format!("spawn {program}: {e}")),
            Err(_) => Err(format!("{program} timed out after {:?}", timeout)),
        }
    };
    block_on(fut)
}

/// Block the current thread on `fut`, using whichever tokio runtime is
/// available. Falls back to spawning a temporary current-thread runtime if
/// no handle is present (handy for unit tests).
pub fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    match Handle::try_current() {
        Ok(handle) => {
            // We're inside a multi-threaded runtime (the bridge uses
            // `#[tokio::main]` which is multi-thread by default). The tool
            // is already on a `spawn_blocking` worker, so a `block_on` on
            // the runtime handle is the documented pattern.
            tokio::task::block_in_place(|| handle.block_on(fut))
        }
        Err(_) => {
            // Fallback used by unit tests that don't set up a runtime.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build fallback runtime");
            rt.block_on(fut)
        }
    }
}
