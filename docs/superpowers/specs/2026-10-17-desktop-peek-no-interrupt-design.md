# 设计：桌面端「查看运行中会话」不得打断在跑的回合

日期：2026-10-17
状态：待评审
关系：来自 2026-10-17 看板缺陷调查的范围 C（独立小卡）。

## 1. 背景与问题

### 1.1 「看一眼」会打断正在跑的回合

`thread/resume` 在服务端的实现里**先中断在跑的 turn**，再回放历史。
`yi-agent-app-server/src/server.rs:3656`（`"thread/resume"` 分支内）：

```rust
// 若该 thread 仍在内存且有活跃 turn，先请求中断，再等待 driver
// 落盘完成，否则紧随 turn/completed 的 resume 会读到尚未写入的历史。
interrupt_and_wait_for_persist(&mut threads, &mut turn_rx, &thread_id).await;
```

`interrupt_and_wait_for_persist`（`server.rs:6054`）取 `active_turn_id` →
`interrupt_tx.try_send(tid)` → 让 agent loop 在最近的检查点取消。
这是**有意的设计**：resume 之后要回放历史，若不先落盘会读到半截数据。

### 1.2 桌面端没有保护，iOS 有

现有防护只覆盖远端客户端。`desktop/src/App.tsx` 的 `selectThread` 里：

```ts
// Remote-only (S2): keep this thread in the warm subscription window, and
// catch up a cold-but-running thread with the read-only `thread/readItems`
// instead of `thread/resume` — resume interrupts the in-flight turn, which
// is exactly what a passive "look at it" must not do. Desktop (stdio) never
// subscribes: its single client receives everything.
if (isRemoteClient()) { /* running → readItems, return */ }
```

`isRemoteClient()` 为真仅当持久化了 relay 绑定（`desktop/src/lib/platform.ts:20`），
即**只有 iPhone 端**。桌面端（stdio）落到的是下面的 `thread/resume` 全量回放路径。

而看板卡会话由宿主**内部**创建（`server.rs:7258 launch_inner`），从没经由桌面端
`thread/start`，所以它永远不在 `warm` 集合里。于是：

> **在桌面上点开一张「正在跑」的看板卡会话，必然走 `thread/resume`，必然打断那一轮 turn。**

### 1.3 该行为在项目内有三处自证

| 位置 | 内容 |
|---|---|
| `server.rs:3656` | 实现：resume 分支先 `interrupt_and_wait_for_persist` |
| `desktop/src/App.tsx:895`（前身） | 注释："resume interrupts the in-flight turn, which is exactly what a passive 'look at it' must not do" |
| `server.rs:21645-21648` | 测试注释："`thread/readItems` 对**正在跑**的会话是只读的…这正是它相对 `thread/resume` 的关键差异（resume 会先 interrupt 在跑的 turn）" |

### 1.4 危害：它把「停」提前到「分支还空着」的时刻

看板调度器把「会话 Idle」直接当成「卡做完了」（`card_scheduler.rs:92-108`），
不做任何「是否产生过提交」的检查。因此一次打断会让卡在**首个提交之前**被判
`awaiting_merge`（若此刻分支恰好是空的，还会被空分支假合并判成 `done`，见
`merge.rs:46-54` 的 `git merge-base --is-ancestor`）。范围 A 的卡负责修判据，
本卡负责**消除那个不必要的打断本身**。

### 1.5 实测暴露的第二个缺陷：重连后陈旧 running 会永久阻断 resume

在实现本卡（验证 `running → 不 resume`）时，实测发现一条既有隐患：

1. 会话在跑 → `thread/status/updated` 把 `ThreadStore.statusSource` 标为 `live`。
2. 侧车崩溃重启。`exited` 处理器清了 `warm` 与订阅窗口（`App.tsx:1480-1486`），
   **但没有清 `statusSource`**。
3. 重连握手 `listAll` 报该会话为 `idle`（新进程里它是冷的），
   但 `seed()` 对 `live` 值不覆盖（`threadStore.ts:109`）→ **状态陈旧为 `running`**。
4. 在改动前，`selectThread` 无论如何都会 `resume`，因此这个陈旧值只是徽标骗人，
   会话仍会被重建——缺陷被 resume 掩盖。
5. **本卡改动之后**，`running` 意味着跳过 resume → 新进程里 agent **永不重建**
   → 用户发消息 → 状态 `running` 选 `turn/interject` → 服务端
   `prepare_turn_core` 的 `threads.get_mut()` 为 `None` → **`-32011 unknown thread`**
   （`server.rs:7897`）。
6. `sendMethodMismatchCode`（`App.tsx:127-130`）只认 `-32013 / -32012`，
   **不认 `-32011`** → 无法自愈，发送永久失败。

