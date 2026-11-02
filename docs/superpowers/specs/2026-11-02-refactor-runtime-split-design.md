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
| 内联测试 | **511** = `provider_turn_admission_tests` **491 行**（L3832–4323）+ `extra_roots_resolution_tests` **20 行**（L4325–4344） |
| 生产代码 | **3,910** |
| `impl RuntimeCoordinator` | L326–3394，**3,069 行** |
| 方法数 | **71**（54 `pub` + 17 私有） |
| 方法体合计 | 2,822 行 |
| `impl` 之外的生产代码 | **841 行**（= 3,910 − 3,069；它的**去向**不是全部留在 `mod.rs`，见 §2.2） |

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
按这个名字分，`spawn` 组会拿走 766 行（含 `start_worker` 324）、`application_root` 组 479 行
——问题只是从 1 个文件变成 16 个文件里的 2 个更大文件。

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
其中 `pub` 54、私有 17。

全文件 `grep -c '^    pub \(async \)\?fn '` 得 **55**，比 54 多 1。多出来的那个是
**L135 的 `CodingRequiresGitReason::as_str`**——它属于 `impl CodingRequiresGitReason`（一个 enum 的
固有实现），在 `impl RuntimeCoordinator` 块之外。`impl ProviderTurnAdmissions`（L3571–3737）
里的方法都不是 `pub`，不贡献计数。

后出现的 8 空格缩进 `fn` 全部在测试模块内（L4148 起），不参与计数。

方法数影响判据的分母（本文所有归组表以 71 为准），但不影响最终目标——目标按行数定，不按方法数定。

---

## 2. A 阶段：纯搬移

**语义约束：只搬不改。** 方法体逐字不动，唯一允许的文本变化是 §2.3 的 10 处可见性关键字。

### 2.1 目标结构（16 个文件）

沿用 `repository` 已确立的模式：`struct` 留在 `mod.rs`，`impl` 分文件，
每个子模块 `use super::*`。这样不需要新约定——Rust 的"私有字段对后代模块可见"规则直接可用。

预算来源：**把 4,421 行逐行分区，每一行恰好归属一个目标文件**（校验脚本见 §7.4。
分区后 impl 之内 71 个方法连同其文档注释共 2,995 行、impl 块内方法间空行 74 行、
impl 之外 1,352 行，合计 4,421，无缺口无重叠）。

`核心`列 = 该方法集合体的实测行数（含文档注释）+ 随之迁移的具名项与自由函数实测行数。
`预算`列 = 核心 + 该文件必需的样板（`use super::*;` + `impl ... {` + `}`，以及每方法间 1 行空行）。

| 文件 | n | 内容 | 核心 | 预算 |
|---|---|---|---|---|
| `runtime/provider_turn.rs` | 0 | `ProviderTurnAdmissions` 子系统 279 行 + 测试 491 行 | 770 | 774（生产 283） |
| `runtime/application_root.rs` | 7 | attach / ensure_supervisor / authorize / activate / detach / spawn_application_child / workspace_service_for + capability/digest 纯函数 | 523 | **535** |
| `runtime/worker_start.rs` | 4 | `start_worker` / `prepare_task_workspace` / `nearest_ancestor_workspace` / `supervisor_workdir_for` + `WorkspaceAssignedFactory` | 474 | **483** |
| `runtime/spawn.rs` | 6 | `spawn_child*` / `resolve_parent_workdir` / `remember*_task_position` + `resolve_extra_roots` + `extra_roots_resolution_tests`（20 行） + git 三件套 | 388 | **399** |
| `runtime/lifecycle.rs` | 12 | cancel / pause / resume / retry / draining / graceful_stop / interrupt_* | 352 | **369** |
| `runtime/reconcile.rs` | 4 | `reconcile_worker_events` / `persist_cascaded_tasks` / `persist_worker_trace_facts` / `worker_cancellation` + `trace_fact_kind` + 4 个辅助（含 `truncate_utf8`） | 344 | **353** |
| `runtime/messaging.rs` | 8 | send_* / request_permission / resolve_permission / mailbox_snapshot / delivery_diff / active_thread_children | 256 | **269** |
| `runtime/mod.rs` | 0 | 结构体、错误类型、常量、`RuntimeStopOptions/Summary`、`AttachedApplicationRoot`、47 行 import、16 行 `mod` 声明 | 190 | **216** |
| `runtime/fork.rs` | 5 | fork 5 方法 + `ForkUpload` + 3 个 fork 常量 | 187 | **197** |
| `runtime/init.rs` | 4 | `open` + `with_fork_max_bytes` / `fork_max_bytes` / `max_direct_children` | 194 | **203** |
| `runtime/hydration.rs` | 0 | 4 个 hydration 辅助 + `RecoveryContext` | 134 | **138** |
| `runtime/children.rs` | 8 | 子任务查询与授权 | 132 | **145** |
| `runtime/watchdog.rs` | 2 | `evaluate_watchdogs` / `record_watchdog_terminal` | 121 | **128** |
| `runtime/session.rs` | 5 | `create_session` 系列 4 个 + `root_task_id` + `AutonomousSession` | 114 | **124** |
| `runtime/schedule.rs` | 2 | `evaluate_schedules` / `prune_terminal_traces` | 107 | **114** |
| `runtime/resident.rs` | 4 | 4 个居留方法 | 61 | **70** |
| **合计** | **71** | | **4,347** | **4,517** |

