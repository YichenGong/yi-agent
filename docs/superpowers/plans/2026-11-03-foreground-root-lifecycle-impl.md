# 前台会话根生命周期 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复前台 application root 被后台 watchdog 误结算而无法 spawn 的问题，并提供有权限、停止证明和用户确认的历史 stalled root 恢复。

**Architecture:** 在现有 guardian 租约体系中补齐 root 授权发行、私有授权桥和 daemon 原子准入，再将前台容器与 worker 的预算策略分离。活动快照由有代际的 driver 上报，不充当心跳。恢复通过冻结、停止证明、预览确认和事务 CAS 产生后继 attempt，不重跑旧 child。

**Tech Stack:** Rust workspace、Tokio、rusqlite/SQLite、Unix socket/私有 FD、现有 guardian/managed process registry、React/TypeScript/Vitest；仅 mock provider。

## Global Constraints

- 设计权威：[spec](../specs/2026-11-03-foreground-root-lifecycle-design.md)，基线 `30de1c65`。
- 不放宽 `ParentSettled`，不通过重启 daemon、改 SQLite 或提高 idle 阈值解决。
- 不新增第二套客户端心跳、租约或跨进程 owner 身份体系；补齐现有体系的授权/回收通路。
- 本次完整保证限定于 supported managed 三端入口。
- 当前心跳间隔 5 秒、租约窗口 30 秒，仍以既有实现为准。
- release/expiry 按既有 `10s grace + 5s confirm` 总窗口回收，不按任务数量乘预算。
- 恢复确认 token（60 秒）；幂等结果随 root 历史保留，不因 token 到期删除。
- 不部署产物、不迁移正在运行的 unmanaged 实例、不调用真实 LLM。
- child、孙任务、scheduled root 保留原 watchdog；legacy 不获得永久豁免。
- 所有变更在 worktree 新分支；不得在 main 直接修改。执行本计划时重新加载 using-git-worktrees 并验证所在分支。
- 每个任务先 RED 再 GREEN；缺接口导致编译失败可以作为第一轮 RED，但须再证明行为断言在实现缺失时失败。
- 各任务提交前必须同步对应 project-management 证据，并执行 `cargo fmt --all`、`just ci`；最终合并结果再跑 `just ci`。
- 中间提交不发布 `foreground_root_lifecycle_v1` 能力；只有全部闭环完成、验收通过才广告该能力。
- 本文件是实施计划，不是执行记录。所有步骤均未执行，不把计划中的 PASS 当作已有结果。

---

## 0. 文件结构与依赖

以下路径相对仓库根。`Create` 是计划新增，其他均沿用现有模块职责；不顺手重构大型 main/runtime。

| 路径 | 职责 |
|---|---|
| Create `yi-agent-rs/crates/yi-agent-lifecycle/src/foreground.rs` | 跨进程 DTO、栅栏、活动阶段、脱敏秘密包装、能力名 |
| Modify `yi-agent-rs/crates/yi-agent-lifecycle/src/{lib.rs,types.rs,client.rs}` | 导出 DTO、控制协议授权、宿主认证客户端 |
| Create `yi-agent-rs/crates/yi-agent-store/src/foreground.rs` | 分类/活动纯规则与结构化错误；不调用网络/worker |
| Create `yi-agent-rs/crates/yi-agent-store/src/repository/foreground.rs` | 绑定版本、活动、恢复操作、停止证据的持久事务 |
| Modify `yi-agent-rs/crates/yi-agent-store/src/repository/{mod.rs,helpers.rs,application_root.rs,watchdog.rs}` | migration、旧库分类、watchdog 双层保护与 startup sweep |
| Create `yi-agent-rs/crates/yi-agent-store/src/runtime/foreground.rs` | coordinator root 绑定、活动、恢复编排；由现有 runtime.rs 声明子模块 |
| Create `yi-agent-rs/crates/yi-agent-store/src/ipc/foreground.rs` | 私有授权桥 daemon 端与新业务 handler |
| Modify `yi-agent-rs/crates/yi-agent-store/src/{lib.rs,lifecycle.rs,runtime.rs}` | 导出、原子准入 gate、worker start/release/attach 接线 |
| Modify `yi-agent-rs/crates/yi-agent-store/src/ipc/{mod.rs,protocol.rs,daemon.rs,client.rs}` | 协商、路由、startup 与有界请求 |
| Create `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/root_authority.rs` | guardian root 授权表、私有桥发送、撤销确认；与工具 spawn FD 分离 |
| Modify `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/{mod.rs,guardian.rs,bringup.rs,reclaim.rs,processes.rs}` | 注册/心跳/释放/重启接线和停止证明 |
| Create `yi-agent-rs/crates/yi-agent-subagent/src/foreground.rs` | 三端共享 driver 绑定、活动保序及恢复宿主客户端 |
| Modify `yi-agent-rs/crates/yi-agent-subagent/src/{lib.rs,attach.rs,binding.rs,thread_root.rs}` | 身份缓存、retry 策略、窄 capability 和 bootstrap |
| Modify `yi-agent-rs/crates/yi-agent-app-server/src/server/driver/{mod.rs,turn_driver.rs}` | 每 turn 生命周期、finally、driver 关闭与宿主恢复路由 |
| Modify `yi-agent-rs/crates/yi-agent/src/{frontend_lease.rs,managed_runtime.rs,main.rs,tui/slash.rs}` | TUI/headless lease、驱动与宿主恢复动作，不新增独立CLI接管 |
| Create `desktop/src-tauri/src/foreground_root.rs` | 壳内 grant 使用与确认恢复命令，向 sidecar 仅传窄授权 |
| Modify `desktop/src-tauri/src/{lib.rs,runtime_lifecycle.rs,bridge.rs}` | 壳/sidecar 宿主通道与 Tauri command 注册 |
| Create `desktop/src/components/ForegroundRootRecovery.tsx` | 原因/影响/阻塞展示及用户确认，不保存秘密 |
| Create `desktop/src/components/ForegroundRootRecovery.test.tsx` | 取消、确认、过期、失败与权限展示 |
| Modify `desktop/src/App.tsx` | 挂载会话恢复动作，不改变 Kanban 状态机 |
| Tests `yi-agent-rs/crates/yi-agent-store/tests/{runtime_coordinator.rs,runtime_ipc.rs}` | C1–C11、C15–C16 的真实 coordinator/socket 覆盖 |
| Create `yi-agent-rs/crates/yi-agent-supervisors/tests/foreground_root_lifecycle.rs` | guardian↔daemon 的私有桥/回收集成 |
| Create `yi-agent-rs/crates/yi-agent-store/tests/foreground_support/mod.rs` | 仅测试夹具，真实临时数据库/daemon、可控 worker、时钟/屏障 |
| Modify `docs/project-management/{subagent-runtime.md,README.md}` | 逐任务证据，最终 C1–C16 完成及索引计数 |

新增文件若依赖另一 crate 的 DTO，只从 lifecycle 导入；禁止 lifecycle 反向依赖 store/supervisors。
Task 1 必须在 `yi-agent-rs/crates/yi-agent-store/Cargo.toml` 添加
`yi-agent-lifecycle = { workspace = true }`、非默认 `[features] foreground-test-support = []`，
并更新 `yi-agent-rs/Cargo.lock`；基线 store 尚无此直接依赖，不能依赖传递导入。
feature只允许测试构造注入，不改变正式启用策略；Task 2/3 开始即用它测试新协议，避免先用hook后定义feature。
`yi-agent-lifecycle` 保持无 Tokio/rusqlite/core/provider 依赖。supervisors 的新桥测试使用同步 `#[test]`、
std thread/UnixStream 与真实服务循环，不使用其 manifest 未声明的 `#[tokio::test]`。
IPC 结构化错误需同时修改 `yi-agent-rs/crates/yi-agent-store/src/ipc/error.rs`，不能仅新增 Rust error enum。
Task 4 的启动登记还需修改 `yi-agent-rs/crates/yi-agent-core/src/subagent/{worker.rs,supervisor.rs}`；
Task 9 的生产私有 FD/能力接线需核对 `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/{roles.rs,managed_channel.rs}`，
不允许只在测试 reader 上接通。测试协议回归位于 `yi-agent-rs/crates/yi-agent-store/tests/subagent_ipc_protocol.rs`。
不引入新的外部依赖。测试模块用 `#[path = "foreground_support/mod.rs"] mod foreground_support;` 显式引入。

依赖图：Task 1 → 2 → 3 → 4；Task 4 → 5、6；Task 6 → 7；Task 2/6/7 → 8；Task 3/6/7 → 9；Task 8/9 → 10；全部 → 11。
Task 6 只实现首次 driver 绑定、同 operation 传输续接及拒绝未证明接替；真正接替由 Task 7 完成，不能前置依赖未实现证明。
Task 5 用 Task 4 的 coordinator 夹具，不依赖 Task 6 的宿主客户端；Task 6 再将同一夹具切换到真实 driver fence。
这些是一个纵向闭环。Task 2–10 的生产行为均由内部 `ForegroundActivation::Disabled/Enabled` 开关控制，
正式入口直到 Task 11 前固定 Disabled，保持旧语义；测试构造入口可注入 Enabled，但不能从环境变量/公开 RPC 开启。
新内部协议 handler 和分类 migration 可先测试，未启用时不能改变 legacy/生产 watchdog 或回收行为。
Task 11 切换正式构造参数后重跑全部门禁；能力字符串不是替代执行开关。

## 1. 统一接口字典

以下是**新增接口契约**，不是声称基线已存在。实现若要改名/改类型，先同步本表和所有调用者。
Rust 示例省略的只有常规 `use`/derive；字段与错误语义不得省略。

### lifecycle::foreground（Task 1）

