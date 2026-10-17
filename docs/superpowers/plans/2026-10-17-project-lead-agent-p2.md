# 项目组长 Agent P2（前后台 async + lead_memory）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让组长从「被动收信」升级为「常驻后台 + 可按需阻塞求助 + 有私有决策账本」，即 spec §6（前后台 async）与 §7.2/§7.3（`lead_memory`）的落地。

**Architecture:** 三块互不耦合的能力，共用 app-server 主循环这一个「项目级唯一能看到全部冷热 thread」的位置：①`memory.md` + `lead_remember`/`lead_recall`（纯本地文件 + 两个工具，最独立）；②阻塞式 `ask_lead`（复用审批的 oneshot 登记表形状，应答者从「用户点按钮」换成「组长回话」）；③后台常驻循环（内联进主循环的 `select!`，形状照抄既有 `card_scheduler_tick` 的 3s interval tick）+ 事件唤醒 + 前台抢占（复用 `interject` 与 `cancel_token`）。**daemon 隔离不变量一字不动**：所有 thread↔lead 通信仍经 app-server 中转。

**Tech Stack:** Rust（`yi-agent-app-server`、`yi-agent-core`）、`tokio`（`select!` / `broadcast` / `oneshot` / `interval`）、TS/React（桌面，仅 P2 末尾需要的少量接线）。

## Global Constraints

- **daemon 隔离不变量不得改动。** 不得新增跨 root capability，不得改 `is_descendant_of`，不得动 `two_conversation_roots_do_not_share_children_or_reach_across` 与 `ending_one_conversation_reclaims_only_its_own_root` 这两条测试。
- **一个项目目录至多一个组长**（判定键 = `ThreadStore::root()` 父目录的 canonical 路径，即 `<cwd>/.yi-agent`）。P2 不得引入第二个组长。
- **`memory.md` 只装「不可再生」的信息。** 可再生的（thread 状态 / 进度 / 交付结论）一律现算，绝不写进 `memory.md`（否则它就是第二个真相源）。
- **`memory.md` 只由 lead 写，且只经 `lead_remember`**（禁止 lead 用 `write`/`edit` 改它）。写入**无需用户确认**（后台无人值守时要能写），但每条必须自带时间戳与来源，且这次工具调用进 lead 自己的 `.jsonl` 转录。
- **`memory.md` 进上下文必须按需**：绝不把整份灌进历史；检索走 `lead_recall`。
- **`memory.md` 无限增长 = 本期接受的已知限制**（spec §7.6）。不做裁剪、不做归纳压缩。将来在 `lead_remember` 里加。
- **子 worker 与普通 thread 不得读写 lead_memory**（spec §7.5）。它们天然看不到 `<项目>/.yi-agent/lead_memory/`（子 worker 的 workspace 在 `.worktrees/<branch>/`）。
- **后台写动作必须留痕 + 可撤销 + 不绕过状态机**（spec §8）：走既有 delivery/review 路径，不得直接改 delivery 记录。
- **阻塞式 `ask_lead` 必须有界**：默认超时 120 秒（与 TUI `wait_agent` 默认 2 分钟一致），超时返回可读文本，绝不让 thread 永久挂死。
- **前排抢占只在工具调用边界发生**（spec §6.3）；被让位的后台进度不丢（靠既有 turn checkpoint）。
- **后台唤醒只注入「有变化 / 需关注」的条目**，不注入全量索引（spec §6.4 配套措施 1、2）。
- **项目纪律靠 system prompt 注入，不在 lead 派活时重复强调**（spec §7.7）。
- **测试基线**：`cargo test -p yi-agent-app-server --lib`；前端 `cd desktop && npx vitest run`。**新增用例只增不改既有断言。**
- **禁用** `cargo test --workspace`（OOM）。跑测试需 `export PATH="$HOME/.local/bin:$PATH"`；node/npx 需 `export PATH="/opt/homebrew/bin:$PATH"`。
- **提交前必须 `cargo fmt --all`**（在 `yi-agent-rs/` 下）。commit message 用 conventional commits，**不写 `Co-Authored-By`**。
- **不在 `main` 上直接改代码**；本计划在 worktree `.worktrees/feat-project-lead-agent-p2` 内执行。
- **每完成一组需求**，同一 commit 内更新 `docs/project-management/yi-agent-app-server.md`（与 `desktop.md`，若动前端）及 README 两处索引计数。

---

## 本仓库测试 API 实况（实现者必读，避免写出不能编译的测试）

`server.rs` 的测试夹具全在 `mod tests`（约 `:11000` 起），是自由函数 + 行式收发：

| 设施 | 真实签名 | 说明 |
| --- | --- | --- |
| `Harness::new()` | `fn new() -> Self` | 默认配置；workdir 是共享固定路径 |
| `Harness::with_factory` | 见 `mod tests` | 需要自定义 workdir/factory 时用 |
| `h.send` | `async fn send(&mut self, line: &str)` | 发一整行 JSON |
| `h.read_value` | `async fn read_value(&mut self) -> serde_json::Value` | 读下一帧（可能是通知） |
| `h.shutdown` | `async fn shutdown(self)` | 收尾 |
| `start_thread` | `async fn start_thread(h: &mut Harness) -> String` | `initialize` + `thread/start`，返回 tid |
| `rpc` | `async fn rpc(h: &mut Harness, id: u64, method: &str, params: Value) -> Value` | 发 + 按 id 收响应 |
| `list_all` | `async fn list_all(h: &mut Harness) -> Value` | 调 `thread/listAll` |
| `h.lead_inbox` | `pub(crate) lead_inbox: crate::lead::LeadInbox` | 与主循环**共享**的信箱句柄（P1 加的） |
| 常量 | `PERMISSION_TIMEOUT`、`EVENT_WAIT`（30s） | |

**工具测试**：`git_diff_tool.rs` / `theme_tool.rs` / `lead.rs` 的做法是「句柄 + 工具」直接单测（`#[tokio::test]`，`handle.subscribe()` 后 `tool.call(json!({..}))`），不必起 app-server。本计划新增的 lead 工具测试照抄这个形状。