核心合计 4,347 + impl 内方法间空行 74 = **4,421**（= 文件实数，校验通过）。
预算合计 4,517 比现值高 96 行（+2.2%），来自每文件的 `use super::*;` / `impl {` / `}` 与每方法 1 行空行。

**为什么 `init.rs` 和 `hydration.rs` 必须存在**（自查时发现的两个预算漏洞）：

- `init` 组的 4 个方法必须落在某个文件。全塞进 `mod.rs` 会让它变成 190 + 194 = 384 行，
  超出"壳文件"的含义（`mod.rs` 应只放声明与类型）。单开 `init.rs` 后 `mod.rs` 保持 216。
  `open` 在 B 阶段还要切成 4 段，放在 `init.rs` 里也有余量。
- `application_root` 组本身 523 行，加上它独占使用的 4 个 hydration 辅助（105 行）
  和 `RecoveryContext`（29 行）会到 657 行，超过 560 上限。单列 `hydration.rs` 后降到 535。
  `RecoveryContext` 本就与 hydration 同源（`open` 构造它、`start_worker`/`resume_task` 读它），
  一并放进 `hydration.rs` 最自然。

**`truncate_utf8` 容易被漏掉**：它没有外部调用者，唯一的调用点在
`worker_failure_terminal_json` 内部（L3417）。两者必须一起进 `reconcile.rs`，
否则编译失败。这类"只被同一个随迁函数使用"的辅助必须跟着它的调用者走。

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

`impl` 之外那 1,352 行（L1–47 的 import、L3395 的空行、L3396–4421）
不是均质的，实测按职责分成六块：

```
L1-47       47   import（按去向分散到各子模块，不集中在 mod.rs）
L48-62      15   两个 fork 上限常量 + FORK_MAX_BYTES_ENV + FORK_UPLOAD_TTL
L63          1   空行
L64-69       6   AutonomousSession
L70-117     48   RuntimeCoordinatorError（全部错误变体）
L118-148    31   CODING_REQUIRES_GIT_PREFIX + CodingRequiresGitReason + impl as_str
L149-158    10   trace_fact_kind
L159-177    19   RuntimeStopOptions / Default / RuntimeStopSummary
L178-190    13   AttachedApplicationRoot
L191-206    16   ForkUpload
L207-234    28   RuntimeCoordinator 结构体
L235-281    47   WorkspaceAssignedFactory + impl AgentWorkerFactory
L282-325    44   capability/digest 纯函数 + in_place_workspace_at + recorded_workspace
L3395        1   空行
L3396-3832 437   ProviderTurnAdmissions 子系统 + 4+1 个自由函数（明细见下表）
L3832-4323 492   mod provider_turn_admission_tests（属性行 + 491 行 test_lines）
L4324        1   空行
L4325-4344  20   mod extra_roots_resolution_tests
L4345        1   空行
L4346-4374  29   RecoveryContext + impl
L4375-4421  47   coding_git_precondition / git_capture / truncate_diff
```

注：`L3832` 是 `#[cfg(test)]` 属性行，它与 L3833 起算的模块体同属一个测试单元，
两种写法（3832 起 / 3833 起）差了 1 行，本文统一按 span 计算。

**L3396–3832 这 437 行并非全归 provider_turn。** 逐段归属后，其中只有 279 行属于它：

