# 插件生命周期去耦合（宿主不认识「看板」这个概念）Design

日期：2026-10-17
状态：**已落地**（2026-10-19）。宿主侧看板面已删净，插件侧 `manifest`/`lifecycle/*` 已就位，
端到端四条验收已实测（见 §6 末尾「落地记录」）。
原状态：设计已确认（对话中拍板：与 G1 一并全量还债）。
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

> **2026-10-18 刷新（第二次：main 前进 98 提交后）**：本 worktree 已并入 main 最新
> （`bf6e1971`；较本 spec 首版所据的 `9d885fb9` 领先 98 提交，合并无冲突）。逐条校正，
> **每条都有实测命令**：
>
> ① **§1.1 的门控已不存在**：Task 1 已落地（`19bc06f5`..`5a201c49`），
>    `plugin_query` 的 `registry::contains` 门控与 `global` 形参已删，现签名为
>    `plugin_query(project, method, plugin, params)`（`app-server/src/server.rs`）。
>    故 §1.1 表里「`plugin_query` 门控（`src/server.rs:1231`）」一行是**历史**；
>    §5.2 的「**新增**通用 `resident/list`」与 D3 已成既成事实
>    （`app-server/src/rpc/resident.rs` + `Domain::Resident`）。
>
> ② **验收 §6.2 的 grep 命令本身不成立**（实测）：
>    a. 它**大小写敏感**，而代码大量使用 `Board` / `BoardPluginState` /
>       `BoardQueryError` / `BoardLauncher`（实测 44 行匹配 `Board` 但不匹配小写 `board`：
>       `app-server` 36、`yi-agent`(TUI/CLI) 8、`yi-agent-supervisors` 0；命令
>       `grep -rn "Board" <crate>/src/ | grep -v board | wc -l`）
>       ——会**假通过**；
>    b. 它的排除表漏了 `onboarding`，而 `grep "board"` **会命中** `onboarding` /
>       `onboarding_rpc` / `onboarding_preferences_path`（仅 `rpc_tests.rs` 就 ≥14 行）
>       ——这条 grep **永远回不到零**。
>    修法与实测口径见改后的 §6.2。
>
> ③ **main 新增了与看板相关的既有面**（非本次改造引入，但 Task 6 的「grep 归零」必须面对）：
>    `e222f2b3` / `9882faf8` 引入 `ThreadStore::is_kanban_worktree`
>    （`app-server/src/thread_store.rs:139`，另见 `:161` 与 `rpc/thread.rs`、
>    `worktree_reclaim.rs`）。它**不读 board registry**，而是按**物理路径约定**
>    `<vcs 根>/.worktrees/kanban/<slug>` 判定（见 `thread_store.rs:124` 注释）。
>    这条约定归**看板插件**（worktree 由谁建谁认），不是宿主登记表——§6.2 必须点名它并
>    给出归属裁定，否则「宿主归零」与「识别看板 worktree」二者不可兼得。
>
> ④ **§1.1 的「体量」低估了 grep 面**：`board` 命中的大头并非 registry，而是
>    `board_project` / `card_id` 的 thread-meta 旧键别名（30 行）、偏好键
>    `board_watchman_enabled`（14 行）、`BoardQueryError`（17 行）、`board_dir`（22 行）等。
>    Task 6 的计划面按此重估。
>
> ⑤ **`yi-agent-supervisors` 已新增 `Layout::logs_dir()`**（`080fa293`，托管子进程日志落盘）：
>    §5.6 的 `Layout` 通用化必须**保留** `logs_dir()`（它已在注释里自认「不写进任何插件
>    自有目录」），别在去专有名时把它一并删掉。
>
> **§4.4 的裁定仍未拍板**（Task 4 的 create 半程继续冻结）；Task 1 已完成，Task 2/3 不受影响。

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
| `app-server` 的 `plugin_query` 门控 | ~~1 处关键门控（`src/server.rs:1231`）~~ **已删（Task 1，`19bc06f5`）** | 按 `yi_agent_boards::registry::contains` 决定「这个项目能不能被问」——本 spec 的目标之一，已落地 |
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
| D10 | 建盘引导 | 见 §4.4（**已裁定 A**：宿主通用 `plugin/install` + 开开关；B/C 被 §6.9 与安装方式排除） |
| D11 | 错误码 | 宿主 `board_query` 一族码名去看板化：**删除 `board_not_created` 整支**（§5.2 摘掉门控后宿主不再有「这个项目没有看板」这个回答），其余码名本已通用、数值码不动；桌面端相应删掉「未创建」这支错误展示与面板内「创建看板」按钮，创建入口保留侧栏右键菜单 |

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
| 桌面 `board/create {project}` → 宿主 `lifecycle::create_with_project` | 桌面先发**通用** `plugin/install {project, plugin}`（宿主：写清单 + 开开关 + 起 daemon，见 §4.4），再发 `plugin/query {plugin:"superpowers-kanban", method:"lifecycle/create", params:{project}}` → 插件：建自己的状态目录 + `resident::require(project, "superpowers-kanban")` |
| 桌面 `board/remove {project}` | 同上，`lifecycle/remove` → 插件：停 daemon、删状态目录、`resident::release` |
| 桌面 `board/list` → 宿主读 `boards.json` | 桌面调**通用** `resident/list`（宿主通用 RPC，回已登记项目），不再有看板专属列举 |

