# 设计：OpenAI base_url 语义对齐（含 /v1）+ 设置页模型测试按钮

日期：2026-10-15
状态：待评审
关联：`2026-10-12-reasoning-effort-and-responses-api-design.md`（同一条 horizon 网关线）、`2026-10-09-first-run-onboarding-design.md`（探测逻辑来源）、`2026-10-09-model-catalog-authority-design.md`（清单语义）

## 1. 背景与问题

### 1.1 直接触发：同一个网关地址，两种协议要两种 URL

用户环境的 `https://llmapi.horizon.auto` 是网关根：

| 协议 | 正确地址 | 说明 |
|---|---|---|
| anthropic | `https://llmapi.horizon.auto` | 客户端补 `/v1/messages` |
| openai | `https://llmapi.horizon.auto/v1` | **带 `/v1`**，客户端补 `/chat/completions` |

而当前实现把 openai 的 base_url 当作「不含 `/v1` 的根」，再硬编码补 `/v1`：

```rust
// yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs
const DEFAULT_BASE_URL: &str = "https://api.openai.com";   // L25，不含 /v1
let path = match channel {
    Channel::Chat      => "/v1/chat/completions",           // L185
    Channel::Responses => "/v1/responses",                  // L186
};
client.post(format!("{}{}", self.base_url, path))           // L191，字符串直接拼接
```

用户按 OpenAI 生态习惯填 `https://llmapi.horizon.auto/v1`，会拼成
`https://llmapi.horizon.auto/v1/v1/chat/completions` → 404。

### 1.2 根因：与 OpenAI 官方约定相反

OpenAI 官方 SDK 里 `base_url` **本身就含版本段**：

- `openai-python`：`OpenAI(base_url="https://api.openai.com/v1")`，之后只拼 `/chat/completions`。
- `openai-node`：同上。
- 绝大多数 OpenAI 兼容网关对外发布的地址即「带 `/v1` 的根」。

也就是说，本仓库当前的「base_url 不含 v1 + 代码补 v1」是**偏离生态约定**的。
Anthropic 恰好相反（`base_url="https://api.anthropic.com"`，SDK 补 `/v1/messages`），
两家的 base_url 语义**本来就不一样**。

### 1.3 现存数据会受影响

`~/.yi-agent/models.json` 当前 3 条：

```json
{"name":"HORIZON-DeepSeek","provider":"openai","api_url":"https://llmapi.horizon.auto", ...}
{"name":"HORIZON-OG-Luna", "provider":"openai","api_url":"https://llmapi.horizon.auto", ...}
{"name":"HORIZON-OG-Sol",  "provider":"openai","api_url":"https://llmapi.horizon.auto", ...}
```

改动后这些「不带 `/v1`」的 openai 条目会打到 `.../chat/completions`（缺 `/v1`）而失效。
因此迁移策略是本次设计的一部分。

### 1.4 附带的体验缺口

设置页 `SettingsModelsTab` 只能「保存」模型，**没有测一次**的入口。用户把地址填错
（正是本次这类 `/v1` 问题）只能保存后靠真实对话去撞 404。首次引导向导
（`OnboardingWizard`）有「测试连接」，设置页没有。

## 2. 目标与非目标

### 目标

1. openai 的 base_url 语义对齐 OpenAI 官方约定（含 `/v1`），用户填生态标准地址即可用。
2. 设置页编辑面板提供「测试」按钮，**保存前**就能验证当前草稿。
3. 旧配置平滑：无歧义的一对一改写自动迁移；有歧义的自定义地址给出非阻塞提示，不改写不阻断。

### 非目标

- 不统一 openai 与 anthropic 的 base_url 语义（生态事实，强行统一反而制造误解）。
- 不做「末尾没 `/v1` 就自动补」的隐式改写（`/openai/v1`、`/api` 这类前缀无法可靠判断，见 §5.1）。
- 不提供「重新运行首次引导」入口（`2026-10-09-first-run-onboarding-design.md` 已 YAGNI）。
- 不改 anthropic 侧任何代码与默认值。

## 3. 设计

### 3.1 openai base_url 语义：含版本段的 API 根

