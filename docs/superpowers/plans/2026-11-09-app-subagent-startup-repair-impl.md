# App Subagent Startup Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让隔离的真实桌面 App 完成模型 `spawn_agent → 子代理 echo hello → wait_agent`，并安全处理旧 daemon 和准备失败。

**Architecture:** 宿主共享一次解析的 BringUpChoice，连接 shell 授权仍单独保存；自动启动受同一实例闸保护。managed 启动先只读探测，再由私有 lifeline Ready 帧证明本次 daemon 就绪。失败按项目显示，确认恢复后只在新 turn 重建委派工具，不重放历史工作。

**Tech Stack:** Rust workspace、Unix sockets/FD、guardian process registry、Tauri、React/TypeScript、Vitest、cargo/nextest/just。

## Global Constraints

- Spec：[App subagent 修复设计](../specs/2026-11-09-app-subagent-startup-repair-design.md)；历史自审：[多轮记录](../reviews/2026-11-09-app-subagent-startup-spec-review.md)；本轮plan复审：[源码与执行边界复审记录](../reviews/2026-11-09-app-subagent-startup-plan-review.md)。

- 基线：`fix/managed-session-container-attach`，HEAD `a0b6f9dc` **加已有未提交修复**。使用当前 worktree，不在 main 修改，不自行合并。
- 此 plan 尚未执行；所有标注“拟新增”的符号、测试和命令均不是已有功能。
- 不自动结束旧 daemon、插件或其他前端工作；安全迁移需本地用户确认，Status-only 不能授权停止。
- 不降级 embedded 以恢复 managed 工具，不伪造 shell 权限，不重放结果未知的 spawn，不跨代复用 capability。
- 不操作正式 App、正式实例配置或正式 relay session；进程测试显式注入 HOME、YI_AGENT_HOME、runtime_dir、models_path。
- 无真实 key 记 SKIPPED，不计 A10 通过；侧车或 CLI 成功不能替代真实窗口验收。
- 不新增依赖；保持当前 Cargo.toml 的 rust-version。若实际必须新增依赖，先重新审查依赖/MSRV，不在本 plan 暗增。
- 提交前 `cargo fmt --all`、`just ci`，相关桌面测试与 build 也必须绿；纯文档提交免编译/CI。
- 每个完成单元同步 `docs/project-management/subagent-runtime.md`、其 README 计数和 `docs/bug-list.md`；中间状态用 `[ ]`，A10 未完成不能把完整 App 修复勾成 `[x]`。
- 本轮只交付文档，不运行以下实施步骤，不提交混合旧改动。未来执行前确认旧改动归属和提交边界；禁止 `git add .`/`git commit -am`。

## 文件与职责地图

以下路径相对 worktree 根。保持既有模块，不为此修复重构整个 server。

| 单元 | 现有文件 | 拟新增文件 | 职责 |
|---|---|---|---|
| 策略 | `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs`、`src/binding.rs`；app-server `src/server.rs`、`src/ws.rs`、`src/server/transport/{mod,scoped}.rs`、`src/server/{assembly,thread_index,driver_resume,plugin_bridge}.rs`、`src/rpc/mod.rs` | 无 | 共享完整 BringUpChoice；生产、冷恢复和 repair 使用同一选择 |
| 自动启动闸 | supervisors `src/watch.rs`、`src/lifecycle/{bringup,migration_gateway,migration_operation}.rs`；app-server `src/rpc/{plugin,ui}.rs`、`src/server/watchman.rs` | 无 | 一次取闸、闸内检查、freeze 无空窗 |
| 存量探测 | supervisors `src/lifecycle/bringup.rs` | 无 | 只读业务 probe，不把超时当 Absent |
| Ready | lifecycle `src/authorization.rs`；supervisors `src/lifecycle/guardian.rs`；CLI `src/managed_runtime.rs` | `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/daemon_ready.rs` | 有界帧 IO/验证独立小模块；guardian 整合认证，daemon 发帧 |
| 错误与恢复 | app-server `src/server/{assembly,thread_index,driver_resume}.rs`、`src/server/driver/turn_driver.rs`；desktop `src/{App.tsx,hooks/useRuntimeLease.ts,hooks/useRuntimeLifecycle.ts,lib/runtimeLease.ts,lib/runtimeLifecycle.ts}`、`src-tauri/src/runtime_lifecycle.rs` | `desktop/src/lib/runtimeStartupIssue.ts`、`desktop/src/components/RuntimeStartupIssue.tsx` 及同名测试文件 | 按项目错误、无 scope 提示、显式 retry、下一 turn 重装配 |
| 侧车 | `yi-agent-rs/crates/yi-agent/tests/managed_production_integration.rs` | 无 | 完整壳协议与可控网关、真实子工具 witness |
| App live | `desktop/scripts/build-sidecar.sh`（复用，不默认修改） | `desktop/scripts/verify-app-subagent.sh`、`docs/superpowers/reviews/2026-11-09-app-subagent-startup-acceptance.md` | 显式打包/启动入口、人工或原生自动化真实窗口验收记录 |

测试使用现有 app-server `src/server/tests/{assembly,thread_index,transport,turn}.rs` 和各模块已有 `#[cfg(test)]`；新增稳定测试名见各 Task。文件归属中的简称 lifecycle/supervisors/app-server/CLI 分别指对应 `yi-agent-rs/crates/yi-agent-*`。

## 执行与提交规则

所有 Rust 命令在 worktree 的 `yi-agent-rs/` 下运行；桌面命令在 `desktop/` 下。先 `git status --short` 保存基线，不覆写旧修复。每个 Task 的测试需实际先红后绿，编译失败仅能证明新增接口缺失，最终还须行为断言；测试零命中不是通过。

每单元结束：更新该项验证记录 → `cargo fmt --all` → 相关测试 → `just ci` → 桌面相关命令（涉及 desktop 时）→ 按路径或 `git add -p` 暂存本单元和进度文档 → 检查 staged diff → conventional commit。不得把别的 Task 或旧未提交代码顺带 stage；无法分离时先整理提交边界，不擅自提交旧代码。这里的步骤可按实际运行时间拆成小步，但不能省略验证。

---

### Task 1：完整有效策略贯穿生产装配（A1–A4）

**Files:** 修改文件地图中的策略单元；测试 app-server `src/server/tests/{assembly,transport}.rs`、subagent `src/attach.rs` 的 tests。

**Interfaces（拟新增/修改）:**

```rust
// attach.rs：纯解析部分由 process_bringup_choice 调用；不改变既有优先级。
pub fn bringup_choice_from(
    value: Option<&str>, context: Option<&ManagedContext>,
) -> crate::binding::BringUpChoice;
// RuntimeAttachments、PluginChannelState：共享同一 Arc，不可由 RPC JSON 覆盖。
bringup: std::sync::Arc<yi_agent_subagent::binding::BringUpChoice>,
// assembly.rs：旧 attach_cwd_runtime 可保留 wrapper，生产只用显式版本。
pub(crate) fn attach_cwd_runtime_with(
    runtimes: &ProjectRuntimes, runtime_dir: &std::path::Path,
    cfg: &yi_agent_runtime::config::RuntimeConfig, models_path: &std::path::Path,
    bringup: &yi_agent_subagent::binding::BringUpChoice,
) -> Result<std::sync::Arc<yi_agent_subagent::binding::RuntimeBinding>, String>;
```

`RuntimeBinding` 使用现有 `yi_agent_subagent::binding::RuntimeBinding`，不能新造第二个类型。解析函数的 `ManagedContext` 特指 `yi_agent_subagent::attach::ManagedContext`（入口上下文），不是 managed_binding.rs 的同名scope结构。完整 choice 的 expected/instance_home 原样保存给 binding repair；生产 `thread_index::attach_delegation` 增加 `&BringUpChoice` 入参，所有调用者显式传递。`production_factory` 不做 attach，不向它塞一个未使用的配置。

WS装配拟新增 `serve_ws_inner_with_attachments` 接缝：沿用现有listener/cfg/workspaces/pairing/theme/build_agent参数，另接 `Arc<RuntimeAttachments>` 与显式home/resident_dir，不读真实HOME或models_path。生产 `serve_ws_body`构造资源一次并调用该入口，现有 `serve_ws_inner`仅兼容wrapper；测试必须用新入口。scoped入口须在watch启动前解析同一Arc并注入watch与attachments，不能watch早启动后才建立快照。fixture使用tempdir的home/models/resident/runtime和假provider、ensure/query/install计数闭包，不依赖生产HOME中缺模型来制造attach失败。

