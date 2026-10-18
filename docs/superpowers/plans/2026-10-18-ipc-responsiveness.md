# IPC Responsiveness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop synchronous IPC from stalling the desktop `app-server` main loop, and make the plugin/daemon request paths cheap.

**Architecture:** Three independent fixes across three crate trees. (1) The plugin's query server replaces its 200ms blind accept-poll with `poll(2)`. (2) The `app-server` detaches its `board/*` and `plugin/*` RPC domains onto the multi-threaded runtime (they touch zero main-loop mutable state), and prefetches board cards for the 3s scheduler tick. (3) The daemon reuses one tokio runtime and one sqlite repository per daemon instead of rebuilding them per request.

**Tech Stack:** Rust 2024 (edition 2024, rust-version 1.85), tokio (multi-thread runtime), rusqlite, serde_json, libc, Unix domain sockets.

## Global Constraints

- Work only in the worktree `.worktrees/ipc-responsiveness` (branch `perf/ipc-responsiveness`). Never commit to `main`.
- Before any `cargo test`, run `ps aux | grep -v grep | grep cargo` and confirm it is empty; run tests per crate (`-p <crate>`), never `--workspace`.
- Run `cargo fmt --all` in `yi-agent-rs/` (and in `plugins/superpowers-kanban/`) before committing.
- Commit messages use conventional-commit prefixes; **no** `Co-Authored-By` line.
- **Never** kill or restart running processes: `superpowers-kanban` (pid 43799) and `yi-agent daemon serve` (pid 3132). Only read-only probing.
- The plugin's `serve_with` doc contract stays: blocking call, caller places it on its own thread.
- Do not change `PROTOCOL_VERSION`, response envelope shapes, or any RPC response's JSON shape.
- English comments in `yi-agent-rs`; the plugin crates use Chinese comments (follow each file's existing language).

---

### Task 1: Plugin query server uses `poll(2)` instead of a 200ms accept sleep

**Rationale:** `serve_with` binds non-blocking and, on `WouldBlock`, sleeps a full `ACCEPT_POLL` (200ms). A query that arrives just after the sleep starts waits ~200ms, so `list` latency is bimodal (0ms or ~271ms, measured). Replacing the sleep with `poll(POLLIN, ACCEPT_POLL)` returns the moment a connection arrives while keeping the `keep_going` check cadence identical — so both existing shutdown tests keep passing unchanged.

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/Cargo.toml`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs:36-40` (constants), `:88-123` (`serve_with`), `:124+` (tests)
- Test: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: `serve_with(socket, dispatch, keep_going)` keeps its exact signature; adds private `fn wait_readable(listener: &UnixListener, timeout: Duration) -> std::io::Result<bool>`.

- [ ] **Step 1: Add the `libc` dependency**

Edit `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/Cargo.toml`, adding `libc` under `[dependencies]`:

```toml
[dependencies]
serde.workspace = true
sha2 = "0.10"
serde_json.workspace = true
libc = "0.2"
```

This needs no `[workspace.dependencies]` entry; the lock already contains `libc 0.2.189`.

- [ ] **Step 2: Write the failing test**

Add to `mod tests` in `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs` (place it after `a_client_that_connects_and_leaves_does_not_stop_serving`):

```rust
    /// 回归：accept 不再盲睡 200ms。三个**连续**查询的总耗时必须远小于
    /// `ACCEPT_POLL`——旧实现里每个查询都要等下一次轮询醒来(实测每个 ~200ms,
    /// 三个合计 ~600ms)，新实现每个 ~1ms。
    #[test]
    fn consecutive_queries_do_not_wait_for_the_accept_poll() {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_path(dir.path()).expect("temp dir fits");
        let dispatch = fake();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let path = socket.clone();
        let handle = std::thread::spawn(move || {
            serve_with(&path, dispatch, || {
                !flag.load(std::sync::atomic::Ordering::SeqCst)
            })
            .unwrap()
        });

        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let start = std::time::Instant::now();
        for _ in 0..3 {
            let mut stream = UnixStream::connect(&socket).expect("must connect");
            stream
                .write_all(br#"{"type":"plugin.query","method":"list","params":{}}"#)
                .unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            let reply: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(reply["type"], "plugin.result", "{reply}");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < ACCEPT_POLL,
            "three back-to-back queries took {elapsed:?}; the accept path is still sleeping"
        );

        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        handle.join().unwrap();
    }
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-ipc consecutive_queries_do_not_wait_for_the_accept_poll`
Expected: FAIL — `three back-to-back queries took ~600ms; the accept path is still sleeping`.

- [ ] **Step 4: Add the `wait_readable` helper**

In `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs`, update the imports and constants block (currently lines 11-14 and 36-38):

```rust
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
```

Keep `CLIENT_READ_TIMEOUT` and `ACCEPT_POLL` as they are, and update `ACCEPT_POLL`'s doc comment:

```rust
/// `keep_going` 的检查粒度，兼作 accept 的等待上限。
///
/// 连接到达时 [`wait_readable`] 立刻返回，故这个值**不再**是每次查询的固有
/// 延迟；它只是「多久回头看一眼 `keep_going`」。
const ACCEPT_POLL: Duration = Duration::from_millis(200);
```

Add the helper immediately above `serve_with`:

```rust
/// 等到 listener 可读(有连接待 accept)，或超时。
///
/// 返回 `true` 表示可以 `accept`；`false` 表示超时——调用方借此保持
/// `keep_going` 的检查粒度，同时不给连接引入 `ACCEPT_POLL` 的固有延迟。
fn wait_readable(listener: &UnixListener, timeout: Duration) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let milliseconds = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: `descriptor` 是一个已初始化的 `pollfd`，`poll` 只读它的 `fd`/`events`
    // 并写 `revents`；`nfds = 1` 与之匹配。没有其它指针参数。
    let ready = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
    match ready {
        // 超时：没有连接。
        0 => Ok(false),
        // 一个待 accept 的连接（或挂断等情况，`accept` 会给出具体错误）。
        count if count > 0 => Ok(true),
        _ => Err(std::io::Error::last_os_error()),
    }
}
```

- [ ] **Step 5: Rewrite the accept loop in `serve_with`**

Replace the body from `listener.set_nonblocking(true)?;` through the end of the `while` loop with:

```rust
    // 仍设非阻塞：`accept` 只在 `poll` 说可读之后调用，但两者之间对端可能已
    // 断开，那时我们要拿到 `WouldBlock` 而不是卡住。
    listener.set_nonblocking(true)?;
    while keep_going() {
        // 连接一到就返回；超时才回到 `keep_going` 检查（粒度不变）。
        if !wait_readable(&listener, ACCEPT_POLL)? {
            continue;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // 单条连接的失败不得拖垮监听：记下就走，继续服务下一个。
                if let Err(error) = serve_connection(dispatch.as_ref(), stream) {
                    eprintln!("superpowers-kanban: query connection failed: {error}");
                }
            }
            // `poll` 与 `accept` 之间对端断开：等下一次事件即可。
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
```

- [ ] **Step 6: Run the plugin test suite**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-ipc`
Expected: PASS, including `consecutive_queries_do_not_wait_for_the_accept_poll`, `the_socket_sits_where_the_manifest_says`, `a_query_is_forwarded_with_its_params_and_answered_verbatim`, `a_failed_dispatch_becomes_an_error_frame`, `malformed_frames_are_refused_instead_of_panicking`, and both `stop.store(true)` shutdown tests.

- [ ] **Step 7: Run the whole plugin workspace tests**

Run: `cd plugins/superpowers-kanban && cargo test`
Expected: PASS (all three crates).

- [ ] **Step 8: Format and commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/ipc-responsiveness
git add plugins/superpowers-kanban/crates/superpowers-kanban-ipc/Cargo.toml plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs plugins/superpowers-kanban/Cargo.lock
git commit -m "perf(kanban-ipc): poll for connections instead of sleeping 200ms"
```

---

### Task 2: Daemon reuses one runtime and one repository per daemon

**Rationale:** `respond()` builds a fresh current-thread tokio runtime and reopens the sqlite repository (`Connection::open` + two PRAGMAs + `migrate`'s schema parse) on **every** request. Measured `Status` p50 = 45ms on a path that touches no plugin. Reuse removes that per-request cost.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:3094-3103` (`respond`), `:910-925` (daemon accept loop, to build the context once), `:1452-1458` (`handle_client` signature)
- Test: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub(crate) struct RequestContext` with `fn new(database_path: &Path) -> Result<Self, IpcError>`, `fn with_repository<R>(&self, f: impl FnOnce(&mut RuntimeRepository) -> Result<R, IpcError>) -> Result<R, IpcError>`, `fn reconcile_if_stale(&self, coordinator: &Arc<RuntimeCoordinator>) -> Result<(), IpcError>`.
  - `fn should_reconcile(last: Option<Instant>, now: Instant, window: Duration) -> bool`.
  - `handle_client` **gains** a `context: &RequestContext` parameter while **keeping** its existing `database_path: &Path` (that path is still used at four other sites inside the function: the trace-subscription arms at `:1524`/`:1544`, `preview_cancel` at `:1558`, and the `SubscribeTrace` arm at `:1571`).

- [ ] **Step 1: Write the failing tests for the reconcile window**

Add to `mod tests` in `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`:

```rust
    #[test]
    fn should_reconcile_only_outside_the_window() {
        let window = Duration::from_millis(5);
        let base = Instant::now();
        // 从未对账过：必须跑。
        assert!(should_reconcile(None, base, window));
        // 刚跑过：窗口内跳过。
        assert!(!should_reconcile(Some(base), base + Duration::from_millis(1), window));
        // 窗口已过：再跑一次。
        assert!(should_reconcile(Some(base), base + Duration::from_millis(6), window));
    }

    #[test]
    fn a_request_context_reuses_one_repository() {
        let dir = tempfile::tempdir().unwrap();
        let database_path = dir.path().join("runtime.sqlite");
        let context = RequestContext::new(&database_path).expect("context builds");
        // 同一个连接可重复使用；两次读取给出同一个 schema 版本。
        let first = context
            .with_repository(|repository| repository.schema_version().map_err(IpcError::from))
            .expect("first read");
        let second = context
            .with_repository(|repository| repository.schema_version().map_err(IpcError::from))
            .expect("second read");
        assert_eq!(first, second);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib should_reconcile_only_outside_the_window a_request_context_reuses_one_repository`
Expected: FAIL to compile — `cannot find function should_reconcile` / `cannot find type RequestContext`.

- [ ] **Step 3: Add `should_reconcile` and `RequestContext`**

In `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`, add just above `fn respond(`:

```rust
/// 是否该在本次请求里跑一轮 worker 对账。
///
/// daemon 的 accept 循环已经每轮跑 [`RuntimeCoordinator::reconcile_worker_events`]，
/// `respond` 里那次是重复的。窗口只用来合并「几乎同时到达」的请求，保证请求仍能
/// 观察到已对账状态，而不是把对账从请求路径上彻底删掉。
fn should_reconcile(last: Option<Instant>, now: Instant, window: Duration) -> bool {
    match last {
        Some(previous) => now.duration_since(previous) >= window,
        None => true,
    }
}

/// 每个 daemon 一份的请求执行上下文：复用一个 tokio runtime 与一条 sqlite 连接。
///
/// `respond` 原先**每请求**都新建这两样（`Runtime::new` + `RuntimeRepository::open`
/// 里的两条 PRAGMA 与 `migrate` 的 schema 解析），单次 `Status` 因此要 ~45ms。
/// `rusqlite::Connection` 是 `!Sync`，故用 `Mutex` 串行持有——daemon 的请求路径
/// 本就串行，不引入新的并发度。
pub(crate) struct RequestContext {
    runtime: tokio::runtime::Runtime,
    repository: std::sync::Mutex<RuntimeRepository>,
    reconciled_at: std::sync::Mutex<Option<Instant>>,
    reconcile_window: Duration,
}

/// 同一窗口内的并发请求合并成一次对账。
const RECONCILE_WINDOW: Duration = Duration::from_millis(5);

impl RequestContext {
    pub(crate) fn new(database_path: &Path) -> Result<Self, IpcError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let repository = RuntimeRepository::open(database_path).map_err(IpcError::from)?;
        Ok(Self {
            runtime,
            repository: std::sync::Mutex::new(repository),
            reconciled_at: std::sync::Mutex::new(None),
            reconcile_window: RECONCILE_WINDOW,
        })
    }

    /// 借出那条长生命周期连接。调用方在闭包内串行使用它。
    pub(crate) fn with_repository<R>(
        &self,
        f: impl FnOnce(&mut RuntimeRepository) -> Result<R, IpcError>,
    ) -> Result<R, IpcError> {
        let mut repository = self
            .repository
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut repository)
    }

    /// 需要时跑一轮对账；窗口内重复调用直接返回。
    pub(crate) fn reconcile_if_stale(
        &self,
        coordinator: &Arc<RuntimeCoordinator>,
    ) -> Result<(), IpcError> {
        let now = Instant::now();
        let last = *self
            .reconciled_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !should_reconcile(last, now, self.reconcile_window) {
            return Ok(());
        }
        self.runtime.block_on(coordinator.reconcile_worker_events())?;
        *self
            .reconciled_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
        Ok(())
    }
}
```

Confirm `use std::time::{Duration, Instant};` is present in the file's import block (add `Instant` if only `Duration` is imported).

- [ ] **Step 4: Rewrite `respond` to take the context**

Replace `fn respond(...)`'s signature and opening lines (currently `:3094-3097`):

```rust
fn respond(
    context: &RequestContext,
    coordinator: &Arc<RuntimeCoordinator>,
    request: IpcRequest,
) -> Result<IpcResponse, IpcError> {
    context.reconcile_if_stale(coordinator)?;
    let mut repository = context
        .repository
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match request {
```

The rest of `respond`'s body already uses `repository` as a `&mut RuntimeRepository` (e.g. `repository.latest_event_id()?`), so no further edits are needed inside the match. Confirm that any arm which currently opens its own repository or builds its own runtime is left untouched (e.g. `SpawnChild` builds a runtime for `spawn_child_and_admit`; only remove a per-arm repository *open* if one exists, and if so replace it with `context.with_repository(...)`).

- [ ] **Step 5: Build the context once in the daemon and thread it through**

In the daemon accept loop setup (`yi-agent-rs/crates/yi-agent-store/src/ipc.rs`), add the context right after the path is turned into an owned `PathBuf` at `:868` and **before** the accept thread is spawned at `:871` (that closure moves `database_path`, so the context must be built first):

```rust
        let database_path = database_path.as_ref().to_path_buf();
        let request_context = Arc::new(RequestContext::new(&database_path)?);
        let cleanup_socket = socket_path.clone();
        let cleanup_lock = lock_path.clone();
```

Then inside the loop, next to the other `Arc::clone`s for the spawned handler, add:

```rust
                        let request_context = Arc::clone(&request_context);
```

and pass it to the handler:

```rust
                        let handler = thread::spawn(move || {
                            let _ = handle_client(
                                stream,
                                &request_context,
                                &stop,
                                &coordinator,
                                &confirmations,
                            );
                        });
```

Update `handle_client`'s signature (`:1452`) — **add** `context` and keep `database_path`:

```rust
fn handle_client(
    mut stream: UnixStream,
    database_path: &Path,
    context: &RequestContext,
    stop: &Arc<AtomicBool>,
    coordinator: &Arc<RuntimeCoordinator>,
    confirmations: &Arc<ConfirmationStore>,
) -> Result<(), IpcError> {
```

and at its `respond` call site (`:1584`) change only that call to:

```rust
            request => match respond(context, coordinator, request) {
```

Leave the other four `database_path` uses untouched. In the accept loop, keep `let database_path = database_path.clone();` and add `let request_context = Arc::clone(&request_context);`, passing both into the spawned closure.

- [ ] **Step 6: Run the crate's tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib`
Expected: PASS, including `should_reconcile_only_outside_the_window` and `a_request_context_reuses_one_repository`. If a pre-existing test asserted per-request reconcile timing, per spec §5 fall back to keeping a per-request reconcile (drop `reconcile_if_stale` from `respond`) while still reusing the context.

- [ ] **Step 7: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/ipc-responsiveness
git add yi-agent-rs/crates/yi-agent-store/src/ipc.rs
git commit -m "perf(daemon): reuse one runtime and repository per daemon"
```

---

### Task 3: Daemon accept loop polls instead of sleeping 10ms

**Rationale:** The daemon's listener sets non-blocking and sleeps 10ms on `WouldBlock`, adding 0-10ms of baseline latency and ~100 wakeups/sec to every request. Same `poll(2)` treatment as Task 1.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:858-859` (`set_nonblocking`), `:915-948` (accept match), and the `listener.accept` error arm
- Test: covered structurally by Task 2's tests plus the existing daemon tests; no new timing test.

**Interfaces:**
- Consumes: `RequestContext` from Task 2.
- Produces: private `fn daemon_accept_wait(listener: &UnixListener, timeout: Duration) -> std::io::Result<bool>`.

- [ ] **Step 1: Add the wait helper**

Add above `fn handle_client(` in `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`:

```rust
/// 等 listener 可读，或超时（粒度同 [`DAEMON_ACCEPT_POLL`]）。
///
/// 与插件侧同款：用 `poll(2)` 取代「非阻塞 + sleep」，连接一到就返回，避免给每个
/// 请求叠加 0-10ms 的盲等，同时把 `thread_stop` 的检查保持在固定节奏。
fn daemon_accept_wait(listener: &UnixListener, timeout: Duration) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let milliseconds = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: 单个已初始化的 `pollfd`，`nfds = 1` 与之一致；无其它指针参数。
    let ready = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
    match ready {
        0 => Ok(false),
        count if count > 0 => Ok(true),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// accept 等待粒度：只决定 `thread_stop` 多久被看一次，不再是请求延迟。
const DAEMON_ACCEPT_POLL: Duration = Duration::from_millis(10);
```

Ensure `use std::os::unix::io::AsRawFd;` is imported in this file. No new dependency is needed: `yi-agent-store/Cargo.toml` already declares `libc = "0.2"` (`:27`).

- [ ] **Step 2: Replace the sleep with the poll gate**

In the daemon accept loop, replace the `match listener.accept()` block's wait behaviour. **Note:** the enclosing `thread::spawn(move || { ... })` closure returns `()` and contains no `?` — so the wait result must be handled explicitly, not with `?`:

```rust
                let ready = match daemon_accept_wait(&listener, DAEMON_ACCEPT_POLL) {
                    Ok(ready) => ready,
                    Err(error) => {
                        tracing::error!(error = %error, "daemon accept poll failed; retrying");
                        continue;
                    }
                };
                if !ready {
                    continue;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        // ...existing handler spawn, unchanged...
                    }
                    // `poll` 与 `accept` 之间对端断开：等下一次事件即可。
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
```

Delete the old `Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => { thread::sleep(Duration::from_millis(10)); }` arm. Keep `listener.set_nonblocking(true)?;` as-is (it prevents `accept` from blocking between the poll and the accept).

- [ ] **Step 3: Run the crate's tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib`
Expected: PASS.

- [ ] **Step 4: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/ipc-responsiveness
git add yi-agent-rs/crates/yi-agent-store/src/ipc.rs
git commit -m "perf(daemon): poll for connections instead of sleeping 10ms"
```

---

### Task 4: Detach `board/*` and `plugin/*` RPC handling from the main loop

**Rationale:** These two domains' handlers touch **no** main-loop mutable state (only `hub`, `client`, `board_dir`, `resident_dir`, `plugin_command`, `theme_handle`, `watchman_*`, `board_launcher` — all owned/`Arc`/`Clone`). The main runtime is multi-threaded (`Runtime::new()`, 18 workers), so `tokio::spawn` runs them on another worker and the main loop returns to `select!` immediately. Measured: `board/list` and `plugin/query` are the two largest blocking paths (up to 527ms per desktop tick).

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs` (add `BoardPluginState`)
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/board.rs` (handler signature)
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/plugin.rs` (handler signature)
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:2718-2737` (the two arms), `:2387` (channel setup), `:2479` (select! branch)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub(crate) struct BoardPluginState` in `rpc/mod.rs` with fields `hub: Arc<Broadcaster>`, `client: ClientId`, `board_dir: PathBuf`, `resident_dir: PathBuf`, `plugin_command: PathBuf`, `theme_handle: ThemeHandle`, `watchman_home: PathBuf`, `watchman_install: WatchmanInstall`, `watchman_uninstall: WatchmanUninstall`, `board_launcher: BoardLauncher`, `fatal_tx: mpsc::Sender<anyhow::Error>`.
  - `pub(crate) async fn board::handle_shared(state: &BoardPluginState, req: &RequestEnvelope, id: RequestId) -> anyhow::Result<Flow>` and the same for `plugin::handle_shared`.
  - `pub(crate) async fn write_or_fatal(state: &BoardPluginState, id: RequestId, resp: ResponseEnvelope)`.
  - **Prerequisite visibility change:** `BoardLauncher`, `WatchmanInstall`, `WatchmanUninstall` are currently private `type` aliases in `server.rs` (`:229`, `:233`, `:237`). This task must widen all three to `pub(crate) type`. `BoardCard` is already `pub(crate)` (`server.rs:1373`).

- [ ] **Step 1: Write the failing test**

Add to `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`:

```rust
/// detach 的前提：这两个域的状态必须是 `Send + Sync + 'static`，否则
/// `tokio::spawn` 编译不过。这条断言就是「它们能脱离主循环执行」的编译期证明
/// ——主循环是单 task，内联 `await` 会串行堵住所有 thread 的 RPC。
#[test]
fn board_and_plugin_shared_state_is_owned_and_send() {
    fn assert_spawnable<T: Send + Sync + 'static>() {}
    assert_spawnable::<crate::rpc::BoardPluginState>();
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib board_and_plugin_shared_state_is_owned_and_send`
Expected: FAIL to compile — `cannot find type BoardPluginState in crate::rpc`.

- [ ] **Step 3: Widen the three type aliases, then add `BoardPluginState` and the write helper**

First, in `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`, widen the three aliases (`:229`, `:233`, `:237`) from `type` to `pub(crate) type`:

```rust
pub(crate) type BoardLauncher = Arc<dyn Fn(&Path) -> Result<bool, String> + Send + Sync>;
pub(crate) type WatchmanInstall = Arc<dyn Fn(&Path, &Path) -> Result<(), String> + Send + Sync>;
pub(crate) type WatchmanUninstall = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;
```

Then in `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs`, add:

```rust
/// [`Domain::Board`] 与 [`Domain::Plugin`] 的 handler 所需的全部状态。
///
/// 与 [`AppState`] 的**关键区别**：这里没有一个 `&mut`，全是 owned/`Arc`，故整份
/// 可 move 进 `tokio::spawn` 的任务。这两个域因此可以脱离主循环执行（主循环是
/// 单 task，内联 `await` 会串行堵住所有 thread 的 RPC）。
pub(crate) struct BoardPluginState {
    pub(crate) hub: std::sync::Arc<crate::broadcast::Broadcaster>,
    pub(crate) client: crate::broadcast::ClientId,
    pub(crate) board_dir: std::path::PathBuf,
    pub(crate) resident_dir: std::path::PathBuf,
    pub(crate) plugin_command: std::path::PathBuf,
    pub(crate) theme_handle: crate::theme_tool::ThemeHandle,
    pub(crate) watchman_home: std::path::PathBuf,
    pub(crate) watchman_install: crate::server::WatchmanInstall,
    pub(crate) watchman_uninstall: crate::server::WatchmanUninstall,
    pub(crate) board_launcher: crate::server::BoardLauncher,
    /// detach 之后唯一需要回到主循环的信号：stdio `local` 写失败是致命的
    /// （`driver_reports_finished_when_writer_fails` 钉死），而 detached 任务
    /// 无法用 `?` 把它抛回去。
    pub(crate) fatal_tx: tokio::sync::mpsc::Sender<anyhow::Error>,
}

/// 写一条响应；写失败（仅 stdio `local` 会失败）经 `fatal_tx` 上报主循环。
pub(crate) async fn write_or_fatal(
    state: &BoardPluginState,
    id: crate::protocol::RequestId,
    resp: crate::protocol::ResponseEnvelope,
) {
    if let Err(error) = crate::server::write_response(&state.hub, &state.client, resp).await {
        let _ = state.fatal_tx.send(error).await;
    }
}
```

Make `WatchmanInstall`, `WatchmanUninstall`, `BoardLauncher` visible to this module (they are already `pub(crate) type` aliases in `server.rs` — reference them as `crate::server::...`).

- [ ] **Step 4: Convert the two handlers to `BoardPluginState`**

In `rpc/board.rs`, change the signature and replace `state.hub`/`state.client` uses with the owned fields (`&state.hub`, `&state.client`), and every `write_response(state.hub, state.client, ...)` with `write_or_fatal(state, id, ...).await` (dropping the trailing `?`):

```rust
pub(crate) async fn handle_shared(
    state: &crate::rpc::BoardPluginState,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
```

`id` is consumed by `write_or_fatal`, so where the body needs it twice, `id.clone()` as today. Update imports to `use crate::rpc::{BoardPluginState, Flow, write_or_fatal};` and keep `project_arg`, `to_json`, `ensure_watchman_installed`. `state.board_launcher` stays an `Arc` — call it directly (`(state.board_launcher)(project)`) rather than through the closure wrapper.

Do the same in `rpc/plugin.rs` with `plugin::handle_shared`, using `state.board_dir`, `state.theme_handle`, and `write_or_fatal`.

- [ ] **Step 5: Rewrite the two arms in `serve()`**

Replace the `Domain::Board` and `Domain::Plugin` arms (`server.rs:2718-2737`) with a single detaching arm each. Both domains now spawn:

```rust
                    crate::rpc::Domain::Board | crate::rpc::Domain::Plugin => {
                        // 门禁（initialize / scope）已在上面同步做完；这里只把
                        // **处理** detach 出去。主循环是单 task，内联 await 会串行
                        // 堵住所有 thread 的 RPC；主 runtime 是多线程的，故
                        // `tokio::spawn` 让它在别的 worker 上跑、主循环立刻回到
                        // `select!`。（`spawn_blocking(..).await` 做不到：主 task
                        // 仍要 await 它返回。）
                        let state = std::sync::Arc::new(crate::rpc::BoardPluginState {
                            hub: std::sync::Arc::clone(&hub),
                            client: client.clone(),
                            board_dir: board_dir.clone(),
                            resident_dir: resident_dir.clone(),
                            plugin_command: plugin_command.clone(),
                            theme_handle: theme_handle.clone(),
                            watchman_home: watchman_home.clone(),
                            watchman_install: std::sync::Arc::clone(&watchman_install),
                            watchman_uninstall: std::sync::Arc::clone(&watchman_uninstall),
                            board_launcher: std::sync::Arc::clone(&board_launcher),
                            fatal_tx: fatal_tx.clone(),
                        });
                        let domain = crate::rpc::domain_for(method.as_str());
                        let req = req.clone();
                        let id = id.clone();
                        tokio::spawn(async move {
                            let result = match domain {
                                crate::rpc::Domain::Board => {
                                    crate::rpc::board::handle_shared(&state, &req, id.clone()).await
                                }
                                _ => {
                                    crate::rpc::plugin::handle_shared(&state, &req, id.clone()).await
                                }
                            };
                            match result {
                                Ok(crate::rpc::Flow::Continue) => {}
                                // 域内兜底：与搬移前的原始 dispatch 逐字一致。
                                Ok(crate::rpc::Flow::Unknown) => {
                                    crate::rpc::write_or_fatal(
                                        &state,
                                        id,
                                        err_response(
                                            req.id_for_unknown(),
                                            RpcError::method_not_found(&req.method),
                                        ),
                                    )
                                    .await;
                                }
                                Err(error) => {
                                    let _ = state.fatal_tx.send(error).await;
                                }
                            }
                        });
                        continue;
                    }
```

Note: `req.id` is a `RequestId` and `req.method` a `String` (`protocol.rs:39-40`). Use the cloned `id` for the response — write `err_response(id, RpcError::method_not_found(&req.method))`; do not invent an `id_for_unknown` helper. Because `id` was moved into `write_or_fatal`, clone it for `handle_shared` first (`handle_shared(&state, &req, id.clone())`) and keep the original for the `Flow::Unknown` fall-through. Verify `RequestEnvelope` and `RequestId` are `Clone` (they are) and that `method` is the same binding the raw `_ =>` arm at `:2888` uses.

- [ ] **Step 6: Add the fatal channel and select! branch**

Next to `let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(64);` (`:2387`) add:

```rust
    // detach 出去的 board/plugin handler 唯一需要回到主循环的信号：stdio
    // `local` 写失败是致命的（`?` 语义从前由内联 await 直接传播）。
    let (fatal_tx, mut fatal_rx) = mpsc::channel::<anyhow::Error>(1);
```

In the main `select!` (`:2479`), add a branch (place it before the `card_ticker` branch):

```rust
            Some(error) = fatal_rx.recv() => return Err(error),
```

- [ ] **Step 7: Run the tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS, including `board_and_plugin_shared_state_is_owned_and_send`, all `board_*` tests (`board_create_then_remove_lists_the_project_between_the_two`, `board/list` flows), `write_response_to_a_gone_ws_client_is_not_fatal`, and `driver_reports_finished_when_writer_fails`.

- [ ] **Step 8: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/ipc-responsiveness
git add yi-agent-rs/crates/yi-agent-app-server/src/
git commit -m "perf(app-server): run board and plugin RPCs off the main loop"
```

---

### Task 5: Prefetch board cards for the scheduler tick

**Rationale:** `card_scheduler_tick` (every 3s) reconciles by calling `board_cards` — a synchronous IPC that would otherwise stall the main loop. Its launch path still needs `&mut threads`, so the tick must stay inline; only its read prefetch moves to a background thread.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:4797-4885` (`card_scheduler_tick`), and its call site at `:2914`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs:190-285` (`run_once`; add `run_once_with_snapshot`)
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub(crate) type BoardCardSnapshot = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, Vec<BoardCard>>>>;`
  - `pub(crate) async fn run_once_with_snapshot<L: CardLauncher>(project, board_dir, tracked, launcher, flags, snapshot: &HashMap<PathBuf, Vec<BoardCard>>)`.
  - `fn spawn_board_cards_prefetch(board_dir: PathBuf) -> BoardCardSnapshot` (a detached OS thread).

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `card_scheduler.rs`:

```rust
/// 快照命中时，对账**不再**走 `board_cards`（那条同步 IPC）。证据：假看板
/// 只应答 `next_launch`（返回 null），若实现仍去查 `board_cards`，`list`
/// 调用会出现在 `calls` 里。
#[tokio::test]
async fn reconciliation_uses_the_snapshot_when_present() {
    let board = FakeBoard::new(vec![], json!({"cards": []}));
    let mut tracked = HashMap::new();
    let mut launcher = NoopLauncher;

    let mut snapshot: HashMap<PathBuf, Vec<BoardCard>> = HashMap::new();
    snapshot.insert(std::fs::canonicalize(&board.project).unwrap(), Vec::new());

    run_once_with_snapshot(
        &board.project,
        &board.board_dir,
        &mut tracked,
        &mut launcher,
        &|_| None,
        &snapshot,
    )
    .await;

    let calls = board.calls.lock().unwrap().clone();
    assert!(
        !calls.iter().any(|call| call["command"]["method"] == "list"),
        "reconciliation must read the snapshot, not issue a live list: {calls:?}"
    );
}
```

Implement `NoopLauncher` in the test module (a `CardLauncher` whose `launch` is `unreachable!()` — no card is launched because `next_launch` returns null).

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib reconciliation_uses_the_snapshot_when_present`
Expected: FAIL to compile — `cannot find function run_once_with_snapshot`.

- [ ] **Step 3: Add the snapshot-aware entry point**

In `card_scheduler.rs`, rename the existing `run_once` body into a private inner function and add two wrappers so the 8 existing call sites (5 in tests, 3 in `server.rs`) are untouched:

```rust
/// 重新导出：宿主侧构造预取快照的共享句柄。见 [`crate::server::BoardCardSnapshot`]。
pub(crate) type BoardCardSnapshot =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<PathBuf, Vec<crate::server::BoardCard>>>>;

pub(crate) async fn run_once<L: CardLauncher>(
    project: &Path,
    board_dir: &Path,
    tracked: &mut HashMap<String, TrackedThread>,
    launcher: &mut L,
    flags: &(dyn Fn(&str) -> Option<ThreadFlags> + Send + Sync),
) {
    run_once_inner(project, board_dir, tracked, launcher, flags, None).await
}

/// 与 [`run_once`] 同义，但**对账**读给定的卡片快照，避免在主循环里做同步 IPC。
///
/// 快照里没有该项目（刚登记、或预取线程还没跑到）时退回实时读取——一次性阻塞，
/// 与改动前同义。
pub(crate) async fn run_once_with_snapshot<L: CardLauncher>(
    project: &Path,
    board_dir: &Path,
    tracked: &mut HashMap<String, TrackedThread>,
    launcher: &mut L,
    flags: &(dyn Fn(&str) -> Option<ThreadFlags> + Send + Sync),
    snapshot: &HashMap<PathBuf, Vec<crate::server::BoardCard>>,
) {
    run_once_inner(project, board_dir, tracked, launcher, flags, Some(snapshot)).await
}

async fn run_once_inner<L: CardLauncher>(
    project: &Path,
    board_dir: &Path,
    tracked: &mut HashMap<String, TrackedThread>,
    launcher: &mut L,
    flags: &(dyn Fn(&str) -> Option<ThreadFlags> + Send + Sync),
    snapshot: Option<&HashMap<PathBuf, Vec<crate::server::BoardCard>>>,
) {
    // ...existing body...
}
```

In `run_once_inner`, replace the reconciliation read (`card_scheduler.rs:283`):

```rust
    let canonical = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    let cards = match snapshot.and_then(|map| map.get(&canonical)) {
        Some(cards) => cards.clone(),
        // 快照缺该项目：退回实时读取（一次性阻塞，与改动前同义）。
        None => match board_cards(project, board_dir) {
            Ok(cards) => cards,
            Err(_) => return,
        },
    };
```

- [ ] **Step 4: Add the prefetch thread and snapshot type in `server.rs`**

Add near the other background-task helpers in `server.rs`:

```rust
/// 预取线程发布的卡片快照：项目根（canonical）→ 该项目的卡片。
pub(crate) type BoardCardSnapshot =
    std::sync::Arc<StdMutex<HashMap<PathBuf, Vec<BoardCard>>>>;

/// 后台预取已登记项目的卡片，供看板 tick 的对账读取。
///
/// 用 `std::thread` 而非 `tokio::spawn`：`board_cards` 走同步 `send_request`，
/// 在 async 任务里会阻塞 worker。形态对齐插件侧的 `start_query_server`。
///
/// 连续失败达 [`PREFETCH_FAILURE_LIMIT`] 次就清空该项目的条目，让对账退回实时
/// 读取——否则一个长期不可用的 daemon 会让快照永久停在旧值。
fn spawn_board_cards_prefetch(board_dir: PathBuf) -> BoardCardSnapshot {
    let snapshot: BoardCardSnapshot =
        std::sync::Arc::new(StdMutex::new(HashMap::new()));
    let published = std::sync::Arc::clone(&snapshot);
    std::thread::Builder::new()
        .name("board-cards-prefetch".into())
        .spawn(move || {
            let mut failures: HashMap<PathBuf, u32> = HashMap::new();
            loop {
                if let Ok(projects) = yi_agent_boards::registry::list(&board_dir) {
                    for board in projects {
                        let key = std::fs::canonicalize(&board.project)
                            .unwrap_or_else(|_| board.project.clone());
                        match board_cards(&board.project, &board_dir) {
                            Ok(cards) => {
                                failures.remove(&key);
                                published.lock().unwrap_or_else(|p| p.into_inner()).insert(key, cards);
                            }
                            Err(error) => {
                                let count = failures.entry(key.clone()).or_insert(0);
                                *count += 1;
                                eprintln!(
                                    "[app-server] board card prefetch failed for {} ({count}): {error}",
                                    board.project.display()
                                );
                                if *count >= PREFETCH_FAILURE_LIMIT {
                                    published
                                        .lock()
                                        .unwrap_or_else(|p| p.into_inner())
                                        .remove(&key);
                                }
                            }
                        }
                    }
                }
                std::thread::sleep(PREFETCH_INTERVAL);
            }
        })
        .expect("board card prefetch thread must start");
    snapshot
}

/// 预取周期：短于看板 tick（3s），让对账读到的新鲜度有余量。
const PREFETCH_INTERVAL: Duration = Duration::from_millis(1000);
/// 连续失败多少次后清空该项目的快照条目。
const PREFETCH_FAILURE_LIMIT: u32 = 5;
```

Then, where `board_dir` is established in `serve()` (the `let board_dir = yi_agent_boards::global_dir()...` binding used by the loop), add:

```rust
    let board_cards_snapshot = spawn_board_cards_prefetch(board_dir.clone());
```

- [ ] **Step 5: Pass the snapshot into the tick**

Add a `snapshot: &HashMap<PathBuf, Vec<BoardCard>>` parameter to `card_scheduler_tick<F>` and, inside its per-board loop, call the snapshot-aware entry point:

```rust
        let cards = board_cards_snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        crate::card_scheduler::run_once_with_snapshot(
            &board.project,
            board_dir,
            tracked,
            &mut launcher,
            &flags,
            &cards,
        )
        .await;
```

At the tick call site (`server.rs:2918`), pass the snapshot: read it once per tick (not per board) and pass `&board_cards_snapshot_map`:

```rust
                let board_cards_snapshot_map = board_cards_snapshot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                card_scheduler_tick(
                    &board_dir,
                    &board_cards_snapshot_map,
                    // ...existing arguments unchanged...
                )
                .await;
```

(The clone is deliberate: read the lock once per tick, never hold it across the `.await`.)

- [ ] **Step 6: Run the tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS, including `reconciliation_uses_the_snapshot_when_present` and every existing `card_scheduler` test (they call `run_once`, which still reads live).

- [ ] **Step 7: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/ipc-responsiveness
git add yi-agent-rs/crates/yi-agent-app-server/src/
git commit -m "perf(app-server): prefetch board cards for the scheduler tick"
```

---

### Task 6: Verify end to end and record evidence

**Rationale:** The spec's §6 acceptance criteria and §6 evidence boundary must be produced as artifacts, and §7 delivery rules honoured (source only, no process restarts).

**Files:**
- Create: `docs/superpowers/plans/2026-10-18-ipc-responsiveness-evidence.md`
- Modify: `docs/project-management/` module file(s) if the repo tracks these fixes there.

**Interfaces:**
- Consumes: all prior tasks.
- Produces: no code.

- [ ] **Step 1: Run each affected crate's tests**

```bash
ps aux | grep -v grep | grep cargo   # must be empty
cd plugins/superpowers-kanban && cargo test
cd ../../yi-agent-rs && cargo test -p yi-agent-store --lib
cd ../../yi-agent-rs && cargo test -p yi-agent-app-server --lib
```
Expected: all PASS.

- [ ] **Step 2: Confirm formatting is clean**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/ipc-responsiveness/yi-agent-rs && cargo fmt --all --check
cd ../../plugins/superpowers-kanban && cargo fmt --all --check
```
Expected: no output, exit 0.

- [ ] **Step 3: Build the plugin binary and measure offline (does not touch the running plugin)**

```bash
cd plugins/superpowers-kanban && cargo build --release
```
Then run the new binary against a scratch state dir and measure `list` latency 30 times, asserting median < 5ms and no sample > 50ms. Record the raw numbers (this is the offline replacement for the running instance, which must not be restarted).

- [ ] **Step 4: Write the evidence file**

Record, in `docs/superpowers/plans/2026-10-18-ipc-responsiveness-evidence.md`: the pre-change baseline from the spec §1.1; the post-change offline results from Steps 1-3; and an explicit statement that Items 1 and 3 only take effect on the running desktop after the user restarts the plugin and daemon (per spec §6 evidence boundary and §7).

- [ ] **Step 5: Commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/ipc-responsiveness
git add docs/
git commit -m "docs(perf): record ipc responsiveness verification evidence"
```

---

## Self-Review Notes

- **Spec coverage:** Task 1 = spec §3 (Item 1). Task 2 + Task 3 = spec §5 (Item 3). Task 4 = spec §4.1-4.3 (Item 2, D). Task 5 = spec §4.4 (Item 2, A'). Task 6 = spec §6/§7. The `lead_ticker` note (spec §4.4) is informational; no task changes it, matching the spec.
- **Ordering:** Tasks 1-3 are independent. Task 4 must precede Task 5 only because both edit `server.rs`; either order works if rebased. No task depends on another's new API.
- **Naming consistency:** `BoardCardSnapshot` must be defined **once**, in `server.rs`, and imported in `card_scheduler.rs` via `use crate::server::BoardCardSnapshot;`. The `card_scheduler.rs` snippet in Task 5 Step 3 re-declares it — implementers must replace that line with the import. No other duplicate definitions exist.
