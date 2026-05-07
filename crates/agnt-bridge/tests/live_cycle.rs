//! Live bridge cycle test (`#[ignore]`d).
//!
//! Spawns a fresh `agnt-bridge` child process pointing at a temp config that
//! talks to ubu's local vLLM. Publishes one `AgentDispatch` on
//! `agent.dispatch.<name>` and asserts the corresponding `AgentReply`
//! arrives on the inbox subject within the timeout.
//!
//! Why ignored:
//! - Requires NATS reachable at `nats://lnx-rig:4222` with `NATS_USER` /
//!   `NATS_PASSWORD` set.
//! - Requires a working vLLM at `http://localhost:8001/v1` serving
//!   `gemma4-26b` (or whatever model the test config picks).
//! - Builds the bridge binary fresh; expensive on first run.
//!
//! Run:
//! ```bash
//! NATS_USER=… NATS_PASSWORD=… LITELLM_API_KEY=anything \
//!   cargo test -p agnt-bridge --test live_cycle -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::process::{Child, Command};

use voicectl_core::events::{AgentDispatch, AgentReply, RequestId};

const NATS_URL: &str = "nats://lnx-rig:4222";
const TEST_AGENT: &str = "agnt-bridge-livetest";

fn workspace_root() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR must be set when running cargo tests");
    PathBuf::from(&manifest)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn bridge_bin() -> PathBuf {
    let bin = workspace_root().join("target/debug/agnt-bridge");
    assert!(
        bin.exists(),
        "agnt-bridge binary not built — run `cargo build -p agnt-bridge` first"
    );
    bin
}

fn write_config(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(format!("{name}.toml"));
    let content = format!(
        r#"
[agent]
name = "{name}"

[backend]
kind = "openai_compat"
url = "http://localhost:8001/v1"
model = "gemma4-26b"
api_key_env = "LITELLM_API_KEY"

[prompt]
# A minimal in-tree prompt — keeps the test independent of what's installed
# under ~/.config.
system_file = "/dev/null"

[tools]
# No tools — the test asks a pure factual question, no need for retrieval.
enabled = []

[bus]
nats_url = "{NATS_URL}"
user_env = "NATS_USER"
password_env = "NATS_PASSWORD"
subscribe_subject = "agent.dispatch.{name}"
publish_prefix = "agent.event.{name}"
"#
    );
    std::fs::write(&path, content).expect("write temp config");
    path
}

/// Wait for a readyness signal: connect to NATS as a client, subscribe to
/// the dispatch subject, and assert that *we* can subscribe (i.e. NATS is up
/// and accepting). The bridge itself is allowed up to `wait_secs` to also
/// connect — we can't probe its internal state without an explicit health
/// check, so the dispatch retry loop below is the actual readiness check.
async fn wait_for_nats(wait_secs: u64) -> async_nats::Client {
    let user =
        std::env::var("NATS_USER").expect("NATS_USER required for the live bridge cycle test");
    let pass = std::env::var("NATS_PASSWORD")
        .expect("NATS_PASSWORD required for the live bridge cycle test");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs);
    loop {
        let opts = async_nats::ConnectOptions::new().user_and_password(user.clone(), pass.clone());
        match opts.connect(NATS_URL).await {
            Ok(c) => return c,
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    panic!("NATS not reachable after {wait_secs}s: {e}");
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

#[tokio::test]
#[ignore = "spawns agnt-bridge against ubu vLLM + lnx-rig NATS; \
            run with --ignored, NATS_USER + NATS_PASSWORD + LITELLM_API_KEY"]
async fn live_bridge_round_trip() {
    // Pre-flight: test creds.
    assert!(std::env::var("NATS_USER").is_ok(), "NATS_USER required");
    assert!(
        std::env::var("NATS_PASSWORD").is_ok(),
        "NATS_PASSWORD required"
    );
    assert!(
        std::env::var("LITELLM_API_KEY").is_ok(),
        "LITELLM_API_KEY required"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg_path = write_config(&tmp, TEST_AGENT);
    let bin = bridge_bin();

    let child = Command::new(&bin)
        .arg("--config")
        .arg(&cfg_path)
        .env("RUST_LOG", "info,agnt_bridge=debug")
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("spawn agnt-bridge: {e}"));
    let _guard = ChildGuard(child);

    // Connect a separate client for the test driver.
    let nats = wait_for_nats(5).await;

    let request_id = RequestId::new();
    let reply_to = format!("agent.response.{TEST_AGENT}.{request_id}");
    let dispatch_subject = format!("agent.dispatch.{TEST_AGENT}");

    // Subscribe FIRST.
    let mut sub = nats
        .subscribe(reply_to.clone())
        .await
        .expect("subscribe reply_to");

    // Give the bridge a beat to subscribe to its own dispatch subject before
    // we publish (NATS doesn't queue messages for non-subscribed subjects;
    // 1.5s is generous in practice).
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    let dispatch = AgentDispatch {
        request_id: request_id.clone(),
        user_input: "say the single word ready".into(),
        context: Some(serde_json::json!({ "from": "live_cycle_test" })),
        reply_to: reply_to.clone(),
    };
    let payload = serde_json::to_vec(&dispatch).unwrap();
    nats.publish(dispatch_subject, payload.into())
        .await
        .expect("publish dispatch");

    let msg = tokio::time::timeout(Duration::from_secs(15), sub.next())
        .await
        .expect("agent reply did not arrive within 15s")
        .expect("subscription closed");

    let reply: AgentReply = serde_json::from_slice(&msg.payload).expect("decode reply");
    eprintln!("[live_cycle] reply: {reply:?}");
    assert_eq!(reply.request_id, request_id, "request_id mismatch");
    assert!(reply.ok, "reply.ok was false (error = {:?})", reply.error);
    assert!(!reply.text.is_empty(), "reply text was empty");
    assert!(
        reply.duration_ms > 0,
        "duration_ms not populated by the bridge"
    );
}
