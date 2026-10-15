# 设计：CoT 规范回传 + Anthropic adaptive thinking

日期：2026-10-15
状态：待评审
关联：`2026-10-12-reasoning-effort-and-responses-api-design.md`（本设计推翻其 §6.3
的一处决策）、`docs/bug-list.md`

## 1. 背景

### 1.1 现状回顾

`2026-10-12-reasoning-effort-and-responses-api-design.md` §6.3 把「CoT 回灌进多轮
上下文」列为明确非目标，理由是多轮回灌会导致内容重复与重复计费。该决策对**跨轮
回灌**成立，但它把两件事合并成了一个决定，遗漏了**同一轮工具循环内的回传**：

- **跨轮回灌**：确为成本取舍，可选。
- **轮内工具回传**：对 Anthropic 与 DeepSeek 是**协议硬要求**，不回传即 400 或
  静默丢弃；对 OpenAI Responses 是官方强烈建议，直接影响推理连续性与 token 消耗。

### 1.2 三个通道的规范强度（本设计的分类基础）

| 通道 | 现状 | 规范要求 | 强度 |
|---|---|---|---|
| Anthropic | 未开启 thinking（无 `thinking` 字段、无 beta header、parser 不认 thinking 块） | 思考块必须**逐字原样**回传；工具循环内不回传即 400 | **硬要求** |
| DeepSeek chat | 未解析 `reasoning_content` | thinking mode + 工具调用时，`reasoning_content` 必须原样带回，否则 400；无工具调用时用后即弃 | **硬要求** |
| OpenAI Responses | 解析 reasoning 文本用于展示；`encrypted_content` 与 reasoning item 全部丢弃 | 「highly recommend」回传；不回传不报错，但模型从头重推、多烧 reasoning token | **强烈建议** |

### 1.3 关键前提：CoT 回传携带的是推理状态凭证

这是本设计所有约束的来源，必须写清楚，避免后续实现偏离。

回传进上下文的 CoT **不是**「让模型复盘自己之前想了什么」的语义记忆，而是
**一份不可重建的推理状态凭证**：

- Anthropic：`thinking` 块的 `signature` 字段是「encrypted copy of the full reasoning」。
  服务端解密 signature 以重建原始思考用于构造 prompt；`display:"omitted"` 时
  `thinking` 文本为空串，signature 仍承载全部信息。
- OpenAI：`encrypted_content` 是同类不透明句柄。
- 凭证与明文可能分离：`display:"omitted"` 下明文为空、凭证完整。**明文可丢，凭证不可丢。**

由此推出三条实现铁律：

1. **逐字保真**：凭证必须原样回传，不得重编码、不得过滤。官方明确「rebuilding the
   message or filtering out `redacted_thinking` blocks triggers a 400 error」。
2. **块结构必须保留**：即使明文为空串，`thinking` 块本身也必须存在且位置正确
   （Anthropic 要求思考块位于 tool_use 块之前）。
3. **凭证的生存期由协议决定**，不由展示需求决定。展示是副作用，不是目的。

### 1.4 前置问题：Anthropic `max_tokens` 必填

已核实：Anthropic Messages API 中 `max_tokens` 是**必填**字段，缺失返回
`400 {"error":{"message":"max_tokens: Field required"}}`。而本项目
`build_agent_config`（`yi-agent-runtime/src/bootstrap.rs:307`）构造的 `GenParams`
只填 `reasoning_effort`，`max_tokens` 为 `None`；`AnthropicRequest` 又是
`skip_serializing_if = "Option::is_none"`，即**该字段根本不会出现在请求体里**。

更关键的是：**thinking token 计入 `max_tokens`**（官方：「Thinking tokens count
toward `max_tokens`, so set it high enough to leave room for both the thinking and
the response text」）。因此若不设或偏小，开启 thinking 后**必然截断**。

本设计将「为 Anthropic 提供 `max_tokens` 默认值」列为**前置改动**（§5.1）。

## 2. 目标与非目标

### 目标

1. **Anthropic 接入 adaptive thinking**：新代模型（4.7+ / 5.x，含 Fable/Mythos）
   使用 `thinking: {type:"adaptive", display:"summarized"}`，不带 `budget_tokens`。
