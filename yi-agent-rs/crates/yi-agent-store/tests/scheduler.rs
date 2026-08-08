use yi_agent_store::schedule::{OverlapPolicy, RuntimePolicy, SchedulePolicy};

#[test]
fn effective_policy_only_narrows_numeric_limits_and_capabilities() {
    let user = RuntimePolicy {
        max_resident_subagents: 16,
        max_turns: 100,
        read_only: false,
        allow_coding: true,
    };
    let project = RuntimePolicy {
        max_resident_subagents: 8,
        max_turns: 80,
        read_only: false,
        allow_coding: true,
    };
    let child = RuntimePolicy {
        max_resident_subagents: 32,
        max_turns: 120,
        read_only: false,
        allow_coding: true,
    };

    let effective = user.narrowed_by(&project).narrowed_by(&child);
    assert_eq!(effective.max_resident_subagents, 8);
    assert_eq!(effective.max_turns, 80);
    assert!(effective.allow_coding);
}

#[test]
fn scheduled_policy_defaults_to_read_only_background_without_overlap_or_catch_up() {
    let policy = SchedulePolicy::default();

    assert!(policy.runtime.read_only);
    assert!(!policy.runtime.allow_coding);
    assert_eq!(policy.runtime.max_resident_subagents, 4);
    assert_eq!(policy.runtime.max_turns, 30);
    assert_eq!(policy.overlap_policy, OverlapPolicy::Skip);
    assert!(!policy.catch_up);
}
