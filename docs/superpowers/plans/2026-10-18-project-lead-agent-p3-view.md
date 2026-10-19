# 项目组长 Agent P3-A（视野工具）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给组长补上两个只读视野工具（`lead_list_threads` / `lead_read_thread`），并顺手修掉 P2 遗留的两处缺陷（`lead_recall` 取最旧不披露；后台 tick 重复注入同一 digest）。

**Architecture:** 新增 `lead_index.rs` 纯函数模块（从磁盘读 meta 与转录尾，零 LLM）；两个工具只在 `is_lead_thread` 门控内注册。实时状态**有意不注入**——状态变化由 P2 已有的后台 digest 推，本块只负责「有谁 / 交付了什么」。两处修复各自独立、各自可测。

**Tech Stack:** Rust（`yi-agent-app-server` crate：`lead_index.rs`（新）/ `lead_memory.rs` / `server.rs` / `lib.rs`）、`std::fs`。

**Spec:** `docs/superpowers/specs/2026-10-18-project-lead-agent-p3-view-design.md`

## Global Constraints

- **纯磁盘派，不注入实时状态**（spec §3.1）：两个工具**不得**引入 `Arc<Mutex<HashMap<..ThreadStatus>>>` 之类共享状态，也不得改 `attach_delegation` / `build_runtime_tooling` / `ServeLauncher` 的签名来传状态。列表**不**报 `Running` / `AwaitingApproval`。
- **只对组长注册**（spec §6，上级 §7.5）：两个工具与 `lead_remember` / `lead_recall` **同一处**、**同一条件**注册（`is_lead_thread` 分支内）。普通 thread 与子 worker 一律看不到。
- **两个工具都 `read_only: true`**：不触发审批、不改任何状态。
- **`project_root` 与账本工具同源**：`cfg.workdir`（= thread cwd = 项目根），不得改用 git toplevel（理由见上级 §7.3 三条）。
- **读范围必须在本项目 store 内**：`lead_read_thread` 的 `thread_id` 必须 `store.load()` 命中，绝不读项目外路径。
- **不新增存储**（上级 §7.1）：不建索引文件、不落盘任何状态。
- **零 core 改动**：只动 `yi-agent-app-server`；不改 `yi-agent-core` 的 `Tool` trait、不改 daemon 隔离。
- **预算上限（spec §5.3、§6.4）**：单条 item 文本 ≤ 500 字符；`lead_read_thread` 整体输出 ≤ 32 KiB；`lead_list_threads` 的「最后一条结论」≤ 200 字符。
- **默认值**：`lead_list_threads` limit 默认 50、上限 200；`lead_read_thread` limit 默认 20、上限 50。
- **测试基线**：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`（当前 main：759 passed）。新增用例只增不改既有断言，**唯一例外**是 Task 1 里那条锁住「最旧优先」的既有测试，它必须随语义一起改。
- **不许并行跑 `cargo test --workspace`**（会 OOM）；按 crate 跑。
- **提交前必须 `cargo fmt --all`**（在 `yi-agent-rs/` 下）。
- 提交信息用 conventional commits、中文、**不写** `Co-Authored-By`。

---

### Task 1: 修复 `lead_recall` 取最旧条目且不披露截断

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lead_memory.rs:86-104`（`search`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lead_memory.rs:232-256`（`LeadRecallTool::call` 的输出）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/lead_memory.rs`（`mod tests`，文件末尾）

**Interfaces:**
- Consumes: 无（改既有函数）。
- Produces: `pub fn search(project_root: &Path, query: &str, limit: usize) -> std::io::Result<Vec<MemoryEntry>>` —— **语义变更**：返回「**最近** `limit` 条」而非最旧 `limit` 条；返回顺序仍是时间**升序**（便于阅读）。新增 `pub fn search_with_total(project_root: &Path, query: &str, limit: usize) -> std::io::Result<(Vec<MemoryEntry>, usize)>` —— 第二个元素是**匹配总数**（供工具披露截断）。Task 3 会用到 `search_with_total` 的签名，保持一致。

