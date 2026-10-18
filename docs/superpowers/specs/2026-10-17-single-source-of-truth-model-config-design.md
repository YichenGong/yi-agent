# 设计：模型配置单一真相源（`.env` 退出模型领域）

日期：2026-10-17
状态：待评审
关系：本文是 `2026-10-09-model-catalog-authority-design.md` 的续篇。那篇做的
是「力度二：清单权威、`.env` 兜底可导入」，并在其 §8 明确写下「未来若做力度三
（废掉 `.env` 模型字段），需另开设计与迁移方案」。**本文就是那个被推迟的力度三。**

## 1. 背景与问题

模型配置目前有两个持久化落点，且优先级由一个隐式规则裁决：

```
会话覆盖（thread meta 的 model_ref）
  → models.json 的 default_model 指认的条目     （models.rs:380 resolve_effective）
    → 回退 cfg（.env / 真实环境变量）            （config.rs:223-259）
```

「打架」由此而来：

1. **静默死配置**。清单一旦设了 `default_model`，`.env` 里的
   `YI_AGENT_PROVIDER` / `YI_AGENT_MODEL` / `MODEL_API_URL` / `MODEL_API_KEY`
   就全部失效——用户改了没反应，无从诊断。
2. **空 key 反向清空**。`resolve_effective` 无条件 `out.api_key = entry.api_key`
   （`models.rs:390`）。清单里一条「无 key 条目」会把 `.env` 的
   `MODEL_API_KEY` 悄悄抹成空串。
3. **存在第三层，绕过所有裁决**。`yi-agent-llm` 的 provider 客户端会自己再读一次
   进程环境变量：`anthropic/client.rs:69,76` 读 `ANTHROPIC_API_KEY` /
   `ANTHROPIC_BASE_URL`，`openai/client.rs:115,122` 读 `OPENAI_API_KEY` /
   `OPENAI_BASE_URL`。在「清单没给 key」时它会悄悄生效，且**不经过任何可见的解析链**。
4. **同一模型属性被劈成两半**。`api_style` / `reasoning_effort` 只存在于清单条目，
   而 `model_context_length`（`YI_AGENT_MODEL_CONTEXT_LENGTH`）只存在于 `.env`
   （`config.rs:288`）。
5. **两条腿优先级不一致**。app-server 走 `resolve_effective`（清单 `default_model`
   可盖掉 CLI 显式传的 `--model`）；CLI/TUI 的 `startup_config_from_catalog`
   （`main.rs:1736`）**刻意只带出** `api_style` / `reasoning_effort`，四要素始终来自
   `.env` + overrides。同一份 `models.json`，CLI 跑的和桌面跑的可以是**两个不同模型**。
6. **`onboarding/apply` 制度化双写**。它先写 `.env`，再尝试导入清单，且允许
   `imported: false`（`onboarding_rpc.rs:96-125`）。
7. **`subagent_model` 只存在于清单**，但兜底又回到 `.env` 的 cfg（`subagent/attach.rs:121`）。

## 2. 目标与非目标

### 目标

1. **模型配置只有一个持久化真相源**：`~/.yi-agent/models.json`。
2. 消灭上述 1-7 全部冲突点。
3. 老用户无痛升级：一次性自动迁移，不要求手工搬运。
4. CLI 与 app-server **同一条解析规则**，不再出现「两条腿不一致」。

### 非目标

- 不改非模型设置（`max_turns` / `compact_*` / `sandbox` / `yolo` / `BOCHA_API_KEY` 等），
  它们仍归 `.env`，不存在打架问题。
- 不并 `preferences.json` / `mcp.json`。本次只收模型这一层（方案 A）。
- 不做 keyring 集成。
- 不给 CLI 增加「选条目」的开关（条目选择交给 TUI `/model` 与桌面下拉，YAGNI）。

## 3. 术语

沿用 `2026-10-09` 那篇的词汇：

- **清单条目**：`models.json` 的 `models[]` 中一条，自带 provider / api_url / model / api_key。
- **持久化真相源**：`~/.yi-agent/models.json`。模型配置**只**在这里落盘。
- **整包注入**：CLI flag 或真实进程环境变量给出的一整套模型四要素。
- **迁移**：把存量 `.env` 中的活跃模型键收边进清单，并把它们注释掉。

## 4. 设计

### 4.1 终态：谁持有什么

