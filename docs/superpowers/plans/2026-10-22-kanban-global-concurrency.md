# 看板并发配额改为按全机运行会话数实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让看板的并发调度按「全局峰值 − 本机全部运行中会话数」判定，而不是只看板内卡片计数。

**Architecture:** 宿主 `driver_tick` 每拍把全机运行会话数编进 `driver/next` 的 `sessions` 字段；插件用它算全机余量 `budget`，再与全局 `flock` 槽位池取交集。全局峰值配置迁到插件自己的全局目录 `~/.yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml`，项目级只保留与配额无关的字段。

**Tech Stack:** Rust（`yi-agent-app-server`、`plugins/superpowers-kanban` workspace）、serde_json、`libc::flock`、tempfile（测试）。

## Global Constraints

- 并发峰值 `default_max_tasks` 与时段窗口 `window` **只在全局文件** `$HOME/.yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml` 生效；项目级同名键一律忽略。
- 运行口径：`Idle` 不计；`AwaitingApproval` 不计；其余且 `active_turn_id.is_some()` 或 `status == Running` 计入。
- 兼容：请求里缺 `sessions` 字段时，插件退化为「看板内口径」并打一行 warn，绝不整拍报错。
- 读路径契约不变：配置缺失/损坏一律回退默认，绝不 panic。
- 提交前在 `plugins/superpowers-kanban` 与 `yi-agent-rs` 各自跑 `cargo fmt --all`。
- 每个 commit 不带 `Co-Authored-By`。

---

### Task 1: 宿主统计全机运行会话数

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs:96-180`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:5679-5770`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/contract/driver-protocol.json`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs`（`mod tests`）

**Interfaces:**
- Consumes: `HashMap<String, ThreadSession>`（`ThreadSession` 字段 `active_turn_id: Option<String>`、`status: Arc<Mutex<ThreadStatus>>`）。
- Produces:
  - `pub(crate) fn running_sessions(threads: &HashMap<String, ThreadSession>) -> u32`
  - `driver_step(..., sessions: u32) -> Option<ActionResult>`（新增第 6 个参数）
  - `driver/next` 请求体新增 `"sessions": {"running": <u32>}`

- [ ] **Step 1: 写失败测试（计数口径）**

直接测判定函数本身——真构造 `ThreadSession` 要带四个通道、`store` 与 `process_manager`，
代价远超这个口径本身：

```rust
    #[test]
    fn a_running_turn_counts_as_one_running_session() {
        assert!(is_a_running_session(ThreadStatus::Running, Some("t1")));
    }

    #[test]
    fn a_session_waiting_for_human_approval_does_not_count() {
        assert!(
            !is_a_running_session(ThreadStatus::AwaitingApproval, Some("t1")),
            "等决策不是运行"
        );
    }

    #[test]
    fn an_idle_session_does_not_count() {
        assert!(!is_a_running_session(ThreadStatus::Idle, None));
    }

    #[test]
    fn a_status_of_running_without_a_live_turn_still_counts() {
        // 状态已推进到 Running 但 turn id 尚未登记/已清的窗口期：保守按在跑算，
        // 宁可少起一个会话，也不超发。
        assert!(is_a_running_session(ThreadStatus::Running, None));
    }

    #[test]
    fn an_idle_status_with_a_leftover_turn_id_still_counts() {
        // 对称的保守方向：turn 还在就不信「空闲」。
        assert!(is_a_running_session(ThreadStatus::Idle, Some("t1")));
    }
```

`ThreadStatus` 需要 `use crate::protocol::ThreadStatus;`（当前 `session_driver.rs` 只 use 了
`serde_json::Value` 与 `std::path::Path`）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib session_driver::tests::a_running_turn_counts -- --nocapture`
Expected: 编译失败，`cannot find function is_a_running_session`。

- [ ] **Step 3: 实现判定函数与 payload 字段**

在 `session_driver.rs`（`threads_payload` 之前）加：

```rust
/// 一个会话此刻算不算「运行中」。
///
/// 口径由用户定义：正在调模型或正在执行进程算运行；**等待人决策不算**
/// （`AwaitingApproval`），闲置也不算。`status` 与 `active_turn_id` 取的是
/// 宿主同一份 `ThreadSession`，与 `driver_tick` 采 idle 快照的判据同源。
pub(crate) fn is_a_running_session(status: ThreadStatus, active_turn: Option<&str>) -> bool {
    if matches!(status, ThreadStatus::AwaitingApproval) {
        return false;
    }
    active_turn.is_some() || matches!(status, ThreadStatus::Running)
}

