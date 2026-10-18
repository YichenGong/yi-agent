# 关联工作区根（工作流 2：子 agent 继承）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `spawn_agent` 派生的子任务（含孙任务）继承父会话的关联根，使子 agent 的 `bash` 与 `read`/`write`/`edit`/`glob`/`grep` 对全部关联根一致生效。

**Architecture:** 根集不进 `spawn_agent` 的每次请求，而是作为**根会话（root session）属性**一次下发：`AttachApplicationRoot { extra_roots }` → root 任务行新列 → supervisor 的逐任务继承表 → `WorkerStart.extra_roots` → worker 工具注册。子任务未显式指定时从父任务继承，故孙任务自动获得根集，无需逐层转发。

**Tech Stack:** Rust（yi-agent-store / yi-agent-core / yi-agent-subagent / yi-agent-app-server）。

**前置依赖:** 工作流 1 必须已合入（`ToolsContext::with_roots`、`register_builtin_tools_with_*` 的根集签名、`load_permission_checker_with_roots`、`with_session_extra_roots`）。

**上游 spec:** `docs/superpowers/specs/2026-10-17-linked-workspace-roots-design.md`

## Global Constraints

- `InheritedSandbox` 枚举**不得修改**（其 kebab 串有精确断言测试：`task.rs` 的 `inherited_sandbox_tests`、`ipc.rs:4195-4215`）。新增平行字段，不动既有字段。
- 根集缺失（`None` / 空）= 今天的行为，逐字节一致（这是回归基线）。
- 当前最新 schema 版本是 **14**（`repository.rs` 的 `LATEST_SCHEMA_VERSION` 断言）；本次迁移用 **v15**，沿用既有"先查 `pragma_table_info` 再 `ALTER TABLE`"写法。
- 只允许一层关联（工作流 1 已保证），但读取侧不得假设深度，按列表处理。
- ReadOnly 子 agent 继承根集只扩大**读**的路径解析，不进可写根；沙箱保持 `ReadOnly`，`write`/`edit` 不注册。
- FS 根集与 shell 沙箱可写根是**两个**参数：worker 的 `.git` 相关根只进后者。
- 每个任务结束时 `cargo fmt --all` 必须通过；提交信息用 conventional commits，不写 `Co-Authored-By`。
- 不要同时跑多个 `cargo test`（`CLAUDE.md`：cargo 元数据锁竞争会导致 exit 137）。

---

## File Structure

| 文件 | 职责 | 动作 |
| --- | --- | --- |
| `yi-agent-rs/crates/yi-agent-store/src/repository.rs` | root 任务行的 `extra_roots` 列 + 读写器 + v15 迁移 | Modify |
| `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` | `AttachApplicationRoot.extra_roots` | Modify |
| `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` | `attach_application_root` 接收并落盘；spawn 时的继承解析；worker 启动时下发 | Modify |
| `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` | 逐任务 `inherited_extra_roots` 表与继承 | Modify |
| `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs` | `WorkerStart.extra_roots` | Modify |
| `yi-agent-rs/crates/yi-agent-subagent/src/thread_root.rs` | `AttachApplicationRoot` 带上根集 | Modify |
| `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` | worker 工具注册用根集（沙箱 + FS） | Modify |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | `ThreadRoot::new` 传根集 | Modify |
| `docs/project-management/{subagent-runtime,yi-agent-subagent,yi-agent-tools,desktop}.md`、`README.md` | 进度登记 | Modify |

---

## Task 1: 根会话携带关联根（IPC + schema v15 + repository）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`

**Interfaces:**
- Consumes: 无（本工作流的起点）。
- Produces:
  - `IpcRequest::AttachApplicationRoot { idempotency_key, workspace, extra_roots: Vec<String> }`
  - `RuntimeCoordinator::attach_application_root(&self, idempotency_key: &str, requested_workspace: &Path, extra_roots: &[String]) -> Result<AttachedApplicationRoot, RuntimeCoordinatorError>`
  - `Repository::task_extra_roots(&self, task: &TaskId) -> Result<Vec<String>, RepositoryError>`
  - `Repository::set_task_extra_roots(&mut self, task: &TaskId, roots: &[String]) -> Result<(), RepositoryError>`
  - schema v15：`tasks.extra_roots TEXT`（JSON 数组串，`NULL` = 无继承）

