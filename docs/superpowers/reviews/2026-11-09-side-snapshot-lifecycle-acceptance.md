# 一次性对话快照旁问 — 验收记录

- Spec：[2026-11-09-side-snapshot-lifecycle-design.md](../specs/2026-11-09-side-snapshot-lifecycle-design.md)
- Plan：[2026-11-09-side-snapshot-lifecycle-impl.md](../plans/2026-11-09-side-snapshot-lifecycle-impl.md)
- 规格复核：[2026-11-09-side-snapshot-lifecycle-spec-review.md](./2026-11-09-side-snapshot-lifecycle-spec-review.md)
- 记录日期：2026-11-09
- 分支：`feat/side-snapshot-lifecycle`

本文件只记**本次实际执行的命令、退出码、场景证据和未验边界**。不沿用历史测试
数量，也不把「自动检查通过」写成「真实设备验收通过」。

## 1. 交付范围

| 层 | 落点 | 提交 |
|---|---|---|
| core | 零工具旁支 prompt、`snapshot_for_side` 边界一次注入 | Task 1–2 |
| runtime | `SideConversationRunner` 一次性 fork、退出即 join、cleanup 收据 | Task 3–5 |
| app-server | 活父发布、入场票据、代次隔离、精确退出、Observe/Control 权限 | Task 6–10 |
| desktop / mobile | 统一 `leaveSide`、迟到 open 补退出、重连先补退出 | Task 11 |
| TUI | `Leave` 命令、可取消创建、`/back` 与 `/side-close` 同义销毁 | Task 12 |
| 跨端回归 | 垂直集成、变异证明、文档与验收 | Task 13（本文件） |

## 2. Task 13 Step 1 — 垂直集成（真实 stdio 生产链路）

新增/修改的文件：

- `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/mod.rs`
  - 内部构造器 `with_config_and_scope_and_watchman_in` 新增
    `live_side_parents: Option<Arc<LiveSideParents>>`；既有调用点一律传 `None`。
  - 新增 `Harness::with_live_side_registry`：注入**共享**活父表与自定义 agent 工厂。
    自造第二份活父表只能证明替身会返回答案，证明不了「父未落盘的进度能被旁支看到」。
  - 新增 `LiveParentResolver`：只查本进程活父表，查不到返回 `ParentNotStarted`。
  - 新增 `live_parent_side_registry`：把 `SideHostResources` 一并交出去，好让用例
    直接断言 `slots_in_use()` 归零，不必为断言在生产类型上开口子。
- `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/side.rs`
  - `a_side_sees_the_live_progress_and_dies_on_exit_without_touching_the_parent`
  - `a_second_client_reads_the_same_generation_but_cannot_control_it`

### 2.1 主链路用例覆盖

单条用例把 start / turn / refresh / prepare / open / turnStart / exit 串在**同一个
宿主**上；这些断言只有走真实链路才成立：

| 断言 | 说明 |
|---|---|
| `thread/start` 后活父表立刻有条目 | 旁支不依赖磁盘落地 |
| 父回合 `turn/completed.status == "completed"` | 父 interrupt 计数为 0 的可观察证据 |
| 父请求 `tools` 非空 | 旁支那条 `tools == []` 才有对照物 |
| 旁支首问请求 `tools == []` | 注册表零工具 |
| 旁支首问历史含父未落盘的「进度A」 | 活父内存快照生效 |
| 同代第二问不含「进度B」 | 快照固定，不实时跟进 |
| 父历史逐字节前后相等 | 旁支两轮问答未写父 session |
| `side/exit` 返回 `closing`/`closed`，槽位归零，`live_cleanups() == 0` | 退出即销毁，无残留清理任务 |
| `registry.side_count() == 0` | 本代真的从注册表摘除 |
| `tombstone_count() >= 2` | 迟到重放能拿到确定答案，而不是「从未存在」 |
| 旧 `readItems` 返回 `side_closing`/`side_not_found` | 退出后正文不可读 |
| 重入是新 `side_id`，含 B、不含旧旁问 | 全新环境 |

### 2.2 跨端权限用例覆盖

两条 stdio 连接共用一个 registry 与一份活父表（同宿主两端）：

- Control 端 `prepare`/`open`/`turnStart` 成功；Observe 端 `side/list` 与
  `side/readItems` 读到同一代、同一正文。
- Observe 端对 `side/prepare`、`side/open`、`side/turnStart`、`side/exit`
  一律 `-32014`（dispatch 处拒绝，而不是进注册表后报 `side_not_found`）。
- 越权尝试之后本代仍存活（`side_count == 1`）。
- Control 端退出后，两端 `side/list` 都为空、`readItems` 都返回
  `side_closing`/`side_not_found`。

