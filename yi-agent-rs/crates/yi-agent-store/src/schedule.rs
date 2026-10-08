//! Effective runtime and scheduled-session policy values.

use std::fmt;

use chrono::{DateTime, Local, Utc};
use croner::parser::{CronParser, Seconds};
use serde::{Deserialize, Serialize};
use yi_agent_core::subagent::task::{BudgetKind, TimeoutKind};

/// A validated user-facing schedule. The expression always uses standard
/// five-field cron; the parser is explicitly configured to reject seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleDefinition {
    pub cron: String,
    pub objective: String,
    pub policy: SchedulePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleDefinitionError {
    EmptyObjective,
    InvalidCron(String),
}

impl fmt::Display for ScheduleDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyObjective => formatter.write_str("schedule objective must not be empty"),
            Self::InvalidCron(reason) => write!(formatter, "invalid five-field cron: {reason}"),
        }
    }
}

impl std::error::Error for ScheduleDefinitionError {}

impl ScheduleDefinition {
    pub fn new(
        cron: impl AsRef<str>,
        objective: impl Into<String>,
    ) -> Result<Self, ScheduleDefinitionError> {
        let cron = cron
            .as_ref()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let objective = objective.into().trim().to_owned();
        if objective.is_empty() {
            return Err(ScheduleDefinitionError::EmptyObjective);
        }
        parse_five_field_cron(&cron)?;
        Ok(Self {
            cron,
            objective,
            policy: SchedulePolicy::default(),
        })
    }

    pub fn next_run_after(
        &self,
        now: DateTime<Local>,
    ) -> Result<DateTime<Local>, ScheduleDefinitionError> {
        parse_five_field_cron(&self.cron)?
            .find_next_occurrence(&now, false)
            .map_err(|error| ScheduleDefinitionError::InvalidCron(error.to_string()))
    }
}

fn parse_five_field_cron(cron: &str) -> Result<croner::Cron, ScheduleDefinitionError> {
    if cron.split_whitespace().count() != 5 {
        return Err(ScheduleDefinitionError::InvalidCron(
            "expected exactly five fields (minute hour day-of-month month day-of-week)".into(),
        ));
    }
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .build()
        .parse(cron)
        .map_err(|error| ScheduleDefinitionError::InvalidCron(error.to_string()))
}

/// A classified failure reaching the runtime retry boundary. Tool callers must
/// explicitly mark a failure retryable; error text is never used as policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryFailure {
    ProviderNetwork,
    ProviderRateLimited,
    ProviderServer,
    ProviderStream,
    Tool { explicitly_retryable: bool },
    PermissionDenied,
    GitConflict,
    TestFailure,
    AuthorityDenied,
    ScopeAmbiguity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    RetryAfter(std::time::Duration),
    Blocked,
    Failed,
}

/// Returns a deterministic bounded retry decision. `retries_used` counts
/// prior automatic retries, while `jitter_millis` is supplied by the caller's
/// random source so this policy remains testable and restart-safe.
pub fn evaluate_retry(
    failure: RetryFailure,
    retries_used: u16,
    retry_limit: u16,
    jitter_millis: u16,
) -> RetryDecision {
    match failure {
        RetryFailure::PermissionDenied
        | RetryFailure::AuthorityDenied
        | RetryFailure::ScopeAmbiguity => RetryDecision::Blocked,
        RetryFailure::GitConflict | RetryFailure::TestFailure => RetryDecision::Failed,
        RetryFailure::Tool {
            explicitly_retryable: false,
        } => RetryDecision::Failed,
        RetryFailure::ProviderNetwork
        | RetryFailure::ProviderRateLimited
        | RetryFailure::ProviderServer
        | RetryFailure::ProviderStream
        | RetryFailure::Tool {
            explicitly_retryable: true,
        } if retries_used < retry_limit => {
            let base_millis = 1_000_u64.saturating_mul(1_u64 << retries_used.min(5));
            let capped_base_millis = base_millis.min(30_000);
            let delay_millis = capped_base_millis
                .saturating_add(u64::from(jitter_millis).min(30_000 - capped_base_millis));
            RetryDecision::RetryAfter(std::time::Duration::from_millis(delay_millis))
        }
        _ => RetryDecision::Failed,
    }
}

/// Immutable limits applied by the daemon to one attempt. Optional ceilings
/// remain unbounded until configured by the user or a narrower project policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogLimits {
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_cost_micros: Option<u64>,
    pub max_wall_time_secs: Option<u64>,
    pub max_idle_time_secs: Option<u64>,
    pub max_resource_wait_secs: Option<u64>,
    pub max_provider_retries: Option<u16>,
    pub max_tool_retries: Option<u16>,
    pub max_rework_cycles: Option<u16>,
}

