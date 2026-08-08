//! Application-owned construction for daemon subagent workers.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use yi_agent_core::subagent::worker::{AgentWorkerFactory, WorkerError, WorkerHandle, WorkerStart};
use yi_agent_core::{Agent, AgentConfig, AgentEvent, Provider, Tool, ToolRegistry, ToolResult};

/// Reuses the selected provider, tool registry, and system prompt for each
/// delegated worker while the supervisor supplies its narrow objective.
pub struct DaemonAgentWorkerFactory {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
    runtime_socket: PathBuf,
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
        }
    }
}

impl AgentWorkerFactory for DaemonAgentWorkerFactory {
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

        Box::pin(async move {
            if objective.trim().is_empty() {
                return Err(WorkerError::Startup("worker objective is required".into()));
            }
            let handle = WorkerHandle::new(cancellation.clone());
            let reporter = handle.clone();
            let mut mailbox = handle.subscribe_messages();
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
                                        agent_cancellation.cancel();
                                    },
                                    _ = cancellation.cancelled(), if !cancellation_forwarded => {
                                        agent_cancellation.cancel();
                                        cancellation_forwarded = true;
                                    },
                                    event = stream.next() => match event {
                                        Some(AgentEvent::Done { .. }) | None => {
                                            if let Some(next_prompt) = message_prompt.take() {
                                                prompt = next_prompt;
                                                continue 'run;
                                            }
                                            reporter.report_completed_without_delivery();
                                            break 'run;
                                        }
                                        Some(AgentEvent::Cancelled) => {
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

struct DaemonSendMessageTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
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
                recipient_task_id: recipient.to_owned(),
                message: message.to_owned(),
            },
        ) {
            Ok(yi_agent_store::ipc::IpcResponse::MessageDelivered) => {
                ToolResult::text("message delivered")
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
    use tempfile::TempDir;
    use yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, send_request};

    use super::*;

    #[tokio::test]
    async fn worker_message_proxy_routes_through_the_daemon() {
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
        };
        let result = tool
            .call(json!({
                "recipient": root_task_id,
                "message": "Need clarification about the acceptance test."
            }))
            .await;

        assert!(!result.is_error);
        assert!(matches!(
            result.content.as_slice(),
            [yi_agent_core::ContentBlock::Text(text)] if text == "message delivered"
        ));
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
