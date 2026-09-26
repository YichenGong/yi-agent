# Subagent 默认只读（按需 worktree）设计

**目标：** 让 subagent 默认不创建 git worktree，只有父 Agent 显式声明
`mode: "coding"` 的 child 才建 worktree；只读 child 直接在应用根目录里以只读
sandbox 运行，走文本结果收口。非 git 目录下降级为只读原地模式。

**状态：** 设计已确认，待转实现计划。

---

## 1. 问题

当前 worktree 是**默认且急切**创建的：`RuntimeCoordinator::start_worker`
（`yi-agent-rs/crates/yi-agent-store/src/runtime.rs:1162-1184`）对每个任务
（root 与 child 一视同仁）都调 `prepare_task_workspace`，于是哪怕一个只读、
只输出文本的子任务也会先 `git worktree add` 出一个隔离目录。

后果：

- **worktree 累积**：N 个 child 就有 N 个 worktree，是
  `docs/bug-list.md` 第 16 行「太多了怎么清理」的根因；上一轮已实现「验收后
  自动回收」，但默认创建本身没变，回收只是下游补救。
- **非 git 目录不可用**：`DaemonWorkspaceService::new`
  （`yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs:170-180`）在
  `git rev-parse --show-toplevel` 失败时静默把 `repository_root` 退化成
  workspace 自身，随后 `git worktree add` 失败，root 任务被标
  `workspace_provision_failed`（`runtime.rs:1162-1184`）。整个 session 不可用。
- **只读 child 被无谓隔离**：只读任务不需要写隔离，也不需要 commit 交付，
  却承担了 worktree 的全部成本。

## 2. 现状关键事实（实现约束）

以下事实决定设计的形状，均已在代码中核实：

- `spawn_agent` 的入参**只有 `task`**（objective 字符串），三个实现
  （core `subagent/supervisor.rs:1664-1671`；daemon
  `subagent_runtime.rs:945-952`、`1040-1047`）一致。**没有任何 per-child
  能力/工具集声明**。
- `WorkerStart`（`yi-agent-core/src/subagent/worker.rs:27-44`）没有工具集或
  能力字段，只有 `workspace: Option<WorkerWorkspace>`。
- child 的工具集是在 factory 里、**晚于** `prepare_task_workspace` 才决定的，
  且来自 factory 的**单一** `SandboxMode`（`subagent_runtime.rs:32-41`，
  默认 `WorkspaceWrite`），所有 child 相同。
- `DelegationContract` / `DelegatedAuthority`
  （`yi-agent-core/src/subagent/contract.rs:49-58`、`141-153`）虽然模型里有
  per-child `tools: BTreeSet<String>`，但**在真实路径里是死代码**：只在
  `contract.rs` 与其测试中出现，无人构造。
- `prepare_task_workspace` 一进来先查 `task_workspace_optional(task)`，命中即
  复用并返回（`runtime.rs:1470-1480`）。**「有没有 worktree」已经持久化**在
  `task_workspaces` 表里。
- `start_with_provider_turn_gate` 在 `request.workspace` 为 `None` 时**直接
  报错** `"worker workspace assignment is required"`
  （`subagent_runtime.rs:415-421`）。因此
  `prepare_task_workspace` 返回 `Ok(None)`（`runtime.rs:1481-1483`）目前**不等于
  「原地运行」，而是等于 worker 启动失败**。本设计因此**保留「每个 worker 都有
  workspace」这一不变量**：只读任务也返回一个 `WorkerWorkspace`（仅 `path`
  有意义），只是不落 `task_workspaces` 行。
- `WorkerStart` 由 supervisor 组装（`supervisor.rs:496`），而 supervisor 不知道
  任何文件系统路径（只有 lease id）；路径是靠 `WorkspaceAssignedFactory`
  （`runtime.rs:154-199`）在 coordinator 侧注入的。**新增任何「路径」类字段都
  要另加跨层搬运**，所以执行根复用 `WorkerWorkspace.path`。
- `attach_application_root` 要求 root **既有 workspace service 又有
  workspace**（`runtime.rs:649-656`、`717-723`），且
  `AttachedApplicationRoot.workspace` 是非可选 `WorkerWorkspace`
  （`runtime.rs:97-102`）。**任何让「只读任务没有 workspace」的设计都会连锁改
  `AttachedApplicationRoot` / IPC / TUI**，故本设计不这么做。
