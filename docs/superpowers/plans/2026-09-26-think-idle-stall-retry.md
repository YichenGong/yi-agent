# THINK 空闲停滞退避重试 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 THINK 阶段的瞬时 provider 停滞带指数退避重试（默认 3 次，2s/4s/8s），并且重试过程对用户可见——耗尽后才终结整轮任务。

**Architecture:** 四层改动。其一在 `provider.rs`，把 idle stall 的合成信号从魔法字符串 `Other("idle timeout")` 换成专用变体 `StopReason::Stalled`。其二在 `agent.rs`，把"acquire lease → call_stream → accumulate"包进 attempt 循环，停滞时退避并重发同一个 `ProviderRequest`，同时发出 `AgentEvent::ProviderRetry { attempt, max, idle_secs }`。其三在各消费端把该事件渲染成用户可见提示（TUI 历史分隔线 / headless stderr / app-server 通知 / desktop 横幅）。其四为文档与进度同步。

**Tech Stack:** Rust edition 2024 / `tokio::select!` / `tracing` / `ratatui`（TUI 测试）/ JSON-RPC 通知（app-server）/ React + vitest（desktop）。

**Spec:** `docs/superpowers/specs/2026-09-26-think-idle-stall-retry-design.md`（v2，重试必须可见）

## Global Constraints

- Rust edition 2024，workspace `rust-version = "1.85"`。
- Rust 工作目录：`yi-agent-rs/`；desktop 工作目录：`desktop/`。
- 分支：`fix/think-idle-stall-retry`（worktree 位于 `.worktrees/fix/think-idle-stall-retry`）。
- 默认退避：`think_stall_retry_limit = 3`，`think_stall_backoff_base = 2s`，即等待 2s / 4s / 8s，单次等待封顶 30s。
- 重试**不得**追加 user 消息、**不得**递增 `turn`、**不得**消耗 `max_turns` 预算。
- 停滞的 partial content **不得**写入 `messages` / `session`；但在所有消费端**必须保留显示**。
- 重试**必须可见**：每个消费端都要渲染提示，文案含进度（`{attempt}/{max}`）。
- 耗尽后的终态 reason 固定为 `"idle timeout after {n} retries"`。
- 既有测试不得回归，特别是 `agent_does_not_report_abnormal_stop_as_end_turn`（用显式 `Stop { reason: Other("idle timeout") }`，语义是 provider 主动上报的异常停止，必须仍走终结路径）。
- 跑测试前先 `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认无残留进程（见 CLAUDE.md）。
- 提交前必须 `cargo fmt --all`（Rust）与 `npx tsc --noEmit`（desktop）；commit message 用 conventional commits，不写 `Co-Authored-By`。

---

## File Structure

| 文件 | 职责 | 本次改动 |
| --- | --- | --- |
| `crates/yi-agent-core/src/provider.rs` | provider 流消费与 stop reason 合成 | 新增 `StopReason::Stalled`；stall 分支改用新变体；新增单测 |
| `crates/yi-agent-core/src/agent.rs` | agent 主循环 | 新增 `AgentEvent::ProviderRetry`、两个 `AgentConfig` 字段、attempt 循环；新增 5 个测试 |
| `crates/yi-agent/src/tui/history.rs` | TUI 事件→历史单元映射 | `ProviderRetry` 插入可见分隔线；新增 2 个测试 |
| `crates/yi-agent/src/tui/app.rs` | TUI 事件路由 | `ProviderRetry` 显式归入 no-op |
| `crates/yi-agent/src/main.rs` | headless drain | `drain_stream_human` 打印 stderr 诊断；新增 1 个测试 |
| `crates/yi-agent-app-server/src/protocol.rs` | app-server 协议类型 | 新增 `Notification::TurnRetry` |
| `crates/yi-agent-app-server/src/translate.rs` | app-server 事件翻译 | `ProviderRetry` → `TurnRetry`；新增 1 个测试 |
| `desktop/src/lib/protocol.ts` | desktop 协议类型 | 新增 `turn/retry` 变体 |
| `desktop/src/lib/session.ts` | desktop 会话状态机 | 记录/清除 `retrying`；新增 2 个测试 |
| `desktop/src/components/ChatView.tsx` | desktop 聊天 UI | 渲染重试横幅 |
| `desktop/src/App.tsx` | desktop 装配 | 传入 `retrying` |
| `crates/yi-agent/src/subagent_runtime.rs` | subagent worker 事件循环 | 映射为 `report_provider_retry()` |
| `docs/bug-list.md`、`docs/project-management/yi-agent-core.md`、`docs/project-management/README.md` | 进度与 bug 清单 | 标记完成 |

---

### Task 1: `StopReason::Stalled` 专用变体

**Files:**
- Modify: `crates/yi-agent-core/src/provider.rs:52-59`（枚举定义）
- Modify: `crates/yi-agent-core/src/provider.rs:88-92`（文档注释）、`:175-186`（stall 分支）
- Modify: `crates/yi-agent-core/src/agent.rs:587-615`（`match stop_reason` 必须补分支才能编译）
- Test: `crates/yi-agent-core/src/provider.rs`（`mod tests` 内，`:246` 之后）

**Interfaces:**
- Consumes: 无（首个任务）。
- Produces: `StopReason::Stalled`（unit 变体，无字段）。Task 2 依赖它作为可重试停滞的唯一判据。

- [ ] **Step 1: 写失败测试**

在 `crates/yi-agent-core/src/provider.rs` 的 `mod tests` 内、`accumulate_stream_eof_without_stop_is_abnormal`（`:366`）之后追加：

```rust
    #[tokio::test]
    async fn accumulate_stream_idle_stall_reports_stalled() {
        // One delta, then the stream never yields again.
        let stream = futures::stream::iter(vec![text_event("partial")])
            .chain(futures::stream::pending())
            .boxed();

        let (content, stop_reason, _) = accumulate_stream(
            stream,
            |_| {},
            Some(std::time::Duration::from_millis(50)),
        )
        .await
        .unwrap();

        // Partial content is still returned so the caller can decide its fate.
        assert_eq!(content, vec![ContentBlock::Text("partial".into())]);
        assert_eq!(stop_reason, StopReason::Stalled);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-core --lib provider::tests::accumulate_stream_idle_stall_reports_stalled`

Expected: 编译失败，报 `no variant named Stalled found for enum StopReason`。

- [ ] **Step 3: 新增变体并改 stall 分支**

在 `crates/yi-agent-core/src/provider.rs:52-59` 的枚举中加入变体：

```rust
/// Why generation stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    /// No provider event arrived within the configured idle timeout. This is
    /// synthesized by `accumulate_stream`, never reported by a provider, and is
    /// the only stop reason the agent loop retries.
    Stalled,
    Other(String),
}
```

把 `provider.rs:182` 的赋值改为：

```rust
        stop_reason = StopReason::Stalled;