2. **轮内硬保证**：同一轮工具循环内，思考块与其凭证必须原样回传。三个通道分别达标。
3. **跨轮默认保留**：默认开启，可关闭（会话级）。
4. **DeepSeek chat 补全**：解析并回传 `reasoning_content`。
5. **OpenAI Responses 补全**：捕获并回传 reasoning item + `encrypted_content`。
6. **effort 接入 Anthropic**：`reasoning_effort` 同时映射到 `output_config.effort`。
7. **持久化与三端一致**：凭证跨进程存活；重开会话可回放思考卡片；新增 protocol Item。

### 非目标

- 不支持手动 extended thinking（`type:"enabled"` + `budget_tokens`）。用户明确不跑
  老模型；新代模型对该配置直接 400。
- 不发 `interleaved-thinking-2025-05-14` beta header。adaptive thinking 自动交错，
  无需该 header；且新代模型对该 header 无手动模式效果。
- 不实现 `previous_response_id` 服务端状态复用（沿用原设计的可移植性立场）。
- 不发送 `thinking.block_binding.prefix_mismatch_behavior` 与
  `thinking-binding-controls-2026-08-01` beta header（见 §4.3 的保守取舍）。
- 不改 OpenAI chat 通道的 `reasoning_effort` 探测/剥离逻辑（原设计已覆盖）。
- 不把 thinking 明文纳入 compaction 摘要内容（见 §4.2）。

## 3. 数据模型

### 3.1 `ContentBlock::Thinking`

在 `yi-agent-core/src/message.rs` 的 `ContentBlock` 增加变体：

```rust
Thinking {
    /// 可读摘要。`display:"summarized"` 时有值；`omitted` 或 redacted 时为空串。
    text: String,
    /// 不透明凭证。回传时逐字不变。
    token: ThinkingToken,
}

/// 推理状态凭证的形态。
///
/// 注意：`Signature` 与 `Redacted` 都是 Anthropic 的凭证载荷，但序列化成**不同**的
/// `type` 值（`thinking` vs `redacted_thinking`），故必须区分为两个变体——单靠
/// 「有没有凭证」无法决定回传时的 `type`。详见 §3.2。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ThinkingToken {
    /// Anthropic 明文 thinking 块（text 可有内容，display 决定）。
    Signature(String),
    /// Anthropic redacted_thinking 块（text 恒为空，只有 data）。
    Redacted(String),
    /// OpenAI Responses 的 encrypted_content。
    Encrypted(String),
    /// 无凭证。DeepSeek 的 reasoning_content 按规范不需回传，明文仅供展示。
    None,
}
```

**设计理由**

- 凭证与明文**分离存储**：因为两者生存期与保真要求不同（1.3 铁律）。用一个
  `signature: Option<String>` 会让「明文可丢、凭证不可丢」这条规则无处表达。
- `ThinkingToken` 用枚举而非裸 `String`：三个 provider 的凭证语义不同（Anthropic
  的 signature 由服务端解密、OpenAI 的 encrypted_content 是独立 item 的载荷、
  DeepSeek 无凭证）。枚举让序列化分支明确，避免把 DeepSeek 的空凭证写成
  `"signature": ""` 这类会被 Anthropic 拒绝的形态。
- `Redacted` 必须是独立变体：Anthropic 的 `redacted_thinking` 块结构是
  `{type, data}`（无 `thinking` 字段），与 `{type, thinking, signature}` 是两种
  不同的 wire 形状。虽然二者都携带不透明凭证，但**回传时的 `type` 不同**，因此
  不能合并为单个 `Signature` 变体。

### 3.2 序列化契约

- **向后兼容**：旧会话 transcript 不含 `Thinking` 变体，反序列化天然安全。
- **保真**：`Thinking` 必须无损往返。`text` 允许为空串，凭证必须逐字保留。
- **`type` 的判别依据**：由 `ThinkingToken` 变体决定，不由 `text` 是否为空决定
  （`text` 为空可能是 `omitted` 的普通 thinking，也可能是 redacted，二者 wire 形状
  不同）。

