# 会话内 subagent token 用量统计 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面端在会话运行过程中看到该会话所有 subagent 烧掉的 input / output / cache read 累计值，并与主会话分开呈现。

**Architecture:** subagent worker 跑在独立 daemon 进程，用量只能经 store 持久化后由 app-server 轮询取得。改造沿既有链路推进：worker 上报完整分项 → `WatchdogUsage` 记分项 → `ListTaskSummaries` 按 task 求和 → 复用已有 500ms children watch → `AgentChild.usage` → server 按 thread 汇总 → `TokenUsage.subagent_*` → 桌面端三处展示。

**Tech Stack:** Rust（yi-agent-core / yi-agent-store / yi-agent-subagent / yi-agent-app-server）、TypeScript + React + vitest（desktop）、ts-rs 生成绑定、rusqlite（JSON 文本列）。

## Global Constraints

- 工作目录：`.worktrees/subagent-usage`，分支 `feat/subagent-usage`。**禁止在 `main` 上改动。**
- 统计口径：subagent 只统计 **input / output / cache read** 三项累计。cache write 不展示。
- `WatchdogUsage.tokens` 语义保持不变（仍为 input + output 之和），不得改为四项之和。
- `subagent_*` 协议字段恒为 `u64`（TS `number`），**不是 Optional**；没有 subagent 时为三个 0。
- 老数据兼容：`usage_json` 缺新键时读出 0；老 daemon 不回 `usage` 时客户端视为 0。
- Rust 改动后必须 `cargo fmt --all`；改 `protocol.rs` 必须重新生成 ts-rs 绑定并一起提交。
- commit 用 conventional commits，**不写** `Co-Authored-By`。
- 展示文案中文，数字千分位。
- 不同时跑多个 `cargo test`；优先按 crate 跑。
- 路径前置：`export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`（cargo）、`export PATH="/opt/homebrew/bin:$PATH"`（node/npx）。

---

## File Structure

| 文件 | 责任 |
| --- | --- |
| `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs` | `WorkerWatchdogEvent::ProviderUsage` 携带四项分项；`report_provider_usage` 签名扩展 |
| `yi-agent-rs/crates/yi-agent-store/src/schedule.rs` | `WatchdogUsage` 增加三个分项字段 |
| `yi-agent-rs/crates/yi-agent-store/src/repository.rs` | `record_watchdog_progress` 累加分项；新增 `attempt_usages_for_tasks` 批量查询；`PersistedTaskSummary.usage` |
| `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` | 把四分项从事件搬到 repository 调用 |
| `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` | `IpcTaskSummary.usage`、`IpcTaskUsage` 结构 |
| `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` | 上报点传完整分项 |
| `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` | `AgentChildUsage`、`AgentChild.usage`、`Notification::TokenUsage.subagent_*` |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | `children_for_thread` 带上 usage；children 变化时汇总并推 `TokenUsage` |
| `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs` | `set_subagent_totals` / `reset_totals` 归零 subagent |
| `desktop/src/lib/protocol.ts` | `SubagentTotals`、`wireTotalsToTotals` 复用、`subagentTotalsFrom` |
| `desktop/src/lib/session.ts` | `subagentTotals` 字段 + 通知写入 + `reset` 归零 |
| `desktop/src/lib/usageCommand.ts` | `formatUsage` 增加 subagent 段 |
| `desktop/src/components/StatusBar.tsx` | 折叠态合计；展开面板传 subagent |
| `desktop/src/components/UsagePanel.tsx` | Session / Subagents 两组 |
| `docs/project-management/desktop.md` | 验收项 |

---

### Task 1: store 记账分项（WatchdogUsage）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/schedule.rs:170-177`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs:1256-1300`
- Test: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`（文件内 `mod tests`）

**Interfaces:**
- Consumes: 无（本任务起点）
- Produces:
  - `WatchdogUsage { turns: u32, tokens: u64, cost_micros: u64, provider_retries: u16, tool_retries: u16, rework_cycles: u16, input_tokens: u64, output_tokens: u64, cache_read_tokens: u64 }`
  - `Repository::record_watchdog_progress(&mut self, task: &TaskId, attempt: &AttemptId, turn_delta: u32, token_delta: u64, provider_retry_delta: u16, tool_retry_delta: u16, meaningful: bool, now: DateTime<Utc>, usage_delta: WatchdogTokenDelta) -> Result<(), RepositoryError>`
  - `WatchdogTokenDelta { input_tokens: u64, output_tokens: u64, cache_read_tokens: u64 }`（定义在 `schedule.rs`）

- [x] **Step 1: 写失败测试 —— 老 JSON 缺新字段时读出 0**

在 `crates/yi-agent-store/src/schedule.rs` 末尾的测试模块加：

```rust
#[test]
fn watchdog_usage_reads_legacy_json_without_token_breakdown() {
    // 老数据里没有这三个键；读出来必须是 0，而不是反序列化失败。
    let legacy = r#"{"turns":3,"tokens":120,"cost_micros":0,
                     "provider_retries":0,"tool_retries":0,"rework_cycles":0}"#;
    let usage: WatchdogUsage = serde_json::from_str(legacy).expect("legacy json must parse");
    assert_eq!(usage.tokens, 120);
    assert_eq!(usage.input_tokens, 0);
    assert_eq!(usage.output_tokens, 0);
    assert_eq!(usage.cache_read_tokens, 0);
}
```

- [x] **Step 2: 运行测试确认失败**

```bash
cd .worktrees/subagent-usage/yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo test -p yi-agent-store --lib watchdog_usage_reads_legacy
```
Expected: 编译失败（`WatchdogUsage` 没有 `input_tokens` 字段）。

- [x] **Step 3: 给 `WatchdogUsage` 加三个字段**

`crates/yi-agent-store/src/schedule.rs`：

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogUsage {
    pub turns: u32,
    pub tokens: u64,
    pub cost_micros: u64,
    pub provider_retries: u16,
    pub tool_retries: u16,
    pub rework_cycles: u16,
    /// 分项累计：会话级展示用。老 `usage_json` 没有这些键，按 0 读。
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
}

/// 一次 provider 调用的分项增量。
///
/// 与 `WatchdogUsage.tokens` 分开传：`tokens` 的语义是 input+output（监督阈值依赖
/// 它），不能因为多了 cache 分项而被改写。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WatchdogTokenDelta {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}
```

- [x] **Step 4: 运行测试确认通过**

```bash
cargo test -p yi-agent-store --lib watchdog_usage_reads_legacy
```
Expected: PASS。

- [x] **Step 5: 写失败测试 —— 分项累加且 tokens 语义不变**

在 `repository.rs` 的测试模块加（沿用该文件既有的临时库 + insert_task/insert_attempt 辅助函数，参照同文件内 `record_watchdog_progress` 的既有测试）：

```rust
#[test]
fn record_watchdog_progress_accumulates_token_breakdown() {
    let mut repo = test_repository();
    let (task, attempt) = sample_task_and_attempt(&mut repo);

    repo.record_watchdog_progress(
        &task,
        &attempt,
        1,
        50,
        0,
        0,
        false,
        Utc::now(),
        WatchdogTokenDelta { input_tokens: 40, output_tokens: 10, cache_read_tokens: 900 },
    )
    .unwrap();
    repo.record_watchdog_progress(
        &task,
        &attempt,
        1,
        30,
        0,
        0,
        false,
        Utc::now(),
        WatchdogTokenDelta { input_tokens: 25, output_tokens: 5, cache_read_tokens: 100 },
    )
    .unwrap();

    let usage = repo.watchdog_usage(&task, &attempt).unwrap();
    assert_eq!(usage.input_tokens, 65);
    assert_eq!(usage.output_tokens, 15);
    assert_eq!(usage.cache_read_tokens, 1000);
    // 语义锁死：tokens 仍只跟 token_delta 走（input+output），不含 cache read。
    assert_eq!(usage.tokens, 80);
}
```