| 行段 | 行数 | 归属 |
|---|---|---|
| L3396–3407 | 12 | `ProviderTurnAdmissions` 结构体 |
| L3408–3414 | 7 | `resolve_extra_roots` → **`spawn.rs`**（被 `spawn_child_with_objective` L1656 调用） |
| L3415–3425 | 11 | `worker_failure_terminal_json` → `reconcile.rs` |
| L3426–3436 | 11 | `truncate_utf8` → `reconcile.rs`（唯一调用点在 L3417，同文件内） |
| L3437–3450 | 14 | `text_completion_terminal_json` → `reconcile.rs` |
| L3451–3460 | 10 | `budget_exhausted_terminal_json` → `reconcile.rs` |
| L3461–3565 | 105 | 4 个 hydration 辅助 → `hydration.rs` |
| L3566–3570 | 5 | `QueuedProviderTurn` |
| L3571–3737 | 167 | `impl ProviderTurnAdmissions`（**无 `pub` 方法**） |
| L3738–3760 | 23 | `ProviderTurnLeaseHandle` + `Drop` |
| L3761–3818 | 58 | `RuntimeProviderTurnGate` + `impl ProviderTurnGate` |
| L3819–3832 | 14 | `ProviderTurnWaitCleanup` + `Drop` |
| **provider_turn 小计** | **279** | 12 + 5 + 167 + 23 + 58 + 14 |
| **其余分给 spawn / reconcile / hydration** | **158** | 7 + 46 + 105 |

它只与 `init` 组有接缝：`open` 构造它、`start_worker` 取 gate。内部无对外引用。

**若不继续往下分、全堆在 `mod.rs`，它就会是 841 + 声明开销 ≈ 860 行**——勉强压线但毫无意义。第一波设计判据写 "841 行 ≤ 900"，
这个判据有两处偏差，方向相反，恰好凑出一个看起来合理的 841：

1. 把内联测试算进了 shell（实际 491 行跟着子系统走、20 行跟着 `spawn.rs` 走，一行都不留在 `mod.rs`）。
2. 把 L3396–3832 整段都当成了"留在 shell 里"的代码。按 `3910 − 3069 = 841` 这种减法算，
   shell 里确实"还剩"841 行，但这 841 行里大部分**不该留在 `mod.rs`**：279 行是
   provider-turn 子系统、46 行归 `reconcile.rs`、105 行归 `hydration.rs`、7 行归 `spawn.rs`。
   真正该留在 `mod.rs` 的只有 **190 行**（47 行 import + 48 行错误 + 31 行 git 前缀
   + 28 行结构体 + 19 + 13 + 3 行空行），加上 16 行 `mod` 声明也不过 **216 行**。

换句话说：**841 这个数字本身是对的，错的是"它 ≤ 900 所以达标"这个推论**——
它默认了 impl 之外的东西全部堆在 `mod.rs`。实际做法是继续往下分，`mod.rs` 只有 216 行。

### 2.3 可见性代价：10 个私有方法必须放宽

私有方法一旦定义在子模块的 `impl` 里，可见范围就是该子模块，跨组调用会被 Rust 拒绝。
实测跨组调用边，**10 个** 私有方法需要改 `pub(super)`：

