# 默认 provider 改为 openai + 修复 Anthropic tool use 三处缺陷

日期：2026-10-29
状态：已确认，待实施

## 1. 问题

两件事一起做，因为第二件决定了第一件不能只是改一行常量。

### 1.1 默认 provider 是 anthropic

`RuntimeConfig::load_inner` 在「用户完全未配置模型」时回退到 `"anthropic"`
（`yi-agent-runtime/src/config.rs:278`），默认地址随之切到
`https://api.anthropic.com`（`:281`）。引导流程也把 anthropic 排在前面并作为
默认值：TUI 的 `"API 格式 [anthropic/openai]（默认 anthropic）"`
（`yi-agent/src/onboarding_prompt.rs:56`），桌面引导
`useState<"anthropic" | "openai">("anthropic")`
（`desktop/src/components/OnboardingWizard.tsx:44`）。

用户要求默认改成 openai。

### 1.2 Anthropic tool use 有三处真实缺陷

三处都用 wiremock 重放 Anthropic 官方报文形状实测确认，不是推测。

**缺陷 A —— 工具调用完全不执行（致命）**

`anthropic/stream.rs:340` 把 `stop_reason: "tool_use"` 映射成
`StopReason::Other("tool_use")`，而 agent 主循环对 `Other` 的处理是发
`Done{Interrupted}` 后 `return`（`agent.rs:1347-1355`），**根本走不到工具执行**。
OpenAI 侧把 `finish_reason: "tool_calls"` 归一为 `EndTurn`
（`openai/stream.rs:270-275`），两条路径行为不一致。

实测（重放 Anthropic 报文 → `accumulate_stream` → agent）：

```
EndTurn  -> tool executed: true,  done: ["MaxTurns"]
Other    -> tool executed: false, done: ["Interrupted { reason: "tool_use" }"]
```

`anthropic/stream.rs:446` 的 `maps_stop_reasons_correctly` 把错误行为写成了断言
（`("tool_use", StopReason::Other("tool_use".to_string()))`），因此现有 125 个
lib 测试全绿也发现不了。

**缺陷 B —— 自动压缩永不触发（致命）**

Anthropic 的 usage 分两次到达：`message_start` 带 `input_tokens`，末端的
`message_delta` 只带 `output_tokens`。`accumulate_stream` 用「后到者整体覆盖」
记录 usage（`provider.rs:311` `last_usage = Some(u.clone())`），于是最终
`input_tokens` 被覆盖成 0。

实测：

```
USAGE: TokenUsage { input_tokens: 100000, output_tokens: 0,  cache_read_input_tokens: Some(90000) }
USAGE: TokenUsage { input_tokens: 0,      output_tokens: 42, cache_read_input_tokens: None }
FINAL USAGE = { input_tokens: 0, output_tokens: 42, ... }
```

`maybe_auto_compact` 正是靠 `last_input_tokens >= threshold` 判定
（`agent.rs:1747`、`:1261` 写入）。后果：Anthropic 会话的自动压缩永不触发，
长会话必然撞上下文上限失败。OpenAI 只在流尾发一次 usage，因此侥幸未暴露。

**缺陷 C —— 无参数工具报错（高）**

Anthropic 对无参数工具可以只发 `content_block_start` + `content_block_stop`，
不保证发 `input_json_delta`。此时累积的 JSON 是空串，`accumulate_stream`
直接 `serde_json::from_str("")` 失败：

```
CALL RESULT = Err(Stream("malformed tool use JSON for id toolu_9: EOF while parsing a value at line 1 column 0"))
```

OpenAI 侧 `finish_tool_calls`（`openai/stream.rs:124-147`）对空 args 有兜底，
不会走到这条路径。

## 2. 目标与非目标

**目标**

