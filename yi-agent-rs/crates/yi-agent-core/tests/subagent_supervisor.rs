use std::sync::{Arc, Mutex};

use serde_json::json;
use yi_agent_core::subagent::supervisor::{
    AgentSupervisor, SpawnError, SupervisorEvent, SupervisorTools,
};
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

#[tokio::test]
async fn built_in_spawn_tool_returns_immediately_and_rejects_leaf_spawn() {
    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(RootSessionId::new())));
    let root = supervisor.lock().unwrap().root_task_id().clone();
    let tools = SupervisorTools::new(supervisor.clone(), root.clone());

    let spawn = tools.spawn_agent();
    assert_eq!(spawn.name(), "spawn_agent");
    assert!(!spawn.call(json!({})).await.is_error);

    let child = supervisor.lock().unwrap().children_of(&root)[0].clone();
    let leaf = supervisor.lock().unwrap().spawn(child).unwrap();
    let leaf_tools = SupervisorTools::new(supervisor, leaf);
    assert!(leaf_tools.spawn_agent().call(json!({})).await.is_error);
    assert_eq!(tools.wait_agent().name(), "wait_agent");
    assert_eq!(tools.send_message().name(), "send_message");
}

#[tokio::test]
async fn send_message_delivers_to_a_direct_child_mailbox() {
    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(RootSessionId::new())));
    let root = supervisor.lock().unwrap().root_task_id().clone();
    let child = supervisor.lock().unwrap().spawn(root.clone()).unwrap();
    let tools = SupervisorTools::new(supervisor.clone(), root);

    let result = tools
        .send_message()
        .call(json!({ "recipient": child.to_string(), "message": "check progress" }))
        .await;

    assert!(!result.is_error);
    assert_eq!(
        supervisor
            .lock()
            .unwrap()
            .mailbox(&child)
            .unwrap()
            .messages()
            .len(),
        1
    );
}
