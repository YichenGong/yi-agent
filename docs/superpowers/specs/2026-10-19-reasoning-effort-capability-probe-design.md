# 设计：模型级 reasoning effort 能力探测与 UI 约束

日期：2026-10-19
状态：已评审（用户确认）
关联：
- `2026-10-12-reasoning-effort-and-responses-api-design.md`（effort 透传、通道探测、
  被拒剥离兜底）
- `2026-10-15-cot-round-trip-and-anthropic-thinking-design.md`（Anthropic 的
  `output_config.effort`）
- `2026-10-17-single-source-of-truth-model-config-design.md`（`models.json` 是
  唯一模型配置源）

## 1. 背景与问题

今天 effort 的可用集合是**运行时才被发现**的：

- 模型条目只有 `reasoning_effort: Option<String>`（`yi-agent-runtime/src/models.rs:76`），
  表示「默认档」，不表示「支持哪些档」。
- 桌面端两处 UI **无条件平铺 7 档**：设置页（`desktop/src/components/SettingsModelsTab.tsx:600`）
  与会话控件（`desktop/src/lib/modelPicker.ts` 的 `REASONING_EFFORTS`）。
- 唯一的兜底是「网关拒绝就把该字段剥掉重投一次」
  （`yi-agent-llm/src/openai/client.rs:366`），结果只存在进程内缓存
  （`effort_reject_key` = `base_url|model|channel`），重启即丢，UI 永远不知道。

后果：用户能选到一个必然被拒的档位，只能靠 400 后的兜底静默降级；且
「这个模型到底支持哪几档」这件事在任何界面都查不到。

### 1.1 已实测的事实

实测矩阵（`2026-10-12-...-design.md` §1.4）证明可用档位是
**模型 × 通道**的函数，不是模型的单一属性：

| 模型 | chat + tools | responses + tools |
|---|---|---|
| DeepSeek | 全 7 档 | 全 7 档 |
| Luna | 只有 `none` | `low..max` |
| Sol | 只有 `none` | `low..max` |

`minimal` 是 GPT-6 系在两条通道上的共同禁区；`max` 在 Luna/Sol 的 chat 端被拒。

探测请求**必须带 tools**：Luna/Sol 的 400 正是「tools + effort≠none」组合触发；
不带 tools 测出来的 `low/medium` 在真实对话（agent 永远带 tools）里仍然会失败。

## 2. 目标与非目标

### 目标

1. 添加/修改模型后，后台对该模型**实际会走的那条通道**跑一遍 7 档探测，把结果
   写进 `models.json`。
2. 桌面端设置页与会话控件的 effort 下拉**只显示该模型支持的档位**。
3. TUI 的 `/effort` 同样按能力集合校验。
4. 能力未知（未探测 / 探测失败）时保持今天的平铺行为，零回归。

### 非目标

- 不做定时轮询/后台周期重测。只有「保存触发」与「手动重新探测」两个入口。
- 不改变现有「被拒剥离重投」兜底；它继续作为安全网。
- 不探测非 effort 的能力（vision / tools / 上下文长度）。
- 不为 Anthropic 引入通道概念。

## 3. 术语

- **通道（channel）**：`chat`、`responses`（OpenAI 兼容）与 `anthropic`。
  存储键分别为 `"chat"` / `"responses"` / `"anthropic"`。
- **能力集合（capabilities）**：某条目在某通道上被探测为支持的档位列表。
- **未探测（unknown）**：没有该通道的记录，或记录带非空 `error`。
- **条目指纹（fingerprint）**：`provider | api_url | model | api_style | api_key`
  拼接后的字符串，用于判断探测期间条目是否被改。

## 4. 数据模型