**工具 trait 真实签名**（`yi-agent-core/src/tool.rs:100`）：

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> Value;
    fn description(&self) -> &str;
    async fn call(&self, args: Value) -> ToolResult;
    fn metadata(&self) -> ToolMetadata { ToolMetadata::default() }
    // 其余方法均有默认实现，不必实现
}
```

`ToolResult::text(String)` / `ToolResult::error(String)`；`ToolMetadata { source, requires_confirmation, read_only, version }`（`ToolSource::Plugin { name }`）。

---

## File Structure

| 文件 | 职责 | 动作 |
| --- | --- | --- |
| `yi-agent-rs/crates/yi-agent-app-server/src/lead_memory.rs` | **新**：`memory.md` 的路径解析、追加、检索（纯文件 + 纯函数，可单测）+ `lead_remember` / `lead_recall` 两个工具 | Create |
| `yi-agent-rs/crates/yi-agent-app-server/src/lead.rs` | 现有组长模块：`LeadInbox` / `AskLeadTool` / `ensure_lead_meta`；P2 在此加**阻塞回信登记表**与后台唤醒事件类型 | Modify |
| `yi-agent-rs/crates/yi-agent-app-server/src/lead_loop.rs` | **新**：后台循环的一轮判定（纯函数：从 thread 快照算出「该不该唤醒」+ 唤醒理由），与主循环内联的 driver 分开，便于单测 | Create |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | 主循环接线：内联 lead tick、路由阻塞回信、装配 lead 工具、前台抢占切点 | Modify |
| `yi-agent-rs/crates/yi-agent-app-server/src/session.rs` | `ThreadSession` 增「该 thread 是否 lead」与其阻塞等待表句柄（若需） | Modify |
| `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` | 注册 `pub mod lead_memory;`、`pub(crate) mod lead_loop;` | Modify |
| `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` | 若需「按 id 取单条 meta」的便捷读（当前只有 `list()`/`load()`） | Modify（仅在确需时） |
| `yi-agent-rs/crates/yi-agent-core/src/agent.rs` | 前台抢占所需的「工具边界让位」钩子（仅当既有原语不足时；优先复用 `interject` + `cancel_token`） | Modify（条件性） |
| `desktop/src/lib/protocol.ts`、`desktop/src/App.tsx` | 若后台循环产生新的 wire 通知，前端按需接线 | Modify（仅当确需） |

**分工理由**：把「memory 的读写」彻底独立成 `lead_memory.rs`（它有自己的路径规则、自己的工具、自己的检索语义），把「该不该唤醒」的判定抽成 `lead_loop.rs` 的纯函数（这样最难的逻辑可以在不起 app-server 的情况下单测），`server.rs` 只增加接线。

**条件性动作的判定原则**：`agent.rs` 与前端两行标了「条件性/仅当确需」——实现者**优先复用既有原语**；只有当既有原语确实不够时**才**动它们，并在 commit message 与报告里明确写出「本可以用什么替代、为何替代不了」。**不要**为了「更干净」而主动扩大改动面。

---

## Task 1: `lead_memory` 模块与 `memory.md` 读写的纯函数

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/lead_memory.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`

**Interfaces:**
- Consumes: 无（纯新模块）。
- Produces:
  - `pub fn memory_path(project_root: &Path) -> PathBuf` — `<project_root>/.yi-agent/lead_memory/memory.md`
  - `pub fn append_entry(project_root: &Path, entry: &MemoryEntry) -> std::io::Result<()>` — 追加一条（自动建目录）
  - `pub fn read_entries(project_root: &Path) -> std::io::Result<Vec<MemoryEntry>>` — 读出全部条目（供 Task 4 的 `lead_recall` 检索）
  - `pub fn search(project_root: &Path, query: &str, limit: usize) -> std::io::Result<Vec<MemoryEntry>>` — 纯函数式检索（无 LLM）
  - `pub struct MemoryEntry { pub timestamp_ms: i64, pub source: String, pub text: String }`

- [ ] **Step 1: 写失败的测试**

在 `lead_memory.rs` 底部加 `#[cfg(test)] mod tests`，写：

```rust
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
            dir.path().join(".yi-agent").join("lead_memory").join("memory.md")
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
                &MemoryEntry { timestamp_ms: ms, source: "s".into(), text: text.into() },
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
            &MemoryEntry { timestamp_ms: 1, source: "s".into(), text: "Prefer FMT".into() },
        )
        .expect("append");
        assert_eq!(search(dir.path(), "fmt", 10).expect("search").len(), 1);
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_memory`
Expected: FAIL（`lead_memory` 模块不存在 / `memory_path` 未定义）

- [ ] **Step 3: 实现最小可用代码**

在 `lead_memory.rs` 顶部实现（行格式：每条一行 JSON，`serde_json` 往返，天然支持条目内含换行）：

```rust
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
        .filter_map(|l| serde_json::from_str::<MemoryEntry>(l).ok())
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
```

在 `lib.rs` 的 `pub mod lead;` 之后加 `pub mod lead_memory;`（保持字母序：`lead.rs` → `lead_memory.rs` → `model_rpc.rs`）。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_memory`
Expected: PASS（5 例）

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/lead_memory.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): 组长账本 memory.md 的读写与检索"
```

---

