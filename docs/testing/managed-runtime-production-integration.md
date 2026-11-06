# Managed runtime 生产接入验收记录

对应 [设计](../superpowers/specs/2026-11-05-managed-runtime-production-integration-design.md)
§8 与 [实施 plan](../superpowers/plans/2026-11-05-managed-runtime-production-integration-impl.md)
Task 10。

本文件只写**实际执行过的**观测。未执行的用例一律标 `UNEXECUTED` 并写明原因，
对应功能保持未完成状态——不把"计划要测"写成"已经通过"。

- 分支：`feat/managed-runtime-production-integration`
- Worktree：`.worktrees/kanban/2026-11-05-managed-runtime-production-integration-design-2026-11-05-managed-runtime-production-integration-impl`
- 平台：macOS（arm64），真实子进程 + pty，mock gateway（不调用真实 LLM）
- 日期：2026-11-06

## 1. 新增的真实进程验收套件

`yi-agent-rs/crates/yi-agent/tests/managed_production_integration.rs`

每条用例都启动真实 `yi-agent` 二进制（`CARGO_BIN_EXE_yi-agent`），不使用注入的
backend，也不使用测试自己 `FrontendLease::register` 冒充生产客户端——后者只能证明
租约表能工作，不能证明任何入口会去登记租约。

夹具约束（这些是断言成立的前提）：

- 每个用例一个 `tempfile::TempDir`，内含项目、instance home、runtime 目录与假 `$HOME`；
- 子进程以**启动身份**登记（pid + 内核启动时间），`same_process_alive` 比对启动身份
  而不是 pid——pid 会被回收，只比号码会把无关进程读成"还活着"；
- 清理只动夹具自己打开的句柄，先父后子，绝不按名字全局 kill；
- `YI_AGENT_BRINGUP` **不设置**：用户不会设它，只有靠环境变量才出现的生命周期不是
  交付的生命周期。

## 2. 逐条结果

