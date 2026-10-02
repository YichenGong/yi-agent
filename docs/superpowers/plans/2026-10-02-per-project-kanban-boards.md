# 项目级 Superpowers 看板 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把「全局一块看板」改成「每个项目各有一块看板」，看板挂在项目下、点开在主区域显示，且能脱离桌面 app 继续跑。

**Architecture:** 新增领域 crate `yi-agent-boards`：全局登记表（`~/.yi-agent/superpowers-kanban/boards.json`）记哪些项目有看板；创建时写清单+开关并起一个 `setsid` 脱离的 `daemon serve`（复用现有 daemon 机制，它自带插件监督）；查询经 `plugin/query` 的 `project` 参数路由到该项目的 daemon socket。桌面端：项目右键创建/移除，项目下多一个看板条目，主区域渲染看板，删掉全局左列。额度用**全局租约目录 + flock**，由插件在推进队列前领取（Python 侧）。

**Tech Stack:** Rust（新 crate + app-server RPC）、TypeScript/React（desktop）、Python（插件 runner）、既有 `libc::flock` 模式。

## Global Constraints

- 主工作区：`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`；cargo 一律 `--offline`，从 `yi-agent-rs/` 跑。
- 插件工作区：`cd plugins/superpowers-kanban`。
- 桌面：`export PATH="/opt/homebrew/bin:$PATH"`；`npx tsc --noEmit`；vitest 需 `TMPDIR="$PWD/.tmpverify"`。
- `TMPDIR=/Users/gongyichen/.yi-agent-tmp`（socket 103 字节上限）。
- 长编译放后台（`process_start`），前台会超时。
- **插件零 `yi-agent-*` 依赖**，不得破坏（可独立安装/卸载）。
- 提交前 `git status` 核对，**不要 `git add -A`**（本仓库是共享工作树，有并发执行者）。
- 全局目录一律 `~/.yi-agent/...`（与既有约定一致）。
- 前缀/命名空间不得合并：daemon socket 用 `yi-agent-`，插件通道 socket 用 `plugin-`（见 socket-fallback spec）。

---

### Task 1: 领域 crate `yi-agent-boards`（登记表 + 脚手架）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-boards/Cargo.toml`
- Create: `yi-agent-rs/crates/yi-agent-boards/src/lib.rs`
- Create: `yi-agent-rs/crates/yi-agent-boards/src/registry.rs`
- Create: `yi-agent-rs/crates/yi-agent-boards/src/scaffold.rs`
- Modify: `yi-agent-rs/Cargo.toml`（workspace members）

**Interfaces:**
- Produces:
  - `boards::global_dir() -> PathBuf` —— `~/.yi-agent/superpowers-kanban`
  - `boards::registry::{list, contains, register, unregister}(...) -> io::Result<...>`
    读写 `<global_dir>/boards.json`，形如 `{"boards":[{"project":"<abs>","created_at":"..."}]}`。
  - `boards::scaffold::install_manifest(project) -> io::Result<()>` —— 写
    `<project>/.yi-agent/supervisors/superpowers-kanban.json`（幂等）。
  - `boards::scaffold::enable_switch(project) -> io::Result<()>` —— 在
    `<project>/.yi-agent/preferences.json` 里把 `superpowers_kanban` 置 true，
    **保留其它键**（read-modify-write + tmp/rename，照抄 `runtime_prefs::save` 的模式）。
- Consumes: 无（叶子 crate）。

- [ ] **Step 1: 建 crate 骨架**

`yi-agent-rs/crates/yi-agent-boards/Cargo.toml`：

```toml
[package]
name = "yi-agent-boards"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = { workspace = true }
libc = "0.2"

[dev-dependencies]
tempfile = "3"
```

加入 workspace `members`（按现有文件里的字母序落位）。**不改 `[workspace.dependencies]`**，避免影响其它 crate。

- [ ] **Step 2: 写失败测试**

`src/registry.rs` 内测试：

