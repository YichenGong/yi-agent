# 通用会话驱动（宿主不认识看板）Design

日期：2026-10-17
状态：设计已确认（对话中逐项拍板：解耦方向 B 全量、驱动方向「拉」），待用户 review 本文件后进入 writing-plans。
范围：宿主（`yi-agent-rs/crates/yi-agent-app-server/`）、插件（`plugins/superpowers-kanban/`）。
关联：Spec 4（`2026-10-01-superpowers-kanban-decoupling-design.md`，解耦宪法）、10-03（`2026-10-03-kanban-cards-as-visible-threads-design.md`，引入卡片调度器之处）、10-08（`2026-10-08-board-merge-as-session-design.md`，会话驱动合并的既有骨架）。

## 1. 背景与问题

### 1.1 宪法与现状的冲突

`2026-10-01-superpowers-kanban-overview-design.md` 的决策表第 8 条（用户逐项拍板）：

> | 8 | 解耦方向 | **B**：插件通过 IPC 暴露状态，**主程序彻底不认识看板** |

Spec 4 把它写成硬目标：

> 让主程序（app-server / TUI）**完全不认识看板**：不知道 `.yi-agent/superpowers-kanban/`、
> 不知道 `board.json`、不知道 `inbox/`、不知道「卡片」。主程序只认识一条通用能力——
> 「向某个插件问一个方法」。

并明确规定方法名归属：

> 承载看板语义的方法名（`list` / `enqueue` / `switch.read` / `switch.write`）是**桌面端与插件
> 之间**的约定，写在插件文档与桌面端代码里，**主程序不感知**。

**现状相反。** 宿主里长出了一整套看板语义，且从无正式修订：

| 位置 | 看板语义体量（非测试） | 内容 |
|---|---|---|
| `card_scheduler.rs` | 803 行 / 67 处 | `plan`、`run_once`、`TrackedThread`、`Outcome`、`objective_for`、`LaunchRequest.card_id`/`board_project` |
| `server.rs` | 112 处 | `KANBAN_PLUGIN` 硬编码、`board_query`、`BoardCard`、`board_cards`、`recover_orphan_cards`、`ServeLauncher`、`board/create\|remove\|list` RPC |
| `worktree_reclaim.rs` | 19 处 | `slugify`（注释自认「与插件 `worktree::slugify` 逐字一致」）、`kanban/<slug>` 约定、`card_id`/`board_project` |

三处最能说明耦合深度：

1. 宿主**写死 12 处 `board.*` 方法名**（`board.next_launch` / `board.mark_running` /
   `board.mark_terminal` / `board.release`）——正是 Spec 4 说「主程序不感知」的那些名字。
2. 宿主**复刻插件文案**：`server.rs:155` 注释「与插件 `tick.rs::objective_for` 逐字等价……
   措辞本身是契约」。
3. 宿主**认识看板状态词汇**（`awaiting_merge` / `needs_you` / `merging`）并据此决策。

`2026-10-03-kanban-cards-as-visible-threads-design.md` 引入 `card_scheduler` 时，全文
**0 次**提到 Spec 4；全局 grep「放宽 / 妥协 / Spec 4」**零命中**。即：方向 B 从未被修订，
后续设计只是绕开了它。

### 1.2 为什么现在要还这笔债

把合并从「插件裸 git」改为「会话里由模型合并」时，若沿用宿主持有看板语义的现状，就必须
让宿主进一步认识「队列顺序 / 队首 / 合并名额」——**债再加一层**。

用户判据（本轮确定的设计原则）：

> 假如没有这个插件，宿主的行为是正确的吗？

正确形态：**没有插件时，宿主 = 一台普通的多会话 app-server，一切正常**。

### 1.3 相关缺陷

- `docs/bug-list.md`：合并卡因 base 工作树脏落 `needs_you` 后无入口、无原因、永久挂起。
- `docs/bug-list.md`【方向性】：合并应改为**在开发会话里由模型执行**，而非插件裸 git。

## 2. 目标与非目标

**目标**

1. 宿主不再含任何看板语义：无 `board.*` 方法名、无看板状态词汇、无看板文案复刻、
   无 `KANBAN_PLUGIN` 硬编码、无 `card_id`/`board_project` 专属字段。
2. 宿主提供**两个通用原语**：①在指定 worktree **新建会话**；②向**已有会话追加一轮**。
3. 队列、顺序、名额、状态、objective 文案全部归插件；宿主只回答「会话忙不忙」。
4. 合并改由**会话里的一轮 turn** 执行（模型合），插件继续用 **git 复核**收尾。
5. 满足判据：**没有插件时宿主行为正常**。