### 3.3 编译期强制（改动面清点）

新增变体会使所有穷尽 `match ContentBlock` 处编译失败。这是**有意利用**的机制：
编译器强制逐点决定思考块的处理方式。已清点的非测试受影响点：

| 位置 | 处理方式 |
|---|---|
| `yi-agent-core/src/compact.rs:95`（token 估算） | 按 `text` 长度估算；凭证不计入（无法估算且不该计入用户预算） |
| `yi-agent-core/src/compact.rs:412`（摘要渲染） | 渲染为 `"[思考]"`，**不**把明文喂给摘要模型（见 §4.2） |
| `yi-agent-core/src/agent.rs:1672`（prefill 估算） | 同 compact：按 `text` 估算 |
| `yi-agent-app-server/src/translate.rs:481` `render_content`（渲染） | 渲染为 `"[thinking]"` 占位，与 `[image]` 同构（见 §6.1） |
| `yi-agent-mcp/src/manager.rs:536` | 不入 MCP 转换路径（MCP 无此概念） |
| `yi-agent-tools/src/fs/view_image.rs:113` | 无关，补 `_ => {}` 分支 |

**注意**：`estimate_prefill_tokens` 与 compact 的 token 口径必须一致，否则状态栏
估算与实际上下文漂移。

## 4. 跨轮保留与 compaction

### 4.1 保留开关

- 新增会话级开关 `preserve_thinking`，默认 `true`。
- 复用现有 `reasoning_effort` 的覆盖机制：`thread_store` 的 `ThreadMeta` 加字段 +
  `SessionCommand` 加变体 + app-server 重建 agent（与 `SetReasoningEffort` 同构，
  见 `yi-agent-app-server/src/session.rs:86`）。
- 关闭时的语义：构造请求前**剥离所有历史轮**的 `Thinking` 块，仅保留当前轮工具
  循环内产生的。**必须同时作用于落盘的 transcript**，否则重开会话时被剥离的块
  「复活」。
- `None`/缺省 = `true`（零迁移：旧 meta 无此字段时按默认开启解释）。

### 4.2 compaction 冲突（本设计最高风险项）

**冲突陈述**：Anthropic 的 prefix 校验要求思考块的前缀（顶层 `system`、`tools`、
`messages` 中该块之前的部分）在两次请求间不变；前缀一变，该块及其后**所有**思考
块失效。而 `build_compacted_messages`（`yi-agent-core/src/compact.rs:279`）的本质
就是重建历史：摘要 + 保留后缀。

**时效性**：官方明确「The API enforces the prefix check by default for accounts
created on or after August 31, 2026」。今天是 2026-10-15，**新建账号默认开启**，
故这是当期问题而非未来问题。

**处理方案（三层）**

1. **剥离被压缩区间的思考块**。压缩语义是「历史被摘要替代」，被替换掉的轮次其推理
   凭证已失去对应上下文，保留必然触发前缀失效。`plan_compaction` 选中的保留区间
   之外的 `Thinking` 块一律丢弃。
2. **保证保留后缀自其首个思考块起 append-only**。摘要**不得**插入保留区间内某个
   assistant 消息的 content 数组中间——那会改变该消息的块序列，破坏前缀。当前
   `build_compacted_messages` 的 `first.content.insert(0, summary)` **正是这种插入**，
   必须改造：摘要应作为**独立的消息/块**置于保留后缀之前，而非插入既有 assistant
   消息内部。

   **这一改动影响所有 provider**（不只 Anthropic），是本设计中波及面最大的一处，
   需重点评审与测试。
3. **不做 `prefix_mismatch_behavior` 兜底**（见 §4.3）。

**明文不进摘要**：`format_messages_for_summary` 不得把 thinking 明文纳入摘要
输入。理由：凭证不会被摘要模型重现，摘要里出现推理内容会造成「摘要声称有、实际
凭证已丢」的语义错位，且徒增摘要成本。

### 4.3 保守取舍：不发 `prefix_mismatch_behavior`

`thinking.block_binding.prefix_mismatch_behavior` 允许显式选择「丢弃」而非「报错」，
但它需要 beta header（`thinking-binding-controls-2026-08-01`），且**是否被网关透传
未经实测**。若网关走 allow-list 剥字段，发了也是白发。