```rust
#[test]
fn a_fresh_registry_lists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    assert!(list(dir.path()).unwrap().is_empty());
    assert!(!contains(dir.path(), Path::new("/projects/a")).unwrap());
}

#[test]
fn registering_is_idempotent_and_survives_a_reread() {
    let dir = tempfile::tempdir().unwrap();
    let project = Path::new("/projects/a");
    register(dir.path(), project).unwrap();
    register(dir.path(), project).unwrap();
    let boards = list(dir.path()).unwrap();
    assert_eq!(boards.len(), 1, "重复登记不得出现两条");
    assert!(contains(dir.path(), project).unwrap());
}

#[test]
fn unregistering_removes_only_the_named_project() {
    let dir = tempfile::tempdir().unwrap();
    register(dir.path(), Path::new("/projects/a")).unwrap();
    register(dir.path(), Path::new("/projects/b")).unwrap();
    unregister(dir.path(), Path::new("/projects/a")).unwrap();
    let boards = list(dir.path()).unwrap();
    assert_eq!(boards.len(), 1);
    assert_eq!(boards[0].project, PathBuf::from("/projects/b"));
}

#[test]
fn a_corrupt_registry_reads_as_empty_instead_of_failing() {
    // 手改坏了的登记表不该让整个侧栏起不来。
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("boards.json"), "{ not json").unwrap();
    assert!(list(dir.path()).unwrap().is_empty());
}
```

`src/scaffold.rs` 内测试：

```rust
#[test]
fn enabling_the_switch_preserves_unrelated_preferences() {
    let dir = tempfile::tempdir().unwrap();
    let prefs = dir.path().join(".yi-agent");
    std::fs::create_dir_all(&prefs).unwrap();
    std::fs::write(
        prefs.join("preferences.json"),
        r#"{"subagent_runtime":"always","superpowers_kanban":false}"#,
    )
    .unwrap();

    enable_switch(dir.path()).unwrap();

    let text = std::fs::read_to_string(prefs.join("preferences.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["superpowers_kanban"], true);
    assert_eq!(value["subagent_runtime"], "always", "无关的键必须保留");
}

#[test]
fn installing_the_manifest_is_idempotent_and_uses_the_absolute_command() {
    let dir = tempfile::tempdir().unwrap();
    install_manifest(dir.path()).unwrap();
    install_manifest(dir.path()).unwrap();
    let path = dir.path().join(".yi-agent/supervisors/superpowers-kanban.json");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(value["name"], "superpowers-kanban");
    assert_eq!(value["switch_key"], "superpowers_kanban");
    assert_eq!(value["query_socket"], "{state_dir}/superpowers-kanban.sock");
    let command = value["command"].as_str().unwrap();
    assert!(command.ends_with("superpowers-kanban"), "got {command}");
    assert!(Path::new(command).is_absolute(), "命令必须绝对路径：{command}");
}
```

- [ ] **Step 3: 运行确认失败**

Run: `cd yi-agent-rs && cargo test --offline -p yi-agent-boards`
Expected: FAIL（函数不存在）

- [ ] **Step 4: 实现**

`registry.rs`：`Board { project: PathBuf, created_at: String }` + `Registry { boards: Vec<Board> }`，
`list/contains/register/unregister` 全部走「读 → 改 → tmp/rename 写」。
`scaffold.rs`：常量清单 JSON（与仓库里现存 `.yi-agent/supervisors/superpowers-kanban.json` 逐字一致，
`command` 取 `/opt/homebrew/bin/superpowers-kanban` 若存在、否则回落 `std::env::var("PATH")` 查到的路径，
查不到就返回错误让调用方报出来）。
**`created_at` 用 `chrono::Local::now().to_rfc3339()`**（workspace 已有 chrono）。
`lib.rs` 挂 `pub mod registry; pub mod scaffold;` 并加 `pub fn global_dir()`（`$HOME/.yi-agent/superpowers-kanban`，
`HOME` 缺失则报错）。

- [ ] **Step 5: 运行确认通过** → PASS
- [ ] **Step 6: Commit**

```bash
git add yi-agent-rs/Cargo.toml yi-agent-rs/Cargo.lock yi-agent-rs/crates/yi-agent-boards
git commit -m "feat(boards): registry and scaffolding for per-project kanban boards"
```

---

### Task 2: 脱离式 daemon（起 / 停）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-boards/src/board_daemon.rs`
- Modify: `yi-agent-rs/crates/yi-agent-boards/src/lib.rs`

