# 重构第二波：app-server 分层（`server.rs` 拆除与 RPC 域收敛）

**状态：** 设计（待评审）
**日期：** 2026-11-03
**范围：** C2 / C3 两块，纯重构，**不改任何对外行为、不改线上协议、不改公共 API**

> 本文承接第一波设计（`2026-11-02-refactor-wave1-design.md`）。第一波 §8 明确把
> `server.rs`（7,590 行）、`rpc/thread.rs` 的 `handle()`、C4 装配统一、
> `main.rs` 拆分划为「C 线，不在本波」。本文只处理其中 **C2 + C3**。
>
> **C4（`build_tool_setup` 5 层套娃 + 三个入口重复装配）与 D3（`main.rs` 拆分）不在本文范围。**
> 它们依赖 C2 落地后的模块边界，单独出 spec。

---

## 0. 事实基线

全部在当前 `main` @ `e6aab02b` 上实测。测量脚本见 §6。

### 0.1 波次定位与已完成的先序项

仓库在 2026-11-02/03 已落地三个 store 拆分提交，**本文的假设全部基于拆分后的现状**：

| 提交 | 内容 | 对本文的影响 |
|---|---|---|
| `c8c445ac` | `ipc.rs` → `ipc/` 7 个子模块 | 无（app-server 只消费 `ipc::` 公共 API） |
| `e814ae51` | `RuntimeRepository` 99 方法 → `repository/` 12 子模块 | 无 |
| `40142da5` | `repository` 7 个自由函数 → `helpers` | 无 |

第一波的 **C1（`server.rs` 测试外移）也已独立落地**：`server.rs` 从 10,972 行降到
**7,590 行**，内联测试从 3,414 行降到 **33 行**，测试搬进 `server/tests/` **12 个文件**
（合计 **21,366 行**）。

**因此本文不含任何测试搬迁工作。** C2/C3 是纯粹的**生产代码**重组。

### 0.2 两个目标文件的构成（实测）

| 文件 | 总行 | 内联测试 | 生产行 | 顶层函数 | 最大单元 |
|---|---|---|---|---|---|
| `server.rs` | 7,590 | 33 | **7,557** | **125 个**（3 `pub` + 71 `pub(crate)` + 51 私有） | `serve()` **973 行**（L2802–3774） |
| `rpc/thread.rs` | 2,809 | 7 | **2,802** | `handle()` + **8 个** helper（1 `pub(crate)` + 7 私有） | `handle()` **2,347 行**（L70–2416） |

`server.rs` 前三大函数占 2,507 行（973 + 817 + 717），前八大函数占 3,744 行：

| 函数 | 行数 | 范围 |
|---|---|---|
| `serve` | **973** | L2802–3774 |
| `driver_tick` | **817** | — |
| `run_thread_driver` | **717** | — |
| `apply_session_command` | 299 | — |
| `start_thread_core` | 298 | — |
| `wait_for_plugin_query_socket_within` | 229 | — |
| `prepare_turn_core` | 221 | — |
| `build_runtime_tooling` | 190 | — |

> 行数口径：**相邻两个顶层 `fn` 起点之差**（含该函数与下一个函数之间的空行、
> 以及夹在中间的类型定义）。`serve()` 因此是 973：函数体止于 L3768 的 `}`，
> 其后还有 6 行尾随注释与空行到 L3774 才到 `apply_session`。
> 本波所有「函数行数」统一用这个口径，§6 的脚本可复现。

### 0.3 真正的根因：`AppState` 是「41 个字段的打包」

`rpc/mod.rs:331–489` 的 `AppState<'a>` 实测 **41 个字段，其中 8 个是 `&'a mut`**：

```rust
pub(crate) struct AppState<'a> {
    // 8 个可变量，全部是 serve() 的局部量
    pub(crate) uploads: &'a mut UploadRegistry,
    pub(crate) threads: &'a mut HashMap<String, ThreadSession>,
    pub(crate) pending_activation: &'a mut HashMap<String, Option<Arc<ThreadRoot>>>,
    pub(crate) process_watches: &'a mut HashMap<String, ProcessWatch>,
    pub(crate) trace_watches: &'a mut HashMap<String, TraceWatch>,
    pub(crate) children_watches: &'a mut HashMap<String, ChildrenWatch>,
    pub(crate) turn_rx: &'a mut mpsc::Receiver<TurnEvent>,
    pub(crate) pending_lead_replies: &'a mut HashMap<String, Vec<String>>,
    // 其余 33 个：28 个 &'a 只读引用 + 5 个按值(Scope/Duration/BuildAgent<bool/Option<&'a str>>)
}
```

18 个域 handler 的签名统一是 `async fn handle(state: &mut AppState<'_>, ...)`。

**结论：拆文件是廉价的（搬 fn + 调可见性），拆状态是昂贵的（改 8 个 `&mut` 的归属）。
本文只做前者。** 这也解释了为什么 C1 能在一个提交里完成、而 C2/C3 需要分步——
测试搬走不涉及借用关系，生产代码重排涉及。

### 0.4 `serve()` 的 973 行在做什么

`serve()` 内约 30 个局部量 + 一个 `tokio::select!`。其中 **18 个 `Domain::*` 分派臂
共 197 行**（L3217–3413，逐臂实测见 §2.3；15 个样板臂各 7–10 行，`Plugin` 43 行，
`Thread` 14 行，`Unknown` 1 行），其余是 tick 源与 transport 编排。

> ⚠️ 初稿写「约占 400 行，每臂 15–30 行」——**错**。实测整块 197 行，多数臂只有
> 7–10 行（宏收敛后能塌成 1 行，见 §2.3）。

关键：这些臂**已经是第二段式分派**（`domain_for(method)` → `rpc::<domain>::handle`），
所以改写它们不会改变分派语义。但**不能把这 18 段抽成一个 `dispatch()` 函数**——
`app_state!()` 是按名捕获 `serve()` 局部量的体内宏、15 个臂内联了 `continue`，
两者都无法跨函数（详见 §2.3）。可行做法是**在 `serve()` 体内用局部宏把 15 个样板臂
各塌成一行**，`Plugin` / `Thread` / `Unknown` 留原样。

### 0.5 `rpc/thread.rs` 的 `handle()` 实测：23 个臂 / 26 个 method

逐臂实测（`match req.method.as_str()`，L70–2416）：

