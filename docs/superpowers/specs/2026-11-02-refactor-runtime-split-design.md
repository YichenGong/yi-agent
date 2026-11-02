# 重构：`yi-agent-store/src/runtime.rs` 拆分

**状态：** 设计（待评审）
**日期：** 2026-11-02
**范围：** 单文件纯重构，分 A / B 两个阶段，**不改任何对外行为**

> 本文档相对第一波设计（`2026-11-02-refactor-wave1-design.md`）的 A2 条目做了**两处纠错**，
> 理由见 §1.3 与 §1.4。A2 原条目不是本文的子集，而是被替换。

---

## 1. 事实基线

所有数字在 `main` @ `b8e98a31` 上逐项实测，测量脚本见 §7。

### 1.1 文件构成

| 项 | 实测 |
|---|---|
| `runtime.rs` 总行数 | **4,421** |
| 内联测试 | **511**（两个模块：L3833–4324 共 491 行、L4326–4346 共 21 行） |
| 生产代码 | **3,910** |
| `impl RuntimeCoordinator` | L326–3394，**3,069 行** |
| 方法数 | **71**（54 `pub` + 17 私有） |
| 方法体合计 | 2,822 行 |
| `impl` 之外的生产代码 | **841 行** |

**测试不穿插在正文中。** 生产区（L1–3832）内 `#[cfg(test)]` 出现次数为 **0**，`mod` 声明为 **0**。
这是相对 `repository.rs` 的一个显著便利：拆分时不存在"测试模块要跟着类型走"的物理障碍，
两个测试模块各自跟着它测的代码走（§2.6）即可。

### 1.2 体量分布

方法体量极度集中，不是均匀摊在 71 个方法上：

| 区间 | 方法数 | 行数占比 |
|---|---|---|
| ≥ 100 行 | **6** | 1,239 行（44%） |
| ≤ 30 行 | 43 | — |
| ≤ 10 行 | **18** | — |

前 6 大方法：

| 方法 | 行数 |
|---|---|
| `start_worker` | 324 |
| `spawn_child_with_objective` | 218 |
| `reconcile_worker_events` | 201 |
| `open` | 179 |
| `attach_application_root` | 170 |
| `ensure_application_root_supervisor` | 147 |

结论：**"文件太长"的实质是 6 个方法太长**，而不是 71 个方法都偏胖。18 个不超过 10 行的方法
基本是薄委托（`create_session` 3 行、`fork_max_bytes` 3 行、`max_direct_children` 3 行）。

### 1.3 纠错一：耦合来自共享字段，不是方法调用

第一波设计的 A2 条目按**方法名前缀**切成 9 组：

```
runtime/fork.rs           begin_fork_upload / append_fork_chunk / ...
runtime/application_root.rs  attach / activate / detach / ...
runtime/spawn.rs          spawn_child* / start_worker / ...
```

这个分法有两处问题，本文替换它。

**问题一：按名字分组得到的是伪边界。** 对 71 个方法建调用图（`self.x(` / `::x(` / `.x(`），
求无向连通分量，结果是 **1 个含 60 个方法的巨块** + 10 个孤立节点。
原因很直接：几乎所有方法都要 `self.repository.lock()`，一条 `repository` 边就把 60 个方法连成一片。
按这个名字分，`spawn` 组会拿走 712 行、`application_root` 组 479 行——问题只是从 1 个文件变成 16 个文件里的 2 个更大文件。

**问题二：名字会骗人。** `spawn_application_child`（27 行）名字像 spawn，实际只是
`attach_application_root` 系列的一环；`start_worker`（324 行）名字像 worker 控制，
实际是 spawn 流程的最后一站。按前缀切会把这两处割到不同文件，制造跨文件往返。

**替代方案：按字段簇分组。** 实测每个方法访问了哪些 `self.` 字段，得到的簇是干净的：

| 字段簇 | 天然归属 |
|---|---|
| `fork_uploads` + `fork_max_bytes` | fork 上传 |
| `resident_tasks/leases/waiting` + `resource_coordinator` | 居留准入 |
| `application_root_workspace_services` + `supervisors` | 应用根 |
| `task_positions` | 位置记忆 |
| `draining` | 停机闸门 |