所以本卡必须一并清掉那条陈旧推送状态，否则修掉打断的同时会引入一个更隐蔽的死锁。

## 2. 目标与非目标

**目标**
1. 桌面端「只是看一眼」一个运行中的会话时，**不打断**在跑的 turn；内容照常可见并持续更新。
2. 让「运行中 → 只读补齐」成为**两种客户端共有**的不变量，而不是远端专属行为。
3. 不引入回归：冷且非运行中的会话仍走 `resume` 全量回放（它是冷会话唯一的就绪途径）。
4. 修掉 §1.5 的陈旧状态死锁。

**非目标（YAGNI）**
- 不改服务端 `resume` 语义（它仍需为冷会话重建 agent、恢复 mode/effort、绑定 process watcher）。
- 不新增协议 RPC。
- 不修调度器误判 Done（范围 A，另一张卡）。
- 不改 iOS 现有行为（它已经对了）。
- 不做「运行中会话的 agent 预热」——纯"看"不预热，与 iOS 现有行为一致。

## 3. 关键约束（实测确认，决定实现细节）

| # | 约束 | 证据 |
|---|---|---|
| C1 | `turn/start` 对**冷会话**返回 `-32011` | `server.rs:7699-7701` → `RpcError::unknown_thread`（`server.rs:7897`） |
| C2 | 桌面端是 `Feed::All`，**本就实时收全部内容通知** | `broadcast.rs:21-27`（`enum Feed { #[default] All, Only(..) }`） |
| C3 | `readItems` 对运行中会话是只读的，已有测试钉住 | `server.rs:21648` `read_items_does_not_interrupt_a_running_turn` |
| C4 | `listAll` 的 summary 带 `status` 字段 | `server.rs:5968-5986`（`"status": thread_status(..)`） |
| C5 | 状态可能陈旧：`statusSource` 保留 `live`，重连不清 | `threadStore.ts:47/109`；`App.tsx:1480-1486` |
| C6 | `-32011` 不在发送自愈的白名单里 | `App.tsx:127-130` |

## 4. 方案

改两处，语义单一。

### 4.1 `selectThread`：把「运行中 → 只读」提为共有不变量

`desktop/src/App.tsx` 的 `selectThread` 中，把最外层的 `if (isRemoteClient())` 拆开：

- **运行中**（`status === "running" || status === "awaiting_approval"`）：
  对**两种客户端**都发只读 `thread/readItems` 补齐内容，然后 `return`，**不 resume**。
- **远端专属**行为保持不动：`thread/subscribe` 只对远端调用
  （桌面是 `Feed::All`，订阅语义不适用），且「已在订阅窗口内 → 纯切视图」的
  早退仍只对远端生效。
- **非运行中的冷会话**：行为完全不变，仍走 `thread/resume` 全量回放。

为避免与函数内已有的 `const view = store.peek(id)` 重复声明，新引入的视图读取
用独立名字 `curView`，运行标志用 `running`。

**取舍**：纯"看"运行中会话时不再预热 agent，且**发送也不会补齐**——运行中会话的发送
走 `turn/interject`（`App.tsx`），永不 `resume`（C1 保证的 `resume` 只发生在冷会话）。
故这两项须分别对待：**权限模式**可从已在手的 `thread/listAll` 快照重新推出（快照恒带
`permission_mode`；`ThreadStore.seed` 只补「未知」、绝不覆盖已知值），chip 在 readItems
路径上照常正确；**reasoning effort** 在清单条目里没有对应字段，也没有读取它的 RPC
（本卡不改服务端、不新增 RPC），故此路径上它只能显示为「未设置（继承 X）」，直到该会话
下一次经 `resume`（被重新冷选中，或本轮结束后）才落定——这是**已接受的限制**。
纯"看"不预热与 iOS 现有行为一致，且 iOS 已用测试固化（`App.test.tsx` 的
`catches a cold running thread up with readItems instead of resume`）。

### 4.2 `ThreadStore.forgetPushStatuses`：作废崩溃前推送的运行状态

新增方法（`desktop/src/lib/threadStore.ts`）：

```ts
forgetPushStatuses(): void { this.statusSource.clear(); }
```

在 `App.tsx` 的 `exited` 处理器里，与 `warm.current.clear()` 同处调用它。

**为什么清 `statusSource` 而不是直接改 `status`**：清除后 `seed()` 会按下一份
`listAll` 快照重新落值。真实仍活着的会话也会随之从 `running` 复位——因为新进程
会把它们真正重建（resume 或首次发送），此后推送再把状态标回 `"live"`。
直接写 `status = "idle"` 反而会伪造一个未经服务端确认的状态。

## 5. 数据流（改后）

