use chrono::{Duration, Utc};
use yi_agent_core::subagent::scheduler::{
    AdmissionError, AdmissionPriority, LeaseMode, ResourceCoordinator, ResourceRequest,
    ResourceScope,
};

#[test]
fn coordinator_exposes_the_confirmed_global_default_capacities() {
    let coordinator = ResourceCoordinator::new();

    assert_eq!(coordinator.capacity("resident:global"), Some(16));
    assert_eq!(coordinator.capacity("coding:global"), Some(6));
    assert_eq!(coordinator.capacity("build:host"), Some(2));
    assert_eq!(ResourceCoordinator::DEFAULT_LLM_PER_PROVIDER_KEY, 8);
    assert_eq!(ResourceCoordinator::RESERVED_COORDINATION_LLM_PERMITS, 1);
}
use yi_agent_core::subagent::task::{RootSessionId, TaskId};

fn request(key: &str) -> ResourceRequest {
    ResourceRequest {
        scope: ResourceScope::Global,
        key: key.into(),
        mode: LeaseMode::Exclusive,
        units: 1,
        deadline: None,
    }
}

fn shared_request(key: &str, units: u16) -> ResourceRequest {
    ResourceRequest {
        scope: ResourceScope::Global,
        key: key.into(),
        mode: LeaseMode::Shared,
        units,
        deadline: None,
    }
}

#[test]
fn roots_take_turns_when_contending_for_a_permit() {
    let mut coordinator = ResourceCoordinator::new();
    coordinator.set_capacity("resident:global", 1);
    let root_a = RootSessionId::new();
    let root_b = RootSessionId::new();
    let a1 = TaskId::new();
    let a2 = TaskId::new();
    let b1 = TaskId::new();
    let now = Utc::now();

    coordinator.enqueue_with_priority_at(
        root_a.clone(),
        a1.clone(),
        request("resident:global"),
        AdmissionPriority::Normal,
        now,
    );
    coordinator.enqueue_with_priority_at(
        root_a.clone(),
        a2,
        request("resident:global"),
        AdmissionPriority::Normal,
        now,
    );
    coordinator.enqueue_with_priority_at(
        root_b.clone(),
        b1.clone(),
        request("resident:global"),
        AdmissionPriority::Normal,
        now,
    );
    let first = coordinator.grant_next_at("resident:global", now).unwrap();
    assert_eq!(first.task_id, a1);
    coordinator.release(first.lease_id).unwrap();

    let second = coordinator.grant_next_at("resident:global", now).unwrap();
    assert_eq!(second.task_id, b1);
}

#[test]
fn direct_parent_subtrees_take_turns_within_the_same_root() {
    let mut coordinator = ResourceCoordinator::new();
    coordinator.set_capacity("resident:global", 1);
    let root = RootSessionId::new();
    let parent_a = TaskId::new();
    let parent_b = TaskId::new();
    let a1 = TaskId::new();
    let b1 = TaskId::new();
    let a2 = TaskId::new();
    let now = Utc::now();

    coordinator.enqueue_with_priority_for_parent_at(
        root.clone(),
        parent_a.clone(),
        a1.clone(),
        request("resident:global"),
        AdmissionPriority::Normal,
        now,
    );
    coordinator.enqueue_with_priority_for_parent_at(
        root.clone(),
        parent_b.clone(),
        b1.clone(),
        request("resident:global"),
        AdmissionPriority::Normal,
        now,
    );
    coordinator.enqueue_with_priority_for_parent_at(
        root,
        parent_a,
        a2.clone(),
        request("resident:global"),
        AdmissionPriority::Normal,
        now,
    );

    let first = coordinator.grant_next_at("resident:global", now).unwrap();
    assert_eq!(first.task_id, a1);
    coordinator.release(first.lease_id).unwrap();

    let second = coordinator.grant_next_at("resident:global", now).unwrap();
    assert_eq!(second.task_id, b1);
    coordinator.release(second.lease_id).unwrap();

    assert_eq!(
        coordinator
            .grant_next_at("resident:global", now)
            .unwrap()
            .task_id,
        a2
    );
}

