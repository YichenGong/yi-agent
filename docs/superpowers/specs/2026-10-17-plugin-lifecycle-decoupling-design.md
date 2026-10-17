# 插件生命周期去耦合（宿主不认识「看板」这个概念）Design

日期：2026-10-17
状态：设计已确认（对话中拍板：与 G1 一并全量还债），待用户 review 本文件后进入 writing-plans。
范围：宿主（`yi-agent-rs/crates/yi-agent-boards/`、`yi-agent-app-server/`）、桌面（`desktop/`）。
关联：Spec 4（`2026-10-01-superpowers-kanban-decoupling-design.md`，本 spec 是其迟到的收尾）、
`2026-10-01-superpowers-kanban-overview-design.md`（决策 8「主程序彻底不认识看板」）、
`2026-10-02-per-project-kanban-boards-design.md`、`2026-10-02-kanban-supervisor-for-embedded-daemons-design.md`。
姊妹篇：G1 `2026-10-17-session-driver-decoupling-design.md`（会话驱动面）。

## 1. 背景与问题

### 1.1 Spec 4 只收了一半

Spec 4 的验收第 3 条写得很清楚：

> 旧路径（直接读 `board.json`）在主程序侧**已不存在**：以「编译期不存在该 crate」为验收
> （删除 `yi-agent-board-ui` 依赖即证明）。

`yi-agent-board-ui` 确实删了，但**宿主里又长出了一个新的看板专属 crate 与一组专属 RPC**：

| 位置 | 体量 | 内容 |
|---|---|---|
| `yi-agent-boards` crate | **1745 行** | `registry`（全局看板登记表）、`lifecycle`（建/删/查）、`scaffold`（清单 + 开关）、`watch`、`watchman`（launchd）、`board_daemon`（拉起 `yi-agent daemon serve`） |
| `server.rs` 的 `board/*` RPC | 3 个方法（15 处） | `board/create`、`board/remove`、`board/list` |
| `settings_store.rs` | 7 处 | `board_watchman_enabled` 偏好键 |
| `ws.rs` | 3 处 | 起会话时注入 `board_dir` 与看板 launcher |
| 桌面 | 8 处（3 文件） | `superpowersKanbanBoards.ts` 调 `board/create\|remove\|list` |

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

`query_socket` 已落地（`manifest.rs` 注释亦自认「Generic: the field names no plugin and
the daemon attaches no meaning to the answers」），但**谁该做「每项目一个 daemon / 拉起插件」
这件事**没有随之通用化。

### 1.3 已有的通用替代物

- `yi_agent_store::resident`（`resident.rs` 头部注释）：
  「通用『常驻 daemon』登记：哪些项目需要常驻 daemon、由谁请求。**与任何具体插件/看板无关**：
  字段只有 `project` 与 `required_by`。」——**这已经是**看板登记表的通用替代。
- `yi_agent_supervisors::manifest`：通用插件清单（`name`/`command`/`args`/`switch_key`/
  `stop_when_disabled`/`query_socket`），看板只是其中一个插件。
- `yi_agent_store::resident::list()` —— 通用「哪些项目要常驻 daemon」。
- `watch.rs::once()`（宿主内循环）读的就是**通用登记**（`resident::list`），注释亦自认
  「读通用登记」。

即：通用机制**都在**，只是外面还套了一层看板专用的 `registry.rs`（`boards.json`）
与 `board/*` RPC。本 spec 把这层套壳去掉。

## 2. 目标与非目标

**目标**

1. 宿主不再有看板专属 crate、登记表与 RPC。
2. 桌面端的「建/删/查看板」改走**通用插件通道**（Spec 4 §5 的原始计划）。
3. 「每项目一个 daemon」与「插件被监督」改为**通用**机制，不再叫 board。
4. 判据：宿主侧 grep `kanban` / `board` / `卡片` 在非测试代码中归零（通用插件机制除外）。
5. 端到端等价：**用户可见行为不变**——桌面上仍能建看板、看板仍会被拉起与监督。

**非目标**

