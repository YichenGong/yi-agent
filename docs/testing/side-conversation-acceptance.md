# 旁问（side conversation）验收记录

- 版本：本 worktree 分支 `kanban/2026-11-03-side-conversation-design-2026-11-03-side-conversation-impl`，HEAD 见 `git log -1`
- 环境：Darwin arm64 / rustc 1.98.1 / cargo 1.98.1 / node + vite（`desktop/`）
- 对照：spec `docs/superpowers/specs/2026-11-03-side-conversation-design.md` §11、§12；计划 `docs/superpowers/plans/2026-11-03-side-conversation-impl.md` Task 21
- 规则：每条写明命令或操作、实际结果。**未执行的项一律写「未执行」，不拿单元测试冒充真机/真实终端验收。**

## 1. 自动门禁（本机已执行）

命令都在 `yi-agent-rs/` 下，前置：

```sh
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
unset YI_AGENT_THREAD_ID YI_AGENT_SESSION   # tools 的 bash 用例对宿主变量敏感
```

| 门禁 | 命令 | 实际结果 |
| --- | --- | --- |
| core 旁问 | `cargo test -p yi-agent-core --lib side_` | 34 passed, 0 failed |
| tools 旁问 | `cargo test -p yi-agent-tools --lib side_` | 40 passed, 0 failed |
| runtime 旁问 | `cargo test -p yi-agent-runtime --lib side_` | 89 passed, 0 failed |
| app-server 旁问 | `cargo test -p yi-agent-app-server --lib side_` | 100 passed, 0 failed |
| TUI 旁问 | `cargo test -p yi-agent --bin yi-agent tui::side` | 27 passed, 0 failed |
| TUI 全量 | `cargo test -p yi-agent --bin yi-agent tui::` | 709 passed, 0 failed |
| app-server 全量 | `cargo nextest run --offline -p yi-agent-app-server --all-features --profile ci` | 1311 passed, 7 skipped |
| yi-agent 全量 | `cargo nextest run --offline -p yi-agent --all-features --profile ci` | 943 passed, 14 skipped |
| core 全量 | `cargo nextest run --offline -p yi-agent-core --all-features --profile ci` | 417 passed |
| tools 全量 | `cargo nextest run --offline -p yi-agent-tools --all-features --profile ci` | 357 passed |
| runtime 全量 | `cargo nextest run --offline -p yi-agent-runtime --all-features --profile ci` | 348 passed |
| clippy | `cargo clippy --offline --workspace --all-targets --all-features -- -D warnings` | 干净 |
| 前端 | `cd desktop && npx vitest run` | 158 文件 / 2201 passed |
| 前端类型 | `cd desktop && npx tsc --noEmit` | 通过 |
| 恢复端到端 | `cargo test -p yi-agent --test e2e_resume` | 4 passed, 0 failed |
| 前端构建 | `cd desktop && npm run build` | 通过 |
| 桌面图 | `just desktop-check` | 通过 |

**完整 `just ci` 已跑通：`EXIT=0`，末行 `CI passed`**，其中
`cargo nextest run --all-features --workspace --profile ci` 为
`4378 tests run: 4378 passed, 28 skipped`（新增 5 条 TUI 回归测试后从 4373 升到
4378），`just ci` 的其余 15 道门禁（fmt-check / lint / build / bindings-check /
secret-check / 版本与 release 检查 / msrv-check / audit 等）全部通过；`audit` 用
本地缓存的 advisory-db（日志已明确告警「结论基于过期数据」，nightly job 会重跑）。
最终日志：`/tmp/ci_side_final2.log`（前一轮 `/tmp/ci_side_final.log` 是 4377 条、同样全绿，那条少了本轮补的规格点名测试）。

这条结论只在**没有别的 worktree 同时跑 cargo** 时才成立：早前一次 `just ci` 的
`test` 步骤里只有 4 条 `subagent_real_e2e` 失败（`isolated runtime daemon did not
become ready`，带 `FAIL + LEAK`），当时另一个 worktree 正在跑自己的 `just ci`；
单独 `cargo test -p yi-agent --test subagent_real_e2e` 是 `6 passed / 2 ignored`，
最终这轮等对方跑完再启动，同一套用例全绿。这是资源竞争，不是用例或本次改动的
回归。

