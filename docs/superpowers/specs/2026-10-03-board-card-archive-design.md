# 看板卡片归档与清理 Design

日期：2026-10-03
状态：设计已定稿，待用户 review 后进入 writing-plans。

## 1. 背景与问题

看板卡片目前**只进不出**：

- `board.rs` 的 `Board` **没有任何移除/归档 API**（`grep` 无 `retain/remove/purge/archive`）；
- 插件 CLI 只有 `run|add|add-merge|list|on|off|workdir`（`main.rs:77` 的 `USAGE`）；
- `command_list`（`main.rs:288`）把**所有非 `queued` 的卡都列出来**，含 `done`/`failed`。

加上一个结构性断点：**「人工合并后如何结算」没有通路**。

- 会话的 objective 明确写着 *"Never merge yourself"*（`card_scheduler.rs` 与插件
  `tick.rs` 逐字一致），所以实现卡跑完只会停在 `awaiting_merge`；
- 已有的**合并卡**（`CardKind::Merge`）执行前先查
  `merge::source_branch_exists`（`merge.rs:45`）；分支已被删除时直接报
  `source branch '…' does not exist`，**无法**把源卡带到 `done`；
- `AwaitingMerge -> Done`（`card.rs:92`）是通往 `done` 的唯一迁移，但目前**没有代码写它**。

后果（实测）：在一个项目上手工合并分支后，卡片永远挂在 `awaiting_merge`，越积越多，
既不可结算也不可清理。本机当前就有三张这样的僵尸卡（`board-smoke`、
`kanban-thread-grouping`、`board-merge-queue`，其分支均已删除、工作均已进 `main`）。

## 2. 目标与非目标

**目标**

- 提供**两段式清理**：默认**归档**（隐藏、可恢复）+ 显式 `purge`（真删、不可逆）。
- 提供**人工合并后的结算通路**：一个 `done <card-id>` 动词，把 `awaiting_merge` 放行为 `done`。
- **真终止态自动归档**：`done`/`failed`/`cancelled` 过宽限期（默认 24h）自动归档，板子自己变干净；
  **需人决策的状态（`awaiting_merge`/`needs_you`）永不自动归档**。
- 让 **superpowers-kanban skill** 负责「你说已合并 → 帮你更新看板进度」。

**非目标**

- 不自动删除源分支或 worktree（不可逆，仍交人工）。
- 不提供「一键清空整板」；`purge` 只允许删**已归档**的卡。
- 不改会话 objective 的 "Never merge yourself" 约束。
- 不引入「用会话解冲突」的降级路径。
- 不做跨项目的归档视图（YAGNI）。
- 不新增 `Card.branch` 字段：实现卡分支按 id 推导（`kanban/<slug>`，`service.rs:129`），
  结算是**核对**而非权威记账。

## 3. 术语与既有事实

- **实现卡**（`CardKind::Impl`）：一对 spec+plan，起会话开发，跑完 `awaiting_merge`。
- **合并卡**（`CardKind::Merge`）：不改代码，把 `source_ref` 合进 `base_ref`。
- **归档**（archived）：隐藏但不删除；`list`/桌面默认不显示。
- **purge**：从 `board.json` 物理移除，不可逆。
- 卡片生命周期用 `Card.state: CardState`（`card.rs:35`）表示；合法迁移见 `card.rs` 的
  `can_transition_to`，其中 `AwaitingMerge -> Done` 已存在（`:92`）。

## 4. 设计

### 4.1 数据（core：`superpowers-kanban-core`）

`Card` 增两个字段（均 `#[serde(default)]`，旧 `board.json` 反序列化无损）：

```rust
/// 已归档：隐藏但仍保留在 board.json 里，可恢复。
#[serde(default)]
pub archived: bool,
/// 进入终态（done/failed/cancelled）的时刻；供宽限期自动归档判断。
#[serde(default)]
pub terminal_at: Option<DateTime<Local>>,
```

`Board` 增方法：

- `archive(&mut self, id: &CardId) -> Result<(), TransitionError>`：置 `archived = true`。
  任何状态都可归档（含 `awaiting_merge`——含义是「我已处理」）。
- `purge(&mut self, id: &CardId) -> Result<(), TransitionError>`：**仅当** `archived == true`
  才从 `cards` 移除；否则拒绝（错误信息说明「必须先归档」）。
- `purge_archived(&mut self) -> usize`：移除所有 `archived` 卡，返回数量。
- `archive_due(&mut self, now: DateTime<Local>, grace: Duration) -> Vec<CardId>`：
  把 `!archived && state.is_terminal() && terminal_at.map(|t| now - t > grace).unwrap_or(false)`
  的卡置为归档，返回被归档的 id。
- `visible(&self) -> impl Iterator<Item = &Card>`：`!archived` 的卡（供 `list`/宿主）。

`transition()`（`board.rs:268`）在把卡迁入终态
（`Done|Failed|Cancelled`）时顺带写 `terminal_at = Some(now)`（若尚未设置）。

### 4.2 CLI（plugin runner：`superpowers-kanban-runner/src/main.rs`）

`USAGE` 扩为 `run|add|add-merge|done|archive|purge|list|on|off|workdir`。新增三个动词：

