# superpowers-kanban（看板插件）

## 模块说明

`plugins/superpowers-kanban/` 是独立 workspace 的看板插件：把 spec+plan 排成队列，在
限流窗口内按顺序起会话开发，跑完在原会话里申请合并名额。宿主（daemon / app-server /
桌面）

**不认识看板**：它只按 supervisor 清单守护进程、按 `session_driver` 每 3s 拉一拍
`driver/next`、执行插件给的通用动作（`launch` / `append`）。插件通过
`superpowers-kanban manifest` 自述清单，宿主通用 `plugin/install` 消费。

本模块文档只管**插件本体与它的 skill**；宿主侧的解耦改造记在
`yi-agent-app-server.md`，桌面看板 UI 记在 `desktop.md`。

## 范围边界

**做什么：**
- 卡片状态机与排队/并发窗口（`crates/superpowers-kanban-core`）
- 插件与宿主的 socket 契约（`crates/superpowers-kanban-ipc`）
- `run` 推进循环、一次性 CLI 动词、`driver/next` 决策（`crates/superpowers-kanban-runner`）
- 面向模型的 `superpowers-kanban` skill（`skills/superpowers-kanban/SKILL.md`）

**不做什么：**
- 不写 spec / plan（上游是 `brainstorming` / `writing-plans`）
- 不在宿主里留任何看板专属代码路径
- 不管桌面看板面板的渲染（`desktop/`）

## Features

- [ ] **完整插件独立发布** — 界面、执行程序与 skill 统一版本，通过通用宿主安装和加载；官方包校验、显式项目启用、安全更新及远端版本匹配均须通过。完成判据：[设计](../superpowers/specs/2026-11-02-kanban-independent-release-design.md) §13 的 A01–A20 全部验收项，尤其在同一 yi-agent 应用包下更新看板界面且主程序发布物摘要不变。[实施总计划](../superpowers/plans/2026-11-02-kanban-independent-release.md) 已列出 27 个主任务、技术验证与子协议审阅门槛，当前仅完成设计、计划与多轮文档复查，尚未实现。

