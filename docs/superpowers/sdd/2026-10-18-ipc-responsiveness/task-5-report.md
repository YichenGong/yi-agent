# Task 5 report: Prefetch board cards for the scheduler tick

Branch: `perf/ipc-responsiveness` (worktree `.worktrees/ipc-responsiveness`), built on `73ec922a` (Task 4).
Commit created: `fce63c30` — `perf(app-server): prefetch board cards for the scheduler tick`.

## 1. What I implemented

The 3s `card_scheduler_tick` used to reconcile by calling `board_cards` — a synchronous
`send_request` IPC — inline on the app-server's single main task. Only that read moved off the
main loop; the launch path still needs `&mut threads`, so the tick itself stays inline.

**`card_scheduler.rs`**
- `run_once` was split: its body became the private `run_once_inner(..., snapshot: Option<&HashMap<PathBuf, Vec<BoardCard>>>)`. Two wrappers preserve every existing call site (5 in tests, 3 in `server.rs`):
  - `run_once` (unchanged semantics, passes `None`, still reads live) — this is what all 8 pre-existing call sites keep calling.
  - `run_once_with_snapshot` (passes `Some(&map)`).
- Reconciliation read now tries the snapshot first:
  `canonical = canonicalize(project)`, then `snapshot.and_then(|m| m.get(&canonical))` → clone; on a miss it falls back to the live `board_cards(project, board_dir)`, exactly as before. A miss therefore degrades to the pre-change behaviour (one-off block) rather than skipping the board.
- `read_snapshot(&BoardCardSnapshot) -> HashMap<PathBuf, Vec<BoardCard>>` clones the published map under one short lock. It lives next to the snapshot consumer, so the tick-side locking policy is in one place and the `BoardCardSnapshot` import is genuinely used (no dead-code warning).
- Module docs note the snapshot-aware entry point.

**`server.rs`**
- `pub(crate) type BoardCardSnapshot = Arc<StdMutex<HashMap<PathBuf, Vec<BoardCard>>>>;` — defined **once** here, in the file that owns the prefetch thread. `card_scheduler.rs` imports it (`use crate::server::{BoardCard, BoardCardSnapshot, board_cards, board_query};`); the brief's Step 3 snippet re-declared it, and I replaced that re-declaration with the import per the plan's Self-Review note.
- `PREFETCH_INTERVAL = 1000ms` (shorter than the 3s tick, so freshness has slack) and `PREFETCH_FAILURE_LIMIT = 5`.
- `spawn_board_cards_prefetch(board_dir) -> BoardCardSnapshot`: a detached `std::thread::Builder::new().name("board-cards-prefetch")` loop (not `tokio::spawn`: `board_cards` is synchronous and would block a worker). Each pass lists the registry and, per board, calls `board_cards`; success inserts under the canonical key; failure increments that project's counter, logs, and at `>= PREFETCH_FAILURE_LIMIT` **removes** the entry so reconciliation falls back to a live read instead of serving stale cards forever. Then `std::thread::sleep(PREFETCH_INTERVAL)`.
- The lock is taken for one map operation at a time — never across the `board_cards` IPC above it, never across the sleep below it.
- `serve()` starts the thread right after `board_dir` is destructured: `let board_cards_snapshot = spawn_board_cards_prefetch(board_dir.clone());`.
- The tick call site clones the snapshot once per tick (not per board) before the `.await`, then passes `&board_cards_snapshot_map`. `card_scheduler_tick` gained a `cards_snapshot: &HashMap<PathBuf, Vec<BoardCard>>` parameter and calls `run_once_with_snapshot(..., cards_snapshot)`.

No `PROTOCOL_VERSION`, request/response shape, or frame shape was touched.

## 2. What I tested, and results

`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib` on the committed tree:
**`test result: ok. 759 passed; 0 failed; 0 ignored;`** (757 pre-existing + `reconciliation_uses_the_snapshot_when_present` + `reconciliation_falls_back_to_a_live_read_without_a_snapshot_entry`).
`cargo fmt --all --check` → clean. `cargo build -p yi-agent-app-server` (non-test) → compiles.

