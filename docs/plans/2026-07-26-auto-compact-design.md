# Auto-Compact 设计

日期: 2026-07-26
状态: 设计

## 背景

`/compact` 手动命令已实现(`yi-agent/src/compact.rs`),但 `AgentConfig.compact_threshold` 字段虽存在,`run_loop` 里没有基于 token 计数自动触发的逻辑。本设计实现 auto-compact:session 过大时在 `run_loop` 内自动压缩。

## 决策

| 决策点 | 选择 | 理由 |
|---|---|---|
| 触发指标 | 最近一次 `Usage.input_tokens` | 真实 prefill token 数,含 cache,无需额外估算 |
| 阈值 | 复用 `AgentConfig.compact_threshold`(`context_length * ratio / 100`,默认 160K) | 已有配置,`None` 或 `0` 关闭 |
| 触发时机 | 每轮 THINK 前 | 不会打断 tool_use/tool_result 对 |
| compact 函数位置 | 移到 `yi-agent-core/src/compact.rs` | `run_loop` 在 core,必须能直接调用 |
| 默认行为 | 默认开,用 `compact_threshold` 控制 | `None` 或 `Some(0)` 关闭 |
| UI 事件 | 新增 `AgentEvent::AutoCompacting { old_msg_count, new_msg_count }` | UI 可显示"已自动压缩 N → M 条" |
| 失败处理 | 只 warn,继续运行,下轮重试 | 不中断用户工作 |
| 防抖 | compact 后基线自动重置(下一轮 input_tokens 变小) | 避免死循环 |

## 架构改动

### 模块迁移

- `yi-agent/src/compact.rs` → `yi-agent-core/src/compact.rs`
- 搬移:`SUMMARY_PROMPT_TEMPLATE`、`format_messages_for_summary`、`build_summary_prompt`、`find_safe_split_point`、`compact_session` 及 10 个测试
- binary 层 `yi-agent/src/compact.rs` 删除或改 `pub use yi_agent_core::compact;`
- `yi-agent/src/main.rs` 的 `/compact` 处理改用 `yi_agent_core::compact::compact_session`

### AgentEvent 新增

```rust
/// Auto-compact 完成事件。
AutoCompacting {
    old_msg_count: usize,
    new_msg_count: usize,
},
```

一个事件表示完成(compact 同步调用)。UI 收到后可显示"已自动压缩 N → M 条"。

### run_loop 改动

在 `run_loop` 开头(`session_len` 记录之后)新增状态:

```rust
let mut last_input_tokens: Option<u32> = None;
```

每轮 THINK 前(在现有 `cancel_token` 检查之后,`turn += 1` 之前):

```rust
if let (Some(threshold), Some(tokens)) =
    (config.compact_threshold.filter(|&t| t > 0), last_input_tokens)
    && tokens >= threshold
    && messages.len() > 4  // 至少 5 条才有 compact 价值
{
    let old_count = messages.len();
    let keep_turns = config.compact_keep_turns.unwrap_or(4);
    match compact_session(&provider, &config, &session, keep_turns).await {
        Ok(new_session) => {
            messages = new_session.messages().to_vec();
            *session.lock().unwrap() = new_session;
            let _ = tx.send(AgentEvent::AutoCompacting {
                old_msg_count: old_count,
                new_msg_count: messages.len(),
            }).await;
        }
        Err(e) => {
            tracing::warn!(error = %e, "auto-compact failed, will retry next turn");
        }
    }
}
```

### accumulate_provider_stream 签名改动

当前签名返回 `(Vec<ContentBlock>, StopReason)`。改为返回 `(Vec<ContentBlock>, StopReason, Option<TokenUsage>)`,把最后一次 `Usage` 事件返回。run_loop THINK 后用返回的 usage 更新 `last_input_tokens = usage.input_tokens`。

compact 后基线自动重置:compact 后下一轮的 input_tokens 只含 summary + 保留轮,会小于 threshold,自然不会再次触发。

## 测试计划

### core 单元测试(mock,在 `yi-agent-core/src/agent.rs` 的 `#[cfg(test)]`)

