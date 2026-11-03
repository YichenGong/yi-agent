# 防休眠加固：实施验证记录（2026-11-03）

本文件记录 [spec](../superpowers/specs/2026-11-03-idle-sleep-hardening-design.md)
与 [plan](../superpowers/plans/2026-11-03-idle-sleep-hardening-impl.md) 的实现结果：
每条验收 ID 对应的测试全名、实际跑出的结果、变异检查的红绿，以及仍待真机验收的
项目。spec/plan 保持为设计与计划基线，不回填实施结论。

**总体结论：自动化通过，真机 macOS 验收待完成。**

- 自动化：门禁全部通过（`just ci` 打印 `CI passed`，见 §4）。
- 真机人工验收：**未执行**，§5 逐项标注 `待人工`。本文件不声称任何真机结论
  （IPv6-only 实际持断言、网关 80 丢包、Wi-Fi 真实断开、真实 `pmset` 断言
  持有/释放、真实退出预算、屏幕可熄灭）。

## 1. 环境与基线

| 项 | 值 |
|---|---|
| 实施基线 | `f634664b` |
| 实施提交 | 见 §6 提交清单 |
| Worktree | `.worktrees/kanban/2026-11-03-idle-sleep-hardening-design-2026-11-03-idle-sleep-hardening-impl` |
| macOS | 26.5.1 |
| rustc / cargo | 1.98.1 / 1.98.1 |
| 依赖与工具链 | 未升级（plan 约束） |

跑门禁前必须清掉会话环境变量，否则 `yi-agent-tools` 里一条与本次改动无关的
用例会误报失败：

```sh
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
unset YI_AGENT_THREAD_ID YI_AGENT_SESSION_ID
```

证据：未 unset 时 `shell::bash::tests::bash_without_a_session_env_leaves_the_variable_unset`
失败（`just ci` 退出码 100）；`unset` 后同一用例 `ok`。

## 2. 验收矩阵：ID → 测试 → 结果

所有新增测试名统一以 `sleep_hardening_` 开头。下表 57 条全部为自动化结果，
**不含真机结论**。

### 2.1 Task 1：被动双栈链路资格（N1–N8）

crate `yi-agent-power`（`cargo test -p yi-agent-power --lib sleep_hardening`，20 passed）。

| ID | 输入 | 测试 | 结果 |
|---|---|---|---|
| N1 | IPv6-only en0，UP+RUNNING，全局/ULA 地址 | `probe::tests::sleep_hardening_ipv6_only_is_online` | 绿（`Online`） |
| N1 | 无可路由 IPv6（仅 `fe80::` link-local） | `probe::tests::sleep_hardening_unusable_addresses_do_not_qualify` | 绿（反例：不提供资格） |
| N2 | IPv4 私网 + IPv6 同接口 | `probe::tests::sleep_hardening_usable_single_addresses_are_online`、`..._same_interface_keeps_both_families`、`..._one_qualifying_interface_is_enough` | 绿（`Online`；聚合后单接口 flags 取并、两族地址都在） |
| N3 | unspecified / loopback / multicast / link-local / broadcast | `probe::tests::sleep_hardening_unusable_addresses_do_not_qualify` | 绿（逐类不提供资格） |
| N4 | 无 RUNNING、DOWN、仅虚拟/隧道前缀 | `probe::tests::sleep_hardening_interfaces_without_a_live_link_are_offline`、`..._successful_empty_snapshot_is_offline` | 绿（成功快照 → `Offline`） |
| N5 | 快照失败 vs 成功但空候选 | `probe::tests::sleep_hardening_unknown_is_not_offline`、`..._successful_empty_snapshot_is_offline` | 绿（`Unknown.eligible()==true`、`Offline.eligible()==false`） |
| N6 | 不再调用网关 80 / provider | 代码事实：`probe.rs` 无 `TcpStream` / `route` / `getaddrinfo` 调用（只剩注释解释为何不用），`supervisor.rs` 的 N5/N6 用例走注入快照 | 绿（保护性：不通知远端也可判 `Online`） |
| N7 | getifaddrs IPv6 字节与重复接口 | `probe::tests::sleep_hardening_sockaddr_in6_bytes_round_trip`、`..._sockaddr_in_bytes_round_trip`、`..._null_and_foreign_families_are_skipped`、`..._same_interface_keeps_both_families`、`..._addressless_nodes_stay_in_the_snapshot`、`..._snapshot_holder_releases_exactly_once` | 绿（`drop` 恰释放一次） |
| N8 | Unknown 连续拍 / 恢复后再未知 | `supervisor::tests::sleep_hardening_entered_unknown_logs_only_on_the_edge`、`..._unknown_edge_after_recovery_logs_again`、`..._unknown_network_still_holds`、`..._unknown_does_not_bypass_the_other_conditions`、`..._network_recovery_regains_eligibility_on_next_tick`、`..._offline_network_releases_after_the_grace` | 绿（按状态边沿记日志，不每拍刷屏） |