关键点：**建/删看板 = 插件的事**（它要建/删自己的状态目录）；
**装插件 = 宿主的通用能力**（它要在插件被拉起之前把清单和开关写好，见 §4.4）；
宿主只提供通用的「装一个插件进一个项目」「登记一个项目需要常驻 daemon」
「转发一条插件方法」，三者都不认识看板。

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

### 4.4 建盘引导：鸡生蛋（**已裁定：A**）

原 §4.2 让插件「写自己的清单」。但插件要被调用，前提是：清单已就位
（`<项目>/.yi-agent/supervisors/`）→ daemon 已拾取并拉起插件 → 插件才在自己的
socket 上应答。而今天**唯一**安装清单的代码是 `yi-agent-boards::scaffold::
install_manifest`，其唯一调用者是 `lifecycle::create_with_project`，即
`board/create`——正是本 spec 要删掉的那条路。于是：

- 若 create 直接改走 `plugin/query`，**全新项目**的 create 必然失败
  （即便摘掉门控，也拨不到尚未被拉起的插件 socket）；
- 验收 §6.9「桌面『新建看板』仍能建」因此不成立。

**裁定：方案 A。** 这不是在三个方案里挑偏好，而是本 spec 自己的约束把 B、C 排除了：

| 方案 | 为什么被排除 / 被选 |
|---|---|
| **C** | 与验收 §6.9「**与今日行为一致**」直接冲突。今天任何项目一键建板都能成（宿主替它写清单），C 会让未装插件的项目失败。§2 目标 5 也写死「用户可见行为不变」。 |
| **B** | 在今天的安装方式下**不可实现**：`plugins/superpowers-kanban/INSTALL.md:46` 只把**二进制**装到 `~/.cargo/bin/`，清单留在源码树 `supervisors/superpowers-kanban.json`；已安装的二进制与源码树文件之间没有可推导关系（实测：`include_str!` 未嵌入清单，只有 `driver-protocol.json` 与 `superpowers-kanban.toml` 被嵌入）。要让它成立只能把清单嵌进二进制再吐出来——那就是 A，只是换了传输方式。 |
| **A（选定）** | 唯一同时满足 §6.9 与「宿主不认识看板」的形态。且它的难点（按插件名通用解析可执行文件）**已经建好**：`app-server/src/server.rs:243` → `plugin_worktree::resolve_plugin_command(name)`；`rpc/mod.rs:119` 也已把 `plugin_command` 记为「§4.4 落地时的接缝，必须保留」。 |

**A 的落地形状**：

1. `yi-agent-supervisors` 新增通用 `install_manifest_for(project, plugin_name, manifest_json)`；
2. 宿主新增通用 RPC `plugin/install {project, plugin}`：按 `plugin` 名在 PATH 上解析
   可执行文件（不硬编码任何插件名），执行 `<plugin> manifest` 取该插件**自述**的
   通用清单，写入 `<项目>/.yi-agent/supervisors/<plugin>.json`；
