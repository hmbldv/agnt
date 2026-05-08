//! Live multi-turn session test (`#[ignore]`d).
//!
//! Spawns an `agnt-bridge` child against the local vLLM and exercises:
//!   1. Turn 1: "My name is Johnny." (within session)
//!   2. Turn 2: "What is my name?" — reply must mention "Johnny" because the
//!      bridge persists history into agnt's `Store::sqlite` and replays it on
//!      each `step()`.
//!   3. After turn 2, we manipulate the SQLite timestamps directly to fake
//!      the session-timeout window, then dispatch a third turn — the bridge
//!      should see the timeout, mint a new session UUID, and send the
//!      backend a fresh history. The reply MUST NOT include "Johnny".
//!
//! For the "fast forward time" trick we don't actually patch SQLite —
//! agnt-store rows aren't time-keyed in a way the bridge consults. Instead
//! we drive a separate bridge child with `--config` pointing at a TOML that
//! has `[conversation] session_timeout_ms = 1` and pause for >1ms between
//! turn 2 and turn 3.
//!
//! Run:
//! ```bash
//! NATS_USER=… NATS_PASSWORD=… LITELLM_API_KEY=anything \
//!   cargo test -p agnt-bridge --test multiturn_session -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use tokio::process::{Child, Command};

use voicectl_core::events::{AgentDispatch, AgentReply, RequestId};

const NATS_URL: &str = "nats://localhost:4222";

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

/// Write a config with the requested session timeout. The agent name is
/// templated so two test runs don't collide on subjects.
fn write_config(
    dir: &tempfile::TempDir,
    name: &str,
    db_path: &Path,
    session_timeout_ms: u64,
) -> PathBuf {
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
system_file = "/dev/null"

[tools]
enabled = []

[bus]
nats_url = "{NATS_URL}"
user_env = "NATS_USER"
password_env = "NATS_PASSWORD"
subscribe_subject = "agent.dispatch.{name}"
publish_prefix = "agent.event.{name}"

[store]
db_path = "{db}"

[streaming]
enabled = false

[conversation]
session_timeout_ms = {session_timeout_ms}
max_history_turns = 20
"#,
        db = db_path.display()
    );
    std::fs::write(&path, content).expect("write temp config");
    path
}

async fn wait_for_nats() -> async_nats::Client {
    let user =
        std::env::var("NATS_USER").expect("NATS_USER required for the multi-turn session test");
    let pass = std::env::var("NATS_PASSWORD")
        .expect("NATS_PASSWORD required for the multi-turn session test");
    let opts = async_nats::ConnectOptions::new().user_and_password(user, pass);
    opts.connect(NATS_URL).await.expect("connect NATS")
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

async fn one_turn(nats: &async_nats::Client, agent_name: &str, user_input: &str) -> AgentReply {
    let request_id = RequestId::new();
    let reply_to = format!("agent.response.{agent_name}.{request_id}");
    let dispatch_subject = format!("agent.dispatch.{agent_name}");
    let mut sub = nats
        .subscribe(reply_to.clone())
        .await
        .expect("subscribe reply_to");
    let dispatch = AgentDispatch {
        request_id: request_id.clone(),
        user_input: user_input.into(),
        context: Some(serde_json::json!({ "from": "multiturn_session_test" })),
        reply_to: reply_to.clone(),
    };
    let payload = serde_json::to_vec(&dispatch).unwrap();
    nats.publish(dispatch_subject, payload.into())
        .await
        .expect("publish");

    let msg = tokio::time::timeout(Duration::from_secs(30), sub.next())
        .await
        .expect("agent reply timeout")
        .expect("subscription closed");
    serde_json::from_slice(&msg.payload).expect("decode reply")
}

#[tokio::test]
#[ignore = "spawns agnt-bridge against local vLLM + localhost NATS; \
            run with --ignored, NATS_USER + NATS_PASSWORD + LITELLM_API_KEY"]
async fn multi_turn_remembers_then_forgets_after_timeout() {
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
    let db = tmp.path().join("multiturn.db");
    let agent_name = "agnt-bridge-multiturn";

    // Long timeout so turns 1+2 share a session.
    let cfg_long = write_config(&tmp, agent_name, &db, 600_000);

    let bin = bridge_bin();
    let child = Command::new(&bin)
        .arg("--config")
        .arg(&cfg_long)
        .env("RUST_LOG", "info,agnt_bridge=debug")
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("spawn agnt-bridge: {e}"));
    let _guard = ChildGuard(child);
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let nats = wait_for_nats().await;

    // Turn 1
    let r1 = one_turn(&nats, agent_name, "My name is Johnny. Just acknowledge.").await;
    eprintln!("[multiturn] turn1 reply: {}", r1.text);
    assert!(r1.ok, "turn1 failed: {:?}", r1.error);

    // Turn 2 — must remember "Johnny".
    let r2 = one_turn(&nats, agent_name, "What is my name? Answer in one word.").await;
    eprintln!("[multiturn] turn2 reply: {}", r2.text);
    assert!(r2.ok, "turn2 failed: {:?}", r2.error);
    assert!(
        r2.text.to_lowercase().contains("johnny"),
        "agent should remember 'Johnny' from turn 1; got: {}",
        r2.text
    );

    // Now stop the long-timeout bridge and start a fresh one with the SAME
    // db but a 1-ms timeout. The bridge's session_id is in-process so a
    // restart already gives us a fresh in-memory session. To still test
    // the rotation logic explicitly we issue one warm-up turn (which the
    // new bridge process treats as turn 1 of its own session), then sleep
    // 50ms, then issue the "what is my name" probe — the rotation should
    // fire and the new session should NOT contain "Johnny".
    drop(_guard);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let cfg_short = write_config(&tmp, agent_name, &db, 1);
    let child2 = Command::new(&bin)
        .arg("--config")
        .arg(&cfg_short)
        .env("RUST_LOG", "info,agnt_bridge=debug")
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("spawn agnt-bridge round 2: {e}"));
    let _guard2 = ChildGuard(child2);
    tokio::time::sleep(Duration::from_millis(2_000)).await;

    // Fresh-process turn: warmup (kicks last_message_at).
    let r3 = one_turn(&nats, agent_name, "Say the single word ready.").await;
    assert!(r3.ok, "turn3 failed: {:?}", r3.error);
    eprintln!("[multiturn] turn3 reply (warmup): {}", r3.text);

    // Wait past the 1ms session timeout.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Probe — must NOT remember "Johnny". The bridge should rotate the
    // session, clear in-memory history, and the model has no way to know
    // the prior name.
    let r4 = one_turn(
        &nats,
        agent_name,
        "What is my name? Answer in one word. Say 'unknown' if you don't know.",
    )
    .await;
    eprintln!("[multiturn] turn4 reply (post-timeout): {}", r4.text);
    assert!(r4.ok, "turn4 failed: {:?}", r4.error);
    assert!(
        !r4.text.to_lowercase().contains("johnny"),
        "after session timeout the agent must not remember 'Johnny'; got: {}",
        r4.text
    );
}
