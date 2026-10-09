# 设计：首次安装引导（Onboarding）

日期：2026-10-09
状态：待评审

## 1. 背景与问题

yi-agent 的模型配置落在**全局**文件 `~/.yi-agent/.env`（16 个变量，分
Model Provider / Agent / Tools 三组）以及桌面端的机器级清单
`~/.yi-agent/models.json`。一个全新用户装好后，这两处都不存在：

- CLI 裸跑 `yi-agent`（进 TUI）时，`cfg.provider` / `cfg.model` / `cfg.api_key`
  全部为空，用户拿到的是一句 provider 内部错误，没有任何「你先去配一个 key」的
  指引；
- 桌面端 App 起来后能连上 sidecar、能进主界面，但一发消息就报错——用户要自己
  摸到「设置 → 模型」才知道该填什么。

也就是说，**从「装好」到「跑通第一次对话」之间没有任何引导**。缺的不是能力
（设置页已能配模型、`model/importEnv` 已能把 `.env` 收边进清单），缺的是
**首次安装的入口**：自动检测「还没配」，并用一段引导把用户送到「已配好」这一步。

本次要补的就是这个入口。

## 2. 目标与非目标

### 目标

1. **共享一套检测与写入逻辑**（放在 `yi-agent-runtime`），CLI 与桌面端两个入口
   都用它，不各写一份。
2. **自动检测「是否需要引导」**：判据不是「文件是否存在」，而是「是否有可用模型
   配置」——对残缺配置（文件在但缺 key）同样能救。
3. **引导收集模型必需字段并写入全局 `.env`**：`provider` / `api_key` / `model` /
   `api_url`（后两者带 provider 默认值）。
4. **可选测试连接**：填完做一次最小请求验证 key 与地址真的能用，失败给出可读
   原因，但**允许仍然保存**（内网模型测试端点可能不通）。
5. **两端各自的界面**：桌面端全屏向导；CLI 交互式 TUI 首启问答 + `yi-agent run`
   一行可读指引。
6. **结束标记兜底**：用户明确「稍后设置」后不再反复打扰，但不阻断任何操作。

### 非目标

- **不新增 `yi-agent init` 子命令**（YAGNI；TUI 首启已覆盖主要场景，需要时后加）。
- **不引导可选项**：Bocha 搜索 key、context length、max turns 等进阶项不进首次
  引导，用户照旧在设置页或直接改 `.env` 配。
- **不改 `yi-agent-web`**：它的 `env_file::write` 语义（重写全部 16 个变量）保持
  原样，不在本次范围内。runtime 侧新写一个**行级保留式**写入器。
- **不动模型解析链**：权威层（`models.json`）/ 兜底层（`.env`）的既有解析顺序
  不变（见 `2026-10-09-model-catalog-authority-design.md`）。
- **不做 keyring / 系统钥匙串集成**。

## 3. 术语

- **就绪（ready）**：当前配置足以跑起一次对话——权威层能解析出默认条目，或兜底
  层 `.env` 的 provider / model / api_key 齐全。
- **需引导（needed）**：不满足就绪。
- **已结束引导（dismissed）**：用户已经走完引导（完成**或**「稍后设置」），写进
  `~/.yi-agent/preferences.json` 的 `onboarding_dismissed`。这个名字刻意不叫
  `skipped`（跳过）：**完成**也置位，语义是「不再自动弹」而非「用户放弃了」。
- **规范写入（canonical write）**：把引导收敛出的模型字段写进全局
  `~/.yi-agent/.env` 的四个键，其余行与注释原样保留。
- **收边（import）**：桌面端把 `.env` 当前配置经已有的 `model/importEnv` 纳入
  `models.json` 并设为默认。

## 4. 设计

### 4.1 架构与职责划分

三层，各自边界清晰：

| 层 | 位置 | 职责 |
|----|------|------|
| 共享逻辑 | `yi-agent-runtime`（新模块 `onboarding.rs`） | 判定就绪/需引导、原子写全局 `.env`、建连接测试、读写结束标记 |
| 桌面 RPC | `yi-agent-app-server`（新 `onboarding/*`） | 把 runtime 能力暴露给桌面前端 |
| 界面 | `desktop/` 全屏向导；`yi-agent` CLI 问答 + `run` 指引 | 呈现与采集输入 |

**为什么共享逻辑放 runtime**：CLI（`yi-agent`）与 app-server 都依赖
`yi-agent-runtime`，且 runtime 已经握有 `.env` 的路径解析
（`config.rs::resolve_global_env_path`）与 `yi-agent-llm` 依赖（可建连接测试）。
放这里，两个入口共用一份判定与写入，不会漂移。

