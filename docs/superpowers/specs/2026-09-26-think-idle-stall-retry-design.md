# THINK 阶段空闲停滞退避重试设计

**目标:** THINK 阶段的瞬时 provider 停滞（idle stall）不再直接终结整轮任务，而是带
指数退避重试（默认 3 次，2s/4s/8s）；并且**重试过程对用户可见**——用户应当被告知
"provider 停滞了，正在重试 1/3"，而不是在无提示的静默中等待。

**状态:** 设计已确认（v3，新增请求超时重试），待转实现计划。

**修订关系:** 本设计修订 `2026-07-27-agent-completion-design.md` 的 Non-goals 中
"不自动重试 provider 错误"一条，**仅限** THINK 阶段的空闲停滞。网络错误、鉴权失败、
工具错误的自动重试策略不变（仍由 `subagent_runtime.rs` 的 `evaluate_retry` 处理）。

**v2 修正说明:** v1 设计把重试做成对用户完全不可见（丢弃 partial、TUI 不产生任何
痕迹）。该取舍被否决：重试必须是**可感知**的。v2 改为保留 partial 并插入显式提示。

**v3 修正说明:** 请求超时（§9）此前仍是硬失败、无重试。用户要求请求超时也要重试
3 次，故纳入同一 attempt 循环。为此必须先把超时的**类型**信息保留到 core：实测
超时的错误 Display 是 `"error decoding response body"`（不含 "timed out"），所以
**不能靠字符串判定**，只能靠 `reqwest::Error::is_timeout()` 在 `stream.rs` 分类。

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

### 1.4 无提示的等待是不可接受的

这是 v2 新增的约束。退避意味着用户在 2s / 4s / 8s 的窗口内看不到任何进展；如果
重试又不可见，用户面对的就是一段无解释的静止画面。既有的可重试路径已经有可见
先例：工具重试会发出 `AgentEvent::ToolRetry`（`agent.rs:200-203`），headless 侧
打印 `[tool-retry:{id}]` 到 stderr（`main.rs:909-911`）。provider 停滞重试必须同样
可见。

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
| `think_stall_retry_limit: u16` | `3` | 单次 THINK 最多重试次数；`0` 关闭重试 |
| `think_stall_backoff_base: Duration` | `2s` | 第 n 次重试等待 `base * 2^(n-1)` |

即默认等待 2s / 4s / 8s，与用户要求一致。单次等待封顶 30s（与
`evaluate_retry` 的封顶一致）。

### 2.4 取消语义

退避 sleep 用 `tokio::select!` 对 `cancel_token`。取消时立即走 `Cancelled`，
并沿用 `agent.rs:540-548` 的回滚语义（`session.truncate(session_len)`）。

### 2.5 耗尽后的终态

重试耗尽仍是 `Done { Interrupted { reason } }`，但 reason 变为
`"idle timeout after N retries"`，便于日志与用户定位"这次是真的挂了"。

### 2.6 可见性（v2 核心修正）

重试通过 `AgentEvent::ProviderRetry { attempt, max, idle_secs }` 上报（`idle_secs`
是本次停滞等待的时长，供提示文案引用，避免消费端各自去猜配置值），**每个消费端都
必须让用户看得见**，且提示文案必须包含"正在重试"与进度：

| 消费端 | 呈现 |
| --- | --- |
| TUI 历史 | 插入 `HistoryCell::Separator`，label 形如 `Provider stalled (no output for 60s) — retrying 1/3`。这是**持久记录**：用户回看时能知道这里停过、重试过 |
| TUI 状态栏 | 不额外改动（历史分隔线已足够，避免引入"何时清除"的状态机） |
| headless `yi-agent run`（human） | stderr 打印 `[provider-retry:1/3]`，与既有 `[tool-retry:{id}]` 格式一致（`main.rs:909-911`） |
| headless（`--json`） | 事件原样序列化为 JSONL（诊断可见） |
| app-server → desktop | 新增 `Notification::TurnRetry { thread_id, turn_id, attempt, max }`（`turn/retry`）；desktop 显示一条提示，并在重试后的文本开始流式输出时自动消失 |

### 2.7 partial 内容的处理（v2 修正）

