use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{path::Path, process::Command};

use chrono::{DateTime, Duration as ChronoDuration, Local, Timelike, Utc};
use futures::future::BoxFuture;
use rusqlite::Connection;
use tempfile::TempDir;
use yi_agent_core::ProviderTurnGate;
use yi_agent_core::RootSessionId;
use yi_agent_core::subagent::task::{
    AttemptId, BudgetKind, ChildWriteMode, DeliveryReport, IntegrationValidation, MessageId,
    PermissionDecision, PermissionRequestId, TaskId, TimeoutKind, WorkspaceLeaseId,
};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryAttestation,
    WorkerRecoveryContext, WorkerRecoveryPreflight, WorkerRecoveryPreflightResult, WorkerStart,
    WorkerWorkspace, WorkerWorkspaceProvider, WorkerWorkspaceRegistry,
};
use yi_agent_store::repository::{
    RuntimeEvent, RuntimeRepository, WatchdogEvidence, WatchdogResourceWait, WatchdogTerminal,
};
use yi_agent_store::runtime::{RuntimeCoordinator, RuntimeCoordinatorError, RuntimeStopOptions};
use yi_agent_store::schedule::{
    MissedRunPolicy, ScheduleDefinition, WatchdogLimits, WatchdogUsage,
};

#[derive(Default)]
struct RecordingFactory;

impl AgentWorkerFactory for RecordingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

struct FailingFactory;

#[derive(Clone, Default)]
struct ProviderGateRecordingFactory {
    received_gate: Arc<Mutex<bool>>,
}

impl AgentWorkerFactory for ProviderGateRecordingFactory {
    fn provider_profile_id(&self) -> Option<String> {
        Some("test-profile".into())
    }

    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

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

impl AgentWorkerFactory for FailingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move {
            let handle = WorkerHandle::new(request.cancellation);
            handle.report_failure("provider disconnected");
            Ok(handle)
        })
    }
}

#[derive(Clone, Default)]
struct MessageRecordingFactory {
    starts: Arc<Mutex<Vec<WorkerStart>>>,
    handles: Arc<Mutex<Vec<WorkerHandle>>>,
    recovery_preflights: Arc<Mutex<Vec<WorkerRecoveryPreflight>>>,
    workspace_service: Option<Arc<dyn WorkerWorkspaceProvider>>,
    workspace_registry: Option<Arc<dyn WorkerWorkspaceRegistry>>,
}

impl AgentWorkerFactory for MessageRecordingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn preflight_recovery(
        &self,
        request: WorkerRecoveryPreflight,
    ) -> WorkerRecoveryPreflightResult {
        self.recovery_preflights.lock().unwrap().push(request);
        WorkerRecoveryPreflightResult::Attested(durable_attestation())
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation.clone());
        self.starts.lock().unwrap().push(request);
        self.handles.lock().unwrap().push(handle.clone());
        Box::pin(async move { Ok(handle) })
    }

    fn default_workspace_service(&self) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        self.workspace_service.clone()
    }

    fn worker_workspace_registry(&self) -> Option<Arc<dyn WorkerWorkspaceRegistry>> {
        self.workspace_registry.clone()
    }
}

#[derive(Clone, Default)]
struct PauseRecordingFactory {
    handles: Arc<Mutex<Vec<WorkerHandle>>>,
}

struct AdmissionObservingFactory {
    database: std::path::PathBuf,
}

struct RecoveryStartObservingFactory {
    database: std::path::PathBuf,
    starts: Arc<Mutex<usize>>,
}

#[derive(Default)]
struct ConflictReportingFactory {
    starts: Arc<Mutex<usize>>,
}

struct StartupErrorFactory;

#[derive(Clone)]
struct StaticWorkspaceService {
    workspace: WorkerWorkspace,
}

impl WorkerWorkspaceProvider for StaticWorkspaceService {
    fn in_place_workspace(
        &self,
        _root_session_id: &RootSessionId,
        _task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(self.workspace.clone())
    }

    fn workspace_in(
        &self,
        _task_id: &TaskId,
        _workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(self.workspace.clone())
    }

    fn read_only_workspace(
        &self,
        _parent: Option<&WorkerWorkspace>,
        _task_id: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        // Read-only runs in the same place; the mode never changes a position.
        Ok(self.workspace.clone())
    }
}

/// The prepared-workspace lookup the runtime consults for a parent-chosen
/// workdir. The test double holds exactly one prepared workspace.
impl WorkerWorkspaceRegistry for StaticWorkspaceService {
    fn register_prepared(&self, workspace: &WorkerWorkspace) {
        let _ = workspace;
    }

    fn prepared_workspace_for_workdir(&self, workdir: &std::path::Path) -> Option<WorkerWorkspace> {
        (self.workspace.path == workdir).then(|| self.workspace.clone())
    }

    fn observe_workdir(&self, workdir: &std::path::Path) -> Result<WorkerWorkspace, WorkerError> {
        // A prepared directory is observed as-is: the parent made it, the daemon
        // only reads its position.
        Ok(WorkerWorkspace {
            path: workdir.to_path_buf(),
            ..self.workspace.clone()
        })
    }
}

struct GitWorkspaceService {
    repository_root: std::path::PathBuf,
}

impl GitWorkspaceService {
    fn new(repository_root: std::path::PathBuf) -> Self {
        Self { repository_root }
    }
}

impl WorkerWorkspaceProvider for GitWorkspaceService {
    fn in_place_workspace(
        &self,
        _root_session_id: &RootSessionId,
        task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        // In place: the task runs in the project directory. No directory is
        // created and no branch is recorded.
        let _ = task_id;
        Ok(WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: self.repository_root.clone(),
            path: self.repository_root.clone(),
            branch: String::new(),
            parent_branch: String::new(),
            base_commit: String::new(),
        })
    }

    fn workspace_in(
        &self,
        _task_id: &TaskId,
        workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        // The parent prepared this directory; the provider only resolves it.
        let base = git_output(workdir, &["rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let branch = git_output(workdir, &["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|branch| branch.trim().to_owned())?;
        Ok(WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: self.repository_root.clone(),
            path: workdir.to_path_buf(),
            branch,
            parent_branch: "main".into(),
            base_commit: base,
        })
    }

    fn read_only_workspace(
        &self,
        parent: Option<&WorkerWorkspace>,
        _task_id: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        let path = parent
            .map(|workspace| workspace.path.clone())
            .unwrap_or_else(|| self.repository_root.clone());
        Ok(WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: self.repository_root.clone(),
            path,
            branch: String::new(),
            parent_branch: String::new(),
            base_commit: String::new(),
        })
    }
}

/// Models a non-git application root: read-only provisioning works in place,
/// while any coding request must be rejected by the coordinator before the
/// service is asked for a worktree.
/// Inspection is the registry's concern, so the double mirrors production: the
/// provider resolves positions, the registry reports deliveries.
impl WorkerWorkspaceRegistry for GitWorkspaceService {
    fn register_prepared(&self, _workspace: &WorkerWorkspace) {}

    fn prepared_workspace_for_workdir(
        &self,
        _workdir: &std::path::Path,
    ) -> Option<WorkerWorkspace> {
        None
    }

    fn inspect_delivery(&self, workspace: &WorkerWorkspace) -> Result<DeliveryReport, WorkerError> {
        let status = git_output(&workspace.path, &["status", "--porcelain"])?;
        if !status.is_empty() {
            return Err(WorkerError::Startup(
                "Git workspace error: dirty child".into(),
            ));
        }
        let head = git_output(&workspace.path, &["rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        if head == workspace.base_commit {
            return Err(WorkerError::Startup(
                "Git workspace error: empty delivery".into(),
            ));
        }
        Ok(DeliveryReport::coding(
            head,
            workspace.parent_branch.clone(),
            workspace.lease_id.clone(),
            "inspected delivery",
        ))
    }
}

#[derive(Clone)]
struct NonGitWorkspaceService {
    repository_root: std::path::PathBuf,
}

impl WorkerWorkspaceProvider for NonGitWorkspaceService {
    fn read_only_workspace(
        &self,
        parent: Option<&WorkerWorkspace>,
        _task_id: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        let path = parent
            .map(|workspace| workspace.path.clone())
            .unwrap_or_else(|| self.repository_root.clone());
        Ok(WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: self.repository_root.clone(),
            path,
            branch: String::new(),
            parent_branch: String::new(),
            base_commit: String::new(),
        })
    }

    fn in_place_workspace(
        &self,
        _root_session_id: &RootSessionId,
        _task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Err(WorkerError::Startup(
            "non-git workspace has no root worktree".into(),
        ))
    }

    fn workspace_in(
        &self,
        _task_id: &TaskId,
        _workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Err(WorkerError::Startup(
            "non-git workspace has no child worktree".into(),
        ))
    }
}

#[derive(Clone)]
struct WorkspaceObservingFactory {
    starts: Arc<Mutex<Vec<WorkerStart>>>,
    handles: Arc<Mutex<Vec<WorkerHandle>>>,
    workspace_service: Arc<dyn WorkerWorkspaceProvider>,
    workspace_registry: Option<Arc<dyn WorkerWorkspaceRegistry>>,
}

impl AgentWorkerFactory for WorkspaceObservingFactory {
    fn recovery_context_for(&self, request: &WorkerStart) -> WorkerRecoveryContext {
        request
            .workspace
            .as_ref()
            .map(workspace_recovery_context)
            .unwrap_or_else(durable_context)
    }

    fn default_workspace_service(&self) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        Some(Arc::clone(&self.workspace_service))
    }

    fn worker_workspace_registry(&self) -> Option<Arc<dyn WorkerWorkspaceRegistry>> {
        self.workspace_registry.clone()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let assigned = request
            .workspace
            .as_ref()
            .expect("worker start includes workspace assignment");
        assert!(request.workspace_lease_id.is_some());
        let _ = assigned;
        let handle = WorkerHandle::new(request.cancellation.clone());
        self.starts.lock().unwrap().push(request.clone());
        self.handles.lock().unwrap().push(handle.clone());
        Box::pin(async move { Ok(handle) })
    }
}

fn workspace_recovery_context(workspace: &WorkerWorkspace) -> WorkerRecoveryContext {
    WorkerRecoveryContext {
        workspace_lease_id: Some(format!("workspace:{}", workspace.path.display())),
        worktree_lease: Some(format!("worktree:{}", workspace.repository_root.display())),
        checkpoint_json: r#"{"state":"workspace-test"}"#.into(),
        tool_state_json: r#"{"state":"workspace-test"}"#.into(),
    }
}

#[derive(Clone)]
struct DerivedWorkspaceService {
    repository_root: std::path::PathBuf,
}

impl WorkerWorkspaceProvider for DerivedWorkspaceService {
    fn in_place_workspace(
        &self,
        _root_session_id: &RootSessionId,
        task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: self.repository_root.clone(),
            path: self.repository_root.join(format!("root-{task_id}")),
            branch: format!("feat/root-{task_id}"),
            parent_branch: "main".into(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        })
    }

    fn workspace_in(
        &self,
        task_id: &TaskId,
        workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: self.repository_root.clone(),
            path: workdir.to_path_buf(),
            branch: format!("feat/child-{task_id}"),
            parent_branch: "main".into(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        })
    }
}

#[derive(Clone, Default)]
struct FailingWorkspaceService;

impl WorkerWorkspaceProvider for FailingWorkspaceService {
    fn in_place_workspace(
        &self,
        _root_session_id: &RootSessionId,
        _task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Err(WorkerError::Startup("Git workspace error: boom".into()))
    }

    fn workspace_in(
        &self,
        _task_id: &TaskId,
        _workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Err(WorkerError::Startup("Git workspace error: boom".into()))
    }
}

