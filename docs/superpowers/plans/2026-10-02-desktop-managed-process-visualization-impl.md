# 桌面端受管后台进程可视化 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 desktop（Tauri GUI + `yi-agent app-server` sidecar）能看见、追尾、终止 agent 通过 `process_start` 起的后台进程。

**Architecture:** 后端把 `ProcessManager`（现已创建即丢弃）随其工具注册表一起带出，新增 `process/list`、`process/read`、`process/kill` 三个 RPC 与一条 `process/updated` 通知；前端在右侧区域新增「后台进程」tab（与「子 agent」并列），列表走状态推送、输出走按需增量拉。

**Tech Stack:** Rust（`yi-agent-runtime` / `yi-agent-app-server` / `yi-agent-tools`）、tokio、serde、JSON-RPC 2.0；前端 React 19 + TypeScript + Tailwind v4 + Vitest + Testing Library。

**设计文档：** `docs/superpowers/specs/2026-10-02-desktop-managed-process-visualization-design.md`

## Global Constraints

- **协议字段命名**：`process/*` 沿用既有 snake_case 信封字段（`thread_id`、`process_id`、`next_cursor`），与 `thread/*`、`turn/*` 一致；**不要**用 camelCase（camelCase 只属于 `agent/*` 命名空间）。
- **协议类型复用**：快照与状态沿用 `yi_agent_tools` 的 `ManagedProcessSnapshot` / `ProcessStatus`（均已 `Serialize`，`ProcessStatus` 带 `#[serde(tag = "state", rename_all = "snake_case")]`）。后端不新造进程类型。
- **作用域**：所有 `process/*` 方法都按 thread 作用域；`process/list` 对未知 thread 返回**空列表**而非错误。
- **不转发 `Output` 事件**：`process/updated` 只由 `Started` / `Ready` / `Exited` / `Killed` 触发。
- **UI 文案（中文）**：tab 标签 `子 agent` / `后台进程`；空态 `暂无后台进程`；终态分组 `已结束`；kill 确认按钮 `终止进程` / `取消`；截断提示 `较早输出已滚出`；滚尾恢复提示 `已暂停追尾`。
- **状态配色语义**：starting/running = 琥珀（`text-amber-400`）、ready = 绿（`text-emerald-400`）、exited = 灰（`text-neutral-400`）、killed/failed = 红（`text-red-400`）。
- **测试要求**：后端每任务跑 `cd yi-agent-rs && cargo test -p yi-agent-runtime` / `-p yi-agent-app-server`；前端跑 `cd desktop && npx vitest run <file>`。TDD：先写失败测试，再实现。
- **不改**：TUI 路径、`ProcessManager` 内部实现（只读取其公开 API）、`SubagentRail` / `SubagentTrace` 的内部实现。

---

## 文件结构

**后端（`yi-agent-rs/`）**

| 文件 | 职责 | 动作 |
|------|------|------|
| `crates/yi-agent-runtime/src/bootstrap.rs` | `ToolSetup` / `AgentBootstrap` 带出 `process_manager` | 修改 |
| `crates/yi-agent-app-server/src/server.rs` | `BuiltAgent` / `RuntimeTooling` 带出 manager；`ThreadSession` 存 manager；三个 `process/*` 处理分支；`process/updated` 推送 | 修改 |
| `crates/yi-agent-app-server/src/session.rs` | `ThreadSession` 加 `process_manager` 字段 | 修改 |
| `crates/yi-agent-app-server/src/protocol.rs` | `Notification::ProcessUpdated` | 修改 |

**前端（`desktop/src/`）**

| 文件 | 职责 | 动作 |
|------|------|------|
| `lib/protocol.ts` | 进程线协议类型 | 修改 |
| `lib/processes.ts` | 进程快照折叠（活跃/终态分组、状态配色、显示名） | 新建 |
| `components/ProcessRail.tsx` | 进程列表 + 空态 + 终态分组 | 新建 |
| `components/ProcessDetail.tsx` | 详情：两栏输出 + 追尾 + kill 确认 | 新建 |
| `components/RightRail.tsx` | tab 容器 | 新建 |
| `components/RightRailCollapsedStrip.tsx` | 收起态细条（带计数，可重开） | 新建 |
| `App.tsx` | 接线：拉列表、路由通知、tab 态、详情态、收起态 | 修改 |

---

## Task 1: `ToolSetup` 带出 process manager

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:100-108`（结构体）、`:192-197`（naked 分支）、`:215-236`（注册与返回）

**Interfaces:**
- Consumes: `yi_agent_tools::ProcessManager`（已存在，`ProcessManager::with_controller(root, controller, writable_roots) -> Arc<Self>`）
- Produces: `ToolSetup.process_manager: Arc<yi_agent_tools::ProcessManager>`；`ToolSetup` 仍含 `tools` / `catalog` / `system_prompt` / `mcp`

- [ ] **Step 1: 写失败测试**

在 `crates/yi-agent-runtime/src/bootstrap.rs` 的 `#[cfg(test)] mod tests` 内，紧邻既有 `build_tools_registers_fs_and_shell` 添加：

```rust
    #[test]
    fn tool_setup_exposes_the_process_manager_it_registered() {
        let mut cfg = sample_config();
        // workdir 指向临时目录:测试不许往仓库根写进程运行时目录。
        cfg.workdir = tempfile::TempDir::new().unwrap().path().to_path_buf();
        let setup = build_tool_setup(&cfg, false).expect("build setup");
        // manager 必须与注册表同行:注册表里有 process_start,就必须有对应的
        // manager 可查——否则 app-server 拿不到句柄,进程面板会是空的。
        assert!(setup.tools.get("process_start").is_some());
        // 起一个真实后台进程,断言它出现在同一份 manager 的 list 里。
        let rt = tokio::runtime::Runtime::new().unwrap();
        let started = rt
            .block_on(setup.process_manager.start(yi_agent_tools::ProcessStartOptions {
                command: "sleep 30".into(),
                name: Some("t1-probe".into()),
                cwd: None,
                env: Default::default(),
                on_exit: Default::default(),
                ready_pattern: None,
                ready_timeout_sec: None,
            }))
            .expect("start");
        assert_eq!(started.name.as_deref(), Some("t1-probe"));
        assert!(setup
            .process_manager
            .list()
            .iter()
            .any(|p| p.name.as_deref() == Some("t1-probe")));
        // 收尾:on_exit 默认 Kill,shutdown 会杀掉它(不留孤儿 sleep 30)。
        // 必须在同一个 runtime 上 block_on —— 进程的 reader/waiter task 挂在
        // 那个 runtime 上,换一个 runtime 收尾是无效的。
        let _ = rt.block_on(setup.process_manager.shutdown());
    }
```

> `on_exit: Default::default()` 即 `OnExitPolicy::Kill`，因此上面的 `shutdown()` 会真正终止 `sleep 30`。若省略收尾，测试会留下一个存活 30 秒的孤儿进程。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime tool_setup_exposes_the_process_manager_it_registered`
Expected: 编译失败 — `no field 'process_manager' on type 'ToolSetup'`

- [ ] **Step 3: 实现**

`ToolSetup` 结构体加字段（`bootstrap.rs:100-108`）：

```rust
pub struct ToolSetup {
    pub tools: Arc<yi_agent_core::ToolRegistry>,
    pub catalog: Option<SkillsCatalogHandle>,
    pub system_prompt: Option<String>,
    /// MCP 管理器;配置缺失/不可用或注册失败时为 `None`。返回它以便**负责生命
    /// 周期的调用方**(headless / TUI)在退出前应调用 `shutdown()` 关闭 stdio 子
    /// 进程;不负责生命周期的调用方可以丢弃它。
    pub mcp: Option<Arc<yi_agent_mcp::McpManager>>,
    /// 支撑 `process_start` / `process_list` / `process_read` / `process_kill`
    /// 四个工具的进程管理器。**必须与 `tools` 里的进程工具同行**：调用方要按
    /// 「哪个注册表生效」选中对应的 manager，否则会查到一个空列表而工具明明可用。
    pub process_manager: Arc<yi_agent_tools::ProcessManager>,
}
```

naked 分支（`bootstrap.rs:192-197`）加一个空 manager——naked 的调用方只读 `tools`（空），不会用它：

```rust
    if naked {
        // 空 root 的 manager:与空 registry 配套,保证结构体总是自洽。
        // naked 调用方只用 `tools`(空),不会起进程。
        let process_manager = yi_agent_tools::ProcessManager::new(std::env::temp_dir());
        return Ok(ToolSetup {
            tools: Arc::new(yi_agent_core::ToolRegistry::new()),
            catalog: None,
            system_prompt: None,
            mcp: None,
            process_manager,
        });
    }
```

注册点（`:215-220`）改为 clone 一份进 setup（不再只 move 进注册表）：

```rust
    let process_manager = yi_agent_tools::ProcessManager::with_controller(
        workspace.to_path_buf(),
        controller,
        cfg.sandbox_writable_roots.clone(),
    );
    yi_agent_tools::register_process_tools(&mut registry, Arc::clone(&process_manager));
```

返回处（`:236`）带上：

```rust
    Ok(ToolSetup {
        tools: Arc::new(registry),
        catalog: prompt.catalog,
        system_prompt: Some(prompt.system_prompt),
        mcp,
        process_manager,
    })
```

> 若 `Ok(ToolSetup { .. })` 的字段名与当前文件不完全一致（例如 `prompt.catalog` 的实际字段），以文件中的现有写法为准，只**新增** `process_manager` 一行。

> **关于测试时「Config」来源**：`sample_config()` 是 `bootstrap.rs` 既有测试模块里的 helper，
> 直接用即可。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime`
Expected: PASS（含新测试与全部既有测试）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && git add crates/yi-agent-runtime/src/bootstrap.rs
git commit -m "feat(runtime): ToolSetup 带出 process manager"
```

---

## Task 2: `AgentBootstrap` 带出 process manager

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:259-280`（结构体）、`:325-355`（两个分支）

**Interfaces:**
- Consumes: Task 1 的 `ToolSetup.process_manager`
- Produces: `AgentBootstrap.process_manager: Arc<yi_agent_tools::ProcessManager>`

- [ ] **Step 1: 写失败测试**

在 `bootstrap.rs` 的 tests 模块追加：

```rust
    #[test]
    fn agent_bootstrap_exposes_the_process_manager() {
        let mut cfg = sample_config();
        cfg.workdir = tempfile::TempDir::new().unwrap().path().to_path_buf();
        let boot = bootstrap_agent(&cfg, PermissionMode::AutoAllow).expect("bootstrap");
        // 与 AgentBootstrap.tools 同理:调用方重建 agent 时要能沿用同一份
        // manager,否则进程列表会跟实际生效的工具集脱钩。
        assert!(boot.tools.get("process_start").is_some());
        assert_eq!(boot.process_manager.list().len(), 0);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime agent_bootstrap_exposes_the_process_manager`
