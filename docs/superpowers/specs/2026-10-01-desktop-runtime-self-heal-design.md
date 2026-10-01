# 桌面端子 Agent runtime 归属与自愈设计

**目标：** 让桌面端（Tauri GUI + `yi-agent app-server` sidecar）不再因为"另一个进程
（Terminal 里的 CLI/TUI）退出"而永久失去子 Agent 委派能力；并在 daemon 不健康时
自动接管、重建。

**状态：** 已设计，待实现。

**相关文档：**
- `docs/superpowers/specs/2026-09-30-desktop-subagent-delegation-design.md`（桌面端委托链路的现状接线）
- `docs/superpowers/specs/2026-08-09-runtime-daemon-design.md`（daemon 生命周期与 IPC 契约，`:12` 明确"不安装 launch agent/service"）
- `docs/project-management/yi-agent-app-server.md`（per-cwd attach 现状）
- `docs/bug-list.md` 第 40 条（TUI 侧 `replace_wedged_daemon` 的由来）

---

## 1. 问题

### 1.1 现象

用户在 Terminal 里跑一个 demo（该进程起过 yi-agent 的 subagent runtime），关掉
Terminal 后，**桌面 App 里的委派全部失效**：无法再 `spawn_agent`，且不会自行恢复，
只能重启 App。

### 1.2 根因（已核实，附代码位置）

1. **daemon 不是一个独立进程，它的命绑在"启动它的那个进程"上。**
   `Daemon` 持有内核 `flock`（`InstanceLock`）与监听线程
   （`yi-agent-rs/crates/yi-agent-store/src/ipc.rs:671`）；`impl Drop`
   （`ipc.rs:983`）与 `stop_listener`（`ipc.rs:838`）会在持锁进程退出时结束监听。
   socket 路径由**项目目录**决定（`ipc.rs:909` `socket_path_for`；超长时回退
   `$TMPDIR/yi-agent-<hash16>.sock`，`ipc.rs:929`），与"谁在监听"无关。

2. **app-server 一旦复用别人的 daemon，就再不持有它的句柄。**
   `attach_project_runtime`（`yi-agent-rs/crates/yi-agent-subagent/src/attach.rs:115`）
   在 `Daemon::start_with_factory` 返回 `AlreadyRunning` 时设
   `embedded_daemon = None`（`attach.rs:127-131`）。若该 daemon 由 Terminal 里的
   CLI/TUI 先起，桌面端只是"蹭"了它的 socket；那个进程一退，桌面端脚下的地基就没了。

3. **没有任何自愈路径。**
   - 按 cwd 的 attach 结果缓存**只插不删**（`yi-agent-app-server/src/server.rs:137`
     是唯一的读写点，全文件无 `remove`），一旦写入 `Ok` 便永久有效。
   - 六个委派工具在注册时把 `runtime_socket` / `session_id` / `task_id` /
     capability **冻结成值**拷贝进去
     （`yi-agent-subagent/src/lib.rs:1097` `register_attached_root_tools`、
     `:1118` `register_application_subagent_tools`、`:1213` `DaemonApplicationSpawnAgentTool`）。
   - 全仓检索 `alive / liveness / try_connect / remove_file.*socket` **零结果**：
     既无探活也无重连。
   - 工具被搬进长驻的 `run_thread_driver`（`server.rs:666` / `:829`），无法中途换 registry。

4. **失败只降级、不告警。** attach/tooling 失败一律 `tracing::warn!`（`server.rs:98`），
   不产生任何面向 UI 的错误，用户看不出"runtime 挂了"。

5. **TUI 有补丁，桌面端没有。** CLI/TUI 在 bring-up 前会调
   `replace_wedged_daemon`（`yi-agent/src/main.rs:789`，调用点 `:636` / `:865`）去退休
   "活着但回 `internal`"的卡死 daemon；该函数在 bin crate，桌面端共享不到，也**不覆盖
   "socket 已死"**这一情形。

### 1.3 本设计选择的策略（用户决策）