```rust
pub const FOREGROUND_ROOT_CAPABILITY: &str = "foreground_root_lifecycle_v1";

pub struct RootRef {
    pub session_id: String,
    pub root_id: String,
    pub attempt_id: String,
}
pub struct ForegroundFence {
    pub root: RootRef,
    pub generation: String,
    pub binding_epoch: u64,
    pub driver_epoch: u64,
}
pub enum ActivityPhase {
    Idle, Running, WaitingForChildren, WaitingForPermission, Unknown,
}
pub struct ActivityUpdate {
    pub fence: ForegroundFence,
    pub turn_id: Option<String>,
    pub seq: u64,
    pub phase: ActivityPhase,
}
pub struct ActivitySnapshot {
    pub update: ActivityUpdate,
    pub changed_at: String, // RFC3339，仅观测，不参与存活授权
}
pub struct DriverBinding {
    pub fence: ForegroundFence,
    pub last_seq: u64,
    pub driver_capability: SecretCapability,
}
pub struct RootBindIntent {
    pub operation_id: String,
    pub thread_key: String,
    pub workspace: std::path::PathBuf,
    pub extra_roots: Vec<std::path::PathBuf>,
    pub previous_root_capability: Option<SecretCapability>,
}
pub enum RootAuthorityScope { Business, RecoveryOnly }
pub struct AuthorityStamp {
    pub scope_key: String,
    pub generation: String,
    pub owner: String,
    pub root_id: String,
    pub binding_epoch: u64,
    pub authority_seq: u64,
    pub challenge_nonce: String,
    pub remaining_lease_ms: u64,
    pub authority_scope: RootAuthorityScope,
}
pub enum AuthorityMessage {
    Install { stamp: AuthorityStamp, authorization: crate::types::RootAuthorization },
    Renew { stamp: AuthorityStamp },
    RevokeOwner { owner: String, generation: String, authority_seq: u64 },
    Drain { generation: String, authority_seq: u64 },
}
pub struct RecoveryRequest {
    pub expected: ForegroundFence,
    pub operation_id: String,
}
pub struct RecoveryImpact {
    pub root: RootRef,
    pub stopped_execution_ids: Vec<String>,
    pub blocked_execution_ids: Vec<String>,
    pub child_ids: Vec<String>,
    pub version: u64,
    pub reason: String,
}
pub struct RecoveryPreview {
    pub request: RecoveryRequest,
    pub impact: RecoveryImpact,
    pub confirmation_token: Option<SecretCapability>, // 停止未确认时 None
    pub expires_at: Option<String>, // 仅签发确认token后有值，Blocked/Preparing不伪造过期时间
}
pub struct RecoveryResult {
    pub operation_id: String,
    pub previous: RootRef,
    pub successor: RootRef,
    pub binding_epoch: u64,
    pub driver_epoch: u64,
}
```

`SecretCapability` 新类型提供 `new(String) -> Self` 和 `expose(&self) -> &str`，Clone/serde 可用，
Debug 只能 `<redacted>`；不实现 Display，不进入转录或持久前端状态。授权/driver capability 不交给 UI；
恢复 confirmation token 仅由可信宿主交到当前确认组件的短期内存，不进入全局状态/localStorage。
上述 DTO 的 Debug 复用它。`RootAuthorization` 仍为现有独立授权秘密，不混用 driver/task capability。
涉及比较的 DTO 派生 PartialEq/Eq（不含浮点字段）；用于 SQL/version 的 u64 必须 checked 转换到 SQLite i64，
溢出返回结构化错误，不能截断或 wrap。

### store::foreground / coordinator（Tasks 1–8）

```rust
pub enum RootExecutionClass { Execution, ForegroundPending, ForegroundManaged, Corrupt }
pub enum ActivityApply { Applied, Duplicate, Stale }
pub enum ForegroundError {
    Unsupported, AuthorityDenied, AdmissionClosed, StaleAttempt, StaleDriver,
    InvalidActivity, SequenceConflict, RecoveryRequired, RecoveryAuthorityMissing,
    StopUnproven(Vec<String>), ConfirmationRequired, ConfirmationExpired,
    OperationConflict, CorruptBinding,
}
pub fn worker_watchdog_applies(class: RootExecutionClass) -> bool;
pub fn reduce_activity(
    current: &ActivityUpdate, incoming: &ActivityUpdate,
) -> Result<ActivityApply, ForegroundError>;

// RuntimeCoordinator 的新增公开业务入口，认证参数为内存秘密而非 client id。
pub async fn bind_foreground_driver(
    &self, authorization: &RootAuthorization, operation_id: &str,
) -> Result<DriverBinding, RuntimeCoordinatorError>;
pub fn set_foreground_activity(
    &self, driver_capability: &SecretCapability, update: &ActivityUpdate,
) -> Result<ActivitySnapshot, RuntimeCoordinatorError>;
pub async fn prepare_foreground_recovery(
    &self, authorization: &RootAuthorization, request: &RecoveryRequest,
) -> Result<RecoveryPreview, RuntimeCoordinatorError>;
pub async fn recover_foreground_root(
    &self, authorization: &RootAuthorization, request: &RecoveryRequest,
    token: &SecretCapability,
) -> Result<RecoveryResult, RuntimeCoordinatorError>;
pub fn foreground_recovery_operation(
    &self, authorization: &RootAuthorization, root: &RootRef, operation_id: &str,
) -> Result<Option<RecoveryResult>, RuntimeCoordinatorError>;
pub fn get_foreground_activity(
    &self, authorization: &RootAuthorization, root: &RootRef,
) -> Result<ActivitySnapshot, RuntimeCoordinatorError>;
```

`foreground_recovery_operation` 的 root 参数用于定位稳定 root/session，attempt_id 只表示原操作上下文，
不能因 active attempt 已变化而拒绝读取已提交结果；先验证当前会话读权，再查原 operation。
`None` 仅表示未提交；prepare 仍在停止中时不能将 None 当作允许新 operation 的依据。
恢复幂等查重在事务中先于旧 attempt/token 检查，但始终晚于当前会话权限验证。
`get_foreground_activity` 仅查询，不修改 phase、续租或驱动绑定。

root 恢复只签发后继身份，不在幂等结果中持久化新秘密；成功后再次 bind driver 获取当前窄 capability。
所有 wire task/session/attempt 字符串在业务入口解析为现有强类型 id，解析失败返回 Validation。

### 1.1 wire、宿主边界与幂等状态（Task 3/6/8/9/10 共用）

桌面壳只依赖 lifecycle，不新增 store/subagent/supervisors 依赖。`ForegroundError` 是 store 内部错误；
Task 1 同时定义 §1.1 的 ForegroundErrorCode、ForegroundFailure、RecoveryOperationStatus 等全部 wire DTO；
Task 3 添加协议 variant/codec，Task 8 才实现恢复行为。提前定义类型不允许提前启用或返回虚假成功。
其 wire 对应类型 `lifecycle::foreground::ForegroundErrorCode` 使用同名无载荷 variant，
StopUnproven 的实体单独放 `ForegroundFailure.blocked_execution_ids: Vec<String>`，reason 只含白名单文案。
`ForegroundFailure` 派生 serde，包含 code、blocked_execution_ids、reason；IPC 在 Task 3 同步修改
protocol.rs/error.rs，新增 `IpcResponse::ForegroundFailure(ForegroundFailure)`。
transport/codec 错误保留在 client 的 Transport/Protocol 分支，不能假装 AuthorityDenied。
`ForegroundClientError::Rejected` 使用 lifecycle 的 ForegroundFailure，不迫使壳引用 store 类型。

**业务 IPC 操作**（全部只在新能力协商通过后发送）：

| Request variant | 参数 | Response variant |
|---|---|---|
| BindApplicationRootDriver | authorization: RootAuthorization, operation_id: String | ForegroundDriverBound(DriverBinding) |
| SetApplicationRootActivity | driver_capability: SecretCapability, update: ActivityUpdate | ForegroundActivity(ActivitySnapshot) |
| GetApplicationRootActivity | authorization: RootAuthorization, root: RootRef | ForegroundActivity(ActivitySnapshot) |
| InspectApplicationRootRecovery | authorization: RootAuthorization, expected: ForegroundFence | ForegroundRecoveryImpact(RecoveryImpact) |
| PrepareApplicationRootRecovery | authorization: RootAuthorization, request: RecoveryRequest | ForegroundRecoveryOperation(RecoveryOperationStatus) |
| RecoverApplicationRoot | authorization: RootAuthorization, request: RecoveryRequest, token: SecretCapability | ForegroundRecoveryOperation(RecoveryOperationStatus) |
| GetApplicationRootRecoveryOperation | authorization: RootAuthorization, root: RootRef, operation_id: String | ForegroundRecoveryOperationLookup(Option<RecoveryOperationStatus>) |

Task 8 新增只读 `inspect_foreground_recovery(&self, authorization: &RootAuthorization,
expected: &ForegroundFence) -> Result<RecoveryImpact, RuntimeCoordinatorError>`，不冻结/停止/发token。
prepare/recover 的 coordinator 内部 async 方法可等待；**wire handler 不占 control reader 等待**，
先持久登记 operation，再交专用后台执行者，立即应答 Preparing。准备/确认共享同一个 operation id。

```rust
pub enum RecoveryOperationStatus {
    Preparing,
    NeedsPreview, // 重启后冻结/历史保留，token 丢失，需要在新授权下重新准备
    Blocked(RecoveryImpact),
    Ready(RecoveryPreview),
    Committed(RecoveryResult),
}
```

现有 `foreground_recovery_operation`（返回已提交 Option 结果）仅作 repository/coordinator 内部查重；
新增公开 `foreground_recovery_status(&self, authorization: &RootAuthorization, root: &RootRef,
operation_id: &str) -> Result<Option<RecoveryOperationStatus>, RuntimeCoordinatorError>` 用于 wire/宿主轮询。
None=此root不存在该operation；Preparing/Blocked/NeedsPreview 均不是恢复成功，也不允许静默更换id重做。
Ready 的 token 仅保留内存，Committed 的结果持久化。restart 将未提交操作投影为 NeedsPreview，
清空原token，维持冻结；相同 operation 重新准备时只更新其授权代际/影响版本，不新增 successor。
持久 request fingerprint 绑定稳定 root/session、原 attempt、恢复动作；generation/授权epoch 是每次提交的
准入栅栏，不作为跨代查询原结果的身份。相同 operation 改 root/attempt/动作必拒绝。
后台执行者和 token 表按 root/operation 限额，prepare 同 id 不重复停止，同root不同id竞争明确返回 OperationConflict。

**壳的控制入口**：在 lifecycle::types 新增 ControlRequest 的 `InspectRootRecovery`、
`PrepareRootRecovery`、`ConfirmRootRecovery`、`RootRecoveryOperation`，每种都携带 ClientGrant；
参数分别使用上表的 expected/request/root+operation，Confirm 带 token。guardian 核实当前 Business lease、
宿主本地会话映射与授权（历史接管还须持原 cap 并满足下述 RecoveryOnly 条件），再调用 daemon；
请求中的授权 scope 限制仍由 daemon 独立检查，不能把新的 Business lease 当作旧 root 的 Business 权限。
response 携带上述 lifecycle DTO。
app-server 不接触 grant。generic model tools 和远端任意 JSON-RPC 不注册这些宿主控制方法。
壳的异步 command 把 blocking ControlClient 工作交给专用 I/O 线程；poll 可取消但不取消已提交恢复。
控制通道每次请求只用现有短预算，不把 15秒停止等待塞进2秒控制往返，后台操作持有统一停止deadline。

