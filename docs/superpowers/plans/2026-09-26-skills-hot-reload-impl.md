# Skills 热重载 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 让 skills catalog 在会话中途自动刷新(TUI 每条消息、app-server 每轮、daemon 每任务),无需重启进程。

**Architecture:** 给 `Agent` 加 `set_system_prompt` setter;在 `yi-agent-runtime` 新增 `SkillsCatalogHandle`(持有 base prompt + 预算策略 + `Arc<SkillsService>`),其 `current_system_prompt()` 每次调用重扫并重渲染 catalog。三个运行面在每次 `agent.run()` 前调用 setter。

**Tech Stack:** Rust, cargo workspace(`yi-agent-rs/`), tokio, async-trait, tempfile。

**设计文档:** `docs/superpowers/plans/2026-09-26-skills-hot-reload-design.md`

**工作目录:** 所有命令都在 worktree `.worktrees/feat-skills-hot-reload/yi-agent-rs/` 下执行。

**通用约定:**
- 每个 Task 结束前跑 `cargo fmt --all`,再 commit。
- 提交信息不写 `Co-Authored-By`。
- 不要并行跑多个 `cargo test`(会争锁 / OOM)。

---

## Task 1: core — `Agent::set_system_prompt`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`(在 `with_permission` 之后,约 line 322)
- Test: 同文件 `mod tests`(在 `agent_config_custom_system_prompt_overrides_default` 之后,约 line 2181)

**Step 1: 写失败测试**

在 `mod tests` 内、`agent_config_custom_system_prompt_overrides_default` 测试之后加入:

```rust
    #[tokio::test]
    async fn set_system_prompt_is_used_by_next_run() {
        use crate::provider::{ProviderError, ProviderEvent};

        struct InspectingProvider(std::sync::Mutex<Option<ProviderRequest>>);

        #[async_trait]
        impl Provider for InspectingProvider {
            async fn call_stream(
                &self,
                request: ProviderRequest,
            ) -> Result<futures::stream::BoxStream<'static, ProviderEvent>, ProviderError> {
                *self.0.lock().unwrap() = Some(request);
                Ok(futures::stream::iter(vec![ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                }])
                .boxed())
            }
        }

        let provider = Arc::new(InspectingProvider(std::sync::Mutex::new(None)));
        let config = AgentConfig {
            system_prompt: Some("original".into()),
            ..Default::default()
        };
        let mut agent = Agent::new(
            provider.clone() as Arc<dyn Provider>,
            Arc::new(ToolRegistry::new()),
            config,
        );

        agent.set_system_prompt(Some("refreshed".into()));
        let stream = agent.run("hi".into()).await.unwrap();
        let _ = collect_events(stream);

        let guard = provider.0.lock().unwrap();
        let seen = guard.as_ref().expect("provider was called");
        assert_eq!(seen.system.as_deref(), Some("refreshed"));
    }
```

注意:`futures::stream::iter(...).boxed()` 需要 `StreamExt`;若测试模块顶部已有 `use futures::stream::StreamExt;` 则无需重复;否则在测试函数内加 `use futures::stream::StreamExt;`。

**Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-core --lib set_system_prompt_is_used_by_next_run`
Expected: 编译失败,`no method named set_system_prompt found for struct Agent`。

**Step 3: 最小实现**

在 `agent.rs` 的 `with_permission` 方法之后(约 line 322,`pub fn session` 之前)加入:

```rust
    /// Replace the system prompt used by subsequent runs.
    ///
    /// `run()` clones the config at the start of each run, so setting this
    /// before `run()` takes effect for that run. Used by hot-reload paths
    /// (e.g. the skills catalog) to refresh the prompt between messages.
    pub fn set_system_prompt(&mut self, prompt: Option<String>) {
        self.config.system_prompt = prompt;
    }
```

**Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-core --lib set_system_prompt_is_used_by_next_run`
Expected: PASS(1 passed)。

**Step 5: fmt + commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "feat(core): add Agent::set_system_prompt for mid-session prompt refresh"
```

---

## Task 2: skills — 锁定 `refresh()` 契约

`SkillsService::refresh()` 已存在(`service.rs:46-51`)但无测试。本 Task 只加一个特征化(characterization)测试,保护 `SkillsCatalogHandle` 依赖的契约:**refresh 后 cache 更新为新结果**。该测试预期**立即通过**(不新增行为)。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-skills/src/service.rs`(`mod tests` 内,`snapshot_caches` 之后,约 line 299)