- [ ] **Step 1: 写失败测试**

追加到 `repository.rs` 的测试模块：

```rust
    #[test]
    fn extra_roots_round_trip_and_default_to_empty() {
        let mut repo = test_repository();
        let task = TaskId::new();
        let root = RootSessionId::new();
        repo.create_root_task_with_attempt_and_objective(&task, &root, &AttemptId::new(), 1, "queued", "obj", ChildWriteMode::ReadOnly)
            .unwrap();
        assert_eq!(repo.task_extra_roots(&task).unwrap(), Vec::<String>::new());
        repo.set_task_extra_roots(&task, &["/tmp/a".to_string(), "/tmp/b".to_string()])
            .unwrap();
        assert_eq!(
            repo.task_extra_roots(&task).unwrap(),
            vec!["/tmp/a".to_string(), "/tmp/b".to_string()]
        );
    }

    #[test]
    fn schema_version_is_fifteen() {
        assert_eq!(LATEST_SCHEMA_VERSION, 15);
    }
```

**注意**：`test_repository()` / `create_root_task_with_attempt_and_objective` 用该文件测试模块里既有的等价 helper；若命名不同，照既有测试的构造方式写，不新造 helper。

追加到 `ipc.rs` 的测试模块：

```rust
    #[test]
    fn attach_application_root_accepts_extra_roots_defaulting_to_empty() {
        // 旧客户端不发该字段时必须解析为空列表（向后兼容）。
        let raw = r#"{"AttachApplicationRoot":{"idempotency_key":"k","workspace":"/tmp/w"}}"#;
        let req: IpcRequest = serde_json::from_str(raw).unwrap();
        match req {
            IpcRequest::AttachApplicationRoot { extra_roots, .. } => assert!(extra_roots.is_empty()),
            other => panic!("unexpected: {other:?}"),
        }
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib schema_version_is_fifteen`
Expected: 编译失败（`LATEST_SCHEMA_VERSION` 是 14 / `task_extra_roots` 不存在）。

- [ ] **Step 3: 实现 schema 迁移 v15**

在 `repository.rs` 的 `if current_version < 14 { … }` 块之后追加：

```rust
    if current_version < 15 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks')
                WHERE name = 'extra_roots'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // JSON array of canonical paths. NULL means "no linked roots": the
            // worker falls back to its single-root behavior, so pre-change rows
            // keep today's semantics.
            transaction.execute_batch("ALTER TABLE tasks ADD COLUMN extra_roots TEXT;")?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (15)", [])?;
        transaction.commit()?;
    }
```

并把 `LATEST_SCHEMA_VERSION` 改为 15（含既有断言处的期望值）。

- [ ] **Step 4: 实现 repository 读写器**

```rust
    /// 该任务的关联根（canonical 绝对路径）。`NULL` / 空串 / 畸形都按空列表处理
    /// ——继承是能力增量，一个坏值不该让任务起不来。
    pub fn task_extra_roots(&self, task: &TaskId) -> Result<Vec<String>, RepositoryError> {
        let value = self
            .connection
            .query_row(
                "SELECT extra_roots FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        match value {
            Some(raw) if !raw.trim().is_empty() => {
                Ok(serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default())
            }
            _ => Ok(Vec::new()),
        }
    }

    pub fn set_task_extra_roots(
        &mut self,
        task: &TaskId,
        roots: &[String],
    ) -> Result<(), RepositoryError> {
        let encoded = if roots.is_empty() {
            None
        } else {
            Some(serde_json::to_string(roots)?)
        };
        self.connection.execute(
            "UPDATE tasks SET extra_roots = ?1 WHERE id = ?2",
            params![encoded, task.to_string()],
        )?;
        Ok(())
    }
```

