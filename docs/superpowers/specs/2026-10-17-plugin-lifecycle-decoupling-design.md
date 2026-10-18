# 插件生命周期去耦合（宿主不认识「看板」这个概念）Design

日期：2026-10-17
状态：设计已确认（对话中拍板：与 G1 一并全量还债），待用户 review 本文件后进入 writing-plans。
范围：宿主（`yi-agent-rs/crates/yi-agent-boards/`、`yi-agent-app-server/`、`yi-agent/`）、
桌面（`desktop/`）。
关联：Spec 4（`2026-10-01-superpowers-kanban-decoupling-design.md`，本 spec 是其迟到的收尾）、
`2026-10-01-superpowers-kanban-overview-design.md`（决策 8「主程序彻底不认识看板」）、
`2026-10-02-per-project-kanban-boards-design.md`、`2026-10-02-kanban-supervisor-for-embedded-daemons-design.md`。
姊妹篇：G1 `2026-10-17-session-driver-decoupling-design.md`（会话驱动面）。

> **2026-10-18 刷新（G1 落地后）**：G1 已合入 main（`62e655db`，见
> `docs/project-management/yi-agent-app-server.md`）。本 spec 据此校正三处：
> ① §1.1 的体量表不再是 G1 前的样子，`card_scheduler`/`board_query` 已删；
> ② 宿主 `server.rs` 已按域拆成 `src/rpc/*.rs`（另有「主循环响应性」改动），
>    `board/*` 处理器现在在 `src/rpc/board.rs`，`AppState` 里的 board 相关字段
>    已搬进 `BoardPluginState`（`src/rpc/mod.rs`）；
> ③ **代价面扩大**：`yi-agent-boards` 不只被 app-server 依赖，`yi-agent`（TUI/CLI）
>    也依赖它，故「删 crate」必须连同 TUI/CLI 一起收口（见 §5.6）。
>
> **刷新后仍有一处需用户裁定**（§4.4 的建盘引导）：原 §4.2 把「写清单」交给插件，
> 但插件只有在清单就位后才会被拉起——这是个鸡生蛋。§4.4 给出裁定提案，待确认后
> 才动 Task 4/5。

## 1. 背景与问题

### 1.1 Spec 4 只收了一半

Spec 4 的验收第 3 条写得很清楚：

> 旧路径（直接读 `board.json`）在主程序侧**已不存在**：以「编译期不存在该 crate」为验收
> （删除 `yi-agent-board-ui` 依赖即证明）。

`yi-agent-board-ui` 确实删了，但**宿主里又长出了一个新的看板专属 crate 与一组专属 RPC**：

| 位置 | 体量（2026-10-18 实测） | 内容 |
|---|---|---|
| `yi-agent-boards` crate | `registry` 200 行、`lifecycle` 741 行、`scaffold` 237 行、`watch` 87 行、`watchman` 199 行、`board_daemon` 213 行、`lib.rs` 68 行 | `registry`（全局看板登记表）、`lifecycle`（建/删/查）、`scaffold`（清单 + 开关）、`watch`、`watchman`（launchd）、`board_daemon`（拉起 `yi-agent daemon serve`） |
| `app-server` 的 `board/*` RPC | 3 个方法（`src/rpc/board.rs`） | `board/create`、`board/remove`、`board/list` |
| `app-server` 的 `plugin_query` 门控 | 1 处关键门控（`src/server.rs:1231`） | 按 `yi_agent_boards::registry::contains` 决定「这个项目能不能被问」 |
| `app-server` 的 `settings_store.rs` / `rpc/ui.rs` | 偏好键 `board_watchman_enabled` | 值守开关 |
| `app-server` 的 `ws.rs` | 3 处 | 起会话时注入 `board_dir` / 看板 launcher |
| **`yi-agent`（TUI/CLI）crate** | `tui/board.rs`、`tui/superpowers_kanban.rs`（读 registry）、`tui/app.rs`、`main.rs`（`Command::Boards`/`BoardsAction::Watch`）、`config.rs:189/196`、`tests/board_e2e.rs`、`tests/board_watchman_e2e.rs` | 直接用 `yi_agent_boards::{lifecycle,registry,scaffold,board_daemon,watch,global_dir}` |
| 桌面 | `superpowersKanbanBoards.ts`、`routes.ts`、`boardIndex.ts`、`hooks/useBoardState.ts`、`App.tsx`、各自的测试 | 调 `board/create|remove|list` |