- **归属策略 = B（不健康才接管）：** 遇到"已存在且健康的 daemon"就复用、不抢占；
  只有 `Dead`（socket 不可连）或 `Wedged`（`Status` 回 `Internal`）才停掉并自己重建。
  代价见 §6。
- **自愈 = 必须做：** 连接失败时判定 stale，重建 daemon + 重新 attach，然后重试一次。

---

## 2. 范围

**做：**
- 共享的三态探活 + 接管/重建（`yi-agent-subagent::attach`）。
- 委派工具的活绑定（`RuntimeBinding`），使 daemon 换代后工具自动指向当届 runtime。
- 每 thread 每轮的廉价探活 + 单飞修复。
- app 启动的归属闸门（复用既有的"自动恢复最近对话"路径，见 §3.6）。
- 消除 bin 里 `replace_wedged_daemon` 的重复实现（改为调用共享层）。

**不做：**
- 不引入 launchd/service 常驻 daemon（`2026-08-09-runtime-daemon-design.md:12` 明确排除）。
- 不做子 agent 任务树的崩溃恢复 / replay（重启后被 sweep 回收，见 §6）。
- 不改前端协议与 UI（`spawn_agent` 仍是普通工具；失败语义见 §3.7）。
- 不顺手修 `docs/bug-list.md` 第 28 条（跨目录全局最近对话）——预热覆盖范围以其为准。

---

## 3. 设计

### 3.1 共享探活与接管（`yi-agent-subagent::attach`）

新增：

```rust
/// daemon 的可达性/健康度三态。Unknown 单列：权限、EMFILE 这类不是"死了"，
/// 不应触发接管。
pub enum RuntimeProbe { Healthy, Wedged, Dead, Unknown }

/// 用只读的 `Status` 探测。`Dead` = ENOENT/ECONNREFUSED（socket 没了或没人 accept）；
/// `Wedged` = 连上了但回 `Internal`（与 TUI 现有 `replace_wedged_daemon` 同一判据）。
pub fn probe_runtime(socket_path: &Path) -> RuntimeProbe;

/// 取用（或重建）该项目 runtime：Healthy -> 复用（embedded_daemon=None）；
/// Dead -> start_with_factory（持锁即证明无活 daemon）+ attach；
/// Wedged -> Stop 退休 + start_with_factory + attach；Unknown -> 不接管，原样返回失败。
pub fn ensure_owned_runtime(
    cfg: &RuntimeConfig,
    runtime_dir: PathBuf,
) -> Result<AttachedProjectRuntime, AttachFailure>;
```

- `Daemon::start_with_factory`（`ipc.rs:702`）已具备"持锁 → 清理残留 socket 节点 →
  重建"的语义，死 daemon 的现场可被安全回收；本设计只补"先探测到它死了"。
- bin 的 `replace_wedged_daemon`（`main.rs:789`）改为调用共享的 probe + retire，
  TUI 行为零变化，且顺手消除重复实现。

### 3.2 活绑定（`RuntimeBinding`）

六个工具不再持冻结值，改持共享句柄：

```rust
pub struct RuntimeBinding {
    cfg: RuntimeConfig,
    runtime_dir: PathBuf,
    /// 当届 runtime（daemon 换代后替换）。
    current: StdMutex<Option<Arc<AttachedProjectRuntime>>>,
    /// 单飞：同 cwd 并发修复串行化（一个拿锁起 daemon，其余读到 AlreadyRunning 转复用）。
    repair: StdMutex<()>,
    /// 本进程自己起的 daemon 句柄，持有以防 drop（drop 即停监听）。
    _daemon: StdMutex<Option<Daemon>>,
}

impl RuntimeBinding {
    /// 取当届 runtime；缺失或已 stale 时修复后返回。
    pub fn current_or_repair(&self) -> Result<Arc<AttachedProjectRuntime>, String>;
    /// 显式修复：单飞 + ensure_owned_runtime，成功后替换 `current`。
    fn repair(&self) -> Result<Arc<AttachedProjectRuntime>, String>;
}
```

- `register_attached_root_tools(registry, binding: Arc<RuntimeBinding>)`；六个工具
  （`lib.rs:1207` / `:1213` 等）改为调用时 `binding.current_or_repair()` 取**当届**
  socket 与 root 三元组后再拨号。
