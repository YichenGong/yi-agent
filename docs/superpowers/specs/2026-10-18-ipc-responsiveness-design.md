# IPC 响应性：主循环去阻塞 + plugin/daemon 快路径

日期：2026-10-18
状态：设计已定稿，待实现
分支：`perf/ipc-responsiveness`

## 1. 问题

用户报告：Desktop 里连 `pwd && ls` 这样的小命令都执行得很慢。

根因不是命令本身慢，而是**所有 thread 共享一个串行主消息循环**：

- Desktop 只有**一个** sidecar 进程（`yi-agent app-server`）。所有 thread 都是该进程内的独立 tokio driver task，**不隔离进程、不隔离 CPU 核**。
- 所有 thread 的 RPC 都经由 `serve()` 里单个 `loop { tokio::select! { stdio_eof, inbound.recv(), turn_rx.recv(), card_ticker.tick(), lead_ticker.tick() } }`（`yi-agent-rs/crates/yi-agent-app-server/src/server.rs:2474` 的 `loop`、`:2479` 的 `select!`）。
- **任何在该循环分支体内 `await` 到完成的阻塞，都会串行堵住所有 thread 的所有请求。** 慢的不是 `pwd && ls`，是它排在被堵住的主循环后面。

实测：`pwd && ls` 本身 p50=10.0ms（sandbox 不是瓶颈），但用户体感很慢——因为它要等主循环空出来。

### 1.1 量化证据

`sample <app-server-pid> 6`，主线程 4730 样本：

```
非空闲执行样本 2171，其中 1613 (74%) 卡在同步 socket IPC __recvfrom
  561  card_scheduler::run_once → board_cards → plugin_query → __recvfrom
  285  card_scheduler::run_once → board_query  → plugin_query → __recvfrom
  407  board/list → lifecycle::status_with → send_request → __recvfrom
按挂钟计：约 34% 的主循环时间花在这些阻塞上
```

三处根因（均已定位到行）：

| 项 | 位置 | 机制 | 实测 |
|---|---|---|---|
| **Item 1** | `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs:38,102,112` | accept 循环 `set_nonblocking(true)` + `std::thread::sleep(200ms)` 盲轮询 | 直连 plugin socket 连续 30 次 `list`：min=0、**median=271ms**、双峰（6 次 ~0ms / 22 次 ~260ms） |
| **Item 2** | `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:2914`（tick 分支）、`:4797`（`card_scheduler_tick` 定义）、`rpc/board.rs:98`、`rpc/plugin.rs:29` | `card_scheduler_tick`（每 3s）与 `board/*`、`plugin/*` 域 handler 在**主循环内联**执行，内含同步 socket IPC | tick 每轮约 2 次 IPC（每次 ~210ms）；desktop 每 2s 一拍含 `refreshSelectedBoard`=527ms、`refreshBoards`=666ms |
| **Item 3** | `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:3094-3103` | `respond()` **每请求**新建 tokio runtime + 重跑 `reconcile_worker_events()` + `RuntimeRepository::open()`（`Connection::open` + 两条 PRAGMA + `migrate` 的 schema 解析） | `Status`（纯 daemon，不经 plugin）p50=**45ms**、min=6.9；10 并发 wall=48ms（几乎串行） |

### 1.2 因果

**Item 1 放大 Item 2**：`card_scheduler::run_once` 一轮有 2-5 次同步 IPC，每次都撞上 plugin 的 200ms 轮询，单轮最坏约 1.5s / 3s 周期。所以 Item 1 是高杠杆（改完每次 IPC 从 270ms 降到 ~45ms），Item 3 次之（45ms 降到 ~10ms），Item 2 是结构性隔离（把阻塞彻底移出主循环）。

## 2. 范围

**做**：三个 Item，一份 spec，一个 worktree。