**prepare 与首次/恢复授权**：原 owner 正常 Business；旧 owner 已过期而 host 持原 root capability 时，
guardian 可先发行 `RootAuthorityScope::RecoveryOnly`，只允许 inspect/freeze/stop/prepare/query，
不更改原owner业务绑定、不允许 bind driver/spawn/活动写/启动工具。这样停止流程不依赖已完成接管。
停止证明齐全后，在恢复 commit 的同一 daemon gate/CAS 中更新新owner绑定；但 guardian 与 SQLite
不是跨进程原子事务，不允许简单写成“两端同时升为 Business”。提交结果含非秘密 commit receipt
(root、operation、后继attempt、binding epoch、generation)；guardian 经私有桥查询已提交记录才签发新 Business
授权，daemon 安装成功前保持 admission closed。响应/进程丢失可用同operation读回 receipt 幂等补完授权，
不能再创建attempt；guardian先升权、提交失败回滚不了的顺序被禁止。旧 RecoveryOnly/cap 不能据此派生，
旧epoch消息仍拒绝。新grant/lease过期则不补发授权，root保持closed，重新授权仍需当前权限核验。
Task 8/9 补故障注入：commit后授权前断桥、授权安装后应答丢失、补完前lease过期，以及撤销与补完并发。
无原 cap 或异 owner 仍活跃则连 RecoveryOnly 也拒绝。task 工具cap不能换取该授权。

**待定活动发送**：ForegroundDriver 保留 `pending: Option<ActivityUpdate>`，发送前分配一次 seq；
响应含糊失败时保留同 seq/同 payload，不先跳号，不发送下一 turn 边界。重连先取当前快照，
同内容已提交则清 pending，否则仅重送该幂等活动；server版本/turn冲突时诊断并关闭本driver的委派。
finish未确认时下一普通聊天可以继续，但委派暂不可用，直到共享客户端完成保序重同步；
不得用“任意更大seq”强行覆盖旧turn。根Objective、TaskStarted与owner lease均不被重同步改写。

## Task 1: 建立分类与活动规则的可测试契约

**Files:** Create lifecycle/src/foreground.rs、store/src/foreground.rs；Modify 两 crate 的 lib.rs、
`yi-agent-rs/crates/yi-agent-store/Cargo.toml`、`yi-agent-rs/Cargo.lock`。
路径前缀见 §0。Tests 放各新模块 `#[cfg(test)] mod tests`。

**Interfaces:** Produces §1 DTO、`worker_watchdog_applies`、`reduce_activity`、`ForegroundError`。
Consumes 现有 RootAuthorization 和 serde；不接数据库、不启用能力。

- [ ] **Step 1: 写失败单元测试。** 分类包含 Execution/Pending/Managed/Corrupt 四种。
  在新模块内建立 `update(seq, turn, phase)` 辅助函数：固定 root/session/attempt/generation、两个 epoch 均为 1，
  仅由参数赋 seq/turn/phase；测试代码：

```rust
#[test]
fn foreground_root_lifecycle_old_turn_finish_is_rejected_even_with_larger_seq() {
    let current = update(9, Some("new"), ActivityPhase::Running);
    let late = update(10, Some("old"), ActivityPhase::Idle);
    assert!(matches!(reduce_activity(&current, &late), Err(ForegroundError::InvalidActivity)));
}
#[test]
fn foreground_root_lifecycle_only_execution_uses_worker_watchdog() {
    assert!(worker_watchdog_applies(RootExecutionClass::Execution));
    assert!(!worker_watchdog_applies(RootExecutionClass::ForegroundManaged));
    assert!(!worker_watchdog_applies(RootExecutionClass::ForegroundPending));
    assert!(!worker_watchdog_applies(RootExecutionClass::Corrupt));
}
```

  补固定断言：同 seq 同内容 Duplicate、异内容 SequenceConflict；低 seq Stale；旧 attempt/driver 拒绝；
  idle 开始新 turn、matching turn 结束成功；running 开始第二 turn 拒绝；SecretCapability Debug 不含原文。
  Pending/Corrupt 不用 worker watchdog 的前提是它们同时不可接受业务，Task 4 必须证明此不变量。

- [ ] **Step 2: 运行 RED。** 在 `yi-agent-rs/` 执行：

```bash
cargo test -p yi-agent-lifecycle foreground_root_lifecycle
cargo test -p yi-agent-store --lib foreground_root_lifecycle
```

  预期新接口未定义或活动/分类断言失败；记录真实失败原因，不接受过滤器 0 tests。

- [ ] **Step 3: 最小实现。** DTO 按 §1 建立；pure reducer 先比 fence、再比序号，再核验 turn 转移：

```rust
pub fn worker_watchdog_applies(class: RootExecutionClass) -> bool {
    matches!(class, RootExecutionClass::Execution)
}
// reduce_activity 的顺序固定：
// 1. root/attempt/generation/binding/driver 不符即拒绝；
// 2. seq < current => Stale；seq == current => Duplicate 或 SequenceConflict；
// 3. current 非 Idle 时新 turn 必须等当前 matching finish；Unknown 只能经可信停止/重绑定重置为 Idle；
// 4. Idle finish 的 incoming.turn_id 必须等于当前 turn_id；
// 5. waiting phase 必须关联当前非空 turn；通过返回 Applied。
```

  Unknown 仅由daemon在driver控制连接失效/重启时生成，公开SetActivity携带Unknown必须拒绝。
  同一epoch Unknown不能直接begin新turn；Task 7证明旧执行停止并bind后由服务器初始化Idle。
  时间戳不参与准入；Duplicate/Stale不刷新changed_at。seq/epoch递增均checked_add，溢出拒绝，不能wrap。

- [ ] **Step 4: 重跑 Step 2 至 GREEN。** 检查每个分支有断言且不读取真实时钟。
- [ ] **Step 5: 更新模块证据并提交。** 保持总 feature `[ ]`；每次提交都先：

```bash
(cd yi-agent-rs && cargo fmt --all && just ci) &&
git add yi-agent-rs/crates/yi-agent-lifecycle/src yi-agent-rs/crates/yi-agent-store/src \
  yi-agent-rs/crates/yi-agent-store/Cargo.toml yi-agent-rs/Cargo.lock docs/project-management &&
git commit -m "feat: define foreground root lifecycle contracts"
```

## Task 2: 持久分类、活动、恢复幂等与安全 startup

**Files:** Create repository/foreground.rs；Modify repository/{mod.rs,helpers.rs,application_root.rs,watchdog.rs}、
ipc/daemon.rs；Test runtime_coordinator.rs 及 repository 模块单测。

**Interfaces:** Consumes §1 DTO/classification。Produces repository 方法：

```rust
pub fn foreground_class(&self, root: &TaskId) -> Result<RootExecutionClass, RepositoryError>;
pub fn foreground_activity(&self, root: &TaskId) -> Result<Option<ActivitySnapshot>, RepositoryError>;
pub fn apply_foreground_activity(&mut self, update: &ActivityUpdate)
    -> Result<ActivitySnapshot, RepositoryError>;
pub fn foreground_recovery_result(&self, root: &TaskId, operation: &str)
    -> Result<Option<RecoveryResult>, RepositoryError>;
```

`ForegroundError` 纳入 RepositoryError/RuntimeCoordinatorError 的结构化 variant，不只 to_string 丢类型。

- [ ] **Step 1: 写 migration/reopen 失败测试。** 在临时旧 schema DB 建一个 application attachment
  和一个没有 attachment 的 scheduled root；迁移两次、关库重开，断言：

```rust
assert_eq!(repo.foreground_class(&application)?, RootExecutionClass::Execution); // 旧 attachment 是 legacy 策略
assert_eq!(repo.foreground_class(&scheduled)?, RootExecutionClass::Execution);
assert!(repo.foreground_activity(&application)?.is_none());
assert!(repo.foreground_recovery_result(&application, "not-created")?.is_none());
```

  使用现有 RuntimeRepository 创建任务/attachment 的测试方式，不能通过向测试库写“已授权”绕过后续认证测试。
  再测：孤立 attachment 为 Corrupt；旧 stalled terminal_json 不变；通用 startup sweep 仅排除启用模式下
  已声明 pending/managed 的 root，legacy与后台执行保持基线行为；child 的原恢复行为保持。
  使用 Task 2 repository 测试 API 建立 managed 身份行只验证迁移/hydration，不宣称它已通过 Task 3 的授权。

- [ ] **Step 2: 运行 RED。**

```bash
cargo test -p yi-agent-store --lib foreground_root_lifecycle
cargo test -p yi-agent-store --features foreground-test-support --test runtime_coordinator foreground_root_lifecycle
```

- [ ] **Step 3: 最小 migration 与存储实现。** 在 helpers.rs 的现有迁移链添加实施时下一个版本，更新
  LATEST_SCHEMA_VERSION 和 schema compatibility 测试。新增表的逻辑键与唯一性：

```sql
-- schema 片段为本任务新增字段契约；外键按现有 tasks/attempts 主键引用。
CREATE TABLE foreground_root_state (
  root_task_id TEXT PRIMARY KEY REFERENCES tasks(id),
  binding_epoch INTEGER NOT NULL CHECK(binding_epoch >= 0),
  driver_epoch INTEGER NOT NULL CHECK(driver_epoch >= 0),
  owner_client_id TEXT,
  admission_state TEXT NOT NULL CHECK(admission_state IN ('closed', 'frozen')),
  driver_bind_operation_id TEXT,
  activity_json TEXT,
  classification TEXT NOT NULL CHECK(classification IN ('legacy', 'pending', 'managed'))
);
CREATE TABLE foreground_recovery_operations (
  root_task_id TEXT NOT NULL REFERENCES tasks(id),
  operation_id TEXT NOT NULL,
  expected_attempt_id TEXT NOT NULL REFERENCES attempts(id),
  request_fingerprint TEXT NOT NULL,
  original_request_json TEXT NOT NULL,
  phase TEXT NOT NULL CHECK(phase IN ('preparing','needs_preview','blocked','ready','committed')),
  impact_json TEXT,
  result_json TEXT,
  PRIMARY KEY(root_task_id, operation_id),
  CHECK((phase = 'committed') = (result_json IS NOT NULL))
);
CREATE TABLE foreground_stop_evidence (
  root_task_id TEXT NOT NULL REFERENCES tasks(id),
  attempt_id TEXT NOT NULL REFERENCES attempts(id),
  execution_id TEXT NOT NULL,
  evidence_json TEXT NOT NULL,
  PRIMARY KEY(root_task_id, attempt_id, execution_id)
);
```

  classification、owner_client_id（非秘密可空）、admission_state（closed/frozen）、driver_bind_operation_id
  一起持久化；open 只作运行时状态，重启一律 closed。migration 将旧 attachment 记为 legacy：
  仍可识别它是前台会话，但 `foreground_class` 的预算策略返回 Execution，保留原 watchdog/旧接入语义。
  只有认证 prepare 才将该 root 转 ForegroundPending，成功 bind 才变 ForegroundManaged；
  若原来有活动执行，必须先完成停止边界，不能迁移一条分类就把运行任务变为无限容器。
  已经 managed 的行在重启保留 managed 身份，授权表为空、准入关闭；不能回填为 Execution。
  activity 读取历史时生成 Unknown 的当前视图，保留原快照用于诊断，不将历史 running 视作活跃 turn。
  startup sweep 仅在启用模式排除 pending/managed root，legacy与后台执行保留原策略；schema preflight 先于迁移。
  不要写 cap/grant/confirmation token 到新表。