Expected: 编译失败 — `no field 'process_manager' on type 'AgentBootstrap'`

- [ ] **Step 3: 实现**

`AgentBootstrap`（`:259` 起）在 `pub tools` 之后加字段：

```rust
    /// 本次装配的进程管理器;与 `tools` 里的进程工具是同一份。
    pub process_manager: Arc<yi_agent_tools::ProcessManager>,
```

在 `bootstrap_agent` 内先取出，再在两个分支各自返回（避免 `setup` 被部分 move 后无法再取字段）：

```rust
    let agent_config = build_agent_config(cfg, setup.system_prompt);

    let provider_handle = Arc::clone(&provider);
    let tools = Arc::clone(&setup.tools);
    let catalog = setup.catalog;
    let process_manager = Arc::clone(&setup.process_manager);
```

两个 `Ok(AgentBootstrap { .. })` 各加一行 `process_manager: Arc::clone(&process_manager),`（Interactive 分支在 `yolo` 之前，AutoAllow 分支同理；两者都要，因为 `Arc` 需要两个所有权）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && git add crates/yi-agent-runtime/src/bootstrap.rs
git commit -m "feat(runtime): AgentBootstrap 带出 process manager"
```

---

## Task 3: app-server 两个装配路径带出 manager 并存入 `ThreadSession`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:49-61`（`BuiltAgent`）、`:63-80`（`RuntimeTooling`）、`:596-632`（`build_runtime_tooling`）、`:636-667`（`wrap_for_delegation`）、`:1108` 与 `:1318`（两处解构）、`:1151` 与 `:1332`（两处 `ThreadSession` 字面量）、`:725-745`（基础工厂）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs:60-78`（`ThreadSession`）

**Interfaces:**
- Consumes: `AgentBootstrap.process_manager`（Task 2）、`ToolSetup.process_manager`（Task 1）
- Produces: `BuiltAgent.process_manager: Arc<ProcessManager>`；`RuntimeTooling.process_manager: Arc<ProcessManager>`；`ThreadSession.process_manager: Arc<ProcessManager>`

- [ ] **Step 1: 先确认基线为绿（本任务不新增测试）**

本任务只做「带出并保存 manager」的接线,不引入新的可观察行为,因此**不写新测试**——
它为 Task 4 的 `process/list` 提供数据来源,行为断言在 Task 4 落地。这里先确认基线:
`BuiltAgent` 是私有的、`Agent` 也没有公开的 `tools()` 访问器,任何从外部断言
「注册表与 manager 配套」的测试都无法编译;强行加一个只为测试而生的公开访问器
属于为测试改生产接口,是本任务不该做的事。

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`

Expected: PASS(既有全绿)。若不绿,先查清再动本任务。

- [ ] **Step 2: 实现**

`BuiltAgent`（`:49-61`）加字段：

```rust
struct BuiltAgent {
    agent: yi_agent_core::Agent,
    provider: Arc<dyn yi_agent_core::Provider>,
    config: yi_agent_core::AgentConfig,
    decision_tx: Option<mpsc::Sender<(u64, Decision)>>,
    decision_rx: Option<yi_agent_runtime::bootstrap::DecisionReceiver>,
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    yolo: yi_agent_core::autonomy::YoloSwitch,
    /// 支撑该 agent 工具集里的进程工具的 manager。**与生效的工具集同行**：
    /// `build_runtime_tooling` 换掉工具集时必须一并替换它。
    process_manager: Arc<yi_agent_tools::ProcessManager>,
}
```

`RuntimeTooling`（`:63`）加：

```rust
struct RuntimeTooling {
    registry: Arc<yi_agent_core::ToolRegistry>,
    permission: Arc<yi_agent_core::permission::PermissionChecker>,
    /// 这一份注册表对应的 manager(与 registry 同源)。
    process_manager: Arc<yi_agent_tools::ProcessManager>,
}
```

`build_runtime_tooling` 取出并返回（`:609-632`）：

```rust
    let setup = yi_agent_runtime::bootstrap::build_tool_setup_with_controller(
        cfg,
        false,
        &workspace_root,
        controller.clone(),
    )
    .map_err(|error| error.to_string())?;
    let mut registry = (*setup.tools).clone();
    let process_manager = Arc::clone(&setup.process_manager);
```

```rust
    Ok(RuntimeTooling {
        registry: Arc::new(registry),
        permission,
        process_manager,
    })
```

`wrap_for_delegation`（`:636`）在解构与重建中都带上：

```rust
fn wrap_for_delegation(built: BuiltAgent, tooling: RuntimeTooling) -> BuiltAgent {
    let BuiltAgent {
        agent,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        ..
    } = built;
    let session = agent.session();
    let mut rebuilt =
        yi_agent_core::Agent::new(provider.clone(), tooling.registry.clone(), config.clone())
            .with_session(session);
    if let Some(rx) = decision_rx.clone() {
        rebuilt = rebuilt.with_permission(tooling.permission.clone(), rx);
    }
    BuiltAgent {
        agent: rebuilt,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        // 注册表换了,manager 必须跟着换:否则面板查的是被丢弃的那一份。
        process_manager: tooling.process_manager,
    }
}
```

两处解构 `let BuiltAgent { .. } = activation.built;`（`:1108`、`:1318`）都加上 `process_manager`，并把它交给 `ThreadSession`：

```rust
                        let BuiltAgent {
                            agent,
                            provider,
                            config,
                            decision_tx,
                            catalog,
                            yolo,
                            process_manager,
                        } = activation.built;
```

`ThreadSession`（`session.rs:60`）加字段：