桌面侧代码的注释自认了分层：

> 与 `superpowersKanbanSwitch.ts` 的分工：那边是「经插件问这个项目的队列」，这边是
> 「**问宿主**这个项目有没有看板」。生命周期属于宿主（登记表 + 每个项目一个 daemon）。

——「生命周期属于宿主」正是 Spec 4 要推翻的判断：**宿主不该知道「看板」这个词**。

### 1.2 Spec 4 早已给出目标形态

Spec 4 §5 迁移第 1 条：

> app-server 的 4 条 `board/*` RPC 被 `plugin/query` 取代；桌面端改调新通道（参数里带
> `plugin: "superpowers-kanban"`）。

这条**从未落地**。桌面端至今仍调 `board/create` / `board/remove` / `board/list`。
同时 Spec 4 §4 第 3 条已设计好通用发现口：

> **清单声明**：`SupervisorManifest` 新增可选字段 `query_socket`（或 `plugin_name`），
> daemon 据此建立「插件名 → socket」映射。字段保持通用，不含插件语义。

`query_socket` 与 `session_driver` 均已落地（`manifest.rs` 注释亦自认「Generic: the field
names no plugin and the daemon attaches no meaning to the answers」），但**谁该做「每项目一个
daemon / 拉起插件」这件事**没有随之通用化。

### 1.3 已有的通用替代物

- `yi_agent_store::resident`（`resident.rs` 头部注释）：
  「通用『常驻 daemon』登记：哪些项目需要常驻 daemon、由谁请求。**与任何具体插件/看板无关**：
  字段只有 `project` 与 `required_by`。」——**这已经是**看板登记表的通用替代。
- `yi_agent_supervisors::manifest`：通用插件清单（`name`/`command`/`args`/`switch_key`/
  `stop_when_disabled`/`query_socket`/`session_driver`），看板只是其中一个插件。
- `yi_agent_store::resident::list()` —— 通用「哪些项目要常驻 daemon」。
- `watch.rs::once()`（宿主内循环）读的就是**通用登记**（`resident::list`），注释亦自认
  「读通用登记」。
- `yi_agent_supervisors::supervisor::Supervisor` —— 通用「按清单 + 开关键托管子进程」，
  由 daemon 的 `yi-agent-runtime::supervise::serve(workdir)` 驱动。

即：通用机制**都在**，只是外面还套了一层看板专用的 `registry.rs`（`boards.json`）
与 `board/*` RPC。本 spec 把这层套壳去掉。

## 2. 目标与非目标

**目标**

1. 宿主不再有看板专属 crate、登记表与 RPC。
2. 桌面端的「建/删/查看板」改走**通用插件通道**（Spec 4 §5 的原始计划）。
3. 「每项目一个 daemon」与「插件被监督」改为**通用**机制，不再叫 board。
4. 判据：宿主 + CLI 侧 grep `kanban` / `board` / `卡片` 在非测试代码中归零
   （通用插件机制除外）。
5. 端到端等价：**用户可见行为不变**——桌面上仍能建看板、看板仍会被拉起与监督。

**非目标**

- 不动 G1 的会话驱动面（`driver/next`、队列、合并）。
- 不改插件自身的状态模型（`board.json` 仍是插件私有；Spec「单一真相源」已另立条目）。
- 不改 `SupervisorManifest` 的既有字段语义（只可能新增通用可选字段）。
- 不重做桌面 UI 布局；只换调用通道。
- 不做「多进程共享监督者」的选举（沿用 10-02 决策：谁拥有 daemon，谁监督）。
- 不引入宿主级全局插件目录（沿用 10-04 决策：插件按项目安装于
  `<项目>/.yi-agent/supervisors/`）。

