# 悬空 daemon 自停 + attach 入口拦截 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 daemon 在自己服务的 runtime 目录消失时主动停止、让 app-server 拒绝为已不存在的项目目录 bring-up，从结构上终止"悬空 daemon 无限重试"并防止复活已删目录。

**Architecture:** 两处独立且互补的改动。（1）`yi-agent-store/src/ipc.rs` 的 accept 循环加一个 1 秒节流的存在性检查，目录消失即打 WARN（真实原因）+ 停止 + `break`，复用循环尾部既有的 socket/lock 清理，把 socket 变成真的死的，从而让既有 `is_liveness_error` → `repair` 自愈链路自然接管；同时给 socket 清理加 `(dev, ino)` 身份护栏，防止误删"同路径重建的新 daemon"的 socket。（2）`yi-agent-app-server/src/server.rs` 的 `attach_cwd_runtime` 在查缓存之前判 `cfg.workdir` 是否存在，缺失则摘缓存并返回明确错误，不 `create_dir_all`。两者判据同源但职责不重叠，故不抽共享抽象。

**Tech Stack:** Rust 1.85（workspace `rust-version`）、`yi-agent-store`（`std::os::unix::net`、`std::os::unix::fs::MetadataExt`）、`yi-agent-app-server`、`tracing`、`tempfile`。

**Spec:** `docs/superpowers/specs/2026-10-18-stranded-daemon-self-stop-design.md`

## Global Constraints

- **不动 wire 契约**：不改 `IpcRequest` / `IpcResponse` / `IpcErrorCode` 任何变体；不改协议文档；不改前端。
- **不动 `log_request_failure`**（`ipc.rs:2847`）与 `error_response` 的行为。那条"故意剥掉原因、不泄露 db/env/实现细节"的安全契约原样保留（spec §2.2）。
- **不加指数退避**（spec §3.2）：目录缺失是二值、不可自愈条件；检测到即停。
- **存在性检查节流 1 秒**，不得每 10ms 一次 `stat`。
- **`attach_cwd_runtime` 的存在性判据是 `cfg.workdir`**（项目根），**不是** `runtime_dir`。只删 `runtime/` 而项目根仍在时应允许重新 attach。
- **`attach_cwd_runtime` 必须在查缓存之前判存在、且必须 `remove` 缓存项**：否则"cwd 缺失"被缓存后，目录重建时 `Err` 会永久挡住恢复。
- **不重建目录**：本改动不得在 cwd 缺失路径上调用 `create_dir_all` 或任何等价物。
- **测试必须强制 socket 落到 `$TMPDIR` 回退路径**（见下方"测试环境前提"），否则 `remove_dir_all` 会顺手删掉 runtime 目录内的 socket，测试**假通过**。这是本计划最容易踩的坑。
- **测试基线**：`cd yi-agent-rs && cargo test -p yi-agent-store`、`cargo test -p yi-agent-app-server --lib`。新增用例只增不改既有断言。
- **不许并行跑 `cargo test --workspace`**（会 OOM）；按 crate 跑，跑前 `ps aux | grep cargo` 确认无残留。
- **提交前必须 `cargo fmt --all`**（在 `yi-agent-rs/` 下）。
- 提交信息用 conventional commits、中文，**不写** `Co-Authored-By`。
- **严禁在 `main` 上直接提交**：本计划在 worktree `.worktrees/stranded-daemon`（分支 `fix/stranded-daemon-self-stop`）内执行。

## 测试环境前提（执行者必读）

事故的关键结构之一是 **socket 不在 runtime 目录里**：`socket_path_for`（`ipc.rs:1095`）只在 `<runtime_dir>/runtime.sock` 超过 `MAX_SOCKET_PATH_BYTES = 103`（`ipc.rs:1046`）时才回退到 `$TMPDIR/yi-agent-<sha256[:16]>.sock`。

本机实测（`std::env::temp_dir()` 为 48 字符）：`tempfile::TempDir` 根只有约 61 字符，`<TempDir>/runtime/runtime.sock` 仅约 81 字符 —— **socket 会留在 runtime 目录内**。那样一来 `remove_dir_all(&runtime)` 会连带删掉 socket，任何"目录没了 → socket 也没了"的断言都会**假通过**，哪怕 daemon 完全没自停。

因此 Task 1、Task 2、Task 4 的 runtime 目录必须**深到强制回退**。做法：在 `TempDir` 下建一个长名子目录（模拟 kanban worktree slug），使 `<runtime_dir>/runtime.sock` 长度 > 103：

```rust
// TempDir 根 61 + "/" + slug + "/.yi-agent/runtime" + "/runtime.sock"
// slug 取 69 字符 → 约 161 字符，稳定超过 103，socket 必然落到 $TMPDIR。
const DEEP_SLUG: &str = "2026-10-17-backend-module-split-design-2026-10-17-backend-module-split";
```

