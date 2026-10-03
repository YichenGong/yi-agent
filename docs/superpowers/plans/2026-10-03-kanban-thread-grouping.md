# 看板会话归拢到所属项目（可折叠小节）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让由看板起的卡片会话在侧栏里归到其所属项目组下（可折叠展开），不再各自成为一个顶层项目。

**Architecture:** 分三层。①**归属落盘**：调度器起卡片会话时，把「项目根 + card_id」写进该会话的 `ThreadMeta`（宿主所有，与插件解耦）。②**分组归拢**：`thread/listAll` 把带 `board_project` 的会话折进项目组，纯卡片工作树目录不再作为顶层组；工作区索引**不动**（冷会话 resume 靠它定位 store）。③**侧栏小节**：项目组内加一个默认折叠的「看板会话」小节，只收 `card_id` 非空的会话。

**Tech Stack:** Rust（`yi-agent-app-server`：`server.rs`/`thread_store.rs`/`card_scheduler.rs`、tokio、serde_json）；桌面 Tauri+React（TypeScript、`ThreadSidebar.tsx`、vitest + @testing-library/react）。

**Spec:** `docs/superpowers/specs/2026-10-03-kanban-thread-grouping-design.md`

## Global Constraints

- 行号以基线 `928293a` 为准；改前先 `grep` 复核（main 会前移）。
- **不动工作区索引的写入**：卡片工作树 cwd 继续进 `WorkspaceIndex`（`server.rs:5285`）——`find_thread_dir`（`server.rs:4854`）/`store_for`（`server.rs:4863`）/`store_lookup`（`server.rs:4874`）靠它定位冷会话 store。本计划只改**分组呈现**。
- 两个新字段一律 `#[serde(default)]`，旧 `meta.json` 反序列化回 `None`（与既有 `permission_mode`/`pin_seq` 同款）。
- 普通会话（`board_project = None`）行为与今天**逐字一致**（分组、顺序、`exists` 语义零回归）。
- 命名统一：字段 `board_project` / `card_id`；折叠键 `board:${workspace}`。
- 测试不得读写真实 `$HOME`；Rust 测试用 `tempfile`，桌面测试用内存数据。
- Commit 前 `cd yi-agent-rs && cargo fmt --all`；只显式 `git add <path>`，绝不 `git add -A`。
- 运行环境：`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`、`export TMPDIR=/Users/gongyichen/.yi-agent-tmp`、`cargo --offline`；桌面 `export PATH="/opt/homebrew/bin:$PATH"`。
- 工作区：本计划在独立 worktree 里执行（`superpowers:using-git-worktrees`），不直接改 `main`。

---

## File Structure

- `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` — `ThreadMeta` 增 `board_project`/`card_id`。
- `yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs` — `LaunchRequest` 增 `board_project`；`run_once` 填充。
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` — `start_thread_core` 写归属；`launch_inner`/`thread/start` 传参；`thread_summary_json` 带出；`thread/listAll` 归组。
- `desktop/src/lib/protocol.ts` — `ThreadSummary` 增两字段。
- `desktop/src/components/ThreadSidebar.tsx` — 可折叠「看板会话」小节。

---

### Task 1: 归属落盘并贯通到 wire

把「会话由看板创建」的事实写进 `ThreadMeta`，并让它出现在 `thread/listAll` 的每个条目里。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（`ThreadMeta` 结构体 + 其全部构造点 + 测试）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs`（`LaunchRequest` + `run_once` + 测试）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`start_thread_core`、`launch_inner`、`thread/start`、`thread_summary_json`、测试助手）
- Modify: `desktop/src/lib/protocol.ts`（`ThreadSummary`）

**Interfaces:**
- Produces:
  - `ThreadMeta.board_project: Option<String>`、`ThreadMeta.card_id: Option<String>`。
  - `LaunchRequest.board_project: String`（`card_scheduler.rs`）；`run_once` 以 **canonical** 项目根填充。
  - `start_thread_core(..., board_project: Option<&str>, card_id: Option<&str>)`。
  - `thread/listAll` 条目新增 `"board_project"` / `"card_id"`（可为 `null`）。

