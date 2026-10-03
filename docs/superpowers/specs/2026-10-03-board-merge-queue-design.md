# 看板合并队列设计（Merge Cards + 每项目单合并闸）

日期：2026-10-03
状态：已通过设计评审，待写实现计划

## 1. 背景与问题

看板现在的卡片是「一对 spec + plan」（`core/card.rs`），由宿主
`card_scheduler` 经 `board.next_launch` 认领、起一个可见会话去实现，跑完回写
`board.mark_terminal(awaiting_merge)`，卡片停在 `AwaitingMerge`。

会话的首轮 objective 明确写着 **"Never merge yourself"**（`runner/tick.rs`
与宿主 `card_scheduler.rs` 逐字一致）。`CardState::AwaitingMerge -> Done` 这条
边（`core/card.rs`）是通往 `Done` 的唯一路径，但目前**没有任何代码写它**。

结论：**目前不存在任何自动合并通路，合并是纯人工动作**（按 `CLAUDE.md`，
由人在 worktree 外执行 `git merge --no-ff`）。这带来三个问题：

1. 完成的分支长期悬在 `AwaitingMerge`，没人负责把它合进 base，分支越飘越远；
2. 「合并」这件事在看板上不可见、不可排队、不可审计；
3. 即便将来要自动合并，也必须保证**同一个项目同时只有一个合并进程**——
   多个合并并发会互相踩踏 base 分支。

## 2. 目标与非目标

**目标**

- 新增一种卡片：**合并卡**（`kind: merge`）。它不写代码，只把源分支合进 base。
- 合并卡与实现卡共用**同一张看板**与 FIFO 顺序，但**不占**跨项目并发名额
  （它不跑会话、不消耗 provider 额度）。
- **每项目同时至多一个合并者**，做成显式不变量（每项目一把 `merge.lock`）。
- 提供两个入口：**对话中手动发起**（优先做）与**命令行/桌面按钮**。
- 会话实现卡到达 `AwaitingMerge` 时，**可自动派生**一张配对的合并卡（开关默认关）。

**非目标（本设计不做）**

- 不自动删除源分支或它的 worktree（不可逆，v1 交给人工/后续）。
- 不做跨项目合并：`source`/`base` 必须同属该项目仓库。
- 不去改会话 objective 里 "Never merge yourself" 的约束——会话仍然不许自己合并；
  合并统一走合并卡。
- 不引入「用会话去解冲突」的降级路径（冲突一律停 `NeedsYou`，等人决策）。

## 3. 术语

- **实现卡**：现有卡片，`kind = implementation`（缺省）。
- **合并卡**：新卡片，`kind = merge`，载荷是 `source`（源分支）与 `base`（目标分支）。
- **配对实现卡**：`origin_card` 指向的那张实现卡；只有自动派生时会设置。
- **基座 worktree**：为执行合并临时建的 worktree，落在
  `<project>/.worktrees/kanban-merge/<base>`，使主检出完全不被打扰。
- **合并闸**：`<state_dir>/merge.lock`，每项目一把，保证串行。

## 4. 设计决策汇总

| # | 决策 | 取值 |
|---|---|---|
| D1 | 合并卡来源 | 手动投递与自动派生**都要**；手动（对话入口）优先做 |
| D2 | 谁执行 merge | **纯本地 git 动作**（`git merge --no-ff`），不烧模型；冲突停 `NeedsYou` |
| D3 | 入哪条队列 | 与实现卡**同一张看板队列**；合并卡**不占**全局并发名额 |
| D4 | 队列顺序 | **严格 FIFO**，共用同一套 `order`，合并卡不插队 |
| D5 | 对话入口 | 既做命令/按钮入口，也做对话说人话入口，二者落到同一投递目录 |
| D6 | 合并卡载荷 | 只带 `source`/`base`，不要求 spec/plan；`base` 由投递方指定，缺省取仓库默认分支 |
| D7 | 串行方式 | 显式每项目 `merge.lock`（flock），把「每项目一个合并者」写成不变量 |
| D8 | 进行中状态 | 新增 `Merging`（不占全局名额，仅持 `merge.lock`） |
| D9 | 与实现卡联动 | 合并**成功**才把配对实现卡 `AwaitingMerge -> Done`；冲突时实现卡停在 `AwaitingMerge` |
| D10 | 合并后清理 | **不自动**删源分支/worktree（X） |
| D11 | 执行地点 | 专用基座 worktree，不碰项目主检出 |
| D12 | 自动派生位置 | 插件 `BoardService.mark_terminal`（与状态迁移同锁，原子） |
| D13 | 自动派生开关 | 项目层偏好 `board_auto_merge`，**默认关** |

