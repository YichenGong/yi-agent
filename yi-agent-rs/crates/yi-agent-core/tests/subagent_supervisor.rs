use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use std::time::Duration;

use serde_json::json;
use yi_agent_core::subagent::mailbox::{MailboxMessageDraft, MessageKind};
use yi_agent_core::subagent::supervisor::{
    AgentSupervisor, SpawnError, SupervisorEvent, SupervisorTools,
};
use yi_agent_core::subagent::task::{
    PauseReason, PermissionRequestId, RootSessionId, TaskDepth, TaskState,
};
use yi_agent_core::subagent::worker::{AgentWorkerFactory, WorkerError, WorkerHandle, WorkerStart};
use yi_agent_core::{ContentBlock, ProviderTurnGate, ProviderTurnLease, ToolRegistry};

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

#[test]
fn spawning_with_an_objective_retains_the_worker_instruction() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();

    let child = supervisor
        .spawn_with_objective(root, "Audit the scheduler fairness tests".into())
        .unwrap();

    assert_eq!(
        supervisor.objective(&child),
        Some("Audit the scheduler fairness tests")
    );
}

struct ImmediateWorkerFactory;

impl AgentWorkerFactory for ImmediateWorkerFactory {
    fn start(
        &self,
        request: WorkerStart,
    ) -> BoxFuture<'static, Result<WorkerHandle, yi_agent_core::subagent::worker::WorkerError>>
    {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

#[derive(Clone, Default)]
struct PauseReportingWorkerFactory {
    handle: Arc<Mutex<Option<WorkerHandle>>>,
}

impl AgentWorkerFactory for PauseReportingWorkerFactory {
    fn start(
        &self,
        request: WorkerStart,
    ) -> BoxFuture<'static, Result<WorkerHandle, yi_agent_core::subagent::worker::WorkerError>>
    {
        let handle = WorkerHandle::new(request.cancellation);
        *self.handle.lock().unwrap() = Some(handle.clone());
        Box::pin(async move { Ok(handle) })
    }
}

struct FailingWorkerFactory;

impl AgentWorkerFactory for FailingWorkerFactory {
    fn start(
        &self,
        _request: WorkerStart,
    ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async { Err(WorkerError::Startup("provider bootstrap failed".into())) })
    }
}

struct ReportingWorkerFactory;

impl AgentWorkerFactory for ReportingWorkerFactory {
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move {
            let handle = WorkerHandle::new(request.cancellation);
            handle.report_failure("provider stream disconnected");
            Ok(handle)
        })
    }
}

#[derive(Clone)]
struct InboxWorkerFactory {
    handle: Arc<Mutex<Option<WorkerHandle>>>,
}

#[derive(Clone)]
struct CapabilityWorkerFactory {
    start: Arc<Mutex<Option<WorkerStart>>>,
}

#[derive(Default)]
struct GateObservingWorkerFactory {
    received_gate: Arc<Mutex<bool>>,
}

struct NoopTurnLease;

struct NoopTurnGate;

impl ProviderTurnGate for NoopTurnGate {
    fn acquire(&self) -> BoxFuture<'static, Result<Box<dyn ProviderTurnLease>, String>> {
        Box::pin(async { Ok(Box::new(NoopTurnLease) as Box<dyn ProviderTurnLease>) })
    }
}

impl AgentWorkerFactory for GateObservingWorkerFactory {
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }

    fn start_with_provider_turn_gate(
        &self,
        request: WorkerStart,
        gate: Option<Arc<dyn ProviderTurnGate>>,
    ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        *self.received_gate.lock().unwrap() = gate.is_some();
        self.start(request)
    }
}

impl AgentWorkerFactory for CapabilityWorkerFactory {
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation.clone());
        *self.start.lock().unwrap() = Some(request);
        Box::pin(async move { Ok(handle) })
    }
}

impl AgentWorkerFactory for InboxWorkerFactory {
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation);
        for message in request.initial_user_messages {
            handle.deliver_worker_message(message);
        }
        *self.handle.lock().unwrap() = Some(handle.clone());
        Box::pin(async move { Ok(handle) })
    }
}

