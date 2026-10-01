# 桌面端受管后台进程可视化设计

**目标：** 让 desktop（Tauri GUI + `yi-agent app-server` sidecar）能看见、追尾、终止
agent 通过 `process_start` 起的后台进程——把 TUI 已有的「受管进程」信息架构，经
app-server 的 JSON-RPC 线协议搬到 GUI。

**状态：** 已设计，待实现。

**相关文档：**

- `docs/superpowers/specs/2026-08-10-managed-background-processes-design.md`（受管进程机制本身）
- `docs/superpowers/specs/2026-08-15-tui-managed-process-status-design.md`（TUI 侧可视化，本设计的信息架构来源）
- `docs/superpowers/specs/2026-09-30-tui-subagent-runtime-preference-design.md`（右列 tab 的既有语义参照）
- `docs/project-management/desktop.md`（desktop 模块现状与验证命令）
- `docs/project-management/yi-agent-tools.md`（进程工具现状）

---

## 1. 问题

### 1.1 现状

agent 能起后台进程，但 GUI 看不见它们。

- **能力存在且已在桌面链路注册。** `naked = false` 时 `build_tool_setup*` **总是**注册
  `ProcessManager` 支撑的进程工具（`yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:133`
  的注释、注册点 `:215-220`），app-server 走的正是这条路径。
  `ProcessManager::start`（`crates/yi-agent-tools/src/process/manager.rs:272`）以
  `.kill_on_drop(false)` spawn 后挂着 reader/waiter task **立刻返回**，进程跨 turn 长驻。
- **desktop 侧零可视化。** `desktop/src/lib/protocol.ts` 无任何 process 类型；组件目录无
  进程面板。唯一沾边的是 `process_start` 作为普通工具调用被 `ToolCallCard` 渲染成一份
  **静态** JSON 结果（`desktop/src/lib/toolSummary.ts:11`），没有实时状态、没有输出追尾、
  没有 kill 入口。
- **TUI 有一整套，可直接当模板。** 状态栏计数 `N proc running`
  （`crates/yi-agent/src/tui/statusbar.rs:170-222`）；Runtime 弹窗 `Tab` 在
  `BashTasks / Processes / Agents` 间切换（`crates/yi-agent/src/tui/process_popup.rs:7-11`），
  列表展示 `process_id | name | pid | elapsed | status`（`:152-165`），详情展示
  `status/ready/elapsed/exit_code/cwd/cmd/on_exit` + stdout/stderr 分栏 +
  `[k] kill` / `[f] follow`（`:191-253`），数据靠 `process_manager.subscribe()` 驱动
  （`crates/yi-agent/src/tui/app.rs:325-326`）。

### 1.2 根因：app-server 从没接这根线

- app-server 的方法清单（`thread/*`、`turn/*`、`agent/*`、`workspace/*`）**没有**任何
  `process/*` 方法；`grep -rn "ProcessEvent\|ProcessManager" crates/yi-agent-app-server/src/`
  零命中。
- 装配层把 manager 建出来后就丢弃：
  - 基础路径 `bootstrap_agent`（`runtime/bootstrap.rs:299`）经 `build_tool_setup_with_switch`
    → `build_tool_setup_with_controller`（`:215-220`）把 `ProcessManager` move 进
    `register_process_tools`，而 `ToolSetup`（`:100-108`）只有
    `tools/catalog/system_prompt/mcp` 四个字段，**没有带出 manager**。
  - 委派路径 `build_runtime_tooling`（`app-server/src/server.rs:596-632`）自建一份
    `ProcessManager` 并同样丢弃，`RuntimeTooling`（`server.rs:63`）只有
    `registry/permission` 两个字段。
- 结果：app-server 拿不到任何 `Arc<ProcessManager>`，无从 list / subscribe / kill。

### 1.3 数据层已就绪（这是本设计成本可控的原因）

- `ManagedProcessSnapshot`（`manager.rs:40`）与 `ProcessStatus`（`:30`）均已
  `derive(Serialize)`，且 `ProcessStatus` 带 `#[serde(tag = "state", rename_all = "snake_case")]`
  ——天然是 GUI 友好的 JSON，协议层无需新造类型。
- `list()`（`:401-409`）给快照、`subscribe()`（`:268`）给 `broadcast::Receiver<ProcessEvent>`
  （事件枚举 `:191-208`）、`read(cursor, max_bytes)`（`:411` 起）给带 `next_cursor` /
  `truncated` 的增量读——TUI 用的就是这套，前端照搬即可。

---

## 2. 决策记录

按 brainstorming 逐项确认（用户决策）：