**约定**：`base_url` = 「含版本段的 API 根」，客户端只拼端点路径。

| 位置 | 现值 | 新值 |
|---|---|---|
| `crates/yi-agent-llm/src/openai/client.rs:25` `DEFAULT_BASE_URL` | `https://api.openai.com` | `https://api.openai.com/v1` |
| `crates/yi-agent-llm/src/openai/client.rs:185` Chat path | `"/v1/chat/completions"` | `"/chat/completions"` |
| `crates/yi-agent-llm/src/openai/client.rs:186` Responses path | `"/v1/responses"` | `"/responses"` |
| `crates/yi-agent-runtime/src/config.rs:241` openai 默认 | `https://api.openai.com` | `https://api.openai.com/v1` |
| `crates/yi-agent-web/src/config_meta.rs:94` `OPENAI_BASE_URL` default | `https://api.openai.com` | `https://api.openai.com/v1` |
| `crates/yi-agent/src/onboarding_prompt.rs:20` CLI 引导 `provider_defaults` | `https://api.openai.com` | `https://api.openai.com/v1` |
| `desktop/src/components/OnboardingWizard.tsx:23` | `https://api.openai.com` | `https://api.openai.com/v1` |

> openai 默认值在仓库里有 **5 处**落点（client / config / config_meta / CLI 引导 /
> 桌面向导）。它们必须同改，否则「引导填的默认值」与「代码兜底默认值」会不一致。
> `onboarding_prompt.rs:20` 的注释已写明「与 `config.rs` 的默认值同源」，改动时
> 保持这一约束。

**保持不变**：

- `trim_end_matches('/')`（去尾部斜杠）保留。
- URL 拼接仍是字符串拼接 `format!("{}{}", base_url, path)`。
- **不引入**「末尾无 `/v1` 则补上」的逻辑。
- anthropic（`anthropic/client.rs:118` 的 `format!("{}/v1/messages", ...)`）与
  `AnthropicProviderOpts` 默认值完全不动。

**效果**：

| provider | 用户填 | 实际请求 |
|---|---|---|
| openai | `https://llmapi.horizon.auto/v1` | `https://llmapi.horizon.auto/v1/chat/completions` |
| openai | `https://api.openai.com/v1`（新默认） | `https://api.openai.com/v1/chat/completions` |
| anthropic | `https://llmapi.horizon.auto` | `https://llmapi.horizon.auto/v1/messages` |

### 3.2 设置页「测试」按钮

**位置**：`desktop/src/components/SettingsModelsTab.tsx` 编辑面板（新增/编辑）底部，
「保存」「取消」旁新增「测试」。

**语义**：测**屏幕上的草稿值**，未保存也能测。与首次引导「填完先测再存」体验一致。

**入参**：`{ provider, model, api_url, api_key?, name? }`

**key 解析规则（核心安全约定）**：

| 场景 | 发送 | 服务端解析 |
|---|---|---|
| 新增，用户填了 key | `api_key` = 草稿值 | 直接用 |
| 编辑，**用户改过** key（`keyEdited === true`） | `api_key` = 草稿值 | 直接用 |
| 编辑，**用户没改** key | 不传 `api_key`，传 `name` | 按 `name` 从 `models.json` 取已存明文 key |
| 编辑，没改 key 且 `name` 在清单里查不到 | 传 `name` | 报错「找不到模型条目，请重新填写密钥」 |
| 都没给 | 两者都不传 | 报错「请填写 API 密钥」 |

**为什么必须由服务端取 key**：设置页 key 框**不回填**（`placeholder="留空则保留原密钥"`，
明文 key 从不经过桌面端，只出掩码，见 `models.ts:112` 与 `models.rs` 的 `mask_key`）。
所以「编辑已有条目且没改 key」时，桌面端**手里没有明文**，只能由服务端按条目名取。

**后端**：新增 JSON-RPC `model/test`。

