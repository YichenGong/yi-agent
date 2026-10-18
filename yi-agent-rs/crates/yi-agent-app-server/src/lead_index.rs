//! 组长的**只读视野**：从磁盘现算「名下有哪些 thread」与「某个 thread 最近说了什么」。
//!
//! 纯磁盘派（上级设计 §3.1）：**不读实时状态**。状态变化由后台 digest 推（P2 已有），
//! 本模块只负责 digest 不承载的东西——目录、交付结论、转录尾部。
//!
//! 全为纯函数：不起 app-server、不调 LLM、可单测（与 `lead_loop` 同一理由）。

use std::path::Path;

/// 一个 thread 的**磁盘**概览。不含实时状态（见模块头）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub title: Option<String>,
    pub updated_at: i64,
    pub item_count: usize,
    /// 最后一条非空助手消息的文本（截断到 [`LIST_LAST_TEXT_MAX`]）。
    pub last_text: Option<String>,
}

/// 一个 thread 的转录尾部窗口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadTail {
    pub thread_id: String,
    /// 该 thread 的 item 总数（含窗口外的）。
    pub total_items: usize,
    /// 已渲染好的逐行文本（时间正序）。
    pub shown_items: Vec<String>,
}

pub const LIST_DEFAULT_LIMIT: usize = 50;
pub const LIST_MAX_LIMIT: usize = 200;
pub const READ_DEFAULT_ITEMS: usize = 20;
pub const READ_MAX_ITEMS: usize = 50;
pub const LIST_LAST_TEXT_MAX: usize = 200;
pub const READ_ITEM_MAX: usize = 500;
pub const READ_MAX_BYTES: usize = 32 * 1024;

