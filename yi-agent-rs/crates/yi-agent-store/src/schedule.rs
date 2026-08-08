//! Effective runtime and scheduled-session policy values.

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