## Task 2: `lead_remember` / `lead_recall` 工具

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lead_memory.rs`

**Interfaces:**
- Consumes: Task 1 的 `MemoryEntry` / `append_entry` / `search` / `memory_path`。
- Produces:
  - `pub struct LeadRememberTool`，`pub fn new(project_root: PathBuf) -> Self`
  - `pub struct LeadRecallTool`，`pub fn new(project_root: PathBuf) -> Self`
  - 两者名字固定为 `"lead_remember"` / `"lead_recall"`（Task 5 据此注册）。

- [ ] **Step 1: 写失败的测试**

在 `lead_memory.rs` 的 `mod tests` 里追加：

```rust
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
        assert!(!out.is_error(), "call must succeed: {out:?}");

        let entries = read_entries(dir.path()).expect("read");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "为什么打回 thread A：偏离验收标准");
        assert_eq!(entries[0].source, "thread-a");
        assert!(entries[0].timestamp_ms > 0, "timestamp must be minted by the tool");
    }

    #[tokio::test]
    async fn lead_remember_rejects_empty_text() {
        let dir = temp_project();
        let tool = LeadRememberTool::new(dir.path().to_path_buf());
        let out = tool.call(serde_json::json!({ "text": "   " })).await;
        assert!(out.is_error(), "blank text must be rejected");
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
        assert!(!out.is_error(), "recall must succeed: {out:?}");
        let body = format!("{out:?}");
        assert!(body.contains("打回 thread A"), "hit must be present: {body}");
        assert!(!body.contains("无关内容"), "non-hit must be absent: {body}");
    }

    #[tokio::test]
    async fn lead_recall_reports_an_empty_ledger_without_erroring() {
        let dir = temp_project();
        let recall = LeadRecallTool::new(dir.path().to_path_buf());
        let out = recall.call(serde_json::json!({ "query": "任何" })).await;
        assert!(!out.is_error(), "empty ledger is not an error: {out:?}");
    }

    #[tokio::test]
    async fn lead_remember_is_not_read_only_and_recall_is() {
        let dir = temp_project();
        assert!(!LeadRememberTool::new(dir.path().to_path_buf()).metadata().read_only);
        assert!(LeadRecallTool::new(dir.path().to_path_buf()).metadata().read_only);
    }
```

> 注：`ToolResult` 是否暴露 `is_error()` 需实现者按真实 API 调整；若没有该辅助方法，用 `format!("{result:?}")` 断言包含成功/错误语义文本（参照 `lead.rs` 现有测试的写法）。**不要**为了测试去改 `ToolResult` 的公开 API。

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_remember lead_recall`
Expected: FAIL（`LeadRememberTool` / `LeadRecallTool` 未定义）

- [ ] **Step 3: 实现最小可用代码**

在 `lead_memory.rs` 追加：

```rust
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

fn lead_memory_metadata() -> ToolMetadata {
    ToolMetadata {
        source: ToolSource::Plugin { name: "lead".to_string() },
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
            Ok(entries) if entries.is_empty() => {
                ToolResult::text("账本里没有匹配的条目。")
            }
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
```

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_remember lead_recall lead_memory`
Expected: PASS

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/lead_memory.rs
git commit -m "feat(app-server): lead_remember / lead_recall 工具"
```

---

## Task 3: 阻塞式 `ask_lead`（复用审批 oneshot 形状）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lead.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs`（若需把回信登记表塞进 `ThreadSession`）

**Interfaces:**
- Consumes: P1 的 `LeadEnvelope` / `LeadInbox` / `AskLeadTool`；审批路径的 oneshot 登记表形状（`server.rs` 的 `pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>`）。
- Produces:
  - `pub struct LeadReplies` — 阻塞回信的登记表，`pub fn new() -> Self`、`pub fn register(&self, request_id: String) -> oneshot::Receiver<String>`、`pub fn resolve(&self, request_id: &str, reply: String) -> bool`（无人等待时返回 `false`）
  - `LeadEnvelope` 增字段 `pub request_id: Option<String>`（`None` = 单向投递，`Some` = 阻塞，需回信）
  - `AskLeadTool` 的 `call` 在 `blocking: true` 时：投递 → 等回信（默认 120s 超时）→ 返回回信文本或超时文本
  - `pub const ASK_LEAD_TIMEOUT: Duration = Duration::from_secs(120);`

- [ ] **Step 1: 写失败的测试**

在 `lead.rs` 的 `mod tests` 里追加：

```rust
    #[test]
    fn resolving_a_registered_request_delivers_the_reply() {
        let replies = LeadReplies::new();
        let rx = replies.register("req-1".to_string());
        assert!(replies.resolve("req-1", "组长回复：先别动".to_string()));
        assert_eq!(rx.blocking_recv().expect("reply"), "组长回复：先别动");
    }

    #[test]
    fn resolving_an_unknown_request_is_a_no_op() {
        let replies = LeadReplies::new();
        assert!(!replies.resolve("nobody", "x".to_string()));
    }

    #[test]
    fn dropping_the_receiver_does_not_panic_on_late_resolve() {
        let replies = LeadReplies::new();
        let rx = replies.register("req-2".to_string());
        drop(rx);
        // 等待方已走（超时），迟到的回信不得 panic。
        assert!(!replies.resolve("req-2", "late".to_string()));
    }

    #[tokio::test]
    async fn blocking_ask_lead_times_out_with_a_readable_message() {
        let inbox = LeadInbox::new();
        let _sub = inbox.subscribe(); // 有人听，投递会成功
        let replies = LeadReplies::new();
        let tool = AskLeadTool::new(inbox, "p".into(), "t".into())
            .with_replies(replies, std::time::Duration::from_millis(50));
        let out = tool
            .call(serde_json::json!({ "message": "怎么办", "blocking": true }))
            .await;
        let body = format!("{out:?}");
        assert!(body.contains("暂未"), "must explain the timeout: {body}");
    }

    #[test]
    fn a_blocking_envelope_carries_a_request_id() {
        let e = LeadEnvelope {
            project_key: "p".into(),
            from_thread_id: "t".into(),
            text: "hi".into(),
            request_id: Some("req-9".into()),
        };
        assert_eq!(e.request_id.as_deref(), Some("req-9"));
        let one_way = LeadEnvelope {
            project_key: "p".into(),
            from_thread_id: "t".into(),
            text: "hi".into(),
            request_id: None,
        };
        assert!(one_way.request_id.is_none());
    }
```

> 实现者注意：`with_replies` 是**本任务要求新增**的构造辅助（默认 `AskLeadTool::new` 不带回信能力 = P1 的纯单向行为，保持既有测试不变）。若你选择别的注入形状（例如把 `LeadReplies` 放进 `new` 的第 4 个参数），**必须**让 P1 既有测试仍能编译通过——它们调用的是 3 参数 `new`。优先选「加 builder 方法」，避免改 `new` 的签名。

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_replies blocking_ask_lead`
Expected: FAIL（`LeadReplies` 未定义 / `LeadEnvelope` 无 `request_id`）

- [ ] **Step 3: 实现最小可用代码**

在 `lead.rs`：

```rust
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::oneshot;