```

更新 `provider.rs:89-92` 的文档注释：

```rust
/// If `idle_timeout` is set and no event arrives within that duration, the
/// stream is considered stalled: the loop breaks and whatever was accumulated
/// so far is returned with `StopReason::Stalled`. This prevents the agent from
/// hanging forever when the provider connection goes silent without a proper
/// terminal event.
```

在 `crates/yi-agent-core/src/agent.rs` 的 `match stop_reason` 中，于 `StopReason::EndTurn => {}` 之前加入占位分支（Task 2 会替换）：

```rust
            StopReason::Stalled => {
                tracing::warn!(turn, "think phase stalled (idle timeout)");
                let _ = tx
                    .send(AgentEvent::Done {
                        reason: DoneReason::Interrupted {
                            reason: "idle timeout".into(),
                        },
                    })
                    .await;
                return;
            }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent-core --lib provider::`

Expected: 全部 PASS（`accumulate_stream_eof_without_stop_is_abnormal` 仍断言 `Other("stream ended without stop")`，不受影响）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/provider.rs crates/yi-agent-core/src/agent.rs
git commit -m "feat(core): distinguish synthesized idle stall as StopReason::Stalled"
```

---

### Task 2: THINK attempt 循环与退避重试

**Files:**
- Modify: `crates/yi-agent-core/src/agent.rs:82-99`（`AgentConfig` 字段）
- Modify: `crates/yi-agent-core/src/agent.rs:101-118`（`Default`）
- Modify: `crates/yi-agent-core/src/agent.rs:186-215`（`AgentEvent` 新增变体）
- Modify: `crates/yi-agent-core/src/agent.rs:445` 附近（新增 `stall_backoff_delay`）
- Modify: `crates/yi-agent-core/src/agent.rs:485-615`（attempt 循环替换单次调用）
- Test: `crates/yi-agent-core/src/agent.rs`（`mod tests` 内）

**Interfaces:**
- Consumes: `StopReason::Stalled`（Task 1）。
- Produces:
  - `AgentConfig.think_stall_retry_limit: u16`（默认 `3`）
  - `AgentConfig.think_stall_backoff_base: std::time::Duration`（默认 `2s`）
  - `AgentEvent::ProviderRetry { attempt: u16, max: u16, idle_secs: u64 }`
  - `fn stall_backoff_delay(base: std::time::Duration, attempt: u16) -> std::time::Duration`
  - 终态 reason 字符串 `"idle timeout after {n} retries"`

- [ ] **Step 1: 写失败测试**

在 `crates/yi-agent-core/src/agent.rs` 的 `mod tests` 内，先加两个测试用 provider（放在既有 `StallAfterDeltaProvider`（`:1568`）之后）：

```rust
    /// Emits one delta then stalls on the first call; succeeds on later calls.
    struct StallOnceThenSucceedProvider {
        calls: std::sync::Mutex<u32>,
    }

    impl StallOnceThenSucceedProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for StallOnceThenSucceedProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            let stream = if *calls == 1 {
                futures::stream::iter(vec![ProviderEvent::TextDelta("stale".into())])
                    .chain(futures::stream::pending())
                    .boxed()
            } else {
                futures::stream::iter(vec![
                    ProviderEvent::TextDelta("fresh".into()),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ])
                .boxed()
            };
            Ok(stream)
        }
    }

    /// Every call stalls, so retries are always exhausted.
    struct AlwaysStallProvider {
        calls: std::sync::Mutex<u32>,
    }

    impl AlwaysStallProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for AlwaysStallProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            *self.calls.lock().unwrap() += 1;
            let stream = futures::stream::iter(vec![ProviderEvent::TextDelta("partial".into())])
                .chain(futures::stream::pending())
                .boxed();
            Ok(stream)
        }
    }
```

