# 去掉 `.env`：配置迁到 `settings.json`

日期：2026-10-31
状态：已批准（阶段一；阶段二按 C 方案另立规格）

## 问题

`~/.yi-agent/.env` 目前承载着 agent 的可调参数（轮次、压缩预算、子 agent 容量、沙箱档位、
技能目录预算、搜索 key）。它带来四个相互独立的问题，前三个已在 2026-10-31 的排查中实测确认。

**1. `yi-agent web` 保存设置会删除它不认识的键。** `env_file::write`
（`yi-agent-web/src/env_file.rs:50`）不是「改一行」，而是**从零重新生成整份文件**：循环源是
`ALL_VARS`（7 条固定元数据），`merged.get(var.key).unwrap_or("")` 决定每行的值。任何不在
`ALL_VARS` 里的键整行消失。实测（把 `env_file.rs` + `config_meta.rs` 原样复制到临时 crate 跑
`write()`）：

```
写入前: YI_AGENT_MAX_TURNS=20 / YI_AGENT_SANDBOX=read-only /
        MY_CUSTOM_FLAG=on / YI_AGENT_SKILLS_DIR=/x
写入后: 只剩 Agent 组 6 个键 + BOCHA_API_KEY，其余三个键全部消失
```

同一机制还制造两个次生问题：

- **未设置的已知键被写成空值**。`get_config`（`api.rs:186`）用「键是否存在」判定 `source`，
  首次保存后所有键都变成 `source: "local"`，UI 从此分不清哪些是真设置的、哪些是默认值。
- **裸写、无权限收紧**。`env_file.rs:80` 用 `std::fs::write` 直接覆盖，实测产物权限
  `-rw-r--r--`；而同一仓库的 `models.rs:698`、`migrate.rs:274` 都显式 `0600`。
  一旦从 web 保存，`BOCHA_API_KEY` 明文会落到 `0644`。

**2. 默认值有三处手抄副本，已经漂移。** 同一份配置的默认值写在三个地方，两组数值不一致：

| 键 | `.env.example` | `config_meta.rs` | Rust 代码（真实生效） |
|---|---|---|---|
| `YI_AGENT_MAX_TURNS` | 20 | 20 | **200**（`config.rs:306`） |
| `YI_AGENT_MAX_DIRECT_CHILDREN` | **4** | 缺失 | **8**（`config.rs:74`） |
| `YI_AGENT_MAX_RESIDENT_SUBAGENTS` | 64 | 缺失 | 64 |

`README.md:290` 写的 200 才是对的。没有任何脚本或测试校验这三处一致。

**3. `.env` 的位置与格式都反直觉。** 唯一的 `.env` 是 `~/.yi-agent/.env`（没有项目级），
但仓库根还有一份 `.env.example`，而 `.gitignore` 又忽略 `.env`/`.env.*`。结果是：用户按习惯
在项目根写 `.env` 完全静默失效，且 `.env.example` 没有任何代码读它。

**4. `YI_AGENT_WORKDIR` 把「启动目录」这条自然规则盖掉了。** `resolve_workdir`
（`config.rs:191`）的优先级是 `--workdir` > `YI_AGENT_WORKDIR` > 当前目录，而工作目录本质上
是会话/项目级属性。桌面端尤其别扭：sidecar 被刻意锚定在实例根的父目录启动
（`bridge.rs:368-380`，防 Finder 启动继承 `/`），于是全局 `YI_AGENT_WORKDIR` 一旦设置就会与
「上次打开的项目」互相打架。

## 目标与非目标

目标：

- `.env` 不再被任何生产代码读取；agent 可调参数落到 `<instance_home>/settings.json`。
- 默认值只有 Rust 一处真相源；web 与桌面端都经 RPC 拿「当前值 + 默认值 + 类型 + 描述」。
- 桌面端获得这些参数的可视化入口。
- 已有用户的存量配置无损迁移。

非目标（本次明确不做）：

- **不删除真实进程环境变量**。`YI_AGENT_HOME`、`YI_AGENT_BUILD_KIND`、
  `YI_AGENT_DISABLE_WATCHMAN`、`YI_AGENT_RUNTIME_DIR`、`YI_AGENT_SKILLS_DIR`、
  `YI_AGENT_TEST_GUARD`、`YI_AGENT_REAL_LLM_*` 必须留在进程环境里：它们由打包
  （`Info.plist` 的 `LSEnvironment`）、构建闸、部署与测试脚本注入，且
  `YI_AGENT_HOME` 在读取任何配置文件**之前**就要用来定位实例根。本次只把它们从
  「配置来源」降级为「临时覆盖层」。
- **不改模型配置**。`models.json` 仍是模型设置的唯一真相源；`.env` 里的模型键早在
  2026-10-17 就已迁走并注释（`migrate.rs`）。
- **不重写 `yi-agent web` 的前端**。阶段一仍用那份手写 HTML，只换存储后端。
- **不做阶段二的浏览器化**。见最后一节。

## 设计

### 存储：`<instance_home>/settings.json`

路径用 `yi_agent_store::paths::home_join("settings.json")`，与 `models.json`、
`websearch.json` 同口径，`$YI_AGENT_HOME` 自动生效。

结构是**类型化**的，不是 `Map<String, String>`：