/// 阻塞式 `ask_lead` 的默认超时。与 TUI `wait_agent` 的默认 2 分钟一致
/// （spec §5.3）：组长是一个 thread、一个 agent，turn 串行，必须有界收口。
pub const ASK_LEAD_TIMEOUT: Duration = Duration::from_secs(120);

/// 阻塞回信的登记表。形状照抄审批路径的 `pending`：
/// 投递方登记一个 `oneshot::Sender` 后 await，应答方（组长那轮的收尾）回填。
#[derive(Default)]
pub struct LeadReplies {
    waiting: Mutex<HashMap<String, oneshot::Sender<String>>>,
}

impl LeadReplies {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一次等待，返回接收端。等待方超时后 drop 掉接收端即可（`resolve` 会安全失败）。
    pub fn register(&self, request_id: String) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(request_id, tx);
        rx
    }

    /// 回填一次。无人等待（已超时/从未登记）返回 `false`，不 panic。
    pub fn resolve(&self, request_id: &str, reply: String) -> bool {
        let sender = self
            .waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(request_id);
        match sender {
            Some(tx) => tx.send(reply).is_ok(),
            None => false,
        }
    }
}
```

`LeadEnvelope` 增字段（**注意 `request_id` 必须 `#[serde]` 无关，它不是 wire 类型**，只是进程内结构，直接加字段即可）：

```rust
pub struct LeadEnvelope {
    pub project_key: String,
    pub from_thread_id: String,
    pub text: String,
    /// `Some` = 阻塞投递，等这个 id 的回信；`None` = 单向投递（P1 行为）。
    pub request_id: Option<String>,
}
```

**必须**同步更新 P1 已有的两处 `LeadEnvelope { .. }` 构造（`AskLeadTool::call` 与 `lead.rs` 测试）补 `request_id: None`，否则编译失败。

`AskLeadTool` 增：

```rust
pub struct AskLeadTool {
    inbox: LeadInbox,
    project_key: String,
    from_thread_id: String,
    // P2：P1 的 `new` 不带它（纯单向），故用 Option 保持既有构造不变。
    replies: Option<(LeadReplies, Duration)>,
}

impl AskLeadTool {
    pub fn new(inbox: LeadInbox, project_key: String, from_thread_id: String) -> Self {
        Self { inbox, project_key, from_thread_id, replies: None }
    }

    /// P2：本 thread 所属项目的组长回信登记表 + 阻塞超时。
    pub fn with_replies(mut self, replies: LeadReplies, timeout: Duration) -> Self {
        self.replies = Some((replies, timeout));
        self
    }
}
```

`call` 里的 `blocking` 分支改为（替换 P1 的「显式拒绝」）：

```rust
        if parsed.blocking {
            let Some((replies, timeout)) = self.replies.clone() else {
                // P2 的装配未提供回信能力（例如 TUI 侧尚未接）：保持 P1 的显式拒绝，
                // 绝不静默降级成单向——那会让模型以为自己在等答复。
                return ToolResult::error(
                    "ask_lead blocking mode is unavailable here; resend without `blocking`",
                );
            };
            let request_id = format!("lead-ask-{}", uuid::Uuid::new_v4());
            let rx = replies.register(request_id.clone());
            if let Err(e) = self.inbox.deliver(LeadEnvelope {
                project_key: self.project_key.clone(),
                from_thread_id: self.from_thread_id.clone(),
                text: text.clone(),
                request_id: Some(request_id),
            }) {
                return ToolResult::error(format!("failed to deliver to lead: {e}"));
            }
            return match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(reply)) => ToolResult::text(format!("组长回复：{reply}")),
                Ok(Err(_)) => ToolResult::error("组长回信通道已关闭。"),
                Err(_) => ToolResult::text(
                    "组长暂未回应（已等待超时）。请自行继续手上的工作，别空等。",
                ),
            };
        }
```

`LeadReplies` 需要 `Clone`（`#[derive(Clone)]` + 内部 `Arc<Mutex<..>>`）——**实现者请用 `Arc<Mutex<HashMap<..>>>` 而非裸 `Mutex`，并 `#[derive(Clone)]`**，因为 `AskLeadTool` 的 `call` 持有 `&self`，克隆句柄比改造 `call` 的所有权简单。上面的测试写成 `LeadReplies::new()` 直接可用；若你改成 `Arc` 内部，测试无需变。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_`
Expected: PASS（P1 既有用例 + 本任务新用例全绿）

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/lead.rs
git commit -m "feat(app-server): 阻塞式 ask_lead 的回信登记表与超时收口"
```

---

## Task 4: 主循环把组长的回复路由回等待中的 thread

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lead.rs`（若 `LeadReplies` 需按项目分表）

**Interfaces:**
- Consumes: Task 3 的 `LeadReplies`。
- Produces: 主循环在组长那轮收尾时，调 `replies.resolve(request_id, <组长本轮最后的助手文本>)`；装配期把同一个 `LeadReplies` 实例注入 `AskLeadTool`。

- [ ] **Step 1: 写失败的测试**

在 `server.rs` 的 `mod tests` 里加（照既有「起 harness + rpc」的写法）：

```rust
    /// 阻塞式 ask_lead：组长那轮跑完后，回信必须回到等待的 thread 的 tool result。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_blocking_ask_lead_receives_the_lead_reply() {
        let (mut h, _dir) = isolated_harness().await;
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).await;
        let _ = read_response(&mut h, 1).await;
        let _ = start_thread(&mut h).await;
        let lead = rpc(&mut h, 2, "thread/lead", serde_json::json!({})).await;
        let lead_id = lead["result"]["thread_id"].as_str().unwrap().to_string();

        // 让组长那轮产出可辨识的回复，再断言等待方收到了它。
        // （实现者：用既有的 recording provider 夹具让组长回 "reply-42"，
        //  然后经 h.lead_inbox 投一条带 request_id 的 envelope，重放主循环路径。）
        //
        // 若主循环路径难以在测试里驱动，退化方案：直接单测
        // `resolve_lead_reply_from_turn(&completed_items, &replies)` 这个纯函数
        // ——把「本轮最后的助手文本」的抽取抽成纯函数即可单测。二选一，
        //  但**必须**有一条测试覆盖「组长的回复被取出并回填到对应 request_id」。
        let _ = lead_id;
    }
