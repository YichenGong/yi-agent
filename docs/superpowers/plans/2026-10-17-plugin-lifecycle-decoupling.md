# 插件生命周期去耦合（宿主不认识「看板」这个概念）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 删除宿主里的看板专属 crate、登记表与 RPC——「建/删看板」改走通用插件通道，「哪些项目要 daemon」以通用 `resident` 登记为唯一来源，宿主侧不再出现「看板」这个概念。

**Architecture:** 复用 Spec 4 早已设计的通用面：`SupervisorManifest`（通用清单）+ `yi_agent_store::resident`（通用常驻登记，字段只有 `project`/`required_by`）+ `IpcRequest::PluginQuery`（通用转发）。看板专属的 `yi-agent-boards` crate（registry/scaffold/lifecycle）被拆除：通用部分（watch/launchd/daemon 生命周期）迁入 `yi-agent-supervisors`，看板专属部分删除并由插件自持（插件经 `plugin_query` 暴露 `lifecycle/*`）。桌面端从 `board/*` RPC 迁到通用 `plugin/query` + `resident/list`。

**Tech Stack:** Rust（`yi-agent-boards` / `yi-agent-supervisors` / `yi-agent-app-server` / `yi-agent-store` / `yi-agent`(TUI/CLI)；serde、launchd plist）；桌面 TypeScript（vitest）。

**Spec:** `docs/superpowers/specs/2026-10-17-plugin-lifecycle-decoupling-design.md`
**前置：** G1（`2026-10-17-session-driver-decoupling.md`）**已落地**（`62e655db` 合入 main；见 `docs/project-management/yi-agent-app-server.md` 的 session_driver 条）。G1 已删 `card_scheduler` 与 `board_query`/`board_cards`。

## 2026-10-18 刷新说明（本 plan 相对初稿的改动）

G1 之后宿主结构变了，初稿的文件路径与依赖面已过时；实测后同步如下（逐条有据）：

1. **路径**：`board/*` 处理器现在在 `app-server/src/rpc/board.rs`；域路由在
   `app-server/src/rpc/mod.rs`（`Domain::Board` / `domain_for` / `BoardPluginState`）；
   `plugin_query` / `plugin_query_self` / `project_arg` 在 `app-server/src/server.rs`。
2. **状态归属**：`board_dir` / `resident_dir` / `plugin_command` / `board_launcher` 已从
   `AppState` 搬进 `BoardPluginState`。
3. **代价面扩大**：`yi-agent-boards` 也被 **`yi-agent`（TUI/CLI）** 依赖
   （`tui/board.rs`、`tui/superpowers_kanban.rs`、`tui/app.rs`、`main.rs`
   `Command::Boards`/`BoardsAction::Watch`、`config.rs:189/196`、
   `tests/board_{e2e,watchman_e2e}.rs`）。Task 6 因此必须一并收口，否则
   `cargo build --workspace` 不绿。
4. **鸡生蛋（新增 Task 4 的前置裁定）**：`plugin_query` 目前按
   `registry::contains` 门控（`app-server/src/server.rs:1231`），而**今天唯一**安装
   `<项目>/.yi-agent/supervisors/<name>.json` 的代码是
   `yi-agent-boards::scaffold::install_manifest`，其唯一调用者是
   `lifecycle::create_with_project`（= 要删的 `board/create`）。所以「create 直接改走
   `plugin/query`」会让**全新项目**建不了看板。spec §4.4 给出三个方案供裁定；
   **Task 4 的 create 半程等裁定**，Task 1/2/3/5/6/7 不受影响。
5. **Task 2 必须有真实调用点**：初稿的 `import_legacy_projects` 没有任何调用者，
   验收「旧环境首启仍能看见看板」无法成立；刷新后明确落到 app-server 启动路径。
6. **Task 5 的 "二选一" 收敛**：`listBoards` 一律改走**通用 `resident/list`**
   （初稿留的「继续用 board/list」与 Task 6 删 `board/list` 矛盾）。
7. **plist / CLI 同批**：初稿把 `watchman.rs` 迁进 `yi-agent-supervisors`，但 plist 跑的
   是看板名的子命令 `yi-agent boards watch`，且 `supervisors::Layout::for_workdir`
   硬编码 `.yi-agent/superpowers-kanban`——不同批改，验收的 grep 与 plist 都会坏。

## 2026-10-18 刷新说明（第二次：main 前进 98 提交后）

本 worktree 已把 main 最新（`bf6e1971`）并入（领先首版所据的 `9d885fb9` 共 98 提交，
**合并无冲突**；`cargo build --workspace` 合后仍绿）。逐条实测校正如下：

8. **Task 1 已完成**：`19bc06f5`（通用 `resident/list` + 摘门控）与 `5a201c49`
   （把重复的 stdio 泵抽进 `crate::test_support::rpc_over_stdio`）。
   评审 + 复审均干净。**Task 1 的 Step 1–5 视为已执行**，本节以下步骤保留为记录。
9. **`plugin_query` 已去门控**：现签名 `plugin_query(project, method, plugin, params)`；
   `registry::contains` 那段与 `global` 形参已不存在。故 Task 6 里「`protocol.rs` 的
   `board_not_created` 整支删除」现在**只剩 `protocol.rs` 一处**要动（`server.rs` 的
   门控测试已在 Task 1 改过）。实测现状：`protocol.rs:161`（码位映射）、`:156`（doc）、
   `:738`（`board_query_numbers_each_reason_without_renumbering` 里的断言）。
10. **主循环的函数与行号已漂移**（Task 3/6 的引用按此更新）：
    - `spawn_board_watchman_loop` 在 `server.rs:1456`，调用点在 `:1496` 与 `:1581`；
    - `ensure_watchman_installed` 在 `server.rs:288`，调用点在 `rpc/board.rs:54`；
    - 偏好键 `board_watchman_enabled` 现散在 `settings_store.rs:99,109`（读
      `yi_agent_boards::watchman::PREFERENCE_KEY`）、`rpc/ui.rs`（2 处）、
      `theme_tool.rs:44`、`server.rs:6474`、`rpc_tests.rs:13886`；`settings_store.rs` 的
      相关测试在 `:377,393,784,807`。
    - CLI：`Command::Boards` / `BoardsAction` 在 `config.rs:188-201`（注释「常驻值守」），
      `main.rs:81-84` 分发、`main.rs:238` `run_boards_watch`、`main.rs:4705` 一条 CLI 测试。
    - `tui/app.rs` 的 `global_dir()` 调用点在 **`:2751`**（初稿写 2745）。
