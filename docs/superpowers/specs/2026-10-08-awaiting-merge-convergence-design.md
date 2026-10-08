# `awaiting_merge` 收敛边 Design

日期：2026-10-08
状态：设计已确认（用户逐项拍板），待用户 review 本文件后进入 writing-plans。
范围：插件 `plugins/superpowers-kanban/`（`superpowers-kanban-core` + `superpowers-kanban-runner`）。不改宿主、不改桌面、不改协议。

## 1. 背景与问题

### 1.1 现象

分支已经并入 `main`，卡片却长期停在 `awaiting_merge`。实测本项目三张卡全部如此：

| 卡 id | 推导分支 `kanban/<slug>` | 分支状态 |
|---|---|---|
| `2026-10-04-kanban-board-as-thread-page-design-...` | `kanban/2026-10-04-kanban-board-as-thread-page-...` | **不存在**（合并提交 `93d1c6e3` 仍在 main） |
| `2026-10-04-thread-git-diff-tab-design-...` | `kanban/2026-10-04-thread-git-diff-tab-...` | 存在且已并入 main |
| `2026-10-07-document-attachments-design-...-impl` | `kanban/2026-10-07-document-attachments-...` | 存在且已并入 main |

### 1.2 根因

`awaiting_merge` 是状态机里的**汇合点**（`core/card.rs:64-105`），但它**没有自动出货口**：

- **进**：卡片会话一结束（无论有无产出），宿主调度器一律先判 `awaiting_merge`
  （`app-server/src/card_scheduler.rs:97-103`），不是 git 复核的结果。
- **出**：`awaiting_merge` 不是终态（`is_terminal` 只认 done/failed/cancelled，`card.rs:56-61`），
  但只有三条人为/显式路径能出：
  1. `merge-request → merging → merge-finish`（`service.rs:494-578`，仅作用于**处于
     `Merging` 的那张合并卡**，不回头碰源实现卡）；
  2. `superpowers-kanban done <card-id>`（`main.rs:526-569`，须人工触发）；
  3. 自动派生合并卡成功时的连带（`service.rs:422-429`，**要求 `origin_card` 非空**）。

没有任何代码会「看见分支已并入 base，就把实现卡翻成 `done`」。

### 1.3 已确认的两条「为什么没自动出」

- **路径 3 从未触发**：它的前置是 `mark_terminal(awaiting_merge)` 自动派生合并卡，而该派生
  **只在项目偏好 `board_auto_merge` 为 `true` 时才发生**（`service.rs:288-306`；`spec
  2026-10-03` D13 明定默认关）。本项目 `preferences.json` 无此键 → 不派生 → 无合并卡 →
  无 `origin_card` → 无连带。board.json 里那两张 `kind=merge` 卡是另经 `add-merge` 手工投递的，
  它们的 `origin_card` 是 `null`。
- **board-as-thread-page 那次是普通 `git merge`**，根本没走合并卡。

## 2. 目标与非目标

**目标**

1. 卡片分支已并入 base 后，**无需人工命令**，卡片自动落到 `done`。
2. 分支**存在但未并入**时，卡片保持 `awaiting_merge`，不被误判。
3. 分支**不存在**（无法判定）时，卡片保持 `awaiting_merge`，并打印**可直接照做**的提示。
4. 收敛只作用于实现卡，绝不误伤 `kind=merge` 卡与 `needs_you` 卡。

**非目标**

- 不扫 `needs_you`（其语义是「等人决定」，自动终态化会削弱信号）。
- 不加延迟阈值、不加「会话是否在跑」的判据（见 §3 D3）。
- 不新增 CLI 子命令（复用现有 `done` 作为分支缺失时的人工出口）。
- 不改终态语义：**不新增 `done → *` 复活边**。
- 不做桌面可见的通知/RPC（日志走后台进程 stderr，与既有 tick 日志一致）。
- 不改全局并发日历：收敛是簿记，**不占**任何名额（`occupies_slot()` 仍只认 `Running`）。

## 3. 设计决策汇总