**决策**：默认不发，正确性由 §4.2 第 1、2 层保证。把它登记为后续可选增强，前提是
实测确认透传。

### 4.4 用户可见性

压缩会常态触发（请求体积变大、更快撞阈值），且每次伴随推理连续性损失。**不得静默**。

- 压缩发生时 emit `AgentEvent::Notice`（复用现有机制），文案说明「已压缩，历史推理
  凭证已释放」。
- 关闭 `preserve_thinking` 时不发该 Notice（无凭证可释放）。

## 5. 请求构造与流解析

### 5.1 Anthropic 请求侧

**前置改动（§1.4）**：为 Anthropic 路径提供 `max_tokens` 默认值。默认值应足够容纳
thinking + 响应文本。实现位置：`build_agent_config` 或 provider 适配层（
`AnthropicGenParams::from(GenParams)`）补默认——**推荐后者**，理由：这是 Anthropic
特有约束，不该污染 provider 中立的 `GenParams`。

**新增请求字段**

```rust
// AnthropicRequest 新增
#[serde(skip_serializing_if = "Option::is_none")]
pub thinking: Option<ThinkingConfig>,      // {type:"adaptive", display:"summarized"}
#[serde(skip_serializing_if = "Option::is_none")]
pub output_config: Option<OutputConfig>,   // {effort:"<档位>"}
```

- `type` 恒为 `"adaptive"`；不发 `budget_tokens`。
- `display` 恒为 `"summarized"`（决策见 §5.4）。
- `effort` 来自 `GenParams.reasoning_effort`，原样映射。`None` = 不发该字段，沿用
  模型默认档（零回归）。

**消息回传**：`From<ProviderRequest> for AnthropicRequest` 的 assistant 分支：

```
ContentBlock::Thinking { text, token } =>
    match token {
        Signature(s) => {"type":"thinking", "thinking":text, "signature":s},
        Redacted(d)  => {"type":"redacted_thinking", "data":d},
        Encrypted(_) => unreachable!(Anthropic 通道不产生此形态),
        None         => 不回传（DeepSeek 专用形态）,
    }
```

**位置约束**：思考块必须保持在同一条 assistant 消息内**位于 tool_use 块之前**。
现有转换按 `m.content` 顺序遍历，只要我们在累积时把思考块 push 在 tool_use 之前，
顺序自然正确。**需要一个测试锁定这一点。**

### 5.2 Anthropic 流解析

现有 parser（`yi-agent-llm/src/anthropic/stream.rs`）完全不认 thinking。新增：

| SSE 事件 | 处理 |
|---|---|
| `content_block_start` with `type:"thinking"` | 开启 thinking 块（记 index → 块类型） |
| `content_block_start` with `type:"redacted_thinking"` | 开启 redacted 块 |
| `content_block_delta` with `thinking_delta` | 累积明文；emit `ProviderEvent::ReasoningDelta{kind:Summary}` |
| `content_block_delta` with `signature_delta` | **累积 signature 片段** |
| `content_block_stop`（thinking 类块） | 产出 `ContentBlock::Thinking` |

**最高风险点**：signature 通过 `signature_delta` **分段流式**到达，必须完整拼接。
少一段即回传失败（且失败形态可能是 400 或静默失效）。**需要针对分段 signature 的
专门测试**，包括 signature 被切成多片、以及 thinking 块出现在 tool_use 之前的顺序。

**redacted 处理**：`redacted_thinking` 块的 `data` 在 `content_block_start` 里一次
给出（非 delta），直接落 `ThinkingToken::Redacted`。

**stop_reason**：现有映射表已含 `"max_tokens"`；新增 `"model_context_window_exceeded"`
若出现则需归类（先归 `Other`，实测后调整）。

### 5.3 DeepSeek chat 与 OpenAI Responses

**DeepSeek chat**（`yi-agent-llm/src/openai/stream.rs`）

- 解析 `delta.reasoning_content` → 累积进 `ContentBlock::Thinking{token: None}` +
  emit `ReasoningDelta{kind:Content}`。