- 不动 G1 的会话驱动面（`driver/next`、队列、合并）。
- 不改插件自身的状态模型（`board.json` 仍是插件私有；Spec「单一真相源」已另立条目）。
- 不改 `SupervisorManifest` 的既有字段语义（只可能新增通用可选字段）。
- 不重做桌面 UI 布局；只换调用通道。
- 不做「多进程共享监督者」的选举（沿用 10-02 决策：谁拥有 daemon，谁监督）。

## 3. 设计决策汇总

| # | 决策 | 取值 |
|---|---|---|
| D1 | 看板登记表 | 删除 `boards.json` / `registry.rs`；「哪些项目要 daemon」以 `resident::list` 为唯一来源 |
| D2 | 建/删看板 | 改走**插件通道**（`plugin_query`），由插件实现 `lifecycle/create` / `lifecycle/remove` |
| D3 | 列已登记项目 | 改为通用 `resident::list`（宿主侧一个通用 RPC，或复用现有 `plugin/list` 面） |
| D4 | daemon 拉起 | 沿用 `watch.rs`（已读通用登记）；`board_daemon.rs` 的「board」命名与路径假设去专有名 |
| D5 | 监督 | 沿用 `yi-agent-supervisors`；`scaffold.rs` 的看板专属模板改为通用清单写出 |
| D6 | 开关键 | `board_watchman_enabled` → 通用键（保留旧键的**兼容读取**，见 D8） |
| D7 | 桌面 | `superpowersKanbanBoards.ts` 改调 `plugin/query`（`plugin: "superpowers-kanban"`） |
| D8 | 迁移 | 旧 `boards.json` 首次运行时由插件/宿主**一次性导入** `resident` 登记，随后只读新源 |
| D9 | crate 归宿 | `yi-agent-boards` 中通用部分（watch / launchd）迁入 `yi-agent-supervisors` 或 `yi-agent-runtime`；看板专属部分删除 |

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
| 桌面 `board/create {project}` → 宿主 `lifecycle::create_with_project` | 桌面 `plugin/query {plugin:"superpowers-kanban", method:"lifecycle/create", params:{project}}` → 插件：`resident::require(project, "superpowers-kanban")` + 建目录 + 写清单 |
| 桌面 `board/remove {project}` | 同上，`lifecycle/remove` → 插件：停 daemon、删 `board.json`、`resident::release` |
| 桌面 `board/list` → 宿主读 `boards.json` | 桌面读**通用**登记（`resident::list` 的 RPC 或插件 `list` 的既有字段） |

关键点：**建/删看板 = 插件的事**（它要建自己的状态目录、写自己的清单）；
宿主只提供通用的「登记一个项目需要常驻 daemon」与「转发一条插件方法」。

### 4.3 一次性迁移

首次运行（宿主或插件任一侧先到）时：

1. 若 `boards.json` 存在且非空：对每个 `project` 调 `resident::require(project, "superpowers-kanban")`。
2. 迁移完成后把 `boards.json` 重命名为 `boards.json.migrated`（保留取证，不被再次读）。
3. 幂等：`resident::require` 本身幂等（重复登记只并集 `required_by`）。

旧偏好键 `board_watchman_enabled`：**读时兼容**（读到旧键仍生效），写时写新通用键。

## 5. 改动明细

### 5.1 删 / 迁

- **删除** `yi-agent-boards/src/registry.rs`（200 行）与 `<global>/boards.json` 的读写。
- **删除** `yi-agent-boards/src/scaffold.rs` 的看板专属模板（237 行），保留/迁出「写出通用
  插件清单」的能力（产物就是 `SupervisorManifest` 的 JSON）。
- **迁移** `yi-agent-boards/src/{watch.rs, watchman.rs, board_daemon.rs}` 到通用归属：
  - `watch.rs` 已通用（读 `resident`）→ 迁 `yi-agent-supervisors` 或 `yi-agent-runtime`。
  - `watchman.rs`（launchd plist）→ 通用化命名（`resident-daemon-watchman`），不认看板。
  - `board_daemon.rs`（`spawn_detached` / `wait_ready` / `stop`）→ 通用「per-project runtime
    daemon 生命周期」，命名去掉 board。
- **删除** `yi-agent-boards/src/lifecycle.rs` 的建/删语义（741 行中的大部分），或整体删除，
  由插件接管（插件经 `plugin_query` 暴露 `lifecycle/*`）。

