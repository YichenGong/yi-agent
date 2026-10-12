# 设计：reasoning effort 可配 + Responses API 通道（用户无感切换）

日期：2026-10-12
状态：待评审
关联：`docs/bug-list.md`（Luna 报 400 一条）、`2026-10-09-model-catalog-authority-design.md`

## 1. 背景与问题

### 1.1 直接触发：Luna 带 tools 必 400

用户在模型清单里新增了 horizon 网关的三个模型（`HORIZON-DeepSeek` /
`HORIZON-OG-Luna` / `HORIZON-OG-Sol`）。使用 Luna 时，**任何带 function tools
的请求**都失败：

```
provider error: invalid request: 400: {"error":{"message":"Function tools with
reasoning_effort are not supported for gpt-6-luna in /v1/chat/completions. To use
function tools, use /v1/responses or set reasoning_effort to 'none'."}}
```

### 1.2 根因（已实测复现，非推测）

用用户真实配置直连 `https://llmapi.horizon.auto` 复现：

| 请求 | 结果 |
|---|---|
| Luna + 无 tools | 200 |
| Luna + tools（不带 `reasoning_effort`） | **400**（上述报错原文） |
| Luna + tools + `reasoning_effort:"none"` | 200，正常返回 `tool_calls` |
| Luna + tools + `low`/`medium` | **400** |
| Sol + tools（不带 `reasoning_effort`） | **400**（`gpt-6.1-sol` 同族） |
| DeepSeek + tools | 200 |

**根因**：不是本客户端发了 `reasoning_effort`——`GenParams` 只有
temperature / max_tokens / top_p / stop（`yi-agent-core/src/provider.rs:25`），
客户端从不发该字段。是 **horizon 网关对 GPT-6 系列默认注入 reasoning**，而该
上游在 `/v1/chat/completions` 下**「function tools + reasoning_effort≠none」是
硬性不兼容**。

**并且主对话永远带 tools**：`agent.rs:921` 每次 think 都发
`tools: tools.schemas()`。只有 `compact.rs:455`（摘要）与 `title.rs:88`（标题）
不带。故 Luna/Sol 的主对话在 chat 通道下**必然 400**，唯一出路是显式传
`reasoning_effort:"none"` 或改走 `/v1/responses`。

### 1.3 第二个缺口：CoT 完全没接

网关的推理模型会产出思维链，本客户端**从不读取**（全量搜索 `reasoning_content`
在 `yi-agent-llm` / `yi-agent-core` 源码零匹配；`openai/stream.rs` 的
`parse_frame` 只取 `delta.content` 与 `delta.tool_calls`）。用户明确要求展示 CoT。

### 1.4 实测能力矩阵（决定设计的关键事实）

**effort 档位接受度矩阵**（实测 horizon，`✓`=200，`✗`=400）：

| 模型 | 端点 | none | minimal | low | medium | high | xhigh | max |
|---|---|---|---|---|---|---|---|---|
| DeepSeek | chat | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| DeepSeek | responses | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Luna | chat | ✓* | ✗ | ✓ | ✓ | ✓ | ✓ | ✗ |
| Luna | responses | ✓ | ✗ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Sol | chat | ✓* | ✗ | ✓ | ✓ | ✓ | ✓ | ✗ |
| Sol | responses | ✗ | ✗ | ✓ | ✓ | ✓ | ✓ | ✓ |

`✓*`（Luna/Sol 的 chat + `none`）**仅在请求不带 tools 时成立**；带 tools 时
只有 `none` 通过——这正是 1.1 报错的来源。

**表外结论**：**没有一个档位能对所有 (模型 × 端点) 通用**。`minimal` 是 GPT-6
系（Luna/Sol）在两端的共同禁区；`max` 在 Luna/Sol 的 chat 端被拒。故设计的
「网关拒则自动剥离重投」（§5.5）不是可选项而是**必需项**，它同时兜住了
`minimal` 与 `max` 的模型差异。

**CoT 明文能力**：