再加测试本体（放在 `agent_think_stream_stall_emits_terminal_within_timeout`（`:1611`）之后）：

```rust
    fn fast_stall_config() -> AgentConfig {
        AgentConfig {
            think_idle_timeout: Some(std::time::Duration::from_millis(50)),
            think_stall_retry_limit: 3,
            think_stall_backoff_base: std::time::Duration::from_millis(10),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_retries_a_transient_idle_stall_and_completes() {
        let provider = Arc::new(StallOnceThenSucceedProvider::new());
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, fast_stall_config());

        let events = collect_events_async(agent.run("hi".into()).await.unwrap()).await;

        // The retry is announced so the user is not left in silent limbo...
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::ProviderRetry { attempt: 1, max: 3, .. }
            )),
            "expected a ProviderRetry event, got: {events:?}"
        );
        // ...and the turn completes normally instead of reporting an interruption.
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AgentEvent::Done {
                    reason: DoneReason::Interrupted { .. }
                }
            )),
            "a transient stall must not surface as Interrupted"
        );
        // The stalled partial ("stale") is NOT committed: only the retried text is kept.
        let session = agent.session();
        let assistants: Vec<&str> = session
            .messages()
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(assistants, vec!["fresh"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_exhausts_stall_retries_then_reports_interruption() {
        let provider = Arc::new(AlwaysStallProvider::new());
        let calls = provider.clone();
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, fast_stall_config());

        let events = collect_events_async(agent.run("hi".into()).await.unwrap()).await;

        // 1 initial attempt + 3 retries.
        assert_eq!(*calls.calls.lock().unwrap(), 4);
        let retries = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ProviderRetry { .. }))
            .count();
        assert_eq!(retries, 3, "expected 3 retries, got: {events:?}");
        match events.last() {
            Some(AgentEvent::Done {
                reason: DoneReason::Interrupted { reason },
            }) => assert_eq!(reason, "idle timeout after 3 retries"),
            other => panic!("expected Interrupted after retries, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancels_during_stall_backoff() {
        let provider = Arc::new(AlwaysStallProvider::new());
        let tools = Arc::new(ToolRegistry::new());
        // Long backoff so the cancel lands while sleeping, not mid-stream.
        let config = AgentConfig {
            think_idle_timeout: Some(std::time::Duration::from_millis(50)),
            think_stall_retry_limit: 3,
            think_stall_backoff_base: std::time::Duration::from_secs(30),
            ..Default::default()
        };
        let mut agent = Agent::new(provider, tools, config);

        let stream = agent.run("hi".into()).await.unwrap();
        // run() resets the cancel token, so capture it AFTER run() returns.
        let cancel_token = agent.cancel_token();
        let _handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            cancel_token.cancel();
        });

        let events = collect_events_async(stream).await;

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Cancelled)),
            "expected Cancelled, got: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Done { .. })),
            "a cancelled run must not also report Done"
        );
    }

    #[test]
    fn agent_config_defaults_stall_retry_policy() {
        let config = AgentConfig::default();
        assert_eq!(config.think_stall_retry_limit, 3);
        assert_eq!(
            config.think_stall_backoff_base,
            std::time::Duration::from_secs(2)
        );
    }

    #[test]
    fn stall_backoff_delay_doubles_and_caps() {
        let base = std::time::Duration::from_secs(2);
        assert_eq!(stall_backoff_delay(base, 1), std::time::Duration::from_secs(2));
        assert_eq!(stall_backoff_delay(base, 2), std::time::Duration::from_secs(4));
        assert_eq!(stall_backoff_delay(base, 3), std::time::Duration::from_secs(8));
        // Capped at 30s no matter how many retries.
        assert_eq!(
            stall_backoff_delay(base, 10),
            std::time::Duration::from_secs(30)
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-core --lib agent::tests::agent_config_defaults_stall_retry_policy`

Expected: 编译失败，报 `no field think_stall_retry_limit on struct AgentConfig`（以及 `no variant ProviderRetry`、`cannot find function stall_backoff_delay`）。

- [ ] **Step 3: 加配置字段、事件变体与退避函数**

在 `crates/yi-agent-core/src/agent.rs:82-99` 的 `AgentConfig` 末尾（`think_idle_timeout` 之后）追加：

```rust
    /// Max automatic retries when the THINK stream stalls (no provider event
    /// within `think_idle_timeout`). `0` disables stall retries.
    pub think_stall_retry_limit: u16,
    /// Base delay for stall retry backoff. Attempt `n` (1-based) waits
    /// `base * 2^(n-1)`, capped at 30s. Defaults to 2s → 2s/4s/8s.
    pub think_stall_backoff_base: std::time::Duration,
```