```

> **给实现者的明确指引**：本任务的难点是「组长的回复文本从哪取」。请把抽取逻辑写成**纯函数**
> `fn lead_reply_text(items: &[crate::protocol::Item]) -> Option<String>`（取本轮最后一条
> 非空 `Item::AgentMessage` 的 `text`，与 `persist_and_finish_turn` 里
> `completed_items_agent_text` 的口径一致——**先读那个函数再写**，不要另立一套），
> 然后直接单测它。主循环只需在组长那轮收尾时对这个纯函数的结果调 `replies.resolve(..)`。
> 这样测试不依赖起真实的 LLM 轮，稳且快。

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_reply_text`
Expected: FAIL（函数未定义）

- [ ] **Step 3: 实现最小可用代码**

1. `server.rs` 加纯函数（放在 `completed_items_agent_text` 附近，复用它）：

```rust
/// 组长那轮要回填给等待方 thread 的文本：本轮最后一条非空助手消息。
fn lead_reply_text(items: &[crate::protocol::Item]) -> Option<String> {
    completed_items_agent_text(&items.to_vec())
}
```

> 实现者：`completed_items_agent_text` 的真实签名可能是 `(&[Item])` 或 `(Vec<Item>)`，
> 按其实际形状适配；**不要**复制它的逻辑。

2. 主循环 `select!` 的 `lead_rx.recv()` 分支：投递后若 `envelope.request_id.is_some()`，
   把该 id 记到「待回填」旁路（例如按 `lead_target.thread_id` 记一个
   `HashMap<String /*lead thread*/, Vec<String /*request_ids*/>>`，或更简单：
   在 `ServeLauncher::deliver_to_lead` 返回时把 `request_id` 与「组长那轮的完成」关联）。
   组长那轮收尾时（`turn_rx` 收到该 lead thread 的 `TurnEvent::Finished`，或
   `persist_and_finish_turn` 路径）取出 `lead_reply_text` 并 `replies.resolve(..)`。

   **实现者指引**：本仓库既有 `completed_items_agent_text` 说明「本轮 items」在收尾处可得；
   选**最小改动**的挂点（优先 `turn_rx` 分支——它已在主循环里，且能拿到 thread_id）。
   若收尾处的 items 不易取，允许把「本轮助手文本」通过 `TurnEvent::Finished` 增带一个
   `reply: Option<String>` 字段传上来——但**必须**同步更新该枚举的**全部**构造点与匹配点
   （`grep -n "TurnEvent::Finished"`），且保持 `thread_id`/`turn_id` 语义不变。

3. 装配：把 `LeadReplies` 实例存进 `RuntimeAttachments`（与 `lead_inbox` 并列），
   在 `build_runtime_tooling` 里通过 `attach_delegation` 注入 `AskLeadTool::with_replies(..)`
   （**只给 lead 也用得上**——其实 `ask_lead` 是给普通 thread 用的，故注入点与 P1 相同）。
   `ASK_LEAD_TIMEOUT` 作为超时传入。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS（全量；新用例 + 既有 636 例）

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent-app-server/src/lead.rs
git commit -m "feat(app-server): 主循环把组长回复回填给阻塞等待的 thread"
```

---

## Task 5: 后台常驻循环的判定纯函数（`lead_loop.rs`）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/lead_loop.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`

