//! Application-owned construction for daemon subagent workers.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use yi_agent_core::subagent::task::DeliveryReport;
use yi_agent_core::subagent::task::{
    AttemptId, ChildWriteMode, RootSessionId, TaskId, WorkspaceLeaseId,
};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryAttestation,
    WorkerRecoveryContext, WorkerRecoveryPreflight, WorkerRecoveryPreflightResult, WorkerStart,
    WorkerWorkspace, WorkerWorkspaceProvider,
};
use yi_agent_core::{
    Agent, AgentConfig, AgentError, AgentEvent, Provider, ProviderError, ProviderTurnGate, Tool,
    ToolRegistry, ToolResult,
};
use yi_agent_store::schedule::{RetryDecision, RetryFailure, evaluate_retry};

const DEFAULT_PROVIDER_RETRY_LIMIT: u16 = 3;
#[cfg(not(test))]
const TUI_WAIT_AGENT_TIMEOUT_MS: u64 = 120_000;
#[cfg(test)]
const TUI_WAIT_AGENT_TIMEOUT_MS: u64 = 10;

/// Reuses the selected provider, tool registry, and system prompt for each
/// delegated worker while the supervisor supplies its narrow objective.
pub struct DaemonAgentWorkerFactory {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
    runtime_socket: PathBuf,
    sandbox: yi_agent_tools::SandboxMode,
    sandbox_writable_roots: Vec<PathBuf>,
    workspace_service: Option<Arc<DaemonWorkspaceService>>,
    recovery_workspace: Option<PathBuf>,
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
}

impl DaemonAgentWorkerFactory {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        config: AgentConfig,
        runtime_socket: PathBuf,
    ) -> Self {
        Self {
            provider,
            tools,
            config,
            runtime_socket,
            sandbox: yi_agent_tools::SandboxMode::WorkspaceWrite,
            sandbox_writable_roots: Vec::new(),
            workspace_service: None,
            recovery_workspace: None,
            catalog: None,
        }
    }

    /// The child's model: the request's override when present, else the
    /// factory's configured model.
    fn worker_config_model(&self, requested: &str) -> String {
        if requested.trim().is_empty() {
            self.config.model.clone()
        } else {
            requested.to_owned()
        }
    }

    pub fn with_sandbox(
        mut self,
        sandbox: yi_agent_tools::SandboxMode,
        writable_roots: Vec<PathBuf>,
    ) -> Self {
        self.sandbox = sandbox;
        self.sandbox_writable_roots = writable_roots;
        self
    }

    /// Configures the Git repository from which task worktrees are created.
    pub fn with_workspace(mut self, workspace: PathBuf) -> Self {
        self.recovery_workspace = Some(workspace.clone());
        self.workspace_service = Some(Arc::new(DaemonWorkspaceService::new(workspace)));
        self
    }

    /// Refresh the skills catalog before each worker task starts.
    pub fn with_catalog(
        mut self,
        catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    ) -> Self {
        self.catalog = catalog;
        self
    }

    fn tool_registry_for_workspace(&self, workspace: PathBuf) -> ToolRegistry {
        let mut tools = (*self.tools).clone();
        yi_agent_tools::register_builtin_tools_with_sandbox(
            &mut tools,
            workspace,
            self.sandbox,
            self.sandbox_writable_roots.clone(),
        );
        tools
    }

    fn worker_tool_registry(
        &self,
        workspace: &WorkerWorkspace,
        workspace_mode: ChildWriteMode,
    ) -> ToolRegistry {
        let mut tools = (*self.tools).clone();
        let (sandbox, writable_roots) = match workspace_mode {
            ChildWriteMode::Coding => {
                let mut writable_roots = vec![workspace.path.clone()];
                writable_roots.extend(git_writable_roots_for_worktree(&workspace.path));
                (self.sandbox, writable_roots)
            }
            ChildWriteMode::ReadOnly => (yi_agent_tools::SandboxMode::ReadOnly, Vec::new()),
        };
        yi_agent_tools::register_builtin_tools_with_sandbox(
            &mut tools,
            workspace.path.clone(),
            sandbox,
            writable_roots,
        );
        tools
    }

    fn worker_tool_names_for_workspace(&self, workspace: PathBuf) -> Vec<String> {
        let mut names = self
            .tool_registry_for_workspace(workspace)
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>();
        names.extend([
            "spawn_agent".to_string(),
            "send_message".to_string(),
            "wait_agent".to_string(),
            "inspect_agent".to_string(),
            "cancel_agent".to_string(),
            "review_agent".to_string(),
        ]);
        names.sort();
        names.dedup();
        names
    }

    fn worker_tool_names(&self) -> Vec<String> {
        self.recovery_workspace
            .clone()
            .map(|workspace| self.worker_tool_names_for_workspace(workspace))
            .unwrap_or_else(|| {
                let mut names = self
                    .tools
                    .schemas()
                    .into_iter()
                    .map(|schema| schema.name)
                    .collect::<Vec<_>>();
                names.extend([
                    "spawn_agent".to_string(),
                    "send_message".to_string(),
                    "wait_agent".to_string(),
                    "inspect_agent".to_string(),
                    "cancel_agent".to_string(),
                    "review_agent".to_string(),
                ]);
                names.sort();
                names.dedup();
                names
            })
    }

    fn recovery_context_for_workspace(&self, workspace: PathBuf) -> WorkerRecoveryContext {
        let git_root = git_output(&workspace, &["rev-parse", "--show-toplevel"]);
        let git_head = git_output(&workspace, &["rev-parse", "HEAD"]);
        let git_status = git_command_output(&workspace, &["status", "--porcelain"]).ok();
        let tool_names = self.worker_tool_names_for_workspace(workspace.clone());
        WorkerRecoveryContext {
            workspace_lease_id: Some(format!("workspace:{}", workspace.display())),
            worktree_lease: git_root.map(|directory| format!("worktree:{directory}")),
            checkpoint_json: json!({
                "kind": "worker_admission",
                "git_head": git_head,
                "git_status": git_status,
            })
            .to_string(),
            tool_state_json: json!({
                "state": "available",
                "registered_tools": tool_names,
            })
            .to_string(),
        }
    }
}

pub struct DaemonWorkspaceService {
    repository_root: PathBuf,
    worktree_root: PathBuf,
    is_git_repository: bool,
    service: yi_agent_tools::worktree::WorktreeService,
}

impl DaemonWorkspaceService {
    pub fn new(workspace: PathBuf) -> Self {
        let git_root = git_output(&workspace, &["rev-parse", "--show-toplevel"]).map(PathBuf::from);
        let is_git_repository = git_root.is_some();
        let repository_root = git_root.unwrap_or(workspace);
        let worktree_root = repository_root.join(".worktrees");
        Self {
            repository_root,
            worktree_root,
            is_git_repository,
            service: yi_agent_tools::worktree::WorktreeService::new(),
        }
    }

    fn inspect_delivery_report(
        &self,
        workspace: &WorkerWorkspace,
    ) -> Result<DeliveryReport, WorkerError> {
        let delivery = self
            .service
            .inspect_delivery(&yi_agent_tools::worktree::ChildWorktree {
                path: workspace.path.clone(),
                branch: workspace.branch.clone(),
                parent_branch: workspace.parent_branch.clone(),
                base_commit: workspace.base_commit.clone(),
            })
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))?;
        let head_commit = delivery.head_commit.clone();
        Ok(DeliveryReport::coding(
            head_commit,
            workspace.parent_branch.clone(),
            workspace.lease_id.clone(),
            serde_json::json!({
                "kind": "clean_delivery",
                "branch": delivery.branch,
                "base_commit": delivery.base_commit,
                "head_commit": delivery.head_commit,
                "clean": delivery.clean,
            })
            .to_string(),
        ))
    }
}

impl WorkerWorkspaceProvider for DaemonWorkspaceService {
    fn supports_coding(&self) -> bool {
        self.is_git_repository
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

    fn in_place_workspace(
        &self,
        root_session_id: &RootSessionId,
        task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        let branch = branch_name(root_session_id, task_id, true);
        let path = self.worktree_root.join(format!(
            "yi-agent-{}-root",
            short(&root_session_id.to_string())
        ));
        let root = self
            .service
            .create_root(&self.repository_root, &branch, &path)
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))?;
        Ok(WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: self.repository_root.clone(),
            path: root.path,
            branch: root.branch,
            parent_branch: root.parent_branch,
            base_commit: root.base_commit,
        })
    }

    fn workspace_in(
        &self,
        parent: &WorkerWorkspace,
        root_session_id: &RootSessionId,
        task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        let branch = branch_name(root_session_id, task_id, false);
        let path = self.worktree_root.join(format!(
            "yi-agent-{}-{}",
            short(&root_session_id.to_string()),
            short(&task_id.to_string())
        ));
        let base = git_output(&parent.path, &["rev-parse", "HEAD"]).ok_or_else(|| {
            WorkerError::Startup("Git workspace error: parent HEAD is unavailable".into())
        })?;
        let child = self
            .service
            .create_child(&parent.path, &base, &branch, &path)
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))?;
        Ok(WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: parent.repository_root.clone(),
            path: child.path,
            branch: child.branch,
            parent_branch: child.parent_branch,
            base_commit: child.base_commit,
        })
    }

    fn inspect_delivery(&self, workspace: &WorkerWorkspace) -> Result<DeliveryReport, WorkerError> {
        self.inspect_delivery_report(workspace)
    }

    fn cleanup_prepared(&self, workspace: &WorkerWorkspace) -> Result<(), WorkerError> {
        if workspace.branch.is_empty() {
            // A read-only workspace owns no worktree or branch; its `path` is
            // the parent's view and must not be removed.
            return Ok(());
        }
        self.service
            .remove_created(
                &workspace.repository_root,
                &workspace.path,
                &workspace.branch,
            )
            .map_err(|error| WorkerError::Startup(format!("Git workspace cleanup error: {error}")))
    }

    fn contains_commit(&self, owner: &WorkerWorkspace, commit: &str) -> Result<bool, WorkerError> {
        self.service
            .contains_commit(&owner.path, commit)
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))
    }

    fn cleanup_accepted(
        &self,
        owner: &WorkerWorkspace,
        child: &WorkerWorkspace,
    ) -> Result<(), WorkerError> {
        self.service
            .remove_accepted_clean(
                &owner.path,
                &yi_agent_tools::worktree::ChildWorktree {
                    path: child.path.clone(),
                    branch: child.branch.clone(),
                    parent_branch: child.parent_branch.clone(),
                    base_commit: child.base_commit.clone(),
                },
            )
            .map_err(|error| WorkerError::Startup(format!("Git workspace cleanup error: {error}")))
    }

    fn reclaim_worktree(&self, workspace: &WorkerWorkspace) -> Result<(), WorkerError> {
        if workspace.branch.is_empty() {
            // A read-only workspace owns no worktree; its `path` is the parent's
            // view and must not be removed.
            return Ok(());
        }
        self.service
            .reclaim_directory(&workspace.repository_root, &workspace.path)
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))
    }

    fn reattach_workspace(&self, workspace: &WorkerWorkspace) -> Result<(), WorkerError> {
        if workspace.branch.is_empty() {
            return Ok(());
        }
        self.service
            .reattach_worktree(
                &workspace.repository_root,
                &workspace.path,
                &workspace.branch,
            )
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))
    }

    fn is_merged_into(
        &self,
        owner: &WorkerWorkspace,
        branch: &str,
        parent_branch: &str,
    ) -> Result<bool, WorkerError> {
        if branch.is_empty() || parent_branch.is_empty() {
            return Ok(false);
        }
        // Test ancestry against the recorded parent branch, not the owner's
        // current HEAD: a worker that ran `git checkout` inside the owner
        // worktree must not change whether a child counts as integrated.
        self.service
            .is_ancestor(&owner.path, branch, parent_branch)
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))
    }
}