- [ ] **合并续跑原开发会话（方案 B）** — 设计见 [2026-11-03-kanban-merge-original-session-design.md](../superpowers/specs/2026-11-03-kanban-merge-original-session-design.md)，设计复核与[实施计划](../superpowers/plans/2026-11-03-kanban-merge-original-session.md)已完成，尚未实现。完成判据：实现卡获授权后只续跑原 `thread_id`，冷会话由宿主恢复；忙碌时等待，不可恢复时 `NeedsYou`，不降级新建；开发 `workdir` 不被合并目录覆盖，并发租约不重复领取。验证入口：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`，测试覆盖设计 §11 的插件与契约判据。

- [x] 全局开关 `superpowers_kanban` 决定进程起停 — 清单 `supervisors/superpowers-kanban.json` 设 `stop_when_disabled: true`，注册表默认关。关掉 = 完全停掉 daemon 含调度（不再留进程轮询）；重开走设置页 `flags/set`（app-server 直写 `flags.json`，不经过 daemon），所以不是单向门。宿主侧同时收口：`plugin/query` 返回 `plugin_disabled`(-32025) 而不是 `daemon_unavailable`，`plugins/list` 每项带 `enabled`，桌面隐藏看板入口与全部 `driver_ref` 会话。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib`（164 passed）；`cargo test -p yi-agent-app-server --lib a_switched_off_plugin`（含 `..._says_disabled_instead_of_broken`、`..._is_still_listed_but_marked_disabled`）；`cd desktop && vitest run src/App.test.tsx -t 看板开关关着时`
- [x] 插件自述清单 — `manifest` 动词把 supervisor 清单打到 stdout，宿主 `plugin/install` 直接消费；`command` 取当前可执行文件绝对路径。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner manifest_`（含"与随包 `supervisors/superpowers-kanban.json` 逐字段相等"）
- [x] **过期清单在启动时自校准** — 清单是安装那一刻的快照，插件升级后没人刷新它，于是新逻辑读旧结论：`stop_when_disabled` 从 `false` 改成 `true` 后，用户关掉开关、进程仍在跑，且界面无任何线索。现在监督线程进入循环前跑一次 `<command> manifest`，用插件自述覆盖过期清单（同目录 temp + rename，截断的清单会被静默跳过，等于插件消失）。只认清单里的 `command`、不认插件名；跑不起来 / 输出不是清单 / 名字对不上都只 warn 并**保留原文件**。判据：`cargo test -p yi-agent-supervisors --lib refresh::`（6 passed）；`cargo test -p yi-agent-supervisors --test supervisor refreshing_a_stale`（变异验证：把刷新替换为空结果后报 "the stale manifest must be replaced by the plugin's own description"）
- [x] 预排队列（`PRE-QUEUED`）与提升 — `add-prequeued` 记一张不占槽的卡，`promote` 补齐 spec/plan 并重排到队尾，id 全程不变。判据：`cargo test -p superpowers-kanban-core` 的 `promote`/`pre_queued` 用例
- [x] 会话内合并名额 — `merge-request` 只取名额（每项目一把 `merge.lock`，`merge.lock.holder` 记 card/pid/since，残留 pid 自动回收）；`merge-finish` **只放锁**，不碰 git、不改状态；准入只挡排队中的实现卡，其余由模型自己负责。判据：`cargo test -p superpowers-kanban-runner --lib merge_request_` 与 `merge_finish_`（`merge_finish_only_releases_the_gate_and_never_touches_git_or_state` 钉住「不放行 git 复核」）
- [x] **skill 与 CLI 不再各自漂移** — `SKILL.md` 随插件发布，模型靠它决定怎么调 CLI。新增三条回归锁（`crates/superpowers-kanban-runner/src/main.rs` 的 `the_skill_*`）：① 动词表**从 `USAGE` 解析**，技能必须给出每个动词的完整命令；② 状态名**从 `CardState` 枚举定义解析**，`list` 会打印的每个状态名技能都必须解释；③ frontmatter `description` 必须覆盖 enqueue / promote / merge / status 四类意图（否则那类说法根本不会加载技能）。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner the_skill`（3 passed）；变异验证：删掉技能里的 `purge` 或 `Paused` 后对应测试分别失败
- [x] skill 覆盖**触发面之外的正文** — 补 `superpowers-kanban archive|purge|list --all|manifest|state`、全部 8 个 `CardState` 词表、默认 `--state-dir` 取 **cwd** 的陷阱（卡片会话 cwd 是实现 worktree，裸跑 `add`/`list` 会指向空目录）、`done` 的兼容语义。判据：同上的 `the_skill_*` 三条
- [x] skill 与 `finishing-a-development-branch` 的冲突裁决 — 对方 skill 的 "Option 1: Merge locally" 会直接 `git checkout base && git merge`，绕过多项目唯一的 `merge.lock`。`SKILL.md` §6 显式裁决：卡片 worktree 内一律走 `merge-request` → `merge-finish`；用户要在看板外手工合并则让其自己执行、再走 `done` 登记；普通分支不插手。判据：`SKILL.md` 的 §6 + `the_skill_documents_every_cli_verb`
- [x] **并发配额按「全机运行会话数」判定** — 限流限的是总流量，配额判定从「只看板内 `Running` 卡数」改为「整机峰值 − 宿主上报的全机运行会话数」：宿主每拍在 `driver/next` 里下发 `sessions.running`（口径：`active_turn_id` 有值或状态 `Running`；`AwaitingApproval` 与人闲不计），插件算 `budget = limit − running` 后只判 `budget > 0`，启动侧**不再二次减**本看板 `Running` 卡数（否则同一批会话被扣两遍），并发判定也不再看「卡片是否绑在某个状态槽」——只看会话是否在跑。峰值配置的唯一来源是整机 `~/.yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml`，项目层即便还写 `default_max_tasks` / `window` 也忽略；`settings.read` 返回合并视图，`settings.write` 校验通过后按字段拆两半落盘。旧宿主不带 `sessions` 时退回**全机租约**口径（`budget = 峰值`，租约本就按活会话记、跨项目共享），绝不整拍报错。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`（164 passed，含 `outside_sessions_consume_the_same_budget`、`a_launch_is_allowed_when_the_whole_machine_has_room`、`a_running_card_is_not_subtracted_twice`、`a_running_count_above_the_peak_yields_zero_budget`、`a_host_that_does_not_report_sessions_falls_back_to_the_lease_pool`）；`cargo test -p superpowers-kanban-core`（116 passed）；宿主侧 `cargo test -p yi-agent-app-server --lib session_driver::tests`。变异验证：让 `launch_budget` 忽略宿主上报后，上述整机口径测试立即失败
- [x] **合并队列也吃全机并发额度** — 合并是一次真实的模型回合（在会话里调模型、跑 `git merge`），与实现会话抢同一份额度。`driver_step` 把余量 `budget = 峰值 − 宿主上报的全机运行会话数` 算在合并队列之前，`budget > 0` 才调 `promote_merge_queue`；余量为 0 时队首**留在 `merge_queued`**，不拿 `merge.lock`，下一拍余量回来再开——不设这道闸，峰值 4 的机器会出现「4 个实现会话 + 1 个合并」。次序仍是合并优先于启动，且合并开局即 `return`，同一拍不再起新会话。租约池大小仍按**峰值**开（`acquire_in` 按 `0..limit` 编号复用槽位，拿会变小的 `budget` 开池会让两批会话撞同一个槽）。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib the_merge_queue_waits_when_the_whole_machine_has_no_room`；变异验证：去掉 `if budget > 0` 后该测试立即红（队首会抢走 `merge.lock` 却跑不起来）
- [x] 升级路径写清"两处都要换" — 覆盖二进制后 `run` 仍持旧 inode（需重启子进程），skill 是独立副本（需重新 `cp`，且新会话才生效）。判据：`plugins/superpowers-kanban/INSTALL.md` 的 §2a 与 §3a（含 `diff … && echo in-sync`）
- [x] **状态流转改由模型驱动（看板不再猜）** — 卡片状态收缩为 8 个（`PreQueued`/`Queued`/`Running`/`NeedsYou`/`AwaitingMerge`/`Done`/`Failed`/`Cancelled`），删除 `Launching`/`Merging`/`Paused`；迁移规则只剩「终态不可复活」。daemon 只写三件事：排队、启动成功、启动失败；`idle`/`git HEAD`/宿主重启**一律不再推断状态**。模型经新增动词 `state <id> <state> [--detail]` / `state --self <state>` 声明状态。判据：`crates/superpowers-kanban-core/src/card.rs` 的 `can_transition_to`；`cargo test -p superpowers-kanban-runner --lib` 里的 `an_idle_session_never_changes_a_running_card`、`a_session_the_host_forgot_never_changes_the_card`、`a_card_moves_only_where_the_model_puts_it`
- [x] **并发按活会话计，与卡片状态解耦** — 宿主在 `driver/next` 的 `threads[]` 里多报 `alive`；插件按观测持有/释放租约，一张卡被标 `done` 但会话还开着仍占名额。删掉 `occupies_slot`/`running_count`/`free_slots`。判据：`a_live_session_keeps_its_slot_even_after_the_card_is_done`；宿主侧 `cargo test -p yi-agent-app-server --lib -- session_driver` 的 `a_thread_that_left_the_host_table_is_reported_dead_on_the_next_tick`
- [x] **观测旁路 `observations.json`（只读，永不写回状态）** — daemon 每拍只记 thread 的 `alive` 与 `last_seen_alive_at`；`list` 派生 `stale`（卡写着 running 但会话没了）与 `last_alive_at`，桌面显示角标与最后活动时间。判据：`crates/superpowers-kanban-runner/src/observation.rs` 的单测 + `list_marks_a_running_card_stale_when_its_session_is_gone`；桌面 `npx vitest run src/lib/superpowersKanbanState.test.ts src/components/SuperpowersKanbanCard.test.tsx`
- [x] **删掉 git 驱动的收敛面** — 移除 `core/converge.rs`、`runner/reconcile.rs`、`runner/runner.rs`、`recover_cards_the_host_forgot`、`has_commits`、`MAX_MERGE_RETRIES`、启动时的 `git merge --abort`、`DriverAction::Append`。daemon 不再碰 git，只剩终态卡超期归档这类纯簿记。判据：`grep -rn "converge\|has_commits\|MAX_MERGE_RETRIES" plugins/superpowers-kanban/crates` 无结果；`cargo test -p superpowers-kanban-core`（108 passed）与 `cargo test -p superpowers-kanban-runner`（181 passed）
- [x] **会话知道「这张卡的状态归我管」** — objective 末尾追加职责说明（含 `state --self` 与 `SUPERPOWERS_KANBAN_CARD`）；`launch` 动作新增通用 `env` 字段，由插件给出键值、宿主只透传，`bash` 子进程因此看得见卡片句柄（宿主不认任何具体键名）。判据：`cargo test -p superpowers-kanban-runner --lib the_objective_tells_the_session_it_owns_the_card_state`；`cargo test -p yi-agent-app-server --lib the_session_env_reaches_the_bash_child`
- [x] **合并要用户确认，队列串行** — `MergeQueued` 是唯一的合并授权，`AwaitingMerge` 只表示「活儿干完了」。用户发话 → 模型 `merge-request <id>` **只入队**（置 `MergeQueued` + `merge_queued_at`），不拿锁；daemon 每拍按入队时刻 FIFO 取队首，**先要会话名额再要 `merge.lock`**（反过来会占着锁等额度、堵住本项目所有合并），拿到才置 `Running` 并为它新开一轮合并会话（文案 `queued_merge_objective` 明说名额已持有、不要再申请）。一拍只推进一个，推完即返回。`merge-cancel <id>` 只在 `MergeQueued` 上生效，退回 `awaiting_merge`——队列由 daemon 每拍推进，若退队走无条件写状态，就会在「用户反悔」与「daemon 跑了下一拍」之间留下竞态。谁在合并由 `merge.lock.holder` 表达（`list` 派生 `merging`，CLI 行尾 `[merging]`，桌面角标），**不设 `Merging` 状态**。独立合并卡（`add-merge`）出生即 `MergeQueued`，否则它会走实现卡会话通路、永远拿不到名额。合并回合一结束，**daemon 自己**在下一拍把名额还回去：锁是它进程里的 `flock`，会话里那句 `merge-finish` 落在另一个进程、只会回 `cleared`；不回收的话一次合并会把本项目此后所有合并永久堵死，而界面上还看不到有人在合。
判据：`cargo test -p superpowers-kanban-runner --lib` 的 `the_daemon_releases_the_gate_once_the_merge_turn_settles`、`merge_finish_clears_a_holder_record_left_by_a_dead_process`、`merge_request_only_queues_the_card`、`merge_cancel_`、`the_queue_head_starts_merging_when_the_gate_is_free`、`only_one_merge_starts_per_tick`、`a_held_gate_leaves_the_queue_untouched`、`a_delivered_merge_card_is_started_by_the_queue`、`list_marks_the_card_that_holds_the_merge_gate`；`cargo test -p superpowers-kanban-core --lib a_delivered_merge_card_is_born_queued_for_merge`；`cd desktop && npx vitest run src/lib/boardIndex.test.ts src/lib/superpowersKanbanBoard.test.ts src/components/SuperpowersKanbanCard.test.tsx`
- [x] 设置面板的时段并发窗口写明时区口径 — 窗口在插件进程内按 `chrono::Local` 判定，UI 原先只给 `09:00` 而不说是哪个时区的 `09:00`，桌面经 relay 连远端宿主时用户无从判断。修复：面板在「全机时段并发窗口」标题下加一行无条件说明（`desktop/src/components/SuperpowersKanbanPluginSettings.tsx`，`data-testid="window-timezone-note"`），`INSTALL.md` 6.5 节补同口径句子；判定逻辑、`superpowers-kanban.toml` 格式与 `plugin/settings/read` 载荷均未改动。判据：`cd desktop && npx vitest run src/components/SuperpowersKanbanPluginSettings.test.tsx`（12 passed，含 `states which clock the window hours follow`）— [设计](../superpowers/specs/2026-10-29-kanban-window-timezone-note-design.md)