## 3. 设计决策汇总

| # | 决策 | 取值 |
|---|---|---|
| D1 | 看板登记表 | 删除 `boards.json` / `registry.rs`；「哪些项目要 daemon」以 `resident::list` 为唯一来源 |
| D2 | 建/删看板 | 改走**插件通道**（`plugin_query`），由插件实现 `lifecycle/create` / `lifecycle/remove` |
| D3 | 列已登记项目 | 改为通用 `resident/list`（宿主侧一个通用 RPC） |
| D4 | daemon 拉起 | 沿用 `watch.rs`（已读通用登记）；`board_daemon.rs` 的「board」命名与路径假设去专有名 |
| D5 | 监督 | 沿用 `yi-agent-supervisors`；`scaffold.rs` 的看板专属模板改为通用清单写出 |
| D6 | 开关键 | `board_watchman_enabled` → 通用键（保留旧键的**兼容读取**，见 D8） |
| D7 | 桌面 | `superpowersKanbanBoards.ts` 改调 `plugin/query`（`plugin: "superpowers-kanban"`） |
| D8 | 迁移 | 旧 `boards.json` 首次运行时一次性导入 `resident` 登记，随后只读新源 |
| D9 | crate 归宿 | `yi-agent-boards` 中通用部分（watch / launchd / daemon 生命周期）迁入 `yi-agent-supervisors`；看板专属部分删除 |
| D10 | 建盘引导 | 见 §4.4（**待用户裁定**） |
| D11 | 错误码 | 宿主 `board_query` 一族码名去看板化（`resident_not_registered` 取代 `board_not_created`），桌面同步 |

## 4. 架构与数据流

### 4.1 目标分层

```
桌面 ──RPC──▶ app-server ──(通用转发)──▶ daemon ──▶ 插件
   │              │                                    │
   │              └─ 通用：resident 登记 / 起 daemon / 监督 ┘
   │                  （不含「看板」任何字样）
   └─ 建看板 = plugin_query(superpowers-kanban, "lifecycle/create", {project})
```

### 4.2 建 / 删看板的新路径

| 旧 | 新 |
|---|---|
| 桌面 `board/create {project}` → 宿主 `lifecycle::create_with_project` | 桌面 `plugin/query {plugin:"superpowers-kanban", method:"lifecycle/create", params:{project}}` → 插件：建自己的状态目录 + 写自己的清单 + `resident::require(project, "superpowers-kanban")` |
| 桌面 `board/remove {project}` | 同上，`lifecycle/remove` → 插件：停 daemon、删状态目录、`resident::release` |
| 桌面 `board/list` → 宿主读 `boards.json` | 桌面调**通用** `resident/list`（宿主通用 RPC，回已登记项目），不再有看板专属列举 |

关键点：**建/删看板 = 插件的事**（它要建自己的状态目录、写自己的清单）；
宿主只提供通用的「登记一个项目需要常驻 daemon」与「转发一条插件方法」。

### 4.3 一次性迁移

首次运行（宿主或插件任一侧先到）时：

1. 若 `boards.json` 存在且非空：对每个 `project` 调 `resident::require(project, "superpowers-kanban")`。
2. 迁移完成后把 `boards.json` 重命名为 `boards.json.migrated`（保留取证，不被再次读）。
3. 幂等：`resident::require` 本身幂等（重复登记只并集 `required_by`）。

旧偏好键 `board_watchman_enabled`：**读时兼容**（读到旧键仍生效），写时写新通用键。

> **落点（刷新补）**：迁移入口必须**有真实调用点**，否则验收 §6.6 永远不成立。
> 裁定放在**宿主通用侧**（`yi-agent-store` 提供纯函数 + app-server 启动时调用一次），
> 理由：① `store` 不认识看板格式，只接受「已解析出的项目列表」，保持通用；
> ② 插件侧不认识 `boards.json`（那是宿主的旧格式），且插件不依赖 `yi-agent-*`。

