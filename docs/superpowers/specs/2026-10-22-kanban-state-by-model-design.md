# 看板状态流转改由模型驱动

日期：2026-10-22
状态：待实现

## 1. 要解决的问题

看板现在**自己判断会话的意图**，于是误报很多。误报不是零星的 bug，而是同一类错误
在七处的重复：系统试图用 `idle` 快照与 `git HEAD` 去推断「会话还活着吗」「活儿干完
了吗」，而这两件事它**根本观测不到**。

### 1.1 现在的自动流转（逐条带位置）

| 触发 | 写出的状态 | 位置 |
|---|---|---|
| 队列有卡 + 名额 | `Queued → Launching → Running` | `service.rs:next_launch` |
| 建 worktree 失败 | `→ Failed` | `service.rs:770` |
| **会话空闲 + 有提交** | `Running → AwaitingMerge` | `service.rs:358-375` |
| **会话空闲 + 无提交** | `Running → NeedsYou` | 同上 |
| **会话由闲变忙** | `AwaitingMerge/NeedsYou → Running` | `service.rs:377-381` |
| **卡有 `thread_id` 但本拍 `threads[]` 里没有** | `→ NeedsYou` | `service.rs:328, 679` |
| **卡停在 `AwaitingMerge`** | 自动追加合并回合，失败自动重试 3 次 | `service.rs:389-430` |
| 合并回合投递失败 | 立刻 `merge_finish` 出判词 | `service.rs:566` |
| 每 tick 探测 git | 分支已并入 → `Done` | `main.rs:1065`、`service.rs:1256` |
| 每 tick | 终态卡过 24h 自动归档 | `main.rs` `archive_due` |
| daemon 启动 | `Running` 无 thread → `NeedsYou`；`Merging` → **`git merge --abort`** 后 `NeedsYou` | `service.rs:1007-1070` |

### 1.2 误报的具体机制

1. **`idle` 被当成「跑完了」**。宿主的 `idle` 是「本轮 turn 结束且无 active turn」
   （`server.rs:5680-5686`），3 秒采一次。模型结束一轮、在文本里问用户一句话，会话
   就是 idle——看板立刻判 `needs_you`；用户回话，卡又被翻回 `running`。这是抖动
   的主因。
2. **`has_commits` 用 `HEAD != base_commit` 猜产出**（`service.rs:1408`）。模型提
   交后又改文件，下一次 idle 被判成「等验收」；只改文档没提交的，被判成「没产出」。
3. **宿主重启 = 所有在跑的卡变 `NeedsYou`**。`DriverState.threads` 只活在
   app-server 内存里（`session_driver.rs`），重启后 `threads` 为空，
   `recover_cards_the_host_forgot` 把每张 `Running` 卡收成 `NeedsYou`。
4. **合并回合没投递成功就判死刑**。`Append ok:false` 立刻 `merge_finish`，此时合并
   根本没跑，git 当然说没并入 → `NeedsYou`，重试计数 +1（`service.rs:566`）。
5. **没人发话也会合并**。`service.rs:389` 只要卡在 `AwaitingMerge` 且会话 idle 就
   自动追加合并回合，与「用户发话才合并」的约定相矛盾。
6. **并发槽位绑在状态上**（`card.rs:53` 只有 `Running` 占槽）。任何一次误判把卡踢
   出 `Running`，槽位立刻释放、下一张卡启动，而那个会话其实还活着——真实并发超过
   日历上限，误报变成实际的限流违规。

### 1.3 根因

系统在用一个状态机代理「会话是否还活着」「是否干完了」。前者只有宿主知道，后者只有
模型知道。现在的实现把两者都近似成 `idle + HEAD`，所以必然误报。

## 2. 决策

**分工原则：每一层只写自己真正确认过的事实。**

| 层 | 写什么 | 依据 |
|---|---|---|
| daemon / 宿主 | 排队、启动成功/失败、并发计数 | 它**亲手做过**的动作 |
| 模型 | 所有判断类状态 | 它**自己知道**的意图 |
| 观测 | 什么都不写，只派生展示 | 只读计算 |

### 2.1 状态集精简为 8 个

删除 `Launching`、`Merging`、`Paused`。保留：

```
pre_queued → queued → running → needs_you | awaiting_merge | done | failed | cancelled
```

- `Launching` 删除：它是「已认领、会话还没起来」的中间态，由 `next_launch` 在内存
  中短暂持有即可，不必落盘成一个用户可见状态。
- `Merging` 删除：合并名额改由项目级锁承载（§2.4），不再需要一张卡当它的持久载体。
- `Paused` 删除：没有独立语义（它等价于「模型说先别跑」，而这由用户自己说）。

### 2.2 迁移表放宽到 (b)

模型调 `state` 时**不做**完整迁移校验。规则只有一条：

> 终态（`done` / `failed` / `cancelled`）不能复活成非终态；其余任意设置。

理由：迁移表原本是为了防止自动逻辑乱跳，现在写状态的是模型，而模型有上下文。整张
表只会让模型在「卡在 queued 但我要直接标 done」这类正当场景上碰壁。

### 2.3 并发 = 活会话数，与状态解耦

现状（槽位绑 `Running`）会让误判直接变成限流违规。新口径：

- 改动只有一处：宿主在 `driver/next` 的 `threads[]` 里除 `thread_id`/`idle` 外
  **多报一个 `alive`**（app-server 的 `threads` map 成员即活会话，取值为真）。这是
  小改动：`server.rs:5678-5688` 的快照多算一个布尔，`session_driver.rs:threads_payload`
  多带一个键，契约文件 `driver-protocol.json` 同步，两端各有测试。