每个相关测试都必须**先断言 socket 确实在 runtime 目录之外**（precondition 断言）。这既是忠实复刻事故结构，也是防"日后路径规则变化导致测试静默变成假通过"的护栏。

---

### Task 1: daemon 在 runtime 目录消失时自停

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（`start_with_factory` 的 accept 循环与循环尾清理；`DAEMON_ACCEPT_POLL` 附近加常量）
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`（追加到 `manually_stopped_daemon_releases_its_socket_and_instance_lock`（`:2837`）之后）

**Interfaces:**
- Consumes: `Daemon::start`（`ipc.rs:824`）、`Daemon::socket_path()`（`ipc.rs:1003`）。
- Produces: 无新公开 API。行为变更：daemon 检测到 `runtime_dir` 不存在时于 ≤1 秒内自行停止并清理 socket/lock。

- [ ] **Step 1: 写失败的测试**

在 `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs` 中，紧跟 `manually_stopped_daemon_releases_its_socket_and_instance_lock`（`:2837`）之后追加：

```rust
/// 事故根因：worktree 被 `git worktree remove --force` 删掉后，daemon 仍在服务
/// 一个不存在的 runtime 目录。socket 因长路径回退落在 `$TMPDIR` 而幸存，所以
/// 进程不死、每个请求都失败、且现有自愈（只认连接层错误）永不触发。
/// 修复：daemon 自己发现目录没了就停，把 socket 变成真的死的。
#[test]
fn a_daemon_stops_once_its_runtime_directory_is_gone() {
    let directory = TempDir::new().unwrap();
    // 必须深到强制 socket 回退到 $TMPDIR：否则 socket 留在 runtime 目录内，
    // `remove_dir_all` 会顺手删掉它，本测试将假通过（见计划"测试环境前提"）。
    let deep_slug = "2026-10-17-backend-module-split-design-2026-10-17-backend-module-split";
    let workdir = directory.path().join(deep_slug);
    let runtime = workdir.join(".yi-agent/runtime");
    let database = runtime.join("runtime.sqlite");

    let daemon = Daemon::start(&runtime, &database).unwrap();
    let socket = daemon.socket_path().to_path_buf();

    // precondition：socket 必须在 runtime 目录之外，本测试才忠实复刻事故结构。
    assert!(
        socket.exists(),
        "precondition: the daemon must have bound its socket"
    );
    assert!(
        !socket.starts_with(&runtime),
        "precondition: the runtime path must be long enough to push the socket into $TMPDIR, \
         otherwise removing the runtime directory would delete the socket too and this test \
         would pass without any fix. socket={}",
        socket.display()
    );

    // 模拟 worktree 回收：runtime 目录被删掉，daemon 进程还在。
    std::fs::remove_dir_all(&runtime).unwrap();

    // 检测是 1 秒节流，故有界等待而不是固定 sleep（参照
    // `daemon_stop_request_releases_the_runtime_for_a_future_manual_start`）。
    // socket 由 daemon 自己清理，所以它的消失就是"daemon 已经停了"的判据。
    let mut stopped = false;
    for _ in 0..240 {
        if !socket.exists() {
            stopped = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let _ = std::fs::remove_file(&socket); // 断言失败时也别把 $TMPDIR 节点留给下一位
    assert!(
        stopped,
        "the daemon must stop and release its socket once its runtime directory is gone"
    );

    // 关键反断言：自停不得把已删的目录"复活"。目录缺失是不可自愈条件，不是
    // "应该重建"的请求。若这里失败，说明清理路径里有 create_dir_all。
    assert!(
        !runtime.exists(),
        "stopping must not recreate the deleted runtime directory"
    );
}
```

- [ ] **Step 2: 运行测试确认它失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc a_daemon_stops_once_its_runtime_directory_is_gone`

Expected: FAIL，断言消息 `the daemon must stop and release its socket once its runtime directory is gone`（当前 daemon 不会因目录消失而停止，$TMPDIR 里的 socket 一直在，轮询耗尽约 6 秒）。**已实证**：禁用自停检查后此测试确实失败于此断言；启用后 1.04 秒通过（正好一次 1 秒节流周期）。

- [ ] **Step 3: 实现自停**

在 `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` 中：

**(a)** 在 `DAEMON_ACCEPT_POLL`（`:1510`）之后新增节流常量：

```rust
/// How often the accept loop asks whether the runtime directory it serves still
/// exists. A missing runtime directory is terminal and unrecoverable -- this
/// daemon's database lives there -- so the check is deliberately coarse: the
/// 10ms accept poll would stat a directory every tick for nothing.
const DAEMON_RUNTIME_DIR_CHECK: Duration = Duration::from_secs(1);
```

**(b)** 把 `runtime_dir` 的所有权带进 listener 线程。在 `let database_path = database_path.as_ref().to_path_buf();`（`:882`）之后加：

```rust
        let watched_runtime_dir = runtime_dir.to_path_buf();
```

（`runtime_dir` 在 `:844` 是 `&Path`，还在作用域内，`.to_path_buf()` 可用。）

**(c)** 在监听闭包内、`let mut last_schedule_minute = None;`（`:893`）之后加：

```rust
            let mut last_runtime_dir_check: Option<Instant> = None;
```

**(d)** 在 `while !thread_stop.load(Ordering::Acquire) {`（`:894`）循环体的**最前面**（`reconcile_worker_events` 之前）插入：

```rust
                // The directory this daemon serves can be removed underneath it
                // -- `git worktree remove --force` on the project does exactly
                // that, and the socket survives in $TMPDIR when the runtime path
                // is too long for `sun_path`. Every later request then fails to
                // open a database that no longer exists, while the socket stays
                // alive, so nothing upstream recognises the daemon as dead and it
                // retries forever. A missing runtime directory is terminal: say
                // why, then stop so the socket dies and the normal repair path
                // can take over.
                if last_runtime_dir_check
                    .map(|last| last.elapsed() >= DAEMON_RUNTIME_DIR_CHECK)
                    .unwrap_or(true)
                {
                    last_runtime_dir_check = Some(Instant::now());
                    if !watched_runtime_dir.exists() {
                        tracing::warn!(
                            runtime_dir = %watched_runtime_dir.display(),
                            "runtime directory disappeared; stopping this daemon so its socket can be replaced"
                        );
                        thread_stop.store(true, Ordering::Release);
                        break;
                    }
                }
```

`break` 之后循环尾部既有的 `remove_if_exists(&cleanup_socket)` / `remove_if_exists(&cleanup_lock)`（`:988` 附近）会自然执行，socket 与 lock 被清掉——**不新增清理代码**。

- [ ] **Step 4: 运行测试确认它通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc a_daemon_stops_once_its_runtime_directory_is_gone`

Expected: PASS。

- [ ] **Step 5: 跑既有 daemon 生命周期测试，确认零回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc daemon`

Expected: PASS（含 `manually_stopped_daemon_releases_its_socket_and_instance_lock`、`daemon_stop_request_releases_the_runtime_for_a_future_manual_start`、`daemon_stop_joins_an_incomplete_client_handler_before_releasing_runtime_files`、`daemon_reclaims_a_stale_lock_when_no_socket_is_listening`）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-store/src/ipc.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs
git commit -m "fix(store): daemon 在 runtime 目录消失时自停，终止悬空重试"
```

---

### Task 2: socket 身份护栏，防自停误删同路径的新 daemon socket

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（新增 `remove_socket_if_same_inode`；`start_with_factory` 记录 socket 身份；监听线程尾部改用它）
- Test: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（文件内新增 `#[cfg(test)] mod socket_identity_tests`，放在既有 `mod socket_path_tests` 之后）

**Interfaces:**
- Consumes: `remove_if_exists`（`ipc.rs:1178`）。
- Produces: `fn remove_socket_if_same_inode(path: &Path, expected: Option<(u64, u64)>)`（模块私有）。

**背景（为什么需要它）**：清理是按路径无条件 `unlink` 的。本改动把"目录删除 → 清理"变成**可达**路径：若目录被删后又被重建（kanban 重试正是同路径重建）、新 daemon 先起来，老 daemon 的清理会删掉**新 daemon 的 socket**，造出第二个悬空 daemon——同一 bug 的递归。

**为什么用单元测试而不是行为测试**：这条竞态要求"老 daemon 的清理恰好落在新 daemon bind 之后"，无法确定性复现；行为测试只能靠 `sleep` 赌时序，必然 flaky。护栏是**纯谓词**，直接对谓词做确定性单测才是正确判据。正常停止路径"删掉自己的 socket"已由既有测试 `manually_stopped_daemon_releases_its_socket_and_instance_lock`（Task 1 Step 5 会跑）覆盖。

- [ ] **Step 1: 写失败的测试**

在 `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` 中，紧跟 `mod socket_path_tests` 模块的结束大括号之后追加：

```rust
#[cfg(test)]
mod socket_identity_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::UnixListener;

    fn identity(path: &Path) -> (u64, u64) {
        let metadata = fs::symlink_metadata(path).expect("node must exist");
        (metadata.dev(), metadata.ino())
    }

    /// 正常情形：节点仍是我们绑定的那个，清理必须删掉它——否则 daemon 停了
    /// 却留下 socket 节点，下一个 attach 会连上一个没人监听的路径。
    #[test]
    fn a_socket_node_we_still_own_is_removed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("own.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let recorded = identity(&path);

        remove_socket_if_same_inode(&path, Some(recorded));

        assert!(!path.exists(), "our own socket node must be cleaned up");
        drop(listener);
    }

    /// 护栏本体：目录被删又被重建（kanban 重试正是同路径重建），新 daemon 先
    /// bind 了同一路径。老 daemon 的清理**不得**碰它——否则它会删掉新 daemon 的
    /// socket，制造出第二个悬空 daemon，正是本次修复要消灭的那类 bug 的递归。
    #[test]
    fn a_replacement_socket_at_the_same_path_is_left_alone() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("reused.sock");
        let first = UnixListener::bind(&path).unwrap();
        let stale_identity = identity(&path);
        // 换掉节点：unlink 后重新 bind，拿到一个 (dev, ino) 不同的新节点。
        // 注意 drop(listener) 本身**不会** unlink 文件——不显式删会拿到
        // EADDRINUSE（已实证：初稿漏了这行，测试报 `AddrInUse`）。
        drop(first);
        std::fs::remove_file(&path).unwrap();
        let _replacement = UnixListener::bind(&path).unwrap();
        assert_ne!(
            stale_identity,
            identity(&path),
            "precondition: rebinding the same path must yield a new inode"
        );

        remove_socket_if_same_inode(&path, Some(stale_identity));

        assert!(
            path.exists(),
            "a socket node bound by a replacement daemon must survive the outgoing daemon's cleanup"
        );
    }

    /// 没记下身份（例如 bind 时节点就已消失）时，宁可不删，也不要冒险删掉别人
    /// 的节点。
    #[test]
    fn an_unknown_identity_removes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("unknown.sock");
        let _listener = UnixListener::bind(&path).unwrap();

        remove_socket_if_same_inode(&path, None);

        assert!(
            path.exists(),
            "without a recorded identity the cleanup must not guess"
        );
    }

    /// 节点已经不在了也要安静通过（daemon 停止与手工删除并发时的常态）。
    #[test]
    fn a_vanished_node_is_not_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("absent.sock");

        remove_socket_if_same_inode(&path, Some((0, 0)));

        assert!(!path.exists());
    }
}
```

- [ ] **Step 2: 运行测试确认它失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib socket_identity_tests`

Expected: 编译失败，`cannot find function \`remove_socket_if_same_inode\``（该函数尚不存在）。

- [ ] **Step 3: 实现护栏**

**(a)** 在 `remove_if_exists`（`:1178`）之后新增：

```rust
/// Removes a socket node only if it is still the one we bound.
///
/// The daemon's exit path unlinks its socket by path. Between a runtime
/// directory being removed and rebuilt -- which is exactly what a kanban retry
/// does -- a replacement daemon can bind the same path first, and the outgoing
/// daemon would then unlink *its* socket, stranding the replacement the same way
/// this whole design exists to prevent. Comparing `(dev, ino)` deletes only the
/// node this daemon bound.
fn remove_socket_if_same_inode(path: &Path, expected: Option<(u64, u64)>) {
    use std::os::unix::fs::MetadataExt;
    // No recorded identity means we cannot tell our node from someone else's, so
    // leave the path alone rather than risk unlinking a live daemon's socket.
    let Some(expected) = expected else {
        return;
    };
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if (metadata.dev(), metadata.ino()) == expected {
        let _ = remove_if_exists(path);
    }
}
```

**(b)** 在 bind 成功之后记录身份。找到 `fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;`（`:869`），紧随其后加：

```rust
        // Record the socket node's identity so the exit path can tell our node
        // apart from a replacement bound at the same path after this directory
        // was rebuilt.
        let socket_identity = {
            use std::os::unix::fs::MetadataExt;
            fs::symlink_metadata(&socket_path)
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .ok()
        };
```

**(c)** 在 `let cleanup_lock = lock_path.clone();`（`:885`）之后加：

```rust
        let cleanup_socket_identity = socket_identity;
```

（监听闭包是 `move`，`Option<(u64, u64)>` 是 `Copy`，闭包会捕获它。）

**(d)** 把监听线程尾部 `let _ = remove_if_exists(&cleanup_socket);`（`:988`）替换为：

```rust
            remove_socket_if_same_inode(&cleanup_socket, cleanup_socket_identity);
```

`cleanup_lock` 的清理**保持不变**（lock 文件由 `InstanceLock` 的 `flock` 语义保护，且既有测试 `daemon_stop_joins_an_incomplete_client_handler_before_releasing_runtime_files` 断言它被删）。

- [ ] **Step 4: 运行测试确认它通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib socket_identity_tests`

Expected: PASS（4 条）。

- [ ] **Step 5: 跑 store 全量测试，确认零回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store`

Expected: PASS。重点确认 `daemon_stop_joins_an_incomplete_client_handler_before_releasing_runtime_files` 里对 `!runtime.join("runtime.sock").exists()` 的断言仍绿——该用例的 runtime 路径较短，socket 就在 runtime 目录内，身份匹配，清理照旧生效。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-store/src/ipc.rs
git commit -m "fix(store): 自停清理只删自己绑定的 socket 节点"
```

---

### Task 3: attach 入口拒绝已消失的项目目录

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:844-870`（`attach_cwd_runtime`）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`（追加到 `a_dead_runtime_is_repaired_on_the_next_tool_call`（`:889`）之后）

**Interfaces:**
- Consumes: `ProjectRuntimes`（`server.rs:147`）、`project_key`（`server.rs:304`）、既有测试辅助 `test_config`（`rpc_tests.rs:1423`）/ `init_git_repo`（`rpc_tests.rs:83`）/ `isolated_models_path`（`rpc_tests.rs:5` 已导入）。
- Produces: 行为变更：`attach_cwd_runtime` 在 `cfg.workdir` 不存在时摘缓存并返回 `Err(String)`，**不**创建任何目录。

- [ ] **Step 1: 写失败的测试**

追加到 `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs` 的 `a_dead_runtime_is_repaired_on_the_next_tool_call`（`:889`，函数结束大括号）之后：

```rust
/// A project directory that no longer exists must not be brought up: the
/// recovery path calls `create_dir_all` on the runtime directory, which would
/// silently recreate a deleted worktree's `.yi-agent/runtime` and hand back a
/// daemon serving a project git no longer knows about.
#[test]
fn a_vanished_project_directory_is_refused_and_not_recreated() {
    let repo = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let runtime = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();
    let workdir = cfg.workdir.clone();

    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));

    // The project directory is gone (the worktree was reclaimed).
    drop(repo);

    // `RuntimeBinding` 不实现 `Debug`，所以 `expect_err` 编译不过（已实证：
    // 报 `RuntimeBinding: Debug` unsatisfied）。用 `match`。
    let error = match attach_cwd_runtime(
        &runtimes,
        runtime.path(),
        &cfg,
        &isolated_models_path(runtime.path()),
    ) {
        Ok(_) => panic!("a vanished project directory must not attach"),
        Err(error) => error,
    };
    assert!(
        error.contains(&workdir.display().to_string()),
        "the error must name the missing directory, got: {error}"
    );

    // The refusal must not resurrect the directory it just reported missing.
    assert!(
        !workdir.exists(),
        "refusing to attach must not recreate the project directory"
    );

    // And the miss must not be cached: once the directory is back, the next call
    // must attach normally. Without dropping the cache entry this would stay
    // refused forever, turning a recoverable state into a dead end.
    std::fs::create_dir_all(&workdir).unwrap();
    init_git_repo(&workdir);
    attach_cwd_runtime(
        &runtimes,
        runtime.path(),
        &cfg,
        &isolated_models_path(runtime.path()),
    )
    .expect("a restored project directory must attach on the next call");
}
```

- [ ] **Step 2: 运行测试确认它失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_vanished_project_directory_is_refused_and_not_recreated`

