//! Effective runtime and scheduled-session policy values.

use serde::Deserialize;

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
        let user = &self.schedule_defaults;
        let project = &project.schedule_defaults;
        let read_only = selection.runtime.read_only
            || user.read_only.unwrap_or(false)
            || project.read_only.unwrap_or(false);

        SchedulePolicy {
            runtime: RuntimePolicy {
                max_resident_subagents: selection
                    .runtime
                    .max_resident_subagents
                    .min(global.max_resident_subagents)
                    .min(user.max_resident_subagents.unwrap_or(u16::MAX))
                    .min(project.max_resident_subagents.unwrap_or(u16::MAX)),
                max_turns: selection
                    .runtime
                    .max_turns
                    .min(global.max_turns)
                    .min(user.max_turns.unwrap_or(u32::MAX))
                    .min(project.max_turns.unwrap_or(u32::MAX)),
                max_wall_time_secs: selection
                    .runtime
                    .max_wall_time_secs
                    .min(global.max_wall_time_secs)
                    .min(user.max_wall_time_secs.unwrap_or(u64::MAX))
                    .min(project.max_wall_time_secs.unwrap_or(u64::MAX)),
                read_only,
                allow_coding: selection.runtime.allow_coding && !read_only,
            },
            priority: selection
                .priority
                .min(user.priority.unwrap_or(SchedulePriority::Critical))
                .min(project.priority.unwrap_or(SchedulePriority::Critical)),
            overlap_policy: selection
                .overlap_policy
                .min(user.overlap_policy.unwrap_or(OverlapPolicy::QueueOne))
                .min(project.overlap_policy.unwrap_or(OverlapPolicy::QueueOne)),
            missed_run_policy: selection
                .missed_run_policy
                .min(
                    user.missed_run_policy
                        .unwrap_or(MissedRunPolicy::CatchUpOnce),
                )
                .min(
                    project
                        .missed_run_policy
                        .unwrap_or(MissedRunPolicy::CatchUpOnce),
                ),
        }
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