### 4.4 建盘引导：鸡生蛋（**待用户裁定**）

原 §4.2 让插件「写自己的清单」。但插件要被调用，前提是：清单已就位
（`<项目>/.yi-agent/supervisors/`）→ daemon 已拾取并拉起插件 → 插件才在自己的
socket 上应答。而今天**唯一**安装清单的代码是 `yi-agent-boards::scaffold::
install_manifest`，其唯一调用者是 `lifecycle::create_with_project`，即
`board/create`——正是本 spec 要删掉的那条路。于是：

- 若 create 直接改走 `plugin/query`，**全新项目**的 create 必然失败
  （`plugin_query` 的门控会回 `board_not_created`，即便摘掉门控，也拨不到
  尚未被拉起的插件 socket）；
- 验收 §6.9「桌面『新建看板』仍能建」因此不成立。

**裁定提案（待确认）**：

| 方案 | 内容 | 代价 |
|---|---|---|
| **A（提案）** | 把「把某个插件装进某个项目」提升为**宿主通用能力**：`yi-agent-supervisors` 新增通用 `install_manifest_for(project, plugin_name, manifest_json)`；宿主新增通用 RPC `plugin/install {project, plugin}`——按 `plugin` 名在 PATH 上解析可执行文件（通用，不再硬编码 `superpowers-kanban`），执行 `<plugin> manifest` 取该插件**自述**的通用清单，写入 `<项目>/.yi-agent/supervisors/<plugin>.json`、打开清单里的 `switch_key`、并确保项目 daemon 在跑。随后桌面再发 `lifecycle/create`。宿主全程不认识看板（清单内容来自插件自述）。 | 插件需新增 `manifest` CLI 动词；宿主新增 1 条通用 RPC；多一跳 |
| B | create 仍保留**宿主通用半程**（装清单 + 开开关 + 起 daemon），看板专属半程（状态目录 + resident）交插件。清单内容来自插件**随包分发**的 `supervisors/<name>.json`，宿主按 `plugin` 名解析路径。 | 宿主仍需知道「清单文件放哪找」的约定；插件随包文件与项目内清单可能漂移 |
| C | 只把桌面 create 改成「先 `plugin/install`（通用），再 `plugin/query lifecycle/create`」，其余不动；接受「插件未装则 create 明确失败并提示装插件」 | 行为有一处可见变化（未装插件时不再自动装） |

**倾向 A**：它在保持「宿主不认识看板」的同时，把「装插件」这件事本身也通用化，
且不引入全局插件目录（仍按项目安装），与 D5、§2 非目标一致。

> **未裁定前，Task 4/5 的 create 半程不开工**；Task 1/2/3/6/7 不受影响。

## 5. 改动明细

### 5.1 删 / 迁

- **删除** `yi-agent-boards/src/registry.rs`（200 行）与 `<global>/boards.json` 的读写。
- **删除** `yi-agent-boards/src/scaffold.rs` 的看板专属模板（237 行），保留/迁出「写出通用
  插件清单」的能力（产物就是 `SupervisorManifest` 的 JSON）。
- **迁移** `yi-agent-boards/src/{watch.rs, watchman.rs, board_daemon.rs}` 到通用归属：
  - `watch.rs` 已通用（读 `resident`）→ 迁 `yi-agent-supervisors`。
  - `watchman.rs`（launchd plist）→ 通用化命名（`resident-daemon-watchman`），不认看板；
    它跑的子命令也随之通用化（见 §5.6 的 `yi-agent` CLI 改动）。
  - `board_daemon.rs`（`spawn_detached` / `wait_ready` / `stop`）→ 通用「per-project runtime
    daemon 生命周期」，命名去掉 board（→ `daemon.rs`）。