New test in `card_scheduler::scheduler_tests`:
- `reconciliation_uses_the_snapshot_when_present` — the brief's test. A `FakeBoard` that answers only `next_launch` (null); the snapshot has an entry for the canonical project root; after `run_once_with_snapshot` the recorded `calls` must contain no `list`. `NoopLauncher` (`launch` is `unreachable!()`) added to the test module per the brief.
- `reconciliation_falls_back_to_a_live_read_without_a_snapshot_entry` — my addition, covering the other half of the contract: with an **empty** snapshot map, a tracked idle card is still reconciled to `board.mark_terminal(awaiting_merge)` via the live read. Without this, "fall back to live" was asserted nowhere and a regression that dropped the fallback would only show up as silently skipped boards in production.
- `BoardCardSnapshot` is defined once in `server.rs`; `card_scheduler.rs` imports it. No `-D warnings`/clippy issue from the import (the warning it initially produced is gone; see §4).

### TDD evidence

**RED** — `cargo test -p yi-agent-app-server --lib reconciliation_uses_the_snapshot_when_present`, after adding only the test:
```
error[E0425]: cannot find function `run_once_with_snapshot` in this scope
   --> crates/yi-agent-app-server/src/card_scheduler.rs:826:9
error[E0425]: cannot find function `run_once_with_snapshot` in this scope
   --> crates/yi-agent-app-server/src/card_scheduler.rs:861:9
error: could not compile `yi-agent-app-server` (lib test) due to 2 previous errors
```
Expected: the brief's Step 2 predicts exactly this compile failure (the entry point does not exist yet).

**GREEN** — same command after implementing:
```
running 2 tests
test card_scheduler::scheduler_tests::reconciliation_uses_the_snapshot_when_present ... ok
test card_scheduler::scheduler_tests::reconciliation_falls_back_to_a_live_read_without_a_snapshot_entry ... ok
test result: ok. 2 passed; 0 failed; 0 ignored; 757 filtered out
```

**Mutation checks (do the tests bite?).** I temporarily broke the implementation twice, confirmed failure, then restored:
1. Forced the snapshot lookup to always miss (`match None::<&Vec<BoardCard>>`): the snapshot test failed with
   `reconciliation must read the snapshot, not issue a live list: [Object {"method": "board.next_launch"}, Object {"method": "list"}]`.
2. Removed the fallback arm (`None => return`): the fallback test failed with `left: 0, right: 1` for `board.mark_terminal` (nothing was reconciled).
Both restored; `git status` clean; the committed tree re-ran at 759 passed.

## 3. Files changed

- `yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs` (+177/-20-ish)
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` (+100)

## 4. Self-review findings

**Does any path hold the snapshot mutex across an await or a sleep?** No.
- Tick side: the only `read_snapshot` call is the tick call site, and its result is cloned into an owned `board_cards_snapshot_map` before `card_scheduler_tick(...).await`. The `card_scheduler_tick` body contains exactly one `.await` (`run_once_with_snapshot`) and no lock; the only `lock()` in that body is `session.status` for the per-board flags snapshot (`server.rs:4958`), which the pre-existing code already released inside the closure — untouched by this task. The mutex guard is not part of the tick's future, so it cannot be held across the await.
- Prefetch side: `published.lock()` guards a single `insert` / `remove` expression; the `board_cards` IPC and `std::thread::sleep` are outside it. No `.await` exists in that thread at all.

**Can the prefetch thread leak or spin?** No.
- Leak: exactly one thread per `serve()`, started after `board_dir` is known, holding only `Arc`s; it has no join handle and no shutdown signal, which is deliberate (same shape as the existing `spawn_board_watchman_loop` and the plugin-side `start_query_server`) and bounded by the process lifetime. The loop is unconditional, so it never returns and never leaves a detached thread behind a dead `serve`.
- Spin: each iteration ends in `std::thread::sleep(1000ms)`, so the loop is bounded at ~1 registry read + N `board_cards` calls per second; an error (`registry::list` returning `Err`, or a per-project `board_cards` failure) cannot hot-loop. This also matches the "never hold the lock across a sleep" constraint.
- Note: the timestamped `[app-server] board card prefetch failed ...` line is printed once per failed project per second. Against a dead daemon on a project whose daemon is permanently down, that is up to 5 lines per project (then the entry is cleared and, with the failure counter retained, still one line per second thereafter). Same shape as the pre-existing convention already recorded for Task 3 ("a persistently failing poll logs error! each iteration; cannot spin hot"). Not new noise in kind, but it is new in volume for a down daemon.

**Do the existing `card_scheduler` tests still pass unchanged?** Yes — all of them, unmodified. They call `run_once`, which still reads live (`snapshot = None`). Test count 757 → 759 with 0 failures.

**Warnings.** The suite's 2 remaining warnings are pre-existing and are **not** mine: `lead_loop.rs:17` (`ThreadFinished`/`UserSpoke`/`FallbackTick` never constructed) and `lead_loop.rs:29` (`title`/`last_user_text` never read). Neither file is in my diff (`git diff --name-only HEAD~1 HEAD` lists only `card_scheduler.rs` and `server.rs`). I did introduce and then remove one of my own: importing `BoardCardSnapshot` only for a doc link initially produced `unused import: BoardCardSnapshot`; giving `read_snapshot` a real use of the type eliminated it. I also handled one new non-test warning: with the tick switched to the snapshot path, `run_once` has no production caller, so the plain `cargo build` warned `function 'run_once' is never used`. Rather than delete an entry point the brief explicitly declares, I kept it with `#[allow(dead_code)]` and a comment explaining it is the declared live-read counterpart exercised by the existing tests — the repo's existing convention for declared-but-unread items (`BoardCard.state`-style fields, `BoardPluginState.watchman_uninstall`).

