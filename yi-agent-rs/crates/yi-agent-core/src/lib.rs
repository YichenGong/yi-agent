//! yi-agent-core: agent loop, session management, and core trait definitions.

pub mod message;
pub mod tool;
pub mod provider;
pub mod agent;

// Re-export most-used types at crate root.
pub use agent::{Agent, AgentConfig, AgentError, AgentEvent, DoneReason, Session};
pub use message::{ContentBlock, ImageSource, Message, Role};
pub use provider::{
    GenParams, Provider, ProviderError, ProviderEvent, ProviderRequest, ProviderResponse, StopReason,
};
pub use tool::{Tool, ToolMetadata, ToolRegistry, ToolResult, ToolSchema, ToolSource};
