# 去掉 `.env`：配置迁到 `settings.json`

日期：2026-10-31
状态：已批准（阶段一；阶段二按 C 方案另立规格）

## 问题

`~/.yi-agent/.env` 目前承载着 agent 的可调参数（轮次、压缩预算、子 agent 容量、沙箱、yolo、
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
  "permissions": {
    "yolo": false,
    "sandbox": "workspace-write"
  }
}
```

每个字段都是 `Option<T>`，`None` 表示「未设置，用默认值」。这带来两个好处：

- 默认值只在 Rust 常量里写一次（`config.rs` 现有的 `RESIDENT_SUBAGENTS_DEFAULT` 等）。
- 「恢复默认」= 从 JSON 里删掉该键，而不是写回当前默认值。将来默认值改了，用户跟着变。

**不含 `workdir` 字段。** 按已批准的决定，工作目录只由 `--workdir`、启动目录、桌面端选项目
决定；`YI_AGENT_WORKDIR` 这一层从 `resolve_workdir` 删除，`config_meta.rs` 的对应条目一并删除。

写入复用 `models.rs` 已验证的那套：临时文件 + `rename` 原子替换 + `0600` + 同目录文件锁
（`update_catalog` 模式）。**不要**沿用 `env_file.rs:80` 的裸写。

### 优先级

```
CLI flag / GUI 覆盖  >  真实进程环境变量  >  settings.json  >  Rust 默认值
```

与现状的唯一区别是第三层从 `.env` 换成 `settings.json`。真实环境变量这一层必须保留：
CI、`just test-*`、部署脚本都在用它做临时覆盖。

### 迁移

三个生产入口（CLI `run_agent`、`yi-agent web`、app-server 的 `run` 与
`serve_stdio_with_relay`）在**读取配置之前**各调一次，与现有
`migrate_model_keys` / `migrate_legacy_model_keys_on_startup` 同一位置、同一幂等性要求：

1. `<instance_home>/.env` 不存在 → 什么都不做（`NothingToDo`）。
2. 存在 → 读出 A 类键：
   `YI_AGENT_MAX_TURNS`、`YI_AGENT_MAX_DIRECT_CHILDREN`、`YI_AGENT_MAX_RESIDENT_SUBAGENTS`、
   `YI_AGENT_SYSTEM_PROMPT`、`YI_AGENT_SKILLS_CATALOG_BUDGET`、`YI_AGENT_COMPACT_RATIO`、
   `YI_AGENT_COMPACT_USER_BUDGET_TOKENS`、`YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS`、
   `YI_AGENT_YOLO`、`YI_AGENT_SANDBOX`、`BOCHA_API_KEY`。
3. **只填充 `settings.json` 里尚未设置的字段**，绝不覆盖已有值。已有文件写不进去时
   （读不出来 / 解析不了）按 `models.rs` 的既有约定 **拒绝写入并保持 `.env` 原样**。
4. `BOCHA_API_KEY` 优先进 `websearch.json` 的 `engines.bocha.api_key`（该文件已热读，
   语义更合适）；`websearch.json` 里已有非空 key 时不动。
5. 以上全部成功后，把 `.env` 改名为 `.env.migrated`。

**为什么改名而不是删除**：那里面是用户唯一一份 `BOCHA_API_KEY` 明文。改名既保证「不再被读」，
又留了取回余地。迁移结果要有一条 `tracing::info`（或缺省 `eprintln`）说明改名去向。

`YI_AGENT_WORKDIR` 在迁移里**直接丢弃**：它的语义已被「启动目录」取代，没有可迁移的落点。
丢弃时打一行 warning，让设置过它的用户知道。

**取值口径必须与 dotenvy 一致。** 现有 `migrate.rs:23` 的 `active_model_keys` 用朴素行拆分，
与 dotenvy 不一致（`bug-list.md:171` 未修，本次实测仍复现）：

```
MODEL_API_KEY=sk-x #note  →  [("MODEL_API_KEY", "sk-x #note")]   ← 注释被并进值
export MODEL_API_KEY=sk-x →  []                                   ← 既不导入也不注释
```

新的 `.env` 读取器必须剥 `export ` 前缀、截断未转义的行内 `#`、剥配对引号。放在
`settings.rs` 里作为一个纯函数，与迁移逻辑分开测试。

### RPC 面

新增三个方法，归 `Domain::Ui`（与 `ui/settings/read|write` 同域、同权限档）：

