# 父任务终结后禁止派生 设计

**日期**: 2026-10-29
**状态**: 待实现
**相关代码**: `yi-agent-core`（`subagent/supervisor.rs`）、`yi-agent-store`（`runtime.rs`、`ipc.rs`）

## 1. 背景

### 1.1 结构不变式只做了一半

`2026-10-02-subagent-cascade-on-terminal-design.md` 把「父任务进入已定终态 ⇒ 其全部子任务也进入终态」做成了结构不变式：`reduce_task` → `settle_terminal` 会在父任务落定终态时递归取消子树（`supervisor.rs:1828`）。

但该不变式只覆盖**父终结的那一刻**。它没有覆盖**相反方向**：父任务已经处在终态时，仍然允许新的子任务被派生出来。

### 1.2 缺失的校验

`AgentSupervisor::spawn_with_objective`（`supervisor.rs:1273`）是唯一的内存内派生入口，它只做三项检查：

```rust
let parent = self.tasks.get(&parent_id).ok_or(SpawnError::ParentNotFound)?;
let child_depth = parent.depth.can_spawn_child().map_err(|_| SpawnError::MaximumDepthReached)?;
if active_direct_children >= self.max_direct_children { return Err(SpawnError::DirectChildLimitReached { .. }); }
```

没有一项检查 `parent.state()`。于是：

- 父任务已完成，仍可派生新子任务。
- 新子任务不在 `settle_terminal` 的级联范围内（级联只在父**状态转换**的那一刻触发，已终结的父不会再转换）。
- 新子任务若随后完成，会向已终态的父投递结果 —— 而 `deliver_message` 恰恰拒绝 `RecipientTerminal`（`supervisor.rs:1377`）。

结果是一棵明确无意义的子树：它不可能被父消费，父也不可能再评审它。

### 1.3 与既有设计的对照

同一个文件里，`deliver_message` 已经实现了「不向终态任务投递」的规则：

```rust
if recipient_task.state().is_terminal() { return Err(MessageDeliveryError::RecipientTerminal); }
```

派生侧的对称规则（「不从终态任务派生」）缺失。本设计补上这一条。

### 1.4 实测现状

2026-10-29 查询本机 `runtime.sqlite`：父已定终态且子未终态的行数为 **0**。

说明 §1.1 那份级联 spec 已经把主路径修好，当前没有存量脏数据。因此本设计的定位是**入口防回归**，而不是历史数据修复 —— 这一事实直接决定 §5 的取舍。

## 2. 目标

**父任务处于「已定终态」时，拒绝派生新的子任务。**

非目标：

- 不改终态集合的定义。
- 不改级联行为（`settle_terminal` 不动）。
- 不改 `recovery_required` 的可恢复语义。
- 不追溯处理历史数据（实测为 0，见 §1.4）。

## 3. 设计

### 3.1 用哪个判定：复用 `classify_terminal`

**关键前提**：`is_terminal()` **不能**用于这个判断。它把 `RecoveryRequired` 也算作终态（`task.rs:221`），而 `RecoveryRequired` 是「等待显式恢复的暂存态，不是终结」：父在 daemon 重启后可以 resume，resume 后仍需要派生新子任务。用它做闸门会误拒恢复中的父任务。

正确做法是复用文件内已有的 `classify_terminal`（`supervisor.rs:1966`），它正是为此存在：

```rust
enum TerminalKind { NonTerminal, Settled, RecoveryRequired }
```

注释已写明它是「Deliberately narrower than `TaskState::is_terminal`」。

规则：仅当 `classify_terminal(parent.state()) == TerminalKind::Settled` 时拒绝。`RecoveryRequired` 放行。

`Settled` 的 8 个状态：`completed`、`completed_no_changes`、`blocked`、`stalled`、`timed_out`、`budget_exhausted`、`failed`、`cancelled`。

这意味着**判定逻辑零新增**：状态分类与语义注释全部已在仓库中，本设计只是把它接到派生入口上。（翻译层仍需一小段代码，见 §3.4。）