**Step 1: 写测试**

在 `mod tests` 内、`snapshot_caches` 之后加入:

```rust
    #[test]
    fn refresh_picks_up_new_skill() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("foo")).unwrap();
        std::fs::write(
            tmp.path().join("foo/SKILL.md"),
            "---\nname: foo\ndescription: x\n---\nbody",
        )
        .unwrap();
        let s = SkillsService::new(vec![(tmp.path().to_path_buf(), SkillScope::User)]);
        assert_eq!(s.snapshot().unwrap().len(), 1);

        std::fs::create_dir_all(tmp.path().join("bar")).unwrap();
        std::fs::write(
            tmp.path().join("bar/SKILL.md"),
            "---\nname: bar\ndescription: y\n---\nbody",
        )
        .unwrap();

        // refresh() re-scans and sees the new skill...
        assert_eq!(s.refresh().unwrap().len(), 2);
        // ...and the refreshed result replaces the cache for later snapshots.
        assert_eq!(s.snapshot().unwrap().len(), 2);
    }
```

**Step 2: 跑测试**

Run: `cargo test -p yi-agent-skills --lib refresh_picks_up_new_skill`
Expected: PASS(1 passed)。这是特征化测试,实现已存在,立即通过属预期。

**Step 3: fmt + commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-skills/src/service.rs
git commit -m "test(skills): lock refresh() cache-update contract"
```

---

## Task 3: runtime — `SkillsCatalogHandle` + 预算策略重构

本 Task 引入 handle、重构预算决策为"启动时定策略",并给 `PromptSetup` / `ToolSetup` / `AgentBootstrap` 加 `catalog` 字段。同时修 `yi-agent/src/main.rs:593` 的 `HeadlessSetup` 字面量以保持编译。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:593-596`(仅加字段)

**Step 1: 写失败测试**

在 `bootstrap.rs` 的 `mod tests` 内,把现有 `resolve_effective_budget_*` 三个测试(约 line 458-479)与两个 `resolve_system_prompt_with_skills_*` 测试(约 line 481-498)**替换**为:

```rust
    #[test]
    fn resolve_catalog_budget_policy_explicit_returns_budget() {
        assert_eq!(resolve_catalog_budget_policy(100_000, 8192, true), Some(8192));
        assert_eq!(resolve_catalog_budget_policy(0, 8192, true), Some(8192));
    }

    #[test]
    fn resolve_catalog_budget_policy_under_budget_returns_budget() {
        assert_eq!(resolve_catalog_budget_policy(4096, 8192, false), Some(8192));
        assert_eq!(resolve_catalog_budget_policy(8192, 8192, false), Some(8192));
        assert_eq!(resolve_catalog_budget_policy(0, 8192, false), Some(8192));
    }

    #[test]
    fn resolve_catalog_budget_policy_non_interactive_returns_budget() {
        // Tests run non-interactive (stdin is not a TTY), so even when
        // total > budget and explicit=false, return the budget without prompting.
        assert_eq!(resolve_catalog_budget_policy(100_000, 8192, false), Some(8192));
    }

    #[test]
    fn catalog_handle_no_skills_returns_base_prompt() {
        // No service => no handle.
        let cfg = sample_config();
        assert!(build_catalog_handle(&cfg, &None).is_none());
    }

    #[test]
    fn catalog_handle_empty_catalog_returns_base_prompt() {
        let cfg = sample_config();
        let svc = Arc::new(yi_agent_skills::SkillsService::new(vec![]));
        let handle = build_catalog_handle(&cfg, &Some(svc)).expect("handle");
        assert_eq!(handle.current_system_prompt(), resolve_system_prompt(None));
    }

    #[test]
    fn catalog_handle_is_byte_stable_when_unchanged() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("foo")).unwrap();
        std::fs::write(
            tmp.path().join("foo/SKILL.md"),
            "---\nname: foo\ndescription: x\n---\nbody",
        )
        .unwrap();
        let svc = Arc::new(yi_agent_skills::SkillsService::new(vec![(
            tmp.path().to_path_buf(),
            yi_agent_skills::SkillScope::User,
        )]));
        let cfg = sample_config();
        let handle = build_catalog_handle(&cfg, &Some(svc)).expect("handle");

        let first = handle.current_system_prompt();
        let second = handle.current_system_prompt();
        assert_eq!(first, second, "unchanged catalog must render identically");
        assert!(first.unwrap().contains("foo"));
    }

    #[test]
    fn catalog_handle_picks_up_new_skill_after_refresh() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("foo")).unwrap();
        std::fs::write(
            tmp.path().join("foo/SKILL.md"),
            "---\nname: foo\ndescription: x\n---\nbody",
        )
        .unwrap();
        let svc = Arc::new(yi_agent_skills::SkillsService::new(vec![(
            tmp.path().to_path_buf(),
            yi_agent_skills::SkillScope::User,
        )]));
        let cfg = sample_config();
        let handle = build_catalog_handle(&cfg, &Some(svc)).expect("handle");

        let before = handle.current_system_prompt().unwrap();
        assert!(!before.contains("bar"));

        std::fs::create_dir_all(tmp.path().join("bar")).unwrap();
        std::fs::write(
            tmp.path().join("bar/SKILL.md"),
            "---\nname: bar\ndescription: y\n---\nbody",
        )
        .unwrap();

        let after = handle.current_system_prompt().unwrap();
        assert!(after.contains("bar"), "new skill must appear after refresh");
    }
```

**Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-runtime --lib resolve_catalog_budget_policy`
Expected: 编译失败,`cannot find function resolve_catalog_budget_policy` / `build_catalog_handle`。

**Step 3: 实现**

3a. 在 `bootstrap.rs` 顶部 import 区(`use crate::config::RuntimeConfig;` 之后)无需新增 import(`Arc` 已有)。

3b. 在 `PromptSetup` 定义(约 line 34)之前插入 handle:

```rust
/// 刷新 skills catalog 并重新拼装 system prompt 的句柄。
///
/// 持有启动时确定的 base prompt 与预算策略;`current_system_prompt()` 每次调用
/// 都重扫 skill 根目录并重渲染 catalog。catalog 未变化时输出逐字节相同,因此
/// 不会破坏 provider 的 prompt cache。
pub struct SkillsCatalogHandle {
    service: Arc<yi_agent_skills::SkillsService>,
    base_prompt: Option<String>,
    /// `None` = 不截断(启动时用户选择了"纳入全部")。
    budget: Option<usize>,
}

impl SkillsCatalogHandle {
    /// Re-scan the skill roots and rebuild the system prompt.
    pub fn current_system_prompt(&self) -> Option<String> {
        let _ = self.service.refresh();
        let catalog = match self.budget {
            Some(budget) => self.service.render_catalog(budget),
            None => self.service.render_catalog(usize::MAX),
        };
        if catalog.is_empty() {
            return self.base_prompt.clone();
        }
        match &self.base_prompt {
            Some(base) => Some(format!("{base}\n\n{catalog}")),
            None => Some(catalog),
        }
    }
}
```

3c. 给 `PromptSetup` 加字段并改写 `build_prompt_setup`:

```rust
pub struct PromptSetup {
    pub skills: Option<Arc<yi_agent_skills::SkillsService>>,
    pub catalog: Option<SkillsCatalogHandle>,
    pub system_prompt: Option<String>,
}

pub fn build_prompt_setup(cfg: &RuntimeConfig) -> Result<PromptSetup> {
    let skills = build_skills_service(cfg)?;
    let catalog = build_catalog_handle(cfg, &skills);
    let system_prompt = match &catalog {
        Some(handle) => handle.current_system_prompt(),
        None => resolve_system_prompt(cfg.system_prompt.clone()),
    };
    Ok(PromptSetup {
        skills,
        catalog,
        system_prompt,
    })
}
```

3d. 给 `ToolSetup` 加字段;naked 分支加 `catalog: None`;非 naked 分支加 `catalog: prompt.catalog`:

```rust
pub struct ToolSetup {
    pub tools: Arc<yi_agent_core::ToolRegistry>,
    pub catalog: Option<SkillsCatalogHandle>,
    pub system_prompt: Option<String>,
}
```

naked 分支(`build_tool_setup_in` 约 line 103):

```rust
        return Ok(ToolSetup {
            tools: Arc::new(yi_agent_core::ToolRegistry::new()),
            catalog: None,
            system_prompt: None,
        });