- [ ] **Step 1: 写失败测试（ThreadMeta 默认值与往返）**

在 `thread_store.rs` 测试模块（文件末尾 `mod tests` 内）追加：

```rust
    #[test]
    fn a_meta_without_board_fields_defaults_to_none() {
        let json = r#"{"thread_id":"t1","cwd":"/w","model":"m","created_at":1,"updated_at":2,"title":null}"#;
        let meta: ThreadMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.board_project, None, "旧 meta 缺字段必须回 None");
        assert_eq!(meta.card_id, None);
    }

    #[test]
    fn board_fields_round_trip() {
        let meta = ThreadMeta {
            thread_id: "t1".into(),
            cwd: "/w".into(),
            model: "m".into(),
            created_at: 1,
            updated_at: 2,
            title: None,
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            board_project: Some("/proj".into()),
            card_id: Some("c1".into()),
        };
        let text = serde_json::to_string(&meta).unwrap();
        let back: ThreadMeta = serde_json::from_str(&text).unwrap();
        assert_eq!(back.board_project.as_deref(), Some("/proj"));
        assert_eq!(back.card_id.as_deref(), Some("c1"));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-app-server --lib thread_store::tests::a_meta_without_board_fields_defaults_to_none thread_store::tests::board_fields_round_trip`
Expected: 编译失败，`no field board_project on type ThreadMeta`。

- [ ] **Step 3: 给 ThreadMeta 加字段并更新全部构造点**

在 `thread_store.rs::ThreadMeta` 的 `pin_seq` 之后追加：

```rust
    /// 该会话由看板创建时，它所属的项目根（绝对路径，canonical）。None = 普通会话。
    #[serde(default)]
    pub board_project: Option<String>,
    /// 该会话对应的看板卡 id。None = 普通会话。
    #[serde(default)]
    pub card_id: Option<String>,
```

然后给**每一个** `ThreadMeta { ... }` 字面量补上 `board_project: None, card_id: None,`（`grep -n "ThreadMeta {" yi-agent-rs/crates/yi-agent-app-server/src/` 会列出全部）：
- `thread_store.rs` 的 `rebuild_meta`（约 :476）、测试 `meta()`（约 :500）、测试 `bad_meta`（约 :634）；
- `server.rs` 的 `start_thread_core`（约 :5267）——此处的两个值由 Step 4 的参数决定，先写 `None`，Step 4 再改。

- [ ] **Step 4: 让 start_thread_core 接收并写入归属**

在 `server.rs::start_thread_core`（约 :5212）签名末尾（`workspaces: &WorkspaceIndex,` 之后）加两个参数：

```rust
    board_project: Option<&str>,
    card_id: Option<&str>,
```

在构造 `meta` 处（约 :5267）把 Step 3 写下的 `None` 改为：

```rust
        board_project: board_project.map(str::to_string),
        card_id: card_id.map(str::to_string),
```

两个调用点：
- `thread/start`（约 :2457）：在 `workspaces` 实参之后追加 `None, None,`。
- `ServeLauncher::launch_inner`（约 :5056）：传入本次请求的归属，追加：

```rust
            Some(&request.board_project),
            Some(&request.card_id),
```

- [ ] **Step 5: LaunchRequest 增字段并在 run_once 填充**

在 `card_scheduler.rs::LaunchRequest`（约 :55）加：

```rust
    /// 发起这张卡的项目根（canonical 绝对路径）。落进卡片会话的 meta。
    pub board_project: String,
```

在 `run_once`（构造 `LaunchRequest` 处，约 :145）填入 **canonical** 项目根：

```rust
        let board_project = std::fs::canonicalize(project)
            .unwrap_or_else(|_| project.to_path_buf())
            .to_string_lossy()
            .into_owned();
        let request = LaunchRequest {
            card_id: card_id.clone(),
            board_project,
            workdir,
            title,
            objective,
        };
```

更新同文件测试助手 `FakeLauncher` 的调用点与断言：`the_scheduler_launches_a_claimed_card_and_reports_it_running`（约 :396）在断言块加：