- **删除** `yi-agent-boards/src/lifecycle.rs` 的建/删语义（741 行中的大部分），或整体删除，
  由插件接管（插件经 `plugin_query` 暴露 `lifecycle/*`）。

### 5.2 宿主

- `src/rpc/board.rs`：**整个域删除**（`board/create` / `board/remove` / `board/list` 三个
  分支）；`src/rpc/mod.rs` 去掉 `Domain::Board` 与 `domain_for` 的 `board` 臂，以及
  `BoardPluginState` 里的 `board_dir` / `board_launcher`（保留通用字段）。
- `src/server.rs`：`plugin_query` 的看板登记表门控（`:1231`）删除——通用转发不该按
  「有没有看板」拒答；`plugin_query` 的 `global` 形参随之删除；board 专属的
  `ensure_watchman_installed` 调用点迁到通用路径（或随 create 迁移）。
- **新增**通用 `resident/list` RPC（回 `{projects: [...]}`，可带 `required_by`），供桌面查
  「已登记项目」。命名与 D3 一致，**不叫** `board/list`。
- `settings_store.rs`：`board_watchman_enabled` → 通用键 `resident_daemon_watchman_enabled`
  + 旧键兼容读。
- `rpc/ui.rs`：同上，读写走通用键；`ui/settings/read|write` 的字段名同步。
- `ws.rs`：去掉 `board_dir` 注入；`plugin_command` 注入若随 scheme A 保留，改为通用
  「按插件名解析」的形态。
- `protocol.rs`：`board_query` 一族码名去看板化（`board_not_created` →
  `resident_not_registered`），数值码不动（-32020..-32024 保持），桌面同步。
- `lib.rs` / `Cargo.toml`：移除 `yi-agent-boards` 依赖（以「编译期不存在该 crate」为验收，
  与 Spec 4 §7 第 3 条同型）。

### 5.3 桌面

- `desktop/src/lib/superpowersKanbanBoards.ts`：`createBoard`/`removeBoard` 改调
  `plugin/query`（`plugin: "superpowers-kanban"`，
  `method: "lifecycle/create" | "lifecycle/remove"`）；`listBoards` 改调**通用**
  `resident/list`（D3 / §4.2；不再有 `board/list`）。
- `desktop/src/lib/routes.ts` + `routes.test.ts`：`boardCreate`/`boardRemove`/`boardList`
  三个常量删除或改写为 `residentList` + 通用插件方法名。
- `desktop/src/lib/boardIndex.ts`：`boardErrorKind` 的 `board_not_created` 码名同步为
  `resident_not_registered`。
- `desktop/src/hooks/useBoardState.ts`：`refreshBoards` 改读 `resident/list`。
- `desktop/src/App.tsx` + `App.test.tsx`：调用点逻辑不变，只换底层通道；测试 mock 的
  `board/list` / `board/create` 断言同步。
- `desktop/src/lib/superpowersKanbanSwitch.ts`：文件头「值守走 ui/settings」的部分保留；
  `readBoardWatchman`/`writeBoardWatchman` 的键名同步为通用键（兼容读旧键）。

### 5.4 插件

- `dispatch.rs`：新增 `lifecycle/create` / `lifecycle/remove`（与既有 `merge_request` 平行）。
- 插件获得「写自己的清单 + 登记 resident」的能力（今日由 `yi-agent-boards::scaffold`/
  `lifecycle` 代劳）。
- 插件**不依赖** `yi-agent-store`：resident 登记文件格式（`project` + `required_by`）
  是**两侧共享的通用文件契约**，插件按格式自行读写并在测试里覆盖。
- 若采纳 §4.4 方案 A：新增 `manifest` CLI 动词，打印插件自述的通用清单 JSON。

### 5.5 G1 遗留的归属缺口（本 spec 必须收口）

