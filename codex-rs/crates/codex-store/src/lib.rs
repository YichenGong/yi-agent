//! codex-store: session persistence layer.
//!
//! 负责会话历史的本地持久化(类似 ~/.claude/projects/)。
//! 依赖 `codex-core` 的 `Session` 抽象,具体存储后端(如 SQLite)
//! 在本 crate 内实现。