一处测试装配限制已记录：两条 Harness 各自起一个 `serve`，而 `attach_hub` 是
「后装覆盖先装」，`side/*` 通知只会送到其中一条连接。生产里一个进程只有一个 hub，
所以该用例不依赖通知，改用 provider 记录 + `accepted_requests` 作为「已受理」证据。

## 3. Task 13 Step 2 — 变异证明

每个变异单独落在生产分支上，跑对应测试必须**失败**，再恢复并复跑通过。变异不提交。

| # | 变异 | 生产文件 | 命中测试 | 结果 |
|---|---|---|---|---|
| 1 | `/back` 只导航，不投递 `Leave` | `yi-agent/src/tui/app.rs` | `side_repair_pasted_back_needs_enter_and_sends_leave` | 失败（「必须发 Leave」） |
| 2 | 旁支工具注册表重新注册 `read` | `yi-agent-runtime/src/side_conversation/policy.rs` | `a_side_sees_the_live_progress_...` | 失败（`tools` 非空） |
| 3 | 快照/runner 两层都保留父 session 句柄 | `source.rs` + `runner.rs` | `a_side_sees_the_live_progress_...` | 失败（同代看到「进度B」） |
| 4 | 清理失败也释放资源槽 | `yi-agent-runtime/src/side_conversation/resources.rs` | `side_cleanup_failure_keeps_the_slot...` | 失败（槽位 0，应为 1） |
| 5 | 未知票据当首次创建放行 | `yi-agent-app-server/src/side_conversation/tickets.rs` | `an_unknown_ticket_is_expired...` 等 3 条 | 失败 |
| 6 | 去掉旧 entry 检查（`entry_matches` 恒 true） | `yi-agent/src/side_driver.rs` | `a_late_frame_from_an_older_entry...` | 失败（过期 entry 的提交被当成新问题） |
| 7 | `side/prepare` 放宽到 `Observe` | `yi-agent-app-server/src/rpc/side.rs` | `a_second_client_reads_the_same_generation...` | 失败（返回成功而非 `-32014`） |

变异 3 值得单独记一笔：只改 `source.rs`（不复制 session）或只改 `runner.rs`
（复用 `source.session`）都**不**让测试变红，因为两处各自独立地复制了一份。
必须同时改两处才能让「父历史逐字节不变 / 同代不跟随」被击穿。这说明该断言
监视的是**端到端的不共享**，而不是某一层的实现细节。

## 4. Task 13 Step 3 — 自动检查

命令与真实输出见下方各小节；只在全部为 0 退出码时记「通过」。

### 4.1 Rust

```bash
cd yi-agent-rs
cargo fmt --all
cargo fmt --all -- --check
cargo clippy -p yi-agent-app-server --all-features --all-targets -- -D warnings
cargo test -p yi-agent-app-server --all-features --lib
just ci
```

| 命令 | 结果 |
|---|---|
| `cargo fmt --all && cargo fmt --all -- --check` | exit 0 |
| `cargo clippy -p yi-agent-app-server --all-features --all-targets -- -D warnings` | exit 0 |
| `cargo test -p yi-agent-app-server --all-features --lib` | `1612 passed; 0 failed; 7 ignored`（342.91s） |
| `cargo test -p yi-agent-app-server --all-features --lib server::tests::side::` | `16 passed; 0 failed` |
| `cargo nextest run --all-features --profile ci -p yi-agent-app-server -E 'test(/server::tests::side::/)'` | `16 tests run: 16 passed` |
| `just ci` | exit 0，`CI passed`（6325 tests run: 6325 passed, 41 skipped；随后 50 tests run: 50 passed） |

#### 4.1.1 首次 `just ci` 的两个失败及修复

第一次 `just ci` 以 exit 100 结束，`target/nextest/ci/junit.xml` 记录 `failures="2"`：

```text
yi-agent-app-server :: server::tests::side::a_side_sees_the_live_progress_and_dies_on_exit_without_touching_the_parent  (side.rs:1124)
yi-agent-app-server :: server::tests::side::a_second_client_reads_the_same_generation_but_cannot_control_it            (side.rs:1342)
```

两处都是 `assert_eq!(registry_view.side_count().await, 0)`。根因不是生产逻辑错，
而是**测试等待条件不完整**：`SideRegistry::drain` 的第 6 步先把资源槽交回，
第 7 步才从 `sides` / `parents` 摘代次并写墓碑。测试只等 `slots_in_use() == 0`
就断言活跃代次为 0，恰好落在两步之间；`--test-threads=1` 时窗口太窄撞不上，
nextest 并行跑就稳定变红。

修复只在测试侧：把「等槽位归零」换成 `wait_for_side_gone`，同时等
`slots_in_use() == 0 && side_count() == 0`（20 秒预算，超时报出两者的实际值）。
生产代码未改。修复后：