```jsonc
{
  "agent": {
    "max_turns": 500,
    "max_direct_children": 8,
    "max_resident_subagents": 64,
    "system_prompt": "",
    "skills_catalog_budget": 8192
  },
  "compact": {
    "ratio": 80,
    "user_budget_tokens": 20000,
    "tool_budget_tokens": 12000
  },
  "startup": {
    "sandbox": "workspace-write"
  }
}
```

每个字段都是 `Option<T>`，`None` 表示「未设置，用默认值」。这带来两个好处：

- 默认值只在 Rust 常量里写一次（`config.rs` 现有的 `RESIDENT_SUBAGENTS_DEFAULT` 等）。
- 「恢复默认」= 从 JSON 里删掉该键，而不是写回当前默认值。将来默认值改了，用户跟着变。

上表是**可持久化的全部字段**。`workdir` 与 `yolo` 刻意不在其中，理由见下两段。

**`yolo` 不进 `settings.json`。** 这是本次修复的一个关键收紧：

- `yolo` 是**安全姿态**，必须每次启动显式给出（`--yolo` /
  `--dangerously-skip-permissions` / `YI_AGENT_YOLO`，后者只认字面 `"true"`，
  见 `config.rs:368-370`）。把它持久化会造出「用户某次开了 yolo，之后每次启动都是
  yolo」的静默降级。
- 更具体地说：`sandbox_promotable`（`config.rs:108`）的语义是「启动时非 yolo、
  且沙箱未被显式指定」时才允许运行期提权。若 yolo 来自持久化配置，加载时就已是
  yolo，`sandbox_promotable` 恒为 false，桌面端运行期切 yolo 的能力会被悄悄关掉。
  这一条 `config.rs:1358-1420` 的五个测试（`sandbox_promotable_false_when_*`）钉的
  正是这个不变量。

因此 `settings.json` 里只有 `startup.sandbox`（非安全姿态，只是默认沙箱档位的选择）。

**`startup` 这个组名是刻意选的，不是 `permissions`。** 仓库里已经有
`<workdir>/.yi-agent/permissions.toml`（`yi-agent-core/src/permission.rs:347`），
那是**工具级逐项授权规则**（`[tool_level] bash = false` 之类）。两者语义完全不同：
`permissions.toml` 决定「哪些工具要问」，`settings.json.startup.sandbox` 决定
「沙箱默认档位」。命名为 `permissions` 会让用户以为它们是一回事，故避开。

**不含 `workdir` 字段。** 按已批准的决定，工作目录只由 `--workdir`、启动目录、桌面端选项目
决定；`YI_AGENT_WORKDIR` 这一层从 `resolve_workdir` 删除，`config_meta.rs` 的对应条目一并删除。

**空字符串等于未设置。** 这一条对**所有**类型生效，且必须在加载与写入两侧都实现：

- 字符串字段：`""` 与 `"   "` 都按 `None` 处理（`config.rs` 现有的
  `filter(|s| !s.is_empty())` 已是这个口径，保持）。
- 数值字段：JSON 里不会有「空数字」，但 `settings/write` 收到空串时必须写成
  `null`（删键），**不能**写 `0` 或留空串——否则一个 `max_turns: 0` 会让 agent
  一轮都跑不了。
- 枚举字段（`startup.sandbox`）：`""` 视为未设置；非法值见错误处理一节。

这条规则直接来自对 `env_file::write` 的修复：旧实现会把未设置的键写成 `KEY=`，
读回时是空串而非「未设置」，于是 `get_config` 的 `source` 判定全体失真
（`api.rs:186`）。新实现必须在写入侧就保持区分——未设置就**不写这个键**。

写入复用 `models.rs` 已验证的那套：临时文件 + `rename` 原子替换 + `0600` + 同目录文件锁
（`update_catalog` 模式）。**不要**沿用 `env_file.rs:80` 的裸写。

### 优先级

```
CLI flag / GUI 覆盖  >  真实进程环境变量  >  settings.json  >  Rust 默认值
```

与现状的唯一区别是第三层从 `.env` 换成 `settings.json`。真实环境变量这一层必须保留：
CI、`just test-*`、部署脚本都在用它做临时覆盖。

**`skills_catalog_budget_explicit` 的语义要跟着改。** 它现在（`config.rs:396-400`）的
含义是「用户显式给了预算」，判定源是 `overrides` 或 `YI_AGENT_SKILLS_CATALOG_BUDGET`
环境变量。它决定 `resolve_catalog_budget_policy`（`bootstrap.rs:608`）在技能目录超出
预算时，是直接截断还是**交互式弹「Include all skills?」**。

`settings.json` 里的值算不算 explicit？判定标准：**explicit 只表示「本次启动被显式指定」，
配置文件里的值是「长期偏好」，不算 explicit。**

- 理由：`settings.json` 的定位与前一层 `.env` 完全相同（都是用户事先写好的持久配置）。
  如果把它算作 explicit，那么任何在 UI 里设过一次预算的用户，从此永久失去
  「目录变大时被提示要不要全量加载」这个行为，且无法察觉。
- 实现：`skills_catalog_budget_explicit` 的表达式**原样保留**，不追加
  `settings.skills_catalog_budget.is_some()`。这个字段的 doc comment 要同步写明
  「不含 `settings.json`」。
- 新增一条测试：`settings.json` 设了 budget、无 env、无 override、总数超预算时，
  `skills_catalog_budget_explicit == false`。

### 迁移