11. **Task 6 的 `rpc_tests.rs` 面**：Board 方法表在 `rpc_tests.rs:2911-2913`
    （`("board/create", Domain::Board)` 等）；另有 `board_dir` 隔离目录（`:1564,1569,1737,
    1789`）。Task 1 已把该文件从 70 条表里 +2 行（`resident/*`）。
12. **`yi-agent-supervisors` 新增 `Layout::logs_dir()`**（main `080fa293`）：Task 3 去
    专有名时**只动 `state_dir`**，保留 `logs_dir()` / `ChildLog`（见 spec §5.6 注）。
13. **main 新引入 `is_kanban_worktree`**（`e222f2b3`/`9882faf8`，`server.rs`→
    `thread_store.rs:139`）：按物理路径约定 `.worktrees/kanban/` 识别插件 worktree，
    不读 board registry。**Task 6/7 的 grep 归零必须先裁定它的归属**（spec §5.2 三选一）。
14. **验收 grep 命令本身要改**（spec §6.2 已给出新命令）：初稿那条**大小写敏感**会
    假通过（漏 `Board*`），且**未排除 `onboarding`** 而 `grep "board"` 命中
    `onboarding*`，**永不归零**。Task 7 用 spec §6.2 的 `-i` + `onboarding` 版。

## Global Constraints

- 宿主非测试代码中不得出现看板语义：`kanban` / `board` / `卡片`（通用插件机制除外）。
- 不改 `SupervisorManifest` 既有字段语义；只新增通用可选字段。
- 迁移必须**幂等**，且保留旧文件取证（改名而非删除）。
- 端到端行为等价：桌面上仍能建/删看板、看板仍被拉起与监督。
- 旧偏好键 `board_watchman_enabled` **读时兼容**，写时写新键。
- 命令一律在仓库根执行；测试用 `cargo test -p <crate> <filter>` / `npx vitest run <file>`。
- 只显式 `git add <path>`；commit 前跑 `cargo fmt --all`（`yi-agent-rs/` 下）。

---

### Task 1: 宿主通用面——`resident/list` RPC，并摘掉 `plugin_query` 的看板门控

> **状态：已完成**（`19bc06f5` + 修复 `5a201c49`；评审与复审均干净）。
> 下列 Step 1–5 保留为已执行记录。产出：`rpc/resident.rs`（`resident/list`）、
> `rpc/mod.rs` 的 `Domain::Resident`、`server.rs` 的 `plugin_query` 去门控与 `global` 形参、
> `yi-agent-store/src/resident.rs` 的 `entries(dir)`；重复的 stdio 泵抽进
> `crate::test_support::rpc_over_stdio`。测试：`-p yi-agent-app-server --lib` 784 通过 / 0 失败。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`plugin_query` 去门控）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs`（`Domain` 新增 `Resident`）
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/resident.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`、`src/server.rs` 的 `mod tests`

**Interfaces:**
- Consumes: `yi_agent_store::resident::{list, default_dir}`（已存在）。
- Produces:
  - RPC `resident/list`，参数 `{}`，回 `{"projects":[{"project":"<abs>","required_by":["<name>"]}]}`。
    纯文件读取（registry 缺失/损坏 → 空表，与 `resident::list` 同口径）。
  - `plugin_query(project, method, plugin, params)`——**去掉 `global` 形参与看板门控**。

- [ ] **Step 1: 写失败测试**

测试放在 `src/rpc/resident.rs` 的 `mod tests`：直接调该模块的 handler，**用注入的临时目录**
（不要读真实 `$HOME`）。`resident/list` 的 handler 因此应接受一个已解析好的 registry 目录
参数（与 `rpc/ui.rs` 的 handler 收 `&AppState` 同一手法：`AppState` 已有的
`resident_dir` 就是那个注入点——`RuntimeAttachments.resident_dir`，
测试里 `Harness` 已经把它指到 tempdir）。

```rust
    /// 通用登记列举：不依赖任何看板概念，也不依赖 daemon。
    #[test]
    fn resident_list_reports_registered_projects() {
        let dir = tempfile::tempdir().unwrap();
        yi_agent_store::resident::require(
            dir.path(), Path::new("/p"), "some-plugin",
        ).unwrap();
        let value = something_that_serializes_the_registry(dir.path());
        assert_eq!(value["projects"][0]["project"], "/p");
        assert_eq!(value["projects"][0]["required_by"][0], "some-plugin");
    }

    /// 空的或损坏的登记表一律回空表：读侧宽容，绝不因一份坏文件让侧栏起不来。
    #[test]
    fn an_absent_or_corrupt_registry_lists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(entries(dir.path()).is_empty(), "absent");
        std::fs::write(
            yi_agent_store::resident::registry_path(dir.path()), "{ not json",
        ).unwrap();
        assert!(entries(dir.path()).is_empty(), "corrupt");
    }
```

在 `src/server.rs` 的 `mod tests` 里另加一条（该文件的测试已有 `tempfile` 与
`fake_daemon` helper）：

```rust
    /// 通用转发不再按「这个项目有没有看板」拒答。
    ///
    /// 反向断言的是**具体错误码**，不是「有没有报错」：摘掉门控后，一个
    /// 未登记的项目会走到「拨 socket」，于是失败码是 `daemon_unavailable`
    /// （socket 不在），**不再**是 `board_not_created`。
    #[test]
    fn a_query_is_not_gated_on_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        // 不放 boards.json，也不要 nonce：这个项目从未登记过。
        let error = plugin_query(&project, "list", "superpowers-kanban", json!({})).unwrap_err();
        assert_eq!(error.code, "daemon_unavailable", "{error}");
        assert_ne!(error.code, "board_not_created", "{error}");
    }
```

