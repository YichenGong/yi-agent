# 子 agent 非 Git 环境降级设计

## 状态

设计草案，等待用户 review。

## 背景与问题

启动子 agent 时，只要父 agent 显式传入 `workdir`，运行时就会强制要求该目录位于 git worktree 内，否则 spawn 直接失败。用户确认的取舍是：**静默自动降级 + 标注风险**——没有 git 也能用，但要让用户/父 agent 知道"这次改动没有版本控制保护"。

现状证据：

1. **硬门槛在 `register_workdir`**（`yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:456-498`）：
   - 目录不存在 → `"workdir does not exist: ... — create it with git worktree add first"`
   - `git rev-parse --show-toplevel` 失败 → `"workdir is not inside a git worktree"`
2. **调用点在 spawn 路径上**（`crates/yi-agent-store/src/runtime.rs:1626-1638`）：`spawn_agent` 带 `workdir` 即调 `observe_workdir`，失败即让整个 spawn 失败（注释明确写着"A bad path fails the spawn rather than deferring to worker start"）。
3. **根会话不需要 git**：`read_only_workspace` / `in_place_workspace`（`lib.rs:530-570`）只返回路径，不做 git 检查。
4. **不给 workdir 的 coding 子 agent 本来就能在非 git 目录跑**：`RuntimeCoordinator` 的 `workspace` 解析（`crates/yi-agent-store/src/runtime.rs:2123-2127`）在无 `spawn_workdir` 时走 `in_place_workspace`，其中没有任何 git 校验。也就是说**当前的不一致是"显式 workdir 反而更严格"**，本设计消除的正是这个不一致，而不是新开一条危险路径。
5. **交付校验依赖 git**：`inspect_delivery_report`（`lib.rs:500-528`）调 `WorktreeService::inspect_workdir`（`crates/yi-agent-tools/src/worktree.rs:67-90`），后者基于 `git rev-parse HEAD` + `git status --porcelain`。非 git 下必然返回 `Git workspace error`，随后在 `lib.rs:1079-1085` 变成 `report_failure`，子任务以 failed 收尾。
6. **交付结构要求 commit**：`DeliveryReport`（`crates/yi-agent-core/src/subagent/task.rs:245-255`）的 `validate_for`（`:285-298`）要求 `commit`、`base_ref`、`evidence` 非空，且 workspace 匹配。非 git 无法构造出合法交付，因此"降级"必须同时改交付结构。
7. **重连匹配依赖 git**：`project_workspace_matches`（`lib.rs:693-700`）用 `git rev-parse --show-toplevel` 比较 `repository_root`；非 git 下恒为 false。
8. **既有测试把旧行为当契约**：`crates/yi-agent-store/tests/runtime_coordinator.rs:3495-3560` 的 `an_autonomous_session_refuses_a_workdir_that_is_not_a_git_worktree` 断言非 git workdir 必须被拒。本设计要改掉它。
9. **模型侧的文案在要求 git**：`spawn_agent` 的 `workdir` 描述（`lib.rs:1957-1975`）写着"Required for 'coding': create it yourself with `git worktree add ...`"；系统提示（`crates/yi-agent-core/src/agent.rs:219-228`）同款口吻。
10. **终态收尾不能用 `CompletedWithoutDelivery`**：`WorkerEvent::CompletedWithoutDelivery` 在 supervisor 里被映射为 `WorkerFailed`（`crates/yi-agent-core/src/subagent/supervisor.rs:968-986`）。非 git 的收尾必须走 `WorkerEvent::Delivered`，否则任务会显示 failed。

关键约束：`WorkerWorkspace`（`crates/yi-agent-core/src/subagent/worker.rs:22-30`）会随 `IpcResponse::ApplicationRootAttached`（`crates/yi-agent-store/src/ipc.rs:476`）跨 IPC 序列化，**不能**用"字段全空"来推断非 git —— `read_only_workspace` / `in_place_workspace` 在 git 仓库里同样是 `branch`、`base_commit` 全空。

## 目标

1. `workdir` 不在 git 仓库内时不再让 spawn 失败：按"不受版本控制保护的就地目录"继续执行。
2. 降级是静默的（不弹窗、不要求用户确认），但风险必须在三个位置可见：spawn 回执、任务列表/子 agent 卡片、交付报告。
3. 非 git 的 coding 子任务能正常以 completed 收尾，且带 `known_limitations`。
4. git 环境下的现有行为逐字不变（worktree 语义、dirty 重试提示、交付 commit 校验）。
5. 交付报告显式区分"git 交付"与"未受版本控制保护"两种形态，不靠空字段猜。