Expected: FAIL。当前 `attach_cwd_runtime` 不判存在性，会尝试 bring-up；`error must name the missing directory` 与 `must not recreate the project directory` 至少一条先失败。

- [ ] **Step 3: 实现拦截**

把 `attach_cwd_runtime`（`server.rs:844`）的函数体开头改为：

```rust
fn attach_cwd_runtime(
    runtimes: &ProjectRuntimes,
    runtime_dir: &Path,
    cfg: &RuntimeConfig,
    models_path: &Path,
) -> Result<Arc<RuntimeBinding>, String> {
    let key = project_key(&cfg.workdir);
    // A project directory that no longer exists has nothing to bring up, and the
    // bring-up path creates the runtime directory -- which would resurrect
    // `.yi-agent/runtime` inside a worktree that was already reclaimed. Refuse
    // before consulting the cache, and *drop* any cached entry: the cache stores
    // failures on purpose so a doomed bring-up is not re-run on every thread, but
    // a missing directory is recoverable -- the project can come back -- and a
    // cached `Err` would lock that recovery out forever.
    if !cfg.workdir.exists() {
        runtimes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key);
        return Err(format!(
            "the project directory {} no longer exists",
            cfg.workdir.display()
        ));
    }
    if let Some(existing) = runtimes.lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
        return existing.clone();
    }
    // Own a *usable* runtime: adopt a healthy daemon, replace a dead or wedged
    // one. The binding it returns heals itself on later calls, so a daemon that
    // dies after this point (e.g. its terminal closes) no longer strands the
    // thread's delegation tools.
    let outcome = yi_agent_subagent::attach::ensure_owned_runtime(
        cfg,
        runtime_dir.to_path_buf(),
        models_path,
    )
    .map(|attached| {
        RuntimeBinding::managed(
            cfg,
            runtime_dir.to_path_buf(),
            models_path.to_path_buf(),
            Arc::new(attached),
        )
    })
    .map_err(|failure| format!("{}: {}", failure.stage, failure.cause));
    runtimes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, outcome.clone());
    outcome
}
```