```

非 naked 分支(约 line 134):

```rust
    Ok(ToolSetup {
        tools: Arc::new(registry),
        catalog: prompt.catalog,
        system_prompt: prompt.system_prompt,
    })
```

3e. 给 `AgentBootstrap` 加字段(约 line 155-163)并在这两个构造点填 `catalog: setup.catalog,`:

```rust
pub struct AgentBootstrap {
    pub agent: yi_agent_core::Agent,
    pub permission: Arc<yi_agent_core::permission::PermissionChecker>,
    pub decision_tx: Option<tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>>,
    pub decision_rx: Option<DecisionReceiver>,
    pub catalog: Option<SkillsCatalogHandle>,
}
```

`bootstrap_agent` 的两个 `Ok(AgentBootstrap { ... })` 各加一行 `catalog: setup.catalog,`。

3f. 用下面的函数替换 `resolve_system_prompt_with_skills`(约 line 315-338)、`resolve_effective_budget`(约 line 340-345)、`prompt_catalog_budget`(约 line 352-367)三者。保留 `is_interactive` 与 `resolve_system_prompt` 不变:

```rust
/// 构造 catalog 刷新句柄。无 skills service 时返回 None。
fn build_catalog_handle(
    cfg: &RuntimeConfig,
    skills: &Option<Arc<yi_agent_skills::SkillsService>>,
) -> Option<SkillsCatalogHandle> {
    let service = skills.as_ref()?.clone();
    let total = service.full_catalog_size();
    let budget = resolve_catalog_budget_policy(
        total,
        cfg.skills_catalog_budget,
        cfg.skills_catalog_budget_explicit,
    );
    Some(SkillsCatalogHandle {
        service,
        base_prompt: resolve_system_prompt(cfg.system_prompt.clone()),
        budget,
    })
}

/// 启动时把预算决策固化为策略:`Some(n)` 截断到 n 字节,`None` 不截断。
///
/// 仅在交互式且 catalog 超预算且未显式指定预算时才询问;刷新路径不调用本函数,
/// 因此不会在会话中途打断用户。
fn resolve_catalog_budget_policy(total: usize, budget: usize, explicit: bool) -> Option<usize> {
    if explicit || total <= budget || !is_interactive() {
        return Some(budget);
    }
    if prompt_include_all(total, budget) {
        None
    } else {
        Some(budget)
    }
}

fn prompt_include_all(total: usize, default: usize) -> bool {
    let total_kb = total / 1024;
    let default_kb = default / 1024;
    eprintln!(
        "Skills catalog is {total_kb} KB, exceeds default {default_kb} KB budget.\n\
         Include all skills? [Y/n]"
    );
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "" | "y" | "yes")
}
```

3g. 修 `yi-agent/src/main.rs:593-596` 的 `HeadlessSetup` 字面量:

```rust
    Ok(HeadlessSetup {
        tools: Arc::new(registry),
        catalog: setup.catalog,
        system_prompt: setup.system_prompt,
    })
```

**Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-runtime --lib`
Expected: 全部 PASS(含新增 7 个测试)。

**Step 5: 编译整个 workspace 确认无破坏**

Run: `cargo build --workspace`
Expected: 成功。若报其它 `ToolSetup` / `PromptSetup` / `AgentBootstrap` 字面量缺字段,补 `catalog: None`(或透传)。

**Step 6: fmt + commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(runtime): add SkillsCatalogHandle with startup-fixed budget policy"
```

---

## Task 4: TUI — 每条消息前刷新

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`(`run_agent` 约 line 822-865;`run_tui_agent` 签名约 line 1092-1104;driver 约 line 1141-1150、1324-1326)

**Step 1: 传入 handle**

在 `run_agent`(约 line 822)处,`let prompt = ...build_prompt_setup(&config)?;` 之后保持注册 SkillTool 不变。把 `prompt.catalog` 传给 `run_tui_agent`:在调用处(`run_tui_agent(...)` 约 line 853-865)新增一个实参 `prompt.catalog`。

注意 `prompt.system_prompt` 已在 line 851 被 `build_agent_config` 取走;Rust 允许按字段部分 move,顺序无碍。

**Step 2: 改 `run_tui_agent` 签名**

在参数列表末尾(`process_manager: Arc<yi_agent_tools::ProcessManager>,` 之后)加:

```rust
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
```

**Step 3: 在 driver 里持有并每条消息前 set**

在 `tokio::spawn(async move {` 的闭包内,`let mut agent = ...;`(约 line 1144-1149)之前加:

```rust
            let catalog = catalog;
```

然后在 `// Run agent` 之后、`match agent.run(text).await`(约 line 1326)**之前**插入:

```rust
                if let Some(handle) = &catalog {
                    if let Some(prompt) = handle.current_system_prompt() {
                        agent.set_system_prompt(Some(prompt));
                    }
                }
```

`/clear`、`/compact`、runtime-start 三处重建 Agent 的路径无需单独处理——上面的 set 在每次 `run()` 前执行,已覆盖。

**Step 4: 编译**

Run: `cargo build -p yi-agent`
Expected: 成功。

**Step 5: fmt + commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(tui): refresh skills catalog before each user message"
```

---

## Task 5: daemon — 每个 worker 任务前刷新

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`(工厂 struct 约 line 34-43、`new` 约 line 46-62、任务构造约 line 466)
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`(`build_daemon_worker_factory` 约 line 501-528)

**Step 1: 工厂加字段 + builder**

在 `DaemonAgentWorkerFactory` struct(约 line 34-43)加字段:

```rust
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
```

在 `new`(约 line 46-62)的 `Self { ... }` 里加 `catalog: None,`。

在 `with_workspace`(约 line 75)之后加 builder:

```rust
    /// Refresh the skills catalog before each worker task starts.
    pub fn with_catalog(
        mut self,
        catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    ) -> Self {
        self.catalog = catalog;
        self
    }
```

**Step 2: 任务开始时应用刷新**

把 `subagent_runtime.rs:466` 的:

```rust
        let config = self.config.clone();
```

改为:

```rust
        let mut config = self.config.clone();
        if let Some(catalog) = &self.catalog {
            if let Some(prompt) = catalog.current_system_prompt() {
                config.system_prompt = Some(prompt);
            }
        }
```

**Step 3: daemon 工厂接线**

在 `main.rs` 的 `build_daemon_worker_factory`(约 line 501-528),把 `prompt.catalog` 取到局部变量,并 chain `.with_catalog(...)`:

```rust
    let prompt = yi_agent_runtime::bootstrap::build_prompt_setup(&config)?;
    let catalog = prompt.catalog;
    let mut registry = yi_agent_core::ToolRegistry::new();
    if let Some(skills) = &prompt.skills {
        registry.register(Arc::new(yi_agent_tools::SkillTool::new(skills.clone())));
    }
    let agent_config =
        yi_agent_runtime::bootstrap::build_agent_config(&config, prompt.system_prompt);
    Ok(Arc::new(
        subagent_runtime::DaemonAgentWorkerFactory::new(
            provider,
            Arc::new(registry),
            agent_config,
            runtime_socket,
        )
        .with_catalog(catalog)
        .with_sandbox(config.sandbox, config.sandbox_writable_roots)
        .with_workspace(config.workdir),
    ))
```

注意 `prompt.skills` 的借用须在 `prompt.system_prompt` move 之前结束(上面的顺序满足);`let catalog = prompt.catalog;` 须在 `prompt.system_prompt` 被 move 前取出,或直接在 move 后取也可(字段独立)。

**Step 4: 编译 + 跑 daemon 相关测试**

Run: `cargo test -p yi-agent --lib subagent_runtime`
Expected: PASS。

**Step 5: fmt + commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(daemon): refresh skills catalog per worker task"
```

---

## Task 6: app-server — 每轮前刷新

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
  - `BuiltAgent`(约 line 37-41)
  - `run` 的映射闭包(约 line 52-61)
  - `thread/start` 解构 + `run_thread_driver` 调用(约 line 220、250-261)
  - `run_thread_driver` 签名(约 line 480-491)与 run 前 set(约 line 496-499)
  - 测试:`BuiltAgent` 字面量(约 line 722、733、744、1472)与 `run_thread_driver` 调用(约 line 1238、1303、1344、1689)

**Step 1: 加字段**

`BuiltAgent`:

```rust
struct BuiltAgent {
    agent: yi_agent_core::Agent,
    decision_tx: Option<mpsc::Sender<(u64, Decision)>>,
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
}
```