**不做**（各自独立，记入 §9 后续）：
- subagent worker 线程放大（`yi-agent-subagent/src/lib.rs:847` 每个 worker 一个 18-worker runtime；daemon 因此有 306 个 tokio worker 线程）
- relay invalid-token 重连风暴（实测 2773 次 / 6525s，约 25 次/分钟）
- `yi-agent-runtime/src/bootstrap.rs:443` 同类「每次会话新建多线程 runtime」
- desktop 侧轮询频率（当前 2s 一拍，不做推送改造）

## 3. Item 1：plugin accept 改 `poll(2)`

### 3.1 现状

`plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs`：

```rust
const ACCEPT_POLL: Duration = Duration::from_millis(200);

pub fn serve_with<D, F>(socket: &Path, dispatch: Arc<D>, keep_going: F) -> std::io::Result<()> {
    // ...
    let listener = UnixListener::bind(socket)?;
    listener.set_nonblocking(true)?;
    while keep_going() {
        match listener.accept() {
            Ok((stream, _)) => { /* serve_connection */ }
            Err(error) if error.kind() == WouldBlock => std::thread::sleep(ACCEPT_POLL),  // ← 根因
            Err(error) => return Err(error),
        }
    }
    // ...
}
```

`serve_with` 的 doc 本就写着「阻塞调用；调用方放到自己的线程里」，且 `start_query_server` 确实 `thread::spawn` 了它（`superpowers-kanban-runner/src/main.rs:758`）。所以「非阻塞 + sleep」只是为了周期性检查 `keep_going`，代价却是**每次连接延迟 0-200ms**。

`keep_going` 在生产中恒为 true（`main.rs:832` 是 `let _query_server = start_query_server(...)`，返回的 `stop` 标志从未被置位）；但两个测试靠 `stop.store(true)` + `handle.join()` 收尾（`server.rs:255`、`:295`）。

### 3.2 改法

把 `sleep(ACCEPT_POLL)` 换成对 listener fd 的 `poll(POLLIN, ACCEPT_POLL)`：

```rust
while keep_going() {
    // 连接一到立刻返回；超时才回到 keep_going 检查。检查节奏仍是 ACCEPT_POLL。
    let ready = poll_listener_once(&listener, ACCEPT_POLL)?;   // 内部 libc::poll(POLLIN, timeout)
    if !ready { continue; }
    match listener.accept() { /* 原样 */ }
}
```

`poll_listener_once` 用 `libc::poll` + `AsRawFd`，约 15 行 `unsafe`（同 crate 内、有边界注释）。ipc crate 增加依赖 `libc = "0.2"`（`Cargo.lock` 已有 0.2.189；runner crate 已在用 `unsafe { libc::flock(...) }`，先例现成）。

### 3.3 为什么选它（而非其它两个候选）

- **纯阻塞 accept**：`keep_going` 只在每请求间被检查，两个测试会永久挂死，必须重写停机语义与两个测试。
- **缩短 ACCEPT_POLL 到 5ms**：1 行改动，但仍是盲轮询（200 次/秒空唤醒），只是把症状压到可接受。
- **`poll(2)`**：连接一到零延迟返回；`keep_going` 检查节奏**完全不变**（仍 200ms），故**两个现有测试一行不改即可继续通过**——这本身就是回归保证。

## 4. Item 2：RPC 域卸载（D）+ tick 快照（A'）

### 4.1 关键事实（决定了方案形态）

1. **主 runtime 是多线程的**：`yi-agent-rs/crates/yi-agent/src/main.rs:50` 用 `tokio::runtime::Runtime::new()`（默认 = 18 worker）。因此 `tokio::spawn` 出去的任务在**别的 worker** 上跑，主循环立刻回到 `select!`。
   - 注意认知陷阱：`spawn_blocking(..).await` **在本结构里无效**——主 task 仍要 await 它返回才回到 `select!`。必须是「spawn 后**不 await**」。