在 `Default for AgentConfig`（`:101-118`）中加入：

```rust
            think_stall_retry_limit: 3,
            think_stall_backoff_base: std::time::Duration::from_secs(2),
```

在 `AgentEvent`（`:186-215`）的 `ToolRetry` 之后加入：

```rust
    /// The THINK stream stalled and is being retried internally. `attempt` is
    /// the 1-based retry number, `max` the configured retry limit, and
    /// `idle_secs` how long the stream was silent. Consumers must surface this
    /// to the user: a silent retry leaves the user staring at a frozen screen.
    ProviderRetry {
        attempt: u16,
        max: u16,
        idle_secs: u64,
    },
```

在 `run_loop` 之前（`agent.rs:445` 附近的 `#[allow(clippy::too_many_arguments)]` 之上）加退避计算函数：

```rust
/// Delay before stall retry `attempt` (1-based): `base * 2^(attempt-1)`,
/// capped at 30s to match the runtime retry policy.
fn stall_backoff_delay(base: std::time::Duration, attempt: u16) -> std::time::Duration {
    let shift = u32::from(attempt.saturating_sub(1)).min(5);
    let scaled = base.saturating_mul(1u32 << shift);
    scaled.min(std::time::Duration::from_secs(30))
}
```

- [ ] **Step 4: 用 attempt 循环替换单次 provider 调用**

把 `crates/yi-agent-core/src/agent.rs:485-579`（从 `let provider_turn_lease = match &provider_turn_gate {` 到 `drop(provider_turn_lease);`）整体替换为：

```rust
        // Retry loop for a transiently stalled THINK stream. Each attempt is a
        // fresh provider turn (own lease); a stalled attempt's partial content
        // is not committed to the session.
        let mut stall_retries: u16 = 0;
        let (content, stop_reason, last_usage) = loop {
            let provider_turn_lease = match &provider_turn_gate {
                Some(gate) => match tokio::select! {
                    lease = gate.acquire() => lease,
                    _ = cancel_token.cancelled() => {
                        let _ = tx.send(AgentEvent::Cancelled).await;
                        return;
                    }
                } {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        let _ = tx
                            .send(AgentEvent::Error(AgentError::ProviderTurnAdmission(error)))
                            .await;
                        return;
                    }
                },
                None => None,
            };

            let stream = match provider.call_stream(req.clone()).await {
                Ok(s) => {
                    tracing::info!(turn, "provider call_stream returned Ok, entering accumulate");
                    s
                }
                Err(e) => {
                    warn!(turn, error = %e, "provider call failed");
                    if tx
                        .send(AgentEvent::Error(AgentError::Provider(e)))
                        .await
                        .is_err()
                    {
                        return; // Receiver dropped, stop the loop
                    }
                    return;
                }
            };

            // Check 2: THINK 中 — select! between accumulate and cancel
            let attempt = tokio::select! {
                result = accumulate_provider_stream(stream, &tx, &model, config.think_idle_timeout) => match result {
                    Ok(v) => {
                        tracing::info!(turn, stop_reason = ?v.1, content_blocks = v.0.len(), "accumulate returned Ok");
                        v
                    }
                    Err(e) => {
                        warn!(turn, error = %e, "provider stream error");
                        if tx.send(AgentEvent::Error(e)).await.is_err() {
                            return;
                        }
                        return;
                    }
                },
                _ = cancel_token.cancelled() => {
                    info!(turn, "agent loop cancelled during think");
                    // THINK 阶段 cancel: session 里只有 user 消息(无 assistant
                    // 回复),truncate 到 session_len 保留 user 消息(可接受,
                    // Anthropic 允许 user 无 assistant 回复)。
                    session.lock().unwrap().truncate(session_len);
                    let _ = tx.send(AgentEvent::Cancelled).await;
                    return;
                }
            };

            // Release the turn lease before sleeping: a retry backoff must not
            // hold provider capacity.
            drop(provider_turn_lease);

            let stalled = matches!(attempt.1, StopReason::Stalled);
            if stalled && stall_retries < config.think_stall_retry_limit {
                stall_retries += 1;
                let delay = stall_backoff_delay(config.think_stall_backoff_base, stall_retries);
                let idle_secs = config
                    .think_idle_timeout
                    .map(|t| t.as_secs())
                    .unwrap_or(0);
                tracing::warn!(
                    turn,
                    attempt = stall_retries,
                    max = config.think_stall_retry_limit,
                    delay_ms = delay.as_millis() as u64,
                    "think phase stalled; retrying after backoff"
                );
                // Surface the retry: a silent backoff looks like a hang.
                if tx
                    .send(AgentEvent::ProviderRetry {
                        attempt: stall_retries,
                        max: config.think_stall_retry_limit,
                        idle_secs,
                    })
                    .await
                    .is_err()
                {
                    return; // Receiver dropped, stop the loop
                }
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel_token.cancelled() => {
                        info!(turn, "agent loop cancelled during stall backoff");
                        session.lock().unwrap().truncate(session_len);
                        let _ = tx.send(AgentEvent::Cancelled).await;
                        return;
                    }
                }
                continue;
            }
            break attempt;
        };
```

