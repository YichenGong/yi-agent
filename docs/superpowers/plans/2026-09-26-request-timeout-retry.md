# 请求超时退避重试 实现计划（v3）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让请求超时（reqwest 总时长上限，默认 300s）与空闲停滞一样，带 2s/4s/8s 退避重试 3 次，重试对用户可见。

**Architecture:** 先把超时的**类型**信息从 provider 层保留到 core：`ProviderEvent` 新增 `StreamError(ProviderError)`，两个 client 不再把中途传输错误字符串化成 `Stop{Other("stream error: ...")}`；`stream.rs` 在仍持有 `reqwest::Error` 时用 `is_timeout()` 分类。`accumulate_stream` 改为返回 `StreamEnd`（携带 partial content），使超时路径也能保留已显示文本。`run_loop` 的 attempt 循环把「超时」加入可重试条件，复用既有退避与计数。

**Spec:** `docs/superpowers/specs/2026-09-26-think-idle-stall-retry-design.md` §9

## Global Constraints

- 工作目录 `yi-agent-rs/`；分支 `fix/think-idle-stall-retry`。
- 重试预算**共享**：`stall_retries` 一个计数，idle stall 与超时合计最多 `think_stall_retry_limit`（默认 3）次。
- 退避复用 `stall_backoff_delay`：2s/4s/8s，单次封顶 30s。
- 只有**超时**可重试；SSE 载荷非法（`ProviderError::Stream`，如 `invalid SSE JSON`）与连接重置等仍终结。
- 判定必须**先查 `is_timeout()`**——实测同一错误 `is_decode()` 也返回 true。
- 既有测试 `agent_reports_provider_stream_stop_as_error`、`request_timeout_is_a_total_deadline_not_an_idle_timeout` 语义不得丢失（后者需适配新事件形态）。
- 提交前 `cargo fmt --all`；不写 `Co-Authored-By`。

---

### Task 1: `ProviderEvent::StreamError` 类型化 + 超时分类

**Files:**
- Modify: `crates/yi-agent-core/src/provider.rs`（`ProviderEvent` 枚举）
- Modify: `crates/yi-agent-llm/src/anthropic/stream.rs:311-313`、`crates/yi-agent-llm/src/openai/stream.rs`（同构点）
- Modify: `crates/yi-agent-llm/src/anthropic/client.rs:151-156`、`crates/yi-agent-llm/src/openai/client.rs:133-138`
- Test: `crates/yi-agent-llm/tests/integration.rs`、`crates/yi-agent-llm/src/anthropic/stream.rs`（`mod tests`）

**Interfaces:**
- Produces: `ProviderEvent::StreamError(ProviderError)`；超时经 `is_timeout()` 分类为 `ProviderError::Network`。

- [x] **Step 1: 写失败测试（stream.rs 单测）**

在 `anthropic/stream.rs` 的 `mod tests` 内加：喂一个 `Err(reqwest::Error)` 无法在单测构造，故改为在集成测试断言端到端行为（见 Step 1b）。先在 `integration.rs` 的 trickle 测试里断言新形态：

```rust
    let last = events.last().expect("at least one event");
    match last {
        ProviderEvent::StreamError(ProviderError::Network(msg)) => assert!(
            msg.contains("stream error") || msg.contains("timed out"),
            "expected a typed transport failure, got: {msg}"
        ),
        other => panic!("expected StreamError after timeout, got: {other:?}"),
    }
```

- [x] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-llm --test integration request_timeout_is_a_total_deadline_not_an_idle_timeout`
Expected: 编译失败 `no variant named StreamError`.

- [x] **Step 3: 加变体 + 分类 + 改 client map**

`provider.rs` 的 `ProviderEvent`：

```rust
    Stop { reason: StopReason },
    /// A mid-stream transport failure. Carries the classified error so the
    /// agent loop can decide retryability without string matching.
    StreamError(ProviderError),