### 2.2 Task 2：电源线程生命周期（G1–G5）

crate `yi-agent-app-server`，前缀 `power_guard::tests::sleep_hardening_`（6 条，全绿）。

| ID | 场景 | 测试 | 结果 |
|---|---|---|---|
| G1 | enabled=true 且已 acquire 后 Drop | `sleep_hardening_drop_releases_once_without_waiting_a_tick`（预算 1s，release 恰一次）、`..._spawn_and_drop_are_bounded` | 绿 |
| G2 | 等待中 enabled=false | `sleep_hardening_disable_wakes_the_worker_and_skips_the_grace` | 绿（被唤醒并跳过宽限） |
| G3 | 首拍中 stop / tick 后 stop | `sleep_hardening_drop_during_a_blocked_tick_waits_then_stops`（Barrier 卡住一次本地调用，另一线程 Drop，调用返回后不再新拍、释放一次） | 绿 |
| G4 | 相同更新 / 伪唤醒 / 换版 | `sleep_hardening_repeated_identical_updates_do_not_tick`（100 次相同 update 不额外 tick） | 绿 |
| G5 | 缩窗 / 扩窗 / 再启用 | `sleep_hardening_grace_changes_reach_the_policy` | 绿（既有 policy 语义不变） |

预算语义：1s / 2s 是**测试预算**，不是 OS 调度或系统调用永不挂起的硬实时承诺。
stop 落进一次正在执行的本地 tick 时，join 会等该调用返回。

### 2.3 Task 3：共享失败收口（T1–T3、T7）

crate `yi-agent-app-server`，`server::turn::failure_tests::sleep_hardening_`（10 条，全绿）。
实现：`crates/yi-agent-app-server/src/server/turn/failure.rs`。

| ID | 场景 | 测试 | 结果 |
|---|---|---|---|
| T1 | RPC receiver 已关闭 | `sleep_hardening_rpc_start_with_closed_receiver_settles_once`：`Ok` 返回、成功响应仍在、`turn/completed{Failed}` 恰一次、id 7 的响应恰一帧、回到 `Idle`、`local_busy==false` | 绿 |
| T2 | opening / running / response 三阶段出站失败 | `..._opening_failure_settles_the_turn`、`..._running_failure_settles_the_turn`、`..._response_failure_settles_the_turn`（同一 `start_turn_core_inner`，仅 `fault` 不同，原错误照样传播） | 绿 |
| T3 | 另一个 turn 已占位 | `..._rollback_by_identity_spares_the_new_turn`、`..._rollback_clears_its_own_placeholder` | 绿（不动新 turn 的 id / `Running`） |
| T7 | notify / store 失败 | `..._notify_failure_still_rolls_back`、`..._store_failure_still_rolls_back` | 绿（仍完成内存回滚） |
| — | 水位同源 | `..._failed_completion_matches_the_stored_watermark`（落盘 `last_completed_at` == 广播 `completed_at`） | 绿 |
| — | 成功路径不误伤 | `..._successful_start_hands_off_without_failure`（prompt 已投递、无 failed、无回滚） | 绿 |

### 2.4 Task 4：launcher / cold lead 复用（T4–T6）

crate `yi-agent-app-server`，`server::session_driver_tests::sleep_hardening_`（3 条，全绿）。

| ID | 场景 | 测试 | 结果 |
|---|---|---|---|
| T4 | 插件 launch send 失败 | `sleep_hardening_launch_prompt_failure_settles_the_announced_turn`（原失败返回、failed 恰一次、水位与广播同源、回 `Idle`） | 绿 |
| T5 | cold 组长首投失败 | `..._cold_lead_prompt_failure_rolls_back_without_fabricating_events`（占位被回滚、原错误串保留、无伪造 `turn/started` 或 `turn/completed`） | 绿 |
| T6 | warm 组长投递失败 | `..._warm_append_failure_leaves_the_live_turn_alone`（既有活跃 turn 与 `Running` 都不动、不伪造完成） | 绿 |