1. 未配置模型时的默认 provider 为 `openai`，且引导流程的默认选项一致。
2. Anthropic 收到 `stop_reason: "tool_use"` 时进入工具执行，与 OpenAI 行为一致。
3. Anthropic 的 `input_tokens` 不被后续 `output_tokens`-only 的 usage 覆盖成 0，
   自动压缩恢复正常。
4. 无参数工具调用按 `{}` 处理，不产生解析错误。
5. 建立 **provider 能力契约测试**：同一套语义用例对 Anthropic 与 OpenAI 各跑
   一遍，避免两条路径再次出现行为漂移。

**非目标**

- 不改 OpenAI 的既有行为（它当前是正确的；只让它在契约测试里一起被约束）。
- 不改 `anthropic/types.rs` 无条件发送 `thinking: {type:"adaptive"}`
  （用户未决，另议）。
- 不处理 `pause_turn` stop reason（同上，另议）。
- 不改任何测试 fixture 里的 provider 字面量（如 `sample_config()`、
  `onboarding_rpc.rs` 的 `fallback()`）：它们只是构造合法值，不代表默认语义。
- 不新增 provider、不改 RPC 协议、不改桌面 UI 布局。

## 3. 设计

### 3.1 默认 provider 改为 openai

改动点（全部是「默认值」语义，不碰显式配置）：

| 文件 | 改动 |
|---|---|
| `yi-agent-runtime/src/config.rs:278` | `unwrap_or_else(\|\| "openai")` |
| `yi-agent-runtime/src/config.rs:281` | 默认 URL 分支已按 provider 判断，无需改（openai → `/v1`） |
| `yi-agent/src/onboarding_prompt.rs:56` | 提示改为 `（默认 openai）`，默认值参数改 `"openai"` |
| `desktop/src/components/OnboardingWizard.tsx:44` | 初值改 `"openai"` |

**不受影响**：`~/.yi-agent/models.json` 或 `.env` 里显式写了 provider 的用户
（`injected_provider.is_some()` 时根本走不到默认分支）。

默认 `model` 保持空串不变 —— 它没有客观默认值，猜一个只会得到 404。

`config.rs:977` 的测试 `load_defaults_provider_to_anthropic` 名不副实：它实际
传入 `provider: Some("anthropic")`，断言的是**显式配置**而非默认回退。实施时
修正该测试使其真正覆盖「未注入 provider 时回退到 openai」，并改名。

### 3.2 缺陷 A：`tool_use` → `EndTurn`

`anthropic/stream.rs` 的 `message_delta` 分支增加一条映射：

```rust
"tool_use" => StopReason::EndTurn,
```

理由：agent 循环判断是否执行工具，靠的是「accumulated content 里有没有
`ToolUse` 块」（`agent.rs:1359` 之后的 `tool_uses` 提取），`StopReason` 只用来
决定「是继续还是终止」。`Other` 是终止信号，`EndTurn` 是继续信号，因此
`tool_use` 必须落在 `EndTurn`。这与 OpenAI 的既有做法一致。

同时修正 `maps_stop_reasons_correctly` 中把 bug 当期望的那一行。

保留 `StopReason::Other` 用于真正未知的 stop reason（未来 Anthropic 新增值时
仍然是安全的终止，不会静默乱跑）。

### 3.3 缺陷 B：usage 字段级合并

在 `yi-agent-core` 给 `TokenUsage` 加一个 crate 内方法：

```rust
impl TokenUsage {
    /// 把一条后到的 usage 片段合进当前快照。
    ///
    /// provider 可以分多次报告一次请求的用量（Anthropic 的 `message_start`
    /// 给 input/cache，末尾 `message_delta` 只给 output_tokens）。因此「后到者
    /// 整体覆盖」会把先到的非零值抹成 0。合并规则：
    ///
    /// - `u32` 字段：后到的非零值覆盖，零值视为「本次未报告」而保留旧值。
    /// - `Option<u32>` 字段：`Some` 覆盖，`None` 保留旧值。
    ///
    /// 已知局限：真实值为 0 且需要把此前非零值清零的场景表达不了。对 token
    /// 计数而言该场景不存在（一次请求的 input_tokens 不会从非零变 0），
    /// 故接受这一启发式。
    pub fn merge_snapshot(&mut self, next: &TokenUsage)
}
```

