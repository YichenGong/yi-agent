# THINK 阶段空闲停滞退避重试设计

**目标:** 让 THINK 阶段的瞬时 provider 停滞（idle stall）对用户不可见——不再把一次
停滞直接变成 `Done { Interrupted { reason: "idle timeout" } }` 终结整轮任务，而是
带指数退避重试（默认 3 次，2s/4s/8s）后仍未恢复才终结。

**状态:** 设计已确认，待转实现计划。

**修订关系:** 本设计修订 `2026-07-27-agent-completion-design.md` 的 Non-goals 中
"不自动重试 provider 错误"一条，**仅限** THINK 阶段的空闲停滞。网络错误、鉴权失败、
工具错误的自动重试策略不变（仍由 `subagent_runtime.rs` 的 `evaluate_retry` 处理）。

---

## 1. 问题

### 1.1 一次瞬时停滞就终结任务

`crates/yi-agent-core/src/provider.rs:116-127` 在 `think_idle_timeout`（默认 60s，
`agent.rs:115`）内没等到任何 provider 事件时 break 出消费循环，并在
`provider.rs:182` 合成 `StopReason::Other("idle timeout")`。

`crates/yi-agent-core/src/agent.rs:607-614` 对该 stop reason 的处理是**直接终结**：

```rust
StopReason::Other(reason) => {
    let _ = tx.send(AgentEvent::Done {
        reason: DoneReason::Interrupted { reason },
    }).await;
    return;
}
```

用户看到的是 `Interrupted: idle timeout` 分隔线（`tui/history.rs:423-428`），整轮
任务结束。而 60s 无事件在实际网络抖动、服务端排队、连接被中间设备静默丢弃时是
常见的瞬时现象——重发同一个请求往往立刻成功。

### 1.2 已有重试机制覆盖不到这条路径

仓库已有 `yi-agent-store/src/schedule.rs:107` 的 `evaluate_retry`（1s 起、翻倍、
封顶 30s、limit 3），由 `crates/yi-agent/src/subagent_runtime.rs:627-643` 使用。但它
挂在 `AgentEvent::Error` 上，而 `provider_retry_failure`
（`subagent_runtime.rs:365-374`）只认 `ProviderError::Network / RateLimited / Server
/ Stream`。空闲停滞**不产生** `Error`，所以交互模式与 subagent 都不会重试它。

`yi-agent-core` 不能依赖 `yi-agent-store`（依赖方向相反），策略必须落在 core 内。

### 1.3 合成停滞与显式 stop reason 共用魔法字符串

`agent.rs:1250` 的既有测试 `agent_does_not_report_abnormal_stop_as_end_turn` 用
**显式** `ProviderEvent::Stop { reason: StopReason::Other("idle timeout") }` 表达
"provider 主动上报的异常停止"。合成停滞与它共用同一个字符串，若按字符串触发重试，
该测试会因为第二次调用返回 `EndTurn` 而失败——字符串当协议是脆弱点。

---

## 2. 设计

### 2.1 用专用变体区分"合成停滞"

`StopReason` 新增变体 `Stalled`（`provider.rs:54-59`）。`accumulate_stream` 在
idle timeout 触发时设置 `StopReason::Stalled`，不再复用 `Other("idle timeout")`。
`Other("stream ended without stop")` 与 provider 主动上报的 `Other(_)` 行为不变。

这样"可重试的停滞"是一个**类型**而不是一段文本：provider 若真的上报
`Other("idle timeout")`，仍是终结路径，`agent.rs:1250` 的测试语义保持成立。

### 2.2 THINK 阶段的 attempt 循环

`run_loop`（`agent.rs:445-...`）中，"provider turn gate acquire → `call_stream`
→ `accumulate_provider_stream`"这一段包进 attempt 循环：

- 每次 attempt **重新** `gate.acquire()`（lease 是 per provider turn 的，
  `agent.rs:485-502`），并在 sleep 前 `drop` 掉租约，不占着租约睡觉。