- 完成/交付判定在 `subagent_runtime.rs:549-562`：有 `workspace_service` 走
  `inspect_delivery`，否则走 `report_completed(text)`。这条文本收口分支已存在，
  但当前判据是「factory 是否配了 workspace_service」（session 级），需改为
  per-child 的 mode。
- 基础设施已有：`SandboxMode::ReadOnly`（`yi-agent-tools/src/sandbox.rs:13-18`）、
  `register_builtin_tools_with_sandbox` 按 `allows_writes()` 过滤 `write`/`edit`
  （`yi-agent-tools/src/lib.rs:67`）、per-tool `ToolMetadata.read_only`
  （`yi-agent-core/src/tool.rs:68-75`）。

## 3. 设计

### 3.1 核心模型：per-task 显式模式

引入一个 per-task 的显式模式：

```rust
enum TaskWorkspaceMode {
    Coding,    // 建 worktree，可写，走 commit 交付与 review
    ReadOnly,  // 不建 worktree，原地只读，走文本结果
}
```

- **默认翻转**：child 默认 `ReadOnly`；`spawn_agent` 新增**可选**字段
  `mode: "coding"`，父要写代码时显式声明。root 默认 `Coding`（它是 session
  隔离与合并的载体，`subagent_runtime.rs:213-236`）。
- **一个开关，两个后果**：`mode` 同时决定 (a) 要不要建 worktree 与
  (b) child 的 sandbox（`ReadOnly` → `SandboxMode::ReadOnly`，从而
  `lib.rs:67` 不注册 `write`/`edit`，bash 的写也被 OS sandbox 拦
  `sandbox.rs:91-92`）。两件事本就绑定，用一个字段表达，不引入第二处真相。
- **类型而非裸 bool**：定义成独立 enum，将来若要升级到
  `DelegationContract` 的多维能力模型，替换点集中。
- **只读是强承诺**：只读 child 物理上写不动文件。若它发现必须改代码，**硬失败**
  并把「需要 coding」写进文本结果，由父重新 `spawn_agent` 一个 coding child。
  实现上，只读 task 也不能再 `spawn_agent(mode: "coding")`：`RuntimeCoordinator::spawn_child_with_objective` 会以可读 reason 拒绝，避免产生父（无 worktree）无法集成的 coding 交付。

### 3.2 工作目录与供给流程

只读 child 没有 worktree，但必须有 cwd。做法是**复用现有的 workspace 管道**
（不新增 `WorkerStart` 字段）：

- `WorkerStart` 新增 `workspace_mode: TaskWorkspaceMode`（`worker.rs:27-44`），
  supervisor 组装 `WorkerStart` 时从 per-task map 填入（镜像现有
  `objectives` map 的写法，`supervisor.rs:80`）。
- 执行根目录**复用 `WorkerWorkspace.path`**：现有
  `WorkspaceAssignedFactory`（`runtime.rs:154-199`）本就把整个
  `WorkerWorkspace`（含 `path`）注入 `WorkerStart`，这条管道不用改。
- `prepare_task_workspace`（`runtime.rs:1463-1519`）按 mode 分派：
  - `Coding`：现有逻辑不变（建 worktree、落 `task_workspaces` 行）。
  - `ReadOnly`：**不建 worktree、不落 `task_workspaces` 行**，但返回一个
    `WorkerWorkspace`，其 `path` 指向执行根（沿 `parent_id` 向上找最近的
    coding 祖先的 `workspace.path`；找不到则为应用根目录），
    `branch`/`base_commit` 留空。该值不持久化，每次供给时重新计算。
- factory（`subagent_runtime.rs:409-460`）改为**按 `request.workspace_mode`
  决定**，而不是按「有没有 workspace」：
  - `Coding` → sandbox 用 `WorkspaceWrite`，`worker_tool_registry` 走
    `git_dir_for_worktree`（现状）。
  - `ReadOnly` → sandbox 用 `SandboxMode::ReadOnly`（`lib.rs:67` 不注册
    `write`/`edit`），**跳过 `git_dir_for_worktree`**。