- [ ] **Step 1: 写失败的测试**

在 `lead_memory.rs` 的 `mod tests` 里追加（放在既有 `search_matches_substrings_and_honours_the_limit` 之后）：

```rust
    #[test]
    fn search_prefers_the_most_recent_entries_when_capped() {
        // 账本装的是「决策与理由」：同一 thread 先后两次决策时，**最新那条才是
        // 权威**。截断必须从最旧的一端丢，否则组长会拿到已被推翻的旧理由——
        // 比拿不到更难排查。
        let dir = temp_project();
        for (ms, text) in [
            (1, "打回 thread A：第一版理由（已推翻）"),
            (2, "打回 thread A：第二版理由（当前）"),
            (3, "打回 thread A：第三版理由（最新）"),
        ] {
            append_entry(
                dir.path(),
                &MemoryEntry { timestamp_ms: ms, source: "s".into(), text: text.into() },
            )
            .expect("append");
        }
        let hits = search(dir.path(), "打回 thread A", 2).expect("search");
        assert_eq!(hits.len(), 2);
        // 升序返回，但窗口落在**尾部**：给的是第 2、3 条，不是第 1、2 条。
        assert_eq!(hits[0].timestamp_ms, 2);
        assert_eq!(hits[1].timestamp_ms, 3);
    }

    #[test]
    fn search_with_total_reports_every_match_not_just_the_window() {
        let dir = temp_project();
        for (ms, text) in [(1, "命中"), (2, "命中"), (3, "命中"), (4, "无关")] {
            append_entry(
                dir.path(),
                &MemoryEntry { timestamp_ms: ms, source: "s".into(), text: text.into() },
            )
            .expect("append");
        }
        let (hits, total) = search_with_total(dir.path(), "命中", 2).expect("search");
        assert_eq!(hits.len(), 2);
        assert_eq!(total, 3, "must report every match, not just the returned window");
    }
```

同时**改**既有 `search_matches_substrings_and_honours_the_limit`：它断言 `capped[0].timestamp_ms == 1`（最旧）。既然语义改成取最近，这条断言必须随之改成尾部窗口。把该测试里 `capped` 那段改为：

```rust
        let capped = search(dir.path(), "打回", 1).expect("search");
        assert_eq!(capped.len(), 1);
        // 取最近一条（时间升序返回，但窗口在尾部）。
        assert_eq!(capped[0].timestamp_ms, 2);
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_memory`
Expected: FAIL —— `search_with_total` 未定义；且 `search_prefers_the_most_recent_entries_when_capped` 断言 `hits[0].timestamp_ms == 2` 实际得 1。

- [ ] **Step 3: 实现最小可用代码**

把 `search` 替换为「取尾部窗口」，并新增 `search_with_total`：

```rust
/// 朴素子串检索（ASCII 不区分大小写），按时间升序返回至多 `limit` 条。
///
/// 截断取**最近**的 `limit` 条（丢掉最旧的）：账本装的是「决策与理由」，同一
/// thread 先后两次决策时最新那条才是权威；返回被推翻的旧理由比返回空更难排查
/// （上级设计 §7.3 的可审计目的）。返回顺序仍是时间升序，只是窗口落在尾部。
///
/// 不做索引、不做分词：`memory.md` 本期无界增长是已知限制（spec §7.6），
/// 现在给它加索引是过早优化。
pub fn search(project_root: &Path, query: &str, limit: usize) -> std::io::Result<Vec<MemoryEntry>> {
    Ok(search_with_total(project_root, query, limit)?.0)
}

/// 同 [`search`]，但**同时**返回匹配总数——供工具披露「匹配 N 条，显示最近 M 条」。
///
/// 不披露截断会让组长以为「账本里只有这几条」，从而误判自己记得比别人少。
pub fn search_with_total(
    project_root: &Path,
    query: &str,
    limit: usize,
) -> std::io::Result<(Vec<MemoryEntry>, usize)> {
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let mut hits: Vec<MemoryEntry> = read_entries(project_root)?
        .into_iter()
        .filter(|e| e.text.to_lowercase().contains(&needle))
        .collect();
    hits.sort_by_key(|e| e.timestamp_ms);
    let total = hits.len();
    // 从尾部取窗口：`split_off` 留下的是前半段，丢弃它即得最近 limit 条。
    if total > limit {
        let keep_from = total - limit;
        hits = hits.split_off(keep_from);
    }
    Ok((hits, total))
}
```

