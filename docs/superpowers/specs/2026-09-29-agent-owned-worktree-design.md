# 子 Agent 工作区交给 Agent 自治设计

**状态：** 评审中。评审决定（2026-09-30）：交付上报保留、仅去 worktree 语义（§3.7）。

## 1. 问题

daemon 目前在"起 subagent runtime"和"派 coding 子任务"两个环节自动创建 git worktree，
并围绕这些 worktree 维护一整套生命周期：记录
workspace、校验交付、自动验收、自动回收。

这套自动化有两个问题：

1. **越权决定**：是否要隔离改动、隔离在哪个分支、叫什么名字，是需要结合上下文判断的
   决策，软件层无法替 agent 做对。
2. **回收成本**：自动创建就必须配套自动回收，而回收链路（detach 同步回收、7 天 TTL、
   `daemon gc`、重连重建）本身就带来持续复杂度与泄漏风险。

本设计把隔离决策交还 agent，daemon 退化为"按给定目录执行"。

## 2. 现状关键事实（实现约束）

- root session 的 workspace 由 `RuntimeCoordinator::attach_application_root` 经
  `root_mode_for()` + `prepare_task_workspace()` 供给；`root_mode_for` 在服务支持
  coding
  时返回 `TaskWorkspaceMode::Coding`，从而建出 `.worktrees/yi-agent-<hash>-root`。
- **root worktree 不只是给子 agent 用的**：它同时是根 agent 自己的 bash/edit 工作目录
  （`main.rs` 的 `register_builtin_tools_with_sandbox` 以
  `attached_root.workspace.path`
  为工具根）。因此"root 不建 worktree"等价于"根 session 直接运行在项目目录"。
- coding child 的 workspace 由 `prepare_child` 建出，base 为父 worktree HEAD。
- 交付纪律（干净校验 + commit 校验 + git ancestry 判定已进父历史）、验收后回收、
  worktree 重建，全部挂在 `AgentWorkspaceService` 与 `RuntimeCoordinator` 上。
  - `mode: coding | read_only` 目前同时承担四件事：建不建 worktree、沙箱是否可写、
    是否注册 write/edit、是否报 commit 交付。

## 3. 设计

### 3.1 核心原则

软件层不再自动建 worktree、不再跟踪子任务工作区、不再自动验收/回收。
**是否隔离、隔离在哪，由 agent 判断**；daemon 只负责"照给定目录执行"。

### 3.2 root 不再建 worktree

- `root_mode_for()` 对 root 恒为 `ReadOnly`，删除 root 的 `Coding` 分支。
- root 直接以项目目录为工具工作目录，不再有 `.worktrees/yi-agent-<hash>-root`
  与对应的 `feat/yi-agent-<hash>-root` 分支。
- **副作用（已确认接受）**：根 agent 的 bash/edit 直接落到用户 checkout 上，
  误改风险由"被 worktree 挡住"转为"靠提示词自觉"。
- **连带收益**：非 git 目录也可用委派（`supports_coding()` 的 git 门禁随之失效）。
- 注意 `root_mode` 只决定**是否为 root 建 workspace**，不决定根 agent 的写权限：
  根 agent 的工具沙箱来自 `main.rs` 的 `config.sandbox`，与这里的 mode 无关。
  否则「root 恒为 `ReadOnly`」会被误读成「根 session 不能写文件」。

### 3.3 子任务工作目录显式传入

- `spawn_agent` 新增参数 `workdir`（可选；缺省时子任务落在原地目录）。
- 父 agent 需要隔离时自己执行 `git worktree add <path> -b <branch>`，
  再把 `<path>` 作为 `workdir` 传给子任务。
- 子任务工具 root 在该路径；子任务不再自己建目录，也不再被分配分支。
- 父子之间"改动落在哪"因此是显式、可检查的输入，而不是隐式约定。

### 3.4 `mode` 退役为纯权限开关

- `read_only`（默认）= 原地只读，不注册 write/edit。
- `coding` = 在传入的 `workdir` 中可写。
- **两者都不再触发任何 worktree 创建。**
  保留"默认只读"这一安全默认值（来源：`2026-09-26-subagent-readonly-default-design.md`）。

### 3.5 整套退役（本设计的主要删除量）

删除以下能力与其持久化：

- `AgentWorkspaceService`：`prepare_root`、`prepare_child`、`inspect_delivery`、
  `cleanup_accepted`、`reclaim_worktree`、`reattach_workspace`、`contains_commit`、
  `is_merged_into`
- `RuntimeCoordinator`：`reclaim_session_worktrees`、`reclaim_idle_worktrees`、
  `recycle_accepted_delivery`
- IPC：`PreviewGc` / `ConfirmGc`（`yi-agent daemon gc` 整个命令）
- 持久化：`task_workspaces` 表、`TaskWorkspaceRecycled` /
  `TaskWorkspaceRecycleFailed` 事件
