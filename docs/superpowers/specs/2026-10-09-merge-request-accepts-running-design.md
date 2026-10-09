# 合并名额接受 `running` 卡 Design

日期：2026-10-09
状态：设计已确认（用户拍板走方案 A），待用户 review 本文件后进入 writing-plans。
范围：插件 `plugins/superpowers-kanban/`（`superpowers-kanban-core` + `superpowers-kanban-runner`）。不改宿主、不改桌面、不改协议。

## 1. 背景与问题

### 1.1 现象

一张实现卡停在 `awaiting_merge`。用户在它的**原会话**里说「加到看板合并队列 / 合并这张卡」，
会话开始跑这一轮合并 turn；此时卡片状态却变成 `running`，随后 skill 调
`superpowers-kanban merge-request <card-id>` 被**拒绝**，卡进不了合并队列。

### 1.2 根因（有代码证据）

触发翻转的是**合并这一轮自己的会话活动**，即合并动作把自己踢出了可合并状态：

1. **翻转来源**：`yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs:110-119`

   ```rust
   TrackState::AwaitingMerge => {
       if t.idle { return None; }
       Some(CardAction::Reconcile { outcome: Outcome::Running })   // awaiting_merge 且会话忙 → 翻回 running
   }
   ```

   宿主对账只认「我曾判成 `awaiting_merge` 的卡，其 thread 又忙了」→ 回写 `board.mark_running`。

2. **放行边**：`plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs:98-99`
   放行 `AwaitingMerge → Running`、`NeedsYou → Running`。该边来自 commit `23846506`
   （修「运行中的卡被误显示为 `awaiting_merge`」），修复本身语义正确。

3. **合并闸只认两个状态**：`plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs:484`

   ```rust
   if !matches!(card.state, CardState::AwaitingMerge | CardState::NeedsYou) {
       return Ok(MergeRequest::Denied(format!(
           "card {card_id} is {:?}, not awaiting_merge/needs_you", card.state)));
   }
   ```

   于是卡一旦被翻成 `running`，`merge-request` 必然 `Denied`。

### 1.3 为什么不是「实现还没写完就翻走了」

`23846506` 修的正是「单次空闲快照被当成终态」。它在**实现阶段**是对的：会话还没写完时，
不该长期显示 `awaiting_merge`。但设计 `2026-10-08-board-merge-as-session`（D3）已把合并改为
**手动、会话驱动**：用户在 `awaiting_merge` 的卡上发话，会话开跑合并这一轮——**同一张卡**。
于是旧的显示修复与新的合并闸直接打架。

## 2. 目标与非目标

**目标**

1. 卡处于 `awaiting_merge` 时，用户发话触发的合并轮（会话变忙、卡被翻成 `running`）**不得**
   让这张卡无法申请到合并名额。
2. 合并名额的准入改为「用户明确要求 + source 分支存在 + 每项目一把 `merge.lock`」，
   不再要求卡必须停在 `awaiting_merge`。
3. 保持 `23846506` 的显示语义零回归：实现阶段会话忙碌仍应把卡翻回 `running`。
4. 保持「一律 git 复核」：合并是否成功仍只看 `merge-base --is-ancestor`，不看模型自述。

**非目标**

- 不新增「合并队列 / awaiting merge slot」这类新状态（用户已否决方案 B）。
- 不改宿主的 `plan` 对账语义（不改翻转条件），不改宿主代码。
- 不改桌面端、不改插件 IPC 协议、不新增 CLI 子命令。
- 不自动 push、不自动删源分支/worktree（沿用既有决定）。
- 不改全局并发日历：`occupies_slot()` 仍只认 `Running`。
- 不清理 `thread_id`、不改会话生命周期（已核实非必要，见 §4.4）。

## 3. 设计决策汇总

| # | 决策 | 取值 |
|---|---|---|
| D1 | 名额准入状态 | `merge_request` 接受 `AwaitingMerge` / `NeedsYou` / **`Running`**；`Merging` 仍拒绝（已占名额） |
| D2 | 状态机 | 放行 `(Running, Merging)`；`(Merging, Running)` **保持拒绝**（合并阶段不回流会话通路） |
| D3 | 名额的持久载体 | 仍是卡片状态 `Merging`——一旦 `granted`，第二次申请因卡在 `Merging` 而 `Denied`，锁只用于判定不跨调用持有 |
| D4 | 拒绝文案 | 从 `not awaiting_merge/needs_you` 改为 `not awaiting_merge/needs_you/running`（如实） |
| D5 | 不新增失败边 | 合并失败一律落 `needs_you`（沿用 `2026-10-08` D6）；`(Merging, Failed)` 仅存量 `kind=merge` 卡仍用 |

## 4. 状态机与名额

### 4.1 状态迁移（`core/card.rs`）

现状允许：`(AwaitingMerge, Merging)`、`(NeedsYou, Merging)`。新增一条：

```rust
(Running, Merging) => true,
```

- `(Merging, Running)` **仍然为假**：合并阶段绝不回流会话通路。
- `occupies_slot()` 不变：`Merging` 不占会话槽位。
- 这条边的语义是「用户在这张**正在跑**的卡上发话要求合并」——不是自动路径，
  因为 `merge_request` 只由 CLI/skill 触发（宿主只会调 `merge_finish`）。

### 4.2 名额申请（`runner/service.rs::merge_request_locked`）

准入从

```rust
if !matches!(card.state, CardState::AwaitingMerge | CardState::NeedsYou) { Denied }
```

放宽为

```rust
if !matches!(
    card.state,
    CardState::AwaitingMerge | CardState::NeedsYou | CardState::Running
) { Denied }
```