impl Default for WatchdogLimits {
    fn default() -> Self {
        Self {
            max_turns: Some(200),
            max_tokens: None,
            max_cost_micros: None,
            max_wall_time_secs: Some(2_700),
            max_idle_time_secs: Some(300),
            max_resource_wait_secs: None,
            max_provider_retries: Some(3),
            max_tool_retries: Some(2),
            max_rework_cycles: Some(2),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogUsage {
    pub turns: u32,
    pub tokens: u64,
    pub cost_micros: u64,
    pub provider_retries: u16,
    pub tool_retries: u16,
    pub rework_cycles: u16,
}

/// Inputs originate in durable attempt facts. In particular,
/// `last_meaningful_at` is never advanced by generated text or repeated stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogObservation {
    pub attempt_started_at: DateTime<Utc>,
    pub last_meaningful_at: DateTime<Utc>,
    pub resource_wait_started_at: Option<DateTime<Utc>>,
    pub usage: WatchdogUsage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogOutcome {
    Stalled,
    TimedOut(TimeoutKind),
    BudgetExhausted(BudgetKind),
}

/// Applies deterministic bound precedence. Resource deadlines use the time a
/// request joined the queue; idle time uses only a recorded meaningful event.
pub fn evaluate_watchdog(
    limits: &WatchdogLimits,
    observation: &WatchdogObservation,
    now: DateTime<Utc>,
) -> Option<WatchdogOutcome> {
    let reached = |used: u64, limit: Option<u64>| limit.is_some_and(|limit| used >= limit);
    if reached(
        observation.usage.turns.into(),
        limits.max_turns.map(u64::from),
    ) {
        return Some(WatchdogOutcome::BudgetExhausted(BudgetKind::Turns));
    }
    if reached(observation.usage.tokens, limits.max_tokens) {
        return Some(WatchdogOutcome::BudgetExhausted(BudgetKind::Tokens));
    }
    if reached(observation.usage.cost_micros, limits.max_cost_micros) {
        return Some(WatchdogOutcome::BudgetExhausted(BudgetKind::Cost));
    }
    if reached(
        observation.usage.provider_retries.into(),
        limits.max_provider_retries.map(u64::from),
    ) {
        return Some(WatchdogOutcome::BudgetExhausted(
            BudgetKind::ProviderRetries,
        ));
    }
    if reached(
        observation.usage.tool_retries.into(),
        limits.max_tool_retries.map(u64::from),
    ) {
        return Some(WatchdogOutcome::BudgetExhausted(BudgetKind::ToolRetries));
    }
    if reached(
        observation.usage.rework_cycles.into(),
        limits.max_rework_cycles.map(u64::from),
    ) {
        return Some(WatchdogOutcome::BudgetExhausted(BudgetKind::ReworkCycles));
    }

    let elapsed = |from: DateTime<Utc>| now.signed_duration_since(from).num_seconds().max(0) as u64;
    if reached(
        elapsed(observation.attempt_started_at),
        limits.max_wall_time_secs,
    ) {
        return Some(WatchdogOutcome::TimedOut(TimeoutKind::WallClock));
    }
    if observation
        .resource_wait_started_at
        .is_some_and(|queued_at| reached(elapsed(queued_at), limits.max_resource_wait_secs))
    {
        return Some(WatchdogOutcome::TimedOut(TimeoutKind::Deadline));
    }
    if reached(
        elapsed(observation.last_meaningful_at),
        limits.max_idle_time_secs,
    ) {
        return Some(WatchdogOutcome::Stalled);
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePolicy {
    /// Conservative default for a *scheduled root* (4). This is NOT the daemon's
    /// resident capacity — that is `YI_AGENT_MAX_RESIDENT_SUBAGENTS`
    /// (`yi_agent_runtime::config::RESIDENT_SUBAGENTS_DEFAULT`, default 64) and
    /// it is what actually admits workers. Retained because this struct is
    /// persisted inside `ScheduleDefinition`.
    pub max_resident_subagents: u16,
    pub max_turns: u32,
    pub max_wall_time_secs: u64,
    pub read_only: bool,
    pub allow_coding: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulePriority {
    Background,
    Normal,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapPolicy {
    Skip,
    QueueOne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissedRunPolicy {
    Skip,
    CatchUpOnce,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulePolicy {
    pub runtime: RuntimePolicy,
    pub priority: SchedulePriority,
    pub overlap_policy: OverlapPolicy,
    pub missed_run_policy: MissedRunPolicy,
}

impl Default for SchedulePolicy {
    fn default() -> Self {
        Self {
            runtime: RuntimePolicy {
                max_resident_subagents: 4,
                max_turns: 200,
                max_wall_time_secs: 900,
                read_only: true,
                allow_coding: false,
            },
            priority: SchedulePriority::Background,
            overlap_policy: OverlapPolicy::Skip,
            missed_run_policy: MissedRunPolicy::Skip,
        }
    }
}
