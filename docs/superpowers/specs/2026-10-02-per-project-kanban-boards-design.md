# 项目级 Superpowers 看板

日期：2026-10-02
状态：核心部分已实施并通过端到端验收（Task 8 全局额度延后，见 §6/§7）

## 1. 背景

### 1.1 用户看到的问题

桌面端点看板的开关勾选框「没反应」。实测三层根因：

1. **失败被吞掉**：`App.tsx` 的 `onToggle` 用 `.catch(() => {})` 静默丢弃写入失败，
   勾选框不变、也不报错。
2. **查询路由用错了目录**：`plugin/query` 按 **app-server 的启动目录**（`cfg.workdir`）
   找 daemon 与插件。桌面 app-server 以 `$HOME` 启动，那里既无 daemon 也无插件清单，
   查询必然失败。
3. **本质**：看板是**全局**的（`ThreadSidebar` 的平级左列，对所有项目只有一个），
   而它需要的 daemon 与插件是**按项目**划分的。两者对不上，所以它在哪儿都问不到东西。

### 1.2 已有的能力（本次要复用的）

- `plugin/query` 泛型通道已存在，宿主不理解看板语义，只负责转发。
- app-server 已有**按 cwd 懒 attach 项目 runtime** 的机制
  （`ProjectRuntimes` + `attach_project_runtime`），且 `AttachedProjectRuntime`
  已持有 `SuperviseHandle`——桌面端已经会为「用过的项目」起 daemon 并监督插件。
- `daemon start` 已能起一个**脱离宿主存活**的 daemon 子进程
  （`spawn()` + stdio 置空，父进程退出后重新挂到 init/launchd）。
- 插件、清单、开关、socket 回退、协议版本对齐均已在
  `2026-10-02-kanban-plugin-socket-fallback-design.md` 中修好。

### 1.3 目标

把「全局一块看板」改成「**每个项目各有一块看板**」，并且看板能**脱离桌面 app 继续跑**。

## 2. 已定决策

| # | 决策 |
|---|---|
| 1 | **显式创建**：项目右键 →「创建 Superpowers 看板」；创建即该项目有常驻 daemon。没看板的项目不起任何进程。 |
| 2 | 看板**条目挂在项目下**，与线程并列，**像会话一样点开**（在主区域显示）。 |
| 3 | 并发额度**全局一份**（所有项目合计）。 |
| 4 | 关掉 desktop app 后，已创建的看板**继续跑**（不做开机自启；「怎么停」一并设计）。 |
| 5 | **移除看板** = 停 daemon + 停插件 + 删队列状态（UI 二次确认）。 |
| 6 | TUI 也支持，入口是**命令**。 |
| 7 | **删掉**左侧全局竖列面板，内容收进各项目看板。 |

## 3. 范围

**本 spec 做（核心）**：

- 项目级看板的**创建 / 移除 / 列表**
- 查询路由按**项目**走
- 侧栏看板条目 + 主区域看板视图（沿用现有三个组件的语义）
- 删掉全局左列面板
- 失败**可见**（不再静默）
- TUI 命令入口
- 创建起的 daemon **脱离 app 存活**

**本 spec 不做**：

- **全局并发额度**（决策 3 要求，但它是独立的跨进程协调问题，另开一轮；见 §6）
- 开机自启
- daemon 崩溃后的自动重启（见 §7 已知缺口）

## 4. 设计

### 4.1 全局登记表

新目录 **`~/.yi-agent/superpowers-kanban/`**（与既有全局配置约定 `~/.yi-agent/` 一致）。

- `boards.json`：登记哪些项目有看板。

```json
{ "boards": [ { "project": "/abs/path/to/project", "created_at": "..." } ] }
```

**为什么要全局登记表**：看板*队列*属于项目（留在 `<项目>/.yi-agent/superpowers-kanban/`），
但「**哪些项目有看板**」是全局问题——桌面端要按它决定侧栏给哪些项目画看板条目，
TUI 要按它判断当前目录有没有看板。跨项目的事放全局，项目自己的事留在项目里。

### 4.2 创建（`board/create`）

输入项目目录，依次：

1. 往 `<项目>/.yi-agent/supervisors/superpowers-kanban.json` 写监督清单（若已存在则跳过）。
2. 在 `<项目>/.yi-agent/preferences.json` 里把 `superpowers_kanban` 置 `true`
   （项目层覆盖全局层，见 `switch.rs` 的两层解析）。
3. 为该项目起一个**脱离式** daemon（复用 `daemon start` 的 `spawn()` 语义，cwd = 项目目录）。
   它持有项目 runtime 锁、监督插件、为线程提供委派。
4. 写 `boards.json` 登记。
5. 返回状态。

**幂等**：重复创建不报错，补齐缺失的部分即可。

