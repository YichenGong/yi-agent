//! Application-owned construction for daemon subagent workers.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryAttestation,
    WorkerRecoveryContext, WorkerRecoveryPreflight, WorkerRecoveryPreflightResult, WorkerStart,
};
use yi_agent_core::{Agent, AgentConfig, AgentEvent, Provider, Tool, ToolRegistry, ToolResult};

/// Reuses the selected provider, tool registry, and system prompt for each
/// delegated worker while the supervisor supplies its narrow objective.
pub struct DaemonAgentWorkerFactory {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
    runtime_socket: PathBuf,
    workspace: PathBuf,
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
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    /// Must match the directory passed to builtin filesystem and shell tools.
    pub fn with_workspace(mut self, workspace: PathBuf) -> Self {
        self.workspace = workspace;
        self
    }

    fn worker_tool_names(&self) -> Vec<String> {
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
    }
}

impl AgentWorkerFactory for DaemonAgentWorkerFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        let workspace = &self.workspace;
        let git_root = git_output(workspace, &["rev-parse", "--show-toplevel"]);
        let git_head = git_output(workspace, &["rev-parse", "HEAD"]);
        let git_status = git_command_output(workspace, &["status", "--porcelain"]).ok();
        let tool_names = self.worker_tool_names();
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

    fn preflight_recovery(
        &self,
        request: WorkerRecoveryPreflight,
    ) -> WorkerRecoveryPreflightResult {
        match validate_recovery_context(&request.context, self.worker_tool_names()) {
            Ok(attestation) => WorkerRecoveryPreflightResult::Attested(attestation),
            Err(reason) => WorkerRecoveryPreflightResult::Conflict(reason),
        }
    }

    fn start(
        &self,
        request: WorkerStart,
    ) -> futures::future::BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let provider = Arc::clone(&self.provider);
        let tools = Arc::clone(&self.tools);
        let config = self.config.clone();
        let runtime_socket = self.runtime_socket.clone();
        let cancellation = request.cancellation.clone();
        let objective = request.objective;
        let initial_user_messages = request.initial_user_messages;

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
            let mut worker_tools = (*tools).clone();
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
                        let mut prompt = objective;
                        'run: loop {
                            let stream = match agent.run(prompt).await {
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
                                        Some(AgentEvent::Done { .. }) | None => {
                                            if pause_forwarded {
                                                reporter.report_paused();
                                                break 'run;
                                            }
                                            if let Some(next_prompt) = message_prompt.take() {
                                                prompt = next_prompt;
                                                continue 'run;
                                            }
                                            reporter.report_completed_without_delivery();
                                            break 'run;
                                        }
                                        Some(AgentEvent::Cancelled) => {
                                            if pause_forwarded {
                                                reporter.report_paused();
                                                break 'run;
                                            }
                                            if let Some(next_prompt) = message_prompt.take() {
                                                prompt = next_prompt;
                                                continue 'run;
                                            }
                                            reporter.report_cancelled();
                                            break 'run;
                                        }
                                        Some(AgentEvent::Error(error)) => {
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

struct DaemonSendMessageTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
    worker_capability: String,
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
            Ok(other) => ToolResult::error(format!("daemon rejected message: {other:?}")),
            Err(error) => ToolResult::error(format!("daemon is unavailable: {error}")),
        }
    }
}

struct DaemonSpawnAgentTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
}

struct DaemonWaitAgentTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
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
                mode: mode.to_owned(),
            },
        );
        match response {
            Ok(yi_agent_store::ipc::IpcResponse::WaitCompleted { status, children }) => {
                ToolResult::text(json!({ "status": status, "children": children }).to_string())
            }
            Ok(other) => ToolResult::error(format!("daemon rejected wait request: {other:?}")),
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
            "properties": { "task": { "type": "string", "description": "Delegated objective." } },
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
        let response = yi_agent_store::ipc::send_request(
            &self.runtime_socket,
            yi_agent_store::ipc::IpcRequest::SpawnChild {
                session_id: self.session_id.clone(),
                parent_task_id: self.caller_task_id.clone(),
                objective: task.to_string(),
            },
        );
        match response {
            Ok(yi_agent_store::ipc::IpcResponse::TaskSpawned { task_id }) => ToolResult::text(
                json!({ "task_id": task_id, "objective": task, "status": "queued" }).to_string(),
            ),
            Ok(other) => ToolResult::error(format!("daemon rejected spawn request: {other:?}")),
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
        AgentWorkerFactory, WorkerEvent, WorkerMessage, WorkerStart,
    };
    use yi_agent_core::{ProviderError, ProviderEvent, ProviderRequest};
    use yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, send_request};
    use yi_agent_store::repository::RuntimeRepository;
    use yi_agent_store::runtime::RuntimeCoordinator;

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
        assert!(resumed_prompt.contains("Recover safely"));
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
    async fn worker_wait_proxy_routes_through_the_daemon() {
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
            },
        )
        .unwrap()
        else {
            panic!("expected child");
        };
        send_request(
            daemon.socket_path(),
            IpcRequest::CancelTask {
                session_id: session_id.clone(),
                task_id: child_task_id,
                recursive: false,
            },
        )
        .unwrap();

        let tool = DaemonWaitAgentTool {
            runtime_socket: daemon.socket_path().to_path_buf(),
            session_id,
            caller_task_id: root_task_id,
        };
        let result = tool.call(json!({ "mode": "all" })).await;

        assert!(!result.is_error);
        assert!(matches!(
            result.content.as_slice(),
            [yi_agent_core::ContentBlock::Text(text)] if text.contains("completed")
        ));
    }
}
