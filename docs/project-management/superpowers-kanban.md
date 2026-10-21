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

- [x] 全局开关 `superpowers_kanban` 决定进程起停 — 清单 `supervisors/superpowers-kanban.json` 设 `stop_when_disabled: true`，注册表默认关。关掉 = 完全停掉 daemon 含调度（不再留进程轮询）；重开走设置页 `flags/set`（app-server 直写 `flags.json`，不经过 daemon），所以不是单向门。宿主侧同时收口：`plugin/query` 返回 `plugin_disabled`(-32025) 而不是 `daemon_unavailable`，`plugins/list` 每项带 `enabled`，桌面隐藏看板入口与全部 `driver_ref` 会话。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib`（164 passed）；`cargo test -p yi-agent-app-server --lib a_switched_off_plugin`（含 `..._says_disabled_instead_of_broken`、`..._is_still_listed_but_marked_disabled`）；`cd desktop && vitest run src/App.test.tsx -t 看板开关关着时`
- [x] 插件自述清单 — `manifest` 动词把 supervisor 清单打到 stdout，宿主 `plugin/install` 直接消费；`command` 取当前可执行文件绝对路径。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner manifest_`（含"与随包 `supervisors/superpowers-kanban.json` 逐字段相等"）
- [x] **过期清单在启动时自校准** — 清单是安装那一刻的快照，插件升级后没人刷新它，于是新逻辑读旧结论：`stop_when_disabled` 从 `false` 改成 `true` 后，用户关掉开关、进程仍在跑，且界面无任何线索。现在监督线程进入循环前跑一次 `<command> manifest`，用插件自述覆盖过期清单（同目录 temp + rename，截断的清单会被静默跳过，等于插件消失）。只认清单里的 `command`、不认插件名；跑不起来 / 输出不是清单 / 名字对不上都只 warn 并**保留原文件**。判据：`cargo test -p yi-agent-supervisors --lib refresh::`（6 passed）；`cargo test -p yi-agent-supervisors --test supervisor refreshing_a_stale`（变异验证：把刷新替换为空结果后报 "the stale manifest must be replaced by the plugin's own description"）
- [x] 预排队列（`PRE-QUEUED`）与提升 — `add-prequeued` 记一张不占槽的卡，`promote` 补齐 spec/plan 并重排到队尾，id 全程不变。判据：`cargo test -p superpowers-kanban-core` 的 `promote`/`pre_queued` 用例
- [x] 会话内合并名额 — `merge-request` 只取名额（每项目一把 `merge.lock`，`merge.lock.holder` 记 card/pid/since，残留 pid 自动回收）；`merge-finish` **只放锁**，不碰 git、不改状态；准入只挡排队中的实现卡，其余由模型自己负责。判据：`cargo test -p superpowers-kanban-runner --lib merge_request_` 与 `merge_finish_`（`merge_finish_only_releases_the_gate_and_never_touches_git_or_state` 钉住「不放行 git 复核」）
- [x] **skill 与 CLI 不再各自漂移** — `SKILL.md` 随插件发布，模型靠它决定怎么调 CLI。新增三条回归锁（`crates/superpowers-kanban-runner/src/main.rs` 的 `the_skill_*`）：① 动词表**从 `USAGE` 解析**，技能必须给出每个动词的完整命令；② 状态名**从 `CardState` 枚举定义解析**，`list` 会打印的每个状态名技能都必须解释；③ frontmatter `description` 必须覆盖 enqueue / promote / merge / status 四类意图（否则那类说法根本不会加载技能）。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner the_skill`（3 passed）；变异验证：删掉技能里的 `purge` 或 `Paused` 后对应测试分别失败
- [x] skill 覆盖**触发面之外的正文** — 补 `superpowers-kanban archive|purge|list --all|manifest|state`、全部 8 个 `CardState` 词表、默认 `--state-dir` 取 **cwd** 的陷阱（卡片会话 cwd 是实现 worktree，裸跑 `add`/`list` 会指向空目录）、`done` 的兼容语义。判据：同上的 `the_skill_*` 三条
- [x] skill 与 `finishing-a-development-branch` 的冲突裁决 — 对方 skill 的 "Option 1: Merge locally" 会直接 `git checkout base && git merge`，绕过多项目唯一的 `merge.lock`。`SKILL.md` §6 显式裁决：卡片 worktree 内一律走 `merge-request` → `merge-finish`；用户要在看板外手工合并则让其自己执行、再走 `done` 登记；普通分支不插手。判据：`SKILL.md` 的 §6 + `the_skill_documents_every_cli_verb`
- [x] 升级路径写清"两处都要换" — 覆盖二进制后 `run` 仍持旧 inode（需重启子进程），skill 是独立副本（需重新 `cp`，且新会话才生效）。判据：`plugins/superpowers-kanban/INSTALL.md` 的 §2a 与 §3a（含 `diff … && echo in-sync`）
- [x] **状态流转改由模型驱动（看板不再猜）** — 卡片状态收缩为 8 个（`PreQueued`/`Queued`/`Running`/`NeedsYou`/`AwaitingMerge`/`Done`/`Failed`/`Cancelled`），删除 `Launching`/`Merging`/`Paused`；迁移规则只剩「终态不可复活」。daemon 只写三件事：排队、启动成功、启动失败；`idle`/`git HEAD`/宿主重启**一律不再推断状态**。模型经新增动词 `state <id> <state> [--detail]` / `state --self <state>` 声明状态。判据：`crates/superpowers-kanban-core/src/card.rs` 的 `can_transition_to`；`cargo test -p superpowers-kanban-runner --lib` 里的 `an_idle_session_never_changes_a_running_card`、`a_session_the_host_forgot_never_changes_the_card`、`a_card_moves_only_where_the_model_puts_it`
- [x] **并发按活会话计，与卡片状态解耦** — 宿主在 `driver/next` 的 `threads[]` 里多报 `alive`；插件按观测持有/释放租约，一张卡被标 `done` 但会话还开着仍占名额。删掉 `occupies_slot`/`running_count`/`free_slots`。判据：`a_live_session_keeps_its_slot_even_after_the_card_is_done`；宿主侧 `cargo test -p yi-agent-app-server --lib -- session_driver` 的 `a_thread_that_left_the_host_table_is_reported_dead_on_the_next_tick`
- [x] **观测旁路 `observations.json`（只读，永不写回状态）** — daemon 每拍只记 thread 的 `alive` 与 `last_seen_alive_at`；`list` 派生 `stale`（卡写着 running 但会话没了）与 `last_alive_at`，桌面显示角标与最后活动时间。判据：`crates/superpowers-kanban-runner/src/observation.rs` 的单测 + `list_marks_a_running_card_stale_when_its_session_is_gone`；桌面 `npx vitest run src/lib/superpowersKanbanState.test.ts src/components/SuperpowersKanbanCard.test.tsx`
- [x] **删掉 git 驱动的收敛面** — 移除 `core/converge.rs`、`runner/reconcile.rs`、`runner/runner.rs`、`recover_cards_the_host_forgot`、`has_commits`、`MAX_MERGE_RETRIES`、启动时的 `git merge --abort`、`DriverAction::Append`。daemon 不再碰 git，只剩终态卡超期归档这类纯簿记。判据：`grep -rn "converge\|has_commits\|MAX_MERGE_RETRIES" plugins/superpowers-kanban/crates` 无结果；`cargo test -p superpowers-kanban-core`（108 passed）与 `cargo test -p superpowers-kanban-runner`（181 passed）
- [x] **会话知道「这张卡的状态归我管」** — objective 末尾追加职责说明（含 `state --self` 与 `SUPERPOWERS_KANBAN_CARD`）；`launch` 动作新增通用 `env` 字段，由插件给出键值、宿主只透传，`bash` 子进程因此看得见卡片句柄（宿主不认任何具体键名）。判据：`cargo test -p superpowers-kanban-runner --lib the_objective_tells_the_session_it_owns_the_card_state`；`cargo test -p yi-agent-app-server --lib the_session_env_reaches_the_bash_child`
