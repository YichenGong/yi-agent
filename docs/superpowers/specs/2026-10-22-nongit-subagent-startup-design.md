# 子 Agent 非 Git 目录启动策略设计

## 状态

设计待用户 review。

## 背景与问题

当前 `spawn_agent` 在非 git 目录下失败，且失败信息对用户不可理解。用户提出的期望是：

1. `read_only` 子 agent 允许在非 git 路径下启动。
2. `coding` 子 agent 仍必须在 git 环境内运行；但要让父 LLM 主动提示用户，并询问是否帮忙创建 git 环境。用户拒绝后，本会话内不再启动 `coding` 子 agent。

### 现状证据

**失败点一：read-only 也被拒，且拒绝发生在准入层。**

- `DaemonAgentWorkerFactory::recovery_context_for_workspace`（`yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:409`）用 `git rev-parse --show-toplevel` 推 `worktree_lease`，非 git 时为 `None`。
- `RuntimeCoordinator::start_worker`（`yi-agent-rs/crates/yi-agent-store/src/runtime.rs`，`transition_task_and_attempt_with_recovery_context_and_resident_lease` 调用处）把上下文交给仓库层。
- `validate_recovery_context`（`yi-agent-rs/crates/yi-agent-store/src/repository.rs:3767`）对 `worktree_lease` 缺失直接报 `worktree lease is missing`。
- 这条路径对 `ChildWriteMode::ReadOnly` 与 `Coding` 一视同仁，没有任何按 mode 分支的判断。

**失败点二：read-only 带着 `workdir` 时先被 git 校验拦住。**

- `spawn_child_with_objective`（`runtime.rs:1626`）无条件对 `workdir` 调 `registry.observe_workdir(workdir)`。
- `DaemonWorkspaceService::register_workdir`（`lib.rs:456`）在 `git rev-parse --show-toplevel` 失败时报 `workdir is not inside a git worktree`。
- 而 `prepare_task_workspace`（`runtime.rs:2106`）对 read-only 任务**根本不用** `spawn_workdir`，只取最近祖先的路径。read-only 的 `workdir` 目前既被校验、又没被使用。

**失败点三：coding 的拒绝信息是内部术语。**

- 现有文案是 `workdir is not inside a git worktree`、`worktree lease is missing`，都没告诉用户"需要 `git init` + 至少一次提交"。
- 其中 `worktree lease is missing` 来自仓库层 `InvalidWorkerRecoveryContext`，经 `ipc_error_code`（`yi-agent-rs/crates/yi-agent-store/src/ipc.rs:2964`，`repository_error_code` 在 `:3041` 把 `InvalidWorkerRecoveryContext` 归入 `Internal`）映射为 `Internal`，而 `ipc_error_message`（同文件 `:2943`）不为它提供 message。客户端最终只看到 `daemon rejected spawn request: internal`——完全没有可操作信息。

**失败点四（实测确认）：空仓库不会被 spawn 拦住，而是启动后交付失败。**

在临时目录实测：

```
$ git init -q .
$ git rev-parse --show-toplevel      -> /private/tmp/...   （成功）
$ git rev-parse HEAD                 -> fatal: ambiguous argument 'HEAD'
```

`register_workdir` 只用 `--show-toplevel` 判断，所以**空仓库能通过 spawn 校验**，`base_commit` 落成空串。子 agent 随后正常启动、写文件，直到交付阶段 `inspect_workdir`（`yi-agent-rs/crates/yi-agent-tools/src/worktree.rs:67`）的 `rev-parse HEAD` 失败，子任务以 failed 收尾。

这比"启动即拒"更糟：用户已经等了整个子任务时长才拿到失败。所以 git 判据必须是"有 HEAD"，不能是"在仓库里"。

**失败点五：spawn 校验失败会留下 queued 僵尸任务。**

`spawn_child_with_objective` 先写任务行（`create_child_task_with_attempt_and_objective`），之后才调 `observe_workdir`（`runtime.rs:1626`）。失败时行已存在且为 `queued`，没有回滚。

**现状契约测试把旧行为钉住了。**