**启动序是本节唯一必须做对的事。** 两条迁移（既有的 `migrate_model_keys` 与新的
`migrate_env_settings`）必须**在同一处、按固定顺序、都在 `load_lenient` 之前**执行。
理由：`load_inner`（`config.rs:242-248`）把「快照真实环境变量 → 加载 `.env` → 剔除
`.env` 带来的模型键」放在同一步里。如果迁移发生在 `load_inner` **之后**，`.env` 已经被
`dotenvy` 注入进程环境，而 `.env` 又被改名，则下一次启动时这些值会凭空消失——但
`settings.json` 从未收到它们。那就是半迁移：配置看起来迁了，重启后没了。

顺序（所有生产入口一致）：

```
1. migrate_model_keys(.env → models.json)        ← 既有，模型键
2. migrate_env_settings(.env → settings.json)    ← 新增，A 类键
3. load_lenient()                                 ← 才读配置
```

两个迁移都必须在 `onboarding_prompt::should_offer` 与任何 `RuntimeConfig` 构造之前完成，
与现有 `main.rs:1301` / `main.rs:63` / `server.rs:1959,2047` / `ws.rs:64` 的接线位置相同。

**五个入口，不是三个。** `migrate_legacy_model_keys_on_startup` 目前被四处调用
（`server.rs:1959`、`server.rs:2047`、`ws.rs:64`，以及 CLI 的两个独立位置
`main.rs:63` 的 `web` 与 `main.rs:1301` 的 `run_agent`）。新的迁移必须挂在**同一个**
`migrate_legacy_model_keys_on_startup` 里，让它一并覆盖这四处——否则
`app-server --listen ws://`（桌面端 relay 与手机配对走的就是这条）不会迁移 A 类键，
表现为「桌面端正常、手机端配置丢失」。

迁移逻辑：

1. `<instance_home>/.env` 不存在 → 什么都不做（`NothingToDo`）。
2. 存在 → 读出 A 类键：
   `YI_AGENT_MAX_TURNS`、`YI_AGENT_MAX_DIRECT_CHILDREN`、`YI_AGENT_MAX_RESIDENT_SUBAGENTS`、
   `YI_AGENT_SYSTEM_PROMPT`、`YI_AGENT_SKILLS_CATALOG_BUDGET`、`YI_AGENT_COMPACT_RATIO`、
   `YI_AGENT_COMPACT_USER_BUDGET_TOKENS`、`YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS`、
   `YI_AGENT_SANDBOX`、`BOCHA_API_KEY`。
   （`YI_AGENT_YOLO` 不在此列，见上一节。）
3. **只填充 `settings.json` 里尚未设置的字段**，绝不覆盖已有值。已有文件写不进去时
   （读不出来 / 解析不了）按 `models.rs` 的既有约定 **拒绝写入并保持 `.env` 原样**。
4. `BOCHA_API_KEY` 优先进 `websearch.json` 的 `engines.bocha.api_key`（该文件已热读，
   语义更合适）；`websearch.json` 里已有非空 key 时不动。
5. 以上全部成功后，把 `.env` 改名为 `.env.migrated`。

**为什么改名而不是删除**：那里面是用户唯一一份 `BOCHA_API_KEY` 明文。改名既保证「不再被读」，
又留了取回余地。迁移结果要有一条 `tracing::info`（或缺省 `eprintln`）说明改名去向。

`YI_AGENT_WORKDIR` 在迁移里**直接丢弃**：它的语义已被「启动目录」取代，没有可迁移的落点。
丢弃时打一行 warning，让设置过它的用户知道。

#### `BOCHA_API_KEY` 的 env 回退必须同时删除

`yi-agent-tools/src/web/config.rs:94-101` 现在是三层：

```rust
pub fn resolved_api_key(&self) -> Option<String> {
    if !self.api_key.is_empty() { return Some(self.api_key.clone()); }
    std::env::var("BOCHA_API_KEY").ok().filter(|v| !v.is_empty())   // ← 回退层
}
```

这层回退的**唯一**价值是「老用户不改配置也能继续用」，前提是那个 env 值来自 `.env`。
`.env` 一旦不再加载，这层回退在下次启动时恒为 `None`，于是出现一个很难查的故障：
搜索在迁移后的当前进程里能用（env 还在），重启后静默失效。

所以本次必须**同时**做两件事：

- 迁移把 key 写进 `websearch.json`（第 4 步）。
- 删除 `resolved_api_key` 的 `std::env::var` 回退，只认文件。

`sandbox` 与 `websearch` 的调用方各有四条（`websearch_rpc.rs:58,146`、
`yi-agent-tools/src/lib.rs:159`），删除回退后要一并确认 `has_key` 的语义变成
「文件里有没有 key」——这正是用户想要的答案。

#### 取值口径必须与 dotenvy 一致 现有 `migrate.rs:23` 的 `active_model_keys` 用朴素行拆分，
与 dotenvy 不一致（`bug-list.md:171` 未修，本次实测仍复现）：

```
MODEL_API_KEY=sk-x #note  →  [("MODEL_API_KEY", "sk-x #note")]   ← 注释被并进值
export MODEL_API_KEY=sk-x →  []                                   ← 既不导入也不注释
```

新的 `.env` 读取器必须剥 `export ` 前缀、截断未转义的行内 `#`、剥配对引号。放在
`settings.rs` 里作为一个纯函数，与迁移逻辑分开测试。

### onboarding 的兜底层必须一起改

这是删掉 `.env` 时最容易漏的一条链：引导流程至今把 `.env` 当作**仅次于 `models.json` 的
兜底配置来源**。`.env` 改名后这些代码不会报错，只会安静地拿到空值。