- **交付分支**：`subagent_runtime.rs:549-562` 现按「factory 有没有
  `workspace_service`」决定；改为**按 mode 决定**：`ReadOnly` → 直接
  `report_completed(text)`（`else` 分支已存在，`subagent_runtime.rs:560-562`）；
  `Coding` → 现有 `inspect_delivery` 路径。

一句话：**只读 child = 无 workspace 行 + `WorkerWorkspace.path` 当 cwd +
ReadOnly sandbox + 文本收口**。

**为何不新增 `execution_root` 字段**：`WorkerStart` 由 supervisor 组装
（`supervisor.rs:496`），而 supervisor 不知道任何文件系统路径（只有 lease
id）；新字段需要在 supervisor 里再加一张 per-task 路径表，或改
`start_worker_with_provider_turn_gate` 签名。复用 `WorkerWorkspace.path` 则
零新增搬运。代价：`WorkerWorkspace` 在只读场景下 `branch`/`base_commit` 为空，
语义上应读作「工作位置」而非严格「git worktree」。

### 3.3 非 git 目录降级

- **探测提前并显式化**：在 root session attach 时探测
  `git rev-parse --show-toplevel`。是 git repo → 现状不变；不是 → 该 session
  进入**降级只读原地模式**。
- **root 降级为 `ReadOnly`**：非 git 时 root 的 `prepare_root` 不再让
  `git worktree add` 失败，而是返回一个降级的 `WorkerWorkspace`
  （`path` = 项目目录本身，`branch`/`base_commit` 为空，**不落
  `task_workspaces` 行**），root 的 mode 为 `ReadOnly`。
  **这样 `attach_application_root` 的「必须有 workspace」契约与
  `AttachedApplicationRoot.workspace: WorkerWorkspace`（`runtime.rs:97-102`）
  都无需改动**，IPC/TUI 消费端也不受影响。
  **语义后果**：非 git 下 root 也变成只读、不能改代码——因为 coding 交付无处
  落地。这是诚实的降级。
- **coding child 明确失败**：非 git 下父若 `spawn_agent(mode: "coding")`，
  供给阶段发现无法建 worktree → 任务以可读 reason 失败（如
  `coding_requires_git_repository`，取代笼统的 `workspace_provision_failed`）。
- **不引入「非 git 但可写」中间态**：有 git 才谈 coding，没有就只读。
- **实现方式**：给 `AgentWorkspaceService` 加探针
  `supports_coding(&self) -> bool`（默认 `true`）与
  `prepare_read_only(&self, parent: Option<&WorkerWorkspace>, task_id) -> Result<WorkerWorkspace>`，
  生产实现 `DaemonWorkspaceService` 在 `new` 时用
  `git_output(workspace, ["rev-parse","--show-toplevel"])` 判定并缓存
  `is_git_repository`。`attach_application_root` 据此决定 root 的 mode；
  `prepare_task_workspace` 在 coding 且 `!supports_coding()` 时报
  `coding_requires_git_repository`。探针放在 service（coordinator 在 attach 与
  provisioning 两处都已持有它），无需新增工厂方法。**不改**
  `workspace_service_for_application_root` 的返回类型，也不改
  `AttachedApplicationRoot`。

### 3.4 持久化与恢复

mode 必须**在 spawn 时落库**，因为 `prepare_task_workspace` 要在 workspace 行
存在之前就读到它（只读任务永远不会有 workspace 行，无法反推）。

- **schema**：tasks 表加一列 `workspace_mode TEXT NOT NULL DEFAULT 'coding'`，
  取值 `'coding' | 'read_only'`。一次迁移。**默认值取 `'coding'`** 而非
  `'read_only'`：迁移会把既有行一并回填，而既有 session 都持有 worktree、必须
  保持 coding 行为；「默认只读」在 **spawn 层**落实（spawn 工具默认
  `read_only`，且每个 child 插入都显式指定 mode），不依赖 DDL 默认。
- **写入**：`spawn_with_objective`（`supervisor.rs:916-958`）接收 mode 存入
  supervisor 的 per-task map（镜像 `objectives`），`spawn_child_with_objective`
  随任务落库。
