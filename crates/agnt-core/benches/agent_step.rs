//! Benchmark `Agent::step` end-to-end against a mock backend.
//!
//! The mock backend returns a canned response immediately, so the benchmark
//! measures the agent loop's per-step overhead: message window preparation,
//! observer dispatch, tool call parsing (none here), and message append.

use agnt_core::{Agent, BackendError, LlmBackend, Message};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use serde_json::Value;

/// Returns `"ok"` with no tool calls — single-turn loop.
struct MockBackend;
impl LlmBackend for MockBackend {
    fn model(&self) -> &str {
        "mock"
    }
    fn chat(
        &self,
        _messages: &[Message],
        _tools: &Value,
        _on_token: Option<&mut dyn FnMut(&str)>,
    ) -> Result<Message, BackendError> {
        Ok(Message {
            role: "assistant".into(),
            content: Some("ok".into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            usage: None,
        })
    }
}

fn make_agent(prior_count: usize) -> Agent<MockBackend> {
    let mut a = Agent::new(MockBackend, "You are helpful.");
    a.max_window = 40;
    a.stream = false;
    for i in 0..prior_count {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        a.messages.push(Message {
            role: role.into(),
            content: Some(format!("prior message {}", i)),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            usage: None,
        });
    }
    a
}

fn bench_step_no_tools(c: &mut Criterion) {
    let mut group = c.benchmark_group("agent_step_no_tools");

    // Sweep prior-history sizes to hit both the borrow path and the
    // truncated-clone path.
    for &prior in &[0usize, 10, 39, 40, 100, 500, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(prior), &prior, |b, &prior| {
            b.iter_batched(
                || make_agent(prior),
                |mut agent| {
                    let _ = black_box(agent.step("hi"));
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_step_no_tools);
criterion_main!(benches);
