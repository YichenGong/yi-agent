# MCP 设置面板（只读查看）设计

日期：2026-10-18
状态：已通过评审，待写实现计划

## 问题

桌面 app 的设置面板里看不到当前项目装了哪些 MCP server，也看不到各自提供哪些工具。
配置得手改 `<项目>/.yi-agent/mcp.json`，工具清单还得去翻 `<项目>/.yi-agent/mcp-cache.json`。

对应待办：`docs/project-management/desktop.md` 的 P3 条目
`[ ] MCP 集成 — 判据：GUI 可查看 / 启停 MCP server`。

## 范围

**做：** 只读查看。按项目列出 MCP server：名字、传输方式（stdio 的 `command`/`args`、http 的 `url`）、
文件里的启用默认值、缓存中的工具清单（可展开）与数量。没有配置时给出「如何添加」与真实路径。

**不做（明确留待后续「MCP 启停」设计）：**
- 不新增探测 RPC（不起子进程、不发网络请求）。
- 不写回 `mcp.json`（面板是只读的，开关只显示文件默认值）。
- 不接 `McpManager`：app-server 里 `build_runtime_tooling` 造出的 manager 句柄目前被丢弃
  （`yi-agent-rs/crates/yi-agent-app-server/src/server.rs:1000` 只取 `setup.tools`），
  会话内实时启停需要先把它留存进 `RuntimeTooling` 并接通 agent 重建，属独立一步。

## 已确认的决策

| # | 决策点 | 结论 |
|---|--------|------|
| 1 | 范围 | 只读展示（不做启停） |
| 2 | 展示粒度 | server 行 + 点击展开工具清单（名字 + 一行描述）；不展示参数 schema |
| 3 | 位置 | 新增独立 Tab「MCP」（桌面左侧栏、手机分类列表各多一项） |
| 4 | 数据来源 | 纯读盘：`mcp.json` + `mcp-cache.json`（按 SHA-256 指纹判定），不发网络、不起进程 |
| 5 | 权限档 | `Observe`（与 `skill/list`、`plugins/list` 同档；iOS 默认 Control 也能用） |
| 6 | 敏感值 | stdio 的 `env` 与 http 的 `headers` **只回键名**，值一律不进响应（连 `${VAR}` 引用也不显示） |
| 7 | 项目作用域 | 与插件/技能同语义：`params.project` 非空用它，否则回落宿主 workdir |

## 架构

```
desktop/src/components/SettingsMcpTab.tsx     新增：面板
desktop/src/lib/mcp.ts                        新增：RPC 接缝 + 防御式解析
desktop/src/components/SettingsDialog.tsx     改：TABS 加 "mcp"、三个 props、一支分派
desktop/src/App.tsx                           改：稳定身份的 mcpCall + projects/defaultProject

yi-agent-rs/crates/yi-agent-app-server/
  Cargo.toml                                  改：加 yi-agent-mcp 依赖
  src/mcp_rpc.rs                              新增：handle_mcp_request(_at)
  src/rpc/mod.rs                              改：Domain::Mcp + domain_for
  src/rpc/mcp.rs                              新增：Observe 门禁的处理臂
  src/server.rs                               改：dispatch 加 Domain::Mcp 分支
  src/rpc_tests.rs                            改：方法表加一行 + Observe 档用例
```

后端复用既有解析，不重写 JSON 逻辑：`McpConfig::load`、`McpCache::load` / `get_for`
（同源指纹判定，避免面板与真实注册路径分叉）。

项目作用域复用 `server.rs::plugin_scope`（已是 `pub(crate)`、就是这个语义），
补一句注释说明 MCP / 插件 / 技能三处共用同一口径。

## 线协议

`mcp/list`，参数 `{ "project"?: string }`。纯读盘，`Observe` 档。