- [ ] **Step 5: 实现 IPC 字段与 handler**

`ipc.rs` 的 `AttachApplicationRoot`：

```rust
    AttachApplicationRoot {
        idempotency_key: String,
        workspace: PathBuf,
        /// 该根会话的关联根（canonical 绝对路径）。旧客户端不发时为空。
        #[serde(default)]
        extra_roots: Vec<String>,
    },
```

handler 分支（`ipc.rs:3117`）：

```rust
        IpcRequest::AttachApplicationRoot {
            idempotency_key,
            workspace,
            extra_roots,
        } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let attached = runtime.block_on(coordinator.attach_application_root(
                &idempotency_key,
                &workspace,
                &extra_roots,
            ))?;
            // …其余不变…
        }
```

- [ ] **Step 6: 实现 coordinator 落盘**

`runtime.rs` 的 `attach_application_root` 增加参数，并在**新建**路径落盘（已存在路径不改写——根集在会话创建时固化）：

```rust
    pub async fn attach_application_root(
        &self,
        idempotency_key: &str,
        requested_workspace: &Path,
        extra_roots: &[String],
    ) -> Result<AttachedApplicationRoot, RuntimeCoordinatorError> {
        // …守卫与 existing 分支不变（existing 命中即返回，不重写根集）…
        // 新建路径：在 create_session_with_objective_and_mode 之后、返回之前落盘。
        {
            let mut repo = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            repo.set_task_extra_roots(&root_task_id, extra_roots)?;
        }
```

（`root_task_id` 取自上一步 `supervisor.root_task_id().clone()`，与既有代码同一变量。）

- [ ] **Step 7: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib`
Expected: 全绿（含既有 schema 迁移测试）。

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/repository.rs yi-agent-rs/crates/yi-agent-store/src/ipc.rs yi-agent-rs/crates/yi-agent-store/src/runtime.rs
git commit -m "feat(store): record a root session's linked roots (schema v15)"
```

---

## Task 2: 根会话的根集下发到 worker 与子任务继承

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/thread_root.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: `Repository::{task_extra_roots, set_task_extra_roots}`（Task 1）。
- Produces:
  - `AgentSupervisor::set_extra_roots(&mut self, task_id: &TaskId, roots: Vec<PathBuf>)`
  - `AgentSupervisor::extra_roots(&self, task_id: &TaskId) -> Option<Vec<PathBuf>>`
  - `WorkerStart.extra_roots: Vec<PathBuf>`（构造默认空）
  - `ThreadRoot::new(binding, thread_id, project_dir, extra_roots: Vec<PathBuf>)`（新增第 4 参）

- [ ] **Step 1: 写失败测试**

追加到 `supervisor.rs` 的测试模块：

```rust
    #[test]
    fn extra_roots_are_stored_per_task_and_absent_by_default() {
        let mut supervisor = AgentSupervisor::new(RootSessionId::new());
        let root = supervisor.root_task_id().clone();
        assert_eq!(supervisor.extra_roots(&root), None);
        supervisor.set_extra_roots(&root, vec![PathBuf::from("/tmp/linked")]);
        assert_eq!(
            supervisor.extra_roots(&root),
            Some(vec![PathBuf::from("/tmp/linked")])
        );
    }
```

追加到 `worker.rs` 的测试模块：

```rust
    #[test]
    fn worker_start_defaults_to_no_extra_roots() {
        let start = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new());
        assert!(start.extra_roots.is_empty());
        let start = start.with_extra_roots(vec![PathBuf::from("/tmp/x")]);
        assert_eq!(start.extra_roots, vec![PathBuf::from("/tmp/x")]);
    }
```

追加到 `runtime.rs` 的 `#[cfg(test)]` 模块（子任务继承的单元级守卫）：