把 `LeadRecallTool::call` 的成功分支改为用 `search_with_total` 并披露截断：

```rust
    async fn call(&self, args: Value) -> ToolResult {
        let parsed: LeadRecallArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        match search_with_total(&self.project_root, &parsed.query, parsed.limit) {
            Ok((entries, total)) if entries.is_empty() => ToolResult::text("账本里没有匹配的条目。"),
            Ok((entries, total)) => {
                let mut out = String::new();
                // 截断必须说出口：不披露会让组长以为账本里只有这几条。
                if total > entries.len() {
                    out.push_str(&format!(
                        "匹配 {total} 条，显示最近 {} 条：\n",
                        entries.len()
                    ));
                }
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
```

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_memory`
Expected: PASS（含两条新测试 + 改过的 `search_matches_substrings_and_honours_the_limit`）

- [ ] **Step 5: 更新文档**

在 `docs/project-management/yi-agent-app-server.md` 的 P2 条目里，把描述 `search` 的那句
「`search` `:93` 现读现搜、**不建索引**」补上截断口径：改为
「`search` `:93` 现读现搜、**不建索引**；截断取**最近** `limit` 条并披露匹配总数（P3-A 修）」。

- [ ] **Step 6: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/lead_memory.rs docs/project-management/yi-agent-app-server.md
git commit -m "fix(app-server): lead_recall 截断改取最近条目并披露匹配总数"
```

---

### Task 2: 修复后台 tick 在审批期重复注入同一 digest

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:2519` 附近（`lead_ticker` 之前新增一个 latch 变量）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:3055-3096`（`lead_ticker.tick()` 分支）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`（或 `server.rs` 的 `mod tests`，视既有 harness 位置而定——见 Step 1）

**Interfaces:**
- Consumes: `lead_tick_once(&mut threads) -> Vec<LeadTickDelivery>`（既有）；`LeadTickDelivery { project_key, lead, text }`（既有）。
- Produces: 主循环局部量 `let mut lead_digest_latch: HashMap<String, String> = HashMap::new();`（键 = `project_key`，值 = 上次**投递过**的 digest 文本）。投递分支只投「文本与上次不同」的项，并把本次文本写回 latch。

- [ ] **Step 1: 写失败的测试**

把「去重」逻辑抽成一个**纯函数**放 `server.rs`（紧邻 `lead_tick_once`），使它可单测：

```rust
/// 过滤掉与上次**已投递**文本相同的投递项，并更新闩锁。
///
/// 背景：`decide_wake` 对每个 `AwaitingApproval` 无条件产出 `ThreadStuck`，而
/// `LEAD_TICK = 30s`、`PERMISSION_TIMEOUT = 300s`——一次审批窗会让组长被同一条
/// 消息唤醒最多 10 次，每次都是带全部上下文的完整 LLM turn（纯噪声 + 真金白银）。
///
/// 去重键取**文本本身**（而非 thread_id/原因）：不用理解状态机，天然覆盖所有
/// wake reason；态势仍每轮现算（`render_digest` 照常跑），只是不重复**投递**
/// 一模一样的文本。内容一变（状态变了 / 新 thread 卡住）即恢复投递。
fn take_changed_digests(
    deliveries: Vec<LeadTickDelivery>,
    latch: &mut HashMap<String, String>,
) -> Vec<LeadTickDelivery> {
    let mut out = Vec::new();
    for delivery in deliveries {
        if latch.get(&delivery.project_key) == Some(&delivery.text) {
            continue;
        }
        latch.insert(delivery.project_key.clone(), delivery.text.clone());
        out.push(delivery);
    }
    out
}
```

测试（放 `server.rs` 的 `mod tests`，与 `lead_tick_once` 的测试同处；若 `server.rs` 没有合适
的 `mod tests`，放 `rpc_tests.rs` 顶部，注意该文件用 `use super::*` 不可用——则写全路径
`crate::server::take_changed_digests`）：

```rust
    #[test]
    fn identical_digests_are_not_delivered_twice() {
        use crate::lead::LeadTarget;
        let mut latch = std::collections::HashMap::new();
        let mk = |text: &str| crate::server::LeadTickDelivery {
            project_key: "/p".to_string(),
            lead: LeadTarget {
                cwd: "/p".to_string(),
                thread_id: "lead-1".to_string(),
            },
            text: text.to_string(),
        };
        // 第一轮：投递。
        let first = take_changed_digests(vec![mk("卡住：thread-a")], &mut latch);
        assert_eq!(first.len(), 1);
        // 第二轮：同样的文本 → 不再投递（审批期的 10 次重复从此消失）。
        let second = take_changed_digests(vec![mk("卡住：thread-a")], &mut latch);
        assert!(second.is_empty());
        // 第三轮：内容变了（新 thread 卡住）→ 恢复投递。
        let third = take_changed_digests(vec![mk("卡住：thread-a, thread-b")], &mut latch);
        assert_eq!(third.len(), 1);
    }