| 私有方法 | 定义于 | 被几个组跨组调用 | 调用方 |
|---|---|---|---|
| `supervisor` | children.rs | **8** | application_root / lifecycle / messaging / reconcile / session / spawn / watchdog / worker_start |
| `release_resident_lease` | resident.rs | 5 | lifecycle / messaging / reconcile / watchdog / worker_start |
| `ensure_admitting` | lifecycle.rs | 5 | application_root / schedule / session / spawn / worker_start |
| `authorize_child_access` | children.rs | 2 | fork / lifecycle |
| `persist_cascaded_tasks` | reconcile.rs | 2 | watchdog / worker_start |
| `remember_task_position` | spawn.rs | 2 | application_root / worker_start |
| `persist_resident_cursor` | resident.rs | 1 | worker_start |
| `authorize_application_root` | application_root.rs | 1 | messaging |
| `workspace_service_for` | application_root.rs | 1 | worker_start |
| `supervisor_workdir_for` | worker_start.rs | 1 | messaging |

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
runtime/mod.rs          生产 ≤ 260 行，内联测试 0        (预算 216)
其余 15 个文件           生产 ≤ 560 行                     (最大 application_root.rs 预算 535)
provider_turn.rs        ≤ 980 行 = 生产 283 + 随迁测试 491（测试不设上限）
总行数                  4,421 → ≤ 4,550                   (见下)
```

**总量上限为什么是 +3% 而不是"守恒 ±1%"。** 纯搬移并不严格守恒：每个新文件要加
`use super::*;` / `impl ... {` / `}` 三行样板、每个方法之间 1 行空行，`mod.rs` 还要加
16 行 `mod` 声明。

实测标定：第一波 `repository.rs` 拆成 12 个文件，总行数 **5,074 → 5,131（+57 = +1.1%）**。
本块 16 个文件、文件数更多，按同比例上浮留 3%（+133 行）= **≤ 4,554**。
若实测超过 4,600，说明有东西搬重复了或没搬走，应当排查而不是调数字。

按 §2.1 的分区预算逐文件相加得 4,515，比现值 4,421 高 94 行（+2.1%），落在上限内。

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

### 2.6 测试模块归属（两个分开走）

```
mod provider_turn_admission_tests   L3833-4324  -> runtime/provider_turn.rs
mod extra_roots_resolution_tests    L4325-4346  -> runtime/spawn.rs
```

* `provider_turn_admission_tests` 测 `ProviderTurnAdmissions` 的配额与租约行为。
* `extra_roots_resolution_tests` 直接测自由函数 `resolve_extra_roots(None, Some(&parent))`
  （代码注释写明"直接测解析函数而非整条 daemon 链路"），因此必须跟这个函数走
  ——它在 `spawn_child_with_objective` 里被调用（L1656），属于 `spawn.rs`。

两个模块不能一起塞进 `provider_turn.rs`。这是"测试跟着被测代码走"的规则，
与第一波 `ipc` 拆分的 16 个测试模块同一条约束。

行数口径（`#[cfg(test)]` 属性行计入所属模块）：
`provider_turn_admission_tests` **491** 行（L3832–4323，其中测试函数 490 行）、
`extra_roots_resolution_tests` **20** 行（L4325–4344）。
前者与 `provider_turn` 子系统 279 行合计 **770**（§2.1 的"核心"列）；后者归 `spawn.rs`（计入其 388）。

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
| `open` | 179 | 4 | `init.rs` |
| `attach_application_root` | 170 | 4 | `application_root.rs` |
| `ensure_application_root_supervisor` | 147 | 3 | `application_root.rs` |

**切分形态（两类，取决于接收者）**

Rust 的 `impl` 声明可以分散在多个文件，**有 `self` 的阶段函数必须是
`RuntimeCoordinator` 的方法**——写成自由函数会拿不到 `self`，也就访问不到私有字段。
但 `open` 是关联函数，它根本还没有 `Self`，这一类只能用自由函数 + 显式传参。

| 目标 | 现签名 | 阶段函数形态 | 中间产物 |
|---|---|---|---|
| `start_worker` / `spawn_child_with_objective` / `reconcile_worker_events` / `attach_application_root` / `ensure_application_root_supervisor` | `&self` | `fn phase_x(&self, ...) -> Result<T, E>` | 需要显式结构体承载跨阶段状态（`&self` 不能就地改字段） |
| `open` | **关联函数，无 `self`** | 自由函数 `fn phase_x(repo: &mut RuntimeRepository, factory: &Arc<dyn ...>, ...) -> Result<T, E>` | 阶段 4 之前 `Self` 尚不存在，无法用 `&self` 方法 |

`open` 的这一条是硬约束，不处理会在实施时才发现：`open` 的签名是
`pub fn open(database_path, factory) -> Result<Self, E>`，阶段 4 之前构造器还没产出 `Self`。
现有代码的注释已经点明这一点——`max_direct_children` 那一处写着
"`open` is an associated constructor with no `self`, and the recovery loops below build
supervisors before `Self` exists, so the limit is read off the factory here and threaded
through as a local"（L456–458）。因此 `open` 的 4 个阶段必须是**自由函数**，
把 `&mut RuntimeRepository` 与 `factory` 显式传参，而不是 `pub(super) fn phase(&self)`。

**第二个硬约束：跨阶段持有的锁守卫。**

