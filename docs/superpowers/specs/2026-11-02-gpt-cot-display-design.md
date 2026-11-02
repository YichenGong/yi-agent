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
替换**掉正在流式累积的本地推理块（`this.items[held] = item`，第 552 行）。由于
`item.text` 是空串，已经流式收到的摘要被清空：

```ts
const held = this.items.findIndex((i) => i.id === this.openReasoning!.id);
if (held >= 0) {
  this.items[held] = item;   // item.text === ""，已收到的摘要被清空
  ...
}
```

这就是**形态 A**：卡片存在、点开空白。

**并且配对帧会再清一次**：紧随其后的 `item/completed` 携带同一个空 `text`
条目，走下面的「按 id 就地替换」分支（第 568 行附近）再做一次 `this.items[index]
= item`。所以只修接管那一处不够——实测确认（2026-11-02，用临时 vitest 用例驱动
真实 `Session`）：`item/started` 清空流式摘要；即使人为把文本补回，
`item/completed` 仍会再次清空。

### 2.3 对照：DeepSeek 通道没有此问题

`yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs:156-161` 在流结束时把
累积的 `reasoning_content` 放进 `ThinkingBlock.text`，接管后仍是明文。

### 2.4 DeepSeek 走 responses 通道时没有卡片（附带的既有行为）

实测 `HORIZON-DeepSeek` 在 `/v1/responses` 上返回 `summary`（374 字符）但
**`encrypted_content` 为空**，而 `stream.rs:153-157` 要求凭证非空才产出
`ThinkingBlock`（`.filter(|s| !s.is_empty())`），故该路径不产生 thinking 条目。
实时显示仍由 `ReasoningDelta` 正常呈现，只是历史回放没有卡片。

这与本缺陷无关，本期**不改**（空凭证不该回传，
`reasoning_item_without_encrypted_content_emits_no_block` 已钉住）。

### 2.5 其它已被覆盖的路径

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
  「已推理（无明文）」，见 §5.2 的取舍说明。

## 4. 方案

### 4.1 后端：让块级条目携带本次推理的明文

**改动点：`openai/responses/stream.rs` 的 `response.output_item.done` 分支**

该分支已持有上游 reasoning item 的完整 `summary` 数组。**实测确认**
（2026-11-02，`HORIZON-OG-Sol` + `effort:high` + `summary:auto`）：

- `response.reasoning_summary_text.done` 的 `text` == 所有 delta 的拼接（341 字符）；
- `output_item.done` 的 `item.summary[].text` 拼接 == 同上，**逐字相等**。

因此**不需要新增增量缓冲区**：直接把 `summary` 数组里的文本拼出来即可。现有代码
已在 `token` 里逐字带走 `summary`，改动最小形态是给同一个数组派生一个展示文本
（下图为示意，`summary` 的取法沿用现有 `item.get("summary")`）：

```rust
// 现状：summary 已由 item.get("summary") 逐字带进 token
let summary: Vec<Value> = item
    .get("summary")
    .and_then(Value::as_array)
    .cloned()
    .unwrap_or_default();
// 新增：同一份数组派生出展示文本（不修改 summary 本身）
let text: String = summary
    .iter()
    .filter_map(|p| p.get("text").and_then(Value::as_str))
    .collect();

events.push(ProviderEvent::ThinkingBlock {
    text,
    token: yi_agent_core::ThinkingToken::Encrypted {
        encrypted_content: enc.to_string(),
        summary,          // 原文逐字，不受 text 影响
    },
});
```

要点：

- `token.summary` 仍是**上游原文逐字**（保留 `type` 字段与分段结构），供回放
  使用；`text` 是给展示用的拼合结果，二者互不影响。实现时**不得**把
  `text` 结构化后写回 `summary`。
- `summary` 为空数组 → `text` 为空串，行为与今天一致，由渲染层降级（§4.3）。
- **不要**把 `response.reasoning_text.delta`（`content`）也并入 `text`：实测
  DeepSeek / Sol 在 responses 通道的 `content` 均为空数组，该事件计数为 0；
  若将来遇到只发 `content` 的模型，`summary` 与 `content` 是两份不同内容，
  拼在一起会串味。那种模型的实时显示仍由 `ReasoningDelta` 负责，
  块级 `text` 为空则回放期显示「已推理（无明文）」——可接受，且不在本期范围。

**为什么放在解析器而不是 translate 层**：`item.summary` 只在该帧可见；
translate 层只拿到 `ThinkingBlock`，放那里需要额外的跨轮归属状态。放解析器后，
TUI / 桌面 / 未来客户端自动一致。

**必须保持的不变式**：凭证（`encrypted_content` 与 `summary` 原文）逐字不变；
`ReasoningDelta` 的实时下发行为不变（前端仍能边流边看，不等到块结束）。

### 4.2 前端：接管时不丢已有明文