- 重试**不追加** user 消息、**不递增** `turn`、不消耗 `max_turns` 预算（否则 3 次
  重试会吃掉三轮额度），只重发同一个 `ProviderRequest`（`ProviderRequest` 已
  `Clone`，`provider.rs:13`）。
- 每次 attempt 前照常发 `EstimatedPrefill`，让状态栏有活动迹象。

### 2.3 退避参数

`AgentConfig` 新增两个字段：

| 字段 | 默认 | 含义 |
| --- | --- | --- |
| `think_stall_retry_limit: u16` | `3` | 单次 THINK 最多重试次数 |
| `think_stall_backoff_base: Duration` | `2s` | 第 n 次重试等待 `base * 2^n` |

即默认等待 2s / 4s / 8s，与用户要求一致。退避封顶 30s（与
`evaluate_retry` 的封顶一致）。

### 2.4 取消语义

退避 sleep 用 `tokio::select!` 对 `cancel_token`。取消时立即走 `Cancelled`，
并沿用 `agent.rs:540-548` 的回滚语义（`session.truncate(session_len)`）。

### 2.5 耗尽后的终态

重试耗尽仍是 `Done { Interrupted { reason } }`，但 reason 变为
`"idle timeout after N retries"`，便于日志与用户定位"这次是真的挂了"。

### 2.6 部分内容的处理（关键取舍）

停滞时 `accumulate_stream` 返回的是**已累积的部分内容**，且这些 delta 已经以
`AgentEvent::AssistantText` 流式推给了 UI。重试会重发请求，模型会重新输出开头文本。

取舍：**丢弃部分内容，不写入 session**。

- 不提交 partial 到 `messages` / `session`（`agent.rs:581-585` 在停滞检查之后，
  天然可以跳过），provider 历史保持"一问一答"，不会出现半截 assistant 消息。
- 新增 `AgentEvent::ProviderRetry { attempt, max }`。TUI 收到该事件时，**弹出
  尾部未完成的 `AssistantMessage` cell**，因此被丢弃的 partial 不会留在屏幕上，
  重试对用户不可见；同时该事件本身是重试的可见信号（诊断用）。

为什么不用另外两种方案：

- **保留 partial 直接重试**：屏幕上会出现 partial 后面紧跟重试文本的重复内容，
  与"对用户不可见"的目标相反。
- **保留 partial 改"续写"**（像 `MaxTokens` 路径 `agent.rs:589-596`）：partial 中若
  已含完整 `ToolUse`，则 `tool_use` 没有配对 `tool_result`，Anthropic 会拒绝该请求，
  需要额外"partial 不含 tool_use"守卫；而且语义从 retry 变成 resume，两件事混在一起。

### 2.7 已知限制

- headless（`yi-agent run`）与 app-server 无法"撤回"已输出的字节：被丢弃的 partial
  文本可能仍留在 stdout / 客户端已渲染的内容里。TUI 是唯一能真正抹掉 partial 的
  消费端。本设计接受该限制，不发明回撤协议。
- 最坏耗时上升：默认 60s idle + 3 次重试 = 最坏 `60*4 + 14 = 254s` 才终态，比现在
  的 60s 长。daemon 侧 `max_idle_time_secs = 300`（`schedule.rs:160`）且
  `ProviderRetry` 不推进 `last_meaningful_at`（`runtime.rs:2760`），254s < 300s，
  仍在 watchdog 预算内但余量不大。

---

## 3. 组件与数据流