**Interfaces:**
- Consumes: `boards::global_dir`（不直接用，仅同类）；项目 runtime 目录 = `<project>/.yi-agent/runtime`。
- Produces:
  - `boards::board_daemon::spawn_detached(exe: &Path, project: &Path) -> io::Result<u32>`
    —— `setsid` 脱离 + stdio 置空 + `cwd=project`，参数 `daemon serve`；返回 child pid。
  - `boards::board_daemon::wait_ready(project: &Path, timeout: Duration) -> bool`
    —— 轮询该项目 daemon socket 是否应答（复用 `yi-agent-store::ipc::socket_path_for` + `Status`）。
  - `boards::board_daemon::is_running(project: &Path) -> bool`
  - `boards::board_daemon::stop(project: &Path) -> Result<(), String>` —— 发 `Stop`，等 socket 消失。

**关键**：`spawn_detached` **必须** `setsid`——否则 app 以进程组信号退出时会把 daemon 一起带走，
决策 4（关掉 app 后继续跑）就不成立。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn a_detached_child_leads_its_own_session_and_runs_in_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let marker = dir.path().join("child.txt");
    let stub = dir.path().join("stub.sh");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\n{{ echo pid=$$; echo cwd=$(pwd); echo pgid=$(ps -o pgid= -p $$ | tr -d ' '); }} > {}\nsleep 30\n",
            marker.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&stub).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).unwrap();

    let pid = spawn_detached(&stub, &project).unwrap();

    // 等 stub 写出自己的身份
    for _ in 0..100 {
        if marker.exists() { break; }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let text = std::fs::read_to_string(&marker).unwrap();
    let get = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .unwrap()
            .to_string()
    };

    assert_eq!(get("cwd"), project.canonicalize().unwrap().display().to_string());
    assert_eq!(
        get("pgid"), get("pid"),
        "setsid 没生效：子进程仍是父进程会话/进程组的成员，app 退出会把它一起带走"
    );

    unsafe { libc::kill(pid as i32, libc::SIGKILL); }
}
```

- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**

```rust
pub fn spawn_detached(exe: &Path, project: &Path) -> std::io::Result<u32> {
    let mut command = std::process::Command::new(exe);
    command
        .args(["daemon", "serve"])
        .current_dir(project)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // setsid: 让子进程成为新会话的首领，脱离父进程的会话与进程组。
    // 没有这一步，app 以进程组信号退出时会把 daemon 一并杀掉，
    // 「关掉 app 后看板继续跑」就不成立。
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command.spawn()?.id())
}
```

`wait_ready` / `is_running`：`socket_path_for(&project.join(".yi-agent/runtime"))` → `send_request(Status)`
→ `Ok` 即就绪。`stop`：发 `Stop`，然后轮询 socket 消失。

- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

```bash
git commit -m "feat(boards): detached per-project daemon start and stop"
```

---

### Task 3: 编排 + app-server 三个 RPC

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-boards/src/lifecycle.rs`
- Modify: `yi-agent-rs/crates/yi-agent-boards/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`（加依赖）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（分发处加三个分支）

**Interfaces:**
- Consumes: Task 1（registry/scaffold）、Task 2（board_daemon）。
- Produces:
  - `boards::lifecycle::create(project: &Path) -> Result<BoardStatus, String>`
    幂等**按 daemon 判断**：scaffold → 若 `is_running` 为假则 `spawn_detached` + `wait_ready` → `register`。
    登记表只记录意图；一个已登记但 daemon 已死的项目必须能被 `create` 重新拉起（决策 1/4）。
    真实启动器 `launch_if_absent` 自身幂等（`is_running` 即早返回），所以不会起第二个 daemon。
  - `boards::lifecycle::remove(project: &Path) -> Result<(), String>`
    `stop` → 删 `<project>/.yi-agent/superpowers-kanban/` → `unregister`（清单与开关保留）。
  - `boards::lifecycle::status(project: &Path) -> BoardStatus`
    `{ registered: bool, daemon_running: bool, queued: usize, running: usize }`（计数经插件查得，查不到记 0）。
  - app-server RPC：`board/create`、`board/remove`、`board/list`。

- [ ] **Step 1: 写失败测试**（lifecycle 的打桩测试）

把 `exe` 作为参数正是为了测试：用一个 stub 脚本当「daemon」，它会建 socket 吗？
不会——所以 `wait_ready` 会超时。**改为让 `create` 接受可注入的「启动器」**：

```rust
pub fn create_with(
    project: &Path,
    launcher: &mut dyn FnMut(&Path) -> Result<bool, String>, // 返回是否已在运行
) -> Result<BoardStatus, String>
```

测试：