| 臂起始行 | 行数 | method |
|---|---|---|
| 76 | 29 | `thread/list` |
| 105 | 51 | `thread/listAll` |
| 156 | 160 | `thread/start` |
| 316 | 57 | `thread/lead` |
| 373 | **386** | `thread/resume` |
| 759 | 107 | `thread/fork` |
| 866 | 46 | `thread/rename` |
| 912 | 134 | `thread/setModel` |
| 1046 | **182** | `thread/setReasoningEffort` |
| 1228 | 116 | `thread/setPreserveThinking` |
| 1344 | 75 | `thread/setPermissionMode` |
| 1419 | 46 | `thread/setPinned` |
| 1465 | 69 | `thread/setTag` |
| 1534 | 61 | `thread/markRead` |
| 1595 | 28 | `thread/group/{create,update,setMembers,delete}` ← **单个或模式臂覆盖 4 个** |
| 1623 | 86 | `thread/reorderPinned` |
| 1709 | **215** | `thread/delete` |
| 1924 | 62 | `thread/clear` |
| 1986 | 51 | `thread/compact` |
| 2037 | 113 | `thread/diff/read` |
| 2150 | 148 | `thread/view/read` |
| 2298 | 45 | `thread/subscribe` |
| 2343 | 66 | `thread/readItems`（臂体止于 L2408） |
| — | 8 | `_ => return Ok(Flow::Unknown)` + `match` 收尾（L2409–2416） |

**臂体合计 2,333 行**（L76–2408，与 §3.2 的分组表一致）；`handle()` 共 2,347 行
= 头 6 行（L70–75：`fn` 声明到 `match`）+ 臂体 2,333 + 尾 8（L2409–2416）。

**合计 23 个臂 / 26 个 method，无重复臂。** 差额 3 来自 `thread/group/*` 那个
或模式臂：一个臂同时匹配 4 个 method。

> ⚠️ 一处容易误判的地方，实施时不要重犯。用不同 grep 口径会得到不同数字，
> 都不是 23：
>
> | 口径 | 结果 | 为什么错 |
> |---|---|---|
> | `grep -c '"thread/'` 全文 | 31 | 把 `apply_group_mutation()` 内部 4 个臂也算进来了 |
> | `grep -E '^        "thread/'` 全文 | 27 | 同上（23 + 4 内层） |
> | 再把或模式的 `| "thread/…` 续行也算 | 30 | 或模式一个臂覆盖 4 个 method，续行不是新臂 |
> | 「30 再补上内层 4 个」 | 34 | 重复计入 |
>
> **正确做法**：只取 `handle()` 函数体内（L70–2416）、8 空格缩进、且**不以 `|` 开头**
> 的 `"thread/…"` 行，得 **23**。§6 给了这个脚本。
>
> `apply_group_mutation()`（L2630–2803）内部也有一个 `match method`，同样匹配
> `"thread/group/create"` 等 4 个字符串——这是最容易把臂数算成 27/31/34 的根源。

---

## 1. 全块共用规则

与第一波 §1 一致，补充两条针对 app-server 的：

1. **只搬不改语义。** 每个提交都是「同样的代码换个位置」。允许的改动仅限：
   可见性（`fn` → `pub(crate) fn` / `pub(super) fn`）、`use` 语句、路径前缀。
2. **公共 API 零变化。** `lib.rs` 导出面不变。特别是 `ok_response` / `err_response` /
   `write_response` / `write_notification` / `store_lookup` / `thread_cwd` / `item_id`
   等项，**必须从 `server.rs` 继续可达**。

   实测：`src/rpc/` 下 **18 个文件**、`src/server/tests/` 下 **3 个文件**
   （`mod.rs` / `transport.rs` / `turn.rs`，tests 目录共 12 个文件）写了
   `crate::server::…`。所以 `server.rs` 的 re-export 面必须覆盖这些符号。
3. **线上协议零变化。** `protocol.rs` 不动；`bindings-check` 必须无 diff。
4. **有外部引用的类型定义留在 `server.rs`，只搬函数。** `server.rs` 实测有
   **32 个顶层类型 + 6 个 `impl`**（`ChildrenWatch` / `ProcessWatch` / `TraceWatch` /
   `PluginQueryError` / `std` / `DeltaCoalescer` 的 `stop()`）。其中 **17 个类型被
   `src/` 下其他文件引用**，一旦移走，`rpc/*` 与 `server/tests/*` 的 import 全部失效：

   | 类型 | 行数 | 外部引用处数 |
   |---|---|---|
   | `RuntimeAttachments` | 81 | 3 |
   | `TurnEvent` (enum) | 22 | 3 |
   | `BuiltAgent` | 17 | 4 |
   | `ThreadCancelOutcome` (enum) | 6 | 4 |
   | `ChildrenWatch` / `ProcessWatch` / `TraceWatch` | 各 3 | 各 3 |
   | `ProjectRuntimes` | 1 | 8 |
   | `ThreadRoots` / `ViewFrames` / `WatchmanInstall` / `WatchmanUninstall` / `ResolvePlugin` / `EnsureDaemon` / `ClientScopes` / `ClientInitialized` / `WsDeviceRegistry` | 各 1 | 1–6 |
   | **合计** | **145** | — |

   这 **17 个类型共 145 行**，必须留在 `server.rs`。

   另外 **15 个类型共 158 行**只在 `server.rs` 内部使用，**可以随对应的函数搬走**
   （`ServeLauncher` 47、`PreparedTurn` 17、`IdleFacts` 14、`RuntimeTooling` 12、
   `TurnPrepareError` 11、`PluginSummary` 10、`SyncAction` 9、`PartialTurnInput` 6、
   `LeadGroup` 6、`SyncHint` 5、`LeadTickDelivery` 5、`Activation` 4、
   `PluginQueryError` 4、`DeltaCoalescer` 4、`LeadTarget` 4）。
   它们没有外部引用，搬进对应子模块更内聚；**但搬不搬都不影响正确性**，
   本波的选择是「就近搬」，因为它们与所在函数强相关（如 `RuntimeTooling` 只服务
   `build_runtime_tooling` 那 3 个函数）。

   `server.rs` 的行数守恒（用于校验搬移无遗漏）：**7,557 = 7,508（125 个函数的跨度，
   已包含夹在函数之间的类型/impl/空行） + 82（文件头 `use` 与首个函数前的类型定义）**。
   注意 7,508 这个「函数跨度」里已经包含了 32 个类型中位于函数之间的部分，
   所以 **145 这个「类型行数」不能直接与 7,508 相加**——它是 7,508 的子集。

   ⚠️ **`BuildAgent<'a>` 不在此列**——它定义在 `rpc/mod.rs:136`，不在 `server.rs`。
   初稿把它误列进来，已删。

   ⚠️ **初稿此处的数字全错**：它写「24 个类型 + 6 个 impl」「13 个被引用」
   「12 个类型共 109 行」「另外 12 个共 200 行」，实测是 32 / 17 / 145 / 158。
   且它的引用清单漏了 `RuntimeAttachments`（81 行，最大的一个）与
   `ClientScopes`、`ClientInitialized`、`PartialTurnInput` 等。