```

> **实现者注意**：`LeadTickDelivery` 与 `LeadTarget` 的字段名请以**当前代码**为准
> （`LeadTickDelivery { project_key, lead, text }`；`LeadTarget { cwd, thread_id }`）。
> 若 `LeadTickDelivery` 可见性不足（`struct` 无 `pub(crate)`），在**同文件**内测试无需改动；
> 若测试放在 `rpc_tests.rs`（另一模块），把两个结构体改成 `pub(crate)`。

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib identical_digests_are_not_delivered_twice`
Expected: FAIL —— `take_changed_digests` 未定义。

- [ ] **Step 3: 实现最小可用代码**

1. 在 `server.rs` 紧邻 `lead_tick_once` 的定义处加上面的 `take_changed_digests` 函数。

2. 在 `let mut lead_ticker = ...`（`server.rs:2519` 附近）**之前**或紧邻处加闩锁局部量：

```rust
    // 上次**已投递**的 digest 文本（键 = project_key）。审批期的重复 tick 会给出
    // 一模一样的文本，靠它压掉（见 `take_changed_digests`）。
    let mut lead_digest_latch: HashMap<String, String> = HashMap::new();
```

3. 把 `lead_ticker.tick()` 分支里的投递循环改为先过滤：

```rust
            _ = lead_ticker.tick() => {
                // ...（既有注释保留）
                for delivery in take_changed_digests(lead_tick_once(&mut threads), &mut lead_digest_latch) {
                    let envelope = crate::lead::LeadEnvelope {
                        project_key: delivery.project_key,
                        from_thread_id: LEAD_LOOP_SENDER.to_string(),
                        text: delivery.text,
                        request_id: None,
                    };
                    // ...（既有 ServeLauncher 构造与 deliver_to_lead 调用逐字保留）
                }
            }
```

> **注意**：`take_changed_digests` 会**在投递前**把文本写进 latch。这与「投递失败要不要回滚」
> 无关——下一次文本相同就不再投，这正是我们要的（失败也已经花了那一轮的成本，
> 无限重试只会放大）。**不要**照抄 `lead_rx` 分支那套 request_id 回滚。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead`
Expected: PASS（含既有 `a_stuck_thread_wakes_the_lead_with_a_digest_naming_it` 与新测试）

- [ ] **Step 5: 文档**

在 `docs/project-management/yi-agent-app-server.md` 的 P2 条目末尾「本期已知限制」之前补一句：
「后台 tick 按 digest 文本去重（`server.rs` 的 `take_changed_digests`）：审批期内同一条
`AwaitingApproval` 不再每 30s 重复唤醒组长（P3-A 修）。」

- [ ] **Step 6: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs docs/project-management/yi-agent-app-server.md
git commit -m "fix(app-server): 后台 tick 按 digest 文本去重,审批期不再重复唤醒组长"
```