- [x] **Step 6: 运行测试确认失败**

```bash
cargo test -p yi-agent-store --lib record_watchdog_progress_accumulates_token_breakdown
```
Expected: 编译失败（`record_watchdog_progress` 少一个参数）。

- [x] **Step 7: 扩展 `record_watchdog_progress`**

`repository.rs`，签名追加 `usage_delta: WatchdogTokenDelta`。函数体（现有
`repository.rs:1287-1290`）已经是：

```rust
        usage.turns = usage.turns.saturating_add(turn_delta);
        usage.tokens = usage.tokens.saturating_add(token_delta);
        usage.provider_retries = usage.provider_retries.saturating_add(provider_retry_delta);
        usage.tool_retries = usage.tool_retries.saturating_add(tool_retry_delta);
```

**只在 `usage.tokens` 那一行之后插入三行**，不要重抄上面已有的赋值（重抄会变成
"先加 turn_delta 再加一次"的双计）：

```rust
        usage.input_tokens = usage.input_tokens.saturating_add(usage_delta.input_tokens);
        usage.output_tokens = usage.output_tokens.saturating_add(usage_delta.output_tokens);
        usage.cache_read_tokens = usage
            .cache_read_tokens
            .saturating_add(usage_delta.cache_read_tokens);
```

`token_delta` 与 `usage_delta` 是**两个独立入参**：前者维持既有 `tokens` 语义
（input+output，供 watchdog 阈值判断），后者记分项。调用点必须两者都传对，不能
指望函数自己从分项推出 `tokens`。

顶部 `use crate::schedule::{... WatchdogTokenDelta}` 补齐导入。

若测试辅助函数 `watchdog_usage(&task, &attempt)` 不存在，在 `impl Repository` 里加：

```rust
    /// 读取某次 attempt 的用量快照。测试与上层聚合共用。
    pub fn watchdog_usage(
        &self,
        task: &TaskId,
        attempt: &AttemptId,
    ) -> Result<WatchdogUsage, RepositoryError> {
        let json: String = self.connection.query_row(
            "SELECT usage_json FROM attempt_watchdogs WHERE task_id = ?1 AND attempt_id = ?2",
            params![task.to_string(), attempt.to_string()],
            |row| row.get(0),
        )?;
        serde_json::from_str(&json).map_err(|error| RepositoryError::InvalidWatchdogSnapshot {
            attempt: attempt.to_string(),
            reason: format!("invalid usage JSON: {error}"),
        })
    }
```

- [x] **Step 8: 运行 store 全量测试**

```bash
cargo test -p yi-agent-store --lib
```
Expected: 全绿。既有调用点会因签名变化报错，按 Step 9 修。

- [x] **Step 9: 修所有调用点**

`grep -rn "record_watchdog_progress" crates/` 找出全部调用点。本阶段（Task 1）只改 `runtime.rs`，传全零分项，保持行为不变：

```rust
            repository.record_watchdog_progress(
                &task_id,
                &attempt,
                turn_delta,
                token_delta,
                provider_retry_delta,
                tool_retry_delta,
                meaningful,
                Utc::now(),
                WatchdogTokenDelta::default(),
            )?;
```

测试内的调用点同样补 `WatchdogTokenDelta::default()`。

- [x] **Step 10: 运行测试并提交**

```bash
cargo test -p yi-agent-store --lib
cargo fmt --all
git add crates/yi-agent-store
git commit -m "feat(store): 监督用量记账增加 token 分项"
```

---

### Task 2: worker 上报完整分项

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs:375-383, 460-467`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs:2955-2976`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:962-968`
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（`mod tests` 内既有 `daemon_worker_reports_provider_usage_to_the_watchdog`）

**Interfaces:**
- Consumes: `WatchdogTokenDelta`（Task 1）
- Produces:
  - `WorkerWatchdogEvent::ProviderUsage { input_tokens: u64, output_tokens: u64, cache_creation_tokens: u64, cache_read_tokens: u64 }`
  - `WorkerHandle::report_provider_usage(&self, input_tokens: u64, output_tokens: u64, cache_creation_tokens: u64, cache_read_tokens: u64)`

- [x] **Step 1: 更新既有测试的期望值（先写期望）**

`crates/yi-agent-subagent/src/lib.rs` 的 `daemon_worker_reports_provider_usage_to_the_watchdog`：

```rust
        assert_eq!(
            events,
            vec![WorkerWatchdogEvent::ProviderUsage {
                input_tokens: 11,
                output_tokens: 5,
                cache_creation_tokens: 3,
                cache_read_tokens: 7,
            }]
        );
```

同时确认测试用的 `UsageReportingProvider` 产生的 `TokenUsage` 里 `cache_creation_input_tokens: Some(3)`、`cache_read_input_tokens: Some(7)`；若原测试用 0，改成 3/7 以便断言分项被带上。

- [x] **Step 2: 运行测试确认失败**

```bash
cargo test -p yi-agent-subagent --lib daemon_worker_reports_provider_usage
```
Expected: 编译失败（枚举变体字段不匹配）。

- [x] **Step 3: 扩展枚举与上报方法**

`crates/yi-agent-core/src/subagent/worker.rs`：

```rust
pub enum WorkerWatchdogEvent {
    ProviderUsage {
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
    },
    ProviderRetry,
    ToolRetry,
    MeaningfulProgress,
}
```

```rust
    /// Records one completed provider turn. The runtime persists this as
    /// budget usage without treating generated model text as progress.
    pub fn report_provider_usage(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
    ) {
        self.report_watchdog(WorkerWatchdogEvent::ProviderUsage {
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
        });
    }
```

- [x] **Step 4: 更新 subagent 上报点**

`crates/yi-agent-subagent/src/lib.rs:962` 附近：

```rust
                                        Some(AgentEvent::Usage { usage, .. }) => {
                                            reporter.report_provider_usage(
                                                u64::from(usage.input_tokens),
                                                u64::from(usage.output_tokens),
                                                u64::from(
                                                    usage.cache_creation_input_tokens.unwrap_or(0),
                                                ),
                                                u64::from(
                                                    usage.cache_read_input_tokens.unwrap_or(0),
                                                ),
                                            );
                                        }
```

- [x] **Step 5: 更新 runtime 的搬运逻辑**

`crates/yi-agent-store/src/runtime.rs:2955-2976`（`for (task_id, update) in watchdog_updates` 里的 match），把五元组换成六元组：

```rust
            let (turn_delta, token_delta, provider_retry_delta, tool_retry_delta, meaningful, usage_delta) =
                match update {
                    WorkerWatchdogEvent::ProviderUsage {
                        input_tokens,
                        output_tokens,
                        cache_creation_tokens: _,
                        cache_read_tokens,
                    } => (
                        1,
                        input_tokens.saturating_add(output_tokens),
                        0,
                        0,
                        false,
                        WatchdogTokenDelta {
                            input_tokens,
                            output_tokens,
                            cache_read_tokens,
                        },
                    ),
                    WorkerWatchdogEvent::ProviderRetry => (0, 0, 1, 0, false, WatchdogTokenDelta::default()),
                    WorkerWatchdogEvent::ToolRetry => (0, 0, 0, 1, false, WatchdogTokenDelta::default()),
                    WorkerWatchdogEvent::MeaningfulProgress => (0, 0, 0, 0, true, WatchdogTokenDelta::default()),
                };
```

注意 `cache_creation_tokens` 本次**不落库**（用户明确不看 cache write），用 `_` 丢弃并在注释里写明原因：

```rust
                    // cache write 本次不统计（用户口径只关注 input/output/cache read）。
                    // 保留在事件里是为了将来记账不需要再动上报链路。
