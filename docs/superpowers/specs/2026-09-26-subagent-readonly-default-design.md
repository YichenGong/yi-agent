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
  「原地运行」，而是等于 worker 启动失败**。
- 完成/交付判定在 `subagent_runtime.rs:549-562`：有 `workspace_service` 走
  `inspect_delivery`，否则走 `report_completed(text)`。这条文本收口分支已存在。
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

### 3.2 工作目录与供给流程

只读 child 没有 worktree，但必须有 cwd。做法是**把「执行根目录」与
「workspace 分配」分开**：

- `WorkerStart` 新增**总是有值**的 `execution_root: PathBuf`
  （`worker.rs:27-44`），作为工具注册与 bash 的 cwd 来源。
- `prepare_task_workspace`（`runtime.rs:1463-1519`）按 mode 分派：
  - `Coding`：现有逻辑不变（建 worktree、落 `task_workspaces` 行、
    `workspace = Some`），`execution_root = workspace.path`。
  - `ReadOnly`：**不建 worktree、不落行**，`workspace = None`，
    `execution_root =` 最近祖先的 `workspace.path`（沿 `parent_id` 向上找最近
    的 coding 祖先；root 是 coding，故总能找到）。
- factory（`subagent_runtime.rs:409-460`）改为：
  - `workspace = Some` → 用 `workspace.path` 当 root、sandbox 用
    `WorkspaceWrite`（现状）。
  - `workspace = None` → 用 `execution_root` 当 root、sandbox 用
    `SandboxMode::ReadOnly`，**跳过 `git_dir_for_worktree`**（非 git 时它本来
    就返回 `None`，`subagent_runtime.rs:631`）。
- **交付分支**：`subagent_runtime.rs:549-562` 现按「factory 有没有
  `workspace_service`」决定；改为**按 mode 决定**：`ReadOnly` → 直接
  `report_completed(text)`（`else` 分支已存在，`subagent_runtime.rs:560-562`）；
  `Coding` → 现有 `inspect_delivery` 路径。

一句话：**只读 child = 无 workspace 行 + 有 execution_root + ReadOnly sandbox
+ 文本收口**。

### 3.3 非 git 目录降级

- **探测提前并显式化**：在 root session attach 时探测
  `git rev-parse --show-toplevel`（即 `workspace_service_for_application_root`，
  `subagent_runtime.rs:355-362`）。是 git repo → 现状不变；不是 → 返回 `None`
  （表示本 session 无 workspace service）。
- **`None` 时 root 自动降级为 `ReadOnly`**：`prepare_task_workspace` 因拿不到
  service 而返回 `None`（`runtime.rs:1481-1483` 已有此分支），root 的
  `execution_root` 就是 attach 时传入的真实项目目录，root 的 sandbox 也降为
  `ReadOnly`。**语义后果**：非 git 下 root 也变成只读、不能改代码——因为 coding
  交付无处落地。这是诚实的降级。
- **coding child 明确失败**：非 git 下父若 `spawn_agent(mode: "coding")`，供给
  阶段发现没有 service → 任务以可读 reason 失败（如
  `coding_requires_git_repository`，取代笼统的 `workspace_provision_failed`）。
- **不引入「非 git 但可写」中间态**：有 git 才谈 coding，没有就只读。

### 3.4 持久化与恢复

mode 必须**在 spawn 时落库**，因为 `prepare_task_workspace` 要在 workspace 行
存在之前就读到它（只读任务永远不会有 workspace 行，无法反推）。

- **schema**：tasks 表加一列 `workspace_mode TEXT NOT NULL DEFAULT 'read_only'`，
  取值 `'coding' | 'read_only'`。一次迁移。
- **写入**：`spawn_with_objective`（`supervisor.rs:916-958`）接收 mode，随任务
  落库。
- **读取**：`prepare_task_workspace` 从任务记录读 mode 决定分派。
- **恢复**：`recovered_tasks()` 带上 mode → `AgentSupervisor::from_recovered_root`
  / `insert_recovered_child`（`runtime.rs:400-410`）恢复它 → 重建 `WorkerStart`
  （`runtime.rs:1303`）时带上 mode 与 `execution_root`。重启后只读仍是只读、
  coding 仍是 coding。
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

## 5. 范围边界

**做：**
- child 默认只读、`spawn_agent(mode: "coding")` 显式声明才建 worktree
- `TaskWorkspaceMode` 类型、`WorkerStart.execution_root`、per-child sandbox
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
  解析；`WorkerStart.execution_root` 装配。
  命令：`cargo test -p yi-agent-core --test subagent_supervisor`
- **store**：只读 child 不建 worktree、不落 `task_workspaces` 行、
  `execution_root` 指向最近 coding 祖先；coding child 行为不变；mode 落库与
  恢复后一致；非 git session → root 降级只读、coding child 以明确 reason 失败。
  命令：`cargo test -p yi-agent-store --test runtime_coordinator`
- **app**：factory 在 `workspace = None` 时用 `execution_root` 起 worker、注册
  只读 registry（无 `write`/`edit`）、走文本收口而非 delivery。
  命令：`cargo test -p yi-agent --bin yi-agent`
- **回归**：`subagent-worktree-recycling` 的 9 个任务测试全绿（回收只对 coding
  任务生效）。

文档同步（同一 commit 内）：`docs/project-management/subagent-runtime.md` 新增
条目（默认只读、coding 显式声明、非 git 降级），带代码位置与验证命令；
`docs/project-management/README.md` 计数同步；`docs/bug-list.md` 第 16 行补充
「默认只读后 coding worktree 数量大幅下降」，并新增/关闭「非 git 目录」条目。
