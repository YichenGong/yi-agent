use chrono::{Duration, Utc};
use yi_agent_core::subagent::scheduler::{
    AdmissionPriority, LeaseMode, ResourceCoordinator, ResourceRequest, ResourceScope,
};
use yi_agent_core::subagent::task::{RootSessionId, TaskId, TaskState};
use yi_agent_core::{AgentTask, AttemptId, TaskEvent};

fn request(
    key: &str,
    mode: LeaseMode,
    deadline: Option<chrono::DateTime<chrono::Utc>>,
) -> ResourceRequest {
    ResourceRequest {
        scope: ResourceScope::Global,
        key: key.into(),
        mode,
        units: 1,
        deadline,
    }
}

#[test]
fn reducer_replay_is_deterministic_and_rejections_are_atomic() {
    let root = RootSessionId::new();
    let parent = TaskId::new();
    let mut left = AgentTask::new_child(root.clone(), parent.clone());
    let mut right = AgentTask::new_child(root, parent);
    // The independently-created tasks have distinct IDs, so replay identity
    // is checked through state/attempt shape rather than full UUID equality.
    let now = Utc::now();
    for task in [&mut left, &mut right] {
        let attempt = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: attempt.clone(),
            },
            now,
        )
        .unwrap();
        task.reduce(
            TaskEvent::WorkerCompletedNoChanges {
                attempt_id: attempt.clone(),
            },
            now,
        )
        .unwrap();
        let result = task
            .reduce(
                TaskEvent::RetryRequested {
                    attempt_id: attempt,
                },
                now,
            )
            .unwrap();
        assert!(result.new_attempt.is_some());
        assert_eq!(task.state(), &TaskState::Queued);
        assert_eq!(task.attempts().len(), 2);
    }
    assert_eq!(left.state(), right.state());
    assert_eq!(left.attempts().len(), right.attempts().len());

    let before = left.clone();
    let stale = AttemptId::new();
    assert!(
        left.reduce(TaskEvent::AdmissionGranted { attempt_id: stale }, now)
            .is_err()
    );
    assert_eq!(left, before);
}

#[test]
fn scheduler_sequence_never_grants_conflicting_or_expired_work() {
    let mut coordinator = ResourceCoordinator::new();
    let now = Utc::now();
    coordinator.set_capacity("workspace:target", 1);
    let root_a = RootSessionId::new();
    let root_b = RootSessionId::new();
    let a = TaskId::new();
    let b = TaskId::new();
    let expired = TaskId::new();

    coordinator.enqueue_with_priority_at(
        root_a.clone(),
        a.clone(),
        request("workspace:target", LeaseMode::Exclusive, None),
        AdmissionPriority::Normal,
        now,
    );
    coordinator.enqueue_with_priority_at(
        root_b.clone(),
        b.clone(),
        request("workspace:target", LeaseMode::Shared, None),
        AdmissionPriority::Normal,
        now,
    );
    coordinator.enqueue_with_priority_at(
        root_b,
        expired.clone(),
        request(
            "workspace:target",
            LeaseMode::Shared,
            Some(now - Duration::seconds(1)),
        ),
        AdmissionPriority::High,
        now,
    );

    let first = coordinator.grant_next_at("workspace:target", now).unwrap();
    assert_eq!(first.task_id, a);
    assert!(coordinator.grant_next_at("workspace:target", now).is_none());
    coordinator.release(first.lease_id.clone()).unwrap();
    coordinator.release(first.lease_id).unwrap();
    let second = coordinator.grant_next_at("workspace:target", now).unwrap();
    assert_eq!(second.task_id, b);
    coordinator.release(second.lease_id).unwrap();
    assert!(coordinator.grant_next_at("workspace:target", now).is_none());
    assert_eq!(coordinator.cancel_task_requests(&expired), 0);
}