停滞时 `accumulate_stream` 返回已累积的部分内容，且这些 delta 已经以
`AgentEvent::AssistantText` 流式推给了 UI。

**决定：保留 partial 显示，同时插入提示分隔线；但 partial 不写入 session。**

- **TUI 保留 partial**：用户已经看到的文本不会被凭空抹掉。提示分隔线插在 partial
  之后，重试产生的新文本成为下一个 assistant cell，因此转录读起来是：
  `[第一段输出] ─ Provider stalled, retrying 1/3 ─ [重试后的完整输出]`。
  重复内容是**被解释过**的，而不是看起来像 bug。
- **不写入 session / messages**：provider 历史保持"一问一答"。若把 partial 当
  assistant 消息提交，重试就会在"已有半截回复"的基础上继续，语义错乱；若改成
  "保留 partial + 提示继续"（像 `MaxTokens` 路径 `agent.rs:589-596`），partial 中
  若已含完整 `ToolUse`，则 `tool_use` 没有配对 `tool_result`，Anthropic 会拒绝该
  请求，需要额外"partial 不含 tool_use"守卫——那是 resume 而非 retry，两件事不能混。

被否决的备选：

- **丢弃 partial 使重试完全不可见**（v1）：违反 §1.4；且把用户已经看到的文本抹掉，
  比重复更突兀。
- **丢弃 partial 但保留提示**：同样有"文本凭空消失"的问题。
- **保留 partial 且不提示**：屏幕上出现重复文本且无解释，用户无法理解发生了什么。

### 2.8 已知限制

- 重试后的输出会与 partial 重复。这是"保留已显示文本"的代价，由提示分隔线解释。
- headless 与 app-server 无法撤回已输出的字节，因此 partial 在 stdout / 客户端
  已渲染内容中保留（与 TUI 行为一致，均保留）。
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
            ├─ emit AgentEvent::ProviderRetry { attempt, max }   # 用户可见
            ├─ select! { sleep(backoff), cancel → Cancelled + rollback }
            └─ continue (不提交 partial、不递增 turn)
          else:
            └─ 走既有 stop_reason 分派（Stalled 耗尽 → Interrupted）