**Step 2: `run` 闭包透传**

约 line 57-60:

```rust
        .map(|b| BuiltAgent {
            agent: b.agent,
            decision_tx: b.decision_tx,
            catalog: b.catalog,
        })
```

**Step 3: 传进 driver**

约 line 220 解构改为:

```rust
                        let BuiltAgent { agent, decision_tx, catalog } = match build_agent() {
```

在 `tokio::spawn(run_thread_driver(...))`(约 line 250-261)的实参末尾(`Arc::clone(&perm_seq),` 之后)加 `catalog,`。

**Step 4: driver 签名 + run 前 set**

`run_thread_driver` 参数末尾(`perm_seq: Arc<AtomicU64>,` 之后)加:

```rust
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
```

在 `while let Some(TurnPrompt { turn_id, prompt }) = prompt_rx.recv().await {` 之后、`let mut stream = match agent.run(prompt).await`(约 line 499)**之前**插入:

```rust
        if let Some(handle) = &catalog {
            if let Some(new_prompt) = handle.current_system_prompt() {
                agent.set_system_prompt(Some(new_prompt));
            }
        }
```

**Step 5: 修测试**

在 4 个 `BuiltAgent { ... }` 字面量(约 line 722-731、733-742、744-753、1472-1497)各加 `catalog: None,`。
在 4 个 `run_thread_driver(...)` 调用(约 line 1238、1303、1344、1689)的实参末尾各加 `None,`。

**Step 6: 跑测试**

Run: `cargo test -p yi-agent-app-server`
Expected: PASS。

**Step 7: fmt + commit**

```bash
cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): refresh skills catalog before each turn"
```

---

## Task 7: 文档 + 收尾验证

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-skills/src/assets/skill-creator/SKILL.md:261-263`
- Modify: `yi-agent-rs/crates/yi-agent-skills/src/assets/skill-installer/SKILL.md:74-76`
- Modify: `docs/project-management/yi-agent-skills.md`
- Modify: `docs/project-management/README.md`
- Modify: `docs/project-management/yi-agent-tools.md:32`(修失效链接)

**Step 1: 改内置 skill 文档**

把 `skill-creator/SKILL.md` 末尾的:

```markdown
## Restart to Reload

yi-agent discovers skills at startup. After adding or modifying a skill, restart yi-agent to load the changes. There is no hot-reload.
```

改为:

```markdown
## Reloading

yi-agent re-scans the skill roots automatically: the TUI and app-server refresh
the catalog before each message/turn, and the daemon refreshes it before each
worker task. New or renamed skills therefore appear without a restart.

A skill's body is read from disk on every `Skill` tool call, so edits to an
existing skill take effect immediately.
```

`skill-installer/SKILL.md` 的对应段落做同样改写(先读该文件确认原文措辞)。

**Step 2: 更新模块文档**

在 `docs/project-management/yi-agent-skills.md` 的 Features 列表末尾加:

```markdown
- [x] Skills catalog 热重载 — `yi-agent-runtime/src/bootstrap.rs::SkillsCatalogHandle::current_system_prompt()` 每条消息/每轮/每任务重扫;验证:`cargo test -p yi-agent-runtime --lib catalog_handle_`
```

并把 `README.md` 模块索引表中 yi-agent-skills 的计数从 `7 / 7` 更新为 `8 / 8`。

**Step 3: 修失效链接**

把 `yi-agent-skills.md:25,31` 与 `yi-agent-tools.md:32` 中 `../plans/` 改为 `../superpowers/plans/`。仅这几处。

**Step 4: 全量相关测试**

Run(逐个,不要并行):
```bash
cargo test -p yi-agent-core
cargo test -p yi-agent-skills
cargo test -p yi-agent-runtime
cargo test -p yi-agent-tools
cargo test -p yi-agent-app-server
cargo test -p yi-agent
```
Expected: 全部 PASS。

**Step 5: 格式检查**

Run: `cargo fmt --all -- --check`
Expected: 无输出(通过)。

**Step 6: commit**

```bash
git add -A
git commit -m "docs(skills): document hot-reload and update module status"
```

---

## 完成后

用 `superpowers:finishing-a-development-branch` 合并回 `main`(`git merge --no-ff feat/skills-hot-reload`),合并后删分支 + 移除 worktree。