实现要点：launcher 里私有 `rollback_turn` / `fail_announced_turn` 已删除，改为导入
`crate::server::turn::failure::{fail_announced_turn, rollback_turn}`；测试注入点
`#[cfg(test)] before_prompt_send` 仅在测试构建存在。

### 2.5 Task 5：显式后台意图（B1–B4）

crate `yi-agent-tools`（`cargo test -p yi-agent-tools --lib sleep_hardening`，6 条，全绿）。

| ID | 场景 | 测试 | 结果 |
|---|---|---|---|
| B1 | 省略 / false / true / 非布尔 | `process::tools::sleep_hardening_tests::sleep_hardening_schema_exposes_explicit_boolean`（`type: boolean`、`default: false`、不在 `required`）、`..._omitted_false_true_deserialize_as_expected`、`..._non_boolean_is_rejected`（`"true"` / `null` / `1` 全拒，不做强转） | 绿 |
| B2 | true 的 Starting / Running / Ready | `process::manager::sleep_hardening_tests::sleep_hardening_predicate_covers_every_status`（6 态逐一；`Ready` 算忙） | 绿 |
| B3 | true 的 Exited / Killed / FailedToStart | 同上（终态不算忙，即使 `list()` 仍保留记录） | 绿 |
| B4 | false 的常驻进程 / `on_exit` 不影响意图 | `..._default_false_process_is_not_busy`、`..._predicate_covers_every_status`；端到端 `..._gated_process_reports_the_intent_end_to_end`（临时文件门闩：`Ready` 时 `start` / `list` / `read` 都读回 `true`，写门闩后有界等到终态、谓词变 `false`） | 绿 |

门闩为何用文件而不是 stdin：进程的 stdin 是 null，读 stdin 会立刻 EOF 退出。

### 2.6 Task 6：busy 聚合与桌面兼容（B5–B8）

| ID | 场景 | 测试 | 结果 |
|---|---|---|---|
| B5 | 会话 `Idle` / 无 turn，但 opted-in 后台仍活跃 | `yi-agent-app-server` `power_busy::tests::sleep_hardening_idle_session_with_background_work_is_busy` | 绿（`local_busy==true`，且终态后回 `false`） |
| B6 | 最后一个 true 进程结束 | 同上 + `yi-agent-power` `supervisor::tests::sleep_hardening_background_busy_follows_the_grace_boundary`（FakeClock：两拍 `Hold`、899s 仍持、恰 900s 释放、另一路 busy 仍 true 则不释放） | 绿 |
| B7 | 旧 sidecar 不返回字段 | `yi-agent-app-server` `server::tests::process::sleep_hardening_process_dtos_expose_the_retained_intent`（`process/list` 与 `process/read` 都带 true/false，缺省读回 `false`）；desktop `src/lib/processes.test.ts` → `prevent_idle_sleep backward compatibility` 两条（缺字段 ⇒ `?? false` 得 `false`，true/false 原样透传） | 绿 |
| B8 | 非 app-server worker / 删会话 / 退出 | `power_busy::tests::sleep_hardening_idle_session_with_opted_out_work_is_not_busy`（opted-out 不算忙）、`..._with_background_work_is_busy` 的删会话阶段（删掉后不再聚合）；非 app-server 入口只保留意图、不持断言（代码事实：断言对象只存在于 `PowerGuard`，仅 app-server 主循环创建） | 绿 |

`local_busy` 不持状态锁遍历进程表；`has_sleep_preventing_processes()` 是常数级内存谓词，
不 spawn、不读盘，主循环 10s 采样不变。

## 3. 变异检查（Step 2）

每组都在**保持公共 API** 的前提下改一处实现，跑对应测试，记录红点，然后还原并确认重新全绿。
所有变异已还原（`git status` 中无残留改动，§3.9 有核对输出）。

