# 设计：让 models.json 成为权威，`.env` 退居可导入的兜底

日期：2026-10-09
状态：待评审

## 1. 背景与问题

模型设置界面（桌面端「设置 → 模型」tab）唯一的数据来源是机器级清单
`~/.yi-agent/models.json`。而模型的**实际生效值**由一条解析链决定：

```
会话覆盖（thread meta 的 model_ref）
  → models.json 的 default_model 指认的条目
    → 回退 cfg（~/.yi-agent/.env / 真实环境变量 / 项目内 .env）
```

问题由此而来：当用户只配置了 `.env`（`YI_AGENT_PROVIDER` / `YI_AGENT_MODEL` /
`MODEL_API_URL` / `MODEL_API_KEY`）、而 `models.json` 不存在或为空时：

- 会话照常在跑 `.env` 里的模型（thread meta 的 `model` 字段能证明）；
- 设置 tab 却显示「还没有配置任何模型」，因为它的数据源是空的。

也就是说，**兜底层对用户完全不可见，且没有任何工具把兜底层收边到权威层**。
用户看到的是「明明有模型，界面却说没有」。这是本次要修的缺口。

## 2. 目标与非目标

### 目标

1. 设置界面永远如实反映**当前实际生效**的默认模型，并指明它来自清单条目还是
   来自 `.env` 兜底。
2. 当生效值来自 `.env` 兜底时，提供一个**一键导入**入口：把 `.env` 当前的
   provider / api_url / api_key / model 落成清单里的一条，并顺手设为全局默认，
   一步完成。
3. 明确 **models.json 是权威**：只要它能解析出默认条目，就以它为准；`.env` 仅在
   权威层无法定论时静默兜底。

### 非目标

- **不改 `.env` 本身**。CLI、headless、TUI、以及「某项目单独一套模型」等既有
  用法全部保持原样，不受影响。
- **不废掉 `.env` 的模型字段**（明确不做「力度三」）。`.env` 作为兜底层长期保留。
- **不做「测试连接」**、不做 keyring 集成、不做清单条目的重命名引用迁移。
- 不改会话级模型下拉（`ModelPicker`）与子 agent 的解析逻辑。

## 3. 术语

- **权威层（authoritative）**：`models.json` 里 `default_model` 指认的、且确实
  存在于清单中的条目。
- **兜底层（fallback）**：`cfg`（来自 `.env` / 环境变量）。
- **生效解析**：`resolve_effective(cfg, catalog, None)` 的结果——即当前默认
  实际会用哪一套 provider / api_url / api_key / model。
- **一键导入（导入当前配置）**：把兜底层当前的 provider / api_url / api_key /
  model 写成清单里的一条，并把它设为 `default_model`。

## 4. 设计

### 4.1 权威链保持不变，只把它显性化

解析顺序维持现状，不新增层级：

1. 权威层能命中（`default_model` 存在且该名字在清单里）→ 用它，`.env` 不参与。
2. 否则 → 用兜底层 `cfg`。

这条链**本来就已经是「有主有备」**，本次不改语义，只把它的结果告诉用户。

### 4.2 服务端：`model/list` 增加「生效解析」视图

`model/list` 的响应新增一个字段 `effective`，描述当前默认实际解析到哪里。
它由服务端计算（只有服务端同时握有 `catalog` 与 `cfg`）：

```
effective: {
  "source": "catalog" | "env",
  "model_ref": string | null,      // source == "catalog" 时：命中的条目显示名
  "provider": string,
  "api_url": string,
  "model": string,
  "api_key_masked": string,
  "has_key": boolean
}
```

计算规则（复用现有 `resolve_effective` 语义，不另写一套）：

- 令 `entry = effective_entry(catalog, None)`（= `default_model` 命中则给条目，
  否则 `None`）。
- `entry` 为 `Some(e)` → `source = "catalog"`、`model_ref = e.name`，其余字段取自 `e`
  （`api_key` 经 `mask_key`）。
- `entry` 为 `None` → `source = "env"`、`model_ref = null`，其余字段取自 `cfg`
  （`api_key` 经 `mask_key`）。

**API key 安全**：`effective` 里的 key 一律只出掩码（`mask_key`），与既有
`model/list` 对条目的处理一致；**绝不**回传明文。

`model/list` 仍是只读、`Observe` 权限，无副作用。

### 4.3 服务端：新增 `model/importEnv`

新增一个写操作 RPC，需 `Control` 权限（与 `model/upsert` 等同档）。语义：
**把 `cfg`（兜底层）当前的 provider / api_url / api_key / model 原子地写成清单里
的一条，并把它设为 `default_model`。**

- 参数：无（或可选 `{ name? }` 供未来扩展；本期前端不带参）。
- 名字：默认取 `cfg.model` 作为显示名；若该名字已在清单中，则自动加后缀
  `-2`、`-3`… 直到不冲突（静默成功，不报错）。
- 落盘：一次原子写 `models.json`（`save_catalog_to` 已具备 read-modify-write +
  0600 + 保留无关顶层键的语义），新条目 `{name, provider, api_url, model, api_key}`，
  同时 `default_model = <新名字>`。
- 返回：`{ ok: true, name: <新条目名>, default_model: <新名字> }`。
- 前置校验：`cfg.api_url` / `cfg.model` 非空、`cfg.provider` 属于
  `anthropic|openai`，否则返回结构化错误（复用既有校验思路），**零落盘**。
