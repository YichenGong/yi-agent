//! Effective runtime and scheduled-session policy values.

use serde::Deserialize;

/// A user, project, root, or child policy layer. Missing values inherit from
/// the broader layer; a narrower layer can never raise a numeric ceiling.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RuntimePolicyLayer {
    #[serde(default)]
    runtime: RuntimeLimitsLayer,
    #[serde(default)]
    attempt_defaults: AttemptLimitsLayer,
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
struct AttemptLimitsLayer {
    max_turns: Option<u32>,
    max_wall_time_secs: Option<u64>,
    max_idle_time_secs: Option<u64>,
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
            read_only: false,
            allow_coding: true,
        };
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
}

fn narrow<T: Ord + Copy>(default: T, broader: Option<T>, narrower: Option<T>) -> T {
    narrower.unwrap_or(default).min(broader.unwrap_or(default))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePolicy {
    pub max_resident_subagents: u16,
    pub max_turns: u32,
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
            read_only,
            allow_coding: self.allow_coding && narrower.allow_coding && !read_only,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlapPolicy {
    Skip,
    QueueOne,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulePolicy {
    pub runtime: RuntimePolicy,
    pub overlap_policy: OverlapPolicy,
    pub catch_up: bool,
}

impl Default for SchedulePolicy {
    fn default() -> Self {
        Self {
            runtime: RuntimePolicy {
                max_resident_subagents: 4,
                max_turns: 30,
                read_only: true,
                allow_coding: false,
            },
            overlap_policy: OverlapPolicy::Skip,
            catch_up: false,
        }
    }
}