现有的权威链（`onboarding.rs:44` `assess`）：

```
1. models.json 的默认条目可用           → Ready{Catalog}
2. 否则读 .env 的四个模型键，全齐且合法 → Ready{Env}
3. 否则                                  → Needed{缺哪几项}
```

读取点（非测试代码，全部实测确认）：

| 位置 | 用途 |
|---|---|
| `onboarding.rs:169` `read_env_fields` | 本体：解析 `.env` 的四个模型键 |
| `onboarding_rpc.rs:21` | `onboarding/status` 的判定输入 |
| `rpc/thread.rs:55` | `missing_model_error` 的 `reasons` 列表 |
| `onboarding_prompt.rs:15` `should_offer` | CLI 首启是否弹引导，第一个形参就是 `env_path` |
| `server.rs:417` 起共 6 处 | `AppState.onboarding_env_path` 的定义与装配 |
| `rpc/mod.rs:359`、`ws.rs:151` | 参数传递 |

（`main.rs:1298` 的 `prefs_path = env_path.with_file_name("preferences.json")` **不是**问题：
那只是路径拼接，不读文件内容，`.env` 改名后依然正确。）

后果（`.env` 已改名成 `.env.migrated` 后）：`read_env_fields` 永远返回全空，于是
`onboarding/status` 恒报 `needed`、`should_offer` 一直弹引导、`missing_model_error`
恒返回四项全缺。

**处置：删掉 `ReadySource::Env` 这一层。**

- `assess(env, catalog)` 简化为 `assess(catalog)`：只保留「清单默认条目可用 → Ready，
  否则 Needed」。`EnvModelFields`、`read_env_fields`、`ReadySource::Env` 一并删除
  （`EnvModelFields` 在 `onboarding.rs` 内出现 8 次、无跨文件引用，删起来是局部的）。
  `ReadySource` 若只剩一个分支，就直接改成 `Result<(), Vec<Missing>>` 或 bool。
- `should_offer(preferences_path)`：丢掉第一个形参。
- `AppState.onboarding_env_path` 整体删除。`onboarding/apply` 早已**只写
  `models.json`**（`onboarding_rpc.rs:76` 的形参就叫 `_env_path`，测试
  `apply_writes_the_catalog_and_never_the_env` 钉着这点），所以这个 path 在删掉
  读取后没有任何用途；`handle_onboarding_request_at` 的第一个形参随之去掉。
  同时清掉 `server.rs` 的 6 处装配、`rpc/mod.rs:359`、`ws.rs:151` 的传递，
  以及 `rpc_tests.rs` 里 4 处 `onboarding_env_path: PathBuf::new()`。
- **不需要迁移**：模型键早就在 `migrate_model_keys` 里进了 `models.json`，
  所以「模型配置只在 `models.json`」在本 spec 之前就已成立。删掉 Env 层只是删掉
  一个已经不再被写入的读取路径，不丢任何用户数据。
- 顺带修一个既有体验缺陷：`missing_model_error` 在「清单有默认条目但 key 为空」时
  会报「四项全缺」，实际只缺 `api_key`。简化成单一来源后，原因列表必须与 `assess`
  的判据同源，避免两处给出不同的缺失描述。

`onboarding.rs` 现有 20 个 `#[test]`，凡是以 `EnvModelFields` 为输入的都要改成 catalog
输入；`should_offer` 的两条测试同步改签名。

### RPC 面

新增两个方法，归 `Domain::Ui`（与 `ui/settings/read|write` 同域、同权限档）：

| 方法 | 语义 |
|---|---|
| `settings/read` | 返回每个字段的 `{key, group, label, type, value, default, description, options}`；`value: null` 表示未设置 |
| `settings/write` | 部分更新；`null` 或空串表示删除该键（回退默认）；返回被拒绝的字段与原因 |
| ~~`settings/test`~~ | **不做。** 校验规则只存在于 `settings/write` 一处；`read` 已把取值范围与 `options` 交给前端做提示，新增一个「只校验」的方法会让同一套规则存在于两处、迟早漂移。若日后确实需要「输入即校验」，给 `write` 加 `dry_run: true` 参数，而不是新增方法。 |

`settings/write` 的拒绝规则（返回 `invalid_setting`，零写入）：

- 数值字段超出合理范围：`max_turns` / `max_direct_children` 须 `> 0`，
  `compact.ratio` 须 `1..=100`，`compact.user_budget_tokens` 须 `> 0`
  （`config.rs:359` 现在就会对 0 报错，只是报在加载期而非写入期——提前到写入期，
  用户填错时立刻看到，而不是下次启动失败）。
- 枚举字段非法：`startup.sandbox` 只接受 `read-only` / `workspace-write` /
  `danger-full-access`（与 `parse_sandbox_mode` 同一份映射，不另写一遍）。
- 未知的 `key`：拒绝，而不是静默丢弃——静默丢弃会让用户以为保存成功了。

**写入必须保留文件里的未知键。** 用户在 `settings.json` 里手写的任何
`settings.rs` 不认识的顶层键，写入时原样保留（读-改-写，不是重建）。这是
`env_file::write` 那个缺陷的直接教训：它的循环源是元数据表而不是文件内容。

`config_meta.rs` 的 `ALL_VARS` 表删除，元数据改由 runtime 的
`settings.rs` 定义（类型、默认值、分组、描述、可选项），web 与桌面端共用同一份。
这是「默认值只有一处」的落点。