fn branch_name(session: &RootSessionId, task: &TaskId, root: bool) -> String {
    if root {
        format!("feat/yi-agent-{}-root", short(&session.to_string()))
    } else {
        format!(
            "feat/yi-agent-{}-{}",
            short(&session.to_string()),
            short(&task.to_string())
        )
    }
}

fn short(value: &str) -> String {
    value.chars().filter(|ch| *ch != '-').take(8).collect()
}

fn provider_retry_failure(error: &AgentError) -> Option<RetryFailure> {
    match error {
        AgentError::Provider(ProviderError::Network(_)) => Some(RetryFailure::ProviderNetwork),
        AgentError::Provider(ProviderError::RateLimited) => Some(RetryFailure::ProviderRateLimited),
        AgentError::Provider(ProviderError::Server(_)) => Some(RetryFailure::ProviderServer),
        AgentError::Provider(ProviderError::Stream(_)) => Some(RetryFailure::ProviderStream),
        AgentError::Provider(ProviderError::Auth(_) | ProviderError::InvalidRequest(_))
        | AgentError::ProviderTurnAdmission(_)
        | AgentError::Compact(_) => None,
    }
}

fn retry_jitter_millis() -> u16 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.subsec_millis() % 250) as u16)
        .unwrap_or(0)
}

impl AgentWorkerFactory for DaemonAgentWorkerFactory {
    fn provider_profile_id(&self) -> Option<String> {
        // A daemon owns one configured provider profile; this identifier never
        // contains the API key or any other credential material.
        Some("daemon-default".into())
    }

    fn default_workspace_service(&self) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        self.workspace_service
            .as_ref()
            .map(|service| Arc::clone(service) as Arc<dyn WorkerWorkspaceProvider>)
    }

    fn workspace_service_for_project(
        &self,
        workspace: &std::path::Path,
    ) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        Some(Arc::new(DaemonWorkspaceService::new(
            workspace.to_path_buf(),
        )))
    }

    fn project_workspace_matches(
        &self,
        workspace: &std::path::Path,
        recorded: &WorkerWorkspace,
    ) -> bool {
        git_output(workspace, &["rev-parse", "--show-toplevel"]).is_some_and(|repository_root| {
            recorded.repository_root == std::path::Path::new(&repository_root)
        })
    }

    fn recovery_context(&self) -> WorkerRecoveryContext {
        self.recovery_workspace
            .clone()
            .map(|workspace| self.recovery_context_for_workspace(workspace))
            .unwrap_or_default()
    }

    fn recovery_context_for(&self, request: &WorkerStart) -> WorkerRecoveryContext {
        request
            .workspace
            .as_ref()
            .map(|workspace| self.recovery_context_for_workspace(workspace.path.clone()))
            .unwrap_or_else(|| self.recovery_context())
    }

    fn preflight_recovery(
        &self,
        request: WorkerRecoveryPreflight,
    ) -> WorkerRecoveryPreflightResult {
        let actual_tools = recovery_workspace_path(&request.context)
            .map(|workspace| self.worker_tool_names_for_workspace(workspace))
            .unwrap_or_else(|| self.worker_tool_names());
        match validate_recovery_context(&request.context, actual_tools) {
            Ok(attestation) => WorkerRecoveryPreflightResult::Attested(attestation),
            Err(reason) => WorkerRecoveryPreflightResult::Conflict(reason),
        }
    }

    fn start(
        &self,
        request: WorkerStart,
    ) -> futures::future::BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        self.start_with_provider_turn_gate(request, None)
    }

    fn start_with_provider_turn_gate(
        &self,
        request: WorkerStart,
        provider_turn_gate: Option<Arc<dyn ProviderTurnGate>>,
    ) -> futures::future::BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let provider = Arc::clone(&self.provider);
        let Some(workspace) = request.workspace.clone() else {
            return Box::pin(async {
                Err(WorkerError::Startup(
                    "worker workspace assignment is required".into(),
                ))
            });
        };
        let workspace_mode = request.workspace_mode;
        let worker_tools = Arc::new(self.worker_tool_registry(&workspace, workspace_mode));
        let mut config = self.config.clone();
        config.model = self.worker_config_model(&request.model);
        if let Some(catalog) = &self.catalog {
            if let Some(prompt) = catalog.current_system_prompt() {
                config.system_prompt = Some(prompt);
            }
        }
        let runtime_socket = self.runtime_socket.clone();
        let cancellation = request.cancellation.clone();
        let objective = request.objective;
        let initial_user_messages = request.initial_user_messages;
        let workspace_service = self.workspace_service.clone();
        let workspace_for_delivery = workspace.clone();

        Box::pin(async move {
            if objective.trim().is_empty() {
                return Err(WorkerError::Startup("worker objective is required".into()));
            }
            let handle = WorkerHandle::new(cancellation.clone());
            for message in initial_user_messages {
                handle.deliver_worker_message(message);
            }
            let reporter = handle.clone();
            let mut mailbox = handle.subscribe_messages();
            let mut pause = handle.subscribe_pause();
            let mut worker_tools = (*worker_tools).clone();
            worker_tools.register(Arc::new(DaemonSpawnAgentTool {
                runtime_socket: runtime_socket.clone(),
                session_id: request.root_session_id.to_string(),
                caller_task_id: request.task_id.to_string(),
            }));
            worker_tools.register(Arc::new(DaemonSendMessageTool {
                runtime_socket: runtime_socket.clone(),
                session_id: request.root_session_id.to_string(),
                caller_task_id: request.task_id.to_string(),
                worker_capability: request.message_capability.clone(),
            }));
            worker_tools.register(Arc::new(DaemonWaitAgentTool {
                runtime_socket: runtime_socket.clone(),
                session_id: request.root_session_id.to_string(),
                caller_task_id: request.task_id.to_string(),
                caller_capability: request.message_capability.clone(),
            }));
            worker_tools.register(Arc::new(DaemonInspectAgentTool {
                runtime_socket: runtime_socket.clone(),
                session_id: request.root_session_id.to_string(),
                caller_task_id: request.task_id.to_string(),
                caller_capability: request.message_capability.clone(),
            }));
            worker_tools.register(Arc::new(DaemonCancelAgentTool {
                runtime_socket: runtime_socket.clone(),
                session_id: request.root_session_id.to_string(),
                caller_task_id: request.task_id.to_string(),
                caller_capability: request.message_capability.clone(),
            }));
            worker_tools.register(Arc::new(DaemonReviewAgentTool {
                runtime_socket: runtime_socket.clone(),
                session_id: request.root_session_id.to_string(),
                caller_task_id: request.task_id.to_string(),
                caller_capability: request.message_capability.clone(),
            }));
            let worker_tools = Arc::new(worker_tools);
            std::thread::Builder::new()
                .name("yi-agent-subagent-worker".into())
                .spawn(move || {
                    let Ok(runtime) = tokio::runtime::Runtime::new() else {
                        reporter.report_failure("could not initialize subagent runtime");
                        return;
                    };
                    runtime.block_on(async move {
                        let mut agent = Agent::new(provider, worker_tools, config);
                        if let Some(gate) = provider_turn_gate {
                            agent = agent.with_provider_turn_gate(gate);
                        }
                        let mut prompt = objective;
                        let mut provider_retries = 0;
                        let mut requested_delivery_commit = false;
                        let mut retrying_provider = false;
                        'run: loop {
                            let stream = match if retrying_provider {
                                agent.retry_current_session().await
                            } else {
                                agent.run(prompt.clone()).await
                            } {
                                Ok(stream) => stream,
                                Err(error) => {
                                    reporter.report_failure(error.to_string());
                                    return;
                                }
                            };
                            let agent_cancellation = agent.cancel_token();
                            let mut stream = Box::pin(stream);
                            let mut cancellation_forwarded = false;
                            let mut pause_forwarded = false;
                            let mut message_prompt = None;
                            let mut assistant_report = String::new();
                            loop {
                                tokio::select! {
                                    message = mailbox.recv(), if message_prompt.is_none() => {
                                        let Some(message) = message else {
                                            reporter.report_failure("worker mailbox closed");
                                            break 'run;
                                        };
                                        // Agent cancellation rolls back an incomplete tool turn.
                                        // The next run keeps the session and adds this message.
                                        message_prompt = Some(format!(
                                            "Direct task message received. Incorporate it before continuing:\n{}",
                                            message.body,
                                        ));
                                        // The message is now bound to the next prompt. Report this
                                        // checkpoint so the daemon can durably prevent replay.
                                        reporter.report_message_consumed(message.id);
                                        agent_cancellation.cancel();
                                    },
                                    _ = cancellation.cancelled(), if !cancellation_forwarded => {
                                        agent_cancellation.cancel();
                                        cancellation_forwarded = true;
                                    },
                                    _ = pause.requested(), if !pause_forwarded => {
                                        agent_cancellation.cancel();
                                        pause_forwarded = true;
                                    },
                                    event = stream.next() => match event {
                                        Some(AgentEvent::Usage { usage, .. }) => {
                                            reporter.report_provider_usage(
                                                u64::from(usage.input_tokens),
                                                u64::from(usage.output_tokens),
                                            );
                                        }
                                        Some(AgentEvent::AssistantText(text)) => {
                                            assistant_report.push_str(&text);
                                        }
                                        Some(AgentEvent::ToolRetry { .. }) => {
                                            reporter.report_tool_retry();
                                        }
                                        Some(AgentEvent::ProviderRetry { .. }) => {
                                            reporter.report_provider_retry();
                                        }
                                        Some(AgentEvent::ToolResult { result, .. }) if !result.is_error => {
                                            // A successful tool result is external, durable progress;
                                            // generated text and streamed stdout are intentionally excluded.
                                            reporter.report_meaningful_progress();
                                        }
                                        Some(AgentEvent::Done { .. }) | None => {
                                            if pause_forwarded {
                                                reporter.report_paused();
                                                break 'run;
                                            }
                                            if let Some(next_prompt) = message_prompt.take() {
                                                prompt = next_prompt;
                                                retrying_provider = false;
                                                assistant_report.clear();
                                                continue 'run;
                                            }
                                            if workspace_mode == ChildWriteMode::Coding {
                                                if let Some(service) = workspace_service.as_ref() {
                                                    match service.inspect_delivery(&workspace_for_delivery) {
                                                        Ok(delivery) => reporter.report_delivery(delivery),
                                                        Err(error)
                                                            if !requested_delivery_commit
                                                                && is_dirty_delivery_error(&error) =>
                                                        {
                                                            requested_delivery_commit = true;
                                                            prompt = "Your worktree contains uncommitted changes, so the delivery cannot be reviewed. Run `git status --porcelain`, then stage every intended change with `git add` and create a commit with `git commit -m` in your assigned worktree. Do not only describe the commands; execute them. After committing, verify `git status --porcelain` is empty.".into();
                                                            retrying_provider = false;
                                                            assistant_report.clear();
                                                            continue 'run;
                                                        }
                                                        Err(error)
                                                            if !assistant_report.trim().is_empty()
                                                                && is_empty_delivery_error(&error) =>
                                                        {
                                                            reporter.report_completed(assistant_report.trim())
                                                        }
                                                        Err(error) => reporter.report_failure(error.to_string()),
                                                    }
                                                } else {
                                                    reporter.report_completed(assistant_report.trim());
                                                }
                                            } else {
                                                reporter.report_completed(assistant_report.trim());
                                            }
                                            break 'run;
                                        }
                                        Some(AgentEvent::Cancelled) => {
                                            if pause_forwarded {
                                                reporter.report_paused();
                                                break 'run;
                                            }
                                            if let Some(next_prompt) = message_prompt.take() {
                                                prompt = next_prompt;
                                                retrying_provider = false;
                                                assistant_report.clear();
                                                continue 'run;
                                            }
                                            reporter.report_cancelled();
                                            break 'run;
                                        }
                                        Some(AgentEvent::Error(error)) => {
                                            let retry = provider_retry_failure(&error).and_then(|failure| {
                                                match evaluate_retry(
                                                    failure,
                                                    provider_retries,
                                                    DEFAULT_PROVIDER_RETRY_LIMIT,
                                                    retry_jitter_millis(),
                                                ) {
                                                    RetryDecision::RetryAfter(delay) => Some(delay),
                                                    RetryDecision::Blocked | RetryDecision::Failed => None,
                                                }
                                            });
                                            if let Some(delay) = retry {
                                                provider_retries += 1;
                                                retrying_provider = true;
                                                reporter.report_provider_retry();
                                                tokio::select! {
                                                    _ = tokio::time::sleep(delay) => continue 'run,
                                                    _ = cancellation.cancelled() => {
                                                        reporter.report_cancelled();
                                                        break 'run;
                                                    }
                                                    _ = pause.requested() => {
                                                        reporter.report_paused();
                                                        break 'run;
                                                    }
                                                }
                                            }
                                            reporter.report_failure(error.to_string());
                                            break 'run;
                                        }
                                        Some(_) => {}
                                    }
                                }
                            }
                        }
                    });
                })
                .map_err(|error| {
                    WorkerError::Startup(format!("could not start worker thread: {error}"))
                })?;
            Ok(handle)
        })
    }
}