- [ ] **Step 4: 重跑 Step 2、现有 startup/schema 测试至 GREEN。** 比较 migration 前后旧 attempt 与 child rows。
- [ ] **Step 5: fmt、just ci、登记测试并提交。** message：`feat: persist foreground root lifecycle facts`。

## Task 3: 接通 guardian 的可信授权桥与 bootstrap

**Files:** Create supervisors/lifecycle/root_authority.rs、store/ipc/foreground.rs、
supervisors/tests/foreground_root_lifecycle.rs；Modify lifecycle/{types.rs,client.rs}、
supervisors/lifecycle/{mod.rs,guardian.rs,bringup.rs,reclaim.rs}、store/ipc/{mod.rs,protocol.rs,daemon.rs}、
store/runtime/foreground.rs（Create）和 runtime.rs（声明子模块）。

**Interfaces:** Consumes RootBindIntent/RootAuthorization/AuthorityMessage。Produces：

```rust
// yi_agent_lifecycle::client::FrontendLease：仅宿主调用
pub fn authorize_root(&mut self, intent: &RootBindIntent, deadline: Deadline)
    -> std::io::Result<RootAuthorization>;
// guardian::root_authority；由租约验证后的 handler 调用
pub fn authorize_root(&mut self, grant: &ClientGrant, intent: &RootBindIntent, deadline: Deadline)
    -> std::io::Result<RootAuthorization>;
// daemon 私有 FD reader：不是公开业务 IPC variant
pub fn apply_authority_message(&self, message: AuthorityMessage)
    -> Result<(), RuntimeCoordinatorError>;
```

私有桥用独立 UnixStream/受控继承 FD，在 bringup 创建并固定 scope/generation/guardian peer；
认证消息只允许此 reader 构造内部 VerifiedAuthority，不接受公开 JSON 的 verified 标志。
所有请求沿用 deadline 总预算，不为每一 hop 重置。

- [ ] **Step 1: 写真实 socket 失败测试。** 在 supervisors 新集成文件内启动真实 guardian/control loop 与 daemon listener，
  采用 `tests/guardian_control.rs::identity_does_not_create_a_runtime` 的 `serve`+std thread+socket readiness 方式。
  `tests/managed_bringup.rs` 的 ScriptedProbe/RecordingLauncher 是模拟对象，不能当作真实授权桥证据。
  夹具用独立服务线程、真实 UnixStream::pair 和临时 Daemon::start_with_factory；进程继承 FD 的生产路径
  另由 Task 9 的现有 process_fixture helper re-exec 测试验证，不谎称同进程服务测试等于跨进程继承测试。添加以下命名用例（都带 foreground_root_lifecycle 前缀）：
  `business_lease_binds_root`、`public_key_cannot_retrieve_capability`、`lease_only_denied`、
  `tool_channel_cannot_install_authority`、`bridge_eof_closes_admission`、`bootstrap_before_business_binding`。
  关键断言使用本任务测试文件中的 `BridgeFixture`：`start() -> Self` 启动临时真实服务线程/socket；
  `authorize_business(&RootBindIntent) -> io::Result<RootAuthorization>`、
  `authorize_lease_only(&RootBindIntent) -> io::Result<RootAuthorization>` 都经真实 control client；
  `public_attach(&str) -> Result<(), BridgeFixtureError>` 将公开业务响应解析为结构化拒绝；
  `attempt_tool_authority_install() -> Result<(), BridgeFixtureError>` 从工具 FD 发授权帧并解析拒绝。
  本测试文件定义 `enum BridgeFixtureError { Rejected(ForegroundFailure), Transport(std::io::Error), Protocol(String) }`；
  各helper只把明确 wire 拒绝转 Rejected，其他错误保留分支，避免把 EOF/codec失败当作权限防护通过。
  测试 helper 只收发协议，不判断/模拟租约是否合法。测试代码：

```rust
#[test]
fn foreground_root_lifecycle_public_keys_and_lease_only_cannot_authorize() {
    let mut f = BridgeFixture::start();
    let intent = RootBindIntent {
        operation_id: "bind-1".into(), thread_key: "thread:test".into(),
        workspace: f.workspace().to_owned(), extra_roots: vec![],
        previous_root_capability: None,
    };
    let auth = f.authorize_business(&intent).unwrap();
    assert!(!auth.root_id.is_empty());
    assert_ne!(auth.capability, intent.thread_key);
    assert!(matches!(f.public_attach(&intent.thread_key),
        Err(BridgeFixtureError::Rejected(ForegroundFailure { code: ForegroundErrorCode::AuthorityDenied, .. }))));
    assert_eq!(f.authorize_lease_only(&intent).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
    assert!(matches!(f.attempt_tool_authority_install(),
        Err(BridgeFixtureError::Rejected(ForegroundFailure { code: ForegroundErrorCode::AuthorityDenied, .. }))));
}
```

  `workspace(&self) -> &Path` 返回 fixture 的临时 Git 仓库；协议不兼容/EOF 不是权限拒绝的替身，
  helper 应区分 Transport/Protocol 错误并让测试失败。夹具创建一个 scope、两个 business owner
  和一个 LeaseOnly，用现有 control request 注册；仅查询 Identity 不能增加租约计数。

- [ ] **Step 2: 运行 RED。**

```bash
cargo test -p yi-agent-supervisors --features yi-agent-store/foreground-test-support --test foreground_root_lifecycle
cargo test -p yi-agent-lifecycle --test control_codec
cargo test -p yi-agent-lifecycle --test control_client
```

- [ ] **Step 3: 实现 prepare/bind 与私有桥。** 控制协议新增授权请求/响应，不把它注册为工具：

```rust
// ControlRequest 新 variant
AuthorizeRoot { grant: ClientGrant, intent: RootBindIntent }
// ControlResponse 新 variant
RootAuthorized(RootAuthorization)
```

  guardian 先验证 Business 活跃租约→daemon 准备/定位 pending root→guardian 验 scope/workspace、
  既有 owner 或原 capability→私有桥安装精确授权→daemon CAS 提交绑定→返回窄授权。
  新 root 只在此成功后为 managed；失败 pending 无执行、无 lease，deadline 到期或 prepare 连接失败时立即幂等撤销本次 pending reservation；
  daemon 启动时扫描清除未提交 pending。不假设基线存在通用“临时对象清理周期”。
  同 operation id 同 intent 幂等；异参数拒绝。已经 managed 的 root 禁止旧公开 attach 路径返 capability，
  也禁用 deterministic legacy capability fallback；legacy-only root 维持旧策略。
  Release/Expire/Drain 首先在 guardian 租约表标记 closing，禁止新授权，然后在不持租约表锁的情况下
  向桥发送有序撤销并等关闭确认。任何旧 Install/Renew 的 seq 小于等于 tombstone 均拒绝；
  同 seq 同 payload 仅幂等应答，不更新本地截止。桥 EOF 即关闭新准入。
  剩余租期不能按 receipt_time + remaining_lease_ms 计算：daemon 在 prepare 或续接 challenge 发出前
  记录本地单调 t0，guardian 回包携带原 nonce 与“验证时剩余租期”，daemon 以 t0 + remaining_lease_ms
  作保守截止。nonce 必须未消费、属于当前桥/generation；回包到达已过此截止则拒绝，避免队列延迟续命。
  每次现有 heartbeat 成功后可通知 daemon 请求一次新 challenge，完成验证再更新期限；该请求本身
  不刷新 guardian LeaseTable，不新建续租线程。不持 guardian 租约表锁等待 daemon，以免双向桥死锁。
  无新 heartbeat 时不能仅靠反复 challenge 保持 owner 存活。
  Resume 轮换控制凭据但不自动轮换 root binding epoch。授权与工具 managed_spawn 分 FD、分 DTO。
  未全部完成时 Identity 不广告完整新能力；新 variant 只在兼容探测通过后发送。
  本任务负测试必须加入：延迟超过 TTL 的 Install、撤销后的旧 Renew、重复同 seq 不延长截止、
  prepare 返回前 owner 已 release、guardian 等待 daemon 时并行 expiry 仍能关闭准入。

- [ ] **Step 4: 重跑 Step 2 至 GREEN。** 再跑已有 guardian_control/client_leases；断开真实 bridge，
  证明不能仅修改两个模拟表就通过。桥撤销失败必须返回回收未完成，不报告成功。
- [ ] **Step 5: fmt、just ci、登记 C13/C14 证据并提交。** message：`feat: bridge guardian authority to foreground roots`。

## Task 4: 将派发、撤销与提交变成原子准入

**Files:** Modify store/{runtime.rs,lifecycle.rs}、store/runtime/foreground.rs、
store/repository/admission.rs、supervisors/lifecycle/reclaim.rs；Tests runtime_coordinator.rs/runtime_ipc.rs。
Create tests/foreground_support/mod.rs，为后续测试提供真实 DB/daemon 与可控 worker。

**Interfaces:** Consumes Task 3 的已验证授权，不能接受任意 `owner: &str` 冒充认证。
Produces coordinator 内部 `ForegroundCommitGate` 与 `AdmissionStamp`，后者含 root/attempt/generation/
binding epoch/authority sequence；仅 gate 校验后创建，不对 wire 暴露构造器。

测试夹具 `ForegroundFixture` 在本任务定义（后续只调用这些方法）：

```rust
pub async fn managed() -> Self; // 临时 DB + workspace + mock factory，安装测试专用私有桥
pub async fn spawn(&self, objective: &str) -> Result<TaskId, RuntimeCoordinatorError>;
pub async fn release_owner(&self) -> Result<(), RuntimeCoordinatorError>;
pub async fn wait_owner_admission_closed(&self);
pub fn hold_before_admission_commit(&self) -> TestBarrier;
pub fn hold_after_admission_commit(&self) -> TestBarrier;
pub fn child_ids(&self) -> Vec<TaskId>;
pub fn assert_all_owned(&self);
pub fn assert_no_started_workers(&self);
```

`TestBarrier` 用两个 tokio oneshot 通知 arrived/release；`arrived(&mut self).await` 等待到达、
`release(self)` 放行。集成测试编译 library 时不会启用 library 的 cfg(test)，因此不能把这些 hook
仅藏在 cfg(test)。沿用Task 1已添加的非默认 `foreground-test-support` feature；所有测试构造入口/
hook 在 cfg(any(test, feature = "foreground-test-support")) 下导出，生产默认无 hook。
以下 runtime_coordinator/runtime_ipc 的新用例命令必须加 `--features foreground-test-support`。
基线 `just test` 已调用 `cargo nextest run --all-features --workspace --profile ci`，lint 也带 --all-features，
因此无需另建 CI 入口或改写16道门禁；Task 4 提交前核实 nextest 列表确实收录这些集成用例。
feature 只开放 fixture/hook 构造，不自动启用生产 ForegroundActivation，否则 --all-features 会改变运行语义。
fixture 运行 coordinator 的真实 spawn/release 代码，不在 helper 重实现准入规则；服务/worker
Drop 时有界 cancel+join，TempDir 必须比服务线程活得更久。跨进程授权真实性仍由 Task 3/9 证明。

