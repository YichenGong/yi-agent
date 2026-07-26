//! yi-agent-core: agent loop, session management, and core trait definitions.

pub mod agent;
pub mod compact;
pub mod message;
pub mod permission;
pub mod provider;
pub mod tool;

// Re-export most-used types at crate root.
pub use agent::{Agent, AgentConfig, AgentError, AgentEvent, DoneReason, Session};
pub use compact::compact_session;
pub use message::{ContentBlock, ImageSource, Message, Role};
pub use provider::{
    GenParams, Provider, ProviderError, ProviderEvent, ProviderRequest, ProviderResponse,
    StopReason, TokenUsage,
};
pub use tool::{
    OutputStream, Tool, ToolEvent, ToolMetadata, ToolRegistry, ToolResult, ToolSchema, ToolSource,
};