| ID | 用例（测试名） | 观测结果 | 状态 |
|---|---|---|---|
| A01 | `a_prepare_over_the_private_channel_brings_up_one_managed_runtime` | 壳侧私有通道（`YI_AGENT_LOCAL_MANAGEMENT` + bootstrap 帧 + `_shell/prepare`）在无环境变量下拉起受管 runtime，返回 `mode: "managed"`，guardian journal 非空，`runtime.sqlite` 已创建 | PASS |
| A01（负向） | `an_unbootstrapped_management_call_starts_nothing` | 未 bootstrap 的 `_shell/prepare` 被拒，且 500ms 后 `runtime.sqlite` 仍不存在——拒绝是副作用上的，不只是文案 | PASS |
| A01（负向） | `a_prepare_for_another_build_is_a_choice_not_a_silent_adoption` | 期望另一个 build 时返回 `VersionChoiceRequired`/`Blocked`，绝不返回 `Connected` | PASS |
| A03 | `a_prepare_over_the_private_channel_brings_up_one_managed_runtime` | 首次 prepare 在无 guardian 时可完成并给出 identity（不虚报 owned/ready：`Connected` 只表示身份匹配） | PASS（prepare 语义半边；壳侧持有租约/ready 由 A04 进程证据覆盖） |
| A04 | `quitting_one_frontend_leaves_the_others_work_running` | TUI 与 headless run 同时持有租约；TUI 退出后，其阻塞 marker 进程消失，run 的 marker 进程启动身份不变 | PASS |
| A06 | `quitting_one_frontend_leaves_the_others_work_running` | 同上：退出一个前端只回收它自己的工作。前提条件改为等 guardian journal 出现两个不同的 live frontend owner，不再靠固定 sleep | PASS |
| A07 | `the_last_frontend_leaving_stops_the_runtime_for_real` | 唯一前端退出后同时满足：进程树消失（启动身份比对）、`runtime.lock` 可获取、control socket 不再应答 | PASS |
| A07/A12 | `a_retired_instance_does_not_resurrect_a_daemon_across_watch_ticks` | 已退役实例 + 仅配置型 resident 登记，连续 3 个 watch tick 均 `once_unattended == None`、ensure 调用 0 次、无 `runtime.sqlite` | PASS |
| A16 | `the_headless_entry_point_is_a_managed_frontend` | 真实 `run` 在无环境变量下逐行走完 `prepare → register_business → heartbeat → authorize_root → attach_root → bind_driver → open_managed_channel → admit_turn`，并产出 `run:` 的 reclaim 记录（`stopped=true`） | PASS |
| A16 | `the_tui_entry_point_is_a_managed_frontend` | 真实默认 TUI（pty）同样走完八段，Ctrl+Q 后产出 `tui:` 的 reclaim 记录（`stopped=true`） | PASS |
| A16 | `only_the_supported_frontends_declare_managed`（`src/main.rs` 单测） | TUI/`run`/`app-server --listen stdio://` 声明受管；`daemon`/`resident`/`web`/`agents`/`completions`/`internal-runtime`/ws 及 relay 不声明 | PASS |
| A02 | `two_concurrent_prepares_yield_one_runtime` | 两个真实 sidecar 的 `_shell/prepare` 同时在路上（先发后读），两者都到达 runtime，且 guardian journal 里 daemon owner 条目**恰好 1 条**——不是"两边都答 Connected"这种弱断言 | PASS |
| A02 | `a_symlinked_project_path_resolves_to_the_same_scope` | 软链路径与真实路径 `scope::resolve` 得到相同的 `project_key` 与 `key`；同时断言两条路径字面不同，排除恒真 | PASS（scope 解析层） |
| A04 | `a_dead_launcher_sidecar_does_not_stop_a_working_runtime` | 侧车 `_shell/prepare` 拉起 runtime 后，用 **SIGKILL** 杀掉该侧车；仍挂着的 TUI 的阻塞 marker 进程启动身份不变、control socket 仍应答、guardian `generation` 未变（未重启而是保留） | PASS（进程级） |
| A05 | `discards a late result for a project the user already left`、`does not record a lease while the shell says it is still starting`（`desktop/src/hooks/useRuntimeLease.test.tsx`）；`closing_the_session_drains_inflight_attempts`、`an_expired_deadline_starts_nothing`（`yi-agent-app-server local_management`） | 切走后迟到的准备结果被丢弃、不落进新项目；`starting` 期间不记录租约；会话关闭时在途 attempt 被排空。**准备中 Cmd+Q** 的真实 App 场景属 A15，未执行 | PASS（前端 + 会话层）；Cmd+Q 半边 UNEXECUTED |
| A08–A11 | — | 迁移预览取消、确认、阶段失败、应答丢失、应用重启对账：由 `yi-agent-supervisors --test runtime_migration`（51 passed）覆盖既有阶段语义，但**未**在真实进程/真实 App 上重跑 | UNEXECUTED（真实进程层面） |
| A13 | `a_replacement_sidecar_adopts_the_runtime_without_a_second_bringup` | 首个 sidecar prepare 拉起 runtime；换成新 secret + 新 epoch 的第二个 sidecar 再次 prepare 后：guardian `generation` 未变（未重启）、journal 中 daemon 条目仍为 1（未二次 bring-up）、两个 sidecar pid 不同（确实发生了替换）。旧 capability 拒绝由下一行的存储层用例覆盖 | PASS（进程级重用 + 换代半边） |
| A13（旧 capability） | `a_capability_from_a_previous_epoch_is_never_accepted`、`a_generation_change_retires_the_old_capability`（`yi-agent-subagent/src/managed_binding.rs:1059`、`:968`） | 上一 epoch 铸造的 capability 不被新 store 接受；换代时旧代 capability 整体退役 | PASS（存储层） |
| A14 | `RuntimeVersionBanner` 组：`explains that a runtime without the control surface must be updated`、`asks the user to refresh when the outcome is unknown`、`refuses to claim nothing is running when the count is incomplete`、`says the exit guarantee is not in effect for a legacy runtime and offers no restart` | unknown / 无控制面 / 影响面不完整 / legacy 四类状态的文案与可执行动作相符；unknown 时要求刷新而不是断言成功 | PASS（前端文案层） |
| A15 | — | 最终独立 App 的红叉 / Dock / Cmd+Q —— 属 Task 11，需要打包产物 | UNEXECUTED |
| A18（部分） | `a_stale_epoch_or_foreign_secret_is_refused_after_bootstrap` | 完成 bootstrap 后，旧 epoch 帧与他人 secret 帧都被拒绝，且拒绝后 `runtime.sqlite` 仍不存在——拒绝是副作用层面的，不只是文案 | PASS（认证半边） |
| A17 | `two_roots_in_one_project_stay_distinct`(`:706`)、`a_project_path_is_not_a_root_id`(`:740`)、`a_scope_update_preserves_the_other_scope`(`:764`)、`revoking_one_conversation_keeps_its_sibling`(`:1103`)、`a_conversation_finds_only_its_own_capability`(`:1127`) | 同项目两个会话得到不同 root/capability；项目路径不能当 root id；一个会话的撤销不影响兄弟会话；跨会话解析不到别人的 capability | PASS（授权存储层） |
| A17（live owner / recovery） | `foreground_root_lifecycle_a_managed_runtime_advertises_the_contract`、`foreground_root_lifecycle_recovery_requests_are_not_silently_successful` | recovery 请求对未持有该 root 的客户端返回拒绝，而不是"没有这个操作"的假安全态 | PASS |
| A19 | `the_launcher_names_the_project_scope_and_instance_explicitly`、`a_real_child_reports_the_identity_the_launcher_declared`（`--test runtime_scope`）、`a_guardian_without_a_named_project_has_no_fallback` | launcher 显式命名 canonical project/scope/instance；真实子进程 cwd 是 runtime 目录仍报出被声明的身份；无名的 guardian 没有 fallback | PASS |
| A19（错身份） | `mismatching_nonempty_scope_is_refused`（`yi-agent/src/managed_runtime.rs`）、`a_refused_managed_start_never_falls_back_to_an_embedded_daemon` | 空或不匹配的 hello scope 一律拒绝；被拒的受管启动不进入后续任何阶段，也不回退 embedded | PASS |
| A20 | `two_scopes_of_one_instance_migrate_within_one_budget`、`a_migration_never_waits_on_the_lock_it_already_holds`（`--test runtime_migration`） | 同实例两 scope 串行迁移在单一时限内完成、不自锁；两 scope 各自只 stop 一次 | PASS |
| A21 | `status_only_evidence_blocks_automatic_takeover`、`a_pid_reused_by_another_process_blocks_takeover`、`an_unproven_identity_is_never_stopped`、`an_embedded_tui_blocks_automatic_takeover`、`incomplete_resurrection_sources_block_takeover`（`--test runtime_migration`） | 只有 Status、PID 被别的进程重用、内嵌 TUI、复活来源不全都阻止自动接管；身份无法证明时绝不执行 stop | PASS |

