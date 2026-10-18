//! 「把 Git Diff 视图推到用户眼前」的句柄与工具。
//!
//! 与 `theme_tool.rs` 同构：工具只负责广播一个焦点意图，真正渲染是前端的事；
//! app-server 的 watcher 把广播翻译成 `ui/gitDiff/focus` 通知扇出。diff 文本本身
//! 一律由 git 计算，工具不生成、不传递 diff。
//!
//! 焦点里带**已算好的帧**（[`GitDiffFocus::frame`]）：工具算一次，watcher 写进
//! 会话槽，避免两处各算一遍——两次读之间仓库可能在变，那会让「回执说的」与
//! 「面板显示的」不一致。

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::broadcast;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

/// 一次「请用户看 diff」的意图。
///
/// 不做 `PartialEq`：帧里是文本，逐字节比没有意义，而工具也不再靠它判断
/// 「值不值得广播」。
#[derive(Debug, Clone)]
pub struct GitDiffFocus {
    /// 哪个会话的视图；`None` = 客户端按当前会话处理。
    pub thread_id: Option<String>,
    /// 模型请求的命令（`None` = 结构化默认）。
    pub command: Option<String>,
    /// 面向用户的说明，随通知带过去。
    pub note: Option<String>,
    /// 已算好的帧。工具算一次、watcher 写进会话槽，避免在两处各算一遍
    /// （两次读之间仓库可能在变，那会让「回执说的」与「面板显示」不一致）。
    pub frame: Option<Box<crate::git_diff::ViewFrame>>,
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
    /// 本会话的仓库路径：执行命令、校验 rev 与算帧都在这里发生。
    repo: std::path::PathBuf,
    /// 本会话起点（默认基准）。工具据此回答「实际用了哪个基准」。
    anchor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ShowGitDiffArgs {
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default, rename = "as")]
    as_kind: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