```

随后把 `usage_delta` 传给 `record_watchdog_progress`。

- [x] **Step 6: 运行测试**

```bash
cargo test -p yi-agent-subagent --lib daemon_worker_reports_provider_usage
cargo test -p yi-agent-store --lib
cargo test -p yi-agent-core --lib subagent
```
Expected: 全绿。

- [x] **Step 7: 提交**

```bash
cargo fmt --all
git add crates/yi-agent-core crates/yi-agent-store crates/yi-agent-subagent
git commit -m "feat(subagent): 上报 provider 用量的完整分项"
```

---

### Task 3: 按 task 聚合分项到 IPC 摘要

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs:329-339（PersistedTaskSummary）、3389-3430（task_summaries）`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:661-670（IpcTaskSummary）、3938-3965（handler）`
- Test: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`（`mod tests`）

**Interfaces:**
- Consumes: `WatchdogUsage` 分项（Task 1）
- Produces:
  - `TaskUsageTotals { input_tokens: u64, output_tokens: u64, cache_read_tokens: u64 }`（`repository.rs`，`Serialize + Deserialize + Default`）
  - `PersistedTaskSummary { task_id, state, is_root, parent_task_id, thread_id, usage: TaskUsageTotals }`
  - `IpcTaskUsage { input_tokens: u64, output_tokens: u64, cache_read_tokens: u64 }`（`ipc.rs`，`#[serde(default)]` 用在字段上）
  - `IpcTaskSummary { ..., usage: IpcTaskUsage }`

- [x] **Step 1: 写失败测试 —— 同一 task 多次 attempt 求和**

```rust
#[test]
fn task_summaries_sum_usage_across_attempts() {
    let mut repo = test_repository();
    let (task, first_attempt) = sample_task_and_attempt(&mut repo);
    // 同一 task 的第二次 attempt（重试）。
    let second_attempt = repo
        .insert_attempt(&task, 2, &Default::default(), &WatchdogUsage::default())
        .unwrap();

    repo.record_watchdog_progress(
        &task, &first_attempt, 1, 0, 0, 0, false, Utc::now(),
        WatchdogTokenDelta { input_tokens: 100, output_tokens: 10, cache_read_tokens: 1000 },
    ).unwrap();
    repo.record_watchdog_progress(
        &task, &second_attempt, 1, 0, 0, 0, false, Utc::now(),
        WatchdogTokenDelta { input_tokens: 50, output_tokens: 5, cache_read_tokens: 500 },
    ).unwrap();

    let summaries = repo.task_summaries(None, false).unwrap();
    let row = summaries.iter().find(|s| s.task_id == task.to_string()).unwrap();
    // 两次 attempt 的真实消耗都要算：是求和，不是取最后一次。
    assert_eq!(row.usage.input_tokens, 150);
    assert_eq!(row.usage.output_tokens, 15);
    assert_eq!(row.usage.cache_read_tokens, 1500);
}

#[test]
fn task_summaries_report_zero_usage_without_attempts() {
    let mut repo = test_repository();
    let (task, _attempt) = sample_task_and_attempt(&mut repo);
    let summaries = repo.task_summaries(None, false).unwrap();
    let row = summaries.iter().find(|s| s.task_id == task.to_string()).unwrap();
    assert_eq!(row.usage, TaskUsageTotals::default());
}
```

注：`insert_attempt` 的实际签名以 `repository.rs` 现有实现为准；若形状不同，按现有签名构造第二次 attempt。

- [x] **Step 2: 运行测试确认失败**

```bash
cargo test -p yi-agent-store --lib task_summaries_sum_usage_across_attempts
```
Expected: 编译失败（`PersistedTaskSummary` 没有 `usage`）。

- [x] **Step 3: 加类型与聚合**

`repository.rs`：

```rust
/// 一个 task 的累计 token 用量（跨它的全部 attempt 求和）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskUsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}
```

`PersistedTaskSummary` 增加 `pub usage: TaskUsageTotals`。

`task_summaries` 在收集完 rows 之后追加聚合（保持原 SQL 不变）：

```rust
        let mut summaries = statement
            .query_map(rusqlite::params_from_iter(parameters), |row| {
                Ok(PersistedTaskSummary {
                    task_id: row.get(0)?,
                    state: row.get(1)?,
                    is_root: row.get(2)?,
                    parent_task_id: row.get(3)?,
                    thread_id: row.get(4)?,
                    usage: TaskUsageTotals::default(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::Sql)?;
        self.attach_task_usages(&mut summaries)?;
        Ok(summaries)
```

新增私有方法（一次批量查询，避免 N+1）：

```rust
    /// 把每个 task 的分项用量挂到摘要上。
    ///
    /// 分项存在 `usage_json` 里，SQL 无法直接 `SUM`，因此在 Rust 侧解析。
    /// 一个 task 可能有多条 attempt（重试），这里按 task 求和——那是真实消耗。
    fn attach_task_usages(
        &self,
        summaries: &mut [PersistedTaskSummary],
    ) -> Result<(), RepositoryError> {
        if summaries.is_empty() {
            return Ok(());
        }
        // `IN` 占位符按实际条数拼，参数仍走绑定，不做字符串插值。
        let placeholders = std::iter::repeat("?")
            .take(summaries.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT task_id, usage_json FROM attempt_watchdogs WHERE task_id IN ({placeholders})"
        );
        let params = summaries.iter().map(|s| s.task_id.clone()).collect::<Vec<_>>();
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(params), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::Sql)?;

        let mut totals: std::collections::HashMap<String, TaskUsageTotals> =
            std::collections::HashMap::new();
        for (task_id, usage_json) in rows {
            // 单条坏 JSON 不该让整个列表失败：跳过它，其余照常累加。
            let Ok(usage) = serde_json::from_str::<WatchdogUsage>(&usage_json) else {
                continue;
            };
            let entry = totals.entry(task_id).or_default();
            entry.input_tokens = entry.input_tokens.saturating_add(usage.input_tokens);
            entry.output_tokens = entry.output_tokens.saturating_add(usage.output_tokens);
            entry.cache_read_tokens =
                entry.cache_read_tokens.saturating_add(usage.cache_read_tokens);
        }
        for summary in summaries.iter_mut() {
            summary.usage = totals.get(&summary.task_id).copied().unwrap_or_default();
        }
        Ok(())
    }
```

- [x] **Step 4: 运行测试确认通过**

```bash
cargo test -p yi-agent-store --lib task_summaries
```
Expected: PASS（两条新测试 + 既有 task_summaries 测试）。

- [x] **Step 5: 写失败测试 —— IPC 序列化带 usage（老 daemon 缺字段为 0）**

`ipc.rs` 测试模块：

```rust
#[test]
fn ipc_task_summary_tolerates_missing_usage() {
    // 老 daemon 不回 usage：客户端必须按 0 处理，而不是整个摘要解析失败。
    let legacy = r#"{"task_id":"t1","state":"running","is_root":false}"#;
    let summary: IpcTaskSummary = serde_json::from_str(legacy).expect("legacy must parse");
    assert_eq!(summary.usage, IpcTaskUsage::default());
}
```

- [x] **Step 6: 运行确认失败**

```bash
cargo test -p yi-agent-store --lib ipc_task_summary_tolerates_missing_usage
```
Expected: 编译失败（`IpcTaskUsage` 不存在）。

- [x] **Step 7: 加 IPC 结构与映射**

`ipc.rs`：

```rust
/// 一个 task 的累计 token 用量（跨它的全部 attempt）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTaskUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}
```

`IpcTaskSummary` 增加：

```rust
    /// 该 task 的累计用量。老 daemon 不回传时为零。
    #[serde(default)]
    pub usage: IpcTaskUsage,
```

handler 里映射（`ListTaskSummaries` 分支）：

```rust
                .map(|task| IpcTaskSummary {
                    task_id: task.task_id,
                    state: task.state,
                    is_root: task.is_root,
                    parent_task_id: task.parent_task_id,
                    thread_id: task.thread_id,
                    usage: IpcTaskUsage {
                        input_tokens: task.usage.input_tokens,
                        output_tokens: task.usage.output_tokens,
                        cache_read_tokens: task.usage.cache_read_tokens,
                    },
                })
```

