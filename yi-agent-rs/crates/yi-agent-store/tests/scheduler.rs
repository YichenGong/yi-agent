use chrono::{Duration, Local, Utc};
use tempfile::TempDir;
use yi_agent_core::subagent::task::{BudgetKind, TimeoutKind};
use yi_agent_store::repository::RuntimeRepository;
use yi_agent_store::schedule::{
    MissedRunPolicy, OverlapPolicy, RetryDecision, RetryFailure, RuntimePolicy, RuntimePolicyLayer,
    ScheduleDefinition, SchedulePolicy, SchedulePriority, WatchdogLimits, WatchdogObservation,
    WatchdogOutcome, WatchdogUsage, evaluate_retry, evaluate_watchdog,
};

#[test]
fn schedule_definition_requires_exactly_five_cron_fields() {
    let definition = ScheduleDefinition::new("0 9 * * 1-5", "inspect project tasks").unwrap();

    assert_eq!(definition.cron, "0 9 * * 1-5");
    assert_eq!(definition.objective, "inspect project tasks");
    assert!(ScheduleDefinition::new("0 0 9 * * 1-5", "inspect project tasks").is_err());
    assert!(ScheduleDefinition::new("0 9 * *", "inspect project tasks").is_err());
    assert!(ScheduleDefinition::new("0 9 * * 1-5", "   ").is_err());
}

#[test]
fn schedule_definition_embeds_conservative_defaults() {
    let definition = ScheduleDefinition::new("0 9 * * 1-5", "inspect project tasks").unwrap();

    assert_eq!(definition.policy, SchedulePolicy::default());
    assert!(definition.policy.runtime.read_only);
    assert!(!definition.policy.runtime.allow_coding);
    assert_eq!(definition.policy.runtime.max_resident_subagents, 4);
    assert_eq!(definition.policy.runtime.max_turns, 200);
    assert_eq!(definition.policy.runtime.max_wall_time_secs, 900);
    assert_eq!(definition.policy.priority, SchedulePriority::Background);
    assert_eq!(definition.policy.overlap_policy, OverlapPolicy::Skip);
    assert_eq!(definition.policy.missed_run_policy, MissedRunPolicy::Skip);
}

#[test]
fn schedule_occurrence_claim_is_durable_and_idempotent() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let definition = ScheduleDefinition::new("0 9 * * 1-5", "inspect project tasks").unwrap();
    let due_at = Local::now() + Duration::minutes(5);

    let schedule = RuntimeRepository::open(&database)
        .unwrap()
        .create_schedule(&definition, due_at)
        .unwrap();
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .claim_schedule_occurrence(&schedule.id, due_at)
            .unwrap()
    );
    assert!(
        !RuntimeRepository::open(&database)
            .unwrap()
            .claim_schedule_occurrence(&schedule.id, due_at)
            .unwrap()
    );

    let schedules = RuntimeRepository::open(&database)
        .unwrap()
        .schedules()
        .unwrap();
    assert_eq!(schedules, vec![schedule]);
}

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
    assert_eq!(policy.runtime.max_turns, 200);
    assert_eq!(policy.runtime.max_wall_time_secs, 900);
    assert_eq!(policy.overlap_policy, OverlapPolicy::Skip);
    assert_eq!(policy.priority, SchedulePriority::Background);
    assert_eq!(policy.missed_run_policy, MissedRunPolicy::Skip);
}

#[test]
fn retry_policy_only_retries_explicit_transient_failures_with_bounded_backoff() {
    assert_eq!(
        evaluate_retry(RetryFailure::ProviderNetwork, 0, 3, 137),
        RetryDecision::RetryAfter(std::time::Duration::from_millis(1_137))
    );
    assert_eq!(
        evaluate_retry(RetryFailure::ProviderRateLimited, 2, 3, 999),
        RetryDecision::RetryAfter(std::time::Duration::from_millis(4_999))
    );
    assert_eq!(
        evaluate_retry(RetryFailure::ProviderServer, 3, 3, 1),
        RetryDecision::Failed
    );
    assert_eq!(
        evaluate_retry(
            RetryFailure::Tool {
                explicitly_retryable: true,
            },
            1,
            2,
            0,
        ),
        RetryDecision::RetryAfter(std::time::Duration::from_secs(2))
    );
    assert_eq!(
        evaluate_retry(
            RetryFailure::Tool {
                explicitly_retryable: false,
            },
            0,
            2,
            0,
        ),
        RetryDecision::Failed
    );
    for failure in [RetryFailure::GitConflict, RetryFailure::TestFailure] {
        assert_eq!(evaluate_retry(failure, 0, 3, 0), RetryDecision::Failed);
    }
    for failure in [
        RetryFailure::PermissionDenied,
        RetryFailure::AuthorityDenied,
        RetryFailure::ScopeAmbiguity,
    ] {
        assert_eq!(evaluate_retry(failure, 0, 3, 0), RetryDecision::Blocked);
    }
    assert_eq!(
        evaluate_retry(RetryFailure::ProviderStream, 10, 20, 500),
        RetryDecision::RetryAfter(std::time::Duration::from_secs(30))
    );
}

#[test]
fn explicit_schedule_selection_is_only_narrowed_by_project_and_global_limits() {
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
    assert!(!effective.runtime.read_only);
    assert!(effective.runtime.allow_coding);
    assert_eq!(effective.priority, SchedulePriority::Normal);
    assert_eq!(effective.overlap_policy, OverlapPolicy::QueueOne);
    assert_eq!(effective.missed_run_policy, MissedRunPolicy::CatchUpOnce);
}