`BOCHA_API_KEY` 不在这张表里 —— 它归 `websearch.json`，已有 `websearch/get|set`。

### 桌面端

`SettingsDialog.tsx` 新增「运行参数」Tab（不复用「通用」：后者的语义是宿主外观，已有主题与
后台值守两节）。按 `agent` / `compact` / `startup` 三组分节渲染，输入控件按类型
（number / text / select）选择。

每项旁边显示「未设置 → 默认 X」；设置过则显示「已设为 Y」并提供「恢复默认」按钮
（发 `value: null`）。

三态必须真的有三态，不能退化成两态：

| 数据 | 显示 | 「恢复默认」按钮 |
|---|---|---|
| `value: null` | 「未设置 · 默认 200」 | 禁用（已经是默认） |
| `value: 500` | 「已设为 500」 | 可用 → 发 `null` |
| `value: ""`（字符串字段） | 与 `null` 同视（见「空字符串等于未设置」） | 禁用 |

`GET` 之后不要在前端把 `null` 归一成 `default`——那正是旧 `get_config` 的做法
（`api.rs:186`），而它导致了「保存一次后所有字段都显示成已设置」。`default` 只用于
展示提示文案，不参与值判定。

### `yi-agent web`

阶段一只换存储，不动页面结构：

- `config_meta.rs` 与 `env_file.rs` 的 `.env` 读写路径整体删除：`settings/*` 的元数据
  （类型、默认值、分组、描述、可选项）由 runtime 的 `settings.rs` 提供，web 只是它的
  HTTP 外壳。`get_config` / `put_config` 改为调该模块的同源函数。
- 依赖方向已验证不会成环：把 `yi-agent-runtime` 从 dev-dependency 提升为正式依赖即可
  （runtime 只依赖 core/llm/tools/mcp/skills/store/supervisors/flags，不依赖 web）。
- **`source` 判定改成基于 `value === null`**，与桌面端同源。现有的
  `local_vars.get(var.key)` 判定必须替换——`env_file::read` 返回的 map 会把
  `KEY=` 也收成一个空串条目，于是「文件中存在但为空」与「已设置」无法区分。
  `source: "local" | "default"` 的取值保留（前端已有依赖），但判定依据换成
  新 `settings/read` 返回的 `value` 是否为 `null`。
- **删除「真实 LLM 测试」面板**及其后端（`api.rs:41-154` 的
  `get_real_llm_test_config` / `put_real_llm_test_config` /
  `validate_real_llm_test_config` / `clear_real_llm_test_key` 四个 handler、
  `lib.rs:12-15` 的 `user_real_test_env_path` 与 `AppState.real_test_env_path`、
  `index.html:329-355` 的 `renderRealLlmTestsTab` 及其分派
  `index.html:322-324`、`assets/` 里的相关标签页条目）。
  理由：这两个职能已被更好的东西覆盖 —— 填配置归「设置 → 模型」（写 `models.json`，
  支持多条目），测连通性归模型条目的「测试」按钮（`model/probe`，见
  `SettingsModelsTab.tsx:899`）。且该面板写的 `~/.yi-agent/.env` 与
  `just test-real-llm` 实际读取的 `yi-agent-rs/.env` 从来不是一个文件。

### justfile

`test-real-llm` / `test-real-e2e` / `test-real-complex` / `test-real-subagent` 四个 recipe
现在都 `source ./.env`（`justfile:100,120,135,152`）。改为从 `models.json` 的默认条目导出
`MODEL_API_KEY` / `YI_AGENT_PROVIDER` / `YI_AGENT_MODEL` / `MODEL_API_URL` 四个变量。

这正好接上 `tests/common/mod.rs:39-140` 的 `resolve_real_llm_test_config` 第 2 级回退
（`MODEL_API_KEY` + provider + model），该回退本来就是这么设计的，只是被 `source .env` 绕过了。
`YI_AGENT_REAL_LLM_*` 专用键保留，仍用于「测一个与日常不同的模型」。

导出逻辑写成一个脚本（`yi-agent-rs/scripts/export-model-env.sh`），四个 recipe 共用，
避免四份重复的表达式。

脚本的三条硬要求：

- **首选 `python3`，回退 `jq`。** `python3` 在本仓库的开发前提（Rust 工具链 + CLT）下必然
  存在，`jq` 则常需 `brew install`。用 `python3` 还能一次读出整个 JSON 再统一处理
  `null`/缺字段，不必为四个变量写四份表达式。
- **两者都缺时打一行提示并 `exit 0`，不 `exit 1`。** 与「无 key 时 real-test 自动跳过、
  exit 0 不算失败」的既有约定一致。缺工具是环境问题，不该伪装成测试失败。
- **只导出非空值。** `models.json` 里某字段为空时不导出空串——否则会把本来能用的真实
  环境变量覆盖成空。这一点由 `unset`/跳过实现，而不是 `export KEY=`。

脚本输出 `export KEY='...'` 形式供 `source`；值里的单引号按 `'\''` 转义（`models.json`
的 key 理论上不含引号，但这是最容易写出未闭合引号的坑）。

## 错误处理