5. **单提交单模块。** 一个提交只搬一组函数，便于 `git show` 逐条核对。
6. **每提交跑**：`cargo test -p yi-agent-app-server`、
   `cargo clippy -p yi-agent-app-server --all-targets -- -D warnings`、
   `cargo fmt --all -- --check`。整块收尾跑 `just ci`。

---

## 2. C2 块：`server.rs` 按职责切模块

### 2.1 现状归类（125 个函数，逐名归组，实测行数为函数体跨度累加）

分组方式：脚本枚举 `server.rs` 全部 125 个顶层 `fn`，按名归入 12 组。
**互斥、无遗漏，函数数与行数合计精确等于 125 / 7,508**（可用 §6 的脚本复现）。

| 组 | 函数数 | 实测行数 | 是否超 1,000 |
|---|---|---|---|
| `driver` | 8 | **1,832** | ⚠️ 是 |
| `transport` | 11 | **1,588** | ⚠️ 是 |
| `turn_lifecycle` | 20 | **1,497** | ⚠️ 是 |
| `thread_index` | 25 | 639 | 否 |
| `agent_assembly` | 14 | 562 | 否 |
| `watchman` | 9 | 365 | 否 |
| `plugin_bridge` | 8 | 282 | 否 |
| `watchers` | 6 | 239 | 否 |
| `runtime_binding` | 8 | 175 | 否 |
| `rpc_io` | 9 | 148 | 否 |
| `task_bridge` | 4 | 99 | 否 |
| `migration` | 3 | 82 | 否 |
| **合计** | **125** | **7,508** | 3 组需二次分片 |

> **本文档初稿的表格有误**：各组合计 126 个函数 / 7,532 行，且 `runtime_binding`
> 写成 9 个函数 / 314 行（真实 8 / 175）、`plugin_bridge` 写成 497（真实 282）。
> 已按脚本重算修正。初稿还写了「121/125 归入上表，其余 4 个归入新增的 task_bridge
> 组」——但 `task_bridge` 本就在上表内，该句自相矛盾，已删。

⚠️ **三个组超 1,000 行，必须二次分片。** 这是本块第一组硬要求：

| 组 | 分片方案 |
|---|---|
| `driver` 1,832 | `turn_driver.rs`（`run_thread_driver` 717）/ `tick.rs`（`driver_tick` 817）/ `lead.rs`（`lead_tick_once` 74、`resolve_lead_target` 22、`take_changed_digests` 24、`snapshot_of` 12、`project_root_of` 22 ≈ 154）/ `evict.rs`（`evict_idle_runtimes` 144） |
| `transport` 1,588 | `mod.rs`（`serve` 973）/ `scoped.rs`（`serve_scoped_core` 161 + `run` 93 + `serve_stdio_with_relay` 89 + `serve_scoped` 51 + `serve_stdio` 39 + `parse_relay_url` 19 ≈ 452）/ `stdio.rs`（`read_lines` 42 + `pump_stdout` 45）/ `notifications.rs`（`pump_theme_notifications` 27 + `pump_git_diff_notifications` 49） |
| `turn_lifecycle` 1,497 | `mod.rs`（`start_thread_core` 298 / `prepare_turn_core` 221 / `start_turn_core` 88）/ `persist.rs`（`persist_and_finish_turn` 131 + `checkpoint` 17 + `build_partial` 25）/ `command.rs`（`apply_session_command` 299 + `apply_session` 23）/ `title.rs`（`generate_title_for_first_input` 64）/ `replay.rs`（`chunk_items_for_replay` 74 + 其余小函数） |

`serve()` 973 行仍在 `transport/mod.rs` 里超限，由 §2.3 解决。

### 2.2 目标结构

```
src/server.rs                     # 只留类型定义 + re-export + mod 声明，目标 ≤ 350 行
src/server/
├── transport/
│   ├── mod.rs                    # serve / serve_scoped / serve_scoped_core
│   ├── scoped.rs                 # run / serve_stdio* / parse_relay_url
│   ├── stdio.rs                  # read_lines / pump_stdout
│   └── notifications.rs          # pump_theme_notifications / pump_git_diff_notifications
├── turn/
│   ├── mod.rs                    # start_turn_core / start_thread_core / prepare_turn_core
│   ├── persist.rs                # persist_and_finish_turn / checkpoint / build_partial
│   ├── command.rs                # apply_session_command / apply_session
│   └── title.rs                  # generate_title_for_first_input
├── driver/
│   ├── mod.rs
│   ├── turn_driver.rs            # run_thread_driver
│   ├── tick.rs                   # driver_tick
│   └── lead.rs                   # lead_tick_once / resolve_lead_target / ...
├── watch/{mod.rs, observers.rs}
├── assembly/
│   ├── mod.rs                    # build_runtime_tooling / rebuild_thread_agent_with_theme / wrap_for_delegation
│   └── session_context.rs        # 4 组 with_session_* / current_session_*
├── plugin/{mod.rs, socket.rs}
├── task_bridge.rs                # cancel_thread_children / forward_to_daemon / ...
├── watchman.rs
├── rpc_io.rs
├── thread_index.rs
├── migration.rs
└── tests/                        # 已有 12 个文件 / 21,366 行，本波不动
```

注意 **没有 `transport/dispatch.rs`**：域分派块受 `app_state!` 宏与内联 `continue`
约束，**必须留在 `serve()` 体内**（见 §2.3）。它只能被局部宏收敛，无法成独立模块。

**`server.rs` 保留类型与 re-export 是刻意设计**：这样 C2 的改动**不外溢到任何其他文件**
（`rpc/*` 与 `server/tests/*` 的 `use crate::server::X` 全部继续解析）。

