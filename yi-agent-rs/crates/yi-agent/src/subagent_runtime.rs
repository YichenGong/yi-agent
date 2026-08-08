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
            tokio::spawn(async move {
                let mut agent = Agent::new(provider, tools, config);
                let Ok(stream) = agent.run(objective).await else {
                    return;
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
                            Some(AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_)) | None => break,
                            Some(_) => {}
                        },
                    }
                }
            });
            Ok(handle)
        })
    }
}
