# 桌面端「过期状态导致消息发不出」设计

**目标：** 修掉桌面 App 在「一轮已结束」之后发消息时报 `-32013 no turn is running`
的故障，并消除其根因——前端用一份可能过期的 thread 状态来决定 RPC 方法，
且对 `-32013` 没有恢复路径。

**状态：** 设计已确认，实现中。

**范围：** 只改 `desktop/src`（前端）。**不做**服务端改动：`turn/completed` →
落盘 → `idle` 推送 → 清 `active_turn_id` 的顺序、`-32013` 错误码语义都正确，
不动。`desktop/src-tauri` 亦不改。

---

## 1. 问题与证据

用户观察：Mac App 运行中，系统已经结束回复（界面上已经"停了"），此时发消息
报错 `no turn is running`；Cmd+R 重载后恢复正常。

### 1.1 根因 A：session 级状态判定与服务端 `active_turn_id` 语义不一致

`send()` 用 `turnActive`（由 `turn/started` / `turn/completed` 驱动）之外，
**另有一个独立的判定源**：`MessageInput` 的 `turnActive` 同时也是"Enter 键
= 发送还是中断"的判据（`MessageInput.tsx:51`/`:64`）。两个判定源各写各的：

- `session.turnActive` —— `turn/started` → true，`turn/completed` → false。
- `statuses[id]` —— `thread/status/updated` 驱动，且被 `thread/listAll` 快照覆盖。

两条流的到达顺序不保证一致（服务端 `turn/completed` 先于
`thread/status/updated: idle` 写出），于是存在一个窗口：
**`turn/completed` 已消费（界面看起来"结束了"），但缓存的 thread 状态仍是
`running`**。此时 `send()` 会发 `turn/interject`。

### 1.2 根因 B：缓存的 thread 状态会被过期快照永久污染

服务端每轮结束的顺序是：发 `turn/completed` → **落盘（append 当前轮的
items 与完整 messages 数组，再 touch meta）** → 推 `thread/status/updated: idle`
→ 通知主循环清 `active_turn_id`（`server.rs:1383-1391`，`:978-986`）。

前端一收到 `turn/completed` 就**立刻**发 `thread/listAll`
（`App.tsx:274`，`void refreshThreads()`，不 await）。该请求若落在上面
**落盘窗口** 内，服务端 `thread_status()` 仍读到 `Running`
（`server.rs:1061-1067`），返回的快照即 `status:"running"`。

`ThreadStore.seed()` **无条件覆盖**实时状态（`threadStore.ts:73-79`），没有
时序保护。于是快照把已由推送纠正为 `idle` 的状态翻回 `running`；而该轮已结束，
之后不会再有状态推送来纠正，**缓存永久停在 `running`**。

`seed()` 的注释自称"快照覆盖状态是安全的（服务端既是快照也是实时流的权威）"，
这个假设在 `turn/completed → 落盘 → Idle` 窗口下不成立。

### 1.3 根因 C：`-32013` 没有恢复路径（真正的致命点）

即便 A、B 任一处让前端误判，只要客户端能对 `-32013` 自愈，用户就不会看到
这个错误。但 `send()` 的 `catch`（`App.tsx:319-327`）只做两件事：写
`lastError`、撤回乐观气泡——**没有识别错误码、没有回退 `turn/start`**。

服务端注释明写这条契约是"返回 `-32013` 让客户端改走 `turn/start`"
（`server.rs:920-922`），设计文档 §D7 也写明"不活跃时用户提交 → 仍走
`turn/start`"——**但前端从未实现**。于是误判不是被自愈，而是直接暴露给用户，
并且消息丢失。

### 1.4 实测证据

在真实服务端（`run_with` + duplex harness）上抓 wire 帧序，一轮正常文本回合：

```
turn/started → status:"running" → <turn/start 响应> → item/started
→ item/delta → item/completed → turn/completed → status:"idle"
```

实测 `turn/completed` 与 `status:"idle"` 之间**隔着整个落盘耗时**；前端正是在
这个窗口里发 `thread/listAll`。