### 1.1 合入 main 时的两次返工（都已修复）

分支收尾时 `main` 已前进 92 个提交（daemon 生命周期那条线），所以合并本身成了
一次真实的集成，暴露两件事：

1. **`fmt-check` 在 `main` 上立刻变红。** 合并里三处「双方各自新增模块声明」的
   冲突按「两边都保留」解决后，声明顺序不是 rustfmt 想要的；我在 worktree 里跑了
   `cargo fmt --all` 却**没有提交那三个文件**就快进合并，于是 CI 是在「工作区已
   格式化的树」上通过的，而提交进去的版本没有。代价很直接：`main` 落地即失败。
   修复 `2fb3b5dc`（纯排序，无语义变化）。教训：`cargo fmt` 之后必须把结果一起
   提交，再让 CI 去评价那棵树。
2. **`--list` 阶段挂起。** 在 `main` 上重跑 `just ci` 时，nextest 卡在
   `subagent_fork_ipc` 等二进制的 `--list --format terse`，`sample` 抓到调用栈只有
   `_dyld_start`（卡在动态链接），同时另一个 worktree 正在全量编译。这几条测试文件
   本次并没有改动（`git log -1 -- yi-agent-rs/crates/yi-agent/tests/subagent_fork_ipc.rs`
   与合并无关）。它们不是失败，是等锁/被干扰下的挂起；等对方跑完再启动即正常。
   所以**在共享 target 的并发环境里，「CI 卡住」要先看有没有别的 worktree 在编译**。

合入 `main` 后的最终验证：`2fb3b5dc` 上跑完整 `just ci` → `EXIT=0`、`CI passed`，
`4979 tests run: 4979 passed, 32 skipped`（比本分支的 4378 多，因为含 `main` 的 92
个提交）。日志 `/tmp/ci_main_final3.log`。同一套用例在并发环境下曾 9 条失败
（`supervisors` 的 20 秒超时类与 `store` 的分片帧用例），等对方 CI 结束后
`cargo test -p yi-agent-supervisors -p yi-agent-subagent -p yi-agent-store` 是
全绿（`SERIAL_EXIT=0`），确认是资源竞争。

## 2. 自动矩阵覆盖（spec §11）

| 场景 | 落点测试 | 状态 |
| --- | --- | --- |
| 同宿主并发 open 只产一个旁支 | `side_concurrent_open_returns_one_runner`、`side_open_from_two_connections_shares_one_host_instance_id` | 通过 |
| open 幂等（create-or-get，不涨 generation） | `side_open_is_create_or_get` | 通过 |
| 父正在跑一轮时 open 不改写父文件 | `side_open_during_a_parent_turn_does_not_touch_parent_files`（jsonl/meta/partial 逐字节比对） | 通过 |
| 冷父快照不改父文件 | `side_cold_snapshot_leaves_every_parent_file_untouched` | 通过 |
| 同 request id 去重、异文本拒绝 | `side_request_id_deduplicates_turn_start`、`side_rejects_a_reused_request_id_with_different_text` | 通过 |
| 第二个在途 turn 被拒 | `side_rejects_second_inflight_turn` | 通过 |
| 过期 interrupt 不杀新轮 | `side_stale_interrupt_does_not_kill_a_newer_turn` | 通过 |
| close 幂等 + 墓碑 + 过期 token | `side_close_is_idempotent_through_the_tombstone`、`side_close_rejects_stale_turn_confirmation`、`side_close_revokes_outstanding_snapshots` | 通过 |
| 关闭后发布是 no-op、终态帧 | `side_publish_after_close_is_a_no_op`、`side_close_publishes_a_terminal_frame_for_its_generation` | 通过 |
| 旁支事件不污染父订阅 | `side_events_follow_parent_subscription_without_polluting_parent` | 通过 |
| 旁支取消不影响父 / 父停止不影响旁支 | `side_cancel_does_not_cancel_parent`（双向：父用自己的 `PendingProvider` 与 core 任务，断言两边流与任务互不牵连） | 通过 |
| yolo 也不放宽只读白名单 | `side_registry_rejects_mutation_even_with_yolo`、`side_policy_refuses_every_dangerous_name` | 通过 |
| 撤权后仍重查根集 | `side_read_access_rechecks_roots_and_blocks_path_races` | 通过 |
| 桌面：旁支帧不 mark-read / 不 stop 父 / 不进 resume | `never routes side notifications into the parent's mark-read or resume`、`does not stop or interrupt the parent when a side frame arrives` | 通过 |
| 桌面：开旁支不动父草稿、不中断父 | `keeps the parent's draft untouched when opening and leaving a side`、`does not interrupt a running parent when opening a side` | 通过 |
| 桌面：返回不停旁支 / 只停旁支自己 | `returns to the parent without stopping or closing the running side`、`stops only the side's own turn` | 通过 |
| 无 capability 旧 server 隐藏所有入口 | `hides every side entry when the host lacks the capability` | 通过 |
| TUI：审批命令模式 Enter 不批准 | `parent_approval_command_mode_does_not_approve_on_enter` | 通过 |
| TUI：旁支内不能回答父审批 | `parent_approval_cannot_be_answered_from_side`（真实 `run_loop_with_side`：旁支里敲 `1`+Enter，裁决通道 0 条，且 `1` 作为旁支 `Submit` 发出；把 `in_side` 路由改成让审批先走，该测试立刻失败） | 通过 |
| TUI：旁支正文不落父 TurnLog | `side_events_never_reach_main_turn_log`（真实 `TuiSessionStore`） | 通过 |
| TUI：旁支转录窄终端按显示宽度折行 | `side_transcript_cells_wrap_cjk_at_display_width`（`tui/app.rs`，20 列下无溢出且确实折成多行） | 通过 |

