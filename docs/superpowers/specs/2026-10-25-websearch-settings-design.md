# Design: 桌面设置页「网页搜索」

## Context

Web 搜索目前只有一条配置通路：环境变量 `BOCHA_API_KEY`。桌面 App 的设置页
（8 个 Tab：通用 / 远程访问 / 插件 / 模型 / 技能 / MCP / 标签 / 实验功能）里
没有任何网页搜索入口。用户要么改 `~/.yi-agent/.env` 再重启，要么根本不知道
这个能力存在。

现存约束（代码事实）：

- `WebSearchTool` 仅在 `BOCHA_API_KEY` 存在时才注册（`yi-agent-tools/src/lib.rs:135`）。
  没有 key 时工具**静默消失**，模型看不到它，用户也无从判断是"没配"还是"坏了"。
- 每次结果条数硬编码 25（`web/search.rs:10`），`freshness` 硬编码 `"noLimit"`、
  `summary` 硬编码 `true`（`web/bocha.rs:110-114`）。
- `base_url` 可通过 `with_base_url` 注入（`web/bocha.rs:28`），但没有配置出口。
- `.env` 只在进程启动时加载一次（`runtime/src/config.rs:167`），改完必须重启。

## Scope

- 新增机器级配置文件 `~/.yi-agent/websearch.json`。
- 桌面设置页新增「网页搜索」Tab。
- 可配参数：启用开关、API Key、Base URL、结果条数、时效范围。
- 新增「测试连接」：用当前配置发一条真实搜索，立刻验证可用性。
- **多引擎架构，但只实现 Bocha。** 配置文件与 UI 都为后续引擎留位，
  加 Tavily / SearXNG 时只需加一个 provider 实现与一条 UI 卡片。

**不在本次范围**：其他引擎的实现、搜索结果的缓存策略、多引擎 fallback 顺序。

## Architecture

```
~/.yi-agent/websearch.json
        ↓ 每次搜索前热读
yi-agent-tools/src/web/config.rs  (WebSearchConfig / EngineConfig / Freshness)
        ↓
BochaSearchProvider (base_url, freshness 注入)  →  WebSearchTool (count 默认值注入)
        ↑
yi-agent-app-server/src/rpc/websearch.rs  (websearch/get|set|test)
        ↑ JSON-RPC
desktop/src/components/SettingsWebSearchTab.tsx
```

三个设计取舍：

**为什么是 json 而不是 `.env`。** `.env` 只在启动时读一次，改完要重启；
`websearch.json` 每次搜索前热读，保存即生效。且与 `models.json`
（`runtime/src/models.rs:1`）、`flags.json`、`preferences.json` 的机器级偏好
模式一致。

**为什么没有 key 也注册工具。** 现在的行为是静默不注册，用户看不出差别。
改为注册后在调用时返回 "not configured"，模型会把这个原因讲给用户，
设置页也能明确显示"未配置 API Key"。

**为什么配置读取放在 tools crate 而不是 runtime。** `WebSearchTool` 在构造时
就需要引擎实例，配置是它的直接输入。放 runtime 会引入反向依赖。

## Data Model

文件：`~/.yi-agent/websearch.json`（实例目录不可解析时回退
`.yi-agent/websearch.json`，与 `models_path` 同口径，见 `runtime/src/models.rs:288`）。

```json
{
  "enabled": true,
  "active_engine": "bocha",
  "engines": {
    "bocha": {
      "api_key": "",
      "base_url": "https://api.bocha.cn/v1",
      "count": 25,
      "freshness": "noLimit"
    }
  }
}
```

字段命名统一 snake_case，与 `models.json`（`api_key` / `base_url` / `default_model`）
一致。

字段约束：

| 字段 | 类型 | 默认 | 约束 |
|---|---|---|---|
| `enabled` | bool | `true` | — |
| `active_engine` | string | `"bocha"` | 未知值回退 `bocha` |
| `engines` | map | 只含 `bocha` | 键名即引擎 id |
| `bocha.api_key` | string | `""` | 空则回退环境变量 `BOCHA_API_KEY` |
| `bocha.base_url` | string | `https://api.bocha.cn/v1` | 空则用默认值 |
| `bocha.count` | number | `25` | 钳制到 1–50 |
| `bocha.freshness` | enum | `noLimit` | `noLimit` / `oneDay` / `oneWeek` / `oneMonth` / `oneYear` |

**容错**：文件缺失、不可读、JSON 损坏一律回退到全默认值并 `tracing::warn!`
（缺失文件是首次运行的正常情形，不打日志）。坏配置绝不阻断搜索启动。

**向后兼容**：`api_key` 为空时读 `BOCHA_API_KEY` 环境变量。老用户不改配置
也能继续用。

## Rust Changes

### 1. `yi-agent-tools/src/web/config.rs`（新增）