**Overbuilding.** No new tests beyond the one explicitly required plus the single fallback test that the contract's other half needed; no new abstractions beyond `read_snapshot`, which I added solely to give the snapshot type a real use and to keep tick-side locking in one place.

## 5. Concerns / deviations from the brief

1. **Step 5's two snippets contradict each other; I followed the call-site one.** The in-tick snippet calls `board_cards_snapshot.lock()...clone()` and passes `&cards`, but that name is not in the tick's scope (the tick receives the map by reference, and the brief never adds the handle to its parameters — doing so would have forced `Arc` into the tick's signature, which the brief's declared interface does not include). I implemented the call-site form: clone once per tick before the `.await`, pass `&board_cards_snapshot_map`, and the tick passes `cards_snapshot` straight to `run_once_with_snapshot`. This satisfies both stated invariants — the lock is read once per tick, not per board, and never held across an `.await`.
2. **The brief's test assertion key is wrong for this test fixture.** `FakeBoard` records `{"method": ...}`, so `call["command"]["method"] == "list"` would compare `Null == "list"` for every call and the assertion would pass vacuously — it would not fail on the pre-fix implementation. I used `call["method"]`, which is the key the fixture actually records (`FakeDaemon` in `server.rs` records the same key and `calls_to("list")` filters on it). Verified non-vacuous by mutation check #1 above. My *invalid* run of the brief's literal assertion was not needed — the key mismatch is visible in the fixture's recorder (`recorded.lock().unwrap().push(json!({"method": method, ...}))`).
3. **Tick parameter named `cards_snapshot`, not `snapshot`.** The brief calls the parameter `snapshot` in prose but never writes the signature. `snapshot` is already a local in the tick's per-board loop (the `HashMap<String, ThreadFlags>` flags snapshot), so that name would shadow it; `cards_snapshot` keeps the declared type and avoids the churn.
4. **Comment language.** The constraint says English comments in `yi-agent-rs`, while adjacent code in these two files is mostly Chinese and Task 4's own diff left Chinese comments in `rpc_tests.rs`/`server.rs`. I wrote all new comments in English as bound, left every pre-existing Chinese comment untouched, and kept the two comments I added inside an existing Chinese doc block (the tick's, and the reconciliation read's) — the file's older blocks keep their language either way. Flagging it in case the reviewer wants newer English while older Chinese is left alone (which is what the other tasks effectively did).
5. **Freshness bound.** Reconciliation now reads a snapshot up to ~1s old (prefetch interval) instead of a live read; the fallback covers the "no entry yet" case but not "entry present and stale". With `PREFETCH_FAILURE_LIMIT = 5` a permanently dead daemon clears the entry within ~5s and reconciliation returns to live reads. A card that changes state between prefetch passes is seen one tick later at worst (3s tick, 1s prefetch) — I believe this is within the intent ("prefetch … for the scheduler tick"), but it is a real behavioural change worth naming.
6. **Tests with a live `serve` plus a registered board** (`the_serve_loop_tick_launches_a_claimed_card` and neighbours) now also run a prefetch thread against the injected tempdir. They pass, and their assertions are all `eventually(...)` / specific-method filters, not "exactly N plugin calls", so the extra background reads do not perturb them. Noting it because it is the one place where the new thread coexists with a live harness.