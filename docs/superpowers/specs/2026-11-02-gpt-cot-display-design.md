# 设计：GPT（Responses 通道）思考块空白的根因修复与降级渲染

日期：2026-11-02
状态：待评审
关联：`2026-10-12-reasoning-effort-and-responses-api-design.md` §2 目标 4、§5.4；
`2026-10-15-cot-round-trip-and-anthropic-thinking-design.md`；
`docs/bug-list.md`

## 1. 问题

用户反馈：在桌面 App 里使用 GPT 模型（`HORIZON-OG-Sol` / `HORIZON-OG-Luna`，
Responses 通道）时，「思考」区块**没有任何内容**。

现状表现有两种形态，会被误认为同一个问题：

- **形态 A（空块）**：界面出现「思考」折叠卡片，点开后是一片空白。
- **形态 B（无块）**：整轮完全没有「思考」卡片。

## 2. 根因（全部为实测与代码证据，非推测）

### 2.1 请求侧：`reasoning_effort` 为空 ⇒ 根本不请求摘要

`yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs:150-156`：

```rust
let reasoning = match req.params.disable_thinking {
    Some(true) => None,
    _ => req.params.reasoning_effort.map(|effort| Reasoning {
        effort,
        summary: "auto".into(),
    }),
};
```

`reasoning_effort` 为 `None` 时，整个 `reasoning` 块不发送，`summary:"auto"`
随之消失。而用户实际清单 `~/.yi-agent/models.json` 中
`HORIZON-OG-Sol` / `HORIZON-OG-Luna` 的 `reasoning_effort` 均为 `null`。

**上游实测**（直连用户配置的 `https://llmapi.horizon.auto/v1/responses`）：

| 请求体 | 返回的 reasoning item |
|---|---|
| 完全不带 `reasoning` | `summary: []`、`content: []`、`encrypted_content` 约 5010 字节，**零摘要增量** |
| `{ "effort": "high", "summary": "auto" }` | 有 `response.reasoning_summary_text.delta`（实测出过 79/82 条增量） |
| `{ "effort": "low", "summary": "auto" }` | 不稳定：Sol 4 次中 1 次有摘要、3 次 `summary` 为空 |
| `{ "summary": "auto" }`（不带 effort） | Luna 3/3 有摘要；Sol 3 次中 1 次有 |

结论：**摘要是否需要，由请求体决定**；且即使请求了摘要，上游也可能返回空
（`summary: []`，仅 `encrypted_content`）。所以「一定有内容」不可保证，
渲染层必须能表达「这次没有明文」。

### 2.2 展示侧：即使拿到了摘要，块级条目也会把内容清空

`yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs:153-171`，块级
`ThinkingBlock` 的 `text` **恒为空串**（摘要早已由
`response.reasoning_summary_text.delta` 走 `ReasoningDelta` 旁路下发）：

```rust
events.push(ProviderEvent::ThinkingBlock {
    text: String::new(),          // <- 摘要不在这里
    token: ThinkingToken::Encrypted { encrypted_content, summary },
});
```

`yi-agent-rs/crates/yi-agent-app-server/src/translate.rs:448-472` 直接用这个空
`text` 生成 `Item::Thinking { text: "", redacted: false }` 并下发
`item/started` + `item/completed`。

前端 `desktop/src/lib/session.ts:537-560` 的「就地接管」逻辑会把这个条目**整体
替换**掉正在流式累积的本地推理块：

```ts
const held = this.items.findIndex((i) => i.id === this.openReasoning!.id);
if (held >= 0) {
  this.items[held] = item;   // item.text === ""，已收到的摘要被清空
  ...
}
```

这就是**形态 A**：卡片存在、点开空白。

### 2.3 对照：DeepSeek 通道没有此问题

`yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs:156-161` 在流结束时把
累积的 `reasoning_content` 放进 `ThinkingBlock.text`，接管后仍是明文。

### 2.4 其它已被覆盖的路径

- TUI 回放 `yi-agent-rs/crates/yi-agent/src/tui/replay.rs:64-73` 已跳过
  `redacted || text.is_empty()` 的条目，**不会**出现空白块；桌面端没有对应判断。
- 设计文档 §2 目标 4 要求「Luna 这类无明文的显示『已推理（reasoning tokens: N）』
  占位」，但全仓库检索 `已推理` **零匹配**，该占位从未实现。