fn is_dirty_delivery_error(error: &WorkerError) -> bool {
    error.to_string().contains("child worktree is dirty")
}

fn is_empty_delivery_error(error: &WorkerError) -> bool {
    error
        .to_string()
        .contains("child delivery has no commits beyond")
}

fn git_writable_roots_for_worktree(workspace: &std::path::Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(git_dir) = git_dir_for_worktree(workspace) {
        roots.push(git_dir.clone());
        if let Some(common_dir) = git_output(workspace, &["rev-parse", "--git-common-dir"]) {
            let common_dir = PathBuf::from(common_dir);
            roots.push(if common_dir.is_absolute() {
                common_dir
            } else {
                workspace.join(common_dir)
            });
        }
    }
    roots
}

fn git_dir_for_worktree(workspace: &std::path::Path) -> Option<PathBuf> {
    let git_dir = git_output(workspace, &["rev-parse", "--git-dir"])?;
    let git_dir = PathBuf::from(git_dir);
    if git_dir.is_absolute() {
        Some(git_dir)
    } else {
        Some(workspace.join(git_dir))
    }
}

fn git_output(directory: &std::path::Path, args: &[&str]) -> Option<String> {
    git_command_output(directory, args)
        .ok()
        .filter(|value| !value.is_empty())
}

fn git_command_output(directory: &std::path::Path, args: &[&str]) -> Result<String, ()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .map_err(|_| ())?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .ok_or(())
}

fn recovery_workspace_path(context: &WorkerRecoveryContext) -> Option<PathBuf> {
    context
        .workspace_lease_id
        .as_deref()
        .and_then(|value| value.strip_prefix("workspace:"))
        .map(PathBuf::from)
}

/// Performs the recovery gate before an Agent or ordinary worker tool exists.
fn validate_recovery_context(
    context: &WorkerRecoveryContext,
    mut actual: Vec<String>,
) -> Result<WorkerRecoveryAttestation, String> {
    let workspace = context
        .workspace_lease_id
        .as_deref()
        .and_then(|value| value.strip_prefix("workspace:"))
        .ok_or_else(|| "recorded workspace lease is missing".to_string())?;
    let worktree = context
        .worktree_lease
        .as_deref()
        .and_then(|value| value.strip_prefix("worktree:"))
        .ok_or_else(|| "recorded worktree lease is missing".to_string())?;
    let workspace = std::path::Path::new(workspace)
        .canonicalize()
        .map_err(|_| "recorded workspace cannot be inspected".to_string())?;
    let worktree = std::path::Path::new(worktree)
        .canonicalize()
        .map_err(|_| "recorded worktree cannot be inspected".to_string())?;
    if !workspace.starts_with(&worktree) {
        return Err("recorded workspace and worktree cannot prove a safe base".into());
    }
    let checkpoint: Value = serde_json::from_str(&context.checkpoint_json)
        .map_err(|_| "recorded checkpoint is invalid".to_string())?;
    let expected_head = checkpoint
        .get("git_head")
        .and_then(Value::as_str)
        .ok_or_else(|| "recorded checkpoint has no Git HEAD".to_string())?;
    let expected_status = checkpoint
        .get("git_status")
        .and_then(Value::as_str)
        .ok_or_else(|| "recorded checkpoint has no Git status".to_string())?;
    let actual_head = git_command_output(&worktree, &["rev-parse", "HEAD"])
        .map_err(|_| "cannot inspect recorded Git HEAD".to_string())?;
    if actual_head != expected_head {
        return Err("recorded Git HEAD no longer matches checkpoint".into());
    }
    let actual_status = git_command_output(&worktree, &["status", "--porcelain"])
        .map_err(|_| "cannot inspect recorded Git status".to_string())?;
    if actual_status != expected_status {
        return Err("recorded Git status no longer matches checkpoint".into());
    }
    let tool_state: Value = serde_json::from_str(&context.tool_state_json)
        .map_err(|_| "recorded tool state is invalid".to_string())?;
    if tool_state.get("state").and_then(Value::as_str) != Some("available") {
        return Err("recorded tool state is unavailable".into());
    }
    let mut expected = tool_state
        .get("registered_tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "recorded tool list is missing".to_string())?
        .iter()
        .map(|name| {
            name.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "recorded tool list is invalid".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    expected.sort();
    actual.sort();
    if expected != actual {
        return Err("recorded tool state no longer matches the worker".into());
    }
    Ok(WorkerRecoveryAttestation {
        checkpoint_json: json!({
            "kind": "recovery_attestation",
            "git_head": actual_head,
            "git_status": actual_status,
        })
        .to_string(),
        tool_state_json: json!({
            "state": "available",
            "registered_tools": actual,
        })
        .to_string(),
        evidence_json: json!({
            "kind": "deterministic_recovery_preflight",
            "workspace": workspace,
            "worktree": worktree,
            "result": "attested",
        })
        .to_string(),
    })
}

struct DaemonApplicationSendMessageTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
    application_capability: String,
}

struct DaemonSendMessageTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
    worker_capability: String,
}

#[async_trait]
impl Tool for DaemonApplicationSendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Send a direct message to an adjacent agent."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": { "type": "string", "description": "Recipient task ID." },
                "message": { "type": "string", "description": "Message body." }
            },
            "required": ["recipient", "message"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let Some(recipient) = args.get("recipient").and_then(Value::as_str) else {
            return ToolResult::error("recipient is required");
        };
        let Some(message) = args.get("message").and_then(Value::as_str) else {
            return ToolResult::error("message is required");
        };
        if message.trim().is_empty() {
            return ToolResult::error("message must be non-empty");
        }
        match yi_agent_store::ipc::send_request(
            &self.runtime_socket,
            yi_agent_store::ipc::IpcRequest::SendApplicationMessage {
                session_id: self.session_id.clone(),
                sender_task_id: self.caller_task_id.clone(),
                capability: self.application_capability.clone(),
                recipient_task_id: recipient.to_owned(),
                message: message.to_owned(),
            },
        ) {
            Ok(yi_agent_store::ipc::IpcResponse::MessageQueued) => {
                ToolResult::text("message queued")
            }
            Ok(other) => ToolResult::error(format_ipc_rejection("message", &other)),
            Err(error) => ToolResult::error(format!("daemon is unavailable: {error}")),
        }
    }
}

#[async_trait]
impl Tool for DaemonSendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Send a structured message to a direct parent or child task."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": { "type": "string", "description": "Direct parent or child task ID." },
                "message": { "type": "string", "description": "Non-empty message body." }
            },
            "required": ["recipient", "message"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let Some(recipient) = args.get("recipient").and_then(Value::as_str) else {
            return ToolResult::error("recipient is required");
        };
        if recipient.parse::<yi_agent_core::TaskId>().is_err() {
            return ToolResult::error("recipient must be a task UUID");
        }
        let Some(message) = args.get("message").and_then(Value::as_str) else {
            return ToolResult::error("message is required");
        };
        if message.trim().is_empty() {
            return ToolResult::error("message must be non-empty");
        }
        match yi_agent_store::ipc::send_request(
            &self.runtime_socket,
            yi_agent_store::ipc::IpcRequest::SendMessage {
                session_id: self.session_id.clone(),
                sender_task_id: self.caller_task_id.clone(),
                worker_capability: self.worker_capability.clone(),
                recipient_task_id: recipient.to_owned(),
                message: message.to_owned(),
            },
        ) {
            Ok(yi_agent_store::ipc::IpcResponse::MessageQueued) => {
                ToolResult::text("message queued")
            }
            Ok(other) => ToolResult::error(format_ipc_rejection("message", &other)),
            Err(error) => ToolResult::error(format!("daemon is unavailable: {error}")),
        }
    }
}

pub fn register_application_subagent_tools(
    registry: &mut ToolRegistry,
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
    application_capability: String,
) {
    registry.register(Arc::new(DaemonApplicationSpawnAgentTool {
        runtime_socket: runtime_socket.clone(),
        session_id: session_id.clone(),
        caller_task_id: caller_task_id.clone(),
        application_capability: application_capability.clone(),
    }));
    registry.register(Arc::new(DaemonApplicationSendMessageTool {
        runtime_socket: runtime_socket.clone(),
        session_id: session_id.clone(),
        caller_task_id: caller_task_id.clone(),
        application_capability: application_capability.clone(),
    }));
    registry.register(Arc::new(DaemonWaitAgentTool {
        runtime_socket: runtime_socket.clone(),
        session_id: session_id.clone(),
        caller_task_id: caller_task_id.clone(),
        caller_capability: application_capability.clone(),
    }));
    registry.register(Arc::new(DaemonInspectAgentTool {
        runtime_socket: runtime_socket.clone(),
        session_id: session_id.clone(),
        caller_task_id: caller_task_id.clone(),
        caller_capability: application_capability.clone(),
    }));
    registry.register(Arc::new(DaemonCancelAgentTool {
        runtime_socket: runtime_socket.clone(),
        session_id: session_id.clone(),
        caller_task_id: caller_task_id.clone(),
        caller_capability: application_capability.clone(),
    }));
    registry.register(Arc::new(DaemonReviewAgentTool {
        runtime_socket,
        session_id,
        caller_task_id,
        caller_capability: application_capability,
    }));
}