**改动点：`desktop/src/lib/session.ts`，三处按 id 替换点**

`item/started` 与 `item/completed` 走**同一个 case 分支**，且配对帧会再次用
同一个空 `text` 的 item 就地替换。所以只在「接管」分支改是不够的——必须在
**所有按 id 替换**的地方统一守卫：

1. `apply` 的接管分支（`this.items[held] = item`，第 552 行）；
2. `apply` 的按 id 替换（`this.items[index] = item`，第 563 行）——配对的
   `item/completed` 走这里；
3. `upsertItems` 的按 id 替换（`this.items[index] = normalized`，第 462 行）
   ——回放补帧走这里。

统一规则（抽成一个私有方法 `mergeThinkingText(existing, incoming)`）：

- `incoming.type !== "thinking"` → 直接用 `incoming`。
- **`incoming.redacted === true` → 直接用 `incoming`，不保留任何文本。**
  这是安全边界：若后到帧把块标记为 redacted，沿用旧明文等于把凭证/明文泄漏到
  本应无明文的占位上。
- `incoming.text` 非空 → 用 `incoming`（服务端有内容时是权威）。
- `incoming.text` 为空 → 返回 `{ ...incoming, text: existingText }`；其中
  `existingText` 取**同 id 条目的现有文本**。该条目有两种形态，都要读：
  接管那一刻是本地 `ReasoningItem`（`type: "reasoning"`，与 `this.openReasoning`
  是同一对象引用，流式摘要就在它的 `text` 里）；配对 `completed` 与 `upsertItems`
  时是已接管的 `thinking` 条目。只认 `type === "thinking"` 会让接管那一步取到
  空串，等于没修。读 `existing.text` 必须在 `breakReasoningRun()` **之前**完成
  （该调用会把 `openReasoning` 置 `null`，但 `items` 里的引用仍在——所以实现上
  直接从 `items` 取，不依赖 `openReasoning`）。

这样 `started` 空文本不会清掉本地流式摘要，且随后的 `completed`（同样是空
文本）也不会把它再清一次。

**注意**：text 被补回后仍必须让比较器看见变化（`structuralVersion += 1` 或
`lastText` 相关路径），否则 UI 不会重绘——实现时沿用各分支现有的失效方式。

理由：空 `text` 不携带任何信息，用它覆盖非空文本是纯损失；而 id 绑定是
「不重复渲染同一服务端条目」的必要条件，必须保留。

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

`replay.rs:64-73` 已跳过 `redacted || text.is_empty()` 的条目，实时路径的
`ReasoningDelta` 直接累积明文。后端改动只会让 `ThinkingBlock.text` 从「恒空」
变成「有值」；`history.rs:1265` 对 `ThinkingBlock` 是空分支，故 TUI 无行为变化。

**但 TUI 因此与桌面端不一致**：同样是「模型推理过、本次无明文」，TUI 什么都不
显示，桌面端显示「已推理（无明文）」。这是有意的取舍（§5.4），不是遗漏。

### 4.5 落盘与回放：空文本条目会被持久化

`Item::Thinking` 经 `server.rs` 的 `completed_items` 进
`ThreadStore::append_turn` 落盘（`thread_store.rs:3003` 的
`thinking_item_round_trips_and_stays_credential_free` 钉住了这一点），冷启动
回放时原样读回。因此：

- 本轮修复**不改变落盘形状**（仍是 `id/text/redacted`，无凭证）。
- 已产生的历史轮次里若存在 `text: ""` 的 thinking 条目，修复后会被渲染成
  「已推理（无明文）」而不是空白卡片——这正是期望行为，**不需要**迁移脚本。
- 新增的摘要文本会随新轮次一起落盘；旧文件不需要回填。

## 5. 边界与取舍

### 5.1 上游返回空摘要时的用户体验

用户不配 effort，或上游该轮未产出摘要（实测 Sol 有此情况）时，看到的是一行
dim 的「已推理（无明文）」，而不是空白卡片。这诚实表达了「模型思考过，但本次
没有可展示的明文」，与 TUI 的取舍（干脆不显示）不同——桌面端保留占位是用户的
选择（方案 A）。

**实测支撑「推理过就一定会有块」**：不带 `reasoning` 字段时，上游仍返回
reasoning item（`summary: []` + `encrypted_content` 5010 字节，无任何摘要增量）。
若某轮模型完全没推理，则上游不返回 reasoning item，也就没有卡片——不会出现
「每轮都冒出一个占位」的噪音。

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

### 5.4 桌面端与 TUI 的表现不一致（有意）

| 场景 | TUI | 桌面 App |
|---|---|---|
| 有明文摘要 | 折叠块，有内容 | 折叠块，有内容 |
| 推理过但无明文 | **不显示** | 「已推理（无明文）」 |
| `redacted`（Anthropic） | 不显示 | 「已加密的思考」 |