| # | 议题 | 决策 | 理由 |
|---|------|------|------|
| Q1 | 形态 | **右侧可折叠 Rail**，与「子 agent」并列成 tab | 后台进程是需持续盯着的长跑任务，藏在计数点后面违背做可视化的初衷；与既有 `SubagentRail` 同构，实现与测试可直接照搬 |
| Q2 | 作用域 | **只看当前对话** | 后端 `ProcessManager` 本就是 per-thread（`server.rs:596`）；且 cwd 就是 thread 的 workspace，跨对话汇总会立刻引出「同名进程属于谁」「kill 作用于哪个 manager」的归属问题 |
| Q3 | 实时性 | **分层**：列表走状态推送，输出走按需增量拉 | 状态变化（活/ready/退出码）低频且值得推；stdout 高频，只在用户盯着详情时拉，避免把终端输出灌进 GUI 事件通道（与 TUI 只在详情页 `block_on(read)` 同一种取舍） |
| Q4 | 详情操作 | **只读 + Kill（带确认）** | 有 kill 才是真可用的面板；GUI 直接起进程（`process/start`）超出目标，属 YAGNI |
| Q5 | 输出呈现 | **分栏（stdout/stderr）+ 可暂停追尾** | 分栏复用现成数据结构、零后端改动；「翻看历史时不被新输出顶跑」是此类面板的刚需 |
| Q6 | chrom 行为 | **整列可收起，tab 条在展开态列头、收起态细条带计数** | 与既有 `SuperpowersKanbanCollapsedStrip` 的收起语义一致；顺带修掉 `SubagentRail` 收起后无法重开的既有缺陷 |

---

## 3. 架构与数据流

一句话：把 TUI 已有的「受管进程」信息架构，经 app-server 的 JSON-RPC 线协议搬到
desktop，右侧区域升级成 tabbed（子 agent | 后台进程）。

```
ProcessManager (per-thread)
   ├─ list()       ──→ RPC process/list     ──→ 列表快照（进 tab / 收到通知后重拉）
   ├─ subscribe()  ──→ 通知 process/updated ──→ 列表刷新（仅状态变化时）
   └─ read(cursor) ──→ RPC process/read     ──→ 详情增量追尾（仅详情打开时轮询）
```

- **列表**：订阅 `ProcessEvent`，只把 `Started / Ready / Exited / Killed` 转成
  `process/updated` 通知；`Output` 事件**不转发**。前端收到通知即重拉 `process/list`。
- **详情输出**：仅当详情面板打开时，前端用上一轮的 `next_cursor` 轮询 `process/read`；
  关闭、切 tab、切对话即停。
- **作用域**：所有方法带 `thread_id`，后端解析到该 thread 生效的 manager（§4.2）。

### 3.1 顺带修复：右列收起后无法重开

`SubagentRail` 目前收起后**没有任何办法重开**——`desktop/src/components/SuperpowersKanbanCollapsedStrip.tsx:5-7`
的注释明确点名「the sibling 「子 agent」 rail collapses with no way to reopen」。
本设计让右列成为承载两个 tab 的区域并赋予整体收起态，必须同时补上收起细条（带 tab 计数），
否则会把既有缺陷固化进新结构。

---

## 4. 后端实现

### 4.1 协议新增（3 方法 + 1 通知）

命名沿用既有 `domain/verb` 风格。

| 方法 | 参数 | 返回 |
|------|------|------|
| `process/list` | `{ thread_id }` | `{ processes: ManagedProcessSnapshot[] }` |
| `process/read` | `{ thread_id, process_id, cursor?, max_bytes? }` | `ProcessReadResult`（含 `next_cursor` / `truncated` / `status`） |
| `process/kill` | `{ thread_id, process_id }` | `{ ok: true }` |
| `process/updated`（通知） | — | `{ thread_id, process_id, state }` |

- 快照与状态枚举**直接复用** `ManagedProcessSnapshot` / `ProcessStatus`；协议层只做包装，
  不新造类型。
- `process/updated` 只由 `Started / Ready / Exited / Killed` 触发；`Output` 不转发。
- **未知 thread**：`process/list` 返回**空列表**而非错误（切走再切回、thread 已删除都是
  正常路径）。`process/read` / `process/kill` 的未知 `process_id` 返回标准 JSON-RPC 错误。
- **滞后丢帧可接受**：`broadcast` 有界，缺帧时前端每次收到通知都重拉 `process/list` 作为
  权威数据源；即使通知全丢，详情里的 `process/read` 也自带 `status`，用户仍能看到真实状态。

### 4.2 接线改动：manager 与其注册表同行

**这是本设计唯一有技术风险的改动。** 建立一条不变量——**manager 与它所支撑的工具注册表
同行**。

改动点：