### 2.1 已执行的命令与结果

```sh
cd yi-agent-rs
cargo test -p yi-agent --test managed_production_integration -- --test-threads=1
# test result: ok. 16 passed; 0 failed; 0 ignored; finished in 19.47s
# （其中 3 条是 common 模块自测，13 条是本文件用例）

cargo nextest run -p yi-agent --test managed_production_integration --profile ci
# Summary: 15 tests run: 15 passed, 0 skipped

cargo test -p yi-agent --test frontend_runtime_lease -- --test-threads=1
# test result: ok. 9 passed; 0 failed

cargo test -p yi-agent --test managed_runtime_lifecycle -- --test-threads=1
# test result: ok. 12 passed; 0 failed; 1 ignored

cargo test -p yi-agent-app-server --lib
# test result: ok. 1484 passed; 0 failed; 7 ignored

cargo test -p yi-agent-subagent
# 全部套件 0 failed（含 bringup_policy 10 passed）
```

### 2.2 全量门禁里发现并修掉的测试脆弱点

在 `just ci` 的 nextest 全量并发运行中，`quitting_one_frontend_leaves_the_others_work_running`
失败过一次（`cargo test -- --test-threads=1` 与单独跑均通过）。诊断为**测试自身**的
时序假设不成立，不是产品缺陷：

