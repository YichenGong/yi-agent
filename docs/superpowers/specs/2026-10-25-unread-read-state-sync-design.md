# 跨端未读状态同步设计（服务端权威已读水位）

> 问题：手机与桌面 App 显示的未读蓝点不一致。用户预期两端一致。
>
> 日期：2026-10-25
> 决策：采用「共享已读水位」——任一端读过，所有端都不再亮；服务端是唯一真相。

## 1. 现状与根因

未读当前是**每个客户端进程内存里的一个布尔值**，从不落盘、从不上线：

- 置位：`desktop/src/lib/threadStore.ts:226` — `applyNotification` 收到 `turn/completed`
  且该会话不是当前会话时 `v.unread = true`。
- 清除：`desktop/src/lib/threadStore.ts:93` — `select()` 时 `unread = false`。
- 生命周期：仅存活于本进程内存（`ThreadView.unread`），刷新/重连即全部丢失。

服务端完全没有这个信息：

- `thread_summary_json_with_status`（`yi-agent-rs/crates/yi-agent-app-server/src/server.rs:4206`）
  产出的会话摘要不含任何未读/已读字段。
- `ThreadMeta`（`yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:39`）
  没有任何 last-read 记录。
- 没有任何标记已读的 RPC（`markRead` / `read_state` / `lastRead` 全仓搜不到；
  `device_store` 的 `last_seen_at` 是配对设备的在线时间，与已读无关）。
- 协议注释已明说：`protocol.rs:591`「落库的 `ThreadSummary` 并不带未读位，
  `thread/listAll` 无从补齐」。

因此不一致是必然的，至少有四条互相独立的路径：

1. **离线期间跑完的回合永远丢点。** `turn/completed` 推不到，
   重连后的 `thread/listAll` 补不回来。
2. **一端读过、另一端仍亮。** 手机点开清除的是本地布尔位，桌面那份无人知晓。
3. **桌面每次启动都消费掉第一个会话的蓝点。** 桌面握手恒回落「选中第一条」
   （`desktop/src/App.tsx:1560`：`keptId = isRemoteClient() ? ... : null`），
   而 `selectThread` 必然 `store.select()` → 清未读。
4. **两端基线不同。** 每端的「已读」基准是本端连接时刻，而不是最后一次真正
   查看的位置。

## 2. 目标与非目标

**目标**

- 未读状态**只有一个真相**：服务端。任一端点开某会话，所有端的该会话蓝点
  同时消失。
- 离线/断连期间跑完的回合，重连后仍能正确显示为未读。
- 桌面端与手机端走同一条链路，不为某一端开特例分支。

**非目标**

- 不做每设备独立的已读位（那会让「手机看过、电脑仍亮」变成设计意图，
  正是本 bug 要消除的现象）。
- 不做「前端主动发起的轮次不算未读」这类细分语义：本设计只按「最后完成时间
  vs 最后读取时间」判定。
- 不做未读计数聚合、不做推送角标、不做已读回执给发送方。

## 3. 数据模型

`ThreadMeta` 新增两个字段，都带 `#[serde(default)]`（缺省读成 `0`），
旧 meta 文件零迁移：

| 字段 | 含义 |
| --- | --- |
| `last_read_at: i64` | 共享已读水位（epoch 毫秒），由服务端时钟写入 |
| `last_completed_at: i64` | 最近一次**回合结束**的时间（epoch 毫秒） |

**判定式：`unread = last_completed_at > last_read_at`。** 服务端与客户端共用
这一条，不存在第二套口径。

### 3.1 为什么不能复用 `updated_at`

`updated_at` 在**回合开始**时也会被 `touch()` 刷新
（`server.rs:5083`，用户一提问就写）。若拿它当「完成时间」，另一端刚发出提问、
本轮还没跑完的会话会立刻亮蓝点——把「正在跑」误报成「跑完了」。故必须单独
记录回合结束时刻。

### 3.2 为什么是水位而不是布尔位

布尔位无法表达「这次完成发生在读取之后」。用两个时间戳取比较，天然获得：

- 单调性：`last_read_at` 只增不减（取 `max`），乱序/重试不会把已读打回未读。
- 幂等性：重复标记已读是 no-op，不产生写盘（但仍会广播一次，见 §4.1）。
- 可合并：客户端收到多个来源（listAll 快照、推送、markRead 回包）时按 `max`
  合入即可收敛，无需「哪个来源更权威」的额外机制。

## 4. 服务端改动

### 4.1 新 RPC `thread/markRead`

- 入参 `{ threadId }`，出参 `{ last_read_at }`。
- 写入 `last_read_at = max(旧值, now_millis())`；值未变大时**不写盘**（避免白推
  进 mtime 让盯着 meta 的 watcher 重读），直接回当前值。