- [x] **Step 1：写纯解析失败测试 `effective_bringup_choice_preserves_context_and_overrides`。** 在同进程构造 context，使用下列数据表；每例还断言 expected 和 instance_home 未丢失。context fixture 使用现有 ArtifactIdentity fixture，不修改 OnceLock 或环境。

```rust
let cases = [
    (true, None, BringUpPolicy::Managed),
    (false, None, BringUpPolicy::Embedded),
    (true, Some("embedded"), BringUpPolicy::Embedded),
    (false, Some("managed"), BringUpPolicy::Managed),
    (true, Some("invalid"), BringUpPolicy::Managed),
    (false, Some("invalid"), BringUpPolicy::Embedded),
];
for (declared, value, expected_policy) in cases {
    let mut context = context.clone();
    context.managed_frontend = declared;
    let choice = bringup_choice_from(value, Some(&context));
    assert_eq!(choice.policy, expected_policy);
    assert_eq!(choice.expected.as_ref(), Some(&context.expected));
    assert_eq!(choice.instance_home.as_ref(), Some(&context.instance_home));
}
assert_eq!(bringup_choice_from(None, None).policy, BringUpPolicy::Embedded);
```

- [x] **Step 2：运行 red。** `cargo test --all-features -p yi-agent-subagent --lib effective_bringup_choice_preserves_context_and_overrides`；预期函数未定义，加入实现后须见 1 个行为测试通过。
  - 实测：`crates/yi-agent-subagent/src/attach.rs::tests::effective_bringup_choice_preserves_context_and_overrides`（六例数据表 + expected/instance_home 保真）与 `a_contextless_process_carries_no_identity` 均 pass（`cargo nextest ... -E 'test(effective_bringup_choice_preserves_context_and_overrides) or test(a_contextless_process_carries_no_identity)'` → 2 passed）。
- [x] **Step 3：最小解析和传递实现。** 纯函数按上述表选择 policy，context 映射 expected/home；`process_bringup_choice` 仅取 env/context 并调用它。scoped/WS 入口在 context 安装后解析一次 Arc；transport 共享给远程连接。watch 读 `bringup.policy == Embedded`，assembly 调用 `bring_up_with(bringup, ...)`，binding repair 保持原 choice；替换生产中的重读全局路径，旧 wrapper 只供兼容调用者。

```rust
// 宿主决策不改变连接身份。
let may_start_embedded = state.bringup.policy == BringUpPolicy::Embedded;
let may_install_startup = may_start_embedded && !state.managed_session;
// 不写 state.managed_session = !may_start_embedded。
```

  - 实测落地：`RuntimeAttachments.bringup`、`AppState.bringup`、`ServeLauncher.bringup`、
    `driver_tick(.., bringup, ..)`、`attach_delegation(.., bringup, ..)` 全部是同一份
    `Arc<BringUpChoice>`；`scoped.rs` 两个 stdio 入口各解析**一次**并与 watch 共享
    （`spawn_resident_watch_loop(dir, Arc::clone(&bringup))` + `bringup: Arc::clone(&bringup)`）。
  - watch 判据改为快照：`plugin_bridge.rs` 用 `bringup.policy == BringUpPolicy::Embedded`，
    不再调 `is_managed_frontend()`；`legacy_migration_tests` 断言生产代码里
    `is_managed_frontend()` 出现 0 次、`process_bringup_choice()` 每入口恰 1 次。
  - 插件安装新增第三个独立判据：`PluginChannelState.bringup_policy`，安装分支
    `if state.managed_session || retired || !host_allows_embedded`。
  - `assembly::attach_cwd_runtime`（重读全局的旧 wrapper）改为 `#[cfg(test)]`，
    生产不再可达；生产调用一律 `attach_cwd_runtime_with(.., bringup)`。
  - **与 plan 字面形态的偏差（有意，已在下方 Step 4/5 用测试钉住）**：WS 侧没有新增
    `serve_ws_inner_with_attachments`，而是把 `bringup: Arc<..>` 沿
    `serve_ws → serve_ws_body → serve_ws_inner → RuntimeAttachments` 传下去。
    计划该接缝的实质目的（①入口只解析一次 ②watch 与 attach 共用同一份）已由
    `legacy_migration_tests` 的「ws 也恰 1 次」断言钉住；剩下的只是 fixture 能否注入
    隔离 home/models，属既有缺口（`serve_ws_inner` 本就读真实 `models_path()`），
    与本轮缺陷无关，留作后续独立项而不是在本轮扩大改动面。

- [x] **Step 4：新增装配测试并 green。** 稳定名 `shared_managed_host_remote_install_does_not_ensure`、`embedded_host_plain_install_still_ensures`、`production_attach_uses_injected_bringup_choice`。通过共享宿主 transport fixture 让远程连接无 shell 身份：旧 install 落盘，startup awaiting-demand，ensure/query-wait/watchman 计数均 0；embedded 对照 ensure > 0。watch fixture 记录 tick 完成而非 sleep。运行：`just test-crate yi-agent-app-server`、`just test-crate yi-agent-subagent`。
- [x] **Step 5：判别性验证。** 临时只将 watch 改回声明判据、只删远程宿主 policy 条件、只把 assembly 改回全局 choice，分别运行对应测试，必须红；逐项恢复并复跑绿。记录 diff 与结果，不回滚 CLI 前置。

  | 变异 | 结果 | 恢复后 |
  |---|---|---|
  | watch 改回 `!is_managed_frontend()` | `the_watch_loop_defers_to_the_managed_frontend_declaration` FAIL | PASS |
  | 删 `!host_allows_embedded` 条件 | `shared_managed_host_remote_install_does_not_ensure` FAIL（`embedded_host_plain_install_still_ensures` 仍 PASS，证明变异只打中该判据） | PASS |
  | `attach_delegation` 改回 `process_bringup_choice()` | `production_attach_uses_injected_bringup_choice` FAIL | PASS |

  - `production_attach_uses_injected_bringup_choice` 走**真实接线** `attach_delegation`
    （不是直接调 `attach_cwd_runtime_with`）：早先直接调下层 helper 的版本在变异下仍
    PASS，因为变异点在上一层；改走真实接线后才具备判别力。这是本轮一次真实的
    「测试看似覆盖、实则打不到」修正。
  - 崩溃后的完整复跑：`-p yi-agent-subagent` 244 passed；`-p yi-agent-app-server`
    1505/1506（唯一失败是文档已记录的既有并发 flake，见 `docs/bug-list.md` 关于
    「失败集合逐轮漂移」的判别说明，单跑即过）。
- [x] **Step 6：按执行与提交规则提交。** 建议 message：`fix: share effective runtime policy across app entry points`。进度仍 `[ ]`，因为自动启动闸 A5 尚未完成。
  - 按用户要求**不自行合并**：只在本 worktree 分支提交，`git merge`/移除 worktree/删分支留给用户决定。

### Task 2：一次持闸覆盖副作用并关闭 freeze 空窗（A5）

**Files:** 文件地图中的自动启动闸单元；测试 watch、migration_operation/gateway 的现有 tests、app-server plugin/UI tests。

**Interfaces（拟新增/修改）:**

```rust
// bringup.rs：仅 gateway 交付迁移时 Some；普通 managed 启动 None。
// 所有已有 HeldScope 字面量明确初始化，不能为了通过编译私自 acquire。
retirement_gate: Option<super::migration_operation::RetirementGate>,
// watchman helper：完整预算签名和生产闭包见下方；不能保留无deadline副作用入口。
pub(crate) fn install_watchman_under_gate(
    gate: &yi_agent_supervisors::lifecycle::migration_operation::RetirementGate,
    install: &crate::server::WatchmanInstall,
    exe: &std::path::Path, home: &std::path::Path,
    deadline: &yi_agent_lifecycle::deadline::Deadline,
) -> Result<(), String>;
```

**预算接口与所有权（拟新增，先测试后接线）：**