### 4.1 `ModelEntry` 新增字段

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub reasoning_capabilities: Option<ReasoningCapabilities>,

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<ChannelCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses: Option<ChannelCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic: Option<ChannelCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCapability {
    /// 探测为支持的档位，按固定顺序（none/minimal/low/medium/high/xhigh/max）去重。
    #[serde(default)]
    pub supported: Vec<String>,
    /// Unix 秒；探测完成时间。
    #[serde(default)]
    pub probed_at: u64,
    /// 非空 = 该通道探测未能完成（鉴权失败/超时/网关异常），`supported` 不可信。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
```

落盘示例：

```jsonc
{
  "name": "HORIZON-OG-Luna",
  "provider": "openai",
  "api_url": "https://llmapi.horizon.auto",
  "model": "gpt-6-luna",
  "api_style": "auto",
  "reasoning_capabilities": {
    "responses": {
      "supported": ["low", "medium", "high", "xhigh", "max"],
      "probed_at": 1760000000
    }
  }
}
```

### 4.2 读路径容错

- 缺失 / `null` / 非对象 → `None`（= 未探测）。
- `supported` 里的未知字符串**丢弃**（不写进列表，也不用它污染 UI）。
- `probed_at` 非数字 → `0`。
- 单个通道的坏值不影响其它通道，更不影响整条条目（与
  `de_optional_string` 的容错口径一致）。

### 4.3 写路径

`model/upsert` 的语义扩展：

| 情形 | `reasoning_capabilities` |
|---|---|
| 新建条目 | `None`（随后触发探测） |
| 修改且 `provider/api_url/model/api_style/api_key` 任一变化 | 置 `None`（旧结果不可信），随后触发探测 |
| 修改但上述字段都没变 | **原样保留** |

`model/delete` 自然随条目消失。

## 5. 探测机制

### 5.1 位置

新增 `yi-agent-runtime/src/reasoning_probe.rs`。放在 runtime 而不是 app-server，
因为：

- provider 构造与凭据解析已经在 runtime（`onboarding::test_connection`）；
- TUI/CLI 也能复用，三端的能力来源同一份代码。

app-server 只负责「触发 + 写回 + 广播」，不含探测逻辑。

### 5.2 输入

```rust
pub struct ProbeTarget<'a> {
    pub provider: ModelProvider,
    pub api_url: &'a str,
    pub api_key: &'a str,
    pub model: &'a str,
    pub api_style: Option<&'a str>,   // 仅 openai
}
```

### 5.3 为什么不能直接复用 `Provider::call_stream`

现有 `OpenaiProvider::call_stream` 在 effort 被拒时**剥离该字段重投并返回成功**
（`yi-agent-llm/src/openai/client.rs:366-398`），且 `auto` 通道在 `/responses` 404
时**自动回退 chat 并返回成功**。直接用它探测会得到「全部 200」的假象：被拒的档位
被兜底掩盖了。

因此探测走一条**独立的裸 HTTP 路径**，不做剥离重投、不做通道回退、不读流内容：

- 新增 `yi-agent-llm/src/probe.rs`，公开：

```rust
pub enum ProbeChannel { Chat, Responses, Anthropic }
pub enum ProbeVerdict { Supported, Unsupported, Indeterminate(String) }

pub struct EffortProbeTarget<'a> {
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub model: &'a str,
    pub channel: ProbeChannel,
}

/// 发一次带 dummy tool 的最小请求，只看状态码与错误体，不读流。
pub async fn probe_effort(
    target: &EffortProbeTarget<'_>,
    effort: Option<&str>,   // None = 基线（不带 effort）
    timeout: Duration,
) -> ProbeVerdict;

