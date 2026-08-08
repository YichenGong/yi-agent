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
            let mut worker_tools = (*tools).clone();
            worker_tools.register(Arc::new(DaemonSpawnAgentTool {
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
                        let stream = match agent.run(objective).await {
                            Ok(stream) => stream,
                            Err(error) => {
                                reporter.report_failure(error.to_string());
                                return;
                            }
                        };
                        let agent_cancellation = agent.cancel_token();
                        let mut stream = Box::pin(stream);
                        let mut cancellation_forwarded = false;
                        loop {
                            tokio::select! {
                                _ = cancellation.cancelled(), if !cancellation_forwarded => {
                                    agent_cancellation.cancel();
                                    cancellation_forwarded = true;
                                }
                                event = stream.next() => match event {
                                    Some(AgentEvent::Done { .. }) | None => {
                                        reporter.report_completed_without_delivery();
                                        break;
                                    }
                                    Some(AgentEvent::Cancelled) => {
                                        reporter.report_cancelled();
                                        break;
                                    }
                                    Some(AgentEvent::Error(error)) => {
                                        reporter.report_failure(error.to_string());
                                        break;
                                    }
                                    Some(_) => {}
                                },
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

struct DaemonSpawnAgentTool {
    runtime_socket: PathBuf,
    session_id: String,
    caller_task_id: String,
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