/// 截断到 `max` 个字符（按 `char` 计，避免切断多字节）。
fn elide(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// 一份 thread 转录的**流式**扫描结果。
///
/// 为什么不复用 `ThreadStore::load`：`load` 会把整份转录建成 `Vec<Item>`（还要合流
/// partial、重建 meta）再返回。列表要为每个 thread 取「最后一条结论 + item 数」，
/// 50 个 thread 就是 50 份完整转录同时进内存——正是 spec §4.2 / §10 要避免的。
/// 这里改为逐行流式读：内存只占「一行」加尾部窗口（≤ `keep_tail` 条），不建整份
/// items；`total_items` 顺带在流式过程中计数（列表要报「（N 条）」）。
struct TranscriptScan {
    /// 该 thread 的 item 总数（含尾部窗口之外的）。
    total_items: usize,
    /// 最后一条非空 `AgentMessage.text`（**未截断**；调用方按各自上限截断）。
    last_agent_text: Option<String>,
    /// 最后 `keep_tail` 条 item，时间升序（只有 `read_tail` 需要，故可为空）。
    tail_items: Vec<crate::protocol::Item>,
}

/// 逐行**流式**扫描某 thread 的 `.jsonl`（绝不把整份转录读进内存）。
///
/// - `Ok(None)` = 该 id 在本 store 内既无 meta 也无 jsonl（绝不读项目外路径）。
///   判定用 `ThreadStore::exists`（只查文件是否存在），**不**用 `load`——后者正是
///   本函数要避开的整读。
/// - 文件缺失视为空；损坏行跳过并记 stderr（与 `ThreadStore::read_log` 的既有宽容
///   口径一致，不让一行坏数据废掉整个列表）。
/// - `keep_tail = 0` 时不留尾部窗口（列表只要计数与最后结论，内存更省）。
fn scan_transcript(
    project_root: &Path,
    thread_id: &str,
    keep_tail: usize,
) -> std::io::Result<Option<TranscriptScan>> {
    use std::collections::VecDeque;
    use std::io::BufRead;

    let store = crate::thread_store::ThreadStore::new(project_root);
    if !store.exists(thread_id) {
        return Ok(None);
    }
    let path = store.root().join(format!("{thread_id}.jsonl"));

    let mut total_items = 0usize;
    let mut last_agent_text: Option<String> = None;
    let mut tail: VecDeque<crate::protocol::Item> = VecDeque::new();

    match std::fs::File::open(&path) {
        Ok(file) => {
            for line in std::io::BufReader::new(file).lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("[app-server] skipping unreadable transcript line: {e}");
                        continue;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let crate::thread_store::TurnLine::Turn { items, .. } =
                    match serde_json::from_str::<crate::thread_store::TurnLine>(&line) {
                        Ok(t) => t,
                        Err(e) => {
                            eprintln!("[app-server] skipping corrupt transcript line: {e}");
                            continue;
                        }
                    };
                for item in items {
                    total_items += 1;
                    if let crate::protocol::Item::AgentMessage { text, .. } = &item {
                        if !text.trim().is_empty() {
                            last_agent_text = Some(text.clone());
                        }
                    }
                    if keep_tail > 0 {
                        tail.push_back(item);
                        while tail.len() > keep_tail {
                            tail.pop_front();
                        }
                    }
                }
            }
        }
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    Ok(Some(TranscriptScan {
        total_items,
        last_agent_text,
        tail_items: tail.into_iter().collect(),
    }))
}

/// 列出本项目（本 store）的 thread 概览，按 `updated_at` 降序。
///
/// 跳过组长自己（`role == LEAD_ROLE`）：它对「我手下有谁」没有信息量，且会让列表自指。
/// 返回 `(窗口, 总数)`，总数**不含**被跳过的组长、含被 `limit` 截掉的部分。
/// 不新增存储：只读既有 `*.meta.json` 与 `*.jsonl`。
pub fn list_summaries(
    project_root: &Path,
    limit: usize,
) -> std::io::Result<(Vec<ThreadSummary>, usize)> {
    let store = crate::thread_store::ThreadStore::new(project_root);
    let metas: Vec<crate::thread_store::ThreadMeta> = store
        .list()?
        .into_iter()
        .filter(|m| m.role.as_deref() != Some(crate::thread_store::LEAD_ROLE))
        .collect();
    let total = metas.len();
    let window: Vec<crate::thread_store::ThreadMeta> = metas.into_iter().take(limit).collect();
    let summaries = window
        .into_iter()
        .map(|m| {
            let scan = scan_transcript(project_root, &m.thread_id, 0)
                .ok()
                .flatten();
            let (item_count, last_text) = match scan {
                Some(s) => (
                    s.total_items,
                    s.last_agent_text
                        .as_deref()
                        .map(|t| elide(t, LIST_LAST_TEXT_MAX)),
                ),
                // 读不到（文件权限 / 非 NotFound 的 IO 错误）不让整个列表失败，
                // 与 `ThreadStore::list` 跳损坏 meta 的宽容口径一致。
                None => (0, None),
            };
            ThreadSummary {
                thread_id: m.thread_id,
                title: m.title,
                updated_at: m.updated_at,
                item_count,
                last_text,
            }
        })
        .collect();
    Ok((summaries, total))
}

/// 读某 thread 转录的尾部窗口。`None` = 该 id 不属于本 store（绝不读项目外路径）。
pub fn read_tail(
    project_root: &Path,
    thread_id: &str,
    limit: usize,
) -> std::io::Result<Option<ThreadTail>> {
    let Some(scan) = scan_transcript(project_root, thread_id, limit)? else {
        return Ok(None);
    };
    let mut shown_items: Vec<String> = scan.tail_items.iter().map(render_item).collect();
    // 整体字节上限：超了从**最旧**一端丢，并在头部标注（spec §5.3 / §6.4 注入预算）。
    let mut bytes: usize = shown_items.iter().map(|s| s.len()).sum();
    let mut dropped = 0usize;
    while bytes > READ_MAX_BYTES && !shown_items.is_empty() {
        bytes -= shown_items.remove(0).len();
        dropped += 1;
    }
    if dropped > 0 {
        shown_items.insert(0, format!("（更早的 {dropped} 条已省略）"));
    }
    Ok(Some(ThreadTail {
        thread_id: thread_id.to_string(),
        total_items: scan.total_items,
        shown_items,
    }))
}

/// 一条 item 的单行渲染（截断到 [`READ_ITEM_MAX`]）。
fn render_item(item: &crate::protocol::Item) -> String {
    use crate::protocol::Item;
    match item {
        Item::UserMessage { text, .. } => format!("用户：{}", elide(text, READ_ITEM_MAX)),
        Item::UserInterjection { text, .. } => {
            format!("用户（中途）：{}", elide(text, READ_ITEM_MAX))
        }
        Item::AgentMessage { text, .. } => format!("助手：{}", elide(text, READ_ITEM_MAX)),
        Item::ToolCall { name, status, .. } => format!("工具 {name}（{status:?}）"),
        Item::Thinking { .. } => "（思考块，略）".to_string(),
    }
}

/// 渲染列表。首行**必须**声明不含实时状态，防组长把「没看到状态」当成「没事」。
pub fn render_summaries(summaries: &[ThreadSummary], total: usize, project_root: &Path) -> String {
    if summaries.is_empty() {
        return format!(
            "本目录（{}）目前没有任何会话。\n（本列表不含实时状态；状态变化会由后台唤醒推送。）",
            project_root.display()
        );
    }
    let mut out = format!(
        "本目录（{}）共有 {total} 个会话（本列表不含实时状态；状态变化会由后台唤醒推送）：\n",
        project_root.display()
    );
    for s in summaries {
        let title = s.title.as_deref().unwrap_or("(无标题)");
        out.push_str(&format!(
            "- {} 「{title}」 更新 {}（{} 条）",
            s.thread_id, s.updated_at, s.item_count
        ));
        if let Some(last) = &s.last_text {
            out.push_str(&format!(" 最后：{last}"));
        }
        out.push('\n');
    }
    if total > summaries.len() {
        out.push_str(&format!(
            "（共 {total} 个会话，仅显示最近 {} 个。）\n",
            summaries.len()
        ));
    }
    out
}

/// 渲染转录尾部。截断必须披露，否则组长以为看到了全部。
pub fn render_tail(tail: &ThreadTail) -> String {
    let mut out = format!(
        "{} 的转录：共 {} 条，显示最近 {} 条：\n",
        tail.thread_id,
        tail.total_items,
        tail.shown_items.len()
    );
    for line in &tail.shown_items {
        out.push_str(line);
        out.push('\n');
    }
    out
}

use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use yi_agent_core::{Tool, ToolResult};

/// 列出本项目里组长的**手下会话**（磁盘真相，不含实时状态）。
pub struct LeadListThreadsTool {
    project_root: PathBuf,
}

impl LeadListThreadsTool {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }
}