| 情形 | 行为 |
|---|---|
| `settings.json` 缺失 | 全部字段 `None`，用默认值；**不**自动创建文件 |
| `settings.json` 存在但 `JSON` 非法 | 记 `tracing::warn`，回退全 `None`；**绝不**因此阻断启动（与 `models.json`、`preferences.json` 同一约定） |
| 写入时 base 读不出来 | 返回错误、保持文件原样（沿用 `save_catalog_to` 的守卫，防静默覆盖） |
| 枚举值非法（如 `sandbox: "bogus"`） | 加载时回退默认值并 warn；`settings/write` 侧直接拒绝 |
| 数值越界（如 `max_turns: 0`） | 加载时回退默认值并 warn；`settings/write` 侧直接拒绝 |
| 迁移时 `settings.json` 不可写 | 不改名 `.env`，返回 `NotWritable` 并 warn；下次启动重试 |
| 迁移时 `.env` 自身不可读 | 返回 `EnvUnreadable`，不动 `settings.json` |
| `YI_AGENT_HOME` 缺失 | `home_join` 返回 `None`，退到 `./.yi-agent/settings.json`（与 `models_path` 同口径） |

**迁移必须幂等**：连续启动两次，第二次应返回 `NothingToDo`（`.env` 已改名，不存在），
且 `settings.json` 不被改写（mtime 不变）。这条要有测试钉住——否则每次启动都重写一遍
配置，会让盯着文件的 watcher 白跑。

## 测试

每个行为都要有一条会因回退而变红的测试：

- `settings.rs` 纯函数：缺文件 → 全 `None`；坏 JSON → 全 `None` 且不 panic；未知顶层键被保留
  （不丢用户手写内容）；`Option` 往返；`0600` 权限。
- **未知键保留**：写入 `agent.max_turns` 后，文件里用户自己加的 `"my_note": "x"` 仍在。
  这是本 spec 修的第一号缺陷的回归网。
- **不再为未设置字段写空值**：`settings/write` 只改目标字段，其余键不出现。
- **空串等于未设置**：`settings/write` 收到 `""` 时该键被删除（或从未出现），
  读回是 `value: null` 而不是 `""`。
- 迁移：`.env` 有值 → 落进 `settings.json`；`settings.json` 已有值 → **不被覆盖**；
  `.env` 改名成功；`settings.json` 不可写 → `.env` 原样；**连跑两次第二次是 `NothingToDo`
  且 `settings.json` 的 mtime 不变**。
- **启动序**：一条测试断言 `migrate_env_settings` 的调用点位于 `load_lenient` 之前
  （静态扫源码，与 `main.rs:2909` 的 `only_the_lenient_loader_exists` 防回归测试同一手法）。
- **ws 入口也迁移**：assert `ws.rs` 的启动段同时调了两个迁移（否则手机端配置丢失）。
- `.env` 读取口径：`KEY=v #c` → `v`；`export KEY=v` → `v`；`KEY="v"` → `v`。
- **BOCHA 回退已删**：`resolved_api_key()` 在 `websearch.json` 无 key 且
  `BOCHA_API_KEY` 环境变量有值时返回 `None`（钉住「不再读 env」）。
- **onboarding 只剩一条权威链**：`assess` 只吃 catalog；清单默认条目可用 → `Ready`，
  不可用 → `Needed`。`.env` 里写着完整的四个模型键也**不影响**判定（这是钉住
  「兜底层已删」的那条测试，否则将来有人会把 Env 分支加回去）。
- **`should_offer` 不因 `.env.migrated` 存在而误判**：目录里有 `.env.migrated`
  且清单无可用条目时，仍返回 `true`（要弹引导），但判定依据是 catalog 而非那个文件。
- **`skills_catalog_budget_explicit` 不认 `settings.json`**：`settings.json` 设了 budget、
  无 env、无 override、总数超预算时该字段为 `false`。
- **零写入的拒绝**：`settings/write` 传 `max_turns: 0` 或 `sandbox: "bogus"` 时返回
  `invalid_setting`，且文件的 mtime 与内容都不变。
- `YI_AGENT_WORKDIR` 不再影响 `resolve_workdir`（设为任意值后断言仍用当前目录）。
- `domain_for` 表补 `settings/read|write`；**不出现** `settings/test`（钉住它没被顺手加回来）。
- web：`put_config` 不再产生空值行；未知键存活；`source` 基于 `null` 判定；`real-llm` 路由返回 404。
- 桌面端：`SettingsRuntimeTab` 渲染三组、恢复默认发 `null`、未设置显示默认值、
  **`value: null` 时「恢复默认」按钮禁用**。
- **`sandbox_promotable` 不被持久化配置影响**：`settings.json` 里没有任何 yolo 字段可读，
  启动时非 yolo 仍 `sandbox_promotable == true`。

验证命令：

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib settings
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib settings
cd yi-agent-rs && cargo test -p yi-agent-web
cd yi-agent-rs && cargo test -p yi-agent
cd desktop && npx tsc --noEmit && npx vitest run --maxWorkers=1
```

## 文档

同一 commit 内更新：

- `docs/project-management/yi-agent-runtime.md`：删掉「`workdir/.yi-agent/.env` > `~/.yi-agent/.env`」
  这段已过时的分层描述（与同文件第 46 行的「项目级 `.env` 概念一并删除」自相矛盾）。
- `docs/project-management/yi-agent-web.md`：范围边界改为「编辑 `settings.json`」，删掉
  「7 个非模型环境变量」的表述。
- `README.md:216-236` 的「配置放在哪」一节：改四层为
  「默认值 / `settings.json` / `models.json` / 命令行与真实环境变量」，并删掉
  「`~/.yi-agent/.env` 是唯一的 `.env`」那段（届时已无 `.env`）。
- `.env.example`：删除。已核实没有任何 Rust 代码读它（`grep -rn "env.example" yi-agent-rs/crates`
  只命中测试里的假 URL `https://env.example`），留着只会误导用户去项目根建 `.env`。