3. **打开清单里的 `switch_key`**（见下「开关」）；
4. 确保项目 daemon 在跑（复用 `yi-agent-supervisors::daemon::ensure_running`）；
5. 桌面 create 改为「先 `plugin/install`，再 `plugin/query lifecycle/create`」。

宿主全程不认识看板：清单内容来自插件自述，宿主只搬字节。

#### 4.4.1 开关（初稿遗漏的一半）

`create_with_project` 当年做了**两件**事，`install_manifest` 只是其一：

```rust
scaffold::install_manifest(project, plugin_command)?;  // 写清单
scaffold::enable_switch(project)?;                    // preferences.json: superpowers_kanban = true
```

第二件在 A/B/C 三个方案里**都没提**，但缺了它行为不等价：

- 开关默认**关**（`superpowers-kanban-core/src/switch.rs:26-31`，测试
  `both_layers_missing_defaults_to_disabled`）；
- 插件的推进循环整体被它门控（`runner/src/main.rs:976`：
  `if !board_switch(...).is_enabled() { sleep; continue; }`）；
- 新的 `lifecycle_create`（`service.rs:950`）**不写**开关，桌面 `createBoard` 也只发
  `lifecycle/create`（`setBoardSwitch` 只在用户手动拨动时调用，`App.tsx:844`）。

后果：清单装好、插件也起来了，但队列**永不推进**——直接违反 §6.9 的「队列可推进」。

**处置**：`plugin/install` 在写完清单后，把清单声明的 `switch_key` 写成 `true`（项目层
`<项目>/.yi-agent/preferences.json`，与宿主 `Supervisor::project_preferences_path()`、
插件 `project_preferences_path()` 同一个落点）。这一步是 A 的**必要组成部分**，不是
新方案；它由宿主执行而不是插件，理由与清单相同——插件在开关打开前不会被拉起。

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
  > **已落地（Task 1，`19bc06f5`）**：`app-server/src/rpc/resident.rs` + `Domain::Resident`
  > + `domain_for` 的 `resident` 臂；形状 `{"projects":[{"project","required_by"}]}`，
  > 纯文件读取、缺失/损坏回空表。
- `settings_store.rs`：`board_watchman_enabled` → 通用键 `resident_daemon_watchman_enabled`
  + 旧键兼容读。
- `rpc/ui.rs`：同上，读写走通用键；`ui/settings/read|write` 的字段名同步。
- `ws.rs`：去掉 `board_dir` 注入；`plugin_command` 注入若随 scheme A 保留，改为通用
  「按插件名解析」的形态。
- `thread_store.rs` / `rpc/thread.rs` / `worktree_reclaim.rs`：main 的 `e222f2b3`/`9882faf8`
  带进 `is_kanban_worktree`（`thread_store.rs:139`）——它按**物理路径约定**
  `<vcs 根>/.worktrees/kanban/<slug>` 识别「这是看板插件的 worktree」，**不读 board
  registry**。§6.2 的宿主归零要求先给它定归属（三选一，实现者取一并写进注释）：
  ① 保留为**通用**「插件 worktree 路径约定」并只做大小写/命名中性化
  （如 `.worktrees/<plugin>/`，由清单声明该插件的前缀）；
  ② 改为读清单/meta 的**通用**标记（如 §5.5 的 `managed_worktree`）而不再猜路径；
  ③ 明确它是**插件侧**的约定，宿主只接受一个通用前缀参数，字符串本身不进宿主源码。
  同一处 `board_project` / `card_id` 的 meta **旧键别名**（`thread_store.rs:56-58`，
  30 行）属 G1 遗留的通用化别名，**保留兼容读**，不计入归零面（它们在 `test`/注释里居多）。