```
用户点开会话
  └─ selectThread(id)
       ├─ 远端 && 已在订阅窗口 → 纯切视图（不变）
       ├─ 远端 → thread/subscribe（不变）
       ├─ store.peek(id).status ∈ {running, awaiting_approval}   ← 两种客户端共有
       │    └─ thread/readItems(after=lastServerItemId) → upsertItems → return
       │       （只读：服务端不碰 turn）
       └─ 否则（冷且非运行中）→ thread/resume 全量回放（不变）
```

```
侧车 exited
  └─ client.dispose(); warm.clear(); store.forgetPushStatuses(); subWindow.reset(); reconnect
       └─ 重连握手 listAll → seed() 用服务端快照重新落 status（不再被陈旧 live 挡住）
            └─ selectThread 于是能对真正冷的会话走 resume（agent 被重建）
```

## 6. 错误处理

- `readItems` 失败：保持既有语义——**静默**。补齐是附加能力，读失败就保留已有内容，
  不改动会话状态，更不因一次读失败而退回 `resume`（那会重新引入打断）。
- `forgetPushStatuses` 是纯内存操作，无失败路径。

## 7. 测试策略

全部落在 desktop 侧（前端不在 CI，故本地必须跑全量）。

**新增（4 条）**
| 测试 | 断言 |
|---|---|
| `App.test.tsx` · `does not interrupt a running thread on the desktop build` | `listStatus=running` + 无 relay 绑定（桌面）→ 发 `thread/readItems`；`thread/resume` **0 次**；`thread/subscribe` 0 次 |
| `App.test.tsx` · `still resumes a cold idle thread on the desktop build` | 回归护栏：`idle` 冷会话 → 仍发 `thread/resume`，且不发 `readItems` |
| `App.test.tsx` · `re-resumes a thread after a crash even though its status was live-running` | 先推 `thread/status/updated(running)` 造出 live 状态 → 断开重连 → 新客户端上必须出现 `thread/resume`（陈旧状态不得阻断） |
| `threadStore.test.ts` · `forgetPushStatuses lets a fresh listing re-seed a stale running status` | 推 running → `seed(idle)` 不覆盖（仍 running）→ `forgetPushStatuses()` → `seed(idle)` 生效（变 idle） |

**更新（2 条，仅换等待锚点）**
`App.test.tsx` 的 `falls back to turn/start when turn/interject answers -32013` 与
`does not ping-pong when both methods fail...` 用 `waitFor(resume)` 作为「应用已加载」的
锚点。running 会话现在正确地不再 resume，故锚点改为 `waitFor(readItems)`。
这两条测的是「发送方法自愈」，与 resume 无关，改锚点不改变其被测语义。

**回归护栏**
- iOS 既有两条（`catches a cold running thread up with readItems instead of resume`、
  `never subscribes on the desktop build` 的 subscribe 断言）必须仍全绿。
- `tsc --noEmit` 无输出；`vitest run` 全绿且用例数不减。

## 8. 验收（实测已通过）

改动落盘后实测：

| 检查 | 结果 |
|---|---|
| `tsc --noEmit` | 无输出 |
| `vitest run`（全量） | **91 文件 / 1144 测试全绿**（改动前 1140，+4 新增） |

> 说明：按既定习惯，本 spec 与配套 plan 里的代码先在本地落盘实测（编译 + 全量测试），
> 通过后回滚工作区，再把验证过的代码写进文档。上表即那次实测的结果。

## 9. 风险

| 风险 | 缓解 |
|---|---|
| 与 `frontend-architecture` 卡冲突（它计划把 `selectThread` 搬进 `useSessionState` hook） | 该卡截至目前**未执行** Task 4（worktree 无 `desktop/src/hooks/`，`App.tsx` 仍有 12 处 `selectThread`）。本卡改动集中在 `selectThread` 内的一个 `if` 块 + `ThreadStore` 新增方法；plan 里写前置检查，若届时函数已搬走，按同一逻辑所在处落点 |
| 纯"看"运行中会话不再预热 agent | 与 iOS 现有行为一致；权限模式由已在手的 `thread/listAll` 快照补回（`ThreadStore.seed` 只补「未知」、绝不覆盖已知值），故 chip 在 readItems 路径上正确；reasoning effort 该路径上不可得（清单无此字段、无读取 RPC），显示为「未设置」直到该会话下次经 `resume`——此为已接受的限制。发送**不会**补齐：运行中会话走 `turn/interject`，永不 `resume`（C1 保证的 `resume` 只在冷会话上） |
| 陈旧状态清过头，把真实活着的会话也标成 idle | 状态随即由下一份 `listAll` 快照与服务端推送重新落值；plan 中以集成测试钉住"重连后仍能 resume" |