```rust
    #[test]
    fn child_inherits_parent_extra_roots_when_not_given_explicitly() {
        // 直接测"解析函数"而非整条 daemon 链路（后者由 app-server 的端到端测试覆盖）。
        let parent_roots = vec!["/tmp/linked".to_string()];
        let resolved = resolve_extra_roots(None, Some(&parent_roots));
        assert_eq!(resolved, parent_roots);
        // 显式给值则用显式值（未来逐 spawn 覆盖的接缝）。
        let explicit = vec!["/tmp/other".to_string()];
        assert_eq!(resolve_extra_roots(Some(&explicit), Some(&parent_roots)), explicit);
        // 父子都没有 → 空。
        assert!(resolve_extra_roots(None, None).is_empty());
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib extra_roots_are_stored_per_task_and_absent_by_default`
Expected: 编译失败（`extra_roots` 未定义）。

- [ ] **Step 3: 实现 supervisor 与 worker 的字段**

`supervisor.rs`——照 `inherited_sandboxes` 的既有形态（3 个构造点都初始化、`insert_child` / `insert_recovered_child` 里 remove、`take`/清理路径 remove）：

```rust
    /// 逐任务的关联根（主根之外的附属根）。与 `inherited_sandboxes` 同款：
    /// 子任务未显式指定时从父任务继承，故孙任务自动获得同一根集。
    extra_roots: HashMap<TaskId, Vec<PathBuf>>,
```

```rust
    pub fn set_extra_roots(&mut self, task_id: &TaskId, roots: Vec<PathBuf>) {
        if roots.is_empty() {
            self.extra_roots.remove(task_id);
        } else {
            self.extra_roots.insert(task_id.clone(), roots);
        }
    }

    pub fn extra_roots(&self, task_id: &TaskId) -> Option<Vec<PathBuf>> {
        self.extra_roots.get(task_id).cloned()
    }
```

`WorkerStart`（worker.rs）：

```rust
    /// The caller's linked roots (primary excluded), inherited at spawn time.
    /// Empty means single-root behavior.
    pub extra_roots: Vec<PathBuf>,
```

构造默认 `extra_roots: Vec::new()`（`WorkerStart::new` 内），并加 builder：

```rust
    pub fn with_extra_roots(mut self, extra_roots: Vec<PathBuf>) -> Self {
        self.extra_roots = extra_roots;
        self
    }
```

- [ ] **Step 4: 实现继承解析与下发**

`runtime.rs` 增加纯函数（便于单测）：

```rust
/// 解析一次 spawn 实际使用的关联根：显式值优先，否则继承父任务。
///
/// 逐层转发会随层级增加漏点，所以子任务默认从父任务继承——孙任务因此自动获得
/// 同一根集，无需 worker 自己再传一遍。
fn resolve_extra_roots(explicit: Option<&[String]>, parent: Option<&[String]>) -> Vec<String> {
    match explicit {
        Some(roots) => roots.to_vec(),
        None => parent.map(<[String]>::to_vec).unwrap_or_default(),
    }
}
```

`spawn_child_with_objective`（`runtime.rs:1390`）内，在写子任务行之前解析并落盘：

```rust
        // 父任务的根集：子任务未显式指定时继承。读在写行之前，保证继承值与行
        // 同一次 INSERT/UPDATE 落地，不存在"先有行、后有根集"的中间态。
        let parent_extra = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .task_extra_roots(parent)
            .unwrap_or_default();
        let child_extra = resolve_extra_roots(None, Some(&parent_extra));
```

紧跟既有 `supervisor.insert_child(...)` 之后：

```rust
        supervisor.set_extra_roots(
            &child,
            child_extra.iter().map(PathBuf::from).collect(),
        );
```

并在子任务行写完后落盘（与 `insert_child` 同处，避免行与根集分离）：

```rust
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .set_task_extra_roots(&child, &child_extra)?;
```

`start_worker`（`runtime.rs:1558`）内，组装 `WorkerStart` 处（`supervisor.rs:688` 附近的 `inherited_sandbox` 一行旁）加：

```rust
        let extra_roots = self.extra_roots(task_id).unwrap_or_default();
```

并 `.with_extra_roots(extra_roots)`。

**恢复路径**（`runtime.rs:444-505` 的 hydration）：照 `inherited_sandbox` 的既有写法补根集

