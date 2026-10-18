//! 组长的私有决策账本 `memory.md` 的读写与检索。
//!
//! 只装**不可再生**的信息（决策与理由、长期偏好、跨 thread 约定）——可再生的
//! 一律现算，见 spec §7.3。行格式是有意为之的「一行一条 JSON」：条目里含换行
//! （模型很爱写多行）也不会破坏结构，且追加是纯 `O_APPEND`，无读-改-写竞态。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 一条账本条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// Unix 毫秒。写入时由 `lead_remember` 铸造，不由模型提供。
    pub timestamp_ms: i64,
    /// 来源（相关 thread id / turn id），使「这条是谁、何时、因为什么写的」可审计。
    pub source: String,
    pub text: String,
}

/// `<project_root>/.yi-agent/lead_memory/memory.md`。
pub fn memory_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".yi-agent")
        .join("lead_memory")
        .join("memory.md")
}

/// 该 thread 是不是组长。
///
/// **唯一权威**是落盘的 `ThreadMeta.role == Some(LEAD_ROLE)`,且不在卡片 worktree
/// 里(见 `ThreadStore::is_lead_meta`):标题、cwd、名字都能被人为凑出组长的样子,
/// 而 role 只有 `lead::ensure_lead_meta` 会写;卡片 worktree 里的那份则是误落盘的
/// 假组长(真机已有 9 例)。读不到(`load` 报错 / 没有 meta)一律视为非组长——
/// 装配期放行一个非组长去写项目账本,比拒绝一个真组长严重得多,故缺证据时不授予。
///
/// 组长 meta 是**懒创建**的:刚落盘、还没跑过一轮的组长没有 `.jsonl`,但
/// `ThreadStore::load` 对 meta-only 也返回 `Some`,所以新建的组长照样能被认出。
pub fn is_lead_thread(store: &crate::thread_store::ThreadStore, thread_id: &str) -> bool {
    store
        .load(thread_id)
        .ok()
        .flatten()
        .map(|l| crate::thread_store::is_lead_meta(&l.meta))
        .unwrap_or(false)
}

/// 追加一条。目录不存在则创建。只 `append`，不做读-改-写。
pub fn append_entry(project_root: &Path, entry: &MemoryEntry) -> std::io::Result<()> {
    let path = memory_path(project_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())
}

/// 读出全部条目。文件不存在视为空；无法解析的行跳过（账本损坏不应让 lead 崩）。
///
/// 跳过时每行记一条 stderr（与 `thread_store` 的 corrupt-record 处理一致）：账本装的是
/// **不可再生**信息，静默丢行会让 lead 以为「记性变少」而不是「文件坏了」。半行
/// （大条目 `write_all` 跨多次 syscall，中途被杀会留半行）与未来的 schema 漂移都会走这里。
pub fn read_entries(project_root: &Path) -> std::io::Result<Vec<MemoryEntry>> {
    let path = memory_path(project_root);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str::<MemoryEntry>(l) {
            Ok(entry) => Some(entry),
            Err(e) => {
                eprintln!("[app-server] skipping corrupt lead memory line: {e}");
                None
            }
        })
        .collect())
}

/// 朴素子串检索（ASCII 不区分大小写），按时间升序返回至多 `limit` 条。
///
/// 不做索引、不做分词：`memory.md` 本期无界增长是已知限制（spec §7.6），
/// 现在给它加索引是过早优化。
pub fn search(project_root: &Path, query: &str, limit: usize) -> std::io::Result<Vec<MemoryEntry>> {
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let mut hits: Vec<MemoryEntry> = read_entries(project_root)?
        .into_iter()
        .filter(|e| e.text.to_lowercase().contains(&needle))
        .collect();
    hits.sort_by_key(|e| e.timestamp_ms);
    hits.truncate(limit);
    Ok(hits)
}

use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

fn lead_memory_metadata() -> ToolMetadata {
    ToolMetadata {
        source: ToolSource::Plugin {
            name: "lead".to_string(),
        },
        requires_confirmation: false,
        read_only: false, // 调用方按需覆盖
        version: None,
    }
}