并把拒绝文案改为 `not awaiting_merge/needs_you/running`（D4）。其余不变：
`source = kanban/<slug>`（实现卡）或自带 refs（合并卡）；source 分支必须存在；
`merge::prepare` 定位 base worktree；成功后 `transition(id, Merging)`。

### 4.3 一次合并轮的时序（修复后的实际路径）

1. 卡停在 `awaiting_merge`。
2. 用户在原会话发话 → 会话开跑合并这一轮 → 宿主的下一拍对账把卡翻成 `running`。
3. skill 调 `merge-request`：卡 `running` 已在准入内 → `granted`（卡 → `Merging`），
   拿到 base worktree 路径。
4. agent 在该 worktree 里 `git merge --no-ff`，冲突就地解决。
5. 该轮结束：skill 调 `merge-finish`，或宿主在会话空闲时代调 `merge-finish`（二者幂等）。
6. 插件先 git 复核再落状态：
   - `merge-base --is-ancestor <source> <base>` 为真 → `Merging → done`
   - 否则 → `Merging → needs_you`

### 4.4 为什么不需要清理 `thread_id`（已核实）

`granted` 之后卡进入 `Merging`，此后宿主的对账有两个收敛保证，无需额外改动：

- **卡在 `merging` 且会话忙碌**：`plan` 走 `if card.state == "merging"` 分支，`!t.idle` → 返回
  `None`，不产出动作（`card_scheduler.rs:81-91`）。
- **卡在 `merging` 且会话空闲**：`plan` 产出 `RequestMerge`；`run_once` 代调 `merge_finish` 后
  **立即 `tracked.remove(&card_id)`**（`card_scheduler.rs:294-306`），此后该卡不再被翻转。

另外，`recover_orphan_cards` 把「`running` 且**无** `thread_id`」的卡判为 `needs_you`
（`server.rs:6162`）。合并轮的收尾发生在 `merging → done|needs_you`，卡最终不是 `running`，
故这条兜底不会误伤；反过来若去清 `thread_id`，只会平添一处与重启恢复耦合的行为，收益为零。

### 4.5 并发与竞态

- **同一卡二次申请**：第一次 `granted` 后卡在 `Merging`，第二次因不在准入集而 `Denied`
  （`service.rs:1236` 既有用例已锁住）。
- **跨卡**：`merge.lock`（flock）保证每项目同一时刻只有一个合并轮；拿不到即 `Busy`，不排队。
- **插件收敛扫描相撞**：`reconcile_merged` 只扫 `awaiting_merge` 实现卡；合并成功后卡是
  `done`（非候选）或 `needs_you`（本就非候选），不会互踩。

## 5. 命令行契约不变

`merge-request` / `merge-finish` 的入参与输出形状不变（`granted` / `busy` / `denied`；
`done` / `needs_you` / `cleared`）。skill 文案无需改：既有「busy → 如实说排队中」、
「denied → 原样转述」仍然成立。**唯一变化**是 `denied` 的触发条件收窄——不再因为卡在
`running` 而拒绝。

## 6. 测试与验收

**core（纯单测，`card.rs`）**

- `(Running, Merging)` 合法。
- `(Merging, Running)` **不合法**（合并阶段不回流会话）。
- `(AwaitingMerge, Running)` 仍合法（`23846506` 显示语义零回归）。

**runner（真实 git 仓库，`service.rs`）**

- 一张 `running` 的实现卡（有 thread_id）+ 存在的 source 分支 → `merge_request` 返回
  `Granted`，卡转 `merging`；随后第二次申请 `Denied`。
- 一张 `queued` 的卡 → 仍 `Denied`（准入只放宽到 `running`，不是全放）。
- 既有 `merge_request_denies_a_card_that_is_not_waiting_to_merge`（queued 卡）保持通过。

**宿主（`card_scheduler.rs`）**

- 零实现改动；既有「`awaiting_merge` 且会话忙 → 翻回 `running`」用例
  （`a_card_awaiting_review_is_revived_when_its_thread_runs_again`）作为 `23846506` 的回归锁
  保持通过。

**验收口径（人可验证）**

1. 拿一张真实 `awaiting_merge` 实现卡，在其会话里说「合并」：卡进「合并中」（DOING 列），
   不再被 `denied`。
2. 合并成功 → 卡 `done`；冲突/未落地 → 卡 `needs_you`，回会话接着说可再次申请名额重试。
3. `cd plugins/superpowers-kanban && cargo fmt --all && cargo test` 通过；
   `cd yi-agent-rs && cargo test -p yi-agent-app-server card_scheduler` 通过。

## 7. 涉及文件

- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`
  （新增 `(Running, Merging)` 迁移 + 用例）
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
  （`merge_request_locked` 准入放宽 + 文案；用例）
- `docs/project-management/yi-agent-app-server.md`（新增条目，与实现同 commit）

## 8. 风险

| 风险 | 处置 |
|---|---|
| 放宽准入后「没写完的卡」也能被合并 | 准入仍要求 source 分支存在；且只有用户显式发话（CLI/skill）才会调 `merge_request`，宿主从不自动调它 |
| 卡在 `running` 时被合并，槽位记账 | `merge_request` 只改状态、不动 `leases`；卡离开 `Running` 后本就不占会话槽位（`occupies_slot` 只认 `Running`） |
| `23846506` 显示语义回归 | 以既有对账用例 + core 迁移用例双向锁住 |
| 宿主在 `merging` 且会话恰逢空闲时过早收尾 | 既有设计（`2026-10-08` D7）以 git 复核为准：真合并才算 `done`，否则 `needs_you` 可重试；本次不改该语义 |