`start_worker` 在 L1809 取到 `supervisor.lock().await` 之后，**这个 `AsyncMutexGuard` 一直活到
L2090 的 `supervisor.start(...)`**，中途只在工作区准备失败的错误分支（L1880）被显式 `drop`。
也就是说：阶段 2 取到的守卫要跨阶段 3、4、5、6 供后续阶段使用。

因此 `start_worker` 的切分**不能**按"每阶段一个 `&self` 方法、各自加锁"来做——那会重复加锁、
或在阶段之间释放锁造成竞态（现有代码在 L1998 的注释里明确写了"while the supervisor guard
is still held, mirroring the watchdog path, so no concurrent retry can swap a victim's
active attempt"）。

可行切法只有两种，实施时选一种并写进 plan：

1. **守卫作为参数传递**：`fn phase_x(&self, supervisor: &mut AgentSupervisor, ...)`，
   把解引用后的 `&mut AgentSupervisor` 传下去，守卫仍由外层 `start_worker` 持有。
2. **保留单方法**，只把**不依赖守卫的**子步骤（如阶段 1 的闸门、阶段 7 的 factory 调用）
   抽出去，中间三段（3–6）留在原方法里。这条路 `start_worker` 压不到 90 行，需要放宽判据。

推荐第 1 种。同理，另外三个目标也持有守卫（`spawn_child_with_objective` 2 次加锁、
`reconcile_worker_events` 逐会话加锁、`attach_application_root` 1 次），
`ensure_application_root_supervisor` 不持锁。

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

### 3.3 验收方式：变异自检（且先查清哪几条已有测试）

7 条约束**不能假设都有测试兜底**——那正是这次重构最大的隐患：把一条"靠注释维持"的约束
变成"没有任何东西维持"的约束。我逐条查了现有测试（`tests/runtime_coordinator.rs` 94 个用例、
`tests/subagent_fork_upload.rs` 14 个），实测覆盖情况：

| # | 约束 | 现有测试 | 状态 |
|---|---|---|---|
| 1 | 父级已结算早于 `take_fork_messages` | `a_settled_parent_cannot_spawn_and_the_gate_runs_before_the_fork_token`（L4678，注释写明用 bogus token 做顺序探针） | **有** |
| 2 | coding git 预检早于写任务行 | `an_empty_repository_is_refused_for_missing_head`（L4627）等一组，断言 `after == before`「a refused spawn must not leave a queued row」 | **有** |
| 3 | `extra_roots` 与 `inherited_sandbox` 故意不对称 | `child_inherits_parent_extra_roots_when_not_given_explicitly` 只测**解析函数**，不测落库时序 | **缺时序断言** |
| 4 | 先 drain trace facts 再 reduce | 全仓库搜 `trace_fact` / `terminal.*trace` 无命中 | **缺** |
| 5 | `first_error` 累积不得 `?` 早返 | `one_supervisors_failure_does_not_discard_another_sessions_completion`（L3748） | **有** |
| 6 | 三处租约释放（游标失败 L1945 / preflight 冲突 L2008 / 准入失败 L2051） | `a_recovery_conflicted_parent_cascades_and_releases_its_live_child`(L1455) 覆盖 preflight 冲突；`startup_failure_closes_attempt_and_releases_admission_leases`(L1578) 覆盖准入失败 | **有 2/3**：只剩游标持久化失败那一支未确认 |
| 7 | 子任务 hydrate 依赖父级 supervisor 已插入 | 全仓库搜 `no recovered root` 无命中 | **缺** |

**结论：7 条里 4 条（#1 #2 #5 #6 的两支）有直接测试，#6 剩一支待确认，#3 #4 #7 完全没有。**

因此 B 阶段的流程不是"切完跑测试"，而是对每条约束：

1. 切完后，故意把该段的顺序调换（或把 `first_error` 改成 `?`、把 drain 移到 reduce 之后）。
2. 跑测试。
   - **变红** → 该约束有测试保护，切分安全，恢复顺序。
   - **全绿** → 没有兜底。**先补一条最小测试让它能红**，再切。

#3 #4 #7 必须先补测试，否则这三段的切分等于把注释里的知识永久丢掉。#4 尤其危险：
它保护的是"为什么 worker 停了"这条诊断信息，丢了不会有任何功能报错，只会在排查现场时
发现 trace 尾部缺一段。