| 组 | 变异 | 变红测试（失败断言） | 还原后 |
|---|---|---|---|
| N | `usable_address` 的 `IpAddr::V6` 分支改 `false`（回到只认 IPv4） | `probe::tests::sleep_hardening_ipv6_only_is_online`、`..._usable_single_addresses_are_online`、`..._one_qualifying_interface_is_enough` | 20 passed |
| G4 | `PowerGuard::update` 去掉 `cv.notify_one()` | `power_guard::tests::sleep_hardening_repeated_identical_updates_do_not_tick` | 6 passed |
| G2/G4/G5 | `wait_timeout_while` 谓词恒 `true`（丢掉输入换版/stop） | `..._disable_wakes_the_worker_and_skips_the_grace`、`..._grace_changes_reach_the_policy`、`..._repeated_identical_updates_do_not_tick` | 6 passed |
| T3 | `rollback_turn` 用 `active_turn_id.is_some()` 代替身份比较 | `sleep_hardening_rollback_by_identity_spares_the_new_turn`（`failure_tests.rs:157`） | 10 passed |
| T1 | RPC 投递失败只清占位、不调 `fail_announced_turn` | `sleep_hardening_rpc_start_with_closed_receiver_settles_once`，`the session must return to Idle` | 10 passed |
| T-水位 | 落盘改用 `completed_at.saturating_sub(1)` | `sleep_hardening_failed_completion_matches_the_stored_watermark`，`the watermark and the broadcast must be the same instant` | 10 passed |
| T5 | 删 cold lead 的 `rollback_turn` | `sleep_hardening_cold_lead_prompt_failure_rolls_back_without_fabricating_events` | 3 passed |
| B2/B3 | `has_sleep_preventing_processes` 去掉 `ProcessStatus::Ready` | `process::manager::sleep_hardening_tests::sleep_hardening_predicate_covers_every_status`（`status Ready with prevent_idle_sleep=true must yield true`）、`..._gated_process_reports_the_intent_end_to_end` | 6 passed |
| B1 | schema 的 `type` 改 `"string"` | `process::tools::sleep_hardening_tests::sleep_hardening_schema_exposes_explicit_boolean` | 6 passed |
| B6 | `policy.rs` 宽限比较 `<` 改 `<=` | `supervisor::tests::sleep_hardening_background_busy_follows_the_grace_boundary`、`..._offline_network_releases_after_the_grace` | 20 passed |
| B7 | `requestsIdleSleepPrevention` 改成 `prevent_idle_sleep !== false`（`undefined` 被当成 true） | `processes.test.ts` 两条 `expected true to be false` | 12 passed |
| B7 | `partitionProcesses` 丢掉 `prevent_idle_sleep` | `processes.test.ts` → `carries true and false through the partition unchanged` | 12 passed |
| B5 | `local_busy` 去掉 `has_sleep_preventing_processes()` | `power_busy::tests::sleep_hardening_idle_session_with_background_work_is_busy` | 2 passed |

一次被**纠正**的变异（记录在此以免误当证据）：最初把水位变异写成
`now_millis() + 1`，因为同一个变量同时供落盘和广播，改后仍自洽，测试**没有**红。
换成「落盘另取一个较早时刻」才红。这说明「一致性」是为了防漂移，而测试确实在
检验同源。

**接口红 vs 行为红**：`prevent_idle_sleep` 字段新增时，未同步的构造点（例如 TUI
测试夹具里的 `ManagedProcessSnapshot` / `ProcessReadResult` 字面量）是编译期
`E0063` 红，属「接口红」，**不**计入产品行为证据。本文件只把上表的运行期断言
算作行为证据。

**保护性绿（非基线红）**：N5/N8 的 Unknown 资格与边沿日志、N6 的「不调用远端」
属于增强；这些用例在基线版本不会红（因为基线根本没有对应概念或直接 panic），
如实记为保护性绿，不强行声称每条都曾基线红。

## 4. 门禁（Step 3）

全部在 worktree 根目录执行，`unset YI_AGENT_THREAD_ID YI_AGENT_SESSION_ID` 后运行。

| 命令 | 结果 |
|---|---|
| `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-power` | `67 passed; 0 failed`（另有 1 个 0-test 目标） |
| `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools --lib process::` | `32 passed; 0 failed` |
| `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server --lib` | `1230 passed; 0 failed; 7 ignored` |
| `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime --lib` | `261 passed; 0 failed` |
| `(cd desktop && npx tsc --noEmit && npx vitest run)` | tsc rc=0；`152 files / 2051 tests passed` |
| `(cd yi-agent-rs && cargo fmt --all && just ci)` | `CI passed`（退出 0） |
| `git diff --check` | 无输出（退出 0） |

`--list` 非空证据：

```
power:      20  sleep_hardening_（--list）
app-server: 22  sleep_hardening_（--list）
tools:       6  sleep_hardening_（--list）
```

`just ci` 的依赖清单以 `yi-agent-rs/justfile` 的 `ci:` 行为准（本仓库当前 19 项依赖），
本文件不复述旧清单。

## 5. 真机 macOS 验收（Step 4）：全部待人工

**本节没有任何通过结论。** 下表逐项写清怎么验、以及为什么现在没验。