#[tokio::test]
async fn supervisor_admission_starts_and_cancels_a_owned_worker() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();
    let child = supervisor.spawn(root.clone()).unwrap();

    supervisor
        .start_worker(&ImmediateWorkerFactory, &child)
        .await
        .unwrap();
    assert!(supervisor.has_worker(&child));
    let cancellation = supervisor.worker_cancellation(&child).unwrap();
    supervisor.cancel_worker(&child).unwrap();
    assert!(cancellation.is_cancelled());
}

#[tokio::test]
async fn supervisor_passes_a_provider_turn_gate_to_the_factory() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let child = supervisor.spawn(supervisor.root_task_id().clone()).unwrap();
    let factory = GateObservingWorkerFactory::default();

    supervisor
        .start_worker_with_provider_turn_gate(&factory, &child, Some(Arc::new(NoopTurnGate)))
        .await
        .unwrap();

    assert!(*factory.received_gate.lock().unwrap());
}

#[tokio::test]
async fn supervisor_marks_paused_only_after_worker_safe_checkpoint_acknowledgement() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let child = supervisor.spawn(supervisor.root_task_id().clone()).unwrap();
    let factory = PauseReportingWorkerFactory::default();
    supervisor.start_worker(&factory, &child).await.unwrap();
    let cancellation = supervisor.worker_cancellation(&child).unwrap();

    supervisor
        .pause_task(&child, PauseReason("user requested pause".into()))
        .unwrap();
    assert_eq!(
        supervisor.task(&child).unwrap().state(),
        &TaskState::Running
    );
    assert!(supervisor.has_worker(&child));
    assert!(!cancellation.is_cancelled());

    factory
        .handle
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .report_paused();
    assert_eq!(
        supervisor.reconcile_worker_events().unwrap(),
        vec![child.clone()]
    );
    assert!(matches!(
        supervisor.task(&child).unwrap().state(),
        TaskState::Paused(_)
    ));
    assert!(!supervisor.has_worker(&child));

    supervisor.resume_task(&child).unwrap();
    assert_eq!(supervisor.task(&child).unwrap().state(), &TaskState::Queued);
}

#[tokio::test]
async fn worker_message_capability_is_required_and_bound_to_the_started_task() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();
    let child = supervisor.spawn(root.clone()).unwrap();
    let factory = CapabilityWorkerFactory {
        start: Arc::new(Mutex::new(None)),
    };

    supervisor.start_worker(&factory, &child).await.unwrap();
    let capability = factory
        .start
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .message_capability
        .clone();

    assert!(
        supervisor
            .send_worker_message(&child, &capability, root.clone(), "continue".into())
            .is_ok()
    );
    assert!(
        supervisor
            .send_worker_message(&child, "forged", root, "continue".into())
            .is_err()
    );
}

#[tokio::test]
async fn queued_user_override_is_delivered_when_its_worker_starts() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let child = supervisor.spawn(supervisor.root_task_id().clone()).unwrap();
    supervisor
        .send_user_override(child.clone(), "explain the blocker first".into())
        .unwrap();
    let handle = Arc::new(Mutex::new(None));
    let factory = InboxWorkerFactory {
        handle: Arc::clone(&handle),
    };

    supervisor.start_worker(&factory, &child).await.unwrap();
    let mut mailbox = handle
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .subscribe_messages();
    let message = tokio::time::timeout(Duration::from_millis(100), mailbox.recv())
        .await
        .expect("queued user message should reach the worker")
        .expect("worker inbox remains open");

    assert_eq!(message.body, "explain the blocker first");
}

#[tokio::test]
async fn worker_consumption_acknowledges_a_user_override_only_after_inbox_receipt() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let child = supervisor.spawn(supervisor.root_task_id().clone()).unwrap();
    supervisor
        .send_user_override(child.clone(), "explain the blocker first".into())
        .unwrap();
    let message_id = supervisor.mailbox(&child).unwrap().messages()[0].id.clone();
    let handle = Arc::new(Mutex::new(None));
    let factory = InboxWorkerFactory {
        handle: Arc::clone(&handle),
    };

    supervisor.start_worker(&factory, &child).await.unwrap();
    assert!(supervisor.take_consumed_user_override_ids().is_empty());

    let received = handle
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .subscribe_messages()
        .recv()
        .await
        .unwrap();
    assert_eq!(received.id, message_id);
    assert!(supervisor.take_consumed_user_override_ids().is_empty());

    handle
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .report_message_consumed(received.id);
    supervisor.reconcile_worker_events().unwrap();

    assert_eq!(
        supervisor.take_consumed_user_override_ids(),
        vec![(child, message_id)]
    );
}