fn format_ipc_rejection(action: &str, response: &yi_agent_store::ipc::IpcResponse) -> String {
    match response {
        yi_agent_store::ipc::IpcResponse::Error {
            code,
            message: Some(message),
        } => format!("daemon rejected {action}: {code}: {message}"),
        yi_agent_store::ipc::IpcResponse::Error {
            code,
            message: None,
        } => format!("daemon rejected {action}: {code}"),
        other => format!("daemon rejected {action}: {other:?}"),
    }
}

/// Resolves the optional `mode` argument for a daemon `spawn_agent` call via the
/// canonical [`ChildWriteMode::parse`], so the tool, the core tool, and the
/// IPC helper agree on the accepted spellings. An omitted mode defaults to
/// read-only; an explicit unknown or non-string value is rejected.
fn spawn_mode(args: &Value) -> Result<ChildWriteMode, ToolResult> {
    match args.get("mode") {
        None => Ok(ChildWriteMode::ReadOnly),
        Some(Value::String(value)) => ChildWriteMode::parse(value)
            .ok_or_else(|| ToolResult::error("mode must be 'coding' or 'read_only'")),
        Some(_) => Err(ToolResult::error("mode must be a string")),
    }
}

/// Resolves the optional `workdir` argument for a daemon `spawn_agent` call.
/// A blank or non-string value is rejected rather than silently ignored; an
/// omitted workdir means the runtime chooses the child's position.
fn spawn_workdir(args: &Value) -> Result<Option<String>, ToolResult> {
    match args.get("workdir") {
        None => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(Value::String(_)) => Err(ToolResult::error("workdir must not be blank")),
        Some(_) => Err(ToolResult::error("workdir must be a string")),
    }
}

/// Resolves the optional `model` argument for a daemon `spawn_agent` call.
/// An omitted model means the child inherits its parent's; a blank or
/// non-string value is rejected rather than silently ignored.
fn spawn_model(args: &Value) -> Result<Option<String>, ToolResult> {
    match args.get("model") {
        None => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(Value::String(_)) => Err(ToolResult::error("model must not be blank")),
        Some(_) => Err(ToolResult::error("model must be a string")),
    }
}

struct DaemonSpawnAgentTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
}

struct DaemonApplicationSpawnAgentTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
    application_capability: String,
}

/// Extract the child's text report from its stored terminal payload, using the
/// same `kind` marker the store writes.
fn text_completion_report(terminal_json: Option<&str>) -> Option<String> {
    let payload = serde_json::from_str::<Value>(terminal_json?).ok()?;
    (payload.get("kind").and_then(Value::as_str) == Some("text_completion"))
        .then(|| payload.get("report").and_then(Value::as_str))
        .flatten()
        .map(str::to_owned)
}

struct DaemonInspectAgentTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
    caller_capability: String,
}

struct DaemonCancelAgentTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
    caller_capability: String,
}

struct DaemonReviewAgentTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
    caller_capability: String,
}

struct DaemonWaitAgentTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
    caller_capability: String,
}

#[async_trait]
impl Tool for DaemonApplicationSpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "Create an asynchronously scheduled direct child agent."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "Delegated objective." },
                "mode": {
                    "type": "string",
                    "enum": ["coding", "read_only"],
                    "description": "Use 'coding' only when the child must change files. Defaults to 'read_only'."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model for this child. Omit to inherit yours."
                },
                "workdir": {
                    "type": "string",
                    "description": "Directory the child works in. Required for 'coding': create it yourself with `git worktree add <path> -b <branch>` first."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let Some(task) = args.get("task").and_then(Value::as_str) else {
            return ToolResult::error("task is required");
        };
        if task.trim().is_empty() {
            return ToolResult::error("task must not be empty");
        }
        let mode = match spawn_mode(&args) {
            Ok(mode) => mode,
            Err(error) => return error,
        };
        let model = match spawn_model(&args) {
            Ok(model) => model,
            Err(error) => return error,
        };
        let workdir = match spawn_workdir(&args) {
            Ok(workdir) => workdir,
            Err(error) => return error,
        };
        let response = yi_agent_store::ipc::send_request(
            &self.runtime_socket,
            yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
                session_id: self.session_id.clone(),
                parent_task_id: self.caller_task_id.clone(),
                capability: self.application_capability.clone(),
                objective: task.to_string(),
                mode: Some(mode.as_str().to_string()),
                model,
                workdir,
            },
        );
        match response {
            Ok(yi_agent_store::ipc::IpcResponse::TaskSpawned { task_id }) => ToolResult::text(
                json!({ "task_id": task_id, "objective": task, "status": "queued" }).to_string(),
            ),
            Ok(other) => ToolResult::error(format_ipc_rejection("spawn request", &other)),
            Err(error) => ToolResult::error(format!("daemon is unavailable: {error}")),
        }
    }
}

#[async_trait]
impl Tool for DaemonInspectAgentTool {
    fn name(&self) -> &str {
        "inspect_agent"
    }

    fn description(&self) -> &str {
        "Read one descendant's state, delivery and report, and optionally its diff. The task must be in your own subtree, and it may already be finished."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The child task to inspect." },
                "include_diff": {
                    "type": "boolean",
                    "default": false,
                    "description": "Also return the child's delivered diff. Off by default."
                }
            },
            "required": ["task_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let Some(task_id) = args.get("task_id").and_then(Value::as_str) else {
            return ToolResult::error("task_id is required");
        };
        if task_id.parse::<yi_agent_core::TaskId>().is_err() {
            return ToolResult::error("task_id must be a task UUID");
        }
        let include_diff = args
            .get("include_diff")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let response = yi_agent_store::ipc::send_request(
            &self.runtime_socket,
            yi_agent_store::ipc::IpcRequest::InspectChild {
                session_id: self.session_id.clone(),
                caller_task_id: self.caller_task_id.clone(),
                capability: self.caller_capability.clone(),
                task_id: task_id.to_owned(),
            },
        );
        let detail = match response {
            Ok(yi_agent_store::ipc::IpcResponse::TaskDetail(detail)) => detail,
            Ok(other) => return ToolResult::error(format_ipc_rejection("inspect request", &other)),
            Err(error) => return ToolResult::error(format!("daemon is unavailable: {error}")),
        };
        let delivery: Value = serde_json::from_str(&detail.delivery_json).unwrap_or(Value::Null);
        let report = text_completion_report(detail.terminal_json.as_deref());
        let mut payload = json!({
            "task_id": detail.task_id,
            "state": detail.state,
            "delivery": delivery,
            "report": report,
        });
        if include_diff {
            let diff = yi_agent_store::ipc::send_request(
                &self.runtime_socket,
                yi_agent_store::ipc::IpcRequest::ReadTaskDiff {
                    task_id: task_id.to_owned(),
                },
            );
            if let Ok(yi_agent_store::ipc::IpcResponse::TaskDiff { diff, .. }) = diff {
                payload["diff"] = json!(diff);
            }
        }
        ToolResult::text(payload.to_string())
    }
}

#[async_trait]
impl Tool for DaemonCancelAgentTool {
    fn name(&self) -> &str {
        "cancel_agent"
    }

    fn description(&self) -> &str {
        "Cancel one of your descendants. Use recursive to cancel its whole subtree. Only tasks in your own subtree may be cancelled."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The descendant task to cancel." },
                "recursive": {
                    "type": "boolean",
                    "default": false,
                    "description": "Also cancel the target's own descendants."
                }
            },
            "required": ["task_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let Some(task_id) = args.get("task_id").and_then(Value::as_str) else {
            return ToolResult::error("task_id is required");
        };
        if task_id.parse::<yi_agent_core::TaskId>().is_err() {
            return ToolResult::error("task_id must be a task UUID");
        }
        let recursive = args
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let response = yi_agent_store::ipc::send_request(
            &self.runtime_socket,
            yi_agent_store::ipc::IpcRequest::CancelChild {
                session_id: self.session_id.clone(),
                caller_task_id: self.caller_task_id.clone(),
                capability: self.caller_capability.clone(),
                task_id: task_id.to_owned(),
                recursive,
            },
        );
        match response {
            Ok(yi_agent_store::ipc::IpcResponse::TaskCancelled) => {
                ToolResult::text(json!({ "task_id": task_id, "status": "cancelled" }).to_string())
            }
            Ok(other) => ToolResult::error(format_ipc_rejection("cancel request", &other)),
            Err(error) => ToolResult::error(format!("daemon is unavailable: {error}")),
        }
    }
}

#[async_trait]
impl Tool for DaemonReviewAgentTool {
    fn name(&self) -> &str {
        "review_agent"
    }

    fn description(&self) -> &str {
        "Act on a direct child's delivery that is awaiting your review: send it back to rework with feedback, or reject it. Use decision \"rework\" to have the child try again with your feedback, or \"reject\" to stop it. To accept a delivery, merge the child's commit into your own branch instead; the runtime then completes the child."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Your direct child task whose delivery you are reviewing." },
                "decision": {
                    "type": "string",
                    "enum": ["rework", "reject"],
                    "description": "rework sends the child back with feedback; reject stops it."
                },
                "message": {
                    "type": "string",
                    "description": "Feedback for a rework, or the reason for a rejection. Must not be empty."
                }
            },
            "required": ["task_id", "decision", "message"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let Some(task_id) = args.get("task_id").and_then(Value::as_str) else {
            return ToolResult::error("task_id is required");
        };
        if task_id.parse::<yi_agent_core::TaskId>().is_err() {
            return ToolResult::error("task_id must be a task UUID");
        }
        let Some(message) = args.get("message").and_then(Value::as_str) else {
            return ToolResult::error("message is required");
        };
        let decision = match args.get("decision").and_then(Value::as_str) {
            Some("rework") => yi_agent_store::ipc::ChildReviewDecision::Rework {
                feedback: message.to_owned(),
            },
            Some("reject") => yi_agent_store::ipc::ChildReviewDecision::Reject {
                reason: message.to_owned(),
            },
            Some(other) => {
                return ToolResult::error(format!(
                    "decision must be \"rework\" or \"reject\", got {other:?}"
                ));
            }
            None => return ToolResult::error("decision is required"),
        };
        let response = yi_agent_store::ipc::send_request(
            &self.runtime_socket,
            yi_agent_store::ipc::IpcRequest::ReviewChild {
                session_id: self.session_id.clone(),
                caller_task_id: self.caller_task_id.clone(),
                capability: self.caller_capability.clone(),
                task_id: task_id.to_owned(),
                decision,
            },
        );
        match response {
            Ok(yi_agent_store::ipc::IpcResponse::ChildReviewAccepted) => ToolResult::text(
                json!({ "task_id": task_id, "status": "review_recorded" }).to_string(),
            ),
            Ok(other) => ToolResult::error(format_ipc_rejection("review request", &other)),
            Err(error) => ToolResult::error(format!("daemon is unavailable: {error}")),
        }
    }
}