- [ ] **Step 1: 写确定性竞态失败测试。**

```rust
#[tokio::test]
async fn foreground_root_lifecycle_release_before_commit_refuses_spawn() {
    let f = std::sync::Arc::new(ForegroundFixture::managed().await);
    let mut barrier = f.hold_before_admission_commit();
    let spawning = { let f = f.clone(); tokio::spawn(async move { f.spawn("late").await }) };
    barrier.arrived().await;
    f.release_owner().await.unwrap();
    barrier.release();
    assert!(matches!(spawning.await.unwrap(), Err(RuntimeCoordinatorError::AdmissionClosed(_))));
    assert!(f.child_ids().is_empty());
}
#[tokio::test]
async fn foreground_root_lifecycle_release_tracks_committed_pending_start() {
    let f = std::sync::Arc::new(ForegroundFixture::managed().await);
    let mut barrier = f.hold_after_admission_commit();
    let spawning = { let f = f.clone(); tokio::spawn(async move { f.spawn("admitted").await }) };
    barrier.arrived().await;
    let releasing = { let f = f.clone(); tokio::spawn(async move { f.release_owner().await }) };
    f.wait_owner_admission_closed().await;
    barrier.release(); // 不能先 await release，否则 release 正在等此 starting slot 停止会自锁
    let _outcome = spawning.await.unwrap(); // 已准入可返回child id或被取消错误，以下断言检查真实效果
    releasing.await.unwrap().unwrap();
    f.assert_all_owned();
    f.assert_no_started_workers();
}
```

  每个等待外包 `tokio::time::timeout(Duration::from_secs(5), ...)`，测试失败也释放屏障并join任务，
  防止RED阶段把整个测试进程挂住。`wait_owner_admission_closed` 读取真实gate状态watch，不能提前回报关闭。
  加 owner B 活跃不 drain、pending/corrupt root 拒派发、长 WaitAgent 不阻塞 release/status。

- [ ] **Step 2: 运行 RED。**

```bash
cargo test -p yi-agent-store --features foreground-test-support --test runtime_coordinator foreground_root_lifecycle
cargo test -p yi-agent-store --features foreground-test-support --test runtime_ipc foreground_root_lifecycle
```

- [ ] **Step 3: 实现短 gate。** 维持锁序 supervisor→commit gate→clients→repository：

```rust
// 业务编排模板；prepare 不启动 worker、不运行用户命令。
let prepared = prepare_non_executing_resources().await?;
let child = {
    let _commit = foreground_commit_gate.lock()?;
    let stamp = validate_current_authority_and_attempt()?;
    persist_child_and_owner_in_one_commit(stamp, prepared)?
};
// 释放 gate 后才做异步启动；启动必须再次验证 child 未取消/冻结。
start_only_if_still_admitted(child).await?;
```

  上述函数名原本只表示局部编排，实际实现必须增加 `StartingExecution` 启动登记，不能以二次检查代替同步：
  1. gate 内持久登记 child 与 owner 时，同时登记 cancellation token、attempt、StartWitness 的 starting slot；
  2. release/freeze 关闭准入时枚举 queued/starting/running 全部 slot，cancel starting 并等待其完成握手；
  3. factory 在 gate 外调用，即使尚未返回 WorkerHandle 仍被 starting slot 跟踪；返回 handle 后，
     若期间已关闭/取消，立即接管该 handle 的 join/工具停止证明，禁止发布为可工作；
  4. 若 worker 首次 provider/tool 执行可能先于 factory 返回，必须先通过该 slot 的启动许可，
     token 已取消时不能开始用户代码；已获许可的执行归为已接受工作，release 必须等其停止，不能漏枚举；
  5. factory 错误/被取消时必须完成 witness，证明确实无执行或提交停止结果，不能直接丢 starting slot。
  在 core 的 worker/supervisor 边界接入此握手，构造测试故意让 factory 在创建执行后阻塞返回。
  增加三个屏障：持久准入前、slot登记后、factory已有执行但handle未返回；逐一与 release 交错。
  工具组启动也须在 guardian registry 中先登记 reservation，再启动/交接，不让 freeze 与工具spawn之间漏出进程。
  release 在 gate 内关 owner、取已登记者集合，放锁后异步停止；watchdog/recovery 后续使用同一 gate。
  数据库失败不发布内存归属；已准备但未提交资源走已有清理，不泄漏 worktree lease。
  网络/WaitAgent/worker join 不持 gate，不持 supervisor 跨物理停止等待。
  注意第三种交错允许 release 前已被准入的 worker 短暂执行；要求是它始终被跟踪且不能在回收完成后运行，
  而不是错误承诺“release 一开始就从未产生过任何执行”。

- [ ] **Step 4: 重跑 Step 2 至 GREEN。** 每个竞态至少测两种交错，验证不是靠全局串行 WaitAgent。
- [ ] **Step 5: fmt、just ci、登记 C7 证据并提交。** message：`fix: make foreground root admission atomic`。

## Task 5: 隔离 worker watchdog 与前台容器

**Files:** Modify repository/watchdog.rs、runtime.rs、runtime/foreground.rs；Tests runtime_coordinator.rs/runtime_ipc.rs。
**Interfaces:** Consumes foreground_class、Task 4 gate；Produces 同一分类策略下的候选筛选与终态 CAS 保护。

扩展 ForegroundFixture（在本任务实现真实 repo/clock 操作）：

```rust
pub async fn advance_watchdog(&self, seconds: i64);
pub fn root_state(&self) -> String;
pub fn root_has_watchdog_terminal(&self) -> bool;
pub fn root_meaningful_progress_count(&self) -> usize;
pub async fn wait_children(&self) -> Vec<TaskId>;
```

`advance_watchdog` 用测试注入时钟推进 lease/heartbeat 与 watchdog：owner 存活测试按现有租约机制
模拟成功 heartbeat，不能把租约期限硬写无限或只推进 watchdog 时间。执行 worker 预算观测使用同一时间轴。

- [ ] **Step 1: 写回归 RED。**

```rust
#[tokio::test]
async fn foreground_root_lifecycle_survives_idle_and_wall_time_without_progress() {
    let f = ForegroundFixture::managed().await;
    f.advance_watchdog(301).await;
    assert!(f.spawn("first").await.is_ok());
    f.advance_watchdog(2400).await; // 总计 2701
    assert!(f.spawn("second").await.is_ok());
    assert!(!f.root_has_watchdog_terminal());
    assert_eq!(f.root_meaningful_progress_count(), 0);
}
```

  另测 root 等待 child；child 有进展且预算未尽则保活；child/孙任务/无 attachment scheduled root
  无进展仍 stalled/timeout；显式父取消仍级联。旧候选快照在 managed bind 后提交不得落终态。

- [ ] **Step 2: 运行 RED。**

```bash
cargo test -p yi-agent-store --features foreground-test-support --test runtime_coordinator foreground_root_lifecycle
cargo test -p yi-agent-store --features foreground-test-support --test runtime_ipc foreground_root_lifecycle
```

- [ ] **Step 3: 实现双层保护。** 候选 SQL 排除已确认 foreground 容器；最终提交在 gate 内用
  classification/expected attempt/binding epoch 再判。不能只修改 evaluate_watchdog 为“depth0跳过”。
  managed owner 失效走 revoke/reclaim，不退回 worker watchdog；legacy Execution 保留原判定。
  Pending/Corrupt 永不接受业务，不因没有 watchdog 而持有后台 worker。保留历史 watchdog 行，
  不插入 progress，不改 child limits，不改变 AgentConfig/provider/tool timeout。

```rust
match repository.foreground_class(task)? {
    RootExecutionClass::Execution => commit_worker_watchdog_terminal(),
    RootExecutionClass::ForegroundManaged => reject_container_watchdog_terminal(),
    RootExecutionClass::ForegroundPending | RootExecutionClass::Corrupt => reject_closed_root(),
}
```

- [ ] **Step 4: 重跑 Step 2 及现有 scheduler 测试至 GREEN。** 检查 usage/progress 数据无伪造写入。
- [ ] **Step 5: fmt、just ci、登记 C1/C3/C5/C6 并提交。** message：`fix: exclude managed foreground containers from worker watchdogs`。

## Task 6: driver epoch 与保序活动同步

**Files:** Modify runtime/foreground.rs、repository/foreground.rs、ipc/{protocol.rs,foreground.rs,daemon.rs}；
Create subagent/src/foreground.rs；Modify subagent/{lib.rs,thread_root.rs,binding.rs}。
Tests 新模块单测、runtime_ipc.rs。

**Interfaces:** Implements §1 `bind_foreground_driver`、`set_foreground_activity`。共享客户端新增：

```rust
pub struct ForegroundDriver;
impl ForegroundDriver {
    pub fn bind(root: std::sync::Arc<ThreadRoot>, authorization: RootAuthorization,
        operation_id: String) -> Result<Self, ForegroundClientError>;
    pub fn begin_turn(&mut self, turn_id: &str) -> Result<(), ForegroundClientError>;
    pub fn waiting(&mut self, phase: ActivityPhase) -> Result<(), ForegroundClientError>;
    pub fn finish_turn(&mut self, turn_id: &str) -> Result<(), ForegroundClientError>;
    pub fn last_snapshot(&self) -> &ActivityUpdate;
}
pub enum ForegroundClientError { Rejected(ForegroundFailure), Transport(String), Protocol(String) }
```

窄 capability 不暴露给 UI；`ThreadRoot` 的 application 写操作携带当前 fence/driver cap，
历史 root capability 留作会话归属授权，不能代替当前 driver 的写权限。

- [ ] **Step 1: 写失败测试。** bind 同 operation id 两次返回同 epoch/last seq；同 owner 新 driver
  未提交停止证明时一律拒绝（此任务不实现接替成功，成功分支放 Task 7）；旧 driver 的更大 seq finish 拒绝；
  matching turn finish 接受；活动不改 objective/TaskStarted。

```rust
let first = driver.last_snapshot().clone();
driver.begin_turn("turn-1")?;
driver.finish_turn("turn-1")?;
driver.begin_turn("turn-2")?;
assert_eq!(driver.last_snapshot().turn_id.as_deref(), Some("turn-2"));
assert!(driver.last_snapshot().seq > first.seq);
assert!(driver.finish_turn("turn-1").is_err());
```

  传输测试在服务器已提交但响应丢失时，以同 seq/operation 重送活动/bind；验证不重放 spawn/message。
  对 ThreadRoot::send_as_root 的 transport-liveness 分支补 SingleShot 负测试，它当前存在可能无条件重发的路径。

- [ ] **Step 2: 运行 RED。**