```rust
// server.rs生产闭包改签名，所有scoped/WS/plugin/UI及tests字面量同步。
type EnsureDaemon = Arc<dyn Fn(&Path, &Deadline) -> Result<bool, String> + Send + Sync>;
type WatchmanInstall = Arc<dyn Fn(&Path, &Path, &Deadline) -> Result<(), String> + Send + Sync>;
// supervisors/daemon.rs：旧ensure_running保留wrapper；project仍是workdir不是runtime_dir。
// 生产显式注入instance_home，不能新入口再依赖全局socket home。
pub fn ensure_running_until(project: &Path, instance_home: &Path, deadline: &Deadline)
    -> Result<bool, String>;
// app-server/watchman.rs：参数沿用现有类型，新增budget。
pub(crate) fn install_watchman_under_gate(
    gate: &RetirementGate, install: &WatchmanInstall,
    exe: &Path, home: &Path, deadline: &Deadline,
) -> Result<(), String>;
```

`watch::once_unattended_as` 新增显式 deadline 的变体，ensure callback 接 `(&[PathBuf], &Deadline)`；旧wrapper兼容非生产调用者。watch loop 的同步取闸/ensure必须整个放入blocking pool，不在Tokio主循环直接调用。plugin改为**一个**持gate的blocking工作单元覆盖ensure→query-wait→install，不能三个独立任务或靠timeout取消JoinHandle。worker自身检查deadline，过期不发起下一副作用；gate只能由实际worker结束释放，调用方丢弃future不能使闸提前释放。

supervisors `watchman.rs` 新增 `install_until` 与budget-aware launchctl接缝，旧 `Command::status` 改为spawn→try_wait轮询同deadline；超时仅kill/wait本次launchctl child，实际launchd是否已接受bootstrap记unknown，不自动重试bootstrap或bootout新服务。短本地文件IO无法承诺内核硬截止：预算约束是每一步前检查、阻塞操作返回后复核，未知状态保留闸/对账责任，不能宣称阻塞线程可强制取消。migration同样不得因外层超时先释放闸。query-readiness与Status的connect/write/read用Task3的绝对deadline IO，不能沿用固定READY_TIMEOUT或只readtimeout。

追加逐单元red/green：① watch blocking调度及expired-budget ensure=0；② plugin中ensure耗尽预算后query/install=0；③注入迟到worker，caller超时后竞争者仍取不到闸，worker结束才释放；④ launchctl fixture沉默/退出慢，不碰真实launchd。命名 `expired_startup_budget_prevents_later_side_effects`、`timed_out_caller_does_not_release_worker_gate`、`watchman_launchctl_obeys_deadline`，前两项放app-server现有plugin tests，最后放supervisors watchman tests。分别运行 `cargo test --all-features -p yi-agent-app-server expired_startup_budget_prevents_later_side_effects`、同crate第二名、`cargo test --all-features -p yi-agent-supervisors --lib watchman_launchctl_obeys_deadline`；逐条确认非零命中。先旧闭包行为red，再修改签名/生产接线green，不以编译失败代替行为red。

- [x] **Step 1：写 `migration_keeps_gate_until_freeze_is_durable` red 测试。** 用真实隔离 gate 和两个线程/channel：迁移已取得 scope、freeze 尚未写；线程 B 用短 Deadline 尝试取 gate，必须 WouldBlock。放行 freeze，确认标记存在后 B 取闸成功但自动启动拒绝。线程 B 成功取得闸却未见 freeze 时测试失败；所有 recv 用 recv_timeout，不用固定 sleep 决定时序。

```rust
// 屏障事件的次序是测试契约；各 channel 由测试构造。
entered_rx.recv_timeout(budget).expect("migration reached pre-freeze barrier");
assert_eq!(contender_rx.recv_timeout(budget).unwrap(), "would-block");
continue_tx.send(()).unwrap();
frozen_rx.recv_timeout(budget).expect("freeze persisted");
assert!(retirement::is_frozen(&instance_home));
```

- [x] **Step 2：运行 red。** `cargo test --all-features -p yi-agent-supervisors --lib migration_keeps_gate_until_freeze_is_durable`，预期现有立即 drop gate 违反断言。测试需记录 B 确实尝试，不因未调度假绿。
- [x] **Step 3：最小锁修改。** `take_locks` 返回 HeldScope 时保存 Some(gate)，协调器在 Freeze 成功后 `held.retirement_gate.take()` 释放；held 需可变。Accepted/证据检查/故障注入提前返回由 RAII 释放，不再取第二次 gate。普通 ensure 的 HeldScope=None。用同一 deadline 覆盖 gate→scope→freeze，不将 root/lease callback 留在 lock 区间。

```rust
// Freeze 成功持久化之后；失败返回保留 RAII 语义。
if matches!(step, Step::Freeze) {
    drop(held.retirement_gate.take());
}
```

- [x] **Step 4：自动启动实现与 tests。** watch、旧 plugin 收尾、UI watchman 启用在 blocking pool 取一次 gate，持闸重查 retired/frozen。plugin 的 ensure→query-wait→watchman-install 同一预算同一 gate，under-gate helper 不再取闸。managed 直接拒绝；plugin 已落盘返回 awaiting-demand/queryable=false；UI 启用偏好保留、ok+warning，禁用仅卸载。新增测试 `plugin_startup_gate_covers_ensure_query_and_watchman`、`managed_watchman_preference_is_saved_without_install`，计数断言与 A5 屏障一起 green。
- [x] **Step 5：覆盖失败释放及正面 witness。** freeze 写失败/取 scope 超时/Accepted 后 fault，另一线程随后可取 gate；其他 scope 等待仍有界。无标记 embedded 对照实际 ensure；检查同调用 helper 不自锁。运行 `just test-crate yi-agent-supervisors`、`just test-crate yi-agent-app-server`；变异恢复立即 drop gate，屏障测试必须红，恢复后绿。

  **实现与判别性证据（本轮实测）：**

  | 判别点 | 变异 | 结果 |
  |---|---|---|
  | 闸必须活过「freeze 可见」 | `take_locks` 取到 scope 后立刻 `drop(gate)` | `migration_keeps_gate_until_freeze_is_durable` FAIL（5s 内失败，报「the gate must still be held while the freeze is not yet durable」） |
  | 失败必须归还闸 | 失败路径 `mem::forget(held.retirement_gate.take())` | `a_failed_freeze_returns_the_instance_gate` FAIL（报「the gate must be released when the migration fails」） |
  | 预算耗尽不得再动副作用 | query 等待去掉 `!budget.is_expired()` | `expired_startup_budget_prevents_later_side_effects` FAIL（报「a spent budget must not be spent further on a query wait」） |
  | launchctl 必须受预算约束 | `run_bounded` 改回 `Command::status()` | `watchman_launchctl_obeys_deadline` FAIL（`sleep 30` 真等到 30s 才返，超过 5s 断言） |

  - 关键修正一：屏障测试最初**自己构造** `HeldScope`，所以「`take_locks` 立刻放闸」的变异打不中（变异下仍 PASS）。改为经生产的 `SystemWorld::take_locks` 取锁后才具备判别力——这正是计划要求的「测试看似覆盖、实则打不到」的修正。
  - 关键修正二：`freeze` 失败时 `confirm` **不返回 Err**，而是把 `failed_at` 记进 journal 并返回 Ok（既有契约）。测试按此断言，而不是假设 Err。
  - 关键修正三：plugin 的 query 等待若直接吃满 30s 单元预算，会让既有 `plugin_install_succeeds_but_reports_not_queryable_when_the_socket_never_appears`（等 30s 事件）超时。改为 `budget.clamp(PLUGIN_READY_TIMEOUT)`，保留原 5s 上限。
  - 覆盖范围：`take_locks` 现在把 gate 放进 `HeldScope.retirement_gate`，`confirm` 在 **Freeze 阶段 journal 落盘之后**才 `take()`；失败/提前返回由 RAII 释放。plugin 的 ensure→query-wait→watchman-install 合并为一个持闸 blocking worker（`rpc/startup.rs::start_plugin`），调用方丢 future 不提前放闸。
  - 验证：`-p yi-agent-supervisors` 522 passed / 0 failed；`-p yi-agent-app-server` 1508 passed，2 failed 均为既有并发 flake（`compact_does_not_erase_last_item_id`、`a_failed_append_still_records_the_completion`，单跑与定向复跑全绿，失败集合逐轮漂移，见 `docs/bug-list.md`）；`-p yi-agent-subagent` 244 passed；`cargo clippy --all-targets --all-features -p yi-agent-app-server -p yi-agent-supervisors -- -D warnings` 干净。