| 文件 | 持有 | 不再持有 |
|---|---|---|
| `~/.yi-agent/models.json` | **全部**模型配置：`models[]`（每条含 name / provider / api_url / model / api_key / api_style / reasoning_effort / **context_length**）、`default_model`、`subagent_model` | — |
| `~/.yi-agent/.env` | 非模型设置：`YI_AGENT_MAX_TURNS`、`YI_AGENT_COMPACT_*`、`YI_AGENT_SANDBOX`、`YI_AGENT_YOLO`、`YI_AGENT_SYSTEM_PROMPT`、`YI_AGENT_SKILLS_CATALOG_BUDGET`、`BOCHA_API_KEY` | `YI_AGENT_PROVIDER` `YI_AGENT_MODEL` `MODEL_API_URL` `MODEL_API_KEY` `ANTHROPIC_API_KEY` `ANTHROPIC_BASE_URL` `OPENAI_API_KEY` `OPENAI_BASE_URL` `YI_AGENT_MODEL_CONTEXT_LENGTH` |
| `<workdir>/.yi-agent/.env` | **文件取消** | 全部 |
| `<workdir>/.yi-agent/preferences.json` | theme / subagent_runtime / relay_url / watchman / kanban | — |
| 进程环境变量 | 非模型设置 + **模型整包临时覆盖** | — |

`model_context_length` 从 `.env` 移入清单条目（`ModelEntry.context_length`，
`Option<u32>`）。它是模型属性：同一条目的 url / key / model 配套，本就该在一起。
移入后 `ApiStyle` / `reasoning_effort` / `context_length` 三个「模型属性」与四要素同源。

### 4.2 解析与优先级：一句话规则

模型生效值只有一条链：

```
会话覆盖（thread meta 的 model_ref，用户在界面里选的）
  > CLI flag / 真实进程 env 整包注入
    > models.json 的 default_model 条目
      > 无 → 走引导 / 报缺少配置
```

层级理由：会话覆盖是**运行期的显式用户动作**（比进程启动参数更晚、更具体），
故仍在最高层；整包注入是**进程级开关**，压过清单这个持久化默认值，但压不过
「本会话就要用 X」这条更具体的指令。

四条配套规则：

1. **注入必须整包**。「整包」= 把 CLI flag 与**真实进程 env**（快照，见 §4.3）
   合并后的结果。其中 `--provider` + `--api-key` **必须给全**，`--api-url` 可缺省
   （按 provider 推断默认）。只给一半 → 结构化报错，**绝不**拼出「anthropic 地址
   配 openai 模型名」这类畸形组合。`--model` 仍是**模型串**（真正发进请求体的
   字符串），不是条目名。选条目由 TUI `/model` 与桌面下拉负责。
2. **`.env` 文件不再提供模型键**。它只在迁移时被注释掉；此后即使存在也不被读取。
3. **删除第三层**。`yi-agent-llm` 的 `anthropic/client.rs` 与 `openai/client.rs`
   中「自己再读 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `*_BASE_URL`」的兜底**删除**。
   key 只能经 `RuntimeConfig` 单点流入，缺则明确报错。
4. **两条腿对齐**。`startup_config_from_catalog`（CLI）不再只带出 effort / api_style，
   而是与 app-server 共用同一个解析函数。这是本次要修的 §1.5。

### 4.3 dotenvy 陷阱与解法（本设计最关键的一处判断）

`load_env_files` 用 `dotenvy::from_path`（`config.rs:139`），它会把 `.env` 的值
**写进进程环境变量**。因此「`.env` 不提供模型键」这条规则，**光靠「不读 `.env` 里的
模型键」是做不到的**——dotenvy 已经把 `MODEL_API_KEY` 等灌进进程 env，之后的
`std::env::var("MODEL_API_KEY")` 无法分辨它是「用户在 shell 里导出的」还是
「刚从 `.env` 读进来的」。

解法，两步：

1. **加载前快照**：在调用 `load_env_files` **之前**，先对模型键做一次进程 env 快照。
   此后「整包注入」只认这份快照。dotenvy 灌入的模型键被无视。
2. **加载后剔除**：加载 `.env` 后，把「不在快照中」的模型键从进程 env 中**移除**，
   避免残留泄漏给子进程（subagent daemon 会被 spawn 成子进程，若继承到
   `MODEL_API_KEY`，子进程侧就会看到一份「凭空的整包注入」）。

对应地，非模型键的加载行为**完全不变**（`.env` 仍是它们的来源）。

### 4.4 迁移（一次性、自动）

触发点只有三个**用户面向**入口（N2）：

- CLI 交互启动（`run_agent`）
- app-server 启动
- `yi-agent web`

**subagent daemon 只读、不写用户配置**。这样「谁可能改用户配置」在代码里可枚举、
可审计；也避免并发写 `.env`（CLI 与 app-server 同时启动）。

行为：

