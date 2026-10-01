# Subagent 继承父的有效沙箱（yolo 随 coding 子任务传播）设计

**目标：** 让 `coding` 子任务的 OS 沙箱**跟随父 agent 的有效沙箱**——父线程切到 YOLO
时，之后由它派发的 coding 子任务也以 `DangerFullAccess`（无沙箱）运行；父处于普通态
时，子任务维持今天的 `workspace-write` 行为不变。`read_only` 子任务恒为 `ReadOnly`，
不受影响。

**状态：** 设计已确认，待转实现计划。

**相关文档：**
`docs/plans/2026-09-27-desktop-yolo-mode-design.md`（桌面端按线程 YOLO：权限层 + 沙箱层共享
`YoloSwitch`）、`docs/superpowers/specs/2026-08-09-yolo-sandbox-semantics-design.md`
（CLI yolo 语义：显式/ env sandbox 永不被提权）、
`docs/superpowers/specs/2026-09-26-subagent-readonly-default-design.md`
（child 默认只读、`mode:"coding"` 显式建 worktree、per-child 沙箱）、
`docs/superpowers/specs/2026-09-29-headless-subagent-completion-and-sandbox-least-privilege-design.md`
（coding child 沙箱收窄到 worktree + 最小 git 目录）。

---

## 1. 背景与现状（已核实）

### 1.1 subagent 当前的沙箱是"启动时静态烧死"的

- daemon 侧工厂 `DaemonAgentWorkerFactory` 持有一个**静态**
  `sandbox: SandboxMode` 字段（`yi-agent-subagent/src/lib.rs:47`），默认
  `WorkspaceWrite`（`:69`），由 `with_sandbox(cfg.sandbox, ...)` 注入
  （`yi-agent-subagent/src/attach.rs:104`）。
- worker 建工具时走 `register_builtin_tools_with_sandbox`
  （`yi-agent-subagent/src/lib.rs:117`、`:140`），该函数内部固定用
  `YoloSwitch::new(false)` + `promotable = false`
  （`yi-agent-tools/src/lib.rs:57`），**永久不可提权**。
- 因此 subagent 完全不知道 `yolo` 的存在：在 `yi-agent-store`、
  `yi-agent-subagent`、`yi-agent-core/src/subagent` 里 `grep yolo` 为空。

### 1.2 子任务没有权限审批层

worker 构建 child agent 是裸 `Agent::new(provider, worker_tools, config)`
（`yi-agent-subagent/src/lib.rs:602`），整个 subagent crate 内没有
`with_permission` / `PermissionChecker` / `decision_rx`。子任务从不弹审批，
其安全**完全依赖 OS 沙箱**。因此"权限跟随父"落在 subagent 上**唯一能变的量就是沙箱**。

### 1.3 可继承的"父的有效沙箱"在内存中可得

线程的沙箱控制器是 `SandboxController { switch, base, promotable }`
（`yi-agent-tools/src/sandbox.rs`），`effective()` 即
`switch.get() && promotable ? DangerFullAccess : base`。app-server 的
`BuiltAgent` 持有 `yolo: YoloSwitch`（`yi-agent-app-server/src/server.rs:55`），
`thread_cfg` 持有 `sandbox` / `sandbox_promotable`。故在装配委派工具时可造出该线程的
`SandboxController`，用于**调用时**取 `effective()`。

### 1.4 依赖约束

`yi-agent-store` 只依赖 `yi-agent-core`，**不依赖** `yi-agent-tools`
（`yi-agent-store/Cargo.toml`），`core` 也不依赖 `tools`。因此"有效沙箱"跨 store 层
只能以**不透明字符串**传递（如同现有 `mode: Option<String>`），在 `yi-agent-subagent`
（依赖 tools）内解析成 `SandboxMode`。

---

## 2. 关键决策（来自 brainstorming）

