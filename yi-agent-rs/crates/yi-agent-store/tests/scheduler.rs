use yi_agent_store::schedule::{
    MissedRunPolicy, OverlapPolicy, RuntimePolicy, RuntimePolicyLayer, SchedulePolicy,
    SchedulePriority,
};

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
fn effective_policy_narrows_resource_and_retry_limits() {
    let user = RuntimePolicyLayer::from_toml(
        r#"
            [resources]
            max_llm_requests_per_provider_key = 8
            reserved_coordination_llm_requests = 1
            max_coding_agents = 6
            max_host_build_jobs = 2
            [attempt_defaults]
            max_provider_retries = 3
            max_tool_retries = 2
            max_rework_cycles = 2
        "#,
    )
    .unwrap();
    let project = RuntimePolicyLayer::from_toml(
        r#"
            [resources]
            max_llm_requests_per_provider_key = 5
            reserved_coordination_llm_requests = 2
            max_coding_agents = 4
            max_host_build_jobs = 1
            [attempt_defaults]
            max_provider_retries = 1
            max_tool_retries = 3
            max_rework_cycles = 1
        "#,
    )
    .unwrap();

    let effective = user.effective_with(&project);
    assert_eq!(effective.max_llm_requests_per_provider_key, 5);
    assert_eq!(effective.reserved_coordination_llm_requests, 1);
    assert_eq!(effective.max_coding_agents, 4);
    assert_eq!(effective.max_host_build_jobs, 1);
    assert_eq!(effective.max_provider_retries, 1);
    assert_eq!(effective.max_tool_retries, 2);
    assert_eq!(effective.max_rework_cycles, 1);
}

#[test]
fn coordination_reserve_is_clamped_to_the_effective_llm_total() {
    let user = RuntimePolicyLayer::from_toml(
        r#"
            [resources]
            max_llm_requests_per_provider_key = 8
            reserved_coordination_llm_requests = 1
        "#,
    )
    .unwrap();
    let project = RuntimePolicyLayer::from_toml(
        r#"
            [resources]
            max_llm_requests_per_provider_key = 0
        "#,
    )
    .unwrap();

    let effective = user.effective_with(&project);
    assert_eq!(effective.max_llm_requests_per_provider_key, 0);
    assert_eq!(effective.reserved_coordination_llm_requests, 0);
}

#[test]
fn effective_policy_only_narrows_numeric_limits_and_capabilities() {
    let user = RuntimePolicy {
        max_resident_subagents: 16,
        max_turns: 100,
        max_wall_time_secs: 2700,
        read_only: false,
        allow_coding: true,
    };
    let project = RuntimePolicy {
        max_resident_subagents: 8,
        max_turns: 80,
        max_wall_time_secs: 1800,
        read_only: false,
        allow_coding: true,
    };
    let child = RuntimePolicy {
        max_resident_subagents: 32,
        max_turns: 120,
        max_wall_time_secs: 3600,
        read_only: false,
        allow_coding: true,
    };

    let effective = user.narrowed_by(&project).narrowed_by(&child);
    assert_eq!(effective.max_resident_subagents, 8);
    assert_eq!(effective.max_turns, 80);
    assert_eq!(effective.max_wall_time_secs, 1800);
    assert!(effective.allow_coding);
}

#[test]
fn scheduled_policy_defaults_to_read_only_background_without_overlap_or_catch_up() {
    let policy = SchedulePolicy::default();

    assert!(policy.runtime.read_only);
    assert!(!policy.runtime.allow_coding);
    assert_eq!(policy.runtime.max_resident_subagents, 4);
    assert_eq!(policy.runtime.max_turns, 30);
    assert_eq!(policy.runtime.max_wall_time_secs, 900);
    assert_eq!(policy.overlap_policy, OverlapPolicy::Skip);
    assert_eq!(policy.priority, SchedulePriority::Background);
    assert_eq!(policy.missed_run_policy, MissedRunPolicy::Skip);
}

#[test]
fn schedule_defaults_parse_and_only_narrow_user_selection_and_global_limits() {
    let user = RuntimePolicyLayer::from_toml(
        r#"
            [runtime]
            max_resident_subagents = 8
            [attempt_defaults]
            max_turns = 40
            max_wall_time_secs = 600
            [schedule_defaults]
            max_resident_subagents = 6
            max_turns = 35
            max_wall_time_secs = 500
            priority = "background"
            read_only = true
            overlap_policy = "skip"
            missed_run_policy = "skip"
        "#,
    )
    .unwrap();
    let project = RuntimePolicyLayer::from_toml(
        r#"
            [schedule_defaults]
            max_resident_subagents = 12
            max_turns = 50
            max_wall_time_secs = 700
        "#,
    )
    .unwrap();
    let user_selection = SchedulePolicy {
        runtime: RuntimePolicy {
            max_resident_subagents: 5,
            max_turns: 32,
            max_wall_time_secs: 450,
            read_only: false,
            allow_coding: true,
        },
        priority: SchedulePriority::Normal,
        overlap_policy: OverlapPolicy::QueueOne,
        missed_run_policy: MissedRunPolicy::CatchUpOnce,
    };

    let effective = user.effective_schedule_with(&project, &user_selection);

    assert_eq!(effective.runtime.max_resident_subagents, 5);
    assert_eq!(effective.runtime.max_turns, 32);
    assert_eq!(effective.runtime.max_wall_time_secs, 450);
    assert!(effective.runtime.read_only);
    assert!(!effective.runtime.allow_coding);
    assert_eq!(effective.priority, SchedulePriority::Background);
    assert_eq!(effective.overlap_policy, OverlapPolicy::Skip);
    assert_eq!(effective.missed_run_policy, MissedRunPolicy::Skip);
}

#[test]
fn schedule_policy_is_clamped_to_effective_global_runtime_limits() {
    let user = RuntimePolicyLayer::from_toml(
        r#"
            [runtime]
            max_resident_subagents = 8
            [attempt_defaults]
            max_turns = 40
            max_wall_time_secs = 600
            [schedule_defaults]
            max_resident_subagents = 16
            max_turns = 100
            max_wall_time_secs = 1800
        "#,
    )
    .unwrap();
    let project = RuntimePolicyLayer::from_toml(
        r#"
            [schedule_defaults]
            max_resident_subagents = 12
            max_turns = 80
            max_wall_time_secs = 1200
        "#,
    )
    .unwrap();
    let selection = SchedulePolicy {
        runtime: RuntimePolicy {
            max_resident_subagents: 16,
            max_turns: 100,
            max_wall_time_secs: 1800,
            read_only: true,
            allow_coding: false,
        },
        ..SchedulePolicy::default()
    };

    let effective = user.effective_schedule_with(&project, &selection);

    assert_eq!(effective.runtime.max_resident_subagents, 8);
    assert_eq!(effective.runtime.max_turns, 40);
    assert_eq!(effective.runtime.max_wall_time_secs, 600);
}