> **必须一并改的既有测试**（它们钉的是被摘掉的门控，改完要仍然断言得有意义）：
> - `a_query_without_a_registered_board_is_refused_with_its_own_code`（`server.rs`）
>   —— 删掉，它的断言正是「有 `board_not_created` 这个码」。
> - `the_rpc_maps_a_missing_board_to_its_own_code_in_data`（`server.rs`）—— 同样以
>   `board_not_created` + `-32020` 为断言。**本任务先把它改为断言
>   `daemon_unavailable` + `-32021`**（Task 6 再据最终码表收口）。
> - `project_and_registry(dir, register)` helper 的 `register` 参数在摘掉门控后
>   只影响「有没有 boards.json」，不再影响行为；保留不影响，但别再用它表达门控语义。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server resident_list_reports plugin_query_does_not_gate -- --nocapture`
Expected: FAIL（`resident/list` 不存在；`plugin_query` 仍门控）

- [ ] **Step 3: 实现**

- `plugin_query` 删掉 `match yi_agent_boards::registry::contains(global, project) { ... }`
  整段与 `global: &Path` 形参；调用点同步（`src/rpc/plugin.rs`）。
- 新增 `rpc/resident.rs`：`"resident/list"` → 读 `resident::default_dir()`，
  用 `registry_path` 解析后按 `resident` 的读侧宽容语义列举（**不要**复制一份解析逻辑：
  若需要 `required_by`，在 `yi_agent_store::resident` 加一个 `entries(dir)` 读取器，
  与 `list` 共用 `read_strict`）。
- `rpc/mod.rs`：`Domain::Resident`（前缀 `resident`）；`app_state!()` 无需新字段
  （只读 `$HOME`），或在需要注入的测试里走 `resident_dir`——若为可测性需要注入，
  复用已有的 `resident_dir` 注入点（`RuntimeAttachments` 里已有）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-app-server`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/ yi-agent-rs/crates/yi-agent-store/src/
git commit -m "feat(app-server): add the generic resident/list RPC and drop the board-query gate"
```

---

### Task 2: 迁移旧 `boards.json` → resident（一次性、幂等、**有真实调用点**）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/resident.rs`（+ 迁入函数）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（启动时调用一次）
- Test: 两文件的 `mod tests`

**Interfaces:**
- Consumes: `resident::require`。
- Produces:
  ```rust
  /// 把调用方解析出的旧登记项目导入通用 resident 登记，并把旧文件改名取证。
  /// `projects` 由调用方从旧格式解析：store 层保持通用，不认识看板格式。
  pub fn import_legacy_projects(
      projects: &[std::path::PathBuf],
      legacy_file: &Path,
      resident_dir: &Path,
      requester: &str,
  ) -> std::io::Result<usize>;
  ```
  `legacy_file` 不存在 → 返回 `0` 且不做任何事；导入成功后改名
  `<legacy_file>.migrated`（`with_extension("json.migrated")`）。