```rust
    /// 该 thread 生效的进程管理器(与它的工具集同行),供 `process/*` 查询。
    pub process_manager: Arc<yi_agent_tools::ProcessManager>,
```

两处 `ThreadSession { .. }` 字面量（`:1151`、`:1332`）各加 `process_manager: Arc::clone(&process_manager),`。

基础工厂（`:725-745`）产出 `BuiltAgent` 时补字段：

```rust
            Ok(BuiltAgent {
                agent: apply_session(built.agent, session),
                provider: built.provider,
                config,
                decision_tx: built.decision_tx,
                decision_rx: built.decision_rx,
                catalog: built.catalog,
                yolo: built.yolo,
                process_manager: built.process_manager,
            })
```

> 注意：`apply_session` 只换 session，不动工具集，所以这里的 manager 与 `built.agent` 的注册表同源——这正是要的。

- [ ] **Step 3: 修复所有编译错误**

Run: `cd yi-agent-rs && cargo build -p yi-agent-app-server`
Expected: 编译通过。测试 fixture（如 `:4874`、`:3436` 等处构造 `BuiltAgent` 的辅助函数，例如 `build_test_agent`、`build_permission_agent`）若报缺字段，按同样方式补上该 harness 自己的 `ProcessManager::new(std::env::temp_dir())`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && git add crates/yi-agent-app-server/src/server.rs crates/yi-agent-app-server/src/session.rs
git commit -m "feat(app-server): 装配路径带出并保存 per-thread process manager"
```

- [ ] **Step 3: 实现**

`BuiltAgent`（`:49-61`）加字段：

```rust
struct BuiltAgent {
    agent: yi_agent_core::Agent,
    provider: Arc<dyn yi_agent_core::Provider>,
    config: yi_agent_core::AgentConfig,
    decision_tx: Option<mpsc::Sender<(u64, Decision)>>,
    decision_rx: Option<yi_agent_runtime::bootstrap::DecisionReceiver>,
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    yolo: yi_agent_core::autonomy::YoloSwitch,
    /// 支撑该 agent 工具集里的进程工具的 manager。**与生效的工具集同行**：
    /// `build_runtime_tooling` 换掉工具集时必须一并替换它。
    process_manager: Arc<yi_agent_tools::ProcessManager>,
}
```

`RuntimeTooling`（`:63`）加：

```rust
struct RuntimeTooling {
    registry: Arc<yi_agent_core::ToolRegistry>,
    permission: Arc<yi_agent_core::permission::PermissionChecker>,
    /// 这一份注册表对应的 manager(与 registry 同源)。
    process_manager: Arc<yi_agent_tools::ProcessManager>,
}
```

`build_runtime_tooling` 取出并返回（`:609-632`）：

```rust
    let setup = yi_agent_runtime::bootstrap::build_tool_setup_with_controller(
        cfg,
        false,
        &workspace_root,
        controller.clone(),
    )
    .map_err(|error| error.to_string())?;
    let mut registry = (*setup.tools).clone();
    let process_manager = Arc::clone(&setup.process_manager);
```

```rust
    Ok(RuntimeTooling {
        registry: Arc::new(registry),
        permission,
        process_manager,
    })
```

`wrap_for_delegation`（`:636`）在解构与重建中都带上：

```rust
fn wrap_for_delegation(built: BuiltAgent, tooling: RuntimeTooling) -> BuiltAgent {
    let BuiltAgent {
        agent,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        ..
    } = built;
    let session = agent.session();
    let mut rebuilt =
        yi_agent_core::Agent::new(provider.clone(), tooling.registry.clone(), config.clone())
            .with_session(session);
    if let Some(rx) = decision_rx.clone() {
        rebuilt = rebuilt.with_permission(tooling.permission.clone(), rx);
    }
    BuiltAgent {
        agent: rebuilt,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        // 注册表换了,manager 必须跟着换:否则面板查的是被丢弃的那一份。
        process_manager: tooling.process_manager,
    }
}
```

两处解构 `let BuiltAgent { .. } = activation.built;`（`:1108`、`:1318`）都加上 `process_manager`，并把它交给 `ThreadSession`：

```rust
                        let BuiltAgent {
                            agent,
                            provider,
                            config,
                            decision_tx,
                            catalog,
                            yolo,
                            process_manager,
                        } = activation.built;
```

`ThreadSession`（`session.rs:60`）加字段：

```rust
    /// 该 thread 生效的进程管理器(与它的工具集同行),供 `process/*` 查询。
    pub process_manager: Arc<yi_agent_tools::ProcessManager>,
```

两处 `ThreadSession { .. }` 字面量（`:1151`、`:1332`）各加 `process_manager: Arc::clone(&process_manager),`。

基础工厂（`:725-745`）产出 `BuiltAgent` 时补字段：

```rust
            Ok(BuiltAgent {
                agent: apply_session(built.agent, session),
                provider: built.provider,
                config,
                decision_tx: built.decision_tx,
                decision_rx: built.decision_rx,
                catalog: built.catalog,
                yolo: built.yolo,
                process_manager: built.process_manager,
            })
```

> 注意：`apply_session` 只换 session，不动工具集，所以这里的 manager 与 `built.agent` 的注册表同源——这正是要的。

- [ ] **Step 4: 修复所有编译错误**

Run: `cd yi-agent-rs && cargo build -p yi-agent-app-server`
Expected: 编译通过。测试 fixture（如 `:4874`、`:3436` 等构造 `BuiltAgent` 的地方）若报缺字段，按同样方式补上该 thread 自己的 `ProcessManager::new(std::env::temp_dir())`。

- [ ] **Step 5: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && git add crates/yi-agent-app-server/src/server.rs crates/yi-agent-app-server/src/session.rs
git commit -m "feat(app-server): 装配路径带出并保存 per-thread process manager"
```

---

## Task 4: `process/list` 与 `process/read` 方法

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（dispatch 的 `match method.as_str()` 内，`"agent/trace/unwatch"` 分支之后）

**Interfaces:**
- Consumes: `ThreadSession.process_manager`（Task 3）
- Produces: RPC `process/list`（入参 `{ thread_id }` → `{ "processes": [ManagedProcessSnapshot...] }`）；RPC `process/read`（入参 `{ thread_id, process_id, cursor?, max_bytes? }` → `ProcessReadResult` 的 JSON）

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 tests 模块追加。复用该文件既有的 harness API：`Harness::new()` /
`Harness::with_factory(..)`（`server.rs:3527`）、`start_thread(&mut h)`（内含
`initialize`）、`h.read_value()`（`server.rs:3598`）、`build_test_agent`（`:3429`）、
`PERMISSION_TIMEOUT`。**不要**新造 harness。

```rust
    #[tokio::test]
    async fn process_list_is_empty_for_a_fresh_thread_and_for_an_unknown_one() {
        let mut h = Harness::new();
        let thread_id = start_thread(&mut h).await;

        // 新 thread:没有进程,列表为空(不是错误)。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"process/list","params":{{"thread_id":"{thread_id}"}}}}"#
        ))
        .await;
        let listed = h.read_value().await;
        assert_eq!(listed["id"], 3);
        assert!(listed["error"].is_null(), "{listed}");
        assert_eq!(listed["result"]["processes"].as_array().unwrap().len(), 0);

        // 未知 thread:仍返回空列表而非错误(切走再切回是正常路径)。
        h.send(
            r#"{"jsonrpc":"2.0","id":4,"method":"process/list","params":{"thread_id":"thread-nope"}}"#,
        )
        .await;
        let unknown = h.read_value().await;
        assert_eq!(unknown["id"], 4);
        assert!(unknown["error"].is_null(), "{unknown}");
        assert_eq!(unknown["result"]["processes"].as_array().unwrap().len(), 0);

        h.shutdown().await;
    }

    #[tokio::test]
    async fn process_read_reports_an_unknown_process_as_an_error() {
        let mut h = Harness::new();
        let thread_id = start_thread(&mut h).await;

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"process/read","params":{{"thread_id":"{thread_id}","process_id":"proc_999"}}}}"#
        ))
        .await;
        let bad = h.read_value().await;
        assert_eq!(bad["id"], 3);
        assert!(bad["result"].is_null(), "{bad}");
        assert!(bad["error"]["message"]
            .as_str()
            .unwrap()
            .contains("process not found"));

        h.shutdown().await;
    }

    /// 生效的那一份 manager 才被看见:thread 必须采用工厂给出的 manager,而不是
    /// 自建一份——否则进程面板显示空列表,而 agent 明明能起进程(设计 §4.2 的陷阱)。
    ///
    /// 顺带验证 `process/read` 的游标增量语义:两次读不重不漏。
    #[tokio::test]
    async fn process_list_and_read_observe_the_managers_the_factory_handed_over() {
        use std::sync::Arc;

        let held: Arc<yi_agent_tools::ProcessManager> =
            Arc::new(yi_agent_tools::ProcessManager::new(std::env::temp_dir()));
        let for_factory = Arc::clone(&held);

        let mut h = Harness::with_factory(
            move |session, cwd, mode| {
                let mut built = build_test_agent(session, cwd, mode)?;
                // 这一份才是「生效」的:thread 必须采用它。
                built.process_manager = Arc::clone(&for_factory);
                Ok(built)
            },
            PERMISSION_TIMEOUT,
        );
        let thread_id = start_thread(&mut h).await;

        // ready_pattern 让 start() 等到输出出现才返回,断言因此是确定性的。
        let started = held
            .start(yi_agent_tools::ProcessStartOptions {
                command: "printf alpha".into(),
                name: Some("t4-probe".into()),
                cwd: None,
                env: Default::default(),
                on_exit: Default::default(),
                ready_pattern: Some("alpha".into()),
                ready_timeout_sec: Some(5),
            })
            .await
            .expect("start");

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"process/list","params":{{"thread_id":"{thread_id}"}}}}"#
        ))
        .await;
        let listed = h.read_value().await;
        assert_eq!(listed["id"], 3);
        assert!(listed["error"].is_null(), "{listed}");
        let names: Vec<&str> = listed["result"]["processes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["name"].as_str())
            .collect();
        assert!(names.contains(&"t4-probe"), "thread must see the held manager: {listed}");

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"process/read","params":{{"thread_id":"{thread_id}","process_id":"{}"}}}}"#,
            started.process_id
        ))
        .await;
        let first = h.read_value().await;
        assert_eq!(first["id"], 4);
        assert!(first["error"].is_null(), "{first}");
        assert!(
            first["result"]["stdout"].as_str().unwrap().contains("alpha"),
            "{first}"
        );
        let cursor = first["result"]["next_cursor"].as_u64().unwrap();
        assert!(cursor > 0, "{first}");

        // 从上一轮游标继续读:没有新输出(不重不漏)。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"process/read","params":{{"thread_id":"{thread_id}","process_id":"{}","cursor":{cursor}}}}}"#,
            started.process_id
        ))
        .await;
        let second = h.read_value().await;
        assert_eq!(second["id"], 5);
        assert!(second["error"].is_null(), "{second}");
        assert_eq!(second["result"]["stdout"].as_str().unwrap(), "", "{second}");

        let _ = held.shutdown().await;
        h.shutdown().await;
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server process_list_is_empty_for_a_fresh_thread_and_for_an_unknown_one`
Expected: FAIL — 响应 `error.code == -32601`(method not found)

- [ ] **Step 3: 实现**

在 dispatch 的 `match method.as_str()` 内添加两个分支：

```rust
                    "process/list" => {
                        let Some(thread_id) =
                            req.params.get("thread_id").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::invalid_params("missing thread_id")),
                            )
                            .await?;
                            continue;
                        };
                        // 未知 thread 返回空表而非错误:切走再切回、thread 已删除
                        // 都是正常路径,报错只会弹一条无意义的红条。
                        let processes = threads
                            .get(&thread_id)
                            .map(|s| s.process_manager.list())
                            .unwrap_or_default();
                        write_response(&writer, ok_response(id, json!({ "processes": processes })))
                            .await?;
                    }
                    "process/read" => {
                        let Some(thread_id) =
                            req.params.get("thread_id").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::invalid_params("missing thread_id")),
                            )
                            .await?;
                            continue;
                        };
                        let Some(process_id) = req
                            .params
                            .get("process_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                        else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::invalid_params("missing process_id")),
                            )
                            .await?;
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        let cursor = req.params.get("cursor").and_then(|v| v.as_u64());
                        let max_bytes = req
                            .params
                            .get("max_bytes")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(64 * 1024) as usize;
                        match session
                            .process_manager
                            .read(yi_agent_tools::ProcessSelector::Id(process_id), cursor, max_bytes)
                            .await
                        {
                            Ok(result) => {
                                write_response(
                                    &writer,
                                    ok_response(
                                        id,
                                        serde_json::to_value(result)
                                            .unwrap_or(serde_json::Value::Null),
                                    ),
                                )
                                .await?
                            }
                            Err(message) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::invalid_params(message)),
                                )
                                .await?
                            }
                        }
                    }
```

在 `server.rs` 顶部 import 补上（若尚未导入）：

```rust
use yi_agent_tools::{ManagedProcessSnapshot, ProcessSelector};
```

`ManagedProcessSnapshot` 供测试断言使用；`ProcessSelector` 供 `process/read` / `process/kill` 使用。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && git add crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): process/list 与 process/read"
```

---

## Task 5: `process/kill` 方法与 `process/updated` 通知

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:108-176`（`Notification` 枚举）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（dispatch 加 `process/kill`；`thread/start` 与 `thread/resume` 里 spawn 进程事件守望者）

**Interfaces:**
- Consumes: `ThreadSession.process_manager`（Task 3）
- Produces: RPC `process/kill`（入参 `{ thread_id, process_id }` → `{ "ok": true }`）；通知 `process/updated`（params `{ thread_id, process_id, state }`，`state` 为 `ProcessStatus` 的 serde 字符串标签：`starting` / `running` / `ready` / `exited` / `killed` / `failed_to_start`）

- [ ] **Step 1: 写失败测试**

`protocol.rs` 的 tests 模块加一条序列化契约测试：

```rust
    #[test]
    fn process_updated_notification_serializes_with_snake_case_params() {
        let n = Notification::ProcessUpdated {
            thread_id: "t1".into(),
            process_id: "proc_1".into(),
            state: "running".into(),
        };
        let v = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "process/updated");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["process_id"], "proc_1");
        assert_eq!(v["params"]["state"], "running");
    }
```

`server.rs` 的 tests 模块加：

```rust
    #[tokio::test]
    async fn process_kill_reports_an_unknown_process_as_an_error() {
        let mut h = Harness::new();
        let thread_id = start_thread(&mut h).await;

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"process/kill","params":{{"thread_id":"{thread_id}","process_id":"proc_999"}}}}"#
        ))
        .await;
        let bad = h.read_value().await;
        assert_eq!(bad["id"], 3);
        assert!(bad["result"].is_null(), "{bad}");
        assert!(!bad["error"].is_null());

        h.shutdown().await;
    }

    /// kill 真的能终止进程,并把状态推到 `exited`/`killed`。
    #[tokio::test]
    async fn process_kill_terminates_a_held_manager_process() {
        use std::sync::Arc;

        let held: Arc<yi_agent_tools::ProcessManager> =
            Arc::new(yi_agent_tools::ProcessManager::new(std::env::temp_dir()));
        let for_factory = Arc::clone(&held);
        let mut h = Harness::with_factory(
            move |session, cwd, mode| {
                let mut built = build_test_agent(session, cwd, mode)?;
                built.process_manager = Arc::clone(&for_factory);
                Ok(built)
            },
            PERMISSION_TIMEOUT,
        );
        let thread_id = start_thread(&mut h).await;

        let started = held
            .start(yi_agent_tools::ProcessStartOptions {
                command: "sleep 300".into(),
                name: Some("t5-probe".into()),
                cwd: None,
                env: Default::default(),
                on_exit: Default::default(),
                ready_pattern: None,
                ready_timeout_sec: None,
            })
            .await
            .expect("start");

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"process/kill","params":{{"thread_id":"{thread_id}","process_id":"{}"}}}}"#,
            started.process_id
        ))
        .await;
        let killed = h.read_value().await;
        assert_eq!(killed["id"], 3);
        assert!(killed["error"].is_null(), "{killed}");
        assert_eq!(killed["result"]["ok"], true);

        // 状态必须落到终态之一,而不是仍显示 running。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let snap = held
                .list()
                .into_iter()
                .find(|p| p.process_id == started.process_id)
                .expect("process must still be listed after kill");
            let state = serde_json::to_value(&snap.status).unwrap();
            let label = state["state"].as_str().unwrap_or("").to_string();
            if label == "killed" || label == "exited" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process never reached a terminal state: {state}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let _ = held.shutdown().await;
        h.shutdown().await;
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server process_kill_reports_an_unknown_process_as_an_error`
Expected: 编译失败（`Notification::ProcessUpdated` 不存在）

- [ ] **Step 3: 实现 — 协议**

`protocol.rs` 的 `Notification` 枚举内（`AgentChildrenUpdated` 之后）添加：

```rust
    /// 该 thread 的某个受管进程状态发生了变化。
    ///
    /// 只由 `Started / Ready / Exited / Killed` 触发，**不含 `Output`**：stdout
    /// 是高频流，只在用户打开详情时经 `process/read` 增量拉取，不灌进事件通道。
    /// 客户端收到本通知即重拉 `process/list` — 它是**提示刷新**，不是权威数据。
    #[serde(rename = "process/updated")]
    ProcessUpdated {
        thread_id: String,
        process_id: String,
        /// `ProcessStatus` 的 serde 标签：starting / running / ready / exited /
        /// killed / failed_to_start。
        state: String,
    },
```

- [ ] **Step 4: 实现 — `process/kill`**

在 dispatch 的 `match method.as_str()` 内（`process/read` 之后）添加：

```rust
                    "process/kill" => {
                        let Some(thread_id) =
                            req.params.get("thread_id").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::invalid_params("missing thread_id")),
                            )
                            .await?;
                            continue;
                        };
                        let Some(process_id) = req
                            .params
                            .get("process_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                        else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::invalid_params("missing process_id")),
                            )
                            .await?;
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        match session
                            .process_manager
                            .kill(ProcessSelector::Id(process_id))
                            .await
                        {
                            Ok(()) => {
                                write_response(&writer, ok_response(id, json!({ "ok": true })))
                                    .await?
                            }
                            Err(message) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::invalid_params(message)),
                                )
                                .await?
                            }
                        }
                    }
```

- [ ] **Step 5: 实现 — 事件守望者**

在 `ChildrenWatch`（`:511`）旁加一个同构类型：

```rust
/// 每个 thread 一个进程状态守望者:订阅该 thread 生效 manager 的广播,把
/// 低频状态事件转成 `process/updated` 通知。`Output` 事件在此丢弃。
struct ProcessWatch {
    task: tokio::task::JoinHandle<()>,
}

impl ProcessWatch {
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}
```

加守望函数（放在 `watch_children` 附近）：

```rust
/// 把 manager 的状态广播转成 `process/updated` 通知。
///
/// 只在状态变化时推送(`Output` 丢弃):高频输出走 `process/read` 按需拉取。
/// 关闭 thread 时由调用方 abort。
async fn watch_processes<W>(
    writer: Arc<MessageWriter<W>>,
    thread_id: String,
    manager: Arc<yi_agent_tools::ProcessManager>,
    mut rx: tokio::sync::broadcast::Receiver<yi_agent_tools::ProcessEvent>,
) where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use yi_agent_tools::ProcessEvent;
    loop {
        match rx.recv().await {
            Ok(event) => {
                let (process_id, state) = match event {
                    ProcessEvent::Started { process_id } => (process_id, "starting"),
                    ProcessEvent::Ready { process_id } => (process_id, "ready"),
                    ProcessEvent::Exited { process_id, .. } => (process_id, "exited"),
                    ProcessEvent::Killed { process_id } => (process_id, "killed"),
                    // 输出是高频流:不转发,详情页自己按需增量拉。
                    ProcessEvent::Output { .. } => continue,
                };
                let _ = manager.list(); // 保证条目仍在(仅作存在性自检,结果丢弃)
                let _ = write_notification(
                    &writer,
                    &Notification::ProcessUpdated {
                        thread_id: thread_id.clone(),
                        process_id,
                        state: state.to_string(),
                    },
                )
                .await;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}
```

在 `run_with` 的局部变量区（`children_watches` 旁，`:816` 附近）加：

```rust
    // 每个 thread 一个进程状态守望者,建立 thread 时拉起。
    let mut process_watches: HashMap<String, ProcessWatch> = HashMap::new();
```

在 `thread/start` 与 `thread/resume` 两处、`ThreadSession { .. }` 插入 `threads` **之后**（`process_manager` 此时仍可用），各加：

```rust
                        if !process_watches.contains_key(&thread_id) {
                            let rx = process_manager.subscribe();
                            let handle = tokio::spawn(watch_processes(
                                Arc::clone(&writer),
                                thread_id.clone(),
                                Arc::clone(&process_manager),
                                rx,
                            ));
                            process_watches.insert(thread_id.clone(), ProcessWatch { task: handle });
                        }
```

在两处 thread 删除/清理路径（`:1563-1568` 与 `:2130-2135` 的 `children_watches.remove` 之后）各加：

```rust
                        if let Some(watch) = process_watches.remove(&thread_id) {
                            watch.stop().await;
                        }
```

> 若 `process_manager` 在两处构造点已被 move 进 `ThreadSession`，用 `Arc::clone(&process_manager)`（或在构造 `ThreadSession` 前先 clone 一份供守望者用）；`ThreadSession` 持有的是 clone，不是唯一所有权。

- [ ] **Step 6: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS

- [ ] **Step 7: 端到端验证：生效的那一份 manager 才被看见**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
说明：本步不新写测试，而是确认 Task 3 + Task 5 合起来后承诺的不变量成立——`thread/start` 起进程（由 agent 工具调用）后，`process/list` 能查到该进程。此端到端断言由 Task 9 的完整回归与人工冒烟覆盖；此处只需确认全量测试绿。

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs && git add crates/yi-agent-app-server/src/protocol.rs crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): process/kill 与 process/updated 通知"
```

---

## Task 6: 前端协议类型与进程快照折叠

**Files:**
- Modify: `desktop/src/lib/protocol.ts`（追加类型）
- Create: `desktop/src/lib/processes.ts`
- Test: `desktop/src/lib/processes.test.ts`

**Interfaces:**
- Produces: `ProcessStatus`、`ProcessInfo`、`ProcessListResult`、`ProcessReadResult`（协议类型）；`isActive(status)`、`partitionProcesses(list)`、`processDisplayName(p)`、`statusColor(status)`、`statusLabel(status)`、`formatElapsed(sec)`（纯函数）

- [ ] **Step 1: 写失败测试**

`desktop/src/lib/processes.test.ts`：

```ts
import { describe, it, expect } from "vitest";
import {
  formatElapsed,
  isActive,
  partitionProcesses,
  processDisplayName,
  statusColor,
  statusLabel,
} from "./processes";
import type { ProcessInfo } from "./protocol";

const p = (id: string, status: ProcessInfo["status"], name?: string): ProcessInfo => ({
  process_id: id,
  name: name ?? null,
  pid: 4242,
  command: "npm run dev",
  cwd: "/tmp",
  status,
  ready: status === "ready",
  on_exit: "kill",
  exit_code: null,
  elapsed_sec: 1.5,
});

describe("processes folding", () => {
  it("treats starting/running/ready as active", () => {
    expect(isActive("starting")).toBe(true);
    expect(isActive("running")).toBe(true);
    expect(isActive("ready")).toBe(true);
  });

  it("treats exited/killed/failed as terminal", () => {
    expect(isActive("exited")).toBe(false);
    expect(isActive("killed")).toBe(false);
    expect(isActive("failed_to_start")).toBe(false);
  });

  it("splits a list into active and finished, keeping order", () => {
    const { active, finished } = partitionProcesses([
      p("proc_1", "running", "dev"),
      p("proc_2", "exited"),
      p("proc_3", "ready", "server"),
      p("proc_4", "killed"),
    ]);
    expect(active.map((x) => x.process_id)).toEqual(["proc_1", "proc_3"]);
    expect(finished.map((x) => x.process_id)).toEqual(["proc_2", "proc_4"]);
  });

  it("falls back to the process id when there is no name", () => {
    expect(processDisplayName(p("proc_7", "running"))).toBe("proc_7");
    expect(processDisplayName(p("proc_8", "running", "dev"))).toBe("dev");
  });

  it("maps every status to a distinct color and label", () => {
    expect(statusColor("running")).toContain("amber");
    expect(statusColor("ready")).toContain("emerald");
    expect(statusColor("exited")).toContain("neutral");
    expect(statusColor("killed")).toContain("red");
    expect(statusLabel("failed_to_start")).toBe("失败");
  });

  it("formats elapsed seconds with one decimal", () => {
    expect(formatElapsed(1.54)).toBe("1.5s");
    expect(formatElapsed(0)).toBe("0.0s");
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/lib/processes.test.ts`
Expected: FAIL — 无法解析 `./processes`

- [ ] **Step 3: 实现 — 协议类型**

`desktop/src/lib/protocol.ts` 追加：

```ts
/**
 * 一个受管后台进程的状态。镜像 Rust 侧 `ProcessStatus` 的 serde 标签
 * (`#[serde(tag = "state", rename_all = "snake_case")]`)。
 */
