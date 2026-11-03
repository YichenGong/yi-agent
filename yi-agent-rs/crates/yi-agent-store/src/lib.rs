//! yi-agent-store: session persistence layer.
//!
//! 负责会话历史的本地持久化(类似 ~/.claude/projects/)。
//! 依赖 `yi-agent-core` 的 `Session` 抽象,具体存储后端(如 SQLite)
//! 在本 crate 内实现。

/// 前台会话根的分类与活动规则。
pub mod foreground;
pub mod ipc;
pub mod lifecycle;
pub mod paths;
pub mod repository;
pub mod resident;
pub mod runtime;
pub mod schedule;
pub mod schema_compat;
pub mod terminal;