```

## 4. 事件与消费端

新增 `AgentEvent::ProviderRetry { attempt: u16, max: u16, idle_secs: u64 }`。`AgentEvent`
是普通枚举，四处穷尽匹配必须同步：

| 消费端 | 处理 |
| --- | --- |
| `tui/history.rs` | push `Separator { label: Some(format!("Provider stalled (no output for {idle_secs}s) — retrying {attempt}/{max}")) }`；不动已有 assistant cell |
| `tui/app.rs` (`route_event`) | 无操作（历史分隔线已是可见信号） |
| `main.rs` `drain_stream_human` | stderr 打印 `[provider-retry:{attempt}/{max}]`（保持与 `[tool-retry:{id}]` 同风格，不写 idle_secs） |
| `main.rs` `drain_stream_json` | 原样输出 JSONL 行 |
| `app-server/translate.rs` | 发 `Notification::TurnRetry { thread_id, turn_id, attempt, max }`；`protocol.rs` 加该变体 |
| `desktop/src/lib/{protocol.ts,session.ts}` | 记录 `retrying`；`ChatView` 渲染提示，收到 `item/delta` 或 `turn/completed` 时清除 |
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
- 不新增用户可见配置文件项（`AgentConfig` 字段先只由默认值与测试驱动；后续若需要
  可挂到 `RuntimeConfig`）。
- 不做"撤回已输出文本"的协议；partial 在所有消费端一律保留显示。

## 7. 测试策略

全部为 mock 测试，不调用真实 LLM。

| 层 | 测试 | 断言 |
| --- | --- | --- |
| `provider.rs` | 短 idle timeout，流发一个 delta 后 pending | 返回 `StopReason::Stalled` |
| `agent.rs` | 首次停滞、第二次成功 | 发 `ProviderRetry { attempt: 1, max: 3, .. }`；终态 `Done { EndTurn }`；session 恰好 1 条 assistant 且内容为重试后的文本（partial 未提交） |
| `agent.rs` | 每次都停滞 | attempt 次数 = limit + 1；`Done { Interrupted }` 且 reason = `"idle timeout after 3 retries"` |
| `agent.rs` | 退避期间取消 | 发 `Cancelled`，不发 `Done` |
| `agent.rs` | `AgentConfig` 默认值 | limit = 3，base = 2s |
| `tui/history.rs` | `ProviderRetry` | 新增一个 `Separator`，label 含 `retrying 1/3` 与 `no output for 60s`；已有 assistant cell 不被移除 |
| `main.rs` | `drain_stream_human` 收到 `ProviderRetry` | stderr 含 `[provider-retry:1/3]`；stdout 只有 assistant 文本 |
| `app-server/translate.rs` | `ProviderRetry` | 产出 `Notification::TurnRetry { attempt: 1, max: 3, .. }` |
| `desktop` (vitest) | `turn/retry` 后收到 `item/delta` | `retrying` 先被设置，随后被清除 |

## 8. 验证命令

```
cd yi-agent-rs
cargo test -p yi-agent-core --lib provider::
cargo test -p yi-agent-core --lib agent::
cargo test -p yi-agent --bin yi-agent drain_stream
cargo test -p yi-agent --bin yi-agent tui::history
cargo test -p yi-agent-app-server --lib
cd ../desktop && npm test
cargo fmt --all -- --check
```

---

## 9. 请求超时重试（v3 新增）

### 9.1 问题

`reqwest::ClientBuilder::timeout` 是**总时长上限**（reqwest 文档原话："applied from
when the request starts connecting until the response body has finished. Also
considered a total deadline"），默认 300s（`anthropic/client.rs:18`、
`openai/client.rs:17`），且 `bootstrap.rs` 未暴露该配置（用户改不了）。实测（见
§9.5）该上限会在**流仍在持续输出**时把连接切断。

切断后：`Error` → `run_loop` 立即 `return`，交互模式**不重试**，用户看到
`Error:` 分隔线、headless 退出码 1。与 idle stall 一样，这通常是瞬时故障
（网络抖动、服务端排队、中间设备静默丢连接），重发往往成功。

### 9.2 关键约束：超时信息在源头丢失

`anthropic/stream.rs:312`（openai 同构）把字节流错误字符串化：

```rust
Err(e) => Poll::Ready(Some(Err(ProviderError::Network(e.to_string())))),
```

`e` 是 `reqwest::Error`，这里丢掉后 `is_timeout()` 再不可得。实测该错误的
Display 是 `"error decoding response body"`——**字符串里没有 "timed out"**，
因此按字符串匹配超时是不可行的（这也是 §9.4 要用类型化 stop reason 的原因）。

实测（本 worktree 真实 TCP trickle 服务器 + 1s 超时）：

```
PROBE display  = error decoding response body
PROBE is_timeout = true
PROBE is_decode  = true
```

注意 `is_decode()` 对同一错误也返回 true，所以判定必须**先查 `is_timeout`**。

### 9.3 可重试 vs 不可重试

`"stream error: "` 前缀混合了两种性质，不能笼统重试：

| 情形 | 性质 | 处置 |
| --- | --- | --- |
| 传输超时（`is_timeout`） | 瞬时 | **重试**（本次目标） |
| 连接重置 / 传输中断（`is_connect`/`is_request`/`is_body` 等） | 瞬时 | 不重试（非目标，见 §9.7） |
| SSE 载荷非法（`ProviderError::Stream`，如 `invalid SSE JSON`） | provider 发了坏数据 | **不重试**，仍终结 |

既有测试 `agent_reports_provider_stream_stop_as_error`（`agent.rs:1500`，用
`StopReason::Other("stream error: invalid SSE payload")`）锁的是第三种，必须保持
终结。§9.4 的类型化设计天然满足：非法载荷走 `ProviderError::Stream` → `Error`，
不进入重试分支。

### 9.4 设计

**(a) `ProviderEvent` 新增类型化终态**

```rust
pub enum ProviderEvent {
    // ...
    Stop { reason: StopReason },
    /// A mid-stream transport failure, preserving its classification so the
    /// agent loop can decide retryability without string matching.
    StreamError(ProviderError),
    Usage(TokenUsage),
}
```

两个 client 的 `map` 改为：

```rust
Err(e) => ProviderEvent::StreamError(e),   // 原来是 Stop{Other("stream error: {e}")}
```

`stream.rs` 保留分类：字节错误处 `if e.is_timeout() { Network(...) } else { Stream(...) }`
（超时归入可重试的 `Network`，其余保持现状），具体分类规则由实现计划细化。

**(b) `accumulate_stream` 返回类型化失败**

`accumulate_stream` 的返回由 `Result<(content, stop_reason, usage), ProviderError>`
改为 `Result<(content, stop_reason, usage), AgentError>`（或等价地用一个内部枚举），
使**部分内容随错误一起返回**——这是 §2.7「partial 保留显示」在超时路径上的延伸。

**(c) 同一 attempt 循环，按类型判定**

`run_loop` 的 attempt 循环增加一条可重试条件：

```rust
let stalled = matches!(stop_reason, StopReason::Stalled);
let timed_out = matches!(&err, AgentError::Provider(ProviderError::Network(_)));
if (stalled || timed_out) && stall_retries < config.think_stall_retry_limit { /* 退避重试 */ }
```

复用同一 `stall_retries` 计数与 `stall_backoff_delay`（2s/4s/8s，封顶 30s），因此
「idle stall + 超时」合计最多 3 次重试，与用户要求的「超时需要 3 次重试」一致。

**(d) 终态**

重试耗尽后仍是 `Done { Interrupted { reason } }`，reason 形如
`"request timeout after 3 retries"`（与 stall 的 `"idle timeout after 3 retries"` 对称）。

### 9.5 实证记录（写入测试）

新增测试 `request_timeout_is_a_total_deadline_not_an_idle_timeout`
（`yi-agent-llm/tests/integration.rs`，commit `b7a6fde`）用真实 TCP 服务器每 300ms
持续发事件、共 3s，provider 超时 1s：

- 1s 超时下流在 **1.01s** 被切断（终态 `Stop{Other("stream error: ...")}`）
- 把超时改为 10s 复跑，同一流跑满 **3.02s** 并正常完成

两者共同证明：服务端从未空闲，切断确实来自「总时长」语义。该测试在本次改动后需要
适配新的事件形态（终态从 `Stop{Other}` 变为 `StreamError(Network)`）。

### 9.6 可见性

超时重试复用 `AgentEvent::ProviderRetry`，新增 `cause` 字段以区分文案：

```rust
ProviderRetry { attempt: u16, max: u16, idle_secs: u64, cause: RetryCause }