#[async_trait]
impl Tool for DaemonWaitAgentTool {
    fn name(&self) -> &str {
        "wait_agent"
    }

    fn description(&self) -> &str {
        "Wait for one, all, or any direct child to report a terminal result."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "mode": { "type": "string", "enum": ["one", "all", "any"] } },
            "required": ["mode"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let Some(mode) = args.get("mode").and_then(Value::as_str) else {
            return ToolResult::error("mode is required");
        };
        if !matches!(mode, "one" | "any" | "all") {
            return ToolResult::error("mode must be one, any, or all");
        }
        let response = yi_agent_store::ipc::send_request(
            &self.runtime_socket,
            yi_agent_store::ipc::IpcRequest::WaitAgent {
                session_id: self.session_id.clone(),
                caller_task_id: self.caller_task_id.clone(),
                capability: self.caller_capability.clone(),
                mode: mode.to_owned(),
                timeout_ms: Some(TUI_WAIT_AGENT_TIMEOUT_MS),
            },
        );
        match response {
            Ok(yi_agent_store::ipc::IpcResponse::WaitCompleted {
                status,
                children,
                reports,
            }) => ToolResult::text(
                json!({ "status": status, "children": children, "reports": reports }).to_string(),
            ),
            Ok(other) => ToolResult::error(format_ipc_rejection("wait request", &other)),
            Err(error) => ToolResult::error(format!("daemon is unavailable: {error}")),
        }
    }
}

