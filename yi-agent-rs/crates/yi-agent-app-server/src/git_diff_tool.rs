//! 「把 Git Diff 视图推到用户眼前」的句柄与工具。
//!
//! 与 `theme_tool.rs` 同构：工具只负责广播一个焦点意图，真正渲染是前端的事；
//! app-server 的 watcher 把广播翻译成 `ui/gitDiff/focus` 通知扇出。diff 文本本身
//! 一律由 git 计算，工具不生成、不传递 diff。

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::broadcast;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

/// 一次「请用户看 diff」的意图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiffFocus {
    /// 哪个会话的视图；`None` = 客户端按当前会话处理。
    pub thread_id: Option<String>,
    /// 显式指定的比较基准；`None` = 客户端用默认（分叉点）规则。
    pub base: Option<String>,
    /// 面向用户的说明，随通知带过去。
    pub note: Option<String>,
}

#[derive(Clone)]
pub struct GitDiffHandle {
    tx: broadcast::Sender<GitDiffFocus>,
}

impl GitDiffHandle {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(16);
        Self { tx }
    }

    pub fn broadcast(&self, focus: GitDiffFocus) {
        let _ = self.tx.send(focus);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<GitDiffFocus> {
        self.tx.subscribe()
    }
}

impl Default for GitDiffHandle {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ShowGitDiffTool {
    handle: GitDiffHandle,
    thread_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ShowGitDiffArgs {
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

impl ShowGitDiffTool {
    /// `thread_id` 绑定时，焦点推给该会话；未绑定时前端按当前会话处理。
    pub fn new(handle: GitDiffHandle, thread_id: Option<String>) -> Self {
        Self { handle, thread_id }
    }
}

#[async_trait::async_trait]
impl Tool for ShowGitDiffTool {
    fn name(&self) -> &str {
        "show_git_diff"
    }

    fn description(&self) -> &str {
        "Open the desktop app's Git Diff tab for this conversation, showing the change since the branch forked. Use when a review of the code you changed would help the user. Pass `base` only when you know the intended comparison (e.g. \"origin/main\"); omit it to use the default merge-base with the repository's default branch."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "base": {
                    "type": "string",
                    "description": "Git ref to compare against (branch, tag, or sha). Omit to let the app use the repository default branch."
                },
                "note": {
                    "type": "string",
                    "description": "One short sentence shown to the user above the diff."
                }
            }
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let parsed: ShowGitDiffArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        let base = parsed
            .base
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty());
        self.handle.broadcast(GitDiffFocus {
            thread_id: self.thread_id.clone(),
            base: base.clone(),
            note: parsed
                .note
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty()),
        });
        match base {
            Some(b) => ToolResult::text(format!("showing git diff against {b}")),
            None => ToolResult::text("showing git diff against the repository default branch"),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Plugin {
                name: "ui".to_string(),
            },
            requires_confirmation: false,
            read_only: true,
            version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn broadcasting_an_explicit_base_reports_it_back() {
        let handle = GitDiffHandle::new();
        let mut rx = handle.subscribe();
        let tool = ShowGitDiffTool::new(handle.clone(), Some("thread-1".into()));
        let result = tool
            .call(serde_json::json!({"base": "origin/main", "note": "review"}))
            .await;
        assert!(!result.is_error);
        let focus = rx.recv().await.unwrap();
        assert_eq!(focus.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(focus.base.as_deref(), Some("origin/main"));
        assert_eq!(focus.note.as_deref(), Some("review"));
    }

    #[tokio::test]
    async fn omitting_base_still_broadcasts() {
        let handle = GitDiffHandle::new();
        let mut rx = handle.subscribe();
        let tool = ShowGitDiffTool::new(handle.clone(), None);
        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        let focus = rx.recv().await.unwrap();
        assert_eq!(focus.base, None);
        assert_eq!(focus.thread_id, None);
    }

    #[test]
    fn tool_advertises_its_name_and_schema() {
        let tool = ShowGitDiffTool::new(GitDiffHandle::new(), None);
        assert_eq!(tool.name(), "show_git_diff");
        assert!(tool.schema()["properties"]["base"].is_object());
    }
}