### 5.2 宿主

- `server.rs`：删 `board/create` / `board/remove` / `board/list` 三个 RPC 分支；
  新增通用 `resident/list`（或复用现有通用面）供桌面查「已登记项目」。
- `settings_store.rs`：`board_watchman_enabled` → 通用键 + 旧键兼容读。
- `ws.rs`：去掉 `board_dir` 注入（改由通用 supervisor 目录推导）。
- `lib.rs` / `Cargo.toml`：移除 `yi-agent-boards` 依赖（以「编译期不存在该 crate」为验收，
  与 Spec 4 §7 第 3 条同型）。

### 5.3 桌面

- `desktop/src/lib/superpowersKanbanBoards.ts`：三个函数改调 `plugin/query`
  （`plugin: "superpowers-kanban"`，`method: "lifecycle/create" | "lifecycle/remove"`）。
- `listBoards()`：改读通用登记（或插件 `list`）。
- `App.tsx` 三处调用点不改逻辑，只换底层通道。

### 5.4 插件

- `dispatch.rs`：新增 `lifecycle/create` / `lifecycle/remove`（与既有 `merge_request` 平行）。
- 插件获得「写自己的清单 + 登记 resident」的能力（今日由 `yi-agent-boards::scaffold`/
  `lifecycle` 代劳）。

## 6. 验收

**宿主（Rust）**

1. 编译期：`yi-agent-boards` 不再是 app-server 的依赖（删依赖即证明）。
2. 非测试代码 grep：`kanban` / `board` / `卡片` 归零（通用插件机制除外）。
3. `board/create` / `board/remove` / `board/list` 三个 RPC 不再存在。

**插件（Rust）**

4. `lifecycle/create`：写清单 + `resident::require`；重复调用幂等。
5. `lifecycle/remove`：停 daemon、删状态、`resident::release`；重复调用安全。

**迁移（Rust）**

6. 给定含 `boards.json` 的旧环境：首次运行后 `resident-daemons.json` 含该项目，
   `boards.json` 被改名，二次运行不再导入。

**桌面（vitest）**

7. `createBoard`/`removeBoard`/`listBoards` 走 `plugin/query`（mock 断言 method 与 plugin）。
8. 兼容：旧偏好键 `board_watchman_enabled` 仍被读到。

**端到端（人可验证）**

9. 桌面上「新建看板」 → 侧栏出现看板、插件被拉起、队列可推进（与今日行为一致）。
10. 「删除看板」 → 队列消失、daemon 停、登记表不再含该项目。

## 7. 涉及文件

- 删：`yi-agent-rs/crates/yi-agent-boards/src/{registry.rs, scaffold.rs}`（或迁出通用部分）、
  `server.rs` 的三个 `board/*` 分支
- 迁：`yi-agent-boards/src/{watch.rs, watchman.rs, board_daemon.rs}` → `yi-agent-supervisors`
  / `yi-agent-runtime`
- 改：`server.rs`、`settings_store.rs`、`ws.rs`、`lib.rs`、`Cargo.toml`
- 插件：`superpowers-kanban-runner/src/dispatch.rs`
- 桌面：`desktop/src/lib/superpowersKanbanBoards.ts`（+ 测试）、`App.tsx`（调用点）

## 8. 风险

| 风险 | 处置 |
|---|---|
| 迁移遗漏导致「看板消失」（旧 `boards.json` 未导入） | 迁移幂等 + 单测覆盖「旧环境首启」；`boards.json.migrated` 留证 |
| 删 crate 牵动 app-server 大量编译错误 | 分步：先加通用替代（resident 路径），再切调用点，最后删依赖 |
| 桌面对新通道的错误形态不识别 | 复用 `pluginIsUnavailable` 既有判定；新增错误码时同步桌面文案 |
| 「每项目一个 daemon」的语义在通用化后变模糊 | 保留 10-02 规则原文：**谁成功启动 daemon，谁负责监督** |
| 与 G1 并行改同一批文件（`server.rs` / 插件 dispatch） | 排序：**先 G1 后 G2**（G1 删 `card_scheduler` 与 `board_query`，G2 再删 `yi-agent-boards` 与 RPC），避免两次改同一处 |