#[test]
fn outstanding_root_permits_softly_penalize_its_next_request() {
    let mut coordinator = ResourceCoordinator::new();
    coordinator.set_capacity("resident:global", 2);
    let root_a = RootSessionId::new();
    let root_b = RootSessionId::new();
    let blocking_root = RootSessionId::new();
    let now = Utc::now();

    coordinator.enqueue_with_priority_at(
        root_a.clone(),
        TaskId::new(),
        shared_request("resident:global", 1),
        AdmissionPriority::Normal,
        now,
    );
    let held = coordinator.grant_next_at("resident:global", now).unwrap();
    coordinator.enqueue_with_priority_at(
        blocking_root,
        TaskId::new(),
        shared_request("resident:global", 1),
        AdmissionPriority::Normal,
        now,
    );
    let blocker = coordinator.grant_next_at("resident:global", now).unwrap();
    let a_waiting = TaskId::new();
    let b_waiting = TaskId::new();
    coordinator.enqueue_with_priority_at(
        root_a,
        a_waiting,
        shared_request("resident:global", 1),
        AdmissionPriority::Normal,
        now,
    );
    coordinator.enqueue_with_priority_at(
        root_b,
        b_waiting.clone(),
        shared_request("resident:global", 1),
        AdmissionPriority::Normal,
        now,
    );

    coordinator.release(blocker.lease_id).unwrap();
    assert_eq!(
        coordinator
            .grant_next_at("resident:global", now)
            .unwrap()
            .task_id,
        b_waiting
    );
    coordinator.release(held.lease_id).unwrap();
}

#[test]
fn same_parent_requests_preserve_fifo_sequence_order() {
    let mut coordinator = ResourceCoordinator::new();
    coordinator.set_capacity("resident:global", 1);
    let root = RootSessionId::new();
    let parent = TaskId::new();
    let first_task = TaskId::new();
    let second_task = TaskId::new();
    let now = Utc::now();

    coordinator.enqueue_with_priority_for_parent_at(
        root.clone(),
        parent.clone(),
        first_task.clone(),
        request("resident:global"),
        AdmissionPriority::Normal,
        now,
    );
    coordinator.enqueue_with_priority_for_parent_at(
        root,
        parent,
        second_task.clone(),
        request("resident:global"),
        AdmissionPriority::Normal,
        now,
    );

    let first = coordinator.grant_next_at("resident:global", now).unwrap();
    assert_eq!(first.task_id, first_task);
    coordinator.release(first.lease_id).unwrap();
    assert_eq!(
        coordinator
            .grant_next_at("resident:global", now)
            .unwrap()
            .task_id,
        second_task
    );
}

#[test]
fn exclusive_workspace_lease_rejects_a_second_holder_until_release() {
    let mut coordinator = ResourceCoordinator::new();
    coordinator.set_capacity("cargo:/repo", 1);
    let root = RootSessionId::new();
    coordinator.enqueue(root.clone(), TaskId::new(), request("cargo:/repo"));
    coordinator.enqueue(root, TaskId::new(), request("cargo:/repo"));

    let lease = coordinator.grant_next("cargo:/repo").unwrap();
    assert!(coordinator.grant_next("cargo:/repo").is_none());
    coordinator.release(lease.lease_id).unwrap();
    assert!(coordinator.grant_next("cargo:/repo").is_some());
}

#[test]
fn resource_units_never_exceed_the_configured_capacity() {
    let mut coordinator = ResourceCoordinator::new();
    coordinator.set_capacity("build:host", 3);
    let root = RootSessionId::new();
    coordinator.enqueue(root.clone(), TaskId::new(), shared_request("build:host", 2));
    coordinator.enqueue(root, TaskId::new(), shared_request("build:host", 2));

    assert!(coordinator.grant_next("build:host").is_some());
    assert!(coordinator.grant_next("build:host").is_none());
}

#[test]
fn exclusive_lease_blocks_shared_holders_even_when_capacity_remains() {
    let mut coordinator = ResourceCoordinator::new();
    coordinator.set_capacity("git-integrate:/repo:main", 2);
    let root = RootSessionId::new();
    coordinator.enqueue(
        root.clone(),
        TaskId::new(),
        request("git-integrate:/repo:main"),
    );
    coordinator.enqueue(
        root,
        TaskId::new(),
        shared_request("git-integrate:/repo:main", 1),
    );

    let exclusive = coordinator.grant_next("git-integrate:/repo:main").unwrap();
    assert!(coordinator.grant_next("git-integrate:/repo:main").is_none());
    coordinator.release(exclusive.lease_id).unwrap();
    assert!(coordinator.grant_next("git-integrate:/repo:main").is_some());
}

