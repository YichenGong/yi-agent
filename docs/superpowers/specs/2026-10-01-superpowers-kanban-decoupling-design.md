# Spec 4：插件化状态通道（通用 plugin/query IPC）

日期：2026-10-01
状态：设计待评审

## 1. 目标

让主程序（app-server / TUI）**完全不认识看板**：不知道 `.yi-agent/superpowers-kanban/`、不知道 `board.json`、不知道 `inbox/`、不知道"卡片"。主程序只认识一条通用能力——"向某个插件问一个方法"。

落地后删除主程序里的 `yi-agent-board-ui` crate 与全部 `board`/`kanban` 语义。

## 2. 现状（为什么需要这一步）

- 插件对主程序：零依赖（已达标）。
- 主程序对插件：app-server 直接依赖 `yi-agent-board-ui` 去读 `board.json`、写 `inbox/`、读写 `preferences.json`——靠**硬编码磁盘格式**耦合。
- daemon 对插件的 IPC（`board-ipc`）只有三条命令且**单向**（`CreateAutonomousSession` / `ListTaskSummaries` / `Status`），没有任何"宿主查询插件"的通道。

## 3. 架构

```
桌面 / TUI ──RPC──▶ app-server ──IPC(通用 plugin/query)──▶ daemon ──转发──▶ 插件
                                                                          │
                                                              插件独占全部状态
                                                    (board.json / inbox / 开关 / 日历)
```

- 主程序只做**转发**，不解释 `method` 与 `params`。
- 承载看板语义的方法名（`list` / `enqueue` / `switch.read` / `switch.write`）是**桌面端与插件之间**的约定，写在插件文档与桌面端代码里，主程序不感知。

## 4. 新增能力

1. **daemon 侧**：新增 `IpcRequest::PluginQuery { plugin: String, method: String, params: Value }` → `IpcResponse::PluginResult { value: Value }`。主程序侧零看板语义。
2. **插件侧**：从"只能发"变为"也能收"——插件监听自己的 socket（`<state_dir>/<plugin>.sock`，即 `<state_dir>/superpowers-kanban.sock`），实现 `list|enqueue|switch.read|switch.write` 四个方法。
3. **清单声明**：`SupervisorManifest` 新增可选字段 `query_socket`（或 `plugin_name`），daemon 据此建立"插件名 → socket"映射。未声明则视为不可查询。字段保持通用，不含插件语义。
4. **插件未安装 / 未声明 / 未在跑**：`PluginQuery` 返回结构化错误（`IpcErrorCode`），桌面端据此显示"插件未安装"，而非崩溃或静默空白。

## 5. 迁移

- app-server 的 4 条 `board/*` RPC 被 `plugin/query` 取代；桌面端改调新通道（参数里带 `plugin: "superpowers-kanban"`）。
- TUI `/superpowers-kanban` 改为经 daemon → 插件取状态，不再直接读 `board.json`。
- **删除** `yi-agent-rs/crates/yi-agent-board-ui`（其 29 个测试随之迁入插件侧或改写）。
- 开关读写同样经插件（插件拥有 `preferences.json` 的两层解析语义）。

## 6. 代价与风险（如实记录）

- 动的是**核心 IPC**（TUI / 桌面 / daemon 共用），是本轮风险最高的一项。
- `PluginQuery` 一旦成型，即成为**通用扩展面**：需明确它的安全边界（任意插件可达哪些方法、是否需要能力声明）。
- 桌面端"能看看板"从此**依赖插件已安装**——这是用户明确接受的结果（"安装后才有"）。

## 7. 测试重点

1. `plugin/query` 能转发到插件并原样返回结果（daemon 不解释内容）。
2. 插件未安装时返回结构化错误，桌面端文案正确。
3. 旧路径（直接读 `board.json`）在主程序侧**已不存在**：以"编译期不存在该 crate"为验收（删除 `yi-agent-board-ui` 依赖即证明）。
4. TUI 与桌面读到的卡片与插件自持状态一致。
