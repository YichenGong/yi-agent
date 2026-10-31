# 合并队列：用户确认后入队，名额空时开跑

日期：2026-10-31
状态：待实现
关系：接在 `2026-10-22-kanban-state-by-model-design.md` 之后。那篇把卡片状态
交还给了模型，本篇补上它留空的一格——**合并要有人先说"可以合"**。

## 1. 要解决的问题

改造后 daemon 不再根据 `idle` / git 推断状态，但合并这一环还留着旧习惯的尾巴：

- `AwaitingMerge` 这个状态同时表达了两件事——"活儿干完了"和"可以合并了"。
  daemon 无从区分，于是任何进入 `AwaitingMerge` 的卡都成了合并候选。
- `merge-request` 一次动作里做两件事：抢 `merge.lock` **和** 让卡进入执行态。
  抢不到就 `Busy`，卡原地不动，等下一拍再被自动选中重试——排队这件事只存在于
  "反复重试"里，没有任何地方能看见"我排第几"。
- 用户没有说"可以合"的位置。模型认为做完了就置 `awaiting_merge`，队列就把它
  当成待办合并的授权。

结果是：用户没有确认，合并也会发生。

## 2. 目标（以及明确的非目标）

**目标**

1. 合并必须由**用户确认**才发生。确认经模型落到 CLI（用户在会话里发话，模型跑
   `merge-request`），daemon 从不会自己决定"该合并了"。
2. 合并排队可观测：卡上有"已确认、等名额"这个状态，`list` 与桌面都能看到。
3. 同一项目同一时刻只有一个合并真正在跑（沿用 `merge.lock`）。
4. 排到队才进入执行态；此前一直是"排队中"。

**非目标**

- 不做合并优先级/插队。FIFO，按入队时间。
- 不做跨项目全局合并并发限制。名额仍是每项目一把 `merge.lock`。
- 不引入 `Merging` 状态。合并是一次会话回合，执行态就是 `Running`；谁在合并由
  `merge.lock.holder` 表达（见 §4.3）。
- daemon 不碰 git、不判合并成败。判词仍归模型（`state --self`）。

## 3. 状态与流转

状态集合从 8 个变 9 个，新增 `MergeQueued`：

| 状态 | 含义 |
|---|---|
| `PreQueued` | 想做，spec/plan 还没写 |
| `Queued` | 排队等实现 |
| `Running` | 有活会话在跑（**合并回合也是这个状态**） |
| `NeedsYou` | 等用户决定 |
| `AwaitingMerge` | 实现做完了，**还没被批准合并** |
| `MergeQueued` | **用户已确认合并，等名额**（本篇新增） |
| `Done` / `Failed` / `Cancelled` | 终态 |

流转（只有这些是被允许的业务路径）：

```
Running ──(模型：活儿干完)──▶ AwaitingMerge
AwaitingMerge ──(用户发话 → 模型 merge-request)──▶ MergeQueued
MergeQueued ──(用户反悔 → 模型 merge-cancel)──▶ AwaitingMerge
MergeQueued ──(daemon：名额空且排到)──▶ Running   ← 这一拍追一回合去合并
Running ──(模型合并完 → state --self done)──▶ Done
```

`can_transition_to` 仍然保持"非终态任意设、终态不可复活"的宽松语义——状态机是
护栏，不是业务逻辑的载体。真正的业务约束落在动作上（§4）。

**关键点：`AwaitingMerge` 不再意味着"可以合并"。** 它只是"活儿干完了"。授权由
`MergeQueued` 承载，而进入 `MergeQueued` 的唯一入口是 `merge-request`。

## 4. 动作

### 4.1 `merge-request <card-id>`：入队，不抢锁

```
1. 读卡；卡不存在 → Denied
2. 准入：卡必须处于 AwaitingMerge
     - 其它状态一律 Denied，报清当前状态
     - 特别地：Queued 的实现卡在会话里还没跑过，不能一步落地
3. 置 MergeQueued，把入队时间写进卡（用于 FIFO）
4. 返回 "queued: <n> merge(s) ahead in this project"
```