- 成功时**总是**广播
  `thread/readStateChanged { thread_id, last_read_at, last_completed_at }`，即便
  水位没推进。理由：这是一次廉价的收敛信号——刚重连、还停在旧水位的另一端收到
  它就能就地补齐，而不必等下一次 `thread/listAll`。「不广播」省下的那点带宽，
  换不来「另一端可能永远停在旧值」的代价。
- 权限：要求 `Scope::Control`，与其它 meta 写操作（`thread/setTag`、
  `thread/setPinned`、`thread/setModel`）同档。`Observe` 是只读视图，
  不得推进已读水位。
- 参数校验先于存在性：未知 thread 回 `-32011`（复用 `RpcError::unknown_thread`）。
- 必须注册进 `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs` 的域路由
  表（`Domain::Thread`），否则会被静默归到 `Domain::Unknown` 而变成
  `method_not_found`。

### 4.2 回合结束时写 `last_completed_at`

写入点有两处，两处都只在「确实有回合结束」时执行：

1. `persist_and_finish_turn`（`server.rs:4481`）中 `turn_id.is_some()` 的分支。
   `clear` / `compact` 收尾传 `turn_id = None`，**不得**写——它们不是回合结束，
   写进去会让「刚清空上下文」的会话凭空亮蓝点。
2. `run_blocks` 直接失败的路径（`server.rs:5112`）：该路径不走 persist，
   但它确实是一次回合结束（`turn/completed(failed)` 已发出），需单独补写。

写入沿用 `set_tag` / `set_pin_seq` 的 `update_meta` 风格，**不刷新 `updated_at`**
（与「置顶/标签属于设置变更而非会话活动」同一理由）。

### 4.3 `turn/completed` 通知携带 `completed_at`

广播发生在落盘**之前**：`translator.on_event` 产出的 `turn/completed` 在
`server.rs:5260` 一带发出，而 `persist_and_finish_turn` 在 `:5504` 才写盘。
客户端收到 `turn/completed` 会立刻 `refreshThreads()`，那次 `thread/listAll`
很可能拿到**旧的** `last_completed_at`。

修法不是在客户端再造一套「来源优先级」（`statusSource` 那种），而是让通知
自带权威时间戳：`turn/completed` 增加 `completed_at`，客户端按 `max` 合入。
竞态因此消失，且不引入新状态。该字段为 `Option<i64>`（`#[serde(default)]` +
`#[ts(optional)]`）：旧服务端不发它时客户端退回本地墙钟，最坏多亮一次蓝点，
不会漏——不值得为一个「更好一点」的时间戳把线协议变成破坏性变更。

### 4.3.1 广播与落盘必须**同源**（实现期补上的不变量）

只用 `completed_at` 解决不了服务端侧的同款竞态：若广播时取一次墙钟、落盘时
再取一次，落盘值必然更晚。而另一端收到广播会立刻发 `thread/markRead`——它写下
的已读水位于是低于稍后落盘的完成水位，服务端判为「读完之后又跑完了一轮」，
**重连后蓝点复活**（正是用户报的现象）。

因此 `completed_at` 在 `Translator::finish_turn` 里**一次性**定下，随通知发出，
并由 `persist_and_finish_turn` 复用同一个值落盘。`run_blocks` 直接失败路径同理。
钉住它的测试：`the_persisted_completion_is_not_later_than_the_broadcast_one`
（未修时实测落盘比广播晚 11ms）。

### 4.3.2 append 失败仍记完成时间

`append_turn` 写日志失败与「回合是否结束」是两件独立的事：日志没存下来，但
回合确确实实结束了、用户也在流里看到了输出。若把完成时间的记账绑在 append 成功
分支上，这一轮就会在未读口径里凭空消失——另一端重连后毫无提示，直到下一轮跑完。
故 `mark_turn_completed` 位于 append 成功/失败**之外**，仍只在 `turn_id.is_some()`
时写（`clear` / `compact` / `SetPreserveThinking` 不是回合结束）。
钉住它的测试：`a_failed_append_still_records_the_completion`。

### 4.4 `thread/listAll` 暴露两个水位

`thread_summary_json_with_status`（`server.rs:4206`）增加 `last_read_at` /
`last_completed_at` 两个键。该函数同时服务分组列表与 `collect_pinned`
（`server.rs:4049`），改一处即两处生效。

### 4.5 投递分层

`thread/readStateChanged` 归 **`Delivery::List`（恒推）**，与
`thread/modelChanged` / `thread/permissionModeChanged` 同一族。

理由与既有注释一致（`protocol.rs:591` 附近）：远程客户端只为少数会话订阅内容，
若把它当内容层过滤，订阅窗口外会话的「已读」就同步不到另一端——正是本 bug
的形态。它只带两个时间戳、不带正文，恒推不违背「内容按需订阅」。

## 5. 客户端改动

### 5.1 `ThreadSummary` 增加两个可选时间戳