- `protocol.rs`：`board_query` 一族码名去看板化。**`board_not_created` 整支删除**
  ——§5.2 摘掉 `plugin_query` 的登记表门控后，宿主不会再回答「这个项目没有看板」；
  其余码名（`daemon_unavailable` / `plugin_unavailable` / `plugin_not_installed` /
  `plugin_rejected`）本已通用，保留，数值码 `-32021..-32024` 不动。
  桌面端同步删掉 `boardErrorKind` 的 `not_created` 支与面板内「创建看板」按钮
  （创建入口保留侧栏项目右键菜单 `ThreadSidebar.tsx:939` 那一条）。
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
- 插件获得「建/删自己的状态目录 + 登记 resident」的能力（今日由 `yi-agent-boards::lifecycle`
  代劳）。**清单不归插件**（§4.4 已裁定 A）：插件在清单就位前起不来，写不了它。
- 插件**不依赖** `yi-agent-store`：resident 登记文件格式（`project` + `required_by`）
  是**两侧共享的通用文件契约**，插件按格式自行读写并在测试里覆盖。
- **新增 `manifest` CLI 动词**（§4.4 方案 A）：stdout 打印插件自述的通用清单 JSON，
  `command` 填该可执行文件自身的绝对路径（`std::env::current_exe()`），其余字段与
  `supervisors/superpowers-kanban.json` 一致。宿主 `plugin/install` 直接消费这段 stdout，
  因此**这份自述是清单的唯一真相源**；随包分发的 `.json` 退化为「文档/安装脚本的参考」，
  两者漂移时以二进制自述为准（§4.4 已说明 B 不可行的原因）。

### 5.4.1 宿主：`plugin/install`（§4.4 方案 A）

- `yi-agent-supervisors`：新增通用 `install_manifest_for(project, plugin_name, manifest_json)`
  与 `enable_switch(project, switch_key)`（从已删的 `scaffold.rs` 迁出，去掉看板名）。
- `app-server`：新增通用 RPC `plugin/install {project, plugin}`：
  1. `plugin_worktree::resolve_plugin_command(plugin)` 解析可执行文件（已存在，
     `server.rs:243` 的接缝就用它）；
  2. 跑 `<plugin> manifest` 取 stdout，校验是 JSON 对象且含 `name`/`switch_key`；
  3. `install_manifest_for` 写 `<项目>/.yi-agent/supervisors/<plugin>.json`；
  4. `enable_switch` 把 `switch_key` 写成 `true`（§4.4.1）；
  5. `daemon::ensure_running(project)`。
  宿主**不解释**清单里的任何插件语义，只按名搬运与开开关。

### 5.5 G1 遗留的归属缺口（本 spec 必须收口）

G1（`2026-10-17-session-driver-decoupling-design.md`）把宿主里的看板**决策面**清零，但留下两处只有本 spec（G2）能修的缺口；`docs/bug-list.md`、G1 plan 的「已知的 G1→G2 过渡缺口」与 §7 已记录，此处再点明以免只看本 spec 的实现者漏做：

- **被驱动会话的 `driver_project` / `driver_ref` 恒为 `None`**：`thread/listAll` 只在
  `driver_project.is_some()` 时把会话折进项目组，被驱动会话因此以各自 cwd 为组键成**未分组顶层项**（不是折进其项目）；且 `worktree_reclaim::decide` 在 `driver_project` 为 `None` 时判 `Keep`，被驱动会话的 worktree **永不回收**。修法：引入通用字段 `managed_worktree`（会话 meta 的可选通用字段），由**插件侧在 `driver/next` 的动作里填值**；`worktree_reclaim::decide` 改为从 worktree 自身读分支；侧栏按 `managed_worktree` 归组。G1 的宿主测试 `a_launch_action_records_no_driver_origin_and_still_starts_a_turn` 现断言 `driver_project == None`，落地时需一并更新该断言。
  > **已由 main `95c488eb`（`27b63e01` 合入）落地（2026-10-19 实测）**：插件在 launch 动作里
  > 填 `driver_project` / `driver_ref`，`session_driver.rs` 据此写入会话归属，`thread/listAll`
  > 折进项目组。注意**实际采用的字段名是 `driver_project`（不是本文初稿提的 `managed_worktree`）**
  > ——`managed_worktree` 在 `yi-agent-rs/crates/` 里零命中。故 G2 **不再需要**动这一处，
  > 只需在验收时确认它仍然成立（Task 7）。
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
>
> **保留 `logs_dir()`**：main 的 `080fa293` 给 `Layout` 新增了
> `logs_dir()`（`<workdir>/.yi-agent/logs`，每清单一个 `<name>.log`），其注释已自认
> 「不写进任何插件自有目录」。去专有名只能动 `state_dir`，**不得**顺手删掉
> `logs_dir()` / `ChildLog`——那是托管子进程日志的落点。