| 模型 | chat/completions + tools | responses + tools | 明文 CoT |
|---|---|---|---|
| DeepSeek | 任意档 effort 都 200 | 200 | **有**（`reasoning_content` / responses 的 `reasoning.content[].text`） |
| Sol | 带 tools 时只有 `none` 通过（不带 effort 直接 400） | `low..max` 全 200；`none`/`minimal` 被拒 | **有**（`reasoning.summary[].text`，需传 `summary:"auto"`） |
| Luna | 带 tools 只有 `none` 过 | `low..max` 全 200；`none` 200、`minimal` 被拒 | **无文本**（`content=[]`、`summary=[]`，仅 `encrypted_content`） |

**图片输入**：chat 的 `image_url` 与 responses 的 `input_image`（data URL）
**均实测可用**，模型正确识别图像内容。

端点可用性探测（实测 horizon）：

| 探测 | 网关不支持 responses | 网关支持但请求有错 |
|---|---|---|
| `GET /v1/responses` | **404** | **405**（路径存在、方法不对） |
| `POST /v1/responses` | **404 + 空 body（0 字节）** | 404 **+ JSON body**（如 `model_not_found`） |

合法档位（网关报错原文）：

- `/v1/responses`：`none, minimal, low, medium, high, xhigh, max`
- `/v1/chat/completions`：`none, minimal, low, medium, high, xhigh, max, ultra`

但**每个模型接受的子集不同**（Sol 拒 `none`/`minimal`，Luna 拒 `minimal`）。

### 1.5 responses 的兼容面（必须明确的取舍）

`/v1/responses` 是 OpenAI 2025-03 推出的新一代接口，**「能力更通用、兼容更
不通用」**：vLLM / Ollama / llama.cpp 等绝大多数第三方 OpenAI 兼容网关**只实现
`/v1/chat/completions`，没有 `/v1/responses`**。因此若把默认整体切到 responses，
任何填了自建/本地推理网关的条目会从「能用」变 404。本设计用「自动探测 + 透明
回退」消除这一风险。

## 2. 目标与非目标

### 目标

1. **修 400**：Luna/Sol 带 tools 的请求不再失败。
2. **reasoning effort 可配**：模型条目给默认档，主界面可做会话级覆盖。
3. **用户无感切换 chat / responses**：自动探测网关能力、自动回退，不要求用户
   理解两个端点的区别，也不因默认 responses 把非官方网关炸掉。
4. **展示 CoT**：DeepSeek / Sol 的明文思维链可作为独立可折叠区域展示；Luna
   这类无明文的显示「已推理（reasoning tokens: N）」占位。
5. **三端可用**：TUI、Mac App、iOS App 都提供 effort 入口。

### 非目标

- 不做「把已产生的 CoT 回灌进多轮上下文」（避免重复计费/内容重复；见 §6.3）。
- 不做 `previous_response_id` 服务端状态复用（实测免它也能做无状态工具往返，
  见 §5.4；引入它会把客户端依赖到网关的有状态会话，违背可移植性）。
- 不引入 `ultra` 档（仅 chat 端点出现、极偏僻）。
- 不废弃 `.env` 兜底，不改 chat 通道既有行为（非 responses 场景字节级不变）。
- 不改 Anthropic provider（本设计仅涉及 openai 通道）。

## 3. 术语

- **通道（channel）**：`chat`（`POST /v1/chat/completions`）或 `responses`
  （`POST /v1/responses`）。
- **api_style**：条目上声明通道的字段，`auto | responses | chat`，默认 `auto`。
  `auto` = 探测 + 回退。
- **探测（probe）**：第一次对该网关发请求时，判定它是否支持 responses。
- **effort**：`reasoning_effort` 档位。解析链：会话覆盖 → 条目默认 → 未设置
  （**未设置 = 不发该字段**）。
- **降级（degrade）**：responses 不可用时改用 chat；chat 下 effort 被网关拒绝
  时去掉该字段。

## 4. 配置模型

### 4.1 `models.json` 条目新增两个可选键

```jsonc
{
  "name": "HORIZON-OG-Luna",
  "provider": "openai",
  "api_url": "https://llmapi.horizon.auto",
  "model": "HORIZON-OG-Luna",
  "api_key": "…",
  "api_style": "auto",              // 新增，默认 "auto"
  "reasoning_effort": "none"        // 新增，可选；缺省 = 不发该字段
}
```