- [ ] **Step 4: 运行测试确认它通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_vanished_project_directory_is_refused_and_not_recreated`

Expected: PASS。

- [ ] **Step 5: 跑既有 attach 测试，确认零回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_dead_runtime_is_repaired_on_the_next_tool_call`，再 `cargo test -p yi-agent-app-server --lib two_threads_in_one_cwd_share_the_daemon_but_not_the_root`（`cargo test` 一次只接一个过滤词）

Expected: PASS（健康的项目目录仍能 attach；已死 runtime 仍能被替换）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "fix(app-server): attach 入口拒绝已消失的项目目录，不复活已删 worktree"
```

---

### Task 4: 事故结构端到端复现（钉住收敛，而非只验谓词）

**Files:**
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`（追加到 Task 3 的用例之后）

**Interfaces:**
- Consumes: Task 1 的自停、Task 3 的拦截、`yi_agent_subagent::attach::probe_runtime` / `RuntimeProbe`（`rpc_tests.rs:900` 附近已在用）。
- Produces: 无新 API。

**为什么单独一个任务**：前三步各自钉住一个谓词，但事故的真正危险是"删目录后系统会自我延续"这个**结构**（daemon 悬空 + 缓存仍在 + 周期调用方不断重试 + 目录被复活）。此任务复刻该结构并断言收敛。