/// 向 `memory.md` 追加一条。**只由 lead 使用**（注册点见 `build_runtime_tooling`）。
///
/// 不需要用户确认是刻意的：后台无人值守时若需批准，就写不了，缺口原样回来
/// （spec §7.3 硬约束 2）。可审计性由「时间戳 + 来源 + 本次调用进 lead 的
/// `.jsonl`」三者保证。
pub struct LeadRememberTool {
    project_root: PathBuf,
}

impl LeadRememberTool {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }
}

#[derive(Deserialize)]
struct LeadRememberArgs {
    text: String,
    #[serde(default)]
    source: Option<String>,
}

#[async_trait::async_trait]
impl Tool for LeadRememberTool {
    fn name(&self) -> &str {
        "lead_remember"
    }

    fn description(&self) -> &str {
        "Append one entry to this project's lead memory. Use it ONLY for information that cannot be recomputed: why you rejected a thread, a long-term preference the user stated, a cross-thread convention. Never record thread status/progress — those are recomputed every wake-up. Entry text must be self-contained."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "The fact to remember. Self-contained, with the reason." },
                "source": { "type": "string", "description": "Where this came from (thread id / turn id). Optional." }
            },
            "required": ["text"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let parsed: LeadRememberArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        let text = parsed.text.trim().to_string();
        if text.is_empty() {
            return ToolResult::error("text must not be empty");
        }
        let entry = MemoryEntry {
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            source: parsed.source.unwrap_or_else(|| "lead".to_string()),
            text,
        };
        match append_entry(&self.project_root, &entry) {
            Ok(()) => ToolResult::text(format!(
                "已记入项目账本（{}）。",
                memory_path(&self.project_root).display()
            )),
            Err(e) => ToolResult::error(format!("failed to write lead memory: {e}")),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        lead_memory_metadata()
    }
}

/// 检索 `memory.md`。**按需**返回，绝不整份灌进上下文（spec §7.3 硬约束 3）。
pub struct LeadRecallTool {
    project_root: PathBuf,
}

impl LeadRecallTool {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }
}

#[derive(Deserialize)]
struct LeadRecallArgs {
    query: String,
    #[serde(default = "default_recall_limit")]
    limit: usize,
}

fn default_recall_limit() -> usize {
    10
}

#[async_trait::async_trait]
impl Tool for LeadRecallTool {
    fn name(&self) -> &str {
        "lead_recall"
    }

