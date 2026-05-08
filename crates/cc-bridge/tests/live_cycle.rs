//! Live cc-bridge cycle test (`#[ignore]`d).
//!
//! Spawns a fresh `cc-bridge` child process pointing at a temp config that
//! invokes `claude --print` locally on the test runner host. Publishes
//! one `AgentDispatch` on `cc.dispatch.<persona>` and asserts the
//! corresponding `AgentReply` arrives on the inbox subject within 60s.
//!
//! Why ignored:
//! - Requires NATS reachable at `nats://localhost:4222` with `NATS_USER` /
//!   `NATS_PASSWORD` set.
//! - Requires a working `claude` binary on the local machine and a valid
//!   Anthropic credential cached for it.
//! - Spends real money — typically ~$0.01–$0.05/run for the tiny prompt.
//!
//! Run:
//! ```bash
//! NATS_USER=… NATS_PASSWORD=… \
//!   cargo build -p cc-bridge && \
//!   cargo test -p cc-bridge --test live_cycle -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::process::{Child, Command};

use voicectl_core::events::{AgentDispatch, AgentReply, RequestId};

const NATS_URL: &str = "nats://localhost:4222";
const TEST_PERSONA: &str = "cclivetest";
const TEST_BRIDGE: &str = "cc-bridge-livetest";

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
    let bin = workspace_root().join("target/debug/cc-bridge");
    assert!(
        bin.exists(),
        "cc-bridge binary not built — run `cargo build -p cc-bridge` first"
    );
    bin
}

fn write_config(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join(format!("{TEST_BRIDGE}.toml"));
    let content = format!(
        r#"
[bridge]
name = "{TEST_BRIDGE}"
subject_root = "cc"

[bus]
nats_url = "{NATS_URL}"
user_env = "NATS_USER"
password_env = "NATS_PASSWORD"

[[personas]]
name = "{TEST_PERSONA}"
host = "localhost"
cwd = "/tmp"
permission_mode = "bypassPermissions"
timeout_sec = 60
"#
    );
    std::fs::write(&path, content).expect("write temp config");
    path
}

