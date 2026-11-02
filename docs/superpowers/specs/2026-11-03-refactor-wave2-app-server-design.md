# 重构第二波：app-server 分层（`server.rs` 拆除与 RPC 域收敛）

**状态：** 设计（待评审）
**日期：** 2026-11-03
**范围：** C2 / C3 两块，纯重构，**不改任何对外行为、不改线上协议、不改公共 API**

> 本文承接第一波设计（`2026-11-02-refactor-wave1-design.md`）。第一波 §8 明确把
> `server.rs`（7,559 行）、`rpc/thread.rs` 的 `handle()`、C4 装配统一、
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

第一波的 **C1（`server.rs` 测试外移）也已独立落地**：`server.rs` 从 10,973 行降到
**7,590 行**，内联测试从 3,414 行降到 **34 行**，测试搬进 `server/tests/` 8 个文件
（合计约 1.4 万行）。

**因此本文不含任何测试搬迁工作。** C2/C3 是纯粹的**生产代码**重组。

### 0.2 两个目标文件的构成（实测）

| 文件 | 总行 | 内联测试 | 生产行 | 顶层函数 | 最大单元 |
|---|---|---|---|---|---|
| `server.rs` | 7,591 | 34 | **7,557** | **125 个**（3 `pub` + 71 `pub(crate)` + 51 私有） | `serve()` **973 行**（L2802–3775） |
| `rpc/thread.rs` | 2,810 | 7 | **2,803** | `handle()` + 6 个 helper | `handle()` **2,347 行**（L70–2416） |

`server.rs` 的前三大函数就占 2,507 行：

| 函数 | 行数 |
|---|---|
| `serve` | **973** |
| `driver_tick` | **817** |
| `run_thread_driver` | **717** |
| `apply_session_command` | 299 |
| `start_thread_core` | 298 |
| `wait_for_plugin_query_socket_within` | 229 |
| `prepare_turn_core` | 221 |
| `build_runtime_tooling` | 190 |