fn durable_context() -> WorkerRecoveryContext {
    WorkerRecoveryContext {
        workspace_lease_id: Some("workspace:test".into()),
        worktree_lease: Some("worktree:test".into()),
        checkpoint_json: r#"{"git_head":"test"}"#.into(),
        tool_state_json: r#"{"state":"available","registered_tools":[]}"#.into(),
    }
}

fn durable_attestation() -> WorkerRecoveryAttestation {
    WorkerRecoveryAttestation {
        checkpoint_json: r#"{"kind":"recovery_attestation","git_head":"test","git_status":""}"#
            .into(),
        tool_state_json: r#"{"state":"available","registered_tools":[]}"#.into(),
        evidence_json: r#"{"kind":"deterministic_recovery_preflight","result":"attested"}"#.into(),
    }
}

impl AgentWorkerFactory for AdmissionObservingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let repository =
            RuntimeRepository::open(&self.database).expect("factory can inspect runtime store");
        assert_eq!(repository.task_state(&request.task_id).unwrap(), "running");
        assert!(
            repository
                .has_active_lease_prefix(&request.task_id, "workspace:")
                .unwrap()
        );
        assert!(
            repository
                .has_active_lease_prefix(&request.task_id, "worktree:")
                .unwrap()
        );
        let connection = Connection::open(&self.database).unwrap();
        let (checkpoint, tool_state): (String, String) = connection
            .query_row(
                "SELECT checkpoint_json, usage_json FROM attempts WHERE id = ?1",
                [request.attempt_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(checkpoint, durable_context().checkpoint_json);
        assert_eq!(tool_state, durable_context().tool_state_json);
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

impl AgentWorkerFactory for RecoveryStartObservingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn preflight_recovery(
        &self,
        _request: WorkerRecoveryPreflight,
    ) -> WorkerRecoveryPreflightResult {
        WorkerRecoveryPreflightResult::Attested(durable_attestation())
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        *self.starts.lock().unwrap() += 1;
        let connection = Connection::open(&self.database).unwrap();
        let attested: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE task_id = ?1 AND kind = 'task_recovery_attested'",
                [request.task_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attested, 1, "recovery attestation must precede start");
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

impl AgentWorkerFactory for ConflictReportingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn preflight_recovery(
        &self,
        _request: WorkerRecoveryPreflight,
    ) -> WorkerRecoveryPreflightResult {
        WorkerRecoveryPreflightResult::Conflict(
            "recorded Git HEAD no longer matches checkpoint".into(),
        )
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        *self.starts.lock().unwrap() += 1;
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

impl AgentWorkerFactory for StartupErrorFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(
        &self,
        _request: WorkerStart,
    ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async { Err(WorkerError::Startup("provider bootstrap failed".into())) })
    }
}

impl AgentWorkerFactory for PauseRecordingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation);
        self.handles.lock().unwrap().push(handle.clone());
        Box::pin(async move { Ok(handle) })
    }
}

#[tokio::test]
async fn worker_receives_its_assigned_workspace_before_provider_start() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let workspace = WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: directory.path().join("repo"),
        path: directory.path().join("repo/.worktrees/worker"),
        branch: "feat/yi-agent-test-root".into(),
        parent_branch: "main".into(),
        base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
    };
    let starts = Arc::new(Mutex::new(Vec::new()));
    let handles = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(WorkspaceObservingFactory {
        starts: Arc::clone(&starts),
        handles,
        workspace_service: Arc::new(StaticWorkspaceService {
            workspace: workspace.clone(),
        }),
        workspace_registry: None,
    });
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();

    coordinator.start_worker(&session, &task).await.unwrap();

    let starts = starts.lock().unwrap();
    assert_eq!(starts.len(), 1);
    assert_eq!(
        starts[0].workspace.as_ref().map(|assigned| &assigned.path),
        Some(&workspace.path)
    );
}

#[tokio::test]
async fn child_recovery_context_uses_the_in_memory_workspace_assignment() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let starts = Arc::new(Mutex::new(Vec::new()));
    let handles = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(WorkspaceObservingFactory {
        starts: Arc::clone(&starts),
        handles: Arc::clone(&handles),
        workspace_service: Arc::new(DerivedWorkspaceService {
            repository_root: directory.path().join("repo"),
        }),
        workspace_registry: None,
    });
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();
    let child = coordinator
        .spawn_child_with_objective(
            &session,
            &root,
            "Complete the delegated task.".into(),
            ChildWriteMode::Coding,
            None,
            None,
        )
        .await
        .unwrap();

    coordinator.start_worker(&session, &child).await.unwrap();

    let starts = starts.lock().unwrap();
    assert_eq!(starts.len(), 2);
    assert_eq!(
        starts[1].workspace_lease_id,
        starts[1]
            .workspace
            .as_ref()
            .map(|workspace| workspace.lease_id.clone()),
    );
    assert!(starts[1].workspace_lease_id.is_some());
    assert!(
        starts[1]
            .workspace
            .as_ref()
            .is_some_and(|workspace| !workspace.path.as_os_str().is_empty())
    );
}

#[tokio::test]
async fn child_delivery_uses_the_assigned_workspace_lease_for_review() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let starts = Arc::new(Mutex::new(Vec::new()));
    let handles = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(WorkspaceObservingFactory {
        starts: Arc::clone(&starts),
        handles: Arc::clone(&handles),
        workspace_service: Arc::new(DerivedWorkspaceService {
            repository_root: directory.path().join("repo"),
        }),
        workspace_registry: None,
    });
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();
    let child = coordinator
        .spawn_child_with_objective(
            &session,
            &root,
            "Complete the delegated task.".into(),
            ChildWriteMode::Coding,
            None,
            None,
        )
        .await
        .unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();
    let workspace = starts.lock().unwrap()[1]
        .workspace_lease_id
        .clone()
        .expect("child start carries assigned workspace lease");

    handles.lock().unwrap()[1].report_delivery(DeliveryReport::coding(
        "deadbeef",
        "main",
        workspace,
        "cargo test",
    ));
    coordinator.reconcile_worker_events().await.unwrap();

    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "awaiting_parent_review"
    );
}

#[tokio::test]
async fn workspace_provisioning_failure_is_terminal_before_provider_start() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let starts = Arc::new(Mutex::new(Vec::new()));
    let handles = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(WorkspaceObservingFactory {
        starts: Arc::clone(&starts),
        handles,
        workspace_service: Arc::new(FailingWorkspaceService),
        workspace_registry: None,
    });
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();

    assert!(coordinator.start_worker(&session, &task).await.is_err());

    assert!(starts.lock().unwrap().is_empty());
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&task).unwrap(), "failed");
    let terminal = repository
        .attempt_terminal_json_for_task(&task)
        .unwrap()
        .expect("workspace failure is terminal evidence");
    assert!(terminal.contains("workspace_provision_failed"));
    assert!(terminal.contains("Git workspace error"));
}

#[tokio::test]
async fn coordinator_starts_root_worker_and_cancels_its_tree() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();

    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &root).await.unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();
    let root_cancellation = coordinator
        .worker_cancellation(&session, &root)
        .await
        .unwrap();
    let child_cancellation = coordinator
        .worker_cancellation(&session, &child)
        .await
        .unwrap();

    coordinator
        .cancel_task(&session, &root, true)
        .await
        .unwrap();

    assert!(root_cancellation.is_cancelled());
    assert!(child_cancellation.is_cancelled());
    assert_eq!(coordinator.task_state(&root).unwrap(), "cancelled");
    assert_eq!(coordinator.task_state(&child).unwrap(), "cancelled");
}

#[tokio::test]
async fn child_workspace_lease_identity_reaches_worker_and_durable_task() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &root).await.unwrap();

    coordinator.start_worker(&session, &child).await.unwrap();

    let start = factory.starts.lock().unwrap()[0].clone();
    let workspace = start
        .workspace_lease_id
        .expect("coding child receives a workspace lease identity");
    let persisted: String = Connection::open(&database)
        .unwrap()
        .query_row(
            "SELECT workspace_lease_id FROM tasks WHERE id = ?1",
            [child.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(persisted, format!("workspace:{workspace}"));
}

#[tokio::test]
async fn duplicate_worker_start_does_not_fail_the_running_attempt() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &root).await.unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();

    assert!(coordinator.start_worker(&session, &child).await.is_err());

    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "running"
    );
    assert_eq!(factory.starts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn coordinator_persists_worker_delivery_and_notifies_direct_parent() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let parent = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &parent).await.unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();
    let workspace = factory.starts.lock().unwrap()[0]
        .workspace_lease_id
        .clone()
        .unwrap();
    let delivery = yi_agent_core::subagent::task::DeliveryReport::coding(
        "deadbeef",
        "main",
        workspace,
        "cargo test -p child",
    );

    factory
        .handles
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .report_delivery(delivery.clone());
    coordinator.reconcile_worker_events().await.unwrap();

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(
        repository.task_state(&child).unwrap(),
        "awaiting_parent_review"
    );
    let mailbox = repository.mailbox_messages_for_task(&parent).unwrap();
    assert_eq!(mailbox.len(), 1);
    assert!(mailbox[0].payload_json.contains(&delivery.id.to_string()));
    let outcome = tokio::time::timeout(
        Duration::from_millis(50),
        coordinator.wait_for_children(
            &session,
            &parent,
            yi_agent_core::subagent::supervisor::WaitMode::All,
        ),
    )
    .await
    .expect("delivery wakes the direct parent")
    .unwrap();
    let yi_agent_core::subagent::supervisor::WaitOutcome::NeedsAttention { reports } = outcome
    else {
        panic!("delivery wakes the direct parent with an attention outcome");
    };
    assert_eq!(
        reports
            .first()
            .and_then(|report| report.delivery.as_deref()),
        Some(delivery.commit.as_str()),
        "the attention outcome still carries the delivered commit"
    );
}

#[tokio::test]
async fn trusted_parent_integration_requires_passed_validation_to_complete_review() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let parent = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &parent).await.unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();
    let workspace = factory.starts.lock().unwrap()[0]
        .workspace_lease_id
        .clone()
        .unwrap();
    let delivery = yi_agent_core::subagent::task::DeliveryReport::coding(
        "deadbeef",
        "main",
        workspace,
        "cargo test -p child",
    );
    factory
        .handles
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .report_delivery(delivery.clone());
    coordinator.reconcile_worker_events().await.unwrap();

    assert!(
        coordinator
            .accept_review(
                &child,
                IntegrationValidation::failed("parent integration tests failed"),
            )
            .await
            .is_err()
    );
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "awaiting_parent_review"
    );

    coordinator
        .accept_review(
            &child,
            IntegrationValidation::passed("cargo test -p parent"),
        )
        .await
        .unwrap();

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&child).unwrap(), "completed");
    let review_actor: String = Connection::open(&database)
        .unwrap()
        .query_row(
            "SELECT actor_json FROM reviews WHERE delivery_id = ?1",
            [delivery.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let review_actor: serde_json::Value = serde_json::from_str(&review_actor).unwrap();
    assert_eq!(review_actor["task_id"], parent.to_string());
    assert_eq!(review_actor["initiated_by"]["kind"], "parent_integration");
    let parent_mailbox = repository.mailbox_messages_for_task(&parent).unwrap();
    assert_eq!(parent_mailbox.len(), 1);
}

#[tokio::test]
async fn unmerged_delivery_stays_awaiting_review_across_reconcile() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let repository_root = directory.path().join("repo");
    std::fs::create_dir(&repository_root).unwrap();
    initialize_git_repository(&repository_root);
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(Arc::new(GitWorkspaceService::new(repository_root.clone()))),
        ..Default::default()
    });
    let (coordinator, _session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;

    coordinator.reconcile_worker_events().await.unwrap();

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(
        repository.task_state(&child).unwrap(),
        "awaiting_parent_review"
    );
    assert!(
        repository
            .event_records_for_task_after(&child, 0)
            .unwrap()
            .iter()
            .all(|event| !matches!(event.event, RuntimeEvent::ReviewAccepted)),
        "an unmerged delivery is never auto-accepted"
    );
}

