# 插件生命周期去耦合（宿主不认识「看板」这个概念）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 删除宿主里的看板专属 crate、登记表与 RPC——「建/删看板」改走通用插件通道，「哪些项目要 daemon」以通用 `resident` 登记为唯一来源，宿主侧不再出现「看板」这个概念。

**Architecture:** 复用 Spec 4 早已设计的通用面：`SupervisorManifest`（通用清单）+ `yi_agent_store::resident`（通用常驻登记，字段只有 `project`/`required_by`）+ `IpcRequest::PluginQuery`（通用转发）。看板专属的 `yi-agent-boards` crate（registry/scaffold/lifecycle）被拆除：通用部分（watch/launchd/daemon 生命周期）迁入 `yi-agent-supervisors`，看板专属部分删除并由插件自持（插件经 `plugin_query` 暴露 `lifecycle/*`）。桌面端从 `board/*` RPC 迁到 `plugin/query`。

**Tech Stack:** Rust（`yi-agent-boards` / `yi-agent-supervisors` / `yi-agent-app-server` / `yi-agent-store`；serde、launchd plist）；桌面 TypeScript（vitest）。

**Spec:** `docs/superpowers/specs/2026-10-17-plugin-lifecycle-decoupling-design.md`
**前置：** G1（`2026-10-17-session-driver-decoupling.md`）**先落地**——G1 已删 `card_scheduler` 与 `board_query`，本 plan 再删 `yi-agent-boards` 与 `board/*` RPC，避免两处并行改同一批文件。

## G1 遗留的必须收口项（G2 承接，勿漏）

G1 把「宿主不认识看板」的**决策面**清零，但**留下两处归属缺口**，只有 G2 能收口（G1 的 plan「已知的 G1→G2 过渡缺口」、`docs/bug-list.md` 的对应条与 spec §7 已记录；此处再点明，避免只看本 plan 的 G2 实现者漏做）：

1. **被驱动会话的 `driver_project` / `driver_ref` 恒为 `None`**（`server.rs` 的 `launch_inner(workdir, title, objective, None, None)`）。两条活消费者因此走错分支：
   - **侧栏不折组**：`thread/listAll` 只在 `driver_project.is_some()` 时把会话折进项目组；被驱动会话落进「本目录自己的会话」桶，以各自 cwd（卡 worktree / 合并会话的项目根）为组键成**未分组顶层项**。
   - **worktree 永不回收**：`worktree_reclaim::decide` 在 `driver_project` 为 `None` 时判 `Keep`。
   - **G2 修法**：引入 spec §5/§9 的通用字段 `managed_worktree`（会话 meta 的可选通用字段），由**插件侧在 `driver/next` 的动作里填值**；`worktree_reclaim::decide` 改为从 worktree 自身读分支、不再靠 `driver_ref` 推 `kanban/<slug>`；侧栏按 `managed_worktree` 归组。注意：G1 的宿主测试 `a_launch_action_records_no_driver_origin_and_still_starts_a_turn` 现断言 `driver_project == None`，G2 落地时需一并更新该断言。
2. **宿主重启后卡滞留 + `Merging` 卡持有合并闸**：G1 删了 `recover_orphan_cards`，且 `DriverState` 是宿主内存，重启后插件收到空 `threads`，`Running`/`Launching`/`Merging` 卡无人结算；停在 `Merging` 的卡会让 `any_merging()` 恒真、该项目再无合并启动。**G2 必须确认收口**：插件侧应对「宿主未上报且无在途动作」的卡做保守恢复（结算为 `needs_you` 并释放合并闸），G2 需验证该恢复在生命周期改造后仍生效，并把仍无主的边界写进验收。

## Global Constraints

- 宿主非测试代码中不得出现看板语义：`kanban` / `board` / `卡片`（通用插件机制除外）。
- 不改 `SupervisorManifest` 既有字段语义；只新增通用可选字段。
- 迁移必须**幂等**，且保留旧文件取证（改名而非删除）。
- 端到端行为等价：桌面上仍能建/删看板、看板仍被拉起与监督。
- 旧偏好键 `board_watchman_enabled` **读时兼容**，写时写新键。
- 命令一律在仓库根执行；测试用 `cargo test -p <crate> <filter>` / `npx vitest run <file>`。

---