| # | 决策 | 取值 |
|---|---|---|
| D1 | 收敛由谁触发 | **插件 tick 自动**：`runner/main.rs` 的推进循环每轮做一次收敛扫描 |
| D2 | 扫描范围 | 仅 `state == AwaitingMerge` 且 `kind == Implementation` 且未归档的卡 |
| D3 | 竞态取向 | **git 事实优先、无条件收敛**：不检查该卡会话是否在跑；撞上追问复活则那一轮失败、卡保持 `done` |
| D4 | 已并入（`merged`） | `(AwaitingMerge, Done)` → `done`（该边已存在，`card.rs:102`），打印收敛日志 |
| D5 | 存在但未并入 | 不动，保持 `awaiting_merge` |
| D6 | 分支不存在 | 不动，打印含 `superpowers-kanban done <id>` 与 workdir 的提示 |
| D7 | 复核主渠道 | 现有 `merge::branch_merged_into`（`git merge-base --is-ancestor`，`merge.rs:46-55`） |
| D8 | 自动派生合并卡 | **移除**：`mark_terminal` 里的 `board_auto_merge` 派生块退役（被 `spec 2026-10-08` D2/D3 取代） |
| D9 | 日志去向 | `eprintln!`（后台 daemon stderr），与 `main.rs:825/832/843` 同渠道 |
| D10 | 分支缺失提示去重 | 同一张卡的 `MissingBranch` 提示**只在本 daemon 生命周期内首次出现时打印**；去重集是**内存**状态（不写 `board.json`） |

## 4. 收敛扫描

### 4.1 触发点与位置

`runner/main.rs` 的推进循环（`main.rs:804`）在 `consume_inbox` 之后、`archive_due` **之前**新增一步
`service.reconcile_merged()`。顺序理由：先收敛（可能产生新的 `done`），再归档（`archive_due` 只清
「进终态且超宽限期」的卡，刚 `done` 的卡 `terminal_at` 才写下、未到 24h 宽限，不会被同拍归档）。
选该循环的理由：它已在做同类簿记（消费投递、归档），且天然按 `effective_interval` 节流；
插件是唯一写 `board.json` 的进程，无需跨进程协调。

开关关闭（`main.rs:815` 的 `continue` 分支）时不扫描——与「关掉开关只停止推进」的既有语义一致。

### 4.2 每张候选卡的判定

对每张候选卡（D2）：

1. `source = kanban/<slugify(card.id)>`，`base = merge::default_branch(project_root)`。
   source 的推导与 `command_done`（`main.rs:548-552`）、`merge_request`（`service.rs:516`）
   **逐字一致**，不另造规则。
2. 三个互斥结论（**纯判据**，只吃两个布尔事实）：
   - `merged == true` → `Converge::Done`；
   - `merged == false && exists == true` → `Converge::Wait`；
   - `exists == false` → `Converge::MissingBranch`。
3. 归并成**只含「该冒出水面」之事**的结构化报告（`reconcile_merged()` 自己拥有去重与状态迁移，
   **不打印**；tick 只负责把报告逐条打印）。纯判据的三态在此被收敛为两类报告项：
   - `Converge::Done` → `transition(id, Done)`，报告 `Converged { id, source, base }`；
   - `Converge::MissingBranch` 且该卡**首次**出现 → 报告 `MissingBranch { id, source, workdir }`；
     非首次 → **不报告**（去重，见 §4.5）；
   - `Converge::Wait` → **不报告**（无状态变化、无告警，无需每 tick 复述）。

   tick 接线处对两类报告各打印一行：
   - `Converged`：`superpowers-kanban: card <id> auto-converged to done (<source> is merged into <base>)`
   - `MissingBranch`：`superpowers-kanban: card <id> is awaiting_merge but <source> is gone; run `superpowers-kanban done <id>` to confirm (worktree: <workdir>)`

### 4.3 职责切分（保持 core 无 I/O）

- **core**（`superpowers-kanban-core`，无 I/O，可纯单测）：
  - 新增 `core/src/converge.rs`：筛选 helper（返回待收敛的实现卡 id）+ 纯判据
    `(exists, merged) -> Converge`（`Converge::{Done, Wait, MissingBranch}`）。