- [x] **Step 8: 运行 IPC 全量测试并提交**

```bash
cargo test -p yi-agent-store --lib ipc
cargo test -p yi-agent-store --lib
cargo fmt --all
git add crates/yi-agent-store
git commit -m "feat(store): 任务摘要携带各 task 的分项用量"
```

---

### Task 4: 协议层暴露 subagent 用量

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:629-641（AgentChild）、455-470（TokenUsage）`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:967-987（children_for_thread）`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`mod tests`）
- Regenerate: `desktop/src/lib/generated/AgentChild.ts` 等

**Interfaces:**
- Consumes: `IpcTaskUsage`（Task 3）
- Produces:
  - `AgentChildUsage { input_tokens: u64, output_tokens: u64, cache_read_tokens: u64 }`
  - `AgentChild { task_id, objective, state, last_step, parent_task_id, usage: AgentChildUsage }`
  - `Notification::TokenUsage { ..., subagent_input_tokens: u64, subagent_output_tokens: u64, subagent_cache_read_tokens: u64 }`
  - `pub(crate) fn subagent_totals(children: &[AgentChild]) -> SubagentTotals`，`SubagentTotals { input: u64, output: u64, cache_read: u64 }`

- [x] **Step 1: 写失败测试 —— children 携带用量**

`server.rs` 测试模块：

```rust
#[test]
fn children_for_thread_carries_task_usage() {
    let tasks = vec![
        yi_agent_store::ipc::IpcTaskSummary {
            task_id: "child-1".into(),
            state: "running".into(),
            is_root: false,
            parent_task_id: None,
            thread_id: Some("t1".into()),
            usage: yi_agent_store::ipc::IpcTaskUsage {
                input_tokens: 120,
                output_tokens: 30,
                cache_read_tokens: 900,
            },
        },
    ];
    let children = children_for_thread(&tasks, "t1");
    assert_eq!(children.len(), 1);
    assert_eq!(
        children[0].usage,
        crate::protocol::AgentChildUsage {
            input_tokens: 120,
            output_tokens: 30,
            cache_read_tokens: 900,
        }
    );
}

#[test]
fn subagent_totals_sums_every_child() {
    let children = vec![
        crate::protocol::AgentChild {
            task_id: "a".into(),
            objective: None,
            state: "running".into(),
            last_step: None,
            parent_task_id: None,
            usage: crate::protocol::AgentChildUsage {
                input_tokens: 10, output_tokens: 1, cache_read_tokens: 100,
            },
        },
        crate::protocol::AgentChild {
            task_id: "b".into(),
            objective: None,
            state: "running".into(),
            last_step: None,
            parent_task_id: None,
            usage: crate::protocol::AgentChildUsage {
                input_tokens: 20, output_tokens: 2, cache_read_tokens: 200,
            },
        },
    ];
    assert_eq!(
        subagent_totals(&children),
        SubagentTotals { input: 30, output: 3, cache_read: 300 }
    );
}
```

- [x] **Step 2: 运行确认失败**

```bash
cargo test -p yi-agent-app-server --lib children_for_thread_carries_task_usage
```
Expected: 编译失败（`AgentChildUsage` 不存在）。

- [x] **Step 3: 加协议结构**

`protocol.rs`：

```rust
/// 一个子任务的累计 token 用量（跨它的全部 attempt）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase")]
pub struct AgentChildUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}
```

`AgentChild` 增加：

```rust
    /// 该子任务的累计用量。老服务端不回传时为零。
    #[serde(default)]
    pub usage: AgentChildUsage,
```

`Notification::TokenUsage` 增加三个字段（放在 `total_*` 之后）：

```rust
        /// 本会话所有 subagent 的累计，**不并入** `total_*`。
        #[ts(type = "number")]
        subagent_input_tokens: u64,
        #[ts(type = "number")]
        subagent_output_tokens: u64,
        #[ts(type = "number")]
        subagent_cache_read_tokens: u64,
```

- [x] **Step 4: 实现 `children_for_thread` 映射与汇总**

`server.rs`：

```rust
        .map(|task| crate::protocol::AgentChild {
            task_id: task.task_id.clone(),
            objective: None,
            state: task.state.clone(),
            last_step: None,
            parent_task_id: task.parent_task_id.clone(),
            usage: crate::protocol::AgentChildUsage {
                input_tokens: task.usage.input_tokens,
                output_tokens: task.usage.output_tokens,
                cache_read_tokens: task.usage.cache_read_tokens,
            },
        })
```

新版汇总函数。`SubagentTotals` 定义在 `protocol.rs`（与 `AgentChildUsage` 同处，
因为 `session.rs` 与 `translate.rs` 都要用它，放在 `server.rs` 会造成反向依赖）：

```rust
/// 一个会话里所有子任务的用量之和。
///
/// 与 [`AgentChildUsage`] 同形状但用途不同：后者是"一个子任务"的量，本类型是
/// "整个会话"的合计。不直接复用前者，是因为合计值还要参与 `/clear` 基线相减。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SubagentTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
}

impl SubagentTotals {
    /// 逐项饱和相减，供 `/clear` 基线扣减用。见 Task 5 Step 9。
    pub(crate) fn saturating_sub(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_sub(other.input),
            output: self.output.saturating_sub(other.output),
            cache_read: self.cache_read.saturating_sub(other.cache_read),
        }
    }
}

/// 子任务用量求和。
///
/// 对**整份列表**求和而不是只算运行中的：已完成的子任务烧掉的 token 同样是这个
/// 会话的真实开销，累计值必须包含它们。
pub(crate) fn subagent_totals(children: &[crate::protocol::AgentChild]) -> SubagentTotals {
    children.iter().fold(SubagentTotals::default(), |mut acc, child| {
        acc.input = acc.input.saturating_add(child.usage.input_tokens);
        acc.output = acc.output.saturating_add(child.usage.output_tokens);
        acc.cache_read = acc.cache_read.saturating_add(child.usage.cache_read_tokens);
        acc
    })
}
```

- [x] **Step 5: 运行测试**

```bash
cargo test -p yi-agent-app-server --lib children_for_thread
cargo test -p yi-agent-app-server --lib subagent_totals
```
Expected: PASS。

- [x] **Step 6: 修既有构造点**

`grep -rn "AgentChild {" crates/yi-agent-app-server/src/` 与测试，全部补 `usage: AgentChildUsage::default()`（测试里需要具体值的按 Step 1 写）。

`Notification::TokenUsage` 的构造点（`translate.rs` 的 `usage_notification`、`reset_totals`）暂时补三个 0，Task 5 再接上真实值。

- [x] **Step 7: 重新生成 ts-rs 绑定**

```bash
cd .worktrees/subagent-usage/yi-agent-rs
cargo test --all-features -p yi-agent-app-server export_bindings
cd .. && git status --porcelain desktop/src/lib/generated
```
Expected: `AgentChild.ts` / `AgentChildUsage.ts` / `Notification*.ts` 有变化。

- [x] **Step 8: 提交（含生成物）**

```bash
cd .worktrees/subagent-usage/yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates desktop/src/lib/generated
git commit -m "feat(app-server): 协议暴露 subagent 用量"
```

---

### Task 5: translator 持有 subagent 累计，driver 按推送更新

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs:110-200`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:832-866`（`watch_children`）、`:4929-4960`（`run_thread_driver` 签名）、`:5680-5750`（`/clear` 分支）、`:6796`（driver spawn）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs:124-150`（`ThreadSession`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/agent.rs:57-70`（children watch 建立）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs:324-410`（`AppState`，若需要转交 sender）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs`（`mod tests`）

**Interfaces:**
- Consumes: `SubagentTotals`（Task 4）、`AgentChild.usage`（Task 4）
- Produces:
  - `Translator::set_subagent_totals(&mut self, totals: SubagentTotals) -> Notification`
  - `Translator::reset_totals()` 返回值中 subagent 归零
  - `ThreadSession.subagent_tx: tokio::sync::watch::Sender<SubagentTotals>`