同时实测：单轮小对话里 `turn/completed` 之后立刻 `thread/listAll` 返回 `idle`
（窗口太小、稳赢）——这解释了该 bug 只在**长对话 / 大 payload** 时出现：每轮
落盘会重写完整 messages 数组（本机真实会话每 4 轮约 23KB）。

### 1.5 为什么 Cmd+R 能修好（决定性证据）

重载 webview **不重启 Rust 侧 sidecar 进程**，服务端的 `active_turn_id`、
`ThreadSession.status` 在 reload 前后完全一样。既然 reload 能修好，故障就只能
在前端进程内的缓存里——与上述定位一致（reload 后 `ThreadStore` 重建，状态从
`listAll` 重新取到正确的 `idle`）。

### 1.6 排除项（已验证不是原因）

- **不是服务端 `active_turn_id` 泄漏**：泄漏不会返回 `-32013`，且不会因
  reload 而恢复。
- **不是帧丢失**：Tauri `tauri-plugin-shell` 逐行读子进程 stdout
  (`process/mod.rs:466-504`，`BufReader::read_line`)，行完整投递。
- **不是服务端非法状态**：那会返回别的错误码，不会 `not_running`。

---

## 2. 修复

三层同时修：C 是直接止血（必须），A/B 让误判根本不再发生（预防）。

### 2.1 C：`send()` 对错误码自愈（止血）

把 `send()` 的"选方法 + 请求"抽成一个可重试的小循环，最多两跳：

- 首选方法按**缓存状态**判：`idle` → `turn/start`；否则 → `turn/interject`。
  （与既有实现一致，保留"界面显示运行中时 Enter 折叠进当前 turn"的语义。）
- 收到 `-32013`（`turn/interject` 但服务端无活跃 turn）→ 回退
  `turn/start`，**并顺手把缓存状态纠正为 `idle`**。
- 收到 `-32012`（`turn/start` 但服务端有活跃 turn）→ 回退
  `turn/interject`，**并顺手把缓存状态纠正为 `running`**。
- 每个方向最多重试一次，避免两个错误码互相弹跳造成的无限循环。

仅当前一请求确实失败时才尝试下一跳；两跳都失败才回滚乐观气泡并把错误写进
`lastError`（保持既有行为）。

判定必须读**原始错误对象的 `code`**（`formatError` 会丢掉它）。

### 2.2 B：快照不再回退实时状态（消除污染源）

`ThreadStore` 增加一张 `statusSource: Map<id, "live" | "snapshot">`，记录每个
thread 的状态**最后一次由谁写下**：`thread/status/updated` 推送 → `"live"`；
`thread/listAll` 快照 → `"snapshot"`；从未学到过 → 无条目。

`seed()` 只写 `statusSource !== "live"` 的 thread：

- **从未学到过**（无条目）→ 用快照值播种；
- **只被快照写过**（`"snapshot"`）→ 用更新的快照覆盖（如新建后首次列表）；
- **已被推送写过**（`"live"`）→ **不覆盖**，保留权威值；
- `drop()` 一并清掉条目，避免残留条目让重建的 thread 永远无法播种。

判据用一句话概括：**快照只播种，不覆盖推送写下的状态。**

不采用"视图是否存在"作为判据：`select()` 会先建视图（默认 `idle`），
此时还没有任何推送，这个默认 `idle` 与推送写下的 `idle` 在视图上无法区分，
会把"仅存在于视图的默认值"误判成权威值，导致该 thread 从此无法被播种。
这也是实现期新增测试
`seeds the status of a view that exists but never heard from the push stream`
钉住的行为。

### 2.3 A：不对的地方——输入框的 `turnActive` 已是对的

原设计（本文件初稿）打算把 `send()` 的**首选**判定也改成 `session.turnActive`。
实现时发现这条改不动，且原因值得记录：

`MessageInput` 在 `turnActive` 为真时把按钮渲染成 **"Stop"** 并绑定
`onInterrupt`，Enter 也走中断分支（`MessageInput.tsx:51`/`:64`）。
也就是说 **`turnActive === true` 时根本不存在可点的 "Send"**——"发送"这条路径
只在 `turnActive === false` 时可达。