2. **主循环已完成域化重构**：`serve()` 里是 `match crate::rpc::domain_for(method)` → `crate::rpc::{ui,model,board,plugin,...}::handle(&mut state, &req, id)`。`board/*` 与 `plugin/*` 已是独立 handler（`rpc/board.rs`、`rpc/plugin.rs`）。
3. **这两个域完全不碰主循环可变状态**。逐字段清点其 `state.` 使用：
   ```
   board.rs : board_dir, board_launcher, client, hub, plugin_command,
              resident_dir, theme_handle, watchman_home, watchman_install
   plugin.rs: board_dir, client, hub, theme_handle
   ```
   **无一 `&mut`**。且这些量全可 move 进 detached task：`hub: Arc<Broadcaster>`（内部 `StdMutex`，注释明写「锁不跨 await，`broadcast`/`reply` 可并发」）、`ThemeHandle` 是 `Clone`（两个 `Arc`）、`BoardLauncher`/`WatchmanInstall`/`WatchmanUninstall` 是 `Arc<dyn Fn + Send + Sync>`、`ClientId`/`RequestEnvelope`/`RequestId` 是 `Clone`。

### 4.2 D：把 `board/*` 与 `plugin/*` 处理 detach 出主循环

在 `serve()` 的 `Domain::Board` / `Domain::Plugin` 两个臂里，把内联 `await handler(...)` 改为 spawn 后 `continue`：

```rust
crate::rpc::Domain::Board | crate::rpc::Domain::Plugin => {
    // 门禁（initialize / scope）已在上面同步做完；detach 只搬「处理」这一步。
    let shared = SharedDomainState {              // 全部 owned / Arc，无 &mut
        hub: Arc::clone(&hub),
        client: client.clone(),
        board_dir: board_dir.clone(),
        resident_dir: resident_dir.clone(),
        plugin_command: plugin_command.clone(),
        theme_handle: theme_handle.clone(),
        watchman_home: watchman_home.clone(),
        watchman_install: Arc::clone(&watchman_install),
        watchman_uninstall: Arc::clone(&watchman_uninstall),
        board_launcher: Arc::clone(&board_launcher),
        fatal_tx: fatal_tx.clone(),               // 见 4.3
    };
    let req = req.clone();
    let id = id.clone();
    let domain = crate::rpc::domain_for(method.as_str());
    tokio::spawn(async move {
        let result = match domain {
            crate::rpc::Domain::Board  => crate::rpc::board::handle_shared(&shared, &req, id).await,
            _                          => crate::rpc::plugin::handle_shared(&shared, &req, id).await,
        };
        // 唯一需要回到主循环的信号：stdio `local` 写失败（致命）。其它错误已在
        // handler 内翻成错误响应，不影响主循环。
        if let Err(error) = result {
            let _ = shared.fatal_tx.send(error).await;
        }
    });
    continue;
}
```

`board.rs` / `plugin.rs` 的 handler 签名从 `&mut AppState` 改成 `&SharedDomainState`（字段名保持 `board_dir` / `hub` / … 不变，故函数体几乎零改动）。

### 4.3 D 的成本：致命错误通道

`write_response`（`server.rs:3148`）对 stdio `local` 客户端写失败会返回 `Err`，主循环靠 `?` 结束会话——`driver_reports_finished_when_writer_fails` 测试钉死了这条。detach 后这个 `Err` 无法再通过 `?` 传播。

**解法**：新增 `fatal_tx: mpsc::Sender<anyhow::Error>`（容量 1），在主循环 `select!` 里加一条分支：

```rust
Some(error) = fatal_rx.recv() => return Err(error),
```

detached task 只在 handler 返回 `Err` 时发一次。这与现有 `turn_rx` 通道同型。

### 4.4 A'：`card_scheduler_tick` 的读 IPC 预取

tick 独立于 RPC，且 `ServeLauncher` 真需要 `&mut threads` 来 launch 会话（`start_thread_core` 的工厂参数）。D 管不到它。做法：