---

### Task 3: 新增 `lead_index.rs` 纯函数模块（列表 + 转录尾）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/lead_index.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（在 `pub mod lead_memory;` 之后加 `pub(crate) mod lead_index;`，保持字母序：`lead.rs` → `lead_index.rs` → `lead_loop.rs` → `lead_memory.rs`）

**Interfaces:**
- Consumes: `crate::thread_store::{ThreadStore, ThreadMeta, LEAD_ROLE}`；`crate::protocol::Item`。
- Produces（Task 4 依赖这些**精确**签名）：
  - `pub struct ThreadSummary { pub thread_id: String, pub title: Option<String>, pub updated_at: i64, pub item_count: usize, pub last_text: Option<String> }`
  - `pub struct ThreadTail { pub thread_id: String, pub total_items: usize, pub shown_items: Vec<String> }`
  - `pub fn list_summaries(project_root: &Path, limit: usize) -> std::io::Result<(Vec<ThreadSummary>, usize)>` —— `(窗口, 总数)`，总数含被 `limit` 截掉的部分，**不含**被跳过的组长自己
  - `pub fn read_tail(project_root: &Path, thread_id: &str, limit: usize) -> std::io::Result<Option<ThreadTail>>` —— `None` = 该 id 不属于本 store
  - `pub fn render_summaries(summaries: &[ThreadSummary], total: usize, project_root: &Path) -> String`
  - `pub fn render_tail(tail: &ThreadTail) -> String`
  - 常量：`pub const LIST_DEFAULT_LIMIT: usize = 50; pub const LIST_MAX_LIMIT: usize = 200; pub const READ_DEFAULT_ITEMS: usize = 20; pub const READ_MAX_ITEMS: usize = 50; pub const LIST_LAST_TEXT_MAX: usize = 200; pub const READ_ITEM_MAX: usize = 500; pub const READ_MAX_BYTES: usize = 32 * 1024;`

- [ ] **Step 1: 写失败的测试**

创建 `lead_index.rs`，先只写模块头 + 测试（实现留空/`unimplemented!()`）：

```rust
//! 组长的**只读视野**：从磁盘现算「名下有哪些 thread」与「某个 thread 最近说了什么」。
//!
//! 纯磁盘派（上级设计 §3.1）：**不读实时状态**。状态变化由后台 digest 推（P2 已有），
//! 本模块只负责 digest 不承载的东西——目录、交付结论、转录尾部。
//!
//! 全为纯函数：不起 app-server、不调 LLM、可单测（与 `lead_loop` 同一理由）。

use std::path::Path;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread_store::{ThreadMeta, ThreadMode, ThreadStore, LEAD_ROLE};

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
            board_project: None,
            card_id: None,
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
        store.create(&meta("lead-1", Some("组长"), 30, Some(LEAD_ROLE)))
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
        let tail = read_tail(dir.path(), "t-a", 2).expect("read").expect("some");
        assert_eq!(tail.total_items, 5);
        assert_eq!(tail.shown_items.len(), 2);
        // 取尾部：最近两条（第 3、4 条）。
        assert!(tail.shown_items[0].contains("第 3 条"), "{:?}", tail.shown_items);
        assert!(tail.shown_items[1].contains("第 4 条"), "{:?}", tail.shown_items);
    }

    #[test]
    fn read_tail_returns_none_for_a_thread_outside_this_store() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_tail(dir.path(), "not-here", 20).expect("read").is_none());
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
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_index`
Expected: FAIL —— `lead_index` 模块不存在（先在 `lib.rs` 加 `pub(crate) mod lead_index;`）/
`list_summaries` 未定义。