## 5. 架构与数据流

延续现有分层：`core` 纯逻辑 / `runner` 做 I/O / `dispatch` + `ipc` 只转发。

```
对话说人话 ─┐
命令/按钮 ──┼─> 投递 {kind:merge, source, base} ─> <state>/inbox/ ─┐
自动派生 ───┘                                                      │
                                                                   v
            插件 tick: inbox::consume ──> 同队列入队 (Queued, kind=merge)
                                                                   │
                                          推进循环: service.merge_next()
                                                                   │
                        merge.lock ─> board.claim_next_merge ─> Merging
                                                                   │
                              基座 worktree 里 git merge --no-ff <source>
                                             │
                ┌────────────────────────────┼────────────────────────────┐
             干净                            冲突                        基础设施错
                │                            │                            │
             Done（并驱动                 NeedsYou                     Failed
           origin_card -> Done）      （--abort 复原）             （实现卡不动）
```

手动入口（本设计第一刀）：用户在对话里说一句，或走命令，投递一张合并卡到
inbox，插件 tick 消费入队，排到它时由 `merge_next` 执行。

## 6. 卡片模型与状态机（`core`）

### 6.1 卡种与字段（`core/card.rs`）

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardKind {
    #[default]
    Implementation,
    Merge,
}
```

`Card` 新增字段，全部 `#[serde(default)]` 以向后兼容旧 `board.json`：

- `kind: CardKind` —— 缺省 `Implementation`。
- `source_ref: Option<String>`、`base_ref: Option<String>` —— 仅合并卡使用。
- `origin_card: Option<CardId>` —— 自动派生时指向配对实现卡；手动投递为 `None`。

`spec_path`/`plan_path` 对合并卡保持为空串/`None`，不动既有字段语义。

### 6.2 合并卡 id（`core/card_id.rs`）

新增 `merge_card_id_for(source, base) -> String`，规则：
`merge-<slug(source)>-into-<slug(base)>`，例如
`merge-kanban-card-1-into-main`。与 `card_id_for` 并列导出，供 TUI/宿主/CLI 共用。

**同名去重**：基础 id 已被看板（含终态卡）占用时，表示「同一对 source/base 要再合一次」，
投递入口改为追加后缀 `-2`、`-3`…直到找到一个未被占用的 id 再落盘。同一 id 重复投递仍
保持现有幂等语义（同 id 覆盖投递文件、已在看板上则不重复入队）。

### 6.3 状态机（`core/card.rs`）

- 新增 `CardState::Merging`。
- `occupies_slot()` **不变**：只有 `Running` 占全局名额，`Merging` 不占。
- 新增迁移：`(Queued, Merging)`、`(Merging, Done)`、`(Merging, NeedsYou)`、
  `(Merging, Failed)`。
- `AwaitingMerge -> Done` 既有边保持，唯一驱动者是「合并成功」。

### 6.4 同一队列、两种选取（`core/board.rs`）

- `claim_next_launch(limit)`：**跳过 `kind == Merge`** 的卡——合并卡永不由会话通路认领。
- 新增 `claim_next_merge(merge_busy: bool) -> Option<CardId>`：
  `merge_busy == true` 直接 `None`；否则取 `order` 最小、`Queued` 且 `kind == Merge`
  的卡，置 `Merging`。
- 合并卡不参与 `free_slots`，实现卡启动不受合并卡影响，反之亦然。

### 6.5 校验（`core/promotion.rs`）

新增 `validate_merge(source, base, project_root)`：

- `source`/`base` 非空；
- `source != base`；
- `source` 分支在仓库中存在（`git rev-parse --verify <source>`）。

不要求 spec/plan。实现卡仍走原 `validate_promotion`。

## 7. 合并执行引擎（`runner`）

### 7.1 每项目合并锁