    fn description(&self) -> &str {
        "Search this project's lead memory for entries matching a query. Returns only matching entries (never the whole ledger). Call it when you need to recall why a past decision was made."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Substring to match (case-insensitive)." },
                "limit": { "type": "number", "description": "Max entries to return (default 10)." }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let parsed: LeadRecallArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        match search(&self.project_root, &parsed.query, parsed.limit) {
            Ok(entries) if entries.is_empty() => ToolResult::text("账本里没有匹配的条目。"),
            Ok(entries) => {
                let mut out = String::new();
                for e in entries {
                    out.push_str(&format!(
                        "- [{}] ({}) {}\n",
                        e.timestamp_ms, e.source, e.text
                    ));
                }
                ToolResult::text(out)
            }
            Err(e) => ToolResult::error(format!("failed to read lead memory: {e}")),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            read_only: true,
            ..lead_memory_metadata()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    #[test]
    fn memory_path_is_under_the_project_state_dir() {
        let dir = temp_project();
        let path = memory_path(dir.path());
        assert_eq!(
            path,
            dir.path()
                .join(".yi-agent")
                .join("lead_memory")
                .join("memory.md")
        );
    }

    #[test]
    fn appending_then_reading_round_trips_entries_in_order() {
        let dir = temp_project();
        let first = MemoryEntry {
            timestamp_ms: 1_000,
            source: "thread-a".into(),
            text: "为什么打回 thread A：偏离了验收标准".into(),
        };
        let second = MemoryEntry {
            timestamp_ms: 2_000,
            source: "thread-b".into(),
            text: "用户长期偏好：提交前必须跑 fmt".into(),
        };
        append_entry(dir.path(), &first).expect("append first");
        append_entry(dir.path(), &second).expect("append second");

        let loaded = read_entries(dir.path()).expect("read");
        assert_eq!(loaded, vec![first, second]);
    }

    #[test]
    fn reading_a_project_without_memory_yields_no_entries() {
        let dir = temp_project();
        assert!(read_entries(dir.path()).expect("read").is_empty());
    }

    #[test]
    fn search_matches_substrings_and_honours_the_limit() {
        let dir = temp_project();
        for (ms, text) in [(1, "打回 thread A"), (2, "打回 thread B"), (3, "无关内容")] {
            append_entry(
                dir.path(),
                &MemoryEntry {
                    timestamp_ms: ms,
                    source: "s".into(),
                    text: text.into(),
                },
            )
            .expect("append");
        }
        let hits = search(dir.path(), "打回", 10).expect("search");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].timestamp_ms, 1);
        assert_eq!(hits[1].timestamp_ms, 2);

        let capped = search(dir.path(), "打回", 1).expect("search");
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn search_is_case_insensitive_for_ascii() {
        let dir = temp_project();
        append_entry(
            dir.path(),
            &MemoryEntry {
                timestamp_ms: 1,
                source: "s".into(),
                text: "Prefer FMT".into(),
            },
        )
        .expect("append");
        assert_eq!(search(dir.path(), "fmt", 10).expect("search").len(), 1);
    }

    #[test]
    fn corrupt_lines_are_skipped_without_losing_valid_entries() {
        let dir = temp_project();
        let valid = MemoryEntry {
            timestamp_ms: 1,
            source: "s".into(),
            text: "打回 thread A：偏离了验收标准".into(),
        };
        let path = memory_path(dir.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create_dir_all");
        std::fs::write(
            &path,
            format!(
                "{}\n{{\"timestamp_ms\": 2, \"source\": \"s\", \"te",
                serde_json::to_string(&valid).expect("serialize")
            ),
        )
        .expect("write");

        let loaded = read_entries(dir.path()).expect("read");
        assert_eq!(loaded, vec![valid]);
    }

    use yi_agent_core::Tool;

    #[tokio::test]
    async fn lead_remember_appends_a_timestamped_entry() {
        let dir = temp_project();
        let tool = LeadRememberTool::new(dir.path().to_path_buf());
        let out = tool
            .call(serde_json::json!({
                "text": "为什么打回 thread A：偏离验收标准",
                "source": "thread-a",
            }))
            .await;
        assert!(!out.is_error, "call must succeed: {out:?}");

        let entries = read_entries(dir.path()).expect("read");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "为什么打回 thread A：偏离验收标准");
        assert_eq!(entries[0].source, "thread-a");
        assert!(
            entries[0].timestamp_ms > 0,
            "timestamp must be minted by the tool"
        );
    }

    #[tokio::test]
    async fn lead_remember_rejects_empty_text() {
        let dir = temp_project();
        let tool = LeadRememberTool::new(dir.path().to_path_buf());
        let out = tool.call(serde_json::json!({ "text": "   " })).await;
        assert!(out.is_error, "blank text must be rejected");
        assert!(read_entries(dir.path()).expect("read").is_empty());
    }

    #[tokio::test]
    async fn lead_recall_returns_matching_entries_only() {
        let dir = temp_project();
        let remember = LeadRememberTool::new(dir.path().to_path_buf());
        remember
            .call(serde_json::json!({ "text": "打回 thread A", "source": "a" }))
            .await;
        remember
            .call(serde_json::json!({ "text": "无关内容", "source": "b" }))
            .await;

        let recall = LeadRecallTool::new(dir.path().to_path_buf());
        let out = recall.call(serde_json::json!({ "query": "打回" })).await;
        assert!(!out.is_error, "recall must succeed: {out:?}");
        let body = format!("{out:?}");
        assert!(
            body.contains("打回 thread A"),
            "hit must be present: {body}"
        );
        assert!(!body.contains("无关内容"), "non-hit must be absent: {body}");
    }

    #[tokio::test]
    async fn lead_recall_reports_an_empty_ledger_without_erroring() {
        let dir = temp_project();
        let recall = LeadRecallTool::new(dir.path().to_path_buf());
        let out = recall.call(serde_json::json!({ "query": "任何" })).await;
        assert!(!out.is_error, "empty ledger is not an error: {out:?}");
    }

    #[tokio::test]
    async fn lead_remember_is_not_read_only_and_recall_is() {
        let dir = temp_project();
        assert!(
            !LeadRememberTool::new(dir.path().to_path_buf())
                .metadata()
                .read_only
        );
        assert!(
            LeadRecallTool::new(dir.path().to_path_buf())
                .metadata()
                .read_only
        );
    }

    fn lead_test_meta(id: &str, role: Option<&str>) -> crate::thread_store::ThreadMeta {
        crate::thread_store::ThreadMeta {
            thread_id: id.into(),
            cwd: "/tmp".into(),
            model: "m".into(),
            created_at: 1,
            updated_at: 1,
            title: None,
            permission_mode: crate::thread_store::ThreadMode::Normal,
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
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

    /// 判定的正臂:落盘 `role == Some(LEAD_ROLE)` 就是组长——meta-only 也算。
    #[test]
    fn a_persisted_lead_role_is_recognised() {
        let dir = temp_project();
        let store = crate::thread_store::ThreadStore::new(dir.path());
        store
            .create(&lead_test_meta(
                "lead-1",
                Some(crate::thread_store::LEAD_ROLE),
            ))
            .expect("create");
        assert!(is_lead_thread(&store, "lead-1"));
    }

    /// 判定的负臂:每一种「像组长但不是」都必须为假,否则普通会话会拿到账本写权限。
    #[test]
    fn only_a_persisted_lead_role_counts() {
        let dir = temp_project();
        let store = crate::thread_store::ThreadStore::new(dir.path());
        // ① role 为 None(普通会话,尽管 cwd 指向同一项目根)。
        store
            .create(&lead_test_meta("plain", None))
            .expect("create");
        // ② 别的字符串冒充 role。
        store
            .create(&lead_test_meta("impostor", Some("Lead")))
            .expect("create");
        // ③ 从未落盘。
        for id in ["plain", "impostor", "ghost"] {
            assert!(
                !is_lead_thread(&store, id),
                "{id} must not count as the lead"
            );
        }
    }

    /// 非法 thread id 让 `load` 报错,此时必须**拒绝**而不是放行(缺证据不授予)。
    #[test]
    fn an_unreadable_thread_id_is_not_the_lead() {
        let dir = temp_project();
        let store = crate::thread_store::ThreadStore::new(dir.path());
        assert!(!is_lead_thread(&store, "../escape"));
    }

    /// 看板 worktree 里的 `role:"lead"` 不是组长——判定必须与 `find_lead` 同口径,
    /// 否则一个假会长到项目账本写权限(真机数据里已有 9 个这样的假组长)。
    #[test]
    fn a_lead_in_a_card_worktree_is_not_the_lead() {
        let dir = temp_project();
        let worktree = dir.path().join(".worktrees/kanban/card-1");
        std::fs::create_dir_all(&worktree).unwrap();
        let store = crate::thread_store::ThreadStore::new(&worktree);
        let mut phantom = lead_test_meta("lead-1", Some(crate::thread_store::LEAD_ROLE));
        phantom.cwd = worktree.display().to_string();
        store.create(&phantom).expect("create");

        assert!(
            !is_lead_thread(&store, "lead-1"),
            "a card worktree has no lead, so its ledger tools must stay unregistered"
        );
    }
}
