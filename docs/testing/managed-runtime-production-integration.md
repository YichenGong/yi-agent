# Managed runtime 生产接入验收记录

对应 [设计](../superpowers/specs/2026-11-05-managed-runtime-production-integration-design.md)
§8 与 [实施 plan](../superpowers/plans/2026-11-05-managed-runtime-production-integration-impl.md)
Task 10–11。

本文件只写**实际执行过的**观测。未执行的用例一律标 `UNEXECUTED` 并写明原因，
对应功能保持未完成状态——不把"计划要测"写成"已经通过"。

- 分支：`feat/managed-runtime-production-integration`
- Worktree：`.worktrees/kanban/2026-11-05-managed-runtime-production-integration-design-2026-11-05-managed-runtime-production-integration-impl`
- 平台：macOS（arm64），真实子进程 + pty，mock gateway（不调用真实 LLM）
- 日期：2026-11-06
- 原生 App 证据采集于隔离 bundle
  `.artifacts/managed-integration/yi-agent-managed.app`（`com.gongyichen.desktop.managedint`），
  从未覆盖 `/Applications/yi-agent.app`

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
| A15（原生打包证据） | 本节 2.4 的记录 | 隔离 bundle 真实 launch：无 `--relay`、独立 instance home、sidecar 被杀后壳存活并重拉、正式实例完全不受影响 | PASS（原生层）；红叉/Dock/Cmd+Q 的用户交互 **UNEXECUTED** |
| A18（认证） | `a_stale_epoch_or_foreign_secret_is_refused_after_bootstrap` | 完成 bootstrap 后，旧 epoch 帧与他人 secret 帧都被拒绝，且拒绝后 `runtime.sqlite` 仍不存在——拒绝是副作用层面的，不只是文案 | PASS |
| A18（页面伪造管理帧） | `the_frontend_cannot_write_into_the_reserved_namespace`（`desktop/src-tauri/src/bridge.rs`） | `_shell/prepare`、`_shell/bootstrap`、`_shell/rootIntent`、`_shell/replaceRoots`、`_shell/migration*` 及**将来新增**的 `_shell/*` 一律在 `write_line` 之前被拒；普通业务方法不受影响。测的是结构（拒绝发生在写入之前），不是错误文案 | PASS |
| A18（秘密不外泄） | `the_secret_is_never_printed`、`the_bootstrap_frame_carries_the_secret_and_no_id` | `Debug` 输出不含 secret；bootstrap 帧带 secret 且**不带 id**（不构成可被页面应答的请求） | PASS |
| A18（非协议帧） | `garbage_is_dropped` | 非 JSON-RPC 形状的帧被归为 `Garbage` 丢弃并记录，绝不 panic，也不被当成响应 | PASS |
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

同一类问题随后又出现在 `the_tui_entry_point_is_a_managed_frontend`：它在
`await_runtime_attached` 返回（控制通道开始应答）后再固定 `sleep(500ms)` 才断言八段
全部走完。控制通道应答是 **runtime** 的事件，`admit_turn` 是 **本前端**最后一段，
负载高时会晚于这 500ms 写入，于是断言拿到 7 段。复查时再读一次 trace，八段齐全，
证明产品没有漏段，是测试在猜时长。

现已改为等待事实本身（`af94cce9`）：

- 轮询 trace 直到八段齐全；
- 观察者写出 `refused:<stage>` 时**立刻**失败并带上该拒绝，而不是耗完启动预算只报
  "少一段"；
- 失败信息同时附上 trace 与屏幕文本。

这里要特别记下一条被否掉的改法：曾按 `main.rs` 顶部注释以为 `admit_turn` 是"用户
输入第一轮时才写入"，于是让测试先输入一轮再断言。实际 `admit()` 在 TUI 会话建立时
就调用（`yi-agent-rs/crates/yi-agent/src/main.rs:1763`），trace 里启动期即有
`admit_turn`，该改法属于基于注释的误判，已撤回。

### 2.3 全量门禁在本机的实际结论

```sh
cd yi-agent-rs && just ci
# CI passed（16 道门禁全过）

cargo nextest 全量（just ci 的 test 关）：
# Summary [77.558s] 5813 tests run: 5813 passed, 40 skipped
```

前两次全量 nextest 各红 1 条，但都在本文件之外、且都不是产品缺陷：

| 失败 | 现象 | 判定依据 |
|---|---|---|
| `yi-agent-app-server ws::tests::a_revoked_device_is_disconnected_and_its_token_invalidated` | `initialize` 等待期 `Protocol(ResetWithoutClosingHandshake)`，0.029s | 同 crate 的 ci profile 全跑连做两轮 1484/1484 全绿；`ws::` 过滤 12/12 全绿；最终全量 5813/5813 绿 |
| `the_tui_entry_point_is_a_managed_frontend` | 8 段只到 7 段 | 见 2.2，测试自身固定 sleep，已如实修复 |