- **回传条件**：仅当该 assistant 消息**包含 tool_calls** 时才序列化
  `reasoning_content` 字段。官方规则：「没有工具调用 → reasoning_content 用后即弃；
  有工具调用 → 必须原样带回」。无工具调用时回传多余字段可能被拒。
- 序列化位置：`OpenaiMessage` 增加 `reasoning_content: Option<String>`，
  `skip_serializing_if = "Option::is_none"`。

**OpenAI Responses**（`yi-agent-llm/src/openai/responses/`）

- stream parser 的 `response.output_item.added` / `.done` 分支识别 reasoning item，
  捕获 `encrypted_content`。
- `responses_request_from` 在 assistant 分支，于 `function_call` item **之前**插入
  reasoning item（携带 `encrypted_content`）。
- **诚实标注**：此通道官方措辞是 "highly recommend"，不回传**不报错**。因此这是
  效果优化，**不是 bug 修复**。验收时不得声称「修好了 X」，除非实测能给出
  reasoning token 下降的数据。

### 5.4 `display` 决策

选 `"summarized"`。理由：

- 项目已有完整 CoT 展示链路（TUI dim 思考块 + `/reasoning` 折叠、app-server
  `thread/reasoningDelta`、桌面端折叠卡片）。新模型默认 `"omitted"` 会让这套设施
  拿到的全是空串。
- 成本不变：`omitted` 同样按 output token 计费，省的只是首字延迟。
- 若后续发现首字延迟不可接受，`display` 可作为配置项暴露（本期先硬编码
  `"summarized"`）。

## 6. 持久化与三端一致

### 6.1 推翻原设计的「不进 transcript」决策

`yi-agent-app-server/src/translate.rs:306` 现有注释：「Display-only CoT. It is
deliberately NOT merged into the open agent message: reasoning never enters the
transcript」。**本设计推翻它。**

**理由**：凭证必须跨进程、跨重启存活。否则重开会话时回传的是残缺历史——块在但
signature 丢了——反而触发 400。持久化不是展示需求，是**正确性需求**。

**改动**：`ReasoningDelta` 的处理从「只发通知」改为「发通知 + 累积进当前 assistant
消息的 `Thinking` 块」。

- 通知**照发**（前端实时展示不受影响）。
- 需区分**展示增量**（推给前端的 `ReasoningDelta`）与**落盘凭证**（留在服务端消息
  的 `Thinking` 块）。
- 必须在 spec/代码注释中显式记录这个反转及其理由，不得静默改掉。

### 6.2 transcript 落盘

`thread_store` 存的是 `Vec<Message>`，`Thinking` 变体随消息自然序列化，
**存储格式本身无需改动**。需要改的是写入时机（§6.1）与 §4.1 的剥离逻辑。

### 6.3 protocol 新增 `Thinking` Item

用户选定方案 (ii)：新增 Item，使重开会话能回放思考卡片。

```rust
// protocol.rs 的 Item 增加
Thinking {
    id: String,
    /// 展示用明文（可为空串）。
    text: String,
    /// 凭证是否 redacted（决定前端是否渲染"已加密"占位）。
    redacted: bool,
    // 注意：不暴露凭证本体给前端（见下）
},
```

**凭证不下发前端**：`signature` / `encrypted_content` 是服务端回传给 API 的凭证，
前端渲染不需要、也不应持有。Item 只携带展示所需字段，凭证留在服务端消息模型里。
这既减小线协议体积，也避免凭证经客户端往返被意外改写。

**落盘与回放**：`translate.rs` 在 thinking 块完成时产出该 Item；`thread_store` 按
现有 item 机制落盘；重开会话时回放渲染折叠卡片。

### 6.4 三端影响

| 端 | 改动 |
|---|---|
| TUI | `ReasoningDelta` 事件形状不变 → **无需改动**；开 `summarized` 后思考块终于有内容 |
| 桌面端 | 实时展示无需改；**历史回放需支持 `Thinking` Item** |
| iOS | 同桌面端 |
| `ReasoningKind` | 复用现有枚举：Anthropic = `Summary`，DeepSeek = `Content`。不新增 |

