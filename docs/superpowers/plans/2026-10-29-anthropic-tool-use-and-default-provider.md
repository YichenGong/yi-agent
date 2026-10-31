# 默认 provider 改为 openai + 修复 Anthropic tool use 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把未配置时的默认 provider 改成 openai，并修复 Anthropic tool use 的三处缺陷（stop reason 误判、usage 覆盖、空参数工具解析失败），同时建立跨 provider 的能力契约测试。

**Architecture:** 三处缺陷分属不同层，各修在其正确位置：stop reason 映射在 `yi-agent-llm/src/anthropic/stream.rs`；usage 合并在 `yi-agent-core/src/provider.rs` 的 `TokenUsage::merge_snapshot`（一处修好所有 provider）；空参数兜底在 `accumulate_stream` 的 `ToolUseEnd` 分支。默认 provider 只改运行时回退值与两处引导初值。契约测试新增 `yi-agent-llm/tests/provider_contract.rs`。

**Tech Stack:** Rust 1.98、tokio、wiremock 0.6、serde_json、React 19 + TypeScript（仅引导初值一行）。

## Global Constraints

- 工作目录：`/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-anthropic-tool-use`（分支 `fix/anthropic-tool-use`），**禁止在 `main` 上改动**。
- `cargo` 不在默认 PATH，必须先 `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`。
- 提交前必须 `cargo fmt --all`（在 `yi-agent-rs/` 下）。
- 不要并行跑多个 `cargo test`；每次跑前确认无残留 `cargo` / `yi_agent_*` 进程。
- commit message 用 conventional commits，**不写** `Co-Authored-By`。
- 不改任何测试 fixture 的 provider 字面量（`sample_config()`、`onboarding_rpc.rs::fallback()`、`onboarding.rs:421` 等）——它们只构造合法值，不代表默认语义。
- 不改 `anthropic/types.rs` 的 `thinking` 配置，不处理 `pause_turn`（spec §2 非目标）。
- 不同步修改 `docs/plans/` 下的历史计划文档。

---

## 文件结构

| 文件 | 职责 | 本次改动 |
|---|---|---|
| `yi-agent-core/src/provider.rs` | `TokenUsage` 定义 + `accumulate_stream` | 加 `merge_snapshot`；改 Usage 分支合并、ToolUseEnd 空参兜底 |
| `yi-agent-llm/src/anthropic/stream.rs` | Anthropic SSE → `ProviderEvent` | `tool_use` → `EndTurn`；修测试期望 |
| `yi-agent-llm/tests/provider_contract.rs` | 新增：跨 provider 能力契约 | 新建 |
| `yi-agent-runtime/src/config.rs` | 运行时默认配置 | 回退值改 openai；修正默认回退测试 |
| `yi-agent/src/onboarding_prompt.rs` | TUI 引导 | 默认值改 openai |
| `desktop/src/components/OnboardingWizard.tsx` | 桌面引导 | 初值改 openai |
| `docs/project-management/yi-agent-llm.md` | 模块进度 | 更新 feature 条目 |

---

### Task 1: `TokenUsage::merge_snapshot` + 单测

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/provider.rs`（`TokenUsage` 定义后，约 `:36-43`）
- Test: 同文件 `mod tests`

**Interfaces:**
- Produces: `pub fn merge_snapshot(&mut self, next: &TokenUsage)`

- [ ] **Step 1: 写失败测试**

在 `provider.rs` 的 `#[cfg(test)] mod tests` 内追加：

```rust
#[test]
fn merge_snapshot_keeps_earlier_input_when_later_reports_zero() {
    let mut acc = TokenUsage {
        input_tokens: 100_000,
        output_tokens: 0,
        cache_read_input_tokens: Some(90_000),
        ..Default::default()
    };
    let next = TokenUsage {
        input_tokens: 0,
        output_tokens: 42,
        ..Default::default()
    };
    acc.merge_snapshot(&next);
    assert_eq!(acc.input_tokens, 100_000, "0 means 'not reported', keep old");
    assert_eq!(acc.output_tokens, 42, "non-zero wins");
    assert_eq!(acc.cache_read_input_tokens, Some(90_000), "None keeps old");
}

#[test]
fn merge_snapshot_lets_a_nonzero_later_value_win() {
    let mut acc = TokenUsage {
        input_tokens: 10,
        ..Default::default()
    };
    acc.merge_snapshot(&TokenUsage {
        input_tokens: 20,
        ..Default::default()
    });
    assert_eq!(acc.input_tokens, 20);
}

#[test]
fn merge_snapshot_some_overrides_none_and_vice_versa() {
    let mut acc = TokenUsage {
        cache_creation_input_tokens: None,
        reasoning_tokens: Some(7),
        ..Default::default()
    };
    acc.merge_snapshot(&TokenUsage {
        cache_creation_input_tokens: Some(5),
        reasoning_tokens: None,
        ..Default::default()
    });
    assert_eq!(acc.cache_creation_input_tokens, Some(5));
    assert_eq!(acc.reasoning_tokens, Some(7));
}
```