- `an_autonomous_session_refuses_a_workdir_that_is_not_a_git_worktree`（`yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs:3525`）断言非 git workdir 必须被拒。
- `a_non_git_cwd_attaches_in_place_with_the_delegation_tools`（`yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs:909`）断言非 git 下 `read_only` 的 `SpawnApplicationChild` 也必须被拒。**本设计要改掉这条断言。**

**read-only 的收尾不依赖 git（这决定了不需要改交付结构）。**

- read-only 收尾走 `report_completed(assistant_report)`（`yi-agent-subagent/src/lib.rs` 的 coding 分支的 else 支），不经过 `inspect_delivery`，也不构造 `DeliveryReport`。
- 因此本设计**不需要** 2026-10-19 草案中的 `WorkspaceVcs` / `DeliveryKind::Unverified`。那份草案主张"静默降级 coding"，与本设计方向相反，明确不采用。

**root 会话不受影响。**

- 应用级 root 的 Agent 由 app-server / TUI 自己持有（`yi-agent-subagent/src/thread_root.rs:225`），不经过 daemon 的 `start_worker` 准入。所以"非 git 目录下 root 能正常对话"成立，只有子任务准入会拒。

## 目标

1. `mode: "read_only"` 的子 agent 在非 git 目录下能真正启动并正常收尾。
2. `mode: "coding"` 的子 agent 在非 git 目录（含空仓库）下被拒，但拒绝信息改成模型与用户都能照做的形式。
3. 父 LLM 收到该拒绝后，会向用户说明并询问是否创建 git 环境；用户拒绝后本会话不再尝试 coding 子 agent。
4. git 环境下的现有行为逐字不变。
5. spawn 失败不再留下 queued 僵尸任务。

## 非目标

- 不新增 `spawn_agent` 工具参数，不新增工具，不做 daemon 侧持久化的"用户已拒绝"状态。
- 不自动执行 `git init`。创建动作由用户同意后、LLM 通过既有 `bash` 工具发起（走既有权限确认）。
- 不改 read-only 的 workspace 语义（仍继承最近祖先路径）。
- 不实现非 git 下的 coding 降级（明确否决 2026-10-19 草案的方向）。

## 设计

### 1. coding 的 git 判据：必须有 HEAD，且在写任务行之前检查

新增 `RuntimeCoordinatorError::CodingRequiresGit { workdir: PathBuf, reason: &'static str }`，映射到 `IpcErrorCode::Validation`（客户端可据此区分"用户/环境问题"与 daemon 故障），并在 `ipc_error_message` 里返回稳定前缀 `CODING_REQUIRES_GIT_PREFIX`。

检查放在 `spawn_child_with_objective` 的**最前面**（`ensure_admitting` 之后、写任务行之前）：

- 仅当 `workspace_mode == ChildWriteMode::Coding` 时执行。
- 目标目录的选取：显式 `workdir` 优先；否则用 `remembered_task_position(parent)` 的 `path`（coding 无 workdir 时会落到 `in_place_workspace`，即 root 的 `repository_root`）。两者都没有时不做前置检查，交给第 3 节的准入兜底。
- 判据（两道都过才放行）：
  1. `git rev-parse --show-toplevel` 成功；
  2. `git rev-parse HEAD` 成功。
- 失败时 `reason` 区分两种：`not_a_repository` / `no_initial_commit`。

第 2 条是本次实测暴露的关键修正。只判第 1 条会把空仓库放进来，最终在交付阶段以一条 `Git workspace error` 失败，用户体验更差。

同时把 `register_workdir`（`lib.rs:456`）的判据改成同样的两道，返回带 `CODING_REQUIRES_GIT_PREFIX` 前缀的错误；这样直接调 `observe_workdir` 的其他入口（`workspace_in`、`observe_workdir`）也得到一致文案。

既有 `prepare_task_workspace` 与准入逻辑不改：前置检查拦住之后，`start_worker` 里的 `worktree lease is missing` 只作为第二道防线，正常路径不会走到。

### 2. read-only 放行：按 mode 区分恢复上下文

只改两处判断，git 路径的逻辑不动。

**(a) `recovery_context_for_workspace`（`lib.rs:409`）按 mode 分支。**