**为什么不复用 `yi-agent-web/src/env_file.rs`**：它的 `write()` 按固定的
`ALL_VARS`（16 个键）**重写整份文件**——支持它的旧实现把「保留分组注释」当作
卖点，但会丢掉用户自己加的键，不适合引导。runtime 侧新写一个**行级保留式**
合并写入器（读-改-写、只动指定键、保留其余行与注释、原子 rename），与项目既有
的 preferences 写入约定一致（临时文件 + rename）。

### 4.2 检测逻辑（B 为主）

runtime 暴露一个**纯函数** `assess`，输入是「`.env` 解析出的模型字段」+
「可选 `models.json` 清单」，输出 `Assessment`：

```
enum Assessment {
    Ready { source: ReadySource },          // 权威层或兜底层
    Needed { reasons: Vec<Missing> },       // 缺什么，用于向导首屏
}

enum Missing { Provider, ApiKey, Model, ApiUrl }
```

判定规则（与既有解析链同构，不另造一套）：

1. **权威层优先**：`effective_entry(catalog, None)` 命中且该条目 `api_key` 非空
   → `Ready { source: Catalog }`。（桌面端持有清单；CLI 传 `None`，跳过本步。）
2. **否则看兜底层**：`cfg.provider` ∈ {anthropic, openai} 且 `cfg.model` 非空且
   `cfg.api_key` 非空 → `Ready { source: Env }`。
3. **都不满足** → `Needed { reasons }`，逐个列出缺失项（如「未设置 API 密钥」），
   向导首屏据此说清到底缺什么，不做笼统的「未初始化」。

`api_url` 不作为必需项：provider 有默认地址（`ANTHROPIC_BASE_URL` /
`OPENAI_BASE_URL`），空即用默认。它出现在 `Missing` 里只在「用户填了非默认
地址但格式非法」时——这类校验放写入前，与 `model/upsert` 的校验口径一致。

### 4.3 写入器（行级保留式）

runtime `onboarding` 模块提供：

```
pub fn write_model_settings(path: &Path, settings: &ModelSettings) -> Result<()>;
```

- 读入现有 `.env` 全文（不存在则视为空）。
- 对 `YI_AGENT_PROVIDER` / `MODEL_API_KEY` / `YI_AGENT_MODEL` / `MODEL_API_URL`
  四个键：**已存在的行就地替换值，缺失的键追加到文件末尾**（追加处补一行
  `# === Model Provider ===` 分组注释，与既有格式一致）。
- 其余所有行（含用户自定义键、注释、空行）**逐字节保留**。
- 原子落盘：写临时文件 → `rename`。**新建文件用 0600**（key 在其中，与
  `models.json` 同敏感级）；**已存在的文件保留其原有权限位**，不擅自 `chmod`。