| 决策点 | 结论 |
|---|---|
| 生效时机 | **A：spawn 时快照**。child 继承父**当时**的有效沙箱；父之后切换不回溯已起的 child |
| 继承什么 | **A1：镜像父的有效沙箱模式**（自动覆盖 pin 场景），而非只看 yolo 布尔 |
| read-only base 冲突 | **B1：给下限**。coding child = `clamp(继承值, 下限 WorkspaceWrite)`，保住"可提交" |
| 后代 | **D1：整棵子树继承**，规则统一为"子继承其直接父任务的有效沙箱" |
| read_only child | **恒 `ReadOnly`**，忽略继承值，不受 yolo 影响 |
| 可写根 | 只跟随**模式**；child 的可写根仍是它自己的（worktree + 最小 git 目录） |
| 模型可见参数 | **不变**。继承是自动的，`spawn_agent` schema 不新增参数 |

---

## 3. 设计

### 3.1 语义规则（单一事实）

> 每个任务的**有效沙箱**由其 spawn 时**继承到的值**按写模式收敛：
> - `read_only` child → 恒 `ReadOnly`（忽略继承值）。
> - `coding` child → `clamp(继承值, 下限 WorkspaceWrite)`：
>   `ReadOnly` → `WorkspaceWrite`，`WorkspaceWrite` / `DangerFullAccess` 照搬。
>
> **继承值来源 = 直接父任务在 spawn 那一刻的有效沙箱。** 对线程（root）而言是
> `SandboxController::effective()`；对 subagent 而言是它自己已解析出的有效沙箱。

推论：父普通态 → 继承 `workspace-write` → coding child `workspace-write`
（**与今天逐字节一致，零回归**）；父 YOLO 且可提权 → 继承 `danger-full-access` →
coding child 无沙箱；父被 pin 沙箱 → 继承那个 pinned 模式（YOLO 也不越过 pin）。

### 3.2 组件与落点

**yi-agent-core**（`subagent/task.rs`，与 `ChildWriteMode` 并列）
新增枚举，供 store 层引用（store 只依赖 core）：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InheritedSandbox { ReadOnly, WorkspaceWrite, DangerFullAccess }

impl InheritedSandbox {
    pub fn as_str(&self) -> &'static str;          // "read-only" / "workspace-write" / "danger-full-access"
    pub fn parse(value: &str) -> Option<Self>;
}
```

`WorkerStart`（`subagent/worker.rs:66`，`workspace_mode` 字段在 `:75`）新增
`inherited_sandbox: Option<InheritedSandbox>` + `with_inherited_sandbox()` 构造器；
`Default`（`:137`）取 `None`。

**yi-agent-core**（`subagent/supervisor.rs`）
仿现有 `objectives` / `workspace_modes` map（`:88-89`），新增
`inherited_sandboxes: HashMap<TaskId, InheritedSandbox>`；`spawn_with_objective`
（`:239`/`:287`）写入；新增 `set_inherited_sandbox` / `inherited_sandbox` 读写；
组装 `WorkerStart` 时填入。

**yi-agent-store**
- IPC：`SpawnChild`（`ipc.rs:179`）与 `SpawnApplicationChild`（`ipc.rs:190`）各加
  `#[serde(default)] sandbox: Option<String>`；handler（`:2511` / `:2538`）用
  `InheritedSandbox::parse` 校验，非法 → `invalid_params`（对齐 `parse_workspace_mode`
  的 `:2994` 写法）。
- 持久化：tasks 表加列 `inherited_sandbox TEXT`（可空）。仿 `workspace_mode`
  的迁移与读写（`repository.rs:359`/`:905`/`:997`/`:3272`/`:3382`）。
- 协调器：`spawn_child_and_admit` 系列（`runtime.rs:656`/`:1027`/`:1158`/`:1260`/
  `:1690`）增加 `inherited_sandbox` 参数并落库；读取侧（`:1315` 一带）在组装
  `WorkerStart`（`:1463`）时填回；恢复路径（`recovered_tasks` →
  `from_recovered_root` / `insert_recovered_child`，`:400-410`）同步恢复。