- [ ] **Step 3: 实现最小可用代码**

在 `lead_index.rs` 的 `mod tests` **之前**实现：

```rust
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

/// 读某 thread 的全部 item（复用既有落盘格式）。读不到按空处理。
fn read_items(project_root: &Path, thread_id: &str) -> Vec<crate::protocol::Item> {
    let store = crate::thread_store::ThreadStore::new(project_root);
    match store.load(thread_id) {
        Ok(Some(loaded)) => loaded.items,
        _ => Vec::new(),
    }
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
            let items = read_items(project_root, &m.thread_id);
            let last_text = items
                .iter()
                .rev()
                .find_map(|it| match it {
                    crate::protocol::Item::AgentMessage { text, .. } if !text.trim().is_empty() => {
                        Some(elide(text, LIST_LAST_TEXT_MAX))
                    }
                    _ => None,
                });
            ThreadSummary {
                thread_id: m.thread_id,
                title: m.title,
                updated_at: m.updated_at,
                item_count: items.len(),
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
    let store = crate::thread_store::ThreadStore::new(project_root);
    let loaded = match store.load(thread_id)? {
        Some(l) => l,
        None => return Ok(None),
    };
    let total_items = loaded.items.len();
    let keep_from = total_items.saturating_sub(limit);
    let window = &loaded.items[keep_from..];
    let mut shown_items: Vec<String> = window.iter().map(render_item).collect();
    // 整体字节上限：超了从**最旧**一端丢，并在头部标注（§6.4 注入预算）。
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
        total_items,
        shown_items,
    }))
}

/// 一条 item 的单行渲染（截断到 [`READ_ITEM_MAX`]）。
fn render_item(item: &crate::protocol::Item) -> String {
    use crate::protocol::Item;
    match item {
        Item::UserMessage { text, .. } => format!("用户：{}", elide(text, READ_ITEM_MAX)),
        Item::UserInterjection { text, .. } => format!("用户（中途）：{}", elide(text, READ_ITEM_MAX)),
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
```

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_index`
Expected: PASS（6 条测试）

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/lead_index.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): 新增 lead_index 纯函数模块(只读视野的列表与转录尾)"
```

---

### Task 4: 两个工具 `LeadListThreadsTool` / `LeadReadThreadTool` + 组长门控注册

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lead_index.rs`（追加两个工具）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`build_runtime_tooling` 的 `is_lead_thread` 分支内注册，现约 `:999`）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/lead_index.rs`（工具层测试）+ `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`（门控测试）

**Interfaces:**
- Consumes: Task 3 的 `list_summaries` / `read_tail` / `render_summaries` / `render_tail` / 常量；`crate::lead_memory::lead_memory_metadata()`（同款 `ToolMetadata` 工厂，`ToolSource::Plugin { name: "lead" }`）。
- Produces: `pub struct LeadListThreadsTool { project_root: PathBuf }`（`new(project_root: PathBuf)`）、`pub struct LeadReadThreadTool { project_root: PathBuf }`（`new(project_root: PathBuf)`）；两者 `impl Tool`，`name()` 分别为 `"lead_list_threads"` / `"lead_read_thread"`，`metadata().read_only == true`。

> **前置修正（本 Task 第一步）**：`lead_memory_metadata()` 当前是**私有** `fn`（`lead_memory.rs:110`），
> 跨模块调用会编译失败。Step 3 的第一件事是把它改成 `pub(crate) fn`。

- [ ] **Step 1: 写失败的测试**

在 `lead_index.rs` 的 `mod tests` 追加：

```rust
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
```

> **实现者注意**：`ContentBlock::Text` 的路径与 `ToolResult.content` 的可见性以**当前代码**为准
> （`yi_agent_core::{ContentBlock, ToolResult}`；`ToolResult.content` 是 `pub`）。若 `content`
> 不可直接迭代，改用既有测试里取文本的同一手法（参考 `lead.rs` / `lead_memory.rs` 的既有测试）。

