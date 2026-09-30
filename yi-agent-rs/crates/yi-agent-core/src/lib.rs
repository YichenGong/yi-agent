//! yi-agent-core: agent loop, session management, and core trait definitions.

pub mod agent;
pub mod autonomy;
pub mod compact;
pub mod message;
pub mod permission;
pub mod provider;
pub mod subagent;
pub mod tool;

// Re-export most-used types at crate root.
pub use agent::{
    Agent, AgentConfig, AgentError, AgentEvent, DoneReason, ProviderTurnGate, ProviderTurnLease,
    RetryCause, Session,
};
pub use compact::{
    CompactError, CompactionPlan, DEFAULT_COMPACT_TOOL_BUDGET_TOKENS,
    DEFAULT_COMPACT_USER_BUDGET_TOKENS, compact_session,
};
pub use message::{ContentBlock, ImageDetail, ImageSource, Message, Role};
pub use provider::{
    GenParams, Provider, ProviderError, ProviderEvent, ProviderRequest, ProviderResponse,
    StopReason, StreamEnd, TokenUsage,
};
pub use subagent::task::{
    AgentTask, AttemptId, ChildWriteMode, RootSessionId, TaskDepth, TaskEvent, TaskId, TaskState,
};
pub use tool::{
    OutputStream, Tool, ToolEvent, ToolMetadata, ToolRegistry, ToolResult, ToolSchema, ToolSource,
};
