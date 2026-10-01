# Superpowers 看板：让内嵌 daemon 也监督插件

日期：2026-10-02
状态：待实施

## 1. 背景

看板插件的生命周期由**宿主侧的监督循环**决定：它扫 `<workdir>/.yi-agent/supervisors/`
下的清单，按开关拉起/停止子进程，并把"声明了 query_socket 且正在运行"的插件登记
进 daemon 的转发表（`register_plugin_sockets`）。

现状：这个监督循环**只长在 `yi-agent daemon serve` 上**（`main.rs` 的
`DaemonAction::Serve` 分支调用 `serve_supervisor`）。但 daemon 有三条启动路径：

| 路径 | 谁在用 | 监督循环 |
|---|---|---|
| `yi-agent daemon serve` | 手工/后台起的常驻 daemon | **有** |
| `attach_tui_runtime`（TUI 内嵌 daemon） | 直接在终端跑 `yi-agent` | **无** |
| `attach_project_runtime`（桌面 app-server 内嵌 daemon） | 桌面 app | **无** |

后两条是**内嵌**形态：谁需要 runtime 谁就顺手起一个 daemon，进程随宿主生命周期。
它们不经过 `DaemonAction::Serve`，因此**没有任何人守护插件**——开关打开、清单就位，
插件进程也不会被拉起，`PluginQuery` 一律回 `plugin … is not available`。

实测症状（本仓库，2026-10-02）：`superpowers-kanban add` 投递成功，卡永远停在
`pending (waiting for the next tick)`；无 `superpowers-kanban run` 进程；
经 TUI 的 daemon 查询返回 `not_found`。

这与安装文档和用户预期都不一致：`INSTALL.md` 说 "装完不需要重启 daemon"，
用户实际是在 TUI / 桌面里用，而不是先手工起一个 `daemon serve`。

## 2. 范围

**做：**

- 把监督循环接进**内嵌 daemon** 路径，使 TUI 与桌面在"自己起了 daemon"时也监督插件。
- 监督循环的生命周期与**它所服务的那个 daemon** 一致：daemon 在，监督在；daemon 走，监督停。
- daemon 已被别的进程持有时（`AlreadyRunning`），**不**启动第二个监督循环，
  也**不**去停别人的插件进程。
- 转发表（`register_plugin_sockets`）在监督循环结束时清空，避免留下指向已死 socket 的路由。

**不做（明确非目标）：**

- 不改插件本身、不改清单格式、不改 `plugin/query` 协议。
- 不改 `daemon serve` 的既有行为（它已经是对的）。
- 不做"多进程共享一个监督者"的选举/仲裁——谁拥有 daemon，谁监督，是唯一规则。
- 不改看板的 CLI、skill、桌面 UI 文案。
- 不引入新的 RPC 或新的持久化状态。

## 3. 设计

### 3.1 一个函数，一个规则

新增宿主侧入口（落点见 §3.3）：

```
serve_supervisor(workdir) -> SupervisorHandle
```

语义不变（沿用现有实现）：后台线程，500ms 一轮对账，每轮末重发转发表；
停止时清空转发表并 `stop_all()`。

差异只在**调用点**：从"只被 `daemon serve` 调用"扩展为"被**每个拥有 daemon 的进程**调用"。

### 3.2 生命周期与所有权

规则一句话：**谁成功启动了 daemon，谁就负责监督；借用了别人的 daemon，就不监督。**

| 情形 | 该进程是否监督 | 理由 |
|---|---|---|
| `Daemon::start_with_factory` 返回 `Ok(daemon)`（我起的） | **是** | 这个 daemon 的生命周期归我 |
| 返回 `AlreadyRunning`（别人起的） | **否** | 监督者应是那个真正拥有 daemon 的进程；重复监督会重复拉进程/互相清表 |
| `daemon serve`（显式常驻） | **是** | 既有行为，不变 |

**已知并接受的行为（不修，写进文档）：** 若先手工起了 `daemon serve`，再开 TUI，
TUI 只是 attach 到那个 daemon，不会监督——这正确，因为 `daemon serve` 已经在监督了。
反之若 TUI 先起 daemon 并监督，之后 TUI 退出，它的 daemon 一并退出，插件进程随之停止
（监督循环 `stop_all`）——这是内嵌 daemon 的固有语义，符合"装了就可用、卸了就消失"。

### 3.3 落点：`yi-agent-runtime`

监督循环的**机制**（`yi-agent-supervisors`）与**转发表**（`yi-agent-store::ipc`）都已存在，
不做改动。新增的是"把二者接起来"的胶水，落点要求：

- 能被 `yi-agent`（TUI 与 `daemon serve`）和 `yi-agent-subagent`（桌面路径）同时使用；
- 不制造依赖环。

选定：**`yi-agent-runtime`**。它已在 `yi-agent` 与 `yi-agent-subagent` 的共同上游，
且目前只依赖 core/llm/tools/mcp/skills——加上 supervisors + store 不成环。
把胶水放这里，两个触碰点各留一行调用，`yi-agent` 里那份重复实现删除。

产出物：

- `yi-agent-runtime` 新增 `supervise` 模块，导出
  `SuperviseHandle { fn is_supervising(&self) -> bool; fn stop(self) }` 与
  `serve(workdir: &Path) -> SuperviseHandle`；
- `catch_unwind` 保护：监督线程 panic 不得拖垮宿主（记录后结束，`stop()` 仍可 join）。

### 3.4 两端接线

- **TUI（`attach_tui_runtime`）**：`embedded_daemon` 为 `Some(_)` 时启动监督，
  句柄存入 `AttachedTuiRuntime`，随会话一起 `Drop`/停止。
- **桌面（`attach_project_runtime`）**：`embedded_daemon` 为 `Some(_)` 时启动监督，
  句柄存入 `AttachedProjectRuntime`，随 binding 一起停止。
- 两处都不监督 `AlreadyRunning` 的情形。

## 4. 验收

1. **单测（机制层）**：`serve(workdir)` 在清单 + 开关就位时拉起假子进程；
   停止后子进程被清理、转发表被清空。
2. **单测（规则层）**：`AlreadyRunning` 的路径不启动监督（`is_supervising() == false`）。
3. **集成（真实插件）**：在隔离项目目录里走"内嵌 daemon + 清单 + 开关"，
   断言 `superpowers-kanban run` 进程出现，且 `PluginQuery` 能取到 `switch.read`。
4. **回归**：主工作区套件全绿（数量不减少）、插件 115 测试、桌面 282 测试、
   `grep -rn yi_agent_board_ui` 仍为空。
5. **手工冒烟**：本仓库 TUI 里 `/superpowers-kanban` 能看到那张冒烟卡被消费（从
   `pending` 变成队列中的卡）。

## 5. 风险

| 风险 | 处置 |
|---|---|
| 两个进程同时监督同一 daemon，互相扯 | 规则收窄为"谁起的谁监督"（§3.2），并用测试锁住 |
| 监督线程 panic 拖垮 TUI/桌面 | `catch_unwind` + 记录，句柄仍可安全停 |
| 内嵌 daemon 退出后转发表残留陈旧路由 | 监督循环结束时 `clear_plugin_sockets()`（既有行为，测试锁住） |
| 修法与 Part 4 的转发表语义冲突 | 不改 `yi-agent-supervisors` 与 `ipc` 的任何公开语义，只加调用点 |
