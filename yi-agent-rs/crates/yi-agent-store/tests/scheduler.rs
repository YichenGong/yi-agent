use chrono::{Duration, Local, Utc};
use tempfile::TempDir;
use yi_agent_core::subagent::task::{BudgetKind, TimeoutKind};
use yi_agent_store::repository::RuntimeRepository;
use yi_agent_store::schedule::{
    MissedRunPolicy, OverlapPolicy, RetryDecision, RetryFailure, ScheduleDefinition, SchedulePolicy,
    SchedulePriority, WatchdogLimits, WatchdogObservation, WatchdogOutcome, WatchdogUsage,
    evaluate_retry, evaluate_watchdog,
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
