use crate::observer::EngineObserver;
use crate::task::*;
use agnt_core::{Agent, AgentBuilder, LlmBackend, Registry, Tool};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Result of the full engine run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineResult {
    pub reason: TerminalReason,
    pub tasks_completed: u32,
    pub tasks_failed: u32,
    pub total_attempts: u32,
    pub credits_consumed: u64,
    pub last_result: Option<String>,
}

/// Mutable state tracked across all task executions.
#[derive(Debug)]
struct EngineState {
    budget_allocated: u64,
    credits_per_step: u64,
    credits: Arc<AtomicU64>,
    tasks_completed: u32,
    tasks_failed: u32,
    total_attempts: u32,
    last_result: Option<String>,
}

impl EngineState {
    fn new(budget_allocated: u64, credits_per_step: u64, credits: Arc<AtomicU64>) -> Self {
        Self {
            budget_allocated,
            credits_per_step,
            credits,
            tasks_completed: 0,
            tasks_failed: 0,
            total_attempts: 0,
            last_result: None,
        }
    }

    fn credits_consumed(&self) -> u64 {
        self.credits.load(Ordering::Relaxed)
    }

    fn is_budget_exhausted(&self, policy: &BudgetPolicy) -> bool {
        if self.budget_allocated == 0 {
            return false;
        }
        let fraction = self.credits_consumed() as f64 / self.budget_allocated as f64;
        fraction >= policy.completion_threshold
    }

    fn into_result(self, reason: TerminalReason) -> EngineResult {
        EngineResult {
            reason,
            tasks_completed: self.tasks_completed,
            tasks_failed: self.tasks_failed,
            total_attempts: self.total_attempts,
            credits_consumed: self.credits_consumed(),
            last_result: self.last_result,
        }
    }
}

/// Configuration for building an AgentEngine.
pub struct EngineConfig<B: LlmBackend> {
    pub backend: B,
    pub tools: Vec<Box<dyn Tool>>,
    pub system_prompt: String,
    pub max_steps: usize,
    /// Flat credits charged per inference step. Real token metering
    /// requires agnt-net to expose usage from HTTP responses.
    pub credits_per_step: u64,
    pub budget_allocated: u64,
    pub ttl_expires_at: chrono::DateTime<chrono::Utc>,
    pub shutdown: Arc<Notify>,
    /// Tools the agent is allowed to call (empty = all).
    pub permitted_tools: Vec<String>,
    /// Tools explicitly denied.
    pub denied_tools: Vec<String>,
}