## 非目标

- 不为非 git 项目自动 `git init`（用户没要求，且会污染用户目录）。
- 不实现基于文件系统快照的回滚/撤销。
- 不改变 worktree 的创建与回收（`worktree_reclaim.rs` 只服务带 `driver_ref` 的看板会话，普通会话恒 `None`，不受影响）。
- 不改变 read-only 子 agent 的 workspace 语义。

## 设计

### 1. 显式的版本控制标记

`WorkerWorkspace` 增加一个字段（`crates/yi-agent-core/src/subagent/worker.rs:22-30`）：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceVcs {
    #[default]
    Git,
    None,
}

pub struct WorkerWorkspace {
    // ... 既有字段不变
    /// 该位置是否有版本控制保护。`None` 表示就地目录、无法提交/比较。
    /// `#[serde(default)]` 让旧口径的报文（只可能是 git）仍然解码为 `Git`。
    #[serde(default)]
    pub vcs: WorkspaceVcs,
}
```

- 默认值必须是 `Git`，不能是 `None`：所有既有构造点都是 git 语义，且跨 IPC 解码的旧帧只可能来自 git 环境。用 `#[derive(Default)]` 把默认变体定成 `Git`。
- `register_workdir` 改为：
  1. 目录不存在 → 仍然报错（这是真实的用户错误，不能降级）。
  2. `git rev-parse --show-toplevel` 成功 → 现有行为，`vcs: Git`，`repository_root` 用 toplevel。
  3. 失败 → `vcs: None`，`repository_root = canonical`，`branch` / `parent_branch` / `base_commit` 留空。**不再返回错误**。
- 约 53 处 `WorkerWorkspace { .. }` 构造点需要补字段。生产代码只有 6 处（`lib.rs:485,546,565`、`runtime.rs:260,273`、`thread_root.rs:283`、`worker.rs:239` 是测试），其余是测试；编译错误会逐个指出，属于机械改动。

### 2. 交付形态

`DeliveryReport`（`task.rs:245-255`）增加：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    #[default]
    Git,
    /// 未受版本控制保护的就地交付：没有 commit、没有 base ref，
    /// 只有一份可核对的改动清单。
    Unverified,
}

pub struct DeliveryReport {
    // ... 既有字段
    #[serde(default)]
    pub kind: DeliveryKind,
}
```

- `#[serde(default)]` 保证存量 `delivery_json`（`legacy_delivery_json_still_deserializes_without_optional_fields` 已覆盖这类兼容）仍然解码为 `Git`。
- `validate_for` 按 `kind` 分支：
  - `Git`：现有四条校验一条不改。
  - `Unverified`：要求 `evidence` 非空、`workspace` 与任务一致；**不**要求 `commit` / `base_ref`。
- 构造：`DeliveryReport::unverified(workspace, evidence)`。`commit` / `base_ref` 保持空串，序列化时不做特殊处理（下游必须按 `kind` 分支，见下）。
- `evidence` 内容：`{"kind":"unverified_delivery","reason":"no_git_repository","path":"<canonical>"}`，并在 `known_limitations` 追加固定文案：
  「该目录不是 Git 仓库：改动未经版本控制保护，无法生成 commit，也无法用 diff 审阅；如需回滚请手动备份。」（`known_limitations` 已随 `DeliveryReport` 序列化，`inspect_agent` 会原样返回。）

非 git 的改动清单：`changed_files` 尽力填充，口径写死为：

- 从 `path` 递归列举常规文件，跳过 `.yi-agent/` 目录与符号链接（不跟随，避免环）；
- 最多列举 1000 条；超过时 `changed_files` 留空，`evidence` 里记 `{"truncated": true, "count": ">1000"}`，并在 `known_limitations` 追加「改动文件过多，未逐一列举」；
- **不**计算内容哈希、**不**区分"改动前就存在"与"本任务新增"——非 git 环境下没有基线可比。这一点必须写进 `known_limitations`：「无法区分本次改动与此前已存在的文件」。

### 3. 运行循环的分支

`crates/yi-agent-subagent/src/lib.rs:1040-1085` 的 coding 收尾逻辑按 `workspace_for_delivery.vcs` 分支：