字段簇的边界之所以可靠：**改一个字段的语义时，需要一起改的方法恰好就是访问它的那些方法。**
这就是"职责"在本模块的真实物理含义。§2.1 的分组表就是按这个原则产出并逐方法校验过的。

### 1.4 纠错二：方法数是 71，不是 69

第一波设计记录为"69 个方法（54 `pub` + 15 私有）"。实测：**71 个 = 54 `pub` + 17 私有**。

核对过程：`awk 'NR>=326 && NR<=3394' | grep -c '^    \(pub \)\?\(async \)\?fn '` 得 71；
其中 `pub` 54、私有 17。`grep -c '^    pub fn \|^    pub async fn '` 全文件得 55，
比 54 多 1 是因为 `impl ProviderTurnAdmissions`（L3571，在 impl 块之外）也贡献了一个。
后出现的 8 空格缩进 `fn` 全在测试模块里，不参与计数。

方法数影响判据的分母（本文所有归组表以 71 为准），但不影响最终目标——目标按行数定，不按方法数定。

---

## 2. A 阶段：纯搬移

**语义约束：只搬不改。** 方法体逐字不动，唯一允许的文本变化是 §2.3 的 10 处可见性关键字。

### 2.1 目标结构（16 个文件）

沿用 `repository` 已确立的模式：`struct` 留在 `mod.rs`，`impl` 分文件，
每个子模块 `use super::*`。这样不需要新约定——Rust 的"私有字段对后代模块可见"规则直接可用。

预算口径：`方法体实测行数` + `随迁的具名项实测行数` + `每文件 ~3 行样板（use super::* / impl / }）`。
下表每一列都是实测，不是估算。

| 文件 | 内容 | 方法体 | 随迁项 | 预算 |
|---|---|---|---|---|
| `runtime/mod.rs` | `RuntimeCoordinator` 结构体、`RuntimeCoordinatorError`、常量、`RuntimeStopOptions/Summary`、`AttachedApplicationRoot`、`mod` 声明 | 0 | 167 | **≈ 210** |
| `runtime/init.rs` | `open` + `with_fork_max_bytes` / `fork_max_bytes` / `max_direct_children` | 194 | 0 | **≈ 200** |
| `runtime/fork.rs` | fork 5 方法 + `ForkUpload` + 两个 fork 常量 + `FORK_MAX_BYTES_ENV` | 156 | 31 | **≈ 200** |
| `runtime/application_root.rs` | attach / ensure_supervisor / authorize / activate / detach / spawn_application_child / workspace_service_for + capability/digest 纯函数 | 479 | 28 | **≈ 510** |
| `runtime/hydration.rs` | 4 个 hydration 辅助 + `RecoveryContext` | 0 | 133 | **≈ 140** |
| `runtime/session.rs` | `create_session` 系列 4 个 + `root_task_id` + `AutonomousSession` | 108 | 6 | **≈ 120** |
| `runtime/spawn.rs` | `spawn_child` / `spawn_child_with_objective` / `spawn_child_and_admit` / `resolve_parent_workdir` / `remember*_task_position` + `resolve_extra_roots` + git 三件套 | 314 | 54 | **≈ 375** |
| `runtime/worker_start.rs` | `start_worker` + `prepare_task_workspace` + `nearest_ancestor_workspace` + `supervisor_workdir_for` + `WorkspaceAssignedFactory` | 427 | 47 | **≈ 480** |
| `runtime/lifecycle.rs` | cancel / pause / resume / retry / draining / graceful_stop / interrupt_* 共 12 个 | 352 | 0 | **≈ 375** |
| `runtime/reconcile.rs` | `reconcile_worker_events` + `persist_cascaded_tasks` + `persist_worker_trace_facts` + `worker_cancellation` + `trace_fact_kind` + 3 个 `*_terminal_json` | 288 | 45 | **≈ 340** |
| `runtime/messaging.rs` | send_* / request_permission / resolve_permission / mailbox_snapshot / delivery_diff / active_thread_children 共 8 个 | 256 | 0 | **≈ 275** |
| `runtime/watchdog.rs` | `evaluate_watchdogs` + `record_watchdog_terminal` | 121 | 0 | **≈ 130** |
| `runtime/children.rs` | 子任务查询与授权 8 个 | 132 | 0 | **≈ 150** |
| `runtime/schedule.rs` | `evaluate_schedules` + `prune_terminal_traces` | 107 | 0 | **≈ 115** |
| `runtime/resident.rs` | 4 个居留方法 | 61 | 0 | **≈ 70** |
| `runtime/provider_turn.rs` | `ProviderTurnAdmissions` 子系统 437 行 + 既有测试 491 行 | 437 | 0 | **928**（生产 440） |