#[test]
fn releasing_the_same_lease_twice_is_idempotent() {
    let mut coordinator = ResourceCoordinator::new();
    let root = RootSessionId::new();
    coordinator.enqueue(root, TaskId::new(), request("resident:global"));
    let lease = coordinator.grant_next("resident:global").unwrap();

    coordinator.release(lease.lease_id.clone()).unwrap();
    coordinator.release(lease.lease_id).unwrap();
}

#[test]
fn queue_capacity_rejects_without_allocating_a_hidden_request() {
    let mut coordinator = ResourceCoordinator::new();
    coordinator.set_queue_capacity(1);
    let root = RootSessionId::new();
    coordinator
        .try_enqueue(root.clone(), TaskId::new(), request("resident:global"))
        .unwrap();

    assert!(matches!(
        coordinator.try_enqueue(root, TaskId::new(), request("resident:global")),
        Err(AdmissionError::QueueCapacityExceeded)
    ));
    assert_eq!(coordinator.queued_request_count(), 1);
}

#[test]
fn expired_resource_request_is_removed_before_grant() {
    let mut coordinator = ResourceCoordinator::new();
    let root = RootSessionId::new();
    let mut expired = request("resident:global");
    expired.deadline = Some(Utc::now() - Duration::seconds(1));
    coordinator.enqueue(root, TaskId::new(), expired);

    assert!(coordinator.grant_next("resident:global").is_none());
    assert_eq!(coordinator.queued_request_count(), 0);
}

#[test]
fn high_priority_request_precedes_a_normal_request() {
    let mut coordinator = ResourceCoordinator::new();
    coordinator.set_capacity("resident:global", 1);
    let normal = TaskId::new();
    let high = TaskId::new();
    coordinator.enqueue(RootSessionId::new(), normal, request("resident:global"));
    coordinator.enqueue_with_priority(
        RootSessionId::new(),
        high.clone(),
        request("resident:global"),
        AdmissionPriority::High,
    );

    assert_eq!(
        coordinator.grant_next("resident:global").unwrap().task_id,
        high
    );
}

#[test]
fn age_prevents_background_starvation_under_normal_contention() {
    let mut coordinator = ResourceCoordinator::new();
    coordinator.set_capacity("resident:global", 1);
    let background = TaskId::new();
    let background_root = RootSessionId::new();
    let normal_root = RootSessionId::new();
    let start = Utc::now();
    coordinator.enqueue_with_priority_at(
        background_root,
        background.clone(),
        request("resident:global"),
        AdmissionPriority::Background,
        start,
    );

    for elapsed_seconds in (0..=300).step_by(30) {
        let now = start + Duration::seconds(elapsed_seconds);
        coordinator.enqueue_with_priority_at(
            normal_root.clone(),
            TaskId::new(),
            request("resident:global"),
            AdmissionPriority::Normal,
            now,
        );

        let lease = coordinator.grant_next_at("resident:global", now).unwrap();
        if lease.task_id == background {
            return;
        }
        coordinator.release(lease.lease_id).unwrap();
    }

    panic!("background work did not gain admission within eleven grants");
}

#[test]
fn unrunnable_high_priority_request_does_not_block_a_runnable_request() {
    let mut coordinator = ResourceCoordinator::new();
    coordinator.set_capacity("resident:global", 1);
    let runnable = TaskId::new();
    coordinator.enqueue_with_priority(
        RootSessionId::new(),
        TaskId::new(),
        shared_request("resident:global", 2),
        AdmissionPriority::High,
    );
    coordinator.enqueue(
        RootSessionId::new(),
        runnable.clone(),
        shared_request("resident:global", 1),
    );

    assert_eq!(
        coordinator.grant_next("resident:global").unwrap().task_id,
        runnable
    );
}

#[test]
fn incompatible_high_priority_request_does_not_block_a_compatible_request() {
    let mut coordinator = ResourceCoordinator::new();
    coordinator.set_capacity("resident:global", 2);
    let root = RootSessionId::new();
    coordinator.enqueue(
        root.clone(),
        TaskId::new(),
        shared_request("resident:global", 1),
    );
    let held_lease = coordinator.grant_next("resident:global").unwrap();
    let runnable = TaskId::new();
    coordinator.enqueue_with_priority(
        root.clone(),
        TaskId::new(),
        request("resident:global"),
        AdmissionPriority::High,
    );
    coordinator.enqueue(root, runnable.clone(), shared_request("resident:global", 1));

    assert_eq!(
        coordinator.grant_next("resident:global").unwrap().task_id,
        runnable
    );
    coordinator.release(held_lease.lease_id).unwrap();
}