### 2.3 硬要求：`serve()` 的域分派块必须就地收敛（**不能抽成函数**）

先纠正一个直觉上很自然、但在 Rust 里不成立的方案：*把这 18 个臂抽成一个
`dispatch()` 函数*。**做不到**，原因有三条，都已实测：

1. **`app_state!()` 是定义在 `serve()` 体内的宏**（L3166–3215，51 行），
   按名捕获 `serve()` 的 41 个局部量（其中 8 个 `&mut`）。宏无法跨函数使用。
2. **15 个样板臂的臂体内联 `continue`**，`continue` 只能作用于所在循环。
3. 臂体是 `&mut` 借用同一批局部量，把它们打包传给函数需要重构整个借用结构
   ——那已是「拆状态」，不属本波（见 §7 第 1 条）。

#### 实测的块构成

| 段 | 行范围 | 行数 |
|---|---|---|
| `ADMIN_METHODS` + `client_scope` 门禁 | 3146–3164 | 19 |
| `app_state!` 宏定义 | 3166–3215 | **51** |
| `match domain_for(method)` 分派块 | 3217–3413 | **197**（15 样板臂 138 + `Thread` 14 + `Plugin` 43 + `Unknown` 1 + `match` 开闭 1） |
| 其后原始 dispatch（`initialize` 等） | 3415–3478 | 64 |

分派块这 197 行里，**15 个臂是 7–10 行的纯样板**（18 个域 − `Unknown` − `Plugin` − `Thread`）：

| 臂 | 行数 |
|---|---|
| `Ui` / `Model` / `Onboarding` / `Image` / `Workspace` / `Process` / `Pair` / `Turn` / `Agent` / `Skill` / `Mcp` | 各 **10** |
| `Resident` / `Flags` / `Remote` / `WebSearch` | 各 **7** |
| `Thread` | 14（多一层 `let mut state`） |
| `Plugin` | **43**（`tokio::spawn` 走 `PluginChannelState`，不 `continue`） |
| `Unknown` | 1 |

#### 可行方案：宏收敛样板，特例留原地

样板臂的差异**只有域模块名**（`ui::handle` / `model::handle` / …）。所以：

```rust
// 收在 serve() 体内，紧跟 app_state! 之后，仍是局部宏
macro_rules! dispatch_domain {
    ($handler:path) => {{
        let mut state = app_state!();
        match $handler(&mut state, &req, id.clone()).await? {
            crate::rpc::Flow::Continue => continue,
            crate::rpc::Flow::Unknown => {}      // 交回下面的原始 dispatch
        }
    }};
}
```

15 个样板臂各自塌成一行：

```rust
crate::rpc::Domain::Ui      => dispatch_domain!(crate::rpc::ui::handle),
crate::rpc::Domain::Model   => dispatch_domain!(crate::rpc::model::handle),
// ...共 15 个
crate::rpc::Domain::Plugin  => { /* 43 行原样保留 */ }
crate::rpc::Domain::Unknown => {}
```

**收益与代价要说清楚**：这 197 行会降到约 **86 行**（1 + 15×1 + 宏 12 + `Thread` 14
+ `Plugin` 43 + `Unknown` 1），但 `serve()` 的总行数只从 973 降到 **862**——因为 `serve()` 的大头是 transport 编排与 4 个 tick 源，
不是这 197 行。**所以 §2.5 里「`serve()` ≤ 450」这个判据是错的，已改为
「≤ 900 + 分派块 ≤ 90 行」。** 这是本 spec 相对初稿最重要的一处修正。

**这个宏方案已用最小工程实测通过**（2026-11-03，**rustc 1.98.1**，macOS）：
定义在 `async fn` 内部的 `macro_rules!` 捕获外层 `let mut` 局部量、宏体内
`continue` 正确跳转到外层 `for` 循环、`$handler:path` 传 `async fn` 并 `.await`
——三条全部成立。**不需要 `#[macro_export]`，也不需要 nightly。**

可复现的最小工程（实测输出 `a=4 b=1 => 401`）：

```rust
#[derive(Default)] struct State { n: u32 }
macro_rules! app_state { () => { State::default() }; }
#[derive(Debug, PartialEq)] enum Flow { Continue, Unknown }
async fn ui_handle(_s: &mut State) -> Flow { Flow::Continue }
async fn model_handle(_s: &mut State) -> Flow { Flow::Unknown }

async fn serve(reqs: Vec<&str>) -> (u32, u32) {
    let mut a = State::default();
    let mut b = State::default();
    macro_rules! dispatch_domain {
        ($h:path) => {{
            let mut state = app_state!();
            match $h(&mut state).await {
                Flow::Continue => { a.n += 1; continue; }   // 跳到外层 for
                Flow::Unknown => {}
            }
        }};
    }
    for r in reqs {
        match r {
            "ui" => dispatch_domain!(ui_handle),
            "model" => dispatch_domain!(model_handle),
            _ => {}
        }
        b.n += 1;
    }
    (a.n, b.n)
}
// 4 次 "ui" 命中 → a=4；"model" 走 Unknown → b 累加 1 次 → 401
```

`a=4, b=1` 的含义：4 次 `ui` 命中全部被 `continue` 跳过（所以 `b` 没被累加），
只有 `model` 那次落到 `Flow::Unknown`、继续往下执行到 `b.n += 1`。
**这正是 `serve()` 里要的语义。**

`serve()` 真正的大头在别处，本波不碰：

- L3415–3478 的原始 dispatch（64 行，`initialize` 里内联了几十个局部量的借用）
- `turn_rx` / `fatal_rx` / driver / lead / power 5 个 tick 分支（约 300 行）
- 约 30 个局部量的初始化（约 180 行）

**把 `serve()` 压到 450 需要把它拆成「主循环 + 若干 stage 函数」，那就是拆状态，
属第三波。** 本波只做宏收敛，不在 `serve()` 里冒险。

实施要求：

1. **`Plugin` 臂一行都不改。** 它要 `tokio::spawn` 一个 owned 的
   `PluginChannelState`、并在 `Flow::Unknown` 时回 `method_not_found`、
   `Err` 时发 `fatal_tx`。这套语义与 15 个样板臂完全不同，硬套宏会改变行为。
2. **一次收敛 4 个样板臂，跑一次测试。** 不要 15 个一起改。
3. 收敛后**逐臂比对 `git show`**，确认只删了样板、没动 `Flow::Unknown` 的落点。

### 2.4 步骤与验收