**接线事实（已核实，勿按旧假设实现）：**

1. `watch_children(hub, thread_id, socket, initial)` **没有** channel 参数，它是从
   `rpc/agent.rs:57` 在第一次 `agent/children/list` 时 `tokio::spawn` 出去的独立任务，
   与 driver 之间没有任何现成通道。driver（`run_thread_driver`）也**没有** store
   socket 参数，不能自己去查 daemon。
2. `watch_children` 已经在每 500ms 轮询 `ListTaskSummaries`，而 `AgentChild` 一旦带上
   `usage`，**用量变化本身就会让 `current != last`**。所以复用它是零额外 IPC 的，
   不要新开轮询。
3. `ThreadSession` 在 `thread/resume` 时会被重建（`rpc/thread.rs:569`），而 driver 也在
   同一处重新 spawn。因此 sender 必须与 driver **在同一处创建**，不能跨 resume 复用。
4. `watch_children` 只在 `agent/children/list` 时才懒创建；桌面端首屏一定会调它
   （`useSessionState.ts:185`），但**不要依赖**这一点来做正确性——没有 watch 时
   subagent 累计就是恒零，展示上等价于"没有 subagent"，可接受。
5. `/clear` 只截断会话日志（`thread_store.rs:862`），**不会删掉 daemon 里的 task 记录**。
   所以 clear 之后 children 列表仍带着旧 usage，若不处理，下一次 children 推送会把
   旧数字带回来。

因为事实 1，**不能**让 `watch_children` 自己发 `TokenUsage`：它拿不到 `translator` 的
`total_*`，单独发一条会把客户端已正确的主会话累计覆盖成零。正确做法是把汇总值送回
driver，由 driver 用自己那份 translator 生成完整通知。

通道用 `tokio::sync::watch` 而非 `mpsc`：它只保留最新值，`watch_children` 用
`send_replace` 永不阻塞、也不积压（500ms 一份，mpsc 满队列会让 watch 任务卡住）。

- [x] **Step 1: 写失败测试 —— subagent 累计不进 total_***

`translate.rs` 的 `mod tests`：

```rust
#[test]
fn subagent_totals_are_reported_separately_from_session_totals() {
    let mut translator = Translator::new("t1".into(), UsageTotals::default());
    let notification = translator.set_subagent_totals(SubagentTotals {
        input: 500,
        output: 60,
        cache_read: 4000,
    });
    match notification {
        Notification::TokenUsage {
            total_input_tokens,
            subagent_input_tokens,
            subagent_output_tokens,
            subagent_cache_read_tokens,
            ..
        } => {
            // 主会话累计不受影响：两组数必须分开。
            assert_eq!(total_input_tokens, 0);
            assert_eq!(subagent_input_tokens, 500);
            assert_eq!(subagent_output_tokens, 60);
            assert_eq!(subagent_cache_read_tokens, 4000);
        }
        other => panic!("expected TokenUsage, got {other:?}"),
    }
}

#[test]
fn reset_totals_clears_subagent_totals() {
    let mut translator = Translator::new("t1".into(), UsageTotals::default());
    translator.set_subagent_totals(SubagentTotals { input: 9, output: 1, cache_read: 0 });
    let notifications = translator.reset_totals();
    let last = notifications.last().expect("reset must emit a notification");
    match last {
        Notification::TokenUsage { subagent_input_tokens, .. } => {
            assert_eq!(*subagent_input_tokens, 0);
        }
        other => panic!("expected TokenUsage, got {other:?}"),
    }
}
```

- [x] **Step 2: 运行确认失败**

```bash
cargo test -p yi-agent-app-server --lib subagent_totals_are_reported_separately
```
Expected: 编译失败（`set_subagent_totals` / `SubagentTotals` 不存在）。

- [x] **Step 3: 加字段与方法**

`translate.rs`：

```rust
    /// 本会话所有 subagent 的累计。与 `committed`/`turn`/`usage` 平行但独立：
    /// 它不来自本 thread 的 provider 流，而是从子任务列表汇总来的。
    subagent: SubagentTotals,
```

`Translator::new` 里初始化 `subagent: SubagentTotals::default()`。

```rust
    /// 更新 subagent 累计并回一条通知。
    ///
    /// 只改 `subagent`，不动 `committed`/`turn`/`usage`——主会话与子 agent 是
    /// 两组独立的数，合并会让 `/usage` 无法分开呈现。
    pub fn set_subagent_totals(&mut self, totals: SubagentTotals) -> Notification {
        self.subagent = totals;
        self.usage_notification()
    }
```

`usage_notification()` 补三个字段：

```rust
            subagent_input_tokens: self.subagent.input,
            subagent_output_tokens: self.subagent.output,
            subagent_cache_read_tokens: self.subagent.cache_read,
```

`reset_totals()` 里加 `self.subagent = SubagentTotals::default();`。

- [x] **Step 4: 运行测试**

```bash
cargo test -p yi-agent-app-server --lib subagent_totals
cargo test -p yi-agent-app-server --lib reset_totals
```
Expected: PASS。

- [x] **Step 5: `watch_children` 增加 sender 参数并推送汇总**

`server.rs` 的签名改为（`initial` 之后加一个参数）：

```rust
pub(crate) async fn watch_children(
    hub: Arc<crate::broadcast::Broadcaster>,
    thread_id: String,
    socket: PathBuf,
    initial: Vec<crate::protocol::AgentChild>,
    subagent_tx: tokio::sync::watch::Sender<SubagentTotals>,
) {
```

循环体内，在推送 children 列表**之后**加：

```rust
        // 用量与列表同源（都来自这次 ListTaskSummaries），故同一次推送里一起更新，
        // 客户端不会看到"列表已变、数字未变"的中间态。
        let totals = subagent_totals(&current);
        // `send_replace` 永不失败、也不阻塞：没有 receiver（driver 已退出）时
        // 值仍被写进共享槽，下一个 driver 起来能立刻读到最新值。
        subagent_tx.send_replace(totals);
```

注意：`current == last` 时函数 `continue` 了，所以只有列表/用量真的变了才推送，
不会每 500ms 都发一条通知。

- [x] **Step 6: `ThreadSession` 持 sender**

`session.rs` 的 `ThreadSession` 加：

```rust
    /// 该会话 subagent 用量的最新汇总。`watch_children` 写，driver 读。
    ///
    /// 用 `watch` 而非 `mpsc`：只关心最新值，且写端在独立 task 里，
    /// `send_replace` 既不阻塞也不积压。
    pub(crate) subagent_tx: tokio::sync::watch::Sender<SubagentTotals>,
```

`SubagentTotals` 定义在 `protocol.rs`（Task 4），需在 `session.rs` 顶部
`use crate::protocol::SubagentTotals;`。

- [x] **Step 7: 三处构造点同步（编译驱动）**

有三处构造 `ThreadSession`：`server.rs:6757`（`start_thread_core`）、
`rpc/thread.rs:569`（`thread/resume`）、`rpc_tests.rs:5971` 与 `:9986`（测试）。

正确做法是**在构造 driver 的同一作用域里先建 channel**，再分别交给
`ThreadSession` 和 `run_thread_driver`：

```rust
    let (subagent_tx, subagent_rx) = tokio::sync::watch::channel(SubagentTotals::default());
```

`ThreadSession { ..., subagent_tx: subagent_tx.clone() }`，`run_thread_driver`
多收一个 `subagent_rx`。

`rpc/agent.rs:57` 建立 watch 时从 session 取 sender：

```rust
                    let subagent_tx = state
                        .threads
                        .get(&thread_id)
                        .map(|s| s.subagent_tx.clone())
                        .unwrap_or_else(|| {
                            tokio::sync::watch::channel(SubagentTotals::default()).0
                        });
                    let task = tokio::spawn(watch_children(
                        Arc::clone(state.hub),
                        thread_id.clone(),
                        socket,
                        children.clone(),
                        subagent_tx,
                    ));
```