- **`done <card-id> [--state-dir <d>]`**：该卡必须在 `awaiting_merge`（否则报错并说明当前状态）。
  先核对：把 id 经 `slugify` 得到 `kanban/<slug>`，用 `merge::source_branch_exists` 判分支是否在；
  在则再判是否已并入 base（`git merge-base --is-ancestor <branch> <base>`，
  base 用 `merge::default_branch`）。核对结果分三种，**如实打印**：
  - `verified`：分支存在且已并入 base；
  - `branch-missing`：分支已不存在（多半已合并后删除）；
  - `not-merged`：分支存在但**尚未**并入 base —— 仍然放行为 `done`（尊重人工确认），
    但打印醒目提示。
  然后 `board.transition(id, Done)` 并落盘。
- **`archive <card-id> | --all-terminal`**：单卡归档；`--all-terminal` 归档所有终态卡
  （`done|failed|cancelled`）。
- **`purge <card-id> | --all-archived`**：单卡清除（仅已归档）；`--all-archived` 清除全部已归档。

三者都复用既有 `--state-dir` 解析；落盘经 `persist::save_board`（既有原子写法）。

### 4.3 自动归档（plugin advance loop）

推进循环每 tick 调一次 `board.archive_due(Local::now(), grace)`，`grace` 默认
`Duration::hours(24)`，可用项目级 `superpowers-kanban.toml` 的
`archive_grace_hours` 覆盖（默认 24；`0` 表示立即归档）。归档后落盘。
`awaiting_merge`/`needs_you`/`running`/`queued` 一律不动（`is_terminal()` 已把前三者排除）。

### 4.4 呈现

- `command_list`（`main.rs:288`）默认用 `board.visible()`；新增 `--all` 显示含归档卡
  （归档行加 ` [archived]` 标记）。
- 宿主的 `board.list`（走 `plugin_query`）同样只回可见卡；桌面看板据此自然不显示归档卡。
- 侧栏看板条目摘要（排队/在跑计数）忽略归档卡。

### 4.5 superpowers-kanban skill

新增一个动作小节：**「我确认合并」**。

- 触发语：`我合并了这张卡`、`确认合并`、`更新看板进度`、`这张卡可以收了`。
- 动作：`superpowers-kanban done <card-id>`；把打印的核对结果原样转述
  （`verified` / `branch-missing` / `not-merged`），并说明「卡片已放行，将在 24h 后自动归档；
  要立刻清理可 `superpowers-kanban archive <card-id>`」。
- 边界：skill **不**执行 `git merge`（合并权归人）；只登记「已合并」这一事实。

## 5. 边界与取舍

| 场景 | 行为 |
|---|---|
| 你手工合并后 | 说一句「确认合并」，skill 调 `done`，卡 24h 后自动归档 |
| 分支已删（无法核对） | `done` 仍放行，打印 `branch-missing`，如实标注 |
| 分支在但没合并 | `done` 仍放行，但打印 `not-merged` 警告 |
| 误归档一张不该收的卡 | 记录仍在；`purge` 之前可逆向（本设计先提供「记录仍在」，取消归档见非目标） |
| 误 `purge` | 不可逆——因此 `purge` 只接受**已归档**卡，多一道确认 |
| 老 `board.json` | 新字段有默认值，无损；无 `terminal_at` 的终态卡按「从现在起算」 |

## 6. 验收

1. `done` 仅对 `awaiting_merge` 生效；对 `running`/`queued` 报错且不改状态。
2. `done` 三种核对结果各有一个用例（伪 git 目录）。
3. `purge` 拒绝**未归档**卡；`purge --all-archived` 只清已归档。
4. `archive --all-terminal` 只归档终态卡，不碰 `awaiting_merge`/`running`。
5. `archive_due` 只归档「超期且终态」；`awaiting_merge` 即使超期也不归档。
6. 旧 `board.json`（无新字段）反序列化无损，`archived` 默认 false。
7. `list` 默认不含归档卡；`list --all` 含。
8. 桌面：含归档卡的板子不渲染归档行。

**验证命令：**

```
cd plugins/superpowers-kanban && cargo test --offline
cd yi-agent-rs && cargo test --offline -p yi-agent-app-server
cd desktop && npx tsc --noEmit && npx vitest run
```

## 7. 涉及文件

- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`（`Card` 新字段）
- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs`
  （`archive`/`purge`/`purge_archived`/`archive_due`/`visible`；`transition` 记 `terminal_at`）
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`（三动词 + `list --all`）
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`（advance loop 调 sweep）
- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs`
  （或同目录新解析）读 `archive_grace_hours`
- `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`（新增「确认合并」动作）
- `plugins/superpowers-kanban/README.md`（登记新动词与自动归档行为）

## 8. 风险

| 风险 | 处置 |
|---|---|
| 自动归档误藏「其实没合并」的卡 | 自动归档**只对真终止态**（`done|failed|cancelled`）；`awaiting_merge` 不在其列（验收 5） |
| `done` 被用于「其实没合并」 | 三种核对结果如实打印；`not-merged` 显式警告；权威判断仍归人 |
| `purge` 误删活卡 | 仅接受已归档卡（验收 3） |
| 旧 `board.json` 兼容 | 新字段 `#[serde(default)]`（验收 6） |
| 归档卡仍在 `board.json`，文件随时间变大 | `purge` 提供回收；`archive` 与「列表可见性」解耦，不阻塞使用 |