```
C2a  runtime_binding + watchers + task_bridge   （小组合并先做，验证搬移流程）
C2b  thread_index                                 （25 个函数，纯 helper，最安全）
C2c  agent_assembly + plugin + watchman + rpc_io + migration
C2d  turn/ 四片（含二次分片）
C2e  driver/ 三片（含二次分片）
C2f  transport/ 分片 + serve() 抽分派臂            （唯一结构变化，最后做）
```

每步验收：

```bash
cargo test -p yi-agent-app-server
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings

# 无可见性泄露：统计新增的 pub/pub(crate)/pub(super) 函数行数
# 注意 `grep '^+pub fn'` 会漏检 pub(crate) —— 实测 3 条样本只抓到 1 条
git diff -U0 -- 'crates/yi-agent-app-server/src/server*' \
  | grep -cE '^\+\s*pub(\(crate\)|\(super\))? (async )?fn'   # 期望 0

# 协议未动
cargo test -p yi-agent-app-server export_bindings
git diff --exit-code -- ../../desktop/src/lib/generated

# 行数单调下降
wc -l src/server.rs
```

### 2.5 C2 完成判据（已验算）

| 指标 | 拆分前 | 目标 | 验算依据 |
|---|---|---|---|
| `server.rs` | 7,590 | **≤ 350** | 必留类型 145（17 个有外部引用）+ impl 21 + `use` 20 + re-export 40 + `mod` 15 + 文档 25 + 空行 40 ≈ 306（理论下限）；**≤ 250 不可达** |
| 最大子模块 | — | **≤ 1,000** | `driver/` 三分后最大 817（`tick.rs`）；`transport/mod.rs` 含 `serve` 约 862，达标 |
| `serve()` | 973 | **≤ 900** | 分派块 197 → 86（§2.3），`973-197+86=862`；其余 776 行不在本波范围 |
| `serve()` 内域分派块 | 197 | **≤ 90** | 15 个样板臂各 1 行（15）+ 宏定义 12 + `Plugin` 43 + `Thread` 14 + `Unknown` 1 + 开闭 1 = 86 |
| `server.rs` 顶层函数 | 125 | **0** | 全部搬进子模块 |
| 测试数 | **1,166** | **1,166** | `cargo test -p yi-agent-app-server -- --list \| grep -c ': test$'`；只搬不改，`server/tests/` 不动 |
| 公共 API | — | **零变化** | `lib.rs` 不动；`server::X` re-export 保持 |

> **初始草案曾写「`serve()` ≤ 450」，那是错的**，已在 §2.3 说明原因：
> `serve()` 的 973 行里只有 197 行是域分派，其余 776 行是 transport 编排、
> 5 个 tick 分支与 30 个局部量初始化。压到 450 必须拆状态，属第三波。

### 2.6 已知风险

1. **`BuildAgent<'a>` 等带生命周期的类型**跨模块搬移会触发新的约束报错。
   缓解：按规则 4，**类型定义一律留在 `server.rs`**，只搬 `fn`。
2. **`production_factory` 属于 `assembly/`，但 C4 会改它。** 本块只搬位置、不动签名；
   C4 的 spec 必须注明「它现在在 `server/assembly/`」。
3. **`driver_tick` 内含 4 处借用规避**（先快照 `idle`、先算全机运行数再构造 launcher）。
   搬进 `tick.rs` 时这些顺序必须原样保留——它是借用检查器逼出来的，不是风格问题。
   改动顺序会直接编译失败，或在更糟的情况下改变并发语义。
4. **`app_state!` 宏与臂内 `continue` 使域分派块无法成模块**（§2.3）。
   这是本块最容易被想当然跳过的一条：若实施者试图抽 `dispatch.rs`，
   会在第一行就撞上「宏不可跨函数」。**把这一条当作硬约束，不是建议。**
5. **`serve()` 只降到 ~862 行，`transport/mod.rs` 会偏大。** 这是刻意接受的：
   `serve()` 的其余部分含 30 个局部量的初始化，搬它的任何一段都需要一并搬借用关系，
   等于提前做第三波。**若评审认为 `transport/mod.rs` 862 行不可接受，
   本块的正确做法是缩小范围（只做其他 10 组），而不是在 `serve()` 里冒险。**

---

## 3. C3 块：`rpc/thread.rs` 的 `handle()` 拆分

### 3.1 这一块和前 18 个域不一样

`rpc/mod.rs` 的模块注释记录了已完成的一次重构：把 `serve()` 里 62 个域方法的
内联臂按域搬进 18 个子模块。**`thread` 域是那次拆分没走完的残留**——
它按「域」拆了，但域内 26 个 method（23 个臂）仍在一个 2,347 行的 `match` 里。

所以 C3 的性质是「同一层级再拆一层」，方法论与 C2 一致（纯搬移），
落点是 `rpc/thread/` 目录。

### 3.2 目标分组（4 组，按调用者语义而非行数均分）

下表「实测行数」是**臂体本身**（不含 helper）；拆分成文件时还要加上归属该组的
helper，见 §3.5 的 helper 归属表。

| 组 | method | 实测行数（臂体） | 理由 |
|---|---|---|---|
| `lifecycle` | `start` / `resume` / `fork` / `delete` / `clear` | **930** | 都要建/拆 agent 与 runtime，共享 `start_thread_core` / `attach_delegation` |
| `settings` | `rename` / `setModel` / `setReasoningEffort` / `setPreserveThinking` / `setPermissionMode` / `setPinned` / `setTag` / `markRead` / `reorderPinned` / `compact` | **866** | 全是对 meta 的读-改-写，共享 `ThreadStore` |
| `content` | `list` / `listAll` / `diff/read` / `view/read` / `subscribe` / `readItems` | **452** | 都是只读投影，共享 `state.workspaces` / `state.threads` |
| `lead_and_groups` | `lead` + `group/{create,update,setMembers,delete}` | **85** | 组长与分组管理 |

合计 **2,333** 行。`handle()` 体 2,347 行 = 头 6（L70–75）+ 臂体 2,333（L76–2408）
+ 尾 8（L2409–2416）。

**8 个 helper（403 行）不留在 `mod.rs`，随调用方走**——归属见 §3.5 的表。
若全留在 `mod.rs`，它至少 540 行（外壳 40 + helper 403 + `use`/`mod` 45 + 文档空行 50），
`≤ 200` 的判据不可达。