**本任务的 runtime_dir 必须与 workdir 相关且够深**——这是忠实复刻的前提：事故里 `runtime_dir == <worktree>/.yi-agent/runtime`，两者一起被删；且必须深到 socket 落 `$TMPDIR`，否则 socket 随目录一起消失，"daemon 自停"这一步就无从验证（假通过）。

- [ ] **Step 1: 写失败的测试**

追加到 `rpc_tests.rs`（Task 3 用例之后）：

```rust
/// The incident, end to end: a live project runtime, its worktree reclaimed (the
/// directory removed, the daemon still running), the cached binding still
/// pointing at it, and periodic callers still asking. The failure mode was never
/// "one wrong predicate" -- it was "the system keeps going": every request
/// re-fails forever. This test pins the whole structure.
#[test]
fn a_reclaimed_worktree_converges_instead_of_retrying_forever() {
    let directory = tempfile::TempDir::new().unwrap();
    // 深路径：强制 socket 回退到 $TMPDIR，忠实复刻"目录删了但 socket 还活着"。
    let deep_slug = "2026-10-17-backend-module-split-design-2026-10-17-backend-module-split";
    let workdir = directory.path().join(deep_slug);
    std::fs::create_dir_all(&workdir).unwrap();
    init_git_repo(&workdir);
    let mut cfg = test_config();
    cfg.workdir = workdir.clone();
    // 与事故一致：runtime 目录就在项目目录里，一起被回收。
    let runtime = workdir.join(".yi-agent/runtime");

    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let binding = attach_cwd_runtime(
        &runtimes,
        &runtime,
        &cfg,
        &isolated_models_path(&runtime),
    )
    .expect("a git project must attach");
    let socket = binding.current().unwrap().socket_path.clone();
    assert_eq!(
        yi_agent_subagent::attach::probe_runtime(&socket),
        yi_agent_subagent::attach::RuntimeProbe::Healthy,
        "the runtime must be live before we reclaim its directory"
    );
    assert!(
        !socket.starts_with(&runtime),
        "precondition: the socket must live outside the runtime directory, or reclaiming the \
         directory would delete it and the daemon-stopped step below would prove nothing"
    );

    // Reclaim the worktree: the project directory disappears, the daemon it
    // started is still running, the binding stays cached -- exactly the incident.
    drop(binding);
    std::fs::remove_dir_all(&workdir).unwrap();

    // (a) The entry point refuses and does not resurrect the directory.
    let error =
        match attach_cwd_runtime(&runtimes, &runtime, &cfg, &isolated_models_path(&runtime)) {
            Ok(_) => panic!("a reclaimed worktree must not attach"),
            Err(error) => error,
        };
    assert!(
        error.contains(&workdir.display().to_string()),
        "the error must name the missing directory, got: {error}"
    );
    assert!(
        !workdir.exists(),
        "the refusal must not recreate the reclaimed project directory"
    );

    // (b) The stranded daemon notices its runtime directory is gone and stops, so
    //     its socket dies -- which is what lets the normal repair path recognise
    //     it. This is the step whose absence caused the 71k retries.
    let mut socket_died = false;
    for _ in 0..240 {
        if !socket.exists() {
            socket_died = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let _ = std::fs::remove_file(&socket);
    assert!(
        socket_died,
        "the daemon must stop once its runtime directory is gone, or nothing upstream ever sees it die"
    );

    // (c) Repeated calls converge: each is refused in constant time and none of
    //     them recreates the directory. (The incident's retries came from periodic
    //     callers like the board prefetch; this loop stands in for those.)
    for _ in 0..5 {
        assert!(
            attach_cwd_runtime(&runtimes, &runtime, &cfg, &isolated_models_path(&runtime)).is_err(),
            "a reclaimed worktree stays refused"
        );
    }
    assert!(
        !workdir.exists(),
        "repeated refusals must not recreate the reclaimed project directory"
    );
}
```