#[derive(Deserialize)]
struct LeadListArgs {
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for LeadListThreadsTool {
    fn name(&self) -> &str {
        "lead_list_threads"
    }

    fn description(&self) -> &str {
        "List the conversations in this project that you (the lead) are responsible for. Shows each thread's id, title, last update time, item count, and its last assistant conclusion. This is a disk-only snapshot and does NOT include live status (running/awaiting approval) — status changes arrive via background wake-ups."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": { "type": "number", "description": "Max threads to list (default 50, max 200)." }
            }
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let parsed: LeadListArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        let limit = parsed
            .limit
            .unwrap_or(LIST_DEFAULT_LIMIT)
            .min(LIST_MAX_LIMIT);
        match list_summaries(&self.project_root, limit) {
            Ok((rows, total)) => {
                ToolResult::text(render_summaries(&rows, total, &self.project_root))
            }
            Err(e) => ToolResult::error(format!("failed to list threads: {e}")),
        }
    }

    fn metadata(&self) -> yi_agent_core::ToolMetadata {
        yi_agent_core::ToolMetadata {
            read_only: true,
            ..crate::lead_memory::lead_memory_metadata()
        }
    }
}

/// 读某个手下会话的**转录尾部**（最近的来往）。
pub struct LeadReadThreadTool {
    project_root: PathBuf,
}