- [x] **Step 6：按规则提交。** `fix: serialize embedded startup with durable migration freeze`，提交 `82e80fb3`。证据：`supervisors` 522 通过、`app-server` 1508 通过（2 个既有并发 flake，单跑即过）、`subagent` 244 通过、clippy 干净；判别性变异四项均成立（取到 scope 即 drop gate → `migration_gateway` 屏障测试红；失败路径泄漏 gate → `a_failed_freeze_returns_the_instance_gate` 红；query 等待跳过预算 → 插件预算测试红；`run_bounded` 改回 `Command::status()` → launchctl 测试 30s 超时红）。

### Task 3：业务 socket 只读预检（A6、A8 的准入）

**Files:** supervisors `src/lifecycle/bringup.rs`、migration_operation.rs 生产 MigrationProbes 接线；进程测试 managed_production_integration.rs。

**Interfaces（拟新增）:**

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusinessProbeOutcome { Absent, Answered, Unresponsive(String) }
pub trait BusinessProbe {
    fn probe(&self, scope: &RuntimeScope, deadline: &Deadline) -> BusinessProbeOutcome;
}
pub struct BusinessChannelProbe;
// 两个显式入口：公开给integration fake使用；生产wrapper注入真实业务probe。
pub fn ensure_managed_with_business(
    scope: &RuntimeScope, expected: &ArtifactIdentity, intent: BringUpIntent,
    deadline: Deadline, probe: &dyn ControlProbe, business: &dyn BusinessProbe,
    launcher: &dyn GuardianLauncher, required_capabilities: &[String],
) -> std::io::Result<BringUpResult>;
pub fn ensure_managed_under_guard_with_business(
    held: &HeldScope, expected: &ArtifactIdentity, intent: BringUpIntent,
    probe: &dyn ControlProbe, business: &dyn BusinessProbe,
    launcher: &dyn GuardianLauncher, required_capabilities: &[String],
) -> std::io::Result<BringUpResult>;
// 旧公开wrapper只转发，不复制决策；迁移HeldScope调用第二入口。
```

`Answered` 只表示业务回答，不包含伪造 RuntimeIdentity。业务 Status 没有 generation，不能解析成 current managed。

测试准确位置为 supervisors `tests/managed_bringup.rs`，复用 `test_scope`、`artifact`、`ScriptedProbe` 和计数launcher，增加 `ScriptedBusinessProbe`（RefCell回答队列与最后结果），fake全显式注入；`tests/migration_gateway.rs`、`tests/runtime_migration.rs` 原fake调用者也同步，不能旧wrapper突然探环境socket。Inspect仍在CoordinationGuard取得前返回，禁止mkdir；可做只读业务probe，但不进入launch或selection路径。补store `ipc/client.rs` 的 `send_status_until(socket: &Path, deadline: &Deadline) -> Result<IpcResponse, IpcError>` 并从ipc/mod.rs导出，复用既有request/response codec；connect使用nonblocking Unix socket+poll+SO_ERROR，write/read逐次poll剩余预算及限制响应长度，非阻塞EAGAIN重探同deadline。不得把 `send_request_with_version_timeout` 的仅readtimeout当总deadline。测试独立UnixStream/listener fixture覆盖connect异常、silent、partial response和有效Status，错误映射Absent仅限ENOENT/ECONNREFUSED，其他fail closed。

- [x] **Step 1：写 `business_live_without_guardian_never_launches` 和 `business_unresponsive_is_not_absent` red 测试。** 实际新增 4 条：上述 2 条 + 正面对照 `both_channels_absent_still_launches` + `inspection_reports_an_unsupervised_business_daemon`；`ScriptedBusinessProbe`（`RefCell<usize>` 计数 + 最后结果）与 `RecordingLauncher` 复用。 使用现有 ControlProbe/GuardianLauncher stub，加 BusinessProbe stub。预期 controlAbsent+businessAnswered 返回 ManualActionRequired，launch 0；Unresponsive 到预算耗尽仍 launch 0；两端Absent+claim/产物正常的对照 launch 1。

```rust
// 在当前 bringup 测试 harness 新增 business 回答队列与 launched 计数。
assert!(matches!(result, BringUpResult::ManualActionRequired { .. }));
assert_eq!(launched.load(std::sync::atomic::Ordering::SeqCst), 0);
// inspect 也不能产生选择文件、runtime目录、Stop或root。
assert!(!selection_path.exists());
```

- [x] **Step 2：运行 red。** 编译期 red：`business_*` 测试因 `BusinessProbe`/`BusinessProbeOutcome`/`ensure_managed_with_business` 未定义而 `E0432`，确认接口先于实现存在。
- [x] **Step 3：实现 bounded probe。** 落地：`store/ipc/client.rs::send_status_until`（非阻塞 `socket`+`connect`+`poll(POLLOUT)`+`getsockopt(SO_ERROR)`，写/读逐次按剩余预算 `poll`，`EAGAIN` 重探同一 `Deadline`，响应长度受 `MAX_FRAME_BYTES` 限制，并校验 `protocol_version`/`request_id`）；`ipc/mod.rs` 导出。`supervisors/lifecycle/bringup.rs` 新增 `BusinessProbe`/`BusinessProbeOutcome`/`BusinessChannelProbe`，仅 `ENOENT`/`ECONNREFUSED` 映射 `Absent`，其余 fail closed 为 `Unresponsive`。`ensure_managed_with_business` 与 `ensure_managed_under_guard_with_business` 为唯一决策点，旧 wrapper 与 `migration_operation.rs::SystemMigrationBackend` 均转发/引用之。业务预检位于 coordination lock 内、`claim_is_ours`/`record_selection`/launch 之前；Inspect 先探 control，仅当其 `Absent` 时再只读探业务，仍在取锁前返回。
 从 scope.instance_home/runtime_dir 用现有 socket 解析函数取得业务路径；connect ENOENT/ECONNREFUSED→Absent，permission/timeout/invalidresponse→Unresponsive；Status 完整有效→Answered。socket 文件存在不等于活。接入 coordination lock 内、所有 launch 前（含 HeldScope）；Inspect 不落盘。控制端可回答仍保持兼容分类，同时不得用控制 Connected 声称 rootReady。
- [x] **Step 4：进程 green 与正面 witness。** `existing_embedded_daemon_blocks_managed_start_without_being_stopped` 通过：真实 `daemon serve`（无 guardian）先正面回答 `Status` 并持有 `runtime.sqlite`；`_shell/prepare` 返回 `Blocked`（reason 含 `without a supervisor`）；control socket 未出现、guardian journal 为空、旧 PID/数据库/应答全部存活；随后的手动分支由 fixture 自己 `SIGTERM` 旧进程，再重探得 `Connected`/`mode=managed` 且 daemon 记录数为 1。证据：`yi-agent` 1093 通过、`supervisors`+`store` 1098 通过。
 新测试 `existing_embedded_daemon_blocks_managed_start_without_being_stopped`：隔离 fixture 启动真实旧 embedded，记录 birth identity、Status 正面回答、在途 fixture 工作；managed prepare 返回 manual且无第二daemon，旧PID/工作仍活。用户明确处理分支由测试 fixture 自己停止其所有者，再双端Absent重探；自动代码没有 Stop。`cargo test --all-features -p yi-agent --test managed_production_integration existing_embedded_daemon_blocks_managed_start_without_being_stopped`，再跑 supervisors crate。
- [x] **Step 5：按规则提交。** `fix: refuse managed startup over an existing business daemon`（未合并，等用户处理）。判别性变异：删除业务预检 → `business_live_without_guardian_never_launches`/`business_unresponsive_is_not_absent` 红，且进程级 A6 测试得到 `Connected/mode=managed`（真的在活 daemon 之上起了第二个）红；`classify_business_error` 全返回 `Absent` → `a_silent_listener_is_unresponsive_not_absent` 红；`wait_for` 忽略 deadline → `a_dripping_peer_cannot_extend_the_budget` 红（2.26s vs 预算 300ms）。三者恢复后全绿。存量/认证项仍 `[ ]`，等待 Task4 与安全迁移验收。

### Task 4：私有、有界 daemon Ready（A7）

**Files:** lifecycle authorization.rs；supervisors lifecycle/mod.rs 注册 daemon_ready.rs、guardian.rs；CLI managed_runtime.rs；各模块 tests、managed_production_integration.rs。

**Interfaces（拟新增）:**

```rust
// authorization.rs：保留 LifelineHello::new 签名（普通辅助测试可 None）。
// new managed-role 启动实际调用 new(...).with_startup_nonce(nonce)。
#[serde(default)]
pub startup_nonce: Option<String>,
impl LifelineHello {
    pub fn with_startup_nonce(mut self, nonce: String) -> Self {
        self.startup_nonce = Some(nonce); self
    }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DaemonReady {
    pub startup_nonce: String,
    pub scope_key: String,
    pub daemon_instance_id: DaemonInstanceId,
    pub build_id: String,
    pub business_protocol: u32,
}
// daemon_ready.rs：使用现有 Deadline，不在 wire 上传 monotonic Instant。
pub const MAX_READY_FRAME_BYTES: usize = 16 * 1024;
pub fn read_ready(stream: &mut std::os::unix::net::UnixStream,
                  deadline: &Deadline) -> std::io::Result<DaemonReady>;
pub fn write_ready(stream: &mut std::os::unix::net::UnixStream,
                   ready: &DaemonReady, deadline: &Deadline) -> std::io::Result<()>;
pub fn validate_ready(actual: &DaemonReady, expected: &DaemonReady) -> std::io::Result<()>;
```

`DaemonInstanceId` 和 BuildId 复用 lifecycle types 的 String 别名。business_protocol 已核实为 u32，配套 daemon 和 guardian 使用本次 artifact.business_protocol_max 作为期望值，不能把 min/max 范围偷偷缩成不同常量。16KiB 只限制这份 Ready 帧，不修改其他协议上限。nonce 由现有密码随机源生成独立 32 bytes→hex；不能由 generation_secret 派生。

- [ ] **Step 1：写 `daemon_ready_rejects_mismatched_identity_and_nonce`、`daemon_ready_partial_frame_has_a_real_deadline`。** 用 UnixStream::pair；逐字段修改 nonce/scope/instance/build/protocol，validate 必须 InvalidData；peer 写长度但只写半个 body且保持连接，reader 必须在预算内 TimedOut。增加 0/超长长度、EOF、无 nonce、迟到 Ready 对照。

```rust
let mut wrong = expected.clone();
wrong.startup_nonce.push('0');
assert_eq!(validate_ready(&wrong, &expected).unwrap_err().kind(),
           std::io::ErrorKind::InvalidData);
// partial-frame peer 保持打开，用 channel 在断言后释放；不是直接 drop 导致 EOF。
assert!(started.elapsed() < budget + std::time::Duration::from_secs(1));
```

- [ ] **Step 2：运行 red。** `cargo test --all-features -p yi-agent-supervisors --lib daemon_ready_`；补 helper 后必须有行为测试，不止缺符号。
- [ ] **Step 3：实现帧 IO。** 延用 hello 的 **control codec envelope**：4字节 big-endian body 长度 + JSON `{control_version, payload}`，用 lifecycle `codec::encode/decode`，拒绝不支持的 CONTROL_VERSION，不直接序列化裸 DaemonReady。`MAX_READY_FRAME_BYTES` 含4字节header；检查 body长度非零且 `body_len <= MAX_READY_FRAME_BYTES - 4` 后才分配。有界循环读完整frame再decode，写encode后的完整frame。每次read/write前检查同一deadline并设置剩余timeout；保存并在成功/失败都恢复原 read/write timeout，恢复失败也fail closed。测试增加 `daemon_ready_uses_versioned_control_envelope`，断言错误版本被拒绝、编码与hello codec一致。禁止依赖一次 read_exact 的每次 syscall timeout 来宣称整体有界。

**Ready 失败归一与提交约束：** 在 lifecycle authorization.rs 拟新增公共 `fresh_startup_nonce() -> std::io::Result<String>`，使用该 crate 已声明的 getrandom 生成独立32bytes→hex，OS随机失败正常返回错误；supervisors不直接引用未声明getrandom，不从generation_secret派生。guardian `start_daemon` 只建一次权威Deadline，`registry.spawn`传其clone，不再重建完整预算；hello写失败立即失败，不能仅warn继续。计算socket/path尽可能在spawn前完成；spawn之后所有 `?` 路径统一进入归属child回收。先完成Ready校验、bridge非阻塞设置等全部fallible准备，再短锁核验birth/alive并一起提交self字段，禁止先设置self.daemon后再 `set_nonblocking()?`。失败时依据registry process_id+owner回收并wait/reap，本次启动预算耗尽后只进入独立且明示的有界cleanup预算（不是延长成功等待），未确认回收则报告cleanup unknown并保留registry责任；不能杀旧socket应答者。新增 `daemon_hello_write_failure_reclaims_only_spawned_child`、`daemon_bridge_setup_failure_does_not_publish_partial_ready`，故障注入点由测试构造，断言self字段无partial ready且旧进程活着。

运行中guardian不虚构现有长期reader：拟用主监督循环对 lifeline `poll(POLLHUP/POLLERR)`及非消费 `peek(MSG_PEEK|MSG_DONTWAIT)`判EOF，和registry exit事件共同撤ready及本代授权。启动read_ready结束后不再读该方向数据，本期daemon仅发一次Ready；若未来加其他帧另行定义dispatcher，不能引入竞争reader。daemon→guardian写Ready及恢复timeout必须在共享FD授权reader启动前完成；reader安装失败/daemon退出由guardian EOF/registry监控撤销ready。EOF与存活child分离的测试必须实际走主监督循环。
- [ ] **Step 4：接入 daemon 和 guardian。** managed daemon 验证 scope与必需nonce，取得独占锁、listener、ledger/foreground bridge及managed监督后，通过唯一序列化writer先写Ready；写入并恢复共享 FD 的 timeout 完成后，才将 FD 交给长期授权 reader。授权帧可暂存在 socket buffer，reader 安装失败不能报 ready。guardian 私有FD唯一启动reader消费Ready，匹配expected与registry.birth，准备bridge成功后短锁查询活性，才提交self.daemon/ledger/bridge。保留registry durable-spawn barrier；删除“任意Status成功即可ready”的权威条件。启动等待不能持registry mutex。失败仅回收本次process_id对应托管child；启动失败归一与运行中监控细节见下面 Ready 约束。

```rust
validate_ready(&actual, &expected)?;
// 在此之后短查registry所属process_id的birth与alive，再提交本次ready。
// 若失败：terminate_granted/wait_exit使用该次process_id与owner，绝不按旧socket PID杀进程。
```

- [ ] **Step 5：进程 green。** `unrelated_status_responder_cannot_satisfy_daemon_ready` 直接驱动 guardian 的 `start_daemon` 私有测试接缝（不先调用会拦截旧进程的 bringup 预检），让活旧 responder占业务socket，新child取锁失败/退出；记录 registry spawn 已发生及 Ready 等待已进入的屏障 witness。guardian不得ready且旧responder仍活。A6另测预检，不能用它代替本例。`daemon_exit_revokes_current_ready` 覆盖启动确认后退出；另加 `daemon_lifeline_eof_revokes_ready_while_child_is_alive`，peer保持child存活但关闭私有FD，ready必须撤销。新guardian+旧无noncedaemon拒绝、不fallback；不同build现存guardian继续既有兼容选择，不要求补发Ready。运行生命周期、supervisors、CLI相关测试和 managed_production_integration。
- [ ] **Step 6：变异与提交。** 仅绕过Ready校验改回Status成功判断，A7进程测试须红，恢复后绿。按规则提交 `fix: authenticate daemon readiness over its private lifeline`。

### Task 5：失败可见、确认恢复与下一 turn 重装配（A8、A11）

**Files:** 文件地图中的错误与恢复单元；额外修改 app-server `src/session.rs`、`src/server/turn/{mod,start}.rs`、`src/rpc/thread.rs`、`src/server/driver/launcher.rs` 的恢复context/driver调用接线；现有 RuntimeVersionBanner 只复用布局，补无scope组件；app-server现有 turn/thread tests、desktop新增issue tests、useRuntimeLease相关tests。

**执行顺序：本Task三个独立review单元，禁止一次混改。**

- [ ] **5A.1：先写UI issue/retry tests（下面Step1中无scope、late failure、retry三例），mock原hook调用，分别看到缺反馈的行为red。**
- [ ] **5A.2：只实现issue/retry与App展示，运行Step2三个文件，实际计数green；DOM/unmount/remote按钮断言通过后review，不动driver。**
- [ ] **5B.1：写native held真实回包测试及A→B→A、release失败两个hook tests，分别运行native单测和useRuntimeLease.test.tsx，确认旧回包/释放规则red。**
- [ ] **5B.2：实现settle/settle_with回填、canonical对账、逐项目chain，复跑5B全部green，确认不授予ready后review。**
- [ ] **5C.1：写Step4同driver连续turn及no-root恢复tests，运行 `cargo test --all-features -p yi-agent-app-server confirmed_generation_recovery_rebuilds_only_next_turn_tooling`，旧driver激活只一次必须行为red。**
- [ ] **5C.2：实现下面driver恢复接缝，复跑同名测试并增加root拒绝、项目B不变断言green；再跑Step5跨模块回归，review后才接Step6迁移矩阵。**

每个red、实现、green动作单独推进；下面Step1–6是各单元的夹具/断言和命令细节，不要求同一次完成。每单元独立按全局验证与提交规则提交，禁止把未完成邻项标[x]。

**Interfaces（拟新增）:**

```typescript
// runtimeStartupIssue.ts，不从WebView计算canonical scope。
export type StartupStage = "acquire" | "daemon" | "root" | "tooling";
export interface RuntimeStartupIssue {
  project: string;
  requestSeq: number;
  attemptId?: string;
  scopeKey?: string;
  daemonInstanceId?: string;
  stage: StartupStage;
  reason: string; // 白名单原因，不是原始异常/帧
}
export function acceptsStartupReply(currentSeq: number, replySeq: number): boolean {
  return currentSeq === replySeq;
}
// useRuntimeLease返回：Map按项目保存错误，activeProject挑选显示，不全局单错误。
export interface RuntimeLeaseFeedback {
  issues: ReadonlyMap<string, RuntimeStartupIssue>;
  retryProject(project: string): void;
}
```

native/前端拟新增的租约反馈契约：

```rust
// ProjectAcquireView：settle/settle_with返回前按当前 ProjectState 回填，并记录同一份view。
// record_view仅存状态，不能只在那里补字段而返回未补的原view；旧数据可缺字段。
#[serde(default, skip_serializing_if = "Option::is_none")]
pub held: Option<bool>,
```
```typescript
// AcquireView增加 held?: boolean；held不是ready。
export function isHeldLease(view: AcquireView): boolean {
  return view.held === true || (view.held === undefined && view.state === "managedReady");
}
export function isReadyLease(view: AcquireView): boolean {
  return view.state === "managedReady";
}
```

回包held未知/缺字段且非managedReady时，用现有native `runtime_registered_projects` 与
`runtime_scope_for_project` 对账，不从blocked推断未持有。hook分别保存held、ready、
acquire-in-flight；held-but-blocked仍显示issue、离开时release、显式retry可再问，不能因
owned已有而吞retry；owned绝不参与root/工具准入。unknown保留对账责任，不自启动。
只在本地有权限的命令中核实，不新增远程释放入口。

**租约所有权与迟到回答约束：** `runtime_registered_projects` 返回 `Vec<String>` 的 scope key，不能拿它直接匹配项目路径。修改 native `runtime_scope_for_project`，通过现有 scope 解析器以只读 canonical 方式解析 symlink/尾斜杠别名，不 mkdir；hook拿 native 返回的 scope key 与注册集合匹配。`settle`、`settle_with`及直接返回的 starting/unknown 路径，都在返回前回填 held，并保存同一view。补 native 序列化及真实返回值测试，不能只测 `record_view` 状态。

保留 hook 现有逐项目 Promise chain；release/acquire 必须同链串行。A→B→A 中旧A回包迟到，如果A重新wanted，不能无条件release杀掉新租约；若确需release，必须排在该项目后续acquire前。旧seq不得覆盖新issue。`release_project` 的 `Result<Vec<String>, String>` 中非空 failures也不是成功；保留owned/unknown责任并只读对账，不盲目重复启动。新增 `late_acquire_does_not_release_a_rewanted_project`、`release_failures_preserve_reconciliation_duty`，以 deferred Promise 精确控制 A→B→A、release-before-reacquire 和失败次序。

后端恢复不接受远程JSON里自报generation：本地shell已认证的prepare/lease/root链路确认新代际，或在下一显式turn的现有prepare链路核实后失效cache。使用既有session/turn admission，不增设远程可调用的“清缓存并授权”RPC。

**driver恢复接缝（拟新增）：** `prepare_turn_core`仅处理RPC/排队，不持agent，不能在那里改工具就声称恢复。新增 driver-owned `DelegationReassemblyContext`，由 `server/turn/start.rs`、driver_resume/launcher创建并传到 `run_thread_driver`，保存注入RuntimeAttachments、canonical cwd/runtime_dir、thread key、shell broker与装配参数；`session.rs`/turn/mod.rs/rpc/thread.rs所有prompt/driver调用点同步。driver消费显式新TurnPrompt后、激活及 `run_tracked_blocks`/模型请求前，尝试 `reassemble_delegation_for_turn(&mut DriverDelegationState, &DelegationReassemblyContext, &TurnPrompt)`；该异步内部函数只在当前driver无正在执行turn时调用，deadline随真实prepare链路传递。`DriverDelegationState`集中持agent、foreground_root/host/resolved、activation_attempted、delegation、children watch句柄；不是只换registry。其拟新增类型定义与入口为：

```rust
// driver/turn_driver.rs：沿用当前server re-export的既有类型；不新造Agent/RuntimeBinding。
pub(crate) struct DelegationReassemblyContext {
    pub attachments: Arc<crate::server::RuntimeAttachments>,
    pub cfg: yi_agent_runtime::config::RuntimeConfig,
    pub cwd: std::path::PathBuf,
    pub runtime_dir: std::path::PathBuf,
    pub thread_id: String,
    pub mode: crate::thread_store::ThreadMode,
}
pub(crate) struct DriverDelegationState {
    pub built: crate::server::BuiltAgent,
    pub foreground_root: Option<Arc<yi_agent_subagent::thread_root::ThreadRoot>>,
    pub foreground_host: Option<Arc<crate::server::foreground::ForegroundDispatch>>,
    pub foreground_host_resolved: bool,
    pub activation_attempted: bool,
    pub delegation: Option<crate::server::delegation::ThreadDelegation>,
    pub children_watch: Option<crate::server::ChildrenWatch>,
    pub subagent_rx: tokio::sync::watch::Receiver<crate::protocol::SubagentTotals>,
}
pub(crate) async fn reassemble_delegation_for_turn(
    state: &mut DriverDelegationState, context: &DelegationReassemblyContext,
    turn: &crate::session::TurnPrompt, deadline: yi_agent_lifecycle::deadline::Deadline,
) -> Result<(), String>;
```

`BuiltAgent`由start/resume在交给driver前完整保留，不丢decision_rx/catalog/yolo/sandbox字段；driver其它会话命令相应访问state.built，不能重新bootstrap覆盖历史。ThreadSession与RPC/process watch还要共享有效tooling句柄：拟新增 `EffectiveProcessTooling { manager: Arc<yi_agent_tools::ProcessManager>, watch: Option<crate::server::ProcessWatch> }` 的 `Arc<tokio::sync::Mutex<_>>`，由start/resume/launcher注入driver和ThreadSession，替代原server `process_watches: HashMap<String, ProcessWatch>` 的watch所有权（不能map和slot同时持有watch）。恢复后在无运行turn处停旧ProcessWatch、更新同一slot并启动新watch，RPC `src/rpc/process.rs` 的list/read/kill取slot快照后立即释放锁再IO；逐一替换旧session.process_manager访问。children watch同样明确唯一owner，若原函数局部/外部map持有，完整转交driver而不留下重复owner；driver退出stop；subagent totals保留历史基线。新增 `next_turn_recovery_updates_rpc_and_process_watcher_manager`，实际启动新manager进程，通过RPC list/read/kill和process/updated通知验证都指向新manager，旧manager不得接收命令。

先认证probe/prepare及本地root授权，再只清目标project失败或确认过期binding，复用assembly attach与 `wrap_for_delegation` 核心工具构造；把所需BuiltAgent装配字段抽为共享helper，不把driver散字段假装直接能调用wrapper。原liveSession Arc、provider、model/config、permission、sandbox/extra roots/anchor保留；更新process_manager/MCP/tool registry和子任务watch通道，旧watch task取消并join或证明已终止，避免重复统计。MCP共享manager不能无故重建。无root/首次降级需重新attach，不能被 `activation_attempted=true` 或后续prompt.activate=None吞掉；不要每轮重写旧objective。新root只按新授权激活。失败删除委派工具并发白名单会话通知；不动历史或已完成/未知child。新增同driver连续两turn测试，第一次无spawn、显式恢复后第二次provider收到spawn schema；仅RPC状态变化不算通过。

- [ ] **Step 1：写 frontend red tests。** DOM测试头部加 `/** @vitest-environment jsdom */`，显式import Vitest及testing-library，afterEach cleanup/恢复timer，每例独立mock。 `acquire_failure_is_visible_without_scope` mock invoke reject/blocked+scope缺失，render无scope提示；`late_failure_does_not_replace_new_project_success` 控制promise完成顺序；`retry_reacquires_same_wanted_project_once` wanted不变点击retry只多一次invoke；blocked重渲染不自旋，remote无本地按钮。 增加 `held_blocked_lease_is_released_without_granting_ready`：native held=true/state=blocked时UI显示失败、离开release一次、没有rootReady；held缺字段通过注册查询对账，迟到held-blocked也必须release。测试最小纯断言：

```typescript
expect(acceptsStartupReply(2, 1)).toBe(false);
expect(acceptsStartupReply(2, 2)).toBe(true);
// hook tests以renderHook/act/deferred Promise控制完成顺序，assert invoke计数而非sleep。
```

- [ ] **Step 2：运行 red。** `npm test -- src/lib/runtimeStartupIssue.test.ts src/hooks/useRuntimeLease.test.tsx src/components/RuntimeStartupIssue.test.tsx`；新增文件按本Task创建，预期界面/返回值缺失。
- [ ] **Step 3：实现 issue+retry 接线。** hook按project记录请求序号和错误，成功仅清同项目最新错误；离开后迟到有效租约释放。App在没有scope也render RuntimeStartupIssue，白名单reason映射显示具体阶段，retry改变独立revision一次，starting按后端提示最多一次自动重探后转提示，不无限轮询启动。scope hook增加恢复revision依赖重新问native，不自己构造路径。现有本地migration preview/confirm/operation commands接线；取消preview无副作用、confirm丢reply查原operation、不能回滚已执行操作。Status-only显示手动处理，没有确认停止按钮；不复制version-switch状态机当legacy迁移。
- [ ] **Step 4：写 backend red test `confirmed_generation_recovery_rebuilds_only_next_turn_tooling`。** 在 `server/tests/turn.rs` 的真实 driver harness 中，使项目A首次attach durablefailure，项目B成功；A原turn运行中仅注入“待重核”信号，不声称 busy runtime 能安全迁移或新代际已认证。原turn结束后，下一显式turn完成已认证prepare，清A失败/过期binding、重新attach root及tooling；B binding不变、历史/model/sandbox/extra roots/anchor均保留，旧task spawn计数不增加。新root拒绝时tooling_error必须被会话通知返回。另加无root且 `activation_attempted=true` 的降级恢复例，不能只测试已有binding重激活。

```rust
assert_eq!(old_turn_spawn_count, spawn_count_after_recovery);
assert_eq!(project_b_binding_before, project_b_binding_after);
assert!(next_turn_tool_names.iter().any(|name| name == "spawn_agent"));
assert_ne!(old_daemon_instance_id, new_daemon_instance_id);
```

- [ ] **Step 5：最小backend实现并 green。** 用canonical project_key定位cache，认证新代际或显式retry重新验证前清该项目durablefailure；成功identity变化只标记受影响session需在新turn重build，复用driver_resume/thread装配逻辑保存原Session。thread_roots过期项只对空闲/下一turn替换；busy root不detach造成正在运行turn中断。把Activation.tooling_error通过既有失败通知送UI，禁止日志直接转原始secret。native先回填ProjectAcquireView.held并更新所有字面量、serde测试；新增 `blocked_acquire_view_reports_held_separately_from_ready`，ready仍false且authorize_root拒绝。运行 `just test-crate yi-agent-app-server`；`cargo test --manifest-path desktop/src-tauri/Cargo.toml blocked_acquire_view_reports_held_separately_from_ready`（从worktree根）；desktop `npm test`、`npm run build`。
- [ ] **Step 6：安全迁移矩阵 green与提交。** 可证明安全实例取消/确认/重复确认/丢reply/历史保留；unknown、busy、Status-only保持manual；fixture用户明确手动处理后再probe/acquire成功。按规则提交 `fix: surface runtime startup failures and recover next-turn tooling`。A8未有安全证据不可标完成。

### Task 6：真实侧车、完整壳授权与模型驱动闭环（A1、A2、A6–A9）

**Files:** managed_production_integration.rs；按需增强现有app-server诊断事件用于watch tick确定性witness，不能写secret到stdout。

**Interfaces（拟新增，仅测试）:**

```rust
// stdout reader线程拥有BufReader；主线程只收channel，避免blocking read_line伪deadline。
struct SidecarFrames {
    rx: std::sync::mpsc::Receiver<std::io::Result<serde_json::Value>>,
}
impl SidecarFrames {
    fn next(&self, deadline: std::time::Instant) -> anyhow::Result<serde_json::Value> {
        let left = deadline.checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| anyhow::anyhow!("sidecar frame deadline expired"))?;
        Ok(self.rx.recv_timeout(left)??)
    }
}
```

reader只允许有界line/frame大小；silent/partial peer由主recv超时，清理时先杀fixture所属sidecar关闭FD再join reader。stderr同样drain并限制ringbuffer，不让pipe填满。write也需poll/deadline，不能解决read却保留无限write。

**真实壳夹具契约：** 现有 `_shell/prepare` 本身不会注册shell lease。fixture必须先依据 `PrepareOutcome::Connected` 核实scope/代际，再用 lifecycle `FrontendLease::register(control_path, client_run_id, ClientKind::App, LeaseMode::Business, &deadline)` 持租约并 `start_heartbeat()`；client_run_id在本次fixture/sidecar重启间稳定。stdio initialize携带 `ShellAuthorization::from_shell(client_run_id)`，按真实bridge bootstrap配置secret/sidecar_epoch，不给WS/relay shell身份。反向 `_shell/authorizeRoot` parse `RootIntentRequest`，核验frame secret、epoch、scope、client_run_id、thread/workspace；调用该lease的真实 `authorize_root(&request.intent, &deadline)`，按native的真实转换回应：`RootAuthorityKey { scope_key: authorization.scope_key, generation: authorization.daemon_instance_id, client_run_id: authorization.client_run_id, thread_key: request.intent.thread_key, root_id: authorization.root_id }` 加 `SecretCapability::new(authorization.capability)`；发送前再次核对authorization scope/client/daemon与当前lease一致，以native broker同样的RootAuthorityEntry格式回应id+epoch。bootstrap secret不是root capability，禁止拿它当capability或日志输出实际capability。实现前读取 `desktop/src-tauri/src/bridge.rs::handle_authorize_request`、runtime_lifecycle::authorize_intent与app-server ManagementClient的真实序列化格式，不自行发明DTO。

夹具保留lease直到parentwait终态、关闭heartbeat并显式释放后才清理；反向处理不堵stdoutreader。integration文件顶部“No test calls FrontendLease::register”注释需更新为“本夹具模拟真实shell职责，非sidecar自租约”；禁止保留与新增夹具矛盾的声明。新增 `sidecar_shell_fixture_rejects_wrong_epoch_and_scope` 先红后绿，wrong epoch/scope/clientrunid/thread拒绝且authorize调用0，合法对照真实mint成功但secret不在输出中。

- [ ] **Step 1：写 `sidecar_silent_and_partial_frames_obey_deadline` red。** 使用本测试binary helper持stdout打开而不发送/只发送半行，现有reader应不能按预算返回；必须用独立外层runner/fixture kill预算防red自身挂死。修复后主next在预算内返回timeout、清理能join，不能用等待到EOF假证明。
- [ ] **Step 2：实现并 green bounded IO。** 用上面channel reader替换Sidecar.stdout，read_frame和call共用一个绝对deadline；不叠加5秒子预算。新增反向request queue/dispatcher：读取_shell请求时核对method/请求id，通过fixture shell注册的真实scope/attempt/authority回应，完整复用现有native broker协议，不能编造capability。普通notifications收集，未知request显式拒绝，不能无声丢帧。
- [ ] **Step 3：可控网关。** 完整读HTTP header+Content-Length（上限1MiB，超限失败），逐父/子对话状态机；模型实际收到工具schema且有spawn_agent才发spawn工具调用，mode=read_only、fork=false、任务为echo hello。子对话只在shell工具可用时发echo hello；收到真实tool stdout hello+exit0后返回子成功。父收到spawn task id后发wait，收到success-terminal后返回结果。所有tool名称/arguments来自真实schema，缺工具直接测试失败，不降级返回done。至少记录以下脱敏witness：

```text
parent.request.tools includes spawn_agent
parent.tool.spawn_agent -> task_id
child.tool.shell -> command echo hello, stdout hello, exit 0
parent.tool.wait_agent -> same task_id, successful terminal
```

- [ ] **Step 4：新增 `managed_sidecar_model_spawns_child_echo_hello_and_waits`。** 初始化真实stdio壳bootstrap→真实acquire/prepare→parentturn，处理反向root intent→gateway驱动完整闭环；只允许测试parent输入，不直接调用spawn。按task/turn关联child实际工具事件与父wait成功；只看父文本或journal都不够。运行：`cargo test --all-features -p yi-agent --test managed_production_integration managed_sidecar_model_spawns_child_echo_hello_and_waits -- --exact`，预期1测试绿。
- [ ] **Step 5：补入口witness及变异。** 首watch tick用diagnostic/注入屏障证明执行；远程旧install在同宿主走真实dispatch；A6/A7活旧PID witness保留。临时回退宿主policy/install条件/Ready校验，对应测试必须红，恢复绿；保留失败时脱敏帧时间线。再运行整份 managed_production_integration，不运行live。
- [ ] **Step 6：按规则提交。** `test: verify model-driven app sidecar subagent delegation`。A9绿不勾A10，不声称App已恢复。

### Task 7：隔离 bundle 的真实窗口验收（A10，最终交付）

**Files:** 新建 desktop/scripts/verify-app-subagent.sh 和 acceptance.md；只复用build-sidecar；更新module/bug-list。

**Interface（拟新增显式入口）:**

文件地图另补 `desktop/scripts/verify-app-subagent-boundaries.test.sh`，用于不启动GUI/真实API的脚本边界测试；acceptance.md只在实际运行后记录结果，不预写PASS。

```text
bash scripts/verify-app-subagent.sh prepare /absolute/isolated-root
bash scripts/verify-app-subagent.sh launch /absolute/isolated-root
bash scripts/verify-app-subagent.sh cleanup /absolute/isolated-root
```

不是既有 `just test-live`：本worktree无此命令，本期不回迁主线测试分级。此入口不进npm test/just ci，prepare/launch明示使用真实模型可能收费。脚本不自动复制正式密钥：用户在隔离根`.yi-agent/models.json`明确配置默认与subagent条目；不依赖全局环境key。没有可用key打印SKIPPED、退出0并不写PASS。

- [ ] **Step 1：写脚本边界测试与最小实现。** `prepare`只接受绝对、非HOME/正式实例、非existingproductionapp路径；canonical核实全部祖先，拒绝symlink root/产物、`/`、真实HOME、正式`~/.yi-agent`及已有非空目录（仅允许匹配本脚本manifest的幂等复用）。产物/清理路径必须严格位于隔离root，manifest不接受任意外部路径，不能只靠路径字符串前缀。输出只能由本脚本manifest声明的产物。记录sidecar build id/hash、bundle id、isolated HOME与pid/birth，secret文件0600。不trace shell，不打印model keys。`cleanup`没有manifest拒绝，不按全机grep杀进程；先desktop父再本fixture子进程，核对birth，清秘密。用临时根/no-key fixture验证SKIPPED且正式路径未动；新增 `desktop/scripts/verify-app-subagent-boundaries.test.sh`（纯shell fixture，mock构建/签名/进程入口，不实际launch或API），命令 `bash scripts/verify-app-subagent-boundaries.test.sh`，覆盖绝对路径、symlink逃逸、恶意manifest、非空root、合法no-key、cleanup幂等。先写测试验证旧未防护脚本red，再实现green，测试入口不默认触发live。

```bash
set -euo pipefail
# 构建顺序必须固定；在worktree desktop目录。
npm ci
npm run build
npm run sidecar:release
npx tauri build --bundles app
# 产出的bundle复制到隔离root，再改plist；禁止在正式/Applications内覆盖。
```

- [ ] **Step 2：准备隔离bundle。** 从cargo metadata解析本worktree target目录，不猜主仓库target；sidecar必须本次release且无主仓库二进制软链。复制tauri bundle到root下，PlistBuddy配置独立CFBundleIdentifier/Name/DisplayName、LSEnvironment绝对YI_AGENT_HOME。sidecar→desktop→bundle三层codesign并`codesign -v`；bundle sidecar执行`internal-runtime identity --json`和shasum，与构建源一致。根内模型配置由用户提供，文件只在本机保留。
- [ ] **Step 3：运行真实窗口。** `launch`命令行直接执行bundle的Contents/MacOS/desktop，避免仅open -a未拉侧车；隔离HOME+YI_AGENT_HOME、watchman禁用、无正式relay参数。实际窗口打开隔离fixture项目，使用唯一nonce prompt：`请调用 spawn_agent，mode=read_only、fork=false，让子代理实际执行 echo hello；你调用 wait_agent 等到成功，报告 task id 和结果。` 若工具schema不同按真实schema对应字段输入，不能临时放宽授权。
- [ ] **Step 4：收集A10证据。** 原生GUI自动化权限不足就人工实际操作并记录，不用浏览器mock/CLI代替。保存脱敏窗口记录、parent模型tools、spawn task/turn、child真实shell stdout/exit、parentwait同task终态，scope/daemon/build关联；只有父回答hello仍FAIL。失败保留阶段记录继续修复，不标ready。key缺失仅SKIPPED；用户暂不允许live则明确A10未完成。
- [ ] **Step 5：最终验证与清理。** `cargo fmt --all && just ci`；desktop `npm test && npm run build`；复跑关键A9及A10当前产物。acceptance表A1–A11列命令/日志路径/实际结果（尚未执行项保持[ ]），剔除secret。按manifest先父后子清理隔离进程与模型秘密，不处理正式进程；按项目reclaim规则预演、仅有确证垃圾时清，不盲目cargo clean。
- [ ] **Step 6：提交验收记录但不合并。** CI/A10全部绿才可把完整App修复勾成[x]、更新README计数；否则记录剩余项并保持[ ]。建议message：`test: record isolated app subagent live acceptance`。停在worktree/分支供用户决定，禁止git merge、移除worktree或删分支。

## Spec 对照与最终检查

| 要求 | Task | 强判据 |
|---|---|---|
| A1，§4.1–4.2 | 1、2、6 | watch首tick实际完成且拒绝，ensure计数0 |
| A2，§4.1–4.2 | 1、2、6 | 共享宿主远程dispatch安装落盘，ensure/query-wait/watchman计数0 |
| A3，§4.1 | 1 | entry/env choice矩阵，expected/home保持，attach与repair使用快照 |
| A4，§4.2 | 1、2 | embedded普通连接实际ensure、自愈正面witness |
| A5，§4.3 | 2 | gate→scope→durablefreeze，无重新准入空窗，无嵌套取闸 |
| A6，§5.1 | 3、6 | 活业务且无guardian不launch、不Stop，超时不当Absent |
| A7，§5.2 | 4、6 | 私有FD nonce/identity/出生匹配、有界IO、退出撤ready、无Status fallback |
| A8，§5.3 | 3、5 | 安全迁移幂等，不可迁manual，取消preview无副作用，历史保留 |
| A9，§7.1 | 6 | 模型schema+反向授权+实际child echo+parentwait；变异红 |
| A10，§7.2 | 7 | 本次隔离bundle真实窗口及真实模型，不用CLI替代 |
| A11，§6 | 5 | 无scope提示、迟到隔离、显式retry、新turn重建、不重放 |
| §8交付 | 每Task、7 | 最新CI/desktop验证、文档计数、用户决定合并 |

- [ ] 对照spec逐项确认，不能有只有plan文字而没有实际测试/验收的完成勾选。
- [ ] 扫描拟新增接口与调用者一致性：BringUpChoice、HeldScope.retirement_gate、DaemonReady、RuntimeLeaseFeedback、SidecarFrames。
- [ ] 确认没有新增真实API进入默认CI，没有打印secret，没有操作正式App或代用户合并。

## 文档交接

此计划是作者自审后的实现指南，不是独立审查结论或已完成代码。执行可选择主会话逐任务推进，或在spawn工具实际可用时使用subagent-driven流程；当前会话不假称能派出审查子代理。无论采用何种执行方式，都保留逐单元review与red/green门禁，并遵守“不自行合并”。
