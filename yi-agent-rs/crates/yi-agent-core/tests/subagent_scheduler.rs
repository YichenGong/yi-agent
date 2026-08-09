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

    coordinator.enqueue(root_a.clone(), a1.clone(), request("resident:global"));
    coordinator.enqueue(root_a.clone(), a2, request("resident:global"));
    coordinator.enqueue(root_b.clone(), b1.clone(), request("resident:global"));
    let first = coordinator.grant_next("resident:global").unwrap();
    assert_eq!(first.task_id, a1);
    coordinator.release(first.lease_id).unwrap();

    let second = coordinator.grant_next("resident:global").unwrap();
    assert_eq!(second.task_id, b1);
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
