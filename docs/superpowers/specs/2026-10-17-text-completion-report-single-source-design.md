# 设计：子任务文本完成报告解码收敛为单一实现（`text_completion_report`）

日期：2026-10-17
状态：待评审
关系：本文来自 2026-10-17 的仓库冗余审计（P0-B）。审计发现该解码函数有三份副本，
且其中一份已与另外两份产生行为漂移。

## 1. 背景与问题

子任务结束时，其「文本完成报告」以一种固定的终态 JSON 形状落盘：

```json
{ "kind": "text_completion", "report": "……" }
```

（可能带额外的 `reason` 字段，例如 `budget_exhausted` 会写
`{"reason":"turn_budget_exhausted","kind":"text_completion","report":"…"}`。）

解码这一形状的函数 `text_completion_report` 被实现了三份：

| 文件 | 定义位置 | 是否过滤空白报告 |
|---|---|---|
| `crates/yi-agent-store/src/repository.rs` | 4140 | **是**：末尾有 `.filter(\|report\| !report.trim().is_empty())` |
| `crates/yi-agent-store/src/runtime.rs` | 3161 | 否 |
| `crates/yi-agent-subagent/src/lib.rs` | 1832 | 否 |

`repository.rs:4136-4139` 的注释还写明「Mirrors the store's own decoder; kept local so
the reclaim path does not reach across module boundaries for one field」——
即作者**知道**这是重复，并主动选择复制。

### 真实危害（行为已漂移）

三份不只在代码上重复，**语义已经不一致**：当 `report` 是空串或纯空白（例如
`{"kind":"text_completion","report":"   "}`）时：

- `repository.rs` 的那份返回 `None`（视为「没有报告」）；
- `runtime.rs` 与 `subagent/src/lib.rs` 的那份返回 `Some("   ")`（视为「有报告」）。

由于 repository 那份用于**孤儿回收路径**（`repository.rs:2304`，把尝试的
`terminal_json` 转写成任务终态），而 runtime/subagent 两份用于**读取路径**，于是同一
个终态 JSON 在「回收」与「读取」两条路径下给出的报告存在性判断不同。这是一个已经
发生、但未被测试覆盖的正确性分歧。

## 2. 目标与非目标

### 目标

1. **解码只有一份实现**，位于 `yi-agent-store`。
2. **空白报告的语义只定义一次**：统一为「空白即无报告」（`None`）。
3. `repository.rs` 与 `runtime.rs` 删除本地副本；`yi-agent-subagent` 改为跨 crate 引用。

### 非目标

- 不改终态 JSON 的**写入**形状（`text_completion_terminal_json` /
  `budget_exhausted_terminal_json` 保持原位、保持输出不变）。
- 不改 `delivery_json` 等其他载荷的解码。
- 不改任何 IPC 协议。

## 3. 设计

### 3.1 为什么落在 `yi-agent-store`

依赖方向决定了唯一可行的落点：

```
yi-agent-core  ←  yi-agent-store  ←  yi-agent-subagent
```

- `yi-agent-store` 依赖 `yi-agent-core`，不依赖 subagent；
- `yi-agent-subagent` 依赖 `yi-agent-store`（`Cargo.toml` 已声明）。

`text_completion_report` 是 `yi-agent-store` 自己写出的终态形状（两份 writer 都在
`store/src/runtime.rs`），因此**解码归 shape 的生产者所有**，放在 store；subagent 作为
下游直接引用。放在 core 亦可，但 core 并不知道「终态 JSON」这一持久化形状，放 core
会引入概念错位。

### 3.2 接口

新建 `crates/yi-agent-store/src/terminal.rs`，在 `src/lib.rs` 声明
`pub mod terminal;`：

```rust
//! 子任务终态 JSON 的公共解码。
//!
//! 终态形状由本 crate 的 `runtime` 模块写出（`text_completion_terminal_json` /
//! `budget_exhausted_terminal_json`），因此解码也归本模块所有，避免回收路径与
//! 读取路径各自再实现一遍而漂移。

/// 从终态 JSON 中取出 `text_completion` 形状的文本报告。
///
/// 返回 `None` 的三种情况：入参为 `None`、JSON 非法、`kind` 不是
/// `text_completion`、或 `report` 为空/纯空白。
pub fn text_completion_report(terminal_json: Option<&str>) -> Option<String> {
    let payload = serde_json::from_str::<serde_json::Value>(terminal_json?).ok()?;
    (payload.get("kind").and_then(serde_json::Value::as_str) == Some("text_completion"))
        .then(|| payload.get("report").and_then(serde_json::Value::as_str))
        .flatten()
        .filter(|report| !report.trim().is_empty())
        .map(str::to_owned)
}
```

**语义定夺**：采用**过滤空白**（即 repository 的现行行为）。理由：一份只有空白的
「报告」对父级没有任何信息量，把它当作「有报告」会让父级收到一个空上下文块；而把它
当作「无报告」则与 `report.unwrap_or("")`（writer 侧写入空串表示「没有内容」）的
意图一致。这是一个刻意的行为变更，会在 §4 用测试锁定。

### 3.3 调用点改造

| 文件 | 改动 |
|---|---|
| `store/src/lib.rs` | 增加 `pub mod terminal;` |
| `store/src/repository.rs` | 删除本地 `fn text_completion_report`（4140-4147）；顶部 `use crate::terminal::text_completion_report;`；调用点 2304 不变 |
| `store/src/runtime.rs` | 删除本地 `fn text_completion_report`（3161-3167）；`use crate::terminal::text_completion_report;`；调用点 3269 不变 |
| `subagent/src/lib.rs` | 删除本地 `fn text_completion_report`（1832-1838）；`use yi_agent_store::terminal::text_completion_report;`；调用点 2009 不变 |

`repository.rs` 中那条「kept local so … does not reach across module boundaries」
的注释一并删除（前提已不成立）。

## 4. 测试策略

新增 `terminal.rs` 单元测试（`#[cfg(test)] mod tests`）：

1. `text_completion` 形状 + 正常报告 → `Some("…")`；
2. 带额外 `reason` 字段（budget_exhausted 形状）→ 仍能取出报告；
3. `kind` 不是 `text_completion` → `None`；
4. `report` 为空串 → `None`；
5. `report` 为纯空白 → `None`（**锁定本设计的行为变更**）；
6. 入参 `None` → `None`；
7. JSON 非法 → `None`。

回归证据：`cargo test -p yi-agent-store` 与 `cargo test -p yi-agent-subagent` 既有测试
全绿（`runtime_coordinator.rs:3330`、`runtime_ipc.rs:5036` 等已覆盖正常报告路径）。

## 5. 验收标准

- `grep -rn "fn text_completion_report" crates/` 只命中 `store/src/terminal.rs` 一处。
- `cargo test -p yi-agent-store -p yi-agent-subagent` 全绿。
- `cargo test --workspace` 全绿。
- `cargo clippy --workspace --all-targets -- -D warnings` 无警告。