```rust
        let expected_project = std::fs::canonicalize(&board.project)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            request.board_project, expected_project,
            "the launch must carry the project root for grouping"
        );
```

- [ ] **Step 6: 更新 server.rs 测试助手 `request()` 并让 thread_summary_json 带出字段**

`server.rs` 测试助手 `request()`（约 :6364）补字段：

```rust
            board_project: "/test/project".to_string(),
```

`thread_summary_json`（约 :4173）在 `json!` 里追加：

```rust
        "board_project": m.board_project,
        "card_id": m.card_id,
```

- [ ] **Step 7: 写失败测试（起卡片会话后 meta 与 wire 都带归属）**

在 `server.rs` 测试模块追加（用既有 `FakeDaemon`/`Host`/`build_test_agent`，参照 `a_claimed_card_becomes_a_visible_session_then_awaits_merge`）：

```rust
    /// 一张卡被起成会话后：它落盘的 meta 与 `thread/listAll` 的条目都必须带
    /// `board_project`(项目根) 与 `card_id`——这是侧栏归组的唯一依据。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_launched_card_records_its_board_origin() {
        let workdir_dir = tempfile::TempDir::new().unwrap();
        let workdir = workdir_dir.path().canonicalize().unwrap();
        let handed = Arc::new(AtomicBool::new(false));
        let work_path = workdir.to_string_lossy().to_string();
        let daemon = FakeDaemon::bind(move |_project, method, _params| match method {
            "board.next_launch" => {
                if handed.swap(true, Ordering::SeqCst) {
                    Ok(serde_json::Value::Null)
                } else {
                    Ok(json!({
                        "card_id": "card-1",
                        "workdir": work_path,
                        "title": "看板 · card-1"
                    }))
                }
            }
            "list" => Ok(json!({ "cards": [{
                "id": "card-1", "state": "running",
                "spec_path": "card-1.spec.md", "plan_path": "card-1.plan.md"
            }] })),
            _ => Ok(json!({ "ok": true })),
        });

        let mut host = Host::new();
        let board_dir = tempfile::TempDir::new().unwrap();
        yi_agent_boards::registry::register(board_dir.path(), &daemon.project).unwrap();
        let flags = Arc::new(StdMutex::new(HashMap::<String, ThreadFlags>::new()));
        let mut tracked: HashMap<String, TrackedThread> = HashMap::new();
        {
            let snapshot = Arc::clone(&flags);
            let flags_fn = move |id: &str| snapshot.lock().unwrap().get(id).copied();
            let mut launcher = host.launcher(&build_test_agent);
            crate::card_scheduler::run_once(
                &daemon.project, board_dir.path(), &mut tracked, &mut launcher, &flags_fn,
            )
            .await;
        }

        // meta 落盘：归属写进了卡片会话自己的 meta.json。
        let metas = crate::thread_store::ThreadStore::new(&workdir).list().unwrap();
        assert_eq!(metas.len(), 1, "the card session persists exactly one meta");
        let expected_project = std::fs::canonicalize(&daemon.project)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            metas[0].board_project.as_deref(),
            Some(expected_project.as_str()),
            "meta must carry the project root"
        );
        assert_eq!(metas[0].card_id.as_deref(), Some("card-1"));
    }
```

- [ ] **Step 8: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-app-server --lib thread_store::tests a_launched_card_records_its_board_origin the_scheduler_launches_a_claimed_card_and_reports_it_running`
Expected: 全部 PASS。

- [ ] **Step 9: 桌面协议加字段**

`desktop/src/lib/protocol.ts::ThreadSummary` 在 `pinned?` 之后追加：

```typescript
  /** 由看板创建时，该会话所属的项目根（绝对路径）。旧服务端缺省视为普通会话。 */
  board_project?: string;
  /** 由看板创建时，该会话对应的卡 id。旧服务端缺省视为普通会话。 */
  card_id?: string;
```

- [ ] **Step 10: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/server.rs \
        desktop/src/lib/protocol.ts
git commit -m "feat(app-server): stamp board origin onto card threads"
```

---