- `docs/bug-list.md`：一号缺陷（web 保存删键）与 `bug-list.md:171` 的 `.env` 解析口径问题
  一并标为已修，附本次的回归测试名。

## 阶段二：`yi-agent web` 复用桌面前端（C 方案）

不在本次实现范围，但方向已定，且阶段一为它铺好了地基。

现状约束（已核实）：

- web 是一份 16KB 手写 HTML（`include_str!("assets/index.html")`），零构建。
- 桌面端是 967KB 的 React bundle，`transportFactory.ts:34` 在无远端配对时走
  `tauriTransport()`，浏览器里没有 Tauri IPC 会直接失败。
- `ws.rs:279` 的生产路径**只认设备 token**（`Authorization: Bearer` 或 `?token=`），
  没有「本机回环免鉴权」分支。

因此阶段二需要三件事：给 `transportFactory` 加「本地 WS 模式」；给浏览器一条拿到 token 的
路径（`yi-agent web` 启动时打印一次性配对码）；解决 `desktop/dist` 如何进入 `yi-agent-web`
二进制（已定：构建期嵌入，缺失则在 `yi-agent web` 启动时硬失败，见下）。

阶段一交付 `settings/*` RPC 面正是这三件事的前置条件 —— 所以本次改动不会被阶段二绑死。

### 前端资源的嵌入：缺失即报错，但报错的位置要分层

`desktop/dist` 是 1.0M、6 个文件、文件名带内容哈希（`assets/index-Dl3f9nH0.js`）。
所以嵌入的不是一个 HTML 字符串，而是一张「路径 → 字节」的表。现状 `api.rs:38` 只有一个
`index_html` 返回 `&'static str`，阶段二需扩成静态资源路由。

**不做降级。** 降级会让「为什么这个功能不见了」变成一个无法回答的问题：用户看到的是
一个能正常打开、但能力不完整的界面，既没有报错也没有提示，只会以为功能坏了。
宁可硬失败，让「没构建前端」这个原因在第一时间暴露。

但「报错」必须报在正确的层，否则会误伤与前端无关的所有人：

| 层 | 行为 | 理由 |
|---|---|---|
| `build.rs` | **永不失败**。`dist` 存在则嵌入，不存在则生成空表 + `HAS_BUNDLE = false` | `justfile:23` 是 `cargo test --all-features --workspace`、`justfile:27` 是 `cargo build --workspace`，`just ci` 依赖两者，而 CI 不构建前端。`build.rs` 一旦 panic，整个 workspace 的编译与测试全部瘫痪 |
| `yi-agent web` 启动 | **硬失败**：打印「前端未构建，请先 `cd desktop && npm ci && npm run build`」并 `exit 1` | 只有真正要跑 web UI 的人受影响。不启动服务、不监听端口、不给出半成品页面 |
| `just package-all` / release | **硬失败**：shell 层先断言 `desktop/dist/index.html` 存在 | 发布产物必须自带完整 UI，否则所有用户的 `yi-agent web` 都是坏的 |

**这样做的净效果**：`cargo build` / `cargo test` / CI 零 Node 前置；开发者忘了构建前端时，
`yi-agent web` 立刻给出可执行的修复指令；发布流程不可能漏掉前端。

`build.rs` 的实现要点：

```rust
// 扫描 ../desktop/dist（相对 crate 根），生成 assets_generated.rs
pub static BUNDLE: &[(&str, &[u8], &str)] = &[
    ("/index.html", include_bytes!("..."), "text/html"),
    ("/assets/index-Dl3f9nH0.js", include_bytes!("..."), "text/javascript"),
    // ...
];
pub const HAS_BUNDLE: bool = true;   // dist 缺失时为 false、表为空
```

- `println!("cargo:rerun-if-changed=../desktop/dist")` 不能漏，否则前端重新 `npm run build`
  后 Rust 不重编，用户看到的是旧界面。
- `content-type` 按扩展名映射（`.js` / `.css` / `.svg` / `.html` / `.woff2` / `.png`），
  未识别的按 `application/octet-stream`。

`yi-agent web` 的检查点在 `serve()` 的最开头，早于 `TcpListener::bind`：

```rust
if !HAS_BUNDLE { bail!("前端未构建……") }
```

**`assets/index.html`（手写的那份）在阶段二删除**，与「不做降级」一致：留着它就会诱导
后来的人把它当兜底接回去。

**测试**：

- 缺 `dist` 时 `cargo build --workspace` 与 `cargo test --workspace` 必须通过
  （这是 CI 的硬约束，也是本设计唯一不可让步的一条）。
- 缺 `dist` 时 `yi-agent web --port 0` 以非零码退出，stderr 含「前端未构建」。
- 有 `dist` 时 `/` 返回 200 且含 `<div id="root">`；`/assets/<表中某文件>` 返回 200
  且 `content-type` 正确；`/assets/不存在.js` 返回 404（**不回落成 index.html**——
  单页应用「未知路径一律 index.html」的惯例会掩盖真实问题，这里明确不采用）。

