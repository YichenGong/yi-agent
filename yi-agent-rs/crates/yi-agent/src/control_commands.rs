//! Canonical metadata for daemon-backed user controls.
//!
//! Clap keeps Rust's typed argument parsing, while the CLI and Slash layers
//! consume this catalog for stable names, usage text, and confirmations.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlCommand {
    Agents,
    Agent,
    Events,
    Diff,
    Mailbox,
    Message,
    Pause,
    Resume,
    Cancel,
    Retry,
    Priority,
    Approve,
    Deny,
    Review,
    Accept,
    Rework,
    Reject,
    Budget,
    Daemon,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub command: ControlCommand,
    pub slash_name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    pub requires_confirmation: bool,
}

impl ControlCommand {
    #[cfg_attr(not(test), allow(dead_code))]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Agents,
            Self::Agent,
            Self::Events,
            Self::Diff,
            Self::Mailbox,
            Self::Message,
            Self::Pause,
            Self::Resume,
            Self::Cancel,
            Self::Retry,
            Self::Priority,
            Self::Approve,
            Self::Deny,
            Self::Review,
            Self::Accept,
            Self::Rework,
            Self::Reject,
            Self::Budget,
            Self::Daemon,
            Self::Help,
        ]
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub const fn slash_name(self) -> &'static str {
        self.spec().slash_name
    }

    pub const fn spec(self) -> CommandSpec {
        match self {
            Self::Agents => spec(
                self,
                "agents",
                "[--project PATH]",
                "显示 agent 任务树",
                false,
            ),
            Self::Agent => spec(self, "agent", "<task-id>", "查看 agent 详情", false),
            Self::Events => spec(self, "events", "<task-id>", "查看任务事件", false),
            Self::Diff => spec(self, "diff", "<task-id>", "查看任务 diff", false),
            Self::Mailbox => spec(self, "mailbox", "<task-id>", "查看任务 mailbox", false),
            Self::Message => spec(self, "message", "<task-id> <text>", "向任务发送消息", false),
            Self::Pause => spec(self, "pause", "<task-id>", "请求安全暂停", false),
            Self::Resume => spec(self, "resume", "<task-id>", "恢复任务", false),
            Self::Cancel => spec(self, "cancel", "<task-id> [--recursive]", "取消任务", true),
            Self::Retry => spec(self, "retry", "<task-id>", "创建新的重试 attempt", false),
            Self::Priority => spec(
                self,
                "priority",
                "<task-id> <level>",
                "调整任务优先级",
                false,
            ),
            Self::Approve => spec(
                self,
                "approve",
                "<request-id> [once|task]",
                "批准权限请求",
                true,
            ),
            Self::Deny => spec(self, "deny", "<request-id>", "拒绝权限请求", true),
            Self::Review => spec(self, "review", "<task-id>", "查看 delivery 审查", false),
            Self::Accept => spec(
                self,
                "accept",
                "<task-id> [--confirm <token>]",
                "预览或接受 delivery 审查",
                true,
            ),
            Self::Rework => spec(
                self,
                "rework",
                "<task-id> <feedback> [--confirm <token>]",
                "预览或请求返工",
                true,
            ),
            Self::Reject => spec(
                self,
                "reject",
                "<task-id> <reason> [--confirm <token>]",
                "预览或拒绝 delivery",
                true,
            ),
            Self::Budget => spec(self, "budget", "<task-id> ...", "查看或收窄任务预算", false),
            Self::Daemon => spec(self, "daemon", "status", "管理本地 runtime daemon", false),
            Self::Help => spec(self, "help", "[command]", "显示控制命令帮助", false),
        }
    }
}

const fn spec(
    command: ControlCommand,
    slash_name: &'static str,
    usage: &'static str,
    description: &'static str,
    requires_confirmation: bool,
) -> CommandSpec {
    CommandSpec {
        command,
        slash_name,
        usage,
        description,
        requires_confirmation,
    }
}