```rust
#[test]
fn create_scaffolds_registers_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let global = dir.path().join("global");

    // 注入的「启动器」按真实启动器的语义工作：第一次被调用时绑定项目的
    // runtime socket 并起一个线程在那里回答 Status（用同一个 listener 复活，
    // 绝不绑第二个）。于是第二次 create 的 is_running 探测为真、短路，
    // 「重复创建不该再起一个 daemon」由 daemon 的存在担保，而不是由登记表。
    // 幂等按 daemon 判断，登记表只记录意图——这样「daemon 已死但仍在登记表」
    // 的项目才能被 create 重新拉起。
    let socket = runtime_socket(&project).unwrap();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let mut listener: Option<std::os::unix::net::UnixListener> = None;
    let mut daemon: Option<std::thread::JoinHandle<usize>> = None;
    let launched = std::cell::Cell::new(0);
    let mut launcher = |_p: &Path| {
        launched.set(launched.get() + 1);
        if listener.is_none() {
            let bound = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            listener = Some(bound.try_clone().unwrap());
            daemon = Some(std::thread::spawn(move || {
                // create #1 的探测发生在 socket 出现之前（那时 is_running 为假、
                // 不建连）；只有 create #2 会真正连上来探一次 Status。
                let mut served = 0usize;
                for _ in 0..1 {
                    let (stream, _) = bound.accept().unwrap();
                    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(request["command"]["type"].as_str(), Some("Status"));
                    let reply = serde_json::json!({
                        "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                        "request_id": request["request_id"],
                        "result": { "type": "Status", "high_water_event_id": 0 },
                    });
                    let mut stream = stream;
                    std::io::Write::write_all(&mut stream, reply.to_string().as_bytes()).unwrap();
                    std::io::Write::write_all(&mut stream, b"\n").unwrap();
                    served += 1;
                }
                served
            }));
        }
        Ok(true)
    };

    let first = create_with_project(&project, &global, &mut launcher).unwrap();
    assert!(first.registered);
    assert_eq!(launched.get(), 1);
    // 清单与开关就位
    assert!(project.join(".yi-agent/supervisors/superpowers-kanban.json").exists());
    let prefs: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project.join(".yi-agent/preferences.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(prefs["superpowers_kanban"], true);

    // 再创建一次：不重复起进程
    let second = create_with_project(&project, &global, &mut launcher).unwrap();
    assert!(second.registered);
    assert_eq!(launched.get(), 1, "重复创建不该再起一个 daemon");

    drop(listener);
    daemon.unwrap().join().unwrap();
}

#[test]
fn remove_stops_unregisters_and_deletes_the_queue_but_keeps_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    std::fs::create_dir_all(project.join(".yi-agent/superpowers-kanban")).unwrap();
    std::fs::write(project.join(".yi-agent/superpowers-kanban/board.json"), "{}").unwrap();
    let global = dir.path().join("global");
    let mut launcher = |_p: &Path| Ok(true);
    create_with_project(&project, &global, &mut launcher).unwrap();

    let mut stopped = 0;
    let mut stopper = |_p: &Path| { stopped += 1; Ok(()) };
    remove_with(&project, &global, &mut stopper).unwrap();

    assert_eq!(stopped, 1);
    assert!(!project.join(".yi-agent/superpowers-kanban/board.json").exists(), "队列状态必须被删");
    assert!(
        project.join(".yi-agent/supervisors/superpowers-kanban.json").exists(),
        "清单是配置不是数据，必须保留"
    );
    assert!(!contains(&global, &project).unwrap());
}
```

（`create_with_project` / `remove_with` 是接受注入器与全局目录的测试友好版本；
生产封装 `create`/`remove` 传 `global_dir()` 与真实 launcher/stopper。）

- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**；app-server 分发处照 `"plugin/query"` 的写法加三个分支
  （解析 `project` 参数 → 调 lifecycle → `ok_response` / `err_response`）。
  依赖里加 `yi-agent-boards = { path = "../yi-agent-boards" }`。
- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

```bash
git commit -m "feat(boards): create/remove/list RPCs backed by the board lifecycle"
```

---

### Task 4: `plugin/query` 按项目路由（修掉「点了没反应」）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`plugin_query` 与其调用点）

**Interfaces:**
- Consumes: Task 1（`registry::contains`）。
- Produces: `plugin_query(project: &Path, global: &Path, method, plugin, params)`，
  错误码：
  - `board_not_created` —— 该项目未登记看板
  - `daemon_unavailable` —— 已登记但 daemon 不可达
  - `plugin_unavailable` —— daemon 在，但插件不在转发表

