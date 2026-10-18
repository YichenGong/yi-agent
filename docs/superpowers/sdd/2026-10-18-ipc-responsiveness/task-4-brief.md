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