export type ProcessStatus =
  | "starting"
  | "running"
  | "ready"
  | "exited"
  | "killed"
  | "failed_to_start";

/** `process/list` 的一项。镜像 Rust 的 `ManagedProcessSnapshot`（snake_case）。 */
export interface ProcessInfo {
  process_id: string;
  name: string | null;
  pid: number | null;
  command: string;
  cwd: string;
  status: ProcessStatus;
  ready: boolean;
  on_exit: "kill" | "keep";
  exit_code: number | null;
  elapsed_sec: number;
}

/** `process/list` 的响应。 */
export interface ProcessListResult {
  processes: ProcessInfo[];
}

/** `process/read` 的响应。`next_cursor` 是下一次增量读的游标。 */
export interface ProcessReadResult {
  process_id: string;
  name: string | null;
  stdout: string;
  stderr: string;
  next_cursor: number;
  truncated: boolean;
  status: ProcessStatus;
  ready: boolean;
}
```

并在 `Notification` 联合里加一条：

```ts
  | {
      method: "process/updated";
      params: { thread_id: string; process_id: string; state: string };
    }
```

- [ ] **Step 4: 实现 — 纯函数**

`desktop/src/lib/processes.ts`：

```ts
/**
 * 受管后台进程的展示层折叠。
 *
 * 纯函数 + 常量,与组件分开:分组的对错是状态 bug,渲染的对错是显示 bug,
 * 两者该分开测(与 `subagents.ts` 的理由相同)。
 */