⚠️ **三处必须二次分片**（都在 §3.5 有详细拆法）：`lifecycle` 953 行 → 三片、
`settings` 866 行 → 四片、`content` 613 行 → 三片。初稿只提到了 `lifecycle`，
漏了后两处，且把 `lifecycle` 写成「1,030 行」。

目标结构：

```
src/rpc/thread/
├── mod.rs                # handle() 外壳 + re-export + #[cfg(test)] helper，≤ 200 行
├── lifecycle/
│   ├── mod.rs            # start + missing_model_error（183）
│   ├── resume.rs         # resume（386）
│   └── delete.rs         # fork / delete / clear（384）
├── settings/
│   ├── mod.rs            # rename / setPinned / setTag / markRead / reorderPinned / compact（379）
│   ├── model.rs          # setModel / setPermissionMode（209）
│   └── effort.rs         # setReasoningEffort / setPreserveThinking（298）
├── content/
│   ├── mod.rs            # list / subscribe / view/read（242）
│   ├── list_all.rs       # listAll + scan_workspace_metas + build_list_all_response + groups_json（212）
│   └── diff_read.rs      # diff/read + diff_read_fallback（134）
└── groups/
    └── mod.rs            # lead + group/* + apply_group_mutation + require_group_id（265）
```

`handle()` 外壳保留在 `mod.rs`：

```rust
pub(crate) async fn handle(state: &mut AppState<'_>, req: &RequestEnvelope, id: RequestId)
    -> anyhow::Result<Flow> {
    let method = req.method.as_str();
    if lifecycle::owns(method)  { return lifecycle::handle(state, req, id).await; }
    if settings::owns(method)   { return settings::handle(state, req, id).await; }
    if content::owns(method)    { return content::handle(state, req, id).await; }
    if groups::owns(method)     { return groups::handle(state, req, id).await; }
    Ok(Flow::Unknown)
}
```

⚠️ **`Flow::Unknown` 的语义必须保住。** 现在兜底是 `_ => Ok(Flow::Unknown)`，
`serve()` 靠它把不认识的 method 交回原始 dispatch。四个 `owns()` 必须
**穷尽且互斥**，漏一个 method 会从「正常处理」变成「静默返回 Unknown」，
这是**行为变化**，而且测试未必抓得到（取决于该 method 是否被覆盖）。

### 3.3 实施前置：先确认没有遗漏的 method

不需要判断死代码（§0.5 已确认 23 个臂、无重复），但需要**录一份 method 基线**：

```bash
# 拆前（在 yi-agent-rs/ 下执行；`HEAD:` 后必须用 ./ 相对路径，
# 否则 git 会按仓库根解析而报 "path does not exist"）
git show HEAD:./crates/yi-agent-app-server/src/rpc/thread.rs \
  | sed -n '70,2416p' \
  | grep -oE '"thread/[a-zA-Z/]+"' | tr -d '"' | sort -u > /tmp/thread_methods_before.txt
wc -l < /tmp/thread_methods_before.txt    # 期望 26
```

拆后：

```bash
grep -rhoE '"thread/[a-zA-Z/]+"' crates/yi-agent-app-server/src/rpc/thread/ \
  | tr -d '"' | sort -u > /tmp/after.txt
diff /tmp/thread_methods_before.txt /tmp/after.txt && echo "OK: 26 个 method 一个不差"
```

另外**必须断言 `owns()` 的并集恰好等于这 26 个**。这里有个陷阱，初稿踩过：

⚠️ **不要把 26 个 method 名手写进测试数组。** 手写清单只能验证「我列出的这些
各归一组」，**抓不到「原来的某个 method 被我漏掉了」**——因为它已经不在清单里。
实测验证过：把 `thread/delete` 从 4 个组的 `owns()` 里删掉、测试仍全绿。

正确做法是**清单从源码枚举**，测试只负责比对：

```rust
#[test]
fn every_thread_method_is_owned_by_exactly_one_group() {
    // 基线文件由 §3.3 的 grep 命令从拆分前的 thread.rs 生成并提交入库
    let all: Vec<&str> = include_str!("../thread_methods_baseline.txt")
        .lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(all.len(), 26, "基线文件必须恰好 26 行");
    let mut unowned = vec![];
    for m in &all {
        let n = [lifecycle::owns(m), settings::owns(m), content::owns(m), groups::owns(m)]
            .iter().filter(|x| **x).count();
        if n != 1 { unowned.push(format!("{m} 被 {n} 组认领")); }
    }
    assert!(unowned.is_empty(), "未归属或重复归属: {unowned:#?}");
}
```

**这个测试是本块的核心验收**，比「编译通过」强得多。已用最小工程实测：
把 `owns()` 的某个 method 摘掉，测试报 `thread/xxx -> 被 0 组认领`；
构造两个组同时认领，报 `被 2 组认领`。两种错误都能抓到。

### 3.4 步骤与验收

```
C3a  录 method 基线 + 写「每个 method 恰好归一组」的测试（先失败）
C3b  content/ 三片（只读投影，最简单，验证搬移机制）
C3c  settings/ 三片
C3d  lifecycle/ 三片
C3e  groups/ + handle() 外壳改成分组分派 + 测试转绿
```

### 3.5 C3 完成判据

| 指标 | 拆分前 | 目标 | 依据 |
|---|---|---|---|
| `rpc/thread/mod.rs` | 2,809 | **≤ 200** | 外壳 ≈40 + `use`/`mod` ≈45 + 文档 30 + 空行 20 ≈ 135 |
| 最大子模块 | — | **≤ 450** | 见下方 helper 归属与二次分片 |
| `handle()` | 2,347 | **≤ 60** | 4 组 × ~6 行 + `match` 开闭 |
| method 数 | 26 | **26，且每组恰好认领自己的** | §3.3 的基线测试 |

**8 个 helper 的归属（必须随调用方走，否则 `mod.rs` 下不来）：**

| helper | 行数 | 调用方 | 归属 |
|---|---|---|---|
| `missing_model_error` | 23 | `start`(L200) / `resume`(L474) | `lifecycle/mod.rs` |
| `scan_workspace_metas` | 41 | `listAll`(L2532) | `content/list_all.rs` |
| `scan_workspace_metas_for_test` | 18 | 仅测试 | 留在 `mod.rs` 的 `#[cfg(test)]` |
| `diff_read_fallback` | 21 | `diff/read`(L2143) | `content/diff_read.rs` |
| `build_list_all_response` | 96 | `listAll`(L142) | `content/list_all.rs` |
| `groups_json` | 24 | `listAll`(L2591) | `content/list_all.rs` |
| `apply_group_mutation` | 173 | `group/*`(L1609) | `groups/mod.rs` |
| `require_group_id` | 7 | `group/*`(L2725/2759/2781) | `groups/mod.rs` |