新增 `runner/merge_lock.rs`（或并入 `single_instance.rs`）：路径
`<state_dir>/merge.lock`，`flock(LOCK_EX|LOCK_NB)`，合并全程持有；拿不到即本轮无进展。
理由与 `plugin.lock` 相同：内核在进程退出时释放（含 `SIGKILL`），不会占死闸门；
且锁在 `drop` 时**不 unlink** 文件。

### 7.2 基座 worktree

- 路径 `<project>/.worktrees/kanban-merge/<base-slug>`，幂等创建
  （`git worktree add <path> <base>`）。
- 启动迁移时若发现遗留的 `Merging` 僵尸，best-effort 在该 worktree 里
  `git merge --abort` 复原后再重试。

### 7.3 合并与分类（`runner/merge.rs`）

在基座 worktree 里执行 `git merge --no-ff <source>`：

| 结果 | 判定 | 卡状态 | 配对实现卡 |
|---|---|---|---|
| 干净合入 | 退出 0 | `Done` | `AwaitingMerge -> Done` |
| 冲突 | 退出非 0 且检测到冲突 | `NeedsYou`（先 `merge --abort` 复原） | 停在 `AwaitingMerge` |
| source 不存在 / base worktree 脏 | 前置校验失败 | `NeedsYou` | 不动 |
| git 无法启动等 | spawn/IO 失败 | `Failed` | 不动 |

冲突一律先 `--abort` 再落状态，保证基座 worktree 事后干净、可重试。

### 7.4 服务层（`runner/service.rs`）

- 新增 `merge_next()`：拿 `merge.lock` → `board.claim_next_merge(merge_busy)` →
  执行 → 回写终态 → 若 `origin_card` 有值且合并成功，驱动它 `-> Done`。
- 合并卡**不走** `next_launch` / `mark_running`，与全局槽位记账完全解耦。
- 推进循环在 `consume_inbox` 之后调用一次 `merge_next()`。**受看板总开关
  （`superpowers_kanban`，`on`/`off`）约束**：总开关关闭时既不推进、也不合并；
  `board_auto_merge` 只控制「是否自动派生合并卡」，与总开关正交。
- `mark_terminal(awaiting_merge)` 时，若 `board_auto_merge` 打开，就地派生一张合并卡
  （`origin_card` = 该卡，`source` = `kanban/<slug>`，`base` = 默认分支），
  与状态迁移在同一把锁内，原子。

### 7.5 崩溃恢复（扩展 `migrate_legacy_running`）

- 启动时对残留 `Merging` 僵尸卡：best-effort `git merge --abort`，卡置 `NeedsYou`
  （不是 `Failed`：合并可能半途，需人确认），并确保 `merge.lock` 可再取。

## 8. 入口与投递

### 8.1 投递格式（`core/inbox.rs`）

- 实现卡不变：`{"id","spec_path","plan_path"}`。
- 合并卡：`{"id","kind":"merge","source":"...","base":"...","origin_card":"<可选>"}`。
- `inbox::consume` 按 `kind` 分派校验：`merge` 走 `validate_merge`，缺省走
  `validate_promotion`；拒绝件照旧归档 `inbox/rejected/`。

### 8.2 入口 1：命令行（`runner/main.rs`）

`superpowers-kanban add-merge <source> [--base <ref>] [--state-dir <d>]`

- 与 `add` 同款：先校验（source 分支存在、source ≠ base），失败立刻非零退出并打印原因；
  成功打印 `delivered <card-id> ...`。
- base 缺省取仓库默认分支。

### 8.3 入口 2：对话说人话（优先做）

- 扩展 `superpowers-kanban` skill：识别「把 X 合进 Y / 合并这个分支」这类意图，
  转成一次 `add-merge` 投递，并如实复述卡 id 与队列位置。
- 保留 skill 的职责边界：「只入队、不代跑」。

### 8.4 入口 3：桌面/TUI（后续）

- 新 RPC 方法 `enqueue_merge`（params `{source, base}`），与现有 `enqueue` 平行放进
  `dispatch`；按钮接上去即可。不阻塞第一刀。

## 9. 对接口与宿主接线

- 插件 `list`：每张卡补 `kind`、`source`、`base`、`origin_card`（实现卡相应为 `null`）。
  仍**不导出 `order`**（与现状一致）。