```bash
cargo test -p yi-agent-store --features foreground-test-support --test runtime_ipc foreground_root_lifecycle
cargo test -p yi-agent-subagent foreground_root_lifecycle
```

- [ ] **Step 3: 实现状态持久与客户端。** daemon 签发 driver epoch、last seq；只在证明旧 driver 停止
  后轮换。恢复/重启不依赖客户端从 0 开始猜序号。活动用 reducer + CAS 写单一快照；
  同 seq 异内容拒绝，旧 fence 拒绝，完成消息保留 matching turn id 供判重。
  ForegroundDriver 使用有界串行发送；只合并同 turn 中间 waiting，不跨 begin/finish 合并。
  transport 含糊失败的 SingleShot 只修连接/返回“结果未知，请先检查”，不自动重发副作用。

```rust
match retry_policy {
    RetryPolicy::Idempotent => resend_same_operation_and_payload(),
    RetryPolicy::SingleShot => return_ambiguous_result_without_resend(),
}
```

  拒绝 stale attempt 不自动恢复终态，不使用字符串匹配 InvalidState 触发 reattach/retry。

- [ ] **Step 4: 重跑 Step 2 至 GREEN。** 加活动请求失败不续租/取消 child 的断言。
- [ ] **Step 5: fmt、just ci、登记 C8/C11 并提交。** message：`feat: synchronize foreground driver activity with epoch fencing`。

## Task 7: 前台停止证明、接替与 owner 回收闭环

**Files:** Modify supervisors/lifecycle/{root_authority.rs,guardian.rs,processes.rs,reclaim.rs}、
store/runtime/foreground.rs、repository/foreground.rs、subagent/foreground.rs；Tests supervisors 新集成文件、runtime_coordinator.rs。

**Interfaces:** Produces lifecycle foreground DTO：

```rust
pub struct QuiescenceRef {
    pub fence: ForegroundFence,
    pub execution_id: String,
    pub evidence_id: String,
}
// wire 只传 reference；证据由 trusted host/guardian 私有通道安装，不能自报 stopped。
// store coordinator:
pub async fn freeze_foreground_root(&self, root: &RootRef)
    -> Result<RecoveryImpact, RuntimeCoordinatorError>;
pub fn verify_foreground_quiescence(&self, fence: &ForegroundFence)
    -> Result<(), RuntimeCoordinatorError>;
```

guardian 维护 driver/前台 turn/工具组的登记，登记绑定 owner/root/attempt/driver epoch；
采用已有 managed process 身份，禁止 PID-only 证明。冻结后新的前台工具组也不得启动。

- [ ] **Step 1: 写停止证明失败测试。** 用真实受管短命进程与可阻塞 mock worker：
  发 cancel 但不 join→StopUnproven；worker join 但工具组未停→StopUnproven；前台 root 无 worker
  且 host 已 await turn、工具停止→允许；无 registry 记录→拒绝；owner A release 不杀 B。

  本任务在 supervisors 的新集成文件定义 `StopScenario::{CancelOnly, JoinedToolAlive, HostAndToolsStopped, MissingEvidence}`，
  以及 `StopFixture::check(scenario: StopScenario) -> Result<(), ForegroundError>`：CancelOnly 使用未触发
  StopWitness 的可控worker；JoinedToolAlive 标记join但保留受管fixture工具进程；HostAndToolsStopped
  经可信host通道安装对应driver证明并等待tool退出；MissingEvidence 不安装记录。check 调真实核验API，
  只将实际错误映射到 ForegroundError，不在helper按scenario直接返回预期结果。

```rust
#[test]
fn foreground_root_lifecycle_stop_requires_host_join_and_tool_evidence() {
    for scenario in [StopScenario::CancelOnly, StopScenario::JoinedToolAlive, StopScenario::MissingEvidence] {
        assert!(matches!(StopFixture::check(scenario), Err(ForegroundError::StopUnproven(_))));
    }
    assert!(StopFixture::check(StopScenario::HostAndToolsStopped).is_ok());
}
```

  用 deadline 注入时钟检查总回收窗口不随 child 数量倍增；停止证明不可由公开业务 socket伪造。

- [ ] **Step 2: 运行 RED。**

```bash
cargo test -p yi-agent-supervisors --features yi-agent-store/foreground-test-support --test foreground_root_lifecycle
cargo test -p yi-agent-supervisors --test client_reclamation
cargo test -p yi-agent-store --features foreground-test-support --test runtime_coordinator foreground_root_lifecycle
```

- [ ] **Step 3: 实现冻结与证明。** gate 内关闭 root/driver 写准入、取得封闭执行集合；gate 外
  请求 host cancel+await turn、worker join、工具组终止；可信通道记录证据再核验集合版本。
  同进程内多个 driver 不是多个 OS 进程：一会话停止只 await 对应 driver task 与该 root/attempt/driver epoch 的
  tool groups，不 kill 承载其他会话的整个 app-server。只有 owner级release允许按该owner回收整组进程。
  新 driver 接替成功的端到端用例放本任务，实现 Task 6 预留的证明核验；首次绑定不要求“前任join”。
  旧 driver 若已死亡由 guardian 核实受管进程身份+工具组，不要求虚构 root worker join。
  evidence_json 必须包含完整原fence、execution集合版本、不可复用的进程/driver execution identity、
  join/工具停止结果和可信来源；重新启动同task或同PID不得复用旧证据。
  可用证据持久保存非秘密身份/退出结果，重启重新核验；没有记录不能断言已停。
  owner release 完成结果合并 guardian OS组与 daemon worker 停止结果，任一缺失都报不完整。
  root detach 同样停止其前台 turn，但不 release 同 App 的其他 root。

```rust
// guard 释放后才等待，不占 coordinator 提交 gate。
let scope = freeze_and_snapshot_execution_set()?;
let proofs = collect_host_worker_and_tool_proofs(&scope, deadline).await;
record_verified_evidence(&scope, proofs)?;
verify_same_frozen_execution_set(&scope)?;
```

  driver EOF 只撤销该 driver 写权并显示 Unknown；owner 活跃时不单凭 EOF 取消 child。
  接替 driver 需要本任务证明，桥失效关闭写准入，但不把短 UI/relay EOF 当作生命周期消息。

- [ ] **Step 4: 重跑 Step 2 至 GREEN。** 加进程退出后 PID 重用/证据身份不匹配拒绝测试；不杀用户真实进程。
- [ ] **Step 5: fmt、just ci、登记 C7/C15/C16 并提交。** message：`feat: verify foreground execution quiescence before rebinding`。

## Task 8: 预览确认与 stalled 后继 attempt 事务恢复

**Files:** Modify repository/foreground.rs、runtime/foreground.rs、ipc/{foreground.rs,protocol.rs,daemon.rs}、
subagent/foreground.rs；Tests runtime_coordinator.rs/runtime_ipc.rs。
**Interfaces:** Implements §1 inspect/prepare/recover/status；Consumes Task 7 停止证明与 Task 3 授权。
旧 owner恢复先获取 §1.1 RecoveryOnly，先冻结/停止再在恢复事务内完成新owner绑定，不要求提前 Business 接管。
新增负测试：RecoveryOnly 可 inspect/prepare/query，但 bind driver/spawn/活动写/工具启动全部拒绝。

扩展 ForegroundFixture：

```rust
pub async fn stalled_from_watchdog() -> Self; // 先按旧语义产生持久 stalled+evidence，再进入新授权流程
pub async fn prepare_recovery(&self, operation: &str) -> Result<RecoveryPreview, RuntimeCoordinatorError>;
pub async fn recover(&self, preview: &RecoveryPreview) -> Result<RecoveryResult, RuntimeCoordinatorError>;
pub fn attempt_count(&self) -> usize;
pub fn previous_terminal_json(&self) -> String;
pub fn worker_start_count(&self) -> usize;
```

historical fixture 使用测试专用旧快照导入，不能让生产接口接受“请直接设 stalled”。

- [ ] **Step 1: 写恢复 RED。**

```rust
#[tokio::test]
async fn foreground_root_lifecycle_recovery_is_idempotent_without_restarting_children() {
    let f = ForegroundFixture::stalled_from_watchdog().await;
    let old_terminal = f.previous_terminal_json();
    let old_children = f.child_ids();
    let starts = f.worker_start_count();
    let count = f.attempt_count();
    let p = f.prepare_recovery("recover-1").await.unwrap();
    let a = f.recover(&p).await.unwrap();
    let b = f.recover(&p).await.unwrap();
    assert_eq!(a, b);
    assert_eq!(f.attempt_count(), count + 1);
    assert_eq!(f.previous_terminal_json(), old_terminal);
    assert_eq!(f.child_ids(), old_children);
    assert_eq!(f.worker_start_count(), starts);
    assert_eq!(f.root_state(), "running");
}
```

  同时加参数化负用例：completed/failed/cancelled/recovery_required、无 watchdog evidence、
  缺原 cap、异活跃 owner、旧 generation/attempt、停止未确认、无确认/过期、同 operation 异参数。
  并发两 operation 竞争、响应丢失重送、提交前 rollback、提交后模拟进程重启各有测试。

- [ ] **Step 2: 运行 RED。**

```bash
cargo test -p yi-agent-store --features foreground-test-support --test runtime_coordinator foreground_root_lifecycle
cargo test -p yi-agent-store --features foreground-test-support --test runtime_ipc foreground_root_lifecycle
```

- [ ] **Step 3: 最小实现。** prepare 先鉴权再冻结/停止；返回阻塞实体，无证明不签 token。
  token 内存索引绑定 request/fence/impact version/expiry；宿主确认后 recover 在 gate+transaction 中：

```sql
-- 逻辑提交顺序；列名以现有 attempts/tasks schema 为准，必须沿用既有 insert_attempt 帮助函数。
-- 1. 查询 (root_task_id, operation_id)：同 fingerprint 返回 result，异 fingerprint 拒绝。
-- 2. 核对 expected attempt、stalled evidence、epoch、冻结集合、有效确认。
-- 3. INSERT successor attempt（number = old + 1，state running，无 worker）；
-- 4. CAS tasks.active_attempt_id，保持 objective/session/root 不变；
-- 5. 更新 foreground epoch、activity idle/turn NULL，写 operation result；COMMIT。
```

  只在 commit 成功后消费确认并发布内存 reducer；失败不留下半个 attempt。commit 后 publish
  失败则关准入、重载持久状态，不能直接返回成功。结果查询先重新认证再读持久幂等结果，
  已完成 operation 在 token 到期/daemon 换代后仍可查询，不需再消费旧 token。
  bind 新 driver 时发新秘密，拒旧 capability；恢复不是 resume_task/start_worker 的别名。
  旧 owner 过期后，可信 host 持原cap先由 guardian 获 RecoveryOnly，证明齐全后恢复事务才完成新owner绑定；
  无cap返回 root_recovery_authority_missing，不凭 key/cwd 接管。预览取消/过期保留已停止事实及旧终态，可再次 prepare。
  在真实socket测试中必须观察 Preparing→Ready/Blocked→Committed 的顺序，确认长准备期间
  guardian 能继续处理 Identity/Heartbeat/Release；查询已提交结果不因旧attempt/token过期被拒。
  对 prepare/recover/status 返回值的约束以 §1.1 为准，不把短响应超时等同回滚或停止失败。