**为什么 `init.rs` 和 `hydration.rs` 必须存在**（自查时发现的两个预算漏洞）：

- `init` 组的 4 个方法必须落在某个文件。全塞进 `mod.rs` 会让它变成 167 + 194 = 361，
  超过"壳文件"的含义（`mod.rs` 应只放声明与类型）。单开 `init.rs` 后 `mod.rs` 保持 210。
  `open` 在 B 阶段还要切成 4 段，放在 `init.rs` 里也有空间。
- `application_root` 组的方法体 479 行，加上它独占使用的 4 个 hydration 辅助（105 行）
  和 capability/digest（28 行）会到 615 行，贴着上限。把 hydration 辅助单列后降到 510。
  `RecoveryContext` 本就与 hydration 同源（`open` 建它、`start_worker`/`resume_task` 读它），
  一并放进 `hydration.rs` 最自然。

**归属校验：71 个方法，无重复，无遗漏。** 分组表逐方法枚举如下（golden table，实施时按此表搬）：

| 组 | 方法（全部） | n | 方法体行数 |
|---|---|---|---|
| spawn | `spawn_child` `spawn_child_with_objective` `spawn_child_and_admit` `resolve_parent_workdir` `remember_task_position` `remembered_task_position` | 6 | 314 |
| application_root | `attach_application_root` `ensure_application_root_supervisor` `authorize_application_root` `activate_application_root` `detach_application_root` `spawn_application_child` `workspace_service_for` | 7 | 479 |
| worker_start | `start_worker` `prepare_task_workspace` `nearest_ancestor_workspace` `supervisor_workdir_for` | 4 | 427 |
| lifecycle | `cancel_task` `cancel_thread_tasks` `cancel_child_authorized` `pause_task` `resume_task` `retry_task` `begin_draining` `ensure_admitting` `graceful_stop` `request_safe_checkpoints` `interrupt_unacknowledged_workers` `active_worker_task_ids` | 12 | 352 |
| reconcile | `reconcile_worker_events` `persist_cascaded_tasks` `persist_worker_trace_facts` `worker_cancellation` | 4 | 288 |
| messaging | `send_message` `send_user_override` `send_application_message` `request_permission` `resolve_permission` `mailbox_snapshot` `delivery_diff` `active_thread_children` | 8 | 256 |
| init | `open` `with_fork_max_bytes` `fork_max_bytes` `max_direct_children` | 4 | 194 |
| fork | `begin_fork_upload` `append_fork_chunk` `abort_fork_upload` `take_fork_messages` `prune_expired_fork_uploads` | 5 | 156 |
| children | `authorize_child_access` `inspect_child_authorized` `wait_for_children` `wait_for_children_authorized` `direct_children` `child_completion_snapshot` `task_state` `supervisor` | 8 | 132 |
| watchdog | `evaluate_watchdogs` `record_watchdog_terminal` | 2 | 121 |
| session | `create_session` `create_session_with_objective` `create_session_with_objective_and_mode` `create_autonomous_session` `root_task_id` | 5 | 108 |
| schedule | `evaluate_schedules` `prune_terminal_traces` | 2 | 107 |
| resident | `release_resident_lease` `persist_resident_cursor` `resident_admission_cursor` `resident_capacity` | 4 | 61 |
| **合计** | | **71** | 2,995 |

方法体合计 2,995，`impl` 块 3,069，差 74 行是方法之间的空行与分隔，搬移时随之分布。

### 2.2 为什么 `provider_turn` 必须独立成文件

`impl` 之外的 841 行生产代码不是均质的，是六个互不相关的关注点：

```
L48-62     15   常量 + AutonomousSession
L70-117    48   RuntimeCoordinatorError（全部错误变体）
L118-148   31   CODING_REQUIRES_GIT_PREFIX + CodingRequiresGitReason
L159-190   32   RuntimeStopOptions / RuntimeStopSummary / AttachedApplicationRoot
L207-234   28   RuntimeCoordinator 结构体
L282-325   44   capability/digest 纯函数 + in_place_workspace_at + recorded_workspace
L3396-3832 437  ProviderTurnAdmissions 子系统（4 个类型 + 2 个 Drop 守卫 + 167 行 impl）
L4347-4374  28   RecoveryContext
L4375-4421  47   coding_git_precondition / git_capture / truncate_diff
```