| # | 项目 | 判定标准 | 状态 |
|---|---|---|---|
| 1 | IPv6-only global/ULA，UP+RUNNING，电池有任务 | `pmset -g assertions` 中出现**本实例 PID** 的 `PreventUserIdleSystemSleep` | 待人工（需改网络配置的真机窗口；未做） |
| 2 | 网关丢 80 / provider 不可达但本地链路在 | 仍持断言（本地链路契约） | 待人工（需专用测试网络；不得为验收改全机防火墙） |
| 3 | true 有限后台任务在 turn 结束后仍持，终态且无其他 busy 后过宽限释放；false 常驻不刷新 busy；插电仍持 | 观察到 Hold → 宽限到期 → 释放；false 进程不改变持有状态 | 待人工 |
| 4 | Wi-Fi 真正断开、采样无合格链路 | 由 `Online` 转 `Offline`，宽限后释放；`Unknown` 不误称 `Offline` | 待人工 |
| 5 | enabled=false 与退出释放 | 仅观测 guard 的 update/stop 时预算 2s；设置保存另有 10s 采样延迟（不合成 50ms 承诺） | 待人工 |
| 6 | 屏幕可熄灭；合盖/手动睡眠不在保证范围 | 屏幕空闲可熄灭；不强制系统睡眠 | 待人工（非目标，不做危险验证） |

真机验收的前置（尚未执行）：用 `TempDir` 隔离 `YI_AGENT_HOME`；真机 worker 的
`spawn_with_factory` 注入含 PID 与唯一 suffix 的测试 label；真实隔离 app-server
仍用固定生产 label 并只按自己的 PID 过滤；改完不复发、不污染正式实例。

已有 `#[ignore]` 真机用例（本轮未跑，不声称结论）：
`yi-agent-app-server` 的 `power_guard::tests::disabling_the_guard_releases_the_real_assertion`
（`crates/yi-agent-app-server/src/power_guard.rs:598`），
需 `--ignored` 且配合人工观测。

## 5.5 评审发现与修正

对 `f634664b..8f8f291e` 做了一次自审（子代理调度因父任务状态被拒，改为逐文件精读
diff + 定点实测）。两条修正，均已单独提交：

1. **`local_busy` 实际持着状态锁做进程表遍历**（`power_busy.rs`）。
   注释承诺「状态锁即取即放，绝不跨到进程表遍历」，但
   `... || !matches!(*session.status.lock()..., Idle) || session.process_manager.…`
   里 guard 是 `||` 操作数的临时值，会活到**整条语句**结束。当前没有反向加锁路径，
   所以不是活的死锁，但注释依赖的不变量不成立。改为在独立作用域里先算成 `bool`。
   另：曾尝试用「另一线程 try_lock 必须 2s 内成功」来锁住这条性质，实测**旧实现
   也能通过**（一次扫描太快，观察线程能从两次调用的间隙抢到锁），该测试无鉴别力，
   已删除，不写入证据。
2. **desktop 兼容性测试是空断言**（`processes.test.ts`）。
   旧测试只断言 `legacy.prevent_idle_sleep ?? false === false`，完全没碰产品代码；
   把规则写成 `!== false`（`undefined` 变 true）仍然全绿。新增产品函数
   `requestsIdleSleepPrevention`，测试改为调用它；该变异现在两条红。

## 6. 提交清单

| 提交 | 内容 |
|---|---|
| `a5321ae7` | `fix: use passive dual-stack sleep eligibility`（Task 1） |
| `7d18d686` | `fix: wake the power worker on input changes`（Task 2） |
| `0605f3c3` | `fix: settle failed rpc turn delivery by identity`（Task 3） |
| `9feda14d` | `fix: share launcher turn failure cleanup`（Task 4） |
| `92c737d0` | `feat: retain explicit background sleep intent`（Task 5） |
| `8f8f291e` | `feat: include opted-in managed work in power busy`（Task 6） |
| `2bf38953` | `fix: release the status lock before scanning processes`（评审修正） |
| 本提交 | `docs: record idle-sleep hardening verification`（Task 7） |

## 7. 未做与不做

- 未升级依赖或工具链。
- 未打包/安装覆盖正式 app。
- 未改 `pmset` / 真实 HOME 配置 / 全机防火墙。
- 未为孤儿 daemon 保活，未覆盖 `yi-agent run` 与 TUI 入口，未覆盖 Linux/Windows
  真实断言。
- 未把任何真机项目写成通过。