### 2.1 同宿主双端 / 断线重连（计划 Task 21 自动矩阵）

| 场景 | 落点 | 状态 |
| --- | --- | --- |
| 两个连接 open 同一父 → 同一 side id / host instance id | `side_open_from_two_connections_shares_one_host_instance_id` | 通过 |
| 一个连接的 snapshot token 不能用于另一个连接 | `side_snapshot_token_does_not_cross_connections` | 通过 |
| 握手通告 capability 与 host id | `side_initialize_advertises_capability_and_a_host_id` | 通过 |
| 断线重连后按 list + 快照对账，不重复 open | `does not call open or renew the token while reconciling`、`keeps an existing view and marks it unsynced when the reconnect list fails` | 通过 |
| 重连后序号有洞 → 重新对账而非跳过 | `re-reconciles instead of skipping a gap`、`re-reconciles when the cache itself has a hole` | 通过 |
| 更早 generation 的迟到帧被丢弃 | `drops frames from an older generation`、`ignores an older generation reported after a newer one was seen` | 通过 |
| closed 帧立即生效，不等缺失序号 | `applies a closed frame without waiting for missing numbers` | 通过 |
| 未确定提交用同一 request id 重试 | `retries uncertain submission with the same request id`、`confirms an uncertain submission via accepted_requests` | 通过 |
| 旧回包不清掉更新的编辑 | `does not overwrite newer edits when restoring a rejected question` | 通过 |
| 后台不轮询、可见时轮询 list | `polls list while visible and stops in the background` | 通过 |
| 不同 side id 不互相串 | `keeps distinct side ids under one host apart` | 通过 |

以上都是「同宿主、双客户端/重连」的确定性用例，用显式帧与失败注入而非 sleep。
真实 relay + 两台设备的端到端仍在第 3 节人工验收里，不能由这些替代。

## 3. 真实终端验收（TUI，已执行）

用 Python `pty.fork` + `TIOCSWINSZ` 起一个 32 行 × 100 列的真 PTY，把 `target/debug/yi-agent`
当普通进程驱动，输出同时喂给 `pyte` 终端模拟器取屏幕快照。模型是真实远端
（`HORIZON-DeepSeek` @ `https://llmapi.horizon.auto/v1`），不是 mock。

- 隔离：`YI_AGENT_HOME=/tmp/side-accept/home`（实际配置目录 `.../home/.yi-agent`），
  工作目录 `/tmp/side-accept/work`，`preferences.json` 预置 `onboarding_dismissed=true`。