### Task 2: thread/listAll 把卡片会话折进项目组

让带 `board_project` 的会话进入其项目组；纯卡片工作树目录不再作为顶层组；普通会话零回归。**不动工作区索引**。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`thread/listAll` 处理器，约 :2390）；测试模块。

**Interfaces:**
- Consumes: Task 1 的 `ThreadMeta.board_project`。
- Produces: `thread/listAll` 的 `groups[]` 语义——每个 `board_project` 只出现一次，其卡片会话在其 `threads` 里；只含卡片会话的目录不产出顶层组。

- [ ] **Step 1: 写失败测试（折叠 + 抑制 + 零回归）**

在 `server.rs` 测试模块追加（`h.read_value()` 循环参照既有 `thread_list_all_groups_by_workspace`）：

```rust
    async fn list_all(h: &mut Harness) -> serde_json::Value {
        h.send(r#"{"jsonrpc":"2.0","id":901,"method":"thread/listAll","params":{}}"#).await;
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(901)) {
                return v;
            }
        }
        panic!("thread/listAll must respond");
    }

    async fn add_workspace(h: &mut Harness, id: u64, path: &str) {
        h.send(&serde_json::json!({"jsonrpc":"2.0","id":id,"method":"workspace/add",
            "params":{"path": path}}).to_string()).await;
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(id)) {
                return;
            }
        }
        panic!("workspace/add must respond");
    }

    fn write_meta(dir: &Path, id: &str, title: &str, board_project: Option<&str>, card_id: Option<&str>) {
        let meta = crate::thread_store::ThreadMeta {
            thread_id: id.into(),
            cwd: dir.to_string_lossy().into(),
            model: "m".into(),
            created_at: 0,
            updated_at: 0,
            title: Some(title.into()),
            permission_mode: crate::thread_store::ThreadMode::Normal,
            pin_seq: None,
            board_project: board_project.map(str::to_string),
            card_id: card_id.map(str::to_string),
        };
        crate::thread_store::ThreadStore::new(dir).create(&meta).unwrap();
    }

    /// 卡片会话(在 worktree、board_project=项目) 折进项目组；worktree 不再顶层成组。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_board_thread_folds_into_its_project_group() {
        let project_dir = tempfile::TempDir::new().unwrap();
        let worktree_dir = tempfile::TempDir::new().unwrap();
        let project = project_dir.path().canonicalize().unwrap();
        let worktree = worktree_dir.path().canonicalize().unwrap();
        let p = project.to_string_lossy().to_string();
        let w = worktree.to_string_lossy().to_string();

        write_meta(&project, "t-plain", "plain", None, None);
        write_meta(&worktree, "t-card", "看板 · card-1", Some(&p), Some("card-1"));

        let mut h = Harness::new();
        initialize(&mut h).await;
        add_workspace(&mut h, 11, &w).await; // worktree 进索引(=卡片会话落盘后的样子)
        add_workspace(&mut h, 12, &p).await;

        let v = list_all(&mut h).await;
        let groups = v["result"]["groups"].as_array().unwrap();
        let ws: Vec<&str> = groups.iter().map(|g| g["workspace"].as_str().unwrap()).collect();
        assert!(ws.contains(&p.as_str()), "the project group must exist: {ws:?}");
        assert!(!ws.contains(&w.as_str()), "the worktree must not be a top-level group: {ws:?}");

        let project_group = groups.iter().find(|g| g["workspace"] == p.as_str()).unwrap();
        let ids: Vec<&str> = project_group["threads"].as_array().unwrap()
            .iter().map(|t| t["thread_id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"t-plain"), "own thread stays: {ids:?}");
        assert!(ids.contains(&"t-card"), "card thread folds in: {ids:?}");
        let card = project_group["threads"].as_array().unwrap()
            .iter().find(|t| t["thread_id"] == "t-card").unwrap();
        assert_eq!(card["card_id"], "card-1", "wire must expose card_id");
        assert_eq!(card["board_project"], p.as_str());
    }

    /// 零回归:一个普通目录的会话仍按目录成组,顺序/存在性与今天一致。
    #[tokio::test(flavor = "multi_thread")]
    async fn plain_workspaces_still_group_by_directory() {
        let dir_a = tempfile::TempDir::new().unwrap();
        let dir_b = tempfile::TempDir::new().unwrap();
        let a = dir_a.path().canonicalize().unwrap().to_string_lossy().to_string();
        let b = dir_b.path().canonicalize().unwrap().to_string_lossy().to_string();
        write_meta(Path::new(&a), "t-a", "a", None, None);
        write_meta(Path::new(&b), "t-b", "b", None, None);

        let mut h = Harness::new();
        initialize(&mut h).await;
        add_workspace(&mut h, 21, &a).await;
        add_workspace(&mut h, 22, &b).await;

        let v = list_all(&mut h).await;
        let groups = v["result"]["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "two plain dirs → two groups: {v}");
        assert_eq!(groups[0]["workspace"], b.as_str(), "most-recent first");
        assert_eq!(groups[1]["workspace"], a.as_str());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-app-server --lib a_board_thread_folds_into_its_project_group plain_workspaces_still_group_by_directory`