**Interfaces:**
- Consumes: `ThreadStatus`（`crate::protocol`）、`ThreadMeta`（`crate::thread_store`）。
- Produces:
  - `pub enum WakeReason { AskLead, ThreadFinished, ThreadStuck, UserSpoke, FallbackTick }`
  - `pub struct ThreadSnapshot { pub thread_id: String, pub title: Option<String>, pub status: ThreadStatus, pub last_user_text: Option<String>, pub updated_at: i64 }`
  - `pub fn decide_wake(snapshots: &[ThreadSnapshot], pending_asks: usize) -> Vec<(String, WakeReason)>` — 纯函数：算出本轮该唤醒哪些 thread、为什么
  - `pub fn render_digest(snapshots: &[ThreadSnapshot], wakes: &[(String, WakeReason)]) -> String` — 把「有变化/需关注」的条目渲染成注入文本（不注入全量）

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, status: ThreadStatus, updated_at: i64) -> ThreadSnapshot {
        ThreadSnapshot {
            thread_id: id.into(),
            title: Some(id.into()),
            status,
            last_user_text: None,
            updated_at,
        }
    }

    #[test]
    fn a_pending_ask_wakes_the_lead() {
        let wakes = decide_wake(&[snap("t1", ThreadStatus::Idle, 10)], 1);
        assert!(wakes.iter().any(|(id, r)| id == "t1" && *r == WakeReason::AskLead));
    }

    #[test]
    fn nothing_changed_and_no_asks_means_no_wake() {
        // 全部 idle、无待处理 ask → 不唤醒（省钱、不烧 token）。
        assert!(decide_wake(&[snap("t1", ThreadStatus::Idle, 10)], 0).is_empty());
    }

    #[test]
    fn a_thread_awaiting_approval_is_stuck_and_wakes_the_lead() {
        let wakes = decide_wake(&[snap("t1", ThreadStatus::AwaitingApproval, 10)], 0);
        assert!(wakes.iter().any(|(_, r)| *r == WakeReason::ThreadStuck));
    }

    #[test]
    fn digest_only_mentions_threads_that_woke() {
        let snaps = vec![
            snap("changed", ThreadStatus::AwaitingApproval, 20),
            snap("quiet", ThreadStatus::Idle, 10),
        ];
        let wakes = decide_wake(&snaps, 0);
        let digest = render_digest(&snaps, &wakes);
        assert!(digest.contains("changed"));
        assert!(!digest.contains("quiet"), "unchanged threads must not be expanded: {digest}");
    }

    #[test]
    fn digest_never_claims_regenerated_state_as_memory() {
        // 覆盖 §6.4 的核心原则：注入的是「每轮现算的投影」，故文本里只描述现状。
        let snaps = vec![snap("t1", ThreadStatus::AwaitingApproval, 5)];
        let wakes = decide_wake(&snaps, 0);
        let digest = render_digest(&snaps, &wakes);
        assert!(digest.contains("t1"));
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_loop`
Expected: FAIL（模块不存在）

- [ ] **Step 3: 实现最小可用代码**

```rust
//! 组长后台循环的**判定**部分：什么算「有变化 / 需关注」。
//!
//! 抽成纯函数是本模块存在的全部理由——最难、最容易错的逻辑（唤醒判据）可以在
//! 不起 app-server、不花一个 token 的情况下单测。真正的循环内联在主循环的
//! `select!` 里（形状照抄 `card_scheduler_tick`），因为 `threads` 是主循环
//! 独占的 `&mut`（spec §6.1）。
//!
//! 零 LLM 成本：判据全是机械事实（状态 + 时间戳），与看板 `reconcile` 同源。

use crate::protocol::ThreadStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeReason {
    /// 有 thread 调了 `ask_lead`。
    AskLead,
    /// 有 thread 到达终态（本轮新增：[Blocked] 讨论见 brief）。
    ThreadFinished,
    /// 有 thread 卡住（等待审批超时未决 / 长时间无进展）。
    ThreadStuck,
    /// 用户在组长的 IM 里说话了。
    UserSpoke,
    /// 低频兜底 tick（扫描前几类是否被漏掉）。
    FallbackTick,
}

#[derive(Debug, Clone)]
pub struct ThreadSnapshot {
    pub thread_id: String,
    pub title: Option<String>,
    pub status: ThreadStatus,
    pub last_user_text: Option<String>,
    pub updated_at: i64,
}

/// 算出本轮该唤醒哪些 thread、为什么。空 = 不唤醒。
pub fn decide_wake(snapshots: &[ThreadSnapshot], pending_asks: usize) -> Vec<(String, WakeReason)> {
    let mut wakes = Vec::new();
    if pending_asks > 0 {
        // 有 ask 时唤醒的理由挂在「最近更新的 thread」上，只为让 digest 有个出口；
        // 真正的投递走 `lead_rx`，与这里无关（本函数只管「要不要起一轮」）。
        if let Some(latest) = snapshots.iter().max_by_key(|s| s.updated_at) {
            wakes.push((latest.thread_id.clone(), WakeReason::AskLead));
        }
    }
    for s in snapshots {
        if s.status == ThreadStatus::AwaitingApproval {
            wakes.push((s.thread_id.clone(), WakeReason::ThreadStuck));
        }
    }
    wakes
}

/// 渲染注入文本：只展开被唤醒的 thread，其余不出现（spec §6.4 配套 1、2）。
pub fn render_digest(snapshots: &[ThreadSnapshot], wakes: &[(String, WakeReason)]) -> String {
    if wakes.is_empty() {
        return String::new();
    }
    let mut out = String::from("本轮有需要你关注的变化：\n");
    for (thread_id, reason) in wakes {
        let status = snapshots
            .iter()
            .find(|s| &s.thread_id == thread_id)
            .map(|s| format!("{:?}", s.status))
            .unwrap_or_else(|| "unknown".to_string());
        out.push_str(&format!("- {thread_id}（{reason:?}，状态 {status}）\n"));
    }
    out
}
```

在 `lib.rs` 加 `pub(crate) mod lead_loop;`（与 `card_scheduler` 同为 `pub(crate)`）。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_loop`
Expected: PASS

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/lead_loop.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): 组长后台唤醒的判定纯函数"
```

---

## Task 6: 把后台循环与 lead 工具接进主循环

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 5 的 `decide_wake` / `render_digest` / `ThreadSnapshot`；P1 的 `resolve_lead_target` / `ServeLauncher::deliver_to_lead`；Task 2 的两个工具。
- Produces: 主循环新增一个低频 interval 分支（照 `card_ticker`），对每个「有组长的项目」跑一轮：收集快照 → `decide_wake` → 非空则向 lead thread 投递一轮（注入 `render_digest` 的文本）。

- [ ] **Step 1: 写失败的测试**

```rust
    /// 后台循环：一个卡住（等待审批）的 thread 必须让组长被唤醒并收到一条含它的注入。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stuck_thread_wakes_the_lead_with_a_digest() {
        // 实现者：用既有 harness 起一个 thread 并置为 AwaitingApproval
        // （可用直接改 `ThreadSession.status` 的测试后门，或经一次真实审批请求），
        // 然后驱动一轮 `lead_tick_once(..)`（把内联分支的体抽成这个可测函数），
        // 断言组长 thread 收到了包含该 thread_id 的注入。
        //
        // 若把体抽成独立函数有借用障碍（`threads: &mut`），退化为：
        // 直接单测「用一组快照调 decide_wake + render_digest」的端到端文本，
        // 并另有一条测试断言主循环确实注册了该 tick 分支（例如用一个可注入的
        // tick 触发器）。二选一，但**必须**有测试覆盖「卡住 → 组长被唤醒」。
    }
```

> **实现者指引**：优先把 tick 的体抽成
> `async fn lead_tick_once(threads, pending_asks, replies, ..) -> Vec<(String, String)>`
> （返回「thread_id → 注入文本」），使测试只需断言这个返回值，不必观测真实 LLM 轮。
> 主循环分支只负责把它算出来的注入逐条投递。

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib stuck_thread_wakes`
Expected: FAIL

- [ ] **Step 3: 实现最小可用代码**

1. 常量：`const LEAD_TICK: Duration = Duration::from_secs(30);`（低频兜底；与看板 3s 区分开，
   因为组长唤醒要花钱）。
2. 在 `serve` 里建 ticker（照 `card_ticker` 的 `interval_at` 写法，避开「首个 tick 立即就绪」）。
3. `select!` 加分支：`_ = lead_ticker.tick() => { /* 调 lead_tick_once 并投递 */ }`。
4. 投递复用 P1 的路径：`resolve_lead_target(project_key)` → `ServeLauncher { lead_target: Some(target), .. }.deliver_to_lead(&envelope)`，
   其中 `envelope.text = render_digest(..)`、`request_id: None`。