G1（`2026-10-17-session-driver-decoupling-design.md`）把宿主里的看板**决策面**清零，但留下两处只有本 spec（G2）能修的缺口；`docs/bug-list.md`、G1 plan 的「已知的 G1→G2 过渡缺口」与 §7 已记录，此处再点明以免只看本 spec 的实现者漏做：

- **被驱动会话的 `driver_project` / `driver_ref` 恒为 `None`**：`thread/listAll` 只在 `driver_project.is_some()` 时把会话折进项目组，被驱动会话因此以各自 cwd 为组键成**未分组顶层项**（不是折进其项目）；且 `worktree_reclaim::decide` 在 `driver_project` 为 `None` 时判 `Keep`，被驱动会话的 worktree **永不回收**。修法：引入通用字段 `managed_worktree`（会话 meta 的可选通用字段），由**插件侧在 `driver/next` 的动作里填值**；`worktree_reclaim::decide` 改为从 worktree 自身读分支；侧栏按 `managed_worktree` 归组。G1 的宿主测试 `a_launch_action_records_no_driver_origin_and_still_starts_a_turn` 现断言 `driver_project == None`，落地时需一并更新该断言。
- **宿主重启后卡滞留 / `Merging` 卡持有合并闸**：G1 删了 `recover_orphan_cards` 而 `DriverState` 是宿主内存，重启后插件收到空 `threads`，`Running`/`Launching`/`Merging` 卡无人结算，停在 `Merging` 的卡会让 `any_merging()` 恒真、该项目再无合并启动。本 spec 需确认插件侧对「宿主未上报且无在途动作」的卡做保守恢复（结算为 `needs_you` 并释放合并闸）在生命周期改造后仍生效。

### 5.6 CLI / TUI（刷新新增：`yi-agent` crate 的收口面）

`yi-agent-boards` 被 `yi-agent`（TUI/CLI）直接依赖，故「删 crate」必须同时收口：

- `yi-agent/src/tui/board.rs`（`/superpowers-kanban create|remove|status`）：改走
  **插件通道**（`plugin/query` 的 `lifecycle/*`）或保留为「提醒用户到插件里操作」；
  不得再 `use yi_agent_boards::*`。
- `yi-agent/src/tui/superpowers_kanban.rs`：**不再读 registry**（那是看板专属登记表）；
  改读通用 `resident::list` 判断「本项目是否要 daemon」。
- `yi-agent/src/tui/app.rs:2745`：`yi_agent_boards::global_dir()` 的调用点去掉。
- `yi-agent/src/main.rs:242`（`run_boards_watch`）+ `config.rs:189/196`
  （`Command::Boards` / `BoardsAction::Watch`）：值守循环改为**通用**子命令
  （如 `yi-agent resident watch`），plist 里的子命令字符串随之更新
  （§5.1 的 `watchman.rs` 通用化必须与本项**同批**落地，否则 plist 指向不存在的子命令）。
- `yi-agent/tests/board_e2e.rs`、`board_watchman_e2e.rs`：改断言新通道/新子命令
  （两测试本就有 `YI_AGENT_BOARD_E2E` 闸，默认跳过）。
- `yi-agent/Cargo.toml`：移除 `yi-agent-boards` 依赖。

> **注意（刷新实测）**：`yi-agent-supervisors::supervisor::Layout::for_workdir` 目前
> **硬编码** `state_dir = <workdir>/.yi-agent/superpowers-kanban`。通用化
> 「per-project daemon」时这里必须一并去专有名（或明确它是「某个插件的 state dir」
> 而由清单提供），否则验收 §6.2 对 `yi-agent-supervisors/src/` 的 grep 不可能归零。

## 6. 验收

**宿主（Rust）**

1. 编译期：`yi-agent-boards` 不再是 app-server **与 `yi-agent`** 的依赖（删依赖即证明）。
2. 非测试代码 grep：`yi-agent-app-server/src/`、`yi-agent-supervisors/src/`、
   `yi-agent/src/`（TUI/CLI）的 `kanban` / `board` / `卡片` 归零（通用插件机制除外）。