- `api_style`：`auto`（默认）| `responses`（强制新端点）| `chat`（强制旧端点）。
- `reasoning_effort`：`none | minimal | low | medium | high | xhigh | max`；
  **缺省即不发**（保证今天所有配置行为零变化）。

二者均为 `#[serde(default)]` 可选字段，缺失/非法时按「默认 / 不发」处理，
**不让整个条目被跳过**——`load_catalog_from` 现有逐条容错语义保持不变。

### 4.2 解析链（三级）

```
会话覆盖（thread meta.reasoning_effort）
  → 条目默认（models.json 条目的 reasoning_effort）
    → 未设置（不发 reasoning_effort 字段）
```

`resolve_effective`（`yi-agent-runtime/src/models.rs:209`）目前改写 provider /
api_url / api_key / model 四字段；本设计扩展为**同时**带出该条目的
`api_style` 与 `reasoning_effort`，随 `RuntimeConfig` 进入 provider 构造。

## 5. Provider 层（`yi-agent-llm`）

### 5.1 通道分派

`OpenaiProvider` 增加两个构造/运行期字段：`api_style`（解析后）与
`reasoning_effort`（解析后）。`call_stream` 按解析出的通道构造请求：

- `chat` → 现有 `openai/types.rs` 的 `OpenaiRequest`（**改动仅在新增可选字段**）
- `responses` → 新增 `openai/responses/` 模块

`auto` 的判定见 §5.3。

### 5.2 chat 通道的唯一改动