- [ ] **Step 1: 写失败测试**（沿用 app-server 现有 harness）

```rust
#[test]
fn a_query_without_a_registered_board_is_refused_with_its_own_code() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let global = dir.path().join("global");

    let error = plugin_query(&project, &global, "list", "superpowers-kanban", json!({})).unwrap_err();
    assert_eq!(error.code, "board_not_created");
}

#[test]
fn a_registered_board_without_a_daemon_reports_daemon_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let global = dir.path().join("global");
    yi_agent_boards::registry::register(&global, &project).unwrap();

    let error = plugin_query(&project, &global, "list", "superpowers-kanban", json!({})).unwrap_err();
    assert_eq!(error.code, "daemon_unavailable", "不能让 UI 误以为是「插件没装」");
}
```

- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**

`plugin_query` 的 `workdir` 参数改成 `project`（语义修正），新增 `global`；
用 `socket_path_for(&project.join(".yi-agent/runtime"))` 解析**项目**的 socket；
错误从裸 `String` 改为带 `code` 的结构体（`err_response(id, RpcError::with_code(...))`，
沿用既有 `RpcError` 构造，若无对应构造器则加一个 `code(code, message)`）。
**调用点**：`"plugin/query"` 分支从 `req.params` 取 `project`（必填）；
缺失时返回 `board_not_created` 之外的 `invalid_params`。

- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

```bash
git commit -m "fix(app-server): route plugin queries to the project's daemon, not the app's cwd"
```

---

### Task 5: 前端数据层

**Files:**
- Create: `desktop/src/lib/superpowersKanbanBoards.ts`
- Create: `desktop/src/lib/boardIndex.ts`
- Modify: `desktop/src/lib/superpowersKanbanSwitch.ts`（`plugin/query` 带 `project`）

**Interfaces:**
- Produces:
  - `createBoard(rpc, project): Promise<void>` —— 调 `board/create`
  - `removeBoard(rpc, project): Promise<void>`
  - `listBoards(rpc): Promise<string[]>` —— 调 `board/list`，返回项目路径
  - `queryPlugin(rpc, project, method, params)` —— **增加 project 参数**（改 `superpowersKanbanSwitch.ts` 里的实现）
  - `boardIndex.ts`：`kanbanItemFor(workspace, boards): boolean`、`summarize(cards): string`（纯函数，易测）
  - `boardErrorKind(error): "not_created" | "daemon_down" | "plugin_missing" | "other"`

- [ ] **Step 1: 写失败测试**（vitest，纯函数优先）

```ts
describe("boardIndex", () => {
  it("只给已登记的项目显示看板条目", () => {
    expect(kanbanItemFor("/a", ["/a", "/b"])).toBe(true);
    expect(kanbanItemFor("/c", ["/a", "/b"])).toBe(false);
  });
  it("摘要把排队与运行分开数", () => {
    expect(summarize([{ state: "queued" }, { state: "queued" }, { state: "running" }]))
      .toBe("2 排队 · 1 运行中");
    expect(summarize([])).toBe("空");
  });
});

describe("boardErrorKind", () => {
  it("区分「没建看板」与「插件没装」", () => {
    expect(boardErrorKind(new Error("board_not_created"))).toBe("not_created");
    expect(boardErrorKind(new Error("daemon_unavailable"))).toBe("daemon_down");
    expect(boardErrorKind(new Error("plugin superpowers-kanban is not available"))).toBe("plugin_missing");
    expect(boardErrorKind(new Error("boom"))).toBe("other");
  });
});

describe("queryPlugin", () => {
  it("把 project 带进 plugin/query 的参数", async () => {
    const calls: unknown[] = [];
    const rpc = async <T,>(m: string, p: unknown) => { calls.push([m, p]); return {} as T; };
    await readBoardSwitch(rpc, "/proj");
    expect(calls[0]).toEqual(["plugin/query", { plugin: "superpowers-kanban", project: "/proj", method: "switch.read", params: {} }]);
  });
});
```

- [ ] **Step 2: 运行确认失败**

Run: `cd desktop && TMPDIR="$PWD/.tmpverify" npx vitest run src/lib/boardIndex.test.ts`
Expected: FAIL

