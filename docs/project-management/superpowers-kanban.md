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
- [x] 会话内合并名额 — `merge-request` 发名额（每项目一把 `merge.lock`）、`merge-finish` 用 git 复核；准入接受 `awaiting_merge`/`needs_you`/`running` 与独立合并卡。判据：`cargo test -p superpowers-kanban-runner merge_request_`
- [x] **skill 与 CLI 不再各自漂移** — `SKILL.md` 随插件发布，模型靠它决定怎么调 CLI。新增三条回归锁（`crates/superpowers-kanban-runner/src/main.rs` 的 `the_skill_*`）：① 动词表**从 `USAGE` 解析**，技能必须给出每个动词的完整命令；② 状态名**从 `CardState` 枚举定义解析**，`list` 会打印的每个状态名技能都必须解释；③ frontmatter `description` 必须覆盖 enqueue / promote / merge / status 四类意图（否则那类说法根本不会加载技能）。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner the_skill`（3 passed）；变异验证：删掉技能里的 `purge` 或 `Paused` 后对应测试分别失败
- [x] skill 覆盖**触发面之外的正文** — 补 `superpowers-kanban archive|purge|list --all|manifest`、全部 11 个 `CardState` 词表、默认 `--state-dir` 取 **cwd** 的陷阱（卡片会话 cwd 是实现 worktree，裸跑 `add`/`list` 会指向空目录）、`add-merge` 由插件自己开合并轮（不要再 `merge-request`）、`done` 仅在 `awaiting_merge` 可用。判据：同上的 `the_skill_*` 三条
- [x] skill 与 `finishing-a-development-branch` 的冲突裁决 — 对方 skill 的 "Option 1: Merge locally" 会直接 `git checkout base && git merge`，绕过多项目唯一的 `merge.lock`。`SKILL.md` §6 显式裁决：卡片 worktree 内一律走 `merge-request` → `merge-finish`；用户要在看板外手工合并则让其自己执行、再走 `done` 登记；普通分支不插手。判据：`SKILL.md` 的 §6 + `the_skill_documents_every_cli_verb`
- [x] **并发配额按「全机运行会话数」判定** — 限流限的是总流量，配额判定从「只看板内 `Running` 卡数」改为「整机峰值 − 宿主上报的全机运行会话数」：宿主每拍在 `driver/next` 里下发 `sessions.running`（口径：`active_turn_id` 有值或状态 `Running`；`AwaitingApproval` 与人闲不计），插件算 `budget = limit − running` 后只判 `budget > 0`，`Board::claim_next_launch(budget)` 不再二次减看板内 `running_count()`（否则同一批会话被扣两遍、会超发）。峰值配置迁到整机唯一来源 `~/.yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml`，项目层即便还写 `default_max_tasks` / `window` 也忽略；`settings.read` 返回合并视图，`settings.write` 校验通过后按字段拆两半落盘。旧宿主不带 `sessions` 时退化为看板内口径并打 warn，绝不整拍报错。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`（171 passed，含 `outside_sessions_consume_the_same_budget`、`a_running_card_is_not_subtracted_twice`、`a_launch_is_allowed_when_the_whole_machine_has_room`、`a_running_count_above_the_peak_yields_zero_budget`、`driver_next_reads_the_machine_wide_running_count_from_the_wire`）；`cargo test -p superpowers-kanban-core`（133 passed，含 `the_global_settings_live_under_the_plugin_directory`、`a_project_level_max_tasks_is_ignored_in_favour_of_the_global_one`）；宿主侧 `cargo test -p yi-agent-app-server --lib session_driver::tests`（23 passed）与 `a_tick_drives_the_plugin_through_driver_next_only`（断言三拍上报 0/0/1）。变异验证：让 `launch_budget` 忽略宿主上报后，上述 3 条整机口径测试立即失败
- [x] 升级路径写清"两处都要换" — 覆盖二进制后 `run` 仍持旧 inode（需重启子进程），skill 是独立副本（需重新 `cp`，且新会话才生效）。判据：`plugins/superpowers-kanban/INSTALL.md` 的 §2a 与 §3a（含 `diff … && echo in-sync`）