该函数目前只接收 `workspace: PathBuf`。改为接收 `workspace_mode: ChildWriteMode`（调用方 `recovery_context_for` 已持有 `&WorkerStart`，其中 `workspace_mode` 字段存在，无需新增参数）。

- `Coding`：行为不变，`worktree_lease` 缺失时保持 `None`（由第 1 节前置检查负责给出可读错误，这里是兜底）。
- `ReadOnly`：`git rev-parse --show-toplevel` 失败时，`checkpoint_json.kind` 记为 `"read_only_checkpoint"`，`git_head` 为 `null`。`worktree_lease` 仍为 `None`。

**(b) `validate_recovery_context`（`repository.rs:3767`）放宽一个分支。**

`worktree_lease` 缺失时：若 `checkpoint_json.kind == "read_only_checkpoint"` 则放行；**其他任何 kind 或未知值都维持原错误**。判据写在 JSON 里而不是靠"字段为空"推断，避免把编码错误当成合法状态。

`workspace_lease_id` 的既有要求不变（`recovery_context_for_workspace` 总会写 `workspace:<path>`）。

read-only 在 **git** 目录下时 `git_root` 有值，`kind` 仍是 `worker_admission`，与今天逐字相同，所以 git 行为不受影响。

### 3. read-only 不再校验 workdir 的 git 归属

`spawn_child_with_objective` 里 `observe_workdir` 的调用（`runtime.rs:1626`）加 mode 条件：

- `Coding`：保持现状（父必须准备 worktree；坏路径立刻失败）。
- `ReadOnly`：跳过 `observe_workdir`。理由：`prepare_task_workspace` 对 read-only 本来就不读 `spawn_workdir`，校验它只会制造与用途无关的失败。

目录不存在仍会由 `prepare_task_workspace` 的既有路径暴露；不新增校验。

**这一改动同时解决僵尸任务问题**：第 1 节的 coding 前置检查已在写行之前，read-only 又不再调 `observe_workdir`，该调用点只剩 coding 的合法路径。

### 4. coding 的拒绝信息改成可照做、可询问的结构

工具层（`spawn_outcome` / `format_ipc_rejection`，`lib.rs`）识别 `CODING_REQUIRES_GIT_PREFIX`，把 daemon 的错误改写成结构化 JSON：

```json
{
  "code": "coding_requires_git",
  "workdir": "/Users/x/notes",
  "reason": "not_a_repository",
  "remedy": {
    "commands": ["git init", "git add -A", "git commit -m \"initial commit\""],
    "requires_initial_commit": true,
    "note": "a repository with no commits still fails: the runtime needs a HEAD to recover into"
  },
  "ask_user": "The coding subagent needs version control in <workdir>. I can create a git repository there and make an initial commit. Do you want me to?"
}
```

- `reason` 取 `not_a_repository` 或 `no_initial_commit`，让模型能区分"还没初始化"与"初始化了但没提交"。
- `ask_user` 给模型一句可直接改写的话术。纯提示词方案下，这是"LLM 会去问用户"的主要保障。
- `remedy.commands` 对 `no_initial_commit` 情形可省去 `git init`，避免模型重复初始化。

识别方式只依赖一个常量前缀（`CODING_REQUIRES_GIT_PREFIX = "coding_requires_git:"`），在代码里集中定义并有测试覆盖。IPC 协议不变。

### 5. 提示词：让模型询问而不是反复重试

两处文案（与现有"文案不漂移"的做法一致，都有测试钉住）：

- `spawn_agent` 的 `workdir` 描述与 `spawn_agent_description`（`lib.rs:1949`、`:1966`）：写明 coding 需要 git 环境（含至少一次提交），收到 `coding_requires_git` 时应询问用户。
- `AgentConfig::default_system_prompt()` 的 "Subagent integration" 段（`yi-agent-core/src/agent.rs:218`）：补一条规则——遇到 `coding_requires_git` 先问用户是否创建 git 环境；同意则用 `bash` 执行 `remedy.commands` 后重试；拒绝则本会话不再启动 coding 子 agent，改用 `read_only` 或直接在本会话完成。

"用户拒绝后不再尝试"的执行者是模型本身，daemon 不记录该状态。**这是已知弱点**：模型可能仍重试。缓解是错误 JSON 自带 `ask_user` 话术 + 系统提示词规则；无法技术强制。用户已明确选择纯提示词方案，设计接受这一点。