保留其后的 `if let Some(usage) = last_usage { ... }`（`:551-556`）、`debug!(turn, content = ?content, ...)`（`:558-559`）以及 `StopReason::Other` 的 `"stream error: "` 分支（`:563-577`）不变。

把 Task 1 加入的 `StopReason::Stalled` 占位分支替换为耗尽路径：

```rust
            StopReason::Stalled => {
                let _ = tx
                    .send(AgentEvent::Done {
                        reason: DoneReason::Interrupted {
                            reason: format!("idle timeout after {stall_retries} retries"),
                        },
                    })
                    .await;
                return;
            }
```

- [ ] **Step 5: 逐个运行新测试确认通过**

Run: `cargo test -p yi-agent-core --lib agent::tests::agent_retries_a_transient_idle_stall_and_completes`

Run: `cargo test -p yi-agent-core --lib agent::tests::agent_exhausts_stall_retries_then_reports_interruption`

Run: `cargo test -p yi-agent-core --lib agent::tests::agent_cancels_during_stall_backoff`

Run: `cargo test -p yi-agent-core --lib agent::tests::agent_config_defaults_stall_retry_policy`

Run: `cargo test -p yi-agent-core --lib agent::tests::stall_backoff_delay_doubles_and_caps`

Expected: 全部 PASS。（cargo 一次只接受一个过滤串，故逐条运行。）

- [ ] **Step 6: 跑整个 core lib 测试确认无回归**

Run: `cargo test -p yi-agent-core --lib`

Expected: 全部 PASS。特别确认 `agent_does_not_report_abnormal_stop_as_end_turn` 仍通过（它走 `Other("idle timeout")`，是 provider 主动上报，不触发重试）。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "feat(core): retry a transiently stalled THINK stream with backoff"
```

---

### Task 3: TUI 可见提示

**Files:**
- Modify: `crates/yi-agent/src/tui/history.rs:361-518`（`push_event`）
- Modify: `crates/yi-agent/src/tui/app.rs:761`（`route_event`）
- Test: `crates/yi-agent/src/tui/history.rs`（`mod tests` 内）

**Interfaces:**
- Consumes: `AgentEvent::ProviderRetry { attempt, max, idle_secs }`（Task 2）。
- Produces: 无新公开 API。`HistoryState::push_event` 在收到 `ProviderRetry` 时 push 一个 `HistoryCell::Separator`。

- [ ] **Step 1: 写失败测试**

在 `crates/yi-agent/src/tui/history.rs` 的 `mod tests` 内追加：

```rust
    #[test]
    fn push_event_provider_retry_shows_visible_separator() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("partial text".into()), 80);

        s.push_event(
            AgentEvent::ProviderRetry {
                attempt: 1,
                max: 3,
                idle_secs: 60,
            },
            80,
        );

        // The retry must be visible, not silent.
        assert_eq!(s.cells.len(), 2, "partial + retry separator");
        assert!(matches!(
            &s.cells[1],
            HistoryCell::Separator { label: Some(label) }
                if label.contains("retrying 1/3") && label.contains("60s")
        ));
        // The already-streamed partial is preserved for the user.
        assert!(matches!(
            &s.cells[0],
            HistoryCell::AssistantMessage { markdown } if markdown == "partial text"
        ));
    }

    #[test]
    fn push_event_provider_retry_keeps_history_before_it() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("done earlier".into()), 80);
        s.push_event(
            AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            80,
        );
        let before = s.cells.len();

        s.push_event(
            AgentEvent::ProviderRetry {
                attempt: 2,
                max: 3,
                idle_secs: 60,
            },
            80,
        );

        assert_eq!(s.cells.len(), before + 1, "only the separator is appended");
        assert!(matches!(s.cells[0], HistoryCell::AssistantMessage { .. }));
        assert!(matches!(s.cells[1], HistoryCell::ToolCall { .. }));
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent --lib tui::history::tests::push_event_provider_retry_shows_visible_separator`

Expected: 编译失败，报 `no variant named ProviderRetry found for enum AgentEvent`（若 core 改动未在同一 worktree，先确认 Task 2 已提交）。

- [ ] **Step 3: 实现可见分隔线**

在 `crates/yi-agent/src/tui/history.rs` 的 `push_event` 中，于 `AgentEvent::Start => {}`（`:366`）之后加入：

```rust
            AgentEvent::ProviderRetry {
                attempt,
                max,
                idle_secs,
            } => {
                // Make the stall retry visible: the user should know the stream
                // went quiet and that yi-agent is retrying, rather than watching
                // a frozen screen during the backoff. The partial text that was
                // already streamed stays on screen above this line.
                self.cells.push(HistoryCell::Separator {
                    label: Some(format!(
                        "Provider stalled (no output for {idle_secs}s) — retrying {attempt}/{max}"
                    )),
                });
            }