| 方法 | 语义 |
|---|---|
| `settings/read` | 返回每个字段的 `{key, group, label, type, value, default, description, options}`；`value: null` 表示未设置 |
| `settings/write` | 部分更新；值为 `null` 表示删除该键（回退默认） |
| `settings/test` | 可选：校验 `sandbox` 等枚举值的合法性，失败返回 `invalid_setting` |

`config_meta.rs` 的 `ALL_VARS` 表删除，元数据改由 runtime 的
`settings.rs` 定义（类型、默认值、分组、描述、可选项），web 与桌面端共用同一份。
这是「默认值只有一处」的落点。

`BOCHA_API_KEY` 不在这张表里 —— 它归 `websearch.json`，已有 `websearch/get|set`。

### 桌面端

`SettingsDialog.tsx` 新增「运行参数」Tab（不复用「通用」：后者的语义是宿主外观，已有主题与
后台值守两节）。按 `agent` / `compact` / `permissions` 三组分节渲染，输入控件按类型
（number / text / select / switch）选择。

每项旁边显示「未设置 → 默认 X」；设置过则显示「已设为 Y」并提供「恢复默认」按钮
（发 `value: null`）。

### `yi-agent web`

阶段一只换存储，不动页面结构：

- `config_meta.rs` 与 `env_file.rs` 的 `.env` 读写路径整体删除：`settings/*` 的元数据
  （类型、默认值、分组、描述、可选项）由 runtime 的 `settings.rs` 提供，web 只是它的
  HTTP 外壳。`get_config` / `put_config` 改为调该模块的同源函数。
- 依赖方向已验证不会成环：把 `yi-agent-runtime` 从 dev-dependency 提升为正式依赖即可
  （runtime 只依赖 core/llm/tools/mcp/skills/store/supervisors/flags，不依赖 web）。
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
避免四份重复的 `jq` 表达式。

## 错误处理

| 情形 | 行为 |
|---|---|
| `settings.json` 缺失 | 全部字段 `None`，用默认值；**不**自动创建文件 |
| `settings.json` 存在但 `JSON` 非法 | 记 `tracing::warn`，回退全 `None`；**绝不**因此阻断启动（与 `models.json`、`preferences.json` 同一约定） |
| 写入时 base 读不出来 | 返回错误、保持文件原样（沿用 `save_catalog_to` 的守卫，防静默覆盖） |
| 枚举值非法（如 `sandbox: "bogus"`） | 加载时回退默认值并 warn；`settings/write` 侧直接拒绝 |
| 迁移时 `settings.json` 不可写 | 不注释/不改名 `.env`，返回 `NotWritable` 并 warn |
| `YI_AGENT_HOME` 缺失 | `home_join` 返回 `None`，退到 `./.yi-agent/settings.json`（与 `models_path` 同口径） |

## 测试

每个行为都要有一条会因回退而变红的测试：

- `settings.rs` 纯函数：缺文件 → 全 `None`；坏 JSON → 全 `None` 且不 panic；未知顶层键被保留
  （不丢用户手写内容）；`Option` 往返；`0600` 权限。
- **未知键保留**：写入 `agent.max_turns` 后，文件里用户自己加的 `"my_note": "x"` 仍在。
  这是本 spec 修的第一号缺陷的回归网。
- **不再为未设置字段写空值**：`settings/write` 只改目标字段，其余键不出现。
- 迁移：`.env` 有值 → 落进 `settings.json`；`settings.json` 已有值 → **不被覆盖**；
  `.env` 改名成功；`settings.json` 不可写 → `.env` 原样。
- `.env` 读取口径：`KEY=v #c` → `v`；`export KEY=v` → `v`；`KEY="v"` → `v`。
- `YI_AGENT_WORKDIR` 不再影响 `resolve_workdir`（设为任意值后断言仍用当前目录）。
- `domain_for` 表补 `settings/read|write|test`。
- web：`put_config` 不再产生空值行；未知键存活；`real-llm` 路由返回 404。
- 桌面端：`SettingsRuntimeTab` 渲染三组、恢复默认发 `null`、未设置显示默认值。

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
- `docs/bug-list.md`：记录本次修复的一号缺陷（web 保存删键）。

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
二进制（构建期嵌入，或在构建脚本里先 `npm run build` 再 `cargo build`）。

阶段一交付 `settings/*` RPC 面正是这三件事的前置条件 —— 所以本次改动不会被阶段二的待定决策
绑死。