那 **437 行**的 provider-turn 准入是一个自洽子系统：`ProviderTurnAdmissions`（12 行结构体）、
`impl`（167 行）、`QueuedProviderTurn`（5 行）、`ProviderTurnLeaseHandle`（23 行，含 `Drop`）、
`RuntimeProviderTurnGate`（58 行，实现 core 的 `ProviderTurnGate`）、`ProviderTurnWaitCleanup`（14 行，含 `Drop`），
外加 158 行只被它使用的自由函数。
它只与 `init` 组有接缝（`open` 构造它、`start_worker` 取 gate），内部无对外引用。

留在 `mod.rs` 的话，`mod.rs` 必然超过 900 行——这正是第一波设计判据写 "841 行 ≤ 900"
会失准的原因：它把 511 行测试算进了 shell 内（实际只有 491 行该跟着走），
同时又漏了这 437 行独立子系统。两者的错误方向相反，恰好凑出一个看起来合理的 841。

### 2.3 可见性代价：10 个私有方法必须放宽

私有方法一旦定义在子模块的 `impl` 里，可见范围就是该子模块，跨组调用会被 Rust 拒绝。
实测跨组调用边，**10 个** 私有方法需要改 `pub(super)`：

| 私有方法 | 被几个组跨组调用 |
|---|---|
| `supervisor` | **8** |
| `ensure_admitting` | 5 |
| `release_resident_lease` | 5 |
| `authorize_child_access` | 3 |
| `persist_cascaded_tasks` | 2 |
| `authorize_application_root` | 1 |
| `persist_resident_cursor` | 1 |
| `workspace_service_for` | 1 |
| `supervisor_workdir_for` | 1 |
| `remember_task_position` | 1 |

其余 7 个私有方法（`ensure_application_root_supervisor`、`resolve_parent_workdir`、
`nearest_ancestor_workspace`、`prepare_task_workspace`、`remembered_task_position`、
`persist_worker_trace_facts`、`active_worker_task_ids`）只被同组调用，保持 `fn`。

**允许的文本变化仅限这 10 处 `fn` → `pub(super) fn`。** 函数体、签名、参数、返回类型逐字不动。

### 2.4 对外 API 不变

顶层 `pub` 项只有 **10 个**：

```
DEFAULT_FORK_MAX_BYTES      FORK_UPLOAD_TTL        AutonomousSession
RuntimeCoordinatorError     CODING_REQUIRES_GIT_PREFIX
CodingRequiresGitReason     RuntimeStopOptions     RuntimeStopSummary
AttachedApplicationRoot     RuntimeCoordinator
```

**全部留在 `mod.rs` 原地，不需要任何 `pub use` 重导出。** 外部引用面同样很小，
只有 3 个文件引用 `yi_agent_store::runtime`：

```
crates/yi-agent-store/tests/runtime_coordinator.rs
crates/yi-agent-store/tests/subagent_fork_upload.rs
crates/yi-agent-subagent/src/lib.rs
```

对比第一波 `ipc` 的 25 个重导出 / 436 处跨 crate 引用，这块的耦合面低两个数量级。

### 2.5 A 阶段判据

```
runtime/mod.rs          生产 ≤ 260 行，内联测试 0        (预算 210)
其余 14 个子模块         生产 ≤ 560 行                     (最大 application_root.rs 预算 510)
provider_turn.rs        ≤ 980 行 = 生产 440 + 随迁测试 491（测试不设上限）
总行数                  4,421 → ≤ 4,550                   (见下)
```

**总量上限为什么是 +3% 而不是"守恒 ±1%"。** 纯搬移并不严格守恒：每个新文件要加
`use super::*;` / `impl ... {` / `}` 三行样板，`mod.rs` 还要加 16 行 `mod` 声明。
实测标定：第一波 `repository.rs` 拆成 12 个文件，总行数 5,074 → 5,131（**+57 = +1.1%**）。
本块 16 个文件、文件数更多，按同比例上浮留 3%（+133 行）= **≤ 4,554**。
若实测超过 4,600，说明有东西搬重复了或没搬走，应当排查而不是调数字。