- [ ] **Step 4: 重跑 Step 2 至 GREEN。** 用实际临时 SQLite 重开验证 successor、旧证据、幂等结果；
  新 turn bind 后可 spawn，旧 child 保持取消且旧 delivery/report 可读。
- [ ] **Step 5: fmt、just ci、登记 C9/C10/C11/C16 并提交。** message：`feat: recover stalled foreground roots through confirmed successor attempts`。

## Task 9: 接入 app-server、TUI、headless 的每回合生命周期

**Files:** Modify subagent/{attach.rs,binding.rs,foreground.rs,thread_root.rs}、
app-server/server/driver/{mod.rs,turn_driver.rs}、yi-agent/src/{frontend_lease.rs,managed_runtime.rs,main.rs}、
desktop/src-tauri/src/{runtime_lifecycle.rs,bridge.rs}；Create desktop/src-tauri/src/foreground_root.rs。
**Interfaces:** Consumes ForegroundDriver 与 host authorize_root。Produces 壳→sidecar 的窄授权、driver 停止/证明宿主通道。

- [ ] **Step 1: 写三端失败测试。** app-server driver 单测放现有 driver 模块内，用 mock provider
  按相同路径传普通/“Kanban” thread metadata，断言授权和活动序列完全相同：

  新增宿主测试夹具 `async fn run_two_mock_turns(kind: HostEntry) -> HostObservation`，HostEntry 包含
  Desktop、KanbanDesktop、Tui、Headless；HostObservation 字段为 phases: Vec<ActivityPhase>、
  activation_event_count: usize、objectives: Vec<String>、wire_variant_names: Vec<String>、
  tool_execution_count: usize。夹具调用实际宿主入口及mock provider，读取真实IPC和事件，不自行拼预期活动。
  KanbanDesktop 必须经现有Kanban创建/启动会话边界进入app-server；不能只把cwd字符串改成“kanban”冒充覆盖。

```rust
let ordinary = run_two_mock_turns(HostEntry::Desktop).await;
let kanban = run_two_mock_turns(HostEntry::KanbanDesktop).await;
for observed in [&ordinary, &kanban] {
    assert_eq!(observed.phases, vec![ActivityPhase::Running, ActivityPhase::Idle,
                                    ActivityPhase::Running, ActivityPhase::Idle]);
    assert_eq!(observed.activation_event_count, 1);
    assert_eq!(observed.objectives.len(), 2);
    assert_eq!(observed.objectives[0], observed.objectives[1]);
}
assert_eq!(kanban.wire_variant_names, ordinary.wire_variant_names);
```

  若Kanban启动边界只能从CLI/plugin触达，将该段宿主测试放到 `yi-agent` 的临时进程集成fixture，
  不让app-server反向依赖plugin，也不把未跑通的fixture标记成C1完成。
  TUI/headless 在 bin 单测中覆盖 success/error/cancel；壳单测证明 sidecar 只能拿 RootAuthorization
  和 driver capability，拿不到 ClientGrant；legacy 无能力返回明确 unsupported，不改原预算。
  持续工具事件跨 2701 秒不报告 root progress，也可下一 turn spawn。

- [ ] **Step 2: 运行 RED。**

```bash
cargo test -p yi-agent-app-server foreground_root_lifecycle
cargo test -p yi-agent --bin yi-agent foreground_root_lifecycle
cargo test -p yi-agent-subagent foreground_root_lifecycle
```

  桌面壳测试在 `desktop/src-tauri/` 运行 `cargo test foreground_root_lifecycle`；
  如 build script 要求 externalBin，按项目说明建立本 worktree 的 sidecar symlink，仅供构建，不能启动正式实例。

- [ ] **Step 3: 最小宿主接入。** bootstrap 探测与 business bind 分离；取得 lease 后 authorize，
  app-server 只经私有 host channel 接收窄授权。首 turn 激活 objective；每 turn 使用共享驱动：

```rust
foreground_activity_sync_warning(foreground.begin_turn(&turn_id));
// 同步失败只暂时禁止本driver委派，不以 ? 中止普通聊天。
let outcome = run_actual_turn().await;
foreground_activity_sync_warning(foreground.finish_turn(&turn_id));
// 上述 warning helper 只记去密诊断；持久化与用户呈现仍使用实际 outcome，绝不重放工具。
return outcome;
```

  将取消、provider错误、driver退出收拢到同一 finalize，不靠 Drop 执行必须 await 的 I/O。
  SIGKILL 走 Unknown+可信进程停止证明，不承诺 finally 必达。恢复/接替处理在旧 turn 之外运行。
  waiting phase 只上报能观测到的状态；看不到权限等待细节可维持 Running。
  UI断连不关闭 driver/owner；线程删除显式 detach；lease release 影响其 owner 的全部 root，不能误用为单会话关闭。

- [ ] **Step 4: 重跑 Step 2 与壳测试至 GREEN。** mock provider 断言工具调用次数，避免 activity 重试引起重复执行。
- [ ] **Step 5: fmt、just ci、登记 C2/C4/C8/C12/C15 并提交。** message：`feat: wire foreground lifecycle across desktop tui and headless hosts`。

## Task 10: 提供非模型的显式恢复入口

**Files:** Modify desktop/src-tauri/src/{foreground_root.rs,lib.rs}、desktop/src/App.tsx；
Create components/ForegroundRootRecovery.tsx 和同名 test.tsx；Modify yi-agent/src/main.rs、yi-agent/src/tui/slash.rs、
app-server/server/driver/mod.rs；共享逻辑仍在 subagent/foreground.rs，不复制 daemon 状态机。

**Interfaces:** Tauri command 接受本地会话引用/operation id，不接受 caller 提供 client id/grant/stopped：

```rust
foreground_root_recovery_inspect(thread_id: String) -> Result<RecoveryImpactView, HostRecoveryError>
foreground_root_recovery_preview(thread_id: String, operation_id: String) -> Result<RecoveryOperationView, HostRecoveryError>
foreground_root_recovery_confirm(thread_id: String, operation_id: String, token: String) -> Result<RecoveryOperationView, HostRecoveryError>
foreground_root_recovery_operation(thread_id: String, operation_id: String) -> Result<Option<RecoveryOperationView>, HostRecoveryError>
```

壳定义可序列化错误：`HostRecoveryError { code: HostRecoveryErrorCode, reason: String }`，
`enum HostRecoveryErrorCode { Rejected(ForegroundErrorCode), Transport, Protocol, SessionNotFound }`；
code 使用 serde 显式tag `kind`/content `code`，reason只取宿主白名单，不透传底层错误/秘密。
前端声明同形 discriminated union 并仅按code展示文案；transport与权限拒绝不得混用。
壳通过自己的会话映射查 root/workspace、lease，并调用 Task 8 API；sidecar 收到停止请求后 cancel/await
旧 turn，经可信 host channel 回证明。命令返回去密 RecoveryPreviewView/RecoveryResult，前端 token
仅保留当前确认组件内存，不进入转录/localStorage/日志。

Headless 使用运行中宿主的认证 API，不新增独立进程的 `agents recover-root` 或 `agent recover-root` CLI：
新CLI进程是新owner，不能凭thread id拿到旧grant/root cap。定义在 subagent::foreground：

```rust
pub struct ForegroundRecoveryHost; // 持有宿主已有授权客户端，不对模型暴露
impl ForegroundRecoveryHost {
    pub fn inspect(&mut self, expected: &ForegroundFence) -> Result<RecoveryImpact, ForegroundClientError>;
    pub fn prepare(&mut self, request: &RecoveryRequest) -> Result<RecoveryOperationStatus, ForegroundClientError>;
    pub fn confirm(&mut self, request: &RecoveryRequest, token: &SecretCapability)
        -> Result<RecoveryOperationStatus, ForegroundClientError>;
    pub fn operation(&mut self, root: &RootRef, id: &str)
        -> Result<Option<RecoveryOperationStatus>, ForegroundClientError>;
}
```

构造器 `ForegroundRecoveryHost::new(authorization: RootAuthorization, binding: Arc<RuntimeBinding>) -> Self`
由已有headless宿主接线，不自行创建lease或从thread id查询秘密。TUI新增 `/agent recover-root` 在同一宿主内
调用其已有认证客户端，并注册到 `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`、帮助/解析测试；
保留 `/agents` 原有列表行为。桌面壳沿 §1.1 ControlRequest 使用自己的grant，不链接此store依赖客户端。
所有token仅在宿主内存和确认组件短期内存传递，不进入argv、shell history或日志。
inspect只读；用户选择“准备恢复（停止旧执行）”后才prepare；取消最终确认不创建successor，
不逆转已完成的停止。没有旧cap/可用原owner的历史会话明确报authority_missing，不承诺凭名称恢复。
本API不注册为 `yi-agent run` 内的模型工具。

- [ ] **Step 1: 写入口 RED。** Vitest 渲染恢复面板，初始展示旧 attempt、原因、影响和停止缺口；
  有缺口不能确认，inspect页取消不启动prepare，prepare后取消最终确认不发recover且保留停止事实，
  连续点击只发一次同operation，响应丢失查询原operation，过期重新prepare，错误不显示成功。

  在该 test.tsx 内直接使用 `render`、`screen`、`fireEvent`、`waitFor` 和 `vi.fn`；每项测试创建独立
  preview/impact/operation props及mock，执行真实组件交互，不只断言未调用的mock：
  1. 渲染 blocked impact（含stalled原因和一个未停止实体），查找“确认恢复”并断言disabled，点击后confirm仍0次。
  2. 渲染完整Ready preview，点击“取消”，断言cancel恰好1次、confirm及prepare均0次。
  3. query先返回同id的Ready，confirm拒绝Transport，第二次query返回同id Committed；
     连续两次click确认后waitFor成功文案，断言confirm恰好1次且参数为该id/token，query恰好2次且均为该id。
  4. query先返回Committed，点击确认只能展示原结果，confirm为0次；再测query拒绝不得显示成功。
  5. 注入固定时钟，过期preview不可confirm；不在Date.parse非法值/空值时误放行。
  mock仅模拟宿主command返回；不得自行拼成功DOM或通过stub替代组件的防双击/查询逻辑。

  TUI解析/帮助覆盖inspect→prepare→confirm及用户取消；headless使用同一授权实例调用上述API，
  无授权构造/只有thread id的尝试不得变成新的接管入口。
  TUI/桌面取消inspect不停止，取消prepare后的最终确认不创建后继、但保留停止事实；
  壳跨thread/workspace伪造映射拒绝；API不出现在模型 tool registry，不能用 LLM `confirmed=true` 触发。

