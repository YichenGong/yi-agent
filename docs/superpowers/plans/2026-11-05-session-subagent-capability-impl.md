# Session Delegation Independent of Execution State Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让普通与 Kanban 会话在历史 root 终态、空闲及安全重连后仍能委派，同时保持 Execution 的终态、权限、取消和停止证明约束。

**Architecture:** 持久 task role 决定容器/执行语义，当前授权与 binding/driver/turn 栅栏决定写准入，旧 TaskState 只保留历史。daemon 将身份切换、turn 关闭和 child 登记放在现有提交屏障内；宿主在每次 Agent::run 前装配不可变的本轮工具表。managed 复用 guardian 权威来源，embedded 使用私有接续凭证，不允许失败降级或按公开 thread key 取回秘密。

**Tech Stack:** Rust workspace、Tokio、rusqlite/SQLite、serde、Unix socket IPC、现有 guardian/foreground/ManagedRun 与 mock provider；不新增外部依赖。

## Global Constraints

- 设计依据：[spec](../specs/2026-11-05-session-subagent-capability-design.md)，代码基线 `ff99452b`；本文件为待执行计划，不是测试报告。
- “普通会话与 Kanban 会话使用相同的委派规则。”
- “角色不得从 `parent_id IS NULL`、cwd、thread id 字符串或 managed 分类猜测。”
- “不将 stalled 覆写为 running，不把已关闭 attempt 重新打开，不为恢复委派调用 retry。”
- “managed 认证失败不能切换成 embedded 来绕过拒绝。”
- “spawn 仍是有副作用的请求：应答丢失不自动重发”。
- “不持有全局 gate 等待网络、LLM、工具或线程退出。”
- “验证使用临时数据库/socket/工作目录，不修改生产 runtime。”时间测试用可控时钟，不真睡 300/2700 秒。
- Execution 的深度、直接 child 上限、队列/运行容量、sandbox、coding Git 与 cwd 校验保持不变；不自动重跑旧 child。
- 新能力名固定 `session_delegation_lifecycle_v1`；Task 11 完成前不广告/启用对外承诺。中间提交是不可发布的开发状态，缺少上下文时拒绝委派，不回退无栅栏写入；role 预算策略不能为了兼容而重新按 managed 分类判断。
- 当前 schema 为 18，新 migration 使用 19；若执行时主线已占用 19，先统一改本计划和测试中的版本，再实施，不能覆盖主线 migration。
- 当前 wire protocol 为 2，本次切到 3；插件客户端、探活与版本负测试必须一起更新，先读 envelope 版本再解析 command。
- 只在隔离 worktree 开发；每次提交前 `cd yi-agent-rs && cargo fmt --all`。代码阶段提交前及合并后均跑 `just ci`；默认测试不调用真实 LLM。
- 每任务的新增测试必须确认非零命中；编译失败仅用于首次声明新接口，之后必须得到目标行为的红→绿证据。先完成本任务测试与审查再开始依赖任务。

---

## 文件与职责图

以下路径均相对仓库根。`Create` 明确为新文件，不能当作现有 API 引用。

| 区域 | 文件 | 职责 |
|---|---|---|
| core | `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs`、`supervisor.rs`、`mod.rs` | 持久角色的内存对应、容器 mailbox/级联/worker 启动策略 |
| core | `yi-agent-rs/crates/yi-agent-core/src/{agent,tool}.rs` | 每 run 克隆不可变工具表，不引入 store/lifecycle 依赖 |
| lifecycle | Create `yi-agent-rs/crates/yi-agent-lifecycle/src/session_delegation.rs`；`src/lib.rs` | wire DTO、秘密脱敏、稳定失败原因 |
| store | Create `yi-agent-rs/crates/yi-agent-store/src/repository/session_container.rs`、`session_container_tests.rs` | role、tombstone、接续摘要、CAS、执行登记持久化 |
| store | `src/repository/{helpers,mod,application_root,watchdog,read,mailbox}.rs`、`src/schema_compat.rs` | v19、hydration、预算/恢复/回收分流、观察 |
| store | Create `src/runtime/session_delegation.rs`；`src/runtime.rs`、`src/runtime/foreground.rs` | 绑定与 turn 协调、屏障、执行冲突、停止证明 |
| store | Create `src/ipc/session_delegation.rs`；`src/ipc/{mod,protocol,daemon,error,foreground,managed}.rs` | v3 路由与结构化错误、全入口防绕过 |
| subagent | Create `yi-agent-rs/crates/yi-agent-subagent/src/{session_delegation,session_identity}.rs`；`src/{lib,thread_root,attach,foreground}.rs` | 宿主接续、0600 凭证、不可变 turn client、工具接线 |
| app-server | `yi-agent-rs/crates/yi-agent-app-server/src/server/{assembly,foreground}.rs`、`server/driver/turn_driver.rs`、`thread_store.rs` | 每轮准备、关闭、失败告知、删除与凭证生命周期 |
| CLI | `yi-agent-rs/crates/yi-agent/src/{main,session_store}.rs`、`src/tui/subagents.rs` | TUI/headless 同一宿主协议，worker 不晋升 |
| tests | 新增 store/subagent 集成测试和 app-server 现有单测模块，见各任务 | 临时库、故障注入、三端 mock 入口 |
| docs | `docs/project-management/{subagent-runtime,README}.md` 与本计划 | 同步完成证据和计数；未验证不勾完成 |

这不是多个可独立交付的 feature：角色、准入、旧入口封堵和三端接线必须一起启用。
按依赖串行执行 Task 1–8，Task 9/10 可在 Task 8 后独立做，Task 11 汇总后才广告能力。

## 公共接口契约（由指定任务创建，不表示现在存在）

### Task 1：共享类型