- 原实现用 `sleep(500ms)` 等运行时把两个客户端都记入 journal；
- 负载高时（当时 load average 9.87，且另一个 worktree 正在跑自己的 CI）guardian
  尚未 journal 第二个 owner，"只回收自己的工作"就失去了前提；
- 现已改为轮询 guardian journal，直到出现**两个不同的 `state: "running"` frontend
  owner**（`await_two_owners_in_journal`），超时时把 journal 原文打进失败信息。

这类固定 sleep 是"在快机器上永远绿、在慢机器上随机红"的典型，改成等真实事实才
是这条断言该有的形状。

> 同一轮 nextest 全量里，`yi-agent-supervisors --test supervisor` 的 3 条
> （`a_childs_exit_is_recorded_once_not_once_per_tick`、
> `a_crashed_child_is_restarted_after_the_backoff`、
> `a_manifest_declared_switch_starts_the_child_without_a_registry_entry`）、
> `managed_launch` 1 条与 `yi-agent-runtime::supervise` 1 条也超时。已核实与本次改动
> **无关**：
>
> - `tests/supervisor.rs` 与 main 完全一致（`git diff --quiet main -- .../tests/supervisor.rs`
>   通过）；
> - 单独运行 `a_crashed_child_is_restarted_after_the_backoff` 0.53s 通过；
> - 该文件 `SPAWN_LOCK` 的注释本身记录了"`cargo nextest` 下每个测试独立进程、锁跨不过
>   进程边界，全组跑仍有测试超时，别再往这里加锁"。
>
> 这是既有的负载敏感问题，登记在案，不在本任务范围内修改。

## 3. 生产调用点审计

命令：

```sh
git grep -n -E 'ensure_running|ensure_daemons|spawn_resident_watch_loop|attach_managed_project_runtime|process_bringup_choice|supervise::serve' -- yi-agent-rs/crates desktop/src-tauri
```

分类口径：

- **managed-gated**：路径上有退役/冻结标记检查，或受真实客户端需求门控；
- **explicit manual/legacy**：只在用户显式启动的手工命令里运行，语义不变；
- **test**：仅测试接缝。

| 调用点 | 分类 | 依据 |
|---|---|---|
| `yi-agent-app-server/src/server/driver/tick.rs` | managed-gated | 本轮修复：每 tick 先查 `may_start_unattended`，再查 `RetirementGate::is_held`，退役/冻结实例直接返回（`a_retired_instance_never_asks_a_driver_plugin_for_work`、`a_frozen_instance_never_asks_a_driver_plugin_for_work`） |
| `yi-agent-app-server/src/server/plugin_bridge.rs:341`（`ensure_daemons`） | managed-gated | 唯一调用方 `spawn_resident_watch_loop` 走 `watch::once_unattended`，先查退役标记、再在 gate 下复查 |
| `yi-agent-app-server/src/server/transport/scoped.rs:49,144`（`spawn_resident_watch_loop`） | managed-gated | 循环体内门控同上；`spawn` 本身只是起任务，不启动 daemon |
| `yi-agent-app-server/src/server/transport/scoped.rs:96,190` / `ws.rs:168`（`ensure_daemon` 注入） | managed-gated | 唯一生产调用方 `rpc/plugin.rs` 在 `state.managed_session \|\| retirement::is_retired(...)` 时返回 `startup: "awaiting-demand"` 且不调用（本轮修复，`a_retired_instance_does_not_start_a_daemon_on_a_plain_install`） |
| `yi-agent/src/main.rs:522`（`ensure_daemons`，resident watch） | managed-gated | 同一 `once_unattended` 门控 |
| `yi-agent/src/main.rs:1074`（`supervise::serve`） | explicit manual/legacy | 手工 `yi-agent daemon serve`；用户显式启动，且不装 managed demand |
| `yi-agent/src/managed_runtime.rs:446`（`serve_managed`） | managed-gated | 受管 daemon 入口，用 live demand 监督插件，不使用 embedded fallback |
| `yi-agent-subagent/src/attach.rs:594`（`attach_managed_project_runtime`） | managed-gated | 只在 `BringUpPolicy::Managed` 分支；`Embedded` 分支保持 `ensure_owned_runtime` 原语义 |
| `yi-agent-subagent/src/attach.rs:1088,1220`（`process_bringup_choice`） | managed-gated | 策略优先级：`YI_AGENT_BRINGUP` → 入口声明 → `default_for_build`(=Embedded) |
| `yi-agent-app-server/src/server/assembly.rs:170`（`process_bringup_choice`） | managed-gated | 项目 attach 按策略分流；managed 无 identity 即拒绝，不静默换 embedded |
| `yi-agent-supervisors/src/watch.rs:9` / `daemon.rs:61`（库函数） | — | 定义处，无副作用 |
| `yi-agent-supervisors/src/supervisor.rs:307,319,392` | explicit manual/legacy | 手工 daemon 的 supervisor 内部实现 |