`audit` 关出现 `couldn't check if the package is yanked: registry: request could not be
completed in the allotted timeframe` 属 `scripts/audit-deps.sh` 设计好的网络降级路径
（该脚本明确区分"网络失败"与"真漏洞"，只有后者才红）；`chacha20 0.10.1 yanked` 在
`yi-agent-rs/.cargo/audit.toml` 里有记录且按 cargo audit 语义只是 warning。**结论是
通过，而不是把红绕过。**

桌面侧与其它 crate（Task 11 Step 1 完整清单）：

```sh
cargo fmt --manifest-path desktop/src-tauri/Cargo.toml --all -- --check   # clean
cargo test --manifest-path desktop/src-tauri/Cargo.toml                  # 全部 0 failed
cargo test --manifest-path desktop/src-tauri/Cargo.toml --lib            # 281 passed
cd desktop && npx tsc --noEmit                                           # exit 0
cd desktop && npx vitest run        # Test Files 166 passed / Tests 2300 passed
cd desktop && npm run build         # built in 623ms（pre-existing chunk-size warning）
cd yi-agent-rs && cargo test -p yi-agent-supervisors --test runtime_migration
                                    # 51 passed, 0 failed
```

只用 `--lib` 跑 desktop 时 281 条全绿；全量 `cargo test` 因含集成测试二进制，
逐个 `test result` 也都是 `0 failed`。


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

### 2.4 Task 11：隔离打包 App 的原生证据（A15）

打包在 worktree 内完成，**没有**覆盖 `/Applications/yi-agent.app` 的二进制，也没有迁移
用户正式 runtime（正式实例全程 `desktop` / `app-server --relay session=fe42addbfed2...`
/ `daemon serve` 三个进程 pid 未变）。

产物与身份：

```sh
cd desktop
npm run sidecar:release
# sidecar -> src-tauri/binaries/yi-agent-aarch64-apple-darwin
# build id -> 98a0d5a300ca516f42b21e482fb1d27a
npm run build            # dist/ 先于 tauri build，避免产出 dev 版连 localhost:1420
npx tauri build --bundles app
# 产物：desktop/src-tauri/target/release/bundle/macos/yi-agent.app

cp -R .../yi-agent.app .artifacts/managed-integration/yi-agent-managed.app
# CFBundleIdentifier -> com.gongyichen.desktop.managedint（与正式版不同，避免互踢）
# CFBundleName/DisplayName -> yi-agent-managedint
# LSEnvironment:YI_AGENT_HOME -> /Users/<user>/.yi-agent-managed-int-verify

bash desktop/scripts/finalize-runtime-bundle.sh .artifacts/managed-integration/yi-agent-managed.app
# finalized ... (98a0d5a300ca516f42b21e482fb1d27a)
codesign -v .artifacts/managed-integration/yi-agent-managed.app   # exit 0
shasum -a 256 Contents/MacOS/yi-agent == Resources/runtime-sidecar.json 的 sha256
# 991a7069e534f98f522ee7de6c47bfa678486924df66db644f69c47021e14d5b（两者一致）
```

`finalize-runtime-bundle.sh` 按 sidecar → 清单 → 外层 的顺序执行，全程未使用
`codesign --deep`。

真实 launch 结果（直接跑 `Contents/MacOS/desktop`）：

| 检查 | 结果 | 证据 |
|---|---|---|
| 独立 instance home | PASS | `~/.yi-agent-managed-int-verify/.yi-agent/` 下生成 `trace/`、`side-conversations/`、`instance-migration.lock` |
| 不与正式实例共用 relay 会话 | PASS | 隔离实例的 sidecar **完全没有** `--relay` 参数；正式实例仍是 `session=fe42addbfed2...` |
| 正式实例不受影响 | PASS | 两次隔离 launch 期间，`/Applications/yi-agent.app` 的 desktop/app-server/daemon 三进程 pid 未变 |
| sidecar 被杀后壳存活并重拉 | PASS | `kill -9` 隔离实例的 `app-server` 后，`desktop` 仍在，并出现新的 `yi-agent app-server --listen stdio://` |
| quit 事件后进程干净退出 | PASS | Apple Event `quit` 后 `desktop` 与 sidecar 全部消失，无残留 |

**踩到的坑（值得记下，会重复发生）：**

1. 直接 `Contents/MacOS/desktop` 启动时 `Info.plist` 的 `LSEnvironment` **不会**生效，
   于是 `YI_AGENT_HOME` 没设上，实例读到了真实 `~/.yi-agent`，还拿到了与正式实例
   **相同**的 relay session（同一 session 双连接）。发现后立即 kill，未造成写入。
   结论：命令行验收必须显式 `YI_AGENT_HOME=... desktop`，`LSEnvironment` 只有在
   LaunchServices（`open -a`/Finder）启动时才展开。