- `WebSearchConfig` / `EngineConfig` / `Freshness` 三个 serde 结构。
- `WebSearchConfig::load(path)` — 容错读，返回默认值兜底。
- `Freshness::as_bocha_str()` — 映射到 Bocha 的 `freshness` 取值。
- `EngineConfig::resolved_api_key()` — 优先级：文件值 > `BOCHA_API_KEY` 环境变量。
- `EngineConfig::clamped_count()` — 钳制到 1–50。
- `websearch_path()` — 定位配置文件，与 `models_path` 同口径。

### 2. `yi-agent-tools/src/web/bocha.rs`

- `BochaSearchProvider::new(api_key)` 保持签名不变（现有测试与调用方不动）。
- 新增 `with_options(api_key, base_url, freshness)`；`new` 转调它。
- `search()` 里的硬编码 `freshness: "noLimit"` 改读实例字段。
- 错误映射（401 / 403 / 429）保持原样复用。

### 3. `yi-agent-tools/src/web/search.rs`

- `WebSearchTool` 增加 `default_count: usize` 字段，构造时注入。
- `new(engine)` 保留（默认 25），新增 `with_default_count(engine, count)`。
- `call()` 里 `args.count.unwrap_or(DEFAULT_COUNT)` 改读 `self.default_count`。

### 4. `yi-agent-tools/src/lib.rs`

注册逻辑改为：

```
读 WebSearchConfig
├─ enabled == false           → 不注册（用户明确关掉）
└─ enabled == true
   ├─ 有 key（文件或环境变量） → 注册 BochaSearchProvider
   └─ 无 key                   → 仍注册，provider 为 UnconfiguredProvider
                                  （调用返回 "web search is not configured"）
```

`UnconfiguredProvider` 是 `WebSearchProvider` 的一个极简实现：`name()` 返回
`"unconfigured"`，`search()` 直接返回 `ToolsError::SearchEngine`。

### 5. `yi-agent-app-server/src/rpc/websearch.rs`（新增）+ `rpc/mod.rs` 注册

| 方法 | 行为 | 权限 |
|---|---|---|
| `websearch/get` | 读配置，`api_key` 回掩码 `••••1234`（复用 `models::mask_key`） | Observe |
| `websearch/set` | 写配置；`api_key` 等于掩码或为空则跳过不写 | Control |
| `websearch/test` | 发一次真实搜索（`query="yi-agent"`, `count=1`），30s 超时，返回 `{ok, message, elapsed_ms}` | Control |

**字段命名统一 snake_case**（与 `model/list` 的 `api_key_masked`、`model/test` 的
`api_key` 一致），不引入 camelCase。

`websearch/test` 的 key 解析与 `model/test` 同语义（`model_rpc.rs:412`）：参数里
给了明文 `api_key` 就用它；否则（掩码或未传）按服务端已存的明文 key 发探测，
绝不用空 key 发一次注定 401 的请求。

`websearch/set` 是读-改-写并保留无关键，落盘用临时文件 + rename 保证原子性
（与 `settings_store.rs` 同一约定）。

## Desktop Changes

### `desktop/src/components/SettingsWebSearchTab.tsx`（新增）

插在 `TABS` 的「MCP」之后（`SettingsDialog.tsx:19`）。

面板结构：

1. **总开关**「启用网页搜索」→ `enabled`
2. **搜索引擎**：一张 Bocha 卡片，选中态高亮；多引擎时横向排列
3. **Bocha 字段**：API Key（password 输入，掩码作 placeholder）、Base URL、
   结果条数（number，1–50）、时效范围（select 五项）
4. **底部**：「保存」+「测试连接」；测试结果就地展示（成功绿 / 失败红 + 原因）
5. **未配置提示**：`api_key` 为空时提示"未配置 API Key，网页搜索将不可用"

### 宿主接线

- `App.tsx` 新增 `websearchCall`，照抄 `modelCall` 的三行模式
  （`App.tsx:434`，空依赖保持稳定身份）。
- `SettingsDialog` 新增 `websearchCall` prop 并透传给新 Tab。

### 测试

`SettingsWebSearchTab.test.tsx` 覆盖：

- 读取后回填各字段；`api_key` 显示掩码
- 改单个字段点保存，只传改动项
- 保存时掩码值不被回写为真实 key
- 测试连接成功 / 失败两种展示
- 无 key 时显示未配置提示

## Error Handling

| 情形 | 行为 |
|---|---|
| 配置文件缺失 | 用默认值，不打日志（首次运行正常） |
| 配置文件损坏 | 用默认值 + `tracing::warn!` |
| 无 key 时调用搜索 | 返回 "web search is not configured"，模型转述给用户 |
| `count` 越界 | 钳制到 1–50，不报错 |
| `freshness` 未知值 | 回退 `noLimit` |
| 测试连接失败 | 翻译成 401 key 无效 / 403 余额不足 / 429 限流 / 超时 / 网络错误 |
| 写配置失败 | RPC 返回错误，UI 保留未保存状态并提示 |

## Migration

无破坏性迁移：老用户 `BOCHA_API_KEY` 环境变量仍是有效兜底。首次打开设置页时
文件不存在，展示默认值；点保存才创建文件。