### Task 1: 通用 resident 登记成为「要 daemon 的项目」唯一来源

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-boards/src/watch.rs`（改读 resident，已是）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`board/list` 改读 resident）
- Test: `server.rs` 的 `mod tests`

**Interfaces:**
- Consumes: `yi_agent_store::resident::{list, require, release, default_dir, registry_path}`（已存在）。
- Produces: 宿主 `board/list` 的**临时**实现改读 `resident::list(default_dir)`（字段仍叫 `boards`
  以免桌面立刻破；Task 5 再改通道）。

- [ ] **Step 1: 写失败测试**

```rust
    /// board/list 的答案来自通用 resident 登记，而不是看板专属的 boards.json。
    #[tokio::test]
    async fn board_list_reports_projects_from_the_generic_resident_registry() {
        // 搭台：在 resident registry 里 require 一个项目；
        // 断言 board/list 的返回含该项目。
        // 反向断言：不写 boards.json 也能列出（证明不再依赖它）。
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server board_list_reports -- --nocapture`
Expected: FAIL（仍读 `boards.json`）

- [ ] **Step 3: 改实现**

`server.rs` 的 `board/list` 分支：把 `yi_agent_boards::registry::list(&board_dir)` 换成
`yi_agent_store::resident::list(&resident_dir)`（`resident_dir` 已在该作用域，G1 后仍可用）。
`status_with` 的调用保留（状态仍来自插件）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-app-server`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "refactor(app-server): list registered projects from the generic resident registry"
```

---

### Task 2: 迁移旧 `boards.json` → resident（一次性、幂等）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-store/src/resident_migrate.rs`（或置于现有 `resident.rs`）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/lib.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: `resident::require`、`yi_agent_boards::registry::list`（旧格式读取器；本任务尚未删）。
- Produces: `pub fn import_legacy_boards(registry_dir: &Path, resident_dir: &Path, requester: &str) -> std::io::Result<usize>`
  —— 逐个 `require`，成功后把 `<registry_dir>/boards.json` 改名为 `boards.json.migrated`，
  返回导入条数；`boards.json` 不存在 → 返回 `0` 且不做任何事。

> 依赖方向注意：`resident` 在 `yi-agent-store`，旧格式读取器在 `yi-agent-boards`，而
> `yi-agent-boards` 依赖 `yi-agent-store`。**不要把 `yi-agent-boards` 拉进 store**：
> 迁移函数接受「已解析出的项目路径列表」作为参数，由调用方（app-server，两者都能依赖）读取旧文件。

修正后的签名：

```rust
/// 把旧的看板登记表内容导入通用 resident 登记，并把旧文件改名取证。
/// `projects` 由调用方从旧格式解析（store 不认识看板格式）。
pub fn import_legacy_projects(
    projects: &[std::path::PathBuf],
    legacy_file: &Path,
    resident_dir: &Path,
    requester: &str,
) -> std::io::Result<usize>;
```

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

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-store import_legacy -- --nocapture`
Expected: FAIL

- [ ] **Step 3: 写实现**

```rust
/// 把旧的看板登记内容导入通用 resident 登记，并把旧文件改名取证。
///
/// `projects` 由调用方从旧格式解析：store 层保持通用，不认识看板格式。
pub fn import_legacy_projects(
    projects: &[PathBuf],
    legacy_file: &Path,
    resident_dir: &Path,
    requester: &str,
) -> std::io::Result<usize> {
    if !legacy_file.exists() {
        return Ok(0);
    }
    let mut imported = 0;
    for project in projects {
        resident::require(resident_dir, project, requester)?;
        imported += 1;
    }
    let migrated = legacy_file.with_extension("json.migrated");
    std::fs::rename(legacy_file, migrated)?;
    Ok(imported)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-store`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-store/src/
git commit -m "feat(store): import legacy board registrations into the resident registry"
```

---

### Task 3: 把通用部分迁出 `yi-agent-boards`

**Files:**
- Move: `yi-agent-boards/src/{watch.rs, watchman.rs, board_daemon.rs}` → `yi-agent-supervisors/src/`
- Modify: 两个 crate 的 `lib.rs` / `Cargo.toml`
- Test: 迁移后的 `mod tests` 原样保留

**Interfaces:**
- Consumes: 无（纯搬迁 + 改名）。
- Produces: `yi-agent-supervisors::watch`（`ensure_daemons_with` / `once`）、
  `yi-agent-supervisors::watchman`（`install` / `uninstall` / `is_installed_for`，plist 名改为
  `resident-daemon-watchman`）、`yi-agent-supervisors::daemon`（`spawn_detached` / `wait_ready` /
  `is_running` / `stop`，路径假设去 board 名）。

- [ ] **Step 1: 写失败测试**

```rust
    // 迁入后，yi-agent-supervisors 的测试套里应包含原来的 watchman 测试。
    #[test]
    fn the_watchman_plist_is_generic_not_board_specific() {
        let contents = watchman::plist_contents(Path::new("/bin/yi-agent"), Path::new("/home/u"));
        assert!(contents.contains("resident-daemon-watchman"));
        assert!(!contents.to_lowercase().contains("board"));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-supervisors the_watchman_plist -- --nocapture`
Expected: FAIL（模块尚不存在）

- [ ] **Step 3: 搬迁**

- `git mv` 三文件；改 `lib.rs` 导出；改 `Cargo.toml` 依赖（supervisors 需要 `yi-agent-store`）。
- plist 标签与文件名去 board 名；`board_daemon.rs` → `daemon.rs`，函数名不变。
- 保留 `yi-agent-boards` 里对它们的 `pub use`（过渡），Task 6 再删。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-supervisors -p yi-agent-boards`
Expected: PASS（两 crate 全绿）

- [ ] **Step 5: 提交**

```bash
git add -A yi-agent-rs/crates/yi-agent-supervisors yi-agent-rs/crates/yi-agent-boards yi-agent-rs/Cargo.toml
git commit -m "refactor(supervisors): move the generic resident-daemon machinery out of the boards crate"
```

---

### Task 4: 插件实现 `lifecycle/create` 与 `lifecycle/remove`

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
- Test: 两文件 `mod tests`

**Interfaces:**
- Consumes: `yi_agent_store::resident::{require, release}`（插件已依赖 store？若不依赖，改为由
  dispatch 通过 `IpcRequest::PluginQuery` 反向不可行——**改用插件直接写文件**：
  resident 是**通用文件格式**，插件按同一格式读写该 JSON。若已有 `superpowers-kanban-ipc`
  的路径工具，复用；否则在 runner 里加一个最小读写器）。
- Produces: dispatch 方法 `"lifecycle/create"` / `"lifecycle/remove"`，参数 `{project}`。

> 依赖裁定（写进代码注释）：**插件不依赖 `yi-agent-store`**（保持「插件对主程序零依赖」，
> 见 Spec 4 §2「插件对主程序：零依赖（已达标）」）。resident 登记文件的格式（`project` +
> `required_by`）是**通用契约**，插件按格式自行读写，并在测试里覆盖。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn lifecycle_create_registers_the_project_and_writes_a_manifest() {
        // 临时 HOME：dispatch lifecycle/create {project} →
        //   - resident 登记出现该项目（required_by 含 superpowers-kanban）；
        //   - <project>/.yi-agent/supervisors/superpowers-kanban.json 存在。
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

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p superpowers-kanban-runner lifecycle_ -- --nocapture`
Expected: FAIL

- [ ] **Step 3: 实现**

- runner 新增 `resident.rs`（最小读写：`require` / `release` / `list`，格式与
  `yi-agent-store::resident` 一致，注释指明「这是两侧共享的通用文件契约」）。
- `dispatch.rs` 加两个分支；`service.rs` 加 `lifecycle_create(project)` / `lifecycle_remove(project)`
  （复用现有 `ensure_worktree` 之外的目录创建逻辑与 `plugin_command()`）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p superpowers-kanban-runner`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/
git commit -m "feat(kanban): expose lifecycle create/remove over the plugin channel"
```

---

### Task 5: 桌面改走 `plugin/query`

**Files:**
- Modify: `desktop/src/lib/superpowersKanbanBoards.ts`
- Modify: `desktop/src/lib/superpowersKanbanBoards.test.ts`
- Modify: `desktop/src/App.tsx`（调用点如无变化则不动）

**Interfaces:**
- Consumes: 插件方法 `lifecycle/create` / `lifecycle/remove`（Task 4）；通用 `plugin/query` RPC。
- Produces: `createBoard` / `removeBoard` 走 `plugin/query`（`plugin: "superpowers-kanban"`）；
  `listBoards` 改读**通用**登记（宿主 `board/list` 在 Task 1 已改读 resident，故此处可不动，
  或改调通用 `resident/list`——二选一，写进注释说明选择）。

- [ ] **Step 1: 写失败测试**

```ts
it("creates a board through the plugin channel", async () => {
  const rpc = vi.fn().mockResolvedValue({});
  await createBoard(rpc, "/proj");
  expect(rpc).toHaveBeenCalledWith("plugin/query", {
    plugin: "superpowers-kanban",
    method: "lifecycle/create",
    params: { project: "/proj" },
  });
});

it("removes a board through the plugin channel", async () => {
  const rpc = vi.fn().mockResolvedValue({});
  await removeBoard(rpc, "/proj");
  expect(rpc).toHaveBeenCalledWith("plugin/query", {
    plugin: "superpowers-kanban",
    method: "lifecycle/remove",
    params: { project: "/proj" },
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `npx vitest run desktop/src/lib/superpowersKanbanBoards.test.ts`
Expected: FAIL（仍断言 `board/create`）

- [ ] **Step 3: 改实现**

```ts
const KANBAN_PLUGIN = "superpowers-kanban";

export async function createBoard(rpc: BoardRpc, project: string): Promise<void> {
  await rpc("plugin/query", {
    plugin: KANBAN_PLUGIN,
    method: "lifecycle/create",
    params: { project },
  });
}

export async function removeBoard(rpc: BoardRpc, project: string): Promise<void> {
  await rpc("plugin/query", {
    plugin: KANBAN_PLUGIN,
    method: "lifecycle/remove",
    params: { project },
  });
}
```

更新文件头注释：生命周期现在**归插件**（受众从「宿主」改为「插件」），并说明
`plugin/query` 是通用通道、宿主只转发。

- [ ] **Step 4: 跑测试确认通过**

Run: `npx vitest run desktop/src/lib/superpowersKanbanBoards.test.ts`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/superpowersKanbanBoards.ts desktop/src/lib/superpowersKanbanBoards.test.ts
git commit -m "refactor(desktop): create and remove boards through the plugin channel"
```

---

### Task 6: 删除宿主侧的看板面（RPC、crate、偏好键）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/{server.rs, settings_store.rs, ws.rs, lib.rs, Cargo.toml}`
- Delete: `yi-agent-rs/crates/yi-agent-boards/`（整 crate）
- Delete: `server.rs` 的 `board/create` / `board/remove` / `board/list` 分支
- Test: 各 crate 的 `mod tests`

**Interfaces:**
- Consumes: Task 1–5 的替代路径已就位。
- Produces: 宿主对 `yi-agent-boards` **零依赖**；三个 `board/*` RPC 不存在；
  偏好键通用化（`board_watchman_enabled` → `resident_daemon_watchman_enabled`，旧键兼容读）。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn the_legacy_watchman_preference_key_still_reads() {
        // 写 {"board_watchman_enabled": false} → 读通用键时得到 false。
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-app-server the_legacy_watchman -- --nocapture`
Expected: FAIL

- [ ] **Step 3: 删除与改名**

- 删三个 `board/*` 分支；`board_dir` 相关局部量去掉。
- `settings_store.rs`：偏好键改名 + 旧键兼容读。
- `ws.rs`：去 `board_dir` 注入。
- 删 `Cargo.toml` 的 `yi-agent-boards` 依赖、`lib.rs` 相关 `use`。
- 删整个 `yi-agent-boards` crate（其通用部分已在 Task 3 迁出；`scaffold.rs` 的清单写出能力
  若 Task 4 已在插件侧实现，则一并删除）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-app-server && cargo build --workspace`
Expected: PASS；`grep -rn "yi-agent-boards" yi-agent-rs/` 无命中

- [ ] **Step 5: 提交**

```bash
git add -A yi-agent-rs/
git commit -m "refactor(app-server): drop the board crate, RPCs and preference key"
```

---

### Task 7: 端到端验收与文档收口

**Files:**
- Modify: `docs/superpowers/specs/2026-10-01-superpowers-kanban-decoupling-design.md`（补一行「已由 G2 收尾」）
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

Run: `cargo test --workspace && npx vitest run`
Expected: 全绿

- [ ] **Step 3: grep 归零核对**

Run:
```bash
grep -rn "kanban\|board" yi-agent-rs/crates/yi-agent-app-server/src/ yi-agent-rs/crates/yi-agent-supervisors/src/ --include=*.rs \
  | grep -viE "keyboard|clipboard|test|//" | head
```
Expected: 无命中（仅允许注释中的历史说明与通用插件机制）

- [ ] **Step 4: 更新 Spec 4 状态**

在 `2026-10-01-superpowers-kanban-decoupling-design.md` 的 §5 迁移第 1 条下补一行：
「已由 `2026-10-17-plugin-lifecycle-decoupling-design.md` 落地」。

- [ ] **Step 5: 提交**

```bash
git add docs/superpowers/specs/2026-10-01-superpowers-kanban-decoupling-design.md
git commit -m "docs: note that the plugin lifecycle decoupling completes spec 4"
```

---

## 自检

- **Spec 覆盖**：§4.2 建/删路径 → Task 4/5；§4.3 迁移 → Task 2；§5.1 删/迁 → Task 3/6；
  §5.2 宿主 → Task 1/6；§5.3 桌面 → Task 5；§5.4 插件 → Task 4；§6 验收 1–3 → Task 6；
  4–5 → Task 4；6 → Task 2；7–8 → Task 5；9–10 → Task 7。
- **类型一致**：`import_legacy_projects(projects, legacy_file, resident_dir, requester)` 在
  Task 2 定义与测试中同名同参；`lifecycle/create` / `lifecycle/remove` 在 Task 4/5 两侧一致。
- **依赖裁定**：插件**不依赖** `yi-agent-store`（保持零依赖），resident 文件格式作为两侧
  共享的通用契约，各自实现并在测试中覆盖——这是本 plan 唯一需要评审认同的取舍。
- **顺序依赖**：G1 → Task 1 → 2 → 3 → 4 → 5 → 6 → 7；Task 1 是 Task 6 删 `board/list` 的前提。