#[tokio::test]
async fn review_confirmation_rejects_a_delivery_head_change_between_preview_and_confirm() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let repository_root = directory.path().join("repo");
    std::fs::create_dir(&repository_root).unwrap();
    initialize_git_repository(&repository_root);
    let service = Arc::new(GitWorkspaceService::new(repository_root.clone()));
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(service.clone() as Arc<dyn WorkerWorkspaceProvider>),
        workspace_registry: Some(service as Arc<dyn WorkerWorkspaceRegistry>),
        ..Default::default()
    });
    let (coordinator, _session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;

    let preview = coordinator
        .preview_review(&child, yi_agent_store::runtime::ReviewDecision::Approve)
        .await
        .unwrap();
    let workspace = factory
        .starts
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .workspace
        .clone()
        .expect("worker received a workspace");
    std::fs::write(workspace.path.join("review_change.txt"), "changed\n").unwrap();
    Command::new("git")
        .args(["add", "review_change.txt"])
        .current_dir(&workspace.path)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "changed after preview"])
        .current_dir(&workspace.path)
        .status()
        .unwrap();

    assert!(
        coordinator
            .confirm_review(
                &child,
                yi_agent_store::runtime::ReviewDecision::Approve,
                &preview.confirmation_token,
            )
            .await
            .is_err()
    );
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "awaiting_parent_review"
    );
}

#[tokio::test]
async fn review_preview_rejects_a_delivery_that_already_has_a_review_decision() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, _session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;
    let preview = coordinator
        .preview_review(&child, yi_agent_store::runtime::ReviewDecision::Approve)
        .await
        .unwrap();
    coordinator
        .confirm_review(
            &child,
            yi_agent_store::runtime::ReviewDecision::Approve,
            &preview.confirmation_token,
        )
        .await
        .unwrap();

    assert!(
        coordinator
            .preview_review(&child, yi_agent_store::runtime::ReviewDecision::Approve)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn coordinator_rework_persists_feedback_and_starts_the_successor_attempt() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, _session, parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;
    let previous_attempt = factory.starts.lock().unwrap()[0].attempt_id.clone();

    coordinator
        .rework_review(&child, "update the parser contract and rerun its tests")
        .await
        .unwrap();

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(
        repository.attempt_state(&previous_attempt).unwrap(),
        "rework_requested"
    );
    assert_eq!(repository.task_state(&child).unwrap(), "running");
    let successor = repository.active_attempt_id(&child).unwrap();
    assert_ne!(successor, previous_attempt);
    let starts = factory.starts.lock().unwrap();
    assert_eq!(starts.len(), 2);
    assert!(
        starts[1]
            .initial_user_messages
            .iter()
            .any(|message| message.body.contains("parser contract"))
    );
    let worker_feedback_id = starts[1].initial_user_messages[0].id.to_string();
    drop(starts);
    let mailbox = repository.mailbox_messages_for_task(&child).unwrap();
    assert_eq!(mailbox.len(), 1);
    assert_eq!(mailbox[0].kind, "rework");
    assert!(mailbox[0].payload_json.contains("parser contract"));
    assert_eq!(worker_feedback_id, mailbox[0].message_id);
    let parent_mailbox = repository.mailbox_messages_for_task(&parent).unwrap();
    assert_eq!(parent_mailbox.len(), 2);
    assert!(parent_mailbox[1].payload_json.contains("rework"));
}

#[tokio::test]
async fn failed_rework_transaction_does_not_advance_the_in_memory_supervisor() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, _session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_rework_review
             BEFORE INSERT ON reviews
             BEGIN
                 SELECT RAISE(ABORT, 'injected review failure');
             END;",
        )
        .unwrap();

    assert!(
        coordinator
            .rework_review(&child, "update the parser contract")
            .await
            .is_err()
    );
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "awaiting_parent_review"
    );

    connection
        .execute_batch("DROP TRIGGER fail_rework_review;")
        .unwrap();
    coordinator
        .rework_review(&child, "update the parser contract")
        .await
        .unwrap();

    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "running"
    );
    assert_eq!(factory.starts.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn coordinator_rejects_delivery_with_a_durable_reason() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, _session, parent, child, _delivery) =
        delivered_child_coordinator(&database, factory).await;

    coordinator
        .reject_review(&child, "the regression test is missing")
        .await
        .unwrap();

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&child).unwrap(), "blocked");
    let mailbox = repository.mailbox_messages_for_task(&child).unwrap();
    assert_eq!(mailbox.len(), 1);
    assert_eq!(mailbox[0].kind, "review_rejected");
    assert!(mailbox[0].payload_json.contains("regression test"));
    let parent_mailbox = repository.mailbox_messages_for_task(&parent).unwrap();
    assert_eq!(parent_mailbox.len(), 2);
    assert!(parent_mailbox[1].payload_json.contains("rejected"));
}

#[derive(Clone, Copy)]
enum ReviewDecision {
    Approve,
    Rework,
    Reject,
}

#[tokio::test]
async fn committed_review_decisions_reach_the_live_parent_with_the_durable_message_id() {
    for decision in [
        ReviewDecision::Approve,
        ReviewDecision::Rework,
        ReviewDecision::Reject,
    ] {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let factory = Arc::new(MessageRecordingFactory::default());
        let (coordinator, session, parent, child, _delivery) =
            delivered_child_coordinator(&database, factory.clone()).await;
        coordinator.start_worker(&session, &parent).await.unwrap();
        let parent_handle = factory.handles.lock().unwrap()[1].clone();
        let mut parent_mailbox = parent_handle.subscribe_messages();

        match decision {
            ReviewDecision::Approve => coordinator.approve_review(&child).await.unwrap(),
            ReviewDecision::Rework => coordinator
                .rework_review(&child, "rerun the parser regression suite")
                .await
                .unwrap(),
            ReviewDecision::Reject => coordinator
                .reject_review(&child, "missing parser regression evidence")
                .await
                .unwrap(),
        }

        let durable = RuntimeRepository::open(&database)
            .unwrap()
            .mailbox_messages_for_task(&parent)
            .unwrap()
            .pop()
            .expect("review transaction persists a parent notification");
        let delivered = tokio::time::timeout(Duration::from_millis(100), parent_mailbox.recv())
            .await
            .expect("committed notification wakes the live parent")
            .expect("live parent mailbox remains open");
        assert_eq!(delivered.id.to_string(), durable.message_id);
        let payload: serde_json::Value = serde_json::from_str(&durable.payload_json).unwrap();
        assert_eq!(delivered.body, payload["message"].as_str().unwrap());

        parent_handle.report_message_consumed(delivered.id.clone());
        coordinator.reconcile_worker_events().await.unwrap();
        assert!(
            RuntimeRepository::open(&database)
                .unwrap()
                .mailbox_message_delivered_at(&delivered.id)
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn rework_acknowledgement_failure_enters_controlled_recovery_before_replay() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_rework_delivery_ack
             BEFORE UPDATE OF delivered_at ON mailbox_messages
             WHEN OLD.kind = 'rework' AND NEW.delivered_at IS NOT NULL
             BEGIN
                 SELECT RAISE(ABORT, 'injected rework acknowledgement failure');
             END;",
        )
        .unwrap();

    assert!(
        coordinator
            .rework_review(&child, "rerun the parser regression suite")
            .await
            .is_err()
    );
    assert_eq!(
        factory.starts.lock().unwrap().len(),
        2,
        "the ambiguous worker start is never hidden as an unattempted delivery"
    );
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "recovery_required"
    );
    let repository = RuntimeRepository::open(&database).unwrap();
    assert!(
        !repository
            .has_active_lease_prefix(&child, "resident:")
            .unwrap()
    );
    assert!(
        repository
            .has_active_lease_prefix(&child, "workspace:")
            .unwrap()
    );
    assert!(
        repository
            .has_active_lease_prefix(&child, "worktree:")
            .unwrap()
    );
    drop(repository);
    let feedback_id: MessageId = RuntimeRepository::open(&database)
        .unwrap()
        .mailbox_messages_for_task(&child)
        .unwrap()[0]
        .message_id
        .parse()
        .unwrap();
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .mailbox_message_delivered_at(&feedback_id)
            .unwrap()
            .is_none()
    );

    connection
        .execute_batch("DROP TRIGGER fail_rework_delivery_ack;")
        .unwrap();
    drop(coordinator);
    let reopened = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    reopened.resume_task(&session, &child).await.unwrap();
    let starts = factory.starts.lock().unwrap();
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[2].initial_user_messages.len(), 1);
    assert_eq!(starts[2].initial_user_messages[0].id, feedback_id);
    drop(starts);
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .mailbox_message_delivered_at(&feedback_id)
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn failed_recovery_persistence_retains_the_admission_context_for_explicit_resume() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_rework_delivery_ack
             BEFORE UPDATE OF delivered_at ON mailbox_messages
             WHEN OLD.kind = 'rework' AND NEW.delivered_at IS NOT NULL
             BEGIN
                 SELECT RAISE(ABORT, 'injected rework acknowledgement failure');
             END;
             CREATE TRIGGER fail_recovery_transition
             BEFORE UPDATE OF state_json ON tasks
             WHEN NEW.state_json = 'recovery_required'
             BEGIN
                 SELECT RAISE(ABORT, 'injected recovery transition failure');
             END;",
        )
        .unwrap();

    assert!(
        coordinator
            .rework_review(&child, "rerun the parser regression suite")
            .await
            .is_err()
    );
    assert!(
        factory.handles.lock().unwrap()[1]
            .cancellation_token()
            .is_cancelled()
    );
    let expected: (Option<String>, Option<String>, String) = connection
        .query_row(
            "SELECT tasks.workspace_lease_id, attempts.checkpoint_json, attempts.usage_json
             FROM tasks JOIN attempts ON attempts.id = tasks.active_attempt_id
             WHERE tasks.id = ?1",
            [child.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    connection
        .execute_batch(
            "DROP TRIGGER fail_rework_delivery_ack;
             DROP TRIGGER fail_recovery_transition;",
        )
        .unwrap();
    coordinator.resume_task(&session, &child).await.unwrap();

    let preflights = factory.recovery_preflights.lock().unwrap();
    let context = &preflights
        .last()
        .expect("resume must pass the recovery gate")
        .context;
    assert_eq!(context.workspace_lease_id, expected.0);
    assert_eq!(context.worktree_lease, durable_context().worktree_lease);
    assert_eq!(context.checkpoint_json, expected.1.clone().unwrap());
    assert_eq!(context.tool_state_json, expected.2);
}

#[tokio::test]
async fn failed_recovery_transition_still_releases_the_durable_resident_lease() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_rework_delivery_ack
             BEFORE UPDATE OF delivered_at ON mailbox_messages
             WHEN OLD.kind = 'rework' AND NEW.delivered_at IS NOT NULL
             BEGIN
                 SELECT RAISE(ABORT, 'injected rework acknowledgement failure');
             END;
             CREATE TRIGGER fail_recovery_transition
             BEFORE UPDATE OF state_json ON tasks
             WHEN NEW.state_json = 'recovery_required'
             BEGIN
                 SELECT RAISE(ABORT, 'injected recovery transition failure');
             END;",
        )
        .unwrap();

    assert!(
        coordinator
            .rework_review(&child, "rerun the parser regression suite")
            .await
            .is_err()
    );
    assert!(
        factory.handles.lock().unwrap()[1]
            .cancellation_token()
            .is_cancelled()
    );
    let expected: (Option<String>, Option<String>, String) = connection
        .query_row(
            "SELECT tasks.workspace_lease_id, attempts.checkpoint_json, attempts.usage_json
             FROM tasks JOIN attempts ON attempts.id = tasks.active_attempt_id
             WHERE tasks.id = ?1",
            [child.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let feedback_id: MessageId = RuntimeRepository::open(&database)
        .unwrap()
        .mailbox_messages_for_task(&child)
        .unwrap()[0]
        .message_id
        .parse()
        .unwrap();

    let repository = RuntimeRepository::open(&database).unwrap();
    assert!(
        !repository
            .has_active_lease_prefix(&child, "resident:")
            .unwrap()
    );
    assert!(
        repository
            .has_active_lease_prefix(&child, "workspace:")
            .unwrap()
    );
    assert!(
        repository
            .has_active_lease_prefix(&child, "worktree:")
            .unwrap()
    );
    drop(repository);

    connection
        .execute_batch(
            "DROP TRIGGER fail_rework_delivery_ack;
             DROP TRIGGER fail_recovery_transition;",
        )
        .unwrap();
    drop(connection);
    drop(coordinator);
    RuntimeRepository::open(&database)
        .unwrap()
        .recover_inflight_tasks()
        .unwrap();
    let reopened = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    reopened.resume_task(&session, &child).await.unwrap();

    let starts = factory.starts.lock().unwrap();
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[2].initial_user_messages.len(), 1);
    assert_eq!(starts[2].initial_user_messages[0].id, feedback_id);
    drop(starts);
    let preflights = factory.recovery_preflights.lock().unwrap();
    let context = &preflights
        .last()
        .expect("resume must pass the recovery gate")
        .context;
    assert_eq!(context.workspace_lease_id, expected.0);
    assert_eq!(context.worktree_lease, durable_context().worktree_lease);
    assert_eq!(context.checkpoint_json, expected.1.unwrap());
    assert_eq!(context.tool_state_json, expected.2);
}