impl ShowGitDiffTool {
    /// `thread_id` 绑定时，焦点推给该会话；未绑定时前端按当前会话处理。
    ///
    /// `repo` 是会话 cwd，`anchor` 是会话起点；两者都在装配期注入——模型看不到
    /// 自己的会话元数据，也没有「当前仓库」这种可猜的东西。
    pub fn new(
        handle: GitDiffHandle,
        thread_id: Option<String>,
        repo: std::path::PathBuf,
        anchor: Option<String>,
    ) -> Self {
        Self {
            handle,
            thread_id,
            repo,
            anchor,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ShowGitDiffTool {
    fn name(&self) -> &str {
        "show_git_diff"
    }

    fn description(&self) -> &str {
        "Open the desktop app's Git Diff tab for this conversation. Pass `command` to show the output of a specific read-only command (e.g. \"git log --oneline -5\") instead of the default diff; it runs inside a read-only sandbox, so it cannot modify the working tree. Otherwise the tab shows the change since this session started; pass `base` only when you know the intended comparison (e.g. \"origin/main\")."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Read-only command whose output should be shown (e.g. \"git log --oneline -5\"). Runs in a read-only sandbox. Omit to show the default diff."
                },
                "as": {
                    "type": "string",
                    "enum": ["diff", "log", "text"],
                    "description": "How the output should be rendered. Omit to let the app detect it from the content."
                },
                "base": {
                    "type": "string",
                    "description": "Git ref to compare against (branch, tag, or sha). Omit to use this session's start point. Ignored when `command` is given."
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
        let note = parsed
            .note
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());

        // 1) 给了命令：跑它。命令帧与结构化帧互斥——模型要什么，就给什么。
        if let Some(command) = parsed
            .command
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
        {
            let frame = crate::git_diff::command_frame(
                &self.repo,
                &command,
                parsed.as_kind.as_deref(),
                note.clone(),
            )
            .await;
            // 帧写进会话槽由 watcher 侧负责（见 `pump_git_diff_notifications`），
            // 工具只广播意图 + 帧。
            self.handle.broadcast(GitDiffFocus {
                thread_id: self.thread_id.clone(),
                command: Some(command.clone()),
                note,
                frame: Some(Box::new(frame.clone())),
            });
            // 退出码 `None` 是「没跑起来」（沙箱不可用）——同样不能报成功。
            if let Some(code) = frame.exit_code {
                if code != 0 {
                    return ToolResult::error(format!(
                        "`{command}` exited {code}: {}",
                        frame.stderr.trim()
                    ));
                }
            } else {
                return ToolResult::error(format!(
                    "`{command}` could not be run: {}",
                    frame.stderr.trim()
                ));
            }
            return ToolResult::text(format!(
                "showing the output of `{command}` ({} bytes{})",
                frame.stdout.len(),
                if frame.truncated { ", truncated" } else { "" }
            ));
        }

        // 2) 给了 base：先校验，再按它算一帧。校验失败**明确报错**，不静默回退
        //    （D3：旧实现回退后仍回报入参，等于对模型撒谎）。
        let explicit = parsed
            .base
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty());
        if let Some(ref rev) = explicit {
            if !crate::git_diff::valid_rev(&self.repo, rev) {
                return ToolResult::error(format!("`{rev}` is not a usable git revision"));
            }
        }
        let frame = crate::git_diff::structured_frame(
            &self.repo,
            explicit.as_deref(),
            self.anchor.as_deref(),
        );
        // `valid_rev` 接受范围表达式（`A..B` 两侧各自可解），但 `resolve_base` 只用
        // 单个 `ref_exists` 复核显式基准——范围在那里解不开，会静默落到候选链。
        // 若不说，模型要的是 `A..B`，拿到的是分支 diff，就违反了「校验失败明确报错」。
        if explicit.is_some() && frame.base_kind != "explicit" {
            let rev = explicit.as_deref().unwrap_or_default();
            return ToolResult::error(format!(
                "`{rev}` is a revision range, not a comparison base; pass a single ref"
            ));
        }
        // 基准不进通知：它已由帧（`thread/view/read` 读回）承载，看板只需知道
        // 「聚焦哪个会话」与说什么话。
        self.handle.broadcast(GitDiffFocus {
            thread_id: self.thread_id.clone(),
            command: None,
            note,
            frame: Some(Box::new(frame.clone())),
        });
        // 回执报**实际**生效的基准，不是入参。
        match (&frame.base, frame.base_kind.as_str()) {
            (Some(b), "session") => ToolResult::text(format!(
                "showing the diff since this session started (base {b})"
            )),
            (Some(b), "explicit") => ToolResult::text(format!("showing the diff against {b}")),
            (Some(b), kind) => ToolResult::text(format!("showing the diff against {b} ({kind})")),
            (None, _) => ToolResult::text("showing uncommitted changes only (no base ref found)"),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            // 用的是内置视图通道，不是某个插件：`ui` 这个名字从来没被谁分支过，
            // 挂着它只会让人以为有插件在供这个工具。
            source: ToolSource::Builtin,
            requires_confirmation: false,
            read_only: true,
            version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取 `ToolResult` 的第一段文本（`ToolResult` 没有文本访问器，
    /// 其 `content` 是 `Vec<ContentBlock>`）。
    fn text_of(result: &ToolResult) -> String {
        match result.content.first() {
            Some(yi_agent_core::ContentBlock::Text(t)) => t.clone(),
            _ => String::new(),
        }
    }

    /// 临时仓库 + 一次 init 提交（与 `git_diff.rs` 的 `repo()` 同形的最小版）。
    fn init_repo(dir: &std::path::Path) {
        for args in [
            vec!["init", "-q", "-b", "trunk"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "T"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(&args)
                    .current_dir(dir)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-q", "-m", "init"]] {
            assert!(
                std::process::Command::new("git")
                    .args(&args)
                    .current_dir(dir)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }

    #[tokio::test]
    async fn broadcasting_an_explicit_base_reports_it_back() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let handle = GitDiffHandle::new();
        let mut rx = handle.subscribe();
        let tool = ShowGitDiffTool::new(
            handle.clone(),
            Some("thread-1".into()),
            dir.path().to_path_buf(),
            None,
        );
        let result = tool
            .call(serde_json::json!({"base": "trunk", "note": "review"}))
            .await;
        assert!(!result.is_error);
        let focus = rx.recv().await.unwrap();
        assert_eq!(focus.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(focus.note.as_deref(), Some("review"));
        assert_eq!(focus.command, None);
        // 基准不再是通知里的字段：它随**帧**（`thread/view/read`）走。回执仍
        // 如实说明用的是哪个基准（否则模型无从判断该不该解释一句）。
        assert!(
            text_of(&result).contains("trunk"),
            "the receipt still names the base: {:?}",
            text_of(&result)
        );
        // 显式基准必须标成 `explicit`，**不得**被算成「本次会话起点」：帧的
        // `base_kind` 是回执措辞与前端标签的来源，标错就是 D3 那种不实之词。
        assert_eq!(
            focus.frame.as_ref().unwrap().base_kind,
            "explicit",
            "an explicit base must not be labelled as the session start"
        );
    }

    /// 范围表达式（`A..B`）不是比较基准：`valid_rev` 两侧可解会让它过第一道门，
    /// 但 `resolve_base` 用单个 `ref_exists` 复核，范围在那里解不开。若不拦，
    /// 模型要的是 `A..B`、拿到的是某个分支 diff —— 又成了「校验失败却静默回退」。
    #[tokio::test]
    async fn a_revision_range_is_rejected_as_a_base() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // 两个都能解的可达名字，让 `valid_rev` 放行，从而测到第二道门。
        for args in [vec!["branch", "other"]] {
            assert!(
                std::process::Command::new("git")
                    .args(&args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let tool = ShowGitDiffTool::new(
            GitDiffHandle::new(),
            Some("thread-1".into()),
            dir.path().to_path_buf(),
            None,
        );
        let result = tool
            .call(serde_json::json!({ "base": "trunk..other" }))
            .await;
        assert!(
            result.is_error,
            "a range must be rejected, not silently substituted: {:?}",
            text_of(&result)
        );
        assert!(
            text_of(&result).contains("trunk..other"),
            "the receipt must name the rejected rev: {:?}",
            text_of(&result)
        );
    }

    #[tokio::test]
    async fn omitting_base_still_broadcasts() {
        let handle = GitDiffHandle::new();
        let mut rx = handle.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let tool = ShowGitDiffTool::new(handle.clone(), None, dir.path().to_path_buf(), None);
        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        let focus = rx.recv().await.unwrap();
        assert_eq!(focus.thread_id, None);
        assert_eq!(focus.note, None);
        assert_eq!(focus.command, None);
        assert!(focus.frame.is_some(), "the frame must ride along");
    }

    /// D3：旧实现在 rev 不可用时会静默回退到别的基准，**同时**在回执里回报
    /// 入参——模型由此以为自己看的是 `no-such-ref`。现在必须明确拒绝。
    #[tokio::test]
    async fn receipt_reports_the_effective_base_not_the_requested_one() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let handle = GitDiffHandle::new();
        let tool = ShowGitDiffTool::new(
            handle.clone(),
            Some("thread-1".into()),
            dir.path().to_path_buf(),
            None,
        );
        let result = tool
            .call(serde_json::json!({ "base": "no-such-ref" }))
            .await;
        assert!(
            result.is_error,
            "an unusable rev must fail loudly: {result:?}"
        );
        let text = text_of(&result);
        assert!(
            text.contains("no-such-ref"),
            "the receipt should say which rev was rejected: {text}"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_command_is_echoed_in_the_receipt_and_broadcast() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let handle = GitDiffHandle::new();
        let mut rx = handle.subscribe();
        let tool = ShowGitDiffTool::new(
            handle.clone(),
            Some("thread-1".into()),
            dir.path().to_path_buf(),
            None,
        );
        let result = tool
            .call(serde_json::json!({ "command": "git log --oneline -1" }))
            .await;
        assert!(
            !result.is_error,
            "a read-only command must succeed: {result:?}"
        );
        assert!(text_of(&result).contains("git log"));
        let focus = rx.recv().await.unwrap();
        assert_eq!(focus.command.as_deref(), Some("git log --oneline -1"));
        assert!(focus.frame.is_some(), "the frame must ride along");
    }

    #[tokio::test]
    async fn the_receipt_names_the_session_base_when_no_base_was_given() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let anchor = crate::git_diff::session_anchor(dir.path());
        let tool = ShowGitDiffTool::new(
            GitDiffHandle::new(),
            None,
            dir.path().to_path_buf(),
            anchor.clone(),
        );
        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        let text = text_of(&result);
        assert!(
            text.contains("session") || text.contains("会话"),
            "must name the session base: {text}"
        );
        assert!(
            anchor
                .map(|a| text.contains(&a[..7.min(a.len())]))
                .unwrap_or(false),
            "the receipt must name the session anchor: {text}"
        );
    }

    #[test]
    fn tool_advertises_its_name_and_schema() {
        let tool = ShowGitDiffTool::new(
            GitDiffHandle::new(),
            None,
            std::path::PathBuf::from("."),
            None,
        );
        assert_eq!(tool.name(), "show_git_diff");
        assert!(tool.schema()["properties"]["base"].is_object());
        assert!(tool.schema()["properties"]["command"].is_object());
        // 视图通道是内置的：挂个 `ui` 插件的名头只会误导。
        assert_eq!(tool.metadata().source, ToolSource::Builtin);
    }
}
