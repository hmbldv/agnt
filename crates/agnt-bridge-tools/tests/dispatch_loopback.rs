//! Live dispatch_agent loopback test (`#[ignore]`d).
//!
//! Stands up a fake "echo agent" on the bus: a tokio task that subscribes
//! to `agent.dispatch.<echo_name>` and replies to whatever `reply_to` the
//! incoming dispatch carried. Then calls
//! [`agnt_bridge_tools::dispatch::dispatch_and_wait`] from the test and
//! asserts the reply text round-trips correctly.
//!
//! Why ignored:
//! - Requires NATS reachable at `nats://localhost:4222` with `NATS_USER` /
//!   `NATS_PASSWORD` set.
//!
//! Run:
//! ```bash
//! NATS_USER=… NATS_PASSWORD=… \
//!   cargo test -p agnt-bridge-tools --test dispatch_loopback -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::time::timeout;

use voicectl_core::config::BusConfig;
use voicectl_core::events::{AgentDispatch, AgentReply};
use voicectl_net::Bus;

const NATS_URL: &str = "nats://localhost:4222";

#[tokio::test]
#[ignore = "requires live NATS at localhost:4222 with NATS_USER + NATS_PASSWORD"]
async fn dispatch_agent_round_trip_against_loopback_echo() {
    assert!(std::env::var("NATS_USER").is_ok(), "NATS_USER required");
    assert!(
        std::env::var("NATS_PASSWORD").is_ok(),
        "NATS_PASSWORD required"
    );

    let bus_cfg = BusConfig {
        nats_url: NATS_URL.into(),
        subject_prefix: String::new(),
        user_env: "NATS_USER".into(),
        password_env: "NATS_PASSWORD".into(),
    };
    let bus = Arc::new(Bus::from_config(&bus_cfg).await.expect("connect NATS"));

    // Fake echo-agent: subscribe to agent.dispatch.<echo>, reply on reply_to.
    let echo_name = format!("dispatch-loopback-echo-{}", uuid::Uuid::new_v4());
    let dispatch_subject = format!("agent.dispatch.{echo_name}");
    let mut sub = bus
        .client
        .subscribe(dispatch_subject.clone())
        .await
        .expect("subscribe echo");

    let echo_bus = Arc::clone(&bus);
    let echo_handle = tokio::spawn(async move {
        if let Some(msg) = sub.next().await {
            let dispatch: AgentDispatch =
                serde_json::from_slice(&msg.payload).expect("decode dispatch");
            let reply = AgentReply {
                request_id: dispatch.request_id.clone(),
                ok: true,
                text: format!("echoed: {}", dispatch.user_input),
                tokens: 0,
                duration_ms: 1,
                tool_calls: Vec::new(),
                error: None,
            };
            let payload = serde_json::to_vec(&reply).unwrap();
            echo_bus
                .client
                .publish(dispatch.reply_to.clone(), payload.into())
                .await
                .expect("publish reply");
        }
    });

    // Give the subscription a beat to register on the server.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let result = timeout(
        Duration::from_secs(5),
        agnt_bridge_tools::dispatch::dispatch_and_wait(&bus, &echo_name, "hello loopback", 5_000),
    )
    .await
    .expect("dispatch_and_wait timed out at the test level");

    let text = result.expect("dispatch_and_wait returned an error");
    assert_eq!(text, "echoed: hello loopback");

    // Clean up the echo subscriber.
    echo_handle.abort();
}
