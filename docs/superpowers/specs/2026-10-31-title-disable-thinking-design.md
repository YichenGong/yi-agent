# 设计：标题生成显式关闭思考

日期：2026-10-31
状态：已确认（方案 B：`GenParams` 加显式布尔位，三通道各自翻译）
关联：`docs/superpowers/specs/2026-10-22-title-on-first-input-design.md`（本设计改变其标题调用的参数口径）

## 1. 问题

LLM 自动标题「不常见」。真机实测（`HORIZON-DeepSeek`，同一个 endpoint / key）：

```
max_tokens=64  → finish_reason: length
                 content: ""
                 reasoning_tokens: 64     ← 额度全被思考吃光
```

根因不是触发链路，是标题调用的参数对推理模型不够：

- `title.rs` 把 `max_tokens` 定为 `64`（旧时代给非推理模型的取值）。
- 推理模型先产 `reasoning_content`，**推理 token 与正文共用 `max_tokens`**。
- 64 被思考烧光后正文为空 → `sanitize_title` 返回 `None` → 走
  `Ok(Ok(None))` 分支静默保留兜底标题（该分支此前没有任何日志）。
- 用户看到的就是「标题一直是首句截断值」。

8 次采样中 1 次 `content` 完全为空、3 次被 `length` 截断。

## 2. 目标与非目标

### 目标

1. 标题调用**显式关闭思考**，不再把额度浪费在推理上。
2. 三条 provider 通道（OpenAI chat / OpenAI responses / Anthropic）都真正关得掉。
3. 缺省行为零回归：其它调用（正常对话）仍按原样思考。
4. 该类失败不再静默。

### 非目标

- 不改标题的触发时机、材料口径、清洗规则。
- 不改 `thread/titleChanged` 协议与桌面端。
- 不为「关思考」引入用户可配置项：这是标题调用的内部决定。

## 3. 设计

### 3.1 为什么需要新的布尔位

三条通道表达「不思考」的方式不同，**无法用一个 effort 档位统一表达**：

| 通道 | 现状 | 关思考的方式 |
|---|---|---|
| OpenAI chat | 模型默认思考 | `reasoning_effort: "none"`（实测有效） |
| OpenAI responses | 靠是否发 `reasoning` 块 | 不发该块 |
| Anthropic | `thinking` **无条件** `Some(adaptive)` | 必须整个省略 `thinking` 字段 |

Anthropic 那条根本做不到用 effort 表达，所以引入显式意图位：

```rust
/// 显式关闭思考；`None` = 不动（零回归），`Some(true)` = 本次调用不思考。
pub disable_thinking: Option<bool>,
```

调用方只声明意图，翻译交给各通道。

### 3.2 各通道翻译

- **chat**：`disable_thinking == Some(true)` → `reasoning_effort = Some("none")`，
  并盖掉调用方带来的档位（两个信号并存会互相矛盾）。
- **responses**：`Some(true)` → `reasoning` 块整体缺席。
- **Anthropic**：`Some(true)` → `thinking` 与 `output_config` 都为 `None`。

### 3.3 标题侧改动

```rust
params.max_tokens = Some(TITLE_MAX_TOKENS);   // 512
params.disable_thinking = Some(true);
params.reasoning_effort = None;               // 与关思考不并存的信号
```

`TITLE_MAX_TOKENS = 512`：关掉思考后几十 token 就够，512 是给「关不掉思考的网关」
留的安全垫——宁可多留额度，也不要静默退回兜底标题。

### 3.4 不再静默

`generate_title_for_first_input` 的 `Ok(Ok(None))` 分支补 `tracing::info`。
这次就是因为该分支完全静默才排查了很久。

## 4. 错误处理

| 情况 | 处理 |
|---|---|
| 网关不认识 `effort: "none"` | 走既有 effort 拒绝重投路径（剥离后重投） |
| 依然产不出正文 | 记 `tracing::info`，保留兜底标题 |
| `disable_thinking` 为 `None` | 完全按旧行为发请求（零回归） |

## 5. 验证

单测（均做过变异验证，删掉对应翻译即红）：

- `yi-agent-core`：`generate_title_disables_thinking`、
  `generate_title_overrides_a_configured_effort`、`generate_title_request_shape_is_bound`
- `yi-agent-llm`：`disable_thinking_serializes_effort_none`、
  `absent_disable_thinking_sends_no_effort`、`disable_thinking_omits_the_thinking_field`、
  `thinking_stays_on_by_default`、`request_omits_reasoning_block_when_thinking_disabled`

真机验证（同一 endpoint / key，标题系统提示 + `max_tokens=512` + `effort=none`）：
10/10 全部返回非空标题，`reasoning_tokens` 恒为 0。
