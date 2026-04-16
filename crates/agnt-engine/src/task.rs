use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A Task is the session contract — what to do, how to retry, when to stop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub task_id: String,
    pub mode: ExecutionMode,
    pub retry: RetryPolicy,
    pub recovery: RecoveryPolicy,
    pub budget: BudgetPolicy,
    pub terminal: TerminalPolicy,
    pub payload: TaskPayload,
}

/// The work itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPayload {
    pub instructions: String,
    #[serde(default)]
    pub context_handles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_output_schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_handle_key: Option<String>,
}

// -- Execution Mode --

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionMode {
    OneShot,
    Loop {
        interval_secs: u64,
        max_iterations: Option<u32>,
    },
    UntilSuccess {
        max_attempts: u32,
    },
    Pipeline {
        steps: Vec<TaskPayload>,
    },
}

// -- Retry --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub backoff: Backoff,
    #[serde(default = "default_retryable_errors")]
    pub retryable_errors: Vec<ErrorClass>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Backoff {
    Fixed { delay_ms: u64 },
    Exponential {
        base_ms: u64,
        cap_ms: u64,
        jitter: bool,
    },
}

impl Default for Backoff {
    fn default() -> Self {
        Self::Exponential {
            base_ms: 500,
            cap_ms: 32_000,
            jitter: true,
        }
    }
}

impl Backoff {
    pub fn delay(&self, attempt: u32) -> Duration {
        match self {
            Self::Fixed { delay_ms } => Duration::from_millis(*delay_ms),
            Self::Exponential {
                base_ms,
                cap_ms,
                jitter,
            } => {
                let delay = (*base_ms * 2u64.saturating_pow(attempt)).min(*cap_ms);
                if *jitter {
                    let jitter_range = delay / 4;
                    let jitter_offset = rand::random::<u64>() % jitter_range.max(1);
                    Duration::from_millis(delay + jitter_offset)
                } else {
                    Duration::from_millis(delay)
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Network,
    RateLimit,
    ServerError,
    InferenceError,
    ContextOverflow,
    ToolFailure,
}

fn default_max_retries() -> u32 {
    10
}

fn default_retryable_errors() -> Vec<ErrorClass> {
    vec![
        ErrorClass::Network,
        ErrorClass::RateLimit,
        ErrorClass::ServerError,
        ErrorClass::InferenceError,
    ]
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            backoff: Backoff::default(),
            retryable_errors: default_retryable_errors(),
        }
    }
}

// -- Recovery --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryPolicy {
    #[serde(default = "default_cascade")]
    pub cascade: Vec<RecoveryStep>,
    #[serde(default = "default_max_escalations")]
    pub max_escalations: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStep {
    TrimContext,
    Compact,
    EscalateBudget,
    FallbackModel,
}

fn default_cascade() -> Vec<RecoveryStep> {
    vec![
        RecoveryStep::TrimContext,
        RecoveryStep::Compact,
        RecoveryStep::EscalateBudget,
    ]
}

fn default_max_escalations() -> u32 {
    2
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            cascade: default_cascade(),
            max_escalations: default_max_escalations(),
        }
    }
}

// -- Budget --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetPolicy {
    #[serde(default = "default_threshold")]
    pub completion_threshold: f64,
    #[serde(default = "default_dim_window")]
    pub diminishing_returns_window: u32,
    #[serde(default = "default_min_progress")]
    pub diminishing_returns_min_progress: u64,
}

fn default_threshold() -> f64 {
    0.9
}
fn default_dim_window() -> u32 {
    3
}
fn default_min_progress() -> u64 {
    50
}

impl Default for BudgetPolicy {
    fn default() -> Self {
        Self {
            completion_threshold: default_threshold(),
            diminishing_returns_window: default_dim_window(),
            diminishing_returns_min_progress: default_min_progress(),
        }
    }
}

// -- Terminal --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalPolicy {
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default = "default_timeout")]
    pub attempt_timeout_secs: u64,
    #[serde(default)]
    pub on_step_failure: StepFailureAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StepFailureAction {
    #[default]
    Abort,
    Skip,
    Retry,
}

fn default_timeout() -> u64 {
    300
}

impl Default for TerminalPolicy {
    fn default() -> Self {
        Self {
            max_turns: None,
            attempt_timeout_secs: default_timeout(),
            on_step_failure: StepFailureAction::default(),
        }
    }
}

// -- Terminal Reason --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalReason {
    Completed,
    PipelineCompleted,
    MaxTurns,
    BudgetExhausted,
    TtlExpired,
    MaxRetries,
    Aborted,
    PolicyBlocked,
    ModelError,
    DiminishingReturns,
    MaxIterations,
    Decommissioned,
}

// -- Cron --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronSchedule {
    pub expression: String,
    #[serde(default = "default_tz")]
    pub timezone: String,
    #[serde(default)]
    pub run_on_start: bool,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
}

fn default_tz() -> String {
    "UTC".into()
}
fn default_max_concurrent() -> u32 {
    1
}

impl Task {
    pub fn new_id() -> String {
        format!("task-{}", uuid::Uuid::new_v4().simple())
    }
}
