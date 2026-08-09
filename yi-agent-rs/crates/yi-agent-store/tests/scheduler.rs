use yi_agent_store::schedule::{OverlapPolicy, RuntimePolicy, RuntimePolicyLayer, SchedulePolicy};

#[test]
fn toml_policy_layers_can_only_narrow_the_user_ceiling() {
    let user = RuntimePolicyLayer::from_toml(
        r#"
            [runtime]
            max_resident_subagents = 16
            max_queued_subagents = 64
            max_depth = 2
            max_direct_children_per_agent = 4
            read_only = false
            allow_coding = true
            [attempt_defaults]
            max_turns = 100
            max_wall_time_secs = 2700
            max_idle_time_secs = 300
        "#,
    )
    .unwrap();
    let project = RuntimePolicyLayer::from_toml(
        r#"
            [runtime]
            max_resident_subagents = 8
            max_queued_subagents = 128
            read_only = true
            allow_coding = true
            [attempt_defaults]
            max_turns = 120
            max_wall_time_secs = 900
        "#,
    )
    .unwrap();

    let effective = user.effective_with(&project);
    assert_eq!(effective.max_resident_subagents, 8);
    assert_eq!(effective.max_queued_subagents, 64);
    assert_eq!(effective.max_turns, 100);
    assert_eq!(effective.max_wall_time_secs, 900);
    assert_eq!(effective.max_idle_time_secs, 300);
    assert!(effective.read_only);
    assert!(!effective.allow_coding);
}

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