1. **`ToolSetup` 带出 manager** — `runtime/bootstrap.rs:100` 增加
   `process_manager: Arc<ProcessManager>`；`:215-220` 不再把它 move 进
   `register_process_tools` 就丢，而是同时放进 `ToolSetup`。
2. **`AgentBootstrap` 带出 manager** — `AgentBootstrap`（`bootstrap.rs:259`）加
   `process_manager` 字段，由 `bootstrap_agent`（`:299`）填充，与既有 `catalog`
   （`:277`）的携带方式同构；app-server 的 `BuiltAgent`（`server.rs:49`）加一个字段。
3. **委派路径带出 manager** — `RuntimeTooling`（`server.rs:63`）加 `process_manager`；
   `wrap_for_delegation`（`server.rs:636`）在换注册表时**一并换 manager**。
4. **thread 记录保存生效的 manager** — 每个 thread driver 订阅**它生效的那一份** manager
   的 broadcast，转成 `process/updated` 通知（对标 TUI 的 `tui/app.rs:325`）。

> **必须避免的陷阱：** app-server 里有**两处**各自创建 `ProcessManager` 且都丢弃——基础
> 路径 `bootstrap_agent`（`bootstrap.rs:299,313`）与委派路径 `build_runtime_tooling`
> （`server.rs:609-629`）。委派成功后是**后者**替换了注册表，所以「该看哪个 manager」必须
> 跟「哪个注册表生效」绑定。否则 git 项目里进程面板会显示一个空列表，而 agent 明明能起进程。
> §7 的测试判据必须能抓住这一点。

**被否的替代方案：** 把 manager 所有权上提到 app-server、创建后传进 `build_tool_setup*`
（每 thread 只有一个 manager，更干净）。否掉的理由：`build_tool_setup*` 与
`bootstrap_agent` 被 TUI / headless 共用，改签名波及面大；「随注册表同行」用最小改动即得到
同一个正确结果。

---

## 5. 前端实现

### 5.1 右列结构

把当前松散的 `SubagentRail` 挂载（`desktop/src/App.tsx:726-733`）升级成 tabbed 容器。

```
RightRail (w-72, 可整体收起)
├── header: [子 agent 2] [后台进程 1]        [收起]
├── body:
│   ├── tab=子 agent → SubagentRail   （现状不动）
│   └── tab=后台进程 → ProcessRail    （新）
└── 详情态（两种 tab 同构）:
    ├── SubagentTrace （现状不动）
    └── ProcessDetail （新）

RightRailCollapsedStrip (w-8, 收起态)
└── 「子 agent 2 · 进程 1」+ 点击展开
```

- **默认 tab = 子 agent**（保持现状心智）；tab 标签带计数；切到没有条目的 tab 显示空态。
- **收起是整列收起**，收起细条同时显示两个计数（顺带修掉 §3.1 的既有缺陷）。
- **组件边界**（每个单一职责，可独立测试）：
  - `ProcessRail.tsx` — 列表 + 空态，纯展示 + `onOpen(id)` 回调（对标 `SubagentRail.tsx`）
  - `ProcessDetail.tsx` — 详情 + 追尾 + kill（对标 `SubagentTrace.tsx`）
  - `RightRail.tsx` / `RightRailCollapsedStrip.tsx` — tab 容器与收起态
  - `lib/processes.ts` — 状态存储（对标 `lib/subagents.ts` 的 `SubagentRailStore`）

### 5.2 列表卡片

展示：`name ?? process_id`、`pid`、状态徽标、`elapsed`、`command`（截断）。
状态配色沿用既有语义：starting/running 琥珀、ready 绿、exited 灰、killed/failed 红
（与 TUI `process_popup.rs:120-128`、`SubagentRail.tsx:10-24` 同一套）。

### 5.3 详情（决策 Q5-C）

- stdout / stderr **分栏两块**，默认追尾最新输出。
- 用户向上滚即**暂停追尾**，滚回底部自动恢复——对标 TUI 的 `scroll_locked`
  （`process_popup.rs:96-118`）。
- 数据用 `process/read` 的 `next_cursor` **增量轮询**（约 500ms），**仅在详情打开时**轮询；
  关闭、切 tab、切对话即停。
- 进程进入终态后：保留最终输出供查看、停止轮询，并显示退出码。

### 5.4 Kill（决策 Q4-B）

详情内一个 Kill 按钮 → 确认框（沿用 `ApprovalDialog` 的手感：Esc=取消、提交后防重复点击，
参考 `desktop/src/components/ApprovalDialog.tsx:24` 的 `submittedRef` 守卫）→
`process/kill` → 成功后刷新列表并把该条目显示为 killed。