- `Git`：现有逻辑不动（`inspect_delivery` → 成功则 `report_delivery`；dirty 则重试提示 commit；无提交则 `report_completed`）。
- `None`：先做一次有界改动扫描，然后 `report_delivery(DeliveryReport::unverified(...))`。**绝不**进入"请执行 git commit"的重试分支；也绝不用 `report_completed_without_delivery()`（会变成 failed）。
- 若扫描本身失败（权限、路径消失），退化为 `DeliveryReport::unverified` + `evidence` 里写明扫描失败原因，仍然以 completed 收尾——用户明确要求"不影响正常使用"。

`inspect_delivery_report`（`lib.rs:500-528`）：

```rust
match workspace.vcs {
    WorkspaceVcs::Git => { /* 现有 inspect_workdir 路径 */ }
    WorkspaceVcs::None => Ok(DeliveryReport::unverified(workspace.lease_id.clone(), evidence)),
}
```

`is_dirty_delivery_error` / `is_empty_delivery_error`（`lib.rs:1195-1203`）只在 `Git` 分支被引用，不需要改。

### 4. 重连匹配

`project_workspace_matches`（`lib.rs:693-700`）改为：

```rust
fn project_workspace_matches(&self, workspace: &Path, recorded: &WorkerWorkspace) -> bool {
    match recorded.vcs {
        WorkspaceVcs::Git => git_output(workspace, &["rev-parse", "--show-toplevel"])
            .is_some_and(|root| recorded.repository_root == Path::new(&root)),
        WorkspaceVcs::None => {
            let canonical = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
            recorded.repository_root == canonical
        }
    }
}
```

### 5. 风险标注的可见位置

三处，缺一不可：

1. **spawn 回执**（给父 agent / LLM 看）：`IpcResponse::TaskSpawned` 增加 `#[serde(default)] unverified: bool`。`RuntimeCoordinator` 在 `observe_workdir` 返回后即可知道（`workspace.vcs == None`），但**当前接口只回 `TaskId`**（`spawn_child_with_objective` 与 `spawn_child_and_admit`，`crates/yi-agent-store/src/runtime.rs:1492,1660`；`spawn_application_child` 同形），所以要把返回值扩成一个小结构：

   ```rust
   pub struct SpawnedChild {
       pub task_id: TaskId,
       /// 位置不在 git 仓库内：改动没有版本控制保护。
       pub unverified: bool,
   }
   ```

   三处 `IpcRequest` 分支（`ipc.rs:3460-3535`）从 `task_id` 改为 `spawned.task_id` 并填 `unverified`。`spawn_outcome`（`lib.rs:1856-1881`）把它写进工具结果的 JSON：`{"task_id":..., "status":"queued", "forked":..., "unverified": true}`，并在 `unverified` 为真时追加一句 `note`：「该目录不是 Git 仓库，子任务的改动没有版本控制保护。」
   - 代价：约 20 处测试的 `IpcResponse::TaskSpawned { task_id }` 模式需要改成 `{ task_id, .. }`；`spawn_child_and_admit` 的调用方（`ipc.rs` 两处、`tests/` 若干）需要解构新结构。
2. **任务列表 / 子 agent 卡片**（给用户看）：`tasks` 表新增列 `workspace_vcs TEXT`（迁移风格与 `repository.rs:4061` 的 `ALTER TABLE tasks ADD COLUMN workspace_mode ...` 一致），插入任务时从 workspace 写入。`IpcTaskSummary` 与协议 `AgentChild`（`crates/yi-agent-app-server/src/protocol.rs:571-583`）增加可选字段 `workspace_vcs?: "git" | "none"`；`desktop/src/lib/generated/AgentChild.ts` 由 ts-rs 重新生成；`SubagentRail` 在 `workspace_vcs === "none"` 时显示一个小徽章「未受版本控制保护」。`desktop/src/lib/subagents.ts` 的 `SubagentRow` 同步加字段。
3. **交付报告**（给父 agent 与 UI 的 diff 页）：`DeliveryReport.kind` + `known_limitations` 已足够；`RuntimeCoordinator::delivery_diff`（`crates/yi-agent-store/src/runtime.rs:2561-2610`）在 `kind == Unverified` 时直接 `Ok(None)`（现有 `commit.is_empty()` 判断已经会返回 None，但显式分支让意图可读）。