## 6. 验收

**宿主（Rust）**

1. 编译期：`yi-agent-boards` 不再是 app-server **与 `yi-agent`** 的依赖（删依赖即证明）。
2. 非测试代码 grep：`yi-agent-app-server/src/`、`yi-agent-supervisors/src/`、
   `yi-agent/src/`（TUI/CLI）的 `kanban` / `board` / `卡片` 归零（通用插件机制除外）。
   > **命令必须如此写**（初稿那条既假通过又永不归零，见顶部刷新 ②）：
   > ```bash
   > grep -rni "kanban\|board\|卡片" \
   >   yi-agent-rs/crates/yi-agent-app-server/src/ \
   >   yi-agent-rs/crates/yi-agent-supervisors/src/ \
   >   yi-agent-rs/crates/yi-agent/src/ --include=*.rs \
   >   | grep -viE "keyboard|clipboard|onboarding|test|//|#\[cfg\(test\)\]"
   > ```
   > 三处修正：**加 `-i`**（否则 `Board` / `BoardPluginState` / `BoardQueryError` 漏检，
   > 假通过）；**排除 `onboarding`**（`grep "board"` 会匹配 `onboarding*`，不排则永不归零）；
   > 保留 `test` / `//` 的排除。归零前须先裁定顶部刷新 ③ 的
   > `is_kanban_worktree`（物理路径约定 `.worktrees/kanban/`）归属。
   > **裁决（2026-10-19 实测，51 行残留，逐项允许）**
   >
   > 初稿的「归零」在字面上不可达，且**不该**可达：剩下的每一行要么是磁盘／系统上
   > 的既成事实（删掉就破坏老环境），要么是用户可见的命令名（删掉就违反 §6.9）。
   > 下面这份清单就是上面那句「通用插件机制除外」的具体化，实现在
   > `app-server/src/legacy_compat.rs` 与 `app-server/src/plugin_worktree.rs`
   > 两处的模块文档里逐条说明了理由。
   >
   > | 落点 | 行数 | 性质 | 为什么不能消掉 |
   > |---|---|---|---|
   > | `legacy_compat.rs` | 13 | 旧格式解析 | `boards.json` 的形状、旧目录 `$HOME/.yi-agent/superpowers-kanban`、迁移写入的 `required_by` 值。全部集中在这一个模块，宿主别处不再拼这些字符串（行号 20,28,39,40,44,48,53,54,59,60,88,91,95） |
   > | `server.rs:2041,2064` | 2 | 迁移调用点 | 只引用上面的常量／函数，不自己持有字面量 |
   > | `settings_store.rs:101` | 1 | 旧偏好键 | 老机器上写下的「关掉登录自启」必须仍生效（§6.8） |
   > | `thread_store.rs:58` | 1 | 旧 meta 键别名 | G1 遗留的兼容读，spec 原文已允许 |
   > | `watchman.rs:22` | 1 | 旧 launchd 标签 | 不清理会在老机器上同时跑两个值守 |
   > | `plugin_worktree.rs:39` | 1 | 插件 worktree 命名空间 | §5.2 方案 (a)：纯路径判据要认出「这是插件划的领地」，宿主必须知道保留名；真机已有 9 例幽灵组长 |
   > | `tui/superpowers_kanban.rs` | 19 | 用户可见命令 `/superpowers-kanban` + 薄转发层 + 文案 | §5.6 明确允许「保留为提示」；这里的 19 行既有命令名与文案，也有 `render_board` 与 `on\|off\|run\|add\|promote` 的动词分发——它们是**薄转发层**，内部走通用插件通道，不是宿主在实现看板语义。改名/删命令等于改用户可见行为（§6.9） |
   > | `tui/slash.rs` | 8 | 同上（命令注册与 `kanban` 过渡别名） | 同上 |
   > | `tui/app.rs` | 4 | 同上（分发与更名提示） | 同上 |
   > | `tui/mod.rs:18` | 1 | 模块名 | 与上面的命令同源 |
   >
   > 合计 **51**（13+2+1+1+1+1+19+8+4+1），与脚本实测一致。
   >
   > **判定标准**：宿主是否**理解**看板语义。它不再理解——建/删走 `plugin/query`，
   > 列举走 `resident/list`，装插件走 `plugin/install`，且 `plugin/install` 里没有
   > 任何具体插件名。上表全是「认出一个既成的名字」而不是「实现一个看板功能」。
   >
   > 复核命令（排除测试模块与注释后应为 51 行，且逐行能对到上表）：
   > ```bash
   > python3 - <<'EOF'
   > import re, pathlib
   > roots=["yi-agent-rs/crates/yi-agent-app-server/src",
   >        "yi-agent-rs/crates/yi-agent-supervisors/src",
   >        "yi-agent-rs/crates/yi-agent/src"]
   > files=[p for r in roots for p in pathlib.Path(r).rglob("*.rs")]
   > pat=re.compile(r"kanban|board|卡片",re.I)
   > excl=re.compile(r"keyboard|clipboard|onboarding|//|#\[cfg\(test\)\]",re.I)
   > def inside(lines):
   >     ins=set(); i=0
   >     while i<len(lines):
   >         if re.match(r"\s*#\[cfg\(test\)\]",lines[i]):
   >             j=i
   >             while j<len(lines) and "{" not in lines[j]: j+=1
   >             if j<len(lines):
   >                 d=0
   >                 for k in range(j,len(lines)):
   >                     d+=lines[k].count("{")-lines[k].count("}")
   >                     ins.add(k+1)
   >                     if d<=0: break
   >         i+=1
   >     return ins
   > n=0
   > for p in files:
   >     t=p.name.endswith("_tests.rs") or "/tests/" in str(p)
   >     lines=p.read_text(errors="replace").splitlines()
   >     skip=inside(lines) if not t else set(range(1,len(lines)+1))
   >     for i,l in enumerate(lines,1):
   >         if pat.search(l) and not excl.search(l) and i not in skip: n+=1
   > print(n)
   > EOF
   > ```
   > 之所以附上这条脚本而不是 spec 原命令：`grep -v test` 只能滤掉**行内含 test
   > 字样**的行，滤不掉整个 `#[cfg(test)] mod tests { … }` 块（实测 `server.rs`
   > 就有 40+ 行测试代码被算进「生产」）。