四条硬门禁：

1. **`cargo test -p yi-agent-store` 347 全绿**（基线：lib 112 / integration 235）
2. **测试名集合一致**。注意 `runtime::` 前缀会变：`provider_turn_admission_tests` 从
   `runtime::provider_turn_admission_tests::*` 变成
   `runtime::provider_turn::provider_turn_admission_tests::*`；
   `extra_roots_resolution_tests` 从 `runtime::` 变成 `runtime::spawn::`。
   与第一波 A3 同一个坑，因此门禁**只比对末段函数名**：

   ```bash
   cargo test -p yi-agent-store -- --list 2>/dev/null | grep ': test$' \
     | sed 's/: test$//; s/.*:://' | sort > /tmp/rt-before.txt   # 拆前
   # ... 拆 ...
   cargo test -p yi-agent-store -- --list 2>/dev/null | grep ': test$' \
     | sed 's/: test$//; s/.*:://' | sort > /tmp/rt-after.txt
   diff /tmp/rt-before.txt /tmp/rt-after.txt && wc -l < /tmp/rt-after.txt   # 期望 347
   ```

3. **公共 API 零变化**：`git diff` 中 `^    pub fn` / `^    pub async fn` 签名行删改数为 0
   （仅 `mod.rs` 声明与 `pub(super)` 允许出现）。
4. **函数体未变**：逐方法做归一化文本比对——把 `pub(super) fn` 归一回 `fn`、统一空白后，
   71 个方法体与拆前逐字相等。

### 2.6 测试模块归属（两个都要跟着搬）

```
L3833 mod provider_turn_admission_tests  → runtime/provider_turn.rs
L4326 mod extra_roots_resolution_tests   → runtime/spawn.rs
```

测试行数正确口径：`provider_turn_admission_tests` 是 **491** 行（原始跨度 492，
其中 1 行是紧跟的 `#[cfg(test)]` 属性的边界重叠）；`extra_roots_resolution_tests` 是 21 行（属测试代码）。
两者合计 512，与文件级 `test_lines` 的 511 相差 1，来自边界重叠，不是遗漏。

`extra_roots_resolution_tests` 直接测 `resolve_extra_roots(None, Some(&parent))` 这个自由函数
（代码里注明"直接测解析函数而非整条 daemon 链路"），所以它必须跟这个函数走，
不能跟 `provider_turn_admission_tests` 一起塞进 `provider_turn.rs`。

---

## 3. B 阶段：拆 6 个巨型方法

**前置条件：A 阶段已合并、全绿。** B 在 A 的文件边界上进行，改动范围只落在
`init.rs`（`open`）/ `spawn.rs` / `reconcile.rs` / `worker_start.rs` / `application_root.rs` 五个文件。

### 3.1 目标

每个方法按**方法体内已存在的阶段注释**切，不发明新划分。切完每个都 ≤ 90 行：

| 方法 | 现 | 阶段 | 落点文件 |
|---|---|---|---|
| `start_worker` | 324 | 7 | `worker_start.rs` |
| `spawn_child_with_objective` | 218 | 5 | `spawn.rs` |
| `reconcile_worker_events` | 201 | 5 | `reconcile.rs` |
| `open` | 179 | 4 | `mod.rs` |
| `attach_application_root` | 170 | 4 | `application_root.rs` |
| `ensure_application_root_supervisor` | 147 | 3 | `application_root.rs` |

阶段划分逐条列出（实施时按此切，不得增减）：

**`start_worker`（7 段）**
1. `ensure_admitting()` 闸门
2. 取 supervisor 并加锁
3. 恢复边界判定（`recovery_boundary` + `has_worker` 去重）
4. 工作区分配（`prepare_task_workspace`，失败路径含级联持久化 + 错误转换）
5. 居留准入（`is_subagent` 分支：游标 → 授租约 → 持久化 → 校验）
6. 恢复预检（`preflight_recovery` + 冲突路径的 victims 持久化与租约释放）
7. 组装 `WorkerStart` + 配额准入 + 建 gate + 启动 worker