### 6. 文案与提示

- `spawn_agent` 的 `workdir` 描述改为：「The directory the child works in. For a git repository, create a worktree yourself (`git worktree add`) so the change is isolated. A directory outside git is allowed, but the child's change is unversioned and cannot be reviewed as a diff.」
- `crates/yi-agent-core/src/agent.rs:219-228` 的系统提示同步改为"在 git 仓库内优先使用 worktree；非 git 目录允许就地工作，但要在最终报告里说明改动未经版本控制保护"。
- 不改 `spawn_agent` 的 `mode` 描述（`coding` / `read_only` 语义不变）。

### 7. diff 面板

`git_diff.rs` 在非 git 下 `session_anchor` 返回 `None`、各 `git_ok` 全失败，`resolve_base` 落到 `worktree-only`，最终产出空 diff（`crates/yi-agent-app-server/src/git_diff.rs:186-217`）。这是既有行为，**不新增降级代码**，但前端在 `workspace` 非 git 时应对空 diff 给出说明文案而不是空白。该文案复用第 5 节第 2 点的 `workspace_vcs` 字段即可。

## 错误处理与边界

| 场景 | 表现 |
| --- | --- |
| `workdir` 不存在 | 仍然 spawn 失败（用户错误） |
| `workdir` 在 git 仓库内 | 与今天逐字一致，`vcs: Git` |
| `workdir` 不是 git | spawn 成功，`vcs: None`，回执 `unverified: true` |
| 非 git + coding 完成 | `Completed` + `Unverified` 交付 + `known_limitations` |
| 非 git + 改动扫描失败 | 仍 `Completed`，`evidence` 记失败原因 |
| 非 git + 重启后匹配位置 | canonical 路径比较 |
| git + dirty worktree | 现有重试提示 commit 的行为不变 |
| 非 git + 要求 diff | `delivery_diff` 返回 `None`；前端按 `workspace_vcs` 显示说明 |

风险（必须写进 spec，因为这是用户接受的取舍）：

- **无隔离**：非 git 下子 agent 直接改项目目录，与父会话共享位置；出错无法用 `git checkout` 回滚。
- **无审阅**：没有 commit 就无法用 `delivery_diff` 审阅改动，只能看文件清单。
- **并发写**：同一非 git 目录上的多个 coding 子 agent 会互相覆盖。本设计不引入锁（YAGNI），但 `known_limitations` 与 spawn 回执都要说明"改动无版本控制保护"。

## 测试与验收

1. `register_workdir` 对非 git 目录返回 `Ok`、`vcs == None`、`repository_root == canonical`。
2. `register_workdir` 对不存在的目录仍然返回 `Err`。
3. git 目录返回 `vcs == Git` 且 `base_commit == HEAD`（既有测试不改）。
4. `DeliveryReport::unverified(...).validate_for(&task)` 通过；`DeliveryReport::coding("", "main", ...)` 仍然因空 commit 失败。
5. 非 git 端到端（`crates/yi-agent-store/tests/runtime_coordinator.rs`）：把 `an_autonomous_session_refuses_a_workdir_that_is_not_a_git_worktree` 改为 `an_autonomous_session_downgrades_a_non_git_workdir`，断言 spawn 成功、任务 `Completed`、`delivery.kind == Unverified`、`known_limitations` 非空。
6. `project_workspace_matches` 在非 git 下对 canonical 路径返回 true、对其它路径返回 false。
7. `spawn_outcome` 的 JSON 在非 git 时带 `unverified: true` 与 `note`。
8. 前端：`SubagentRail` 对 `workspace_vcs === "none"` 渲染徽章（`SubagentRail.test.tsx`）。
9. 命令：`cargo test -p yi-agent-core --lib subagent`、`cargo test -p yi-agent-subagent --lib`、`cargo test -p yi-agent-store --test runtime_coordinator`、`cargo test -p yi-agent-app-server`；`cd desktop && npx vitest run src/components/SubagentRail.test.tsx src/lib/subagents.test.ts`；`cargo fmt --all`.

## 与其它设计的关系

- 与模型门禁 spec 独立：本设计不碰模型解析。
- 与空状态 spec 独立。
- 若"未配置模型"与"非 git 降级"同时发生，先报模型错误（spawn 在模型解析之后），错误优先级由实现顺序自然保证。