3. `board/create` / `board/remove` / `board/list` 三个 RPC 不再存在（`Domain::Board` 删除）。
3a. **通用 `plugin/install {project, plugin}`**（§4.4 方案 A）：
   - 按 `plugin` 名在 PATH 上解析可执行文件，**宿主源码里不出现任何具体插件名**；
   - 执行 `<plugin> manifest`，把 stdout 原样写入
     `<项目>/.yi-agent/supervisors/<plugin>.json`（宿主不解析清单语义，只校验是 JSON 对象）；
   - 把清单声明的 `switch_key` 写成 `true`（项目层 `preferences.json`），
     **保留无关键**（与 `runtime_prefs` 的 read-modify-write + tmp/rename 同型）；
   - 确保项目 daemon 在跑（复用 `yi-agent-supervisors::daemon::ensure_running`）；
   - 幂等：重复调用不重复写、不重置用户后来关掉的开关之外的状态
     （开关按「安装即开启」写入，与今日 `enable_switch` 一致）。
3b. **开关这半程不得丢失**（§4.4.1）：新建的看板在**无人工干预**下队列可推进
   （即 `switch.read` 回 `on: true`，`source: "project"`）。

**插件（Rust）**

4. `lifecycle/create`：建状态目录 + `resident::require`（**不写清单**，清单归宿主的
   `plugin/install`，见 §4.4）；重复调用幂等。