async fn wait_for_nats(wait_secs: u64) -> async_nats::Client {
    let user =
        std::env::var("NATS_USER").expect("NATS_USER required for the live cc-bridge cycle test");
    let pass = std::env::var("NATS_PASSWORD")
        .expect("NATS_PASSWORD required for the live cc-bridge cycle test");
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
#[ignore = "spawns cc-bridge against a local claude + localhost NATS; \
            run with --ignored, NATS_USER + NATS_PASSWORD + a logged-in \
            claude install"]
async fn live_cc_bridge_round_trip() {
    assert!(std::env::var("NATS_USER").is_ok(), "NATS_USER required");
    assert!(
        std::env::var("NATS_PASSWORD").is_ok(),
        "NATS_PASSWORD required"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg_path = write_config(&tmp);
    let bin = bridge_bin();

    let child = Command::new(&bin)
        .arg("--config")
        .arg(&cfg_path)
        .env("RUST_LOG", "info,cc_bridge=debug")
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("spawn cc-bridge: {e}"));
    let _guard = ChildGuard(child);

    let nats = wait_for_nats(5).await;

    let request_id = RequestId::new();
    let reply_to = format!("cc.response.{TEST_PERSONA}.{request_id}");
    let dispatch_subject = format!("cc.dispatch.{TEST_PERSONA}");

    let mut sub = nats
        .subscribe(reply_to.clone())
        .await
        .expect("subscribe reply_to");

    // Give the bridge a beat to subscribe before we publish.
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    let dispatch = AgentDispatch {
        request_id: request_id.clone(),
        user_input: "Reply with exactly: OK. Nothing else.".into(),
        context: Some(serde_json::json!({ "from": "live_cycle_test" })),
        reply_to: reply_to.clone(),
    };
    let payload = serde_json::to_vec(&dispatch).unwrap();
    nats.publish(dispatch_subject, payload.into())
        .await
        .expect("publish dispatch");

    // Claude calls average ~5-10s on cache-warm; allow a generous 60s.
    let msg = tokio::time::timeout(Duration::from_secs(60), sub.next())
        .await
        .expect("agent reply did not arrive within 60s")
        .expect("subscription closed");

    let reply: AgentReply = serde_json::from_slice(&msg.payload).expect("decode reply");
    eprintln!("[live_cycle] reply: {reply:?}");
    assert_eq!(reply.request_id, request_id, "request_id mismatch");
    assert!(reply.ok, "reply.ok was false (error = {:?})", reply.error);
    assert!(!reply.text.is_empty(), "reply text was empty");
    assert!(
        reply.text.to_uppercase().contains("OK"),
        "expected reply text to contain OK, got: {}",
        reply.text
    );
    assert!(
        reply.duration_ms > 0,
        "duration_ms not populated by the bridge"
    );
}

#[tokio::test]
#[ignore = "spawns cc-bridge with daily_cost_limit_usd=0.001 to assert \
            quota enforcement; same env requirements as round_trip"]
async fn live_cc_bridge_quota_rejects_after_first_call() {
    assert!(std::env::var("NATS_USER").is_ok(), "NATS_USER required");
    assert!(
        std::env::var("NATS_PASSWORD").is_ok(),
        "NATS_PASSWORD required"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg_path = tmp.path().join("quota_test.toml");
    let cfg_content = format!(
        r#"
[bridge]
name = "cc-bridge-quotatest"
subject_root = "cc"

[bus]
nats_url = "{NATS_URL}"

[[personas]]
name = "ccquotatest"
host = "localhost"
cwd = "/tmp"
permission_mode = "bypassPermissions"
timeout_sec = 60
daily_cost_limit_usd = 0.001
"#
    );
    std::fs::write(&cfg_path, cfg_content).unwrap();

    let bin = bridge_bin();
    let child = Command::new(&bin)
        .arg("--config")
        .arg(&cfg_path)
        .env("RUST_LOG", "info,cc_bridge=debug")
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("spawn cc-bridge: {e}"));
    let _guard = ChildGuard(child);

    let nats = wait_for_nats(5).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    // First call burns the budget.
    let rid1 = RequestId::new();
    let reply_to1 = format!("cc.response.ccquotatest.{rid1}");
    let mut sub1 = nats.subscribe(reply_to1.clone()).await.unwrap();
    let d1 = AgentDispatch {
        request_id: rid1.clone(),
        user_input: "Reply with exactly: OK. Nothing else.".into(),
        context: None,
        reply_to: reply_to1.clone(),
    };
    nats.publish(
        "cc.dispatch.ccquotatest".to_string(),
        serde_json::to_vec(&d1).unwrap().into(),
    )
    .await
    .unwrap();
    let r1: AgentReply = serde_json::from_slice(
        &tokio::time::timeout(Duration::from_secs(60), sub1.next())
            .await
            .expect("first reply timeout")
            .expect("first reply none")
            .payload,
    )
    .expect("decode first reply");
    eprintln!("[quota_test] first reply: {r1:?}");
    assert!(r1.ok, "first call should succeed: {:?}", r1.error);

    // Second call should be rejected.
    let rid2 = RequestId::new();
    let reply_to2 = format!("cc.response.ccquotatest.{rid2}");
    let mut sub2 = nats.subscribe(reply_to2.clone()).await.unwrap();
    let d2 = AgentDispatch {
        request_id: rid2.clone(),
        user_input: "Reply with exactly: OK. Nothing else.".into(),
        context: None,
        reply_to: reply_to2.clone(),
    };
    nats.publish(
        "cc.dispatch.ccquotatest".to_string(),
        serde_json::to_vec(&d2).unwrap().into(),
    )
    .await
    .unwrap();
    let r2: AgentReply = serde_json::from_slice(
        &tokio::time::timeout(Duration::from_secs(15), sub2.next())
            .await
            .expect("second reply timeout (quota check should be fast)")
            .expect("second reply none")
            .payload,
    )
    .expect("decode second reply");
    eprintln!("[quota_test] second reply: {r2:?}");
    assert!(!r2.ok, "second call should be rejected by quota");
    assert!(
        r2.error
            .as_deref()
            .unwrap_or("")
            .contains("daily cost limit"),
        "expected daily cost limit error, got: {:?}",
        r2.error
    );
}