import type { ProcessInfo, ProcessStatus } from "./protocol";

/** 仍在推进、值得默认显示的三个状态。 */
const ACTIVE: ProcessStatus[] = ["starting", "running", "ready"];

export function isActive(status: ProcessStatus): boolean {
  return ACTIVE.includes(status);
}

/**
 * 把完整列表拆成「活跃」与「已结束」两组,各自保持后端给的顺序。
 *
 * 后端 `ProcessManager::list()` 返回全部条目(只在启动失败或关闭时移除),
 * 长对话里终态会持续累积;分组放在展示层,后端不加 retention 语义。
 */
export function partitionProcesses(processes: ProcessInfo[]): {
  active: ProcessInfo[];
  finished: ProcessInfo[];
} {
  const active: ProcessInfo[] = [];
  const finished: ProcessInfo[] = [];
  for (const p of processes) {
    // 非活跃即终态:exited / killed / failed_to_start 由 else 分支覆盖,
    // 无需再列一遍,少一处会漂移的枚举。
    (isActive(p.status) ? active : finished).push(p);
  }
  return { active, finished };
}

export function processDisplayName(p: ProcessInfo): string {
  return p.name && p.name.length > 0 ? p.name : p.process_id;
}

export function statusColor(status: ProcessStatus): string {
  if (status === "ready") return "text-emerald-400";
  if (status === "exited") return "text-neutral-400";
  if (status === "killed" || status === "failed_to_start") return "text-red-400";
  return "text-amber-400";
}

const LABELS: Record<ProcessStatus, string> = {
  starting: "启动中",
  running: "运行中",
  ready: "就绪",
  exited: "已退出",
  killed: "已终止",
  failed_to_start: "失败",
};

export function statusLabel(status: ProcessStatus): string {
  return LABELS[status];
}

export function formatElapsed(sec: number): string {
  return `${sec.toFixed(1)}s`;
}
```

> 终态无需单独的常量列表：`partitionProcesses` 的 else 分支就是「非活跃即终态」，
> 少一处会漂移的枚举。

- [ ] **Step 5: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/lib/processes.test.ts`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add desktop/src/lib/protocol.ts desktop/src/lib/processes.ts desktop/src/lib/processes.test.ts
git commit -m "feat(desktop): 进程线协议类型与展示层折叠"
```

---

## Task 7: `ProcessRail` 列表组件

**Files:**
- Create: `desktop/src/components/ProcessRail.tsx`
- Test: `desktop/src/components/ProcessRail.test.tsx`

**Interfaces:**
- Consumes: `ProcessInfo`、`partitionProcesses`、`processDisplayName`、`statusColor`、`statusLabel`、`formatElapsed`（Task 6）
- Produces: `<ProcessRail processes={ProcessInfo[]} selectedProcessId={string | null} onOpen={(id: string) => void} />`

- [ ] **Step 1: 写失败测试**

`desktop/src/components/ProcessRail.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { ProcessRail } from "./ProcessRail";
import type { ProcessInfo } from "../lib/protocol";

afterEach(cleanup);

const p = (id: string, status: ProcessInfo["status"], name?: string): ProcessInfo => ({
  process_id: id,
  name: name ?? null,
  pid: 4242,
  command: "npm run dev",
  cwd: "/tmp",
  status,
  ready: status === "ready",
  on_exit: "kill",
  exit_code: null,
  elapsed_sec: 1.5,
});

