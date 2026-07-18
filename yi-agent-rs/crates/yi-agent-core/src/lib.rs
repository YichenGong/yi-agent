//! yi-agent-core: agent loop, session management, and core trait definitions.
//!
//! 这个 crate 定义了 agent 的核心抽象(`Tool`、`Provider`、`Session`),
//! 并实现 think → act → observe 的主循环。具体工具实现在 `yi-agent-tools` /
//! `yi-agent-mcp`,LLM 实现在 `yi-agent-llm`,持久化实现在 `yi-agent-store`。
