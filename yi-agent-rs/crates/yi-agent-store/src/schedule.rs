//! Effective runtime and scheduled-session policy values.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use yi_agent_core::subagent::task::{BudgetKind, TimeoutKind};

/// Immutable limits applied by the daemon to one attempt. Optional ceilings
/// remain unbounded until configured by the user or a narrower project policy.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

/// A user, project, root, or child policy layer. Missing values inherit from
/// the broader layer; a narrower layer can never raise a numeric ceiling.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RuntimePolicyLayer {
    #[serde(default)]
    runtime: RuntimeLimitsLayer,
    #[serde(default)]
    resources: ResourceLimitsLayer,
    #[serde(default)]
    attempt_defaults: AttemptLimitsLayer,
    #[serde(default)]
    schedule_defaults: ScheduleDefaultsLayer,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RuntimeLimitsLayer {
    max_resident_subagents: Option<u16>,
    max_queued_subagents: Option<u16>,
    max_depth: Option<u8>,
    max_direct_children_per_agent: Option<u8>,
    read_only: Option<bool>,
    allow_coding: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ResourceLimitsLayer {
    max_llm_requests_per_provider_key: Option<u16>,
    reserved_coordination_llm_requests: Option<u16>,
    max_coding_agents: Option<u16>,
    max_host_build_jobs: Option<u16>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AttemptLimitsLayer {
    max_turns: Option<u32>,
    max_wall_time_secs: Option<u64>,
    max_idle_time_secs: Option<u64>,
    max_provider_retries: Option<u16>,
    max_tool_retries: Option<u16>,
    max_rework_cycles: Option<u16>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ScheduleDefaultsLayer {
    max_resident_subagents: Option<u16>,
    max_turns: Option<u32>,
    max_wall_time_secs: Option<u64>,
    priority: Option<SchedulePriority>,
    read_only: Option<bool>,
    overlap_policy: Option<OverlapPolicy>,
    missed_run_policy: Option<MissedRunPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRuntimePolicy {
    pub max_resident_subagents: u16,
    pub max_queued_subagents: u16,
    pub max_depth: u8,
    pub max_direct_children_per_agent: u8,
    pub max_turns: u32,
    pub max_wall_time_secs: u64,
    pub max_idle_time_secs: u64,
    pub max_llm_requests_per_provider_key: u16,
    pub reserved_coordination_llm_requests: u16,
    pub max_coding_agents: u16,
    pub max_host_build_jobs: u16,
    pub max_provider_retries: u16,
    pub max_tool_retries: u16,
    pub max_rework_cycles: u16,
    pub read_only: bool,
    pub allow_coding: bool,
}

impl RuntimePolicyLayer {
    pub fn from_toml(input: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(input)
    }

    pub fn effective_with(&self, narrower: &Self) -> EffectiveRuntimePolicy {
        let defaults = EffectiveRuntimePolicy {
            max_resident_subagents: 16,
            max_queued_subagents: 64,
            max_depth: 2,
            max_direct_children_per_agent: 4,
            max_turns: 100,
            max_wall_time_secs: 2700,
            max_idle_time_secs: 300,
            max_llm_requests_per_provider_key: 8,
            reserved_coordination_llm_requests: 1,
            max_coding_agents: 6,
            max_host_build_jobs: 2,
            max_provider_retries: 3,
            max_tool_retries: 2,
            max_rework_cycles: 2,
            read_only: false,
            allow_coding: true,
        };
        let max_llm_requests_per_provider_key = narrow(
            defaults.max_llm_requests_per_provider_key,
            self.resources.max_llm_requests_per_provider_key,
            narrower.resources.max_llm_requests_per_provider_key,
        );
        EffectiveRuntimePolicy {
            max_resident_subagents: narrow(
                defaults.max_resident_subagents,
                self.runtime.max_resident_subagents,
                narrower.runtime.max_resident_subagents,
            ),
            max_queued_subagents: narrow(
                defaults.max_queued_subagents,
                self.runtime.max_queued_subagents,
                narrower.runtime.max_queued_subagents,
            ),
            max_depth: narrow(
                defaults.max_depth,
                self.runtime.max_depth,
                narrower.runtime.max_depth,
            ),
            max_direct_children_per_agent: narrow(
                defaults.max_direct_children_per_agent,
                self.runtime.max_direct_children_per_agent,
                narrower.runtime.max_direct_children_per_agent,
            ),
            max_turns: narrow(
                defaults.max_turns,
                self.attempt_defaults.max_turns,
                narrower.attempt_defaults.max_turns,
            ),
            max_wall_time_secs: narrow(
                defaults.max_wall_time_secs,
                self.attempt_defaults.max_wall_time_secs,
                narrower.attempt_defaults.max_wall_time_secs,
            ),
            max_idle_time_secs: narrow(
                defaults.max_idle_time_secs,
                self.attempt_defaults.max_idle_time_secs,
                narrower.attempt_defaults.max_idle_time_secs,
            ),
            max_llm_requests_per_provider_key,
            reserved_coordination_llm_requests: narrow(
                defaults.reserved_coordination_llm_requests,
                self.resources.reserved_coordination_llm_requests,
                narrower.resources.reserved_coordination_llm_requests,
            )
            .min(max_llm_requests_per_provider_key),
            max_coding_agents: narrow(
                defaults.max_coding_agents,
                self.resources.max_coding_agents,
                narrower.resources.max_coding_agents,
            ),
            max_host_build_jobs: narrow(
                defaults.max_host_build_jobs,
                self.resources.max_host_build_jobs,
                narrower.resources.max_host_build_jobs,
            ),
            max_provider_retries: narrow(
                defaults.max_provider_retries,
                self.attempt_defaults.max_provider_retries,
                narrower.attempt_defaults.max_provider_retries,
            ),
            max_tool_retries: narrow(
                defaults.max_tool_retries,
                self.attempt_defaults.max_tool_retries,
                narrower.attempt_defaults.max_tool_retries,
            ),
            max_rework_cycles: narrow(
                defaults.max_rework_cycles,
                self.attempt_defaults.max_rework_cycles,
                narrower.attempt_defaults.max_rework_cycles,
            ),
            read_only: self.runtime.read_only.unwrap_or(defaults.read_only)
                || narrower.runtime.read_only.unwrap_or(false),
            allow_coding: self.runtime.allow_coding.unwrap_or(defaults.allow_coding)
                && narrower
                    .runtime
                    .allow_coding
                    .unwrap_or(defaults.allow_coding)
                && !(self.runtime.read_only.unwrap_or(defaults.read_only)
                    || narrower.runtime.read_only.unwrap_or(false)),
        }
    }

    /// Resolves a scheduled root selection against user, project, and global
    /// runtime limits. Project schedule defaults can only further restrict it.
    pub fn effective_schedule_with(
        &self,
        project: &Self,
        selection: &SchedulePolicy,
    ) -> SchedulePolicy {
        let global = self.effective_with(project);
        let project = &project.schedule_defaults;
        let read_only =
            global.read_only || selection.runtime.read_only || project.read_only.unwrap_or(false);

        SchedulePolicy {
            runtime: RuntimePolicy {
                max_resident_subagents: selection
                    .runtime
                    .max_resident_subagents
                    .min(global.max_resident_subagents)
                    .min(project.max_resident_subagents.unwrap_or(u16::MAX)),
                max_turns: selection
                    .runtime
                    .max_turns
                    .min(global.max_turns)
                    .min(project.max_turns.unwrap_or(u32::MAX)),
                max_wall_time_secs: selection
                    .runtime
                    .max_wall_time_secs
                    .min(global.max_wall_time_secs)
                    .min(project.max_wall_time_secs.unwrap_or(u64::MAX)),
                read_only,
                allow_coding: global.allow_coding && selection.runtime.allow_coding && !read_only,
            },
            priority: selection
                .priority
                .min(project.priority.unwrap_or(SchedulePriority::Critical)),
            overlap_policy: selection
                .overlap_policy
                .min(project.overlap_policy.unwrap_or(OverlapPolicy::QueueOne)),
            missed_run_policy: selection.missed_run_policy.min(
                project
                    .missed_run_policy
                    .unwrap_or(MissedRunPolicy::CatchUpOnce),
            ),
        }
    }

    /// Builds the policy for a schedule without an explicit root selection.
    /// User schedule defaults seed that selection; project settings then only
    /// narrow it through the normal effective-policy path.
    pub fn default_schedule_policy_with(&self, project: &Self) -> SchedulePolicy {
        let defaults = &self.schedule_defaults;
        let mut policy = SchedulePolicy::default();
        if let Some(value) = defaults.max_resident_subagents {
            policy.runtime.max_resident_subagents = value;
        }
        if let Some(value) = defaults.max_turns {
            policy.runtime.max_turns = value;
        }
        if let Some(value) = defaults.max_wall_time_secs {
            policy.runtime.max_wall_time_secs = value;
        }
        if let Some(value) = defaults.priority {
            policy.priority = value;
        }
        if let Some(value) = defaults.read_only {
            policy.runtime.read_only = value;
            if value {
                policy.runtime.allow_coding = false;
            }
        }
        if let Some(value) = defaults.overlap_policy {
            policy.overlap_policy = value;
        }
        if let Some(value) = defaults.missed_run_policy {
            policy.missed_run_policy = value;
        }
        self.effective_schedule_with(project, &policy)
    }
}

fn narrow<T: Ord + Copy>(default: T, broader: Option<T>, narrower: Option<T>) -> T {
    narrower.unwrap_or(default).min(broader.unwrap_or(default))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePolicy {
    pub max_resident_subagents: u16,
    pub max_turns: u32,
    pub max_wall_time_secs: u64,
    pub read_only: bool,
    pub allow_coding: bool,
}

impl RuntimePolicy {
    /// Combine a lower-precedence policy with a more restrictive layer.
    pub fn narrowed_by(&self, narrower: &Self) -> Self {
        let read_only = self.read_only || narrower.read_only;
        Self {
            max_resident_subagents: self
                .max_resident_subagents
                .min(narrower.max_resident_subagents),
            max_turns: self.max_turns.min(narrower.max_turns),
            max_wall_time_secs: self.max_wall_time_secs.min(narrower.max_wall_time_secs),
            read_only,
            allow_coding: self.allow_coding && narrower.allow_coding && !read_only,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulePriority {
    Background,
    Normal,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapPolicy {
    Skip,
    QueueOne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissedRunPolicy {
    Skip,
    CatchUpOnce,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
                max_turns: 30,
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