1. 检出 `.env` 中存在**活跃**（未被注释的）模型键。
2. 若清单**为空** → 先把它们导入成清单里的一条（名字取 `cfg.model`，重名自动加
   `-2` / `-3` 后缀，静默成功），并设为 `default_model`。
3. 若清单**已有内容** → **不导入**（避免覆盖用户既有配置），仅做第 4 步。
4. 把这些行改写为注释：`# MIGRATED: MODEL_API_KEY=...`。原值可查、可手工恢复。
   文件其余内容、既有注释、分组顺序**一律不动**（行级保留式改写）。
5. 若清单本次确有导入，追加一行 warning 日志说明迁移结果。

三种情形对照：

| 情形 | 清单 | `.env` | 进程 env | 动作 |
|---|---|---|---|---|
| (a) | 空 | 有活跃模型键 | — | 导入清单 + 注释 `.env` 键 |
| (b) | 已有内容 | 有活跃模型键 | — | 仅注释 `.env` 键（它们已是死配置）+ warning |
| (c) | — | 无 | 有模型键 | **不迁移、不 warning**（这是 A2 正常注入） |

边界：

- **幂等**：注释后不再有活跃模型键，下次启动自然不再触发。
- **`.env` 不可写**（只读环境 / 容器）→ 降级为「忽略 + 一次性 warning」，
  **不阻断启动**。迁移失败绝不阻塞程序。
- **原子性**：清单写入沿用 `save_catalog_to`（读-改-写 + 临时名 + `rename` 原子替换
  + `0600`）。`.env` 改写沿用行级保留式原子写。任一步失败都不留半成品。

### 4.5 显式 workdir 模式的语义统一

今天 `--workdir <dir>` / `YI_AGENT_WORKDIR` 会让 `is_workdir_explicit` 为真，
从而 `global_env_path = None`（`config.rs:216`）——即**故意跳过全局 `.env`**，
只读项目自己的 `.env`（`2026-07-25-config-layering-design.md` 的「显式指定时完全控制，
不被全局污染」）。T1 砍掉项目级 `.env` 后，该模式会变成「一个配置文件都不读」。

**统一为**：`--workdir` 只决定「工具的工作目录」，配置**一律来自
`~/.yi-agent/.env`**。删除 `is_workdir_explicit` 这条分叉。规则一句话：
「配置永远全局一份，workdir 只影响干活的地方。」

「我不想用全局配置」这个诉求，用 CLI flag / 进程 env 已经能表达。

### 4.6 各入口改造清单

- **CLI / TUI**：`startup_config_from_catalog` 改为调用统一解析函数；
  `--model` 的帮助文本改为「模型串；覆盖时须同时给出 `--provider` 与 `--api-key`」。
- **app-server**：`resolve_effective` 增加「整包注入」这一级，
  `production_factory` 与 CLI 共用同一函数。
- **subagent**：`subagent_entry` 保持「`subagent_model` → 默认条目」，
  去掉回退 `.env` 的兜底（`attach.rs:121`）。
- **`onboarding/apply`**：从**双写**（`.env` + 清单 + `imported` 标记）改为
  **单写清单**；`imported` / `import_error` / `env_written` 字段一并清理。
  仍保留「天然一次原子写」的既有性质。
- **WebUI（`yi-agent web`）**：从 `config_meta::ALL_VARS` **移除 9 个模型键**
  （provider / model / context_length / api_key / api_url / anthropic 两键 /
  openai 两键），表单只留非模型键，并加一行说明「模型请在桌面端『设置 → 模型』
  中配置（写入 `models.json`）」。**不再存在绕过清单的写入口**。
- **`yi-agent-web` 的双 `.env` 路径**：随 T1 简化为单一全局路径
  （`api.rs:31-34` 的 `env_path` / `global_env_path` 收敛为一个）。
- **`models.rs`**：`ModelEntry` 新增 `context_length: Option<u32>`；
  `resolve_effective` 命中条目时写回 `cfg` 的 `compact_threshold`。
- **`config.rs`**：模型四要素不再从 env 读；`model_context_length` 不再从 env 读。
- **`.env.example`**：删掉模型相关行。
- **文档**：`README.md:200` 与 `README.en.md:208-209` 的「项目配置覆盖全局」三行删除；
  配置表与 `--model` 说明重写。

### 4.7 数据流（迁移，情形 a）

```
CLI 启动 ──▶ 读 ~/.yi-agent/.env（行级解析，找活跃模型键）
               │ 有 + 清单空
               ├─▶ save_catalog_to(models.json)：追加条目 + default_model 设为它
               ├─▶ 行级改写 .env：四个键静默成 `# MIGRATED: ...`
               └─▶ warning 日志：Migrated model settings from .env to models.json
             ──▶ 继续正常启动（此时清单已是唯一真相源）