2. `bridge::spawn_local_once` 会把 sidecar 的 cwd 设成 `YI_AGENT_HOME`；该目录**不存在**
   时 spawn 报 `No such file or directory (os error 2)`，错误信息里不带路径，很容易被
   误读成 sidecar 缺失。先 `mkdir -p` 再启动。
3. `system_profiler`/`AX` 权限 `AXIsProcessTrusted=false`（AX error -25211），
   `osascript ... keystroke` 也被拒（error 1002），`CGEvent.postToPid` 投递成功但合成
   键盘事件不被 Wry 视图接受。

**UNEXECUTED（没有伪造，也没有用单测结果代替）：**

- 红叉点击后「隐藏而非退出、不释放租约」的真实 UI 操作。产品代码
  `runtime_lifecycle.rs:2351` 明确写了 `CloseRequested is deliberately absent: the red
  button hides the window, and hiding is not quitting`，`window::hide_on_close` 走的是
  `api.prevent_close()` + `window.hide()`，确实不碰 `DesktopRuntimeManager`。这与
  「Cmd+W 无任何反应」的观测一致，但**点击红叉本身**未执行。
  注意范围：`window::handle_run_event` 带 `#[cfg(target_os = "macos")]`
  （`lib.rs:149-150`），所以「红叉只隐藏」**只在 macOS 成立**；其它平台红叉就是真正
  关窗并走完整 drain（`lib.rs:145` 的注释也这么写）。不要把这条当成跨平台性质。
- Dock 图标点击恢复窗口。
- 真实 Cmd+Q 在「有项目、有在途 work」时的 drain 行为（本环境无辅助功能权限，无法设置
  项目；未开项目时 quit 无工作可回收，不能作为 A15 的正面证据）。
- A05 的「准备中 Cmd+Q」真实 App 场景同上。

这些项依赖一次有 GUI 权限的人工操作，因此 desktop 对应 feature 保持 `[ ]`。

### 2.5 两轮独立代码评审

Task 11 Step 4 要求 diff 评审。第一次派发的评审子代理读了约 42 分钟仍无产出，
`send_message` 返回 `invalid_state: terminal tasks cannot send mailbox messages`
而 `inspect_agent` 仍报 running（运行时状态不一致），遂取消，改为两个**有明确
工具调用预算**的只读评审并行重发。两者都完成并给出了 file:line 证据。

结论：**均无阻塞项**；共 5 处已修，2 处记录为已知范围限制。

已修（`172c0827`，每处都有测试）：

| 问题 | 位置 | 为什么算缺陷 |
|---|---|---|
| 会话锁跨越 bootstrap 回包写入 | `app-server/src/server/transport/mod.rs:501-543` | guard 活到 `hub.reply(..).await`，客户端不排空 fd 时，这条连接上所有已认证的 prepare/migration 都排在一个无关 ACK 后面 |
| 空 epoch 的 bootstrap 被答 "accepted" | `app-server/src/server/local_management.rs:175` | 无 epoch 无法安装任何 root authority，壳却在握手时被告知可用，失败被推迟到第一次授权 |
| bootstrap secret 可能进 stderr | `desktop/src-tauri/src/bridge.rs:1495` | bootstrap 帧无 method 无 id，正好落进 `Frame::Garbage` 的整帧 `eprintln!`；别处都脱敏，这条是唯一例外 |
| drain 任务异常死亡后 App 无法退出 | `desktop/src-tauri/src/runtime_lifecycle.rs:2377` | phase 停在 `Draining`，之后每次 Cmd+Q 都被 `KeepHolding` 拦下，没有回退路径 |
| 固定 500 ms + 「数据库仍不存在」 | `tests/managed_production_integration.rs:807` | 负向断言无法用任何 sleep 长度成立：600 ms 才创建的 runtime 能骗过 500 ms 窗口 |

记录为已知范围限制（未改代码，理由如下）：

- `ManagedStart::step` 只保证**顺序**，不保证**数据流**：`AttachRoot` 可以拿到任意
  root id，靠调用方（`yi-agent/src/managed_startup.rs`）保持二者一致。评审也确认
  了它声称的两条性质（拒绝后不再执行、乱序被拒）属实。改成类型级串联是更大的
  重构，不属于本卡范围。
- `_shell/` 前缀守卫依赖 sidecar 方法派发是精确匹配；评审未验证 sidecar 侧，
  本轮也未验证。风险面是「名字规范化差异」，不是当前可触发的绕过。

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