`accumulate_stream` 的 `ProviderEvent::Usage` 分支改为：

```rust
match &mut last_usage {
    Some(acc) => acc.merge_snapshot(&u),
    None => last_usage = Some(u.clone()),
}
```

选择在 core 而非 Anthropic 内部合并的理由：一个 provider 的 usage 拆分策略
不该泄漏进另一个 provider 的代码；OpenAI 现在行为不变，但同一份契约今后对所有
provider 生效。

### 3.4 缺陷 C：空 tool 参数按 `{}`

`accumulate_stream` 的 `ToolUseEnd` 分支：

```rust
let json = if json.trim().is_empty() { "{}" } else { json.as_str() };
```

只处理「完全为空」的情况；非空但畸形（如被超时截断的半截 JSON）仍然报错，
因为那确实意味着数据损坏，静默吞掉会让工具收到错误参数。

### 3.5 provider 能力契约测试

新增 `yi-agent-llm/tests/provider_contract.rs`，用同一组语义用例分别对
Anthropic 与 OpenAI 各断言一次。测试只依赖 `Provider` trait 与 wiremock，
不依赖具体报文细节，因此断言的是**能力契约**：

| 用例 | 断言 |
|---|---|
| 纯文本流 | 累积出单个 `Text` 块，`Stop = EndTurn` |
| 单工具调用 | 累积出 `ToolUse{id,name,input}`，`Stop = EndTurn` |
| 多工具一次返回 | 两个 `ToolUse` 块，顺序与报文一致 |
| 文本 + 工具混排 | `content` 顺序为 `[Text, ToolUse]` |
| 无参数工具 | 解析为 `input = {}`，不报错 |
| usage 合并 | 最终 `input_tokens` 与 `output_tokens` 都非零 |
| 401 / 429 / 500 | 分别映射 `Auth` / `RateLimited` / `Server` |

其中「无参数工具」与「usage 合并」两项在修复前必然失败 —— 这就是回归护栏。

## 4. 测试策略

三层，全部默认运行（无 API key）：

1. **单元**：`TokenUsage::merge_snapshot` 的四条规则（非零覆盖、零保留、
   `Option` 语义、完整快照）。
2. **契约**：上述 `provider_contract.rs`，覆盖两个 provider。
3. **端到端**：`anthropic/stream.rs` 内新增一条重放真实报文的测试，断言
   `tool_use` 报文最终产出 `Stop::EndTurn` 且内容含工具块。

验收命令：

```
cargo test -p yi-agent-llm
cargo test -p yi-agent-core
cargo test -p yi-agent-runtime
cargo test -p yi-agent --lib
npx tsc --noEmit            # desktop/ 下，确认引导初值改动不破类型
```

真实 LLM 测试（`--ignored`）不在 CI 范围，本次不改动其断言。

## 5. 风险

- **默认 provider 变更影响面**：只影响「完全未配置」的新用户与从未写过
  `models.json` 的环境；已显式配置的用户走 `injected_provider` 分支，不受影响。
- **`merge_snapshot` 的启发式**：以「0 = 未报告」为前提。已在文档注明局限，
  且 token 计数场景下不成立反例。若将来出现需要清零的 provider，应改为
  `Option<u32>` 全量字段，届时是显式的破坏性变更。
- **修 A 后 Anthropic 的工具循环才真正跑起来**：此前该路径在生产中未被走过，
  首次修复后可能暴露下游（工具执行、结果回传）的既有问题。契约测试与端到端
  测试覆盖了单轮链路，多轮链路由 `yi-agent-core` 既有 `tool_loop_*` 测试保证。