#[tokio::test]
async fn failed_rework_fallback_reports_ack_cleanup_and_recovery_failures() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, _session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory).await;
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_rework_delivery_ack
             BEFORE UPDATE OF delivered_at ON mailbox_messages
             WHEN OLD.kind = 'rework' AND NEW.delivered_at IS NOT NULL
             BEGIN
                 SELECT RAISE(ABORT, 'injected rework acknowledgement failure');
             END;
             CREATE TRIGGER fail_process_lease_cleanup
             BEFORE UPDATE OF state ON resource_leases
             WHEN OLD.resource_key LIKE 'resident:%' AND NEW.state = 'released'
             BEGIN
                 SELECT RAISE(ABORT, 'injected process lease cleanup failure');
             END;
             CREATE TRIGGER fail_recovery_transition
             BEFORE UPDATE OF state_json ON tasks
             WHEN NEW.state_json = 'recovery_required'
             BEGIN
                 SELECT RAISE(ABORT, 'injected recovery transition failure');
             END;",
        )
        .unwrap();

    let error = coordinator
        .rework_review(&child, "rerun the parser regression suite")
        .await
        .expect_err("the injected fallback failures must surface");
    let RuntimeCoordinatorError::ControlledRecoveryPersistence {
        acknowledgement,
        cleanup: Some(cleanup),
        recovery: Some(recovery),
    } = error
    else {
        panic!("expected combined controlled recovery evidence, got {error:?}");
    };
    assert!(acknowledgement.contains("injected rework acknowledgement failure"));
    assert!(cleanup.contains("injected process lease cleanup failure"));
    assert!(recovery.contains("injected recovery transition failure"));
}

#[tokio::test]
async fn cleanup_failure_rolls_back_controlled_recovery_transition() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, _session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory).await;
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_rework_delivery_ack
             BEFORE UPDATE OF delivered_at ON mailbox_messages
             WHEN OLD.kind = 'rework' AND NEW.delivered_at IS NOT NULL
             BEGIN
                 SELECT RAISE(ABORT, 'injected rework acknowledgement failure');
             END;
             CREATE TRIGGER fail_process_lease_cleanup
             BEFORE UPDATE OF state ON resource_leases
             WHEN OLD.resource_key LIKE 'resident:%' AND NEW.state = 'released'
             BEGIN
                 SELECT RAISE(ABORT, 'injected process lease cleanup failure');
             END;",
        )
        .unwrap();

    let error = coordinator
        .rework_review(&child, "rerun the parser regression suite")
        .await
        .expect_err("the injected cleanup failure must surface");
    let RuntimeCoordinatorError::ControlledRecoveryPersistence {
        acknowledgement,
        cleanup: Some(cleanup),
        recovery: None,
    } = error
    else {
        panic!("expected acknowledgement plus cleanup evidence, got {error:?}");
    };
    assert!(acknowledgement.contains("injected rework acknowledgement failure"));
    assert!(cleanup.contains("injected process lease cleanup failure"));

    let repository = RuntimeRepository::open(&database).unwrap();
    let task_state = repository.task_state(&child).unwrap();
    let resident_active = repository
        .has_active_lease_prefix(&child, "resident:")
        .unwrap();
    assert!(
        !(task_state == "recovery_required" && resident_active),
        "cleanup failure must not leave recovery_required with an active resident lease"
    );
}

#[tokio::test]
async fn restart_hydrates_a_committed_user_review_notification() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, session, parent, child, delivery) =
        delivered_child_coordinator(&database, factory).await;
    let actor_json = local_user_review_actor(&parent);
    let notification_id = MessageId::new();
    RuntimeRepository::open(&database)
        .unwrap()
        .approve_delivery_review(&child, &delivery.id, &parent, &actor_json, &notification_id)
        .unwrap();
    drop(coordinator);
    let restart_factory = Arc::new(MessageRecordingFactory::default());
    let reopened = RuntimeCoordinator::open(&database, restart_factory.clone()).unwrap();

    reopened.start_worker(&session, &parent).await.unwrap();

    let starts = restart_factory.starts.lock().unwrap();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].initial_user_messages.len(), 1);
    assert_eq!(starts[0].initial_user_messages[0].id, notification_id);
    assert!(starts[0].initial_user_messages[0].body.contains("approved"));
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "awaiting_parent_review"
    );
}

#[tokio::test]
async fn restart_hydrates_review_mail_when_the_session_contains_a_failed_task() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, session, parent, child, delivery) =
        delivered_child_coordinator(&database, factory).await;
    let notification_id = MessageId::new();
    RuntimeRepository::open(&database)
        .unwrap()
        .approve_delivery_review(
            &child,
            &delivery.id,
            &parent,
            &local_user_review_actor(&parent),
            &notification_id,
        )
        .unwrap();
    let parent_attempt = RuntimeRepository::open(&database)
        .unwrap()
        .active_attempt_id(&parent)
        .unwrap();
    Connection::open(&database)
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET state_json = 'failed' WHERE id = '{parent}';
             UPDATE attempts SET state = 'failed' WHERE id = '{parent_attempt}';"
        ))
        .unwrap();
    drop(coordinator);

    let reopened = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let mailbox = reopened.mailbox_snapshot(&session, &parent).await.unwrap();

    assert_eq!(mailbox.len(), 1);
    assert_eq!(mailbox[0].id, notification_id);
}

#[tokio::test]
async fn restart_hydrates_rework_once_with_its_durable_message_id() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, session, parent, child, delivery) =
        delivered_child_coordinator(&database, factory).await;
    let prior_attempt = RuntimeRepository::open(&database)
        .unwrap()
        .active_attempt_id(&child)
        .unwrap();
    let successor = AttemptId::new();
    let feedback_id = MessageId::new();
    let actor_json = local_user_review_actor(&parent);
    RuntimeRepository::open(&database)
        .unwrap()
        .rework_delivery_review(
            &child,
            &delivery.id,
            &parent,
            "rerun the parser regression suite",
            &feedback_id,
            &successor,
            2,
            &actor_json,
            &MessageId::new(),
        )
        .unwrap();
    drop(coordinator);
    let first_factory = Arc::new(MessageRecordingFactory::default());
    let first_restart = RuntimeCoordinator::open(&database, first_factory.clone()).unwrap();

    first_restart.start_worker(&session, &child).await.unwrap();

    {
        let first_starts = first_factory.starts.lock().unwrap();
        assert_eq!(first_starts.len(), 1);
        assert_eq!(first_starts[0].attempt_id, successor);
        assert_eq!(first_starts[0].initial_user_messages.len(), 1);
        assert_eq!(first_starts[0].initial_user_messages[0].id, feedback_id);
    }
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .mailbox_message_delivered_at(&feedback_id)
            .unwrap()
            .is_some()
    );
    drop(first_restart);
    Connection::open(&database)
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET state_json = 'queued' WHERE id = '{child}';
             UPDATE attempts SET state = 'queued' WHERE id = '{successor}';"
        ))
        .unwrap();
    let second_factory = Arc::new(MessageRecordingFactory::default());
    let second_restart = RuntimeCoordinator::open(&database, second_factory.clone()).unwrap();

    second_restart.start_worker(&session, &child).await.unwrap();

    assert!(
        second_factory.starts.lock().unwrap()[0]
            .initial_user_messages
            .is_empty()
    );
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .attempt_state(&prior_attempt)
            .unwrap(),
        "rework_requested"
    );
}

#[tokio::test]
async fn restart_hydrates_rejection_evidence_with_the_durable_message_id() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, session, parent, child, delivery) =
        delivered_child_coordinator(&database, factory).await;
    let reason_id = MessageId::new();
    let actor_json = local_user_review_actor(&parent);
    RuntimeRepository::open(&database)
        .unwrap()
        .reject_delivery_review(
            &child,
            &delivery.id,
            &parent,
            "missing regression evidence",
            &reason_id,
            &actor_json,
            &MessageId::new(),
        )
        .unwrap();
    drop(coordinator);

    let reopened = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let mailbox = reopened.mailbox_snapshot(&session, &child).await.unwrap();

    assert_eq!(mailbox.len(), 1);
    assert_eq!(mailbox[0].id, reason_id);
    assert!(matches!(
        mailbox[0].kind,
        yi_agent_core::subagent::mailbox::MessageKind::ReviewRejected(ref id) if id == &reason_id
    ));
    let durable = RuntimeRepository::open(&database)
        .unwrap()
        .mailbox_messages_for_task(&child)
        .unwrap();
    assert_eq!(durable[0].message_id, reason_id.to_string());
}

fn local_user_review_actor(parent: &yi_agent_core::TaskId) -> String {
    serde_json::to_string(&serde_json::json!({
        "kind": "task",
        "task_id": parent,
        "source": "daemon",
        "initiated_by": { "kind": "local_user", "source": "daemon" },
    }))
    .unwrap()
}