## 3. 决策范围（已与用户确认）

用户选择方案 A + 选项 1：

- **方案 A**：让「有没有内容」决定渲染；同时修根因（让摘要能到达块级条目）。
- **选项 1**：**不动请求侧**。不改变「`reasoning_effort` 为 `None` 就不发
  `reasoning` 块」的既有契约，因此
  `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:715`
  的 `unset_reasoning_effort_stays_none` 保持不变。用户若想拿到摘要，需要在模型
  设置里为该条目选一个 effort 档位（`summary:"auto"` 随行发出）。

**非目标：**

- 不为缺省 effort 自动补 `summary:"auto"`（被选项 1 排除）。
- 不改变凭证回传逻辑（`encrypted_content` / `summary` 的逐字保真契约不动）。
- 不隐藏 GPT 的思考块（用户明确否决「纯隐藏」）。
- 不实现「reasoning tokens 占位」所需的跨层字段传递——本期降级文案用静态
  「已推理（无明文）」，见 §5.3 的取舍说明。

## 4. 方案

### 4.1 后端：让块级条目携带本次推理的明文

**改动点：`openai/responses/stream.rs`**

Responses 流解析器累积本轮的摘要文本，并在 `response.output_item.done` 生成
`ThinkingBlock` 时填入 `text`：

- 新增字段 `reasoning_buffer: String`，在
  `response.reasoning_summary_text.delta`（以及已有分支的
  `response.reasoning_text.delta`）处 `push_str`。
- 在构造 `ThinkingBlock` 的**同一处** `std::mem::take(&mut self.reasoning_buffer)`
  作为 `text`，与 `openai/stream.rs:156-161` 的 DeepSeek 处理保持同构。
- 缓冲为空的场景（上游返回 `summary: []`）→ `text` 仍为空串，语义与今天一致，
  由渲染层降级。

**为什么放在流解析器而不是 translate 层**：`ReasoningDelta` 与 `ThinkingBlock`
是同一次上游事件流的两个投影，只有解析器能保证「这一轮的摘要」与「这一轮的
reasoning item」严格对应；放 translate 层需要额外的跨轮归属状态，且 TUI 直接
消费 `ThinkingBlock`，放在解析器三端自动一致。

**必须保持的不变式**：凭证（`encrypted_content`、`summary` 原文）逐字不变；
`ReasoningDelta` 的实时下发行为不变（前端仍能流式看到思考过程，不等块结束）。

### 4.2 前端：接管时不丢已有明文

**改动点：`desktop/src/lib/session.ts` 的 `item/started` 分支**

当前无条件 `this.items[held] = item`。改为：

- 若 `item.text` 非空 → 照旧整体替换（服务端为权威）。
- 若 `item.text` 为空且本地块 `this.openReasoning.text` 非空 → **保留本地文本**，
  但仍把该条目的 `id` 记为服务端 id（使配对的 `item/completed` 按 id 就地替换，
  而不是误吞后续本地块）。实现上替换为
  `{ ...item, text: <held text> }`，保持 `type/id/redacted` 来自服务端。
- 若两者都为空 → 照旧替换（交给渲染层降级）。

理由：空 `text` 不携带任何信息，用它覆盖非空文本是纯损失；而 id 绑定是「不重复
渲染同一服务端条目」的必要条件，必须保留。

### 4.3 前端：空明文/redacted 的降级渲染

**改动点：`desktop/src/components/ChatView.tsx` 的 `ReasoningBlock`**

当前 `redacted` 显示「已加密的思考」，其它一律渲染 `text`（可能是空串，得到
空白）。改为三态：

| `redacted` | `text` | 渲染 |
|---|---|---|
| `true` | — | 「已加密的思考」（不变） |
| `false` | 非空 | 原文（不变） |
| `false` | 空 | 「已推理（无明文）」 |

实现上只在展开区做分支，折叠头、「思考」标题与 `text-fg-subtle` 样式不变；
`data-testid="reasoning-block"` 保留，避免既有测试失效。

与服务端的历史回放（`Item::Thinking`）共用同一组件，因此**回放期也自动修好**，
不需要单独改回放路径。

### 4.4 TUI：无需改动

`replay.rs` 已跳过空/redacted 条目，实时路径的 `ReasoningDelta` 直接累积明文。
本次后端改动只会让 `ThinkingBlock.text` 从「恒空」变成「有值」，TUI 本身不消费
该字段用于展示（`history.rs:1265` 为空分支），故无行为变化。

