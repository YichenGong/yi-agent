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
    AttemptId, RootSessionId, TaskId, TaskWorkspaceMode, WorkspaceLeaseId,
};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, AgentWorkspaceService, WorkerError, WorkerHandle,
    WorkerRecoveryAttestation, WorkerRecoveryContext, WorkerRecoveryPreflight,
    WorkerRecoveryPreflightResult, WorkerStart, WorkerWorkspace,
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
        workspace_mode: TaskWorkspaceMode,
    ) -> ToolRegistry {
        let mut tools = (*self.tools).clone();
        let (sandbox, writable_roots) = match workspace_mode {
            TaskWorkspaceMode::Coding => (
                self.sandbox,
                git_dir_for_worktree(&workspace.path).into_iter().collect(),
            ),
            TaskWorkspaceMode::ReadOnly => (yi_agent_tools::SandboxMode::ReadOnly, Vec::new()),
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

impl AgentWorkspaceService for DaemonWorkspaceService {
    fn supports_coding(&self) -> bool {
        self.is_git_repository
    }

    fn prepare_read_only(
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

    fn prepare_root(
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

    fn prepare_child(
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

    fn workspace_service(&self) -> Option<Arc<dyn AgentWorkspaceService>> {
        self.workspace_service
            .as_ref()
            .map(|service| Arc::clone(service) as Arc<dyn AgentWorkspaceService>)
    }

    fn workspace_service_for_application_root(
        &self,
        workspace: &std::path::Path,
    ) -> Option<Arc<dyn AgentWorkspaceService>> {
        Some(Arc::new(DaemonWorkspaceService::new(
            workspace.to_path_buf(),
        )))
    }

    fn application_root_workspace_matches(
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
        let config = self.config.clone();
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
                runtime_socket,
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
                                            if workspace_mode == TaskWorkspaceMode::Coding {
                                                if let Some(service) = workspace_service.as_ref() {
                                                    match service.inspect_delivery(&workspace_for_delivery) {
                                                        Ok(delivery) => reporter.report_delivery(delivery),
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

fn is_empty_delivery_error(error: &WorkerError) -> bool {
    error
        .to_string()
        .contains("child delivery has no commits beyond")
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
/// canonical [`TaskWorkspaceMode::parse`], so the tool, the core tool, and the
/// IPC helper agree on the accepted spellings. An omitted mode defaults to
/// read-only; an explicit unknown or non-string value is rejected.
fn spawn_mode(args: &Value) -> Result<TaskWorkspaceMode, ToolResult> {
    match args.get("mode") {
        None => Ok(TaskWorkspaceMode::ReadOnly),
        Some(Value::String(value)) => TaskWorkspaceMode::parse(value)
            .ok_or_else(|| ToolResult::error("mode must be 'coding' or 'read_only'")),
        Some(_) => Err(ToolResult::error("mode must be a string")),
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
        let response = yi_agent_store::ipc::send_request(
            &self.runtime_socket,
            yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
                session_id: self.session_id.clone(),
                parent_task_id: self.caller_task_id.clone(),
                capability: self.application_capability.clone(),
                objective: task.to_string(),
                mode: Some(mode.as_str().to_string()),
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
        let response = yi_agent_store::ipc::send_request(
            &self.runtime_socket,
            yi_agent_store::ipc::IpcRequest::SpawnChild {
                session_id: self.session_id.clone(),
                parent_task_id: self.caller_task_id.clone(),
                objective: task.to_string(),
                mode: Some(mode.as_str().to_string()),
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
    fn daemon_spawn_mode_uses_the_canonical_parser() {
        assert_eq!(
            spawn_mode(&json!({})).unwrap(),
            TaskWorkspaceMode::ReadOnly,
            "an omitted mode defaults to read-only"
        );
        assert_eq!(
            spawn_mode(&json!({ "mode": "coding" })).unwrap(),
            TaskWorkspaceMode::Coding
        );
        assert_eq!(
            spawn_mode(&json!({ "mode": "read_only" })).unwrap(),
            TaskWorkspaceMode::ReadOnly
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
        let registry = factory.worker_tool_registry(&workspace, TaskWorkspaceMode::Coding);
        let write = registry.get("write").expect("write tool");
        assert!(
            !write
                .call(json!({"path":"delivery.txt","content":"ready\n"}))
                .await
                .is_error
        );
        let bash = registry.get("bash").expect("bash tool");
        assert!(
            !bash
                .call(json!({"command":"git add delivery.txt"}))
                .await
                .is_error,
            "child sandbox must permit its Git index writes"
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

        let read_only = factory.worker_tool_registry(&workspace, TaskWorkspaceMode::ReadOnly);
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

        let coding = factory.worker_tool_registry(&workspace, TaskWorkspaceMode::Coding);
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
            .prepare_root(&RootSessionId::new(), &TaskId::new(), &AttemptId::new())
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
    fn non_git_workspace_service_supports_only_read_only() {
        let directory = tempfile::TempDir::new().unwrap();
        if std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(directory.path())
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            eprintln!("skip: temp dir is inside a git repository");
            return;
        }
        let service = DaemonWorkspaceService::new(directory.path().to_path_buf());

        assert!(!service.supports_coding());

        let workspace = service
            .prepare_read_only(None, &TaskId::new())
            .expect("read-only workspace is always available");
        assert_eq!(workspace.path, directory.path());
        assert!(workspace.branch.is_empty());
        assert!(workspace.base_commit.is_empty());
    }

    #[test]
    fn git_workspace_service_supports_coding() {
        let directory = tempfile::TempDir::new().unwrap();
        initialize_git_repository(directory.path());
        let service = DaemonWorkspaceService::new(directory.path().to_path_buf());

        assert!(service.supports_coding());
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
            .prepare_read_only(Some(&parent), &TaskId::new())
            .unwrap();
        assert_eq!(workspace.path, parent.path);
    }

    #[test]
    fn daemon_workspace_cleanup_accepted_removes_child_worktree_and_branch() {
        let directory = TempDir::new().unwrap();
        initialize_git_repository(directory.path());
        let service = DaemonWorkspaceService::new(directory.path().to_path_buf());
        let root = service
            .prepare_root(&RootSessionId::new(), &TaskId::new(), &AttemptId::new())
            .unwrap();
        let child = service
            .prepare_child(
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

        RuntimeRepository::open(&database)
            .unwrap()
            .recover_inflight_tasks()
            .unwrap();
        first_worker.cancel();

        let restarted = RuntimeCoordinator::open(&database, factory).unwrap();
        restarted.resume_task(&session, &task).await.unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if provider.requests.lock().unwrap().len() == 2 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("resumed daemon worker should begin only after recovery attestation");

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
            .with_workspace_mode(TaskWorkspaceMode::Coding);
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
            .with_workspace_mode(TaskWorkspaceMode::Coding);
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
            .with_workspace_mode(TaskWorkspaceMode::ReadOnly);
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
                session_id: session_id.clone(),
                parent_task_id: root_task_id.clone(),
                objective: "Inspect the target".into(),
                mode: None,
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
                session_id: session_id.clone(),
                parent_task_id: root_task_id.clone(),
                objective: "Inspect the target".into(),
                mode: None,
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