```

在 `crates/yi-agent/src/tui/app.rs` 的 `route_event` 中，于 `AgentEvent::ToolRetry { .. } => {}`（`:761`）之后加入：

```rust
        // The retry is surfaced through a history separator; the status bar
        // needs no extra state (which would raise "when do we clear it?").
        AgentEvent::ProviderRetry { .. } => {}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent --lib tui::history::tests::push_event_provider_retry`

Expected: 两个测试都 PASS。

- [ ] **Step 5: 跑 TUI 测试确认无回归**

Run: `cargo test -p yi-agent --lib tui::`

Expected: 全部 PASS。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/history.rs crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): show a visible notice when the provider stalls and retries"
```

---

### Task 4: headless 与 subagent 消费端

**Files:**
- Modify: `crates/yi-agent/src/main.rs:893-945`（`drain_stream_human`）
- Modify: `crates/yi-agent/src/subagent_runtime.rs:573`（`ToolRetry` 旁）
- Test: `crates/yi-agent/src/main.rs`（`mod tests` 内）

**Interfaces:**
- Consumes: `AgentEvent::ProviderRetry { attempt, max, idle_secs }`（Task 2）。
- Produces: 无新 API。`drain_stream_human` 向 stderr 打印 `[provider-retry:{attempt}/{max}]`。

- [ ] **Step 1: 写失败测试**

在 `crates/yi-agent/src/main.rs` 的 `mod tests` 内、`drain_stream_human_suppresses_done_endturn_on_stderr`（`:1659`）之后追加：

```rust
    #[test]
    fn drain_stream_human_reports_provider_retry_on_stderr() {
        let stream = futures::stream::iter(vec![
            AgentEvent::AssistantText("partial".into()),
            AgentEvent::ProviderRetry {
                attempt: 1,
                max: 3,
                idle_secs: 60,
            },
            AgentEvent::Done {
                reason: yi_agent_core::DoneReason::EndTurn,
            },
        ])
        .boxed();
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();

        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 0);
        let err_text = String::from_utf8(err).unwrap();
        assert!(
            err_text.contains("[provider-retry:1/3]"),
            "the retry must be announced on stderr, got: {err_text}"
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "partial\n",
            "stdout must carry only assistant text"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent --bin yi-agent drain_stream_human_reports_provider_retry_on_stderr`

Expected: 编译失败，报 `no variant named ProviderRetry found for enum AgentEvent`。

- [ ] **Step 3: 实现两个消费端**

`crates/yi-agent/src/main.rs`：在 `drain_stream_human` 的 `AgentEvent::ToolRetry { id }`（`:909-911`）之后加入：

```rust
            yi_agent_core::AgentEvent::ProviderRetry { attempt, max, .. } => {
                let _ = writeln!(err, "[provider-retry:{attempt}/{max}]");
            }
```

`crates/yi-agent/src/subagent_runtime.rs`：在 `Some(AgentEvent::ToolRetry { .. }) => { reporter.report_tool_retry(); }`（`:573-575`）之后加入：

```rust
                                        Some(AgentEvent::ProviderRetry { .. }) => {
                                            reporter.report_provider_retry();
                                        }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent --bin yi-agent drain_stream`

Expected: 全部 PASS，含新测试。

- [ ] **Step 5: 跑相关测试确认无回归**

Run: `cargo test -p yi-agent --bin yi-agent subagent_runtime`