**`spawn_child_with_objective`（5 段）**
1. 前置校验（`ensure_admitting`、队列容量、父级已结算）
2. workdir 解析 + coding git 预检 + thread_id/extra_roots 解析
3. 取 fork 消息（`take_fork_messages`）
4. supervisor 建任务（`spawn_with_objective` + 三处 seeding）
5. 落库（任务行 + guarded `extra_roots` UPDATE）+ 工作区注册 + 模型设置

**`reconcile_worker_events`（5 段）**
1. 取全部 supervisors 快照
2. 逐会话 drain trace facts
3. 逐会话 reduce + 收集 transitions
4. 收集 deliveries / consumed_overrides / watchdog_updates
5. 持久化 + 父级通知

**`open`（4 段）**
1. 打开 repo + 恢复租约与居留游标
2. 逐 recovered task 建 recovery_contexts
3. 逐任务 hydrate（子任务走 insert，根任务走 from_recovered_*）
4. 资源游标恢复 + 组装 `Self`

**`attach_application_root`（4 段）**
1. 加锁 + `ensure_admitting` + 幂等键校验 + 取 workspace service
2. 既有分支（重连/重激活：校验 project、sync 位置、ensure supervisor、读回 extra_roots）
3. 新建分支（铸 capability → 建 session → 建 supervisor → 建 root 任务）
4. extra_roots 落库 + 返回 `AttachedApplicationRoot`

**`ensure_application_root_supervisor`（3 段）**
1. 已存在则早返回；否则读任务列表
2. 逐任务 hydrate（根 vs 子两条不对称路径）
3. 装入 supervisors

### 3.2 真正的风险：切点两侧的顺序语义

**B 阶段的难点不是"能否切"，而是切错一处就是静默数据损坏，而不是编译错误。**
代码里有 7 处被注释明确标注为不可交换的顺序约束。这 7 条必须逐条进入 plan 的步骤表，
每条都要有对应的验证方式。

| # | 位置 | 约束 | 反了会怎样 |
|---|---|---|---|
| 1 | `spawn` L1556 | 父级已结算检查**必须早于** `take_fork_messages` | token 已消费，调用方重试拿到「未知 token」而非真实原因 |
| 2 | `spawn` L1586 | coding git 预检**必须早于**写任务行 | 留下调用方看不见也清不掉的幽灵 queued 任务 |
| 3 | `spawn` L1646 | `extra_roots`（行 INSERT 后的 guarded UPDATE）与 `inherited_sandbox`（随 INSERT）**故意不对称** | 切段时若"顺手统一"，会改动 INSERT 契约、波及所有调用点 |
| 4 | `reconcile` L2977 | **先 drain trace facts 再 reduce** | 终态/暂停转变移除 worker 时连带丢光缓冲 facts，永久丢失"为什么停" |
| 5 | `reconcile` L2973 | 用 `first_error` 累积，**不得 `?` 提前返回** | 丢弃已收集的其它会话转变，把别人已完成的工作"反完成"并卡住 |
| 6 | `worker_start` L1945/L2008/L2051 | 居留租约：游标持久化失败 → 释放；preflight 冲突 → 释放；准入失败 → 释放。三处都在 supervisor 锁内 | 并发 retry 换掉受害任务的 active attempt，guarded 写失效 |
| 7 | `open` L488 | recovered **子任务** hydrate 依赖父级 supervisor 已先插入 | 报「persisted recovered child has no recovered root」，恢复失败 |

### 3.3 验收方式：变异自检（本阶段的硬要求）

只跑测试绿是不够的——上表 7 条里，#4 #5 #6 现有测试**未必**覆盖得到。
所以每切一段，都要配一次**变异自检**：

1. 切完后，故意把该段的顺序调换（或把 `first_error` 改成 `?`、把 drain 移到 reduce 之后）。
2. 跑测试。
   - **变红** → 该约束有测试保护，切分安全，恢复顺序。
   - **全绿** → 该约束**没有测试兜底**。先补一条最小测试让它能红，再切。
     否则这次重构就把一个"靠注释维持"的约束变成了"没有任何东西维持"的约束。

第 6 条（三处租约释放）尤其重要：现有 `tests/runtime_coordinator.rs`（4,736 行 / 94 测试）
里有租约相关用例，但"preflight 冲突路径在锁内释放"这个具体时序未必被覆盖，必须先验。

### 3.4 B 阶段判据

