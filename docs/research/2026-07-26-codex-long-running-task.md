# Codex 长任务持续推进机制调研

> 调研对象：`/Users/gongyichen/Documents/TechnicalStuff/projects/OpenSource/codex`
> 调研目标：理解 codex 如何尽可能保证一个任务长时间、持续地往下执行
> 日期：2026-07-26

## 目录

1. [上下文管理：自动压缩](#1-上下文管理自动压缩)
2. [多层 Token 预算系统](#2-多层-token-预算系统)
3. [错误分类：可恢复-vs-致命](#3-错误分类可恢复-vs-致命)
4. [重试与降级策略](#4-重试与降级策略)
5. [Agent 主循环：模型驱动的续行](#5-agent-主循环模型驱动的续行)
6. [中断与恢复](#6-中断与恢复)
7. [工具执行健壮性](#7-工具执行健壮性)
8. [任务分解：子-agent-委派](#8-任务分解子-agent-委派)
9. [设计思想提炼](#9-设计思想提炼)
10. [关键配置项速查表](#10-关键配置项速查表)
11. [系统提示词：鼓励持续推进的指令](#11-系统提示词鼓励持续推进的指令)

---

## 1. 上下文管理：自动压缩

核心思路：**接近 token 上限时自动摘要历史**，而非丢失上下文。

### 1.1 Pre-sampling 压缩

每轮采样前检查 token 使用量，若超过阈值则先压缩历史。

- **代码位置**：`codex-rs/core/src/session/turn.rs:158` — `run_pre_sampling_compact()`
- **触发条件**：token 使用量超过 `model_auto_compact_token_limit`（默认 80% 上下文窗口）

### 1.2 Mid-turn 压缩

采样后若 `should_roll_over` 为真，触发压缩后 `continue` 继续循环。

- **代码位置**：`codex-rs/core/src/session/turn.rs:354-375`
- **触发条件**：`should_roll_over = new_context_window_request || token_limit_reached`
- **调用**：`run_auto_compact(CompactionReason::ContextLimit, CompactionPhase::MidTurn)`

### 1.3 压缩实现

- `codex-rs/core/src/compact.rs:92-122` — `run_inline_auto_compact_task()` 构造摘要 prompt
- `codex-rs/core/src/compact.rs:221-321` — `run_compact_task_inner_impl()` 调 LLM 生成摘要，替换历史尾部
- 流程：选取历史尾部约 20K tokens → 发给模型生成摘要 → 用摘要替换被压缩部分 → 循环 `continue`

### 1.4 Context Window Token 预算计算

- **代码位置**：`codex-rs/core/src/session/context_window.rs:23-91` — `context_window_token_status()`
- **作用**：计算 token 使用量相对于上下文窗口的比例
- **两种 scope**：
  - `AutoCompactTokenLimitScope::Total`：完整上下文
  - `AutoCompactTokenLimitScope::BodyAfterPrefix`：前缀之后新增的 token
- 计算 `base_window_tokens_remaining` = min(剩余至 auto_compact 阈值, 剩余至完整窗口)
- 配置 fallback buffer 时额外加 `auto_compact_fallback_buffer_tokens`

### 1.5 Token 预算提醒系统

- **代码位置**：`codex-rs/core/src/session/token_budget.rs:1-61` — `maybe_record()`
- **配置结构**：`codex-rs/core/src/config/mod.rs:1112-1128` — `TokenBudgetConfig`
- **作用**：剩余 token 低于阈值时注入提醒消息，让模型感知预算紧张
- **配置项**：
  - `features.token_budget.reminder_threshold_tokens`：提醒阈值
  - `features.token_budget.reminder_message_template`：提醒模板
  - `features.token_budget.auto_compact_fallback_prompt`：预算为 0 时注入的 prompt
  - `features.token_budget.auto_compact_fallback_buffer_tokens`：强制压缩前的 buffer
  - `features.token_budget.guidance_message`：预算限制的指导消息

---

## 2. 多层 Token 预算系统

不同粒度的预算叠加，各司其职。

| 层级 | 作用域 | 文件 | 说明 |
|------|--------|------|------|
| Context window | 单次推理请求 | `context_window.rs:23-91` | 模型一次能接收的最大 token |
| Auto-compact limit | 触发压缩的阈值 | `compact.rs` | 默认 80% 窗口 |
| **Rollout budget** | **整个 session** | `rollout_budget.rs:44-52` | session 级总预算 |

### 2.1 Rollout Budget（Session 级预算）

- **代码位置**：`codex-rs/core/src/rollout_budget.rs:1-100`
- **结构**：`RolloutBudget` 带可配置 token 上限，支持加权计数
- **提醒**：`codex-rs/core/src/session/rollout_budget.rs:8-23` — `maybe_record_reminder()` 在剩余 token 低于阈值时注入提醒
- **耗尽处理**：`codex-rs/core/src/session/rollout_budget.rs:26-36` — `record_rollout_budget_usage()` emit `SessionBudgetExceeded` 错误（不可重试）
- **配置项**：
  - `features.rollout_budget.limit_tokens`：session 总加权 token 预算（启用时必填）
  - `features.rollout_budget.reminder_at_remaining_tokens`：提醒触发阈值（必填）
  - `features.rollout_budget.sampling_token_weight`：输出 token 权重
  - `features.rollout_budget.prefill_token_weight`：输入 token 权重

---

## 3. 错误分类：可恢复 vs 致命

`is_retryable()` 是所有重试/降级决策的入口。

- **代码位置**：`codex-rs/protocol/src/error.rs:176-213`

### 致命错误（不重试）

- `TurnAborted`、`SessionBudgetExceeded`、`Interrupted`
- `UsageNotIncluded`、`QuotaExceeded`
- `InvalidImageRequest`、`InvalidRequest`、`RefreshTokenFailed`
- `UnsupportedOperation`、`Sandbox` 错误
- `ContextWindowExceeded`
- `ThreadNotFound`、`AgentLimitReached`
- `Spawn`、`SessionConfiguredNotFirstEvent`
- `ServerOverloaded`、`CyberPolicy`

### 可重试错误

- `Stream`（连接断开）
- `Timeout`、`RequestTimeout`
- `UnexpectedStatus`、`ResponseStreamFailed`
- `ConnectionFailed`、`InternalServerError`
- `InternalAgentDied`
- `IO`、`JSON`、`TokioJoin` 错误

---

## 4. 重试与降级策略

### 4.1 指数退避重试

- **代码位置**：`codex-rs/core/src/responses_retry.rs:22-79` — `handle_retryable_response_stream_error()`
- **重试包装**：`codex-rs/core/src/session/turn.rs:1123-1217` — `run_sampling_request()`
- **退避函数**：`codex-rs/core/src/util.rs:85-90` — `backoff()`
- **公式**：`200ms * 2^(attempt-1)` + 10% jitter
- **最大重试次数**：由 provider 的 `stream_max_retries()` 决定

### 4.2 传输层降级

- **代码位置**：`codex-rs/core/src/responses_retry.rs:31-46`
- **机制**：重试耗尽时 WebSocket → HTTPS 自动切换，重试计数器重置为 0
- **条件**：`try_switch_fallback_transport` 可用时

### 4.3 图片消毒重试

- **代码位置**：`codex-rs/core/src/session/turn.rs:434-455`
- **机制**：`InvalidImageRequest` 错误时，从历史中剔除无效图片，然后重试本轮

### 4.4 Usage Limit 处理

- **代码位置**：`codex-rs/core/src/session/turn.rs:1188-1193`
- **机制**：`UsageLimitReached` 错误通过 `update_rate_limits()` 传播限流信息

### 4.5 非致命错误不杀 session

- **代码位置**：`codex-rs/core/src/session/turn.rs:457-467`
- **机制**：其他错误 emit 给 UI 后 `break` 循环，**让用户能继续对话**（而非终止整个 session）

---

## 5. Agent 主循环：模型驱动的续行

### 5.1 主循环结构

**代码位置**：`codex-rs/core/src/session/turn.rs:227-469` — `run_turn()`

```python
# 伪代码
async def run_turn(session, turn_context, input, cancellation_token):
    # Step 1: 采样前压缩检查
    await run_pre_sampling_compact(session, turn_context)

    # Step 2: 构建上下文（skills, plugins, hooks）
    world_state = await build_context()

    # Step 3: 记录输入

    # Step 4: 主循环
    while True:
        # 4a. 排空执行中排队进来的用户输入
        pending = drain_pending_input()
        if blocked: break

        # 4b. Rollout budget 提醒
        maybe_record_reminder()

        # 4c. 采样（带重试）
        result = await run_sampling_request(...)

        # 4d. 处理结果
        match result:
            Ok((output, input)):
                if output.needs_follow_up:
                    accept_mailbox_delivery()
                    # 检查 token 限制
                    if needs_follow_up AND (new_context_request OR token_limit_reached):
                        await run_auto_compact()
                        continue  # 压缩后继续
                    if not needs_follow_up:
                        run_stop_hooks()
                        break  # 本轮完成
                    continue  # 模型还要继续工具调用

            Err(TurnAborted): return Err
            Err(InvalidImageRequest): sanitize_and_retry
            Err(other): emit_error_and_break  # 让用户继续
```

### 5.2 续行 vs 停止的决策

**继续条件**（不 break）：
1. `needs_follow_up` 为真：模型表示还要继续工具调用
2. `has_pending_input`：队列中有待处理的用户输入
3. `should_roll_over = needs_follow_up && (new_context_request || token_limit_reached)`：压缩后继续

**停止条件**（break）：
1. `!needs_follow_up`：模型给出最终输出且 stop hooks 说停
2. 非致命错误：emit 给客户端后 break，用户可继续
3. `TurnAborted`：传播给调用方

### 5.3 关键设计点

- **没有硬编码最大轮次**：循环持续条件 = `needs_follow_up` OR `has_pending_input`
- **模型主导续行**：是否继续由模型的 `needs_follow_up` 信号决定
- **Rollout budget 间接限制**：通过 session 级 token 预算限制总执行量

### 5.4 多 Agent 并发限制

- **代码位置**：`codex-rs/core/src/agent/control/execution.rs:14-116` — `AgentExecutionLimiter`
- 仅适用于 V2 子 agent（`MultiAgentVersion::V2` 和 `SessionSource::SubAgent()`）
- 通过 `AgentControl::with_session_id(session_id, max_threads)` 传递

---

## 6. 中断与恢复

### 6.1 用户中断

- **入口**：`codex-rs/core/src/session/handlers.rs:722-725` — `Op::Interrupt` 处理
- **执行**：`codex-rs/core/src/session/mod.rs:3905-3912` — `interrupt_task()` 调 `abort_all_tasks(TurnAbortReason::Interrupted)`
- **abort 实现**：`codex-rs/core/src/tasks/mod.rs:497-525` — `abort_all_tasks()`
  - 取消 cancellation token
  - emit `TurnAborted` 事件
  - 清空 pending input 队列

### 6.2 中断后自动重启

- **代码位置**：`tasks/mod.rs:523` — `maybe_start_turn_for_pending_work()`
- **机制**：中断后检查 mailbox，若有 `trigger_turn` 标记的消息，自动开新一轮

### 6.3 Session 持久化与恢复

- **持久化**：`RolloutRecorder` 自动落盘到 `codex_home/sessions/`
- **恢复 API**：
  - `codex-rs/core/src/thread_manager.rs:778-845` — `resume_thread_from_rollout()` 和 `resume_thread_with_history()`
  - 从 rollout 路径重建 `initial_history`
  - `resume_thread_with_history()` 用持久化的对话历史初始化新 session
- **测试支持**：`resume_thread_from_rollout_with_user_shell_override_for_tests()`

### 6.4 回滚与截断

- **代码位置**：`codex-rs/core/src/thread_rollout_truncation.rs:1-80`
- **机制**：
  - `user_message_positions_in_rollout()` 和 `fork_turn_positions_in_rollout()` 处理回滚标记
  - `ThreadRolledBack` 事件标记允许移除最近 N 轮历史

### 6.5 Agent 图持久化

- **代码位置**：`codex-rs/core/src/agent/control.rs:672-701` — `persist_thread_spawn_edge_for_source()`
- 通过 `AgentGraphStore` 持久化子 agent 关系

---

## 7. 工具执行健壮性

### 7.1 工具编排器

- **代码位置**：`codex-rs/core/src/tools/orchestrator.rs:136-512` — `ToolOrchestrator::run()`
- **执行流程**：
  1. **审批检查**：确定是否需要审批（`Skip` / `Forbidden` / `NeedsApproval`）。启用 strict auto-review 时，即使 `Skip` 也要走 guardian review
  2. **沙箱选择**：`sandbox_override_for_first_attempt()` 根据工具权限和沙箱策略决定初始沙箱类型
  3. **首次尝试**：`run_attempt()` 在初始沙箱内执行
  4. **拒绝时升级**：若返回 `SandboxErr::Denied` 且 `escalate_on_failure()` 为真，关闭沙箱重试（`sandbox = None`）
  5. **网络审批**：`ActiveNetworkApproval` 支持 immediate 或 deferred 模式

### 7.2 沙箱升级策略

- **代码位置**：`codex-rs/core/src/tools/orchestrator.rs:293-511`
- **机制**：
  1. 首次在沙箱内执行
  2. 若被 `Denied` 且允许升级，关闭沙箱重试
  3. 升级成功 emit `"escalated"` 遥测；失败则返回错误

### 7.3 工具运行时错误处理

- 工具错误冒泡到 `run_sampling_request`，按 `is_retryable()` 分类
- 致命工具错误通过外层循环作为 unexpected error surfacing，break 本轮但用户可继续

### 7.4 并行工具执行

- **代码位置**：`codex-rs/core/src/tools/parallel.rs`
- **机制**：`ToolCallRuntime` + `dispatch_handle` 管理并发，`FuturesOrdered` 保证有序完成
- **容错**：单个工具失败不影响其他工具，错误收集后统一上报

---

## 8. 任务分解：子 Agent 委派

### 8.1 Multi-Agent V2 派生

- **spawn 工具**：`codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs` — `SpawnAgent` 工具
- **派生准备**：`codex-rs/core/src/agent/control/spawn.rs` — `prepare_thread_spawn()`, `prepare_agent_metadata()`
- **机制**：
  - 主 agent 通过 `spawn_agent` 工具调用派生子 agent
  - 每个子 agent 独立 thread，继承环境、exec policy、config
  - 子 agent 异步运行，通过 mailbox / inter-agent communication 回报
  - 完成监听器 `maybe_start_completion_watcher`（`control.rs:435-518`）监控子 agent 状态并转发结果
  - 深度限制：`exceeds_thread_spawn_depth_limit()` 防止无限递归

### 8.2 子 Agent 配置

- `features.multi_agent_v2.max_concurrent_threads_per_session`：最大并发子 agent 数
- `features.multi_agent_v2.min_wait_timeout_ms` / `max_wait_timeout_ms` / `default_wait_timeout_ms`：等待子 agent 完成的超时阈值

### 8.3 Inter-Agent 通信

- **代码位置**：`codex-rs/core/src/agent_communication.rs` — `AgentCommunicationContext`, `AgentCommunicationKind`
- **机制**：子 agent 通过 `InterAgentCommunication` 发送结构化消息，mailbox 队列（`TurnInputQueue`）管理对父 agent 的延迟投递

---

## 9. 设计思想提炼

1. **优雅降级优先于硬失败**：压缩 > 截断，沙箱升级 > 拒绝，传输降级 > 报错
2. **模型主导续行**：`needs_follow_up` 信号驱动循环，不设硬性轮次上限
3. **多层预算分工**：context window / auto-compact / rollout budget 三层各司其职
4. **错误分类是基础**：`is_retryable()` 是所有重试/降级决策的入口
5. **持久化兜底**：session 落盘 + 可恢复，长任务可跨进程续传
6. **分解复杂任务**：子 agent 异步委派是处理超长任务的主要手段
7. **非致命错误不杀 session**：错误 emit 给 UI 后 break 循环，用户可继续对话
8. **预算感知**：模型能感知 token 预算紧张，主动调整策略

---

## 10. 关键配置项速查表

| 机制 | 文件 | 配置项 | 默认值 |
|------|------|--------|--------|
| Auto-compaction | `turn.rs:158,354`, `compact.rs:92-122` | `model_auto_compact_token_limit`, `model_auto_compact_token_limit_scope` | 80% 上下文窗口 |
| Token budget 提醒 | `token_budget.rs:6-61` | `features.token_budget.reminder_threshold_tokens` | None（禁用） |
| Rollout budget | `rollout_budget.rs:44-52` | `features.rollout_budget.limit_tokens` | None（禁用） |
| 重试退避 | `responses_retry.rs:22-79` | provider 的 `stream_max_retries()` | provider 特定 |
| 传输降级 | `responses_retry.rs:31-46` | WebSocket → HTTPS 自动降级 | WS 可用时启用 |
| 沙箱升级 | `tools/orchestrator.rs:293-511` | `escalate_on_failure()` per tool | 工具特定 |
| 最大子 agent 数 | `agent/control/execution.rs:86-95` | `multi_agent_v2.max_concurrent_threads_per_session` | 平台特定 |
| 限流 | `turn.rs:1188-1193` | `update_rate_limits()` | API 特定 |
| Session 持久化 | `thread_manager.rs:778-845` | `RolloutRecorder` | 始终启用 |
| 中断 | `session/mod.rs:3905-3912`, `tasks/mod.rs:497-525` | N/A | 始终可用 |
| 错误分类 | `protocol/src/error.rs:176-213` | `is_retryable()` | 硬编码 |
| Context window 计算 | `context_window.rs:23-91` | `model_context_window` | 模型默认 |

---

## 11. 系统提示词：鼓励持续推进的指令

Codex 的提示词分层与 yi-agent 不同，它把"持续推进"的指令分散在两层：
**base_instructions**（模型级系统提示）和 **collaboration mode**（协作模式级
developer_instructions）。普通模式下走的都是 Default 协作模式，而非 Goal 模式。

### 11.1 提示词分层结构

| 层级 | 文件 | 注入位置 | 角色 |
|------|------|----------|------|
| **base_instructions** | `codex-rs/protocol/src/prompts/base_instructions/default.md` | system prompt 主体 | 核心 "keep going" 指令 |
| **collaboration mode（Default）** | `codex-rs/collaboration-mode-templates/templates/default.md` | developer_instructions | "优先假设并执行而非问问题" |
| **collaboration mode（Plan）** | `codex-rs/collaboration-mode-templates/templates/plan.md` | developer_instructions | "问清楚再做"（与 Default 对立） |
| **collaboration mode（Execute）** | `codex-rs/collaboration-mode-templates/templates/execute.md` | 仅常量保留，TUI 不暴露 | 已合并入 Default |
| **compact prompt** | `codex-rs/prompts/templates/compact/prompt.md` | 压缩时注入 | "摘要要为下一个 LLM 无缝续行而写" |
| **goal continuation** | `codex-rs/prompts/templates/goals/continuation.md` | Goal 模式续行时注入 | 仅 Goal 模式生效 |

注入路径（`codex-rs/core/src/session/mod.rs:608-612`）：

```rust
let base_instructions = config
    .base_instructions
    .clone()
    .or_else(|| conversation_history.get_base_instructions().map(|s| s.text))
    .unwrap_or_else(|| model_info.get_model_instructions(config.personality));
```

`get_model_instructions()` 默认返回 `BASE_INSTRUCTIONS_DEFAULT`
（`codex-rs/protocol/src/models.rs:1258`），即
`protocol/src/prompts/base_instructions/default.md` 的内容。

### 11.2 普通模式核心"别停"指令

**位置**：`codex-rs/protocol/src/prompts/base_instructions/default.md:125`

```
## Task execution

You are a coding agent. Please keep going until the query is
completely resolved, before ending your turn and yielding back to the
user. Only terminate your turn when you are sure that the problem is
solved. Autonomously resolve the query to the best of your ability,
using the tools available to you, before coming back to the user.
Do NOT guess or make up an answer.
```

这条指令做了三件事：
1. **明确续行义务**："keep going until the query is completely resolved"
2. **限制结束条件**："Only terminate your turn when you are sure that the
   problem is solved"
3. **要求自主解决**："Autonomously resolve... using the tools available"

### 11.3 协作模式层（Default）的补充

**位置**：`codex-rs/collaboration-mode-templates/templates/default.md:11`

```
In Default mode, strongly prefer making reasonable assumptions and
executing the user's request rather than stopping to ask questions.
If you absolutely must ask a question because the answer cannot be
discovered from local context and a reasonable assumption would be
risky, ask the user directly with a concise plain-text question.
```

意图：**默认假设并执行，而不是停下来问**。只有"本地无法发现 + 合理假设有
风险"才允许问。

### 11.4 Plan 模式（对立面，对照参考）

**位置**：`codex-rs/collaboration-mode-templates/templates/plan.md`

Plan 模式是 Default 的反面：要求"问清楚再做"，禁止 mutating 动作，只允许
non-mutating 探索。三种模式形成光谱：

| 模式 | TUI 可见 | 核心指令 |
|------|---------|----------|
| Plan | 是 | 问清楚再做，禁止 mutating |
| Default | 是 | 强烈优先假设并执行，实在不行才问 |
| Execute | 否（已合并） | 禁止问用户，必须假设并继续 |

### 11.5 关于 Goal 模式的说明

`continuation.md` 里那套"3 轮 blocked 审计 / 完成要取证 / 不许缩范围"的硬性
规则**只在 Goal 模式下生效**。普通模式（Default）下 Codex 采取的是**轻量约束
+ 模型自身判断**策略，只有 base_instructions 的 "keep going" + Default 协作
模式的 "strongly prefer assumptions" 两条指令，没有 goal 模式那种复杂审计。

### 11.6 压缩摘要的"续行导向"指令

**位置**：`codex-rs/prompts/templates/compact/prompt.md:9`

```
Be concise, structured, and focused on helping the next LLM
seamlessly continue the work.
```

意图：压缩不是终结，而是为下一次接力的 handoff，摘要要写成能让下一个 LLM
**无缝续行**。

### 11.7 设计哲学总结

普通模式下 Codex 的"持续推进"策略其实**非常简洁**：

1. **base_instructions 一条核心指令**：keep going until completely resolved
2. **collaboration mode 一条补充**：prefer assumptions over questions
3. **compact prompt 一条 handoff 指令**：write for seamless continuation
4. **Goal 模式才上硬性审计**：3 轮 blocked / 完成取证 / 禁止缩范围

核心思路是：**普通任务靠模型自身判断，只有显式长任务（Goal 模式）才加硬性
约束**。这与 yi-agent 当前的"无任何 keep going 指令"形成鲜明对比。