- 新增一个后台 OS 线程（`std::thread` + 短 sleep，形态对齐 `start_query_server`；**不用 `tokio::spawn`**，因为 `board_cards` 走同步 `send_request`），周期性地为每个已登记项目预取 `board_cards` 结果，发布到 `Arc<StdMutex<HashMap<PathBuf, Vec<BoardCard>>>>`。
- `run_once` 的**对账**段改读该快照（`card_scheduler.rs:283` 处 `board_cards(...)`）。
- **写路径与 `board.next_launch`（认领）保持实时**——它们是状态变更，必须权威；`next_launch` 在无 queued 卡时只打 1 次 IPC，Item 1+3 之后成本约 10-20ms / 3s（约 0.5%），可接受。

**同型的第二处内联 tick（同批处理）**：主循环还有 `_ = lead_ticker.tick()`（`server.rs:2943`，30s 一次 `LEAD_TICK`），同样因需要 `&mut threads`（`deliver_to_lead`）而内联，且注释明写「`tokio::spawn` 会拆掉这条前提」。实测 `lead_tick_once`（`server.rs:5349`）本身只读 `threads` 做内存判定、**不含同步 IPC**，故它不产生 §1.1 那类阻塞；本 spec 不改它，只在此记录，避免实现者误以为漏了一处。

### 4.5 为什么 D+A' 优于纯快照

| | 纯快照 | D + A' |
|---|---|---|
| 覆盖 `plugin/query`（**最大一路 527ms**） | 覆盖不到（params 任意，无法预取） | 整个 Plugin 域 |
| 数据陈旧 | ≤1s 窗口 | RPC 路径**零陈旧** |
| 新增共享状态 | 需 `BoardSnapshot` + 后台线程 | 复用已有域模块，仅加 `SharedDomainState` |
| 失败语义 | 静默保留旧值 | 与现状一致（真实报错） |
| 落点 | 主循环内多处 | 2 个臂 |

### 4.6 快照失败语义（A' 部分）

- 预取线程拿不到结果时**保留旧值**，但必须 `eprintln!` 记录；连续 N 次（建议 5）失败后**清空该项目的快照条目**，让对账退回实时读取（而非永久停在旧值）。
- 快照尚未覆盖某项目（刚登记）时，对账退回实时 `board_cards`（一次性阻塞，可接受，行为需写进代码注释）。

## 5. Item 3：daemon 请求快路径

`yi-agent-rs/crates/yi-agent-store/src/ipc.rs`：

1. **复用 runtime + repository**：`respond()`（`:3094`）不再每请求 build runtime、不再每请求 `RuntimeRepository::open`（含两条 PRAGMA 与 `migrate` 的 `sqlite3Prepare`/`sqlite3RunParser`，栈采样可见）。改为在 daemon 启动时建一处长生命周期实例；`rusqlite::Connection` 是 `!Sync`，用 `Mutex` 串行持有（现状 coordinator 已串行，不引入新的并发度）。
2. **合并 reconcile**：daemon 的 accept 循环本已每轮跑 `reconcile_worker_events()`（`:883`），`respond()` 里那次（`:3102`）是重复的。改为带极短新鲜度窗口的合并（≤5ms 内不重跑），保留「请求观察到已对账状态」的语义。
3. **accept 循环改 `poll(2)`**（同 Item 1 手法）：消掉每请求 0-10ms 的 `sleep(10ms)` 基线盲等与约 100 次/秒空唤醒（`:945-948`，`set_nonblocking` 在 `:859`）。

**若风险 1 或 2 触发现有测试的时序假设**，则退化为「只复用 runtime/repository，保留每请求 reconcile」——那才是 45ms 的大头（runtime 建立 + sqlite open + schema 解析）。

## 6. 验收