```
6 个目标方法      各自 ≤ 90 行
全模块最大方法体   ≤ 150 行（由 start_worker 324 降下来）
文件行数          允许比 A 阶段涨 ≤ 10%（签名开销），不得出现新文件 > 700 行生产
测试              347 全绿不变
7 条顺序约束      每条都有"调换即变红"的证据记录在 plan 中
```

---

## 4. 明确不做

| 不做 | 理由 |
|---|---|
| 不拆 `ProviderTurnAdmissions` 内部结构 | 167 行 impl + 2 个 Drop 守卫，是一个完整状态机；A 阶段整体搬走即可 |
| 不动 `ProviderTurnGate` 的 trait 定义 | 定义在 `yi-agent-core`，不属于本块 |
| 不把 18 个 ≤10 行的薄委托合并 | 它们是 `pub` API 或跨组接缝，合并会破坏 §2.1 的字段簇边界 |
| 不改 `RuntimeCoordinator` 的 18 个字段 | 字段是分组依据本身；改字段等于改职责划分，属另一个议题 |
| 不动 511 行测试的内容 | 只搬位置，断言逐字不变 |
| 不做 `runtime` 之外的任何 crate 改动 | 外部只有 3 个文件引用，且都是 `use`，路径不变故无需改 |

---

## 5. 阶段与顺序

| 阶段 | 内容 | 前置 | 判据 |
|---|---|---|---|
| **A** | 14 文件纯搬移 + 10 处 `pub(super)` | 无 | §2.5 |
| **B** | 6 个巨型方法按阶段切分 | A 全绿 | §3.4 |

A 与 B 分两个 PR。理由：B 的全部风险集中在切点顺序语义上，而这些切点只有在 A 完成后
文件边界清晰时才便于逐个审阅；且 A 本身已把 3,910 行摊到 16 个文件（最大 510 行生产），
"看不懂"的主因已消除，B 是按需增值。

---

## 6. 与第一波 A2 条目的关系

第一波设计的 A2 条目（`runtime.rs` 按 9 组拆子模块、判据 ≤900 行）被本文档**替换**，原因：

1. 分组依据错（按方法名 → 按字段簇），见 §1.3。
2. 判据不可达且无意义。第一波实施时 `runtime.rs` 一行未动，理由是那一波 plan 里写着
   "不改 `runtime.rs` —— 它属于另一块"；但 spec 里保留了 ≤900 的判据，导致两处文档冲突。
   本文以实测基线重新定判据（§2.5），冲突消解。
3. 方法数记错（69 → 71），见 §1.4。

`docs/project-management/yi-agent-store.md` 需补一条本文实施的记录，并在
`2026-11-02-refactor-wave1-design.md` 的 A2 行加注"已由本文档替换"。

---

## 7. 测量方法（可复现）

三个数字踩过坑，记录正确做法：

**总行数 / 生产行数**。不能用"首个 `#[cfg(test)]` 之前"估算——虽然本文件恰好适用，
但 `repository.rs` 当初就因此算错。正确做法是遍历每个 `#[cfg(test)]`，
跳过属性行，若下一行是 `mod x;` 则计 1 行，否则找到列 0 的 `}` 作为块结束：

```python
def test_lines(L):
    tot = 0; i = 0; n = len(L)
    while i < n:
        if L[i].strip() == '#[cfg(test)]':
            j = i + 1
            while j < n and (L[j].strip().startswith('#[') or L[j].strip() == ''):
                j += 1
            if j >= n: break
            if L[j].rstrip().endswith(';'):
                tot += j - i + 1; i = j + 1; continue
            k = j
            while k < n and L[k] != '}': k += 1
            tot += k - i + 1; i = k + 1
        else:
            i += 1
    return tot
```

**不要用花括号计数**：字符串字面量里的 `{` 会误导（`default_system_prompt` 的 raw string 踩过）。

**方法边界**。在 `impl RuntimeCoordinator {` 起始行之后，匹配 `^    (pub\()?[a-z]* ?(async )?fn (\w+)`，
然后从首个 `{` 起做深度计数到 0。**注意 8 空格缩进的 `fn` 全在测试模块内**（L4148 起），
若不限制在 impl 区间内会多算。

**测试名集合门禁**。`sed 's/.*:://'` 只取末段，因为拆文件会让模块路径多一层
（与第一波 A3 同一坑，已在最小工程验证过 `sub::alpha_tests::the_test` 的行为）。