Expected: `a_board_thread_folds_into_its_project_group` **FAIL**（worktree 出现在 `ws` 里）；`plain_workspaces_still_group_by_directory` PASS（回归保护，先绿）。

- [ ] **Step 3: 改 thread/listAll 的聚合逻辑**

把 `thread/listAll` 处理器（约 :2390）的循环体替换为下述实现（`pinned` 与响应写法不变）：

```rust
                    "thread/listAll" => {
                        // 归属优先:带 `board_project` 的会话折进它的项目组;
                        // 只产出这类会话的目录(卡片 worktree)不再顶层成组。
                        // **不动索引**:目录仍留在 workspaces 里,冷会话定位靠它。
                        use std::collections::{BTreeMap, HashMap};
                        let dirs = workspaces.list();
                        let mut exists_of: HashMap<String, bool> = HashMap::new();
                        for dir in &dirs {
                            exists_of.insert(dir.clone(), Path::new(dir).is_dir());
                        }
                        // 每个「组」的会话;组的顺序稍后按索引顺序 + 新项目追加确定。
                        let mut by_group: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
                        let mut group_order: Vec<String> = Vec::new();
                        let mut push_group = |order: &mut Vec<String>, ws: &str| {
                            if !order.iter().any(|d| d == ws) {
                                order.push(ws.to_string());
                            }
                        };
                        for dir in &dirs {
                            if !Path::new(dir).is_dir() {
                                continue; // 失效目录:有新会话引用它时再建组
                            }
                            let metas = match crate::thread_store::ThreadStore::new(Path::new(dir)).list() {
                                Ok(metas) => metas,
                                Err(e) => {
                                    eprintln!("[app-server] thread/listAll failed to list {dir}: {e}");
                                    Vec::new()
                                }
                            };
                            let own: Vec<_> = metas.iter().filter(|m| m.board_project.is_none()).collect();
                            // 该目录有「自己的」普通会话(或本就为空)才作为顶层组保留;
                            // 否则它只是卡片 worktree,归拢后被抑制。
                            if !own.is_empty() || metas.is_empty() {
                                push_group(&mut group_order, dir);
                                for m in own {
                                    by_group.entry(dir.clone())
                                        .or_default()
                                        .push(thread_summary_json(m, &threads));
                                }
                            }
                            // 卡片会话:归入 board_project 指定的组(可为索引外的项目)。
                            for m in metas.iter().filter(|m| m.board_project.is_some()) {
                                let target = m.board_project.clone().unwrap();
                                if !group_order.iter().any(|d| d == &target) {
                                    exists_of.entry(target.clone())
                                        .or_insert_with(|| Path::new(&target).is_dir());
                                    push_group(&mut group_order, &target);
                                }
                                by_group.entry(target)
                                    .or_default()
                                    .push(thread_summary_json(m, &threads));
                            }
                        }
                        let groups: Vec<serde_json::Value> = group_order
                            .into_iter()
                            .map(|ws| {
                                json!({
                                    "workspace": ws,
                                    "exists": exists_of.get(&ws).copied().unwrap_or(false),
                                    "threads": by_group.remove(&ws).unwrap_or_default(),
                                })
                            })
                            .collect();
                        let pinned: Vec<serde_json::Value> = collect_pinned(&workspaces)
                            .iter()
                            .map(|m| thread_summary_json(m, &threads))
                            .collect();
                        write_response(
                            &hub, &client,
                            ok_response(id, json!({ "groups": groups, "pinned": pinned })),
                        )
                        .await?;
                    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-app-server --lib thread_list_all_groups_by_workspace a_board_thread_folds_into_its_project_group plain_workspaces_still_group_by_directory`