TUI 的取舍是「没有可读内容就不占屏幕」；桌面端按用户选择保留占位。两者的差异
在 spec 里显式记录，避免以后被当作 bug 修掉。

### 5.5 DeepSeek 走 responses 通道时的额外观察

见 §2.4：该模型在 responses 通道返回 `summary` 但无 `encrypted_content`，因凭证
过滤而不产出 thinking 条目（实时 `ReasoningDelta` 仍正常）。本期不改，属既有行为。

## 6. 影响面

| 位置 | 改动 |
|---|---|
| `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs` | 从 `item.summary[]` 拼出文本填入 `ThinkingBlock.text` |
| `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs` | 不改（选项 1） |
| `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs` | 不改（已透传 `text`） |
| `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` | 不改（`Item::Thinking` 形状不变） |
| `yi-agent-rs/crates/yi-agent/src/tui/replay.rs` | 不改（已跳过空文本） |
| `desktop/src/lib/session.ts` | 三处按 id 替换统一走「空文本不覆盖非空」守卫 |
| `desktop/src/components/ChatView.tsx` | 空明文降级为「已推理（无明文）」 |
| `desktop/src/lib/generated/Item.ts` | 不改（形状不变） |

## 7. 测试

**Rust（`cargo test -p yi-agent-llm`）**

1. `reasoning_item_summary_text_is_carried_into_the_thinking_block`：喂入带
   `summary: [{type:"summary_text",text:"thought A"},{type:"summary_text",text:" B"}]`
   与 `encrypted_content` 的 `output_item.done`，断言 `ThinkingBlock.text ==
   "thought A B"`，且 `token` 里的 `encrypted_content` 与 `summary` 数组**逐字
   不变**（`summary` 仍是带 `type` 的原始结构与两段，未被拼平）。
2. `empty_summary_yields_an_empty_thinking_block_text`：只喂
   `output_item.done`（`summary: []`）且 `encrypted_content` 非空，断言
   `text == ""`（保住降级路径）。
3. 既有用例必须继续全绿：
   `reasoning_item_done_emits_thinking_block_with_encrypted_content`、
   `reasoning_item_done_captures_summary_verbatim`、
   `reasoning_item_done_captures_an_empty_summary_as_an_empty_vec`、
   `reasoning_item_without_encrypted_content_emits_no_block`。

**Rust（`cargo test -p yi-agent-app-server`）**

4. 既有 `thinking_block_becomes_a_credential_free_thinking_item` 与
   `thinking_item_round_trips_and_stays_credential_free` 继续全绿（translate 未改）。

**前端（`npx vitest run`）**

5. `session.test.ts`：本地块已有 `"T1aT1b"` 时收到 `text: ""` 的
   `item/started`，断言条目仍保留 `"T1aT1b"`，且 `id` 已换成服务端 id。
6. `session.test.ts`：紧接着的配对 `item/completed`（同样 `text: ""`）不得再次
   清空——断言文本仍是 `"T1aT1b"`（这条覆盖 §4.2 的第 2 个替换点，是本 spec
   前一版遗漏的缺陷）。
7. `session.test.ts`：服务端 `text` 非空时以服务端为准（覆盖「不丢权威更新」）。
8. `session.test.ts`：`upsertItems` 收到空文本的同 id thinking 条目时不清空
   已有文本（覆盖回放补帧）。
9. `ChatView.test.tsx`：`{ type: "thinking", text: "", redacted: false }` 展开后
   可见「已推理（无明文）」。
10. `ChatView.test.tsx`：`text` 非空时仍渲染原文，且**不出现**「已推理（无明文）」
    ——护栏，防止占位文案顶掉真实摘要。
11. `session.test.ts`：已有明文 `"SUMMARY"` 的条目收到 `redacted: true` 的空文本
    帧时，**不得**保留旧明文（断言 `text === ""` 且 `redacted === true`）——覆盖
    §4.2 的安全边界。
12. 既有「已加密的思考」「折叠可展开」「回放卡片」「两轮 CoT 位置」用例继续全绿。

**端到端（人工，非 CI）**：在 App 里用 `HORIZON-OG-Sol` + effort `high` 跑一轮
需要推理的提问，确认「思考」块展开后有摘要；再用不配 effort 的条目跑一轮，确认
显示「已推理（无明文）」而非空白。

## 8. 验收判据

- `cd yi-agent-rs && cargo test -p yi-agent-llm`：全绿，含 §7 的新增两条。
- `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`：全绿。
- `cd desktop && npx vitest run`：全绿，含 §7 的新增七条（`session.test.ts` 五条 +
  `ChatView.test.tsx` 两条）。
- `cd desktop && npx tsc --noEmit`：无错误。
- `cd yi-agent-rs && just ci`：全绿。
- 人工验收：两种配置下分别看到「有摘要」与「已推理（无明文）」，**都不再出现空白
  卡片**。