/// `auto` 的通道判定：只发一次 `/responses`，按 `looks_like_missing_route` 判定。
pub async fn detect_openai_channel(
    base_url: &str,
    api_key: &str,
    model: &str,
    timeout: Duration,
) -> Result<ProbeChannel, String>;
```

- 请求体复用现有 builder（`openai/types.rs` 的 `OpenaiRequest::from`、
  `openai/responses/types.rs` 的 `responses_request_from`、`anthropic/types.rs` 的
  `AnthropicRequest::from`），避免探测与真实请求的序列化逻辑分叉。这几个构造器
  在 crate 内提升为 `pub(crate)` 即可，不扩大公开面。

### 5.4 通道解析

- `anthropic` → 只有 `anthropic` 一条通道。
- `openai` + `api_style = Some("chat")` → 只测 `chat`。
- `openai` + `api_style = Some("responses")` → 只测 `responses`。
- `openai` + `api_style = None/auto` → 先 `detect_openai_channel` 判定，再只测
  判定出的那一条；判定失败（鉴权/网络）→ 写一条 `error`，`supported` 留空。
  不写另一条通道的记录：没测过的东西不冒充「支持」。

### 5.5 探测序列

每个通道共 **8 个请求**：1 个基线 + 7 个档位。

1. **基线**：不带 `reasoning_effort`，带 dummy tool。
   - 2xx → 通道可用，继续逐档探测；
   - 失败 → 中止，写 `error`（原因可读）。这条把「鉴权/模型不存在/网络」与
     「档位不支持」彻底分开，避免把 401 记成「7 档都不支持」。
2. **7 档**：按 `none → minimal → low → medium → high → xhigh → max` 串行，
   每档单独一个请求，带同一个 dummy tool 与该档 `reasoning_effort`。

请求形状：

- `messages = [user("ping")]`
- `tools = [noop]`（`{"name":"noop","description":"noop","parameters":{"type":"object","properties":{}}}`）
- `params = { max_tokens: 64 }` + 可选 `reasoning_effort`
- `stream = true`：拿到 2xx 即**丢弃响应流**并断开，不解析内容。

若基线 2xx 但 7 档全部被拒 → 记 `supported: []`（该模型在此通道上不接受任何
显式档位，请求应不带该字段），不记 `error`。

### 5.6 结果分类

| 响应 | 判定 |
|---|---|
| 2xx | 该档 **支持** |
| 400/422 且错误体点名 `reasoning_effort` / `reasoning.effort`（含 "Supported value" / "does not support" / "not supported" / "unsupported"） | 该档 **不支持** |
| 400/422 但未点名 effort | 该档 **不确定**（`Indeterminate`），保留为未知，不写进 `supported` |
| 其它 4xx/5xx（401/403/404/429/5xx） | **中止整轮探测**，`error` = 可读原因 |
| 网络错误 / 超时 | **中止整轮探测**，`error` = 可读原因 |

OpenAI 侧的判别直接复用 `yi_agent_llm::openai::error::is_effort_rejection`
（状态码 + body 同时看），不另写一套。Anthropic 侧新增同形的
`anthropic::error::is_effort_rejection`：400/422 且 body 含 `output_config` 或
`effort`，并含 "unsupported"/"does not support"/"invalid"。

### 5.7 时序与预算

- 基线 + 7 档**串行**，避免同一网关瞬时并发触发限流。
- 单请求超时 10s；整轮总预算 30s。
- 总预算耗尽：保留已得结果，剩余档位不写（= 未知，不算「不支持」），`error` 保持为空。
- 探测期间该条目的并发探测请求被丢弃（返回 `started: false`）。

### 5.8 写回与防覆盖

探测完成时：

1. 重新 `load_catalog_from`；
2. 按 `name` 定位条目；
3. 重算指纹，与探测开始时不同 → **丢弃结果**（条目已被改/删）；
4. 相同 → 写入对应通道的 `ChannelCapability`，`save_catalog_to` 原子落盘。

## 6. 运行时语义

### 6.1 请求期

`resolve_effective` 继续带出条目默认 `reasoning_effort`。新增一层过滤：

- 条目对该通道有**可信能力数据**（记录存在且 `error` 为空）时，若默认档不在
  `supported` 内 → 请求**不带** `reasoning_effort`，并 emit 一次
  `ProviderEvent::Notice`「该模型不支持所配推理档，已忽略」。
- 能力未知（无记录 / 有 `error`）→ 行为与今天完全一致（照发，被拒由现有剥离兜底）。

注意：会话覆盖档位同样适用这条过滤——覆盖值不在 `supported` 内时按「未发送」处理。

### 6.2 写入期

`thread/setReasoningEffort` 增加校验：

- 条目对该通道有可信能力数据 → 只接受 `supported` 内的档位 + `null`；
  其它值回 `-32602`，错误消息附上支持的档位列表。
- 能力未知 → 沿用现有「七档白名单」校验（零回归）。

`model/upsert` 对条目默认档的现有七档白名单校验不变；探测结果与默认档的冲突
由 6.1 的运行时过滤兜住，不在写入期拒绝（用户可能先配档、后探测）。

## 7. RPC 与推送

### 7.1 读取

`model/list` 的每个条目新增：

```jsonc
"reasoning_capabilities": {
  "chat": {"supported": ["none"], "probed_at": 1760000000},
  "responses": {"supported": ["low","medium"], "probed_at": 1760000000, "error": null}
}
```

`effective` 视图同样携带（来自命中条目或 `cfg`；`cfg` 无能力数据时省略）。

### 7.2 触发

新增 `model/probe`（`Control` 档）：

- 入参 `{ "name": "<条目名>" }`；
- 条目不存在 → `model_not_found`；
- 已有一条在探测 → `{ "started": false }`；
- 否则后台启动 → 立即回 `{ "started": true }`。

`model/upsert` 判定字段变化后，自动触发同一探测（不阻塞 `model/upsert` 的响应）。

### 7.3 推送

新增通知 `model/capabilitiesChanged`：

```jsonc
{"method":"model/capabilitiesChanged","params":{"name":"HORIZON-OG-Luna",
 "reasoning_capabilities":{"responses":{"supported":[...],"probed_at":...}}}}