**与既有懒 attach 的关系（实现时勿打架）**：app-server 已有「打开某项目的线程时
按 cwd 懒 attach 项目 runtime」的机制。创建看板起的 daemon **先**存在之后，
那条路径会走 `Daemon::start_with_factory` 的 `AlreadyRunning` 分支**借用**这个 daemon，
不会另起一个。两条路径都以「项目 runtime 锁」为准，因此不会出现两个 daemon 抢一个项目。
反过来说：**若某项目已有活跃 daemon（例如你先开过线程），创建看板应复用它**，
而不是强起第二个。

**脱离的边界（实现时须验证）**：现有 `daemon start` 是 `spawn()` 且不 `setsid`。
父进程（app-server / app）退出后子进程会被重新挂到 init/launchd，通常能存活；
但若 app 以进程组信号退出，子进程可能一并被杀。**实现时须实测「退出 app 后 daemon 是否仍在」**，
不满足则在 plan 里补 `setsid`（或等价的脱离手段）。

### 4.3 移除（`board/remove`）

1. 停该项目的 daemon（`daemon stop` 语义）。
2. 停插件（daemon 退出时监督循环会 `stop_all`）。
3. 删除 `<项目>/.yi-agent/superpowers-kanban/`（队列状态、board.json）。
4. 从 `boards.json` 取消登记。

**不可逆**，UI 二次确认。清单文件与 preferences 键**保留**（它们是「这个项目想跑看板」的配置，
不是队列数据；重建看板时正好复用）。

### 4.4 查询路由（修掉「没反应」）

`plugin/query` 增加 **`project`** 参数（项目目录），路由到**该项目的** daemon socket，
不再用 app-server 的启动目录。

- 该项目没有登记看板 → 返回结构化错误（`board_not_created`），UI 显示「启动看板」入口。
- 有登记但 daemon 不可达 → 返回 `daemon_unavailable`，同样可见。

现有 `plugin_query(workdir, ...)` 的 workdir 参数保留，但调用点必须传**项目目录**而非 `cfg.workdir`。

### 4.5 UI

**侧栏**（`ThreadSidebar`）：

- 项目行右键菜单增加「创建 Superpowers 看板」/「移除看板」（按登记状态二选一）。
- 有看板的项目，行下多一个**看板条目**，与线程并列；带摘要徽标（如「3 排队 · 1 运行中」）。
- **看板条目不是线程**：它是一个新的条目类型，只携带「项目路径」这一个标识，
  点击后主区域渲染该项目的看板。它**不**进入线程列表、**不**产生会话、
  **不**参与 `thread/listAll` 的分组；重心是「选中哪个项目看板」这一份 UI 状态，
  与「当前选中哪个线程」彼此独立（可以同时有）。
- 点击条目 → 选中该项目的看板，主区域渲染。

**主区域**：沿用现有三个组件的语义，重排到主区域：

- `SuperpowersKanbanSettings`（开关 + 来源）
- `SuperpowersKanbanEnqueue`（spec/plan 入队）
- `SuperpowersKanbanView`（卡片列表）

**删除**：左侧全局竖列面板（`kanbanCollapsed` / `SuperpowersKanbanCollapsedStrip` 的现有用法）
整体移除。

**失败可见**：`onToggle` 等写操作失败必须显示错误（内联提示或 banner），
不得再用 `.catch(() => {})` 静默。

### 4.6 TUI

命令入口（如 `/kanban on` / `/kanban off` / `/kanban status`），作用于**当前会话目录**，
走与桌面端**同一套**机制（清单 + 开关 + 脱离式 daemon + 登记表）。
命令只做创建/移除/查看，不做第二套语义。

### 4.7 并发额度（本轮占位）

额度**全局一份**（决策 3）。本轮**不实现**，但登记表与租约将共用
`~/.yi-agent/superpowers-kanban/`，为下一轮留位。当前各 daemon 各自按日历读并发上限
（限流时段 3 / 不限流 10），因此**同时跑多个看板会各自为政**——这是本轮已知缺口，见 §7。

## 5. 验收

1. **创建**：右键创建后，`boards.json` 有登记、清单与开关就位、daemon 脱离存活、插件在跑。
2. **查询路由**：`plugin/query` 带 `project` 能拿到该项目的 `switch.read` / `list`；
   `project` 指向无看板项目时返回 `board_not_created`。
3. **失败可见**：插件/daemon 不可用时，开关操作给出可见错误（有对应前端测试）。
4. **移除**：停 daemon + 停插件 + 队列状态被删 + 取消登记；UI 二次确认。
5. **脱离**：创建后关闭 desktop app，daemon 与插件仍在跑（实测）。
6. **UI**：看板条目挂在项目下、可点开；全局左列面板已删除。
7. **TUI**：`/kanban on` 在当前目录建看板，与桌面端同源。
8. **回归**：宿主、插件、桌面全绿；既有单项目看板能力不回退。

## 6. 非目标（下一轮）