fn git_output(directory: &Path, args: &[&str]) -> Result<String, WorkerError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))?;
    if !output.status.success() {
        return Err(WorkerError::Startup(format!(
            "Git workspace error: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))
}

fn git_ok(directory: &Path, args: &[&str]) -> Result<(), WorkerError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))?;
    if !output.status.success() {
        return Err(WorkerError::Startup(format!(
            "Git workspace error: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn initialize_git_repository(directory: &Path) {
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.email", "tests@example.com"],
        vec!["config", "user.name", "Runtime Tests"],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(directory)
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(directory.join("README.md"), "base\n").unwrap();
    assert!(
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(directory)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "base"])
            .current_dir(directory)
            .status()
            .unwrap()
            .success()
    );
}

async fn delivered_child_coordinator(
    database: &std::path::Path,
    factory: Arc<MessageRecordingFactory>,
) -> (
    RuntimeCoordinator,
    RootSessionId,
    yi_agent_core::TaskId,
    yi_agent_core::TaskId,
    yi_agent_core::subagent::task::DeliveryReport,
) {
    let coordinator = RuntimeCoordinator::open(database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let parent = coordinator.root_task_id(&session).unwrap();
    if factory.workspace_service.is_some() {
        coordinator.start_worker(&session, &parent).await.unwrap();
    }
    let child = coordinator
        .spawn_child_with_objective(
            &session,
            &parent,
            "Complete the delegated task.".into(),
            ChildWriteMode::Coding,
            None,
            None,
        )
        .await
        .unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();
    let child_start = factory.starts.lock().unwrap().last().unwrap().clone();
    let workspace = child_start.workspace_lease_id.clone().unwrap();
    let commit = if let Some(worker_workspace) = child_start.workspace.as_ref() {
        std::fs::write(worker_workspace.path.join("delivery.txt"), "ready\n").unwrap();
        git_ok(&worker_workspace.path, &["add", "delivery.txt"]).unwrap();
        git_ok(&worker_workspace.path, &["commit", "-m", "child delivery"]).unwrap();
        git_output(&worker_workspace.path, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned()
    } else {
        "deadbeef".into()
    };
    let delivery = yi_agent_core::subagent::task::DeliveryReport::coding(
        commit,
        "main",
        workspace,
        "cargo test -p child",
    );
    factory
        .handles
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .report_delivery(delivery.clone());
    coordinator.reconcile_worker_events().await.unwrap();
    (coordinator, session, parent, child, delivery)
}

#[test]
fn coordinator_creates_an_isolated_root_with_its_objective_snapshot() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator
        .create_session_with_objective("Produce the scheduled report.".into())
        .unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    let detail = RuntimeRepository::open(&database)
        .unwrap()
        .task_detail(&root)
        .unwrap();
    assert_eq!(detail.session_id, session.to_string());
    let delivery = serde_json::from_str::<serde_json::Value>(&detail.delivery_json).unwrap();
    assert_eq!(delivery["objective"], "Produce the scheduled report.");
    assert!(delivery.get("schedule_policy").is_none());
}

#[test]
fn due_schedule_creates_a_new_isolated_root_with_its_objective() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let interactive = coordinator.create_session().unwrap();
    let definition = ScheduleDefinition::new("* * * * *", "Produce the scheduled report.").unwrap();
    RuntimeRepository::open(&database)
        .unwrap()
        .create_schedule(&definition, Local::now())
        .unwrap();

    let sessions = coordinator.evaluate_schedules(Local::now()).unwrap();

    assert_eq!(sessions.len(), 1);
    assert_ne!(sessions[0], interactive);
    let root = coordinator.root_task_id(&sessions[0]).unwrap();
    let detail = RuntimeRepository::open(&database)
        .unwrap()
        .task_detail(&root)
        .unwrap();
    let delivery = serde_json::from_str::<serde_json::Value>(&detail.delivery_json).unwrap();
    assert_eq!(delivery["objective"], "Produce the scheduled report.");
    assert_eq!(delivery["schedule_policy"]["runtime"]["max_turns"], 30);
}

#[test]
fn active_schedule_instance_skips_a_later_overlapping_occurrence() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let definition = ScheduleDefinition::new("* * * * *", "Produce the scheduled report.").unwrap();
    let first_due = Local::now();
    let schedule = RuntimeRepository::open(&database)
        .unwrap()
        .create_schedule(&definition, first_due)
        .unwrap();

    assert_eq!(
        coordinator.evaluate_schedules(Local::now()).unwrap().len(),
        1
    );
    let second_due = Local::now();
    RuntimeRepository::open(&database)
        .unwrap()
        .advance_schedule(&schedule.id, second_due)
        .unwrap();

    assert!(
        coordinator
            .evaluate_schedules(Local::now())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .schedule_occurrence_outcomes(&schedule.id)
            .unwrap(),
        vec!["fired", "skipped_overlap"]
    );
}

#[test]
fn offline_default_skips_elapsed_schedule_occurrences() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let now = Local::now()
        .with_second(0)
        .and_then(|value| value.with_nanosecond(0))
        .unwrap();
    let definition = ScheduleDefinition::new("* * * * *", "Produce the scheduled report.").unwrap();
    let schedule = RuntimeRepository::open(&database)
        .unwrap()
        .create_schedule(&definition, now - ChronoDuration::minutes(3))
        .unwrap();

    assert!(coordinator.evaluate_schedules(now).unwrap().is_empty());
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .schedule_occurrence_outcomes(&schedule.id)
            .unwrap(),
        vec!["missed", "missed", "missed"]
    );
}

#[test]
fn catch_up_once_fires_only_the_latest_elapsed_occurrence() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let now = Local::now()
        .with_second(0)
        .and_then(|value| value.with_nanosecond(0))
        .unwrap();
    let mut definition =
        ScheduleDefinition::new("* * * * *", "Produce the scheduled report.").unwrap();
    definition.policy.missed_run_policy = MissedRunPolicy::CatchUpOnce;
    let schedule = RuntimeRepository::open(&database)
        .unwrap()
        .create_schedule(&definition, now - ChronoDuration::minutes(3))
        .unwrap();

    assert_eq!(coordinator.evaluate_schedules(now).unwrap().len(), 1);
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .schedule_occurrence_outcomes(&schedule.id)
            .unwrap(),
        vec!["missed", "missed", "fired"]
    );
}

#[tokio::test]
async fn runtime_passes_an_llm_gate_to_a_profile_aware_worker_factory() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(ProviderGateRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();

    assert!(*factory.received_gate.lock().unwrap());
}

#[tokio::test]
async fn runtime_persists_initial_attempts_for_root_and_child_workers() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &root).await.unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();

    let starts = factory.starts.lock().unwrap();
    let root_attempt = starts
        .iter()
        .find(|start| start.task_id == root)
        .unwrap()
        .attempt_id
        .clone();
    let child_attempt = starts
        .iter()
        .find(|start| start.task_id == child)
        .unwrap()
        .attempt_id
        .clone();
    drop(starts);
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.attempt_state(&root_attempt).unwrap(), "running");
    assert_eq!(repository.attempt_state(&child_attempt).unwrap(), "running");
}

#[tokio::test]
async fn admission_persists_recovery_boundary_before_worker_start() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(
        &database,
        Arc::new(AdmissionObservingFactory {
            database: database.clone(),
        }),
    )
    .unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();
}

#[tokio::test]
async fn recovered_root_resumes_in_a_fresh_attempt_after_runtime_restart() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository
        .record_recovery_context(
            &task,
            &attempt,
            "workspace:project",
            "worktree:feature/recovery",
            r#"{"checkpoint":"before restart"}"#,
            r#"{"tool":"git","status":"clean"}"#,
        )
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);

    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    assert_eq!(coordinator.root_task_id(&session).unwrap(), task);

    coordinator.resume_task(&session, &task).await.unwrap();
    let starts = factory.starts.lock().unwrap();
    assert_eq!(starts.len(), 1);
    assert_ne!(starts[0].attempt_id, attempt);
    drop(starts);
    let events = RuntimeRepository::open(&database)
        .unwrap()
        .event_records_after(0)
        .unwrap();
    let attested = events
        .iter()
        .position(|event| event.event == RuntimeEvent::TaskRecoveryAttested)
        .unwrap();
    let started = events
        .iter()
        .rposition(|event| event.event == RuntimeEvent::TaskStarted)
        .unwrap();
    assert!(attested < started);
}

#[test]
fn recovered_successor_remains_gated_after_restart() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let interrupted_attempt = yi_agent_core::AttemptId::new();
    let successor_attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &interrupted_attempt, 1, "running")
        .unwrap();
    repository
        .record_recovery_context(
            &task,
            &interrupted_attempt,
            "workspace:test",
            "worktree:test",
            &durable_context().checkpoint_json,
            &durable_context().tool_state_json,
        )
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    repository
        .activate_successor_attempt(
            &task,
            &successor_attempt,
            2,
            "recovery_gated",
            RuntimeEvent::TaskQueued,
        )
        .unwrap();
    drop(repository);

    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();

    assert_eq!(coordinator.root_task_id(&session).unwrap(), task);
    assert_eq!(coordinator.task_state(&task).unwrap(), "recovery_gated");
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .attempt_state(&successor_attempt)
            .unwrap(),
        "recovery_gated"
    );
}

#[tokio::test]
async fn attested_successor_remains_resumable_after_restart_before_start() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "recovery_gated")
        .unwrap();
    repository
        .record_recovery_context(
            &task,
            &attempt,
            "workspace:test",
            "worktree:test",
            &durable_context().checkpoint_json,
            &durable_context().tool_state_json,
        )
        .unwrap();
    repository
        .attest_recovery_gate(&task, &attempt, &durable_attestation())
        .unwrap();
    drop(repository);

    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();

    assert_eq!(coordinator.root_task_id(&session).unwrap(), task);
    assert_eq!(coordinator.task_state(&task).unwrap(), "recovery_attested");
    coordinator.resume_task(&session, &task).await.unwrap();
    assert_eq!(coordinator.task_state(&task).unwrap(), "running");
}

#[tokio::test]
async fn recovery_attestation_is_durable_before_worker_start() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository
        .record_recovery_context(
            &task,
            &attempt,
            "workspace:test",
            "worktree:test",
            &durable_context().checkpoint_json,
            &durable_context().tool_state_json,
        )
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);
    let starts = Arc::new(Mutex::new(0));
    let coordinator = RuntimeCoordinator::open(
        &database,
        Arc::new(RecoveryStartObservingFactory {
            database: database.clone(),
            starts: starts.clone(),
        }),
    )
    .unwrap();

    coordinator.resume_task(&session, &task).await.unwrap();

    assert_eq!(*starts.lock().unwrap(), 1);
}

#[tokio::test]
async fn recovery_conflict_is_durable_without_factory_start() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);
    let factory = Arc::new(ConflictReportingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();

    coordinator.resume_task(&session, &task).await.unwrap();
    coordinator.reconcile_worker_events().await.unwrap();

    assert_eq!(*factory.starts.lock().unwrap(), 0);
    assert_eq!(coordinator.task_state(&task).unwrap(), "blocked");
    let terminal = RuntimeRepository::open(&database)
        .unwrap()
        .attempt_terminal_json_for_task(&task)
        .unwrap()
        .unwrap();
    let terminal: serde_json::Value = serde_json::from_str(&terminal).unwrap();
    assert_eq!(terminal["reason"], "recovery_conflict");
    assert!(terminal["evidence"].as_str().unwrap().contains("Git HEAD"));
}

#[tokio::test]
async fn recovery_required_task_rejects_direct_start_without_factory_action() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();

    assert!(coordinator.start_worker(&session, &task).await.is_err());

    assert!(factory.starts.lock().unwrap().is_empty());
    assert_eq!(coordinator.task_state(&task).unwrap(), "recovery_required");
}