4a. 插件新增 `manifest` 动词：stdout 输出**自述清单**（与
   `plugins/superpowers-kanban/supervisors/superpowers-kanban.json` 同构，
   `command` 为该可执行文件自身路径），退出码 0。
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

### 落地记录（2026-10-19）

| 验收 | 证据 |
|---|---|
| 1. 依赖删净 | `grep -rn "yi-agent-boards" yi-agent-rs/crates/*/Cargo.toml` 无命中；crate 目录已删（`1dcababe`） |
| 2. 宿主 grep | 复核脚本（上方）实测 **51** 行，逐文件与 §6.2 表一致 |
| 3. board RPC 消失 | `grep -rn "Domain::Board\|board/create\|board/remove\|board/list" yi-agent-rs/crates --include=*.rs` 只剩 5 行注释里的历史说明（`server.rs:366,9263`、`rpc/plugin.rs:364`、`rpc_tests.rs:17026`、`tests/common/mod.rs:292`），无代码引用 |
| 3a. `plugin/install` | `app-server/src/rpc/plugin.rs`；`rpc_tests.rs` 的 `plugin_install_*` 九条 |
| 3b. 开关半程 | `plugin/install` 调 `enable_switch`；`switch.read` 回 `on:true, source:"project"`（真实二进制 e2e） |
| 4/4a/5. 插件动词 | `superpowers-kanban` 的 `manifest` / `lifecycle/create` / `lifecycle/remove`；`main.rs` 测试 |
| 6. 旧 `boards.json` 迁移 | `app-server/src/server.rs` 两个 stdio 入口 + `ws::serve_ws`；`legacy_compat.rs` 解析 |
| 7/8. 桌面 | 108 files / 1399 tests；`createBoard`/`removeBoard` 走 `plugin/query`，`listBoards` 走 `resident/list` |
| 9/10. 端到端 | 真实二进制：`plugin/install` → `lifecycle/create` → `switch.read on` → `resident/list` 含该项目 → `lifecycle/remove` → 登记表清空 |

新增缺陷（本 spec 之外的既有问题，一并修掉）：daemon 重启后，上一代留下的健康插件
进程不进 `running`，其查询路由从不登记 → `plugin/query` 恒回 `-32022`。修于
`yi-agent-supervisors/src/supervisor.rs::query_sockets`（`running` **或** 当前应答），
三个测试钉住（`3a62c78d`）。

## 7. 涉及文件

- 删：`yi-agent-rs/crates/yi-agent-boards/`（整 crate；通用部分 §5.1 先迁出）、
  `app-server/src/rpc/board.rs`、`rpc/mod.rs` 的 `Domain::Board`
- 迁：`yi-agent-boards/src/{watch.rs, watchman.rs, board_daemon.rs}` → `yi-agent-supervisors`
- 改（宿主）：`app-server/src/{server.rs, protocol.rs, settings_store.rs, ws.rs, lib.rs,
  Cargo.toml, rpc/mod.rs, rpc/ui.rs, rpc_tests.rs, thread_store.rs, rpc/thread.rs,
  worktree_reclaim.rs}`
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
| 建盘引导鸡生蛋（§4.4）导致「全新项目建不了看板」 | **已裁定 A**（§4.4）：宿主通用 `plugin/install` 写清单 + 开开关 + 起 daemon，桌面再发 `lifecycle/create`。开关这半程初稿遗漏，见 §4.4.1 |
| 桌面对新通道的错误形态不识别 | 复用 `pluginIsUnavailable` / `boardErrorKind` 既有判定；码名去看板化（D11）时同步桌面文案与 `routes.test.ts` |
| 「每项目一个 daemon」的语义在通用化后变模糊 | 保留 10-02 规则原文：**谁成功启动 daemon，谁负责监督** |
| 与 G1 并行改同一批文件（`server.rs` / 插件 dispatch） | 排序：**先 G1 后 G2**（G1 已落地，本 spec 已据其刷新） |