- 迁移的**旧格式解析**留在调用方（app-server）：读
  `<board_dir>/boards.json` 的 `boards[].project`。注意：Task 6 会删 `registry.rs`，
  故此处解析**不要**依赖 `yi_agent_boards`——直接读 JSON 的 `boards[].project`
  （几十行），以免迁移代码随 crate 一起被删。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn legacy_projects_are_imported_and_the_old_file_is_renamed() {
        let dir = tempfile::tempdir().unwrap();
        let resident = dir.path().join("resident");
        std::fs::create_dir_all(&resident).unwrap();
        let legacy = dir.path().join("boards.json");
        std::fs::write(&legacy, r#"{"boards":[{"project":"/p","created_at":"x"}]}"#).unwrap();

        let imported = import_legacy_projects(
            &[std::path::PathBuf::from("/p")], &legacy, &resident, "superpowers-kanban",
        ).unwrap();

        assert_eq!(imported, 1);
        assert_eq!(resident::list(&resident), vec![std::path::PathBuf::from("/p")]);
        assert!(!legacy.exists(), "the old file is renamed");
        assert!(dir.path().join("boards.json.migrated").exists());
    }

    #[test]
    fn importing_twice_is_a_no_op_the_second_time() {
        // 第二次调用时 legacy 已不存在 → 返回 0，且 resident 内容不变。
    }
```

app-server 侧另加一条：**启动路径真的会调它**（把「旧环境首启」钉在接线处，
而不只是钉在纯函数上）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-store import_legacy -- --nocapture`
Expected: FAIL

- [ ] **Step 3: 写实现 + 接线**

- `resident.rs` 加 `import_legacy_projects`（见上）。
- app-server 启动处（`run` / `serve` 的入口，与 `spawn_board_watchman_loop` 同一段
  初始化）调用一次：解析 `<board_dir>/boards.json` → `import_legacy_projects`。
  失败只记 `tracing::warn!`，**不得**阻断启动（迁移是便利，不是前置）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-store && cargo test -p yi-agent-app-server`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-store/src/ yi-agent-rs/crates/yi-agent-app-server/src/
git commit -m "feat(store): import legacy board registrations into the resident registry"
```

---

### Task 3: 把通用部分迁出 `yi-agent-boards`（含 plist 与 CLI 子命令同批通用化）

**Files:**
- Move: `yi-agent-boards/src/{watch.rs, watchman.rs, board_daemon.rs}` → `yi-agent-supervisors/src/`
  （`board_daemon.rs` → `daemon.rs`）
- Modify: `yi-agent-supervisors/src/lib.rs`、`Cargo.toml`（+ `yi-agent-store` 依赖）
- Modify: `yi-agent-supervisors/src/supervisor.rs`（`Layout` 去专有名）
- Modify: `yi-agent/src/main.rs`、`src/config.rs`（`boards watch` → `resident watch`）
- Modify: `yi-agent-boards/src/lib.rs`（过渡期 `pub use` 保留）
- Test: 迁移后的 `mod tests` 原样保留

**Interfaces:**
- Produces: `yi-agent-supervisors::watch`（`ensure_daemons_with` / `ensure_daemons` / `once`）、
  `yi-agent-supervisors::watchman`（`install` / `uninstall` / `is_installed_for` /
  `plist_contents`；标签与 plist 名改为 `resident-daemon-watchman`）、
  `yi-agent-supervisors::daemon`（`spawn_detached` / `wait_ready` / `is_running` / `stop`）。
- CLI：`yi-agent resident watch --interval-secs <n>`（取代 `yi-agent boards watch`）。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn the_watchman_plist_is_generic_not_board_specific() {
        let contents = watchman::plist_contents(Path::new("/bin/yi-agent"), Path::new("/home/u"));
        assert!(contents.contains("resident-daemon-watchman"));
        assert!(!contents.to_lowercase().contains("board"));
        assert!(contents.contains("resident")); // 跑的通用子命令
    }

    #[test]
    fn the_supervisor_layout_does_not_hardcode_a_plugin_state_dir() {
        // Layout::for_workdir 不得再出现 "superpowers-kanban"。
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-supervisors the_watchman_plist the_supervisor_layout -- --nocapture`
Expected: FAIL（模块尚不存在）

- [ ] **Step 3: 搬迁**

- `git mv` 三文件；改 `lib.rs` 导出；supervisors 需要的 `yi-agent-store`
  （watch 读 `resident`、daemon 用 `store::ipc`）加进 `Cargo.toml`。
- plist 标签/文件名去 board 名（`ai.yi-agent.resident-daemon-watchman`），
  `ProgramArguments` 的子命令改 `resident watch`。
- `Layout::for_workdir` 的 `state_dir` 不再硬编码 `.yi-agent/superpowers-kanban`：
  改为通用（如 `<workdir>/.yi-agent/state`）或由清单提供；**任一改法都要同步
  `manifest.rs` 的 `{state_dir}` 展开与插件侧 `--state-dir` 的约定**
  （`plugins/superpowers-kanban/crates/superpowers-kanban-core/src/layout.rs`），
  否则既有安装的清单会指到错目录。
- `yi-agent-boards` 里保留对它们的 `pub use`（过渡），Task 6 再删。
- `yi-agent/src/main.rs` 与 `config.rs` 同步：`Command::Boards`/`BoardsAction` →
  `Command::Resident`/`ResidentAction::Watch`，`run_boards_watch` → `run_resident_watch`
  （改读通用登记，逻辑不变）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-supervisors -p yi-agent-boards -p yi-agent`
Expected: PASS（三 crate 全绿）

- [ ] **Step 5: 提交**

```bash
git add -A yi-agent-rs/crates/yi-agent-supervisors yi-agent-rs/crates/yi-agent-boards yi-agent-rs/crates/yi-agent yi-agent-rs/Cargo.toml
git commit -m "refactor(supervisors): move the generic resident-daemon machinery out of the boards crate"
```

---

### Task 4: 插件实现 `lifecycle/create` 与 `lifecycle/remove`

> **裁定已定（spec §4.4 方案 A）**：清单归宿主的通用 `plugin/install`，插件不写清单。
> 本任务的 create 半程因此只做「建状态目录 + `resident::require`」。
>
> **状态（2026-10-19）**：`lifecycle/create` 与 `lifecycle/remove` 已落地并过评审
> （`c62f36b2`、`58de81f8`）。**本任务剩余的是 Step 1b 的 `manifest` 动词**，
> 它原先挂在「方案 A」下，现随裁定生效——见下方 Step 1b。

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/resident.rs`
- （方案 A）Modify: `.../src/main.rs`（新增 `manifest` 动词）
- Test: 各文件 `mod tests`

**Interfaces:**
- Produces: dispatch 方法 `"lifecycle/create"` / `"lifecycle/remove"`，参数 `{project}`。
- **依赖裁定（写进代码注释）**：**插件不依赖 `yi-agent-store`**（保持「插件对主程序
  零依赖」，见 Spec 4 §2「插件对主程序：零依赖（已达标）」）。resident 登记文件的格式
  （`project` + `required_by`）是**两侧共享的通用契约**，插件按格式自行读写，
  并在测试里覆盖。

- [x] **Step 1: 写失败测试**（已落地，`c62f36b2`/`58de81f8`）

```rust
    #[test]
    fn lifecycle_create_registers_the_project() {
        // 临时 HOME：dispatch lifecycle/create {project} →
        //   resident 登记出现该项目（required_by 含 superpowers-kanban）。
    }

    #[test]
    fn lifecycle_remove_unregisters_and_stops() {
        // 先 create，再 remove → resident 不再含该项目，状态目录被删。
    }

    #[test]
    fn lifecycle_create_is_idempotent() {
        // 连续两次 create：登记不重复（required_by 并集）。
    }
```

- [ ] **Step 1b: `manifest` 动词（§4.4 方案 A 的插件侧）**

```rust
    #[test]
    fn manifest_prints_a_self_describing_manifest() {
        // 跑 `manifest` 子命令 → stdout 是可解析的 JSON 对象；
        //   name/switch_key/args 与 supervisors/superpowers-kanban.json 一致；
        //   command 是当前可执行文件的绝对路径（不是裸名）。
    }
```

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner manifest_ -- --nocapture`
Expected: FAIL

实现要点：

- `main.rs` 的 `parse_subcommand` 加 `"manifest"` 臂；`usage` 字符串同步。
- 输出的清单**以二进制为准**：`command` 用 `std::env::current_exe()` 的绝对路径
  （宿主解析到的裸名在 `PATH` 与守护进程的 `PATH` 不一致时会起不来，这正是当年
  `scaffold.rs` 注释里记的那条坑）。
- 其余字段与 `plugins/superpowers-kanban/supervisors/superpowers-kanban.json` 逐字一致
  （`args` 里的 `{runtime_dir}`/`{state_dir}`/`{workdir}` 占位符由 supervisor 展开，
  原样输出即可）。
- 不加新依赖（插件对主程序零依赖；`serde_json` 已有）。

> **为何以二进制自述为准**：`INSTALL.md:46` 只装二进制，源码树里的
> `supervisors/superpowers-kanban.json` 在已安装环境里不可达（spec §4.4 已论证方案 B
> 因此不可行）。随包 `.json` 退化为参考文档；两者漂移时以 `manifest` 输出为准。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner lifecycle_ -- --nocapture`
Expected: FAIL

- [ ] **Step 3: 实现**

- 新增 runner `resident.rs`（最小读写：`require` / `release` / `list`，格式与
  `yi-agent-store::resident` 一致——`{"projects":[{"project":...,"required_by":[...]}]}`，
  原子写 tmp+rename，读侧宽容；注释指明「这是两侧共享的通用文件契约」）。
- `dispatch.rs` 加两个分支；`service.rs` 加 `lifecycle_create(project)` /
  `lifecycle_remove(project)`（复用现有目录创建逻辑与 `plugin_command()`；
  create **不写清单**（§4.4 已裁定 A），只建自己的状态目录 + `require`）。
- `remove` 的「停 daemon」：插件不能直接停宿主的 per-project daemon
  （那是宿主/监督者的事）；改为 `resident::release` + 删自己的状态目录，
  由宿主/监督者观察到登记消失后停进程。**在注释里写明这条分工**，并在测试里断言
  「release 已发生 + 状态目录已删」而不是「进程已停」。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/
git commit -m "feat(kanban): expose lifecycle create/remove over the plugin channel"
```

---

### Task 4b: 宿主通用 `plugin/install`（§4.4 方案 A 的宿主侧）

> **为什么单列一任务**：它是「全新项目建不了看板」的唯一正解（spec §4.4 已裁定 A），
> 且**与 Task 6 的删面互相独立**——Task 6 删 `yi-agent-boards`，本任务把其中
> `scaffold` 的通用能力（写清单 + 开开关）搬到宿主通用侧。先做本任务，
> Task 6 才有地方可迁，否则删完就出现「没人装清单」的空洞。

**Files:**
- Create/Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/install.rs`（通用安装：写清单 + 开开关）
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/plugin.rs`（新增 `plugin/install` 分支）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs`（`PluginChannelState` 用上
  已有的 `plugin_command` 接缝；**不要**再新增一个按名解析的重复实现）
- Test: `yi-agent-supervisors/src/install.rs` 的 `mod tests`；
  `yi-agent-app-server/src/rpc_tests.rs` 的 `plugin_install_*`

**Interfaces:**
- Consumes: `plugin_worktree::resolve_plugin_command(name)`（已存在，`server.rs:243`）、
  `daemon::ensure_running(project)`（已存在）、`yi_agent_store::resident`（不需要）。
- Produces:
  - `yi_agent_supervisors::install::install_manifest_for(project, plugin_name, manifest_json) -> io::Result<PathBuf>`
  - `yi_agent_supervisors::install::enable_switch(project, switch_key) -> io::Result<()>`
  - RPC `plugin/install {project, plugin}` → `{ installed: true, manifest: <path> }`

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn installing_writes_the_manifest_under_the_projects_supervisors_dir() {
        // install_manifest_for(tmp, "demo", json) →
        //   <tmp>/.yi-agent/supervisors/demo.json 内容逐字等于 json。
    }

    #[test]
    fn installing_twice_overwrites_instead_of_duplicating() {
        // 同名两次、内容不同 → 文件只有一个，内容是第二次的。
    }

    #[test]
    fn enabling_the_switch_preserves_unrelated_keys() {
        // preferences.json 已有 {"subagent_runtime":"x"} →
        //   enable_switch(project, "demo_on") 后两键都在，demo_on == true。
    }
```

RPC 层：

```rust
    #[test]
    fn plugin_install_resolves_the_command_and_writes_what_the_plugin_describes() {
        // 临时 PATH 上放一个 shell 脚本冒充插件，`manifest` 打印一份 JSON →
        //   plugin/install 后 <项目>/.yi-agent/supervisors/<plugin>.json 存在且等于那段 JSON，
        //   且 preferences.json 里该清单的 switch_key 为 true。
    }

    #[test]
    fn plugin_install_reports_a_missing_plugin_without_writing_anything() {
        // PATH 上没有该名字 → 结构化错误（plugin_not_installed），
        //   supervisors/ 与 preferences.json 都未被创建/修改。
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors install_ -- --nocapture`
Expected: FAIL（模块不存在）

- [ ] **Step 3: 实现**

- `install.rs`：从已删的 `yi-agent-boards/src/scaffold.rs`（`git show <T6 base>:...`）迁出
  两个纯函数，**去掉所有看板名**：
  - `install_manifest_for(project, plugin_name, manifest_json)`：`create_dir_all`
    `<project>/.yi-agent/supervisors`，写 `<plugin_name>.json`。文件名来自参数，
    不来自清单内容（清单里的 `name` 只用来校验一致，不一致就报错）。
  - `enable_switch(project, switch_key)`：read-modify-write
    `<project>/.yi-agent/preferences.json`，只置 `switch_key: true`，
    其余键原样保留；tmp + rename（照抄 `runtime_prefs::save` 的写法）。
- `rpc/plugin.rs`：`"plugin/install"` 分支：
  1. `project_arg(&req.params)`（已存在的 helper，会 canonicalize）；
  2. `plugin` 名非空；
  3. `resolve_plugin_command(plugin)` → 无则 `plugin_not_installed`；
  4. `Command::new(exe).arg("manifest").output()` → 非 0 或非 JSON 对象则
     `plugin_rejected`（带 stderr 首行）；
  5. 校验 `manifest["name"] == plugin`，否则 `plugin_rejected`；
  6. `install_manifest_for` + `enable_switch(project, manifest["switch_key"])`；
  7. `daemon::ensure_running(project)`（失败只记录、不回滚：清单已装好，
     下一轮 watch 会再拉起）。
- **宿主源码里不得出现具体插件名**：本分支只读 `plugin` 形参，验收 §6.2 的 grep 因此
  不受影响。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors && cargo test -p yi-agent-app-server plugin_install`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-supervisors/src/install.rs \
        yi-agent-rs/crates/yi-agent-supervisors/src/lib.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/rpc/plugin.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "feat(app-server): install a plugin into a project generically"
```

---

### Task 5: 桌面改走通用通道（`plugin/query` + `resident/list`）

**Files:**
- Modify: `desktop/src/lib/superpowersKanbanBoards.ts` + `superpowersKanbanBoards.test.ts`
- Modify: `desktop/src/lib/routes.ts` + `routes.test.ts`
- Modify: `desktop/src/lib/boardIndex.ts`（错误码名同步，见 D11）
- Modify: `desktop/src/hooks/useBoardState.ts`
- Modify: `desktop/src/App.tsx`（调用点逻辑不变）+ `App.test.tsx`（mock 断言同步）

**Interfaces:**
- Consumes: 插件方法 `lifecycle/create` / `lifecycle/remove`（Task 4）；通用
  `plugin/query`；通用 `resident/list`（Task 1）。
- Produces: `createBoard` / `removeBoard` 走 `plugin/query`（`plugin: "superpowers-kanban"`）；
  `listBoards` 走**通用** `resident/list`。

- [ ] **Step 1: 写失败测试**

```ts
it("creates a board through the plugin channel", async () => {
  const rpc = vi.fn().mockResolvedValue({});
  await createBoard(rpc, "/proj");
  expect(rpc).toHaveBeenCalledWith("plugin/query", {
    plugin: "superpowers-kanban",
    project: "/proj",
    method: "lifecycle/create",
    params: { project: "/proj" },
  });
});

it("removes a board through the plugin channel", async () => {
  const rpc = vi.fn().mockResolvedValue({});
  await removeBoard(rpc, "/proj");
  expect(rpc).toHaveBeenCalledWith("plugin/query", {
    plugin: "superpowers-kanban",
    project: "/proj",
    method: "lifecycle/remove",
    params: { project: "/proj" },
  });
});

it("lists registered projects from the generic resident registry", async () => {
  const rpc = vi.fn().mockResolvedValue({ projects: [{ project: "/a" }] });
  expect(await listBoards(rpc)).toEqual(["/a"]);
  expect(rpc).toHaveBeenCalledWith("resident/list", {});
});
```

> `plugin/query` 的 `project` 是**路由目标**（宿主据此拨那个项目的 daemon socket），
> 与 `params.project`（插件据此知道给哪个项目建盘）是两件事，**都要带**——
> 与 `superpowersKanbanSwitch.ts` 的 `queryPlugin` 同形。

- [ ] **Step 2: 跑测试确认失败**

Run: `npx vitest run desktop/src/lib/superpowersKanbanBoards.test.ts`
Expected: FAIL（仍断言 `board/create` 等）

- [ ] **Step 3: 改实现**

```ts
const KANBAN_PLUGIN = "superpowers-kanban";

export async function createBoard(rpc: BoardRpc, project: string): Promise<void> {
  await rpc("plugin/query", {
    plugin: KANBAN_PLUGIN,
    project,
    method: "lifecycle/create",
    params: { project },
  });
}

export async function removeBoard(rpc: BoardRpc, project: string): Promise<void> {
  await rpc("plugin/query", {
    plugin: KANBAN_PLUGIN,
    project,
    method: "lifecycle/remove",
    params: { project },
  });
}
```

- `listBoards` 改调 `RPC.residentList`（`"resident/list"`），保留既有的形状容忍
  （裸数组 / `items` / `projects`），不认识时回空表。
- `routes.ts`：`boardCreate`/`boardRemove`/`boardList` 换成 `residentList: "resident/list"`
  （前两个不再是 RPC 名，故不必留在表里）；`routes.test.ts` 同步。
- `boardIndex.ts` 的 `boardErrorKind`：**删掉 `not_created` 那一支**。Task 1 摘掉门控后，
  宿主不会再发「这个项目没有看板」这个码（`plugin_query` 不再按登记表拒答），
  故「未创建」不再是一个**凭证读到的错误**，而是由 `listBoards` 的结果表达的事实。
  与之配套：`App.tsx` 面板里那个由 `boardError === "not_created"` 触发的「创建看板」
  按钮删掉——创建入口保留侧栏项目右键菜单那一条
  （`ThreadSidebar` 按 `kanbanItemFor(g.workspace, boards)` 决定显示「创建」还是「移除」，
  见 `components/ThreadSidebar.tsx:939`，那条路本来就不依赖这个错误码）。
  `BOARD_ERROR_TEXT` 的 `not_created` 一句与 `App.test.tsx` 里两条以
  `board_not_created` 驱动的用例同步删除/改写。
- 文件头注释更新：生命周期现在**归插件**（受众从「宿主」改为「插件」），
  `plugin/query` 是通用通道、宿主只转发；列举走通用 `resident/list`。
- `App.test.tsx` 的 mock：`board/list` → `resident/list`；`board/create` 断言 →
  `plugin/query`。

- [ ] **Step 4: 跑测试确认通过**

Run: `npx vitest run desktop/src/lib/superpowersKanbanBoards.test.ts desktop/src/lib/routes.test.ts desktop/src/App.test.tsx`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/superpowersKanbanBoards.ts desktop/src/lib/superpowersKanbanBoards.test.ts \
        desktop/src/lib/routes.ts desktop/src/lib/routes.test.ts desktop/src/lib/boardIndex.ts \
        desktop/src/hooks/useBoardState.ts desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "refactor(desktop): create and remove boards through the plugin channel"
```

---

### Task 6: 删除宿主侧的看板面（RPC、crate、偏好键）+ `yi-agent`(TUI/CLI) 收口

**Files:**
- Delete: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/board.rs`；`rpc/mod.rs` 的
  `Domain::Board` 与 `BoardPluginState` 的 `board_dir`/`board_launcher`
- Delete: `yi-agent-rs/crates/yi-agent-boards/`（整 crate）
- Modify: `yi-agent-app-server/src/{server.rs, protocol.rs, settings_store.rs, rpc/ui.rs, ws.rs, lib.rs, Cargo.toml, rpc_tests.rs, thread_store.rs, rpc/thread.rs, worktree_reclaim.rs, theme_tool.rs}`
- Modify: `yi-agent-rs/Cargo.toml`（workspace members）
- Modify: `yi-agent/src/{main.rs, config.rs, tui/board.rs, tui/superpowers_kanban.rs, tui/app.rs, Cargo.toml}`
- Modify: `yi-agent/tests/{board_e2e.rs, board_watchman_e2e.rs}`
- Test: 各 crate 的 `mod tests`

**Interfaces:**
- Consumes: Task 1–5 的替代路径已就位。
- Produces: 宿主与 `yi-agent`(TUI/CLI) 对 `yi-agent-boards` **零依赖**；
  `board/*` RPC 不存在；偏好键通用化
  （`board_watchman_enabled` → `resident_daemon_watchman_enabled`，旧键兼容读）。

- [ ] **Step 1: 写失败测试**

> **实测漂移（刷新 10/12）**：`tui/app.rs` 的 `global_dir()` 调用点在 **`:2751`**（非 2745）；
> `Layout` 去专有名时**保留 `logs_dir()`**（main `080fa293` 新增，托管子进程日志落点）。
> `board_watchman_enabled` 的现存落点：`settings_store.rs:99,109`（经
> `yi_agent_boards::watchman::PREFERENCE_KEY`）、`rpc/ui.rs`（2 处）、`theme_tool.rs:44`、
> `server.rs:6474`、`rpc_tests.rs:13886`；`settings_store.rs` 的读写测试在 `:377,393,784,807`。

```rust
    #[test]
    fn the_legacy_watchman_preference_key_still_reads() {
        // 写 {"board_watchman_enabled": false} → 读通用键时得到 false。
    }

    #[test]
    fn the_generic_watchman_preference_key_wins_over_the_legacy_one() {
        // 两键同时存在时以新键为准。
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server the_legacy_watchman -- --nocapture`
Expected: FAIL

- [ ] **Step 3: 删除与改名**

- 删 `rpc/board.rs` 整文件与 `Domain::Board`（`domain_for` 去掉 `board` 臂、
  `serve()` 的 detach 分支只留 `Plugin`）；`rpc_tests.rs` 的 Board 方法表删掉。
- `protocol.rs`：`board_query` 一族码名去看板化。**`board_not_created` 整支删除**
  （Task 1 已摘门控，宿主不再有「这个项目没有看板」这个回答），其余码位换算为通用名
  （`daemon_unavailable` / `plugin_unavailable` / `plugin_not_installed` /
  `plugin_rejected` 本已通用，保留；数值码 `-32021..-32024` 保持不动）。
  `protocol.rs` 的 `board_query_numbers_each_reason_without_renumbering`（`protocol.rs:723`，
  `board_not_created` 断言在 `:738`）同步更新（随码删除）。
  > **Task 1 已改过的**：`a_query_without_a_registered_board_is_refused_with_its_own_code`
  > 已删、`the_rpc_maps_a_missing_board_to_its_own_code_in_data` 已更名为
  > `the_rpc_reports_a_missing_daemon_in_data_for_an_unregistered_project` 并改断言
  > `daemon_unavailable`（`server.rs:6736`/`:6749` 是 Task 1 新写的门控测试）。**本任务
  > 不要再按初稿的旧测试名去找**。
- `settings_store.rs` + `rpc/ui.rs`：偏好键改名 + 旧键兼容读；`ui/settings/read|write`
  的字段名同步（桌面 Task 5 已改）。实测落点：`settings_store.rs:99,109`（现读
  `yi_agent_boards::watchman::PREFERENCE_KEY`）、`rpc/ui.rs`（2 处）、`theme_tool.rs:44`、
  `server.rs:6474`、`rpc_tests.rs:13886`；`settings_store.rs` 的读写测试在 `:377,393,784,807`。
- `ws.rs`：去 `board_dir` 注入；`plugin_command` 注入按 §4.4 裁定处理。
- `server.rs`：删 `ensure_watchman_installed`（`server.rs:288`，调用点 `rpc/board.rs:54`）
  的看板调用点（或其通用化落点）、`board_dir` 局部量、`board_launcher` 句柄；
  `spawn_board_watchman_loop`（`server.rs:1456`，调用点 `:1496`/`:1581`）更名
  （`spawn_resident_watch_loop`）并改调 `yi_agent_supervisors::watch`。
- **`is_kanban_worktree` 归属裁定**（spec §5.2 三选一，见刷新 13）：
  `thread_store.rs:139`（另见 `:161` 与 `rpc/thread.rs`、`worktree_reclaim.rs`）按物理路径
  `.worktrees/kanban/` 识别插件 worktree。**这是 main `e222f2b3`/`9882faf8` 新引入的既有面**：
  要么保留为通用「插件 worktree 前缀」约定（只中性化命名）、要么改读 §5.5 的通用
  `managed_worktree` 标记、要么由插件侧声明前缀。**不定归属则 Task 7 的 grep 不可能归零**。
  同处的 `board_project`/`card_id` meta **旧键别名**（`thread_store.rs:56-58`）保留兼容读。
- 删 `Cargo.toml` 的 `yi-agent-boards` 依赖（app-server 与 `yi-agent` 两处）、
  `lib.rs` 相关 `use`、workspace members 一行。
- `yi-agent`(TUI/CLI)：`tui/board.rs` 改走插件通道（或改为提示）；
  `tui/superpowers_kanban.rs` 不再读 registry（改读 `resident::list`）；
  `tui/app.rs:2751` 去掉 `global_dir()`；`main.rs`/`config.rs` 的 `resident watch`
  已在 Task 3 落地，此处删残留的 `yi_agent_boards` 引用（`main.rs:81-84` 分发、
  `main.rs:238` `run_boards_watch`、`config.rs:188-201`、CLI 测试 `main.rs:4705`）。
- `tests/board_{e2e,watchman_e2e}.rs`：改断言新通道/新子命令（仍受
  `YI_AGENT_BOARD_E2E` 闸保护）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-app-server && cargo test -p yi-agent && cargo build --workspace`
Expected: PASS；`grep -rn "yi-agent-boards\|yi_agent_boards" yi-agent-rs/crates/ --include=*.rs` 无命中

- [ ] **Step 5: 提交**

```bash
git add -A yi-agent-rs/
git commit -m "refactor(app-server): drop the board crate, RPCs and preference key"
```

---

### Task 7: 端到端验收与文档收口

**Files:**
- Modify: `docs/superpowers/specs/2026-10-01-superpowers-kanban-decoupling-design.md`（补一行「已由 G2 收尾」）
- Modify: `docs/project-management/yi-agent-app-server.md`、`yi-agent-cli.md`、`yi-agent-tui.md`
  （按 AGENTS.md 的规则同步条目与 `README.md` 计数）
- Test: 手工端到端

**Interfaces:**
- Consumes: Task 1–6 全部。
- Produces: 宿主侧 grep 归零的可复核证据。

- [ ] **Step 1: 写验收清单（人可验证）**

- 桌面「新建看板」→ 侧栏出现看板、插件被拉起、队列可推进。
- 桌面「删除看板」→ 队列消失、daemon 停、登记表不再含该项目。
- 旧环境（含 `boards.json`）首启 → 该看板仍出现（迁移生效）。
- 卸载插件 → 桌面不再显示看板条目，且无崩溃。

- [ ] **Step 2: 跑全量测试**

Run: `cargo test --workspace --jobs 2 && npx vitest run`
Expected: 全绿

- [ ] **Step 3: grep 归零核对**

Run（**必须用 `-i` 并排除 `onboarding`**，见刷新 14 / spec §6.2）:
```bash
grep -rni "kanban\|board\|卡片" \
  yi-agent-rs/crates/yi-agent-app-server/src/ \
  yi-agent-rs/crates/yi-agent-supervisors/src/ \
  yi-agent-rs/crates/yi-agent/src/ --include=*.rs \
  | grep -viE "keyboard|clipboard|onboarding|test|//|#\[cfg\(test\)\]"
```
Expected: 无命中（仅允许注释中的历史说明与通用插件机制）。

> 初稿那条命令有两处错（实测）：① 大小写敏感会漏 `Board` / `BoardPluginState` /
> `BoardQueryError` / `BoardLauncher`（**假通过**，实测 48 行）；② 未排除 `onboarding`
> 而 `grep "board"` 命中 `onboarding*`（**永不归零**，仅 `rpc_tests.rs` 就 ≥14 行）。
> 跑之前先落实 Task 6 的 `is_kanban_worktree` 归属裁定（刷新 13）。

- [ ] **Step 4: 更新 Spec 4 状态**

在 `2026-10-01-superpowers-kanban-decoupling-design.md` 的 §5 迁移第 1 条下补一行：
「已由 `2026-10-17-plugin-lifecycle-decoupling-design.md` 落地」。

- [ ] **Step 5: 提交**

```bash
git add docs/
git commit -m "docs: note that the plugin lifecycle decoupling completes spec 4"
```

---

## 自检

- **Spec 覆盖**：§4.2 建/删路径 → Task 4/5；§4.3 迁移（含落点）→ Task 2；
  §4.4 建盘引导 → Task 4 前置裁定；§5.1 删/迁 → Task 3/6；§5.2 宿主 → Task 1/6；
  §5.3 桌面 → Task 5；§5.4 插件 → Task 4；§5.6 CLI/TUI → Task 3/6；
  §6 验收 1–3 → Task 6；4–5 → Task 4；6 → Task 2；7–8 → Task 5；9–10 → Task 7。
- **类型一致**：`import_legacy_projects(projects, legacy_file, resident_dir, requester)`
  在 Task 2 定义与测试中同名同参；`lifecycle/create` / `lifecycle/remove` 在 Task 4/5
  两侧一致；`resident/list` 的形状在 Task 1（产出）/Task 5（消费）一致。
- **依赖裁定**：插件**不依赖** `yi-agent-store`（保持零依赖），resident 文件格式作为
  两侧共享的通用契约，各自实现并在测试中覆盖——这是本 plan 唯一需要评审认同的取舍。
- **顺序依赖**：Task 1 → 2 → 3 → 4 → **4b** → 5 → 6 → 7；Task 1 是 Task 5/6 的前提
  （`resident/list` 先于删 `board/list`）；Task 3 的 plist/CLI 与 Task 6 的 crate 删除
  必须分两次落地（先迁后删），但 plist 的子命令字符串不允许中间态指向不存在的命令。
- **刷新（第二次，main +98 提交后）**：Task 1 已完成；Task 3/6/7 的行号与引用已按
  `bf6e1971` 实测更新；验收 grep 命令已修正（`-i` + 排 `onboarding`）；
  新引入的 `is_kanban_worktree` 需先定归属（Task 6）。详见顶部「第二次刷新说明」。
- **§4.4 已裁定（2026-10-19）**：取方案 A。理由不是偏好，而是约束排除：
  C 违反验收 §6.9「与今日行为一致」；B 在「只装二进制」的安装方式下不可实现
  （清单留在源码树，二进制与它无可推导关系）。故新增 **Task 4b**（宿主通用
  `plugin/install`）与 Task 4 的 **Step 1b**（插件 `manifest` 动词）。
- **§4.4.1 开关（初稿遗漏）**：`create_with_project` 当年写清单**和**开关；
  开关默认关且门控插件推进循环，故 `plugin/install` 必须一并 `enable_switch`，
  否则「看板建了、队列永不推进」，违反 §6.9。已在 Task 4b 与验收 §6.3a/3b 落项。