/// 本进程此刻有几个会话在跑。宿主不认识看板：所有归属的会话都在内
/// （手动开的、别的插件起的、看板起的），这正是「限总流量」要的口径。
pub(crate) fn running_sessions(threads: &HashMap<String, ThreadSession>) -> u32 {
    threads
        .values()
        .filter(|session| {
            let status = *session.status.lock().unwrap_or_else(|p| p.into_inner());
            is_a_running_session(status, session.active_turn_id.as_deref())
        })
        .count() as u32
}
```

`driver_step` 加参数并放进 payload：

```rust
pub(crate) async fn driver_step<L: SessionLauncher>(
    query: &(dyn Fn(&Value) -> Result<Value, String> + Sync),
    threads: &mut Vec<DriverThread>,
    results: &mut Vec<ActionResult>,
    launcher: &mut L,
    idle: &(dyn Fn(&str) -> bool + Sync),
    sessions_running: u32,
) -> Option<ActionResult> {
```

```rust
    let params = serde_json::json!({
        "threads": threads_payload(threads, idle),
        "results": results_payload,
        "sessions": { "running": sessions_running },
    });
```

- [ ] **Step 4: 更新契约文件**

`contract/driver-protocol.json` 的 `request` 里，在 `results` 之后加：

```json
    "sessions": {
      "running": 3
    },
```

并在 `request._comment` 末尾补一句：`sessions.running 是宿主本进程此刻的**全机**运行会话数（不含等待决策的会话），供插件算全机余量。`

- [ ] **Step 5: 契约测试与调用点**

`server.rs:5762` 的调用改成：

```rust
            let sessions_running = crate::session_driver::running_sessions(threads);
            ...
            let outcome = crate::session_driver::driver_step(
                &query,
                &mut state.threads,
                &mut state.results,
                &mut launcher,
                &idle,
                sessions_running,
            )
            .await;
```

`session_driver.rs` 里 6 处测试调用（`:375`、`:403`、`:424`、`:454`、`:481`、`:511`、`:549`、`:652`）各补一个实参 `0`。

契约断言加一条：

```rust
    #[test]
    fn the_contract_pins_the_whole_machine_running_count() {
        let contract: Value = serde_json::from_str(DRIVER_CONTRACT).expect("契约必须是合法 JSON");
        assert!(
            contract["request"]["sessions"]["running"].is_u64(),
            "request.sessions.running 必须是数字"
        );
    }
```

- [ ] **Step 6: 跑测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib session_driver::tests`
Expected: PASS（含新增 4 条与契约 1 条）。

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/server.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-runner/contract/driver-protocol.json
git commit -m "feat(app-server): driver/next 上报全机运行会话数"
```

---

### Task 2: 全局配置路径与并发字段归属

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/layout.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs:145-300`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: `HOME` 环境变量。
- Produces:
  - `layout::global_settings_path() -> Option<PathBuf>` = `$HOME/.yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml`
  - `ConcurrencyCalendar::load_global()` / `load_project(state_dir)` / `load_merged(state_dir)`
  - `ConcurrencyCalendar::save_global()` / `save_project(state_dir)`
  - `ConcurrencyCalendar::limit_at(now)` 语义改为「全机峰值」

- [ ] **Step 1: 写失败测试**

在 `layout.rs` 的 `mod tests`：

```rust
    #[test]
    fn the_global_settings_live_under_the_plugin_directory() {
        assert_eq!(
            global_plugin_dir_in(Path::new("/home/tester")),
            PathBuf::from("/home/tester/.yi-agent/plugins/superpowers-kanban")
        );
    }
```

在 `calendar.rs` 的 `mod tests`。**全部走 `*_in` 变体，不碰进程环境**——`std::env::set_var`
在并行测试里会互相干扰，且 Rust 2024 里已不安全：

```rust
    #[test]
    fn a_project_level_max_tasks_is_ignored_in_favour_of_the_global_one() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj/.yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("superpowers-kanban.toml"), "default_max_tasks = 10\n").unwrap();
        let home = dir.path().join("home");
        let global_dir = home.join(".yi-agent/plugins/superpowers-kanban");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(global_dir.join("superpowers-kanban.toml"), "default_max_tasks = 4\n").unwrap();

        let calendar = ConcurrencyCalendar::load_merged_in(&project, &home);
        assert_eq!(calendar.default_max_tasks, 4, "项目层的峰值必须被忽略");
    }

    #[test]
    fn a_missing_global_file_falls_back_to_the_builtin_default() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj/.yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&project).unwrap();
        let calendar = ConcurrencyCalendar::load_merged_in(&project, &dir.path().join("empty-home"));
        assert_eq!(calendar.default_max_tasks, DEFAULT_MAX_TASKS);
    }

    #[test]
    fn non_concurrency_fields_still_come_from_the_project_file() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj/.yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("superpowers-kanban.toml"), "interval_secs = 42\n").unwrap();
        let calendar = ConcurrencyCalendar::load_merged_in(&project, &dir.path().join("home"));
        assert_eq!(calendar.interval_secs, 42, "节拍仍归项目级");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core global_settings live_under_the_plugin`
Expected: 编译失败，`cannot find function global_settings_path`。

- [ ] **Step 3: 实现布局与加载**

`layout.rs` 加：

```rust
/// 插件自己的全局配置目录：`$HOME/.yi-agent/plugins/superpowers-kanban/`。
///
/// 与 `$HOME/.yi-agent/superpowers-kanban/`（看板状态 + `leases/`）刻意分开：
/// 那个是**运行时状态**，这个是**用户配置**。混在一起时，清理状态目录会连带
/// 抹掉用户的并发配额。
pub fn global_plugin_dir_in(home: &Path) -> PathBuf {
    home.join(".yi-agent").join("plugins").join("superpowers-kanban")
}

pub fn global_settings_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(global_plugin_dir_in(Path::new(&home)).join("superpowers-kanban.toml"))
}
```

`calendar.rs` 把 `load_preferring_new` 拆成三段：

```rust
    /// 全局层：并发字段的唯一来源。
    pub fn load_global_in(home: &Path) -> Self {
        Self::load_or_default(&crate::layout::global_plugin_dir_in(home).join("superpowers-kanban.toml"))
    }

    /// 项目层：只剩与配额无关的三个字段。
    pub fn load_project(state_dir: &Path) -> Self {
        let new = state_dir.join("superpowers-kanban.toml");
        if new.is_file() {
            return Self::load_or_default(&new);
        }
        let legacy = state_dir.join("kanban.toml");
        if legacy.is_file() {
            return Self::load_or_default(&legacy);
        }
        Self::default()
    }

    /// 合并视图：并发字段取全局，其余取项目。
    ///
    /// 项目层即便还写着 `default_max_tasks` / `window` 也**不读**——限的是总流量，
    /// 每项目各配一个峰值必然超发（A 配 4、B 配 10 → 全机 10）。
    pub fn load_merged_in(state_dir: &Path, home: &Path) -> Self {
        let global = Self::load_global_in(home);
        let project = Self::load_project(state_dir);
        Self {
            default_max_tasks: global.default_max_tasks,
            windows: global.windows,
            interval_secs: project.interval_secs,
            archive_grace_hours: project.archive_grace_hours,
            instance_wait_secs: project.instance_wait_secs,
        }
    }

    pub fn load_merged(state_dir: &Path) -> Self {
        match std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            Some(home) => Self::load_merged_in(state_dir, Path::new(&home)),
            // 没有 HOME：没有全局层可读，回退内置默认（并发）+ 项目层（其余）。
            None => {
                let project = Self::load_project(state_dir);
                Self {
                    default_max_tasks: DEFAULT_MAX_TASKS,
                    windows: Vec::new(),
                    ..project
                }
            }
        }
    }
```

写入侧：

```rust
    pub fn save_global_in(&self, home: &Path) -> std::io::Result<()> {
        let dir = crate::layout::global_plugin_dir_in(home);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("superpowers-kanban.toml");
        let tmp = dir.join(format!("superpowers-kanban.toml.{}.tmp", std::process::id()));
        std::fs::write(&tmp, self.to_toml())?;
        std::fs::rename(&tmp, &path)
    }

    pub fn save_project(&self, state_dir: &Path) -> std::io::Result<()> {
        // 只写非并发字段：并发字段落到项目层会造成「看起来配了、实际被忽略」。
        let text = format!(
            "interval_secs = {}\narchive_grace_hours = {}\ninstance_wait_secs = {}\n",
            self.interval_secs, self.archive_grace_hours, self.instance_wait_secs
        );
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("superpowers-kanban.toml");
        let tmp = state_dir.join(format!("superpowers-kanban.toml.{}.tmp", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    }
```

`save` 与 `load_preferring_new` 先保留为 `save_project` / `load_merged` 的别名，供本任务内的调用点继续编译；Task 3 再逐个改掉。

- [ ] **Step 4: 跑测试**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core calendar:: && cargo test -p superpowers-kanban-core layout::`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add plugins/superpowers-kanban/crates/superpowers-kanban-core/src/layout.rs \
        plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs
git commit -m "feat(kanban): 并发峰值配置迁到插件全局目录"
```

---

### Task 3: 判定口径改为全机余量

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs:218-320`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs:292-310, 470-500, 735-800, 805-840`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs:199-265, 339-360`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/runner.rs:18-30`
- Test: 上述各文件 `mod tests`

**Interfaces:**
- Consumes: `driver/next` 参数里的 `sessions.running: u64`（缺省视为 `None`）。
- Produces:
  - `Board::claim_next_launch(&mut self, budget: usize) -> Option<CardId>`
  - `BoardService::driver_step(&self, threads, results, sessions_running: Option<u32>)`
  - `BoardService::next_launch(&self, sessions_running: Option<u32>, now)`
  - `runner::plan_launches(board, budget: usize)`

- [ ] **Step 1: 写失败测试（双重扣减回归锁）**

`service.rs` 的 `mod tests` 加：

```rust
    /// 全机 P=4、看板外 3 个会话在跑、看板内已有 1 个在跑 → 一个都不许起。
    ///
    /// 这是本次改造的核心回归锁：若仍走 `free_slots(limit)`（它会再减掉看板内的
    /// 1 张 Running），就会错误地再起 1 个，全机变成 5。
    #[test]
    fn outside_sessions_consume_the_same_budget_as_board_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");
        // card-1 已在跑（占 1 个看板会话）。
        service.mark_running("card-1", "thread-1").unwrap();
        // 全机 4 个在跑：card-1 + 3 个看板外的会话。
        assert!(
            service.driver_step(&[], &[], Some(4)).unwrap().is_none(),
            "budget = 4 - 4 = 0，不能起新会话"
        );
        assert_eq!(state_of(&service, "card-1"), "running");
        assert_eq!(state_of(&service, "card-2"), "queued");
    }

    #[test]
    fn a_launch_is_allowed_when_the_whole_machine_has_room() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");
        // 全机 3 个在跑（全是看板外的），峰值 4 → 还剩 1 个。
        let action = service
            .driver_step(&[], &[], Some(3))
            .unwrap()
            .expect("还剩一个名额");
        assert!(
            matches!(action, DriverAction::Launch { ref card_id, .. } if card_id == "card-1"),
            "{action:?}"
        );
    }

    /// 旧宿主不带 `sessions`：退化为看板内口径，照常调度。
    #[test]
    fn a_host_that_does_not_report_sessions_falls_back_to_the_board_count() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        assert!(
            service.driver_step(&[], &[], None).unwrap().is_some(),
            "缺字段不能挡住调度"
        );
    }
```

`board.rs` 的 `mod tests` 加：

```rust
    #[test]
    fn a_zero_budget_claims_nothing_even_with_a_full_queue() {
        let mut board = board_with(&["a"]);
        assert!(board.claim_next_launch(0).is_none());
        assert_eq!(board.get(&CardId::new("a")).unwrap().state, CardState::Queued);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner outside_sessions_consume`
Expected: 编译失败（`driver_step` 只收 2 个参数）。

- [ ] **Step 3: 改 board 的认领判定**

`board.rs`：

```rust
    /// 认领下一张排队卡所需的并发门槛由调用方按**全机余量**算好。
    ///
    /// 这里刻意不再减 `running_count`：`budget` 已经扣过全机所有在跑会话
    /// （含看板自己的），再减一次会让同一批会话被扣两遍。
    pub fn claim_next_launch(&mut self, budget: usize) -> Option<CardId> {
        if budget == 0 {
            return None;
        }
        let next = self.next_startable()?;
        if let Some(card) = self.cards.iter_mut().find(|c| c.id == next) {
            card.state = CardState::Launching;
        }
        Some(next)
    }
```

删掉 `free_slots`、`start_due`，`running_count` 仅保留给测试与 `list` 展示（若已无人调用则删除）。相应测试改成按新接口写；`board.rs:612-731` 里依赖 `start_due` 的用例，用 `claim_next_launch(1)` + 手动 `transition(Running)` 复刻。

- [ ] **Step 4: 改 service 的判定**

`service.rs` 的 `next_launch`：

```rust
    /// 认领下一张排队卡：先算出**全机余量**，再在全局槽位池里占一个。
    ///
    /// 两道闸门量的是不同的事，必须同时满足（取交集）：
    /// - `budget = 全局峰值 - 宿主报的全机运行会话数`：限的是总流量，看板外的
    ///   会话与看板内的会话吃的是同一份额度；
    /// - 全局 `flock` 槽位池（大小 = 全局峰值）：兜住同机多个插件进程的并发
    ///   竞争，池子满时即便 budget 还有余也不超发。
    pub fn next_launch(
        &self,
        sessions_running: Option<u32>,
        _now: DateTime<Local>,
    ) -> Result<Option<LaunchClaim>, String> {
        let mut inner = self.lock();
        let Some(dir) = inner.leases_dir.clone() else {
            return Err("no lease directory".into());
        };
        let calendar = ConcurrencyCalendar::load_merged(&self.state_dir);
        let limit = calendar.limit_at(Local::now());
        let budget = match sessions_running {
            Some(running) => (limit as usize).saturating_sub(running as usize),
            None => {
                // 旧宿主：退化为看板内口径（峰值 − 看板内 Running 数）。
                let cards = self.locked_cards();
                (limit as usize).saturating_sub(cards.running_count())
            }
        };
        if budget == 0 {
            return Ok(None);
        }
        let Some(lease) = lease::acquire_in(&dir, limit as usize) else {
            return Ok(None);
        };
        // ...（其余与现状相同，唯一差别是下面这行）
        let claimed = persist::with_board(&self.state_dir, |board| {
            let Some(card_id) = board.claim_next_launch(budget) else {
                return Ok(None);
            };
            // ... worktree / base_commit / workdir / title 逻辑原样保留
            Ok(Some(LaunchClaim { card_id: card_id.0, workdir, title }))
        })?;
        // ...（错误处理与 lease 入库原样保留）
    }
```

`driver_step` 签名加 `sessions_running: Option<u32>`，并把第 5 段（`service.rs:474-480`）的日历读取换成：

```rust
        // 5) 启动：队首、全机有余量 → 交出 `launch`。
        match self.next_launch(sessions_running, chrono::Local::now()) {
```

删掉原来那三行 `let limit = ... limit_at(...)`。

在此基础上补一条**调用点批量更新**步骤（实测全仓 41 处 `driver_step(`、22 处 `.next_launch(`，逐个人工改必然漏）：

- [ ] **Step 4b: 批量更新既有调用点**

先跑 `grep -rn "driver_step(&\|\.driver_step(" yi-agent-rs/crates plugins/superpowers-kanban/crates --include=*.rs | wc -l`
与新签名对账。既有用例里的 `limit` 参数**语义换成 `Some(limit)`**（等价于「宿主说全机只有看板这
几张在跑」），这样原有「峰值 3 → 起 3 张」的断言全部保持成立：

```bash
# 插件侧测试：next_launch(N, at()) → next_launch(Some(N), at())
cd plugins/superpowers-kanban
perl -pi -e 's/service\.next_launch\((\d+), at\(\)\)/service.next_launch(Some($1), at())/g' \
  crates/superpowers-kanban-runner/src/service.rs
# 插件侧测试：driver_step(&[], &[]) → driver_step(&[], &[], None)
perl -pi -e 's/\.driver_step\(&\[\], &\[\]\)/.driver_step(\&[], \&[], None)/g' \
  crates/superpowers-kanban-runner/src/dispatch.rs \
  crates/superpowers-kanban-runner/src/service.rs
```

宿主侧 41 处 `driver_step(` 同理补第 6 个实参 `0`：`perl -pi -e 's/driver_step\(&query, &mut threads, &mut results, &mut launcher, &idle\)/driver_step(\&query, \&mut threads, \&mut results, \&mut launcher, \&idle, 0)/g'`。
批量替换后**必须** `cargo test` 全绿才算改对；`grep` 复查残余（`grep -rn "next_launch([0-9]" crates` 应无输出）。

- [ ] **Step 5: 改 dispatch 与 runner**

`dispatch.rs` 的 `driver/next` 分支，在解析 `results` 之后加：

```rust
            // 宿主报的全机运行会话数。旧宿主不发 → `None` → 退化为看板内口径
            // （见 `BoardService::next_launch`）。缺字段不是错误：插件比宿主新
            // 时必须照常调度。
            let sessions_running = params
                .get("sessions")
                .and_then(|sessions| sessions.get("running"))
                .and_then(Value::as_u64)
                .map(|running| running.min(u32::MAX as u64) as u32);
            if sessions_running.is_none() {
                eprintln!(
                    "superpowers-kanban: driver/next carried no `sessions.running`; \
                     falling back to board-only concurrency accounting"
                );
            }
```

调用改 `service.driver_step(&snapshot, &results, sessions_running)?`。

`board.next_launch` 分支（`:233`）改为「没有宿主会话事实，按看板内口径」：

```rust
        "board.next_launch" => {
            // 这条通路拿不到宿主的全机事实（它不是 tick 驱动，是 IPC 调用），
            // 故按看板内口径退化；日常调度走 `driver/next`。
            match service.next_launch(None, chrono::Local::now()) {
```

`runner.rs`：

```rust
/// How many cards to start now, in FIFO order, without mutating the board.
/// `budget` is the whole-machine free allowance, already net of every running
/// session (the board's own included).
pub fn plan_launches(board: &Board, budget: usize) -> Vec<CardId> {
    board.queued_in_order().into_iter().take(budget).collect()
}
```

其测试里 `plan_launches(&board, 3)` 等调用改为「先自己算 budget」：

```rust
    #[test]
    fn a_running_card_leaves_room_for_the_rest_of_the_queue() {
        let mut board = board_with(&["a", "b", "c"]);
        // `a` 被认领 → `Launching`，不再出现在 `queued_in_order` 里。
        assert_eq!(board.claim_next_launch(1), Some(CardId::new("a")));
        // 全机余量 2（峰值 3、1 个在跑）——调用方已经扣过，这里不再减。
        assert_eq!(
            plan_launches(&board, 2),
            vec![CardId::new("b"), CardId::new("c")],
        );
    }
```

- [ ] **Step 6: 跑测试**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core && cargo test -p superpowers-kanban-runner --lib`
Expected: PASS。

- [ ] **Step 7: Commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add -A
git commit -m "feat(kanban): 并发判定改为全机余量，避免看板内外双重计数"
```

---

### Task 4: settings 读写按字段拆分

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs:339-360`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs:992-1000, 52`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs:719-730`
- Test: `dispatch.rs` 的 `mod tests`

**Interfaces:**
- Consumes: `ConcurrencyCalendar::load_merged` / `save_global_in` / `save_project`（Task 2）。
- Produces: `settings.read` 返回合并视图；`settings.write` 拆两半落盘。

- [ ] **Step 1: 写失败测试**

```rust
    /// 并发字段写进全局文件，其余写进项目文件。
    ///
    /// 走 `dispatch_with_home`（Task 2 新增的注入版）而不是 `std::env::set_var`：
    /// 后者在并行测试里会污染同进程的其它用例。
    #[test]
    fn settings_write_splits_concurrency_from_scheduling_fields() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let state = dir.path().join("proj/.yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state).unwrap();

        dispatch_with_home(&state, &home, None, "settings.write", &json!({
            "settings": {
                "default_max_tasks": 5,
                "interval_secs": 25,
                "archive_grace_hours": 24,
                "instance_wait_secs": 10,
                "windows": []
            }
        }))
        .unwrap();

        let global = std::fs::read_to_string(
            home.join(".yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml"),
        )
        .unwrap();
        assert!(global.contains("default_max_tasks = 5"), "{global}");
        let project = std::fs::read_to_string(state.join("superpowers-kanban.toml")).unwrap();
        assert!(project.contains("interval_secs = 25"), "{project}");
        assert!(
            !project.contains("default_max_tasks"),
            "项目层不得再写并发字段：{project}"
        );
    }
```

`dispatch_with_home(state_dir, home, global, method, params)` 是本任务新增的注入入口，
签名与既有 `dispatch_with_global` 平行，只多一个 `home`：

```rust
pub fn dispatch_with_home(
    state_dir: &Path,
    home: &Path,
    global: Option<SwitchValue>,
    method: &str,
    params: &Value,
) -> Result<Value, String>
```

`dispatch_with_global` 保留并转调它（`home` 取 `std::env::var_os("HOME")`）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner settings_write_splits`
Expected: FAIL，全局文件不存在。

- [ ] **Step 3: 实现拆分**

`settings.read`：

```rust
        "settings.read" => {
            // 合并视图：用户看到的是一份完整配置，但字段归属不同（并发归全局）。
            let calendar =
                superpowers_kanban_core::calendar::ConcurrencyCalendar::load_merged(state_dir);
            Ok(json!({ "settings": calendar.settings_json() }))
        }
```

`settings.write`：

```rust
        "settings.write" => {
            let settings = params
                .get("settings")
                .ok_or_else(|| "settings.write needs a `settings` object".to_string())?;
            // 校验**先于**任何写入：非法输入必须一个字节都不落盘。两半都从同一份
            // 载荷解析，校验通过后才分别写，避免写出「全局更新了、项目没更新」。
            let calendar =
                superpowers_kanban_core::calendar::ConcurrencyCalendar::from_settings_json(settings)
                    .map_err(|error| error.to_string())?;
            let home = std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(std::path::PathBuf::from)
                .ok_or_else(|| "cannot write the global settings: HOME is unset".to_string())?;
            calendar
                .save_global_in(&home)
                .map_err(|error| format!("could not save the global settings: {error}"))?;
            calendar
                .save_project(state_dir)
                .map_err(|error| format!("could not save the project settings: {error}"))?;
            Ok(json!({ "ok": true }))
        }
```

其余读取点（`main.rs:52`、`main.rs:992`、`service.rs:476`、`service.rs:723`、`dispatch.rs:236`）一律 `load_preferring_new` → `load_merged`。

- [ ] **Step 4: 跑测试**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add -A
git commit -m "feat(kanban): settings 读写按全局/项目拆分字段"
```

---

### Task 5: 契约双侧联调测试与文档

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`（契约测试）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`driver_tick` 端到端）
- Modify: `docs/project-management/superpowers-kanban.md`
- Modify: `plugins/superpowers-kanban/INSTALL.md`

**Interfaces:**
- Consumes: Task 1 的 `sessions` 字段、Task 3 的 `driver_step(..., Option<u32>)`。
- Produces: 无新增 API。

- [ ] **Step 1: 写失败测试（插件侧契约）**

`service.rs` 的 `mod tests`：

```rust
    /// 契约的 `request.sessions.running` 就是插件认的那条字段名。
    #[test]
    fn the_driver_contract_carries_the_machine_wide_running_count() {
        let contract: Value = serde_json::from_str(DRIVER_CONTRACT).expect("契约必须是合法 JSON");
        assert!(
            contract["request"]["sessions"]["running"].is_u64(),
            "宿主必须钉住 sessions.running：{}",
            contract["request"]
        );
    }
```

- [ ] **Step 2: 端到端：宿主真把非零计数送出去**

在 `server.rs` 的 driver tick 测试模块里，复用既有 `FakeDaemon` fixture，断言请求里 `sessions.running` 反映真实在跑的会话：

```rust
    /// `sessions.running` 必须是**经 tick 上送的真事实**，不是常量 0。
    #[test]
    fn the_tick_reports_how_many_sessions_are_running() {
        // 起一个会话并让它处在 Running（复用本模块既有的 host 夹具与
        // `is_idle` 判据），再跑一拍 tick，断言 `sessions.running == 1`。
        // 断言点与既有 `idle_series` 用例同一处：FakeDaemon 记下的 params。
        assert_eq!(sent[0]["sessions"]["running"], 1);
    }
```

（沿用 `server.rs:8695` 那套 `idle_seen` 记录机制，改成同时记 `params["sessions"]["running"]`。）

- [ ] **Step 3: 跑测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib the_tick_reports_how_many`
Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner the_driver_contract_carries`
Expected: 均 PASS。

- [ ] **Step 4: 更新文档**

`docs/project-management/superpowers-kanban.md` 的 Features 追加一条：

```markdown
- [x] 并发配额按**全机运行会话数**判定 — 宿主在 `driver/next` 里下发 `sessions.running`（口径：`AwaitingApproval` 与人闲不计，其余 `active_turn` 或 `Running` 计入），插件算 `峰值 − running` 作为余量，不再只看板内 `Running` 计数（双重扣减会超发）。峰值与时段窗口只从 `~/.yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml` 读，项目级同名键忽略。判据：`cargo test -p superpowers-kanban-runner outside_sessions_consume_the_same_budget`、`cargo test -p superpowers-kanban-runner a_host_that_does_not_report_sessions`、`cargo test -p superpowers-kanban-core a_project_level_max_tasks_is_ignored`；宿主侧 `cargo test -p yi-agent-app-server --lib session_driver::tests` 与 `the_tick_reports_how_many_sessions_are_running`
```

`INSTALL.md` 在配置小节补：

````markdown
### 并发配额（全局）

峰值与时段窗口是**机器级**配置，落在插件自己的全局目录：

```
~/.yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml
```

```toml
default_max_tasks = 4

[[window]]
days = ["mon", "tue", "wed", "thu", "fri"]
start = "09:00"
end = "18:00"
max_tasks = 2
```

它限的是**全机总流量**：看板外的会话（桌面里手工开的）与看板会话吃同一份额度。
项目层 `<项目>/.yi-agent/superpowers-kanban/superpowers-kanban.toml` 只认
`interval_secs` / `archive_grace_hours` / `instance_wait_secs`；
即便写了 `default_max_tasks` 也会被忽略。
````

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../plugins/superpowers-kanban && cargo fmt --all
git add -A
git commit -m "test(kanban): 钉住全机并发口径与配置归属，补文档"
```

---

## 验证清单

```bash
cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core \
  && cargo test -p superpowers-kanban-runner --lib
cd ../../yi-agent-rs && cargo test -p yi-agent-app-server --lib session_driver::tests \
  && cargo test -p yi-agent-app-server --lib the_tick_reports_how_many
```

全绿后再合并。注意 `cargo test` 前先 `ps aux | grep cargo` 确认无残留进程（见 CLAUDE.md）。