core 的 `subagent::task` 添加并经 `subagent::mod` 导出：

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRole {
    #[default]
    Execution,
    SessionContainer,
}
```

lifecycle 新模块使用已有 `foreground::{SecretCapability, ActivityPhase, QuiescenceRef}`
和 `types::RootAuthorization`，定义以下 DTO（全部 serde；秘密字段的 Debug 必须脱敏）：

```rust
pub const SESSION_DELEGATION_CAPABILITY: &str = "session_delegation_lifecycle_v1";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRef { pub session_id: String, pub root_id: String }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFence {
    pub root: SessionRef, pub generation: String,
    pub binding_epoch: u64, pub driver_epoch: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTurn {
    pub fence: SessionFence, pub turn_id: String,
    pub capability: SecretCapability,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBinding {
    pub fence: SessionFence, pub capability: SecretCapability,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionIdentity {
    Managed(RootAuthorization),
    Embedded(SecretCapability),
    LegacyUpgrade(SecretCapability),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareSession {
    pub root: SessionRef, pub operation_id: String,
    pub host_run_id: String,
    pub expected_binding_epoch: u64, pub identity: SessionIdentity,
    pub current_binding: Option<SessionBinding>,
    pub replacement_resume_digest: Option<String>,
    pub stopped: Vec<QuiescenceRef>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFailureCode {
    SessionIdentityRequired, AuthorityDenied, SessionClosed, OwnerConflict,
    StaleGeneration, StaleBinding, StaleTurn, StopUnproven,
    AdmissionClosed, ProtocolMismatch, CorruptAttachment,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFailure { pub code: SessionFailureCode, pub reason: String }
```

`PrepareSession.stopped` 只引用已有受信证据，不接受客户端 self-report 的 `joined=true`。
`host_run_id` 由 SessionHost 构造时随机生成，生存期内固定，用于区分活 owner；它不是授权。
旧 owner 控制流尚活且 run id 不同则 OwnerConflict，即使同一接续凭证也不得自动强占。
在 lifecycle 新模块再定义 `pub fn session_resume_digest(secret: &SecretCapability) -> String`，
使用本 crate 已有 sha2 计算带域前缀 `session-resume-v1\0` 的 SHA-256 小写 hex；
store/subagent 共享此函数，不各自实现哈希、不增加外部依赖。
`SessionRef` 不含 attempt；对旧 foreground `RootRef` 的适配保持旧 attempt 不变。

### Task 2/5：持久 CAS 数据

在 store `repository::session_container` 定义/导出：

```rust
pub struct SessionContainerRecord {
    pub root: SessionRef, pub closed: bool, pub binding_epoch: u64,
    pub driver_epoch: u64, pub owner_run_id: Option<String>,
    pub resume_digest: Option<String>, pub legacy_upgrade_consumed: bool,
}
pub struct SessionBindingCommit {
    pub root: SessionRef, pub expected_binding_epoch: u64,
    pub operation_id: String, pub request_digest: String,
    pub owner_run_id: String, pub generation: String,
    pub resume_digest: Option<String>,
}
// commit 只由已鉴权的 coordinator 调用；不能暴露为业务请求。
pub fn task_role(&self, task: &TaskId) -> Result<TaskRole, RepositoryError>;
pub fn session_container(&self, task: &TaskId)
    -> Result<Option<SessionContainerRecord>, RepositoryError>;
pub fn commit_session_binding(&mut self, commit: &SessionBindingCommit)
    -> Result<SessionContainerRecord, RepositoryError>;
pub fn close_session_container(&mut self, task: &TaskId) -> Result<(), RepositoryError>;
```

以上方法属于 `RuntimeRepository`。事务错误统一经 `RepositoryError::SessionDelegation(SessionFailure)`。
同 operation id + 同 request_digest 返回原 epoch；同 id 异 payload 拒绝 Conflict，不能重新加 epoch。
幂等不复活秘密：同一 generation 的短期能力从当前内存返回；daemon 已换代时旧操作只可查询
原提交结果，需新 operation id 做当前 generation 的绑定，旧 turn 绝不重开。

### Task 5/6：协调与纯策略

store 新 runtime 模块导出（`pub(crate)` 给 IPC，测试在模块内访问）：

```rust
impl RuntimeCoordinator {
    pub fn prepare_session(&self, request: PrepareSession)
        -> Result<SessionBinding, RuntimeCoordinatorError>;
    pub fn begin_session_turn(&self, binding: &SessionBinding, turn_id: &str)
        -> Result<SessionTurn, RuntimeCoordinatorError>;
    pub fn end_session_turn(&self, turn: &SessionTurn)
        -> Result<(), RuntimeCoordinatorError>;
    pub fn authorize_session_write(&self, turn: &SessionTurn)
        -> Result<(), RuntimeCoordinatorError>;
}
```

`authorize_session_write` 只能预检，产生副作用的提交必须在同一 gate 中重核；不能把一次
成功返回当永久 permit。gate 中先校验角色/身份/代次，再登记副作用；网络/启动在 gate 外。
为单元测试在相同模块定义纯函数：

```rust
pub(crate) fn validate_turn(current: &SessionTurn, presented: &SessionTurn, open: bool)
    -> Result<(), SessionFailureCode>;
pub(crate) fn may_replace_driver(joined: bool, tools_stopped: bool) -> bool {
    joined && tools_stopped
}
```

### Task 7/8：宿主与工具

在 subagent 新模块定义：

```rust
pub struct SessionHost; // 持有 root、当前已验证 binding；不在工具调用时换身份。
#[derive(Clone)]
pub struct SessionTurnClient; // 固定 RuntimeHandle + SessionTurn + 本轮 AtomicBool。
pub enum SessionClientError { Rejected(SessionFailure), Transport(String), Protocol(String) }
impl SessionHost {
    pub fn new(root: Arc<ThreadRoot>, identity: SessionIdentity,
        replacement_resume_digest: Option<String>) -> Self;
    pub fn prepare(&mut self, operation_id: &str) -> Result<(), SessionClientError>;
    pub fn begin_turn(&mut self, turn_id: &str) -> Result<SessionTurnClient, SessionClientError>;
    pub fn finish_turn(&mut self, turn: &SessionTurnClient) -> Result<(), SessionClientError>;
}
impl SessionTurnClient {
    pub fn context(&self) -> &SessionTurn;
    pub fn close_locally(&self);
    pub fn ensure_open(&self) -> Result<(), SessionClientError>;
}
pub fn register_session_turn_tools(
    registry: &mut ToolRegistry, turn: Arc<SessionTurnClient>,
    controller: yi_agent_tools::SandboxController,
    thread_id: Option<String>, caller: CallerContext,
);
```

`finish_turn` 先本地 close，再发 EndSessionTurn；未确认时 host 不能 begin 新 turn。
传输失败不偷偷拿 `ThreadRoot::handle()` 新身份替代原调用；read-only 查询可走独立安全重连。

---

## Task 1：定义角色、会话 DTO 和稳定错误

**Files:** 修改 core `src/subagent/{task,mod}.rs`；新建 lifecycle `src/session_delegation.rs`，修改 `src/lib.rs`。

**Interfaces:** 产出上文 Task 1 的完整 DTO；`AgentTask` 新增私有 role（serde default）和
`pub fn role(&self) -> TaskRole`。现有构造器全部默认 Execution，另增
`pub fn new_session_container(session: RootSessionId) -> Self` 和
`pub fn hydrate_session_role(&mut self, role: TaskRole) -> Result<(), String>`；后者只允许
root/depth Root 晋升，不允许把 child 晋升。只供 daemon 恢复已验证角色，不接收客户端声明。

- [ ] **Step 1：写红测试**，放 task.rs 内部测试模块：

```rust
#[test]
fn session_delegation_lifecycle_role_is_not_root_depth() {
    let execution = AgentTask::new_root(RootSessionId::new());
    assert_eq!(execution.role(), TaskRole::Execution);
    let container = AgentTask::new_session_container(RootSessionId::new());
    assert_eq!(container.role(), TaskRole::SessionContainer);
    assert_eq!(container.state(), &TaskState::Queued);
    let mut child = AgentTask::new_child(container.root_session_id.clone(), container.id);
    assert!(child.hydrate_session_role(TaskRole::SessionContainer).is_err());
}
```

lifecycle 同模块增加 `session_delegation_lifecycle_secret_debug_is_redacted`：用
`SecretCapability::new("never-log-me")` 构造 SessionTurn，断言 Debug 不含该串；serde 往返相等。
使用现有 SecretCapability 构造器的实际签名，不另创可泄漏 Display。

- [ ] **Step 2：确认红**：`cd yi-agent-rs && cargo test -p yi-agent-core --lib session_delegation_lifecycle_role_is_not_root_depth`；初始缺接口编译失败，补齐类型后观察 child 晋升被拒的行为断言。
- [ ] **Step 3：最小实现**：按公共契约落类型，role 初始化统一默认；晋升条件用 `parent_id.is_none() && depth == TaskDepth::Root`，不改 state/attempt。失败 enum 使用 snake_case，reason 不携带秘密。
- [ ] **Step 4：确认绿**：`cargo test -p yi-agent-core --lib session_delegation_lifecycle && cargo test -p yi-agent-lifecycle session_delegation_lifecycle`，均非零命中。
- [ ] **Step 5：提交**：`cargo fmt --all && git add crates/yi-agent-core crates/yi-agent-lifecycle && git commit -m "feat: define session delegation roles and wire contract"`（以下提交命令均在 yi-agent-rs）。

## Task 2：v19 迁移与容器记录，不改历史执行证据

**Files:** 修改 store `src/repository/{helpers,mod,application_root}.rs`、`src/schema_compat.rs`；新建 `src/repository/{session_container,session_container_tests}.rs`。

**Interfaces:** 消费 TaskRole/SessionRef；产出上文 Repository 查询接口与 SessionContainerRecord。
`commit_session_binding` 到 Task 5 才实现；此任务不开放准入。

- [ ] **Step 1：红测试**，在 repository 新测试模块使用父模块私有 connection：

```rust
#[test]
fn session_delegation_lifecycle_migration_preserves_terminal_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime.sqlite");
    let (session, root, attempt) = (RootSessionId::new(), TaskId::new(), AttemptId::new());
    let mut r = RuntimeRepository::open(&path).unwrap();
    r.create_task_with_attempt(&root, &session, &attempt, 1, "stalled").unwrap();
    r.record_application_root_attachment("thread:old", &session, &root,
        "old-digest", "old-secret", "/tmp/workspace").unwrap();
    // 制作 v18 形状，不靠重新 attach 晋升；本任务显式创建这个测试辅助函数。
    downgrade_to_v18(&mut r);
    drop(r);
    for _ in 0..2 {
        let r = RuntimeRepository::open(&path).unwrap();
        assert_eq!(r.task_role(&root).unwrap(), TaskRole::SessionContainer);
        assert_eq!(r.connection.query_row("SELECT state_json FROM tasks WHERE id=?1",
            [root.to_string()], |row| row.get::<_, String>(0)).unwrap(), "stalled");
        assert_eq!(r.connection.query_row("SELECT COUNT(*) FROM attempts WHERE task_id=?1",
            [root.to_string()], |row| row.get::<_, i64>(0)).unwrap(), 1);
        assert!(!r.session_container(&root).unwrap().unwrap().closed);
    }
}
```

`downgrade_to_v18(r: &mut RuntimeRepository)` 的实际 SQL：删除本任务新增的所有 v19 表、
`ALTER TABLE tasks DROP COLUMN role; DELETE FROM schema_migrations WHERE version=19;`。
在 drop 前以 `SELECT * FROM attempts`、`SELECT * FROM events` 保存整行 JSON 快照，重开后逐字相等。
新增 `session_delegation_lifecycle_migration_rejects_corrupt_attachment`：两个不同 thread key
指同一 root、root/session 不匹配、attachment 指 child 三个夹具；角色不晋升且可诊断。
损坏项在独立 migration_errors 表记录，不令其他合法会话丢失；attach 该项返回 CorruptAttachment。

- [ ] **Step 2：确认红**：`cargo test -p yi-agent-store --lib session_delegation_lifecycle_migration`，缺 schema/角色查询失败。
- [x] **Step 3：实现**：

```sql
ALTER TABLE tasks ADD COLUMN role TEXT NOT NULL DEFAULT 'execution'
  CHECK(role IN ('execution','session_container'));
CREATE TABLE session_containers (
 root_task_id TEXT PRIMARY KEY REFERENCES tasks(id),
 root_session_id TEXT NOT NULL,
 closed INTEGER NOT NULL DEFAULT 0 CHECK(closed IN (0,1)),
 binding_epoch INTEGER NOT NULL DEFAULT 0,
 driver_epoch INTEGER NOT NULL DEFAULT 0,
 owner_run_id TEXT, resume_digest TEXT,
 legacy_upgrade_consumed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE session_binding_operations (
 root_task_id TEXT NOT NULL, operation_id TEXT NOT NULL,
 request_digest TEXT NOT NULL, generation TEXT NOT NULL,
 binding_epoch INTEGER NOT NULL, driver_epoch INTEGER NOT NULL,
 PRIMARY KEY(root_task_id, operation_id)
);
CREATE TABLE session_migration_errors (
 attachment_key TEXT PRIMARY KEY, reason TEXT NOT NULL
);
CREATE TABLE session_host_executions (
 execution_id TEXT PRIMARY KEY,
 root_task_id TEXT NOT NULL REFERENCES tasks(id), generation TEXT NOT NULL,
 binding_epoch INTEGER NOT NULL, driver_epoch INTEGER NOT NULL,
 turn_id TEXT NOT NULL, identity_json TEXT NOT NULL,
 phase TEXT NOT NULL CHECK(phase IN ('starting','running','unknown','stopped')),
 evidence_json TEXT, execution_set_version INTEGER NOT NULL
);
```

在迁移事务内验证 attachment 唯一归属、root 的 parent/depth/session、workspace 一致、state 为
attached/detached；合法项回填 role 与关闭的**准入**，`closed=0` 不代表准入开放。
detached 不改 attached；历史无关闭证据的 cancelled 在 prepare 时阻塞，不靠 migration 猜测。
新 attachment 的 task/容器记录/attachment 在同一事务提交，避免崩溃留下半角色；新建时使用
相同校验，不在事后每次查询 opportunistic 晋升。revision/SchemaRange 测试同步到19。

- [ ] **Step 4：绿与历史回归**：`cargo test -p yi-agent-store --lib session_delegation_lifecycle_migration && cargo test -p yi-agent-store --test schema_compat`；旧 foreground migration 测试改为同时断言 role=SessionContainer、classification=legacy，不能靠删测试掩盖旧设计替代。
- [ ] **Step 5：提交**：fmt 后提交 `feat: persist session container roles without rewriting attempts`。

## Task 3：supervisor 统一角色策略、mailbox 与关闭

**Files:** core `src/subagent/supervisor.rs`；`tests/subagent_supervisor.rs`。

**Interfaces:** 消费 AgentTask.role；新增
`AgentSupervisor::new_session_container(session: RootSessionId) -> Self`、
`close_session_container(&mut self) -> Result<Vec<TaskId>, String>`。
容器 open/closed 的内存位由 coordinator hydration 设置；supervisor 不验证 IPC secret。

- [ ] **Step 1：红测试**，复用该文件已有 fail_task API：

```rust
#[test]
fn session_delegation_lifecycle_old_terminal_is_not_container_admission() {
    use yi_agent_core::subagent::task::{AgentTask, AttemptId, TaskDepth, TaskFailure, TaskRole};
    let (session, root, attempt) = (RootSessionId::new(), TaskId::new(), AttemptId::new());
    let mut historical = AgentTask::hydrated_review_task(session, root.clone(), None,
        TaskDepth::Root, attempt.clone(), 1, TaskState::Failed(TaskFailure::new("old failure")));
    historical.hydrate_session_role(TaskRole::SessionContainer).unwrap();
    let mut s = AgentSupervisor::from_hydrated_review_root(historical, "old objective".into());
    let child = s.spawn(root.clone()).unwrap();
    assert_eq!(s.task(&child).unwrap().role(), TaskRole::Execution);
    assert_eq!(s.task(&root).unwrap().active_attempt_id(), &attempt);
    assert!(matches!(s.task(&root).unwrap().state(), TaskState::Failed(_)));
    s.close_session_container().unwrap();
    assert!(s.spawn(root).is_err());
}
```

另在 supervisor.rs 内部测试用真实 `AgentTask::hydrated_review_task` 为八种 Settled
状态各造容器/Execution，调用 `spawn_with_objective`；容器允许、Execution ParentSettled。
给容器投递完成报告成功；同状态 Execution recipient 拒绝；关闭容器不接受新业务消息。
新增 `session_delegation_lifecycle_execution_stays_settled`，保留原级联测试断言。

- [ ] **Step 2：确认红**：`cargo test -p yi-agent-core --test subagent_supervisor session_delegation_lifecycle`。
- [ ] **Step 3：实现角色策略**：

```rust
// 在 spawn_with_objective 与各 mailbox sender/recipient 检查中共用该语义。
fn execution_is_settled(task: &AgentTask) -> bool {
    task.role() == TaskRole::Execution
        && classify_terminal(task.state()) == TerminalKind::Settled
}
```

`settle_terminal` 对容器不因历史 state 级联；`start_task/start_worker/retry_task` 拒绝容器
执行事件，不让它进入 worker factory。容器关闭显式遍历取消后代；已关闭重复调用幂等。
审计 `deliver_message`、completion snapshot/ack、review/交付、reconcile 的所有终态检查：
只读查询历史允许，Execution sender/recipient 的原语义保持（包括原来对 recovery_required 的判断）。
不能机械地把所有 `is_terminal()` 替换成 classify_terminal。

- [ ] **Step 4：绿**：`cargo test -p yi-agent-core --test subagent_supervisor && cargo test -p yi-agent-core --lib session_delegation_lifecycle`；覆盖深度、child 上限、叶子拒绝、关闭后 descendant 取消。
- [ ] **Step 5：提交**：fmt 后提交 `feat: separate container admission from execution terminal state`。

## Task 4：coordinator、watchdog、启动恢复与回收全路径分流

**Files:** store `src/runtime.rs`、`src/runtime/foreground.rs`、`src/repository/{watchdog,application_root,read}.rs`；新建 `tests/session_delegation_lifecycle.rs`。

**Interfaces:** 新建 application root 选用 `AgentSupervisor::new_session_container`；hydration 每 task 读取 role。
`RuntimeRepository::task_role` 是预算的唯一角色来源；旧 ForegroundClassification 仅管授权，不能继续决定预算。

- [ ] **Step 1：红测试**，在 repository 的 session_container_tests.rs 写候选测试：

```rust
#[test]
fn session_delegation_lifecycle_container_is_absent_from_worker_watchdog() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = RuntimeRepository::open(dir.path().join("runtime.sqlite")).unwrap();
    let (sid, root, attempt) = (RootSessionId::new(), TaskId::new(), AttemptId::new());
    r.create_task_with_attempt(&root, &sid, &attempt, 1, "running").unwrap();
    r.record_application_root_attachment("thread:new", &sid, &root,
        "digest", "secret", "/tmp/workspace").unwrap();
    assert_eq!(r.task_role(&root).unwrap(), TaskRole::SessionContainer);
    assert!(r.active_attempt_watchdogs().unwrap().iter().all(|w| w.task_id != root));
}
```

另用现有 `tests/runtime_coordinator.rs` 的 factory/工作目录夹具复制到新测试文件
（保留真实 factory impl，不用空壳 mock coordinator），增加
`session_delegation_lifecycle_worker_budget_and_scheduled_root_unchanged`：可控时钟推进到301/2701，
容器无终态，child 和 scheduled root 仍命中 watchdog；用 start 计数断言容器从不进 factory。

- [ ] **Step 2：确认红**：`cargo test -p yi-agent-store session_delegation_lifecycle_container_is_absent_from_worker_watchdog`。
- [ ] **Step 3：实现**：候选 SQL 加 `tasks.role='execution'`；终态提交事务再次读 role，
返回现有 skipped-container verdict。recover_inflight_tasks/recovered_tasks/orphan reclaim/
release_leases_of_settled_tasks 不把历史 terminal 容器当 worker，且不提前释放未知执行的 lease。
open() hydration 不依赖 foreground activation switch：SessionContainer root 留容器、Execution
child 停放，只有有持久容器的 session 可允许“无 recovered worker root”，其他仍报损坏。
trace 可按保留期清理，但不能据此删除开放容器、mailbox、报告/交付或接续记录。

- [ ] **Step 4：绿与旧快照竞态**：`cargo test -p yi-agent-store --test session_delegation_lifecycle && cargo test -p yi-agent-store --lib session_delegation_lifecycle`；在候选读出后设置 barrier、迁移为容器再放行终态提交，断言旧候选不生效、attempt/event 快照不变。
- [ ] **Step 5：提交**：fmt 后提交 `fix: exclude all session containers from worker lifecycle sweeps`。

## Task 5：身份接续、持久 CAS 和按代次的停止证明

**Files:** 新建 store `src/runtime/session_delegation.rs`；修改 `src/runtime.rs`、`src/runtime/foreground.rs`、`src/repository/session_container.rs`、`src/ipc/{authority,foreground}.rs`。

**Interfaces:** 实现公共契约中的 prepare_session/commit_session_binding/may_replace_driver。
新增 `RuntimeCoordinatorError::SessionDelegation(SessionFailure)`；角色与生命周期错误不使用自由文本分支。

- [ ] **Step 1：红测试**，新 runtime 模块内写最小停止边界：

```rust
#[test]
fn session_delegation_lifecycle_replacement_needs_both_stop_facts() {
    assert!(!may_replace_driver(false, false));
    assert!(!may_replace_driver(true, false));
    assert!(!may_replace_driver(false, true));
    assert!(may_replace_driver(true, true));
}
```

另在 repository 测试构造 `SessionBindingCommit`（字段见契约），第一次 expected_epoch=0
提交 epoch=1；相同 op/digest 重试还是1；不同 op 同 expected_epoch=0 拒绝；插入 operation
后注入事务失败，容器 epoch 仍0且内存未发布 binding。测试名
`session_delegation_lifecycle_binding_cas_and_rollback`。

- [ ] **Step 2：确认红**：`cargo test -p yi-agent-store --lib session_delegation_lifecycle_binding_cas_and_rollback`。
- [ ] **Step 3：实现以下事务和停止登记，不调用 retry/start_worker**：

```text
prepare_session:
  验证 role / attachment / tombstone / 项目范围
  managed => 现有 AuthorityTable::Business 验证；RecoveryOnly 不准业务绑定
  embedded => SHA-256 校验私有 resume secret；仅无 managed 约束时接受
  legacy => 校验调用者已持有的旧 capability；CAS 消耗一次；写新 resume digest
  若 current_binding 的 capability/epoch/generation 均有效且 owner 相同，直接复用，不增加 epoch
  只有失效或真正替换绑定才走以下冻结/停止/CAS；仅 host_run_id 相等不足以取回活 binding secret
  在 gate 下冻结将被替换的 driver epoch，读取旧宿主执行集合与冲突 Execution
  gate 外验证受信 stop refs；无证据保持 unknown，不主动隐藏地停止
  gate 内重核集合版本、owner、epoch、权限；事务 CAS 写记录/operation
  提交成功才安装当前 generation 的 runtime-only binding secret
```

复用已有 `ExecutionIdentity/StopEvidence/ProcessIdentity`，不接受公开 RPC 自证。
停止证据来源必须实际接通：managed 复用私有桥的 guardian/daemon 证据；embedded 新增
仅宿主绑定能力可开的控制流，能力不交给工具。控制流先由 daemon 签发 ExecutionIdentity，
宿主以 ManagedRun join + 工具进程组观察回报，daemon 核对 channel owner/fence/identity 后
作为 EvidenceSource::Host 写证据；工具只有 turn secret，不能开控制流。断流写 unknown，
绝不当 stopped；新宿主不能给旧宿主补写“已停”，缺证据须受信恢复。
Task 6 定义控制帧，Task 7 实现客户端，Task 9/10 在真实 join/工具组退出处上报。
使用 Task 2 已建的持久 `session_host_executions` 表：root、generation、binding/driver epoch、turn、
execution identity JSON、phase(starting/running/unknown/stopped)、受信证据 JSON、集合版本；
在启动前登记、完成证明后提交，不以进程/内存表消失删除 unknown。worker 停止仍走原表/lease。
存活 child 不属于替换 driver 的停止集合；daemon 重启 child 保持 parked，未证明释放的
资源不归还；共享 cwd 的写冲突保守阻塞，无冲突只读可接续。

CAS 成功但应答丢失：同进程相同 operation 可返回同 binding；进程重启后须新 operation，
不复活旧 secret。关闭/删除保留 tombstone；普通 detach 不写永久 closed。

- [ ] **Step 4：绿与授权负例**：`cargo test -p yi-agent-store --lib session_delegation_lifecycle`；再跑 `cargo test -p yi-agent-store --test managed_attach_authority`。补齐 `session_delegation_lifecycle_legacy_identity_is_not_public_key`：仅 thread key、错误旧 secret、二次升级拒绝；合法持有者升级不改 attempts/events；managed 失败不接受 embedded。
- [ ] **Step 5：提交**：fmt 后提交 `feat: bind session delegation with identity and stop proof fences`。

## Task 6：turn 写准入及所有 IPC 路由防绕过

**Files:** 新建 store `src/ipc/session_delegation.rs`；修改 `src/ipc/{mod,protocol,daemon,error,managed}.rs`、`src/runtime{.rs,/session_delegation.rs}`；`tests/{subagent_ipc_protocol,subagent_fork_upload}.rs`。

**Interfaces:** 实现 begin/end/authorize_session_write；在 IpcRequest 添加：

```rust
AttachSessionContainer {
    idempotency_key: String, workspace: PathBuf, extra_roots: Vec<String>,
    operation_id: String, creation_resume_digest: Option<String>,
    identity: Option<SessionIdentity>,
},
PrepareSessionDelegation { request: PrepareSession },
BeginSessionTurn { binding: SessionBinding, turn_id: String },
EndSessionTurn { turn: SessionTurn },
OpenSessionHostControl { binding: SessionBinding },
```

OpenSessionHostControl 建立专用宿主流，不是模型工具；成功响应 `SessionHostControlReady`。
其后仅此流解析新 DTO `SessionHostControlRequest`：

```rust
pub enum SessionHostControlRequest {
    Register { turn_id: String },
    Stopped { evidence: yi_agent_lifecycle::foreground::StopEvidence },
}
pub enum SessionHostControlResponse {
    Registered { execution: yi_agent_lifecycle::foreground::ExecutionIdentity },
    StopRecorded,
    Rejected { failure: SessionFailure },
}
```

Register 的 execution_id 由 daemon 生成，先落盘再应答；Stopped 只接受此控制流自己的
execution/fence，仍要 joined 与 tools_stopped 双证据，不得替别的宿主/worker上报。
业务 command enum 不含 Stopped；带 turn secret 开控制流稳定拒绝 AuthorityDenied。
客户端 SessionHost 新增 `register_execution(&mut self, turn_id: &str) ->
Result<ExecutionIdentity, SessionClientError>`、`record_stopped(&mut self, evidence: StopEvidence) ->
Result<(), SessionClientError>`，Task 7 实现，Task 9/10 调用；类型使用 lifecycle 现有 DTO。
控制流启动前端若崩溃，持久 starting/unknown 保留，不自动清除。

AttachSessionContainer 首次创建验证非空摘要并原子绑定；命中既有记录时可以返回无秘密定位，
不凭新提交的摘要替换 owner。旧 AttachApplicationRoot 命中容器只给 ProtocolMismatch，
不能再取回 capability。新响应 `SessionContainerAttached { root: SessionRef, workspace: WorkerWorkspace }`
不含秘密；managed 的 reserve/install 仍走 guardian 私有桥，公开 attach 不能强改 managed 身份。
PrepareSessionDelegation 才签发当前绑定能力。
响应另添加 `SessionPrepared { binding: SessionBinding }`、`SessionTurnOpened { turn: SessionTurn }`、
`SessionTurnClosed`、`SessionDelegationRejected { failure: SessionFailure }`。
所有可来自会话的写 request 加 `#[serde(default)] session_turn: Option<SessionTurn>`，
默认只为解码后给出明确拒绝，不表示允许无 turn。coordinator 内部公共实现也必须接收/重核。

- [ ] **Step 1：红测试**，纯 turn 比较放 runtime 新模块：

```rust
#[test]
fn session_delegation_lifecycle_old_turn_is_not_refreshed() {
    use yi_agent_lifecycle::foreground::SecretCapability;
    let a = SessionTurn {
        fence: SessionFence { root: SessionRef { session_id: "s".into(), root_id: "r".into() },
            generation: "g".into(), binding_epoch: 1, driver_epoch: 1 },
        turn_id: "a".into(), capability: SecretCapability::new("secret-a"),
    };
    let mut b = a.clone(); b.turn_id = "b".into();
    assert_eq!(validate_turn(&b, &a, true), Err(SessionFailureCode::StaleTurn));
    assert_eq!(validate_turn(&a, &a, false), Err(SessionFailureCode::AdmissionClosed));
    assert_eq!(validate_turn(&a, &a, true), Ok(()));
}
```

- [ ] **Step 2：确认红**：`cargo test -p yi-agent-store --lib session_delegation_lifecycle_old_turn_is_not_refreshed`。
- [ ] **Step 3：实现**：begin 同 binding+turn 重试幂等；不同 turn 在旧 turn 关闭确认前拒绝。
End 对匹配 turn 幂等，不取消 child；旧 turn End 不关闭新 turn。写操作重核顺序为 generation、
真实 role、binding、driver、turn、secret、open；secret 常量时间/既有 digest 比较，不记录原文。

逐条路由审计（不能只检查 SpawnApplicationChild）：

| 路由 | SessionContainer | Execution |
|---|---|---|
| SpawnChild / SpawnApplicationChild | 必须当前 turn；旧普通入口不能无 turn 直通 | 自身 task/attempt 能力及 ParentSettled；application 入口拒绝 |
| Begin/Chunk/Finish/Consume ForkUpload | 每阶段同 turn；token 记录 fence；拒绝不消费 | 原 task/attempt 与大小/TTL 检查 |
| SendMessage / SendApplicationMessage / SendUserMessage | 当前 turn + 原邻接/子树权限；宿主用户注入走受信宿主权限而非伪装 worker | 原 sender/recipient 规则 |
| CancelChild / PreviewCancel / ConfirmCancel / CancelThreadTasks | 当前 turn 或受信宿主取消权限；确认绑定范围/执行集合版本 | 原取消能力与范围；不能按 task id 接管 |
| mailbox ack、交付确认/持久 review 操作 | 写入时检查当前 turn；只读 wait/inspect/history 不需旧 turn 开放 | 原权限 |
| Detach/close/恢复命令 | 专门宿主绑定/恢复权限；不能用旧 turn 重新开放 | 不晋升 |

fork 上传预检和 spawn commit 的 fence 必须相同；spawn 拒绝不消费合法上传，end 后过期上传
只可清理。新终态 child 结果投递由 daemon 的执行能力授权，不被已结束的父 turn 阻断。

- [ ] **Step 4：绿与协议测试**：`cargo test -p yi-agent-store --lib session_delegation_lifecycle && cargo test -p yi-agent-store --test subagent_ipc_protocol && cargo test -p yi-agent-store --test subagent_fork_upload`。新增 `session_delegation_lifecycle_no_legacy_write_bypass`，对上表各写路由用缺 turn、旧 turn、child 冒用 application 三组负例，断言 child/mailbox/upload 数量不变。
- [ ] **Step 5：提交**：fmt 后提交 `feat: fence every session write by immutable turn identity`。

## Task 7：宿主私有凭证存储与接续客户端

**Files:** 新建 subagent `src/{session_identity,session_delegation}.rs`；修改 `src/{lib,thread_root,attach,binding}.rs`；新建 `tests/session_delegation_client.rs`。

**Interfaces:** 实现公共契约 SessionHost/SessionTurnClient/SessionClientError；另定义：

```rust
pub struct SessionIdentityStore { /* private directory */ }
impl SessionIdentityStore {
    pub fn new(directory: PathBuf) -> Self;
    pub fn load_or_create(&self, thread_id: &str)
        -> std::io::Result<yi_agent_lifecycle::foreground::SecretCapability>;
    pub fn remove(&self, thread_id: &str) -> std::io::Result<()>;
}
```

目录由宿主会话存储提供，不用 cwd 推断身份。新建用已有安全随机源；文件新建0600、目录0700，
拒绝 symlink/非法 thread id，使用 create_new 和原子 rename/fsync；并发创建 loser 读取 winner，
不能覆盖。现有文件损坏/权限不安全时拒绝，不当“无文件”重建。

- [x] **Step 1：红测试**，在 session_identity.rs：

```rust
#[test]
fn session_delegation_lifecycle_resume_secret_is_private_and_stable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let store = SessionIdentityStore::new(dir.path().join("identities"));
    let a = store.load_or_create("thread-one").unwrap();
    let b = store.load_or_create("thread-one").unwrap();
    assert_eq!(a, b);
    let path = dir.path().join("identities/thread-one.json");
    assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(store.load_or_create("../escape").is_err());
}
```

新增客户端测试 `session_delegation_lifecycle_single_shot_lost_response`：临时 UnixListener 接收一次
spawn、记录 child 创建、丢响应；客户端不得再发第二个 spawn，不将旧调用换 turn 重试。
server 线程必须 join，超时 socket 释放后清理，不留下悬挂测试。

- [x] **Step 2：确认红**：`cargo test -p yi-agent-subagent session_delegation_lifecycle_resume_secret_is_private_and_stable`。
- [x] **Step 3：实现**：使用 Task 6 的 AttachSessionContainer；先在宿主0600存储落盘凭证，
新建才提交 `creation_resume_digest`，既有容器用 identity prepare，不用旧公开 attach 领秘密。
为 ThreadRoot 添加 `pub fn attach_session_container(&self, identity: &SessionIdentity,
operation_id: &str, creation_resume_digest: Option<String>) -> Result<SessionRef, SessionClientError>`：
缓存稳定 SessionRef/workspace，不伪造 legacy AttachedRoot.capability。SessionHost::prepare 调用
该接口定位，然后 PrepareSessionDelegation；准备成功将 binding secret 仅留在 SessionHost；begin 返回 turn secret 后才构造本轮 RuntimeHandle，
其 capability 字段设为 turn secret，不把宿主控制能力交给 SessionTurnClient。
后续所有会话工具只使用本轮 client；ThreadRoot 旧 handle() 仅供旧模式/Execution，不用于新模式业务。
同 key 已存在只返回无秘密 SessionRef/工作目录，不再返回历史 capability。
旧 daemon 未广告能力则宿主显式报 ProtocolMismatch；不能调用旧 attach 领 secret 再 upgrade。
已运行宿主缓存的旧 capability 可升级：先落盘新 resume secret，再提交其摘要。
SessionHost 在会话 driver 生命周期中复用，不每轮重建；prepare 携带自己的 current_binding，
有效时复用且 epoch 不变，失效才安全接续。添加 `session_delegation_lifecycle_prepare_reuses_live_binding`
测试：两轮 prepare/begin/end 的 binding/driver epoch 相同、turn 不同、child 不重启；无 binding
capability 仅同 host_run_id 的新实例不能取回活 secret。
SessionHost 仅在 prepare/begin 时重连，工具使用捕获的 handle/token；finish 失败保留本地关闭
和未确认状态。创建应答丢失后用预存凭证证明身份，不新增容器。
`ThreadRoot::from_handle` 不自动创建 SessionHost，worker 工具仍保持 Execution 路径。

- [x] **Step 4：绿**：`cargo test -p yi-agent-subagent --lib session_delegation_lifecycle && cargo test -p yi-agent-subagent --test session_delegation_client && cargo test -p yi-agent-subagent --test attach_delegation`；断言 runtime DB、日志、thread JSONL、fork transcript 都不出现新长期 secret。
- [x] **Step 5：提交**：fmt 后提交 `feat: reconnect session hosts with private resume credentials`。

  已提交：`3e4accbf`。


**完成证据（2026-11-05）：**

- 私有凭证存储 `crates/yi-agent-subagent/src/session_identity.rs`：目录 0700、文件 0600、
  `create_new` + 临时文件 + 原子 rename/fsync、并发 loser 读 winner、拒 symlink 与非法 thread id、
  损坏或不安全权限拒绝而非重建；`resume_digest`/`fingerprint` 供线上使用。
- 客户端 `crates/yi-agent-subagent/src/session_delegation.rs`：`SessionHost`（prepare / begin_turn /
  finish_turn / register_execution / record_stopped）、`SessionTurnClient`（固定 handle + 本地关闭闸门）、
  `HostControl`（独立控制流，binding capability 才能打开）。turn capability 只放本轮 `RuntimeHandle`，
  宿主 binding 不出 `SessionHost`。
- `ThreadRoot::attach_session_container` 只返回无秘密 `SessionContainerAttachment`；`creation_resume_digest`
  只在新建容器时发送，接管既有容器不发送（避免身份劫持）。
- 泄漏断言：`session_delegation_lifecycle_resume_secret_never_reaches_durable_state` 递归扫描运行目录
  （runtime DB 及 WAL/SHM、其余运行产物），豁免项只有 0600 凭证目录本身；同时断言 DB 内确实存在
  resume digest，避免"扫了个空目录"式的假通过。
- 测试：`cargo test -p yi-agent-subagent --offline --test session_delegation_client` 8 passed；
  `cargo test -p yi-agent-subagent --offline` 全绿；`cargo test -p yi-agent-store --offline
  --features foreground-test-support` 全绿。

## Task 8：run 级工具快照，不让旧调用偷取新能力

**Files:** core `src/{agent,tool}.rs`；subagent `src/{lib,session_delegation}.rs`；`tests/session_delegation_client.rs`。

**Interfaces:** core 添加
`Agent::set_tools_for_next_run(&mut self, tools: Arc<ToolRegistry>)`，只替换下次 start_run_inner 克隆的 Arc；
实现 `register_session_turn_tools`。所有会话委派工具持有 `Arc<SessionTurnClient>`；
写工具要求 turn 仍开放，wait/inspect 等只读操作验证 daemon 签发的 turn secret 与当前有效
binding，但不要求该 turn 仍开放；新 turn 建立后旧只读 token 可失效，由新工具继续查询。
不使用旧 ThreadRoot::handle() 重新领身份。恢复新 binding 后由新 turn 工具表承接只读查询。

- [x] **Step 1：红测试**，在 core agent.rs 内部测试直接检验旧快照：

```rust
#[test]
fn session_delegation_lifecycle_run_tools_are_an_immutable_snapshot() {
    let first = Arc::new(ToolRegistry::new());
    let second = Arc::new(ToolRegistry::new());
    let provider = ScriptedProvider::new(vec![]);
    let mut agent = Agent::new(Arc::new(provider), first.clone(), AgentConfig::default());
    let captured = agent.tools();
    agent.set_tools_for_next_run(second.clone());
    assert!(Arc::ptr_eq(&captured, &first));
    assert!(Arc::ptr_eq(&agent.tools(), &second));
}
```

该测试放 `agent.rs` 已有测试模块，`ScriptedProvider::new` 已存在，不新建伪 provider。
行为测试 `session_delegation_lifecycle_delayed_old_tool_cannot_borrow_new_turn`：用两个本轮工具表
和 barrier，让 A 的工具等到 B 已开启才执行；断言 A 返回 StaleTurn/本地 closed，wire 捕获中
没有 B 的 secret，B spawn 成功；fork begin 也使用同一个 A 上下文而不是另取 handle。

- [x] **Step 2：确认红**：`cargo test -p yi-agent-core --lib session_delegation_lifecycle_run_tools_are_an_immutable_snapshot`。
- [x] **Step 3：实现**：

```rust
pub fn set_tools_for_next_run(&mut self, tools: Arc<ToolRegistry>) {
    self.tools = tools;
}
// 宿主在每轮模型调用前：
let mut next_tools = (*agent.tools()).clone();
register_session_turn_tools(&mut next_tools, turn.clone(), controller, thread_id, caller);
agent.set_tools_for_next_run(Arc::new(next_tools));
```

替换五个委派工具的会话版本（spawn/send/wait/inspect/cancel），不能留下仍读共享 current-turn
的旧 spawn/fork/cancel 对象。核心 start_run_inner 已克隆 Arc，确认 provider 自动重试/同 run
多次 ACT 仍保持该 Arc；新 turn 不修改旧工具。只读查询保留权限及终态结果读取。

- [x] **Step 4：绿**：`cargo test -p yi-agent-core --lib session_delegation_lifecycle && cargo test -p yi-agent-subagent --test session_delegation_client`；再跑 subagent 的现有 fork tests，验证拒绝不消耗上传。
- [x] **Step 5：提交**：fmt 后提交 `feat: capture delegation capability in each run tool snapshot`。

  已提交：`8c7f53a1`。


**完成证据（2026-11-05）：**

- core `crates/yi-agent-core/src/agent.rs`：`Agent::set_tools_for_next_run` 只替换下一次 run 的结果，
  `start_run_inner` 仍在运行时克隆一次 `Arc`；测试
  `session_delegation_lifecycle_run_tools_are_an_immutable_snapshot` 用 `Arc::ptr_eq` 钉住。
- subagent `register_session_turn_tools`（`crates/yi-agent-subagent/src/lib.rs`）：五个委派工具
  （spawn/send/wait/inspect/cancel）全部持有 `Arc<SessionTurnClient>`，不再走 `ThreadRoot::handle()`
  的旧 application capability；spawn 的 fork 上传也改用本轮 turn（`upload_forked_context_via_turn`）。
- 写工具先过本地 `ensure_open()`，再由 daemon 复核 turn；读工具用 `SessionTurnClient::send_read`，
  不要求 turn 仍开放，所以一个已结束的回合仍能读自己启动的 child 结果。
- store：`RuntimeCoordinator::is_issued_session_turn` + `authorize_issued_session_turn` 让 wait/inspect
  接受本代次签发的 turn secret；`EndSessionTurn` 改为标记关闭而非删除记录，否则"关闭"会等于
  "本轮历史不可读"。`CancelChild` / `BeginForkUpload` 补 `session_turn` 并按容器写准入检查。
- 行为测试 `session_delegation_lifecycle_delayed_old_tool_cannot_borrow_new_turn`：A 回合的 spawn
  工具在 B 开启后调用被拒，A 关闭后 inspect 仍可读，B 的工具仍可 spawn。
- 变异验证：去掉本地 `ensure_open` 后 daemon 仍以 `StaleTurn` 拒绝；去掉
  `authorize_issued_session_turn` 后关闭回合的 inspect 立刻 `authority_denied`。两者都退化为失败，
  证明断言不是空转。
- 测试：`cargo test -p yi-agent-subagent --offline`、`-p yi-agent-store --offline --features
  foreground-test-support`、`-p yi-agent-core --offline --lib`、`-p yi-agent-app-server`、`-p yi-agent
  --jobs 2` 全绿；`cargo clippy --workspace --all-targets` 无警告。

## Task 9：app-server/desktop/Kanban 每回合接线

**Files:** app-server `src/server/{assembly,foreground}.rs`、`src/server/driver/turn_driver.rs`、`src/thread_store.rs`、`src/server/tests/{assembly,turn,thread_lifecycle}.rs`；subagent `src/session_delegation.rs`。

**Interfaces:** 消费 SessionHost + SessionIdentityStore + set_tools_for_next_run；ThreadStore 新增
`pub fn session_identity_directory(&self) -> PathBuf` 返回 `root/.session-identities`；不写入 ThreadMeta。
`foreground` 的连接结果必须区分 Embedded / ManagedAuthorized / ManagedRejected，不能把后两者
都压成 Option::None 再 fallback。

- [x] **Step 1：红测试**：在 `server/tests/turn.rs` 使用现有 Harness 和真实 turn/start，先写每轮准备的最小入口测试：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn session_delegation_lifecycle_app_server_prepares_every_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    for id in [31, 32] {
        h.send(&serde_json::json!({"jsonrpc":"2.0", "id":id, "method":"turn/start",
            "params":{"threadId":tid, "input":[{"type":"text","text":"hello"}]}}).to_string()).await;
        let mut prepared = false;
        loop {
            let value = h.read_value().await;
            if value["method"] == "thread/delegationStatus/updated" {
                prepared |= value["params"]["ready"] == true;
            }
            if value["method"] == "turn/completed" { break; }
        }
        assert!(prepared, "each turn must announce daemon-confirmed delegation readiness");
    }
    h.shutdown().await;
}
```

本任务新增 `thread/delegationStatus/updated` 通知契约：params 为 `{threadId, turnId,
ready: bool, failure: Option<SessionFailure>}`，不含能力秘密；ready 仅在 begin 确认后发。
追加 `session_delegation_lifecycle_app_server_two_turns_keep_background_child`：用
`build_test_agent_with_provider(provider, session, cwd, mode)`（现有签名）注入本任务新增
`DelegationProvider`。provider 按当前 prompt 的唯一标记为每轮发一次真实 spawn tool use，
tool result 后 EndTurn；标题请求单独 EndTurn；child 目标无父标记则等待测试 channel 完成。
每轮脚本使用真实 ProviderEvent：

```rust
vec![
    E::ToolUseStart { id: "spawn-a".into(), name: "spawn_agent".into() },
    E::ToolUseDelta { id: "spawn-a".into(), partial_json:
        serde_json::json!({"task":"child without parent marker", "fork":false}).to_string() },
    E::ToolUseEnd { id: "spawn-a".into() },
    E::Stop { reason: StopReason::EndTurn },
]
```

`DelegationProvider` 实现 `Provider::call_stream(&self, req: ProviderRequest) ->
Result<BoxStream<'static, ProviderEvent>, ProviderError>`，持有已发父标记集合与 child channel。
第二轮 inspect 第一轮 child 的 id，查询实际 parent/root，断言正常 finish 未发 child cancel；
最后通过 channel 结束 child 并 join。参数化普通/Kanban标记，不在生产路径判断卡片 state。
`thread/delete` 测试断言 tombstone 存在，旧绑定 spawn 拒绝、只读历史仍授权可查。

- [x] **Step 2：确认红**：`cargo test -p yi-agent-app-server session_delegation_lifecycle_app_server`，确认实际命中测试，非0。

  实际执行（`--offline`，worktree `session-delegation-impl`）：

  ```
  cargo test -p yi-agent-app-server --offline --lib -- session_delegation_lifecycle_app_server
  # 3 个用例全部命中并通过：
  #   session_delegation_lifecycle_app_server_prepares_every_turn
  #   session_delegation_lifecycle_app_server_two_turns_keep_background_child
  #   1 filtered out（旧名匹配）
  ```
- [ ] **Step 3：实现**：保留稳定 foreground_root，但把“一次 activation”与每轮 delegation prepare
分开；每轮模型前 prepare/begin、装工具；所有正常/中断/断连/clear/compact/错误返回统一 finish。
使用已有 `Agent::run_tracked_blocks(blocks)` 而不是会丢弃控制句柄的 run_blocks；返回
`TrackedAgentRun { events, control }`，沿用普通事件流和主会话预算。start 前通过宿主控制流
register_execution，全部退出路径在本地关闭 turn 后调用
`control.stop_and_join(Duration::from_secs(2)).await`，再读取实际工具组观察结果提交证据。
不使用 side_defaults 预算；不以 Dropped stream/Done event 代替 join，不以 join 代替 groups-gone。
显式保留的后台 process 保持独立登记/资源占用，正常 finish 不调用 ProcessManager::shutdown
杀服务，也不伪造其停止；同绑定新 turn 可继续使用其受控身份，替换宿主时仍按冲突与停止
边界决定，不因它还活着把旧 turn 写权限重新开放。
本地 close 立即生效；daemon 未确认关闭时后续 turn 仍可对话，但写工具明确 unavailable，
不能复用上一轮工具。失败写入明确 Notice/状态字段，不只 tracing warning。
宿主退出按租约释放范围处理，独立 child 的处置遵循既有 owner release 规则；正常 turn finish
不等同 owner release。ThreadStore fork 不复制 `.session-identities`，新会话生成独立凭证。

- [x] **Step 4：绿与重启**：`cargo test -p yi-agent-app-server session_delegation_lifecycle && cargo test -p yi-agent-app-server two_threads_in_one_cwd_share_the_daemon_but_not_the_root`；同一临时库重建宿主，合法 identity 可接续、旧 turn 拒绝，另一会话 B 不换 epoch。

  实际执行：

  ```
  cargo nextest run -p yi-agent-app-server --offline
  # Summary [ 30.186s] 1446 tests run: 1446 passed, 7 skipped

  cargo nextest run -p yi-agent-subagent --offline
  # Summary [ 10.424s] 187 tests run: 187 passed

  cargo nextest run -p yi-agent-store --offline --features foreground-test-support
  # Summary [  4.581s] 534 tests run: 534 passed

  cargo clippy --offline --workspace --all-targets \
    --features yi-agent-store/foreground-test-support -- -D warnings
  # 无 error / 无 crate warning（仅 ts-rs 与 html5ever 的既有提示）
  ```

  重启语义由两个新集成用例钉住（`crates/yi-agent-subagent/tests/session_delegation_client.rs`，真实 Daemon + 持久库）：

  * `session_delegation_lifecycle_a_new_host_after_a_frontend_restart_delegates`
    —— 新宿主无内存绑定，仅凭落盘 resume secret 重新证明身份并再次 `spawn`；
  * `session_delegation_lifecycle_an_unproven_restart_is_refused_not_guessed`
    —— 崩溃宿主留下 `starting` 行时，新宿主得到 `StopUnproven`，不把缺记录当停止证明。

  未证明停止的拒绝也由 store 用例加固
  （`crates/yi-agent-store/tests/managed_attach_authority.rs`）：
  `session_delegation_lifecycle_an_unjoined_stop_is_not_a_stop_proof`。

  身份三态（`Embedded` / `Managed` / `Refused`）在
  `crates/yi-agent-subagent/src/thread_root.rs` 的 `ForegroundAuthority` 落地，
  不再压成 `Option::None`；`crates/yi-agent-app-server/src/server/delegation.rs`
  的 `a_refused_authority_is_not_downgraded_to_embedded` 钉住“拒绝不等于嵌入”。
- [x] **Step 5：提交**：fmt 后提交 `feat: prepare fenced delegation on every app server turn`。

## Task 10：TUI/headless 接线与观察语义

**Files:** CLI `src/{main,session_store}.rs`、`src/tui/subagents.rs`；store `src/ipc/protocol.rs`、`src/repository/read.rs`；新增 `yi-agent-rs/crates/yi-agent/tests/session_delegation_lifecycle.rs`。

**Interfaces:** 消费 Task 7/8；`TuiSessionStore` 委托 ThreadStore 的 identity 目录；headless
有持久 thread 时复用同目录，无持久 thread 时创建进程独有会话/凭证（不承诺重新启动定位同会话）。
IpcTaskDetail/IpcTaskSummary 添加 role、container_activity、historical_execution_state；容器 state 展示字段标注历史，不把 stalled 当当前能力；Execution 旧 state 语义不变。
active-only 查询的 SQL 也按角色/容器关闭记录分流，不能因为旧 state terminal 隐藏仍开放的
会话；container_activity 为 unknown 时如实返回，不用历史 running/stalled 补值。

- [ ] **Step 1：红测试**：在新集成测试声明 `mod common;`，只使用其不访问真实 API 的
`run_command_with_timeout`。下面的 `SpawnGateway` 是本任务新增测试夹具，不是现有 API：

```rust
#[test]
fn session_delegation_lifecycle_headless_entry_uses_container_context() {
    use std::{process::Command, time::Duration};
    let dir = tempfile::tempdir().unwrap();
    let gateway = SpawnGateway::start();
    let mut command = Command::new(env!("CARGO_BIN_EXE_yi-agent"));
    command.arg("--workdir").arg(dir.path())
        .args(["--provider", "openai", "--api-key", "test-key", "--model", "test-model", "--yolo"])
        .arg("--api-url").arg(&gateway.url)
        .args(["run", "--json", "--subagents", "PARENT_SPAWN_ONCE"])
        .env("YI_AGENT_HOME", dir.path().join("instance"))
        .env("YI_AGENT_RUNTIME_DIR", dir.path().join("runtime"))
        .env("YI_AGENT_BRINGUP", "embedded");
    let output = common::run_command_with_timeout(&mut command, Duration::from_secs(60)).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let events = common::parse_events(&String::from_utf8_lossy(&output.stdout));
    let spawned: Vec<_> = events.iter().filter_map(|e| e.get("ToolResult")).collect();
    assert_eq!(spawned.len(), 1, "must execute the real registered spawn tool");
    assert_eq!(spawned[0]["result"]["is_error"], false);
    let repository = yi_agent_store::repository::RuntimeRepository::open(
        dir.path().join("runtime/runtime.sqlite")).unwrap();
    let mut containers = 0;
    let mut executions = 0;
    for task in repository.task_snapshots().unwrap() {
        let id: yi_agent_core::TaskId = task.task_id.parse().unwrap();
        match repository.task_role(&id).unwrap() {
            yi_agent_core::subagent::task::TaskRole::SessionContainer => containers += 1,
            yi_agent_core::subagent::task::TaskRole::Execution => executions += 1,
        }
    }
    assert_eq!((containers, executions), (1, 1));
}
```

为避免 headless 测试夹具依赖真实模型，新增以下测试 support（本文件内，std HTTP/SSE，
不改 Cargo 依赖）；必要 import 直接使用全路径。Gateway Drop 必须 shutdown+join：

```rust
struct SpawnGateway {
    url: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl SpawnGateway {
    fn start() -> Self {
        use std::{io::{BufRead, Read, Write}, sync::atomic::Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(peer) => peer,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5)); continue;
                    }
                    Err(e) => panic!("mock accept: {e}"),
                };
                socket.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                let mut first = String::new(); reader.read_line(&mut first).unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new(); reader.read_line(&mut line).unwrap();
                    if line == "\r\n" { break; }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                }
                let mut bytes = vec![0; length]; reader.read_exact(&mut bytes).unwrap();
                if first.contains("/responses") {
                    socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let messages = body["messages"].as_array().unwrap();
                let parent = messages.iter().any(|m| m["content"].to_string().contains("PARENT_SPAWN_ONCE"));
                let done = messages.iter().any(|m| m["role"] == "tool");
                let frame = if parent && !done {
                    serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"spawn-1",
                        "type":"function","function":{"name":"spawn_agent","arguments":
                        serde_json::json!({"task":"return a short answer", "fork":false}).to_string()}}]},
                        "finish_reason":"tool_calls"}]})
                } else {
                    serde_json::json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {frame}\n\ndata: [DONE]\n\n");
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        Self { url, stop, thread: Some(thread) }
    }
}
impl Drop for SpawnGateway {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}
```

网关的5ms等待只用于非阻塞 accept，不模拟预算时间或停止证明。CLI 测试还要查询真实 worker
start 事件，容器没有 worker 启动；数据库实际路径以 runtime paths helper 为准，若 override
目录布局与示例不同，使用同一个 helper 建路径并保留角色断言，不扫描用户目录。
TUI 在 main.rs 的实际 turn 装配 helper 以 mock run 跑两轮，测试名
`session_delegation_lifecycle_tui_entry_refreshes_turn_not_worker_identity`。

- [ ] **Step 2：确认红**：`cargo test -p yi-agent --test session_delegation_lifecycle`；初始旧入口无上下文导致拒绝/角色不符。
- [ ] **Step 3：实现**：移除 root_activated 对“本轮是否可以委派”的替代作用；每轮 begin/finish、
换不可变工具表；调用已有 `run_tracked_blocks` 保留 control，执行登记先于模型开始，
所有退出路径真实 stop_and_join 后结合工具组观察上报，不能仅 drop stream。
cancel/错误/drop 路径先 close locally，再等待可信结束/工具组清理。
TUI/headless 不因 fixed binding 就省略 turn；daemon worker 继续从 Execution 注册路径装工具。
观察接口读容器记录与历史快照，禁止把容器绑定成功伪写 `tasks.state_json=running`。

- [ ] **Step 4：绿**：`cargo test -p yi-agent --test session_delegation_lifecycle && cargo test -p yi-agent --bin yi-agent session_delegation_lifecycle`；验证 TUI 退出/取消不回收别人的 runtime，会话 fork 不复制身份。
- [ ] **Step 5：提交**：fmt 后提交 `feat: wire session delegation into tui and headless hosts`。

## Task 11：版本切换、重启/竞态回归、启用与交付

**Files:** store `src/ipc/{protocol,daemon}.rs`、subagent `src/{attach,binding}.rs`；
store `tests/{session_delegation_lifecycle,subagent_ipc_protocol,schema_compat}.rs`；
CLI `tests/session_delegation_lifecycle.rs`、`tests/managed_runtime_lifecycle.rs`；
现有 plugin IPC 客户端中引用 PROTOCOL_VERSION 的构造点（在 crates 内定点 grep 版本符号后更新）；
项目进度、spec 状态与本计划。

**Interfaces:** `PROTOCOL_VERSION=3`；Status 的能力列表在完整接线后新增
SESSION_DELEGATION_CAPABILITY。旧版本写入返回 SessionFailureCode::ProtocolMismatch，
envelope 版本先于 command 解析，不能仅得到 opaque JSON parse error。

- [x] **Step 1：红测试**：新集成测试覆盖真实 Daemon 临时 socket 与持久库：

```rust
#[test]
fn session_delegation_lifecycle_v2_envelope_cannot_issue_unfenced_write() {
    let directory = TempDir::new().unwrap();
    let daemon = Daemon::start(directory.path().join("runtime"),
        directory.path().join("runtime.sqlite")).unwrap();
    let request = serde_json::json!({"protocol_version": 2, "request_id": "old",
        "command": {"type": "SpawnApplicationChild", "session_id": "s",
            "parent_task_id": "p", "capability": "old", "objective": "x"}});
    let frame = serde_json::to_vec(&request).unwrap();
    // 现有 helper 写 JSON + newline，读取真实 response envelope。
    let response = send_in_chunks(daemon.socket_path(), &frame, frame.len());
    assert_eq!(response["request_id"], "old");
    assert_eq!(response["result"]["type"], "SessionDelegationRejected");
    assert_eq!(response["result"]["failure"]["code"], "protocol_mismatch");
}
```

使用 `subagent_ipc_protocol.rs` 已存在的 `send_in_chunks`，不新建 framing helper。
业务 socket 实际为 newline-delimited JSON，不能误写长度前缀协议；EOF/超时不算正确拒绝。
同一组测试对合法 v3 envelope 的未知 command 字段另断言 Validation，证明版本和结构错误分流。

- [x] **Step 2：确认红**：`cargo test -p yi-agent-store --test subagent_ipc_protocol session_delegation_lifecycle_v2_envelope_cannot_issue_unfenced_write`。
- [x] **Step 3：加入确定性故障与并发回归后启用**：

```text
barrier 1: spawn 已鉴权、未提交 -> close 提交 -> spawn 复核失败，child 数不增
barrier 2: spawn 已登记、未启动 -> owner release -> 启动结果被收养并停止，lease 未提前释放
barrier 3: binding DB commit 注入失败 -> 内存 capability 表不增加
barrier 4: 两个 prepare 同 expected epoch -> 只有一个 commit；同 op retry 不加 epoch
restart: 终态历史容器 + parked child -> generation 更新，容器不 recovery_required
restart: unknown driver/tool group -> StopUnproven；不得假装缓存为空就是已停
delivery: 新 turn inspect/wait 得到旧 child 报告，历史 terminal 容器正常接收
```

回归名和覆盖见下表。clock 通过 watchdog 已有 observation timestamp 注入；新增 gate barrier
只在 `cfg(test)` 可用，用 channel/barrier 不 sleep。子进程重启用旧进程/工具组 kernel identity
与可信 join+groups-gone，不能用 kill 后等待固定毫秒代替证明。
版本测试覆盖 new-client/old-daemon、old-client/new-daemon；拒绝后不自动杀/升级活 daemon，
不降 schema。升级丢凭证测试返回 SessionIdentityRequired，不获取旧 capability。
最后将三端 capability 检测切为强制新模式并广告；不足条件委派明确不可用、对话仍可继续。

- [x] **Step 4：完整验证与变异验证**：按下表运行每个 crate 的过滤测试，保存非零命中日志；
随后 `cargo fmt --all && just ci`。变异在本 worktree 临时应用并逐个撤销：
(a) 删 role 分流、(b) 恢复容器 ParentSettled、(c) 忽略 turn secret/epoch、(d) 容器 RecipientTerminal、
(e) watchdog commit 不复核 role。每次对应回归必须失败，撤销后绿；不提交变异。
`git diff --check`、确认没有新 secret/生产 DB 文件；更新模块 `[x]` 必须附实际测试命令与代码位置，
索引完成计数由实际复算得出。
- [ ] **Step 5：提交与合并**：fmt、just ci 全绿、请求代码审查且无阻塞后提交
`feat: enable verified session delegation lifecycle v1`。按 finishing-a-development-branch 流程
在 main `git merge --no-ff <implementation-branch>`，合并后再跑 `just ci`；通过才清理自己的
worktree/分支，不停止其他会话的编译或删除它们的 target。

**当前停点（2026-11-06）**：实现与全部门禁已完成，提交已落在实现分支上，但**按用户要求停在合并之前**，等用户确认后才执行 `git merge --no-ff`。因此本 Step 保持 `[ ]`，不代表实现未完成。

## C1–C17 证据映射（已实际运行）

每个表格测试名必须成为真实 `#[test]`/`#[tokio::test]` 函数，不能只出现在文档或注释。
`cargo test -p <crate> <name> -- --nocapture` 运行前先 `-- --list` 确认该名；零命中视为失败。
Task 11 负责补齐此前任务未直接给出完整代码的集成场景，使用真实 coordinator/socket/host，
不能用纯 DTO equality 冒充流程验收。

实测命令基线（均在 `yi-agent-rs/` 下，`export PATH` 略）：`cargo test -p <crate> <target> --offline <features> <name>`。
下表“实际位置”一列是运行后核对的结果，与早期计划草稿的 crate 归属不同处已按实测修正。

| 判据 | 任务 | 实际位置与运行结果 |
|---|---|---|
| C1 | 1–4、9 | core `--lib session_delegation_lifecycle_role_is_not_root_depth` PASS；**store**（非 app-server）`--test session_delegation_lifecycle --features foreground-test-support session_delegation_lifecycle_app_server_roles_for_normal_and_kanban` PASS。该用例直接建真实 daemon，覆盖普通项目目录与 `.worktrees/kanban/card-1` 均为 `SessionContainer`，但**不在** app-server crate 内 |
| C2 | 4、11 | store `--test runtime_coordinator --features foreground-test-support session_delegation_lifecycle_managed_and_embedded_survive_worker_deadlines` PASS |
| C3 | 3、4 | store `--test session_delegation_lifecycle ... session_delegation_lifecycle_worker_budget_and_scheduled_root_unchanged` PASS；core `--lib session_delegation_lifecycle_execution_stays_settled` PASS |
| C4 | 2、5、11 | store `--test session_delegation_lifecycle ... session_delegation_lifecycle_historical_root_rebind_preserves_evidence` PASS |
| C5 | 2、5 | store `--test session_delegation_lifecycle ... session_delegation_lifecycle_closed_and_revoked_sessions_stay_closed` PASS |
| C6 | 5、11 | store `--test session_delegation_lifecycle ... session_delegation_lifecycle_unproven_old_execution_blocks_rebind` PASS |
| C7 | 6–8 | store `--test subagent_runtime_e2e --features foreground-test-support session_delegation_lifecycle_no_legacy_write_bypass` PASS；subagent `--test session_delegation_client session_delegation_lifecycle_delayed_old_tool_cannot_borrow_new_turn` PASS |
| C8 | 3、6 | store `--test session_delegation_lifecycle ... session_delegation_lifecycle_execution_cannot_use_application_routes` PASS |
| C9 | 3、9 | app-server `--lib session_delegation_lifecycle_app_server_two_turns_keep_background_child` PASS |
| C10 | 3、11 | store `--test session_delegation_lifecycle ... session_delegation_lifecycle_terminal_history_container_receives_delivery` PASS |
| C11 | 5、7、9–11 | **store**（非 yi-agent）`--test session_delegation_lifecycle ... session_delegation_lifecycle_frontend_and_daemon_restart` PASS：真实 socket + 重启 daemon，断言复用同一 root、`binding_epoch` 前进、旧 turn secret 失效、容器不启动 worker |
| C12 | 5、7、11 | store `--lib --features foreground-test-support session_delegation_lifecycle_binding_cas_and_rollback` PASS；**store**（非 subagent）`--test session_delegation_lifecycle ... session_delegation_lifecycle_single_shot_lost_response` PASS：同 op 重试返回同一 epoch，`session_binding_operations` 仅 1 行 |
| C13 | 4、6、11 | store `--test session_delegation_lifecycle ... session_delegation_lifecycle_old_watchdog_and_close_spawn_races` PASS（barrier 1：turn 已发、close 后 spawn 被拒、child 数不增、worker 不启动、后续 prepare 返回 `SessionClosed`）；barrier 2 释放在 `--test runtime_coordinator foreground_root_lifecycle_release_*`；barrier 3 `session_delegation_lifecycle_a_failed_binding_commit_leaves_no_capability` PASS 且已做变异验证；barrier 4 `session_delegation_lifecycle_concurrent_prepare_commits_once` + C12 |
| C14 | 3、6、9、11 | store `--test session_delegation_lifecycle ... session_delegation_lifecycle_two_sessions_keep_permissions_and_quotas` PASS |
| C15 | 8–10 | yi-agent `--test session_delegation_lifecycle session_delegation_lifecycle_headless_entry_uses_container_context` PASS；yi-agent `--bin yi-agent session_delegation_lifecycle_tui_entry_refreshes_turn_not_worker_identity` PASS（yi-agent 无 lib target，必须用 `--bin`） |
| C16 | 5–7、11 | store `--test subagent_ipc_protocol --features foreground-test-support session_delegation_lifecycle_v2_envelope_cannot_issue_unfenced_write` PASS；store `--test managed_attach_authority --features foreground-test-support session_delegation_lifecycle_managed_denial_never_falls_back_to_embedded` PASS |
| C17 | 2、4 | store `--lib --features foreground-test-support session_delegation_lifecycle_migration_preserves_terminal_evidence` PASS；同 target `..._migration_rejects_corrupt_attachment` PASS |

补充回归（不在 C 表内但被本次改动直接覆盖）：

| 场景 | 位置与结果 |
|---|---|
| 关闭容器的 tombstone 跨重启恢复 | store `--test session_delegation_lifecycle ... session_delegation_lifecycle_closed_container_stays_closed_across_restart` PASS；变异“删掉 hydration 的 `hydrate_session_container_closed`”后该用例失败于 `a restarted daemon must not deliver into a closed mailbox, got MessageQueued`，恢复后绿 |
| spawn 预检识别 closed | store `spawn_child_with_objective` 的 role 判定旁新增 tombstone 读取，`ParentSettled { state: "session_closed" }`；C13 用例覆盖 |

### 本次补上的两处真实缺口

1. **准入层不看 closed**。`admit_session_write` 只按 role + turn 放行，`spawn_child_with_objective` 的预检也只看历史 state 与 role。于是“容器已关闭、turn 仍握在手里”的窗口能让 spawn 成功。修复：两处都在同一把 repository 锁内读取 `session_containers.closed`，容器关闭即拒绝（准入返回 `SessionClosed`，预检返回 `ParentSettled { state: "session_closed" }`）。
2. **hydration 不恢复 closed**。`ensure_application_root_supervisor` 只恢复 role，重启后 supervisor 的 `closed_containers` 为空，而 `can_accept_user_override` / `recipient_is_closed` 只读内存。修复：hydration 读取容器记录并调用新增的 `AgentSupervisor::hydrate_session_container_closed()`（只恢复 tombstone、不重跑取消，历史行逐字不变）。

### 与用户原始报错的关系

`a settled parent task (state: stalled) cannot spawn descendants` 来自 `SpawnError::ParentSettled`。根因是 `AgentTask.role` 在 hydration 时若没有恢复成 `SessionContainer`，历史 `stalled` 会被当成“父任务已结算”。`hydrate_session_role` 已在 Task 4 修复；本次补的两处是同一规则的另一半（显式关闭必须压过角色豁免），否则“重启后容器仍能工作”与“关闭后容器仍能工作”会互相矛盾。

## 执行自检与停点

- [x] Task 1–11 每项测试红→绿、提交、审查；接口改动同步所有消费者，不留下 compile-only stub。
- [x] 确认每个 C 编号有真实运行日志，尤其 embedded、TUI、headless，不用 managed 单例代替全覆盖。
- [x] 主会话审查当前 spec/plan；若旧会话没有可信凭证，报告身份阻塞而非声称自动修好。
- [ ] `just ci` 在实现分支与 main 合并结果各全绿一次；默认不运行真实 LLM。
- [ ] 只有完成实现与证据后才标记模块需求完成；本次文档提交保持 `[ ]`，计数仍为64/75。

本计划完成仅表示可以进入实施，不表示当前安装 App、daemon 或当前会话的 spawn 错误已修复。