- 驱动脚本：`/tmp/side-accept/drive2.py`（全流程）、`drive4.py`（主视图再 `/side`）、
  `drive5.py`（关闭后再开，含打字过程）、`drive6.py`（旁支内 `/side-close`）、
  `drive7.py`（关闭后按字符重开）。
  原始 PTY 流：`/tmp/side-accept/pty{2,4,5,6,7}_raw.log`；快照：`shot-*.txt`。

| 步骤 | 操作 | 实际结果 |
| --- | --- | --- |
| 1 | 启动，子 Agent runtime 弹窗按 `n` | 进入主界面，显示 `HORIZON-DeepSeek` 与输入框 |
| 2 | 发长任务「写约一千字，分二十小节介绍中国古代朝代」 | 父轮开始流式输出 |
| 3 | 父轮运行中 `/side 用一个自然段说明布尔代数，段末写标记 旁支独特标记7f3a` | 切到旁支视图，顶部 `旁问（只读）：继承父会话上下文 · 父会话运行中 · /back 返回 · /side-close 关闭`；旁支答出布尔代数并以该标记结尾 |
| 4 | 旁支内追问「再用一句话说明它和集合论的关系」 | 旁支第二轮正常回答（多轮成立），父轮仍在跑 |
| 5 | `/back` | 回主视图，父轮继续输出（未被中断） |
| 6 | 主视图再 `/side` | **切回同一个旁支**，已有正文（含标记）仍在 |
| 7 | `/side` 后按 Esc | 只停旁支；屏幕仍显示旁支视图与已收敛的正文 |
| 8 | `/back` → `/side-close` | 关闭旁支，输出「已经在主会话了，无需 /back」等主视图提示，旁支不再接收帧 |
| 9 | `/quit` | 正常退出并打印 `yi-agent resume <thread-id>` |

磁盘侧证据（取 `/tmp/side-accept/work/.yi-agent/threads/`）：

```
$ grep -l "旁支独特标记7f3a" *.jsonl *.json
NONE(未出现在任何父 thread 文件)
$ grep -c "布尔代数" *.jsonl
0
```

即：整场只有父会话的 `thread-<id>.jsonl`（约 59 KB，父任务正文），旁支的两轮问答
一个字节都没进父会话文件；父 `meta.json` 的 `title` 是父任务的提问。

### 3.1 真实终端发现并修复的四个缺陷

这四条**只有真跑 PTY 才会暴露**，`TestBackend` 上的既有测试全绿也没抓到：

1. **主视图再次 `/side` 切不回已有旁支。** spec §4.1 要求「有旁支则打开」。
   `apply_event` 只在 `views.side.is_none()` 时建立视图，已有旁支时 driver 走幂等
   路径、只补一帧 `SideStatus`，而该帧只更新状态不切视图。
   修复：`execute_side_command_in_main` 的 `Open` 分支在旁支存活时先
   `side.views.open(side_id, generation)` 再发 `Open`。视图切换只由**用户显式
   open** 驱动，后台帧不得把人拽走。
   回归测试：`opening_a_side_from_main_view_switches_back_to_the_existing_view`。
2. **执行 `/side` 后命令补全弹窗残留。** 旁支拦截分支 `continue` 掉了循环末尾的
   `sync_popup`，输入缓冲已被清空但弹窗仍挂在屏幕上，看起来像命令没执行。
   修复：拦截分支在 `continue` 前补一次 `sync_popup`。
   回归测试：`side_command_from_main_view_clears_the_command_popup`。
3. **旁支视图里 `/side-close` 不返回父视图。** spec §4.1 规定旁支里的
   `/side-close` 是「取消并关闭，返回父视图」，但实现只把 `Close` 发给 driver、
   等 `SideClosed` 帧回来，返回父视图并不当场发生——用户关掉了旁支，却还被留在
   那个视图里。修复：`handle_side_key` 的 `SideInput::Close` 分支在发送 `Close`
   后立即 `side.views.back()`；终态帧随后只负责把关闭原因写进 notice。
   回归测试：`closing_a_side_from_its_own_view_returns_to_the_parent`。