**非目标**

- 不动插件生命周期（建 / 删看板、daemon 监督、watchman、`board/*` RPC、`yi-agent-boards`
  crate）——见 G2 spec（`2026-10-17-plugin-lifecycle-decoupling-design.md`）。
- 不改核心 IPC 协议面（不新增 `IpcRequest` 变体）：本轮复用现有 `plugin_query` 转发。
- 不改桌面对话 UI、不改 thread/会话协议（`thread/start` / `turn/start` 语义不动）。
- 不改合并的**复核**不变量：一律以 git 事实裁定，不看模型自述。
- 不引入「模型自述成功即判成功」的路径。

## 3. 设计决策汇总

| # | 决策 | 取值 |
|---|---|---|
| D1 | 驱动方向 | **拉**：宿主每 tick 主动询问驱动插件；插件返回至多一个通用动作 |
| D2 | 转发通道 | 复用现有 `plugin_query` → daemon → 插件 socket；**不新增 IPC 变体** |
| D3 | 宿主知识面 | 只有「会话忙不忙」+ 执行两个通用动作；无看板字样 |
| D4 | 通用动作 | `launch`（新建会话）、`append`（向已有会话追加一轮），或 `null` |
| D5 | 插件的会话标识 | 插件持有 `card_id → thread_id` 映射（`thread_id` 由结果回报带回） |
| D6 | 发现口 | `yi_agent_store::resident::list`（已通用）+ 清单可选字段 `session_driver: true` |
| D7 | 合并执行者 | **会话内的一轮 turn**（模型合）；宿主不再是执行者 |
| D8 | 合并名额 | 仍每项目一把 `merge.lock`；串行由插件的卡片状态 + 名额共同保证 |
| D9 | 失败去向 | 合并轮失败 → 卡 `needs_you`，**可自动重试**（不再是无入口死胡同） |
| D10 | 复核 | 插件 `merge-base --is-ancestor <source> <base>` 为真才 `done` |
| D11 | 退役 | 插件 `merge_next()`（裸 git 自动合并）**删除**；宿主 `card_scheduler.rs` **删除** |
| D12 | 动作结果回报 | 宿主下一拍把 `{action_id, ok, thread_id?, error?}` 回传插件 |

## 4. 架构与协议

### 4.1 数据流

```
┌────────────── 宿主（通用，不认识看板）──────────────┐
│ 每 tick：对每个「声明 session_driver 的插件」：        │
│   plugin_query(plugin, "driver/next",               │
│                { threads:[…], results:[…] })        │
│   → 得到至多一个通用动作 → 无脑执行 → 下拍回报结果     │
└─────────────────────────────────────────────────────┘
        ▲ plugin_query（现有通道）      │ 执行（复用内核，不发 RPC 响应帧）
        │                              ▼
┌───────┴─────────────────┐   start_thread_core（新建会话）
│ 插件（看板语义所有者）    │   prepare_turn_core（已有会话追加一轮）
│ 队列 / 名额 / 状态 / 文案 │
└─────────────────────────┘
```

### 4.2 `driver/next` 请求（宿主 → 插件）

```jsonc
{
  "threads": [
    // 宿主为本插件启动或追加过的会话；`idle` 是宿主唯一能提供的事实。
    { "thread_id": "thread-…", "idle": true }
  ],
  "results": [
    // 上一拍动作的结果；首次为空数组。
    { "action_id": "act-17", "ok": true,  "thread_id": "thread-…" },
    { "action_id": "act-18", "ok": false, "error": "worktree not found" }
  ]
}
```

- `threads` 是宿主自身的通用记账（哪些会话是它按插件指示起的），非看板语义。
- 宿主不解释 `results` 的语义，只做搬运。

### 4.3 `driver/next` 应答（插件 → 宿主）

```jsonc
// 新建会话
{ "action_id": "act-19", "kind": "launch",
  "workdir": "/abs/worktree", "title": "看板 · <spec stem>", "objective": "<整段文案>" }

// 向已有会话追加一轮
{ "action_id": "act-20", "kind": "append",
  "thread_id": "thread-…", "objective": "<整段文案>" }

// 无事可做
{ "action": null }
```

- `objective` 由**插件**给出（宿主不再复刻文案）。
- `title` 只用于会话名片；可为空。
- 宿主对未知 `kind`：忽略并记日志，不执行、不报错（前向兼容）。

### 4.4 宿主执行语义