### 0.3 真正的根因：`AppState` 是「41 个借用的打包」

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
    // 其余 33 个是 &'a 只读借用或按值拷贝
}
```

18 个域 handler 的签名统一是 `async fn handle(state: &mut AppState<'_>, ...)`。

**结论：拆文件是廉价的（搬 fn + 调可见性），拆状态是昂贵的（改 8 个 `&mut` 的归属）。
本文只做前者。** 这也解释了为什么 C1 能在一个提交里完成、而 C2/C3 需要分步——
测试搬走不涉及借用关系，生产代码重排涉及。

### 0.4 `serve()` 的 973 行在做什么

`serve()` 内约 30 个局部量 + 一个 `tokio::select!`。其中 **18 个 `Domain::*` 分派臂
约占 400 行**（每臂 15–30 行，形如「调域 handler，`Flow::Continue` 就 `continue`」），
其余是 4 个 tick 源与 transport 编排。

关键：这些臂**已经是第二段式分派**（`domain_for(method)` → `rpc::<domain>::handle`），
所以抽走它们不会改变分派语义，只是把 18 段样板收进一个函数。

### 0.5 `rpc/thread.rs` 的 `handle()` 实测：25 个 method / 23 个臂

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
| 2343 | 74 | `thread/readItems` |

**合计 23 个臂 / 25 个 method，无重复臂。**

> ⚠️ 一处容易误判的地方，实施时不要重犯：`thread/group/create` 在文件中出现两次
> （L1595 与 L2686）。L2686 那处**不是** `handle()` 的臂，而是
> `apply_group_mutation()` 内部的嵌套 `match method`。用「grep 8 空格缩进的引号模式」
> 会把它误当成交互重复，进而得出「34 个臂、有重复」的错误结论。

---

## 1. 全块共用规则

与第一波 §1 一致，补充两条针对 app-server 的：

1. **只搬不改语义。** 每个提交都是「同样的代码换个位置」。允许的改动仅限：
   可见性（`fn` → `pub(crate) fn` / `pub(super) fn`）、`use` 语句、路径前缀。
2. **公共 API 零变化。** `lib.rs` 导出面不变。特别是 `ok_response` / `err_response` /
   `write_response` / `write_notification` / `store_lookup` / `thread_cwd` / `item_id`
   等被 `rpc/*`（17 处）与 `server/tests/*`（6 处）引用的项，**必须从 `server.rs` 继续可达**。
3. **线上协议零变化。** `protocol.rs` 不动；`bindings-check` 必须无 diff。
4. **类型留在 `server.rs`，只搬函数。** 实测被外部引用的类型有
   `TurnEvent`、`ProjectRuntimes`、`ViewFrames`、`WatchmanInstall`、`WatchmanUninstall`、
   `ResolvePlugin`、`EnsureDaemon`、`ProcessWatch`、`TraceWatch`、`ChildrenWatch`、
   `BuildAgent<'a>`。这些一旦移走，`server/tests/*` 与 `rpc/*` 的 import 全部失效。
5. **单提交单模块。** 一个提交只搬一组函数，便于 `git show` 逐条核对。
6. **每提交跑**：`cargo test -p yi-agent-app-server`、
   `cargo clippy -p yi-agent-app-server --all-targets -- -D warnings`、
   `cargo fmt --all -- --check`。整块收尾跑 `just ci`。

---

## 2. C2 块：`server.rs` 按职责切模块

### 2.1 现状归类（125 个函数，逐名归组，实测行数为函数体跨度累加）

| 组 | 函数数 | 实测行数 | 是否超 1,000 |
|---|---|---|---|
| `driver` | 8 | **1,813** | ⚠️ 是 |
| `transport` | 11 | **1,588** | ⚠️ 是 |
| `turn_lifecycle` | 20 | **1,444** | ⚠️ 是 |
| `thread_index` | 25 | 575 | 否 |
| `agent_assembly` | 14 | 541 | 否 |
| `plugin_bridge` | 8 | 497 | 否 |
| `runtime_binding` | 9 | 314 | 否 |
| `task_bridge` | 4 | **99** | 否 |
| `watchers` | 6 | 244 | 否 |
| `rpc_io` | 9 | 216 | 否 |
| `watchman` | 9 | 119 | 否 |
| `migration` | 3 | 82 | 否 |

**归组完整性已实测**：121/125 归入上表；4 个未归的是
`cancel_thread_children` / `forward_to_daemon` / `list_task_summaries` / `read_task_trace`，
归入新增的 `task_bridge` 组（daemon IPC 转发）。

⚠️ **三个组超 1,000 行，必须二次分片。** 这是本块第一组硬要求：

| 组 | 分片方案 |
|---|---|
| `driver` 1,813 | `turn_driver.rs`（`run_thread_driver` 717）/ `tick.rs`（`driver_tick` 817）/ `lead.rs`（`lead_tick_once`、`resolve_lead_target`、`take_changed_digests`、`snapshot_of`、`project_root_of` ≈ 280） |
| `transport` 1,588 | `mod.rs`（`serve` 973）/ `scoped.rs`（`serve_scoped*` 161 + `run` 93 + `serve_stdio*` 89 + `parse_relay_url`）/ `stdio.rs`（`read_lines` 59 + `pump_stdout`）/ `notifications.rs`（两个 `pump_*`） |
| `turn_lifecycle` 1,444 | `mod.rs`（`start_turn_core` / `start_thread_core` 298 / `prepare_turn_core` 221）/ `persist.rs`（`persist_and_finish_turn` 131 + `checkpoint` + `build_partial`）/ `command.rs`（`apply_session_command` 299 + `apply_session`）/ `title.rs` |

`serve()` 973 行仍在 `transport/mod.rs` 里超限，由 §2.3 解决。

### 2.2 目标结构

```
src/server.rs                     # 只留类型定义 + re-export + mod 声明，目标 ≤ 250 行
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
└── tests/                        # 已有 8 个文件，本波不动
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
   ——那已是「拆状态」，不属本波（见 §7.1）。

#### 实测的块构成

| 段 | 行范围 | 行数 |
|---|---|---|
| `ADMIN_METHODS` + `client_scope` 门禁 | 3146–3164 | 19 |
| `app_state!` 宏定义 | 3166–3215 | **51** |
| `match domain_for(method)` 分派块 | 3217–3413 | **197** |
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

**收益与代价要说清楚**：这 197 行会降到约 **75 行**，但 `serve()` 的总行数只从
973 降到约 **856**——因为 `serve()` 的大头是 transport 编排与 4 个 tick 源，
不是这 197 行。**所以 §2.5 里「`serve()` ≤ 450」这个判据是错的，已改为
「≤ 900 + 分派块 ≤ 75 行」。** 这是本 spec 相对初稿最重要的一处修正。

**这个宏方案已用最小工程实测通过**（2026-11-03，rustc 1.85 / edition 2024）：
定义在函数体内部的 `macro_rules!` 捕获外层局部量、宏体内 `continue` 正确跳转
到外层 `loop`、`$handler:path` 传 `async fn` 全部成立。实测程序输出 `401`，
与「ui 臂 2 次命中 → `a.n=2`，`b` 只累加 1 次（另外 2 次被 `continue` 跳过）」
的预期一致。**不需要 `#[macro_export]`，也不需要 nightly。**

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

# 无 pub 泄露：原本 pub(crate) 的不得变成 pub
git diff -- 'src/server*' | grep '^+pub fn' && echo "FAIL: 新增 pub" || echo OK

# 协议未动
cargo test -p yi-agent-app-server export_bindings
git diff --exit-code -- ../../desktop/src/lib/generated

# 行数单调下降
wc -l src/server.rs
```

### 2.5 C2 完成判据（已验算）

| 指标 | 拆分前 | 目标 | 验算依据 |
|---|---|---|---|
| `server.rs` | 7,591 | **≤ 250** | 11 个类型/结构体定义 + re-export + 13 个 `mod` 声明 ≈ 180 行，留余量 |
| 最大子模块 | — | **≤ 1,000** | `driver/` 三分后最大 817（`tick.rs`）；`transport/mod.rs` 含 `serve` 约 856，达标 |
| `serve()` | 973 | **≤ 900** | 分派块 197 → 75（§2.3）；`serve()` 的其余 776 行不在本波范围 |
| `serve()` 内域分派块 | 197 | **≤ 75** | 15 个样板臂各 1 行 + `Plugin` 43 行 + `Thread` 14 行 + `Unknown` 1 行 |
| `server.rs` 顶层函数 | 125 | **0** | 全部搬进子模块 |
| 测试数 | 现状 | **不变** | 只搬不改，`server/tests/` 不动 |
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
5. **`serve()` 只降到 ~856 行，`transport/mod.rs` 会偏大。** 这是刻意接受的：
   `serve()` 的其余部分含 30 个局部量的初始化，搬它的任何一段都需要一并搬借用关系，
   等于提前做第三波。**若评审认为 `transport/mod.rs` 856 行不可接受，
   本块的正确做法是缩小范围（只做其他 10 组），而不是在 `serve()` 里冒险。**

---

## 3. C3 块：`rpc/thread.rs` 的 `handle()` 拆分

### 3.1 这一块和前 18 个域不一样

`rpc/mod.rs` 的模块注释记录了已完成的一次重构：把 `serve()` 里 62 个域方法的
内联臂按域搬进 18 个子模块。**`thread` 域是那次拆分没走完的残留**——
它按「域」拆了，但域内 25 个 method 仍在一个 2,347 行的 `match` 里。

所以 C3 的性质是「同一层级再拆一层」，方法论与 C2 一致（纯搬移），
落点是 `rpc/thread/` 目录。

### 3.2 目标分组（4 组，按调用者语义而非行数均分）

| 组 | method | 实测行数 | 理由 |
|---|---|---|---|
| `lifecycle` | `start` / `resume` / `fork` / `delete` / `clear` | **1,030** | 都要建/拆 agent 与 runtime，共享 `start_thread_core` / `attach_delegation` |
| `settings` | `rename` / `setModel` / `setReasoningEffort` / `setPreserveThinking` / `setPermissionMode` / `setPinned` / `setTag` / `markRead` / `reorderPinned` / `compact` | **828** | 全是对 meta 的读-改-写，共享 `ThreadStore` |
| `content` | `list` / `listAll` / `diff/read` / `view/read` / `subscribe` / `readItems` | **487** | 都是只读投影，共享 `state.workspaces` / `state.threads` |
| `lead_and_groups` | `lead` + `group/{create,update,setMembers,delete}` | **85** | 组长与分组管理 |

合计 **2,341** 行（`handle()` 体 2,347 行，差值 6 是分组边界的空行）。
`handle()` 外壳与 6 个 helper 留在 `rpc/thread/mod.rs`。

⚠️ **`lifecycle` 组 1,030 行超标**：`thread/resume` 单臂 386 行、`thread/delete` 215 行。
把 `resume` 与 `delete` 内的大段逻辑抽成私有 helper（`resume_restore_history`、
`delete_cleanup_subscriptions`）是合理的，但**这已经从「纯搬移」变成「函数内重构」**。
本块的取舍：**先接受 `lifecycle.rs` 1,030 行**（仍在 1,000 附近的同一量级），
或把 `resume` 单独成 `lifecycle/resume.rs`。推荐后者——它本来就是最大的一臂。

目标结构：

```
src/rpc/thread/
├── mod.rs              # handle() 外壳 + 4 个 owns() + 共用 helper + re-export，≤ 200 行
├── lifecycle/
│   ├── mod.rs          # start / fork / clear
│   ├── resume.rs       # resume（386 行）
│   └── delete.rs       # delete（215 行）
├── settings.rs
├── content.rs
└── lead_and_groups.rs
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
# 拆前
git show HEAD:src/rpc/thread.rs \
  | sed -n '70,2416p' \
  | grep -oE '"thread/[a-zA-Z/]+"' | tr -d '"' | sort -u > /tmp/thread_methods_before.txt
wc -l /tmp/thread_methods_before.txt    # 期望 25
```

拆后：

```bash
grep -rhoE '"thread/[a-zA-Z/]+"' src/rpc/thread/ | tr -d '"' | sort -u > /tmp/after.txt
diff /tmp/thread_methods_before.txt /tmp/after.txt && echo "OK: 25 个 method 一个不差"
```

另外**必须断言 `owns()` 的并集恰好等于这 25 个**——不是靠 diff 名子串，
而是写一个测试：

```rust
#[test]
fn every_thread_method_is_owned_by_exactly_one_group() {
    for m in ["thread/list", "thread/listAll", /* ...25 个... */] {
        let n = [lifecycle::owns(m), settings::owns(m), content::owns(m), groups::owns(m)]
            .iter().filter(|x| **x).count();
        assert_eq!(n, 1, "{m} 必须恰好属于一个组");
    }
}
```

**这个测试是本块的核心验收**，比「编译通过」强得多：编译通过只能证明代码能跑，
证明不了 25 个 method 都还能被分派到。

### 3.4 步骤与验收

```
C3a  录 method 基线 + 写「每个 method 恰好归一组」的测试（先失败）
C3b  content 组（只读投影，最简单，验证搬移机制）
C3c  settings 组
C3d  lifecycle/ 三片（start / resume / delete 分文件）
C3e  lead_and_groups 组 + handle() 外壳改成分组分派 + 测试转绿
```

### 3.5 C3 完成判据

| 指标 | 拆分前 | 目标 |
|---|---|---|
| `rpc/thread.rs` | 2,810 | **≤ 200**（`mod.rs`：外壳 + 4 个 `owns()` + helper） |
| 最大子模块 | — | **≤ 450**（`lifecycle/resume.rs` 386 + helper 抽取后） |
| `handle()` | 2,347 | **≤ 60** |
| method 数 | 25 | **25，且每组恰好认领自己的** |

---

## 4. C2 与 C3 可以并行（更正上一轮的判断）

上一轮我说「C2 必须在 C3 之前」，理由是「C3 的臂体大量用 `state.`，跟着 `AppState`
的搬移走最省事」。**实测后这条不成立**，原因：

- C3 的臂体用的是 `state: &mut AppState<'_>` 的**字段访问**，而 `AppState` 定义在
  `rpc/mod.rs`，两个块都不改它。
- C3 对 `crate::server::X` 的引用（`store_lookup`、`start_thread_core`、
  `attach_delegation` 等）靠 C2 的 re-export 继续可达（规则 2/4）。

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
wc -l src/server.rs src/rpc/thread/mod.rs                    # ≤ 250 / ≤ 200
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

L=open('src/server.rs').read().split('\n')
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
print(f"字段={len(f)} mut={sum(1 for x in f if \"&'a mut\" in x)}")
PY
```

---

## 7. 明确不做的事

1. **不拆 `AppState` 的 41 个字段（含 8 个 `&mut`）。** 那是第三波的事，需要单独 spec——
   它要把「主循环局部量」变成真正的状态机，行为风险远高于搬移。本波只是让
   `AppState` 出现在更多文件里，字段与签名一律不变。
2. **不动 `server/tests/` 的 8 个文件（约 1.4 万行）。** 那是 C1 的产物。规则 4
   （类型留在 `server.rs`）正是为了不动它们。
3. **不动 `protocol.rs` 与 `translate.rs`。** 前者影响 TS 绑定；后者 652 行生产代码、
   `impl Translator` 476 行，未超阈值。
4. **不动 `thread_store.rs`（1,286 行生产 / 2,975 行测试）。** 它在 C1 后已接近合理，
   且被 `rpc/thread.rs` 与 `server.rs` 同时依赖，动它会把两块的重叠面引进来。
5. **不做 C4（装配统一）与 D3（`main.rs`）。** 见文首说明。
6. **不为拆而拆。** 某组不足 150 行且只有 2 个函数时，并入同层最大模块，不单独成文件
   （`migration.rs` 82 行 3 个函数属临界，可并入 `rpc_io.rs`）。
