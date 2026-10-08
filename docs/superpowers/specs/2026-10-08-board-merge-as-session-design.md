# 看板合并改为会话驱动 + 侧栏摘要修复 Design

日期：2026-10-08
状态：设计已确认（四段逐一 review 通过），待用户 review 本文件后进入 writing-plans。
范围：桌面端（`desktop/`）、插件（`plugins/superpowers-kanban/`）、宿主（`yi-agent-rs/crates/yi-agent-app-server/`）。

## 1. 背景与问题

用户反馈两件事：

1. **桌面上看不到看板进度。** 侧栏那一行「看板」看起来始终「什么都没在跑」，合并卡卡住了也无从得知。
2. **合并工作应该能被对话管理。** 合并应当发生在**原卡片的会话**里（没有会话才新建），
   四列看板**复用原卡片**而不是新增一行；同一时间段只允许一个会话在合并。

### 1.1 三处已确认的桌面缺陷（证据）

实测已证后端链路完全正常：经已安装的 app-server 发 `plugin/query`（plugin=superpowers-kanban,
method=list）返回全部 5 张卡（3 张 `awaiting_merge` + 2 张合并卡 `needs_you`），
`switch.read` 返回 `{"on":true,"source":"project"}`。问题全在桌面端呈现层：

- **硬伤 1（摘要口径）**：`desktop/src/lib/boardIndex.ts::summarize()` **只数**
  `queued` 与 `running`。`needs_you`/`awaiting_merge`/`merging` 一律不进摘要 →
  上述 5 张卡算出来是 `"0 排队 · 0 运行中"`，看起来「没反应」。
- **硬伤 2（摘要不刷新）**：`desktop/src/App.tsx` 每 2 秒的 `boardTick` 只调
  `refreshSelectedBoard()`（刷**已打开**的看板卡片）；更新侧栏摘要的 `refreshBoards()`
  只在握手时调用。不点进看板，侧栏摘要就停在上次连接时的值。
- **硬伤 3（卡片不显示合并信息）**：`desktop/src/components/SuperpowersKanbanCard.tsx`
  只渲染 `title`/`state`/`threadId`，不渲染 `source`/`base`。
  `superpowersKanbanState.ts` 已为合并卡算出 `detail = "<source> → <base>"`，但卡片没用它。
  合并卡又无 `thread_id`，连「点进去看详情」的退路都没有。

## 2. 目标与非目标

**目标**

1. 修复上述三处桌面缺陷：摘要口径、摘要刷新、合并卡信息展示。
2. 合并改为**会话驱动**：由卡片**原会话**里的一轮 turn 执行；无会话时新建。
3. 合并是**原卡片的一个阶段**：四列看板复用原卡，不新增行；状态机新增
   `awaiting_merge → merging → {done | needs_you}`。
4. **流量控制**：同一项目同一时刻**至多一个合并轮**（沿用 `merge.lock`，每项目一把）。
5. 合并**手动触发**：卡停在 `awaiting_merge` 不动；用户在会话里说人话，经 skill
   向看板**申请名额**，拿到才动手，忙则排队。

**非目标**

- 不自动 push；不自动删源分支/worktree（沿用既有决定）。
- 不改全局并发日历：合并轮**不占**全局任务名额（`occupies_slot()` 仍只认 `Running`）。
- 不新增「模型自述成功即判成功」的路径——一律以 git 复核为准。
- 不重做四列视图本身（列映射与卡片内容另有改动点，见 §7）。
- 存量 `kind=merge` 独立卡不迁移、不重写（按原逻辑收尾）。

## 3. 设计决策汇总

| # | 决策 | 取值 |
|---|---|---|
| D1 | 合并由谁执行 | **会话驱动**：原会话追加一轮，agent 执行 `git merge --no-ff` 并解冲突 |
| D2 | 合并的载体 | **原卡片的一个阶段**（不新增看板行）；无原会话才新建会话 |
| D3 | 触发方式 | **手动**：卡停 `awaiting_merge`；用户会话内说人话经 skill 申请名额 |
| D4 | 名额作用域 | **每项目一个**（`merge.lock`）；合并轮不占全局名额 |
| D5 | 合并 git 落点 | **专用合并 worktree** `<项目>/.worktrees/kanban-merge/<slug>`，不碰主检出 |
| D6 | 失败去向 | 一律 `needs_you`（**无** `merging → failed` 自动边），等用户回会话续 |
| D7 | 成功判定 | **git 复核**：`git merge-base --is-ancestor <source> <base>` 为真才 `done` |
| D8 | 名额释放 | 该轮结束（无论成败）立即释放 `merge.lock` |
| D9 | 重启兜底 | 卡在 `merging` 但会话不再跑 → 退回 `needs_you`，不自动重试 |
| D10 | 会话内说人话 | `needs_you` + 用户再说话 → **重新申请名额**，不直接回 `running` |
| D11 | 桌面摘要口径 | 同时统计 `running` / `needs_you` / `awaiting_merge`+`merging` |
| D12 | 摘要刷新 | 并入 2 秒轮询（与看板卡片同拍） |