```

`anthropic/stream.rs:311-313`：

```rust
                Poll::Ready(Some(Err(e))) => {
                    // Classify while the reqwest::Error is still in hand: its
                    // Display for a deadline is "error decoding response body",
                    // which carries no "timed out" text. is_timeout() inspects
                    // the source chain and is the only reliable signal.
                    let error = if e.is_timeout() {
                        ProviderError::Network(format!("stream error: timed out: {e}"))
                    } else {
                        ProviderError::Network(format!("stream error: {e}"))
                    };
                    return Poll::Ready(Some(Err(error)));
                }
```

（`openai/stream.rs` 同构点做同样处理。）

`anthropic/client.rs` 与 `openai/client.rs` 的 `map`：

```rust
                Err(e) => ProviderEvent::StreamError(e),
```

- [x] **Step 4: 确认通过**

Run: `cargo test -p yi-agent-llm --test integration`
Expected: PASS（trickle 测试现在断言 `StreamError(Network)`）。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/provider.rs crates/yi-agent-llm/src
git commit -m "feat(llm): preserve mid-stream failure classification as StreamError"
```

---

### Task 2: `accumulate_stream` 保留 partial 并返回类型化结束

**Files:**
- Modify: `crates/yi-agent-core/src/provider.rs`（`accumulate_stream` 及其 ~12 个测试调用点）
- Modify: `crates/yi-agent-core/src/agent.rs:1081-1110`（`accumulate_provider_stream`）

**Interfaces:**
- Produces:
  ```rust
  pub enum StreamEnd {
      Stopped(StopReason),
      /// Transport failure; partial content is returned alongside.
      Failed(ProviderError),
  }
  pub async fn accumulate_stream<F>(...) -> Result<(Vec<ContentBlock>, StreamEnd, Option<TokenUsage>), ProviderError>
  ```
  外层 `Result` 仍只表示 accumulate 自身失败（如 malformed tool JSON）。

- [x] **Step 1: 写失败测试**

在 `provider.rs` 的 `mod tests` 内加：

```rust
    #[tokio::test]
    async fn accumulate_stream_surfaces_transport_failure_with_partial() {
        let stream = futures::stream::iter(vec![
            text_event("partial"),
            ProviderEvent::StreamError(ProviderError::Network("stream error: timed out".into())),
        ])
        .boxed();

        let (content, end, _) = accumulate_stream(stream, |_| {}, None).await.unwrap();

        // Partial text is preserved so consumers can keep showing it.
        assert_eq!(content, vec![ContentBlock::Text("partial".into())]);
        assert!(matches!(
            end,
            StreamEnd::Failed(ProviderError::Network(msg)) if msg.contains("timed out")
        ));
    }
```

- [x] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-core --lib provider::tests::accumulate_stream_surfaces_transport_failure_with_partial`
Expected: 编译失败（`StreamEnd` 不存在）。

- [x] **Step 3: 实现 `StreamEnd` 与签名变更**

`provider.rs` 加枚举，`accumulate_stream` 的 `stop_reason` 局部改为 `end: StreamEnd`：
- `ProviderEvent::Stop { reason }` → `end = StreamEnd::Stopped(reason)`
- `ProviderEvent::StreamError(e)` → `end = StreamEnd::Failed(e); break false;`
- stall → `end = StreamEnd::Stopped(StopReason::Stalled)`
- 无 Stop 的 EOF → `end = StreamEnd::Stopped(StopReason::Other("stream ended without stop"))`
- 返回 `Ok((content, end, last_usage))`

`agent.rs` 的 `accumulate_provider_stream` 返回值随之改为同样的元组。

- [x] **Step 4: 更新调用点与既有测试**

`provider.rs` 内 ~12 个测试把 `stop_reason` 断言改为 `StreamEnd::Stopped(...)`；
`agent.rs` 的 `agent_think_stream_stall_emits_terminal_within_timeout` 等按新类型适配。

Run: `cargo test -p yi-agent-core --lib provider::`
Run: `cargo test -p yi-agent-core --lib agent::`
Expected: PASS。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src
git commit -m "refactor(core): return typed stream end with preserved partial"
```