**yi-agent-subagent**（解析 + clamp 的唯一落点）
- `worker_tool_registry(workspace, workspace_mode, inherited)`
  （`lib.rs:126`）：`ReadOnly` → `SandboxMode::ReadOnly`（不变）；`Coding` →
  `SandboxMode::from(clamp(inherited.unwrap_or(cfg.sandbox), ≥WorkspaceWrite))`。
- 两个 spawn 工具各携带"父的有效沙箱"：
  - `DaemonApplicationSpawnAgentTool`（`lib.rs:1213`，`impl Tool` 在 `:1259`，
    由 `register_application_subagent_tools` `:1111` 构造，桌面端与 TUI 共用）新增字段
    `controller: SandboxController`，`call()`（`:1260` 一带）内实时取
    `controller.effective()` 作为 `sandbox` 发出（仅当 `mode == "coding"`）。
  - `DaemonSpawnAgentTool`（`lib.rs:1207`，worker 内注册处 `:556`，`impl Tool` 在
    `:1589`）新增字段 `sandbox: SandboxMode`（= 该 worker 已解析的有效沙箱），
    `call()` 时作为 `sandbox` 发出（仅当 `mode == "coding"`），实现 D1 逐层传递。

**yi-agent-runtime / app-server**
`build_runtime_tooling`（`server.rs:183-201`）用
`SandboxController::new(built.yolo.clone(), thread_cfg.sandbox, thread_cfg.sandbox_promotable)`
造控制器，传给 `register_attached_root_tools`（`server.rs:196`）。TUI 侧
`register_attached_root_tools` 调用点（`yi-agent/src/main.rs:616`、`:721`）同理用启动时
`cfg.yolo`。

### 3.3 数据流

```
线程切 YOLO（即时）→ 模型调 spawn_agent(mode:"coding")
  → DaemonApplicationSpawnAgentTool.call(): controller.effective() = DangerFullAccess
  → IPC SpawnApplicationChild{ sandbox: Some("danger-full-access") }
  → coordinator 落库 tasks.inherited_sandbox = "danger-full-access"
  → supervisor 组装 WorkerStart{ workspace_mode: Coding,
                                  inherited_sandbox: Some(DangerFullAccess) }
  → factory: clamp(DangerFullAccess, ≥WorkspaceWrite) = DangerFullAccess
  → coding child 工具集走裸 sh（无沙箱）

孙子（D1）: 该 child 的 DaemonSpawnAgentTool 带着 DangerFullAccess
  → 再次 spawn 时同法下传，逐层继承
```

### 3.4 错误处理

| 情况 | 处理 |
|---|---|
| IPC `sandbox` 非法字符串 | `invalid_params` |
| 旧客户端不发该字段 / 旧 DB 行 | 缺省 `None` → factory 回退 `cfg.sandbox`（= 今天行为） |
| coding child 继承到 `read-only` | clamp 抬到 `workspace-write`（B1），不报错 |
| `read_only` child | 恒 `ReadOnly`，忽略继承值 |
| `read_only` child 试图 `spawn_agent(mode:"coding")` | 沿用既有拒绝（readonly-default 设计），不因本改动放开 |
| 线程无 attached runtime（委派不可用） | 无 spawn 工具，不涉及 |

### 3.5 持久化与恢复

- 落库时机：与 `workspace_mode` 同一 INSERT（`repository.rs:905`/`:997`），保证
  `prepare_task_workspace` 之前即可读到。
- 迁移：新增列可空；既有行回填 `NULL`，读取端把 `NULL` 视为"未知 → 用 `cfg.sandbox`"，
  **不改变旧 session 行为**。
- 恢复：重启后 coding/read_only 与继承沙箱一并恢复，与既有 mode 恢复同源。

---

## 4. 备选方案与否决理由