## 5. 边界与取舍

### 5.1 上游返回空摘要时的用户体验

用户不配 effort，或上游该轮未产出摘要（实测 Sol 有此情况）时，看到的是一行
dim 的「已推理（无明文）」，而不是空白卡片。这诚实表达了「模型思考过，但本次
没有可展示的明文」，与 TUI 的取舍（干脆不显示）不同——桌面端保留占位是用户的
选择（方案 A）。

### 5.2 为什么不显示 reasoning tokens 数值

设计文档 §2 目标 4 原意是「已推理（reasoning tokens: N）」。实现它需要把
`TokenUsage.reasoning_tokens` 从核心层一路带到 `Item::Thinking`
（`protocol.rs:864` 当前只有 `id/text/redacted`），并处理「块条目在
`item/started` 时 usage 尚未到达」的时序问题（该块先于 `response.completed`
下发，数值只能事后补推一条 `item/completed`）。

本期**不做**：这是独立的数据管线改动，与「空白块」这个缺陷不成比例。文案取
静态的「已推理（无明文）」。若后续确有需求，另立 spec。

### 5.3 与「缺省不发 reasoning」契约的关系

选项 1 明确不改请求侧。因此**不改** `types.rs:150-156`，也**不改**
`bootstrap.rs` 的 `unset_reasoning_effort_stays_none`。用户在模型设置里给 GPT
条目选 effort 后才有摘要；不选则稳定显示占位。

## 6. 影响面

| 位置 | 改动 |
|---|---|
| `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs` | 累积摘要并填入 `ThinkingBlock.text` |
| `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs` | 不改（选项 1） |
| `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs` | 不改（已透传 `text`） |
| `desktop/src/lib/session.ts` | 接管时保留非空本地文本 |
| `desktop/src/components/ChatView.tsx` | 空明文降级为「已推理（无明文）」 |
| `desktop/src/lib/generated/Item.ts` | 不改（形状不变） |

## 7. 测试

**Rust（`cargo test -p yi-agent-llm`）**

1. `reasoning_summary_deltas_are_carried_into_the_thinking_block`：喂入
   `response.reasoning_summary_text.delta` × N 与带 `encrypted_content` 的
   `output_item.done`，断言 `ThinkingBlock.text` 等于拼接后的摘要，且
   `token` 的 `encrypted_content` / `summary` 逐字不变。
2. `empty_summary_yields_an_empty_thinking_block_text`：只喂
   `output_item.done`（`summary: []`），断言 `text == ""`（保住降级路径）。
3. 既有用例 `reasoning_delta_becomes_a_reasoning_notification`、
   `reasoning_item_done_captures_summary_verbatim`、
   `replayed_reasoning_item_always_carries_a_summary_field` 必须继续全绿。

**前端（`npx vitest run`）**

4. `session.test.ts`：本地块已有 `"T1aT1b"` 时收到 `text: ""` 的
   `item/started`，断言条目仍保留 `"T1aT1b"`，且 `id` 已换成服务端 id；随后的
   `item/completed` 按 id 就地替换、不产生重复。
5. `session.test.ts`：服务端 `text` 非空时仍以服务端为准（覆盖「不丢权威更新」）。
6. `ChatView.test.tsx`：`{ type: "thinking", text: "", redacted: false }` 展开后
   可见「已推理（无明文）」，且不出现空字符串文本节点。
7. 既有「已加密的思考」「折叠可展开」「回放卡片」用例继续全绿。

**端到端（人工，非 CI）**：在 App 里用 `HORIZON-OG-Sol` + effort `high` 跑一轮
需要推理的提问，确认「思考」块展开后有摘要；再用不配 effort 的条目跑一轮，确认
显示「已推理（无明文）」而非空白。

## 8. 验收判据

- `cd yi-agent-rs && cargo test -p yi-agent-llm`：全绿，含 §7 的新增两条。
- `cd desktop && npx vitest run src/lib/session.test.ts src/components/ChatView.test.tsx`：全绿，含 §7 的新增三条。
- `cd desktop && npx tsc --noEmit`：无错误。
- `cd yi-agent-rs && just ci`：全绿。
- 人工验收：两种配置下分别看到「有摘要」与「已推理（无明文）」，**都不再出现空白
  卡片**。