helper 合计 **403 行**。若全留在 `mod.rs`，`mod.rs` 至少 540 行，`≤ 200` **不可达**——
这是初稿的又一个错误（它写「外壳 + 6 个 helper」，实际是 8 个）。

⚠️ **content 组会超 450，必须二次分片**：`content` 的 method 臂 452 行 +
`scan_workspace_metas` 41 + `build_list_all_response` 96 + `groups_json` 24 = **613 行**。
拆法：

| 片 | 内容 | 约计 |
|---|---|---|
| `content/mod.rs` | 4 个 `owns()` + list / subscribe / view_read | 29+45+148+~20 ≈ 242 |
| `content/list_all.rs` | `listAll` 臂 + `scan_workspace_metas` + `build_list_all_response` + `groups_json` | 51+41+96+24 ≈ 212 |
| `content/diff_read.rs` | `diff/read` 臂 + `diff_read_fallback` | 113+21 ≈ 134 |

⚠️ **`settings` 组 866 行也超 450**（初稿只说 lifecycle 要拆，漏了它）。拆法：

| 片 | method | 约计 |
|---|---|---|
| `settings/mod.rs` | `rename` / `setPinned` / `setTag` / `markRead` / `reorderPinned` / `compact` + `owns()` | 46+46+69+61+86+51+~20 ≈ 379 |
| `settings/model.rs` | `setModel` / `setPermissionMode` | 134+75 ≈ 209 |
| `settings/effort.rs` | `setReasoningEffort` / `setPreserveThinking` | 182+116 ≈ 298 |

⚠️ **`lifecycle` 组 953 行也超 450**（含 `missing_model_error` 23）。按 §3.4 的
「三片」拆后分别 183 / 386 / 384，全部达标。

**行数守恒校验**：`handle` 体 2,347 + 8 个 helper 403 = **2,750**。
拆后 10 个片合计：

| 片 | 行数 |
|---|---|
| `lifecycle/mod.rs` | 183 |
| `lifecycle/resume.rs` | 386 |
| `lifecycle/delete.rs` | 384 |
| `settings/mod.rs` | 379 |
| `settings/model.rs` | 209 |
| `settings/effort.rs` | 298 |
| `content/mod.rs` | 242 |
| `content/list_all.rs` | 212 |
| `content/diff_read.rs` | 134 |
| `groups/mod.rs` | 265 |
| **合计** | **2,692** |

余下 **58 行** = 各片自己的 `use`/`//!`/函数间空行（10 个文件），
另 `rpc/thread/mod.rs` 外壳约 135 行。与 `thread.rs` 的 2,809 总行对得上。

---

## 4. C2 与 C3 可以并行（更正上一轮的判断）

上一轮我说「C2 必须在 C3 之前」，理由是「C3 的臂体大量用 `state.`，跟着 `AppState`
的搬移走最省事」。**实测后这条不成立**，原因：

- C3 的臂体用的是 `state: &mut AppState<'_>` 的**字段访问**，而 `AppState` 定义在
  `rpc/mod.rs`，两个块都不改它。
- C3 对 `crate::server::X` 的引用靠 C2 的 re-export 继续可达（规则 2/4）。

  **实测**：`rpc/thread.rs:27-35` 有一整块
  `use crate::server::{...}`，共 **31 个符号**——3 个类型（`BuiltAgent` /
  `ProcessWatch` / `ThreadCancelOutcome`）+ **28 个函数**。这 28 个函数分散在
  C2 的 **8 个组**里：

  | 组 | C3 需要的函数 |
  |---|---|
  | `thread_index` | `attach_delegation` `collect_pinned` `item_id` `require_known_thread` `require_thread_id` `resolve_thread_cwd` `store_lookup` `thread_cwd` `thread_summary_json` `thread_summary_json_with_status` `worktree_json` |
  | `rpc_io` | `err_response` `ok_response` `write_notification` `write_notification_await` `write_response` |
  | `agent_assembly` | `detach_unused_runtimes` `with_session_extra_roots` `with_session_model_ref` `with_session_reasoning_effort` |
  | `turn_lifecycle` | `chunk_items_for_replay` `interrupt_and_wait_for_persist` `linked_extra_roots` `start_thread_core` |
  | `driver` | `run_thread_driver` |
  | `runtime_binding` | `with_session_preserve_thinking` |
  | `task_bridge` | `cancel_thread_children` |
  | `watchers` | `watch_processes` |

  这三个事实合起来说明**并行是安全的**：

  1. C3 不新增对 `crate::server::` 的引用（它只是把 `handle()` 的臂搬进子模块）。
     所以合并前后，`thread.rs` 需要的符号集始终是这 31 个。
  2. C2 的每个中间提交都被「`cargo test -p yi-agent-app-server` 必须绿」卡着。
     只要某个提交把 `store_lookup` 之类搬进子模块，它就**必须**同时补上
     re-export，否则 `thread.rs` 立刻编译失败、该提交过不去。
  3. 两块改的文件集不重叠（C2：`server.rs` + `server/**`；C3：`rpc/thread.rs`
     + `rpc/thread/**`），合并时无文本冲突。

  唯一需要人工确认的事：**C3 合并回来后重跑一次 `cargo test -p yi-agent-app-server`**，
  因为「C2 的 re-export 表」和「C3 需要的符号集」从未在同一次编译里同时存在过。

| | 改动的文件集 |
|---|---|
| C2 | `server.rs` → `server/**` |
| C3 | `rpc/thread.rs` → `rpc/thread/**` |

**交集为空，可以并行。**

唯一建议的排序理由**不是依赖，而是风险隔离**：§2.3（`serve()` 抽分派臂）是两块里
唯一有结构变化、可能改变行为的地方。先做它、让问题早暴露，C3 的纯搬移就更安全。

---

## 5. 完成定义