3. `board/create` / `board/remove` / `board/list` 三个 RPC 不再存在（`Domain::Board` 删除）。

**插件（Rust）**

4. `lifecycle/create`：写清单 + `resident::require`；重复调用幂等。
5. `lifecycle/remove`：停 daemon、删状态、`resident::release`；重复调用安全。

**迁移（Rust）**

6. 给定含 `boards.json` 的旧环境：首次运行后 `resident-daemons.json` 含该项目，
   `boards.json` 被改名，二次运行不再导入。**且必须有真实调用点**（§4.3 落点）。

**桌面（vitest）**

7. `createBoard`/`removeBoard` 走 `plugin/query`（mock 断言 method 与 plugin）；
   `listBoards` 走通用 `resident/list`。
8. 兼容：旧偏好键 `board_watchman_enabled` 仍被读到。

**端到端（人可验证）**

9. 桌面上「新建看板」 → 侧栏出现看板、插件被拉起、队列可推进（与今日行为一致）。
10. 「删除看板」 → 队列消失、daemon 停、登记表不再含该项目。

## 7. 涉及文件

- 删：`yi-agent-rs/crates/yi-agent-boards/`（整 crate；通用部分 §5.1 先迁出）、
  `app-server/src/rpc/board.rs`、`rpc/mod.rs` 的 `Domain::Board`
- 迁：`yi-agent-boards/src/{watch.rs, watchman.rs, board_daemon.rs}` → `yi-agent-supervisors`
- 改（宿主）：`app-server/src/{server.rs, protocol.rs, settings_store.rs, ws.rs, lib.rs,
  Cargo.toml, rpc/mod.rs, rpc/ui.rs, rpc_tests.rs}`
- 改（CLI/TUI）：`yi-agent/src/{main.rs, config.rs, tui/board.rs, tui/superpowers_kanban.rs,
  tui/app.rs, Cargo.toml}`、`yi-agent/tests/{board_e2e.rs, board_watchman_e2e.rs}`
- 改（通用）：`yi-agent-store/src/`（迁移函数 + 可能的新 RPC 支撑）、
  `yi-agent-supervisors/src/{supervisor.rs, manifest.rs, lib.rs}`
- 插件：`superpowers-kanban-runner/src/{dispatch.rs, service.rs}`（+ 若采纳 §4.4A 则
  `main.rs` 加 `manifest` 动词）
- 桌面：`desktop/src/lib/{superpowersKanbanBoards.ts, routes.ts, boardIndex.ts,
  superpowersKanbanSwitch.ts}` + 测试、`desktop/src/hooks/useBoardState.ts`、
  `desktop/src/App.tsx` + `App.test.tsx`

## 8. 风险

| 风险 | 处置 |
|---|---|
| 迁移遗漏导致「看板消失」（旧 `boards.json` 未导入） | 迁移幂等 + 单测覆盖「旧环境首启」+ **必须有调用点**（§4.3）；`boards.json.migrated` 留证 |
| 删 crate 牵动 app-server 与 `yi-agent`（TUI/CLI）大量编译错误 | 分步：先加通用替代（resident 路径 + 通用 CLI 子命令），再切调用点，最后删依赖；§5.6 与 §5.1 的 plist 必须同批 |
| 建盘引导鸡生蛋（§4.4）导致「全新项目建不了看板」 | **待用户裁定**后方开工 Task 4/5；未裁定前先做 Task 1/2/3/6/7 |
| 桌面对新通道的错误形态不识别 | 复用 `pluginIsUnavailable` / `boardErrorKind` 既有判定；码名去看板化（D11）时同步桌面文案与 `routes.test.ts` |
| 「每项目一个 daemon」的语义在通用化后变模糊 | 保留 10-02 规则原文：**谁成功启动 daemon，谁负责监督** |
| 与 G1 并行改同一批文件（`server.rs` / 插件 dispatch） | 排序：**先 G1 后 G2**（G1 已落地，本 spec 已据其刷新） |