- [ ] **Step 2: 运行确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --lib merge_snapshot
```
预期：编译失败，`no method named merge_snapshot`。

- [ ] **Step 3: 实现**

在 `TokenUsage` 定义之后、`ReasoningKind` 之前插入：

```rust
impl TokenUsage {
    /// 把一条后到的 usage 片段合进当前快照。
    ///
    /// provider 可能分多次报告同一次请求的用量：Anthropic 的 `message_start`
    /// 带 `input_tokens` 与 cache 字段，末端的 `message_delta` 只带
    /// `output_tokens`。若用「后到者整体覆盖」，先到的非零值会被抹成 0，
    /// 而自动压缩正是靠 `input_tokens` 判定（见 `Session::last_input_tokens`）。
    ///
    /// 合并规则：
    /// - `u32` 字段：后到的**非零**值覆盖；零值视为「本次未报告」，保留旧值。
    /// - `Option<u32>` 字段：`Some` 覆盖，`None` 保留旧值。
    ///
    /// 已知局限：无法表达「真实值为 0 且需把此前的非零值清零」。对 token 计数
    /// 该场景不存在（一次请求的 `input_tokens` 不会由非零变 0），故接受。
    pub fn merge_snapshot(&mut self, next: &TokenUsage) {
        if next.input_tokens != 0 {
            self.input_tokens = next.input_tokens;
        }
        if next.output_tokens != 0 {
            self.output_tokens = next.output_tokens;
        }
        if next.cache_creation_input_tokens.is_some() {
            self.cache_creation_input_tokens = next.cache_creation_input_tokens;
        }
        if next.cache_read_input_tokens.is_some() {
            self.cache_read_input_tokens = next.cache_read_input_tokens;
        }
        if next.reasoning_tokens.is_some() {
            self.reasoning_tokens = next.reasoning_tokens;
        }
    }
}
```

- [ ] **Step 4: 运行确认通过**

```bash
cargo test -p yi-agent-core --lib merge_snapshot
```
预期：3 passed。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all && git add -A && git commit -m "feat(core): add TokenUsage::merge_snapshot for split usage reports"
```

---

### Task 2: `accumulate_stream` 改用合并 + 空参数兜底

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/provider.rs`（`accumulate_stream` 内 `Usage` 与 `ToolUseEnd` 分支，约 `:300-315`、`:281-289`）
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `TokenUsage::merge_snapshot`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn accumulate_merges_split_usage_instead_of_overwriting() {
    let provider = MockProvider {
        events: vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Usage(TokenUsage {
                input_tokens: 100_000,
                output_tokens: 0,
                cache_read_input_tokens: Some(90_000),
                ..Default::default()
            }),
            ProviderEvent::Usage(TokenUsage {
                input_tokens: 0,
                output_tokens: 42,
                ..Default::default()
            }),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
    };
    let (_content, _end, usage) =
        accumulate_stream(provider.stream_for_test(), |_| {}, None).await.unwrap();
    let usage = usage.expect("usage");
    assert_eq!(usage.input_tokens, 100_000, "input must survive the later output-only report");
    assert_eq!(usage.output_tokens, 42);
}

#[tokio::test]
async fn a_zero_arg_tool_call_without_arguments_parses_as_empty_object() {
    let provider = MockProvider {
        events: vec![
            ProviderEvent::ToolUseStart { id: "t1".into(), name: "list_files".into() },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ],
    };
    let (content, _end, _usage) =
        accumulate_stream(provider.stream_for_test(), |_| {}, None).await.unwrap();
    assert!(
        content.iter().any(|b| matches!(b, ContentBlock::ToolUse { input, .. } if input == &serde_json::json!({}))),
        "empty arguments must become {{}}, got: {content:?}"
    );
}
```