```bash
# 逐块
cargo test -p yi-agent-app-server
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings
cargo fmt --all -- --check

# 整波收尾（在 yi-agent-rs/ 下）
just ci      # 15 道门禁全绿

# 结构指标
wc -l src/server.rs src/rpc/thread/mod.rs                    # ≤ 350 / ≤ 200
find src/server src/rpc/thread -name '*.rs' | xargs wc -l | sort -rn | head -3   # 最大 ≤ 1000
```

人工核对三项：

1. `git diff --exit-code -- ../../desktop/src/lib/generated`（协议未动）
2. `git diff -- lib.rs`（公共导出面未动）
3. 逐组 `git show --stat` 确认每个提交只碰一个模块

---

## 6. 测量方法（可复现）

本文数字用以下脚本得到，避免第一波 spec 里出现过的「测试模块不在文件末尾
导致估算错误」：

```bash
python3 - <<'PY'
import re
def test_lines(L):
    tot=0;i=0;n=len(L)
    while i<n:
        if L[i].strip()=='#[cfg(test)]':
            j=i+1
            while j<n and (L[j].strip().startswith('#[') or L[j].strip()==''): j+=1
            if j>=n: break
            if L[j].rstrip().endswith(';'): tot+=j-i+1;i=j+1;continue
            k=j
            while k<n and L[k]!='}': k+=1
            tot+=k-i+1;i=k+1
        else:i+=1
    return tot

# 必须用 splitlines()：split('\n') 在文件末尾有换行时会多算 1 行空串，
# 使 total 变成 7591（真值 7590）。prod 不受影响（7557），但 total 会误导。
L=open('src/server.rs').read().splitlines()
t=test_lines(L)
print(f"total={len(L)} inline_test={t} prod={len(L)-t}")

fn_re=re.compile(r'^(pub(\(crate\))?\s+)?(async\s+)?fn\s+(\w+)')
idx=[(i,m.group(4)) for i,l in enumerate(L) if (m:=fn_re.match(l))]
print(f"顶层函数={len(idx)}")
rows=[]
for k,(i,name) in enumerate(idx):
    end=idx[k+1][0] if k+1<len(idx) else len(L)
    rows.append((end-i,name))
rows.sort(reverse=True)
for s,n in rows[:8]: print(f"{s:5d} {n}")
PY
```

`AppState` 字段数用：

```bash
python3 - <<'PY'
import re
L=open('src/rpc/mod.rs').read().split('\n')
s=next(i for i,l in enumerate(L) if 'pub(crate) struct AppState' in l)
e=next(i for i in range(s,len(L)) if L[i]=='}')
f=[l for l in L[s:e] if re.match(r'^\s*pub\(crate\) [a-z_]+:',l)]
mut=[x for x in f if "&'a mut" in x]      # 不能写进 f-string：表达式里不能有反斜杠
print(f"字段={len(f)} mut={len(mut)}")
PY
```

`rpc/thread.rs` 的 `handle()` 臂数与各组行数用：

```bash
python3 - <<'PY'
import re
L=open('src/rpc/thread.rs').read().splitlines()
h=next(i for i,l in enumerate(L) if re.match(r'^pub\(crate\) async fn handle',l))
e=h
while L[e] != '}': e += 1
starts=[i for i in range(h,e) if re.match(r'^        "thread/', L[i])]
starts.append(e)
print(f"handle() = {e-h+1} 行, 臂 = {len(starts)-1} 个")
for k in range(len(starts)-1):
    a,b = starts[k], starts[k+1]
    name=re.match(r'^        "(thread/[^"]+)"', L[a]).group(1)
    print(f"L{a+1:5d} {b-a:4d} {name}")
PY
```

> 注意：或模式臂（`thread/group/*` 的 4 个 method 共用一条臂）只统计臂首行，
> 所以**臂数 23 不等于 method 数 26**。`starts` 不能把 `| "thread/..."` 续行
> 也计入，否则 `markRead` 会被算成 89 行（真值 61）。

`server.rs` 的 125 个函数按 §2.1 的 12 组归类，可用下列脚本复算
（组名与函数名的映射需手工维护，脚本只负责求和与查漏）：

```bash
python3 - <<'PY'
import re
L=open('src/server.rs').read().splitlines()
fns=[]
for i,l in enumerate(L):
    m=re.match(r'^(pub(?:\(crate\))?\s+)?(?:async\s+)?fn\s+(\w+)', l)
    if m: fns.append((i+1, m.group(2)))
b=[f[0] for f in fns]+[len(L)+1]
d={n: b[k+1]-ln for k,(ln,n) in enumerate(fns)}
print(f"顶层函数 {len(d)} 个, 跨度合计 {sum(d.values())}")
# 把 §2.1 的 12 组函数名填入后，即可用 d[name] 求和并校验互斥/无遗漏
PY
```

---

## 7. 明确不做的事

1. **不拆 `AppState` 的 41 个字段（含 8 个 `&mut`）。** 那是第三波的事，需要单独 spec——
   它要把「主循环局部量」变成真正的状态机，行为风险远高于搬移。本波只是让
   `AppState` 出现在更多文件里，字段与签名一律不变。
2. **不动 `server/tests/` 的 12 个文件（21,366 行）。** 那是 C1 的产物。规则 4
   （类型留在 `server.rs`）正是为了不动它们。
3. **不动 `protocol.rs` 与 `translate.rs`。** 前者影响 TS 绑定；后者 651 行生产代码、
   `impl Translator` 476 行，未超阈值。
4. **不动 `thread_store.rs`（1,285 行生产 / 2,975 行测试，总 4,260 行）。** 它在 C1 后已接近合理，
   且被 `rpc/thread.rs` 与 `server.rs` 同时依赖，动它会把两块的重叠面引进来。
5. **不做 C4（装配统一）与 D3（`main.rs`）。** 见文首说明。
6. **不为拆而拆。** 某组不足 150 行且只有 2 个函数时，并入同层最大模块，不单独成文件
   （`migration.rs` 82 行 3 个函数属临界，可并入 `rpc_io.rs`）。

---

## 8. 与第一波 / 第三波的边界

本波只做「搬文件 + 收敛 `serve()` 分派 + 拆 `handle()`」三类机械重排，**不改任何运行时行为**。与之相邻但明确排除的两块：

| 相邻工作 | 为何不在本波 | 何时做 |
|---|---|---|
| 拆 `AppState`（把 8 个 `&mut` 收进真状态机） | 行为风险高，需要单独 spec | 第三波 |
| C4 装配统一 / D3 `main.rs` | 与文件搬移正交，混在一起无法定位回归 | 后续波次 |