/// Run a task to completion with full retry + recovery.
///
/// Creates an agnt-core Agent<B> with an EngineObserver, then handles
/// retry, recovery cascade, budget, and execution modes.
pub async fn run_agent<B: LlmBackend + 'static>(
    config: EngineConfig<B>,
    task: Task,
) -> EngineResult {
        let credits = EngineObserver::credits_counter();
        let observer = Arc::new(EngineObserver::new(
            credits.clone(),
            config.permitted_tools.clone(),
            config.denied_tools.clone(),
        ));

        // Build the agnt-core Agent.
        let mut registry = Registry::new();
        for tool in config.tools {
            registry.register(tool);
        }

        let agent = AgentBuilder::new(config.backend)
            .system(&config.system_prompt)
            .tools(vec![]) // tools registered via registry below
            .max_steps(config.max_steps)
            .observer(observer)
            .build();

        let mut agent: Option<Agent<B>> = match agent {
            Ok(mut a) => {
                a.tools = registry;
                Some(a)
            }
            Err(e) => {
                warn!(error = %e, "failed to build agent");
                return EngineResult {
                    reason: TerminalReason::ModelError,
                    tasks_completed: 0,
                    tasks_failed: 0,
                    total_attempts: 0,
                    credits_consumed: 0,
                    last_result: Some(e),
                };
            }
        };

        let mut state = EngineState::new(config.budget_allocated, config.credits_per_step, credits);
        let shutdown = config.shutdown;
        let ttl = config.ttl_expires_at;

        // Execute based on mode.
        match &task.mode {
            ExecutionMode::OneShot => {
                let result = execute_task(&task, &task.payload, &mut agent, &mut state, &shutdown, ttl).await;
                match result {
                    StepResult::Success(s) => {
                        state.tasks_completed += 1;
                        state.last_result = Some(s);
                        state.into_result(TerminalReason::Completed)
                    }
                    StepResult::Failed(r) => {
                        state.tasks_failed += 1;
                        state.into_result(r)
                    }
                }
            }

            ExecutionMode::UntilSuccess { max_attempts } => {
                for attempt in 0..*max_attempts {
                    let result = execute_task(&task, &task.payload, &mut agent, &mut state, &shutdown, ttl).await;
                    match result {
                        StepResult::Success(s) => {
                            state.tasks_completed += 1;
                            state.last_result = Some(s);
                            return state.into_result(TerminalReason::Completed);
                        }
                        StepResult::Failed(_) => {
                            state.tasks_failed += 1;
                            if attempt + 1 < *max_attempts {
                                debug!(attempt = attempt + 1, "UntilSuccess: retrying");
                            }
                        }
                    }
                }
                state.into_result(TerminalReason::MaxRetries)
            }

            ExecutionMode::Loop { interval_secs, max_iterations } => {
                // Reject interval_secs = 0 early — a zero-interval loop is a
                // tight spin that saturates the CPU and provides no useful
                // behaviour.  Fail fast here rather than deep in the loop body.
                if *interval_secs == 0 {
                    warn!("Loop mode rejected: interval_secs must be >= 1");
                    return EngineResult {
                        reason: TerminalReason::ModelError,
                        tasks_completed: 0,
                        tasks_failed: 0,
                        total_attempts: 0,
                        credits_consumed: 0,
                        last_result: Some("interval_secs must be >= 1".into()),
                    };
                }

                let mut iteration = 0u32;
                loop {
                    if max_iterations.map_or(false, |max| iteration >= max) {
                        return state.into_result(TerminalReason::MaxIterations);
                    }

                    let result = execute_task(&task, &task.payload, &mut agent, &mut state, &shutdown, ttl).await;
                    match result {
                        StepResult::Success(s) => {
                            state.tasks_completed += 1;
                            state.last_result = Some(s);
                        }
                        StepResult::Failed(r) => {
                            state.tasks_failed += 1;
                            warn!(iteration, reason = ?r, "loop iteration failed");
                        }
                    }

                    iteration += 1;

                    // Sleep for interval, but respect shutdown.
                    tokio::select! {
                        _ = shutdown.notified() => {
                            return state.into_result(TerminalReason::Aborted);
                        }
                        _ = tokio::time::sleep(Duration::from_secs(*interval_secs)) => {}
                    }

                    if chrono::Utc::now() >= ttl {
                        return state.into_result(TerminalReason::TtlExpired);
                    }
                    if state.is_budget_exhausted(&task.budget) {
                        return state.into_result(TerminalReason::BudgetExhausted);
                    }
                }
            }

            ExecutionMode::Pipeline { steps } => {
                for (idx, step_payload) in steps.iter().enumerate() {
                    let result = execute_task(&task, step_payload, &mut agent, &mut state, &shutdown, ttl).await;
                    match result {
                        StepResult::Success(s) => {
                            state.tasks_completed += 1;
                            state.last_result = Some(s);
                        }
                        StepResult::Failed(r) => {
                            state.tasks_failed += 1;
                            match task.terminal.on_step_failure {
                                StepFailureAction::Abort => return state.into_result(r),
                                StepFailureAction::Skip => {
                                    warn!(step = idx, "pipeline step failed, skipping");
                                    continue;
                                }
                                StepFailureAction::Retry => return state.into_result(r),
                            }
                        }
                    }
                }
                state.into_result(TerminalReason::PipelineCompleted)
            }
        }
}

enum StepResult {
    Success(String),
    Failed(TerminalReason),
}