- **读取**：`prepare_task_workspace` 从任务记录读 mode 决定分派。
- **恢复**：`recovered_tasks()` 带上 mode → `AgentSupervisor::from_recovered_root`
  / `insert_recovered_child`（`runtime.rs:400-410`）恢复它 → 组装 `WorkerStart`
  （`supervisor.rs:496`）时带上 mode。重启后只读仍是只读、coding 仍是 coding。
- **与 workspace 行的一致性**：coding 任务恢复时优先复用已有 workspace 行
  （`runtime.rs:1470-1480` 现状不变）；只读任务没有行，也不会凭空建。

## 4. 备选方案与否决理由

- **选项 2（接上 `DelegationContract` 多维能力模型）**：语义最完整（per-child
  `tools`、`PathScope`、预算、交付策略），但它是死代码，要接就得补整条 spawn
  链路的契约构造 + 持久化 + 各消费方，是独立大项目，且多维能力的消费方目前
  不存在。**否决**，但本设计把开关定义成独立 enum，为将来升级留好接缝。
- **运行中自动升级只读 child 为 coding**：对使用者最顺滑，但要在任务运行中改
  workspace/工具集，破坏「供给在启动前一次定死」的模型，易出竞态。**否决**。
- **父对「可能写」的一律声明 coding**：把判断全推给 LLM，容易退化回「默认建
  worktree」，本次改动白做。**否决**。
- **只读 child 用真实项目目录（`.worktrees` 的父目录）当 cwd**：读的是干净基线，
  看不到父的未提交改动。**否决**，改为继承最近祖先的 worktree 路径，让只读
  child 看到父当前的视图。
- **mode 只放内存、恢复时从 workspace 行反推**：零迁移，覆盖绝大多数场景，但
  「spawn 后未启动就重启的 coding 任务」会漂移成只读。**否决**，改为落库。
- **新增 `WorkerStart.execution_root: PathBuf` 字段**：语义最干净（执行根 ≠
  worktree），但 `WorkerStart` 由 supervisor 组装、supervisor 不知路径，需要
  再加一张 per-task 路径表或改 `start_worker_with_provider_turn_gate` 签名；
  并且让只读任务「没有 workspace」会连锁改
  `AttachedApplicationRoot` / IPC / TUI。**否决**，改为复用
  `WorkerWorkspace.path`（见 3.2）。

## 5. 范围边界

**做：**
- child 默认只读、`spawn_agent(mode: "coding")` 显式声明才建 worktree
- `TaskWorkspaceMode` 类型、`WorkerStart.workspace_mode`、per-child sandbox
- mode 落库与恢复语义
- 非 git session 降级为只读原地模式

**不做：**
- `DelegationContract` / `DelegatedAuthority` 的落地（留接缝，不实现）
- 只读 child 的运行中升级
- 「非 git 但可写」的中间态
- worktree 池化/复用

## 6. 验证

按 crate 跑（遵守 `CLAUDE.md`：跑前 `ps aux | grep cargo`，避免 workspace 全量）：

- **core**：`spawn_agent` schema 含可选 `mode`；`TaskWorkspaceMode` 默认值与
  解析；`WorkerStart.workspace_mode` 装配。
  命令：`cargo test -p yi-agent-core --test subagent_supervisor`
- **store**：只读 child 不建 worktree、不落 `task_workspaces` 行、
  `WorkerWorkspace.path` 指向最近 coding 祖先；coding child 行为不变；mode 落库
  与恢复后一致；非 git session → root 降级只读、coding child 以明确 reason 失败。
  命令：`cargo test -p yi-agent-store --test runtime_coordinator`
- **app**：factory 按 `request.workspace_mode` 分支——只读时用
  `WorkerWorkspace.path` 当 cwd、注册只读 registry（无 `write`/`edit`）、走文本
  收口而非 delivery。
  命令：`cargo test -p yi-agent --bin yi-agent`
- **回归**：`subagent-worktree-recycling` 的 9 个任务测试全绿（回收只对 coding
  任务生效）。

文档同步（同一 commit 内）：`docs/project-management/subagent-runtime.md` 新增
条目（默认只读、coding 显式声明、非 git 降级），带代码位置与验证命令；
`docs/project-management/README.md` 计数同步；`docs/bug-list.md` 第 16 行补充
「默认只读后 coding worktree 数量大幅下降」，并新增/关闭「非 git 目录」条目。