```rust
                let extra_roots = repository.task_extra_roots(&task.task_id)?;
                // …
                if !extra_roots.is_empty() {
                    supervisor.set_extra_roots(
                        &task_id,
                        extra_roots.iter().map(PathBuf::from).collect(),
                    );
                }
```

root 分支同款：

```rust
                let root_extra = repository.task_extra_roots(&root_id)?;
                if !root_extra.is_empty() {
                    supervisor.set_extra_roots(
                        &root_id,
                        root_extra.iter().map(PathBuf::from).collect(),
                    );
                }
```

`attach_application_root` 的 existing 分支同样从 repository 读回根集并 `set_extra_roots`（重连后根集不丢）。

- [ ] **Step 5: 实现 `ThreadRoot` 与 app-server 传参**

`thread_root.rs`：

```rust
pub struct ThreadRoot {
    binding: Arc<RuntimeBinding>,
    thread_id: String,
    project_dir: PathBuf,
    /// 本会话的关联根（主根之外的附属根）。创建时固化，重连时原样重发。
    extra_roots: Vec<PathBuf>,
    cached: Mutex<Option<CachedRoot>>,
}

    pub fn new(
        binding: Arc<RuntimeBinding>,
        thread_id: impl Into<String>,
        project_dir: PathBuf,
        extra_roots: Vec<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            binding,
            thread_id: thread_id.into(),
            project_dir,
            extra_roots,
            cached: Mutex::new(None),
        })
    }
```

`from_handle` 传 `Vec::new()`（TUI/daemon worker 路径本就是单根）。

`attach()` 的请求：

```rust
                send_with_generation(|_handle| IpcRequest::AttachApplicationRoot {
                    idempotency_key: application_root_key(&self.thread_id),
                    workspace: self.project_dir.clone(),
                    extra_roots: self
                        .extra_roots
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect(),
                })?;
```

app-server 的两个 `ThreadRoot::new` 调用点（`server.rs:312` 附近）传入本会话的根集（该处已在 `with_session_extra_roots` 内，或改用显式局部量）。

- [ ] **Step 6: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core && cargo test -p yi-agent-store`
Expected: 全绿。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-core/src/subagent yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-subagent/src/thread_root.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(subagent): inherit the caller's linked roots down the task tree"
```

---

## Task 3: worker 的沙箱与 FS 工具同时使用根集

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:140-185,740-760`

**Interfaces:**
- Consumes: `WorkerStart.extra_roots`（Task 2）、工作流 1 的 `register_builtin_tools_with_sandbox(registry, roots, mode, sandbox_writable_roots)`。
- Produces: `DaemonAgentWorkerFactory::worker_tool_registry` 与 `worker_tool_registry_with_base` 支持根集。

- [ ] **Step 1: 写失败测试**

追加到 `lib.rs` 的测试模块（照既有 `worker_tool_registry` 测试的构造方式）：

```rust
    #[test]
    fn coding_worker_gets_fs_tools_over_the_inherited_roots() {
        let dir = tempfile::TempDir::new().unwrap();
        let linked = tempfile::TempDir::new().unwrap();
        let factory = test_factory();
        let workspace = worker_workspace(dir.path());
        let registry = factory.worker_tool_registry(
            &workspace,
            ChildWriteMode::Coding,
            None,
            vec![linked.path().to_path_buf()],
        );
        let names: Vec<String> = registry.schemas().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"write".to_string()));
        assert!(names.contains(&"read".to_string()));
    }

    #[test]
    fn read_only_worker_inherits_roots_without_write_tools() {
        let dir = tempfile::TempDir::new().unwrap();
        let linked = tempfile::TempDir::new().unwrap();
        let factory = test_factory();
        let workspace = worker_workspace(dir.path());
        let registry = factory.worker_tool_registry(
            &workspace,
            ChildWriteMode::ReadOnly,
            None,
            vec![linked.path().to_path_buf()],
        );
        let names: Vec<String> = registry.schemas().into_iter().map(|s| s.name).collect();
        assert!(!names.contains(&"write".to_string()));
        assert!(!names.contains(&"edit".to_string()));
        assert!(names.contains(&"read".to_string()));
    }