- 插件 `dispatch`：新增 `enqueue_merge`，与 `enqueue` 平行；实现卡 `enqueue` 不变。
- 宿主 `BoardCard`（`app-server/src/server.rs`）：增加可选 `kind`/`source`/`base`，
  `board_cards` 解析时读取；解析仍「缺 id/state 才跳过」，合并卡因有 id/state 被保留。
- 宿主 `card_scheduler`：**基本不动**。`plan()` 只对 `running + thread_id + tracked`
  动作，合并卡从不进 `running`；`recover_orphan_cards` 只看 `running` 无 thread_id。
  唯一要求：`Merging` 不被当成 `running` 类处理（已确认不会）。
- 默认 base 解析（`runner`，git I/O）：`git symbolic-ref --short refs/remotes/origin/HEAD`
  去前缀，失败退化 `main`；显式传 `base` 时以传值为准。
- 桌面 `parseBoard`（`desktop/src/lib/superpowersKanbanState.ts`）：跳过条件从
  「`plan_path` 为空就丢」放宽为「`id`/`state` 为空才丢」，合并卡 `detail` 渲染为
  `<source> → <base>`。

## 10. 测试与验收

**core（纯逻辑）**

- `CardKind` 默认与 `#[serde(default)]` 向后兼容：旧 `board.json` 读回 `Implementation`。
- 状态机：`Queued->Merging->{Done,NeedsYou,Failed}` 合法；`Merging` 不占名额。
- `claim_next_launch` **跳过**合并卡（关键回归：混排队列里合并卡永不被会话通路认领）。
- `claim_next_merge(true)` 为 `None`；`false` 时取 `order` 最小的合并卡。
- 合并卡 FIFO，不插队。
- `merge_card_id_for` 规则 + 去重后缀。
- `validate_merge`：空 ref / source==base / source 不存在 → 拒绝；正常对通过。

**runner（真实 git，临时仓库）**

- 干净合并：两分支分叉 → `merge_next` → 卡 `Done`、base 出现合并提交、
  `origin_card` 由 `AwaitingMerge -> Done`。
- 冲突：同一行冲突 → `Merging -> NeedsYou`，基座 worktree 经 `--abort` 复原
  （`git status` 干净），实现卡仍在 `AwaitingMerge`。
- 串行闸：连调两次 `merge_next`，第二次在第一次持锁时无进展；子进程持锁被 `SIGKILL`
  后锁可回收（抄 `single_instance` 现有测法）。
- 崩溃恢复：留下 `Merging` 僵尸的 `board.json` → 启动迁移后变 `NeedsYou`，
  `merge.lock` 可再取。
- 主检出不被污染：全流程跑完，`git -C <project> status` 干净、仍停在原分支。
- inbox：`kind:merge` 投递被消费入队；缺 ref 的投递进 `inbox/rejected/`。

**dispatch / IPC**

- `enqueue_merge` 校验失败不落盘；成功返回 `{id}` 且 inbox 出现该文件。
- `list` 对合并卡输出 `kind/source/base`，实现卡相应字段为 `null`。

**宿主（app-server）**

- `board_cards` 能解析合并卡的 `kind/source/base`；缺字段退化为实现卡语义。
- 回归：`Merging` 卡不被 `card_scheduler::plan` 或 `recover_orphan_cards` 当 `running`。

**desktop**

- `parseBoard`：`plan_path` 为空的合并卡不再被丢弃，`detail` 渲染 `<source> → <base>`；
  实现卡行为回归不变。

**验收口径（人可验证）**

1. 对话里说「把 `kanban/card-1` 合进 `main`」→ `list` 出现一张排队合并卡；跑完 base 上
   多一个 `--no-ff` 合并提交，实现卡 `Done`。
2. 故意制造冲突 → 合并卡 `NeedsYou`、实现卡仍 `AwaitingMerge`、基座 worktree 干净可重试。
3. 同项目连投两张合并卡 → 严格一前一后，任一时刻只有一个在跑。

## 11. 未决 / 后续

- 合并后清理（删源分支/worktree）：v1 明确不做（D10），需要时另开。
- 用会话解冲突的降级路径：不做，冲突一律 `NeedsYou`。
- 桌面/TUI 的合并按钮（入口 3）：接口已留，接 UI 即可。
- 自动派生开关 `board_auto_merge` 打开后，`AwaitingMerge` 卡应能自动产生合并卡——
  需要一条端到端测试覆盖「自动派生 + 执行 + 驱动 Done」。