**它不再碰 `merge.lock`。** 抢锁是 daemon 在开跑那一拍的事（§4.4）。这是本篇最
重要的一处改动：把"申请"与"执行"拆开，排队才有地方存在。

重复调用幂等：卡已在 `MergeQueued` 时返回当前排队位置，不报错、不改顺序。

### 4.2 `merge-cancel <card-id>`：退队

```
1. 卡必须处于 MergeQueued，否则 Denied（报当前状态）
2. 置回 AwaitingMerge，清掉入队时间
3. 返回 "cancelled: <card-id> is back to awaiting_merge"
```

**为什么必须有这个动词**：队列由 daemon 每拍自动推进。如果退队只能靠
`state <id> awaiting_merge`，那么在"你决定反悔"和"daemon 跑了下一拍"之间就有
竞态——很可能卡已经被送进 `Running` 了，而那时再改回去会与一个正在跑的合并回合
打架。`merge-cancel` 只在 `MergeQueued` 上生效，daemon 只推进仍在 `MergeQueued`
的卡，两者对同一张卡的判定天然互斥。

### 4.3 `merge-finish <card-id>`：放锁（不变）

只释放 `merge.lock`、清 holder，不碰 git、不改状态。与改造前一致。

### 4.4 daemon 每拍：队列推进

`driver_step` 在启动新卡之前跑一遍队列推进：

```
1. 候选 = 所有 MergeQueued 的卡，按入队时间排序（同时间按 order，再按 id）
2. 若 merge.lock 已被持有（holder 指向任何卡）→ 本拍不动
3. 逐张候选（顺序严格照 §4.5：先会话名额，后合并锁）：
     a. 卡有 thread_id 且该会话不空闲 → 跳过（会话正忙，追不进去）
     b. acquire 一个会话名额（全机池）失败 → 本拍不推进（不是换下一张：
        名额是全机共享的，换一张也一样拿不到）
     c. acquire merge.lock（带 holder）失败 → 把刚才的会话名额放回，下一张
     d. 置卡 Running，记 pending（Launch 或 Append）
     e. 交出动作，本拍结束（一拍最多开一个合并）
```

会话选择（与先前的决策一致）：

- 卡有 `thread_id` 且会话空闲 → `Append`，在原会话里追一回合
- 卡没有 `thread_id`，或会话已不在 → `Launch`，新开一个会话，worktree 用
  `merge::prepare(base)` 检出的那棵树

**`[merging]` 的显示**：`list` 对 `Running` 且 `merge.lock.holder` 指向该卡的
行，行尾追加 ` [merging]`；`plugin/query` 的 `list` 回包同样带 `merging: true`。
这是**派生展示**，不是新状态——与 `stale` 同一个手法。

### 4.5 名额与并发名额是两件事

- `merge.lock`（每项目一把）：保证同时只有一个合并在跑。**本篇不变。**
- 会话并发名额（`lease.rs`，全机池）：合并回合是一次真实的模型回合，一样要吃
  额度。队列推进在拿 `merge.lock` 之前先要拿到会话名额；拿不到就把锁放回、
  本拍不推进（卡留在 `MergeQueued`，不占合并锁，也不占会话名额）。

顺序必须是"先会话名额、后 merge.lock"，否则会出现"占着合并锁等额度"——期间
同一个项目的所有合并都被堵住，而它们本可以排在后面等着。

## 5. 删除的东西

| 删掉 | 理由 |
|---|---|
| `MAX_MERGE_RETRIES` 与 `merge_retry_count` | 合并失败不再是"自动重试"，而是模型写 `needs_you`，由用户决定 |
| `any_merging()` 的 `Merging` 判定 | 已无 `Merging` 状态；并发由 `merge.lock` 表达 |
| `Queued + kind == Merge` 走合并轮 | 独立合并卡就是一张普通排队卡，由 `next_launch` 起会话，会话里自己 `merge-request` |
| 入队时的 `merge::prepare` | 入队不建 worktree；开跑那一拍才建（否则排队卡会提前建出一堆 tree） |