- [ ] **Step 2: 运行 RED。**

```bash
(cd desktop && npm test -- src/components/ForegroundRootRecovery.test.tsx)
(cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent foreground_root_lifecycle)
```

- [ ] **Step 3: 实现最小恢复面板与控制命令。** 显示 prepare 的真实影响、阻塞/到期信息，按钮仅在
  确认 token 有效且 blocked为空时可点；确认 handler 锁住同 operation，超时先查询结果，不能新建 id
  猜测重试。没有历史 cap 提示 root_recovery_authority_missing，不提供 SQL/重启建议。

  组件定义 `preview: RecoveryPreviewView`、`confirmRecovery(id, token): Promise<RecoveryOperationView>`、
  `queryOperation(id): Promise<RecoveryOperationView | null>`、`cancel(): void` props；RecoveryPreviewView
  字段为 operationId: string、token: string | null、blocked: string[]、expiresAt: string | null。
  RecoveryOperationView 是 `{ phase: "preparing" | "needs_preview" } |
  { phase: "blocked"; impact: RecoveryImpactView } | { phase: "ready"; preview: RecoveryPreviewView } |
  { phase: "committed"; result: RecoveryResultView }`。RecoveryImpactView包含root/原因/child/blocked实体及版本，
  RecoveryResultView仅含去密operation/root/attempt/epoch字段；Ready必须带preview，否则轮询后取不到token。
  Rust view均在壳的foreground_root.rs定义，derive Serialize并 `#[serde(rename_all = "camelCase")]`，
  与TS字段逐一对应；不直接把RootAuthorization/DriverBinding序列化给前端。
  双击保护使用 useRef 的同步锁，不只依赖异步 setState：

```tsx
const inFlight = useRef(false);
async function confirmOnce() {
  const token = preview.token;
  if (!token || preview.blocked.length !== 0 || inFlight.current) return;
  if (!preview.expiresAt || !Number.isFinite(Date.parse(preview.expiresAt)) ||
      Date.now() >= Date.parse(preview.expiresAt)) {
    setFailure("确认已过期，请重新准备恢复");
    return;
  }
  inFlight.current = true;
  setConfirming(true);
  try {
    const before = await queryOperation(preview.operationId);
    if (!before || before.phase !== "ready") {
      if (before) setOperation(before);
      else setFailure("尚无可确认操作，请查询原操作或重新准备");
      return;
    }
    setOperation(await confirmRecovery(preview.operationId, token));
  } catch {
    try {
      const current = await queryOperation(preview.operationId);
      if (current) setOperation(current);
      else setFailure("尚无提交结果，请检查同一操作，勿重复新建恢复");
    } catch { setFailure("结果暂未知，请稍后查询同一操作"); }
  } finally {
    inFlight.current = false;
    setConfirming(false);
  }
}
```

  只有 phase=committed 显示成功；Preparing/Blocked/NeedsPreview 均不视为成功。
  查询失败保留operation id；每次用户重试先查询该id，仅Ready且有本次用户点击时才可再确认同id，
  不自动新建operation或自动重发确认；token过期后仍允许查询已提交结果。
  原始错误不展示/记录以免泄露秘密；机器code按白名单映射原因，不能匹配英文错误自动恢复。
  成功后新 driver bind，下一新 turn 才派发，不重跑失败工具。

- [ ] **Step 4: 重跑 Step 2、桌面 `npm run build`、壳恢复测试至 GREEN。** 不启动/安装任何 app。
- [ ] **Step 5: fmt、just ci、登记恢复入口证据并提交。** message：`feat: expose confirmed foreground root recovery controls`。

## Task 11: 全矩阵回归、能力发布与收尾审查

**Files:** 已有所有测试与 docs/project-management；Modify 现有 RuntimeIdentity/ArtifactIdentity 能力组装处，
以 `FOREGROUND_ROOT_CAPABILITY` 常量统一广告；不手写多处字符串。
**Interfaces:** Consumes Tasks 1–10 完整闭环；Produces 对外 supported managed 能力和实现验收报告。

- [ ] **Step 1: 补齐矩阵尚缺的 RED。** 对照下表检查有实际测试名称，不把一个 happy path 等同整行：

| Spec | 必需任务与测试重点 |
|---|---|
| C1 | 5、9：普通/Kanban、301/2701秒及真实宿主派发 |
| C2 | 5、9：工具持续但 root progress 仍零 |
| C3 | 5：等待 child、有进展且未耗尽 child 预算 |
| C4 | 6、9：跨 turn idle、objective 不覆盖 |
| C5 | 5：child/孙/scheduled root watchdog 与显式取消 |
| C6 | 4、5：旧快照提交栅栏、worker 不被活动豁免 |
| C7 | 3、4、7：真实桥、准入竞态、停止双证据、owner 隔离 |
| C8 | 6、7、9：UI断连/租约续接/旧driver/乱序finish |
| C9 | 8、10：确认后继、旧历史、新 turn spawn |
| C10 | 3、7、8、10：权限、停止、确认、幂等负测试 |
| C11 | 2、6、8：迁移/重开库/重启/秘密脱敏 |
| C12 | 9、10：三端managed与legacy降级 |
| C13 | 3：公开key/LeaseOnly/工具FD不能授权 |
| C14 | 3、4、9：bootstrap、桥EOF、续租/撤销 |
| C15 | 2、6、7、9：硬退出、接替、startup sweep |
| C16 | 4、7、8：冻结、新派发、丢响应、事务前后故障、无历史授权 |

  新增缺失用例时先移除对应保护/注入错误，确认断言真的失败；恢复保护后 GREEN。

- [ ] **Step 2: 运行完整定向验收并记录非零命中。** 在 yi-agent-rs/ 顺序执行：

```bash
cargo test -p yi-agent-store --features foreground-test-support --test runtime_coordinator foreground_root_lifecycle
cargo test -p yi-agent-store --features foreground-test-support --test runtime_ipc foreground_root_lifecycle
cargo test -p yi-agent-store --lib foreground_root_lifecycle
cargo test -p yi-agent-subagent foreground_root_lifecycle
cargo test -p yi-agent-app-server foreground_root_lifecycle
cargo test -p yi-agent --bin yi-agent foreground_root_lifecycle
cargo test -p yi-agent-lifecycle foreground_root_lifecycle
cargo test -p yi-agent-supervisors --features yi-agent-store/foreground-test-support --test foreground_root_lifecycle
cargo test -p yi-agent-lifecycle --test client_leases
cargo test -p yi-agent-supervisors --test client_reclamation
cargo test -p yi-agent-store --test scheduler
cargo fmt --all
just ci
```

  再跑桌面 `npm test -- src/components/ForegroundRootRecovery.test.tsx`、`npm run build` 和壳测试。
  每条新测试过滤命令须列出实际测试名称与数量，0 tests 视作未验收。构建锁排队按项目说明诊断，
  不删除 lock 文件，不杀其他会话的 cargo，不用 workspace 全量无限并行。

- [ ] **Step 3: 完整闭环通过后才启用能力。**

```rust
// 仅 managed 且私有授权桥已建立、schema/业务协议均支持时广告。
if managed && authority_bridge_ready && foreground_schema_supported {
    capabilities.push(FOREGROUND_ROOT_CAPABILITY.to_owned());
}
```

  这三项由运行时真实状态判定，不是新建 hardcoded true。Task 11 将正式managed构造参数切为Enabled，
  只有artifact静态声明支持且当前bridge/schema就绪才在RuntimeIdentity广告；不要求已经绑定root，
  否则“发现能力→注册→绑定”会循环。lease/owner授权仍是后续独立准入检查。
  测试构造注入不自动进入ArtifactIdentity。能力探测失败/旧端继续明确降级；
  回归先探测再发 variant，确认旧 parser 不会导致偷偷重启或假成功。启用后重跑 Step 2。

- [ ] **Step 4: 自检与独立 code review。** 加载 requesting-code-review，审查真实 diff，重点看
  可伪造授权、锁序、跨进程撤销确认、stop proof、recover CAS、SingleShot重发、秘密泄漏。
  问题逐项修复并重跑相关测试；没有证据的停止/授权不得以“设计上应该安全”放行。
  更新 subagent-runtime feature 为 `[x]` 仅在 C1–C16 全有证据时，README 索引同步；
  若有受环境阻塞项保持 `[ ]`，报告具体未验收项，不虚增完成计数。

- [ ] **Step 5: 提交与受控合并。** 提交前再次 `cargo fmt --all && just ci`，提交 message：
  `fix: enable verified foreground root lifecycle isolation`。加载 finishing-a-development-branch，
  经用户选择后才合并；如获准，main 上 `git merge --no-ff <implementation-branch>`，
  在合并结果再跑 `just ci`，然后移除已合并 worktree/分支。不部署或修改运行中实例。

## 计划自审

- 覆盖：spec §4 的授权/兼容→Tasks 1–4/9；§5 watchdog→5；§6 activity→1/6/9；
  §7退出重启→2/3/7/9；§8恢复→7/8/10；§9并发→4/6/8；§10迁移协议→2/3/11；C1–C16见 Task 11。
- 类型：跨 crate DTO 只在 lifecycle::foreground；序号/代际为 u64；root/session/attempt wire 为 String；
  进入 store 时转现有强类型。driver/root/task 三类秘密互不代用；幂等结果不含 capability。
- 依赖：没有先启用豁免再补可信 owner 的发布窗口；最终能力只在 Task 11 开放。
- 本计划中的代码片段是实施契约与核心断言，不是已写入源码的实现；执行者必须完成对应生产接线，
  不得以 fixture 代替真实桥/三端集成，也不得以文档检查替代测试。

### 本次反复自审修订记录

- 接口/依赖复核：补 store 的 lifecycle直接依赖、测试feature、结构化wire错误和全部查询返回类型；
  明确 Task 1 先定义跨进程DTO、Task 8 再实现恢复，避免前序任务依赖不存在的类型。
- 授权/并发复核：租约有效期以challenge发送时刻保守计算；登记starting execution关闭check/start窗口；
  RecoveryOnly打破停止/授权循环；恢复提交与guardian升权采用关闭准入下的幂等补完，而非虚构跨进程原子事务。
- 重启/兼容复核：legacy保留原策略，pending由有界reservation清理；测试注入不自动开启生产语义；
  完整闭环到Task 11才正式启用，不把能力广告当作唯一执行开关。
- 宿主/UI复核：取消未经授权的新CLI接管方案，区分只读inspect与有停止副作用的prepare；
  补真实入口/真实socket/继承FD测试边界、确定性竞态屏障、同operation查询与双击保护。
- 修订回查：同步spec与进度登记；保留11个任务、55个待执行步骤与C1–C16追踪。
  本轮只进行文档检查与临时内存SQLite约束验证；未执行Rust/桌面实现测试，未提交或合并。
  自审未发现的实现问题仍须由RED/GREEN、故障注入和独立代码审查发现，不据此宣称运行时正确。