Expected: 全部 PASS。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/main.rs crates/yi-agent/src/subagent_runtime.rs
git commit -m "feat: announce provider stall retries in headless and subagent modes"
```

---

### Task 5: app-server 通知与 desktop 提示

**Files:**
- Modify: `crates/yi-agent-app-server/src/protocol.rs:104-143`（`Notification`）
- Modify: `crates/yi-agent-app-server/src/translate.rs:263-270`（忽略分支改为发通知）
- Test: `crates/yi-agent-app-server/src/translate.rs`（`mod tests` 内）
- Modify: `desktop/src/lib/protocol.ts:41-54`（`Notification` 联合类型）
- Modify: `desktop/src/lib/session.ts:25-70`（`apply`）
- Modify: `desktop/src/components/ChatView.tsx:1-60`
- Modify: `desktop/src/App.tsx:104`
- Test: `desktop/src/lib/session.test.ts`

**Interfaces:**
- Consumes: `AgentEvent::ProviderRetry { attempt, max, idle_secs }`（Task 2）。
- Produces:
  - `Notification::TurnRetry { thread_id: String, turn_id: String, attempt: u16, max: u16 }`（wire method `turn/retry`）
  - `Session.retrying: { attempt: number; max: number } | null`

- [ ] **Step 1: 写失败测试（Rust 侧）**

在 `crates/yi-agent-app-server/src/translate.rs` 的 `mod tests` 内追加（该文件已有
`fn translator() -> Translator` helper，直接复用）：

```rust
    #[test]
    fn provider_retry_becomes_turn_retry_notification() {
        let mut t = translator();
        let out = t.on_event(AgentEvent::ProviderRetry {
            attempt: 1,
            max: 3,
            idle_secs: 60,
        });
        assert!(
            matches!(
                out.as_slice(),
                [Notification::TurnRetry {
                    attempt: 1,
                    max: 3,
                    ..
                }]
            ),
            "expected TurnRetry, got: {out:?}"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-app-server --lib provider_retry_becomes_turn_retry_notification`

Expected: 编译失败，报 `no variant named TurnRetry found for enum Notification`。

- [ ] **Step 3: 加通知变体与翻译**

`crates/yi-agent-app-server/src/protocol.rs`：在 `Notification`（`:104-143`）的 `TurnCompleted` 之后加入：

```rust
    #[serde(rename = "turn/retry")]
    TurnRetry {
        thread_id: String,
        turn_id: String,
        attempt: u16,
        max: u16,
    },
```

`crates/yi-agent-app-server/src/translate.rs`：把 `AgentEvent::ProviderRetry` 从忽略分支中移出，改为独立分支（放在 `AgentEvent::Error` 分支之后、忽略分支之前）：

```rust
            AgentEvent::ProviderRetry { attempt, max, .. } => {
                // The desktop client shows this so a stalled stream does not
                // look like a hang during the backoff window.
                out.push(Notification::TurnRetry {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    attempt,
                    max,
                });
            }
```

并从忽略分支（`:263-270`）中确认没有重复列出 `ProviderRetry`。

- [ ] **Step 4: 运行 Rust 侧测试确认通过**

Run: `cargo test -p yi-agent-app-server --lib`

Expected: 全部 PASS。

- [ ] **Step 5: 写失败测试（desktop 侧）**

在 `desktop/src/lib/session.test.ts` 末尾追加：

```ts
  it("records a retry notice and clears it when text resumes", () => {
    const s = new Session();
    s.apply({
      method: "turn/retry",
      params: { thread_id: "t", turn_id: "u1", attempt: 1, max: 3 },
    });
    expect(s.retrying).toEqual({ attempt: 1, max: 3 });

    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "hi" } });
    expect(s.retrying).toBeNull();
  });

  it("clears a retry notice when the turn completes", () => {
    const s = new Session();
    s.apply({
      method: "turn/retry",
      params: { thread_id: "t", turn_id: "u1", attempt: 2, max: 3 },
    });
    s.apply({
      method: "turn/completed",
      params: { thread_id: "t", turn_id: "u1", status: "completed" },
    });
    expect(s.retrying).toBeNull();
  });
```

- [ ] **Step 6: 运行测试确认失败**

Run: `cd desktop && npm test`

Expected: FAIL —— `s.retrying` 为 `undefined`，且 `turn/retry` 不是已知 method（TS 类型报错）。

- [ ] **Step 7: 实现 desktop 侧**

`desktop/src/lib/protocol.ts`：在 `Notification` 联合类型中加入：

```ts
  | {
      method: "turn/retry";
      params: { thread_id: string; turn_id: string; attempt: number; max: number };
    }
```

`desktop/src/lib/session.ts`：在类字段区（`usage` 之后）加：

```ts
  retrying: { attempt: number; max: number } | null = null;