若 `MockProvider` 没有直接拿流的辅助方法，用现有测试里同样的写法（`provider.call_stream(req).await.unwrap()`）代替 `stream_for_test()`。

- [ ] **Step 2: 运行确认失败**

```bash
cargo test -p yi-agent-core --lib accumulate_merges_split_usage_instead_of_overwriting a_zero_arg_tool_call
```
预期：两条都失败（一条 `input_tokens == 0`，一条 `malformed tool use JSON`）。

- [ ] **Step 3: 实现**

`Usage` 分支改为：

```rust
ProviderEvent::Usage(u) => {
    match &mut last_usage {
        Some(acc) => acc.merge_snapshot(&u),
        None => last_usage = Some(u.clone()),
    }
    on_event(ProviderEvent::Usage(u));
}
```

`ToolUseEnd` 分支解析处改为：

```rust
ProviderEvent::ToolUseEnd { id } => {
    if let Some((name, json)) = tool_uses.remove(&id) {
        // 无参数工具可以完全不发参数增量（Anthropic 只发
        // `content_block_start` + `content_block_stop`）。空串不是畸形数据，
        // 按空对象处理；非空但截断的 JSON 仍报错——那是真的数据损坏。
        let json = if json.trim().is_empty() { "{}".to_string() } else { json };
        let input: serde_json::Value = serde_json::from_str(&json).map_err(|e| {
            ProviderError::Stream(format!("malformed tool use JSON for id {id}: {e}"))
        })?;
        content.push(ContentBlock::ToolUse { id, name, input });
    }
}
```

- [ ] **Step 4: 运行确认通过**

```bash
cargo test -p yi-agent-core --lib
```
预期：全绿（含原有用例，确认没有行为回归）。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all && git add -A && git commit -m "fix(core): merge split usage reports and accept empty tool arguments"
```

---

### Task 3: Anthropic `tool_use` → `EndTurn`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs`（`message_delta` 分支的 stop reason 映射，约 `:213-227`；测试 `maps_stop_reasons_correctly` 约 `:440-450`）
- Test: 同文件 `mod tests`

- [ ] **Step 1: 改测试期望（红）**

把 `maps_stop_reasons_correctly` 中的这一行：

```rust
("tool_use", StopReason::Other("tool_use".to_string())),
```

改成：

```rust
// Anthropic 以 `tool_use` 表示「模型要求执行工具」。agent 循环靠内容里的
// ToolUse 块决定是否执行，`Other` 却会被当成终止原因直接中断（见
// agent.rs 的 StopReason::Other 分支），故必须归一为 EndTurn，与 OpenAI
// 的 finish_reason=tool_calls 处理一致。
("tool_use", StopReason::EndTurn),
```

- [ ] **Step 2: 运行确认失败**

```bash
cargo test -p yi-agent-llm --lib maps_stop_reasons_correctly
```
预期：fail（`Other("tool_use")` != `EndTurn`）。

- [ ] **Step 3: 实现**

在 `message_delta` 的匹配里加一条：

```rust
"end_turn" => StopReason::EndTurn,
"tool_use" => StopReason::EndTurn,
"max_tokens" => StopReason::MaxTokens,
```

- [ ] **Step 4: 端到端回归测试（真实报文形状）**

追加一条测试，重放完整 Anthropic 报文（`message_start` → text → tool_use → `message_delta{stop_reason:tool_use}`）：

```rust
#[tokio::test]
async fn a_full_tool_use_response_ends_with_end_turn() {
    let body = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100}}}\n\n\
         event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
         event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me read.\"}}\n\n\
         event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\",\"input\":{}}}\n\n\
         event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}\n\n\
         event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
         event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":50}}\n\n\
         event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let bytes = body.to_string().into_bytes();
    let events: Vec<ProviderEvent> = collect_events(vec![bytes.as_slice()])
        .await
        .into_iter()
        .filter_map(|r| r.ok())
        .collect();

    let stop = events
        .iter()
        .find_map(|e| match e {
            ProviderEvent::Stop { reason } => Some(reason.clone()),
            _ => None,
        })
        .expect("a Stop event must be emitted");
    assert_eq!(
        stop,
        StopReason::EndTurn,
        "tool_use must map to EndTurn or the agent loop never executes the tool"
    );
}
```