- `ProjectRuntimes` 从 `HashMap<cwd, Result<Arc<AttachedProjectRuntime>>>`
  升级为 `HashMap<cwd, Arc<RuntimeBinding>>`（`server.rs:69`）。
- **旧 root 不显式 detach**，理由见 §3.3。

### 3.3 daemon 换代后旧 root 的处理（决策与理由）

`recover_inflight_tasks`（`repository.rs:2645`）在启动时把 `running` 任务置为
`recovery_required`，随后 `reclaim_orphaned_tasks`（`repository.rs:2736`）在
grace 期限（`DEFAULT_ORPHAN_GRACE_SECS = 300`，`repository.rs:24`）后把被弃的 root
置 `cancelled`、其 attachment 置 `detached`（`repository.rs:2835`）、释放 lease。

**考虑并否决"稳定 idempotency key 复用同一 root"：** `attach_application_root`
（`runtime.rs:692`）确实按 key 幂等，同 key 会返回同一 root 并对 detached 自动
reattach。但它对"daemon 崩溃后处于 `recovery_required` 的 root"是否能在
`activate_application_root`（`runtime.rs:917`）后被安全唤醒**不确定**——
`recovery_required` 在既有恢复设计中是"需显式 resume 的暂存态"，不是可自动恢复态。
把 repair 建在这个不确定行为上会引入更隐蔽的失败。

**采用：** repair 用新的 uuid key 建**全新 root**；旧 root 由 daemon 启动 sweep 收敛
（grace 300s 后 `cancelled` + `detached`）。代价（旧 session/task 短期残留）见 §6。

### 3.4 数据流

```
工具调用
 └─ binding.current_or_repair() → runtime
     └─ send_request(runtime.socket, …)
         ├─ Ok            → 正常返回
         └─ Err(connect)  → binding.repair()          // 单飞：ensure_owned_runtime
                            └─ 取新 current，重试一次
                                ├─ Ok  → 正常返回
                                └─ Err → 本轮工具错误（降级；下次调用会再试）
```

**每轮探活：** driver 在 `agent.run()` 前对该 thread 的 binding 做一次
`probe_runtime`（只读 `Status`，本地 socket，亚毫秒级）；`Dead/Wedged` 先 `repair()`。
这把"已知死了却还要先失败一次"的窗口压到"轮内"。仍无法消除"探活健康、随后立刻死"
的竞态——那是任何方案都消不掉的。

### 3.5 触发点与归属闸门

| 时机 | 行为 |
|---|---|
| app-server 启动（sidecar 拉起） | 不新增独立阶段；见 §3.6 |
| `thread/start` / `thread/resume` | 经 `binding_for(cwd)`：首次 `ensure_owned_runtime`，之后取缓存 |
| 每个 turn 开始 | 该 thread 的 binding 做一次 `probe`，不健康则修复（§3.4） |
| 工具调用连接失败 | `repair()` + 重试一次（§3.4） |
| `thread/delete` 且有项目无活 thread | 维持现有 `detach_unused_runtimes`（`server.rs:156`），但**改为经 binding**，不复用已游离的旧句柄 |

### 3.6 归属闸门（复用既有自动恢复，不新增阶段）

桌面端启动即 `thread/listAll` 并恢复最近对话（`desktop/src/App.tsx:296-299` 取
`groups.flatMap(g=>g.threads)[0]`，:123 走 `thread/resume`），而 resume 在 app-server
内就 `attach_delegation`（`server.rs:799`）。因此 **app 启动 → 自动 resume →
attach** 已是天然的归属点，无需额外"启动预热"。

**但不在 start/resume 时抢占**：按 B，此时探测到健康的外部 daemon 就复用，
不 `Stop` 它（§1.3）。真正的接管发生在它死/卡死之后。

### 3.7 错误处理与降级

- 探活/修复失败 → 仍 `tracing::warn!` 降级（与 `server.rs:98` 同语义）。
- **关键差异：** 因为工具改为活绑定，失败**不再永久**——下一次工具调用或下一轮探活
  会再次触发修复。现状的永久失效正是"缓存 Ok + 冻结 socket"造成的，这是治本点。