/// Execute a single task payload with retry + recovery cascade.
///
/// `agent` is passed as `&mut Option<Agent<B>>` so we can move it into
/// `spawn_blocking` (which requires `'static + Send`) and return it after the
/// step completes.  The `Option` is always `Some` on entry and on any path
/// where the agent is still valid.  On timeout the spawned thread holds the
/// agent — we cannot get it back without waiting — so we accept the loss and
/// leave the slot `None`; the caller (the mode loop) terminates immediately
/// after a `StepResult::Failed`, so the `None` is never accessed again.
async fn execute_task<B: LlmBackend + 'static>(
    task: &Task,
    payload: &TaskPayload,
    agent: &mut Option<Agent<B>>,
    state: &mut EngineState,
    shutdown: &Arc<Notify>,
    ttl: chrono::DateTime<chrono::Utc>,
) -> StepResult {
    let mut consecutive_low_progress = 0u32;

    // Validate attempt_timeout_secs once, before the retry loop.
    // Zero is treated as "use the default" (300s / 5 minutes).
    let attempt_timeout_secs = {
        let raw = task.terminal.attempt_timeout_secs;
        if raw == 0 { 300 } else { raw }
    };
    let timeout_dur = Duration::from_secs(attempt_timeout_secs);

    for attempt in 0..=task.retry.max_retries {
        state.total_attempts += 1;
        let credits_before = state.credits_consumed();

        // Pre-checks.
        if chrono::Utc::now() >= ttl {
            return StepResult::Failed(TerminalReason::TtlExpired);
        }
        if state.is_budget_exhausted(&task.budget) {
            return StepResult::Failed(TerminalReason::BudgetExhausted);
        }

        let instructions = payload.instructions.clone();

        // Agent::step() is sync and blocks on network I/O (ureq).
        // spawn_blocking hands it off to a dedicated blocking thread pool,
        // making it cancellable by tokio::time::timeout.  Agent<B> is Send
        // because LlmBackend: Send + Sync (trait bound), Observer: Send + Sync,
        // MessageStore: Send + Sync, Tool: Send + Sync, and on_token is
        // Option<Box<dyn FnMut + Send>>.
        //
        // We move the agent out of the Option into the closure, then return it
        // alongside the step result so we can restore it after each attempt.
        let moved_agent = agent.take().expect("agent must be Some at step entry");

        let handle = tokio::task::spawn_blocking(move || {
            let mut a = moved_agent;
            let result = a.step(&instructions);
            (a, result) // always return the agent so the caller can restore it
        });

        let timed = tokio::time::timeout(timeout_dur, handle).await;

        let step_result: Result<String, String> = match timed {
            Err(_elapsed) => {
                // Timeout fired.  The blocking thread may still be running;
                // we cannot retrieve the agent from it without blocking
                // indefinitely.  Leave the slot None — the callers below
                // return StepResult::Failed immediately after this path, so
                // the None is never dereferenced.
                warn!(
                    attempt,
                    timeout_secs = attempt_timeout_secs,
                    "agent step timed out"
                );
                Err(format!("timeout after {}s", attempt_timeout_secs))
            }
            Ok(Err(join_err)) => {
                // The blocking thread panicked.  We cannot recover the agent.
                tracing::error!(attempt, panic = %join_err, "agent step thread panicked");
                Err(format!("agent thread panicked: {}", join_err))
            }
            Ok(Ok((returned_agent, result))) => {
                // Happy path: restore the agent for the next iteration.
                *agent = Some(returned_agent);
                result
            }
        };

        match step_result {
            Ok(output) => {
                info!(attempt, output_len = output.len(), "step succeeded");
                // Add inference credits (flat cost per step — real token
                // metering needs agnt-net to expose usage from responses).
                state.credits.fetch_add(state.credits_per_step, Ordering::Relaxed);
                return StepResult::Success(output);
            }
            Err(error) => {
                let credits_used = state.credits_consumed() - credits_before;
                debug!(attempt, error = %error, "step failed");

                // Classify error.
                let error_class = classify_error(&error);

                if !task.retry.retryable_errors.contains(&error_class) {
                    return StepResult::Failed(TerminalReason::ModelError);
                }

                // Diminishing returns detection.
                if credits_used < task.budget.diminishing_returns_min_progress {
                    consecutive_low_progress += 1;
                    if consecutive_low_progress >= task.budget.diminishing_returns_window {
                        return StepResult::Failed(TerminalReason::DiminishingReturns);
                    }
                } else {
                    consecutive_low_progress = 0;
                }

                // If retries exhausted, try recovery cascade.
                // Only attempt recovery if the agent is still available
                // (it will be None after a timeout or panic).
                if attempt == task.retry.max_retries {
                    if let Some(ref mut a) = agent {
                        for step in &task.recovery.cascade {
                            match step {
                                RecoveryStep::TrimContext => {
                                    info!("recovery: trimming context");
                                    let max = a.max_window;
                                    if a.messages.len() > max {
                                        let drain = a.messages.len() - max;
                                        a.messages.drain(1..=drain); // keep system prompt
                                    }
                                }
                                RecoveryStep::Compact => {
                                    info!("recovery: compacting (clearing non-system messages)");
                                    a.messages.truncate(1); // keep only system prompt
                                }
                                RecoveryStep::EscalateBudget => {
                                    info!("recovery: budget escalation (not connected to mesh)");
                                    // In standalone mode, no parent to escalate to.
                                    // In mesh mode, msh-gtwy handles this via NATS.
                                }
                                RecoveryStep::FallbackModel => {
                                    info!("recovery: fallback model (not implemented)");
                                    // Would require swapping the backend, which needs
                                    // a different Agent<B>. Deferred.
                                }
                            }
                        }
                    }
                    return StepResult::Failed(TerminalReason::MaxRetries);
                }

                // Backoff before retry.
                let delay = task.retry.backoff.delay(attempt);
                debug!(attempt, delay_ms = delay.as_millis(), "backing off");
                tokio::select! {
                    _ = shutdown.notified() => {
                        return StepResult::Failed(TerminalReason::Aborted);
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }

    StepResult::Failed(TerminalReason::MaxRetries)
}

/// Classify an error string into an ErrorClass for retry decisions.
///
/// Input is truncated to 256 bytes before any pattern matching.  This prevents
/// an adversarial model from manipulating retry behaviour by injecting
/// classification keywords into an arbitrarily long error payload (H2).
fn classify_error(error: &str) -> ErrorClass {
    let error = &error[..error.len().min(256)]; // truncate before any pattern matching
    let lower = error.to_lowercase();
    if lower.contains("timeout") || lower.contains("connection") || lower.contains("econnreset") {
        ErrorClass::Network
    } else if lower.contains("rate limit") || lower.contains("429") || lower.contains("529") {
        ErrorClass::RateLimit
    } else if lower.contains("500") || lower.contains("502") || lower.contains("503") {
        ErrorClass::ServerError
    } else if lower.contains("context") || lower.contains("too long") || lower.contains("413") {
        ErrorClass::ContextOverflow
    } else if lower.contains("tool") || lower.contains("dispatch") {
        ErrorClass::ToolFailure
    } else {
        ErrorClass::InferenceError
    }
}
