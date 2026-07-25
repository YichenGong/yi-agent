# 配置文件层级合并设计

## 背景

当前 `yi-agent` 的 `.env` 加载逻辑只支持单一来源(`--workdir` / `YI_AGENT_WORKDIR` / 当前目录),无法满足"全局通用配置 + 项目本地配置"的叠加需求。同时直接读取 `$(pwd)/.env` 容易与项目自身的 `.env`(如前端工具链)冲突。

## 设计目标

- 全局配置(`~/.yi-agent/.env`)放通用配置(如 API key),全局可用
- 本地配置(`$(pwd)/.yi-agent/.env`)放项目特定配置,覆盖全局
- 显式指定(`--workdir` / `YI_AGENT_WORKDIR`)时完全控制,不被全局污染
- 统一目录结构 `.yi-agent/.env`,避免与项目 `.env` 冲突

## 配置文件位置

| 模式 | 路径 | 目录创建 |
|---|---|---|
| `--workdir <dir>` | `<dir>/.yi-agent/.env` | 不存在则报错 |
| `YI_AGENT_WORKDIR=<dir>` | `<dir>/.yi-agent/.env` | 不存在则报错 |
| 默认 fallback | `$(pwd)/.yi-agent/.env` + `~/.yi-agent/.env` | 自动创建空目录 |

## 加载顺序

优先级从高到低,先加载的不会被后加载覆盖:

1. **真实环境变量**(进程已有的,谁都不覆盖)
2. **`--workdir` / `YI_AGENT_WORKDIR` 指定的 `.env`**(跳过全局)
3. **默认 fallback 模式**:
   - 先加载 `$(pwd)/.yi-agent/.env`
   - 再加载 `~/.yi-agent/.env`(只补充,不覆盖)

因为 `dotenvy` 默认不覆盖已存在的环境变量,所以先加载 local 再加载 global 天然实现"local 覆盖 global,global 只兜底"的语义。

## 目录与文件处理策略

| 场景 | 行为 |
|---|---|
| `--workdir` 指定,目录不存在 | 报错(用户显式指定,应知道自己在做什么) |
| `--workdir` 指定,`.env` 不存在 | 静默跳过 |
| fallback 模式,`$(pwd)/.yi-agent/` 不存在 | 自动创建空目录 |
| fallback 模式,`~/.yi-agent/` 不存在 | 自动创建空目录 |
| 任何 `.env` 文件不存在 | 静默跳过 |

## 代码改动范围

### `crates/yi-agent/src/config.rs`

- **`resolve_env_path`**:改为返回 `<dir>/.yi-agent/.env` 而不是 `<dir>/.env`
- **`load`**:实现"显式指定跳过全局,fallback 合并全局"的逻辑
- **新增**:目录不存在时自动创建(`mkdir -p` 语义)
- **新增**:判断是否为显式指定(用于决定是否加载全局兜底)

### `crates/yi-agent-web/src/lib.rs`

- **移除** `find_env_example` 函数

### `crates/yi-agent-web/src/api.rs`

- **移除** `AppState` 中的 `env_example_path` 字段
- **移除** `GET /api/config` 返回中的 `envExamplePath` 和 `envExampleContent` 字段

### 移除 `.env.example` 相关逻辑

理由:`config_meta::ALL_VARS` 已是单一事实来源,`.env.example` 是冗余信息。`GET /api/config` 已经用 `ALL_VARS` 组织返回结构(分组、类型、默认值、描述),前端无需 `.env.example` 内容。

## Breaking Changes

- `--workdir ~/config` 现在读 `~/config/.yi-agent/.env` 而不是 `~/config/.env`
- 默认模式读 `$(pwd)/.yi-agent/.env` 而不是 `$(pwd)/.env`
- `.env.example` 不再被查找或使用
- web API `GET /api/config` 不再返回 `envExamplePath` / `envExampleContent` 字段

---

## 第二阶段:Web 配置 UI 的 scope 切换

### 背景

第一阶段实现了 CLI agent 模式的 local + global 合并,但 `yi-agent web` 仍只操作单个 `.env` 文件。用户希望通过 Web 轻松编辑全局配置(如 API key),配一次全局可用,不用每个目录重新配。

### 设计目标

- Web 显示合并视图(local + global),用户能看到所有生效的值
- 用户能切换"编辑本地 / 编辑全局",按当前 scope 写入
- 启动时命令行输出服务信息,让用户知道服务地址和加载了哪些配置文件

### 后端改动

#### `crates/yi-agent-web/src/lib.rs`

**启动时输出**:

```
yi-agent web 配置服务器已启动
  地址:    http://127.0.0.1:7292
  本地配置: <local_path>
  全局配置: <global_path>  (仅 fallback 模式显示)
按 Ctrl+C 停止
```

`--workdir` 显式指定时跳过全局行(与 CLI agent 一致)。

**解析两个路径**:
- `local_env_path`:由 `resolve_env_path` 解析
- `global_env_path`:仅 fallback 模式下解析 `~/.yi-agent/.env`,显式 `--workdir` 时为 `None`

`AppState` 增加 `global_env_path: Option<PathBuf>`。

#### `crates/yi-agent-web/src/api.rs`

**GET /api/config** 返回合并视图:
- 先读 global,再用 local 覆盖
- 每个变量增加 `source` 字段:`"local"` / `"global"` / `"env"`(真实环境变量) / `"default"`(都没有,用默认值)
- 返回 `localPath` 和 `globalPath`(可选)供前端显示

**PUT /api/config** 增加 `scope` 参数:
- `scope: "local"`(默认)→ 写到 `local_env_path`
- `scope: "global"` → 写到 `global_env_path`
- 如果 `scope: "global"` 但 `global_env_path` 为 `None`(显式 `--workdir` 模式)→ 返回 400 错误

### 前端改动

#### `crates/yi-agent-web/src/assets/index.html`

- 顶部加 scope 切换开关:**[本地] [全局]**,默认"本地"
- 变量列表始终显示合并视图
- 每个变量旁边小标签显示来源(`本地`/`全局`/`环境变量`/`默认`)
- 保存按钮按当前选中 scope 调用 PUT

### 行为矩阵

| 启动方式 | local_path | global_path | GET 合并 | scope 切换 |
|---|---|---|---|---|
| `yi-agent web` | `$(pwd)/.yi-agent/.env` | `~/.yi-agent/.env` | local + global | 本地/全局 |
| `yi-agent web --workdir <dir>` | `<dir>/.yi-agent/.env` | `None` | 只 local | 只有本地 |