**为什么不改成「运行期读 `desktop/dist` 目录」**：那样没有构建期耦合，改前端不用重编，
但发布出去的二进制会依赖一个外部目录，单文件分发就没了；且换台机器跑会直接失败，
而失败原因（目录不在）与「配置坏了」难以区分。构建期嵌入让产物自包含。

**为什么不 `build.rs` 直接失败**：见上表第一行——它会连带 `cargo test` 与 CI 一起红，
把「没装 Node」的成本转嫁给完全不碰前端的 Rust 任务。报错该报给用 web UI 的人，
而不是报给所有编译这个仓库的人。

## 附：本 spec 的自审记录

初稿完成后做了一轮逐条核对（核实每条断言对应的代码/行号/行为，而非复查自己的措辞），
发现并修掉五个问题。记录在此，便于审阅时判断改动是否到位。

**1. 启动序没写死（会导致半迁移）。** 初稿只说「在读取配置之前」，但没说明两条迁移的
相对顺序、也没说清 `load_inner` 里「环境快照 → 加载 `.env` → 剔除模型键」是同一段。
若迁移落在 `load_inner` 之后，`.env` 已注入进程序环境又被改名，下次启动值就凭空消失。
现已在「迁移」一节写死三步顺序，并要求两条迁移挂在同一个函数里。

**2. 漏了 ws 入口（会让手机端丢配置）。** 初稿写「三个生产入口」，实际
`migrate_legacy_model_keys_on_startup` 被四处调用（`server.rs:1959`、`server.rs:2047`、
`ws.rs:64`，加 CLI 的两个独立位置）。`app-server --listen ws://` 正是桌面 relay 与手机
配对走的路径，漏掉它表现为「桌面正常、手机配置丢失」。

**3. `BOCHA_API_KEY` 的 env 回退没删。** `web/config.rs:94-101` 在文件无 key 时回退读
`std::env::var("BOCHA_API_KEY")`。`.env` 不再加载后，该回退在重启时恒为 `None`，
制造「当前进程能用、重启后静默失效」的难查故障。已明确要求同时删除回退。

**4. `permissions` 组名与既有 `permissions.toml` 冲突。** 仓库已有
`<workdir>/.yi-agent/permissions.toml`（逐工具授权规则），语义完全不同。已改名为
`startup`，并在 spec 里写明两者区别，避免用户误以为是同一份配置。

**5. `yolo` 不该持久化。** 它会破坏 `sandbox_promotable` 的不变量（启动时非 yolo 才允许
运行期提权），且把「一次开过 yolo」变成「永久 yolo」。已从 `settings.json` 移除，
并在测试一节加了钉住该不变量的用例。

**6.（第二轮）漏了 onboarding 的 `.env` 兜底层。** 这是第二轮自审最重要的发现，初稿完全
没提。引导流程（`onboarding.rs:44` 的 `assess`）把 `.env` 当作仅次于 `models.json` 的
配置来源，`onboarding/status`、`should_offer`、`missing_model_error` 三处都在读它。
`.env` 改名后这些代码不报错，只会静默拿到空值，表现为「每次启动都弹引导」「错误信息
恒报四项全缺」。已新增「onboarding 的兜底层必须一起改」一节，要求删掉
`ReadySource::Env` 与 `AppState.onboarding_env_path`。幸运的是 `onboarding/apply`
早就只写 `models.json`，所以这一步不丢用户数据。

**7.（第二轮）`skills_catalog_budget_explicit` 没定义新语义。** 它决定技能目录超预算时
是直接截断还是交互式询问。`settings.json` 的值算不算 「显式指定」是个必须选边的叉路，
初稿没写，实施时会随手二选一。已定：**不算**，因为配置文件是长期偏好而非本次启动的
显式指令；判定表达式原样保留。

**8.（第二轮）前端资源降级方案被推翻。** 初稿写的是「缺 `dist` 就用占位页降级」，
目的是让纯 Rust 开发不受 Node 阻塞。用户否决了这个方向并给出理由：降级会让「为什么功能
不见了」无法回答，用户看到的是能打开但能力不全的界面，只会以为功能坏了。改判为**硬失败**，
但把失败点从 `build.rs` 移到 `yi-agent web` 启动处——因为 `justfile:23/27` 的
`cargo test --workspace` 与 `cargo build --workspace` 是 CI 入口，CI 不构建前端，
`build.rs` 一旦 panic 就会把「没装 Node」的成本转嫁给所有 Rust 任务。

此外补了两处初稿含糊的地方：**空字符串等于未设置**（覆盖字符串/数值/枚举三类，且必须在
写入侧实现，否则会重演 `env_file::write` 写空占位导致 `source` 失真的老问题）；
**`value: null` 的三态渲染**（未设置 / 已设置 / 恢复默认按钮是否可用），避免前端又退化成
两态。

### 三个待定项的裁决（2026-10-31 已定）

- **`settings/test` 砍掉。** 校验只存在于 `settings/write` 一处，`read` 已把取值范围与
  `options` 交给前端做提示。若日后需要「输入即校验」，给 `write` 加 `dry_run` 参数，
  不新增方法。
- **`export-model-env.sh` 首选 `python3`、回退 `jq`，两者都缺则提示并 `exit 0`。**
  缺工具是环境问题，不该伪装成测试失败（与「无 key 时 real-test 自动跳过」同一约定）。
- **前端资源缺失即报错，不降级；但报错位置分层**（`build.rs` 永不失败、
  `yi-agent web` 硬失败、发布流程硬失败），见「前端资源的嵌入」一节。

三项都已写入正文，不再是开放问题。
