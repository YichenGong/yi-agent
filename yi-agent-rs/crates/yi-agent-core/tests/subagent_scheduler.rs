use yi_agent_core::subagent::scheduler::{
    LeaseMode, ResourceCoordinator, ResourceRequest, ResourceScope,
};
use yi_agent_core::subagent::task::{RootSessionId, TaskId};

fn request(key: &str) -> ResourceRequest {
    ResourceRequest {
        scope: ResourceScope::Global,
        key: key.into(),
        mode: LeaseMode::Exclusive,
        units: 1,
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