#[test]
fn schedule_defaults_seed_a_conservative_policy_without_an_explicit_selection() {
    let user = RuntimePolicyLayer::from_toml(
        r#"
            [schedule_defaults]
            max_resident_subagents = 3
            max_turns = 20
            max_wall_time_secs = 600
            priority = "background"
            read_only = true
            overlap_policy = "skip"
            missed_run_policy = "skip"
        "#,
    )
    .unwrap();

    let effective = user.default_schedule_policy_with(&RuntimePolicyLayer::default());

    assert_eq!(effective.runtime.max_resident_subagents, 3);
    assert_eq!(effective.runtime.max_turns, 20);
    assert_eq!(effective.runtime.max_wall_time_secs, 600);
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

#[test]
fn explicit_user_schedule_selection_can_exceed_user_schedule_defaults() {
    let user = RuntimePolicyLayer::from_toml(
        r#"
            [runtime]
            max_resident_subagents = 8
            [attempt_defaults]
            max_turns = 100
            max_wall_time_secs = 3600
            [schedule_defaults]
            max_resident_subagents = 4
            max_turns = 30
            max_wall_time_secs = 900
        "#,
    )
    .unwrap();
    let project = RuntimePolicyLayer::default();
    let selection = SchedulePolicy {
        runtime: RuntimePolicy {
            max_resident_subagents: 6,
            max_turns: 40,
            max_wall_time_secs: 1200,
            read_only: false,
            allow_coding: true,
        },
        priority: SchedulePriority::Normal,
        overlap_policy: OverlapPolicy::QueueOne,
        missed_run_policy: MissedRunPolicy::CatchUpOnce,
    };

    let effective = user.effective_schedule_with(&project, &selection);

    assert_eq!(effective.runtime.max_resident_subagents, 6);
    assert_eq!(effective.runtime.max_turns, 40);
    assert_eq!(effective.runtime.max_wall_time_secs, 1200);
}

#[test]
fn schedule_selection_intersects_global_project_capabilities() {
    let user = RuntimePolicyLayer::from_toml(
        r#"
            [runtime]
            allow_coding = true
        "#,
    )
    .unwrap();
    let read_only_project = RuntimePolicyLayer::from_toml(
        r#"
            [runtime]
            read_only = true
        "#,
    )
    .unwrap();
    let no_coding_project = RuntimePolicyLayer::from_toml(
        r#"
            [runtime]
            allow_coding = false
        "#,
    )
    .unwrap();
    let selection = SchedulePolicy {
        runtime: RuntimePolicy {
            max_resident_subagents: 1,
            max_turns: 1,
            max_wall_time_secs: 1,
            read_only: false,
            allow_coding: true,
        },
        priority: SchedulePriority::Normal,
        overlap_policy: OverlapPolicy::QueueOne,
        missed_run_policy: MissedRunPolicy::CatchUpOnce,
    };

    let read_only = user.effective_schedule_with(&read_only_project, &selection);
    assert!(read_only.runtime.read_only);
    assert!(!read_only.runtime.allow_coding);

    let no_coding = user.effective_schedule_with(&no_coding_project, &selection);
    assert!(!no_coding.runtime.read_only);
    assert!(!no_coding.runtime.allow_coding);
}

#[test]
fn watchdog_uses_persisted_progress_and_resource_queue_time() {
    let now = Utc::now();
    let limits = WatchdogLimits {
        max_turns: Some(30),
        max_tokens: Some(1_000),
        max_cost_micros: None,
        max_wall_time_secs: Some(900),
        max_idle_time_secs: Some(300),
        max_resource_wait_secs: Some(60),
        max_provider_retries: Some(3),
        max_tool_retries: Some(2),
        max_rework_cycles: Some(2),
    };
    let observation = WatchdogObservation {
        attempt_started_at: now - Duration::seconds(30),
        last_meaningful_at: now - Duration::seconds(1),
        resource_wait_started_at: Some(now - Duration::seconds(61)),
        usage: WatchdogUsage {
            turns: 1,
            tokens: 10,
            cost_micros: 0,
            provider_retries: 0,
            tool_retries: 0,
            rework_cycles: 0,
        },
    };

    assert_eq!(
        evaluate_watchdog(&limits, &observation, now),
        Some(WatchdogOutcome::TimedOut(TimeoutKind::Deadline))
    );
}

#[test]
fn watchdog_classifies_turn_and_token_budgets_without_waiting_for_wall_clock() {
    let now = Utc::now();
    let limits = WatchdogLimits {
        max_turns: Some(30),
        max_tokens: Some(1_000),
        max_cost_micros: None,
        max_wall_time_secs: Some(900),
        max_idle_time_secs: Some(300),
        max_resource_wait_secs: None,
        max_provider_retries: Some(3),
        max_tool_retries: Some(2),
        max_rework_cycles: Some(2),
    };
    let observation = WatchdogObservation {
        attempt_started_at: now,
        last_meaningful_at: now,
        resource_wait_started_at: None,
        usage: WatchdogUsage {
            turns: 30,
            tokens: 1_000,
            cost_micros: 0,
            provider_retries: 0,
            tool_retries: 0,
            rework_cycles: 0,
        },
    };

    assert_eq!(
        evaluate_watchdog(&limits, &observation, now),
        Some(WatchdogOutcome::BudgetExhausted(BudgetKind::Turns))
    );

    let mut token_observation = observation;
    token_observation.usage.turns = 29;
    assert_eq!(
        evaluate_watchdog(&limits, &token_observation, now),
        Some(WatchdogOutcome::BudgetExhausted(BudgetKind::Tokens))
    );
}