- [ ] **Step 5: 运行确认通过并提交**

```bash
cargo test -p yi-agent-llm --lib && cargo fmt --all && git add -A && \
git commit -m "fix(llm): map anthropic tool_use stop reason to EndTurn"
```

---

### Task 4: 跨 provider 能力契约测试

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-llm/tests/provider_contract.rs`

**Interfaces:**
- Consumes: `AnthropicProvider`、`OpenaiProvider`（`yi-agent-llm` 的公开导出）、`yi_agent_core::{Provider, ProviderRequest, ProviderEvent, ContentBlock, StopReason, ProviderError, GenParams, Message, ToolSchema}`

- [ ] **Step 1: 写测试骨架 + 两个 provider 的构造器**

用一个枚举把「provider 种类」参数化，每条用例对两种各跑一次：

```rust
#[derive(Clone, Copy, Debug)]
enum Kind {
    Anthropic,
    Openai,
}

fn provider(kind: Kind, base: &str) -> Arc<dyn Provider> { /* 按 kind 构造，api_style=chat */ }
fn post_path(kind: Kind) -> &'static str { /* Anthropic → "/v1/messages"；Openai → "/chat/completions" */ }
```

- [ ] **Step 2: 逐条实现 spec §3.5 的七条契约**

对每条用例，两边各自用「该 provider 的真实报文形状」作为 mock body，但断言同一套语义结果：纯文本 / 单工具 / 多工具 / 文本+工具混排 / 无参数工具 / usage 合并 / 错误码。

- [ ] **Step 3: 运行**

```bash
cargo test -p yi-agent-llm --test provider_contract
```
预期：全绿。若「无参数工具」或「usage 合并」失败，说明 Task 2 未生效，回查。

- [ ] **Step 4: 变异验证（确认测试真的有判别力）**

临时把 `merge_snapshot` 的 `input_tokens` 判断改回整体覆盖，确认契约测试变红，再改回来。若不变红，说明该用例断言不足。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all && git add -A && git commit -m "test(llm): add cross-provider capability contract tests"
```

---

### Task 5: 默认 provider 改为 openai

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs:278`，测试 `:977-1002`
- Modify: `yi-agent-rs/crates/yi-agent/src/onboarding_prompt.rs:56-57`
- Modify: `desktop/src/components/OnboardingWizard.tsx:44`

- [ ] **Step 1: 写/改测试（红）**

`config.rs` 里把 `load_defaults_provider_to_anthropic` 整体替换为：

```rust
/// 真正覆盖「未注入 provider 时的回退」。
///
/// 旧版本这个位置的测试名叫 `..._to_anthropic`，却传了
/// `provider: Some("anthropic")` —— 断言的是显式配置，回退路径其实没有测试。
#[test]
fn defaults_provider_to_openai_when_nothing_is_configured() {
    let _lock = ENV_TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    let _env = isolated_config_env();
    let overrides = ConfigOverrides {
        provider: None,
        api_url: None,
        // 回退路径要求 key 也存在，否则走的是「缺 key 报错」分支。
        api_key: None,
        model: None,
        max_turns: None,
        workdir: Some(PathBuf::from(".")),
        system_prompt: None,
        model_context_length: None,
        compact_ratio: None,
        compact_keep_turns: None,
        compact_user_budget_tokens: None,
        compact_tool_budget_tokens: None,
        yolo: false,
        sandbox: None,
        sandbox_writable_roots: Vec::new(),
        skip_permissions: false,
        skills_catalog_budget: None,
    };
    let config = RuntimeConfig::load_lenient(&overrides).unwrap();
    assert_eq!(config.provider, "openai");
    assert_eq!(config.api_url, "https://api.openai.com/v1");
    assert_eq!(config.model, "", "no default model name may be invented");
}
```

注意：`load` 在缺 key 时会 `bail!`，故用 `load_lenient`（`allow_missing_key`）。

- [ ] **Step 2: 运行确认失败**

```bash
cargo test -p yi-agent-runtime --lib defaults_provider_to_openai
```
预期：fail（actual `"anthropic"`）。

- [ ] **Step 3: 实现**

`config.rs:278`：

```rust
let provider = injected_provider.unwrap_or_else(|| "openai".to_string());
```

`onboarding_prompt.rs:56-57`：

```rust
"API 格式 [openai/anthropic]（默认 openai）: ",
"openai",
```

`OnboardingWizard.tsx:44`：

```tsx
const [provider, setProvider] = useState<"anthropic" | "openai">("openai");
```

- [ ] **Step 4: 检查是否还有其它行为默认**

```bash
grep -rn 'unwrap_or_else(|| "anthropic"\|provider_defaults\|默认 anthropic' \
  yi-agent-rs/crates --include=*.rs | grep -v "_test\|tests/"