补充核对：

- `yi-agent/src/main.rs:5698` 的 `serve_without_manifest_refresh` 只在 `#[cfg(test)]` 代码里使用；
- 配置型 install/watch 路径（`ui/settings/write`、watchman 安装）均检查退役标记；
- 本轮改动**没有**放宽任何公开 CLI 语义：`default_for_build` 仍是 `Embedded`。

## 4. 本轮修复的真实缺口

1. **`driver_tick` 绕过退役栅栏**：原实现每 3 秒读 resident 登记并向 driver 插件要工作，
   在已退役/冻结实例上会启动真实 session。已加两处早退并补 RED→GREEN 用例
   （`2473ec0d`）。
2. **`plugin/install` 在已退役实例上仍会拉起无人监督 daemon**：原逻辑只看
   `managed_session`，普通非壳连接在 retired 实例上仍调用 `ensure_daemon`。已加
   `retirement::is_retired` 门控（`2473ec0d`）。
3. **受管入口从未选择 `for_managed_frontend`**：`process_bringup_choice` 只读
   `YI_AGENT_BRINGUP`，所以 TUI/headless/sidecar 在没有该变量时一律走 embedded。
   按 spec §3 的规则改为「入口显式声明 + 环境变量优先 + 默认仍 Embedded」
   （`4dbb7ddc`）。

### 4.1 与历史 revert 的关系

`6c88218f` 曾从 `ManagedContext` 的**存在**推断 managed，`eb109537` 正确回退了它：
那会把手工 `daemon` 一起卷进 guardian 拥有的 runtime，并让受管路径的诚实拒绝变成
"测试坏了"。本轮不是重做那个推断，而是把声明写进入口本身：

```rust
// yi-agent-subagent/src/attach.rs
let declared = managed_context().is_some_and(|c| c.managed_frontend);
let policy = match std::env::var("YI_AGENT_BRINGUP").ok().as_deref() {
    Some("managed") => BringUpPolicy::Managed,
    Some("embedded") => BringUpPolicy::Embedded,
    _ if declared => BringUpPolicy::for_managed_frontend(),
    _ => BringUpPolicy::default_for_build(),
};
```

`managed_frontend` 由二进制按命令给出（`is_managed_frontend_entry`），
`default_for_build` 不变。因此 `yi-agent daemon status` 仍然描述它自己启动的
runtime。

### 4.2 两个既有 fixture 的调整

历史 revert 提到的三个失败用例（`a_headless_run_registers_and_releases_its_lease`、
`a_headless_runs_tool_processes_are_owned_by_that_run`、`a_tui_exit_releases_its_lease`）
在本轮入口改动后再次失败。诊断结论：它们的 guardian fixture **没有 daemon**，
因此 `daemon_build_id` 为 `None`，`classify_compatibility` 判定 `Unknown`，
受管路径拒绝——这是拒绝逻辑在正确工作，不是缺陷。

