# IPC Responsiveness — Verification Evidence

Plan: [`2026-10-18-ipc-responsiveness.md`](./2026-10-18-ipc-responsiveness.md) ·
Spec: [`2026-10-18-ipc-responsiveness-design.md`](../specs/2026-10-18-ipc-responsiveness-design.md) ·
Branch: `perf/ipc-responsiveness` (worktree `.worktrees/ipc-responsiveness`) ·
Base: `main` @ `3abff916`

All six tasks are implemented and committed. This file records what was measured,
what was *not* measured, and how to collect the missing numbers.

## 1. Commits

| Task | Commit | Subject |
| --- | --- | --- |
| 1 | `a15e0fbb` | `perf(kanban-ipc): poll for connections instead of sleeping 200ms` |
| 2 | `b3c6c6f5` | `perf(daemon): reuse one runtime and repository per daemon` |
| 2 (review fix) | `1dfc6fed` | `perf(daemon): scope the repository lock to the arms that use it` |
| 3 | `c826428d` | `perf(daemon): poll for connections instead of sleeping 10ms` |
| 4 | `73ec922a` | `perf(app-server): run board and plugin RPCs off the main loop` |
| 5 | `fce63c30` | `perf(app-server): prefetch board cards for the scheduler tick` |

## 2. Pre-change baseline (spec §1.1, for reference)

- `pwd && ls` itself: p50 = 10.0ms. The command is not slow; it waits behind the
  serial main loop.
- `sample <app-server-pid> 6`, main thread, 4730 samples: 2171 non-idle, of which
  **1613 (74%)** sat in synchronous socket IPC `__recvfrom`
  (561 `card_scheduler::run_once → board_cards`, 285 `→ board_query`,
  407 `board/list → lifecycle::status_with`); ≈**34% of main-loop wall time**.
- Item 1 — plugin accept loop (`set_nonblocking` + 200ms `sleep`): direct
  back-to-back `list` 30×: min 0, **median 271ms**, bimodal (6× ~0ms, 22× ~260ms).
- Item 2 — `card_scheduler_tick` (~2 IPC/tick) and the `board/*` / `plugin/*`
  handlers ran inline on the main loop; desktop's 2s tick contained
  `refreshSelectedBoard` = 527ms and `refreshBoards` = 666ms.
- Item 3 — `respond()` built a runtime and reopened the repository per request:
  `Status` (pure daemon, no plugin) p50 = **45ms**, min 6.9; 10 concurrent wall 48ms.

## 3. Post-change results (measured, offline)

Offline means: the worktree's own compiled binaries and test harnesses. This is the
substitute the spec's §6 evidence boundary calls for, because the *running*
processes (plugin pid 43799, daemon pid 3132) still execute the old binaries.

### 3.1 Test suites — all green

| Command | Result |
| --- | --- |
| `cargo test -p superpowers-kanban-ipc` | 28 passed, 0 failed |
| `cargo test` (plugin workspace) | 250 passed across four binaries |
| `cargo test -p yi-agent-store --lib` | 73 passed, 0 failed |
| `cargo test -p yi-agent-store --test runtime_ipc` | 97 passed, 0 failed |
| `cargo test -p yi-agent-app-server --lib` | 759 passed, 0 failed |
| `cargo fmt --all --check` (both workspaces) | clean |

One pre-existing, unrelated failure exists in
`runtime_coordinator::an_autonomous_session_refuses_a_workdir_that_is_not_a_git_worktree`:
it asserts the literal `"not a git repository"`, but this machine's git emits the
localized `"fatal: 不是 git 仓库：…"`. It fails identically at the branch base
(`a15e0fbb`) and is locale-dependent, not caused by this work.

### 3.2 Item 1 — plugin accept poll

`consecutive_queries_do_not_wait_for_the_accept_poll` times three back-to-back
queries against a real socket:

| Implementation | 3 consecutive queries |
| --- | --- |
| Before (`sleep(200ms)` blind poll) | **631.8ms** (≈3 × 200ms; the assertion fails) |
| After (`poll(2)`) | a few ms; whole 28-test binary runs in 0.30s |

Both shutdown tests (`a_client_that_connects_and_leaves_does_not_stop_serving` and
the `stop` path) pass unchanged, i.e. the `keep_going` check cadence is preserved.

### 3.3 Item 3 — daemon

`daemon_handler_thread_leak` drives 1500 fresh connections through the real accept
loop. Same test, same assertions:

| Implementation | Wall time |
| --- | --- |
| Before Task 3 (`10ms` sleep on `WouldBlock`) | **21.97s** (implementer, under different load: 63.10s) |
| After Task 3 (`poll(2)`) | **0.83s** |

**Correction to the spec/plan's stated mechanism.** §1.1 and the plan's Task 2
rationale attribute the daemon's 45ms `Status` p50 to the per-request
`Runtime::new()` + `RuntimeRepository::open()`. Direct measurement does not support
that: those two cost ~0.2ms combined. The 45ms was the accept loop's 10ms
`WouldBlock` sleep — with each request landing in a fresh sleep window and the
sleeping thread not rescheduled promptly, ~4.5 intervals elapsed per request.
Dropping that sleep to 100µs moved `Status` p50 from 46.8ms to 0.54ms. That is why
Task 3, not Task 2, is the dominant daemon fix; Task 2 remains correct and necessary
(it removes ~0.2ms/request, and its repository reuse is what makes the per-arm
borrow in `1dfc6fed` expressible).

Task 2 also produced a fix the brief did not anticipate: `respond` originally held
the repository guard across the whole `match`. Since each connection has its own
handler thread, requests *are* concurrent, so one `WaitAgent` with
`timeout_ms: None` (which the CLI/TUI send, and which never returns) would have
frozen every `Status` poll behind that lock — the very symptom under repair.
`1dfc6fed` scopes the borrow to the four arms that use sqlite, with a regression
test that fails on the pre-fix code (250ms wait behind a slow holder) and passes now.

### 3.4 Item 2 — app-server

- `board/*` and `plugin/*` handling now runs on the multi-threaded runtime via
  `tokio::spawn`; the main loop `continue`s immediately. The `initialize`/scope gate
  stays synchronous and ahead of the detach point.
- The 3s `card_scheduler_tick` now reconciles from a background prefetch thread's
  snapshot (published once per second) instead of a synchronous `board_cards` IPC.
  On a snapshot miss it falls back to the live read, i.e. to the pre-change behaviour.
- Proven by test, not by timing: 759 app-server lib tests pass, including
  `board_create_then_remove_lists_the_project_between_the_two` (four round-trips
  through the detached path), the `plugin_query_tests` (verbatim forwarding + error
  mapping over a fake daemon on a unix socket), and
  `unknown_method_returns_method_not_found` (the raw fall-through is intact).

The spec's acceptance metric for Item 2 (the `sample`-based "`__recvfrom` 34% → <5%")
is **not** measured here: it requires `sample` on the running app-server, which
still executes the old binary. See §4.

## 4. Evidence boundary — what still needs a restart

Per the spec's §6 evidence boundary and §7 delivery rules, this delivery changed
**source only**. No running process was killed or restarted.

**The reported symptom does not change on the user's screen until the processes are
restarted with the new binaries:**

- **Item 1** (plugin) needs the plugin rebuilt/installed and its process restarted.
- **Item 3** (daemon) needs `yi-agent daemon serve` restarted.
- **Item 2** (app-server) needs the desktop app-server restarted, since the app
  embeds its own `yi-agent app-server` binary.

How to collect the live numbers afterwards:

1. `scripts/rebuild.sh plugin` (installs `/opt/homebrew/bin/superpowers-kanban`),
   then restart the plugin process.
2. Restart `yi-agent daemon serve`.
3. Restart the desktop app.
4. Re-run the §1.1 probes: 30 direct `list` calls (expect median < 5ms, no sample
   > 50ms, no bimodality); `sample <app-server-pid> 6` (expect `__recvfrom` share
   near < 5%); `Status` p50 (expect < 10ms).

## 5. Accepted behavioural changes

| Change | Why it is acceptable |
| --- | --- |
| Detached `board/*` / `plugin/*` lose deterministic ordering vs other RPCs | Registry write is atomic tmp+rename; `create_with_project` is idempotent; the desktop does not issue these concurrently. Clients already match responses by id and tolerate interleaving. |
| Scheduler reconciliation reads a snapshot up to ~1s old | Prefetch interval is 1s vs a 3s tick; a dead daemon's entry is cleared within ~5s (`PREFETCH_FAILURE_LIMIT`) and reconciliation falls back to a live read; a miss also falls back to live. |
| `fatal_tx` (capacity 1) carries stdio write failures from detached handlers | Preserves the previous fatal semantics that `?` used to propagate inline; the first error returns from `serve()`. |
| Daemon `respond` repository access is serialized per arm | Unchanged in effect from before (each arm opened its own connection), but the connection is now shared, so only one arm touches sqlite at a time. |