Expected: 全部 PASS（含既有 `thread_list_all_groups_by_workspace` 零回归）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): fold card threads into their project group"
```

---

### Task 3: 侧栏可折叠「看板会话」小节

项目组内、看板入口下方，加一个默认折叠的小节，只列 `card_id` 非空的会话。折叠状态独立于项目组（键 `board:${workspace}`）。

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Modify: `desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Consumes: Task 1 的 `ThreadSummary.card_id` / `board_project`。
- Produces: 组内有卡片会话时渲染一个 `aria-label="看板会话"` 的可折叠小节；折叠时其会话行不可见。

- [ ] **Step 1: 写失败测试**

在 `ThreadSidebar.test.tsx` 的 `describe("ThreadSidebar 看板", ...)` 内追加（沿用 `renderSidebar`/`thread()` 助手）：

```tsx
  it("把卡片会话收进默认折叠的「看板会话」小节，展开后可见", () => {
    const withCard: WorkspaceGroup[] = [
      {
        workspace: "/proj",
        exists: true,
        threads: [
          thread("t-plain", "plain-thread", "/proj"),
          { ...thread("t-card", "看板 · card-1", "/proj"), card_id: "card-1", board_project: "/proj" },
        ],
      },
    ];
    const { container } = renderSidebar({ groups: withCard, boards: ["/proj"] });

    // 小节存在，但默认折叠 → 卡片行不可见，普通行仍在。
    expect(screen.getByLabelText("看板会话")).toBeTruthy();
    expect(container.textContent).not.toContain("看板 · card-1");
    expect(container.textContent).toContain("plain-thread");

    fireEvent.click(screen.getByLabelText("看板会话"));
    expect(container.textContent).toContain("看板 · card-1");
  });

  it("没有卡片会话时不渲染「看板会话」小节", () => {
    const { container } = renderSidebar({ groups: boardGroups, boards: ["/proj"] });
    expect(container.querySelector('[aria-label="看板会话"]')).toBeNull();
    expect(container.textContent).toContain("board-thread");
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: 两个新用例 FAIL（`Unable to find a label with the text of: 看板会话`）。

- [ ] **Step 3: 实现小节**

在 `ThreadSidebar.tsx`：新增独立折叠状态（与组折叠分开）：

```tsx
  const [boardCollapsed, setBoardCollapsed] = useState<Set<string>>(new Set());
  const toggleBoardCollapse = (ws: string) => {
    setBoardCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(ws)) next.delete(ws);
      else next.add(ws);
      return next;
    });
  };
```

在组渲染里，把 `renderBoardEntry(g.workspace)` 之后、普通 thread 列表之前插入小节（默认折叠 = 不在集合里视为折叠）：

```tsx
              {!isCollapsed && kanbanItemFor(g.workspace, boards) && renderBoardEntry(g.workspace)}
              {!isCollapsed && renderBoardThreads(g)}