因此调整的是夹具，不是产品行为：

- 该文件的 guardian 改为报告真实构建身份（不再伪造 `test-build`）；
- 这三个用例显式设置 `YI_AGENT_BRINGUP=embedded`，因为它们测的是租约释放语义，
  而它们的 runtime 结构上就是 legacy；
- `managed_runtime_lifecycle.rs` 的 `an_embedded_run_never_reports_a_managed_stage`
  同样显式声明 `embedded`——`run` 现在是受管入口，不声明就测不到 embedded 分支。

受管入口自身的租约行为由 `managed_production_integration.rs` 在真实 guardian +
真实 daemon 上覆盖，避免"降低产品校验来让测试变绿"。

## 5. 结论

**Task 10 仍未完成、功能未完成。** 原因不是测试失败，而是剩下的证据都只能由
打包后的真实 App 产生：

已有证据（全部实跑，非声明）：

- 真实进程级：A01、A02、A04、A06、A07、A12、A13、A16；
- 语义/存储层：A05（前端 + 会话层）、A14（前端文案层）、A17、A18（认证半边）、
  A19、A20、A21；
- 门禁：`tsc --noEmit`、`vitest`（2300 passed）、Tauri 全套（含
  `quit_drains_runtime` 3 passed）均通过。

仍然缺的，只有一类：

- **A15**（最终隔离 App 的红叉隐藏 / Dock 恢复 / Cmd+Q 回收 / 与正式实例隔离）
  以及依附于它的 A05「准备中 Cmd+Q」——都需要 Task 11 的打包产物；
- **A08–A11 的真实进程层面**：既有阶段语义已由 `--test runtime_migration`
  （含 `operation` 模块 18 条）覆盖，但未在真实 legacy daemon 上重跑；
- `just ci` 与 `npm run build`（见 §6.2），必须在本机负载正常时重跑。

因此 `docs/project-management/desktop.md` 中对应 feature **保持 `[ ]`**，
不提前勾选。

## 6. 门禁执行情况

### 6.1 已运行且通过

```sh
(cd desktop && npx tsc --noEmit)
# 无输出，退出码 0

(cd desktop && npx vitest run)
# Test Files 166 passed (166)
#      Tests 2300 passed (2300)

cargo test --manifest-path desktop/src-tauri/Cargo.toml
# 全部套件 0 failed；quit_drains_runtime: 3 passed
```

### 6.2 尚未运行

```sh
(cd yi-agent-rs && just ci)
cargo fmt --manifest-path desktop/src-tauri/Cargo.toml --all -- --check
(cd desktop && npm run build)
```

`just ci` 的 nextest 全量并发在本机未能取得可信结果：运行时 load average 约 11，
且有另一个 worktree（`session-delegation-impl`）同时编译整个 workspace。已核实
`yi-agent-supervisors --test supervisor` 等超时用例与本次改动无关（`git diff
--quiet main -- .../tests/supervisor.rs` 通过，单跑 0.53s 通过）。这些门禁必须在
Task 11 的干净环境下重跑，本文件**不**声称它们通过。

### 6.3 已知未覆盖部分

desktop 的 `quit_drains_runtime.rs` 仍是 MockRuntime 版本：`MockRuntime::request_exit`
是 `unimplemented!()`，无法在本层驱动真实 `app.exit(0)`。quit barrier 的语义由
`desktop/src-tauri/src/runtime_lifecycle.rs` 的 paused-clock 单元测试覆盖
（`a_quit_during_registration_releases_the_late_lease`、`a_hung_daemon_cannot_hold_the_quit_past_the_budget`、
`multi_project_quit_releases_in_parallel_under_one_deadline`、`a_project_is_never_registered_after_the_quit_has_begun`），
真实 App 的红叉 / Dock / Cmd+Q 属 A15，需 Task 11 的打包产物。