```json
{
  "configured": true,
  "path": "/abs/path/project/.yi-agent/mcp.json",
  "enabled": true,
  "servers": [
    {
      "name": "horizon-websearch",
      "kind": "http",
      "enabled": true,
      "command": null,
      "args": [],
      "url": "https://llmapi.horizon.auto/mcp-servers/horizon-websearch/Mcp",
      "env_keys": [],
      "header_keys": [],
      "tools": [
        { "name": "SearchPro", "description": "联网搜索API…", "read_only": false }
      ],
      "tool_count": 1,
      "read_only_count": 0
    }
  ]
}
```

要点：

- `configured: false` 表示该项目没有 `mcp.json`——正常态，不是错误。此时 `servers: []`。
- `tools: null` 与 `tools: []` 刻意区分：前者 = 缓存缺失或指纹不匹配（首次调用该 server 时才探测），
  后者 = 探测过但该 server 不提供工具。面板对两者显示不同文案，**不假装清单是最新的**。
- `env_keys` / `header_keys` 只有键名；`input_schema` 不下发（缓存里那份 schema 可能有上千字）。
- `mcp.json` 存在但 JSON 非法 → 返回带 `path` 的错误，让用户知道去改哪个文件。
- 响应形状用手写 `json!`，前端防御式解析（与 `skill/list` / `plugins/list` 同类，不走 ts-rs DTO）。

## 面板

- 顶部 `SettingsProjectPicker`（`ariaLabel="MCP 设置项目"`），候选/默认值由 App 传入。
- 一行总开关状态：`MCP：已启用 / 已停用`（读文件顶层 `enabled`，纯展示）。
- 每个 server 一段：标题行 = 名字 + 状态胶囊（已启用/已停用）+ 传输摘要
  （stdio 显示 `command args…` 等宽单行截断；http 显示 `url`）；次要行列出 `env:` / `header:`
  的**键名**。
- 折叠范式照抄技能 Tab（`aria-expanded` + `▸/▾`）：点开列出工具名 + 一行描述，
  摘要为「N 个工具（M 个只读）」。
- 空态「未安装任何 MCP server」+「如何添加」：路径取服务端回的真实 `path`（不硬编码
  `~/.yi-agent`），附可复制的 stdio 与 http 样例。
- 加载中与读失败各有明确文案（失败用 `role="alert"`），不静默空白。

## 测试

前端（`desktop/src/components/SettingsMcpTab.test.tsx`）：

- 空态文案；渲染 server 行；`tools: null` 与 `tools: []` 提示**必须不同**。
- 请求带 `{ project }`，且切换项目会重新拉取（照技能 Tab 的回归用例）。
- 渲染结果不含任何 env/header 值（夹具带假 token，断言文本里查不到）。

后端（`mcp_rpc.rs` 的 `#[cfg(test)]`）：

- 无 `mcp.json` → `configured: false`；stdio / http 各解析正确（含 `env_keys` / `header_keys` 只有键名）。
- 缓存命中 → `tools` / `tool_count` / `read_only_count` 正确。
- **缓存指纹不匹配 → `tools: null`**（关键回归，钉死「不假装最新」）。
- 非法 JSON → 报错且带路径；响应文本不含 env/header 的值。

集成（`rpc_tests.rs`）：方法表加 `("mcp/list", Domain::Mcp)`，并加一条 `Observe` 档可调用的用例。

## 文档

- `docs/project-management/desktop.md`：原 P3 条目拆两条——新增
  `[x]` MCP 面板（只读查看，带代码位置与测试命令），原条目收窄为 `[ ] MCP 启停`
  （保留「判据：GUI 可启停 MCP server」）。本次只兑现「可查看」，不整条标 `[x]`。
- `docs/project-management/yi-agent-mcp.md`：补一行「设置页只读接缝 `mcp/list`」。
- `README.md`：若计数受影响则同步。

## 交付

按仓库规范：在 `.worktrees/feat-mcp-settings-panel`（分支 `feat/mcp-settings-panel`）上实现，
跑 `cd desktop && npx vitest run && npx tsc --noEmit` 与
`cd yi-agent-rs && cargo test -p yi-agent-app-server mcp && cargo fmt --all`，
全绿后 `git merge --no-ff` 回 `main`。