`OpenaiGenParams` 新增可选字段：

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub reasoning_effort: Option<String>,
```

`None` 时序列化结果与今天**逐字节相同**。这保证非推理模型/未配置 effort 的
存量行为零回归。

### 5.3 自动探测与透明回退（`auto`）

**探测是网关级属性**（base_url 相同即同一网关），在 provider 内按 `base_url`
缓存判定结果（进程内 `Mutex<HashMap<String, Channel>>`）。

首次对某网关发请求（`auto`）时：

1. 发 `POST /v1/responses`
2. 若响应为 **404/405 且 body 为空**（= 路径不存在，实测与「模型不存在」的
   404+JSON 可区分）→ 判定该网关 `chat-only`，**立刻用同一逻辑请求（改成 chat
   请求体）重投一次**，并缓存 `chat`。
3. 否则用 responses，缓存 `responses`。
4. 后续请求直接走缓存通道，零额外探测开销。

**安全性**：回退发生在 POST 刚返回、**响应体尚未流式下发**的时刻，不存在
「半截内容已渲染再换端点」的脏状态；agent 回合不被污染。

**逃生门**：`api_style` 显式设为 `responses` / `chat` 时**跳过探测**，直接走
指定通道。用于自动探测判错（例如网关能连但不返回标准事件流）时手动纠正。

### 5.4 responses 请求/响应映射

**请求**（实测确认）：

- system prompt → `instructions` 字段
- 消息 → `input`（items 数组）
- 工具 → `tools`（`{type:"function", name, description, parameters}`，**无嵌套
  `function` 包裹**，与 chat 不同）
- 推理档 → `reasoning: { effort, summary: "auto" }`（`summary:"auto"` 才能拿到
  明文摘要，§1.4）
- 工具结果回流 → `input` 追加 `{type:"function_call", call_id, name, arguments}`
  与 `{type:"function_call_output", call_id, output}` 两项。**实测免
  `previous_response_id` 的无状态重放可用**，故客户端继续无状态重放全量历史。

**流式事件 → `ProviderEvent` 映射**（`openai/responses/stream.rs`）：

| responses 事件 | 映射 |
|---|---|
| `response.output_text.delta` | `ProviderEvent::TextDelta` |
| `response.reasoning_text.delta` | `ProviderEvent::ReasoningDelta { text, kind: Content }` |
| `response.reasoning_summary_text.delta` | `ProviderEvent::ReasoningDelta { text, kind: Summary }` |
| `response.function_call_arguments.delta` | `ToolUseDelta`（按 `item_id`/`output_index` 聚合） |
| `response.output_item.added`(function_call) | `ToolUseStart` |
| `response.output_item.done`(function_call) | `ToolUseEnd` |
| `response.completed` | `Stop` + `Usage`（含 `reasoning_tokens`） |

### 5.5 effort 被拒时的自动剥离（兜底，保证"无感"）

chat 通道下若网关回 `param: reasoning.effort` / `reasoning_effort` 类 400
（模型不支持该档，如 Sol 拒 `none`）→ **自动去掉该字段重投一次**，并按
`(base_url, model, channel)` 缓存该降级；同时 emit 一次
`ProviderEvent::Notice`（§7）告知用户「该模型不支持所配推理档，已忽略」。

### 5.6 `GenParams` 与 `ProviderEvent` 扩展（`yi-agent-core`）

- `GenParams` 增 `reasoning_effort: Option<String>`（沿用 `..Default::default()`
  的构造点无需改）。
- `ProviderEvent` 新增两个变体：
  - `ReasoningDelta { text: String, kind: ReasoningKind }`（`Content` = DeepSeek
    的完整思考；`Summary` = Sol 的结构化摘要）
  - `Notice { level: NoticeLevel, text: String }`（回退/降级的低调提示，§7）
- `TokenUsage` 增 `reasoning_tokens: Option<u32>`（responses 提供；chat 的
  DeepSeek 不提供则 `None`）。

## 6. 展示层

### 6.1 CoT 作为独立可折叠区域

- **TUI**：`ReasoningDelta` 累积成一段 dim 色「思考」块，位于正文之前；默认
  显示，`/reasoning` 可切折叠。
- **Mac / iOS**：`item/started` 级别的「思考」卡片，默认折叠、可展开。
- Luna 这类无明文的：展示 `已推理（reasoning tokens: N）` 占位（N 取自
  `TokenUsage.reasoning_tokens`）。

### 6.2 展示会不会重复计入 tokens / 上下文

**不会**：`reasoning_*` 只进展示流，**不进 `ProviderRequest.messages`**；usage
里 `reasoning_tokens` 只做展示与统计，不改 compact 的 token 估算口径（除非
后续单独立项）。

### 6.3 明确不做：CoT 回灌进多轮上下文

多轮里回灌 reasoning 会导致内容重复与重复计费，且不同网关回灌格式不一。本期
**只展示、不回灌**。这是有意的决策，写进非目标。

## 7. 回退/降级的可见性（用户无感 = 无需操作，但不是永远不知情）

自动回退 chat、自动剥离 effort 时，emit `ProviderEvent::Notice`，在**状态栏级**
给一行低调提示（不进弹窗、不进对话卡片）：

- 「该地址不支持 Responses，已自动改用 Chat」
- 「该模型不支持所配推理档（`xxx`），已忽略」

理由：完全静默会让「为什么 Luna 的 reasoning 没生效」变成新的困惑源。用户
不需要做任何操作，但能知道走了哪条路。

## 8. 三端入口

### 8.1 桌面端（Mac / iOS）

- **设置 → 模型 → 编辑条目**：`reasoning effort` 作为**正常字段**（与「API
  格式」平级），选项：`未设置 / none / minimal / low / medium / high / xhigh /
  max`。默认 `未设置`。
- **「▸ 高级」折叠区**（默认收起）：仅放 `api_style`（`自动 / responses /
  chat`，默认 `自动`）。普通用户无需展开。
- **主界面**：模型下拉（`ModelPicker`）旁新增 **effort 控件**，改的是**当前
  会话覆盖**（`thread/setReasoningEffort`），显示生效值（覆盖 ?? 条目默认 ??
  未设置）。

### 8.2 TUI

- `/model <name>` 行为不变。
- 新增 `/effort <档位>` 设会话覆盖、`/effort`（无参）展示当前生效值与来源。
- 渲染端：推理块（§6.1）+ Notice 行（§7）。

### 8.3 RPC / meta 层（app-server）

照抄 `thread/setModel`（`server.rs:3770`）的成熟模式：

- `thread_store::ThreadMeta` 增 `reasoning_effort: Option<String>`
  （`#[serde(default)]`，旧 meta 加载为 `None`）。
- `ThreadMeta::set_reasoning_effort`（读-改-写 + 原子写，与 `set_model` 同锁）。
- RPC `thread/setReasoningEffort`（`Control` 档，与 `setModel` 同权限）。
- 列表层广播 `thread/reasoningEffortChanged`（与 `thread/modelChanged` 同理，
  跨客户端同步）。