```

在 `apply` 的 `case "item/delta"` 分支开头（累积文本前）清除提示：

```ts
      case "item/delta": {
        // Text resumed: the retry succeeded, so the notice has served its purpose.
        this.retrying = null;
        const { item_id, delta } = notification.params;
```

在 `case "turn/completed"` 分支中加入清除：

```ts
      case "turn/completed":
        this.turnActive = false;
        this.retrying = null;
        this.lastStatus = notification.params.status;
        this.lastError = notification.params.error ?? null;
        break;
```

在 `case "turn/started"` 之后加入新分支：

```ts
      case "turn/retry":
        this.retrying = { attempt: notification.params.attempt, max: notification.params.max };
        break;
```

`desktop/src/components/ChatView.tsx`：把签名改为接收 `retrying` 并在错误横幅之前渲染：

```tsx
export function ChatView({
  items,
  error,
  retrying,
}: {
  items: Item[];
  error?: string | null;
  retrying?: { attempt: number; max: number } | null;
}) {
```

并在 `{error && (` 之前插入：

```tsx
      {retrying && (
        <div className="my-2 rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm text-amber-300">
          Provider stalled — retrying {retrying.attempt}/{retrying.max}…
        </div>
      )}
```

`desktop/src/App.tsx`：把 `<ChatView items={session.items} error={session.lastError} />` 改为：

```tsx
        <ChatView
          items={session.items}
          error={session.lastError}
          retrying={session.retrying}
        />
```

- [ ] **Step 8: 运行 desktop 测试与类型检查**

Run: `cd desktop && npm test`

Run: `cd desktop && npx tsc --noEmit`

Expected: 全部 PASS，无类型错误。

- [ ] **Step 9: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/protocol.rs crates/yi-agent-app-server/src/translate.rs
git commit -m "feat(app-server): emit turn/retry when the provider stalls"

cd ../desktop
git add src/lib/protocol.ts src/lib/session.ts src/lib/session.test.ts src/components/ChatView.tsx src/App.tsx
git commit -m "feat(desktop): show a retry notice while the provider stalls"
```

---

### Task 6: 文档与进度同步

**Files:**
- Modify: `docs/bug-list.md`
- Modify: `docs/project-management/yi-agent-core.md`
- Modify: `docs/project-management/README.md`

**Interfaces:**
- Consumes: 前五个任务的实现与测试命令。
- Produces: 无代码接口。

- [ ] **Step 1: 更新 bug-list**

把 `docs/bug-list.md` 中这一行：

```
- [ ] agent.rs:608  的  StopReason::Other("idle timeout")  分支应该带退避重试（比如 3次、2s/4s/8s），而不是  return 。这样瞬时 stall 对用户就不可见了。
```

改为：

```
- [x] THINK 阶段瞬时 stall 直接终结整轮任务（修复：停滞信号改为专用 `StopReason::Stalled`，`agent.rs` attempt 循环带 2s/4s/8s 退避重试 3 次并新增 `AgentEvent::ProviderRetry { attempt, max, idle_secs }`；重试对用户可见——TUI 历史分隔线、headless stderr `[provider-retry:n/max]`、app-server `turn/retry` → desktop 横幅；停滞 partial 保留显示但不写入 session。验证：`cargo test -p yi-agent-core --lib agent::tests::agent_retries_a_transient_idle_stall_and_completes`、`cargo test -p yi-agent --lib tui::history::tests::push_event_provider_retry_shows_visible_separator`、`cargo test -p yi-agent --bin yi-agent drain_stream_human_reports_provider_retry_on_stderr`、`cargo test -p yi-agent-app-server --lib provider_retry_becomes_turn_retry_notification`、`cd desktop && npm test`。见 [设计](../superpowers/specs/2026-09-26-think-idle-stall-retry-design.md)）
```

- [ ] **Step 2: 更新 core 模块进度**

在 `docs/project-management/yi-agent-core.md` 的 Features 列表末尾（插件系统那条之前）加入：

```
- [x] THINK 阶段空闲停滞退避重试 — `provider.rs::StopReason::Stalled` + `agent.rs` attempt 循环（默认 3 次、2s/4s/8s，封顶 30s）；`AgentConfig.think_stall_retry_limit` / `think_stall_backoff_base` 可调；停滞 partial 不写入 session；`AgentEvent::ProviderRetry` 使重试对用户可见；验证：`cargo test -p yi-agent-core --lib agent::tests::agent_exhausts_stall_retries_then_reports_interruption` — [设计](../superpowers/specs/2026-09-26-think-idle-stall-retry-design.md)
```

- [ ] **Step 3: 同步 README 计数**

在 `docs/project-management/README.md` 中把 yi-agent-core 行由 `15 / 16` 改为：

```
| yi-agent-core | 16 / 17 | [详情](./yi-agent-core.md) |
```

- [ ] **Step 4: 校验文档无残留未完成标记**

Run: `grep -n "idle timeout" docs/bug-list.md`

Expected: 只剩已修复的 `[x]` 行，无 `[ ]` 行。

- [ ] **Step 5: 提交**

```bash
git add docs/bug-list.md docs/project-management/yi-agent-core.md docs/project-management/README.md
git commit -m "docs: record THINK idle-stall retry fix"
```

---

## 收尾验证（全部任务完成后执行）

Run: `cd yi-agent-rs && cargo fmt --all -- --check`

Run: `cargo test -p yi-agent-core --lib`

Run: `cargo test -p yi-agent --lib tui::`

Run: `cargo test -p yi-agent --bin yi-agent drain_stream`

Run: `cargo test -p yi-agent-app-server --lib`

Run: `cd desktop && npm test && npx tsc --noEmit`

Expected: 全部 PASS。任何失败都不得声称完成。

---

## Self-Review

**1. Spec coverage**

| Spec 章节 | 覆盖任务 |
| --- | --- |
| §2.1 专用变体 | Task 1 |
| §2.2 attempt 循环 | Task 2 Step 4 |
| §2.3 退避参数 | Task 2 Step 3 |
| §2.4 取消语义 | Task 2 Step 4（`select!` on cancel）+ `agent_cancels_during_stall_backoff` |
| §2.5 耗尽终态 | Task 2 Step 4 |
| §2.6 可见性（v2 核心） | Task 3（TUI）、Task 4（headless/subagent）、Task 5（app-server/desktop） |
| §2.7 partial 处理（v2 修正） | Task 2（不提交）+ Task 3 Step 3（保留显示） |
| §2.8 已知限制 | Task 6（文档记录） |
| §4 消费端表 | Task 3 / 4 / 5 |
| §7 测试策略 | Task 1/2/3/4/5 各 Step 1 |

**2. Placeholder scan**：无 TBD / TODO；每个代码步骤都给了完整代码，且已核对现场签名（`Translator::new(thread_id)` 单参数 + `mod tests` 内 `translator()` helper、`drain_stream_human_sync`、`Message { role, content }`、`HistoryCell::Separator`）。

**3. Type consistency**：`think_stall_retry_limit: u16`、`think_stall_backoff_base: Duration`、`AgentEvent::ProviderRetry { attempt: u16, max: u16, idle_secs: u64 }`、`stall_backoff_delay(base: Duration, attempt: u16) -> Duration`、`Notification::TurnRetry { thread_id, turn_id, attempt: u16, max: u16 }`、`Session.retrying: { attempt: number; max: number } | null` 在定义处与使用处同名同型。