#[tokio::test]
async fn startup_failure_retains_the_concrete_factory_error() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(StartupErrorFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();

    assert!(coordinator.start_worker(&session, &task).await.is_err());

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&task).unwrap(), "failed");
    let terminal = repository
        .attempt_terminal_json_for_task(&task)
        .unwrap()
        .unwrap();
    let terminal: serde_json::Value = serde_json::from_str(&terminal).unwrap();
    assert_eq!(terminal["reason"], "worker_start_failed");
    let error = terminal
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    assert!(
        error.contains("provider bootstrap failed"),
        "terminal evidence should include concrete startup error: {terminal}"
    );
    let attempt_state: String = Connection::open(&database)
        .unwrap()
        .query_row(
            "SELECT state FROM attempts WHERE task_id = ?1 ORDER BY number DESC LIMIT 1",
            [task.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(attempt_state, "failed");
}

#[tokio::test]
async fn startup_failure_closes_attempt_and_releases_admission_leases() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(StartupErrorFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();

    assert!(coordinator.start_worker(&session, &task).await.is_err());

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&task).unwrap(), "failed");
    assert!(
        !repository
            .has_active_lease_prefix(&task, "workspace:")
            .unwrap()
    );
    assert!(
        !repository
            .has_active_lease_prefix(&task, "worktree:")
            .unwrap()
    );
}

#[tokio::test]
async fn resident_lease_is_released_after_child_startup_failure() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(StartupErrorFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &root).await.unwrap();

    assert!(coordinator.start_worker(&session, &child).await.is_err());

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&child).unwrap(), "failed");
    assert!(
        !repository
            .has_active_lease_prefix(&child, "resident:")
            .unwrap()
    );
}

#[tokio::test]
async fn admission_persistence_failure_leaves_child_queued_and_retryable() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &root).await.unwrap();
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_all_admission_updates
             BEFORE UPDATE OF state_json ON tasks
             BEGIN
                 SELECT RAISE(ABORT, 'injected repeated sqlite failure');
             END;",
        )
        .unwrap();

    assert!(coordinator.start_worker(&session, &child).await.is_err());
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "queued"
    );
    assert!(factory.starts.lock().unwrap().is_empty());

    connection
        .execute_batch("DROP TRIGGER fail_all_admission_updates;")
        .unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();

    assert_eq!(factory.starts.lock().unwrap().len(), 1);
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "running"
    );
}

#[tokio::test]
async fn recovered_legacy_empty_attempt_task_resumes_with_a_successor_attempt() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository.create_task(&task, &session, "running").unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);

    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    assert_eq!(coordinator.root_task_id(&session).unwrap(), task);
    coordinator.resume_task(&session, &task).await.unwrap();
    assert_eq!(factory.starts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn recovered_child_resumes_after_runtime_restart() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let root = yi_agent_core::TaskId::new();
    let root_attempt = yi_agent_core::AttemptId::new();
    let child = yi_agent_core::TaskId::new();
    let child_attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&root, &session, &root_attempt, 1, "running")
        .unwrap();
    repository
        .create_child_task_with_attempt_and_objective(
            &child,
            &session,
            &root,
            1,
            &child_attempt,
            1,
            "running",
            "Preserve this recovered child objective.",
            yi_agent_core::ChildWriteMode::Coding, // Task 5/6 threads the requested mode through here.
            None,
        )
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);

    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    coordinator.resume_task(&session, &child).await.unwrap();
    assert_eq!(factory.starts.lock().unwrap()[0].task_id, child);
    assert_eq!(
        factory.starts.lock().unwrap()[0].objective,
        "Preserve this recovered child objective."
    );
}

#[tokio::test]
async fn unsafe_recovery_inspection_blocks_the_task_with_recovery_conflict() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);

    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    coordinator.resume_task(&session, &task).await.unwrap();
    factory.handles.lock().unwrap()[0]
        .report_recovery_conflict("dirty worktree cannot prove a safe base");
    coordinator.reconcile_worker_events().await.unwrap();

    assert_eq!(coordinator.task_state(&task).unwrap(), "blocked");
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .event_records_after(0)
            .unwrap()
            .iter()
            .any(|event| event.event == RuntimeEvent::TaskBlocked)
    );
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .attempt_terminal_json_for_task(&task)
            .unwrap()
            .as_deref(),
        Some(r#"{"reason":"recovery_conflict"}"#)
    );
}

#[tokio::test]
async fn cancellation_closes_the_active_attempt_before_a_restart() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &task).await.unwrap();
    let attempt = factory.starts.lock().unwrap()[0].attempt_id.clone();

    coordinator
        .cancel_task(&session, &task, false)
        .await
        .unwrap();
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.attempt_state(&attempt).unwrap(), "cancelled");
    assert!(repository.attempt_ended_at(&attempt).unwrap().is_some());
    drop(repository);

    let mut repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.recover_inflight_tasks().unwrap(), 0);
    assert_eq!(repository.attempt_state(&attempt).unwrap(), "cancelled");
}

#[tokio::test]
async fn coordinator_persists_worker_failure_reported_by_the_factory() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(FailingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();
    coordinator.reconcile_worker_events().await.unwrap();

    assert_eq!(coordinator.task_state(&root).unwrap(), "failed");
    let terminal = RuntimeRepository::open(&database)
        .unwrap()
        .attempt_terminal_json_for_task(&root)
        .unwrap()
        .expect("worker failure must persist terminal evidence");
    let terminal: serde_json::Value = serde_json::from_str(&terminal).unwrap();
    assert_eq!(terminal["reason"], "worker_failed");
    assert_eq!(terminal["error"], "provider disconnected");
}

#[tokio::test]
async fn resident_admission_persists_and_restores_the_fairness_cursor() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &root).await.unwrap();

    coordinator.start_worker(&session, &child).await.unwrap();
    let in_memory_cursor = coordinator.resident_admission_cursor();
    let cursor = RuntimeRepository::open(&database)
        .unwrap()
        .admission_cursor("resident:global")
        .unwrap();
    assert!(cursor.is_some());
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .has_active_lease_prefix(&child, "resident:")
            .unwrap()
    );
    assert_eq!(cursor.as_ref().unwrap().sequence, in_memory_cursor.sequence);
    drop(coordinator);

    let reopened = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    assert_eq!(reopened.resident_admission_cursor(), in_memory_cursor);
    drop(reopened);
}

#[tokio::test]
async fn global_resident_capacity_leaves_excess_child_queued() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let mut children = Vec::new();
    for _ in 0..17 {
        let session = coordinator.create_session().unwrap();
        let root = coordinator.root_task_id(&session).unwrap();
        let child = coordinator.spawn_child(&session, &root).await.unwrap();
        children.push((session, child));
    }

    for (session, child) in children.iter().take(16) {
        coordinator.start_worker(session, child).await.unwrap();
    }
    let (session, child) = &children[16];
    assert!(coordinator.start_worker(session, child).await.is_err());
    assert_eq!(coordinator.task_state(child).unwrap(), "queued");
}

#[tokio::test]
async fn releasing_a_resident_lease_admits_a_queued_child() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let mut children = Vec::new();
    for _ in 0..17 {
        let session = coordinator.create_session().unwrap();
        let root = coordinator.root_task_id(&session).unwrap();
        let child = coordinator.spawn_child(&session, &root).await.unwrap();
        children.push((session, child));
    }
    for (session, child) in children.iter().take(16) {
        coordinator.start_worker(session, child).await.unwrap();
    }
    let (queued_session, queued_child) = &children[16];
    assert!(
        coordinator
            .start_worker(queued_session, queued_child)
            .await
            .is_err()
    );

    // Freeing one resident slot must release its capacity so the queued child
    // becomes admissible. This pins the `release_resident_lease` bookkeeping
    // that the lock-order fix narrowed without changing its observable effect.
    let (victim_session, victim_child) = &children[0];
    coordinator
        .cancel_task(victim_session, victim_child, false)
        .await
        .unwrap();

    coordinator
        .start_worker(queued_session, queued_child)
        .await
        .unwrap();
    assert_eq!(coordinator.task_state(queued_child).unwrap(), "running");
}

#[tokio::test]
async fn fair_resident_grant_is_retained_for_the_selected_queued_child() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let mut children = Vec::new();
    for _ in 0..17 {
        let session = coordinator.create_session().unwrap();
        let root = coordinator.root_task_id(&session).unwrap();
        let child = coordinator.spawn_child(&session, &root).await.unwrap();
        children.push((session, child));
    }
    for (session, child) in children.iter().take(16) {
        coordinator.start_worker(session, child).await.unwrap();
    }
    let (waiting_session, waiting_child) = &children[16];
    assert!(
        coordinator
            .start_worker(waiting_session, waiting_child)
            .await
            .is_err()
    );

    let (released_session, released_child) = &children[0];
    coordinator
        .cancel_task(released_session, released_child, false)
        .await
        .unwrap();
    let later_session = coordinator.create_session().unwrap();
    let later_root = coordinator.root_task_id(&later_session).unwrap();
    let later_child = coordinator
        .spawn_child(&later_session, &later_root)
        .await
        .unwrap();

    assert!(
        coordinator
            .start_worker(&later_session, &later_child)
            .await
            .is_err()
    );
    coordinator
        .start_worker(waiting_session, waiting_child)
        .await
        .unwrap();
    assert_eq!(coordinator.task_state(waiting_child).unwrap(), "running");
    assert_eq!(coordinator.task_state(&later_child).unwrap(), "queued");
}

#[tokio::test]
async fn queued_capacity_rejects_without_persisting_a_child_task() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    for _ in 0..RuntimeCoordinator::DEFAULT_GLOBAL_QUEUED_SUBAGENTS {
        let session = coordinator.create_session().unwrap();
        let root = coordinator.root_task_id(&session).unwrap();
        coordinator.spawn_child(&session, &root).await.unwrap();
    }
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    assert!(matches!(
        coordinator.spawn_child(&session, &root).await,
        Err(yi_agent_store::runtime::RuntimeCoordinatorError::QueueCapacityExceeded)
    ));
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .queued_task_count()
            .unwrap(),
        64
    );
}

#[tokio::test]
async fn coordinator_retries_a_terminal_task_as_a_new_running_attempt() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();
    coordinator
        .cancel_task(&session, &root, false)
        .await
        .unwrap();

    coordinator.retry_task(&session, &root).await.unwrap();

    assert_eq!(coordinator.task_state(&root).unwrap(), "running");
}

#[tokio::test]
async fn draining_rejects_all_new_runtime_admissions() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator.begin_draining().await.unwrap();

    assert!(coordinator.create_session().is_err());
    assert!(coordinator.spawn_child(&session, &root).await.is_err());
    assert!(coordinator.start_worker(&session, &root).await.is_err());
    assert!(coordinator.retry_task(&session, &root).await.is_err());
    assert!(coordinator.resume_task(&session, &root).await.is_err());
}

#[tokio::test]
async fn draining_is_persisted_before_safe_checkpoint_requests() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(PauseRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    coordinator.begin_draining().await.unwrap();
    coordinator.request_safe_checkpoints().await.unwrap();

    let events = RuntimeRepository::open(&database)
        .unwrap()
        .event_records_after(0)
        .unwrap();
    let draining = events
        .iter()
        .position(|event| event.event == RuntimeEvent::RuntimeDraining)
        .expect("draining must be persisted before checkpoint requests");
    let pause_requested = events
        .iter()
        .position(|event| event.event == RuntimeEvent::TaskPauseRequested)
        .expect("safe checkpoint request must be persisted");
    assert!(draining < pause_requested);

    factory.handles.lock().unwrap()[0].report_paused();
    coordinator.reconcile_worker_events().await.unwrap();
    assert_eq!(coordinator.task_state(&root).unwrap(), "paused");
}