- `cargo test -p yi-agent-app-server --all-features --lib server::tests::side::` → `16 passed; 0 failed`（9.14s）
- `cargo nextest run --all-features --profile ci -p yi-agent-app-server -E 'test(/server::tests::side::/)'` → `16 tests run: 16 passed`（0.628s，并行）

这条记录同时说明一件事：**「在更快的机器上单线程跑过」不等于 CI 通过**，
只有 `just ci` 的 nextest 并行结果算数。

#### 4.1.2 第二次 `just ci` 的两个无关失败

修复 side 用例后重跑，`target/nextest/ci/junit.xml` 仍是 `failures="2"`，但失败的
是两条**与本改动无关**的用例：

```text
yi-agent::managed_production_integration :: quitting_one_frontend_leaves_the_others_work_running
  crates/yi-agent/tests/managed_production_integration.rs:1474
  断言：退出 TUI 不该回收 headless run 的工作；实际 journal 里出现
  result: ReclaimResult { stopped: true }，即并行负载下守护进程提前回收

yi-agent-supervisors::guardian_control :: identity_does_not_create_a_runtime
  crates/yi-agent-supervisors/tests/guardian_control.rs:77
  Os { code: 61, kind: ConnectionRefused }
```

两条都在单独运行时通过（`identity_does_not_create_a_runtime` 0.022s 通过；
`quitting_one_frontend_leaves_the_others_work_running` 19.689s 通过）。判定为
并行负载下的时序噪声，依据是 CLAUDE.md「看到并发预检 WARN 且失败是超时类时，
先独占复跑再判断」的规则，以及这两条既不引用 side 代码也不在 side 测试 target 内。
#### 4.1.3 第三、四轮与最终通过

- 第三轮：`6325 tests run: 6320 passed, 5 failed`，失败集合与第二轮完全不同
  （`a_tui_exit_releases_its_lease`、`quitting_one_frontend_leaves_the_others_work_running`、
  `server::tests::process::read_items_returns_persisted_totals`、
  `a_minute_maintenance_tick_terminates_an_over_budget_attempt`、
  `killing_the_executor_group_stops_its_children`），全部单独复跑通过。
  当时本机 `load averages: 20.59 31.76 34.18`，另有会话在跑测试，
  并有一个卡住 9 小时以上、占 99% CPU 的残留测试进程。
- 第四轮：`6324 passed, 1 failed`，只剩 `a_tui_exit_releases_its_lease`
  （`expected 1 reclaim record(s) within 12s, found 0`，实际耗时 21.0s，
  超过测试自身 12 秒预算）。该用例连续三次单独复跑分别 3.945s / 3.523s / 3.976s
  全部通过，判定为并行负载超时而非回归。
- 清理掉本工作树里一个卡了 4 小时 42 分的残留测试进程（PID 24126/24127）后，
  第五轮 `just ci`：

```text
     Summary [  81.384s] 6325 tests run: 6325 passed, 41 skipped
     Summary [   0.299s] 50 tests run: 50 passed, 1569 skipped
CI passed
JUST_CI_EXIT=0
```

这是最终证据：**`just ci` exit 0，6325 + 50 全过**。前四轮的失败无一条落在
side 相关 target 内，且逐条单独复跑均通过。

### 4.2 desktop

```bash
cd desktop
npx tsc --noEmit
npx vitest run
npm run build
```

| 命令 | 结果 |
|---|---|
| `npx tsc --noEmit` | exit 0 |
| `npx vitest run` | `194 passed (194)` 文件 / `2850 passed (2850)` 测试 |
| `npm run build` | exit 0（仅 chunk 体积与动态导入警告，均为既有） |

## 5. Task 13 Step 4 — 真实交互验收

| 端 | 场景 | 状态 |
|---|---|---|
| TUI | 主长任务 → `/side` 问进展 → 追问 → 主继续 → 旁支不刷新 → 回答中 `/back` → 重入取得新进展 | 未执行 |
| 桌面 | 同上，含切会话/切项目 | 未执行 |
| 手机 | 同宿主连同一 relay；后台/短断线不退出；断网明确退出后重连先补退出 | 未执行 |
| release 构建 | `/side` 入口与六个 RPC 都不存在（`-32601`） | 未执行 |

未执行的原因写在交付说明里；在真机跑过之前，对应 feature 保持未完成。

## 6. 未验边界

- 供应商侧远端物理进程的强制终止不在本 spec 范围（§10），只能保证宿主侧
  runner join、转录与资源槽归零。
- 手机端真实 relay 断线时序依赖网络条件，自动测试只覆盖状态机分支，不替代真机。