```

投递层级：**全部客户端**（与 `model/list` 同域，不是线程级），因为能力是机器级
清单属性。桌面端收到后就地更新内存清单，不重拉全量。

## 8. 三端 UI

### 8.1 桌面设置页（`SettingsModelsTab`）

- 清单行/卡片新增一列「推理档位」：
  - 有可信数据：`chat none` / `responses low·medium·high`（多通道用 `；` 分隔）；
  - 有 `error`：`未探测（<原因>）`；
  - 无记录：`未探测`。
- 编辑表单的档位下拉按 `provider + api_style` 选出的通道过滤：
  - 能力未知 → 平铺 7 档（今天的行为）；
  - 能力可信 → 只列 `supported`（加「未设置」项）；
  - 当前值不在 `supported` 内 → 下拉里显示为「不支持（high）」但允许保留，
    避免静默改用户配置。
- 条目行/卡片新增「重新探测」按钮，调 `model/probe`，按钮进入「探测中…」状态。

### 8.2 桌面会话控件（`ModelPicker`）

- `buildEffortOptions` 签名扩展为
  `(currentOverride, entryDefault, supported: string[] | null)`：
  - `supported === null` → 今天的行为（「未设置」+ 7 档）；
  - `supported` 非空 → 「未设置」+ 仅 supported 档位；
  - `supported === []` → 只显示「未设置」，并提示「该模型在此通道上不接受显式档位」。
- `entryEffortOf` 之外新增 `entrySupportedEfforts(list, ref, channel)`；
  通道由 `entry.api_style` 决定（`auto` 时取已探测的那条；两条都有则优先
  `responses`，与运行期 `resolve_channel` 的默认一致）。

### 8.3 TUI

- `/effort <tier>` 解析后按当前条目能力校验；不在 `supported` 内 →
  一行错误「该模型不支持 xxx（支持：a/b/c）」。
- `/effort`（无参）状态行追加支持的档位列表。

## 9. 错误处理与边界

| 场景 | 行为 |
|---|---|
| 探测时鉴权失败（401） | 该通道记 `error`，UI 显示未探测；不把 7 档都标为不支持 |
| 探测超时 | 保留已得结果，剩余档位未知；`error` 为空（避免把部分结果整体作废） |
| 探测期间条目被改 | 丢弃结果，不落盘 |
| `auto` 通道判定失败 | 不写任何通道记录，`error` 记原因 |
| 旧 `models.json` 无该字段 | 未探测；行为与今天一致 |
| 旧客户端读到新字段 | 忽略未知字段（JSON 反序列化默认行为） |
| 条目被删除时探测结束 | 定位不到 → 丢弃 |
| 服务端重启 | 能力已在磁盘，无需重测 |

## 10. 测试策略

### runtime（`yi-agent-runtime`）

- `ReasoningCapabilities` 序列化往返；坏值（字符串/数组/非数字 `probed_at`）容错；
  未知档位被丢弃。
- `upsert` 保留/清空语义：连接字段不变保留，任一变化清空。
- 探测（wiremock）：
  - 基线 200 + 7 档全 200 → `supported` 为全 7 档；
  - 基线 200 + 200/400-effort 混合 → 只记 200 的档位；
  - 基线 401 → `error` 非空、`supported` 空，且**只发 1 个请求**（不逐档试）；
  - 某档 400 未点名 effort → 该档不进 `supported`，其余档继续探测；
  - 超时 → 部分结果保留、`error` 为空；
  - 断言每个请求体都带 dummy tool 与对应 `reasoning_effort`；基线不带该字段。
- `auto` 通道判定：`POST /v1/responses` 404 空 body → 只测 chat；200 → 只测
  responses；断言另一通道无记录。
- **判别性验证**：让探测走 `Provider::call_stream` 而不是裸 HTTP 时，
  「400-effort 被剥离重投」用例必须变红——证明探测确实绕开了兜底。

### `yi-agent-llm`

- `probe_effort` 的分类矩阵（2xx / effort 类 400 / 其它 400 / 401 / 429 / 超时）。
- `detect_openai_channel` 对 404 空 body、404 JSON、405 空 body、200 的判定。
- 断言探测请求体与真实请求体在 `tools` 与 `reasoning_effort` 上同形。

### app-server（`yi-agent-app-server`）

- `model/list` 带能力字段；
- `model/upsert` 字段变化触发探测、未变化不触发；
- `model/probe` 去重（第二次回 `started:false`）、`model_not_found`；
- 探测完成后广播 `model/capabilitiesChanged`；
- 探测期间条目被改 → 磁盘上不出现该结果；
- `thread/setReasoningEffort` 拒绝不在 `supported` 内的档位（错误消息含支持列表）；
  能力未知时仍接受七档。

### desktop（vitest）

- `buildEffortOptions`：`supported = null` 平铺 7 档；`["low","high"]` 只出这两档；
  `[]` 只出「未设置」。
- `SettingsModelsTab`：能力列渲染、未探测/错误态、档位下拉过滤、当前值不受支持时
  的展示、「重新探测」按钮调用 `model/probe`。

### TUI

- `/effort` 拒绝不支持档位并给出支持列表；能力未知时行为不变。

## 11. 落地顺序

1. **P1 数据模型**：`ReasoningCapabilities` + 读路径容错 + `upsert` 保留/清空语义。
2. **P2 裸探测**：`yi-agent-llm/src/probe.rs`（分类矩阵 + 通道判定 + 与真实请求
   同形的 body 构造）。
3. **P3 探测编排**：`yi-agent-runtime/src/reasoning_probe.rs`（基线 + 7 档串行、
   预算、写回防覆盖）。
4. **P4 接线**：`model/probe` RPC + `model/list` 字段 + `model/capabilitiesChanged`
   推送 + `upsert` 自动触发。
5. **P5 约束**：运行期过滤（不发不支持的档位）+ `thread/setReasoningEffort` 校验。
6. **P6 三端 UI**：设置页、会话控件、TUI。

P1–P4 单独可交付（能力可查、可探测），P5 开始改变行为，P6 收口 UI。
## 12. 实现状态

截至 2026-10-19，P1–P6 全部落地，53 个计划步骤全部完成。

| 阶段 | 落点 | 判据 |
|---|---|---|
| P1 数据模型 | `yi-agent-runtime/src/models.rs:91` / `:105` / `:79` | `cargo test -p yi-agent-runtime --lib reasoning_capabilities` |
| P2 裸探测 | `yi-agent-llm/src/probe.rs:65` / `:148` / `:42` | `cargo test -p yi-agent-llm --lib probe::tests`（11） |
| P3 探测编排 | `yi-agent-runtime/src/reasoning_probe.rs:54` | `cargo test -p yi-agent-runtime --lib reasoning_probe`（4） |
| P4 接线 | `yi-agent-app-server/src/model_rpc.rs:75` / `:90` / `:120`、`src/rpc/model.rs:169` / `:216`、`src/protocol.rs:309` | `cargo test -p yi-agent-app-server --lib model_probe_`（3）+ `an_upsert_with_a_new_connection_starts_a_probe` / `an_upsert_that_only_changes_the_key_does_not_reprobe` |
| P5 约束 | `yi-agent-runtime/src/models.rs:610`（运行期过滤）、`yi-agent-app-server/src/rpc/thread.rs:973`（写入期校验） | `cargo test -p yi-agent-app-server --lib set_reasoning_effort_`（4） |
| P6 桌面 | `desktop/src/lib/modelPicker.ts:111` / `:149`、`SettingsModelsTab.tsx:96` / `:317`、`ModelPicker.tsx:177` | `cd desktop && npx vitest run`（1417） |
| P6 TUI | `yi-agent/src/tui/slash.rs:371`、`yi-agent/src/tui/app.rs:2436` / `:2397` | `cargo test -p yi-agent --bins effort`（21） |

全量回归：`cargo test -p yi-agent-runtime --lib`（182）、`-p yi-agent-llm --lib`（125）、
`-p yi-agent-app-server --lib`（881，4 ignored）、`-p yi-agent --bins`（654）。

落地后另修的一处并发缺陷（不属于本设计的探测逻辑，但由后台写回引入）：
`models.json` 的读改写原先无互斥，探测写回会覆盖同时落盘的 `default_model`
等修改。见 `docs/bug-list.md` 对应条目与 `models.rs` 的
`update_catalog` / `update_catalog_if`。

**实现期偏离设计的两处（均为收紧，未放宽约束）**：

1. 状态行「可用」列表与 `/effort` 放行集合**同源**（`render_effort_status` 收
   `supported` 参数）。设计只要求拒绝不支持的档位，但如果状态行仍平铺七档，
   用户会照着一份点不动的清单去敲，等于把拒绝藏到下一次交互里。
2. `model/capabilitiesChanged` 在桌面端 `threadStore` 里被**显式忽略**。它携带的
   是机器级事实（某模型支持哪些档位），没有 `threadKey`，若落进会话通知分支会
   往 `undefined` 会话写正文。