impl LeadReadThreadTool {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }
}

#[derive(Deserialize)]
struct LeadReadArgs {
    thread_id: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for LeadReadThreadTool {
    fn name(&self) -> &str {
        "lead_read_thread"
    }

    fn description(&self) -> &str {
        "Read the tail of one of your project's conversation transcripts (the most recent items). Use it to see what a thread actually delivered or where it got stuck. Returns only the most recent items, and says how many were omitted."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "thread_id": { "type": "string", "description": "The thread id to read (must belong to this project)." },
                "limit": { "type": "number", "description": "Max items to return (default 20, max 50)." }
            },
            "required": ["thread_id"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let parsed: LeadReadArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        let limit = parsed
            .limit
            .unwrap_or(READ_DEFAULT_ITEMS)
            .min(READ_MAX_ITEMS);
        match read_tail(&self.project_root, &parsed.thread_id, limit) {
            Ok(Some(tail)) => ToolResult::text(render_tail(&tail)),
            Ok(None) => ToolResult::error(format!(
                "no such thread in this project: {} (it may belong to another project)",
                parsed.thread_id
            )),
            Err(e) => ToolResult::error(format!("failed to read thread: {e}")),
        }
    }

    fn metadata(&self) -> yi_agent_core::ToolMetadata {
        yi_agent_core::ToolMetadata {
            read_only: true,
            ..crate::lead_memory::lead_memory_metadata()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread_store::{LEAD_ROLE, ThreadMeta, ThreadMode, ThreadStore};

    fn meta(id: &str, title: Option<&str>, updated_at: i64, role: Option<&str>) -> ThreadMeta {
        ThreadMeta {
            thread_id: id.into(),
            cwd: "/tmp".into(),
            model: "m".into(),
            created_at: 1,
            updated_at,
            title: title.map(str::to_string),
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            anchor_sha: None,
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: true,
            extra_roots: Vec::new(),
            role: role.map(str::to_string),
            content_epoch: 0,
            last_item_id: None,
        }
    }

    #[test]
    fn list_skips_the_lead_itself_but_counts_the_rest() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&meta("t-a", Some("A"), 10, None)).unwrap();
        store.create(&meta("t-b", Some("B"), 20, None)).unwrap();
        store
            .create(&meta("lead-1", Some("组长"), 30, Some(LEAD_ROLE)))
            .unwrap();

        let (rows, total) = list_summaries(dir.path(), 50).expect("list");
        assert_eq!(total, 2, "the lead itself must not be counted");
        assert_eq!(rows.len(), 2);
        // 按 updated_at 降序（ThreadStore::list 的既有口径）。
        assert_eq!(rows[0].thread_id, "t-b");
        assert_eq!(rows[1].thread_id, "t-a");
    }

    #[test]
    fn list_reports_the_last_assistant_text_of_each_thread() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&meta("t-a", Some("A"), 10, None)).unwrap();
        store
            .append_turn(
                "t-a",
                &crate::thread_store::TurnLine::Turn {
                    items: vec![
                        crate::protocol::Item::UserMessage {
                            id: "user-1".into(),
                            text: "干活".into(),
                            attachments: Vec::new(),
                            images: Vec::new(),
                        },
                        crate::protocol::Item::AgentMessage {
                            id: "a-1".into(),
                            text: "已完成，等你验收".into(),
                        },
                    ],
                    usage: None,
                    messages: Vec::new(),
                },
            )
            .unwrap();