- [ ] **Step 3: 实现**（含把 `setBoardSwitch/readBoardSwitch/fetchBoard/enqueueBoardCard` 都加上 `project` 形参）
- [ ] **Step 4: 运行确认通过** + `npx tsc --noEmit` → PASS
- [ ] **Step 5: Commit**

```bash
git commit -m "feat(desktop): board data layer with per-project queries"
```

---

### Task 6: 侧栏（右键创建/移除 + 看板条目 + 选中态）

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Modify: `desktop/src/App.tsx`
- Modify: `/Users/gongyichen/.yi-agent/skills/superpowers-kanban/SKILL.md`（若涉及术语，另见 Task 9）

**Interfaces:**
- Consumes: Task 5（`listBoards/createBoard/removeBoard/kanbanItemFor/summarize`）。
- Produces:
  - `ThreadSidebar` 新 props：`boards: string[]`、`onCreateBoard(path)`、`onRemoveBoard(path)`、
    `onOpenBoard(path)`、`selectedBoard: string | null`、`boardSummaries: Record<string, string>`。
  - `App` 新状态：`selectedBoard: string | null`（与 `currentId` 相互独立）。

- [ ] **Step 1: 写失败测试**

```tsx
it("项目右键菜单提供创建看板", async () => {
  render(<ThreadSidebar {...baseProps} boards={[]} onCreateBoard={spyCreate} ... />);
  await openWorkspaceMenu("/proj");
  fireEvent.click(screen.getByText("创建 Superpowers 看板"));
  expect(spyCreate).toHaveBeenCalledWith("/proj");
});

it("已登记的项目显示看板条目并可打开", async () => {
  render(<ThreadSidebar {...baseProps} boards={["/proj"]} onOpenBoard={spyOpen} selectedBoard={null} ... />);
  fireEvent.click(screen.getByText("看板"));
  expect(spyOpen).toHaveBeenCalledWith("/proj");
});

it("未登记的项目没有看板条目", () => {
  render(<ThreadSidebar {...baseProps} boards={[]} ... />);
  expect(screen.queryByText("看板")).toBeNull();
});
```

- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**

工作区右键菜单（现有 `contextWs` 那段）里按 `boards.includes(g.workspace)` 二选一显示「创建 Superpowers 看板」/「移除看板」。
在 `!isCollapsed` 的线程列表**之前**插入看板条目（`aria-label="看板"`，`title={g.workspace}`），
选中态用与线程一致的底色。`onOpenBoard` 在 `App` 里 `setSelectedBoard(path)`（**不**动 `currentId`）。
移除时 `window.confirm` 二次确认（决策 5 不可逆）。

- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

```bash
git commit -m "feat(desktop): per-project kanban entry in the sidebar"
```

---

### Task 7: 主区域看板视图 + 删全局左列 + 失败可见

**Files:**
- Modify: `desktop/src/App.tsx`
- Delete: `desktop/src/components/SuperpowersKanbanCollapsedStrip.tsx`（若无其它引用）

**Interfaces:**
- Consumes: Task 6（`selectedBoard`）。
- Produces: 主区域在 `selectedBoard !== null` 时渲染该项目的看板；全局左列移除。

- [ ] **Step 1: 写失败测试**

```tsx
it("选中看板时主区域显示该项目看板，且不再有全局左列", async () => {
  render(<App ... />);
  await openBoardFor("/proj");
  expect(screen.getByText("Superpowers 看板")).toBeInTheDocument();       // 主区域
  expect(screen.queryByLabelText("收起看板")).toBeNull();                    // 全局列的折叠按钮没了
});

it("开关写入失败会显示错误，而不是静默", async () => {
  rpc.mockImplementation((m: string) => m === "plugin/query"
    ? Promise.reject(new Error("board_not_created"))
    : Promise.resolve({}));
  render(<App ... />);
  await openBoardFor("/proj");
  fireEvent.click(screen.getByRole("checkbox"));
  expect(await screen.findByText(/尚未创建看板/)).toBeInTheDocument();
});
```

- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**

- 删 `kanbanCollapsed` 状态、`SuperpowersKanbanCollapsedStrip` 的渲染与 import（文件无引用后删除）。
- 删左侧那个 `flex w-72` 看板列。
- 主区域：`selectedBoard` 非空时，在该项目上下文渲染
  `SuperpowersKanbanSettings` + `SuperpowersKanbanEnqueue` + `SuperpowersKanbanView`；
  `project={selectedBoard}` 传给所有 board RPC。