`ScriptedProvider` 按 `call_index` 消耗脚本。auto-compact 调 `compact_session` → `provider.call()` → 消耗一个额外脚本。

| # | 测试名 | 脚本序列 | 断言 |
|---|---|---|---|
| 1 | `auto_compact_triggers_when_threshold_exceeded` | [tool_use + Usage(input=200)] [summary_text] ["done" + EndTurn] | events 含 `AutoCompacting`;最终 session 含 summary + 保留轮 |
| 2 | `auto_compact_skipped_below_threshold` | [tool_use + Usage(input=50)] ["done" + EndTurn] | 无 `AutoCompacting`;session 正常累积 |
| 3 | `auto_compact_skipped_when_threshold_none` | 同 #1,config.compact_threshold=None | 无 `AutoCompacting` |
| 4 | `auto_compact_skipped_when_threshold_zero` | 同 #1,compact_threshold=Some(0) | 无 `AutoCompacting` |
| 5 | `auto_compact_skipped_on_first_turn` | [Usage(input=999) + EndTurn] | 首轮无 last_input_tokens,不 compact |
| 6 | `auto_compact_failure_continues_loop` | [tool_use + Usage(200)] [provider error] [tool_use] ["done"] | compact 失败后 run_loop 继续到 Done |
| 7 | `auto_compact_resets_baseline` | [tool_use + Usage(200)] [summary] [tool_use + Usage(200)] [summary2] ["done"] | 两次 compact 之间有一次 THINK,不无限循环 |

### e2e 真实测试(`yi-agent/tests/e2e_real.rs`)

新增 `e2e_auto_compact_triggers`:
- `--compact-ratio 1`(threshold ≈ 2000 tokens)
- `--compact-keep-turns 1`
- prompt:read 一个较大文件(如 `Cargo.toml`),单轮 input_tokens 超 2000
- 断言:出现 `AutoCompacting` 事件则验证 payload;未出现也不 fail(模型行为不稳定)
- `#[ignore]` 标记,需 `--ignored` 运行,无 API key 时 `eprintln!("skip")` 返回

### justfile recipe

新增 `test-real-auto-compact`,或并入 `test-real-e2e`(可能需单独 recipe,因要传 `--compact-ratio 1`)。

## 实现清单

1. `yi-agent-core/src/lib.rs`:新增 `pub mod compact;`
2. `yi-agent-core/src/compact.rs`:从 binary 层搬过来
3. `yi-agent-core/src/agent.rs`:
   - `AgentEvent` 新增 `AutoCompacting { old_msg_count, new_msg_count }`
   - `run_loop` 开头新增 `last_input_tokens: Option<u32>`
   - 每轮 THINK 前插入 auto-compact 检查块
   - `accumulate_provider_stream` 改返回 `(Vec<ContentBlock>, StopReason, Option<TokenUsage>)`
   - run_loop THINK 后用返回的 usage 更新 `last_input_tokens`
4. `yi-agent/src/compact.rs`:删除或改 `pub use yi_agent_core::compact;`
5. `yi-agent/src/main.rs`:`/compact` 分支改用 `yi_agent_core::compact::compact_session`
6. `yi-agent-tui/src/app.rs`:收到 `AutoCompacting` 事件显示提示(可选)
7. `yi-agent/tests/e2e_real.rs`:新增 `e2e_auto_compact_triggers`
8. `docs/project-management/yi-agent-core.md`:登记 auto-compact feature
9. `docs/project-management/README.md`:同步计数

## TDD 执行顺序

1. 写测试 #1(red) → 改 `accumulate_provider_stream` 签名 + 加 auto-compact 块(green)
2. 依次写测试 #2-#7,每写一个 red 就补对应分支
3. 迁移 `compact.rs` 到 core(纯移动 + 改路径)
4. 改 binary 层 `/compact` 路径
5. 跑 `cargo test -p yi-agent-core` 全绿
6. 跑 `cargo fmt --all` + `cargo test -p yi-agent --lib`
7. 写真实 e2e 测试,用 `just test-real-e2e` 验证
