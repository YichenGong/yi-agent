use yi_agent_core::subagent::supervisor::{AgentSupervisor, SpawnError, SupervisorEvent};
use yi_agent_core::subagent::task::{RootSessionId, TaskDepth};

#[test]
fn spawn_enforces_depth_two_and_four_direct_children() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();

    let child = supervisor.spawn(root.clone()).unwrap();
    let leaf = supervisor.spawn(child.clone()).unwrap();
    assert_eq!(supervisor.task(&leaf).unwrap().depth, TaskDepth::Leaf);
    assert!(matches!(
        supervisor.spawn(leaf),
        Err(SpawnError::MaximumDepthReached)
    ));

    for _ in 0..3 {
        supervisor.spawn(root.clone()).unwrap();
    }
    assert!(matches!(
        supervisor.spawn(root),
        Err(SpawnError::DirectChildLimitReached)
    ));
}

#[test]
fn spawning_enqueues_child_and_emits_a_structured_event() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();

    let child = supervisor.spawn(root.clone()).unwrap();

    assert_eq!(supervisor.children_of(&root), &[child.clone()]);
    assert!(matches!(
        supervisor.events().last(),
        Some(SupervisorEvent::TaskSpawned { parent_id, task_id }) if parent_id == &root && task_id == &child
    ));
}