| 动作 | 宿主行为 |
|---|---|
| `launch` | 复用 `start_thread_core` + `prepare_turn_core` 的**无响应帧**路径（今日 `ServeLauncher::launch_inner` 已如此）：起会话、落 meta、投递 objective。成功回报 `{ok:true, thread_id}`。 |
| `append` | 对**已存在**会话复用同一条无响应帧路径的「准备 + 通知 + 投递」段（今日只对首轮实现，需抽出并支持已有 thread）。成功回报 `{ok:true, thread_id}`。 |
| 任一失败 | 回报 `{ok:false, error:"<原因>"}`。**不改插件状态**——由插件决定怎么处置。 |

**可见性**：两个动作都发 thread-keyed 通知（`turn/started`、开启项、`thread/status/updated`
的 `Running`），使桌面侧栏看得见「合并在跑」。这与「不发 RPC 响应帧」不冲突：前者是
thread-keyed 内容流，后者是 RPC 客户端应答。

**并发约束（沿用既有设计）**：driver 内联在主循环 tick 里，不与任何 RPC 分支并发
（`threads` 仍是主循环局部量，`&mut` 借用按顺序发生）。抽出的公共段必须保持这一点。

### 4.5 发现机制

- 宿主遍历 `yi_agent_store::resident::list(resident_dir)`（通用登记表，字段只有
  `project` / `required_by`）得到「需要常驻 daemon 的项目」。
- 对每个项目读 `list_installed_plugins(scope)`（现有函数），挑出清单里
  `session_driver == true` 的插件（清单新增**通用**可选字段，默认 `false`）。
- 删除 `KANBAN_PLUGIN` 常量与 `board_query`（改调通用 `plugin_query`）。

### 4.6 插件侧队列与合并（改造后的样子）

- **启动**：队列里 `queued` 且有名额且日历放行 → 返回 `launch` 动作（含 objective）。
  宿主回报 `thread_id` → 卡 `running`；失败 → 卡 `failed`（原因取 `error`）。
- **对账**：宿主回报的 `threads[].idle` 是**唯一**的「这一轮跑完了」信号。插件据此推进：
  `running` 且空闲 → `awaiting_merge`（有提交）/ `needs_you`（无提交）。
- **合并轮**（D7）：
  1. 卡 `awaiting_merge`（或 `needs_you` 重试）且其会话 **idle** → 试拿 `merge.lock`；
     拿到 → 卡 `merging`，返回 `append` 动作，objective 为「在你已检出的 base 工作树里把
     `<source>` 并入 `<base>`；冲突就地解决；不 push、不删分支」。
  2. 会话跑完（下一次 idle）→ 插件调 `merge_finish`：`merge-base --is-ancestor` 真 → `done`；
     假 → `needs_you`（**可再次入轮自动重试**，D9）。无论成败释放名额。
  3. 「base 工作树脏」不再是拒绝理由：脏/未跟踪文件的判断与善后**交给模型**（这正是
     `2026-10-03` 裸 git 两态分支结构上做不到的）。
- **objective 文案**：`objective_for` 迁回插件；实现卡首轮**保留** `Never merge yourself`
  （首轮不该合），合并轮用**另一段** objective（明确要求执行合并）。

## 5. 宿主改动明细

- **删除** `card_scheduler.rs`（803 行），新增 `session_driver.rs`：
  - 保留通用部分：tick 遍历、`ThreadFlags{idle}`、driver 侧会话记账、`run_once` 骨架。
  - 删除看板部分：`Outcome`（看板状态词汇）、`CardAction::RequestMerge`（直调 `merge_finish`）、
    `objective_for`、`LaunchRequest.card_id`/`board_project`、12 处 `board.*` 方法名。
- `server.rs`：删 `KANBAN_PLUGIN`、`board_query`、`BoardCard`、`board_cards`；
  `ServeLauncher` → `SessionLauncher`，抽出「准备 + 通知 + 投递」公共段支持 `launch` / `append`。
- `recover_orphan_cards`（启动兜底）：改为**通用**形态——宿主不认「卡片」，只管「本插件名下、
  本进程未跟踪的会话」；对账语义交插件。若无法通用化则**删除**，由插件的「崩溃恢复」自持
  （插件已有 `migrate_legacy_running`）。
- `worktree_reclaim.rs`：`card_id` / `board_project` / `kanban/<slug>` 改为通用
  `managed_worktree`（会话 meta 里的可选通用字段）。**归 G2**（见 §7）。

## 6. 插件改动明细

- `dispatch.rs`：新增 `driver/next`（服务端实现，与 `merge_request` 平行）；保留桌面端仍需的
  `list` / `enqueue` / `switch.*` / `settings.*` / `merge_*`。