```

## 5. 错误处理与边界

| 情形 | 行为 |
|---|---|
| 整包注入给了一半（如只给 `--provider`） | 结构化错误（`invalid_model`），零副作用 |
| `.env` 不可写（迁移时） | warning + 降级，不阻断启动；清单若无导入则不动 |
| 清单不可写（迁移导入时） | warning + 降级；`.env` **不被注释**（避免「键注释了、清单没落」的数据丢失） |
| 迁移中途 `rename` 失败 | 原子写保证磁盘无半成品；下次启动重试 |
| 无清单且无注入 | CLI 走 `onboarding_prompt`；app-server 走 `load_lenient` + 引导（现状不变） |
| 悬空 `default_model` | 视为无默认，回退注入或引导（现状不变） |
| key 缺失（删掉第三层后） | provider 构造明确报错，文案指向「在设置 → 模型中补 key」 |

注意「清单不可写」与「`.env` 不可写」的**顺序**：迁移必须**先落清单、后注释 `.env`**。
反过来会造出「`.env` 已被注释、清单没落成」→ 用户的 key 凭空丢失。

## 6. 测试策略

`yi-agent-runtime`：

- 整包注入齐备 / 半包 → 裁决正确 / 结构化错误。
- `.env` 中的模型键被无视（含「清单为空时也不生效」）。
- dotenvy 陷阱回归：把 `MODEL_API_KEY` 写进 `.env` → `load` 后 `api_key` **不**取自它；
  且该键**不残留**在进程 env（子进程看不到）。
- `context_length` 随条目生效（`compact_threshold` 由条目算出）。
- 迁移三情形 (a)/(b)/(c) 各自的行为与幂等。
- 迁移时 `.env` 只读 → 降级 + 不阻断。
- 迁移时清单不可写 → `.env` **未**被注释。

`yi-agent-app-server`：

- `resolve_effective` 新增「整包注入」层级的裁决顺序。
- `onboarding/apply` 单写清单（响应不再含 `imported` / `env_written`）。
- `model/list` 的 `effective.source` 在注入生效时的取值。

`yi-agent-llm`：

- 删除第三层后，无 key → 明确 `ProviderError::Auth`（不再静默读 env）。

`yi-agent`：

- **一致性测试**：同一份 `models.json` + 同一组注入，CLI 与 app-server 解析出
  **同一个** provider / api_url / model。这是防「两条腿再次分叉」的回归锁。

真机 e2e：

- `cargo test -p yi-agent --test model_catalog_live_reload_e2e -- --ignored`
  保持绿（本次不破坏热重载）。

## 7. 与既有设计的关系

- `2026-10-09-model-catalog-authority-design.md`：本文是其**力度三**，替换其 §2
  中「不废掉 `.env` 的模型字段」「不改 `.env` 本身」两条非目标。
  其成果（`model/list` 的 `effective` 视图、`model/importEnv`）**保留**——
  `importEnv` 从「兜底层收边」变成「迁移/救援路径」，语义仍成立。
- `2026-10-04-model-settings.md`：其 §11「未配置 models.json 时行为与今天一致」
  **被本文替换**：未配置清单时不再回退 `.env` 模型键，而是走引导。
- `2026-07-25-config-layering-design.md`：其「项目级 `.env` 覆盖全局」与
  「显式 workdir 跳过全局」两条被本文 T1 / U1 取消，非模型键的层级语义保留。
- `2026-10-17-model-catalog-live-reload-design.md`：热重载机制不动，仍是
  「谁用清单、谁负责新鲜度」。

## 8. 未决 / 风险

- **破坏性变更需要发布说明**：老用户升级时模型行为会变（原先靠
  `ANTHROPIC_API_KEY` 兜底生效的会失效）。迁移能覆盖 `.env` 情形，但
  **纯靠 shell 导出 env** 的用户（无 `.env` 文件）会撞上「整包」要求。
  需要在 README / 发布说明里明确写出这条。
- **整包要求的严格度**：`--api-url` 可缺省（按 provider 推断默认），
  但 `--provider` 与 `--api-key` 必须给全。若实践发现太严（如只想临时换 url），
  可考虑放宽，但**本期从严**（从严可放宽，反之则难）。
- **`context_length` 迁移**：`.env` 里的 `YI_AGENT_MODEL_CONTEXT_LENGTH` 是**全局单值**，
  而条目是多条。迁移时只能把它写进**被导入的那一条**（若有）。多条目场景下
  其余条目需用户手工补。此残缺需在 warning 文案里说清。
- **`ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` 的历史用户**：它们此前可绕过
  `MODEL_API_URL` 生效。迁移应把它们视为「活跃模型键」一并注释。