- **runner**：
  - 新增 `reconcile.rs`，只做 git I/O（`source_branch_exists` / `branch_merged_into`）与
    按 §4.2 第 3 条归并报告。
  - `BoardService::reconcile_merged(&mut self) -> Vec<Reconcile>`：**拥有**状态迁移与去重，
    返回应打印的报告项（`Converged` / 首次的 `MissingBranch`）。
  - 去重集 `notified_missing_branch: HashSet<CardId>` 挂在 `Inner` 上（与 `board`/`leases` 同锁），
    由 `reconcile_merged` 读写。

### 4.4 竞态（D3 的显式后果）

宿主对账会把「`awaiting_merge` 的卡所在会话又忙起来」翻回 `running`（`card_scheduler.rs:110-119`）。
若插件 tick 的收敛与这一次复活相撞：

- 插件先落 `done` → 宿主的 `board.mark_running` 落在终态上被拒（`card.rs:66-67`），该调用是
  `let _ = board_query(...)` 吞错，故**静默失败**，卡保持 `done`。
- 这是**有意接受**的：分支已并入 base 后「完成」是事实状态；追问续跑的本意是「合并前再改改」，
  一旦合并落地，后续改动应开新卡。

### 4.5 分支缺失提示的去重（D10）

`MissingBranch` 的卡不会被状态改动（它永远停在 `awaiting_merge`），所以「每 tick 重打」会让一行
相同告警无限刷屏。去重规则：

- 去重集 `notified_missing_branch: HashSet<CardId>`，挂在 `BoardService::Inner` 上（与 `board`、
  `leases` 同锁），**纯内存**，不写 `board.json`（卡片 schema 不变）。
- **由 `reconcile_merged()` 读写**（它已持锁）：对判定为 `MissingBranch` 的卡，仅当 `insert(id)`
  返回 `true`（首次）才把它放进报告；非首次不入报告，tick 自然不会打印。同一 daemon 生命周期内
  每张卡至多一条提示。
- **插入即视为已提示**：插入与「放进报告」在同一处，不存在「打印失败才插入」的分支。
- **重启语义**：daemon 重启后去重集清空，同一张卡会**再提示一次**。这是可接受的——该提示要求人工
  动作，重发一次比漏发安全；也避免为「只提示一次」给卡片 schema 加持久字段（那要改序列化契约、
  每卡多一次落盘，收益不成比例）。
- 卡一旦离开 `awaiting_merge`（例如被手工 `done`），即使仍在去重集里也无害：它不再入选候选集，
  集合条目成为惰性垃圾，daemon 重启即回收。

## 5. 顺带修正：退役自动派生合并卡（D8）

`service.rs:287-306` 的派生块**能工作且被测试覆盖**（`service.rs:1040-1072` 断言
`board_auto_merge=true` 时派生出的卡带 `origin_card == "card-2"`）。它不是死代码；本设计移除它，
理由是**被更新决策取代**：

- `spec 2026-10-08` D2/D3 已把合并定为「手动、会话驱动、不新增看板行」；
- 保留它会让同一目标存在**两条自动路径**（本设计的收敛扫描 + 派生卡的 `merge_next`），二者
  都可能在无人发话时改 `main`，且互相竞争；
- 它还与「插件不跑 git」的取向相悖：`merge_next` 由插件自行 `git merge`（`service.rs:461`），
  而收敛扫描只读 git 事实、不写。

**处置**：

- 删除 `mark_terminal` 内的派生块（`service.rs:287-306`），使 `mark_terminal` 回归「只落状态 +
  释放槽位」。
- 把 `service.rs:1040-1072` 的用例替换为**反向断言**：即使 `board_auto_merge = true`，
  `mark_terminal(awaiting_merge)` 也**不**派生合并卡。
- 保留：`CardKind::Merge`、`add-merge`（手工投递独立合并卡的通路）、`merge_next` 与
  `claim_next_merge`（`spec 2026-10-08` §5.5 明确「存量 `kind=merge` 卡不迁移、原逻辑收尾」）。
  收敛扫描按 D2 只认 `Implementation`，故不会碰这些卡。