- `service.rs`：
  - 删除 `merge_next()`（D11）与 `MergeClaim`；`run_daemon` 主循环去掉该调用。
  - `next_launch` / `mark_running` / `mark_terminal` / `release` **保留为内部函数**，
    改由 `driver/next` 的决策逻辑调用（宿主不再直接调这些方法名）。
  - 新增 `driver_step(threads, results) -> Option<DriverAction>`：本轮唯一的决策入口，
    合并原先散在宿主 `plan()` 里的推进规则。
- `tick.rs`：`objective_for` 拆成两段——`implementation_objective`（保留 `Never merge yourself`）
  与 `merge_objective(source, base, workdir)`。
- 清单（`scaffold.rs` 模板）：新增 `"session_driver": true`。

## 7. 切分（G1 / G2）

| 项 | 归属 |
|---|---|
| `card_scheduler.rs` → `session_driver.rs`；通用 `launch`/`append` | **G1** |
| `driver/next` 协议；插件接管推进决策；退役 `merge_next` | **G1** |
| `KANBAN_PLUGIN` / `board_query` / `BoardCard` / `board_cards` 删除 | **G1** |
| `worktree_reclaim.rs` 的 `card_id`/`kanban` 语义 | **G2** |
| `board/create\|remove\|list` RPC、`yi-agent-boards` crate、`settings_store` 的 `board_watchman_enabled`、桌面 8 处调用 | **G2**（独立 spec/plan） |

G1 完成后，宿主里**决策性**的看板语义归零；G2 再收口生命周期面。

## 8. 验收

**宿主（Rust 单测，依赖注入，不启 serve）**

1. 应答为 `launch` → 宿主调用 launcher 一次、回报 `{action_id, ok, thread_id}`。
2. 应答为 `append` → 宿主对**已有 thread** 准备一轮并投递，不新建会话。
3. 应答为 `null` → 无副作用。
4. `results` 原样上送：上一拍的 `ok:false` 出现在下一次请求的 `results` 里。
5. 未知 `kind` → 忽略且不 panic。
6. 宿主源码 grep：`board.` / `awaiting_merge` / `needs_you` / `objective_for` / `KANBAN_PLUGIN`
   在非测试代码中**归零**（编译期以「`card_scheduler.rs` 不存在」为验收）。

**插件（Rust）**

7. `driver/next` 决策：队首有序、名额受限、`queued → launch`、`running+idle → awaiting_merge`。
8. 合并轮：`awaiting_merge+idle` → `append`（objective 含 source/base）；回报成功 → 卡 `merging`；
   `merge_finish` 复核为真 → `done`、为假 → `needs_you` 且**能再次入轮**。
9. 名额：连发两次 → 第二次不出 `append`（等第一次释放）。
10. `merge_next` 已不存在（编译期）；主循环无裸 git 合并。

**端到端（人可验证）**

11. 投两张实现卡 + 两张合并卡 → 严格一前一后，任一时刻只有一个合并会话在跑。
12. 合并过程在**会话里可见**，可追问、可干预；合并期间卡显示「合并中」。
13. 让 base 工作树存在未跟踪文件 → 合并**仍然成功**（模型判断并善后），不再落 `needs_you`。
14. 卸载插件 → 宿主照常起手工会话、跑 RPC，无任何看板报错。

## 9. 涉及文件

- 宿主：`yi-agent-app-server/src/{card_scheduler.rs→session_driver.rs, server.rs, lib.rs}`
- 插件：`superpowers-kanban-runner/src/{dispatch.rs, service.rs, tick.rs, main.rs}`、
  `superpowers-kanban-core/src/{board.rs, card.rs}`（如需状态机调整）、
  `yi-agent-boards/src/scaffold.rs`（清单模板；crate 属 G2，但 G1 需要加该字段）
- 文档：本 spec、对应 plan、`docs/bug-list.md`（两条缺陷的收口）

## 10. 风险

| 风险 | 处置 |
|---|---|
| 「拉」使合并触发最多晚一个 tick（默认 3s / 插件 interval 60s） | 明确接受：合并是分钟级操作，延迟无意义 |
| `append` 的 TOCTOU（查到 idle → 追加时已忙） | 宿主 `prepare_turn_core` 本就拒绝忙会话；失败回报 `ok:false`，插件下拍重试 |
| 抽取「准备 + 通知 + 投递」公共段破坏既有借用顺序 | 单测覆盖 `launch` 既有路径零回归；driver 仍内联主循环（不新增并发） |
| 插件决策逻辑上移带来行为回归 | 逐条迁移宿主 `plan()` 的既有测试断言到插件侧（同型用例） |
| `recover_orphan_cards` 无法通用化 | 删除，由插件 `migrate_legacy_running` 自持（已有） |
