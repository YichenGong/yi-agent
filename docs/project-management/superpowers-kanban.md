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

- [x] 插件自述清单 — `manifest` 动词把 supervisor 清单打到 stdout，宿主 `plugin/install` 直接消费；`command` 取当前可执行文件绝对路径。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner manifest_`（含"与随包 `supervisors/superpowers-kanban.json` 逐字段相等"）
- [x] 预排队列（`PRE-QUEUED`）与提升 — `add-prequeued` 记一张不占槽的卡，`promote` 补齐 spec/plan 并重排到队尾，id 全程不变。判据：`cargo test -p superpowers-kanban-core` 的 `promote`/`pre_queued` 用例
- [x] 会话内合并名额 — `merge-request` 发名额（每项目一把 `merge.lock`）、`merge-finish` 用 git 复核；准入接受 `awaiting_merge`/`needs_you`/`running` 与独立合并卡。判据：`cargo test -p superpowers-kanban-runner merge_request_`
- [x] **skill 与 CLI 不再各自漂移** — `SKILL.md` 随插件发布，模型靠它决定怎么调 CLI。新增三条回归锁（`crates/superpowers-kanban-runner/src/main.rs` 的 `the_skill_*`）：① 动词表**从 `USAGE` 解析**，技能必须给出每个动词的完整命令；② 状态名**从 `CardState` 枚举定义解析**，`list` 会打印的每个状态名技能都必须解释；③ frontmatter `description` 必须覆盖 enqueue / promote / merge / status 四类意图（否则那类说法根本不会加载技能）。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner the_skill`（3 passed）；变异验证：删掉技能里的 `purge` 或 `Paused` 后对应测试分别失败
- [x] skill 覆盖**触发面之外的正文** — 补 `superpowers-kanban archive|purge|list --all|manifest`、全部 11 个 `CardState` 词表、默认 `--state-dir` 取 **cwd** 的陷阱（卡片会话 cwd 是实现 worktree，裸跑 `add`/`list` 会指向空目录）、`add-merge` 由插件自己开合并轮（不要再 `merge-request`）、`done` 仅在 `awaiting_merge` 可用。判据：同上的 `the_skill_*` 三条
- [x] skill 与 `finishing-a-development-branch` 的冲突裁决 — 对方 skill 的 "Option 1: Merge locally" 会直接 `git checkout base && git merge`，绕过多项目唯一的 `merge.lock`。`SKILL.md` §6 显式裁决：卡片 worktree 内一律走 `merge-request` → `merge-finish`；用户要在看板外手工合并则让其自己执行、再走 `done` 登记；普通分支不插手。判据：`SKILL.md` 的 §6 + `the_skill_documents_every_cli_verb`
- [x] 升级路径写清"两处都要换" — 覆盖二进制后 `run` 仍持旧 inode（需重启子进程），skill 是独立副本（需重新 `cp`，且新会话才生效）。判据：`plugins/superpowers-kanban/INSTALL.md` 的 §2a 与 §3a（含 `diff … && echo in-sync`）