## 4. 状态机与名额

### 4.1 状态迁移

复用现有 `CardState::Merging`（`core/card.rs`）。原文中 `Merging` 属于合并卡；
本设计把它改归**实现卡**的合并阶段，语义不变（不占会话槽位）。

```
awaiting_merge ──► merging ──► done
                       └─────► needs_you ──► merging   （再次申请名额即重试）
```

- 新增/放行迁移：`(AwaitingMerge, Merging)`、`(Merging, Done)`、`(Merging, NeedsYou)`、
  `(NeedsYou, Merging)`。
- `occupies_slot()` **不变**：仍只有 `Running` 占会话槽位；`Merging` 不占。
- **移除** `(Merging, Failed)` 这条自动边（D6）；`Failed` 只保留给显式基础设施错误路径
  （本设计不从合并轮自动进入）。

### 4.2 合并名额

- 名额 = 每项目一把 `merge.lock`（复用 `runner/merge_lock.rs::acquire`，flock 语义不变）。
- 「一次一个」的范围是**每项目**：一个项目就是一个仓库，跨项目合并不会踩同一个 base。
- 合并轮**不占**全局并发名额（§2 非目标）。

### 4.3 一次合并轮的流程

1. 用户在卡片**原会话**里说「合并 / 把这条合进 main」。
2. 会话里的 agent 经 skill 调 `superpowers-kanban merge-request <card_id>`。
3. 插件：校验卡处于 `awaiting_merge`/`needs_you` → 试拿 `merge.lock`
   - 拿不到 → 返回 `busy`（卡不动，排队；由会话里的 agent 如实转述「本项目已有合并在跑」）
   - 拿到 → 卡 `→ merging`，`merge::prepare()` 建好专用 worktree，
     返回 `granted` + 合并 worktree 路径 + `source`/`base` + 指令
4. agent 在该 worktree 里 `git merge --no-ff <source>`，冲突就地解决。
5. 该轮结束 → agent 调 `superpowers-kanban merge-finish <card_id>`（或由宿主对账代调）。
6. 插件**先 git 复核**再落状态：
   - `merge-base --is-ancestor <source> <base>` 为真 → `merging → done`
   - 否则 → `merging → needs_you`
7. 无论成败，**释放 `merge.lock`**。

## 5. 命令与接口

### 5.1 插件 CLI（`runner/main.rs`）

- `superpowers-kanban merge-request <card-id> [--state-dir <d>]`
  返回三态：`granted` / `busy` / `denied`（附原因）。`granted` 时打印合并 worktree 路径。
- `superpowers-kanban merge-finish <card-id> [--state-dir <d>]`
  做 git 复核并落 `done`/`needs_you`，释放名额。

### 5.2 插件 dispatch（`runner/dispatch.rs`）

新增 `merge_request`、`merge_finish` 两个方法（与既有 `enqueue_merge` 平行），
供宿主与 CLI 共用同一实现。

### 5.3 会话内 skill

在 `plugins/superpowers-kanban/skills/superpowers-kanban/` 扩展（安装到 `~/.yi-agent/skills/`）：
- 识别「合并 / 把 <分支> 合进 <base>」意图；
- **唯一动作**是调 `merge-request` 申请名额；拿到才按返回的 worktree 路径执行合并；
- 明确写入「不得绕过名额直接 `git merge`」；
- 轮次结束时调 `merge-finish`。

### 5.4 宿主（`card_scheduler.rs` / `server.rs`）

- 对账新增：`merging` 卡所在会话空闲且本轮由本进程发起 → 调 `merge_finish` 复核落状态。
- 重启兜底（D9）：启动时见 `merging` 但对应会话不在跑 → 退回 `needs_you`，不自动重试。
- `needs_you` 收窄（D10）：卡在 `needs_you` 且用户再说话 → 走**重新申请名额**，
  不直接回 `running`。原有「`awaiting_merge` + 会话忙碌 → 回 `running`」不变。

