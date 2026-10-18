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