#[tokio::test]
async fn draining_interrupts_unacknowledged_workers_after_the_grace_deadline() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();
    let cancellation = coordinator
        .worker_cancellation(&session, &root)
        .await
        .unwrap();

    coordinator.begin_draining().await.unwrap();
    coordinator.request_safe_checkpoints().await.unwrap();
    coordinator
        .interrupt_unacknowledged_workers()
        .await
        .unwrap();

    assert!(cancellation.is_cancelled());
    assert_eq!(coordinator.task_state(&root).unwrap(), "recovery_required");
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .event_records_after(0)
            .unwrap()
            .iter()
            .any(|event| event.event == RuntimeEvent::TaskRecoveryRequired)
    );
}

#[tokio::test]
async fn graceful_stop_waits_for_a_cooperative_safe_checkpoint() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(PauseRecordingFactory::default());
    let coordinator = Arc::new(RuntimeCoordinator::open(&database, factory.clone()).unwrap());
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    let reporter = factory.handles.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        reporter.lock().unwrap()[0].report_paused();
    });
    let summary = coordinator
        .graceful_stop(RuntimeStopOptions {
            grace: Duration::from_millis(100),
        })
        .await
        .unwrap();

    assert_eq!(summary.paused, 1);
    assert_eq!(summary.recovery_required, 0);
    assert_eq!(coordinator.task_state(&root).unwrap(), "paused");
}

#[tokio::test]
async fn coordinator_persists_pause_only_after_worker_safe_checkpoint_acknowledgement() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(PauseRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();
    coordinator.pause_task(&session, &root).await.unwrap();

    assert_eq!(coordinator.task_state(&root).unwrap(), "running");
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&root).unwrap(), "running");
    assert!(
        repository
            .event_records_after(0)
            .unwrap()
            .iter()
            .any(|record| record.event == RuntimeEvent::TaskPauseRequested)
    );
    drop(repository);

    assert!(coordinator.resume_task(&session, &root).await.is_err());
    assert_eq!(coordinator.task_state(&root).unwrap(), "running");

    factory.handles.lock().unwrap()[0].report_paused();
    coordinator.reconcile_worker_events().await.unwrap();

    assert_eq!(coordinator.task_state(&root).unwrap(), "paused");
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&root).unwrap(), "paused");
    assert!(
        repository
            .event_records_after(0)
            .unwrap()
            .iter()
            .any(|record| record.event == RuntimeEvent::TaskPaused)
    );

    coordinator.resume_task(&session, &root).await.unwrap();
    assert_eq!(coordinator.task_state(&root).unwrap(), "running");
}

#[tokio::test]
async fn coordinator_persists_an_external_override_only_after_worker_consumes_it() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator
        .send_user_override(&root, "continue with the fix".into())
        .await
        .unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    let initial = factory.starts.lock().unwrap()[0]
        .initial_user_messages
        .clone();
    assert_eq!(initial.len(), 1);
    let message_id = initial[0].id.clone();
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .mailbox_message_delivered_at(&message_id)
            .unwrap(),
        None
    );

    factory.handles.lock().unwrap()[0].report_message_consumed(message_id.clone());
    coordinator.reconcile_worker_events().await.unwrap();

    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .mailbox_message_delivered_at(&message_id)
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn retry_replays_an_unconsumed_external_override_but_not_an_acknowledged_one() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator
        .send_user_override(&root, "keep the existing scope".into())
        .await
        .unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();
    coordinator
        .cancel_task(&session, &root, false)
        .await
        .unwrap();
    coordinator.retry_task(&session, &root).await.unwrap();

    let message_id = {
        let starts = factory.starts.lock().unwrap();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[1].initial_user_messages.len(), 1);
        starts[1].initial_user_messages[0].id.clone()
    };

    factory.handles.lock().unwrap()[1].report_message_consumed(message_id);
    coordinator.reconcile_worker_events().await.unwrap();
    coordinator
        .cancel_task(&session, &root, false)
        .await
        .unwrap();
    coordinator.retry_task(&session, &root).await.unwrap();

    assert!(
        factory.starts.lock().unwrap()[2]
            .initial_user_messages
            .is_empty()
    );
}

#[test]
fn watchdog_terminal_is_durable_once_and_releases_every_lease() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository
        .record_recovery_context(
            &task,
            &attempt,
            "workspace:watchdog",
            "worktree:watchdog",
            r#"{"git_head":"test"}"#,
            r#"{"state":"available"}"#,
        )
        .unwrap();

    let timestamp = DateTime::parse_from_rfc3339("2026-08-09T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let evidence = WatchdogEvidence {
        last_meaningful_event_id: Some(42),
        last_meaningful_at: timestamp,
        elapsed_secs: 301,
        current_wait: Some(WatchdogResourceWait {
            resource_key: "llm:test".into(),
            queued_at: timestamp,
        }),
    };
    let event_id = repository
        .record_watchdog_terminal(&task, &attempt, WatchdogTerminal::Stalled, &evidence)
        .unwrap();
    assert!(event_id.is_some());
    assert_eq!(repository.task_state(&task).unwrap(), "stalled");
    assert_eq!(repository.attempt_state(&attempt).unwrap(), "stalled");
    assert!(repository.attempt_ended_at(&attempt).unwrap().is_some());
    let terminal: serde_json::Value = serde_json::from_str(
        &repository
            .attempt_terminal_json_for_task(&task)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        terminal["watchdog"],
        serde_json::to_value(&evidence).unwrap()
    );
    assert_eq!(terminal["outcome"]["kind"], "stalled");
    assert!(
        !repository
            .has_active_lease_prefix(&task, "workspace:")
            .unwrap()
    );
    assert!(
        !repository
            .has_active_lease_prefix(&task, "worktree:")
            .unwrap()
    );

    // A repeated watchdog pass is a no-op: it cannot append a second terminal event.
    assert_eq!(
        repository
            .record_watchdog_terminal(&task, &attempt, WatchdogTerminal::Stalled, &evidence)
            .unwrap(),
        None
    );
    let stalled_events = repository
        .event_records_after(0)
        .unwrap()
        .into_iter()
        .filter(|event| event.event == RuntimeEvent::TaskStalled)
        .collect::<Vec<_>>();
    assert_eq!(stalled_events.len(), 1);
    assert_eq!(stalled_events[0].payload_json, terminal.to_string());
}

#[tokio::test]
async fn runtime_watchdog_stalls_a_running_worker_once() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &task).await.unwrap();

    let timestamp = DateTime::parse_from_rfc3339("2026-08-09T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let evidence = WatchdogEvidence {
        last_meaningful_event_id: None,
        last_meaningful_at: timestamp,
        elapsed_secs: 301,
        current_wait: None,
    };
    assert!(
        coordinator
            .record_watchdog_terminal(&session, &task, WatchdogTerminal::Stalled, evidence.clone())
            .await
            .unwrap()
    );

    assert_eq!(coordinator.task_state(&task).unwrap(), "stalled");
    assert!(
        factory.handles.lock().unwrap()[0]
            .cancellation_token()
            .is_cancelled()
    );
    let terminal: serde_json::Value = serde_json::from_str(
        &RuntimeRepository::open(&database)
            .unwrap()
            .attempt_terminal_json_for_task(&task)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        terminal["watchdog"],
        serde_json::to_value(&evidence).unwrap()
    );
    assert!(
        !coordinator
            .record_watchdog_terminal(&session, &task, WatchdogTerminal::Stalled, evidence)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn runtime_watchdog_can_timeout_a_queued_resource_wait() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();
    let timestamp = DateTime::parse_from_rfc3339("2026-08-09T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    assert!(
        coordinator
            .record_watchdog_terminal(
                &session,
                &task,
                WatchdogTerminal::TimedOut(TimeoutKind::Deadline),
                WatchdogEvidence {
                    last_meaningful_event_id: None,
                    last_meaningful_at: timestamp,
                    elapsed_secs: 60,
                    current_wait: Some(WatchdogResourceWait {
                        resource_key: "resident:global".into(),
                        queued_at: timestamp,
                    }),
                },
            )
            .await
            .unwrap()
    );
    assert_eq!(coordinator.task_state(&task).unwrap(), "timed_out");
}

#[test]
fn watchdog_terminal_variants_emit_their_distinct_event_kinds() {
    for (terminal, expected_state, expected_event) in [
        (
            WatchdogTerminal::TimedOut(TimeoutKind::WallClock),
            "timed_out",
            RuntimeEvent::TaskTimedOut,
        ),
        (
            WatchdogTerminal::BudgetExhausted(BudgetKind::Turns),
            "budget_exhausted",
            RuntimeEvent::TaskBudgetExhausted,
        ),
    ] {
        let session = RootSessionId::new();
        let task = yi_agent_core::TaskId::new();
        let attempt = yi_agent_core::AttemptId::new();
        let mut repository = RuntimeRepository::open(":memory:").unwrap();
        repository
            .create_task_with_attempt(&task, &session, &attempt, 1, "running")
            .unwrap();
        let timestamp = DateTime::parse_from_rfc3339("2026-08-09T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        repository
            .record_watchdog_terminal(
                &task,
                &attempt,
                terminal,
                &WatchdogEvidence {
                    last_meaningful_event_id: None,
                    last_meaningful_at: timestamp,
                    elapsed_secs: 1,
                    current_wait: None,
                },
            )
            .unwrap();

        assert_eq!(repository.task_state(&task).unwrap(), expected_state);
        let terminal: serde_json::Value = serde_json::from_str(
            &repository
                .attempt_terminal_json_for_task(&task)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(terminal["outcome"]["kind"], expected_state);
        assert!(
            repository
                .event_records_after(0)
                .unwrap()
                .into_iter()
                .any(|event| event.event == expected_event)
        );
    }
}

#[test]
fn attempt_watchdog_snapshot_survives_a_repository_reopen() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let timestamp = DateTime::parse_from_rfc3339("2026-08-09T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let limits = WatchdogLimits {
        max_turns: Some(30),
        max_tokens: Some(1_000),
        max_cost_micros: Some(2_000),
        max_wall_time_secs: Some(900),
        max_idle_time_secs: Some(300),
        max_resource_wait_secs: Some(60),
        max_provider_retries: Some(3),
        max_tool_retries: Some(2),
        max_rework_cycles: Some(2),
    };
    let usage = WatchdogUsage {
        turns: 4,
        tokens: 100,
        cost_micros: 30,
        provider_retries: 1,
        tool_retries: 0,
        rework_cycles: 0,
    };
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository
        .save_attempt_watchdog_snapshot(
            &task,
            &attempt,
            &limits,
            &usage,
            Some(42),
            timestamp,
            Some(&WatchdogResourceWait {
                resource_key: "llm:test".into(),
                queued_at: timestamp,
            }),
        )
        .unwrap();
    drop(repository);

    let persisted = RuntimeRepository::open(&database)
        .unwrap()
        .attempt_watchdog_snapshot(&attempt)
        .unwrap()
        .unwrap();
    assert_eq!(persisted.limits, limits);
    assert_eq!(persisted.observation.usage, usage);
    assert_eq!(persisted.last_meaningful_event_id, Some(42));
    assert_eq!(persisted.observation.last_meaningful_at, timestamp);
    assert_eq!(
        persisted.observation.resource_wait_started_at,
        Some(timestamp)
    );
    assert_eq!(
        persisted.current_wait,
        Some(WatchdogResourceWait {
            resource_key: "llm:test".into(),
            queued_at: timestamp,
        })
    );
}

#[tokio::test]
async fn coordinator_evaluates_a_persisted_watchdog_budget_once() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();
    let attempt: yi_agent_core::AttemptId = Connection::open(&database)
        .unwrap()
        .query_row(
            "SELECT active_attempt_id FROM tasks WHERE id = ?1",
            [task.to_string()],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
        .parse()
        .unwrap();
    let now = Utc::now();
    RuntimeRepository::open(&database)
        .unwrap()
        .save_attempt_watchdog_snapshot(
            &task,
            &attempt,
            &WatchdogLimits {
                max_turns: Some(1),
                max_tokens: None,
                max_cost_micros: None,
                max_wall_time_secs: None,
                max_idle_time_secs: None,
                max_resource_wait_secs: None,
                max_provider_retries: None,
                max_tool_retries: None,
                max_rework_cycles: None,
            },
            &WatchdogUsage {
                turns: 1,
                ..WatchdogUsage::default()
            },
            None,
            now,
            None,
        )
        .unwrap();

    assert_eq!(coordinator.evaluate_watchdogs(now).await.unwrap(), 1);
    assert_eq!(coordinator.task_state(&task).unwrap(), "budget_exhausted");
    assert_eq!(coordinator.evaluate_watchdogs(now).await.unwrap(), 0);
}

#[tokio::test]
async fn coordinator_seeds_watchdog_snapshots_for_root_and_child_attempts() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &root).await.unwrap();
    let repository = RuntimeRepository::open(&database).unwrap();

    for task in [&root, &child] {
        let attempt: yi_agent_core::AttemptId = Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT active_attempt_id FROM tasks WHERE id = ?1",
                [task.to_string()],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .parse()
            .unwrap();
        let snapshot = repository
            .attempt_watchdog_snapshot(&attempt)
            .unwrap()
            .expect("new attempts have a watchdog snapshot");
        assert_eq!(snapshot.limits.max_turns, Some(100));
        assert_eq!(snapshot.limits.max_wall_time_secs, Some(2_700));
        assert_eq!(snapshot.limits.max_idle_time_secs, Some(300));
        assert_eq!(snapshot.limits.max_provider_retries, Some(3));
        assert_eq!(snapshot.limits.max_tool_retries, Some(2));
        assert_eq!(snapshot.limits.max_rework_cycles, Some(2));
        assert_eq!(snapshot.observation.usage, WatchdogUsage::default());
        assert_eq!(snapshot.current_wait, None);
    }
}