4. **关闭旧旁支后再 `/side` 无法新建旁支。** 真实 PTY 复现 `/side` → `/side-close`
   → 再 `/side`，屏幕完全无反应。两个原因叠加：
   ① `apply_event` 开头「已关闭视图不再接受帧」的守卫不区分 `side_id`/`generation`，
   新旁支的帧被当作旧旁支的迟到帧丢掉；② 建立视图的条件是 `views.side.is_none()`，
   而关闭后旧视图仍在（`closed` 有值），于是新旁支永远建不出视图。
   修复：① 守卫只丢弃**属于当前已关闭视图**的帧；② 建立视图改为
   「没有视图，或视图已死**且**目标不是它自己」——「已死就重建」会把旧旁支的
   迟到帧重新造成视图，等于让关掉的旁支复活，所以身份判断不能省。
   回归测试：`a_new_side_can_open_after_the_previous_one_closed`（同时断言旧旁支
   迟到帧仍被 `Discard`、不会顶掉新视图）。

四条回归测试都先写成红：修复前分别失败于「必须切回已有旁支视图」「弹窗必须收起」
「必须返回父视图」「新旁支的帧不能被当作旧旁支的迟到帧丢掉」。

修复后在同一个真 PTY 上复验：`drive4.py` 主视图再 `/side` 切回已有旁支且弹窗收起；
`drive6.py` 旁支内 `/side-close` 当场回到父视图（快照 `C-after-enter` 已是父会话）；
`drive5.py` / `drive7.py` 关闭后再 `/side` 成功开出第二代旁支并答出 `第二代旁支` /
`SIDE-TWO`。

### 3.2 真实 LLM 端到端（`just test-real-e2e`）

`4 passed, 1 failed`。失败的是 `e2e_auto_compact_triggers`，与本功能无关：

- 该测试靠「上下文 200k × 1% ≈ 2000 tokens」的阈值触发压缩，而本机
  `models.json` 的 `HORIZON-DeepSeek` 条目 `context_length` 为 `null`。
  实测同一命令 `EstimatedPrefill` 只有 3349~4415，是「单轮读 500 行」这一
  设计前提不再成立，不是代码回归。
- 对照实验：在 **main（`6639335d`）已编译的二进制**上跑同一条
  `--compact-ratio 1 run --json` 命令，`AutoCompacting` 计数同样为 `0`；
  本分支也是 `0`。两边一致，说明是既有环境/模型条件，不是本次改动引入。
- 本分支未改任何 compact 相关文件（`git diff main --stat | grep -i compact`
  为空），`e2e_real.rs` 相对 merge-base 也无改动。

## 4. 人工验收（**未执行**）

以下项需要真实手机与 relay 宿主，本 worktree 内没有移动端工程，无法执行：

- [ ] 桌面 + 真实手机同 relay 宿主：父长任务运行中开旁问、追问、返回、仅停旁支、父任务仍完成
- [ ] 桌面同时看见手机开的同一旁支（同一 side id）
- [ ] 冷父开旁支、手机后台唤醒后重连、close 冲突二次确认
- [ ] 无 capability 的旧 server：两端都不显示入口
- [ ] Observe 设备能读旁支、不能 Control（发送/停止/关闭）
- [ ] TUI：父审批中开旁问（其余 TUI 项已在第 3 节真实 PTY 完成）

未执行原因：需要至少一台真实手机、relay 宿主与移动端工程；Vitest 的 jsdom
视口不能替代真实设备。桌面端本轮只到单测 + 构建 + `just desktop-check`，
§12.5 真机部分仍未验收。

## 5. 2026-11-05 修复验收

本轮修的是「旁问进去以后退不出来 / 按了没反应」这一类可见性缺陷，
spec 与 plan：`docs/superpowers/specs/2026-11-05-side-conversation-repair-design.md`、
`docs/superpowers/plans/2026-11-05-side-conversation-repair-impl.md`。

### 5.1 构建身份与执行环境

| 项 | 值 |
| --- | --- |
| 基线 commit | `d25822ca`（Task 10 提交后），修复起点 `f35b2066` |
| 平台 | macOS aarch64（本机 worktree，未打包 .app） |
| 桌面端 | 源码 + `npx tsc --noEmit` + `npx vitest run`（未跑真实 WebView） |
| 移动端 | **未执行**：本 worktree 无移动端工程、无真机 |
| relay / hostKey | **未执行**：未连接任何 relay 宿主，无脱敏标识可记 |
| TUI | 源码级 TestBackend（80×24、32×12）与 `just ci`；**未跑真实 PTY** |