- 交付纪律中的**自动**部分：`RuntimeCoordinator::reconcile_integrated_deliveries`
  （`runtime.rs:3263`，经 `contains_commit` 判定子提交已进父 HEAD）与
  `confirm_review` 对 worktree 的 `inspect_delivery` 复核。
  coding child 自身的交付上报**保留**，见 §3.7。
- `yi-agent-tools/src/worktree.rs`：仅移除**为 daemon 编排**的方法
  （`create_root`/`create_child`/`inspect_delivery`/`merge_accepted`/
  `remove_accepted_clean`/`reclaim_directory`/`reattach_worktree`/`remove_created`/
  `remove_clean`/`contains_commit`/`is_ancestor`）。
  **`ignore_inside_repository` / `ignore_project_path` 必须保留** ——
  它们仍被 `ignore_project_local_runtime_state` 用于把 `<workdir>/.yi-agent`
  排除出 checkout，与 worktree 创建无关。
- 系统提示词（`agent.rs` 的 Subagent integration 段）：由"子 agent 交付后你自己
  `git merge --no-ff`"改为"需要隔离改动时，自己 `git worktree add` 并把路径传给
  `spawn_agent`"

### 3.6 `task_workspaces` 迁移策略

降级为"不再写入 + 建表语句移除"。**已有库中保留旧表与旧行，不主动 DROP**，
避免动用户数据；运行期不再读也不再写。

### 3.7 交付上报保留，仅去掉 worktree 语义

交付**证据链保留**：coding child 仍在自己的 `workdir` 里 commit，仍上报
`DeliveryReport`（`AwaitingParentReview` / `review_agent` / `accept_review` 结构不变）。
只删除挂在 worktree 上的**自动**部分（见 §3.5）。

- **交付身份改由 workdir 承担**：`WorkspaceLeaseId` 不再来自 `task_workspaces` 行，
  改为由 workdir 路径派生（与 `recovery_context_for_workspace` 现有的
  `format!("workspace:{}", path)` 同一形态）。`AgentTask::validate_for` 的
  `delivery.workspace == task.workspace` 校验因此仍然成立，只是来源变了。
- **coding child 收口**：`subagent_runtime.rs` 原先在
  `workspace_mode == Coding` 时经 `AgentWorkspaceService::inspect_delivery` 产出交付；
  该 trait 方法退役后，改为在子任务的 `workdir` 上直接做 git 探测（干净校验 + HEAD
  与 base 的差异）。「child worktree is dirty」的重试提示语义保持不变，只把措辞里的
  "worktree" 改为子任务 `workdir`。
- **只读 child 不变**：仍走文本结果收口，无交付。
- **diff 读取不变**：`inspect_agent` / `ReadTaskDiff` 仍以子任务 `workdir` 为 diff 目录，
  不依赖 `task_workspaces`。

## 4. 备选方案与否决理由

- **子 agent 自己建 worktree**（否决）：路径与分支名由每个子 agent 各自决定，
  易撞名、易建在意外位置，且父 agent 事先不知道改动落在哪，合并与清理无从下手。
- **子任务继承父当前目录**（否决）：根 agent 目录即项目根，coding 子任务会直接在
  项目根改文件，隔离形同虚设，且并发子任务互相踩。
- **mode 完全退役、权限全交沙箱配置**（否决）：会丢掉"默认只读"这一专门安全默认值，
  且改动面更大。

## 5. 范围边界

- 不引入新的 worktree 自动化（本次是净删除）。
- 不主动 DROP 已有库中的 `task_workspaces` 表与行。
- 不改动 provider / TUI 渲染 / 权限检查器本身的语义。
- `mode` 的沙箱与工具注册分支逻辑基本不动，仅移除其 worktree 触发作用。
- 交付上报结构（`DeliveryReport` / `AwaitingParentReview` / `review_agent`）保留，
  改动仅限其中依赖 worktree 身份的部分（§3.7）。

## 6. 测试影响

- `runtime_coordinator`、`runtime_ipc`、`subagent_worktree` 中大量用例建立在
  "root 或 coding child 一定有 worktree"之上，需改写或删除。
- `yi-agent-tools/tests/subagent_worktree.rs` 整体大概率随之退役
  （但其中 `ignore_*` 相关用例需迁到保留测试中）。
- 新增用例应覆盖：root 在项目目录原地运行、`spawn_agent` 的 `workdir` 生效、
  `mode` 只影响写权限而不建目录、非 git 目录可委派。
- 交付链路（§3.7）改为断言：coding child 在 `workdir` 里的交付仍可上报，
  且 `delivery.workspace` 与由 `workdir` 派生的身份一致（不再有 `task_workspaces` 行）。

## 7. 验证

- `cargo test -p yi-agent-store --test runtime_coordinator`
- `cargo test -p yi-agent-store --test runtime_ipc`
- `cargo test -p yi-agent --bin yi-agent`
- 端到端：干净 git 项目下 `run --subagents`，断言**不产生**任何
  `.worktrees/yi-agent-*-root`，且退出后项目 checkout 无残留。