pub enum RetryCause { IdleStall, RequestTimeout }
```

| 消费端 | 超时文案 |
| --- | --- |
| TUI 历史 | `Provider request timed out — retrying 1/3` |
| headless stderr | `[provider-retry:1/3 timeout]`（stall 保持 `[provider-retry:1/3]` 兼容） |
| app-server → desktop | `TurnRetry { attempt, max, cause }`；desktop 横幅文案按 cause 切换 |

`AgentEvent` 与 `Notification` 都是普通枚举，穷尽匹配点（`tui/history.rs`、
`tui/app.rs`、`main.rs`、`translate.rs`、`subagent_runtime.rs`）需同步。

### 9.7 非目标

- 不重试连接重置等**其他**传输错误（只做超时）。它们同样经 `StreamError` 到达
  core，后续要覆盖只需扩展 (c) 的判定，无需再改协议。
- 不改 300s 总时长上限的数值，也不把它改成「空闲超时」语义（那是另一个问题：
  长流被总时长切断的根因）。仅在本设计中记录为已知限制。
- 不把该超时做成用户可配置项（`bootstrap.rs` 仍用默认值）。

### 9.8 最坏耗时（更新）

默认 `60s idle × 4 + 14s 退避 = 254s`（纯 stall），超时路径为
`300s × 4 + 14s = 1214s`。daemon 侧 `max_wall_time_secs = 2700`（`schedule.rs:159`）
可容纳；但交互模式无 wall-clock 上限，用户需知道超时重试最坏约 20 分钟。TUI 的
可见提示（§9.6）是这里的必要缓冲。