- 权限档 `Scope::Control`（与 `onboarding/test` 一致：触及密钥）。
- 探测是 `async`，而 `model_rpc::handle_model_request_at` 是同步的 → 在
  `crates/yi-agent-app-server/src/server.rs` 的 `model/*` 分发处**单独开一个
  `model/test` 分支**，直接 `await` 调 `yi_agent_runtime::onboarding::test_connection`，
  不走同步的 `handle_model_request_at`（与既有 `onboarding/test` 分支同一写法）。
- key 解析放在服务端新函数里：`api_key` 给字符串即用；否则若有 `name` 则
  `load_catalog_from(models_path)` 查条目取明文 key。
- 解析出 `ModelSettings` → 复用 `test_connection`（15s 超时、只探测、不落盘）。
  **不写第二套探测实现**。

**回包**：`{ ok: bool, reason: string | null }`（与 `onboarding/test` 同形）。

**前端反馈**：按钮进入「测试中…」禁用态；结束就地显示「连接成功」或失败原因
（复用 onboarding 的中文 reason）。测试与保存互不影响：测试不清草稿、不写盘。

### 3.3 旧配置迁移：精确迁移 + 启发式提示

#### 3.3.1 精确迁移（无歧义，一对一）

在 `crates/yi-agent-runtime/src/models.rs` 的加载路径
（`load_catalog_from`，L105）加一次**纯函数改写**：

- 条件：`provider == openai` 且 `api_url.trim_end_matches('/') == "https://api.openai.com"`。
- 动作：改写为 `https://api.openai.com/v1`。
- 其它任何地址（含 `https://llmapi.horizon.auto`、`https://api.openai.com/v1`
  自身）**一律不动**。

**只在读取期改写、不回写文件**：显示与生效都用改写后的值，但 `models.json`
不被本次读取改写。理由：读一次配置就改用户文件副作用过大；旧默认值到新默认值
是一对一映射，读取期改写即可保证行为正确。

#### 3.3.2 启发式提示（UI 层，非阻塞）

设置页模型列表里，对 `provider == openai` 且 `api_url`（去尾斜杠后）**不以 `/v1`
结尾**的条目，行内显示一条提示：

> OpenAI 兼容接口地址通常以 `/v1` 结尾，请确认

- **不阻断**编辑、保存、删除、测试任何操作。
- 只是「请确认」，不宣称地址一定错——`/api/v1`、`/openai/v1` 这类前缀同样被判为
  「含 `/v1`」而不会提示；无法判断的（如 `/api`）宁可少提示也不误报硬结论。
- 用户现有 3 条 horizon 条目会被立刻标出，据此手动改成 `https://llmapi.horizon.auto/v1`。

## 4. 数据流

```
用户填 URL ──► models.json(api_url) ──► load_catalog_from
                                          │
                                          ├─ openai 且 = 旧默认 ──► 改写为 /v1（仅内存）
                                          └─ 其它 ──► 原样
                                          │
                                          ▼
                                    resolve_effective ──► RuntimeConfig.api_url
                                          │
                                          ▼
                              OpenaiProvider.base_url（含 /v1）
                                          │
                              format!("{}{}", base_url, "/chat/completions")
                                          │
                                          ▼
                         https://llmapi.horizon.auto/v1/chat/completions

设置页「测试」──► model/test{provider,model,api_url,api_key?|name?}
                     │
                     ├─ api_key 给了 ──► 直接用
                     └─ 没给 + name ──► models.json 取明文 key
                     │
                     ▼
              test_connection(ModelSettings) ──► {ok, reason} ──► 内联显示
```

## 5. 边界与错误处理

### 5.1 为什么不自动补 `/v1`（拒绝的方案 C）

「末尾没有版本段就补 `/v1`」看着省事，但不可预测：

- `/api`、`/openai`、`/proxy` 这类前缀**无法判断**后面是否还需 `/v1`；
- 判断错了就是静默打错地址，报错时无法向用户解释「为什么它替我改了」。

方案 A 的立场是：**不注入隐式改写，用户填网关真实地址**。代价是默认值含 `/v1`
需要用户在自建地址时留意；由 §3.3.2 的提示来兜住这一点。

### 5.2 迁移的幂等与安全

- 改写条件精确等于旧默认值，天然幂等（改完的值不等于旧默认值，二次加载不再改）。
- 只碰 `provider == openai`，anthropic 条目零影响。
- 尾斜杠变体（`https://api.openai.com/`）也命中（比较前先 `trim_end_matches('/')`）。