- [ ] **Step 2: 确认它在修复前会失败**

临时把 `attach_cwd_runtime` 的存在性拦截注释掉（或 `git stash` Task 3 的改动），运行：

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_reclaimed_worktree_converges_instead_of_retrying_forever`

Expected: FAIL（无拦截时目录会被重建，或 daemon 不自停导致 `socket_died` 为假）。

- [ ] **Step 3: 恢复实现，确认测试通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_reclaimed_worktree_converges_instead_of_retrying_forever`

Expected: PASS。

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "test(app-server): 复刻 worktree 回收事故结构，钉住收敛"
```

---

### Task 5: 登记 bug 并跑全量验证

**Files:**
- Modify: `docs/bug-list.md`（未修复条目区，`:30` 一带）

**Interfaces:**
- Consumes: 无。
- Produces: 无。

- [ ] **Step 1: 追加 bug-list 条目**

在 `docs/bug-list.md` 未修复条目区（`- [ ] 当前一些测试需要手工测试验证...` 一带）追加一行，登记为已修复：

```markdown
- [x] worktree 被 `git worktree remove --force` 删除后，服务它的 daemon 悬空存活并无限重试（根因：**生命周期缺陷，三层叠加**。(1) daemon 服务的 `runtime.sqlite` 随 worktree 目录消失，但 socket 因长路径回退落在 `$TMPDIR`（`yi-agent-store/src/ipc.rs:1095` `socket_path_for`，`MAX_SOCKET_PATH_BYTES=103`）而幸存，进程不死；(2) 系统里所有自愈都建立在"socket 死了"这个信号上，而本故障恰恰让 socket 活着——`is_liveness_error`（`yi-agent-subagent/src/binding.rs:267`）只认连接层错误，`unable to open database file` 是 daemon **正常应答**的协议级错误码，故 binding 不修复、`probe_runtime` 仍答 `Healthy`；(3) `ProjectRuntimes` 缓存（`server.rs:147`）只插不删、`detach_unused_runtimes` 唯一调用点是 `thread/delete`，而裸 `git worktree remove` 根本不走那条路。现场取证：`session-20261018-050932.jsonl` 单文件 71,651 条、对应 71,623 个不同 `request_id`（说明重试来自外部周期源——看板 prefetch 1s / `watch_children` 500ms / 看板 tick 3s / supervisor 500ms，而非 daemon 内部循环）；删除命令 05:24:48.370 与第一条错误 05:24:48.760 **相隔 0.39 秒**、路径一致——执行删除的 agent 就住在托管该 daemon 的 app-server 进程里，是自杀式因果；该错误最早可追溯到 2026-10-03。修复：daemon 侧 accept 循环加 1 秒节流的存在性检查，目录消失即打 WARN（真实原因）+ 停止 + 清理 socket/lock（`ipc.rs`）；清理加 `(dev,ino)` 身份护栏，防误删同路径重建的新 daemon socket；app-server 侧 `attach_cwd_runtime`（`server.rs:844`）在查缓存之前判 `cfg.workdir` 是否存在，缺失则摘缓存 + 返回明确错误、不 `create_dir_all`，从而不复活已删 worktree。**测试前提**：相关用例必须用深路径强制 socket 落 `$TMPDIR`，否则 `remove_dir_all` 会连带删掉 socket 而假通过。验证：`cargo test -p yi-agent-store`（含 `a_daemon_stops_once_its_runtime_directory_is_gone`、`socket_identity_tests` 4 例）、`cargo test -p yi-agent-app-server --lib`（含 `a_vanished_project_directory_is_refused_and_not_recreated`、`a_reclaimed_worktree_converges_instead_of_retrying_forever`）— [设计](../superpowers/specs/2026-10-18-stranded-daemon-self-stop-design.md)
```

- [ ] **Step 2: 跑两个 crate 的全量测试**

Run: `ps aux | grep -v grep | grep cargo`（确认无残留）→ 然后

Run: `cd yi-agent-rs && cargo test -p yi-agent-store && cargo test -p yi-agent-app-server --lib`

Expected: 全绿，无既有断言回归。

- [ ] **Step 3: 确认格式**

Run: `cd yi-agent-rs && cargo fmt --all -- --check`

Expected: 无输出（通过）。

- [ ] **Step 4: 提交，并按项目规范合回 main**

```bash
cd .. && git add docs/bug-list.md docs/superpowers/specs/2026-10-18-stranded-daemon-self-stop-design.md docs/superpowers/plans/2026-10-18-stranded-daemon-self-stop.md
git commit -m "docs: 登记悬空 daemon bug，附 spec 与 plan"
```

然后按 `superpowers:finishing-a-development-branch`：回 `main` → `git merge --no-ff fix/stranded-daemon-self-stop` → 删分支 + 移除 worktree。

---

## Self-Review

**1. Spec coverage**

| Spec 小节 | 对应 Task |
|---|---|
| §3.2 daemon 自停（节流检查 + WARN + 复用既有清理） | Task 1 |
| §3.2 socket 身份护栏 | Task 2 |
| §3.3 attach 入口拦截（查缓存前判存在 + 摘缓存） | Task 3 |
| §3.1 职责划分（daemon 判 runtime_dir / app-server 判 workdir） | Task 1 + Task 3 |
| §4.1 daemon 自停测试 | Task 1 Step 1 |
| §4.2 socket 身份护栏测试 | Task 2 Step 1 |
| §4.3 attach 拦截测试（含"重建后能 attach"） | Task 3 Step 1 |
| §4.4 事故结构复现（收敛） | Task 4 |
| §5 落点清单（含 `docs/bug-list.md`） | Task 1–5 |
| §2.2 非目标（不动 wire / 不动 `log_request_failure` / 不做回收钩子） | Global Constraints |
| §6 风险（清理时机 / 测试时序 / 不重建断言） | Global Constraints + "测试环境前提" |

无遗漏。

**2. Placeholder scan**

无 TBD / TODO / "类似 Task N"；每个代码步骤都给了可直接落盘的完整代码块。

**3. Type consistency**

- `remove_socket_if_same_inode(path: &Path, expected: Option<(u64, u64)>)`：Task 2 Step 3(a) 定义，Step 1 的 4 条测试与 Step 3(d) 的调用处签名一致。
- `cleanup_socket` / `cleanup_socket_identity` / `socket_identity` / `watched_runtime_dir` / `last_runtime_dir_check`：Task 1 与 Task 2 之间命名一致。
- `attach_cwd_runtime(runtimes, runtime_dir, cfg, models_path)` 签名未变，Task 3 / Task 4 调用一致。
- `DAEMON_RUNTIME_DIR_CHECK`：Task 1 定义与使用一致。
- 测试变量 `deep_slug` 在 Task 1（store 集成测试）与 Task 4（app-server 测试）中各自局部定义，值相同，无跨文件依赖。

**4. 关键修正（相对初稿）**

- **Task 1 / Task 4 的 socket 位置**：初稿用 `TempDir` 根作 runtime_dir，实测只有约 81 字符，socket 会留在 runtime 目录内、被 `remove_dir_all` 连带删除，测试假通过。已改为深路径 + precondition 断言。
- **Task 2 的测试层次**：初稿想用行为测试赌"老 daemon 清理撞上新 daemon bind"的时序，那无法确定性复现、必然 flaky。已改为对纯谓词 `remove_socket_if_same_inode` 的确定性单测，正常停止路径交由既有测试覆盖。
- **`drop(UnixListener)` 不 unlink 文件**：Task 2 的"同路径换节点"用例必须先 `std::fs::remove_file` 再重新 `bind`，否则报 `EADDRINUSE`。已实证并修正。
- **`RuntimeBinding` 不实现 `Debug`**：Task 3 / Task 4 不能用 `.expect_err()`（编译不过），改用 `match`。已实证并修正。
- **`cargo test` 一次只接一个过滤词**：Task 3 Step 5 拆成两条命令。

**5. 实证验证记录（本计划已实际执行过一遍）**

本计划的任务在编写时已在 worktree 内**完整实现并运行**，不是纸面推演：

| 验证 | 命令 | 结果 |
|---|---|---|
| Task 1 自停 | `cargo test -p yi-agent-store --test runtime_ipc a_daemon_stops_once_its_runtime_directory_is_gone` | PASS（1.04s，正好一次节流周期） |
| Task 2 护栏 4 例 | `cargo test -p yi-agent-store --lib socket_identity_tests` | PASS（4/4） |
| Task 3 拦截 | `cargo test -p yi-agent-app-server --lib a_vanished_project_directory_is_refused_and_not_recreated` | PASS |
| Task 4 事故复现 | `cargo test -p yi-agent-app-server --lib a_reclaimed_worktree_converges_instead_of_retrying_forever` | PASS |
| **负向验证**（禁用自停检查 + 禁用 attach 拦截后重跑） | 同上四条 | **全部 FAILED** —— 证明它们不是假通过 |
| store 全量 | `cargo test -p yi-agent-store` | 全绿 |
| app-server 全量 | `cargo test -p yi-agent-app-server --lib` | **761 passed**（基线 759 + 新 2 条） |

负向验证失败信息（摘录）：Task 1 报 `the daemon must stop and release its socket once its runtime directory is gone`；Task 3 报 `a vanished project directory must not attach`；Task 4 报 `a reclaimed worktree must not attach`。