## 6. 各消费面的同步

| 面 | 改动 |
|---|---|
| `card.rs` | 加 `MergeQueued`；`Card` 加 `merge_queued_at: Option<DateTime<Local>>` |
| `service.rs` | 重写 `merge_request`（只入队）、新增 `merge_cancel`、`driver_step` 加队列推进、删重试账 |
| `dispatch.rs` | `card.state` 已能设任意状态；加 `merge_cancel` 方法；`list` 回包加 `merging` |
| `main.rs` | 加 `merge-cancel` 动词与 USAGE；`list` 行尾加 `[merging]` |
| `tick.rs` | `merge_objective` 文案改为"名额已到手，直接合并"（不再自己申请） |
| `SKILL.md` | 动词表加 `merge-cancel`；§2b 改为"用户确认后 `merge-request` 入队，等会话被告知再合并"；§2d 状态表加 `MergeQueued`；§3 `list` 说明 `[merging]` |
| `desktop` | 状态映射加 `MergeQueued`（含中文名与颜色）；`[merging]` 角标 |
| `docs/project-management/superpowers-kanban.md` | 登记本篇 |

## 7. 验收判据

1. `cargo test -p superpowers-kanban-core`：`MergeQueued` 的存在与 `can_transition_to`
   的宽松语义有测试覆盖。
2. `cargo test -p superpowers-kanban-runner --lib merge_request_`：
   - 只把卡置 `MergeQueued`，**不**拿 `merge.lock`（断言语义：另一个 `merge_request`
     仍能成功入队）
   - 非 `AwaitingMerge` 的卡被拒
   - 重复调用幂等且报告排队位置
3. `cargo test -p superpowers-kanban-runner --lib merge_cancel_`：
   - 只在 `MergeQueued` 上生效，退回 `awaiting_merge`
   - 已 `Running` 的卡被拒
4. `cargo test -p superpowers-kanban-runner --lib merge_queue_`：
   - 名额空 + 一张排队卡 → 本拍交出动作、卡置 `Running`、holder 指向它
   - 两张排队卡 → 一拍只开一个，第二张仍在 `MergeQueued`
   - holder 已被占 → 本拍不开跑，卡留在 `MergeQueued`
   - 会话不空闲 → 跳过该卡，取下一张
5. `cargo test -p superpowers-kanban-runner --bin superpowers-kanban`：`merge-cancel`
   的参数解析、`list` 的 `[merging]` 行尾、skill 回归锁（每个动词都在 SKILL.md 里）。
6. 桌面：`npx vitest run src/lib/superpowersKanban* src/components/SuperpowersKanban*`
   与 `npx tsc --noEmit`。
7. 端到端手验（spec §4 复现）：一张卡置 `awaiting_merge`，**什么都不做** →
   `list` 里它不会自己变成别的状态；模型跑 `merge-request` → 变 `MergeQueued`；
   下一拍 → `Running` 且 `[merging]`；模型 `state --self done` → `Done`。

## 8. 取舍

**为什么不给队列做持久化的序号**。FIFO 用 `merge_queued_at` 排序就够了，序号需要
在每次入队/退队时重排，多一份可能与时间戳不一致的状态。时间戳是入队那一刻的事实。

**为什么 `merge-cancel` 不复用 `state`**。见 §4.2：`state` 是无条件写，会在"反悔"
与"daemon 推进"之间留竞态；`merge-cancel` 带着"必须仍在 MergeQueued"的前置条件，
在锁内判定，天然安全。

**为什么合并执行态不设独立状态**。合并是会话回合，回合期间会话在跑——这已经是
`Running` 的语义。再加一个状态只会让"这张卡到底在跑什么"变成需要两个字段才能
回答的问题，而 `merge.lock.holder` 已经能精确回答。