5. 只对「确实存在组长的项目」跑：用 `workspaces.list()` + `find_lead` 判定，无组长则跳过
   （**不要**在这里懒创建组长——那是 P1 的 `thread/lead` 与 `listAll` 的职责，见 P1 的门控理由）。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS（全量）

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): 组长后台循环接入主循环(事件唤醒+低频兜底)"
```

---

## Task 7: lead 工具装配（`lead_remember`/`lead_recall` 只给组长）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 2 的 `LeadRememberTool` / `LeadRecallTool`；P1 的 `LEAD_ROLE` / `find_lead`。
- Produces: `build_runtime_tooling`（或其调用方 `attach_delegation`）在**该 thread 是组长**时，额外注册 `lead_remember` / `lead_recall`，并把 `project_root` 绑为**项目根**（不是 `.worktrees/...`）。

- [ ] **Step 1: 写失败的测试**

```rust
    /// lead 专用工具的可见性：普通会话没有，组长有。
    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_lead_sees_the_memory_tools() {
        let (mut h, _dir) = isolated_harness().await;
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).await;
        let _ = read_response(&mut h, 1).await;
        let _ = start_thread(&mut h).await;                       // 普通会话
        let lead = rpc(&mut h, 2, "thread/lead", serde_json::json!({})).await;
        let _lead_id = lead["result"]["thread_id"].as_str().unwrap().to_string();

        // 断言两个 thread 的工具名集合：普通会话不含 lead_remember/lead_recall，
        // 组长含。（实现者：参照既有 `DELEGATION_TOOLS` 断言测试取工具名的做法。）
    }
```

> 实现者：判定「该 thread 是不是组长」的**唯一权威**是它落盘的 `ThreadMeta.role`
> （`== Some(LEAD_ROLE)`），**不要**靠标题、cwd 或名字猜。装配期可从
> `ThreadStore::load(thread_id)?.meta.role` 取（`load` 已返回 `meta`）。
> 请把该判定抽成一个纯/小函数（例如 `fn is_lead_thread(store: &ThreadStore, id: &str) -> bool`）便于单测。

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib only_the_lead_sees`
Expected: FAIL

- [ ] **Step 3: 实现最小可用代码**

在 `build_runtime_tooling` 里、`ask_lead` 注册附近，按 `role == Some(LEAD_ROLE)` 追加注册：

```rust
    // lead_memory 只给组长：普通 thread 与子 worker 一律看不到（spec §7.5）。
    // 判定用落盘 role（唯一权威），不靠名字/标题猜。
    if crate::lead_memory::is_lead_thread(&store, thread_id) {
        let project_root = Path::new(&workspace_root).to_path_buf();
        registry.register(Arc::new(crate::lead_memory::LeadRememberTool::new(project_root.clone())));
        registry.register(Arc::new(crate::lead_memory::LeadRecallTool::new(project_root)));
    }
```

> `is_lead_thread` 放在 `lead_memory.rs`（或 `lead.rs`，二选一，保持一致）：
> ```rust
> pub fn is_lead_thread(store: &crate::thread_store::ThreadStore, thread_id: &str) -> bool {
>     store
>         .load(thread_id)
>         .ok()
>         .flatten()
>         .map(|l| l.meta.role.as_deref() == Some(crate::thread_store::LEAD_ROLE))
>         .unwrap_or(false)
> }
> ```
> **注意 project_root**：`workspace_root` 已是该 thread 的 workspace 根（`build_runtime_tooling`
> 开头就取了）。组长的 workspace 根就是项目根（它不用 `.worktrees/`），与 spec §7.3
> 「工作目录一致性」一致。若发现 `workspace_root` 对组长并非项目根，**停下来报告**，
> 不要用 `.worktrees/` 或 `cfg.workdir` 兜底猜测。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS（全量）

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent-app-server/src/lead_memory.rs
git commit -m "feat(app-server): 仅组长可见 lead_remember / lead_recall"
```

---

## Task 8: 前台抢占（后台 turn 在工具边界让位）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`（**仅当**既有原语不足）

**Interfaces:**
- Consumes: 既有 `interject`（`Agent::interject` / `InterjectionRequest`）与 `cancel_token`。
- Produces: 组长跑后台 turn 时，若期间有前台请求（用户在 IM 说话 / 有新的 `ask_lead`），
  本轮在**工具调用边界**收口，前台请求优先跑。

- [ ] **Step 1: 写失败的测试**

> 这是 P2 里唯一**深度耦合 core** 的任务，因此测试策略必须收敛。
> **先读 `yi-agent-core/src/agent.rs` 的 `run_loop`（`:928` 起）与
> `interjection_before_end_turn_keeps_the_turn_alive`（`:2463`）**，
> 判断能否用「注入一条 priority interjection」表达让位语义。

```rust
    /// 后台 turn 被前台请求让位：前沿到达时，后台在其工具边界收口。
    #[tokio::test]
    async fn a_foreground_request_makes_the_background_turn_yield() {
        // 实现者：用 recording provider + 一个「多轮工具调用」的脚本，
        // 让后台 turn 在第 1 个工具后阻塞；期间注入前台请求，
        // 断言后台 turn 以 Interrupted/EndTurn 收口，且**已完成的工具结果保留**
        // （即 checkpoint 语义：让位不丢进度）。
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib foreground_request_makes`
Expected: FAIL

- [ ] **Step 3: 实现最小可用代码**

**实现者必须先做的判定（不得跳过）**：读 `agent.rs` 后回答——「让位」能否只用既有原语表达？

- **首选（零 core 改动）**：后台 turn 期间，前台请求经**既有** `interject` 注入，
  由 agent 在下一个 turn 边界自然折叠（`inject_pending` 已在 `max_turns` 检查后、
  THINK 前调用）。若这已满足「前台不需要等后台的 LLM 调用」这一验收（spec §6.3
  第三条：**投影秒回**——因为前台渲染读的是同一份状态，不必等后台），则**本任务不需要
  改 core**，只需在主循环侧保证：前台请求不被后台 turn 阻塞（即 `interject` 立即可达）。
  → 此情况下本任务的实现就是：**加一条测试**证明前台消息在后台 turn 运行期间即被接受
  （复用 `interject` 语义），并**明确记录**「抢占 = 投影层不阻塞 + 边界折叠」这一设计裁定。