- `Unknown` 探活结果不接管、不改缓存，仍走降级。

---

## 4. 测试

**共享层（`yi-agent-subagent`）：**
- `probe_reports_dead_without_a_socket`、`probe_reports_wedged_on_internal`、
  `probe_reports_healthy_on_status`。
- `ensure_owned_starts_a_daemon_when_none_is_listening`（Dead → 起新 daemon + attach 成功）。
- `ensure_owned_reuses_a_healthy_daemon`（Healthy → `embedded_daemon=None`）。
- `binding_repair_is_single_flight`（并发 N 次 repair 只起一个 daemon）。

**app-server 集成（临时 git repo + 临时 runtime + mock provider）：**
- `a_dead_runtime_is_repaired_on_the_next_tool_call`：attach → 停掉 daemon → 发
  `spawn_agent` → 断言自愈后调用成功、root 三元组已换新。
- `a_turn_start_repairs_a_dead_runtime_before_running`（§3.4 探活路径）。
- 回归：existing `a_git_project_gets_the_delegation_tools` /
  `a_non_git_cwd_attaches_in_place_with_the_delegation_tools` /
  `two_threads_in_one_cwd_share_one_attached_runtime`
  （`server.rs:1800` 起）必须继续绿。

**回归命令：** `cargo test -p yi-agent-subagent`、
`cargo test -p yi-agent-app-server`、`cargo test -p yi-agent --bin yi-agent`、
`cd desktop && npx tsc --noEmit && npm test`。

---

## 5. 完成判据

1. 复现本次故障后能自愈：`cargo test -p yi-agent-app-server --lib <自愈用例>` 全绿。
2. 桌面端在项目 runtime 被外部进程带走后，**无需重启 App**，下一次委派（或下一轮）
   恢复可用。
3. TUI 行为零回归：`cargo test -p yi-agent --bin yi-agent`。
4. 单飞成立：同一 cwd 多 thread 并发修复只起一个 daemon。

---

## 6. 代价、风险与已知边界

1. **B 的固有窗口：** 外部（如 TUI）起的**健康** daemon 之后死掉时，自愈发生在
   "下一次工具调用 / 下一轮探活"，该次调用会先失败。§3.4 的每轮探活把窗口压到轮内。
2. **不抢占的代价：** 若 Terminal 先起了健康 daemon，桌面端复用；关掉该 Terminal 后，
   桌面端要等下一次调用/下一轮才自愈（功能恢复，但有一次性失败）。这正是本次现象，
   已被自愈覆盖。
3. **旧 root 短期残留：** repair 建新 root，旧 root 在 grace（300s）后被 sweep 收敛；
   期间 DB 里会多一条 `recovery_required`/`cancelled` 的 session/task 记录。属于有界的
   清理延迟，不是泄漏。
4. **子 agent 任务不可恢复：** daemon 崩溃前在跑的子任务会被 sweep 回收，不做 replay
   （丢 daemon 的固有后果，非本设计引入）。
5. **`recovery_required` 不自动唤醒：** 本设计不去唤醒被 park 的旧 root（理由见 §3.3）。
6. **预热覆盖是尽力而为：** 归属闸门走"自动恢复最近对话"路径；跨目录全局最近
   （含"升级安装旧对话不显示"）是已登记的 P2 bug（`bug-list.md` 第 28 条），本设计不顺手改。
7. **签名波及：** `register_attached_root_tools` 改收 `Arc<RuntimeBinding>` 会波及 TUI
   调用点，是本设计最集中的机械改动。

---

## 7. 文档同步（实现阶段）

- 更新 `docs/project-management/desktop.md` 与 `yi-agent-app-server.md`（归属策略 +
  自愈 + 判据）。
- 更新 `docs/project-management/subagent-runtime.md` / `yi-agent-subagent.md`
  （新增 `probe_runtime` / `ensure_owned_runtime` / `RuntimeBinding`）。
- `docs/bug-list.md` 新增一条（[ ] 未修复 → 实现后改 [x]），附根因、修复位置、验证命令。