---

### Task 3: attempt 循环纳入超时 + `cause` 字段

**Files:**
- Modify: `crates/yi-agent-core/src/agent.rs`（`AgentEvent::ProviderRetry`、`RetryCause`、attempt 循环、终态 reason）
- Test: `crates/yi-agent-core/src/agent.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum RetryCause { IdleStall, RequestTimeout }
  ProviderRetry { attempt: u16, max: u16, idle_secs: u64, cause: RetryCause }
  ```
  超时耗尽终态 reason：`"request timeout after {n} retries"`。

- [x] **Step 1: 写失败测试**

```rust
    /// Succeeds on the 2nd call; the 1st fails with a timeout mid-stream.
    struct TimeoutOnceThenSucceedProvider { calls: std::sync::Mutex<u32> }
    // call_stream: 1st → StreamError(Network("stream error: timed out")); 2nd → text+Stop{EndTurn}

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_retries_a_request_timeout_and_completes() {
        // 断言：发出 ProviderRetry{cause: RequestTimeout, attempt: 1, max: 3}；
        // 终态 Done{EndTurn}；无 Interrupted；session 只有重试后的文本。
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_exhausts_timeout_retries_then_reports_interruption() {
        // 每次都超时 → 4 次调用；3 条 ProviderRetry；终态 reason == "request timeout after 3 retries"。
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_does_not_retry_a_malformed_sse_payload() {
        // StreamError(ProviderError::Stream("invalid SSE JSON")) → 1 次调用，Error，无 ProviderRetry。
    }
```

- [x] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-core --lib agent::tests::agent_retries_a_request_timeout_and_completes`
Expected: 编译失败（`cause`/`RetryCause` 不存在）。

- [x] **Step 3: 实现**

- `RetryCause` 枚举 + `ProviderRetry` 加 `cause`。
- attempt 循环：
  ```rust
  let (content, end, last_usage) = /* accumulate */;
  let retryable = match &end {
      StreamEnd::Stopped(StopReason::Stalled) => Some(RetryCause::IdleStall),
      StreamEnd::Failed(ProviderError::Network(_)) => Some(RetryCause::RequestTimeout),
      _ => None,
  };
  if let Some(cause) = retryable {
      if stall_retries < config.think_stall_retry_limit { /* emit ProviderRetry{cause}, sleep, continue */ }
  }
  ```
- 非可重试的 `StreamEnd::Failed(e)` → `AgentEvent::Error` 并 return（保持终结）。
- 耗尽终态按 cause 选择 reason 文案。

- [x] **Step 4: 确认通过**

Run: `cargo test -p yi-agent-core --lib`
Expected: PASS（含既有 stall 测试与 `agent_reports_provider_stream_stop_as_error` 的等价覆盖）。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "feat(core): retry a request timeout with the shared stall backoff"
```

---

### Task 4: 消费端按 cause 呈现

**Files:**
- Modify: `crates/yi-agent/src/tui/history.rs`、`crates/yi-agent/src/tui/app.rs`
- Modify: `crates/yi-agent/src/main.rs`
- Modify: `crates/yi-agent-app-server/src/protocol.rs`、`translate.rs`
- Modify: `desktop/src/lib/protocol.ts`、`session.ts`、`components/ChatView.tsx`
- Test: 各文件测试

**Interfaces:**
- `Notification::TurnRetry { thread_id, turn_id, attempt, max, cause }`（wire `turn/retry`）
- `Session.retrying: { attempt, max, cause } | null`

- [x] **Step 1: 写失败测试**