- 全局并发额度（跨进程租约；决策 3 的落地）
- 崩溃自愈 / 开机自启
- 「一个总管进程统管所有看板」（本轮用「每项目一个脱离式 daemon」）

## 7. 已知缺口（本轮明确接受）

1. **额度不跨项目协调**：多开看板会各自按日历上限跑，合计可能超服务端限流。
   这是决策 3 尚未落地的直接后果，下一轮解决。
2. **daemon 崩溃无人重启**：app 未打开时，某项目 daemon 半夜崩溃就是停了。
   用户需重开 app 或重跑创建动作。下一轮（总管进程或 launchd）解决。
3. **进程数量**：每个有看板的项目 = 1 daemon + 1 插件。项目多时进程数线性增长。

## 8. 风险

| 风险 | 处置 |
|---|---|
| daemon 未真正脱离，退出 app 后被一并杀掉 | 实测；不满足则补 setsid（§4.2） |
| 移除看板删数据不可逆 | UI 二次确认；清单/开关保留以便重建 |
| 登记表与实际状态不一致（手删了目录、daemon 已死） | 以「登记表 + 实时探测」为准，探测失败即视为未就绪并可见 |
| 删全局左列影响既有用户习惯 | 折叠条与开关一并迁入项目看板，能力不减 |

## 8.1 开关关闭时插件必须继续运行（2026-10-02 修复）

原实现里 `Supervisor::reconcile` 在开关关闭时**停止**插件进程。这与插件的设计
相矛盾：`run_daemon` 只在开关关闭时**空转**（「关掉开关只停止推进，绝不取消已在
daemon 中运行的会话」），并另起线程提供查询通道。而开关的读写（`switch.read` /
`switch.write`）本身就是经这条通道由插件回答的——插件一停，通道就没了，开关既
读不到也写不了，于是「关」成了单向门：桌面端点一下关闭，再点打开只会得到
「插件未安装」。

修法：清单新增 `stop_when_disabled`（缺省 `true`，旧清单语义不变）。声明了查询
通道、因而必须随时可被查询的进程置 `false`，监督器在开关关闭时保留它（`off`
仍表示「停止推进」，只是不再等于「消失」）。看板清单置 `false`。依据：

- 插件 `run_daemon` 已自行按开关门控 —— 保留进程正是它设计中的样子；
- 全系统 `supervisors/*.json` 里**只有** `superpowers-kanban` 这一个被托管进程，
  改动面只有它一个。

端到端实测：关闭 → `switch.read` 回 `{on:false,source:"project"}`；再打开 →
`switch.write` 成功、`switch.read` 回 `{on:true}`。

## 9. 端到端验收记录（2026-10-02）

以**真实二进制**跑通闭环（`yi-agent app-server` + 脱离式 `daemon serve` + 真插件
`/opt/homebrew/bin/superpowers-kanban`），配置隔离在临时 `HOME`，脚本化复现于
`yi-agent-rs/crates/yi-agent/tests/board_e2e.rs`（`YI_AGENT_BOARD_E2E=1` 开启；
缺插件时报告原因并跳过，不让外部工具缺失污染测试套件）。

四条验收（均实测通过，非推测）：

0. **说明**：Step 3 的四条以 `board/create`、`board/remove`、`plugin/query` 三条 RPC
   直接驱动——这正是桌面右键与侧栏调用它们的同一条路径；**未**做 UI 层合成点击
   （无无头桌面 harness），故不声称「点过那个菜单」。UI 自身由桌面单测覆盖。

1. **创建**：`board/create` → 登记表出现该项目、`supervisors/superpowers-kanban.json`
   与 `preferences.json` 的 `superpowers_kanban:true` 就位、daemon 就绪
   （`daemon_running:true`，实测约 0.3s）。
2. **查询**：`plugin/query` 带 `project` → `switch.read` 回 `{on:true,source:"project"}`、
   `list` 回 `{cards:[]}`；不带 `project` 回 `-32602`（数字码，无 `data.code`）。
3. **脱离存活**：app-server 以 rc 0 退出后，`daemon serve` 与插件进程仍在、
   `runtime.sock` 仍在应答。
4. **移除**：`board/remove` → daemon/插件停止、队列目录 `.yi-agent/superpowers-kanban`
   删除、清单保留、登记表清空。

**与计划的一处偏差（如实记录）**：计划 Task 10 Step 2/4 假定「替换真实二进制 +
Task 8 已落地」。实际：

- **未**覆盖 `~/.cargo/bin/yi-agent` 与 `/opt/homebrew/bin/superpowers-kanban`
  （不动用户已安装的二进制）；改为对 worktree 构建出的二进制做非侵入式验收。
- **Task 8（全局并发额度）本轮未做**（人类裁决），故 Step 4 原文「改为已由 Task 8
  落地」不成立；§6/§7 保持「延后」。