#[tokio::test]
async fn coordinator_persists_worker_usage_and_meaningful_progress() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &task).await.unwrap();

    factory.handles.lock().unwrap()[0].report_provider_usage(11, 5);
    factory.handles.lock().unwrap()[0].report_provider_retry();
    factory.handles.lock().unwrap()[0].report_tool_retry();
    factory.handles.lock().unwrap()[0].report_meaningful_progress();
    coordinator.reconcile_worker_events().await.unwrap();

    let attempt: yi_agent_core::AttemptId = Connection::open(&database)
        .unwrap()
        .query_row(
            "SELECT active_attempt_id FROM tasks WHERE id = ?1",
            [task.to_string()],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
        .parse()
        .unwrap();
    let snapshot = RuntimeRepository::open(&database)
        .unwrap()
        .attempt_watchdog_snapshot(&attempt)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.observation.usage.turns, 1);
    assert_eq!(snapshot.observation.usage.tokens, 16);
    assert_eq!(snapshot.observation.usage.provider_retries, 1);
    assert_eq!(snapshot.observation.usage.tool_retries, 1);
    assert!(snapshot.last_meaningful_event_id.is_some());
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .event_records_after(0)
            .unwrap()
            .into_iter()
            .any(|event| event.event == RuntimeEvent::TaskProgress)
    );
}

#[tokio::test]
async fn coordinator_resolves_permission_with_a_daemon_owned_actor() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();
    let request = PermissionRequestId::new();
    coordinator.start_worker(&session, &task).await.unwrap();

    coordinator
        .request_permission(&session, &task, request.clone(), r#"{"tool":"shell"}"#)
        .await
        .unwrap();
    assert_eq!(
        coordinator.task_state(&task).unwrap(),
        "waiting_for_permission"
    );

    coordinator
        .resolve_permission(&request, PermissionDecision::Allow)
        .await
        .unwrap();

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(
        repository.permission_request_state(&request).unwrap(),
        "allowed"
    );
    assert_eq!(repository.task_state(&task).unwrap(), "queued");
    let event = repository
        .event_records_after(0)
        .unwrap()
        .into_iter()
        .find(|event| event.event == RuntimeEvent::PermissionResolved)
        .expect("permission resolution is audited");
    assert!(event.payload_json.contains("local_user"));
    assert!(!event.payload_json.contains(&task.to_string()));
}

/// Resolves a prepared workdir while letting the root run elsewhere, so the
/// root's row and the child's resolved row never share a workspace path.
#[derive(Clone)]
struct PreparedWorkdirService {
    in_place: WorkerWorkspace,
    prepared: WorkerWorkspace,
}

impl WorkerWorkspaceProvider for PreparedWorkdirService {
    fn in_place_workspace(
        &self,
        _root_session_id: &RootSessionId,
        _task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(self.in_place.clone())
    }

    fn workspace_in(
        &self,
        _task_id: &TaskId,
        _workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(self.prepared.clone())
    }
}

impl WorkerWorkspaceRegistry for PreparedWorkdirService {
    fn register_prepared(&self, workspace: &WorkerWorkspace) {
        let _ = workspace;
    }

    fn observe_workdir(&self, _workdir: &std::path::Path) -> Result<WorkerWorkspace, WorkerError> {
        Ok(self.prepared.clone())
    }

    fn prepared_workspace_for_workdir(&self, workdir: &std::path::Path) -> Option<WorkerWorkspace> {
        (self.prepared.path == workdir).then(|| self.prepared.clone())
    }
}

#[tokio::test]
async fn a_coding_child_runs_in_the_workdir_its_parent_prepared() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let project_root = directory.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let prepared = directory.path().join("prepared-child");
    std::fs::create_dir_all(&prepared).unwrap();
    let workspace = WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: project_root.clone(),
        path: prepared.clone(),
        branch: "feat/yi-agent-prepared-child".into(),
        parent_branch: "main".into(),
        base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
    };
    let service = Arc::new(PreparedWorkdirService {
        in_place: WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: project_root.clone(),
            path: project_root.clone(),
            branch: String::new(),
            parent_branch: String::new(),
            base_commit: String::new(),
        },
        prepared: workspace.clone(),
    });
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(service.clone() as Arc<dyn WorkerWorkspaceProvider>),
        workspace_registry: Some(service as Arc<dyn WorkerWorkspaceRegistry>),
        ..Default::default()
    });
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    // The parent prepared this directory and handed over its path; the runtime
    // must resolve it, not create a `.worktrees/...` directory of its own.
    let child = coordinator
        .spawn_child_with_objective(
            &session,
            &root,
            "implement".into(),
            ChildWriteMode::Coding,
            None,
            Some(prepared.clone()),
        )
        .await
        .unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();

    let starts = factory.starts.lock().unwrap();
    let started = starts
        .iter()
        .find(|start| start.task_id == child)
        .expect("the child worker started");
    assert_eq!(
        started.workspace.as_ref().map(|workspace| &workspace.path),
        Some(&prepared)
    );
    assert!(
        !prepared.join(".worktrees").exists(),
        "no worktree was created"
    );
}

#[tokio::test]
async fn a_root_runs_in_the_project_directory_without_creating_a_worktree() {
    let directory = TempDir::new().unwrap();
    let project_root = directory.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let database = directory.path().join("runtime.sqlite");
    let starts = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(WorkspaceObservingFactory {
        starts: Arc::clone(&starts),
        handles: Arc::new(Mutex::new(Vec::new())),
        workspace_service: Arc::new(StaticWorkspaceService {
            workspace: WorkerWorkspace {
                lease_id: WorkspaceLeaseId::new(),
                repository_root: project_root.clone(),
                path: project_root.clone(),
                branch: String::new(),
                parent_branch: String::new(),
                base_commit: String::new(),
            },
        }),
        workspace_registry: None,
    });
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();

    let starts = starts.lock().unwrap();
    assert_eq!(
        starts[0]
            .workspace
            .as_ref()
            .map(|workspace| &workspace.path),
        Some(&project_root),
        "a root runs in the project directory itself"
    );
    assert!(!project_root.join(".worktrees").exists());
}

#[tokio::test]
async fn mode_only_changes_write_access_not_the_directory() {
    let directory = TempDir::new().unwrap();
    let project_root = directory.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let database = directory.path().join("runtime.sqlite");
    let starts = Arc::new(Mutex::new(Vec::new()));
    let service = Arc::new(StaticWorkspaceService {
        workspace: WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: project_root.clone(),
            path: project_root.clone(),
            branch: String::new(),
            parent_branch: String::new(),
            base_commit: String::new(),
        },
    });
    let factory = Arc::new(WorkspaceObservingFactory {
        starts: Arc::clone(&starts),
        handles: Arc::new(Mutex::new(Vec::new())),
        workspace_service: service.clone() as Arc<dyn WorkerWorkspaceProvider>,
        workspace_registry: Some(service as Arc<dyn WorkerWorkspaceRegistry>),
    });
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    // The parent handed over a workdir; the mode alone must not change where
    // the child runs.
    let child = coordinator
        .spawn_child_with_objective(
            &session,
            &root,
            "audit".into(),
            ChildWriteMode::ReadOnly,
            None,
            Some(project_root.clone()),
        )
        .await
        .unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();

    let starts = starts.lock().unwrap();
    let started = starts.iter().find(|start| start.task_id == child).unwrap();
    assert_eq!(
        started.workspace.as_ref().map(|workspace| &workspace.path),
        Some(&project_root),
        "a read-only child still runs in its position, not a generated directory"
    );
    assert_eq!(started.workspace_mode, ChildWriteMode::ReadOnly);
    assert!(!project_root.join(".worktrees").exists());
}

#[tokio::test]
async fn read_only_child_runs_in_place_without_a_workspace_row() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let repository_root = directory.path().join("repo");
    std::fs::create_dir(&repository_root).unwrap();
    initialize_git_repository(&repository_root);
    let service = Arc::new(GitWorkspaceService::new(repository_root.clone()));
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(service.clone() as Arc<dyn WorkerWorkspaceProvider>),
        workspace_registry: Some(service as Arc<dyn WorkerWorkspaceRegistry>),
        ..Default::default()
    });
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    // `spawn_child` defaults to read-only, so the child must run in place.
    let child = coordinator.spawn_child(&session, &root).await.unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();

    let _repository = RuntimeRepository::open(&database).unwrap();

    let starts = factory.starts.lock().unwrap();
    let root_workspace = starts[0]
        .workspace
        .as_ref()
        .expect("root worker owns a workspace");
    let child_workspace = starts[1]
        .workspace
        .as_ref()
        .expect("read-only child still receives a workspace");
    assert_eq!(child_workspace.path, root_workspace.path);
    assert!(child_workspace.branch.is_empty());
    assert_eq!(starts[1].workspace_mode, ChildWriteMode::ReadOnly);
}

#[tokio::test]
async fn non_git_application_root_can_be_reattached() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let application_root = directory.path().join("not-a-repo");
    std::fs::create_dir(&application_root).unwrap();
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(Arc::new(NonGitWorkspaceService {
            repository_root: application_root.clone(),
        })),
        ..Default::default()
    });
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();

    let first = coordinator
        .attach_application_root("non-git-project", &application_root)
        .await
        .unwrap();
    // Reattaching the same non-git root must stay idempotent even though it
    // keeps no `task_workspaces` row.
    let second = coordinator
        .attach_application_root("non-git-project", &application_root)
        .await
        .unwrap();

    assert_eq!(second.session_id, first.session_id);
    assert_eq!(second.root_task_id, first.root_task_id);
    assert_eq!(second.workspace.path, application_root);
    assert!(second.workspace.branch.is_empty());
}