`desktop/src/lib/protocol.ts` 的 `ThreadSummary` 增加
`last_read_at?: number` / `last_completed_at?: number`。旧服务端不发送，
读取时按 `0` 处理。

### 5.2 `ThreadStore` 从「置布尔位」改为「合并水位并派生」

- `view(id)` 内部新增两个水位（`lastReadAt` / `lastCompletedAt`），
  `unread` 改为由二者派生而非独立状态。
- `seed(threads)`：对每条 summary 按 `max` 合入两个水位，重算 `unread`。
  这是「离线期间跑完的回合重连后仍亮蓝点」的唯一来源。
- `turn/completed`：
  - 非当前会话 → 合入 `completed_at`（缺失则用本地时钟），`unread = true`。
  - 当前会话 → 合入 `completed_at`，并由 App 发 `markRead`：用户正看着，
    服务端水位必须跟上，否则重连后蓝点会「复活」。
- `thread/readStateChanged` → 合入水位、重算 `unread`。**这是跨端清除蓝点的入口。**
- `select(id)` → 本地立即清点（保持「打开即清」的手感），App 随即发
  `thread/markRead`。

### 5.3 App 接线

- `selectThread` 成功后发 `thread/markRead`（不 await、失败忽略：已读是附加
  能力，绝不能因为它失败而卡住切换）。
- 当前会话收到 `turn/completed` 后同样发一次。
- `onNotification` 增加 `thread/readStateChanged` 分支（走 `applyNotification`）。

## 6. 数据流

```
端 A 点开会话 x
  → ThreadStore.select(x)：本地 unread = false（即时手感）
  → RPC thread/markRead { threadId: x }
  → 服务端 last_read_at[x] = max(旧, now)，落盘
  → 广播 thread/readStateChanged { x, last_read_at, last_completed_at }（List 层，恒推）
  → 端 B 收到 → 合入水位 → unread = (completed > read) 为假 → 蓝点消失
```

```
会话 y 在端 B 离线时跑完
  → 服务端 last_completed_at[y] 落盘
  → 端 B 重连 → thread/listAll 带出 last_read_at=0 / last_completed_at=T
  → seed 合入 → unread = (T > 0) 为真 → 蓝点亮起
```

## 7. 边界与错误处理

| 情形 | 行为 |
| --- | --- |
| 旧 meta 缺两个字段 | 读成 `0/0` ⇒ `unread = false`（与今日行为一致，不凭空造未读） |
| 旧服务端不发时间戳 | 客户端按 `0` 处理；`turn/completed` 的 `completed_at` 缺失时用本地时钟 |
| `markRead` 未知 thread | `-32011`，客户端忽略错误 |
| `markRead` 值未推进 | 不写盘，但照常广播一次（廉价收敛信号），回当前值 |
| 时钟回拨 | 写入一律取 `max(旧值, now)`，不会倒退 |
| `clear` / `compact` | 不写 `last_completed_at`（不是回合结束） |
| 当前会话跑完 | 本地清点 + 上报已读，两端都不亮 |

## 8. 测试

**Rust（`cargo test -p yi-agent-app-server --lib`）**

- `mark_read_persists_and_is_monotonic`：两次标记，`last_read_at` 只增不减，
  第二次不产生额外写盘。
- `legacy_meta_without_read_fields_reads_as_zero`：旧 JSON 反序列化后两字段为 0，
  且未读判定为假。
- `turn_completion_records_last_completed_at`：跑完一轮后 meta 的
  `last_completed_at > 0`，且 `updated_at` 的语义不变。
- `clear_and_compact_do_not_record_completion`：这两条路径不写
  `last_completed_at`。
- `read_state_changed_is_a_list_delivery`：`delivery() == List`。
- `read_state_reaches_an_unsubscribed_client`：未订阅该会话的客户端仍收到
  `thread/readStateChanged`（钉住「窗口外也能同步已读」）。
- `list_all_reports_read_state`：`thread/listAll` 的条目带两个水位。

**TypeScript（`cd desktop && npx vitest run`）**

- `threadStore.test.ts`：seed 从水位派生未读；`readStateChanged` 清除未读；
  非当前会话的 `turn/completed` 置未读；`select` 清未读。
- `App.test.tsx`（直接复现用户报的问题）：两个客户端，A 选中会话并发
  `thread/markRead`，服务端广播 `readStateChanged` → B 的蓝点消失。

## 9. 风险

- **协议改动**：新增两个 summary 字段 + 一个通知 + 一个 RPC。旧客户端忽略未知
  字段与通知即可，无需版本协商；字段缺省语义保证零迁移。
- **写盘频率**：`markRead` 在用户切换会话时发生，频率与人手动作为同一量级；
  值未推进时不写盘，重复点击无开销。
- **桌面端行为变化**：桌面现在也会上报已读、也会被手机清除蓝点。这是本设计
  的全部目的，需在 `docs/project-management/desktop.md` 明确记录。
