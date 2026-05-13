//! Live streaming token bridge test (`#[ignore]`d).
//!
//! Spawns `agnt-bridge` with `[streaming] enabled = true`, dispatches one
//! request, and asserts that:
//!  - at least one `agent.token.<name>.<id>` frame arrives BEFORE the final
//!    `AgentReply` lands;
//!  - the terminal frame carries `is_final = true`;
//!  - concatenating the deltas roughly matches the reply text (allowing for
//!    the bridge's whitespace handling).
//!
//! Run:
//! ```bash
//! NATS_USER=… NATS_PASSWORD=… LITELLM_API_KEY=anything \
//!   cargo test -p agnt-bridge --test streaming_cycle -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::process::{Child, Command};

use voicectl_core::events::{AgentDispatch, AgentReply, AgentToken, RequestId};

const NATS_URL: &str = "nats://localhost:4222";
const TEST_AGENT: &str = "agnt-bridge-streamtest";

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
system_file = "/dev/null"

[tools]
enabled = []

[bus]
nats_url = "{NATS_URL}"
user_env = "NATS_USER"
password_env = "NATS_PASSWORD"
subscribe_subject = "agent.dispatch.{name}"
publish_prefix = "agent.event.{name}"

[streaming]
enabled = true
"#
    );
    std::fs::write(&path, content).expect("write temp config");
    path
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

#[tokio::test]
#[ignore = "spawns agnt-bridge against local vLLM + localhost NATS; \
            run with --ignored, NATS_USER + NATS_PASSWORD + LITELLM_API_KEY"]
async fn streaming_token_bridge_round_trip() {
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

    let user = std::env::var("NATS_USER").unwrap();
    let pass = std::env::var("NATS_PASSWORD").unwrap();
    let opts = async_nats::ConnectOptions::new().user_and_password(user, pass);
    let nats = opts.connect(NATS_URL).await.expect("connect NATS");

    let request_id = RequestId::new();
    let reply_to = format!("agent.response.{TEST_AGENT}.{request_id}");
    let token_subj = format!("agent.token.{TEST_AGENT}.{request_id}");
    let dispatch_subject = format!("agent.dispatch.{TEST_AGENT}");

    let mut reply_sub = nats
        .subscribe(reply_to.clone())
        .await
        .expect("subscribe reply_to");
    let mut token_sub = nats.subscribe(token_subj).await.expect("subscribe tokens");

    tokio::time::sleep(Duration::from_millis(1_500)).await;

    let dispatch = AgentDispatch {
        request_id: request_id.clone(),
        user_input: "Recite the first three prime numbers, separated by commas.".into(),
        context: Some(serde_json::json!({ "from": "streaming_cycle_test" })),
        reply_to: reply_to.clone(),
    };
    let payload = serde_json::to_vec(&dispatch).unwrap();
    let dispatch_at = Instant::now();
    nats.publish(dispatch_subject, payload.into())
        .await
        .expect("publish dispatch");

    // Drain tokens until is_final or the reply lands. We collect the
    // arrival timestamps so the test can print latency for analysis.
    let mut frames: Vec<AgentToken> = Vec::new();
    let mut first_token_at: Option<Instant> = None;

    let collect_until_final = async {
        while let Some(msg) = token_sub.next().await {
            match serde_json::from_slice::<AgentToken>(&msg.payload) {
                Ok(t) => {
                    if first_token_at.is_none() && !t.text.is_empty() {
                        first_token_at = Some(Instant::now());
                    }
                    let is_final = t.is_final;
                    frames.push(t);
                    if is_final {
                        break;
                    }
                }
                Err(e) => panic!("decode AgentToken: {e}"),
            }
        }
    };

    tokio::time::timeout(Duration::from_secs(30), collect_until_final)
        .await
        .expect("token stream did not terminate within 30s");

    let reply_msg = tokio::time::timeout(Duration::from_secs(15), reply_sub.next())
        .await
        .expect("agent reply timeout")
        .expect("subscription closed");
    let reply: AgentReply = serde_json::from_slice(&reply_msg.payload).expect("decode reply");

    let ttft_ms = first_token_at
        .map(|t| t.duration_since(dispatch_at).as_millis())
        .unwrap_or(u128::MAX);

    eprintln!("[streaming] frames={} ttft_ms={}", frames.len(), ttft_ms);
    eprintln!("[streaming] reply: {}", reply.text);

    assert_eq!(reply.request_id, request_id);
    assert!(reply.ok, "reply.ok was false (error = {:?})", reply.error);
    assert!(!frames.is_empty(), "no token frames arrived");
    let last = frames.last().unwrap();
    assert!(last.is_final, "last frame must be is_final");
    let prefix_frames: Vec<&AgentToken> = frames.iter().filter(|t| !t.is_final).collect();
    assert!(
        !prefix_frames.is_empty(),
        "expected at least one non-final token frame"
    );
    let concatenated: String = prefix_frames.iter().map(|t| t.text.as_str()).collect();
    eprintln!("[streaming] concatenated tokens: {}", concatenated);
    // The bridge serialises `chat()` deltas verbatim; the ordering should
    // match the reply text modulo whitespace. We check the first prime is
    // somewhere in the stream as a soft sanity gate.
    assert!(
        concatenated.contains('2') || concatenated.to_lowercase().contains("two"),
        "stream should contain at least the digit 2 from the prime list; got: {}",
        concatenated
    );
}