- `model/list`、`model/upsert`、`thread/resume`/`thread/start` 响应带上
  `reasoning_effort` 与 `api_style`（沿用既有「响应携带权威值」范式）。

## 9. 错误处理与边界

| 场景 | 行为 |
|---|---|
| 网关无 responses（404 空 body） | auto 下回退 chat + Notice |
| 网关 responses 能连但返回非标准事件流 | 解析失败 → `ProviderError::Stream`（与今天同）；用户可显式 `api_style:"chat"` |
| 模型拒所配 effort | 剥离该字段重投 + Notice + 缓存降级 |
| `api_style` 显式 `responses` 但网关没有 | 直接报错（尊重用户强制），不静默回退 |
| 旧 `models.json` / 旧 thread meta 无新字段 | 按 `auto` / 未设置处理，零迁移 |
| responses 的图片输入 | **已验证可用**：`input_image` + data URL 正常，模型正确识别图像内容（实测回答 "Red"）。此前一次失败是测试用 1x1 PNG 无效所致，非网关限制 |

## 10. 测试策略

- **`yi-agent-llm`（wiremock）**：
  - chat 通道未配 effort 时请求体逐字节不变（回归钉）
  - chat 配 effort 时携带 `reasoning_effort`
  - auto 探测：mock 404 空 body → 断言回退到 `/v1/chat/completions` 且只发一次
    responses 探测；判 `chat-only` 后缓存不再探测
  - auto 探测：mock 200 → 走 responses
  - effort 被拒（400 `param: reasoning.effort`）→ 剥离重投 + Notice + 缓存
  - responses SSE 解析：`output_text.delta` / `reasoning_text.delta` /
    `reasoning_summary_text.delta` / `function_call_arguments.delta` /
    `completed`（含 usage.reasoning_tokens）
  - responses 工具往返：`function_call` → `function_call_output` 无状态重放
- **`yi-agent-core`**：`ProviderEvent::ReasoningDelta` 累积、`Notice` 透传、usage
  含 `reasoning_tokens` 不影响 compact 估算
- **`yi-agent-app-server`**：`thread/setReasoningEffort` 落盘 + 广播 +
  `model/list` 带新字段（照 `thread_set_model_*` 用例群）
- **`yi-agent`（TUI）**：`/effort` 设值/展示；推理块渲染；Notice 行
- **`desktop`（vitest）**：`SettingsModelsTab` 的 effort 字段 + 高级区折叠；
  `ModelPicker` 旁 effort 控件写会话覆盖并回显生效值

## 11. 分阶段落地建议

1. **P1（修 bug 最小闭环）**：`api_style` + `reasoning_effort` 字段贯穿；chat
   通道带 effort；auto 探测 + 回退；effort 剥离兜底。→ Luna/Sol 400 消失。
2. **P2（responses 通道）**：responses 请求/事件流/工具往返 + usage。
3. **P3（展示）**：`ReasoningDelta` + 三端渲染 + Notice 行。
4. **P4（入口）**：thread meta 覆盖 + RPC + 广播 + 三端控件。

P1 单独可交付且能立刻解决用户报错；P2–P4 逐步推进。

## 12. 待确认 / 待验证

1. ~~responses 图片输入格式~~ **已验证**（见 §1.4 / §9）：`input_image` + data
   URL 可用，模型正确识别图像。
2. ~~`minimal` 档稳定性~~ **已验证**（见 §1.4 矩阵）：`minimal` 是 GPT-6 系在
   两端的共同禁区，由「网关拒则自动剥离」（§5.5）兜住，无需裁剪 UI 选项。
3. `xhigh` / `max` 的 UI 呈现：实测二者对三模型在 responses 端全通，仅在
   Luna/Sol 的 chat 端 `max` 被拒。故 **UI 可全量平铺 7 档**，被拒档位由自动
   剥离兜底 + 一行提示；无需为 `xhigh`/`max` 单独设「高级」分区。此项按此结论
   定稿，实现中若发现网关行为变化再调整。
4. **唯一保留的实现级未知**：网关 responses 端点在某些自建网关上「能连但返回
   非标准事件流」时的具体形态无法预先枚举——已由「显式 `api_style:"chat"`
   逃生门」覆盖（§5.3 / §9）。
