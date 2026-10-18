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