        let (rows, _) = list_summaries(dir.path(), 50).expect("list");
        assert_eq!(rows[0].last_text.as_deref(), Some("已完成，等你验收"));
        assert_eq!(rows[0].item_count, 2);
    }

    #[test]
    fn read_tail_returns_the_most_recent_items_and_the_total() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&meta("t-a", Some("A"), 10, None)).unwrap();
        for n in 0..5 {
            store
                .append_turn(
                    "t-a",
                    &crate::thread_store::TurnLine::Turn {
                        items: vec![crate::protocol::Item::AgentMessage {
                            id: format!("a-{n}"),
                            text: format!("第 {n} 条"),
                        }],
                        usage: None,
                        messages: Vec::new(),
                    },
                )
                .unwrap();
        }
        let tail = read_tail(dir.path(), "t-a", 2)
            .expect("read")
            .expect("some");
        assert_eq!(tail.total_items, 5);
        assert_eq!(tail.shown_items.len(), 2);
        // 取尾部：最近两条（第 3、4 条）。
        assert!(
            tail.shown_items[0].contains("第 3 条"),
            "{:?}",
            tail.shown_items
        );
        assert!(
            tail.shown_items[1].contains("第 4 条"),
            "{:?}",
            tail.shown_items
        );
    }

    #[test]
    fn read_tail_returns_none_for_a_thread_outside_this_store() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(
            read_tail(dir.path(), "not-here", 20)
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn render_summaries_says_it_carries_no_live_status() {
        let dir = tempfile::TempDir::new().unwrap();
        let rows = vec![ThreadSummary {
            thread_id: "t-a".into(),
            title: Some("A".into()),
            updated_at: 10,
            item_count: 3,
            last_text: Some("等你验收".into()),
        }];
        let out = render_summaries(&rows, 1, dir.path());
        // 首行必须声明不含实时状态，否则组长会把「没看到状态」当成「没事」。
        assert!(out.contains("不含实时状态"), "{out}");
        assert!(out.contains("t-a"));
        assert!(out.contains("等你验收"));
    }

    #[test]
    fn render_tail_discloses_truncation() {
        let tail = ThreadTail {
            thread_id: "t-a".into(),
            total_items: 50,
            shown_items: vec!["助手：最近一条".into()],
        };
        let out = render_tail(&tail);
        assert!(out.contains("共 50 条"), "{out}");
        assert!(out.contains("最近 1 条"), "{out}");
    }

    #[tokio::test]
    async fn lead_list_threads_tool_reports_the_projects_threads() {
        use yi_agent_core::Tool;
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&meta("t-a", Some("A"), 10, None)).unwrap();
        let tool = LeadListThreadsTool::new(dir.path().to_path_buf());
        assert_eq!(tool.name(), "lead_list_threads");
        assert!(tool.metadata().read_only);
        let out = tool.call(serde_json::json!({})).await;
        assert!(!out.is_error, "{out:?}");
        let text = tool_text(&out);
        assert!(text.contains("t-a"), "{text}");
    }

    #[tokio::test]
    async fn lead_read_thread_tool_refuses_a_thread_outside_the_project() {
        use yi_agent_core::Tool;
        let dir = tempfile::TempDir::new().unwrap();
        let tool = LeadReadThreadTool::new(dir.path().to_path_buf());
        assert_eq!(tool.name(), "lead_read_thread");
        let out = tool.call(serde_json::json!({ "thread_id": "nope" })).await;
        assert!(out.is_error, "must refuse an unknown thread: {out:?}");
    }

    #[tokio::test]
    async fn lead_read_thread_tool_requires_a_thread_id() {
        use yi_agent_core::Tool;
        let dir = tempfile::TempDir::new().unwrap();
        let tool = LeadReadThreadTool::new(dir.path().to_path_buf());
        let out = tool.call(serde_json::json!({})).await;
        assert!(out.is_error);
    }

    /// 从 `ToolResult` 里取纯文本（与既有测试同款小工具）。
    fn tool_text(result: &yi_agent_core::ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|b| match b {
                yi_agent_core::ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