- `onToggle` 的 `.catch(() => {})` 换成 `setBoardError(boardErrorKind(e))` 并把错误渲染成内联提示
  （`not_created` → 「该项目尚未创建看板」+「创建看板」按钮；`daemon_down` → 「看板进程不可达」；
  `plugin_missing` → 「插件未安装」）。
- **改 `desktop/src/lib/superpowersKanbanState.ts` 里那两行注释**：它写着
  「Mirrors the Rust `yi_agent_board_ui::state::load_cards`」，而该 crate 并不存在——改成描述真实来源。

- [ ] **Step 4: 运行确认通过** + `npx tsc --noEmit` → PASS
- [ ] **Step 5: Commit**

```bash
git commit -m "feat(desktop): render the board in the main area and surface write failures"
```

---

### Task 8: 全局并发租约（Python 侧）—— **本轮不做（人类裁决）**

> **状态：延后。** 人类裁示「先做核心 9 个任务，额度留到有第二个看板时再上」。
> 理由：全局租约是跨进程协调，只有在**同时跑多个看板**时才有意义；本轮单看板场景下
> 各 daemon 按日历上限（限流 3 / 不限流 10）各自为政已够用。这是有意延后，不是遗漏
> （见 spec §6 非目标、§7 已知缺口 1）。下面的步骤保留作下一轮的起点。

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lease.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`（推进循环用租约）

**Interfaces:**
- Produces:
  - `lease::acquire(project: &Path, limit: usize) -> Option<Lease>` —— 在
    `~/.yi-agent/superpowers-kanban/leases/` 下找/建一个名额；满了返回 `None`。
  - `Lease` 持有 flock 与一个名额文件；`Drop` 释放（unlink + 关 fd）。
- Consumes: `libc`（插件已否？若无则加 `libc = "0.2"`；flock 语义照抄宿主 `InstanceLock`）。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn the_first_n_holders_get_slots_and_the_next_one_waits() {
    let dir = tempfile::tempdir().unwrap();
    let a = lease::acquire_in(&dir.path().to_path_buf(), 2).unwrap();
    let _b = lease::acquire_in(&dir.path().to_path_buf(), 2).unwrap();
    assert!(lease::acquire_in(&dir.path().to_path_buf(), 2).is_none(), "第 3 个必须等");
    drop(a);
    assert!(lease::acquire_in(&dir.path().to_path_buf(), 2).is_some(), "释放后必须能领到");
}

#[test]
fn a_slot_held_by_a_dead_process_is_reclaimed() {
    // 进程被 kill -9 时不会跑 Drop。名额必须能靠 flock 自动回收，
    // 否则额度会被永久占死。
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["hold_one_slot", path.to_str().unwrap()])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(lease::acquire_in(&path, 1).is_none(), "子进程占着唯一的位");
    std::process::Command::new("kill")
        .args(["-9", &child.id().to_string()])
        .status()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(lease::acquire_in(&path, 1).is_some(), "SIGKILL 后名额必须可回收");
}
```

（第二个测试用一个隐藏的 `#[test]`-旁路入口：`argv[1] == "hold_one_slot"` 时领一个名额并 `sleep(30)`；
用 `#[ignore]` 的测试函数承载该逻辑，或加一个 `#[ctor]` 风格分支——**实现选最小可行的那种**，
若过于绕则退化为单测 flock 语义本身。）

- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**；`main.rs` 的推进循环：
  `let limit = calendar.limit_at(now)` 之后，用 `lease::acquire(&project_root, limit)`，
  `None` 就跳过本轮启动（卡片留在 queued），`Some(_lease)` 才 `run_once`。
  **先领额度再建 worktree**（建 worktree 不耗模型调用，但省得白建）。
- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

```bash
git commit -m "feat(kanban-plugin): global concurrency lease shared across projects"
```

---

### Task 9: TUI 命令入口

**命令名与语义（人类裁决，覆盖下文字面）**：规范名是 `/superpowers-kanban`
（`/kanban` 仅保留过渡别名）。子命令：
- `on` / `off` —— **插件开关**（保持现状，写 `switch.write`）。
- `create` / `remove` / `status` —— 看板**生命周期**，作用于当前会话目录，走
  `yi_agent_boards::lifecycle::{create_in, remove_in, status_with}`。
- `run` / `add <spec> <plan>` / 无参 —— 保持现状（走插件通道）。

