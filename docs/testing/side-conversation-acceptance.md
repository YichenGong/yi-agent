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
| TUI 全量 | `cargo test -p yi-agent --bin yi-agent tui::` | 704 passed, 0 failed |
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

**完整 `just ci` 已一次跑通：`EXIT=0`，末行 `CI passed`**，其中
`cargo nextest run --all-features --workspace --profile ci` 为
`4372 tests run: 4372 passed, 28 skipped`，`just ci` 的其余 15 道门禁
（fmt-check / lint / build / bindings-check / secret-check / 版本与 release 检查 /
msrv-check / audit 等）全部通过；`audit` 用本地缓存的 advisory-db（日志已明确
告警「结论基于过期数据」，nightly job 会重跑）。

早前一次性 `just ci` 的 `test` 步骤曾被系统 SIGKILL（同期另一 worktree 在编译，
属资源竞争而非用例失败），本轮重跑未复现。

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

## 3. 人工验收（**未执行**）

以下项需要真实设备 / 真实终端，本会话无法执行，因此整体交付保持未完成：

- [ ] 桌面 + 真实手机同 relay 宿主：父长任务运行中开旁问、追问、返回、仅停旁支、父任务仍完成
- [ ] 桌面同时看见手机开的同一旁支（同一 side id）
- [ ] 冷父开旁支、手机后台唤醒后重连、close 冲突二次确认
- [ ] 无 capability 的旧 server：两端都不显示入口
- [ ] Observe 设备能读旁支、不能 Control（发送/停止/关闭）
- [ ] TUI：父审批中开旁问、Esc 只停旁支、退出与 resume 后状态正确、窄 CJK 终端渲染

未执行原因：需要至少一台真实手机与 relay 宿主、以及一个真实 TTY。Vitest 的 jsdom
视口与 Rust `TestBackend` 是模拟层，不能替代上述项。