按要求不复制用户原始问题、不记录完整配置。

### 5.2 自动验证（已执行，全部通过）

```bash
(cd desktop && npx tsc --noEmit && npx vitest run)     # 159 files / 2226 tests passed
(cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::)              # 710 passed
(cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent side_repair_)       # 21 passed
(cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent side_driver)        # 9 passed
(cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent side_events_never_reach_main_turn_log)
(cd yi-agent-rs && just desktop-check)                 # 2226 passed
(cd yi-agent-rs && just ci)                            # 16 道门禁，CI passed（5428 tests）
git diff --check                                        # 无输出
```

一次说明：首轮 `just ci` 出现 `yi-agent-app-server ws::tests::a_client_leaving_does_not_kill_the_shared_loop` 单条失败。
该用例依赖真实 socket + 300ms sleep，单独重跑连跑 5 次全部通过，
随后完整 `just ci` 重跑通过；与本次改动无关（改动集中在 `crates/yi-agent/src/tui/`、
`side_driver`）。**不把基线 parser 错误或该类偶发项当成可接受失败**，此处只记录
已复现通过的事实。

### 5.3 W/T 覆盖

自动侧覆盖（测试全名见 plan Task 8–10 证据）：

| 验收 ID | 自动证据 | 真机/真实 PTY |
| --- | --- | --- |
| T01 | `side_repair_paste_stays_in_the_visible_side_draft`、`side_repair_typed_and_completed_back_work_through_the_loop`、`side_repair_tab_completes_then_enter_executes` | 未执行 |
| T02 | `side_repair_close_survives_a_full_side_channel`、`side_repair_close_survives_a_dropped_side_receiver` | 未执行 |
| T03 | `side_repair_paste_back_survives_a_parent_approval_through_the_loop` | 未执行 |
| T04 | `side_repair_paste_back_through_the_loop_returns_to_main_and_keeps_the_parent_draft`、`a_new_side_can_open_after_the_previous_one_closed` | 未执行 |
| T05 | `side_repair_tab_without_popup_does_not_edit_the_draft`、`side_repair_esc_closes_the_popup_before_stopping` | 未执行 |
| T06 | `side_repair_first_ctrl_c_in_side_stops_only_the_side`、`side_repair_editing_after_ctrl_c_rearms_the_confirmation` | 未执行（真实清理器/租约未单测覆盖，见 5.4） |
| T07 | `side_repair_ctrl_q_quits_even_with_the_runtime_popup_open`、`side_repair_ctrl_q_quits_with_a_pending_parent_approval`、`side_repair_quit_commands_leave_the_side_without_a_submit` | 未执行 |
| T08 | `side_repair_esc_closes_the_popup_before_stopping` | 未执行 |
| T09 | 上列两条 Full/Closed 用例 + driver 的 `stale_close_target_reports_error_and_keeps_the_live_side`、`stale_interrupt_does_not_stop_the_live_turn`、`failed_open_reports_a_main_target_error` | 未执行 |
| T10 | `side_status_bar_labels_the_conversation_without_covering_the_first_item`（80×24 与 32×12 × 深/浅）、`main_view_has_no_standalone_side_banner`、`side_events_never_reach_main_turn_log` | 未执行 |

W01–W16（桌面/手机）本轮**未改动对应实现**，也**未在真机复验**；本表不据此宣称通过。

### 5.4 明确未执行项

- 真实 WebView 桌面验收（本 worktree 只跑源码级测试，未打包 `.app`）。
- iPhone / relay / 双端同一 side id、后台唤醒重连、close 冲突二次确认。
- 真实 PTY 的 bracketed paste（`\x1b[200~/back\x1b[201~\r`、
  `\x1b[200~/side-close\x1b[201~\r`）、Esc、Ctrl+C×2、Ctrl+Q、退出命令。
- 真实模型流式回答（未取得授权，未调用真实 LLM）。
- 退出清理器/租约释放的真进程用例：当前无对应 fixture，本轮以源码级退出路径
  测试代替，**不作为已验收**。

因此本轮**不宣布修复验收通过**，`docs/project-management/yi-agent-tui.md` 中该条
保持未勾。