| 项 | 指标 | 手段 |
|---|---|---|
| 1 | 连续 30 次直连 plugin `list`：min≈0、**median<5ms**、无 >50ms 样本（双峰消失）；两个 `stop` 测试继续通过 | 只读探针 + `cargo test -p superpowers-kanban-ipc` |
| 2 | `sample` 主线程：`run_once`+`status_with` 内 `__recvfrom` 占比 **34% → <5%** | `sample` 解析 + `cargo test -p yi-agent-app-server --lib` |
| 3 | `Status` **p50<10ms**；`PluginQuery` p50 与 `Status` 同量级（<20ms） | 只读探针 + `cargo test -p yi-agent-store --lib` |
| 全局 | 全程不 kill / 不重启 pid 43799（superpowers-kanban）与 3132（daemon serve）；只发只读探测 | — |

**证据边界（必须诚实记录）**：Item 1 与 Item 3 的「改后实测」需要重启 plugin 二进制与 daemon 才生效。本次交付能提供的证据是：(a) 改前实测基线（本文档 §1.1）；(b) 离线基准（直接跑新编译的 plugin 二进制 / daemon 单测）；(c) 单测全绿。真机「改后」数据由用户在方便时重启后采集。

## 7. 交付

`scripts/rebuild.sh plugin` 会把新插件 `install` 到 `/opt/homebrew/bin/superpowers-kanban`，但**运行中的 pid 43799 用的是进程内旧二进制**。

约定：**本次只改源码 + 提交；spec 明确「需重启 plugin 与 daemon 生效」；由用户在方便时重启。绝不主动 kill 任何运行中的进程。**

## 8. 风险与缓解

| # | 风险 | 缓解 |
|---|---|---|
| 1 | D 改变 stdio 写失败的致命语义 | `fatal_tx` 通道（4.3）；跑 `driver_reports_finished_when_writer_fails` 与 `write_response_to_a_gone_ws_client_is_not_fatal` |
| 2 | D 让 `board/create`（拉 daemon + 装 watchman）与其它 RPC 的**相对顺序**不再确定 | `registry::write` 是 tmp+rename 原子（`registry.rs:104`）；`create_with_project` 幂等；desktop 不并发发这些；spec 记录该顺序变化 |
| 3 | 同一文件并发 read-modify-write 可丢更新（detach 前不存在，因串行） | 同上（原子 rename + 幂等）；`boards.json` 的 read-modify-write 已在单进程内，detach 后仍在单进程内（只是不同 task），风险增量低 |
| 4 | 响应乱序 | 已确认安全：desktop 客户端按 id 缓冲（`rpc.ts:188` 起）；测试 harness `rpc()` 按 id 匹配（`rpc_tests.rs` `read_response(h, id)`） |
| 5 | `initialize`/scope 门禁若被 detach 绕过 → 越权 | 门禁在 `server.rs:2621` 同步完成，detach 点在其后；不改门禁 |
| 6 | Item 3 删除每请求 reconcile 破坏时序假设 | 先跑 `cargo test -p yi-agent-store --lib` 评估依赖面；必要时退化为只复用 runtime/repository（§5 末段） |
| 7 | 一次改三棵 crate 树 + `server.rs`（`yi-agent-app-server`，7,925 行）回归面宽 | 每 Item 独立 commit 可回滚；按 crate 跑测试；Item 2 只加接线、复用既有域边界 |
| 8 | 无并发 cargo（OOM / exit 137） | 跑测试前 `ps aux \| grep cargo` 确认为空；按 crate 跑而非 `--workspace` |

## 9. 后续（本次不做）

- 除 `board/*` 与 `plugin/*` 外的域，若将来引入同步 IPC，同样按 D 的做法卸载。
- daemon 每请求 reconcile 的彻底消除（推模型而非拉模型）。
- subagent worker 线程放大（`subagent/src/lib.rs:847`）。
- relay invalid-token 重连风暴（`ws.rs:288` 的 `ws connection rejected`）。
- desktop 从 2s 轮询改为服务端推送（`ui/board/updated` 通知）。