**密钥落点说明**：写 `MODEL_API_KEY`（通用键，覆盖 provider 专属键），与
`.env.example` 的语义一致，也与 `models.ts` / `model/importEnv` 读的 `cfg.api_key`
同源。不写 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`，避免「两个键各有值」时的语义
歧义。

### 4.4 连接测试（可选步骤）

runtime 提供：

```
pub async fn test_connection(settings: &ModelSettings) -> ConnectionOutcome;
```

用已有的 `yi_agent_llm::AnthropicProvider` / `OpenaiProvider` 构造**临时**
provider（不进 agent、不注册工具），发一次**最小请求**（1 token 的 ping），把
结果翻成可读原因：

| 情况 | 文案 |
|------|------|
| 401 / 403 | API 密钥无效或无权限 |
| 连接失败 / DNS 解析失败 | 无法连接 API 地址 |
| 模型不存在 / 不被接受 | 模型标识不被该地址接受 |
| 超时 | 连接超时 |
| 其它 | 连接失败：<原始信息> |

- **可选，不是硬门槛**：失败时界面出「仍然保存」按钮，用户可继续。
- **明文 key 全程不落日志、不回显**：沿用项目既有掩码约定
  （`models.rs::mask_key`、`env_file::mask`）。
- 测试**不写任何文件**，纯探测。

### 4.5 引导结束标记

`~/.yi-agent/preferences.json` 新增一个布尔键 `onboarding_dismissed`：

- 读-改-写，保留其余键（与 `settings_store` / `tui/runtime_prefs` 同一约定，
  原子 rename）。
- 用户点「稍后设置」→ 置 `true`；引导成功完成 → 置 `true`（完成即「不再需要
  自动弹」）。
- `true` 之后 `assess` 的结果照算（`Needed` 仍为 `Needed`），但**是否自动弹**
  由调用方结合该标记决定：`should_offer = needed && !dismissed`。
- 设置页/`.env` 始终可用，用户随时能自己配（跳过不等于功能禁用）。

### 4.6 桌面端 RPC（`onboarding/*`）

`yi-agent-app-server` 新增四个方法（权限档与 `model/*` 同层，`Control`）：

| 方法 | 入参 | 回包 | 说明 |
|------|------|------|------|
| `onboarding/status` | `{}` | `{ needed, dismissed, reasons }` | 只读；`reasons` 同 `Missing` |
| `onboarding/apply` | `{ provider, model, api_url, api_key }` | `{ ok, env_written, imported, import_error? }` | 写 `.env` + 尝试收边 |
| `onboarding/test` | `{ provider, model, api_url, api_key }` | `{ ok, reason? }` | 只探测，不落盘 |
| `onboarding/dismiss` | `{}` | `{ ok }` | 写结束标记 |

**`onboarding/apply` 的原子性与部分失败**：

1. 先校验四字段（provider 合法、model/model 非空、api_url 若给则格式合法），
   不合法即结构化错误、**零写入**。
2. `write_model_settings` 写全局 `.env`。失败 → 结构化错误、零写入。
3. 写成功后，**尝试收边**：调 `model/importEnv` 的同等逻辑把配置纳入
   `models.json` 并设默认。
   - 成功 → `{ ok: true, env_written: true, imported: true }`。
   - 失败 → **不吞错**：`{ ok: true, env_written: true, imported: false,
     import_error: "<可读原因>" }`。桌面端仍能正常对话（env 兜底生效），前端
     提示「已保存到 .env，但未纳入模型清单（可稍后在设置里点『导入当前配置』）」。
4. 本步**不**写结束标记——该标记由前端在向导收尾调
   `onboarding/dismiss`（语义为「引导已结束」）统一写，避免两处写同一标记。

`api_key` 只在**本次请求内**使用，绝不回包、绝不落日志。

### 4.7 桌面端界面（A：全屏向导）

新增 `desktop/src/components/OnboardingWizard.tsx`：

- 步骤：**欢迎** → **选 API 格式** → **填 key / 模型 / 地址** → **测试连接
  （可选）** → **完成**。
- 每步都可「稍后设置」（= 调 `onboarding/dismiss` 并关闭向导，进主界面）。
- 字段形状、校验、RPC 封装思路复用 `SettingsModelsTab` / `lib/models.ts`
  的既有约定（`provider: "anthropic" | "openai"`，url 预填默认值）。
- 测试连接失败 → 按钮从「保存」变「仍然保存」，并内联显示可读原因。
- 向导收尾：`onboarding/apply` → 成功或部分成功都进主界面；部分成功时在主界面
  顶部留一条可关闭提示。

**接线**（`App.tsx`）：握手完成后（`ui/settings/read` 之后）查一次
`onboarding/status`；`needed && !dismissed` → 盖住主界面渲染向导；否则照常进主界面。
向导关闭（完成或跳过）后刷新一次 `model/list`，让设置页状态与之一致。

### 4.8 CLI 侧行为（A + B）

- **交互式 TUI 首启**：检测到 `needed && !dismissed` → 进入终端问答（选格式 →
  输 key → 模型 → 地址 → 是否测试连接），完成后进 TUI。复用 runtime 的
  `assess` / `write_model_settings` / `test_connection`。
- **`yi-agent run`（非交互）**：不弹问答。检测到 `Needed` → 打印一行可读中文
  指引后**以非零码退出**：

  > 尚未配置模型：请运行 `yi-agent` 完成初始化，或设置环境变量
  > `ANTHROPIC_API_KEY`（或 `OPENAI_API_KEY`）。

  而不是甩一个 provider 内部错误。
- **TUI 与 `run` 的判定入口不同**：TUI 只看 `.env`（不读清单），与 CLI 既有
  行为一致；桌面端才把 `models.json` 纳入判定。

### 4.9 数据流

```
桌面端 ── onboarding/status ──▶ app-server ── assess(.env, models.json) ──▶ {needed, dismissed, reasons}
        ◀─ needed && !dismissed：渲染向导
用户填字段
        ── onboarding/test（可选）──▶ runtime::test_connection（临时 provider，只探测）
        ◀─ {ok, reason?}
        ── onboarding/apply ──▶ write_model_settings(全局 .env) → importEnv(models.json)
        ◀─ {ok, env_written, imported, import_error?}
        ── onboarding/dismiss（收尾，写"引导已结束"标记）
        ◀─ 进主界面

CLI 首启（TUI）── assess(.env, None) ──▶ needed? 终端问答 ──▶ write_model_settings ──▶ 进 TUI
CLI `yi-agent run` ── assess(.env, None) ──▶ needed? 打印指引 → 非零退出
```

## 5. 错误处理与边界

- **`.env` 不可写 / 磁盘错误** → `onboarding/apply` 结构化错误，零写入；前端
  内联报错，用户可改路径重试。
- **收边（importEnv）失败** → 不吞错，如实回包 `imported: false` + 原因；`.env`
  已写入且生效（env 兜底），功能不受阻。
- **测试连接超时** → 有明显上限（建议 15s），超时归入「连接超时」文案，不悬挂。
- **用户填了 api_url 但格式非法** → 写入前校验拒绝，零写入。
- **`preferences.json` 损坏** → 结束标记读取回退 `false`（照常可能弹引导），
  写入用读-改-写 + 原子 rename，坏文件不阻断启动（与 `settings_store` 同约定）。
- **老用户（`.env` 已齐）** → `assess` 直接 `Ready`，不弹引导。
- **老用户（有清单但无 key）** → 权威层 `api_key` 空 → 落回兜底层判定；两层都
  不齐则 `Needed`，引导能顺手补齐。
- **跳过后再想看引导** → 本期不提供「重新运行引导」入口（YAGNI）；用户直接去
  设置页配，或删掉 `preferences.json` 的 `onboarding_dismissed` 键。
- **多客户端并发写 `.env`** → 行级保留式写入是读-改-写 + 原子 rename；极端并发
  下后写覆盖先写（最后一次写赢），与既有配置写入同一风险等级，不额外加锁。
- **iOS 远端客户端** → 不在本次范围：`.env` 在跑 agent 的主机上，iOS 侧没有可
  初始化的本机配置。iOS 上的 `PairingScreen` 已是它自己的首启门禁。

## 6. 测试策略

**runtime（`cargo test -p yi-agent-runtime`）**

- `assess` 判定矩阵（纯函数，全组合）：
  - 清单命中且 key 非空 → `Ready{Catalog}`（传清单时）。
  - 清单无/空 + `.env` 三件齐 → `Ready{Env}`。
  - 清单无 + `.env` 缺 key → `Needed{[ApiKey]}`；缺 model → `Needed{[Model]}`。
  - 清单传 `None`（CLI 路径）→ 只看 `.env`。
- `write_model_settings`：
  - 空文件/不存在 → 创建并含四个键 + 分组注释。
  - 已存在键就地更新，**其它键与注释逐字节保留**（含用户自定义键）。
  - 缺失键追加。
  - 原子性：写入后原文件内容要么全旧要么全新（构造 rename 路径断言）。
- 结束标记：缺文件读回 `false`；写读往返；损坏文件回退 `false` 不 panic。

**app-server（`cargo test -p yi-agent-app-server`，注入式路径，不碰真实 HOME）**

- `onboarding/status`：needed / ready / dismissed 三种组合的回包。
- `onboarding/apply`：写 `.env` + 收边成功；`.env` 写成功但收边失败 → 回包
  `imported: false` + 原因且 `.env` 内容正确。
- `onboarding/apply` 非法输入 → 结构化错误、磁盘无变化。
- `onboarding/test`：用 **wiremock** 模拟成功 / 401 / 超时，**不调真实 API**。
- `onboarding/dismiss`：写标记并回读。
- 权限：四个方法需 `Control`（低权限被拒）。
- 明文 key 永不外泄（响应里不含明文）。

**desktop（`cd desktop && npx vitest run`）**

- `lib/onboarding.ts` 纯封装单测（方法名 + 参数形状）。
- `OnboardingWizard.test.tsx`：步骤流转、测试失败可「仍然保存」、跳过调
  `onboarding/dismiss`、字段校验。
- `App.test.tsx`：`needed && !dismissed` → 渲染向导；`ready` 或 `dismissed` → 主界面。
- 部分成功（`imported: false`）→ 主界面顶部提示。

**CLI（`cargo test -p yi-agent`）**

- 首启检测 → 进入问答的用例（注入假的 stdin/stdout 或抽出的纯函数）。
- `yi-agent run` 遇 `Needed` → 打印指引 + 非零退出（断言 stderr 文案与退出码）。

## 7. 与既有设计的关系

- 复用 `2026-10-09-model-catalog-authority-design.md` 的解析链与
  `model/importEnv`：引导的收边正是该设计里的「一键导入」，不另造。
- `.env` 作为兜底层长期保留；本次是**写入侧**的补齐（此前只有读与 `importEnv`
  这一个收边动作，没有首次写入的引导）。
- 不改 `.env` 解析、不改会话覆盖、不改 `ModelPicker`。

## 8. 未决 / 风险

- **「重新运行引导」入口**：本期不做，记为后续候选。
- **可选项（Bocha key 等）是否纳入引导**：本期不纳入，记为后续候选。
- **连接测试的请求形状**：按 provider 选最小 chat 请求（1 token）；若某些兼容
  端点对最小请求也有额外要求，需在实现时以 wiremock 固定住形状，必要时放宽为
  「模型列表端点」探测。实现时以能稳定区分 401 / 连不上 / 模型不存在为准。
- **`models.json` 收边在 CLI 侧不需要**：CLI 不读清单，故 CLI 引导只写 `.env`；
  这是有意的非对称，与两端的解析链一致。