- `tui/history.rs`：`ProviderRetry{cause: RequestTimeout}` → label 含 `request timed out` 与 `retrying 1/3`。
- `main.rs`：`drain_stream_human` → stderr 含 `[provider-retry:1/3 timeout]`；stall 仍为 `[provider-retry:1/3]`。
- `translate.rs`：`TurnRetry` 携带 `cause`。
- `desktop session.test.ts`：`turn/retry` 带 cause → `retrying.cause`。

- [x] **Step 2: 运行确认失败**（各层）

- [x] **Step 3: 实现**

TUI 文案按 cause：
```rust
let label = match cause {
    RetryCause::IdleStall => format!("Provider stalled (no output for {idle_secs}s) — retrying {attempt}/{max}"),
    RetryCause::RequestTimeout => format!("Provider request timed out — retrying {attempt}/{max}"),
};
```
headless：`IdleStall => "[provider-retry:{a}/{m}]"`，`RequestTimeout => "[provider-retry:{a}/{m} timeout]"`（stall 文案不变，向后兼容既有测试）。
app-server：`TurnRetry` 加 `cause: String`（`"idle_stall"` / `"request_timeout"`）。
desktop：横幅文案按 cause 切换。

- [x] **Step 4: 确认通过**

Run: `cargo test -p yi-agent --bin yi-agent tui::`、`cargo test -p yi-agent --bin yi-agent drain_stream`、`cargo test -p yi-agent-app-server --lib`、`cd desktop && npm test && npx tsc --noEmit`

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src crates/yi-agent-app-server/src
git commit -m "feat: surface request-timeout retries distinctly across consumers"
cd ../desktop && git add src && git commit -m "feat(desktop): distinguish timeout retries in the notice"
```

---

### Task 5: 文档

- [x] 更新 `docs/bug-list.md`（该条已 `[x]`，补超时重试）
- [x] 更新 `docs/project-management/yi-agent-core.md` 与 `README.md` 计数
- [x] 提交

---

## 收尾验证

Run: `cd yi-agent-rs && cargo fmt --all -- --check`
Run: `cargo test -p yi-agent-core --lib && cargo test -p yi-agent-llm`
Run: `cargo test -p yi-agent --bin yi-agent`
Run: `cargo test -p yi-agent-app-server --lib`
Run: `cd desktop && npm test && npx tsc --noEmit`

## Self-Review

- Spec §9.2（源头分类）→ Task 1；§9.4(b)（partial 保留）→ Task 2；§9.4(c)(d)（重试与终态）→ Task 3；§9.6（可见性）→ Task 4；§9.3（不可重试）→ Task 3 Step 1 的 malformed 测试。
- 已知限制（§9.7/§9.8）不实现，仅记录。

### 执行偏差（诚实记录）

计划假定每个 Task 都先跑出预期的 RED 再实现。实际执行有两处偏离，记录如下以免文档失真：

- **Task 2 Step 2**：`StreamEnd` 因 Task 1 的类型化改动引发的编译错误被**提前实现**，故该步预期的
  "编译失败（`StreamEnd` 不存在）"未单独观察到。改为运行 Step 1 的测试，观察到的是
  **断言失败**——它暴露了真实缺陷（传输失败被 `break false` 后又被 `!received_stop` 分支覆盖，
  分类信息丢失），随后以 `transport_failed` 标志修复。
- **Task 3 Step 2**：`RetryCause` 与 attempt 循环的改造和其测试在**同一批次**完成，测试首次运行即通过，
  未观察到预期的 "编译失败（`cause`/`RetryCause` 不存在）" RED。行为由 Task 1 的集成测试 RED
  与 Task 2 的断言 RED 覆盖，但 Task 3 自身未经历独立的 RED 阶段。
- 另：Task 1 Step 1 的 RED 真实观察到（`no variant named StreamError found for enum ProviderEvent`）。
- 计划设想 Task 1-4 各自单独提交；实际合并为**一个** commit（`feat: retry request timeouts with the shared stall backoff`），因其改动相互依赖（Task 2 的 `StreamEnd` 由 Task 1 的编译错误驱动、Task 3 消费 Task 2 的类型）。