```
预期：只剩 `onboarding_prompt.rs` 的 `provider_defaults`（那是「按 provider 查默认
URL」的查询函数，不是「默认选哪个 provider」）。

- [ ] **Step 5: 运行并通过**

```bash
cargo test -p yi-agent-runtime --lib && cargo test -p yi-agent --lib
cd ../desktop && npx tsc --noEmit
```

- [ ] **Step 6: 提交**

```bash
cargo fmt --all && git add -A && \
git commit -m "feat: default the provider to openai when nothing is configured"
```

---

### Task 6: 更新模块进度文档

**Files:**
- Modify: `docs/project-management/yi-agent-llm.md`

- [ ] **Step 1: 追加三条 feature 记录**

按现有格式，每条带**可验证判据**（文件:行号 + 可执行命令）：

```markdown
- [x] Anthropic `stop_reason: tool_use` 归一为 `EndTurn` — `anthropic/stream.rs` 的
  `message_delta` 分支；此前映射成 `Other("tool_use")`，agent 循环对 `Other` 直接
  中断，导致 Anthropic 的工具调用**从不执行**。验证：
  `cargo test -p yi-agent-llm --lib a_full_tool_use_response_ends_with_end_turn`
- [x] usage 分片字段级合并 — `yi-agent-core/src/provider.rs` 的
  `TokenUsage::merge_snapshot`；`message_delta` 只带 `output_tokens` 时不再把
  `input_tokens` 抹成 0（否则自动压缩永不触发）。验证：
  `cargo test -p yi-agent-core --lib merge_snapshot`
- [x] 空参数工具按 `{}` 处理 — `accumulate_stream` 的 `ToolUseEnd` 分支；Anthropic
  的无参数工具可以完全不发 `input_json_delta`。验证：
  `cargo test -p yi-agent-core --lib a_zero_arg_tool_call_without_arguments_parses_as_empty_object`
- [x] 跨 provider 能力契约测试 — `yi-agent-llm/tests/provider_contract.rs`，同一套
  语义用例对 Anthropic 与 OpenAI 各跑一遍。验证：
  `cargo test -p yi-agent-llm --test provider_contract`
```

- [ ] **Step 2: 提交**

```bash
git add -A && git commit -m "docs: record anthropic tool use fixes in module progress"
```

---

### Task 7: 全量验收 + 合并

- [ ] **Step 1: 按 crate 逐个跑（不要 `--workspace`）**

```bash
ps aux | grep -E "cargo|yi_agent" | grep -v grep   # 必须为空
cargo test -p yi-agent-core
cargo test -p yi-agent-llm
cargo test -p yi-agent-runtime
cargo test -p yi-agent --lib
```

- [ ] **Step 2: 格式检查**

```bash
cargo fmt --all --check
```

- [ ] **Step 3: 合并回 main**

```bash
cd <主仓库> && git merge --no-ff fix/anthropic-tool-use
```

- [ ] **Step 4: 清理**

```bash
git worktree remove .worktrees/fix-anthropic-tool-use && git branch -d fix/anthropic-tool-use
```

---

## 自检

**spec 覆盖**

| spec 条目 | 对应 Task |
|---|---|
| §3.1 默认 provider | Task 5 |
| §3.2 缺陷 A（tool_use） | Task 3 |
| §3.3 缺陷 B（usage 合并） | Task 1 + Task 2 |
| §3.4 缺陷 C（空参数） | Task 2 |
| §3.5 契约测试 | Task 4 |
| §4 测试策略三层 | Task 1/2（单元）、Task 3（端到端）、Task 4（契约） |

**遗留决策（未纳入本计划，等用户另行确认）**

- `anthropic/types.rs` 无条件发送 `thinking: {type:"adaptive"}`：对不支持
  extended thinking 的模型/网关会 400。
- `pause_turn` stop reason 未处理：长任务会被当成未知原因中断。