测试里的两处构造点用 `tokio::sync::watch::channel(SubagentTotals::default()).0`。

- [x] **Step 8: driver 消费并推送**

`run_thread_driver` 签名加 `mut subagent_rx: tokio::sync::watch::Receiver<SubagentTotals>`。
`run_thread_driver` 与 `SubagentTotals` 分处 `server.rs` / `protocol.rs`，按既有风格在
`server.rs` 顶部 `use` 里补 `crate::protocol::SubagentTotals`。

在**内层** `select!` 里加一个分支（与 `delta_tick` / `checkpoint_tick` 同级）：

```rust
                // subagent 用量变化：`watch_children` 已把它算好送过来。
                // 用 `changed()` 而非每 tick 轮询：没变化时这个 future 一直 pending，
                // 不产生空唤醒。收到后立刻推一条完整的 TokenUsage（主会话累计由
                // 本 driver 的 translator 提供，故 total_* 保持正确）。
                Ok(()) = subagent_rx.changed() => {
                    let totals = *subagent_rx.borrow_and_update();
                    let n = translator.set_subagent_totals(totals);
                    if write_notification(&hub, &n).await.is_err() {
                        // 写帧失败即本轮放弃：与其它写失败站点同款。
                        let _ = turn_tx.send(finished_event(&thread_id, &turn_id, None, None)).await;
                        if let Err(e) = store.clear_partial(&thread_id) {
                            eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
                        }
                        return;
                    }
                }
```

注意 `Ok(()) = ...` 这个模式：`changed()` 返回 `Result<(), RecvError>`，sender 全部
drop 时它是 `Err`，模式不匹配会让该分支**永久禁用**（`select!` 的既有语义），正是
想要的——不要写成会导致 panic 的 `unwrap`。

同样的分支也要加进**外层** `select!`（空闲路径），否则空闲时收到变化不会推送：

```rust
            Ok(()) = subagent_rx.changed() => {
                let totals = *subagent_rx.borrow_and_update();
                let n = translator.set_subagent_totals(totals);
                let _ = write_notification(&hub, &n).await;
                continue;
            }
```

- [x] **Step 9: `/clear` 后把基线钉住**

`/clear` 只截断会话日志，daemon 的 task 记录还在，children 列表照样带着旧 usage。
若不处理，clear 后下一次推送会把旧数字带回来（`translator.reset_totals()` 归零了，
但 `watch_children` 送来的还是"清零前"的累计）。

做法：driver 维护一个基线，`/clear` 时把**当前收到的** subagent 汇总记为基线，
此后上报的都是"当前总量 − 基线"：

```rust
    // `/clear` 之前已经烧掉的 subagent 用量。daemon 的 task 记录不会随 clear 删除，
    // 故必须靠基线把旧量扣掉，否则清零后旧数字会被下一次推送带回来。
    let mut subagent_baseline = SubagentTotals::default();
    // 最近一次从 watch 收到的原始汇总（未减基线）。
    let mut subagent_raw = SubagentTotals::default();
```

两个 select 分支里改成：

```rust
                    subagent_raw = *subagent_rx.borrow_and_update();
                    let totals = subagent_raw.saturating_sub(subagent_baseline);
                    let n = translator.set_subagent_totals(totals);
```

`/clear` 分支（既有 `if clearing { for n in translator.reset_totals() ... }`）里补：

```rust
                if clearing {
                    // 顺序要紧：先把当前原始汇总钉成基线，再归零 translator。
                    subagent_baseline = subagent_raw;
                    for n in translator.reset_totals() {
                        let _ = write_notification(&hub, &n).await;
                    }
                }
```

`SubagentTotals::saturating_sub` 已在 Task 4 定义（逐项饱和相减：基线来自同一条数据
流，理论上不会超过当前值，但 daemon 重启/任务被清理都可能让它短暂倒挂——此时报 0
好过 panic 或回绕成天文数字）。

这个基线只活在 driver 内，`/clear` 之外的一切都不动它——`/compact`、切换模型都不该
重置 subagent 累计。

- [x] **Step 10: 补基线语义的单测**

放在 `server.rs` 的 `mod tests`（纯函数层，不必拉起整个 driver）：

```rust
#[test]
fn subagent_baseline_subtracts_cleared_usage() {
    let raw = SubagentTotals { input: 900, output: 90, cache_read: 8000 };
    let baseline = SubagentTotals { input: 400, output: 40, cache_read: 3000 };
    assert_eq!(
        raw.saturating_sub(baseline),
        SubagentTotals { input: 500, output: 50, cache_read: 5000 }
    );
}

#[test]
fn subagent_baseline_never_underflows() {
    // daemon 重启 / 任务被清理都可能让当前值短暂小于基线：报 0，不许回绕。
    let raw = SubagentTotals { input: 10, output: 1, cache_read: 5 };
    let baseline = SubagentTotals { input: 900, output: 90, cache_read: 8000 };
    assert_eq!(raw.saturating_sub(baseline), SubagentTotals::default());
}
```

- [x] **Step 11: 运行 app-server 全量并提交**

```bash
cargo test -p yi-agent-app-server --lib
cargo fmt --all
cd .. && git add yi-agent-rs/crates && git commit -m "feat(app-server): 会话累计携带 subagent 用量"
```

Expected: PASS。`children_for_thread` 的既有测试（`rpc_tests.rs:2704` 起）会因
`AgentChild` 新增字段需要补 `usage: Default::default()`，一并修掉。

---

### Task 6: 桌面端状态与协议映射

**Files:**
- Modify: `desktop/src/lib/protocol.ts`
- Modify: `desktop/src/lib/session.ts:114, 264, 388, 690`
- Test: `desktop/src/lib/session.test.ts`

**Interfaces:**
- Consumes: `Notification::TokenUsage` 的 `subagent_*`（Task 4 生成物）
- Produces:
  - `SubagentTotals { input: number; output: number; cacheRead: number }`
  - `Session.subagentTotals: SubagentTotals`（**初始为全零，不为 null**）
  - `Session.subagentCount: number`（该会话子任务数，供读屏句子用）

- [x] **Step 1: 写失败测试**

`session.test.ts`：

```typescript
it("keeps subagent totals separate from the session totals", () => {
  const s = new Session();
  s.apply({
    method: "thread/tokenUsage/updated",
    params: {
      thread_id: "t",
      model: "m",
      input_tokens: 10,
      output_tokens: 2,
      cache_creation_input_tokens: 0,
      cache_read_input_tokens: 5,
      total_input_tokens: 100,
      total_output_tokens: 20,
      total_cache_creation_input_tokens: 0,
      total_cache_read_input_tokens: 50,
      subagent_input_tokens: 900,
      subagent_output_tokens: 90,
      subagent_cache_read_tokens: 8000,
    },
  });
  // 两组数分开存：主会话 100/20/50，子 agent 900/90/8000。
  expect(s.usageTotals).toEqual({ input: 100, output: 20, cacheRead: 50, cacheWrite: 0 });
  expect(s.subagentTotals).toEqual({ input: 900, output: 90, cacheRead: 8000 });
});

it("defaults subagent totals to zero when the server omits them", () => {
  const s = new Session();
  expect(s.subagentTotals).toEqual({ input: 0, output: 0, cacheRead: 0 });
});
```

- [x] **Step 2: 运行确认失败**

```bash
export PATH="/opt/homebrew/bin:$PATH"
cd .worktrees/subagent-usage/desktop
npx vitest run src/lib/session.test.ts
```
Expected: FAIL（`subagentTotals` undefined）。

- [x] **Step 3: 加类型与字段**

`protocol.ts`：