- **方案 2：daemon/root 会话级可变 YOLO 开关（运行期设置）**。等于 Q1=B，需新增跨进程
  可变状态与 IPC，且 TUI/headless 无运行期开关会行为分叉。**否决**。
- **方案 3：落地 `DelegationContract` / `DelegatedAuthority`**。最通用的 per-child 能力
  模型，但是死代码，需补整条链路，是独立大项目，且被 readonly-default 设计明确推迟。
  **否决**，本设计保留其接缝（独立枚举）。
- **A2（只看 yolo 布尔）**：会绕过用户显式 pin 的沙箱，语义不一致。**否决**，取 A1。
- **B2（照字面镜像 read-only）**：coding child 会写不动 worktree、无法交付，任务静默失败。
  **否决**，取 B1。
- **D2（只第一层跟随）**：同一子树内权限深浅不一，难解释。**否决**，取 D1。

---

## 5. 范围边界

**做：**
- `InheritedSandbox` 类型、IPC 字段、tasks 列、supervisor per-task map
- spawn 时对"父的有效沙箱"取快照并逐层传给 coding child（clamp ≥ WorkspaceWrite）
- read_only child 恒只读
- 桌面端与 TUI 两条装配路径共用 `yi-agent-subagent` 层

**不做：**
- 运行期回溯（Q1=A）；daemon 级会话开关
- `DelegationContract` / `DelegatedAuthority` 落地
- `spawn_agent` schema 新增可见参数（继承自动）
- 只读 child 的运行中升级
- 继承父的 `sandbox_writable_roots`（只跟随模式）

---

## 6. 验证

按 crate 跑（遵守 `CLAUDE.md`：跑前 `ps aux | grep cargo`，避免 workspace 全量）：

- **core**：`InheritedSandbox` 解析/序列化往返；`WorkerStart.inherited_sandbox`
  装配；supervisor map 读写。
  命令：`cargo test -p yi-agent-core --test subagent_supervisor`
- **store**：spawn 落库与恢复一致；IPC 非法值拒绝；缺字段回退；迁移不破坏旧行。
  命令：`cargo test -p yi-agent-store --test runtime_coordinator` 与 `runtime_ipc`
- **subagent**：clamp 三分支（read-only→workspace-write、workspace-write→不变、
  danger→danger）；`read_only` 恒 ReadOnly；两个 spawn 工具发出的 `sandbox` 值正确。
  命令：`cargo test -p yi-agent --bin yi-agent`（`subagent_runtime` 单测）
- **app-server**：YOLO 线程 spawn 出的 coding child 世代沙箱为 `DangerFullAccess`；
  普通线程为 `WorkspaceWrite`。命令：`cargo test -p yi-agent-app-server`
- **e2e（可选，真实 LLM，`#[ignore]`）**：YOLO 线程起 coding child，子进程可写 worktree
  外 / 联网；普通线程则被拒。

## 7. 文档更新（同一 commit）

- `docs/project-management/subagent-runtime.md`：新增条目（coding child 继承父有效沙箱、
  read_only 恒只读、clamp 下限），带代码位置与验证命令。
- `docs/project-management/README.md`：计数同步。
- 本 spec 自身入库。

## 8. 风险与注意事项

- **安全敏感路径**：继承会把 coding child 的最低权限从"worktree 最小集"提升到
  `DangerFullAccess`（父 YOLO 时）。这是**用户显式开启 YOLO**的后果，与父 agent 同权；
  但必须在文档中写明"YOLO 会随 coding 子任务传播"。
- **`promotable` 兜底不可绕过**：显式 / env pin 的沙箱永不被继承成更宽的模式。
- **两份真相风险**：必须保证 factory 用**父快照**而非全局 `cfg.sandbox` 决定 coding child
  沙箱，否则 D1 断裂。
- **store 不得依赖 tools**：跨层只传字符串，解析集中在 `yi-agent-subagent`。