- 插件按 `alive` 持有/释放槽位租约，**不看卡状态**。一张卡模型标了 `done` 但会话还
  开着，仍占并发——因为它确实还在烧 token。

### 2.4 合并名额改为项目级锁

`Merging` 删除后，整轮合并（跨会话、数分钟）的独占载体改成：

```
<state_dir>/merge.lock    内容：{"card_id": "...", "pid": 1234, "since": "<rfc3339>"}
```

- `merge-request <card>`：取 `flock`；若锁内记录指向的 pid 仍存活 → `busy`；否则视为
  残留（上一个进程崩了），覆写记录并授权。
- `merge-finish <card>`：**只释放名额**，不改状态、不碰 git。
- 不做 git 复核。模型说合完了就是合完了——这与 §2.2 同一立场：判断归模型。

同一时刻全项目只有一个进程能合并。

### 2.5 daemon 不再碰 git

删除：`reconcile_merged`（自动 `done`）、`reconcile::plan`、启动时的
`git merge --abort`、`has_commits`、`recover_cards_the_host_forgot`、
`Running + idle` 对账、自动合并回合、合并失败自动重试（`MAX_MERGE_RETRIES`）。

保留：终态卡超期自动归档（纯簿记，不表达任何判断）。

### 2.6 模型怎么知道要写状态

两条注入：

1. **objective 文案**（`tick.rs:engagement_objective`）末尾加一句职责说明：这张卡的
   看板状态由你维护，结束回合前、需要用户决策时、完成时，按 skill 调 `state`。
2. **环境变量** `SUPERPOWERS_KANBAN_CARD=<card_id>`，与既有
   `YI_AGENT_THREAD_ID` 同一条路子（`server.rs` 装配期注入 bash 子进程），让
   `state --self` 不用手写 id。

### 2.7 观测（只读派生）

`list` 与桌面要显示「会话还在不在」，但它们直接读 `board.json`，拿不到宿主内存里的
会话表。因此 daemon 每拍把观测结果写进一个**只读旁路文件**，与卡状态严格分开：

```
<state_dir>/observations.json
{"threads": {"thread-1": {"alive": true, "last_seen_alive_at": "<rfc3339>"}}}
```

- 只有 daemon 写，`list` / 桌面只读。
- `alive` 取自宿主本拍 `threads[].alive`；`last_seen_alive_at` 是 daemon 自己记的
  「最后一次看到它还活着」的时刻（宿主不提供时间）。
- 派生标记 `stale` = 卡是 `running` 且其 `thread_id` 在观测里 `alive=false`。

按用户选择呈现为：**角标 + 最后活动时间**。不做任何自动降级，观测文件**永不**写回
卡片状态。

## 3. 接口

### 3.1 新 CLI 动词

```
superpowers-kanban state <card-id> <state> [--detail <text>]
superpowers-kanban state --self <state> [--detail <text>]
```

`<state>` 取 8 个值之一。`--self` 从 `SUPERPOWERS_KANBAN_CARD` 取 id，缺失时报错并
提示「当前不在卡片会话里」。

对应 IPC 方法 `card.state`，与既有 `promote` / `merge_request` 同一分派面
（`dispatch.rs`）。

### 3.2 迁移表

`CardState::can_transition_to` 收缩为：`is_terminal()` 为真时一律 false，否则 true。

### 3.3 要删的代码面

- `CardState::Launching` / `Merging` / `Paused` 及其全部迁移边
- `service.rs`：`recover_cards_the_host_forgot`、`driver_step` 步骤 1.5/2/3/4、`has_commits`、
  `MAX_MERGE_RETRIES`、`merge_retries`
- `core/converge.rs` 整个模块、`runner/reconcile.rs` 整个模块
- `service.rs::merge_finish` 的 git 复核段
- `migrate_legacy_running` 的合并 abort 段（仅保留 `Running` 无 thread → `needs_you`
  这一条启动迁移）
- `DriverAction::Append`（续跑由用户在会话里说话触发，不再由插件追加）

### 3.4 SKILL.md

新增一节「维护卡片状态」，给出判定表：

| 情境 | 写入状态 |
|---|---|
| 计划跑完、有产出待验收 | `awaiting_merge` |
| 需要用户决策 | `needs_you` |
| 已合并/用户确认完成 | `done` |
| 环境阻塞、无法继续 | `failed` + `--detail` |
| 用户放弃 | `cancelled` |

并明确：**不要**为了「让看板好看」而提前写 `done`；卡片状态是给用户的承诺。

## 4. 验收

- `cargo test -p superpowers-kanban-core`：迁移表新口径（终态不复活、非终态任意）
- `cargo test -p superpowers-kanban-runner`：`state` 动词、`--self` 缺环境变量报错、
  `merge.lock` 残留 pid 回收、并发按 `alive` 计数
- `cargo test -p yi-agent-app-server --lib session_driver`：`alive` 字段进快照
- 契约：`driver-protocol.json` 两端断言同时更新
- 变异验证：删掉 `alive` 传递后，并发计数测试必须失败
- 实机：起一张卡，模型在一轮里问用户问题 → 卡**不**应被自动改成 `needs_you`

## 5. 后果

- 模型忘了写状态或会话被杀 → 卡停在 `running`，看板显示 `stale` + 最后活动时间，
  需人收拾。这是用户明确选择的代价（零误报优先）。
- 并发可能略高于「卡状态看起来的数量」——因为会话比卡片活得久。这是刻意的：真实
  占用优先于账面整齐。