### 5.5 列表膨胀：前端分组

`ProcessManager::list()` 返回**全部**条目，只在 spawn 失败或 shutdown 时移除
（`manager.rs:401-409`），所以长对话里 exited/killed 会持续累积。

本设计决定：**前端默认只显示活跃条目（starting/running/ready），终态条目折叠进一个
「已结束 (N)」分组**可展开查看。理由：后端不加 retention 逻辑（避免动 TUI 共用的 manager
语义），把纯展示层的取舍放在展示层。

---

## 6. 错误处理与边界

### 6.1 错误处理

- `process/list` 对**未知 thread** 返回空列表而非错误（见 §4.1）。
- `process/read` / `process/kill` 对未知 `process_id` 返回标准 JSON-RPC 错误，前端在详情内
  **内联**显示，**不清空已有输出**。
- `process/read` 的 `truncated` 为真（ring buffer 已滚掉早期输出，上限 256 KiB，
  `manager.rs:18` `DEFAULT_STREAM_CAP_BYTES`）时，详情顶部标注「较早输出已滚出」，
  而不是假装输出完整。
- **sidecar 事后才起：不适用。** `bridge.rs` 在 app 启动时即 spawn sidecar，没有「连接后补」
  的窗口。

### 6.2 边界

- **工具集重建**（同一 thread 重新 attach / `wrap_for_delegation` 触发）时，旧 manager 的
  进程会从新列表消失。这是**正确行为**（旧注册表已不生效），写在此处以免被当成 bug。
- **thread 删除 / 切走**：前端停止该 thread 的轮询与订阅，不做后台空转。
- **进程数上限 16**（`manager.rs:303`）：列表照常显示，只是 agent 再起会被拒。
- **无进程且无子 agent**：两个 tab 都空态，右列仍可整体收起。

---

## 7. 测试判据

**后端（`cd yi-agent-rs && cargo test -p yi-agent-app-server` / `-p yi-agent-runtime`）：**

- git 项目 + 委派成功时，`process/list` 反映的是**生效工具集**起的进程（而非那个被丢弃的
  manager 的空列表）——抓住 §4.2 的陷阱。
- 非 git cwd（委派降级）时同样可 list / read / kill。
- `process/read` 的 `cursor` 增量语义：两次读**不重不漏**。
- 未知 thread 返回空列表而非错误。

**前端（`cd desktop && npx vitest run`）：**

- tab 切换与计数。
- 收起 → 细条 → 展开往返（收起态可见计数）。
- 空态文案；列表卡片字段；终态组折叠/展开。
- 详情两栏渲染；追加输出时「停留底部则跟随、上滚则暂停」。
- kill 确认框：Esc 取消、防重复提交。

**整体：** `cd desktop && npx tsc --noEmit && npm test` + `cd desktop/src-tauri && cargo test`
+ `cd desktop && npm run sidecar:release && npm run tauri build`。

---

## 8. 非目标（YAGNI）

- 从 GUI 直接起进程（`process/start`）——超出「可视化已有进程」；`process_start` 本是
  `requires_confirmation: true` 的 agent 工具，GUI 直起会绕过 agent 的意图层。
- stdout/stderr 单流真交错——后端缓冲区不带时间戳，需改共用结构，成本不值，且与 TUI 行为分叉。
- 跨对话汇总进程——本轮按 per-thread 作用域（Q2-A）；真需要时可增量加聚合视图，不返工。
- 后端 retention / 终态条目清理——用前端分组替代（§5.5）。
- 进程输出的全文搜索 / 导出。

---

## 9. 成功判据

agent 通过 `process_start` 起一个长跑命令后：

1. 右侧「后台进程」tab 立刻出现该条目（状态 running、带 pid 与计时）。
2. 点开详情能看到实时输出并追尾。
3. Kill 能真正终止进程且列表转为 killed。
4. 切到别的对话不显示该进程。
5. 收起右列后，从细条能重新展开。

---

## 10. 影响面

- **新增后端**：`process/*` RPC 方法与 `process/updated` 通知；`ToolSetup` /
  `AgentBootstrap` / `BuiltAgent` / `RuntimeTooling` 各加一个 `process_manager` 字段。
- **新增前端**：`ProcessRail.tsx`、`ProcessDetail.tsx`、`RightRail.tsx`、
  `RightRailCollapsedStrip.tsx`、`lib/processes.ts` 及各自测试。
- **改动既有**：`App.tsx` 的右列挂载点；`protocol.ts` 加进程类型。
- **不动**：TUI（继续用自己的 `ProcessManager` 路径）、`ProcessManager` 本身、
  `SubagentRail` / `SubagentTrace` 的内部实现。