```typescript
/** 会话内所有 subagent 的累计用量。恒为数字，无子 agent 时全零。 */
export interface SubagentTotals {
  input: number;
  output: number;
  cacheRead: number;
}
```

`session.ts`：

```typescript
  /**
   * 本会话所有 subagent 的累计。
   *
   * 与 `usageTotals` 平行但独立：主会话与子 agent 是两组数，界面要分开呈现。
   * 恒为数字（不是 null）：没有子 agent 时全零，客户端用 `> 0` 判断是否展示。
   */
  subagentTotals: SubagentTotals = { input: 0, output: 0, cacheRead: 0 };
```

`reset()` 里重置为全零。

`thread/tokenUsage/updated` 分支（约 690 行 `usageTotals` 赋值处）之后加：

```typescript
        // 老服务端不发这三个字段：保持全零而不是 undefined。
        if (typeof notification.params.subagent_input_tokens === "number") {
          this.subagentTotals = {
            input: notification.params.subagent_input_tokens,
            output: notification.params.subagent_output_tokens,
            cacheRead: notification.params.subagent_cache_read_tokens,
          };
        }
```

- [x] **Step 4: 跑测试与类型检查**

```bash
npx vitest run src/lib/session.test.ts
npx tsc --noEmit
```
Expected: PASS / 干净。既有测试若构造 `thread/tokenUsage/updated` 缺三个新字段，补 0（生成物把三者标为必填）。

- [x] **Step 5: 提交**

```bash
git add desktop/src
git commit -m "feat(desktop): 会话保存 subagent 累计用量"
```

---

### Task 7: 状态栏折叠态显示合计

**Files:**
- Modify: `desktop/src/components/StatusBar.tsx`
- Test: `desktop/src/components/StatusBar.test.tsx`

**Interfaces:**
- Consumes: `SubagentTotals`（Task 6）
- Produces: `StatusBar` props 新增 `subagentTotals: SubagentTotals`、`subagentCount: number`

- [x] **Step 1: 写失败测试**

```typescript
it("shows the sum of session and subagent usage when collapsed", () => {
  const { container } = render(
    <StatusBar
      cwd="/w"
      status="connected"
      totals={{ input: 1_000, output: 100, cacheRead: 10_000, cacheWrite: 0 }}
      subagentTotals={{ input: 500, output: 50, cacheRead: 5_000 }}
      subagentCount={2}
      fallbackUsage={null}
      usageId="t1"
    />,
  );
  // 折叠态是合计：1000+500 / 10000+5000 / 100+50。
  expect(screen.getByText(/1\.5k/)).toBeTruthy();
  expect(screen.getByText(/15\.0k/)).toBeTruthy();
  // 读屏句子与可见数字表达同一件事（都是合计），并说明含 subagent。
  expect(container.querySelector(".sr-only")?.textContent).toContain("including subagents");
  expect(container.querySelector(".sr-only")?.textContent).toContain("2 subagents");
});

it("keeps the existing sentence when there are no subagents", () => {
  const { container } = render(
    <StatusBar
      cwd="/w"
      status="connected"
      totals={{ input: 1_000, output: 100, cacheRead: 10_000, cacheWrite: 0 }}
      subagentTotals={{ input: 0, output: 0, cacheRead: 0 }}
      subagentCount={0}
      fallbackUsage={null}
      usageId="t1"
    />,
  );
  expect(container.querySelector(".sr-only")?.textContent).toBe(
    "1,000 cumulative input / 10,000 cumulative cache read / 100 cumulative output",
  );
});
```

- [x] **Step 2: 运行确认失败**

```bash
npx vitest run src/components/StatusBar.test.tsx
```
Expected: FAIL（缺 props）。

- [x] **Step 3: 实现合计**

`StatusBar.tsx`：

```typescript
  /** 折叠态展示的是合计：主会话 + 本会话所有 subagent。 */
  const shown: UsageTotals | null =
    totals ?? fallback;
```

其中 `fallback` 与现有逻辑一致；合计用：

```typescript
  const combined: UsageTotals | null = shown
    ? {
        input: shown.input + subagentTotals.input,
        output: shown.output + subagentTotals.output,
        cacheRead: shown.cacheRead + subagentTotals.cacheRead,
        cacheWrite: shown.cacheWrite,
      }
    : null;
```

三组 `RollingToken` 全部改用 `combined`。读屏句子：

```typescript
            <span className="sr-only">
              {hasSubagents
                ? `${thousands(combined.input)} cumulative input / ${thousands(combined.cacheRead)} cumulative cache read / ${thousands(combined.output)} cumulative output, including subagents, ${subagentCount} subagents`
                : `${thousands(combined.input)} cumulative input / ${thousands(combined.cacheRead)} cumulative cache read / ${thousands(combined.output)} cumulative output`}
            </span>
```

`hasSubagents = subagentCount > 0 || subagentTotals.input + subagentTotals.output + subagentTotals.cacheRead > 0`。

`UsagePanel` 传 `subagentTotals` 与 `hasSubagents`（Task 8 用）。

- [x] **Step 4: 跑测试**

```bash
npx vitest run src/components/StatusBar.test.tsx
npx tsc --noEmit
```
Expected: PASS。

- [x] **Step 5: 更新 App 调用点**

`App.tsx` 的 `<StatusBar ...>` 补。两处数据源都已在 `useSessionState` 里备好，
**不要**再给 `SubagentRailStore` 加计数方法（`railRows` 就是当前会话的行）：

```tsx
            subagentTotals={current?.session.subagentTotals ?? { input: 0, output: 0, cacheRead: 0 }}
            subagentCount={sess.railRows.length}
```

（`sess.railRows` 由 `useSessionState.ts:268` 提供：`currentId ? railStore.get(currentId) : []`。）

- [x] **Step 6: 提交**

```bash
npx vitest run src/App.test.tsx
git add desktop/src
git commit -m "feat(desktop): 状态栏折叠态显示含子 agent 的合计用量"
```

---

### Task 8: 展开面板分两组

**Files:**
- Modify: `desktop/src/components/UsagePanel.tsx`
- Test: `desktop/src/components/StatusBar.test.tsx`（面板在该文件里断言）

**Interfaces:**
- Consumes: `SubagentTotals`、`hasSubagents`
- Produces: `UsagePanel` props `{ totals, subagentTotals, hasSubagents, isFallback, model }`

- [x] **Step 1: 写失败测试**

```typescript
it("splits session and subagent usage in the panel", () => {
  render(
    <StatusBar
      cwd="/w"
      status="connected"
      totals={{ input: 1_000, output: 100, cacheRead: 10_000, cacheWrite: 0 }}
      subagentTotals={{ input: 500, output: 50, cacheRead: 5_000 }}
      subagentCount={1}
      fallbackUsage={null}
      usageId="t1"
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Token usage details" }));
  const panel = screen.getByRole("dialog");
  expect(within(panel).getByText("Session")).toBeTruthy();
  expect(within(panel).getByText("Subagents")).toBeTruthy();
  expect(within(panel).getByText("1,000 tokens")).toBeTruthy();   // 主会话 input
  expect(within(panel).getByText("500 tokens")).toBeTruthy();     // subagent input
});

it("hides the subagent group when there are none", () => {
  render(
    <StatusBar
      cwd="/w"
      status="connected"
      totals={{ input: 1, output: 2, cacheRead: 0, cacheWrite: 0 }}
      subagentTotals={{ input: 0, output: 0, cacheRead: 0 }}
      subagentCount={0}
      fallbackUsage={null}
      usageId="t1"
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Token usage details" }));
  expect(within(screen.getByRole("dialog")).queryByText("Subagents")).toBeNull();
});
```

- [x] **Step 2: 运行确认失败**

```bash
npx vitest run src/components/StatusBar.test.tsx
```
Expected: FAIL（没有 `Session`/`Subagents` 行）。

- [x] **Step 3: 实现分组**

`UsagePanel.tsx` 的 `dl` 改为两段（`hasSubagents` 为真才渲染第二段）：