- `switch::read_bool`：作为通用布尔偏好读取器**保留**（不因移除唯一生产调用方而删），
  但 `board_auto_merge` 这个键不再被读。

## 6. 不做什么

- 不扫 `needs_you`；不新增 `(NeedsYou, Done)` 迁移边。
- 不加「会话空闲才收敛」的判据（那需把宿主会话状态喂给插件，重新耦合两个进程）。
- 不给 `done` 加自动归档之外的任何特效；归档宽限期不变（24h，`service.rs:141-153`）。
- 不新增 CLI；分支缺失的人工出口仍是 `superpowers-kanban done <card-id>`。
- 不改桌面端（收敛后卡的呈现由既有状态映射负责）。

## 7. 测试与验收

**core（纯单测）**

- 判据三态：`(exists=true, merged=true) -> Done`、`(true,false) -> Wait`、`(false,_) -> MissingBranch`。
- 筛选 helper：只返回 `awaiting_merge` + `implementation` + 未归档；`needs_you`、`kind=merge`、
  已归档卡一律不在结果里。

**runner（真实 git 仓库，沿用既有 `project_with_worktree` 风格）**

- 卡停在 `awaiting_merge`，其 `kanban/<slug>` 已并入 `main` → `reconcile_merged()` 后卡为 `done`。
- 分支存在但**未**并入 → 卡仍为 `awaiting_merge`。
- 分支被删（但 `93d1c6e3` 类合并提交仍在 main）→ 卡仍为 `awaiting_merge`，报告的
  `MissingBranch` 条目含该卡 id 与 workdir。
- **去重**：同一卡连续调 `reconcile_merged()` 两次，第二次的报告中 `MissingBranch` **不再出现**
  （首次已记入去重集）；另一张不同的缺分支卡仍会出现。
- `kind=merge` 的卡与 `needs_you` 的卡：调 `reconcile_merged()` 前后状态不变。
- 移除派生：`board_auto_merge=true` 时 `mark_terminal(awaiting_merge)` **不**新增合并卡
  （替换 `service.rs:1040` 的原断言）。

**验收口径（人可验证）**

1. 对本项目三张卡：两张分支已并入的，跑一次 tick 后自动变 `done`；board-as-thread-page 那张
   （分支已删）保持 `awaiting_merge` 并打印 `done <id>` 提示，手跑一次 `done` 后归位。
2. `superpowers-kanban list` 在收敛后不再显示这三张为 `awaiting_merge`。
3. 反向：把一张未合并的实现卡置 `awaiting_merge`，停留多个 tick 状态不变。
4. 缺分支的提示只出现一次：board-as-thread-page 那张（分支已删）在 daemon 存活期间只打印一条
   `run 'superpowers-kanban done <id>'` 提示，后续 tick 不重复。

## 8. 涉及文件

- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/converge.rs`（新增：纯判据 `Converge`
  + 候选卡筛选，无 I/O）
- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/lib.rs`（导出 `converge` 模块）
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/reconcile.rs`（新增：git I/O + 归并报告）
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
  （`reconcile_merged()`；`Inner` 加 `notified_missing_branch`；删 `mark_terminal` 派生块；
  改 `service.rs:1040` 用例）
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`（tick 接线 + 日志）
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs`（若 `reconcile` 需导出）

## 9. 风险

| 风险 | 处置 |
|---|---|
| 收敛误判「已合并」 | 只在 `merge-base --is-ancestor` 为真时判 `done`；分支缺失一律不判（D6）。绝不用「分支不存在」推断「已合并」。 |
| 收敛撞上追问复活 | 明确接受（D3）；代价是那一轮续跑静默失败、卡已 `done`。 |
| 移除自动派生造成既有行为回归 | 以反向断言锁住（§7）；`spec 2026-10-08` §5.5 的「存量合并卡不迁移」不受影响。 |
| tick 频率下 git 调用开销 | 候选集通常为空（无 `awaiting_merge` 实现卡时零 git 调用）；存在候选时才 `git`，且每 tick 至多每卡一次。 |
| `MissingBranch` 提示刷屏 | 去重（D10）：每 daemon 生命周期内每卡至多一条；重启后重发一次，可接受。 |