### 5.5 存量合并卡

`kind=merge` 独立卡的 `merge_next()` 执行路径**退役**（不再由插件自行跑 git）。
存量卡不迁移：仍按原逻辑 `claim_next_merge` + 本地合并收尾，避免留下无人认领的卡。

## 6. 桌面端改动

### 6.1 修三处硬伤

- `boardIndex.ts::summarize()`：改为统计 `running`、`needs_you`、`awaiting_merge`+`merging`
  三类，输出形如 `"3 运行中 · 2 待合并 · 1 待处理"`（空看板仍为 `"空"`）。
- `App.tsx`：把 `refreshBoards()` 并入 2 秒轮询，使侧栏摘要随看板变化刷新。
- `SuperpowersKanbanCard.tsx`：`kind === "merge"` 时渲染 `source → base`（用已有 `detail`）。

### 6.2 四列看板与状态文案

- 列映射：`merging` → `DOING`；`awaiting_merge` → `NEED DECISION`（沿用现有映射）。
- 状态文案：`merging` → 「合并中」。
- **不做**「等待合并名额」这类文案：桌面拿不到「本项目合并名额是否被占用」的事实
  （无对应 RPC），凭空显示会误导。名额忙的真相只在用户发起合并时由 `merge-request`
  返回 `busy`，由会话里的 agent 如实转述。

### 6.3 侧栏

- 合并发生在**原会话**里，故侧栏「看板会话」小节**无需新增条目**：原会话即入口。
- 摘要按 §6.1 的口径更新。

## 7. 测试与验收

**桌面（vitest）**

- `summarize()`：`running`/`needs_you`/`awaiting_merge`/`merging` 各自计入对应类；空 → `"空"`。
- `App`：摘要随轮询刷新（mock 两次不同数据，断言第二次生效）。
- `SuperpowersKanbanCard`：`kind=merge` 渲染 `source → base`；实现卡不渲染该行。
- 列映射：`merging` 落 `DOING`。

**插件（Rust）**

- `merge-request`：卡不在可合并态 → `denied`；已被占用 → `busy`；成功 → `granted` 且卡 `merging`。
- `merge-finish`：git 复核为真 → `done`；为假 → `needs_you`；两者都释放名额。
- 名额：连发两次 `merge-request`，第二次 `busy`；finish 后再发可 `granted`。
- 状态机：`(AwaitingMerge,Merging)`/`(Merging,Done)`/`(Merging,NeedsYou)`/`(NeedsYou,Merging)` 合法；
  `Merging` 不占会话槽位。

**宿主（Rust）**

- 对账：`merging` 卡会话空闲 → 调 `merge-finish`。
- 兜底：`merging` 无在跑会话 → 退 `needs_you`。
- D10：`needs_you` + 再说话 → 申请名额而非回 `running`。

**验收口径（人可验证）**

1. 侧栏「看板」摘要显示真实计数（含待合并/待处理），且不点进去也会随时间刷新。
2. 合并卡卡片上能看到 `source → base`。
3. 在原实现卡会话里说「合并」→ 卡进「合并中」；同项目此时再触发一张 → 排队等名额。
4. 合并成功 → 卡 `done`；冲突或未解完 → 卡停「合并待处理」，回会话接着说可重试。

## 8. 涉及文件

- `desktop/src/lib/boardIndex.ts`（`summarize` 口径）+ `.test.ts`
- `desktop/src/App.tsx`（摘要并入轮询）
- `desktop/src/components/SuperpowersKanbanCard.tsx`（合并信息）+ `.test.tsx`
- `desktop/src/lib/superpowersKanbanBoard.ts`（`merging` 列映射）+ `.test.ts`
- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`（迁移表）
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/{dispatch,service,merge,merge_lock,main}.rs`
- `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`（会话内入口）
- `yi-agent-rs/crates/yi-agent-app-server/src/{card_scheduler,server}.rs`

## 9. 风险

| 风险 | 处置 |
|---|---|
| 合并轮烧模型但不算限流名额 | 明确接受（D4）；每项目 1 个名额已限定规模 |
| 会话驱动导致「假成功」 | 一律 git 复核（D7），不看模型自述 |
| `needs_you` 语义被改（D10）影响既有对账 | 单测覆盖原 `awaiting_merge` 路径零回归 |
| 存量 `kind=merge` 独立卡被误改 | 明确不迁移、原逻辑收尾（§5.5） |