- 幂等性：不追求幂等。重复点会再落一条（名字带后缀），这是可接受的——
  但前端在导入成功后应立即重读、按钮随即消失，正常路径下不会重复点。

一个原子 RPC 而非前端两步（`upsert` + `setDefault`）的理由：只有服务端握有明文
`api_key`（前端拿不到），且两步之间可能半途失败留下「条目落了、默认没设」的中间态。

### 4.4 桌面端：始终显示「当前生效」并给导入入口

`SettingsModelsTab` 的「全局默认模型」区域改为始终渲染一行状态：

- `effective.source == "catalog"`：
  显示 `当前生效：<model>（清单条目「<model_ref>」）`。无按钮。
- `effective.source == "env"`：
  显示 `当前实际在用 <model>（来自 .env，尚未纳入清单）`，并在旁边渲染
  **「导入当前配置」**按钮。点击调用 `model/importEnv`，成功后**重读**
  `model/list`（沿用该 tab 既有的「写后重读、不做乐观更新」约定）；失败内联报错。

「全局默认模型」下拉维持现状（选条目 = 设 `default_model`；「跟随全局默认」=
置空即回到兜底）。因为有了上面的状态行，「置空后其实是回退到 `.env`」这件事
不再隐晦。

**子 agent 不另做一套**：它的解析链终点就是全局默认，全局默认一旦明确（被导入
或被显式指定），子 agent 自然跟着明确。子 agent 若需要与主对话**不同**的模型，
用现有的「子 agent 模型」下拉选一条即可，本次不加额外状态与按钮。

### 4.5 数据流（用户点了「导入当前配置」）

```
桌面端 ── model/importEnv ──▶ app-server
                               │ 读 cfg（兜底层）+ models.json
                               │ 组装新条目（名字去重）→ 一次原子写
                               │ default_model = 新名字
                               ◀── { ok, name, default_model }
桌面端 ── model/list（重读）──▶ ...
                               ◀── effective.source == "catalog"（状态行翻转，按钮消失）
```

## 5. 错误处理与边界

- `models.json` 不可写 / 磁盘错误 → `model/importEnv` 返回内部错误，前端内联报错，
  清单与默认值**均未被改动**（原子写保证）。
- `cfg` 本身非法（如 `api_url` 为空）→ 结构化错误、零落盘。
- `cfg.api_key` 为空（例如用户只配了 url/model，key 靠环境）→ 仍可导入，落一条
  `api_key: ""` 的条目；此时界面该条「密钥」列显示「未设置」。这是如实反映，不是错误。
- 导入后用户又改了 `.env`：清单条目**不跟着变**（它已是独立副本）。这正是
  「显式接管」的语义；如需同步，用户应编辑清单条目。
- 悬空 `default_model`（指向已删除的条目）：`effective_entry` 命中不了 → 归为
  `source == "env"`，状态行如实说「来自 .env」，并给出导入入口。

## 6. 测试策略

服务端（`yi-agent-app-server` / `yi-agent-runtime`，走既有 `handle_model_request_at`
注入式测试，**不碰真实 HOME**）：

- `effective.source == "catalog"`：清单有 `default_model` 指向 A → `model_ref == "A"`，
  字段取自 A，key 掩码。
- `effective.source == "env"`：清单为空 / 无默认 / 默认悬空 → `model_ref == null`，
  字段取自 `cfg`，key 掩码、**不含明文**。
- `model/importEnv`：清单为空 → 落一条、名字 == `cfg.model`、`default_model` 设为它；
  再读 `model/list` 时 `effective.source == "catalog"`。
- `model/importEnv` 重名 → 名字自动加后缀（`-2`），不报错、不覆盖既有条目。
- `model/importEnv` 在 `cfg.api_url` 为空时 → 结构化错误、磁盘无变化。
- 权限：`model/importEnv` 需 `Control`（低权限被拒）。
- 明文 key 永不外泄（沿用既有断言的风格：响应里不存在 `api_key` 的明文）。

桌面端（vitest）：

- `source == "catalog"` → 渲染「清单条目」文案、无导入按钮。
- `source == "env"` → 渲染「来自 .env」文案 + 导入按钮；点击调用 `model/importEnv`
  并重读（注入的 `modelCall` 断言两次调用）。
- 失败路径 → 内联报错、输入/状态不被破坏。

## 7. 与既有设计的关系

本次是 `2026-10-04-model-settings-design.md` 的增量补齐，不改其既定语义：

- 该设计 §11「未配置 `models.json` 时行为与今天一致（走 `.env`/cfg）」**继续成立**；
  本次只是把这条回退**显性化**并给出收边工具（力度二：清单权威、`.env` 兜底可导入）。
- 不触碰会话覆盖（`thread/setModel`）、`ModelPicker`、子 agent 解析。

## 8. 未决 / 风险

- 「一键导入」是否应同时提供「设为子 agent 模型」的选项——本期**不做**，YAGNI；
  用户导入后可在子 agent 下拉里手动选。
- 导入同名时本期采用「加后缀新增一条」，不覆盖、不合并。
- 未来若做「力度三」（废掉 `.env` 模型字段），需另开设计与迁移方案；不在本期。