## 7. 测试策略

### 7.1 凭证保真（最高优先级）

- **Anthropic 分段 signature**：signature 被切成多片 delta → 拼接后逐字等于原始值。
- **逐字往返**：构造含 `Thinking` 的 `ProviderRequest` → 序列化 JSON → 断言
  `signature`/`data` 与输入逐字相同，且 `type` 为 `thinking`/`redacted_thinking` 正确。
- **块顺序**：含 thinking + tool_use 的 assistant 消息，序列化后 thinking 必须在
  tool_use 之前。
- **明文可丢、凭证不可丢**：`display:"omitted"` 场景（text 为空串）的块仍完整回传。

### 7.2 轮内回传

- **Anthropic**：脚本化 provider，第一轮返回 thinking + tool_use，第二轮请求断言
  思考块原样出现。
- **DeepSeek**：有 tool_calls 时序列化含 `reasoning_content`；无 tool_calls 时不含。
- **Responses**：reasoning item 在 `function_call_output` 之前出现。

### 7.3 compaction（高风险）

- 压缩后**被压缩区间**无 `Thinking` 块残留。
- 压缩后保留后缀的 thinking 块前缀自其起 append-only（摘要未插入 assistant 消息
  内部）。
- 压缩触发的 `Notice` 在 `preserve_thinking=true` 时发出、`false` 时不发。
- **跨 provider 回归**：改造后的 `build_compacted_messages` 对 OpenAI 路径行为不变。

### 7.4 开关与持久化

- `preserve_thinking` 默认 `true`（旧 meta 无字段 → true）。
- 关闭时历史轮思考块被剥离，且**落盘结果同样被剥离**。
- 会话重开后凭证完整（往返一致）。

### 7.5 `max_tokens`

- Anthropic 请求体**必定**含 `max_tokens`（非 None）。
- 值与 model 上限的关系有测试锁定（不与"必要空间"冲突）。

## 8. 风险与未决

| 风险 | 说明 | 缓解 |
|---|---|---|
| compaction 改动波及所有 provider | §4.2 第 2 层改的是核心逻辑 | 专项测试 + 破坏性评审；分步提交便于二分定位 |
| signature 分段拼接 | 少一段即失败，且失败形态可能静默 | 专门测试；实现时把拼接做成单一聚合点 |
| `prefix_mismatch_behavior` 不透传 | 未实测 | 保守不发；登记为后续项 |
| Responses 回传无报错反馈 | 做错了不会报错，只会更贵 | 需实测 token 数据才能声称收益 |
| `max_tokens` 默认值选取 | 过小截断、过大浪费配额（该字段是**预留**） | 按模型上限设定并在 catalog 可覆盖 |
| DeepSeek 回传条件 | 绑 tool_calls 的存在 | 针对性测试；并在真实端点实测一次 |

**未决问题**

1. Anthropic `max_tokens` 默认值的具体数值（取决于目标模型的输出上限）。
2. `model_context_window_exceeded` stop_reason 的归类（实测后定）。
3. `prefix_mismatch_behavior` 是否在你们的网关上被透传（需实测）。
4. §1.2 表格中「DeepSeek chat + tools 全档 200」与官方硬要求存在张力——**建议实测
   一次**：若真实端点进入 thinking mode 且执行校验，则本设计的 §5.3 是修 bug；
   若网关不做校验，则它是防御性实现。

## 9. 交付顺序建议

1. **数据模型**（§3）+ 编译期强制补齐所有 match → 独立可提交，行为零变化。
2. **Anthropic 请求侧 + `max_tokens`**（§5.1、§1.4）→ 请求形状正确。
3. **Anthropic 流解析**（§5.2）→ 能产生并回传思考块。**此步结束即可端到端验证工具循环。**
4. **DeepSeek chat**（§5.3）→ 独立，风险低。
5. **OpenAI Responses**（§5.3）→ 独立，风险低。
6. **compaction 改造**（§4.2）→ 风险最高，单独提交 + 重点评审。
7. **持久化 + protocol Item**（§6）→ 依赖 1-3。
8. **开关 + 三端回放**（§4.1、§6.3）。