于是"首选方法跟随界面"这件事，在 A 的层面**已经由 `MessageInput` 保证了**：
界面显示 Send 时 `turnActive` 必为 false，`statuses` 若仍是过期的 `running`，
才产生分歧。把首选判定改成 `turnActive` 会让这个可达场景下首选变成
`turn/start`，反而**破坏**"运行中把话折叠进当前 turn"的产品行为，并使既有
测试 `uses turn/interject instead of turn/start while a turn is running` 失去意义。

**结论：** 首选判定保留 `statuses[id]`（可点击路径下的最佳猜测），由 §2.1 的
错误码自愈负责兜住两侧的分歧窗口。这比"改判定源"更小、更准确。
`session.turnActive` 仍在自愈时被同步纠正（服务端才是权威）。

### 2.4 测试

**`desktop/src/App.test.tsx`（集成，主要）**

1. 回归：过期 `running` + 服务端 `-32013` → 自动发出 `turn/start`，不报错、
   气泡保留、文本送达。
2. 反向：过期 `idle`（`turnActive=false`）+ 服务端 `-32012` → 自动发出
   `turn/interject`，气泡保留。
3. 不无限循环：两跳都失败时，`turn/start` 与 `turn/interject` 各只出现一次，
   且 `lastError` 有值、乐观气泡被回滚。
4. 首选方法跟随缓存状态：`running` 状态（经 listAll 播种）下发送 → 首选
   `turn/interject`；`idle` 下发送 → 首选 `turn/start`。（既有测试
   `uses turn/interject ...` / `still uses turn/start ...` 即覆盖。）

**`desktop/src/lib/threadStore.test.ts`（单元）**

5. `seed()` 不回退既有状态：先收到 `idle` 推送，再来一份 `running` 快照，
   状态仍是 `idle`。
6. `seed()` 仍播种新 thread：本地没有视图时取快照值。
7. `seed()` 仍播种"视图已存在但从未收到推送"的 thread（`select()` 先建视图）。
8. `drop()` 后再 `seed()` 仍能播种（`statusSource` 不残留）。
9. `seed()` 不破坏 cwd/model 的播种（既有行为不变）。

**验证命令**

```
cd desktop && npm test
cd desktop && npm run build
```

---

## 3. 影响面

| 文件 | 改动 |
|---|---|
| `desktop/src/App.tsx` | `send()` 抽成"首选方法（按缓存状态）+ 错误码自愈（最多两跳）"；自愈时同步纠正 `session.turnActive` 与缓存状态 |
| `desktop/src/lib/threadStore.ts` | `seed()` 加"不回退既有状态"不变量；更新注释表述 |
| `desktop/src/App.test.tsx` | +4 个集成用例（自愈两向、不循环、首选跟随界面） |
| `desktop/src/lib/threadStore.test.ts` | +2 个单元用例（不回退 / 仍播种） |
| `docs/project-management/desktop.md` | 登记该修复与验证命令 |

服务端（`yi-agent-rs/`）与 `desktop/src-tauri/` 不动。

---

## 4. Non-goals

- **不改服务端错误码语义**（`-32013` / `-32012` 维持现状）。
- **不改服务端 `turn/completed` → Idle 的写出顺序**（语义正确，"结束"与
  "空闲"之间本就应有落盘完成的间隔）。
- **不引入协议级状态版本号**：本次用"快照不回退"的本地不变量即可闭合，
  版本号属于协议演进，留作独立议题。
- **不改 `refreshThreads` 的调用时机**：它仍负责刷新标题/时间/分组，
  只是不再污染状态。
- **不改 `send()` 的首选判定源**（保留 `statuses[id]`）：`MessageInput` 在
  `turnActive` 为真时按钮就是 "Stop"，"发送"路径只在 `turnActive === false`
  时可达，故首选判定必须是缓存状态，否则会破坏"运行中折叠进当前 turn"的
  产品行为。见 §2.3。