### 5.3 测试按钮的失败面

| 情况 | 表现 |
|---|---|
| 地址格式非法（非 http/https） | 服务端校验拒绝，回结构化错误 |
| 地址/密钥错误（401/404） | `ok:false` + 可读 reason（沿用 `test_connection` 文案） |
| 连不上（内网不可达） | 15s 上限内超时，归入「连接超时」文案，不悬挂 |
| 编辑未改 key 且条目已被别处删除 | 报错「找不到模型条目，请重新填写密钥」，不静默用空 key |

### 5.4 兼容性

- 旧 CLI/前端不传 `name` 的新字段：`model/test` 是**新增方法**，老客户端不会调用，
  无兼容问题。
- `model/list` 回包形状不变（迁移在服务端内存里完成，条目形状照旧）。

## 6. 测试策略

### Rust 单元测试

- `models.rs`：精确迁移
  - 旧默认值 `https://api.openai.com` → 改写为 `.../v1`；
  - 带尾斜杠 `https://api.openai.com/` → 同样改写；
  - 自定义地址 `https://llmapi.horizon.auto` → **不动**；
  - 已是新默认 `https://api.openai.com/v1` → 不动（幂等）；
  - anthropic + 任意地址 → 不动。
- `openai/client.rs`：拼 URL
  - base_url 含 `/v1` → 实际路径 `/v1/chat/completions`（**不出现 `/v1/v1`**）；
  - `DEFAULT_BASE_URL` 断言为新值。

### Rust 集成测试（改动量最大）

`crates/yi-agent-llm/tests/openai_integration.rs` 与 `openai_responses.rs`：
wiremock mock 的路径已是 `/v1/...`，而测试里 `base_url: Some(server.uri())` 现在会让
客户端拼出 `/chat/completions`（缺 `/v1`）→ 需把测试的 base_url 改为
`format!("{}/v1", server.uri())`（含尾部斜杠变体的那条用例改为 `"{}/v1/"`）。

### 前端测试

- `SettingsModelsTab.test.tsx`：
  - 改了 key → `model/test` 参数含 `api_key`；
  - 没改 key（编辑已有条目）→ 参数含 `name`、**不含** `api_key`；
  - 提示文案：「openai 且 URL 不以 /v1 结尾」渲染提示；「anthropic」或「含 /v1」不渲染。
- `OnboardingWizard.test.tsx`：默认值断言更新为 `.../v1`。

### 端到端验证

用用户真实配置：把 3 条 horizon 的 `api_url` 改为 `https://llmapi.horizon.auto/v1`，
跑通一次真实对话；并在设置页用「测试」按钮验证成功/失败两条路径。

## 7. 影响面清单

| 文件 | 改动 |
|---|---|
| `crates/yi-agent-llm/src/openai/client.rs` | 默认值 + 两个 path 常量 |
| `crates/yi-agent-runtime/src/config.rs` | openai 默认 api_url |
| `crates/yi-agent-runtime/src/models.rs` | 加载期精确迁移（纯函数 + 单测） |
| `crates/yi-agent-web/src/config_meta.rs` | `OPENAI_BASE_URL` default |
| `crates/yi-agent-app-server/src/server.rs` | 新增 `model/test` async 分支 |
| `crates/yi-agent-app-server/src/model_rpc.rs` | （可选）key 解析辅助 |
| `desktop/src/components/SettingsModelsTab.tsx` | 测试按钮 + 提示 + 入参组装 |
| `desktop/src/lib/models.ts` | `testModel` 封装 |
| `desktop/src/components/OnboardingWizard.tsx` | 默认值 |
| `crates/yi-agent-llm/tests/openai_*.rs` | 测试 base_url 补 `/v1` |
| `crates/yi-agent/src/onboarding_prompt.rs` | CLI 引导默认值 + 相关断言 |
| `crates/yi-agent-runtime/src/config.rs` | 默认值断言（L952） |
| `crates/yi-agent/tests/common/mod.rs` | 测试用默认值 |