```tsx
      <div className="mb-1 font-semibold text-fg-muted">Session</div>
      <dl className="mb-2 grid grid-cols-2 gap-y-1 font-mono">
        {/* input / output / cache read，主会话 */}
      </dl>
      {hasSubagents && (
        <>
          <div className="mb-1 font-semibold text-fg-muted">Subagents</div>
          <dl className="mb-2 grid grid-cols-2 gap-y-1 font-mono">
            {/* input / output / cache read，subagent */}
          </dl>
        </>
      )}
```

成本行改为：

```tsx
        <dt className="text-fg-muted">Est. cost</dt>
        <dd className="text-right">
          {formatCost(...)}
          {hasSubagents && <span className="text-fg-subtle"> (主会话)</span>}
        </dd>
```

- [x] **Step 4: 跑测试并提交**

```bash
npx vitest run src/components/StatusBar.test.tsx
npx tsc --noEmit
git add desktop/src
git commit -m "feat(desktop): 用量面板分开显示主会话与子 agent"
```

---

### Task 9: `/usage` 分两段

**Files:**
- Modify: `desktop/src/lib/usageCommand.ts`
- Modify: `desktop/src/App.tsx`（`/usage` 分支）
- Test: `desktop/src/lib/usageCommand.test.ts`

**Interfaces:**
- Consumes: `Session.subagentTotals`
- Produces: `formatUsage(snapshot: Usage, totals: UsageTotals | null, subagent: SubagentTotals | null, opts: { compactThreshold: number | null }): string`

- [x] **Step 1: 写失败测试**

```typescript
describe("formatUsage with subagents", () => {
  const totals = { input: 1_234, output: 56, cacheRead: 7_890, cacheWrite: 0 };
  const subagent = { input: 500, output: 60, cacheRead: 4_000 };

  it("prints a separate subagent section", () => {
    const out = formatUsage(base, totals, subagent, { compactThreshold: null });
    expect(out).toContain("主会话 input: 1,234  output: 56");
    expect(out).toContain("主会话 cache read: 7,890");
    expect(out).toContain("subagent input: 500  output: 60");
    expect(out).toContain("subagent cache read: 4,000");
    expect(out).toContain("（主会话，不含 subagent）");
  });

  it("keeps the single-section format when there are no subagents", () => {
    const out = formatUsage(base, totals, { input: 0, output: 0, cacheRead: 0 }, {
      compactThreshold: null,
    });
    // 无 subagent 时既有文案逐字不变（含"累计"字样、不含"主会话"）。
    // 这不是遗漏：改文案会连带改掉既有测试，且没有信息增益。
    expect(out).toContain("累计 input: 1,234  output: 56");
    expect(out).toContain("累计 cache read: 7,890");
    expect(out).not.toContain("主会话");
    expect(out).not.toContain("subagent");
  });

  it("treats a missing subagent total as none", () => {
    const out = formatUsage(base, totals, null, { compactThreshold: null });
    expect(out).not.toContain("subagent");
    expect(out).toContain("累计 input: 1,234  output: 56");
  });
});
```

- [x] **Step 2: 运行确认失败**

```bash
npx vitest run src/lib/usageCommand.test.ts
```
Expected: FAIL（参数个数不匹配）。

- [x] **Step 3: 实现**

`usageCommand.ts` 签名改为四参，`null`/全零 = 无 subagent：

```typescript
const hasSubagents =
  subagent !== null &&
  subagent.input + subagent.output + subagent.cacheRead > 0;
```

**文案是条件切换的**，不要无条件改名：

- 无 subagent（`!hasSubagents`）：走现有分支，文案逐字不变（`累计 input:` /
  `累计 cache read:` / `估算成本:`），既有测试一行都不用改。
- 有 subagent：主会话两行由 `累计 input:` 改为 `主会话 input:`（「累计」在含 subagent
  的语境里会被误读为合计），再追加两行 `subagent input:` / `subagent cache read:`，
  成本行追加「（主会话，不含 subagent）」。

理由：「主会话」这个限定只有在**同时**显示 subagent 时才有区分意义；无 subagent 时
它是个空指涉，只是无谓地改动用户已经熟悉的输出。

- [x] **Step 4: 接线 App**

```typescript
        session.notice(
          formatUsage(u, session.usageTotals, session.subagentTotals, {
            compactThreshold: threshold,
          }),
        );
```

- [x] **Step 5: 跑测试并提交**

```bash
npx vitest run src/lib/usageCommand.test.ts src/App.test.tsx
npx tsc --noEmit
git add desktop/src
git commit -m "feat(desktop): /usage 分开显示主会话与 subagent 用量"
```

---

### Task 10: 文档与全量校验

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`（计数）

- [x] **Step 1: 登记验收项**

在 desktop.md 的 Features 里加一条 `[x]`，包含：口径（input/output/cache read）、三处展示、代码位置、验证命令。

- [x] **Step 2: 更新索引计数**

按 `grep -c '^- \[x\]'` 实测值更新 `README.md` 的 desktop 行。

- [x] **Step 3: 全量校验**

```bash
cd .worktrees/subagent-usage/yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo fmt --all -- --check
cargo test -p yi-agent-store --lib
cargo test -p yi-agent-core --lib
cargo test -p yi-agent-subagent --lib
cargo test -p yi-agent-app-server --lib
```

```bash
export PATH="/opt/homebrew/bin:$PATH"
cd .worktrees/subagent-usage/desktop
npx tsc --noEmit
npx vitest run
```
Expected: 全绿。

- [x] **Step 4: 绑定一致性**

```bash
cd .worktrees/subagent-usage/yi-agent-rs
cargo test --all-features -p yi-agent-app-server export_bindings
cd .. && git status --porcelain desktop/src/lib/generated
```
Expected: 无输出（生成物与服务端一致）。

- [x] **Step 5: 提交**

```bash
git add docs
git commit -m "docs: 登记 subagent 用量统计"
```

---

## Self-Review

**Spec 覆盖检查：**

| Spec 要求 | 任务 |
| --- | --- |
| §5.1 `WatchdogUsage` 分项 | Task 1 |
| §5.2 事件带四项 | Task 2 |
| §5.3 `IpcTaskUsage` | Task 3 |
| §5.4 按 task 聚合（Rust 侧求和） | Task 3 |
| §5.5 协议字段 | Task 4 |
| §6 会话级汇总 + `/clear` 归零 | Task 5 |
| §7.1 折叠态合计 | Task 7 |
| §7.2 展开面板分组 | Task 8 |
| §7.3 `/usage` 分段 | Task 9 |
| §8 老数据兼容 | Task 1 Step 1、Task 3 Step 5、Task 6 Step 1 |
| §9 测试 | 各任务 Step 1 |
| §10 实施顺序 | Task 1–10 顺序一致 |

**占位符扫描：** 无 TBD / TODO；每个代码步骤都给了可落地的片段。

**类型一致性检查：**
- `WatchdogTokenDelta`（Task 1）→ `record_watchdog_progress` 参数（Task 1/2）一致。
- `TaskUsageTotals`（Task 3）→ `IpcTaskUsage`（Task 3）→ `AgentChildUsage`（Task 4）字段名均为 `input_tokens/output_tokens/cache_read_tokens`。
- `SubagentTotals`（Task 4，Rust，字段 `input/output/cache_read`）与 TS 侧 `SubagentTotals`（Task 6，`input/output/cacheRead`）**刻意不同**：Rust 沿用 snake_case，TS 沿用 camelCase，与既有 `UsageTotals` 的处理一致。
- `formatUsage` 参数顺序（Task 9）在 App 调用点（Task 9 Step 4）一致。

**已知取舍：**
- cache write 在 Task 2 Step 5 显式丢弃（用户口径不看），但在事件里保留传递，避免将来再加要重改上报链路。
- 0.5 秒轮询延迟是架构决定的，spec §11 已记录为已知限制。