（计划原文写的 `/kanban on|off` 去建/删看板是错的：`on|off` 已被开关占用。）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/board.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/superpowers_kanban.rs`（本目录未登记看板时给指引）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`（Kanban 分支按子命令分发）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`（usage/description）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml`（加 `yi-agent-boards` 依赖）

**Interfaces:**
- Consumes: Task 3（`lifecycle::{create_in, remove_in, status_with}`）。
- Produces: `/superpowers-kanban create|remove|status`，作用于**当前会话目录**。

- [ ] **Step 1: 写失败测试**（沿用 TUI 现有命令测试的写法）

```rust
#[test]
fn create_reports_a_created_board_for_this_directory() {
    let dir = tempfile::tempdir().unwrap();
    let out = handle_board(dir.path(), &dir.path().join("global"), "create");
    assert!(out.lines.iter().any(|l| l.contains("看板已创建")), "{:?}", out.lines);
}

#[test]
fn status_reports_whether_this_directory_has_one() {
    let dir = tempfile::tempdir().unwrap();
    let out = handle_board(dir.path(), &dir.path().join("global"), "status");
    assert!(out.lines.iter().any(|l| l.contains("未创建")), "{:?}", out.lines);
}
```

- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**；`tui/board.rs::handle_board(project, global, subcommand)` 调 lifecycle；
  `app.rs` 的 `SlashCommand::Kanban` 分支按子命令分发（`create|remove|status` 走 board，
  其余沿用 `superpowers_kanban::handle_kanban`）；输出用 notice，与既有命令一致
- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

```bash
git commit -m "feat(tui): /superpowers-kanban create, remove and status for this directory"
```

---

### Task 10: 端到端验收 + 文档收尾

- [x] **Step 1: 全量回归**（宿主 / 插件 / 桌面），确认既有能力不回退
  - 宿主：`yi-agent-boards` 28、`yi-agent-app-server` 197、`yi-agent` 556 + 新集成测试 1，全绿、warning-clean
  - 插件：`plugins/superpowers-kanban` 123 全绿（一处既有 `unused variable: other` 警告，非本轮引入）
  - 桌面：`npx tsc --noEmit` 干净；`vitest run` 314/33 文件全绿
- [x] **Step 2: 真机闭环 — 改为非侵入式**（原计划要替换并重签名用户已装的二进制，
  **未执行**；不动 `~/.cargo/bin/yi-agent` 与 `/opt/homebrew/bin/superpowers-kanban`）。
  改为用 worktree 构建出的 `yi-agent` 起真 app-server、真脱离式 daemon、真插件，配置隔离在临时 `HOME`。
- [x] **Step 3: 分四条实测**（脚本化于 `yi-agent-rs/crates/yi-agent/tests/board_e2e.rs`，
  `YI_AGENT_BOARD_E2E=1` 开启；详细证据见 spec §9）：
  1. 创建 → 登记表有它、清单与开关就位、daemon 在跑、插件在跑（实测通过）
  2. `plugin/query` 带 `project` → 取到 `switch.read`（`{on:true,source:"project"}`）/ `list`
  3. **退出 app-server → daemon/插件仍在跑**（`is_running` 仍为真、socket 仍在）
  4. 移除 → daemon 停、队列状态删、清单保留
- [x] **Step 4: 更新 spec**：状态改为「核心已实施并通过端到端验收」；§9 记录实测与偏差。
  **注意**：原文「改成已由 Task 8 落地」**不成立**——Task 8 本轮按人类裁决延后，§6/§7 保持「延后」。
- [ ] **Step 5: Commit**

```bash
git commit -m "test(boards): per-project board end-to-end verification"
```

---

## Self-Review

**Spec 覆盖**：决策 1（Task 3）2（Task 6）3（Task 8）4（Task 2/10-3）5（Task 3）6（Task 9）7（Task 7）；
§4.4 路由（Task 4）；失败可见（Task 7）。

**一处需实现者注意的类型一致性**：Task 5 给 `queryPlugin` 加了 `project` 形参后，
Task 6/7 所有调用点都要传；漏传会编译不过（这是有意的，好过静默走错目录）。

**已知取舍**：Task 8 第二个测试（SIGKILL 回收）在纯单测里较绕，
实现若发现不可行，**必须**改为至少覆盖「进程被 kill 后名额可回收」的集成测，
不得直接删掉——那正是额度会不会被永久占死的关键。