门控测试加到 `rpc_tests.rs`（紧邻既有 `only_the_lead_sees_the_memory_tools`）：

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_lead_sees_the_view_tools() {
        // 非组长装配后，两个新工具都不得出现在注册表里（上级设计 §7.5）。
        // 复用既有 harness：起一个普通 thread，检查其工具集。
        // 实现者：照抄 `only_the_lead_sees_the_memory_tools` 的形状，把
        // `lead_remember`/`lead_recall` 换成 `lead_list_threads`/`lead_read_thread`。
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_index`
Expected: FAIL —— `LeadListThreadsTool` / `LeadReadThreadTool` 未定义。

- [ ] **Step 3: 实现最小可用代码**

**先**把 `lead_memory.rs:110` 的 `fn lead_memory_metadata()` 改成 `pub(crate) fn lead_memory_metadata()`
（跨模块复用同一份 `ToolSource::Plugin { name: "lead" }` 口径，不新造第二份）。

在 `lead_index.rs` 的 `mod tests` 之前追加：

```rust
use std::path::PathBuf;
use serde::Deserialize;
use serde_json::Value;
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
        let limit = parsed.limit.unwrap_or(LIST_DEFAULT_LIMIT).min(LIST_MAX_LIMIT);
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
        let limit = parsed.limit.unwrap_or(READ_DEFAULT_ITEMS).min(READ_MAX_ITEMS);
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
```

在 `server.rs` 的 `is_lead_thread` 分支内（`LeadRecallTool` 注册之后）追加两个注册：

```rust
        registry.register(Arc::new(crate::lead_index::LeadListThreadsTool::new(
            project_root.clone(),
        )));
        registry.register(Arc::new(crate::lead_index::LeadReadThreadTool::new(
            project_root,
        )));
```

> **注意**：`project_root` 在该分支里已被 `LeadRememberTool::new(project_root.clone())` 借用过；
> 最后一个 `LeadRecallTool::new(project_root)` **move** 了它。新代码必须把两个新工具的注册
> **插在** `LeadRecallTool` 注册**之前**（或把最后一个改成 `project_root.clone()`），否则借用检查失败。
> 实现者请按「先 remember、再 list、再 read、最后 recall」的顺序排。

- [ ] **Step 4: 运行确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead`
Expected: PASS（含 3 条工具测试 + 门控测试）

- [ ] **Step 5: 补充：把新工具文档化**

在 `docs/project-management/yi-agent-app-server.md` 新增一条 P3-A 条目（放 P2 条目之后）：

```
- [x] 项目组长 Agent（P3-A 视野工具）—— `lead_list_threads` + `lead_read_thread`（只读、
  纯磁盘派：不含实时状态，状态变化仍由后台 digest 推）：新增纯函数模块 `src/lead_index.rs`
  （`list_summaries` / `read_tail` / `render_summaries` / `render_tail` + 两个工具），
  只对组长注册（与 `lead_remember`/`lead_recall` 同门控，`is_lead_thread`，`src/server.rs:999` 一带）。
  预算：单条 item ≤500 字符、整体 ≤32 KiB、列表「最后一条结论」≤200 字符、列表默认 50/上限 200、
  读默认 20/上限 50。验证：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_index` —
  [设计](../superpowers/specs/2026-10-18-project-lead-agent-p3-view-design.md)、
  [计划](../superpowers/plans/2026-10-18-project-lead-agent-p3-view.md)
```

同步把两个索引表的 `yi-agent-app-server` 计数**各自 +1**。注意两个文件当前值**并不一致**，逐一按实际值改：

- 根 `README.md`（`README.md:266`）：当前 `44 / 44` → 改为 `45 / 45`
- `docs/project-management/README.md`（`docs/project-management/README.md:27`）：当前 `47 / 47` → 改为 `48 / 48`

- [ ] **Step 6: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/lead_index.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs docs/project-management/yi-agent-app-server.md README.md docs/project-management/README.md
git commit -m "feat(app-server): 组长视野工具 lead_list_threads / lead_read_thread"
```

---

### Task 5: 收口——全量验证与文档对齐

**Files:**
- Modify: `docs/superpowers/specs/2026-10-17-project-lead-agent-design.md`（把「P3 待实现」改成「P3-A 已实现；B/C 待实现」）
- Verify: 全部测试与 fmt

- [ ] **Step 1: 跑全量测试**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```
Expected: PASS（759 + 本次新增 ≈ 12 条）

- [ ] **Step 2: 跑 daemon 隔离不变量**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc -- \
  two_conversation_roots_do_not_share_children_or_reach_across \
  ending_one_conversation_reclaims_only_its_own_root
```
Expected: 2 passed（本设计**零** core / daemon 改动，不变量必须原样通过）

- [ ] **Step 3: fmt**

Run:
```bash
cd yi-agent-rs && cargo fmt --all -- --check
```
Expected: exit 0

- [ ] **Step 4: 更新上级 spec 状态行**

把 `docs/superpowers/specs/2026-10-17-project-lead-agent-design.md` 第 4 行的
「状态：P1/P2 已实现并合入，P3 待实现」改为
「状态：P1/P2 已实现并合入；P3-A（视野工具）已实现（见 2026-10-18-project-lead-agent-p3-view-design.md）；
P3-B（权限留痕/撤销，§8 前提待重定义）与 P3-C（移动端专用 UI）待实现」。

并在 §11 的 P3 小节补一句指向 P3-A 设计。

- [ ] **Step 5: 提交**

```bash
cd .. && git add docs/superpowers/specs/2026-10-17-project-lead-agent-design.md
git commit -m "docs: 标记 P3-A(组长视野工具)已实现,记录 B/C 待办"
```

---

## 自检记录（写计划时已核对）

**1. Spec 覆盖**

| Spec 章节 | 对应 Task |
|---|---|
| §3 纯磁盘派、不注入状态 | Task 3（模块只读磁盘）、Task 4（工具签名不含状态）、Global Constraints |
| §4 `lead_list_threads` | Task 3（`list_summaries`/`render_summaries`）+ Task 4 |
| §4.2 尾部扫描取最后结论 | Task 3（`read_items` + `.rev().find_map`） |
| §5 `lead_read_thread` | Task 3（`read_tail`/`render_tail`）+ Task 4 |
| §5.3 渲染口径与上限 | Task 3（`render_item` + `READ_ITEM_MAX` / `READ_MAX_BYTES`） |
| §6 注册与边界 | Task 4（`is_lead_thread` 分支）+ 门控测试 |
| §7.1 `lead_recall` 取最近 + 披露 | Task 1 |
| §7.2 tick digest 去重 | Task 2 |
| §8 测试与验证 | 各 Task 的 Step 4 + Task 5 |
| §9 明确不做 | 无 Task（刻意的：不做实时状态投影 / 不做 §8 权限留痕 / 不做移动端 UI） |

**2. 占位符扫描**：无 TBD/TODO。两处 `实现者注意` 是**可见性依赖提示**（要求实现者按当前代码
核对 `ContentBlock`/`ToolResult`/`LeadTickDelivery` 的字段与路径），不是「待补的代码」——
每段需要写的代码都已给出全文。

**3. 类型一致性**：`search_with_total` 在 Task 1 定义、Task 1 使用；`ThreadSummary`/`ThreadTail`/
`list_summaries`/`read_tail`/`render_summaries`/`render_tail` 在 Task 3 定义、Task 4 使用，
签名逐一核对一致；`LeadListThreadsTool`/`LeadReadThreadTool` 在 Task 3 的 Interfaces 声明、
Task 4 实现。`lead_memory_metadata()` 复用既有函数（Task 4 依赖它，不新造）。