```
run_loop (per turn)
  └─ attempt loop (0..=limit)
       ├─ gate.acquire()                 # 每次 attempt 独立租约
       ├─ provider.call_stream(req)      # 同一个 req,Clone 复用
       ├─ accumulate_provider_stream(...)
       │    └─ StopReason::Stalled  ──┐
       ├─ drop(lease)                 │
       └─ if Stalled && attempt < limit:
            ├─ emit AgentEvent::ProviderRetry { attempt, max }
            ├─ select! { sleep(backoff), cancel → Cancelled + rollback }
            └─ continue (不提交 partial、不递增 turn)
          else:
            └─ 走既有 stop_reason 分派（Stalled 耗尽 → Interrupted）
```

## 4. 事件与消费端

新增 `AgentEvent::ProviderRetry { attempt: u16, max: u16 }`。`AgentEvent` 是普通
枚举，四处穷尽匹配必须同步：

| 消费端 | 处理 |
| --- | --- |
| `tui/history.rs` | 弹出尾部未完成的 `AssistantMessage` cell（丢弃 partial）；不新增历史行 |
| `tui/app.rs` (`route_event`) | 重置 decode 估算（partial 的 decode 计数一并作废） |
| `main.rs` `drain_stream_human` | 忽略（不写 stdout，避免污染管道输出） |
| `main.rs` `drain_stream_json` | 原样输出 JSONL 行（诊断可见） |
| `app-server/translate.rs` | 忽略（与 `ToolRetry` 同级），不产生通知 |
| `subagent_runtime.rs` | 映射为 `reporter.report_provider_retry()`，与 `ToolRetry` 对称，进入 daemon 预算记账 |

## 5. 错误处理

- 重试期间收到 `cancel_token`：立即 `Cancelled`，不进入下一次 attempt。
- `provider.call_stream` 返回 `Err`：行为不变（发 `Error` 并终结），本轮不重试——
  网络类错误仍由既有 `evaluate_retry` 路径在 runtime 层处理。
- `"stream ended without stop"` 与 `"stream error: ..."`：行为不变。前者仍是
  终结路径（**非目标**，见 §6）。

## 6. 非目标

- 不重试 `"stream ended without stop"`（EOF 无 Stop）。它与 idle stall 同类，但不在
  本次诉求内；若后续要覆盖，应作为独立变更并复用同一 attempt 循环。
- 不改动 `subagent_runtime.rs` 既有的 `evaluate_retry` 策略与网络错误重试。
- 不新增配置项到用户可见配置文件（`AgentConfig` 字段先只由默认值与测试驱动；
  后续若需要可挂到 `RuntimeConfig`）。
- 不发明"撤回已输出文本"的协议。

## 7. 测试策略

全部为 mock 测试，不调用真实 LLM。

| 层 | 测试 | 断言 |
| --- | --- | --- |
| `provider.rs` | `accumulate_stream` + 短 idle timeout，流只发一个 delta 后 pending | 返回 `StopReason::Stalled` |
| `agent.rs` | 首次停滞、第二次成功 | 只发一次 `Done { EndTurn }`；有 `ProviderRetry`；session 恰好 1 条 assistant 且内容为重试后的文本（partial 未提交） |
| `agent.rs` | 每次都停滞 | attempt 次数 = limit + 1；`Done { Interrupted { reason } }` 且 reason 含重试次数 |
| `agent.rs` | 退避期间取消 | 发 `Cancelled`，不发 `Done` |
| `agent.rs` | `AgentConfig` 默认值 | limit = 3，base = 2s |
| `tui/history.rs` | `ProviderRetry` 在尾部为未完成 assistant 时 | 该 cell 被弹出，cells 数不增 |
| `tui/history.rs` | `ProviderRetry` 在尾部非 assistant 时 | 无 cell 变动，不 panic |
| `main.rs` | `drain_stream_human` 收到 `ProviderRetry` | stderr/stdout 无新增输出 |

## 8. 验证命令

```
cd yi-agent-rs
cargo test -p yi-agent-core --lib provider::
cargo test -p yi-agent-core --lib agent::
cargo test -p yi-agent --bin yi-agent drain_stream
cargo test -p yi-agent --lib tui::history
cargo fmt --all -- --check
```