#[async_trait]
impl Tool for DaemonSpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "Create an asynchronously scheduled direct child agent."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "Delegated objective." },
                "mode": {
                    "type": "string",
                    "enum": ["coding", "read_only"],
                    "description": "Use 'coding' only when the child must change files. Defaults to 'read_only'."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model for this child. Omit to inherit yours."
                },
                "workdir": {
                    "type": "string",
                    "description": "Directory the child works in. Required for 'coding': create it yourself with `git worktree add <path> -b <branch>` first."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let Some(task) = args.get("task").and_then(Value::as_str) else {
            return ToolResult::error("task is required");
        };
        if task.trim().is_empty() {
            return ToolResult::error("task must not be empty");
        }
        let mode = match spawn_mode(&args) {
            Ok(mode) => mode,
            Err(error) => return error,
        };
        let model = match spawn_model(&args) {
            Ok(model) => model,
            Err(error) => return error,
        };
        let workdir = match spawn_workdir(&args) {
            Ok(workdir) => workdir,
            Err(error) => return error,
        };
        let response = yi_agent_store::ipc::send_request(
            &self.runtime_socket,
            yi_agent_store::ipc::IpcRequest::SpawnChild {
                session_id: self.session_id.clone(),
                parent_task_id: self.caller_task_id.clone(),
                objective: task.to_string(),
                mode: Some(mode.as_str().to_string()),
                model,
                workdir,
            },
        );
        match response {
            Ok(yi_agent_store::ipc::IpcResponse::TaskSpawned { task_id }) => ToolResult::text(
                json!({ "task_id": task_id, "objective": task, "status": "queued" }).to_string(),
            ),
            Ok(other) => ToolResult::error(format_ipc_rejection("spawn request", &other)),
            Err(error) => ToolResult::error(format!("daemon is unavailable: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use futures::StreamExt;
    use futures::stream::BoxStream;
    use tempfile::TempDir;
    use yi_agent_core::subagent::task::{AttemptId, MessageId, RootSessionId, TaskId};
    use yi_agent_core::subagent::worker::{
        AgentWorkerFactory, WorkerEvent, WorkerMessage, WorkerStart, WorkerWatchdogEvent,
    };
    use yi_agent_core::{ProviderError, ProviderEvent, ProviderRequest, TokenUsage};
    use yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, send_request};
    use yi_agent_store::repository::RuntimeRepository;
    use yi_agent_store::runtime::RuntimeCoordinator;
    use yi_agent_tools::worktree::WorktreeService;

    use super::*;

    struct HangingProvider;

    #[async_trait]
    impl Provider for HangingProvider {
        async fn call_stream(
            &self,
            _request: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            Ok(futures::stream::pending().boxed())
        }
    }

    #[test]
    fn ipc_rejection_formatter_includes_error_message() {
        let response = IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::InvalidState,
            message: Some("an agent may have at most four direct children".into()),
        };

        assert_eq!(
            format_ipc_rejection("spawn request", &response),
            "daemon rejected spawn request: invalid_state: an agent may have at most four direct children"
        );
    }

    #[test]
    fn worker_config_uses_the_requested_model_and_falls_back_to_the_factory_default() {
        let directory = tempfile::TempDir::new().unwrap();
        let factory_config = AgentConfig {
            model: "factory-model".into(),
            ..AgentConfig::default()
        };
        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(RecordingProvider::default()),
            Arc::new(ToolRegistry::new()),
            factory_config,
            directory.path().join("runtime.sock"),
        );

        assert_eq!(
            factory.worker_config_model("small-model"),
            "small-model",
            "a requested model overrides the factory default"
        );
        assert_eq!(
            factory.worker_config_model(""),
            "factory-model",
            "an empty request inherits the factory default"
        );
    }

    #[test]
    fn recovery_tool_names_include_every_child_orchestration_tool() {
        let directory = tempfile::TempDir::new().unwrap();
        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(RecordingProvider::default()),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            directory.path().join("runtime.sock"),
        );

        let names = factory.worker_tool_names();

        for expected in [
            "spawn_agent",
            "send_message",
            "wait_agent",
            "inspect_agent",
            "cancel_agent",
            "review_agent",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "the recovery preflight must know {expected}, got {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn review_agent_validates_its_arguments_before_touching_the_daemon() {
        let tool = DaemonReviewAgentTool {
            runtime_socket: PathBuf::from("/tmp/unused.sock"),
            session_id: "s".into(),
            caller_task_id: "c".into(),
            caller_capability: "k".into(),
        };
        let uuid = "00000000-0000-0000-0000-000000000001";
        assert!(tool.call(json!({})).await.is_error, "task_id is required");
        assert!(
            tool.call(json!({"task_id": "nope", "decision": "rework", "message": "m"}))
                .await
                .is_error,
            "task_id must be a UUID"
        );
        assert!(
            tool.call(json!({"task_id": uuid, "message": "m"}))
                .await
                .is_error,
            "decision is required"
        );
        assert!(
            tool.call(json!({"task_id": uuid, "decision": "approve", "message": "m"}))
                .await
                .is_error,
            "approve is not an agent decision: integration is the parent\'s own git action"
        );
        assert!(
            tool.call(json!({"task_id": uuid, "decision": "rework"}))
                .await
                .is_error,
            "message is required"
        );
    }

    #[tokio::test]
    async fn cancel_agent_requires_a_task_id_and_a_valid_uuid() {
        let tool = DaemonCancelAgentTool {
            runtime_socket: PathBuf::from("/tmp/unused.sock"),
            session_id: "s".into(),
            caller_task_id: "c".into(),
            caller_capability: "k".into(),
        };
        assert!(tool.call(json!({})).await.is_error, "task_id is required");
        assert!(
            tool.call(json!({"task_id": "nope"})).await.is_error,
            "task_id must be a UUID"
        );
    }

    #[tokio::test]
    async fn inspect_agent_rejects_a_missing_task_id_and_returns_a_delivery_summary() {
        let directory = tempfile::TempDir::new().unwrap();
        let socket = directory.path().join("runtime.sock");
        let tool = DaemonInspectAgentTool {
            runtime_socket: socket.clone(),
            session_id: "session".into(),
            caller_task_id: "caller".into(),
            caller_capability: "capability".into(),
        };

        let missing = tool.call(json!({})).await;
        assert!(missing.is_error, "task_id is required");

        let reached = tool.call(json!({"task_id": "not-a-uuid"})).await;
        assert!(
            reached.is_error,
            "a malformed task id fails before any daemon call"
        );
    }

    #[test]
    fn inspect_agent_schema_defaults_include_diff_to_false() {
        let schema = DaemonInspectAgentTool {
            runtime_socket: PathBuf::from("/tmp/unused.sock"),
            session_id: "s".into(),
            caller_task_id: "c".into(),
            caller_capability: "k".into(),
        }
        .schema();
        assert_eq!(schema["properties"]["include_diff"]["default"], false);
        assert_eq!(schema["required"], json!(["task_id"]));
    }

    #[test]
    fn spawn_model_accepts_a_model_and_rejects_a_blank_one() {
        assert_eq!(
            spawn_model(&json!({"model": "small-model"})).unwrap(),
            Some("small-model".to_string())
        );
        assert_eq!(spawn_model(&json!({})).unwrap(), None);
        assert!(
            spawn_model(&json!({"model": "   "})).is_err(),
            "a blank model is not a valid request"
        );
        assert!(
            spawn_model(&json!({"model": 7})).is_err(),
            "a non-string model is not a valid request"
        );
    }

    #[test]
    fn daemon_spawn_mode_uses_the_canonical_parser() {
        assert_eq!(
            spawn_mode(&json!({})).unwrap(),
            ChildWriteMode::ReadOnly,
            "an omitted mode defaults to read-only"
        );
        assert_eq!(
            spawn_mode(&json!({ "mode": "coding" })).unwrap(),
            ChildWriteMode::Coding
        );
        assert_eq!(
            spawn_mode(&json!({ "mode": "read_only" })).unwrap(),
            ChildWriteMode::ReadOnly
        );

        let unknown = spawn_mode(&json!({ "mode": "bogus" })).unwrap_err();
        assert!(unknown.is_error);
        assert!(matches!(
            unknown.content.as_slice(),
            [yi_agent_core::ContentBlock::Text(text)]
                if text.contains("mode must be 'coding' or 'read_only'")
        ));

        let non_string = spawn_mode(&json!({ "mode": 5 })).unwrap_err();
        assert!(non_string.is_error);
        assert!(matches!(
            non_string.content.as_slice(),
            [yi_agent_core::ContentBlock::Text(text)] if text.contains("mode must be a string")
        ));
    }

    #[derive(Default)]
    struct RecordingProvider {
        requests: Mutex<Vec<ProviderRequest>>,
    }

    #[async_trait]
    impl Provider for RecordingProvider {
        async fn call_stream(
            &self,
            request: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            self.requests.lock().unwrap().push(request);
            Ok(futures::stream::iter([ProviderEvent::Stop {
                reason: yi_agent_core::StopReason::EndTurn,
            }])
            .boxed())
        }
    }

    #[derive(Default)]
    struct RecordingHangingProvider {
        requests: Mutex<Vec<ProviderRequest>>,
    }

    #[derive(Default)]
    struct DirtyDeliveryProvider {
        calls: Mutex<usize>,
        requests: Mutex<Vec<ProviderRequest>>,
    }

    #[async_trait]
    impl Provider for DirtyDeliveryProvider {
        async fn call_stream(
            &self,
            request: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            self.requests.lock().unwrap().push(request);
            let mut calls = self.calls.lock().unwrap();
            let events = match *calls {
                0 => vec![
                    ProviderEvent::ToolUseStart {
                        id: "write-delivery".into(),
                        name: "bash".into(),
                    },
                    ProviderEvent::ToolUseDelta {
                        id: "write-delivery".into(),
                        partial_json: r#"{"command":"printf 'ready\\n' > delivery.txt"}"#.into(),
                    },
                    ProviderEvent::ToolUseEnd {
                        id: "write-delivery".into(),
                    },
                    ProviderEvent::Stop {
                        reason: yi_agent_core::StopReason::EndTurn,
                    },
                ],
                1 => vec![ProviderEvent::Stop {
                    reason: yi_agent_core::StopReason::EndTurn,
                }],
                2 => vec![ProviderEvent::Stop {
                    reason: yi_agent_core::StopReason::EndTurn,
                }],
                3 => vec![
                    ProviderEvent::ToolUseStart {
                        id: "commit-delivery".into(),
                        name: "bash".into(),
                    },
                    ProviderEvent::ToolUseDelta {
                        id: "commit-delivery".into(),
                        partial_json:
                            r#"{"command":"git add delivery.txt && git commit -m 'deliver'"}"#
                                .into(),
                    },
                    ProviderEvent::ToolUseEnd {
                        id: "commit-delivery".into(),
                    },
                    ProviderEvent::Stop {
                        reason: yi_agent_core::StopReason::EndTurn,
                    },
                ],
                4 | 5 => vec![ProviderEvent::Stop {
                    reason: yi_agent_core::StopReason::EndTurn,
                }],
                call => {
                    return Err(ProviderError::InvalidRequest(format!(
                        "unexpected call {call}"
                    )));
                }
            };
            *calls += 1;
            Ok(futures::stream::iter(events).boxed())
        }
    }

    struct UsageReportingProvider;

    struct TextAnswerProvider;

    #[async_trait]
    impl Provider for TextAnswerProvider {
        async fn call_stream(
            &self,
            _request: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            Ok(futures::stream::iter([
                ProviderEvent::TextDelta("sub-agent 正常完成，结果可读".into()),
                ProviderEvent::Stop {
                    reason: yi_agent_core::StopReason::EndTurn,
                },
            ])
            .boxed())
        }
    }

    #[async_trait]
    impl Provider for UsageReportingProvider {
        async fn call_stream(
            &self,
            _request: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            Ok(futures::stream::iter([
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 11,
                    output_tokens: 5,
                    ..TokenUsage::default()
                }),
                ProviderEvent::Stop {
                    reason: yi_agent_core::StopReason::EndTurn,
                },
            ])
            .boxed())
        }
    }

    #[derive(Default)]
    struct OneTransientFailureProvider {
        calls: Mutex<u8>,
        message_counts: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl Provider for OneTransientFailureProvider {
        async fn call_stream(
            &self,
            request: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            self.message_counts
                .lock()
                .unwrap()
                .push(request.messages.len());
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                return Err(ProviderError::Network("temporary disconnect".into()));
            }
            Ok(futures::stream::iter([ProviderEvent::Stop {
                reason: yi_agent_core::StopReason::EndTurn,
            }])
            .boxed())
        }
    }

    #[async_trait]
    impl Provider for RecordingHangingProvider {
        async fn call_stream(
            &self,
            request: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            self.requests.lock().unwrap().push(request);
            Ok(futures::stream::pending().boxed())
        }
    }

    /// Polls `condition` until it holds, then returns; panics after `timeout`.
    ///
    /// Work handed to a worker thread completes on its own schedule, so a test
    /// that needs to observe that work must wait for the observable effect (a
    /// recorded request, a queued event) rather than sleeping for a guessed
    /// duration. A guessed delay races whenever the machine is loaded, which is
    /// exactly how these tests flaked under parallel execution.
    async fn wait_until(condition: impl Fn() -> bool, description: &str) {
        let timeout = Duration::from_secs(10);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if condition() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out after {timeout:?} waiting for {description}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn initialize_git_repository(directory: &std::path::Path) -> String {
        for args in [
            vec!["init"],
            vec!["config", "user.email", "tests@example.com"],
            vec!["config", "user.name", "Recovery Tests"],
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
        std::fs::write(directory.join("checkpoint.txt"), "stable\n").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "checkpoint.txt"])
                .current_dir(directory)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "test checkpoint"])
                .current_dir(directory)
                .status()
                .unwrap()
                .success()
        );
        git_output(directory, &["rev-parse", "HEAD"]).unwrap()
    }

    fn worker_workspace(directory: &std::path::Path) -> WorkerWorkspace {
        WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: directory.to_path_buf(),
            path: directory.to_path_buf(),
            branch: "feat/yi-agent-test-root".into(),
            parent_branch: "main".into(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        }
    }

    #[tokio::test]
    async fn child_worker_registry_allows_git_index_writes_in_its_repository() {
        let repository = TempDir::new().unwrap();
        let base_head = initialize_git_repository(repository.path());
        let child_path = repository.path().join(".worktrees/child");
        let child = WorktreeService::new()
            .create_root(
                repository.path(),
                "feat/yi-agent-child-registry",
                &child_path,
            )
            .unwrap();
        let workspace = WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: repository.path().to_path_buf(),
            path: child.path,
            branch: child.branch,
            parent_branch: child.parent_branch,
            base_commit: base_head,
        };
        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(RecordingProvider::default()),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            repository.path().join("runtime.sock"),
        )
        .with_sandbox(yi_agent_tools::SandboxMode::WorkspaceWrite, Vec::new());
        let registry = factory.worker_tool_registry(&workspace, ChildWriteMode::Coding);
        let write = registry.get("write").expect("write tool");
        assert!(
            !write
                .call(json!({"path":"delivery.txt","content":"ready\n"}))
                .await
                .is_error
        );
        let bash = registry.get("bash").expect("bash tool");
        let add = bash.call(json!({"command":"git add delivery.txt"})).await;
        assert!(
            !add.is_error,
            "child sandbox must start Git staging: {add:?}"
        );
        assert!(
            matches!(
                add.content.as_slice(),
                [yi_agent_core::ContentBlock::Text(output)] if output.starts_with("exit: 0\n")
            ),
            "child sandbox must permit its Git index writes: {add:?}"
        );
        let commit = bash
            .call(json!({
                "command": "git -c user.name=test -c user.email=test@example.invalid commit -m delivery"
            }))
            .await;
        assert!(
            !commit.is_error,
            "child sandbox must start Git commit: {commit:?}"
        );
        assert!(
            matches!(
                commit.content.as_slice(),
                [yi_agent_core::ContentBlock::Text(output)] if output.starts_with("exit: 0\n")
            ),
            "child sandbox must permit Git commit metadata writes: {commit:?}"
        );
        assert_eq!(
            git_output(&workspace.path, &["status", "--porcelain"]),
            None,
            "a committed child delivery must leave its worktree clean",
        );
        assert_ne!(
            git_output(&workspace.path, &["rev-parse", "HEAD"]),
            Some(workspace.base_commit),
            "the child commit must advance HEAD beyond its delivery base",
        );
    }

    #[tokio::test]
    async fn child_tool_registry_enforces_workspace_sandbox_boundaries() {
        let parent = TempDir::new().unwrap();
        let child = TempDir::new().unwrap();
        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(RecordingProvider::default()),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            parent.path().join("runtime.sock"),
        )
        .with_sandbox(yi_agent_tools::SandboxMode::WorkspaceWrite, Vec::new());
        let registry = factory.tool_registry_for_workspace(child.path().to_path_buf());
        let write = registry
            .get("write")
            .expect("workspace-write includes write");

        assert!(
            !write
                .call(json!({"path":"inside.txt","content":"inside"}))
                .await
                .is_error
        );
        assert!(
            write
                .call(json!({"path":"../parent.txt","content":"escape"}))
                .await
                .is_error
        );
        assert!(
            write
                .call(json!({"path":parent.path().join("absolute.txt"),"content":"escape"}))
                .await
                .is_error
        );
        assert_eq!(
            std::fs::read_to_string(child.path().join("inside.txt")).unwrap(),
            "inside"
        );
        assert!(!parent.path().join("parent.txt").exists());
        assert!(!parent.path().join("absolute.txt").exists());

        let read_only = DaemonAgentWorkerFactory::new(
            Arc::new(RecordingProvider::default()),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            parent.path().join("runtime.sock"),
        )
        .with_sandbox(yi_agent_tools::SandboxMode::ReadOnly, Vec::new())
        .tool_registry_for_workspace(child.path().to_path_buf());
        assert!(read_only.get("write").is_none());
        assert!(read_only.get("edit").is_none());
    }

    #[test]
    fn read_only_workers_get_no_write_tools() {
        let directory = tempfile::TempDir::new().unwrap();
        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(RecordingProvider::default()),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            directory.path().join("runtime.sock"),
        )
        .with_sandbox(yi_agent_tools::SandboxMode::WorkspaceWrite, Vec::new());
        let workspace = WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: directory.path().to_path_buf(),
            path: directory.path().to_path_buf(),
            branch: String::new(),
            parent_branch: String::new(),
            base_commit: String::new(),
        };

        let read_only = factory.worker_tool_registry(&workspace, ChildWriteMode::ReadOnly);
        let read_only_names: Vec<_> = read_only
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect();
        assert!(
            !read_only_names
                .iter()
                .any(|name| name == "write" || name == "edit"),
            "read-only registry must omit write/edit, got {read_only_names:?}"
        );

        let coding = factory.worker_tool_registry(&workspace, ChildWriteMode::Coding);
        let coding_names: Vec<_> = coding
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect();
        assert!(
            coding_names.iter().any(|name| name == "write")
                && coding_names.iter().any(|name| name == "edit"),
            "coding registry must include write/edit, got {coding_names:?}"
        );
    }

    #[test]
    fn recovery_gate_attests_matching_git_and_tool_state_without_provider_action() {
        let directory = TempDir::new().unwrap();
        let head = initialize_git_repository(directory.path());
        let provider = Arc::new(RecordingProvider::default());
        let factory = DaemonAgentWorkerFactory::new(
            provider.clone(),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            directory.path().join("runtime.sock"),
        )
        .with_workspace(directory.path().to_path_buf());
        let result = factory.preflight_recovery(WorkerRecoveryPreflight {
            task_id: TaskId::new(),
            attempt_id: AttemptId::new(),
            context: factory.recovery_context(),
        });

        let WorkerRecoveryPreflightResult::Attested(attestation) = result else {
            panic!("matching recovery evidence must be attested");
        };
        assert!(attestation.checkpoint_json.contains(&head));
        assert!(
            attestation
                .evidence_json
                .contains("\"result\":\"attested\"")
        );
        assert!(provider.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn recovery_gate_rejects_mismatched_git_and_tools_without_provider_action() {
        let directory = TempDir::new().unwrap();
        initialize_git_repository(directory.path());
        let provider = Arc::new(RecordingProvider::default());
        let factory = DaemonAgentWorkerFactory::new(
            provider.clone(),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            directory.path().join("runtime.sock"),
        )
        .with_workspace(directory.path().to_path_buf());
        let mut context = factory.recovery_context();
        context.checkpoint_json = json!({
            "git_head": "0000000000000000000000000000000000000000",
            "git_status": "",
        })
        .to_string();
        let result = factory.preflight_recovery(WorkerRecoveryPreflight {
            task_id: TaskId::new(),
            attempt_id: AttemptId::new(),
            context,
        });

        assert!(matches!(
            result,
            WorkerRecoveryPreflightResult::Conflict(reason) if reason.contains("Git HEAD")
        ));
        let mut context = factory.recovery_context();
        context.tool_state_json =
            r#"{"state":"available","registered_tools":["unexpected_tool"]}"#.into();
        let result = factory.preflight_recovery(WorkerRecoveryPreflight {
            task_id: TaskId::new(),
            attempt_id: AttemptId::new(),
            context,
        });
        assert!(matches!(
            result,
            WorkerRecoveryPreflightResult::Conflict(reason) if reason.contains("tool state")
        ));
        assert!(provider.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn daemon_workspace_cleanup_removes_prepared_root_worktree_and_branch() {
        let directory = TempDir::new().unwrap();
        initialize_git_repository(directory.path());
        let service = DaemonWorkspaceService::new(directory.path().to_path_buf());
        let workspace = service
            .in_place_workspace(&RootSessionId::new(), &TaskId::new(), &AttemptId::new())
            .unwrap();
        assert!(workspace.path.exists());
        assert!(
            Command::new("git")
                .args([
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", workspace.branch)
                ])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );

        service.cleanup_prepared(&workspace).unwrap();

        assert!(!workspace.path.exists());
        assert!(
            !Command::new("git")
                .args([
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", workspace.branch)
                ])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn read_only_workspace_inherits_the_parent_path() {
        let directory = tempfile::TempDir::new().unwrap();
        let service = DaemonWorkspaceService::new(directory.path().to_path_buf());
        let parent = WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: directory.path().to_path_buf(),
            path: directory.path().join("parent-view"),
            branch: String::new(),
            parent_branch: String::new(),
            base_commit: String::new(),
        };

        let workspace = service
            .read_only_workspace(Some(&parent), &TaskId::new())
            .unwrap();
        assert_eq!(workspace.path, parent.path);
    }

    #[test]
    fn daemon_workspace_cleanup_accepted_removes_child_worktree_and_branch() {
        let directory = TempDir::new().unwrap();
        initialize_git_repository(directory.path());
        let service = DaemonWorkspaceService::new(directory.path().to_path_buf());
        let root = service
            .in_place_workspace(&RootSessionId::new(), &TaskId::new(), &AttemptId::new())
            .unwrap();
        let child = service
            .workspace_in(
                &root,
                &RootSessionId::new(),
                &TaskId::new(),
                &AttemptId::new(),
            )
            .unwrap();

        std::fs::write(child.path.join("delivery.txt"), "ready\n").unwrap();
        for args in [
            vec!["add", "delivery.txt"],
            vec!["commit", "-m", "child delivery"],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&child.path)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        // Not integrated yet: the child head is not an ancestor of the owner,
        // and refusal is git-level, leaving the worktree in place.
        assert!(!service.contains_commit(&root, &child.branch).unwrap());
        let error = service.cleanup_accepted(&root, &child).unwrap_err();
        assert!(
            error.to_string().contains("has not been merged"),
            "unexpected error: {error}"
        );
        assert!(child.path.exists());

        assert!(
            Command::new("git")
                .args(["merge", "--no-ff", &child.branch, "-m", "integrate"])
                .current_dir(&root.path)
                .status()
                .unwrap()
                .success()
        );
        assert!(service.contains_commit(&root, &child.branch).unwrap());

        service.cleanup_accepted(&root, &child).unwrap();
        assert!(!child.path.exists());
        assert!(
            !Command::new("git")
                .args([
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", child.branch)
                ])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn daemon_workspace_reclaim_removes_directory_and_keeps_branch_and_workspace() {
        let directory = TempDir::new().unwrap();
        initialize_git_repository(directory.path());
        let service = DaemonWorkspaceService::new(directory.path().to_path_buf());
        let root = service
            .in_place_workspace(&RootSessionId::new(), &TaskId::new(), &AttemptId::new())
            .unwrap();
        std::fs::write(root.path.join("delivery.txt"), "ready\n").unwrap();
        Command::new("git")
            .args(["add", "delivery.txt"])
            .current_dir(&root.path)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "root delivery"])
            .current_dir(&root.path)
            .status()
            .unwrap();
        let delivered = git_output(&root.path, &["rev-parse", "HEAD"]).unwrap();

        service.reclaim_worktree(&root).unwrap();

        assert!(!root.path.exists(), "directory is reclaimed");
        assert!(
            Command::new("git")
                .args([
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", root.branch)
                ])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success(),
            "branch ref survives the reclaim"
        );
        assert_eq!(
            git_output(directory.path(), &["rev-parse", &root.branch]).unwrap(),
            delivered
        );

        service.reattach_workspace(&root).unwrap();

        assert!(root.path.exists(), "worktree is rebuilt from the branch");
        assert_eq!(
            git_output(&root.path, &["rev-parse", "HEAD"]).unwrap(),
            delivered,
            "rebuild restores the delivered tip"
        );
    }

    /// The production merge check must answer from the recorded `parent_branch`,
    /// not from the owner worktree's current `HEAD`. This is the daemon-level
    /// counterpart of the coordinator's
    /// `reclaim_uses_the_recorded_parent_branch_not_the_owner_head`, which
    /// exercises a test double rather than this implementation.
    #[test]
    fn daemon_merge_check_uses_the_recorded_parent_branch_not_the_owner_head() {
        let directory = TempDir::new().unwrap();
        initialize_git_repository(directory.path());
        let service = DaemonWorkspaceService::new(directory.path().to_path_buf());
        let root = service
            .in_place_workspace(&RootSessionId::new(), &TaskId::new(), &AttemptId::new())
            .unwrap();
        let child = service
            .workspace_in(
                &root,
                &RootSessionId::new(),
                &TaskId::new(),
                &AttemptId::new(),
            )
            .unwrap();

        std::fs::write(child.path.join("delivery.txt"), "ready\n").unwrap();
        for args in [
            vec!["add", "delivery.txt"],
            vec!["commit", "-m", "child delivery"],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&child.path)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        // A child with its own unmerged commit is not yet integrated. (A freshly
        // created child branch points at the parent's HEAD and is trivially an
        // ancestor, so the commit is what makes this assertion meaningful.)
        assert!(
            !service
                .is_merged_into(&root, &child.branch, &child.parent_branch)
                .unwrap(),
            "an unmerged child is not reported as merged"
        );

        assert!(
            Command::new("git")
                .args([
                    "merge",
                    "--no-ff",
                    &child.branch,
                    "-m",
                    "integrate the child",
                ])
                .current_dir(&root.path)
                .status()
                .unwrap()
                .success()
        );

        assert!(
            service
                .is_merged_into(&root, &child.branch, &child.parent_branch)
                .unwrap(),
            "a merged child is reported as merged"
        );

        // Move the owner off its own branch, onto a commit that predates the
        // merge. A HEAD-relative check would now answer "not merged"; the
        // parent_branch-relative check must still answer "merged".
        let side_track = "owner-side-track";
        assert!(
            Command::new("git")
                .args(["checkout", "-b", side_track, &root.base_commit])
                .current_dir(&root.path)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            git_output(&root.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            side_track
        );
        assert!(
            service
                .is_merged_into(&root, &child.branch, &child.parent_branch)
                .unwrap(),
            "the answer does not depend on the owner's current HEAD"
        );

        // Empty inputs never authorize a reclaim.
        assert!(
            !service
                .is_merged_into(&root, "", &child.parent_branch)
                .unwrap(),
            "an empty child branch is never merged"
        );
        assert!(
            !service.is_merged_into(&root, &child.branch, "").unwrap(),
            "an empty parent branch is never merged"
        );
    }

    #[tokio::test]
    async fn restarted_daemon_worker_receives_durable_recovery_evidence_from_normal_work() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        initialize_git_repository(&workspace);
        let provider = Arc::new(RecordingHangingProvider::default());
        let factory = Arc::new(
            DaemonAgentWorkerFactory::new(
                provider.clone(),
                Arc::new(ToolRegistry::new()),
                AgentConfig::default(),
                directory.path().join("runtime.sock"),
            )
            .with_workspace(workspace),
        );
        let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
        let session = coordinator.create_session().unwrap();
        let task = coordinator.root_task_id(&session).unwrap();
        coordinator.start_worker(&session, &task).await.unwrap();
        let first_worker = coordinator
            .worker_cancellation(&session, &task)
            .await
            .unwrap();

        // The worker runs on its own thread, so `start_worker` returning says
        // nothing about whether it has reached the provider yet. Cancel only
        // after the first turn is recorded: cancelling earlier can land before
        // the worker ever issues its request, and then `requests[0]` is never
        // written and the assertion below can never be satisfied.
        wait_until(
            || !provider.requests.lock().unwrap().is_empty(),
            "the first worker to issue its provider request",
        )
        .await;

        RuntimeRepository::open(&database)
            .unwrap()
            .recover_inflight_tasks()
            .unwrap();
        first_worker.cancel();

        let restarted = RuntimeCoordinator::open(&database, factory).unwrap();
        restarted.resume_task(&session, &task).await.unwrap();

        wait_until(
            || provider.requests.lock().unwrap().len() == 2,
            "the resumed worker to issue its provider request",
        )
        .await;

        let requests = provider.requests.lock().unwrap();
        let resumed_prompt = match &requests[1].messages[0].content[..] {
            [yi_agent_core::ContentBlock::Text(text)] => text,
            content => panic!("expected a text-only resumed prompt, got {content:?}"),
        };
        assert!(resumed_prompt.contains("Root session objective not specified."));
        assert!(!resumed_prompt.contains("RECOVERY_CONTROLLER"));
    }

    #[tokio::test]
    async fn daemon_worker_reports_consumption_after_preloaded_message_reaches_prompt_checkpoint() {
        let message_id = MessageId::new();
        let directory = TempDir::new().unwrap();
        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(HangingProvider),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            directory.path().join("runtime.sock"),
        );
        let request = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
            .with_objective("Continue the delegated task.")
            .with_workspace(worker_workspace(directory.path()))
            .with_initial_user_messages(vec![WorkerMessage {
                id: message_id.clone(),
                body: "continue with the fix".into(),
            }]);
        let handle = factory.start(request).await.unwrap();

        let events = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let events = handle.take_events();
                if events
                    .iter()
                    .any(|event| matches!(event, WorkerEvent::MessageConsumed { message_id: id } if id == &message_id))
                {
                    return events;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker should consume its preloaded override");
        assert!(events.iter().any(
            |event| matches!(event, WorkerEvent::MessageConsumed { message_id: id } if id == &message_id)
        ));
        handle.cancel();
    }

    #[tokio::test]
    async fn daemon_worker_requests_a_commit_for_a_dirty_delivery_before_reporting_failure() {
        let directory = TempDir::new().unwrap();
        let base_head = initialize_git_repository(directory.path());
        let root_path = directory.path().join(".worktrees/yi-root");
        let root = WorktreeService::new()
            .create_root(directory.path(), "feat/yi-agent-dirty-delivery", &root_path)
            .unwrap();
        let workspace = WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: directory.path().to_path_buf(),
            path: root.path.clone(),
            branch: root.branch.clone(),
            parent_branch: root.parent_branch.clone(),
            base_commit: base_head,
        };
        let provider = Arc::new(DirtyDeliveryProvider::default());
        let factory = DaemonAgentWorkerFactory::new(
            provider.clone(),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            directory.path().join("runtime.sock"),
        )
        .with_workspace(directory.path().to_path_buf());
        let handle = factory
            .start(
                WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
                    .with_objective("Create and commit delivery.txt.")
                    .with_workspace_mode(ChildWriteMode::Coding)
                    .with_workspace(workspace.clone()),
            )
            .await
            .unwrap();

        let delivery = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let events = handle.take_events();
                if let Some(delivery) = events.into_iter().find_map(|event| match event {
                    WorkerEvent::Delivered(delivery) => Some(delivery),
                    WorkerEvent::Failed(error) => panic!(
                        "worker failed instead of requesting a commit: {error}; status={:?}; log={:?}",
                        git_output(&root.path, &["status", "--porcelain"]),
                        git_output(&root.path, &["log", "-1", "--format=%B"]),
                    ),
                    _ => None,
                }) {
                    return delivery;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker should repair the dirty delivery and report it");

        assert_eq!(delivery.workspace, workspace.lease_id);
        assert_eq!(
            git_output(&root.path, &["status", "--porcelain"]),
            None,
            "committed delivery must leave the worktree clean",
        );
        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            6,
            "the worker must continue through write, dirty inspection, recovery commit, and post-commit turns",
        );
        let recovery_prompt = requests[3]
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|content| match content {
                yi_agent_core::ContentBlock::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .find(|text| text.contains("git commit -m"));
        assert!(
            recovery_prompt.is_some(),
            "the commit turn must be prompted by dirty-delivery recovery: {:?}",
            requests[3].messages,
        );
    }

    #[tokio::test]
    async fn daemon_worker_reports_a_structured_delivery_when_it_completes_cleanly() {
        let directory = TempDir::new().unwrap();
        let base_head = initialize_git_repository(directory.path());
        let root_path = directory.path().join(".worktrees/yi-root");
        let root = WorktreeService::new()
            .create_root(directory.path(), "feat/yi-agent-test-root", &root_path)
            .unwrap();
        std::fs::write(root.path.join("delivery.txt"), "ready\n").unwrap();
        Command::new("git")
            .args(["add", "delivery.txt"])
            .current_dir(&root.path)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "prepared delivery"])
            .current_dir(&root.path)
            .status()
            .unwrap();
        let head = git_output(&root.path, &["rev-parse", "HEAD"]).unwrap();
        let workspace = WorkerWorkspace {
            lease_id: yi_agent_core::subagent::task::WorkspaceLeaseId::new(),
            repository_root: directory.path().to_path_buf(),
            path: root.path.clone(),
            branch: root.branch.clone(),
            parent_branch: root.parent_branch.clone(),
            base_commit: base_head,
        };
        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(RecordingProvider::default()),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            directory.path().join("runtime.sock"),
        )
        .with_workspace(directory.path().to_path_buf());
        let request = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
            .with_objective("Return immediately after one clean provider turn.")
            .with_workspace(workspace.clone())
            .with_workspace_mode(ChildWriteMode::Coding);
        let handle = factory.start(request).await.unwrap();

        let delivery = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let events = handle.take_events();
                if let Some(delivery) = events.into_iter().find_map(|event| match event {
                    WorkerEvent::Delivered(delivery) => Some(delivery),
                    WorkerEvent::CompletedWithoutDelivery => {
                        panic!("worker completed without a structured delivery report")
                    }
                    _ => None,
                }) {
                    return delivery;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker should complete and report a delivery");

        assert_eq!(delivery.commit, head);
        assert_eq!(delivery.base_ref, workspace.parent_branch);
        assert_eq!(delivery.workspace, workspace.lease_id);
    }

    #[tokio::test]
    async fn daemon_worker_reports_provider_usage_to_the_watchdog() {
        let directory = TempDir::new().unwrap();
        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(UsageReportingProvider),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            directory.path().join("runtime.sock"),
        );
        let request = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
            .with_objective("Complete the delegated task.")
            .with_workspace(worker_workspace(directory.path()));
        let handle = factory.start(request).await.unwrap();

        let events = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let events = handle.take_watchdog_events();
                if !events.is_empty() {
                    return events;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker should report provider usage");
        assert_eq!(
            events,
            vec![WorkerWatchdogEvent::ProviderUsage {
                input_tokens: 11,
                output_tokens: 5,
            }]
        );
    }

    #[tokio::test]
    async fn daemon_worker_reports_text_completion_without_a_workspace_delivery() {
        let directory = TempDir::new().unwrap();
        let base_commit = initialize_git_repository(directory.path());
        let branch = git_output(directory.path(), &["branch", "--show-current"])
            .unwrap()
            .trim()
            .to_string();
        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(TextAnswerProvider),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            directory.path().join("runtime.sock"),
        )
        .with_workspace(directory.path().to_path_buf());
        let workspace = WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: directory.path().to_path_buf(),
            path: directory.path().to_path_buf(),
            branch,
            parent_branch: "main".into(),
            base_commit,
        };
        let request = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
            .with_objective("Report whether the sub-agent is healthy.")
            .with_workspace(workspace)
            .with_workspace_mode(ChildWriteMode::Coding);
        let handle = factory.start(request).await.unwrap();

        let report = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(report) = handle
                    .take_events()
                    .into_iter()
                    .find_map(|event| match event {
                        WorkerEvent::Completed { report } => Some(report),
                        WorkerEvent::CompletedWithoutDelivery => {
                            panic!("text-only completion should carry a report")
                        }
                        _ => None,
                    })
                {
                    return report;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker should report text completion");

        assert_eq!(report, "sub-agent 正常完成，结果可读");
    }

    #[tokio::test]
    async fn daemon_read_only_worker_reports_text_completion() {
        let directory = TempDir::new().unwrap();
        let base_commit = initialize_git_repository(directory.path());
        let branch = git_output(directory.path(), &["branch", "--show-current"])
            .unwrap()
            .trim()
            .to_string();
        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(TextAnswerProvider),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            directory.path().join("runtime.sock"),
        )
        .with_workspace(directory.path().to_path_buf());
        let workspace = WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: directory.path().to_path_buf(),
            path: directory.path().to_path_buf(),
            branch,
            parent_branch: "main".into(),
            base_commit,
        };
        let request = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
            .with_objective("Report whether the sub-agent is healthy.")
            .with_workspace(workspace)
            .with_workspace_mode(ChildWriteMode::ReadOnly);
        let handle = factory.start(request).await.unwrap();

        let report = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(report) = handle
                    .take_events()
                    .into_iter()
                    .find_map(|event| match event {
                        WorkerEvent::Completed { report } => Some(report),
                        WorkerEvent::CompletedWithoutDelivery => {
                            panic!("read-only completion should carry a report")
                        }
                        _ => None,
                    })
                {
                    return report;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("read-only worker should report text completion");

        assert_eq!(report, "sub-agent 正常完成，结果可读");
    }

    #[tokio::test]
    async fn daemon_worker_retries_a_transient_provider_failure() {
        let directory = TempDir::new().unwrap();
        let provider = Arc::new(OneTransientFailureProvider::default());
        let factory = DaemonAgentWorkerFactory::new(
            provider.clone(),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            directory.path().join("runtime.sock"),
        );
        let handle = factory
            .start(
                WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
                    .with_objective("Complete the delegated task.")
                    .with_workspace(worker_workspace(directory.path())),
            )
            .await
            .unwrap();

        let saw_retry = tokio::time::timeout(Duration::from_secs(3), async {
            let mut saw_retry = false;
            loop {
                let events = handle.take_watchdog_events();
                saw_retry |= events
                    .iter()
                    .any(|event| matches!(event, WorkerWatchdogEvent::ProviderRetry));
                if saw_retry && *provider.calls.lock().unwrap() == 2 {
                    return saw_retry;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker should retry a transient provider failure");
        assert!(saw_retry);
        assert_eq!(*provider.calls.lock().unwrap(), 2);
        assert_eq!(*provider.message_counts.lock().unwrap(), vec![1, 1]);
    }

    #[tokio::test]
    async fn unbound_worker_message_proxy_is_rejected_by_the_daemon() {
        let directory = TempDir::new().unwrap();
        let daemon = Daemon::start(
            directory.path().join("runtime"),
            directory.path().join("runtime.sqlite"),
        )
        .unwrap();
        let IpcResponse::SessionCreated {
            session_id,
            root_task_id,
        } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
        else {
            panic!("expected session");
        };
        let IpcResponse::TaskSpawned {
            task_id: child_task_id,
        } = send_request(
            daemon.socket_path(),
            IpcRequest::SpawnChild {
                workdir: None,
                session_id: session_id.clone(),
                parent_task_id: root_task_id.clone(),
                objective: "Inspect the target".into(),
                mode: None,
                model: None,
            },
        )
        .unwrap()
        else {
            panic!("expected child");
        };

        let tool = DaemonSendMessageTool {
            runtime_socket: daemon.socket_path().to_path_buf(),
            session_id,
            caller_task_id: child_task_id,
            worker_capability: "forged".into(),
        };
        let result = tool
            .call(json!({
                "recipient": root_task_id,
                "message": "Need clarification about the acceptance test."
            }))
            .await;

        assert!(result.is_error);
    }

    #[tokio::test]
    async fn worker_wait_proxy_rejects_unbound_capability() {
        let directory = TempDir::new().unwrap();
        let daemon = Daemon::start(
            directory.path().join("runtime"),
            directory.path().join("runtime.sqlite"),
        )
        .unwrap();
        let IpcResponse::SessionCreated {
            session_id,
            root_task_id,
        } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
        else {
            panic!("expected session");
        };
        let IpcResponse::TaskSpawned {
            task_id: child_task_id,
        } = send_request(
            daemon.socket_path(),
            IpcRequest::SpawnChild {
                workdir: None,
                session_id: session_id.clone(),
                parent_task_id: root_task_id.clone(),
                objective: "Inspect the target".into(),
                mode: None,
                model: None,
            },
        )
        .unwrap()
        else {
            panic!("expected child");
        };
        let IpcResponse::CancelPreview {
            confirmation_token, ..
        } = send_request(
            daemon.socket_path(),
            IpcRequest::PreviewCancel {
                task_id: child_task_id.clone(),
                recursive: false,
            },
        )
        .unwrap()
        else {
            panic!("expected cancel preview");
        };
        send_request(
            daemon.socket_path(),
            IpcRequest::ConfirmCancel {
                task_id: child_task_id.clone(),
                recursive: false,
                confirmation_token,
            },
        )
        .unwrap();

        let tool = DaemonWaitAgentTool {
            runtime_socket: daemon.socket_path().to_path_buf(),
            session_id,
            caller_task_id: root_task_id,
            caller_capability: "forged".into(),
        };
        let result = tool.call(json!({ "mode": "all" })).await;

        assert!(result.is_error);
        assert!(matches!(
            result.content.as_slice(),
            [yi_agent_core::ContentBlock::Text(text)] if text.contains("rejected") || text.contains("unavailable")
        ));
    }
}