- **备选（确需 core 改动）**：若「工具边界让位」必须新增钩子（例如让后台 turn 在一个
  工具完成后主动检查 `cancel_token` 之外的 `yield` 信号），则在 `agent.rs` 的 ACT 阶段
  之后加一个最小的 `yield_check: Option<...>`（默认 `None`，**保证既有行为逐字不变**），
  并在 `AgentConfig` 里给一个关闭的默认值。**任何** core 改动都必须：
  1. 默认路径（组长以外）行为不变——用既有测试全绿证明；
  2. 在报告里写清「为什么既有原语不够」的具体反例。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib` 且 `cargo test -p yi-agent-app-server --lib`
Expected: 均 PASS

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add -A
git commit -m "feat: 组长后台 turn 对前台请求让位"
```

---

## Task 9: 收口——文档、计数与已知限制登记

**Files:**
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/desktop.md`（若 P2 动了前端）
- Modify: `README.md`、`docs/project-management/README.md`
- Modify: `docs/superpowers/specs/2026-10-17-project-lead-agent-design.md`

**Interfaces:** 无（纯文档）。

- [ ] **Step 1: 更新模块文件**

在 `docs/project-management/yi-agent-app-server.md` 追加一条 `[x]` 条目，覆盖：
`lead_memory/memory.md` 与两个工具、阻塞式 `ask_lead`（120s 有界）、后台常驻循环
（事件唤醒 + 低频 tick）、前台让位；带真实代码位置与验证命令。**同时**在条目里明确
登记本期已知限制（`memory.md` 无限增长、不裁剪）。

- [ ] **Step 2: 同步两处索引计数**

```bash
# 以模块文件真实计数为准（不要手算）
for f in yi-agent-app-server desktop; do
  x=$(grep -cE '^- \[x\]' docs/project-management/$f.md)
  o=$(grep -cE '^- \[ \]' docs/project-management/$f.md)
  echo "$f: $x / $((x+o))"
done
```

把结果写进 `README.md` 与 `docs/project-management/README.md` 对应行。

- [ ] **Step 3: 更新 spec 状态**

把 spec 的 `### P2 前后台 async + lead_memory` 小节标为「已实现」，并把 §11 状态行改为
「P1/P2 已实现并合入，P3 待实现」。

- [ ] **Step 4: 全量验证**

```bash
export PATH="$HOME/.local/bin:$PATH"
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
cargo test -p yi-agent-store --test runtime_ipc -- \
  two_conversation_roots_do_not_share_children_or_reach_across \
  ending_one_conversation_reclaims_only_its_own_root
export PATH="/opt/homebrew/bin:$PATH"
cd ../desktop && npx vitest run && npx tsc --noEmit && echo TSC_OK
```

Expected: 全绿；daemon 不变量 2 例通过。

- [ ] **Step 5: 提交**

```bash
cd /path/to/worktree && git add -A
git commit -m "docs: 登记项目组长 Agent P2 并同步模块与索引计数"
```

---

## Self-Review

**Spec 覆盖（对照 spec §5.2/§5.3、§6、§7.2/§7.3/§7.6）**

| spec 要求 | 落点 |
| --- | --- |
| 阻塞路径复用审批形状（§5.2） | Task 3 的 `LeadReplies`（oneshot 登记表）+ Task 4 的收尾回填 |
| 排队的有界降级，默认 120s（§5.3） | Task 3 的 `ASK_LEAD_TIMEOUT` + 超时返回可读文本 |
| 回信落到 thread（§5.4） | Task 3 阻塞回信作为 tool result；Task 4 路由 |
| 后台循环住 app-server 主循环（§6.1） | Task 6 内联 tick（不是 driver、不是 spawn） |
| 唤醒 = 事件驱动 + 低频兜底（§6.2） | Task 5 `decide_wake` + Task 6 的 30s tick |
| 前台抢占（§6.3） | Task 8（优先复用 interject；投影秒回 + 边界折叠） |
| 注入预算：只注有变化的（§6.4 配套 1、2） | Task 5 的 `render_digest`（只展开被唤醒者） |
| lead 单独压缩阈值（§6.4 配套 3） | **本计划未做**——见下方「本计划明确不做」 |
| `lead_remember` / `lead_recall`（§7.2） | Task 2 |
| `memory.md` 路径与三条硬约束（§7.3） | Task 1（路径/只追加）+ Task 2（只经工具写/按需检索） |
| 只由 lead 读写（§7.5） | Task 7（按落盘 `role` 判定注册） |
| 无限增长 = 本期已知限制（§7.6） | Task 9 登记，`search` 不做索引 |

**本计划明确不做（避免实现者顺手扩大）**

- `lead_list_threads` / `lead_read_thread`（完整形态）→ **P3**。P2 的后台循环只用
  `ThreadSnapshot` 的机械字段（§7.1 的索引雏形），不实现「按需读某 thread 转录」的工具。
- `memory.md` 的裁剪/归纳压缩 → 本期接受为已知限制（spec §7.6）。
- lead 单独压缩阈值（§6.4 配套 3）→ 属「配置项」而非机制；需要真实数据才能定值，
  单列在 P3 或后续，不在本计划。
- 权限留痕 / 可撤销 → **P3**。P2 的后台循环**只读**并发起 turn，不写 delivery 记录，
  故不触发 §8 的写动作审计要求。
- 移动端专用 UI → **P3**。

**类型一致性检查**：`MemoryEntry`（Task 1）→ Task 2 使用；`LeadReplies`/`request_id`（Task 3）
→ Task 4 使用；`ThreadSnapshot`/`WakeReason`/`decide_wake`/`render_digest`（Task 5）→ Task 6 使用；
`is_lead_thread` 在 Task 7 引入并被 Task 7 的测试使用。名称全程一致。

**已知风险（实现者须在报告中回答）**

1. Task 4 的「组长回复文本」抽取：必须复用 `completed_items_agent_text`，不得另立口径。
2. Task 6 的 tick 频率与「不懒创建组长」：组长缺失时**跳过**，不得在后台循环里创建。
3. Task 7 的 `project_root`：组长必须是项目根；若 `workspace_root` 不符，**停下来报告**。
4. Task 8 的 core 改动是**条件性**的：优先零改动，改动必须默认路径行为不变。