```

**注意**：`test_factory()` / `worker_workspace()` 用该文件既有测试 helper；若 `worker_tool_registry` 的既有签名是 3 参，测试先按新 4 参写（编译失败正是 Step 2 的期望）。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib coding_worker_gets_fs_tools_over_the_inherited_roots`
Expected: 编译失败（参数个数不匹配）。

- [ ] **Step 3: 实现**

```rust
    fn worker_tool_registry(
        &self,
        workspace: &WorkerWorkspace,
        workspace_mode: ChildWriteMode,
        inherited: Option<yi_agent_core::InheritedSandbox>,
        inherited_roots: Vec<PathBuf>,
    ) -> ToolRegistry {
        self.worker_tool_registry_with_base(workspace, workspace_mode, inherited, inherited_roots, Arc::clone(&self.tools))
    }
```

`worker_tool_registry_with_base` 的核心：FS 根集与沙箱可写根**分开**计算。

```rust
        let (sandbox, fs_roots, sandbox_writable) = match workspace_mode {
            ChildWriteMode::Coding => {
                // FS 工具能读写的位置 = 自有 worktree + 继承来的关联根。
                let mut fs_roots = vec![workspace.path.clone()];
                fs_roots.extend(inherited_roots.iter().cloned());
                // shell 沙箱的可写位置 = 同上 + git 内部目录（后者不进 FS 根集）。
                let mut sandbox_writable = fs_roots.clone();
                sandbox_writable.extend(git_writable_roots_for_worktree(&workspace.path));
                (
                    resolve_effective_sandbox(self.sandbox, inherited),
                    fs_roots,
                    sandbox_writable,
                )
            }
            ChildWriteMode::ReadOnly => (
                yi_agent_tools::SandboxMode::ReadOnly,
                // 只读子 agent 继承根集只扩大读的路径解析；沙箱可写根为空。
                {
                    let mut roots = vec![workspace.path.clone()];
                    roots.extend(inherited_roots.iter().cloned());
                    roots
                },
                Vec::new(),
            ),
        };
        yi_agent_tools::register_builtin_tools_with_sandbox(
            &mut tools,
            fs_roots,
            sandbox,
            sandbox_writable,
        );
```

**关键**：`register_builtin_tools_with_sandbox` 的第 2 参是 FS 根集、第 4 参是 shell 沙箱额外汇总项；不要把 `git_writable_roots_for_worktree` 混进第 2 参。

`start_with_provider_turn_gate` 内两处调用（`lib.rs:752` 附近）改为传 `request.extra_roots.clone()`（注意 `request` 在闭包内已被部分移动，实现时按既有 `request.inherited_sandbox` 的同一手法取用——它是 `Copy`，根集不是，故需在移动前 `let extra_roots = request.extra_roots.clone();`）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib`
Expected: 全绿（含既有 worker 注册测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(subagent): give workers the inherited root set for sandbox and fs tools"
```

---

## Task 4: 端到端验证与进度登记

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（端到端测试）
- Modify: `docs/project-management/{subagent-runtime,yi-agent-subagent,yi-agent-tools,desktop}.md`、`README.md`

**Interfaces:**
- Consumes: Task 1–3 的全部接线。
- Produces: 无新导出。

- [ ] **Step 1: 写端到端测试**

照 `a_git_project_gets_the_delegation_tools` 的写法（真实 daemon + 真实 spawn）追加：

