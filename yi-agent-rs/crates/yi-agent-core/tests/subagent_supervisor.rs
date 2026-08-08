use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use yi_agent_core::subagent::mailbox::{MailboxMessageDraft, MessageKind};
use yi_agent_core::subagent::supervisor::{
    AgentSupervisor, SpawnError, SupervisorEvent, SupervisorTools,
};
use yi_agent_core::subagent::task::{PermissionRequestId, RootSessionId, TaskDepth};
use yi_agent_core::{ContentBlock, ToolRegistry};

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

#[tokio::test]
async fn wait_all_returns_only_after_all_direct_children_are_terminal() {
    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(RootSessionId::new())));
    let root = supervisor.lock().unwrap().root_task_id().clone();
    let first = supervisor.lock().unwrap().spawn(root.clone()).unwrap();
    let second = supervisor.lock().unwrap().spawn(root.clone()).unwrap();
    supervisor.lock().unwrap().start_task(&first).unwrap();
    supervisor.lock().unwrap().start_task(&second).unwrap();
    let wait_tool = SupervisorTools::new(supervisor.clone(), root).wait_agent();

    let waiting = tokio::spawn(async move { wait_tool.call(json!({ "mode": "all" })).await });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());

    supervisor
        .lock()
        .unwrap()
        .fail_task(&first, "first failed")
        .unwrap();
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());

    supervisor
        .lock()
        .unwrap()
        .fail_task(&second, "second failed")
        .unwrap();
    let result = waiting.await.unwrap();
    assert!(!result.is_error);
    assert!(matches!(&result.content[0], ContentBlock::Text(text) if text.contains("completed")));
}

#[tokio::test]
async fn wait_any_returns_after_the_first_terminal_child() {
    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(RootSessionId::new())));
    let root = supervisor.lock().unwrap().root_task_id().clone();
    let first = supervisor.lock().unwrap().spawn(root.clone()).unwrap();
    let second = supervisor.lock().unwrap().spawn(root.clone()).unwrap();
    supervisor.lock().unwrap().start_task(&first).unwrap();
    supervisor.lock().unwrap().start_task(&second).unwrap();
    let wait_tool = SupervisorTools::new(supervisor.clone(), root).wait_agent();

    let waiting = tokio::spawn(async move { wait_tool.call(json!({ "mode": "any" })).await });
    tokio::task::yield_now().await;
    supervisor
        .lock()
        .unwrap()
        .fail_task(&first, "first failed")
        .unwrap();

    let result = waiting.await.unwrap();
    assert!(!result.is_error);
    assert_eq!(
        supervisor
            .lock()
            .unwrap()
            .task(&second)
            .unwrap()
            .state()
            .is_terminal(),
        false
    );
}

#[tokio::test]
async fn wait_is_interrupted_by_a_permission_request() {
    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(RootSessionId::new())));
    let root = supervisor.lock().unwrap().root_task_id().clone();
    let child = supervisor.lock().unwrap().spawn(root.clone()).unwrap();
    let wait_tool = SupervisorTools::new(supervisor.clone(), root.clone()).wait_agent();

    let waiting = tokio::spawn(async move { wait_tool.call(json!({ "mode": "any" })).await });
    tokio::task::yield_now().await;
    let request = MailboxMessageDraft::new(
        child.clone(),
        root,
        MessageKind::PermissionRequest(PermissionRequestId::new()),
        None,
    );
    supervisor
        .lock()
        .unwrap()
        .send_message(&child, request)
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("permission request should interrupt the wait")
        .unwrap();
    assert!(
        matches!(&result.content[0], ContentBlock::Text(text) if text.contains("needs_attention"))
    );
}

#[test]
fn supervisor_toolset_registers_all_three_builtin_schemas() {
    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(RootSessionId::new())));
    let root = supervisor.lock().unwrap().root_task_id().clone();
    let tools = SupervisorTools::new(supervisor, root);
    let mut registry = ToolRegistry::new();

    tools.register_into(&mut registry);

    let names = registry
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["send_message", "spawn_agent", "wait_agent"]);
}