#[tokio::test]
async fn worker_consumption_does_not_classify_agent_mail_as_an_external_override() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();
    let child = supervisor.spawn(root.clone()).unwrap();
    let handle = Arc::new(Mutex::new(None));
    let factory = InboxWorkerFactory {
        handle: Arc::clone(&handle),
    };
    supervisor.start_worker(&factory, &child).await.unwrap();
    supervisor
        .send_user_message(&root, child.clone(), "report status".into())
        .unwrap();
    let message_id = supervisor.mailbox(&child).unwrap().messages()[0].id.clone();

    handle
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .report_message_consumed(message_id);
    supervisor.reconcile_worker_events().unwrap();

    assert!(supervisor.take_consumed_user_override_ids().is_empty());
}

#[tokio::test]
async fn worker_startup_failure_is_recorded_after_admission_without_a_handle() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let child = supervisor.spawn(supervisor.root_task_id().clone()).unwrap();

    assert!(
        supervisor
            .start_worker(&FailingWorkerFactory, &child)
            .await
            .is_err()
    );

    assert!(!supervisor.has_worker(&child));
    assert!(matches!(
        supervisor.task(&child).unwrap().state(),
        TaskState::Failed(_)
    ));
}

#[tokio::test]
async fn supervisor_reduces_worker_failure_events_and_releases_the_handle() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let child = supervisor.spawn(supervisor.root_task_id().clone()).unwrap();
    supervisor
        .start_worker(&ReportingWorkerFactory, &child)
        .await
        .unwrap();

    assert_eq!(
        supervisor.reconcile_worker_events().unwrap(),
        vec![child.clone()]
    );

    assert!(!supervisor.has_worker(&child));
    assert!(matches!(
        supervisor.task(&child).unwrap().state(),
        TaskState::Failed(_)
    ));
}

#[tokio::test]
async fn built_in_spawn_tool_returns_immediately_and_rejects_leaf_spawn() {
    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(RootSessionId::new())));
    let root = supervisor.lock().unwrap().root_task_id().clone();
    let tools = SupervisorTools::new(supervisor.clone(), root.clone());

    let spawn = tools.spawn_agent();
    assert_eq!(spawn.name(), "spawn_agent");
    assert!(spawn.call(json!({})).await.is_error);
    assert!(
        !spawn
            .call(json!({ "task": "Inspect the current task state." }))
            .await
            .is_error
    );

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
async fn send_message_wakes_the_recipient_worker_inbox() {
    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(RootSessionId::new())));
    let root = supervisor.lock().unwrap().root_task_id().clone();
    let child = supervisor.lock().unwrap().spawn(root.clone()).unwrap();
    let factory = InboxWorkerFactory {
        handle: Arc::new(Mutex::new(None)),
    };
    supervisor
        .lock()
        .unwrap()
        .start_worker(&factory, &child)
        .await
        .unwrap();
    let mut inbox = factory
        .handle
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .subscribe_messages();

    let tools = SupervisorTools::new(supervisor, root);
    assert!(
        !tools
            .send_message()
            .call(json!({ "recipient": child.to_string(), "message": "check progress" }))
            .await
            .is_error
    );
    let message = inbox.recv().await.unwrap();
    assert_eq!(message.body, "check progress");
}

#[test]
fn terminal_task_cannot_send_a_message_to_its_parent() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();
    let child = supervisor.spawn(root.clone()).unwrap();
    supervisor.start_task(&child).unwrap();
    supervisor.fail_task(&child, "worker stopped").unwrap();

    let result = supervisor.send_user_message(&child, root, "late message".into());

    assert!(result.is_err());
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