### 3.2 新增错误变体

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SpawnError {
    #[error("parent task does not exist")]
    ParentNotFound,
    #[error("leaf tasks cannot spawn descendants")]
    MaximumDepthReached,
    #[error("an agent may have at most {limit} direct children")]
    DirectChildLimitReached { limit: usize },
    #[error("a settled parent task (state: {state}) cannot spawn descendants")]
    ParentSettled { state: &'static str },
}
```

`state` 用 `task_state_label()`（`supervisor.rs:1981`）的返回值，让错误指向具体状态（`completed` vs `cancelled` 的处理建议不同）。

`Copy` 派生要求 `&'static str`，与现有 `DirectChildLimitReached { limit: usize }` 风格一致。

### 3.3 判定位置：`spawn_with_objective` 的第一道检查

放在 `ParentNotFound` 之后、`can_spawn_child` 之前：

```rust
let parent = self.tasks.get(&parent_id).ok_or(SpawnError::ParentNotFound)?;
if let TerminalKind::Settled = classify_terminal(parent.state()) {
    return Err(SpawnError::ParentSettled { state: task_state_label(parent.state()) });
}
```

理由：父是否存在比父是否已终结更基础，错误顺序应符合直觉；深度检查只有在父可派生时才有意义。

### 3.4 为什么不在 `runtime.rs` 层拦截

`runtime.rs:1530` 的 `spawn_child_with_objective` 在调用 `supervisor.spawn_with_objective` **之前**已经做了两件有副作用的事：

1. `take_fork_messages`（`runtime.rs:1180`）会 `uploads.remove(token)` —— **消耗掉 fork 上传**。
2. 在此之前的所有校验（队列容量、`coding_git_precondition`）都刻意排在写行之前，注释明确写着「a refusal must not leave a queued ghost task」。

若只在 supervisor 层拦截，fork 消息会先被消耗再失败，用户重试时 token 已失效。

**所以两层都要有**：

- `runtime.rs` 层：在 `take_fork_messages` **之前**预检，保护 fork 上传不被无谓消耗。
- supervisor 层：作为权威闸门，覆盖所有内存内派生（含测试与未来调用点）。

预检的判定必须复用同一份分类，不能另写一套字符串清单。

`repository.task_state()` 返回的是 `state_json` 的原文，实测库里存的是裸标签（`completed`、`cancelled`、…，无引号、无 JSON 包裹），所以字符串比较在技术上可行 —— **但不要这样做**。那会在 store crate 里诞生第二份「哪些状态算已定终态」的清单，与 `classify_terminal` 分头漂移，正好违背 §3.1 的承诺。

**不要复用 `hydrated_application_root_state`**（`runtime.rs:3489`）。它看起来像「标签 → `TaskState`」的现成解析器，但实际不可用：

- 它把 `waiting_for_resource` / `waiting_for_permission` / `waiting_for_children` 全部折叠成 `TaskState::Queued`，信息有损。
- 它把 `recovery_gated` / `recovery_attested` 折叠成 `RecoveryRequired`，是 app root 水合专用的宽松投影。
- `other` 分支直接报错，对历史脏值不容错。

**注意 `TaskState` 的 serde 形态不是库里的形态**：该枚举没有 `#[serde(rename_all = ...)]`，所以 `serde_json::from_str::<TaskState>("\"completed\"")` 会失败，而库里存的正是裸的 `completed`（见上文实测）。不要顺手用 serde 反序列化。

正确做法：给 `TaskState` 增加一个可反解析的标签入口（与 `task_state_label` 的输出格式互为逆运算），让 `runtime.rs` 走

```
task_state_label 输出 ←→ TaskState 解析 ← classify_terminal
```

这条唯一路径。若解析失败（历史脏值），按「放行交给 supervisor 权威判定」处理，而不是按「拒绝」处理：预检只是保护区，不是判定权，误拒一个本可派生的父任务比多消耗一次 fork 上传更糟。

预检失败返回新变体 `RuntimeCoordinatorError::ParentSettled { state: String }`。

### 3.5 恢复路径不受影响

`insert_recovered_child`（`supervisor.rs:351`）是**独立 API**，不经过 `spawn_with_objective`，因此恢复流程完全不受 3.3 的拦截影响。这是本方案能安全收紧派生入口的前提。

### 3.6 IPC 接线

两处 `match` 需要显式分支：

- `ipc.rs:2969` `ipc_error_message`：目前只对 `DirectChildLimitReached` 给出可读消息，兜底返回 `None`。**必须新增 `ParentSettled` 分支**，否则模型只能看到错误码 `invalid_state`、看不到原因。
- `ipc.rs:2998` `ipc_error_code`：`Spawn(_)` 兜底已是 `InvalidState`，语义正确，无需单独分支（编译不会报错，但需确认测试断言）。

工具层 `format_ipc_rejection`（`subagent/lib.rs:1656`）会在有 message 时输出 `daemon rejected {action}: {code}: {message}`，因此新消息开箱即见。

## 4. 测试

按 TDD，先写红测试。

### 4.1 `yi-agent-core`（supervisor 单元测试）

1. **父 `Completed` 后派生被拒** → 返回 `ParentSettled { state: "completed" }`，且子任务数不变。
2. **父 `Failed` / `Cancelled` 后派生被拒** → 同上。
3. **父 `RecoveryRequired` 时派生仍被允许** → 这是最关键的一条，锁定 §3.1 的取舍。若误用 `is_terminal()`，本测试必须失败。
4. **父 `Running` 时派生照常成功** → 防止闸门过宽。
5. 子任务数在拒绝后不增加：断言 `children_of(parent)` 长度不变（证明拒绝发生在 `tasks.insert` 之前，未留下半个子任务）。

### 4.2 `yi-agent-store`（runtime 集成测试）

6. **fork 上传不被浪费**：父已终态时带 `fork_token` 连续调用两次 ——

   - 第一次返回 `ParentSettled`。
   - 第二次**仍返回 `ParentSettled`**，而不是 `ForkUpload("fork upload token is unknown")`。

   这是只用公开 API 就能证明「预检排在 `take_fork_messages` 之前」的断言。`fork_uploads` 是私有字段（`runtime.rs:225`），测试不可直接读取，因此不要写私有字段断言。
7. **无幽灵任务行**：失败后 `tasks` 表中该 session 的任务数不变。

### 4.3 变异验证

临时把 3.3 的判定改为 `is_terminal()`：测试 3（`RecoveryRequired` 放行）必须由绿转红。这证明测试真的在测那条取舍，而不是碰巧通过。

## 5. 取舍与不做的部分

### 5.1 为什么不处理「存量孤儿」

先前讨论过三种方案，实测数据（§1.4）后收敛到**不做**：

- **只跳过、不处理存量**：脏数据永久留存、每次启动 warn、占 lease。
- **跳过并自动标 `cancelled`**：自愈彻底，但等于自动改用户数据，可能误杀本可恢复的任务。
- **跳过并释放 lease、任务行不动**：只还资源，不判命运。

实测孤儿数为 **0**，三种方案都没有现实收益，却各自引入风险与额外代码。而 490 个 active lease 挂在终态任务上是**另一个独立的租约泄漏问题**（`resource_leases` 的释放路径），与本设计无关，混进来只会让两者都难以验证。

因此本设计**明确不做**存量清扫，只在北京 §6 记录该现象，留待独立设计。

### 5.2 仍存在的结构缺口

本设计是**入口闸门**，不是结构不变式。真正的结构解法是让「父已终结」与「存在活子」这两个状态在数据库层面互斥（例如触发器或 `CHECK` 约束），代价过高且与现有 reducer 架构冲突。

因此 `2026-10-02` 那份 spec 的结构不变式仍然依赖各调用点自觉；本设计只是把最危险的那个缺口（派生入口）堵上。

## 6. 观测到但不在本设计范围内的问题

- **租约泄漏**：2026-10-29 实测 490 条 `state='active'` 的 `resource_leases`，所属任务状态分布为 `completed` 372 / `blocked` 96 / `recovery_required` 22。终态任务的 lease 本应释放。这是独立缺陷，需单独设计。

## 7. 验收

```bash
cargo test -p yi-agent-core --lib subagent::supervisor
cargo test -p yi-agent-store --test runtime_coordinator parent_settled
cargo fmt --all --check
```

人工确认：

- 父任务完成后调用 `spawn_agent` → 模型看到 `daemon rejected spawn: invalid_state: a settled parent task (state: completed) cannot spawn descendants`。
- 父任务处于 `recovery_required`（daemon 重启后未 resume）→ `spawn_agent` 仍可成功。