```

新增渲染函数（放在 `renderBoardEntry` 附近）：

```tsx
  /**
   * 项目组内的「看板会话」小节：只收 `card_id` 非空的会话，默认折叠。
   * 折叠状态独立于项目组（键 `board:${workspace}`），互不牵连。
   */
  const renderBoardThreads = (g: WorkspaceGroup) => {
    if (!kanbanItemFor(g.workspace, boards)) return null;
    const cards = g.threads.filter((t) => !t.pinned && t.card_id);
    if (cards.length === 0) return null;
    const isBoardCollapsed = !boardCollapsed.has(g.workspace);
    return (
      <div key={`board-threads:${g.workspace}`}>
        <button
          type="button"
          aria-label="看板会话"
          aria-expanded={!isBoardCollapsed}
          onClick={() => toggleBoardCollapse(g.workspace)}
          className="flex w-full items-center gap-1 px-2 py-1 text-xs text-fg-subtle hover:bg-raised/50 focus:bg-raised/50 focus:outline-none"
        >
          <span className="shrink-0 px-0.5">{isBoardCollapsed ? "▸" : "▾"}</span>
          <span className="min-w-0 flex-1 truncate text-left">看板会话 ({cards.length})</span>
        </button>
        {!isBoardCollapsed && cards.map((t) => renderThread(t, { rowAttr: "group" }))}
      </div>
    );
  };
```

注意：`g.threads.filter((t) => !t.pinned)` 的普通行列表**也要排除卡片会话**，否则卡片会话会在小节之外再出现一次：

```tsx
              {!isCollapsed &&
                g.threads
                  .filter((t) => !t.pinned && !t.card_id)
                  .map((t) => renderThread(t, { rowAttr: "group" }))}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx tsc --noEmit && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: 全部 PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): collapsible board-sessions section per project"
```

---

### Task 4: 全量回归与真机确认

**Files:** 无代码改动；仅验证。

- [ ] **Step 1: Rust 全量**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-app-server`
Expected: 全部 `test result: ok`，无 FAILED。

- [ ] **Step 2: 桌面全量**

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: tsc 干净；全部测试通过。

- [ ] **Step 3: 真机手动确认（可选，需重启 app）**

重建并重装 app 后：在一张卡片会话存在的前提下打开侧栏，确认卡片会话不再作为顶层项目出现、而是收在所属项目的「看板会话」小节里（默认折叠、可展开）。

- [ ] **Step 4: 提交（如 Step 1-2 有格式修正）**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -u
git commit -m "chore: fmt after board-thread grouping"
```

---

## Self-Review

**Spec 覆盖：**
- §4.1 归属落盘 → Task 1（`ThreadMeta` 两字段、`run_once` 填充、`start_thread_core` 写入、`thread_summary_json` 出站、`protocol.ts`）。
- §4.2 分组归拢但不破 lookup → Task 2（`thread/listAll`；索引写入不动）。
- §4.3 侧栏可折叠小节 → Task 3（默认折叠、独立键、只收 `card_id`）。
- §5 边界（插件关/卸载仍归组；重启仍在；历史会话不误收；看板移除不追删）→ 由「归属写在 meta、与插件无关」与「`board_project.is_none()` 才按目录成组」共同保证；Task 2 的零回归用例覆盖「历史会话不误收」。
- §6 验收 1–6 → Task 2 用例覆盖 1/2/3；Task 3 覆盖 4；Task 1 Step 8 覆盖 5；Task 4 覆盖 6。

**类型一致性：** `board_project`/`card_id` 在 `ThreadMeta`（`Option<String>`）、`LaunchRequest`（`board_project: String`）、wire（`null` 或字符串）、`ThreadSummary`（`?: string`）四处命名一致；`start_thread_core` 的 `Option<&str>` 与 `meta` 的 `Option<String>` 由 `map(str::to_string)` 衔接；折叠键 `board:${workspace}` 在实现里以 `boardCollapsed` 集合承载（键实际用 `g.workspace`，注释里的 `board:` 前缀用于语义区分，不入集合键，避免与组折叠键空间混淆）。

**已检查的坑：**
- `ThreadMeta` 字面量共 4 处（`thread_store.rs` ×3、`server.rs` ×1），漏改会编译失败——Step 3 明确列出。
- `LaunchRequest` 字面量共 2 处（`card_scheduler.rs` 生产、`server.rs` 测试助手），Step 5/6 都改。
- `thread/start` 与 `launch_inner` 两个 `start_thread_core` 调用点都要补参（Step 4）。
- 普通行列表必须排除 `card_id`，否则卡片会话重复出现（Task 3 Step 3 已含）。