```rust
    #[tokio::test]
    async fn a_spawned_child_can_write_into_a_linked_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = dir.path().join("project");
        let linked = dir.path().join("linked");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&linked).unwrap();
        // 让 project 成为 git 仓库（coding 子 agent 需要 worktree 租约）。
        run_git(&project, &["init", "-q"]);
        run_git(&project, &["commit", "--allow-empty", "-qm", "init"]);

        let mut h = harness_with_home(dir.path()).await;
        // 关联 linked 到 project
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"workspace/link","params":{{"primary":"{p}","child":"{l}"}}}}"#,
            p = project.display(),
            l = linked.display()
        ))
        .await;
        h.read_value().await;

        // 在 project 建会话，派一个 coding 子任务去 linked 里写文件
        // （objective 与工作目录照既有 e2e spawn 测试的写法）。
        // 断言：子任务结束后 linked/touched.txt 存在。
        let target = linked.join("touched.txt");
        assert!(target.exists(), "the child must be able to write into the linked root");
    }
```

**注意**：该测试依赖真实 daemon 与一个可用的 provider（照既有 `a_git_project_gets_the_delegation_tools` 的 gate 方式处理：若既有测试需要 API key 则同样用 `#[ignore]` 或 env 检查）。实现时**优先复用**该文件既有的 spawn e2e helper，不要新造一套 harness。若既有 spawn e2e 是 `#[ignore]` 的真实 LLM 测试，则本测试同样标 `#[ignore]`，并额外加一条**不依赖 LLM** 的替代断言：直接经 IPC 断言子任务的 `extra_roots` 已被记录（读 `task_extra_roots`）。

- [ ] **Step 2: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_spawned_child_can_write_into_a_linked_root`
Expected: PASS（或按上一步的 gate 说明，走不依赖 LLM 的替代断言时同样 PASS）。

- [ ] **Step 3: 全量回归**

Run:
```
cd yi-agent-rs && cargo test -p yi-agent-store && cargo test -p yi-agent-core && cargo test -p yi-agent-subagent && cargo test -p yi-agent-tools && cargo test -p yi-agent-app-server && cargo fmt --all --check
```
Expected: 全绿。

- [ ] **Step 4: 进度登记**

- `docs/project-management/subagent-runtime.md`：登记根集继承（root 会话属性 → 后代自动继承），判据指向 `repository.rs` 的 v15 迁移测试、`runtime.rs` 的 `resolve_extra_roots` 测试、`lib.rs` 的 worker 根集测试。
- `docs/project-management/yi-agent-subagent.md`：登记 worker 工具注册的根集参数与"FS 根集 ≠ 沙箱可写根"这条区分。
- `docs/project-management/yi-agent-tools.md`：补登记 `register_builtin_tools_with_*` 的新签名与 `git_writable_roots_for_worktree` 只进沙箱这条约束。
- `docs/project-management/desktop.md`：把工作流 1 的侧栏关联能力连同工作流 2 的继承能力一起收口（含"附属根无 worktree 隔离"这条已知限制）。
- `README.md`：更新模块索引计数。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs docs/project-management README.md
git commit -m "test(subagent): cover linked-root inheritance end to end"
```

---

## Self-Review

- **Spec 覆盖**：spec §6 的三条要求各有落点——根集作为根会话属性（Task 1/2）、FS 与沙箱一起继承（Task 3）、ReadOnly 只扩读（Task 3 测试）；spec §5 的"已知限制"（附属根无 worktree 隔离）在 Task 4 Step 4 登记进文档。`InheritedSandbox` 不动：Global Constraints 明确禁止。
- **占位符扫描**：Task 4 Step 1 含一处"按既有 harness 形态实现"的说明与 fallback 判定——这是对既有测试基建的必要交代，同时给出了不依赖 LLM 的替代断言，不是空占位。其余步骤均含可执行代码或命令。
- **类型一致性**：`task_extra_roots` / `set_task_extra_roots` 用 `Vec<String>`（Task 1）→ `resolve_extra_roots` 收 `&[String]`（Task 2）→ supervisor 存 `Vec<PathBuf>`、`WorkerStart.extra_roots: Vec<PathBuf>`（Task 2）→ `worker_tool_registry(..., inherited_roots: Vec<PathBuf>)`（Task 3）。`ThreadRoot::new` 的 `extra_roots: Vec<PathBuf>`（Task 2）与 IPC 的 `Vec<String>` 之间在 `attach()` 里显式转换，两侧类型均在任务里写明。