## 测试判据

每条都能独立跑，不依赖真实 LLM。

**T1 read-only 在非 git 目录下能启动并完成**
`cargo test -p yi-agent-store --test runtime_coordinator non_git_read_only_child_starts_and_completes`
- 非 git 临时目录 + 既有 `NonGitWorkspaceService` 替身（`runtime_coordinator.rs:368`）。
- 断言 `start_worker` 返回 `Ok`，任务终态为 `Completed`（不是 `failed`）。

**T2 coding 在非 git 目录下被拒且不产生任务行**
`cargo test -p yi-agent-store --test runtime_coordinator non_git_coding_child_is_refused_without_a_queued_row`
- 断言 spawn 返回 `CodingRequiresGit`，且 `queued_task_count()` 前后不变。

**T3 空仓库被拒，理由是 `no_initial_commit`**
`cargo test -p yi-agent-store --test runtime_coordinator an_empty_repository_is_refused_for_missing_head`
- 临时目录 `git init` 后不提交，断言 spawn 被拒且 `reason == "no_initial_commit"`。

**T4 工具层把错误翻译成结构化 JSON**
`cargo test -p yi-agent-subagent --lib spawn_error_reports_coding_requires_git`
- 断言 `spawn_agent` 的 `ToolResult` 内容可解析，含 `code`、`remedy.commands`、`requires_initial_commit: true`、非空 `ask_user`。

**T5 IPC 映射：Validation 而非 Internal**
`cargo test -p yi-agent-store --lib coding_requires_git_mapping_tests`
- 断言客户端收到的 code 是 `validation`，且 message 含前缀。防止退回今天"只看到 internal"的状态。
- 必须是 `ipc.rs` 的内联 `mod`：`ipc_error_message` / `ipc_error_code` 都是私有 `fn`（`ipc.rs:2943` / `:2964`），`--test runtime_ipc` 这类集成测试访问不到。

**T6 git 下行为不变**
- `cargo test -p yi-agent-store --test runtime_coordinator`（含 `read_only_child_runs_in_place_without_a_workspace_row`）
- `cargo test -p yi-agent-subagent --test attach_delegation`
- `cargo test -p yi-agent-app-server a_git_project_gets_the_delegation_tools`

**T7 契约测试按新语义更新**
- `a_non_git_cwd_attaches_in_place_with_the_delegation_tools`：`read_only` 必须被接受，`coding` 必须被拒且带 `coding_requires_git`。
- `an_autonomous_session_refuses_a_workdir_that_is_not_a_git_worktree`：保留拒绝，断言错误含可照做指引。

**T8 提示词不漂移**
`cargo test -p yi-agent --bin yi-agent default_system_prompt_requires_a_workdir_for_isolation`（`main.rs:3586`）新增断言：系统提示词必须提到 `coding_requires_git` 的询问义务。
工具文案的同族断言在 `yi-agent-subagent/src/lib.rs` 的 `spawn_agent_schema_pins_the_wire_contract`（`spawn_agent_description` 的精确相等要改成 `starts_with`，否则改文案即挂）。

## 风险

1. **纯提示词的执行强度**：模型可能忽略 `ask_user` 反复重试。缓解是错误 JSON 自带话术 + 系统提示词规则；无法技术强制。这是本方案的结构性代价。
2. **`checkpoint_json` 的 kind 约定**：新增 `read_only_checkpoint` 后，恢复流程要能识别它。现有恢复只在 `recovery_gated` 时使用 checkpoint，read-only 子任务不会进入该状态；`validate_recovery_context` 对未知 kind 保持拒绝，避免放宽过头。
3. **前置检查与实际运行目录的偏差**：编码 "coding 无 workdir 时用父任务记住的位置" 依赖父任务已启动过（`remembered_task_position`）。父未启动时前置检查跳过，由准入兜底为 `worktree lease is missing`（仍是 `Validation`，但文案不如结构化错误友好）。这个窗口在应用级 root 场景下不会出现（root 先跑才会 spawn），可接受。
4. **两道 git 判据的成本**：每次 coding spawn 多一次 `rev-parse` 子进程，可忽略。
