//! Application-owned construction for daemon subagent workers.

use std::sync::Arc;

use futures::StreamExt;
use yi_agent_core::subagent::worker::{AgentWorkerFactory, WorkerError, WorkerHandle, WorkerStart};
use yi_agent_core::{Agent, AgentConfig, AgentEvent, Provider, ToolRegistry};

/// Reuses the selected provider, tool registry, and system prompt for each
/// delegated worker while the supervisor supplies its narrow objective.
pub struct DaemonAgentWorkerFactory {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
}

impl DaemonAgentWorkerFactory {
    pub fn new(provider: Arc<dyn Provider>, tools: Arc<ToolRegistry>, config: AgentConfig) -> Self {
        Self {
            provider,
            tools,
            config,
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
        let cancellation = request.cancellation.clone();
        let objective = request.objective;

        Box::pin(async move {
            if objective.trim().is_empty() {
                return Err(WorkerError::Startup("worker objective is required".into()));
            }
            let handle = WorkerHandle::new(cancellation.clone());
            let reporter = handle.clone();
            std::thread::Builder::new()
                .name("yi-agent-subagent-worker".into())
                .spawn(move || {
                    let Ok(runtime) = tokio::runtime::Runtime::new() else {
                        reporter.report_failure("could not initialize subagent runtime");
                        return;
                    };
                    runtime.block_on(async move {
                        let mut agent = Agent::new(provider, tools, config);
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