describe("ProcessRail", () => {
  it("renders one card per active process with name, pid and status", () => {
    render(<ProcessRail processes={[p("proc_1", "running", "dev")]} onOpen={vi.fn()} />);
    expect(screen.getByText("dev")).toBeTruthy();
    expect(screen.getByText(/4242/)).toBeTruthy();
    expect(screen.getByText("[运行中]")).toBeTruthy();
    expect(screen.getByText(/1\.5s/)).toBeTruthy();
  });

  it("shows an empty state when there are no processes at all", () => {
    render(<ProcessRail processes={[]} onOpen={vi.fn()} />);
    expect(screen.getByText("暂无后台进程")).toBeTruthy();
  });

  it("falls back to the process id when there is no name", () => {
    render(<ProcessRail processes={[p("proc_9", "running")]} onOpen={vi.fn()} />);
    expect(screen.getByText("proc_9")).toBeTruthy();
  });

  it("collapses terminal processes into an expandable group", () => {
    render(
      <ProcessRail
        processes={[p("proc_1", "running", "dev"), p("proc_2", "exited", "old-job")]}
        onOpen={vi.fn()}
      />,
    );
    // 终态默认不出现在卡片区,只以一个分组标题暴露计数。
    expect(screen.queryByText("old-job")).toBeNull();
    expect(screen.getByText(/已结束.*1/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /已结束/ }));
    expect(screen.getByText("old-job")).toBeTruthy();
  });

  it("invokes onOpen with the process id when a card is clicked", () => {
    const onOpen = vi.fn();
    render(<ProcessRail processes={[p("proc_7", "running", "dev")]} onOpen={onOpen} />);
    fireEvent.click(screen.getByRole("button", { name: /查看进程 dev/ }));
    expect(onOpen).toHaveBeenCalledWith("proc_7");
  });

  it("marks the process the user has opened", () => {
    render(
      <ProcessRail
        processes={[p("proc_1", "running", "a"), p("proc_2", "running", "b")]}
        onOpen={vi.fn()}
        selectedProcessId="proc_2"
      />,
    );
    expect(
      screen.getByRole("button", { name: /查看进程 b/ }).getAttribute("aria-current"),
    ).toBe("true");
    expect(
      screen.getByRole("button", { name: /查看进程 a/ }).getAttribute("aria-current"),
    ).toBeNull();
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/components/ProcessRail.test.tsx`
Expected: FAIL — 无法解析 `./ProcessRail`

- [ ] **Step 3: 实现**

`desktop/src/components/ProcessRail.tsx`：

```tsx
/**
 * 主对话旁的受管后台进程暂留区。
 *
 * 与「子 agent」并列成 tab、可折叠,不是弹窗:用户要能在读主对话的同时瞥见
 * 后台服务是否还活着。点卡片回调 `onOpen(processId)` 进入详情。
 */
import { useState } from "react";
import type { ProcessInfo } from "../lib/protocol";
import {
  formatElapsed,
  partitionProcesses,
  processDisplayName,
  statusColor,
  statusLabel,
} from "../lib/processes";

function ProcessCard({
  process,
  selected,
  onOpen,
}: {
  process: ProcessInfo;
  selected: boolean;
  onOpen: (id: string) => void;
}) {
  const name = processDisplayName(process);
  return (
    <button
      type="button"
      aria-label={`查看进程 ${name}`}
      onClick={() => onOpen(process.process_id)}
      aria-current={selected ? "true" : undefined}
      className={`w-full rounded border px-3 py-2 text-left hover:border-neutral-600 ${
        selected ? "border-sky-600 bg-neutral-900" : "border-neutral-800 bg-neutral-900"
      }`}
    >
      <div className="flex items-center gap-2">
        <span className={`text-xs ${statusColor(process.status)}`}>
          [{statusLabel(process.status)}]
        </span>
        <span className="text-xs text-neutral-500">
          {process.pid !== null ? `pid ${process.pid}` : "pid -"}
        </span>
        <span className="text-xs text-neutral-500">{formatElapsed(process.elapsed_sec)}</span>
      </div>
      <div className="mt-1 truncate text-sm text-neutral-200">{name}</div>
      <div className="mt-1 truncate font-mono text-xs text-neutral-500">{process.command}</div>
    </button>
  );
}

export function ProcessRail({
  processes,
  selectedProcessId,
  onOpen,
}: {
  processes: ProcessInfo[];
  onOpen: (processId: string) => void;
  /** 用户已打开的进程,高亮让暂留区显示选择。 */
  selectedProcessId?: string | null;
}) {
  const [showFinished, setShowFinished] = useState(false);
  const { active, finished } = partitionProcesses(processes);

  if (processes.length === 0) {
    return <p className="px-3 py-2 text-sm text-neutral-500">暂无后台进程</p>;
  }

  return (
    <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
      {active.length === 0 ? (
        <p className="px-1 py-2 text-sm text-neutral-500">暂无运行中的后台进程</p>
      ) : (
        <ul className="flex flex-col gap-2">
          {active.map((p) => (
            <li key={p.process_id}>
              <ProcessCard
                process={p}
                selected={selectedProcessId === p.process_id}
                onOpen={onOpen}
              />
            </li>
          ))}
        </ul>
      )}
      {finished.length > 0 && (
        <div className="mt-3">
          <button
            type="button"
            aria-expanded={showFinished}
            className="text-xs text-neutral-500 hover:text-neutral-300"
            onClick={() => setShowFinished((v) => !v)}
          >
            已结束 ({finished.length}) {showFinished ? "收起" : "展开"}
          </button>
          {showFinished && (
            <ul className="mt-2 flex flex-col gap-2">
              {finished.map((p) => (
                <li key={p.process_id}>
                  <ProcessCard
                    process={p}
                    selected={selectedProcessId === p.process_id}
                    onOpen={onOpen}
                  />
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/components/ProcessRail.test.tsx`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/ProcessRail.tsx desktop/src/components/ProcessRail.test.tsx
git commit -m "feat(desktop): ProcessRail 进程列表"
```

---

## Task 8: `ProcessDetail` 详情组件（两栏输出 + 追尾 + kill 确认）

**Files:**
- Create: `desktop/src/components/ProcessDetail.tsx`
- Test: `desktop/src/components/ProcessDetail.test.tsx`

**Interfaces:**
- Consumes: `ProcessInfo`、`ProcessReadResult`（Task 6）
- Produces: `<ProcessDetail info={ProcessInfo} output={{stdout: string; stderr: string; truncated: boolean} | null} onClose={() => void} onKill={() => Promise<void>} />`

- [ ] **Step 1: 写失败测试**

`desktop/src/components/ProcessDetail.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, fireEvent, cleanup, screen, waitFor } from "@testing-library/react";
import { ProcessDetail } from "./ProcessDetail";
import type { ProcessInfo } from "../lib/protocol";

afterEach(cleanup);

const info = (status: ProcessInfo["status"] = "running"): ProcessInfo => ({
  process_id: "proc_1",
  name: "dev",
  pid: 4242,
  command: "npm run dev",
  cwd: "/tmp",
  status,
  ready: status === "ready",
  on_exit: "kill",
  exit_code: status === "exited" ? 0 : null,
  elapsed_sec: 3.5,
});

const output = { stdout: "ready on 3000\n", stderr: "warn: slow\n", truncated: false };

describe("ProcessDetail", () => {
  it("renders status, command and both output panes", () => {
    render(<ProcessDetail info={info()} output={output} onClose={vi.fn()} onKill={vi.fn()} />);
    expect(screen.getByText(/dev/)).toBeTruthy();
    expect(screen.getByText("npm run dev")).toBeTruthy();
    expect(screen.getByText(/ready on 3000/)).toBeTruthy();
    expect(screen.getByText(/warn: slow/)).toBeTruthy();
  });

  it("warns when earlier output has scrolled out of the buffer", () => {
    render(
      <ProcessDetail
        info={info()}
        output={{ ...output, truncated: true }}
        onClose={vi.fn()}
        onKill={vi.fn()}
      />,
    );
    expect(screen.getByText("较早输出已滚出")).toBeTruthy();
  });

  it("shows an empty state before the first read", () => {
    render(<ProcessDetail info={info()} output={null} onClose={vi.fn()} onKill={vi.fn()} />);
    expect(screen.getAllByText("(空)").length).toBe(2);
  });

  it("asks for confirmation before killing, and cancels on Escape", () => {
    const onKill = vi.fn();
    render(<ProcessDetail info={info()} output={output} onClose={vi.fn()} onKill={onKill} />);
    fireEvent.click(screen.getByRole("button", { name: "终止进程" }));
    expect(screen.getByRole("dialog")).toBeTruthy();
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(onKill).not.toHaveBeenCalled();
  });

  it("calls onKill once even if confirm is clicked twice", async () => {
    const onKill = vi.fn().mockResolvedValue(undefined);
    render(<ProcessDetail info={info()} output={output} onClose={vi.fn()} onKill={onKill} />);
    fireEvent.click(screen.getByRole("button", { name: "终止进程" }));
    const confirm = screen.getByRole("button", { name: "确认终止" });
    fireEvent.click(confirm);
    fireEvent.click(confirm);
    await waitFor(() => expect(onKill).toHaveBeenCalledTimes(1));
  });

  it("hides the kill affordance for a terminal process", () => {
    render(
      <ProcessDetail info={info("exited")} output={output} onClose={vi.fn()} onKill={vi.fn()} />,
    );
    expect(screen.queryByRole("button", { name: "终止进程" })).toBeNull();
    expect(screen.getByText(/退出码 0/)).toBeTruthy();
  });

  it("pauses following when the user scrolls up, and resumes at the bottom", () => {
    render(<ProcessDetail info={info()} output={output} onClose={vi.fn()} onKill={vi.fn()} />);
    const stdout = screen.getByTestId("process-stdout");
    // jsdom 不做布局:显式给尺寸与滚动位置,模拟"用户上滚"。
    Object.defineProperty(stdout, "scrollHeight", { value: 400, configurable: true });
    Object.defineProperty(stdout, "clientHeight", { value: 100, configurable: true });
    stdout.scrollTop = 100;
    fireEvent.scroll(stdout);
    expect(screen.getByText("已暂停追尾")).toBeTruthy();
    stdout.scrollTop = 300;
    fireEvent.scroll(stdout);
    expect(screen.queryByText("已暂停追尾")).toBeNull();
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/components/ProcessDetail.test.tsx`
Expected: FAIL — 无法解析 `./ProcessDetail`

- [ ] **Step 3: 实现**

`desktop/src/components/ProcessDetail.tsx`：

```tsx
/**
 * 受管后台进程详情:状态摘要 + stdout/stderr 分栏 + 追尾 + kill。
 *
 * 输出由调用方增量拉取后传进来(本组件不轮询),这样"只在详情打开时拉"这条
 * 约束由唯一的数据所有者(App)负责,组件保持可独立测试。
 */
import { useEffect, useRef, useState } from "react";
import type { ProcessInfo } from "../lib/protocol";
import { statusLabel } from "../lib/processes";

/** 一个输出栏:内容追加时若用户仍在底部则跟随,上滚即暂停。 */
function OutputPane({
  title,
  text,
  testId,
  onFollowChange,
}: {
  title: string;
  text: string;
  testId: string;
  onFollowChange: (following: boolean) => void;
}) {
  const ref = useRef<HTMLPreElement | null>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 8;
    if (atBottom) el.scrollTop = el.scrollHeight;
  }, [text]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="px-2 py-1 text-xs text-neutral-500">{title}</div>
      <pre
        ref={ref}
        data-testid={testId}
        className="min-h-0 flex-1 overflow-auto whitespace-pre-wrap px-2 pb-2 font-mono text-xs text-neutral-300"
        onScroll={() => {
          const el = ref.current;
          if (!el) return;
          onFollowChange(el.scrollHeight - el.scrollTop - el.clientHeight < 8);
        }}
      >
        {text}
      </pre>
    </div>
  );
}

export function ProcessDetail({
  info,
  output,
  onClose,
  onKill,
}: {
  info: ProcessInfo;
  output: { stdout: string; stderr: string; truncated: boolean } | null;
  onClose: () => void;
  onKill: () => Promise<void>;
}) {
  const [confirming, setConfirming] = useState(false);
  const [paused, setPaused] = useState(false);
  const submitted = useRef(false);
  const terminal =
    info.status === "exited" || info.status === "killed" || info.status === "failed_to_start";

  useEffect(() => {
    if (!confirming) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setConfirming(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [confirming]);

  return (
    <section
      aria-label={`进程详情 ${info.process_id}`}
      className="flex min-h-0 flex-1 flex-col border-t border-neutral-800 bg-neutral-950"
    >
      <div className="flex items-center justify-between px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <span className="text-xs text-neutral-400">[{statusLabel(info.status)}]</span>
          <span className="truncate text-sm text-neutral-200">
            {info.name ?? info.process_id}
          </span>
          <span className="text-xs text-neutral-600">
            {info.pid !== null ? `pid ${info.pid}` : "pid -"}
          </span>
          {terminal && info.exit_code !== null && (
            <span className="text-xs text-neutral-500">退出码 {info.exit_code}</span>
          )}
        </div>
        <div className="flex items-center gap-2">
          {paused && <span className="text-xs text-amber-400">已暂停追尾</span>}
          <button
            type="button"
            className="text-xs text-neutral-400 hover:text-neutral-200"
            onClick={onClose}
            aria-label="关闭进程详情"
          >
            关闭
          </button>
        </div>
      </div>
      <div className="px-3 pb-2 font-mono text-xs text-neutral-500">{info.command}</div>
      {output?.truncated && (
        <div className="px-3 pb-2 text-xs text-amber-400">较早输出已滚出</div>
      )}
      <div className="flex min-h-0 flex-1 flex-col">
        <OutputPane
          title="stdout"
          text={output?.stdout || "(空)"}
          testId="process-stdout"
          onFollowChange={(f) => setPaused(!f)}
        />
        <OutputPane
          title="stderr"
          text={output?.stderr || "(空)"}
          testId="process-stderr"
          onFollowChange={() => {}}
        />
      </div>
      {!terminal && (
        <div className="flex items-center justify-end gap-2 border-t border-neutral-800 px-3 py-2">
          <button
            type="button"
            className="rounded border border-red-800 px-2 py-1 text-xs text-red-300 hover:bg-red-950"
            onClick={() => {
              submitted.current = false;
              setConfirming(true);
            }}
          >
            终止进程
          </button>
        </div>
      )}
      {confirming && (
        <div
          role="dialog"
          aria-modal="true"
          aria-label="确认终止进程"
          className="absolute inset-0 flex items-center justify-center bg-black/60"
        >
          <div className="w-80 rounded border border-neutral-700 bg-neutral-900 p-4">
            <p className="text-sm text-neutral-200">
              终止 {info.name ?? info.process_id}?
            </p>
            <div className="mt-3 flex justify-end gap-2">
              <button
                type="button"
                className="rounded px-2 py-1 text-xs text-neutral-400 hover:text-neutral-200"
                onClick={() => setConfirming(false)}
              >
                取消
              </button>
              <button
                type="button"
                className="rounded bg-red-800 px-2 py-1 text-xs text-white hover:bg-red-700"
                onClick={async () => {
                  if (submitted.current) return;
                  submitted.current = true;
                  await onKill();
                  setConfirming(false);
                }}
              >
                确认终止
              </button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}
```

> `role="dialog"` 的 Esc 用 `window` 上的 `keydown` 监听处理。`fireEvent.keyDown` 派发的
> KeyboardEvent 会冒泡到 `window`，因此测试里对 dialog 元素派发即可被捕获。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/components/ProcessDetail.test.tsx`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/ProcessDetail.tsx desktop/src/components/ProcessDetail.test.tsx
git commit -m "feat(desktop): ProcessDetail 详情与 kill 确认"
```

---

## Task 9: `RightRail` tab 容器与收起细条

**Files:**
- Create: `desktop/src/components/RightRail.tsx`
- Create: `desktop/src/components/RightRailCollapsedStrip.tsx`
- Test: `desktop/src/components/RightRail.test.tsx`
- Test: `desktop/src/components/RightRailCollapsedStrip.test.tsx`

**Interfaces:**
- Produces: `<RightRail tabs={{id: string; label: string}[]} activeTab={string} onTab={(id) => void} onCollapse={() => void} children={ReactNode} />`；`<RightRailCollapsedStrip summary={string} onExpand={() => void} />`

- [ ] **Step 1: 写失败测试**

`desktop/src/components/RightRail.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { RightRail } from "./RightRail";

afterEach(cleanup);

const tabs = [
  { id: "subagents", label: "子 agent 2" },
  { id: "processes", label: "后台进程 1" },
];

describe("RightRail", () => {
  it("renders one tab per entry and marks the active one", () => {
    render(
      <RightRail tabs={tabs} activeTab="processes" onTab={vi.fn()}>
        <p>body</p>
      </RightRail>,
    );
    expect(screen.getByRole("button", { name: "子 agent 2" })).toBeTruthy();
    const active = screen.getByRole("button", { name: "后台进程 1" });
    expect(active.getAttribute("aria-current")).toBe("true");
  });

  it("switches tabs on click", () => {
    const onTab = vi.fn();
    render(
      <RightRail tabs={tabs} activeTab="subagents" onTab={onTab}>
        <p>body</p>
      </RightRail>,
    );
    fireEvent.click(screen.getByRole("button", { name: "后台进程 1" }));
    expect(onTab).toHaveBeenCalledWith("processes");
  });

  it("renders its children as the column body", () => {
    render(
      <RightRail tabs={tabs} activeTab="subagents" onTab={vi.fn()}>
        <p>body</p>
      </RightRail>,
    );
    expect(screen.getByText("body")).toBeTruthy();
  });

  it("offers a collapse control when the caller wants one", () => {
    const onCollapse = vi.fn();
    render(
      <RightRail tabs={tabs} activeTab="subagents" onTab={vi.fn()} onCollapse={onCollapse}>
        <p>body</p>
      </RightRail>,
    );
    fireEvent.click(screen.getByRole("button", { name: /收起右侧栏/ }));
    expect(onCollapse).toHaveBeenCalled();
  });
});
```

`desktop/src/components/RightRailCollapsedStrip.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { RightRailCollapsedStrip } from "./RightRailCollapsedStrip";

afterEach(cleanup);

describe("RightRailCollapsedStrip", () => {
  it("shows the tab counts while collapsed", () => {
    render(<RightRailCollapsedStrip summary="子 agent 2 · 进程 1" onExpand={vi.fn()} />);
    expect(screen.getByText("子 agent 2 · 进程 1")).toBeTruthy();
  });

  it("expands the rail when clicked", () => {
    const onExpand = vi.fn();
    render(<RightRailCollapsedStrip summary="子 agent 0 · 进程 0" onExpand={onExpand} />);
    fireEvent.click(screen.getByRole("button", { name: /展开右侧栏/ }));
    expect(onExpand).toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/components/RightRail.test.tsx src/components/RightRailCollapsedStrip.test.tsx`
Expected: FAIL — 模块不存在

- [ ] **Step 3: 实现**

`desktop/src/components/RightRail.tsx`：

```tsx
/**
 * 右侧栏容器:把「子 agent」与「后台进程」作为并列的两个 tab。
 *
 * 与主对话并列、可整体收起。tab 标签自带计数,收起态由
 * `RightRailCollapsedStrip` 继续显示计数,这样收起不等于丢失态势。
 */
import type { ReactNode } from "react";

export function RightRail({
  tabs,
  activeTab,
  onTab,
  onCollapse,
  children,
}: {
  tabs: { id: string; label: string }[];
  activeTab: string;
  onTab: (id: string) => void;
  onCollapse?: () => void;
  children: ReactNode;
}) {
  return (
    <aside
      aria-label="右侧栏"
      className="flex w-72 min-w-0 shrink-0 flex-col border-l border-neutral-800 bg-neutral-925"
    >
      <div className="flex items-center justify-between px-2 py-2">
        <div className="flex items-center gap-1" role="tablist" aria-label="右侧栏分页">
          {tabs.map((t) => (
            <button
              key={t.id}
              type="button"
              role="tab"
              aria-selected={activeTab === t.id}
              aria-current={activeTab === t.id ? "true" : undefined}
              onClick={() => onTab(t.id)}
              className={`rounded px-2 py-1 text-xs ${
                activeTab === t.id
                  ? "bg-neutral-800 text-neutral-100"
                  : "text-neutral-500 hover:text-neutral-300"
              }`}
            >
              {t.label}
            </button>
          ))}
        </div>
        {onCollapse && (
          <button
            type="button"
            aria-label="收起右侧栏"
            className="text-xs text-neutral-500 hover:text-neutral-300"
            onClick={onCollapse}
          >
            收起
          </button>
        )}
      </div>
      <div className="flex min-h-0 flex-1 flex-col">{children}</div>
    </aside>
  );
}
```

`desktop/src/components/RightRailCollapsedStrip.tsx`：

```tsx
/**
 * 收起态的右侧栏细条。
 *
 * 收起与展开必须成对存在——既有的「子 agent」rail 收起后没有重开的入口,
 * 这条细条就是为了不重复那个缺陷:它保留态势(两个 tab 的计数)并给出唯一的
 * 展开入口。
 */
export function RightRailCollapsedStrip({
  summary,
  onExpand,
}: {
  summary: string;
  onExpand: () => void;
}) {
  return (
    <aside
      aria-label="右侧栏"
      className="flex w-8 shrink-0 flex-col items-center gap-2 border-l border-neutral-800 bg-neutral-925 py-3"
    >
      <span className="text-xs text-neutral-500" title={summary}>
        {summary}
      </span>
      <button
        type="button"
        aria-label="展开右侧栏"
        className="text-xs text-neutral-500 hover:text-neutral-300"
        onClick={onExpand}
      >
        侧栏
      </button>
    </aside>
  );
}
```

> `summary` 直接渲染（`w-8` 下会换行堆叠），因此测试里的可见文本断言成立；`title` 只是鼠标悬停的补充。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/components/RightRail.test.tsx src/components/RightRailCollapsedStrip.test.tsx`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/RightRail.tsx desktop/src/components/RightRailCollapsedStrip.tsx \
  desktop/src/components/RightRail.test.tsx desktop/src/components/RightRailCollapsedStrip.test.tsx
git commit -m "feat(desktop): RightRail tab 容器与收起细条"
```

---

## Task 10: 在 `App.tsx` 接线

**Files:**
- Modify: `desktop/src/App.tsx`（imports；状态；`refreshProcesses`；`selectThread`；通知分支；渲染右列）

**Interfaces:**
- Consumes: Tasks 6-9 的全部产物
- Produces: 右列 tabbed 区域（子 agent | 后台进程）+ 详情 + 整体收起；`process/updated` 路由；详情打开期间以 500ms 增量轮询 `process/read`

- [ ] **Step 1: 扩充 mock 并写失败测试**

`desktop/src/App.test.tsx` 的 `vi.hoisted` 状态对象里加一个进程列表（供 mock 返回）：

```ts
    // `process/list` 返回的进程;默认空,需要时由用例填充。
    processes: [] as Array<Record<string, unknown>>,
```

`beforeEach` 里重置它：

```ts
  state.processes = [];
```

mock 的 `request` 方法内、`if (method === "thread/compact") ...` 之前加一条：

```ts
      if (method === "process/list") {
        return { processes: state.processes };
      }
      if (method === "process/read") {
        return {
          process_id: (params as { process_id: string }).process_id,
          name: "dev",
          stdout: "ready on 3000\n",
          stderr: "",
          next_cursor: 0,
          truncated: false,
          status: "running",
          ready: false,
        };
      }
      if (method === "process/kill") return { ok: true };
```

在文件末尾新增一个 describe 块：

```tsx
describe("App 后台进程接线", () => {
  const runningProcess = {
    process_id: "proc_1",
    name: "dev",
    pid: 4242,
    command: "npm run dev",
    cwd: "/w",
    status: "running",
    ready: false,
    on_exit: "kill",
    exit_code: null,
    elapsed_sec: 1.5,
  };

  const readCount = () =>
    clients[0].requests.filter((r) => r.method === "process/read").length;

  it("shows the process tab and lists a process reported by process/list", async () => {
    state.processes = [runningProcess];
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const tab = await screen.findByRole("tab", { name: /后台进程/ });
    fireEvent.click(tab);
    expect(await screen.findByText("dev")).toBeTruthy();
  });

  it("refetches the process list when process/updated arrives", async () => {
    state.processes = [runningProcess];
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const before = clients[0].requests.filter((r) => r.method === "process/list").length;

    // 通知只是"该重拉了":服务端不把权威数据放在事件里。
    state.notifHandlers[0]({
      method: "process/updated",
      params: { thread_id: "t1", process_id: "proc_1", state: "exited" },
    });

    await waitFor(() =>
      expect(
        clients[0].requests.filter((r) => r.method === "process/list").length,
      ).toBeGreaterThan(before),
    );
  });

  it("opens a detail that polls process/read, and stops polling once closed", async () => {
    state.processes = [runningProcess];
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    fireEvent.click(await screen.findByRole("tab", { name: /后台进程/ }));
    fireEvent.click(await screen.findByRole("button", { name: /查看进程 dev/ }));

    await waitFor(() => expect(readCount()).toBeGreaterThan(0));
    expect(await screen.findByText(/ready on 3000/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /关闭进程详情/ }));
    const settled = readCount();
    // 轮询间隔 500ms:等 1.2s;若清理没生效,计数必然增长。
    await new Promise((r) => setTimeout(r, 1200));
    expect(readCount()).toBe(settled);
  });
});
```

> 说明：进程类型在测试里用普通对象字面量（而非 `ProcessInfo`）以匹配 mock 的 `Record<string, unknown>` 形状；`state.processes` 的类型就是为此设成对象的数组。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: FAIL — 找不到 `后台进程` tab

- [ ] **Step 3: 实现 — imports 与状态**

`App.tsx` 顶部补 imports：

```tsx
import { ProcessRail } from "./components/ProcessRail";
import { ProcessDetail } from "./components/ProcessDetail";
import { RightRail } from "./components/RightRail";
import { RightRailCollapsedStrip } from "./components/RightRailCollapsedStrip";
import type { ProcessListResult, ProcessReadResult, ProcessInfo } from "./lib/protocol";
```

在组件内（`railCollapsed` 附近）加状态：

```tsx
  const [processes, setProcesses] = useState<ProcessInfo[]>([]);
  const [openProcessId, setOpenProcessId] = useState<string | null>(null);
  const [processOutput, setProcessOutput] = useState<{
    stdout: string;
    stderr: string;
    truncated: boolean;
  } | null>(null);
  const [railTab, setRailTab] = useState<"subagents" | "processes">("subagents");
```

- [ ] **Step 4: 实现 — 拉取与通知路由**

加一个与 `refreshSubagents` 同构的拉取函数：

```tsx
  /**
   * 取某对话当前的后台进程列表。
   *
   * 失败即放弃:与子 agent 暂留区同类,附属视图读不到就保持上一次的值(或空),
   * 绝不因为它把对话打断。未知 thread 服务端返回空表,不会走到 catch。
   */
  const refreshProcesses = async (threadId: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      const r = await c.request<ProcessListResult>("process/list", { threadId });
      // 只写当前对话:后台 thread 的状态通知不应改到用户正在看的这一列。
      if (store.currentId === threadId) {
        setProcesses(r.processes);
        force((v) => v + 1);
      }
    } catch {
      // 附属视图:读失败不改动已有内容。
    }
  };
```

在通知处理里、`agent/trace/event` 分支之后加：

```tsx
      if (n.method === "process/updated") {
        // 通知只是"该重拉了":它不携带权威数据,重拉 process/list 才是。
        void refreshProcesses(n.params.thread_id);
        return;
      }
```

在 `selectThread` 里、`void refreshSubagents(id);` 之后加：

```tsx
    // 该对话的后台进程:重进对话时重新拉取。
    void refreshProcesses(id);
    // 切对话即关闭详情:详情属于特定进程,留着会指向别的对话的运行结果。
    setOpenProcessId(null);
    setProcessOutput(null);
```

- [ ] **Step 5: 实现 — 详情轮询**

加一个 effect，仅在详情打开且进程未终态时以 500ms 增量轮询：

```tsx
  useEffect(() => {
    if (!currentId || !openProcessId) return;
    const c = clientRef.current;
    if (!c) return;
    let cursor = 0;
    let cancelled = false;
    let timer: number | undefined;

    const tick = async () => {
      try {
        const r = await c.request<ProcessReadResult>("process/read", {
          thread_id: currentId,
          process_id: openProcessId,
          cursor,
        });
        if (cancelled) return;
        cursor = r.next_cursor;
        setProcessOutput({
          stdout: r.stdout,
          stderr: r.stderr,
          truncated: r.truncated,
        });
        // 终态即停:不再有新增输出,退出码已经由列表侧展示。
        if (r.status === "exited" || r.status === "killed" || r.status === "failed_to_start") {
          return;
        }
      } catch {
        // 读失败(例如进程刚被移除):不打断 UI,等下一次 tick 或用户关闭。
      }
      if (!cancelled) timer = window.setTimeout(tick, 500);
    };
    void tick();
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [currentId, openProcessId]);
```

- [ ] **Step 6: 实现 — 渲染右列**

把现有的 `{currentId && !railCollapsed && (<SubagentRail ... />)}` 整块替换为：

```tsx
        {currentId && railCollapsed && (
          <RightRailCollapsedStrip
            summary={`子 agent ${railStore.get(currentId).length} · 进程 ${
              processes.filter((p) => p.status !== "exited" && p.status !== "killed" && p.status !== "failed_to_start").length
            }`}
            onExpand={() => setRailCollapsed(false)}
          />
        )}
        {currentId && !railCollapsed && (
          <RightRail
            tabs={[
              { id: "subagents", label: `子 agent ${railStore.get(currentId).length}` },
              { id: "processes", label: `后台进程 ${processes.length}` },
            ]}
            activeTab={railTab}
            onTab={(id) => {
              setRailTab(id === "processes" ? "processes" : "subagents");
              // 切 tab 即离开当前详情:留着会让详情盖住另一个 tab 的列表。
              setOpenProcessId(null);
              setProcessOutput(null);
            }}
            onCollapse={() => setRailCollapsed(true)}
          >
            {railTab === "processes" ? (
              openProcessId && processes.some((p) => p.process_id === openProcessId) ? (
                <ProcessDetail
                  info={processes.find((p) => p.process_id === openProcessId)!}
                  output={processOutput}
                  onClose={() => {
                    setOpenProcessId(null);
                    setProcessOutput(null);
                  }}
                  onKill={async () => {
                    try {
                      await clientRef.current?.request("process/kill", {
                        thread_id: currentId,
                        process_id: openProcessId,
                      });
                    } finally {
                      // 无论成败都重拉:失败时列表会保持原状态,不留下假象。
                      await refreshProcesses(currentId);
                    }
                  }}
                />
              ) : (
                <ProcessRail
                  processes={processes}
                  selectedProcessId={openProcessId}
                  onOpen={(id) => {
                    setOpenProcessId(id);
                    setProcessOutput(null);
                  }}
                />
              )
            ) : openSubagent ? (
              <SubagentTrace
                taskId={openSubagent}
                row={railStore.get(currentId).find((r) => r.taskId === openSubagent) ?? null}
                children={childrenOf(railStore.get(currentId), openSubagent)}
                rows={traceRows}
                onClose={() => void closeDetail(currentId)}
                onDrill={(taskId) => void openDetail(currentId, taskId)}
                onMessage={async (taskId, message) => {
                  await clientRef.current?.request("agent/message", {
                    threadId: currentId,
                    taskId,
                    message,
                  });
                }}
                onCancel={async (taskId) =>
                  await clientRef.current!.request<AgentCancelPreviewResult>(
                    "agent/cancel/preview",
                    { threadId: currentId, taskId },
                  )
                }
                onConfirmCancel={async (taskId, token) => {
                  await clientRef.current?.request("agent/cancel", {
                    threadId: currentId,
                    taskId,
                    confirmationToken: token,
                  });
                }}
              />
            ) : (
              <SubagentRail
                rows={railStore.get(currentId)}
                selectedTaskId={openSubagent}
                onOpen={(taskId) => void openDetail(currentId, taskId)}
              />
            )}
          </RightRail>
        )}
```

> 注意：`SubagentRail` 不再接收 `onCollapse`——收起改由外层 `RightRail` 统一负责（这正是修掉"收起后无法重开"的那一步）。`SubagentRail` 的 `onCollapse` prop 保持可选、不做删除，其余调用方不受影响。

- [ ] **Step 7: 运行测试确认通过**

Run: `cd desktop && npx tsc --noEmit && npx vitest run src/App.test.tsx`
Expected: PASS

- [ ] **Step 8: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): 右侧栏接入后台进程 tab 与详情"
```

---

## Task 11: 全量回归与文档更新

**Files:**
- Modify: `docs/project-management/desktop.md`（Features 加一条；非目标/已知限制如有变化同步）
- Modify: `docs/project-management/yi-agent-app-server.md`（若其方法清单需要同步）

**Interfaces:**
- Consumes: Tasks 1-10 的全部产物
- Produces: 绿的回归结果 + 与实现一致的模块文档

- [ ] **Step 1: 跑全部后端测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime -p yi-agent-app-server -p yi-agent-tools`
Expected: PASS（含 Tasks 1-5 的新测试，及 TUI 侧未被破坏的既有测试）

- [ ] **Step 2: 跑全部前端测试与类型检查**

Run: `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`
Expected: PASS，构建产出 `dist/`

- [ ] **Step 3: 跑 Tauri 后端测试**

Run: `cd desktop/src-tauri && cargo test`
Expected: PASS（bridge 未改动，应保持绿）

- [ ] **Step 4: 更新模块文档**

在 `docs/project-management/desktop.md` 的 Features 列表末尾（`TitleBar` 那条之后）追加：

```markdown
- [x] 受管后台进程可视化（右侧「后台进程」tab：列表 / 详情 / 追尾 / 终止）— 协议 `process/list` / `process/read` / `process/kill` + 通知 `process/updated`；后端让 `ProcessManager` 随其工具集同行（`AgentBootstrap` / `RuntimeTooling` / `BuiltAgent` / `ThreadSession` 各带出 `process_manager`）；前端 `ProcessRail` / `ProcessDetail` / `RightRail` / `RightRailCollapsedStrip` + `lib/processes.ts`，右列由 `SubagentRail` 升级为 tabbed（子 agent | 后台进程），并补上收起后重开的入口（修掉 `SubagentRail` 收起无法重开的既有缺陷）；列表走状态推送、输出走按需增量拉（仅详情打开时轮询）；判据：`cd yi-agent-rs && cargo test -p yi-agent-runtime -p yi-agent-app-server` 与 `cd desktop && npx vitest run && npx tsc --noEmit`；见 [设计](../superpowers/specs/2026-10-02-desktop-managed-process-visualization-design.md)
```

- [ ] **Step 5: 提交**

```bash
git add docs/project-management/desktop.md docs/project-management/yi-agent-app-server.md
git commit -m "docs: 登记受管后台进程可视化"
```

- [ ] **Step 6: 人工冒烟（成功判据）**

Run: `cd desktop && npm run sidecar && npm run tauri dev`

逐项确认（设计文档 §9）：
1. 在 GUI 里选一个 git 项目目录，让 agent 执行 `process_start`（例如 `npm run dev` 或 `sleep 300`）。
2. 右侧「后台进程」tab 立刻出现该条目（状态、pid、计时）。
3. 点开详情能看到实时输出并追尾；向上滚出现「已暂停追尾」，滚回底部恢复。
4. 「终止进程」→ 确认 → 列表转为「已终止」，`ps` 里进程确实消失。
5. 切到另一个对话，不显示该进程。
6. 收起右列 → 细条 → 重新展开，计数仍在。

---

## 自审记录

**Spec 覆盖：** §3 数据流 → Task 4/5/10；§4.1 协议 → Task 4/5；§4.2 接线（manager 同行）→ Task 1/2/3；§3.1 收起缺陷 → Task 9/10；§5.1 右列结构 → Task 9/10；§5.2 卡片 → Task 7；§5.3 详情 → Task 8/10；§5.4 kill → Task 5/8；§5.5 终态分组 → Task 6/7；§6.1 错误处理 → Task 4/5/6/8；§6.2 边界 → Task 4/10；§7 测试判据 → 各任务 Steps + Task 11；§8 非目标 → 未建 `process/start`；§9 成功判据 → Task 11 Step 6。

**已知的诚实偏差：** Task 5 的 Step 7 与 Task 3 的 Step 1 说明中，对"委派路径下生效的那一份 manager"的断言受限于 `build_runtime_tooling` 需要真实 git 项目 + daemon 才能调用，因此自动化守卫的是"注册表与 manager 配套"这一不变量，端到端由 Task 11 Step 6 的人工冒烟覆盖。这是有意取舍，不是遗漏。

**类型一致性：** `ProcessInfo` / `ProcessStatus` / `ProcessReadResult`（TS）与 `ManagedProcessSnapshot` / `ProcessStatus` / `ProcessReadResult`（Rust）字段一一对应，均为 snake_case；`process/updated` 的 `state` 用 `string`（避免前后端枚举漂移导致解析失败）；`ProcessWatch` 与既有 `ChildrenWatch` 同构；`RightRail` 的 `tabs[].id` 用 `"subagents" | "processes"`。