#6 只剩"游标持久化失败 → 释放租约"（L1945）这一支未确认。它的触发需要让
`persist_resident_cursor` 写库失败，现有测试夹具没有这个注入点，大概率要补。
补法与前一条相同：注入一个失败的 repository 操作，断言租约已释放。
补测试的方式是注入一个会失败的 `RuntimeRepository` 操作并断言租约已释放，
不需要新的测试基础设施。

### 3.4 B 阶段判据

```
6 个目标方法      各自 ≤ 90 行
全模块最大方法体   ≤ 150 行（由 start_worker 324 降下来）
文件行数          允许比 A 阶段涨 ≤ 10%（签名开销），不得出现新文件 > 700 行生产
测试              347 全绿不变
7 条顺序约束      每条都有"调换即变红"的证据记录在 plan 中
锁守卫            不新增加锁点：`grep -c 'lock().await' worker_start.rs` 与拆前同值
```

最后一条是关键门禁。`start_worker` 拆成多个方法时，若把守卫改成每阶段各自加锁，
`lock().await` 的次数会从 1 涨到 N——这时测试**大概率仍然全绿**（单线程测试看不出竞态），
但生产环境的并发语义已经变了。所以这道门禁不能靠测试，必须靠 grep 计数。

`spawn_child_with_objective` 基线 2 次、`reconcile_worker_events` 1 次、
`attach_application_root` 1 次、`ensure_application_root_supervisor` 0 次。

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
| **A** | 16 文件纯搬移 + 10 处 `pub(super)` | 无 | §2.5 |
| **B** | 6 个巨型方法按阶段切分 | A 全绿 | §3.4 |

A 与 B 分两个 PR。理由：B 的全部风险集中在切点顺序语义上，而这些切点只有在 A 完成后
文件边界清晰时才便于逐个审阅；且 A 本身已把 3,910 行摊到 16 个文件（最大 535 行生产），
"看不懂"的主因已消除，B 是按需增值。

---

## 6. 与第一波 A2 条目的关系

第一波设计的 A2 条目（`runtime.rs` 按 9 组拆子模块、判据 ≤900 行）被本文档**替换**，原因：

1. 分组依据错（按方法名 → 按字段簇），见 §1.3。
2. 判据不可达且无意义。第一波实施时 `runtime.rs` 一行未动，理由是那一波 plan 里写着
   "不改 `runtime.rs` —— 它属于另一块"；但 spec 里保留了 ≤900 的判据，导致两处文档冲突。
   本文以实测基线重新定判据（§2.5），冲突消解。
3. 方法数记错（69 → 71），见 §1.4。

`docs/project-management/yi-agent-store.md` 需补一条本文实施的记录（A、B 分两次提交），
并在 `2026-11-02-refactor-wave1-design.md` 的 A2 行加注"已由本文档替换"。

第一波 spec 的 A2 行现文为：

```
| A2 | `runtime.rs` 按 9 组拆子模块 | 同上 |
```

其中「9 组」与 §2.1 的 16 个文件对不上，实施时以本文 §2.1 为准。

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

**方法边界（已实跑验证，产出 71 个）**。在 `impl RuntimeCoordinator {` 起始行之后逐行匹配：

```python
import re
pat = r'^    (pub(\([a-z]+\))? )?(async )?fn (\w+)'
```

四个捕获组依次为「`pub` 与否」「`pub(...)` 限定」「`async` 与否」「方法名」。
注意 `pub` 之后的空格是**必需**的（`pub fn` / `pub async fn`），漏掉会只匹配到私有方法
（实测漏掉得 17 个而非 71 个）。

找到起始行后，从同一行（或后续行）首个 `{` 起做花括号深度计数到 0，即为方法结束。
**注意 8 空格缩进的 `fn` 全在测试模块内**（L4148 起），因此循环上界必须是 `impl` 块的 `}`，
不能扫到文件末尾。

方法体行数有两种口径，本文两处都标明了用哪个：
- **不含文档注释**：2,822 行（§1.1 "方法体合计"）
- **含文档注释与属性行**：2,995 行（§2.1 分区表）

两者差 **173 行**，全部是方法上方的 `///` / `//` 注释。

**测试名集合门禁**。`sed 's/.*:://'` 只取末段，因为拆文件会让模块路径多一层
（与第一波 A3 同一坑，已在最小工程验证过 `sub::alpha_tests::the_test` 的行为）。
