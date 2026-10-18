# Task 2 Report: Daemon reuses one runtime and one repository per daemon

**Status:** DONE_WITH_CONCERNS

**Commit:** `b3c6c6f5` — `perf(daemon): reuse one runtime and repository per daemon`

**Worktree:** `.worktrees/ipc-responsiveness` (never touched `main`)

**Scope:** only `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` (+133 / −7).

---

## What I implemented

Followed the brief's steps in order, verbatim.

1. **Step 1 — failing tests.** Added a `#[cfg(test)] mod request_context_tests` at the end of
   `ipc.rs` (after `fork_wire_tests`) containing exactly the brief's two tests,
   `should_reconcile_only_outside_the_window` and `a_request_context_reuses_one_repository`,
   including the Chinese comments. The brief says "add to `mod tests`"; this file has no
   single `mod tests` — it has nine per-concern modules (`subscription_queue_tests`,
   `plugin_protocol_tests`, …, `fork_wire_tests`). I followed that existing convention with a
   new sibling module rather than inventing a `mod tests`.

2. **Step 3 — `should_reconcile` + `RequestContext`.** Inserted immediately above `fn respond(`
   (now `:3098` and `:3108`). `use std::time::{Duration, Instant};` was already at `:13`, so
   no import change was needed. `should_reconcile` returns `true` when `last` is `None`, and
   otherwise compares `now.duration_since(previous) >= window`.

3. **Step 4 — `respond`.** Signature is now
   `respond(context: &RequestContext, coordinator, request)`; body opens with
   `context.reconcile_if_stale(coordinator)?;` then locks the long-lived connection and binds
   it to the existing `let mut repository`. Nothing inside the `match` body changed, so every
   existing `repository.*` use compiles untouched. Per the brief I left every per-arm
   `tokio::runtime::Builder::new_current_thread()` alone (14 of them, including
   `SpawnChild` → `spawn_child_and_admit`). No arm opened its own repository, so no arm
   needed `context.with_repository(...)`.

4. **Step 5 — build once, thread through.** `let request_context = Arc::new(RequestContext::new(&database_path)?);`
   is placed immediately after `let database_path = database_path.as_ref().to_path_buf();`
   (now `:869`) and therefore **before** `let listener = thread::spawn(move || {` (now `:872`),
   which is required because that closure moves `database_path`. In the accept loop's `Ok`
   arm, `let request_context = Arc::clone(&request_context);` was added next to the other
   clones and passed as the new second argument to `handle_client`, keeping
   `&database_path`. `handle_client` gained `context: &RequestContext` after
   `database_path: &Path`; its other four `database_path` uses (the `SubscribeTrace` arm, the
   `SubscribeEvents` arm, `preview_cancel`, and `stream_trace_subscription`) are untouched.
   Only the fallback arm's call changed: `request => match respond(context, coordinator, request)`.

5. **One deviation (deviation #1 in the brief's own instruction):** `with_repository` is
   *only* called from the new test, so `cargo check -p yi-agent-store --all-targets` emitted
   `warning: method 'with_repository' is never used` in the library build, which an
   incremental build promotes to the visible `yi-agent-store (lib) generated 1 warning` line.
   I verified HEAD (`a15e0fbb`) builds with **0** warnings, so this would have been a new
   regression in the build output. I kept the method (it is part of the brief's declared
   interface and must stay `pub(crate)`) and added `#[allow(dead_code)]` plus a two-line
   comment stating it is the test/interface entry point. `with_repository` is dead in
   non-test builds regardless of any annotation — the warning is about the *test-only* use.
   Nothing else was over-built.

## What I tested and results

| Command | Result |
| --- | --- |
| `cargo test -p yi-agent-store --lib should_reconcile_only_outside_the_window a_request_context_reuses_one_repository` (RED) | FAIL to compile, 4 errors (expected — see below) |
| `cargo test -p yi-agent-store --lib` (GREEN) | **72 passed, 0 failed**, 0.56s |
| `cargo check -p yi-agent-store --all-targets` | 0 warnings, 0 errors |
| `cargo fmt --all --check` | clean |
| `cargo test -p yi-agent-store --test runtime_ipc` | 97 passed, 0 failed |
| `… --test subagent_ipc_protocol` | 3 passed |
| `… --test subagent_fork_ipc` | 7 passed |
| `… --test subagent_fork_upload` | 14 passed |
| `… --test daemon_handler_thread_leak` | 1 passed (65s) |
| `… --test repository_decisions / resource_cursor / scheduler / subagent_runtime_e2e` | 4 / 1 / 7 / 3 passed |
| `… --test runtime_coordinator` | 76 passed, **1 failed — pre-existing, unrelated** (see below) |

I ran the store crate's integration binaries because this task changes the daemon's request
path; all IPC-facing binaries pass, including the daemon thread-leak test that drives 1500
fresh connections.

## TDD evidence

### RED

Command (note: `cargo test` accepts one positional filter, so both names go after `--`; the
brief's two-positional form is rejected by cargo with `error: unexpected argument`):

```
cd yi-agent-rs && cargo test -p yi-agent-store --lib -- should_reconcile_only_outside_the_window a_request_context_reuses_one_repository
```

Output (tail):

```
error[E0425]: cannot find function `should_reconcile` in this scope
    --> crates/yi-agent-store/src/ipc.rs:4421:17
     |
4421 |         assert!(should_reconcile(None, base, window));
     |                 ^^^^^^^^^^^^^^^^ not found in this scope
error[E0433]: cannot find type `RequestContext` in this scope
    --> crates/yi-agent-store/src/ipc.rs:4432:23
     |
4432 |         let context = RequestContext::new(&database_path).expect("context builds");
     |                       ^^^^^^^^^^^^^^ use of undeclared type `RequestContext`
error: could not compile `yi-agent-store` (lib test) due to 4 previous errors
```

**Why expected:** exactly the brief's forecast — `cannot find function should_reconcile` and
`cannot find type RequestContext`. Both names were introduced only in Step 3, so the failure
is the missing production code, not test setup.

### GREEN

```
cd yi-agent-rs && cargo test -p yi-agent-store --lib
```

Output (tail):

```
test ipc::request_context_tests::should_reconcile_only_outside_the_window ... ok
test ipc::request_context_tests::a_request_context_reuses_one_repository ... ok
test result: ok. 72 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.56s
```

`cargo check -p yi-agent-store --all-targets` emits no warnings, so the test output is pristine.

## Files changed

| File | Change |
| --- | --- |
| `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` | +133 / −7: `should_reconcile`, `RequestContext` + `RECONCILE_WINDOW`, rewritten `respond` signature/opening, context built once at daemon start and threaded into the accept loop and `handle_client`, new `request_context_tests` module |

No other file changed. `git diff a15e0fbb` (Task 1's commit) shows only this file.

## Self-review findings

- **Implemented everything?** Yes. All six code steps, plus Step 7 (`cargo fmt --all` clean,
  conventional commit, no `Co-Authored-By` — verified with `git log -1 --format=%B | grep -i co-authored`).
- **Names accurate?** `RequestContext`, `RequestContext::new`, `with_repository`,
  `reconcile_if_stale`, `should_reconcile`, `RECONCILE_WINDOW` — all match the brief's
  Interfaces section. `handle_client` gained `context` and kept `database_path`.
- **Tests verify real behavior?** `a_request_context_reuses_one_repository` builds a real
  context against a real `tempfile` sqlite path and performs two reads through the same
  `Mutex`-held connection, asserting both return the same schema version. It is a genuine
  reuse check — on the old code there was no context to construct at all (RED). It does not,
  however, *prove* connection identity (two identical opens would also give equal schema
  versions); identity is proven structurally by `with_repository` locking a single field, not
  by the assertion. Noted honestly rather than overstated.
- **Overbuilding / YAGNI?** None beyond deviation #1, which is a one-line annotation, not new
  behavior. No new public API, no error-type change, no constant change.
- **Constraints honored:** `PROTOCOL_VERSION`, all `IpcRequest`/`IpcResponse` and response
  frame shapes, `PLUGIN_QUERY_TIMEOUT`, `CLIENT_REQUEST_TIMEOUT`, and `COMMAND_WRITE_DEADLINE`
  are untouched. `rusqlite::Connection` stays behind the `Mutex`. No process was killed or
  restarted. `ps aux | grep cargo` was checked before every cargo invocation; one peer's
  cargo (`superpowers-kanban-runner` in a different worktree) was observed once and had
  exited before my next run.
- **Housekeeping:** every scratch/benchmark artifact I created for the investigation below was
  removed. Final `git status --porcelain` is empty and the working tree is byte-identical to
  `b3c6c6f5` (`git diff --quiet HEAD` → identical).

## Concerns

**1. This change does not measurably improve the `Status` p50, and the task's stated root
cause does not hold up.** This is the main concern and I could not in good conscience report
a clean DONE.

I built a scratch harness (a real `Daemon::start` in a `TempDir`, real
`send_request(socket, IpcRequest::Status)` over the socket — the same pattern as the existing
`daemon_handler_thread_leak` test) and measured with and without my change:

| Code under test | Status round-trip |
| --- | --- |
| Before (`a15e0fbb`, fresh runtime + fresh `RuntimeRepository::open` per request) | p50 **41.1ms**, p90 50.1ms |
| After (`b3c6c6f5`, one reused context) | p50 **46.0ms**, p90 49.9ms |

No improvement, arguably noise-level worse. Then I measured the work actually removed:

| Component (in-process, warm) | Cost |
| --- | --- |
| `RuntimeRepository::open` (`Connection::open` + 2 PRAGMAs + `migrate` parse) | p50 **0.19ms** |
| Fresh current-thread tokio `Runtime` build | sub-0.1ms |
| One reused connection read (`latest_event_id`) | p50 **0.01ms** |
| `reconcile_worker_events` on a reused runtime | p50 **~0.0ms** |

The entire removed per-request cost is ~0.2ms, not ~45ms. I also instrumented the live
daemon: server-side `respond` is **~62–285µs** and the accept loop's per-iteration work is
**tens of µs**. Client-side phase timing showed the cost is not connect (0.05ms) and not
write (0.03ms) but the **read wait: p50 47.6ms**, i.e. the daemon is not answering promptly
even though it computes quickly.

I then isolated the real mechanism by temporarily changing the accept loop's
`thread::sleep(Duration::from_millis(10))` (the `WouldBlock` arm) to `from_micros(100)`:

| Variant | Status round-trip |
| --- | --- |
| Committed code (`sleep(10ms)`) | p50 **46.8ms**, p90 47.6ms |
| Same code with `sleep(100µs)` | p50 **0.54ms**, p90 0.55ms |

So on this machine the dominant ~45ms is the **accept loop's 10ms `WouldBlock` sleep**
(the target of Task 3, not Task 2). Consistent with that: `set_nonblocking(true)` plus a
10ms blind sleep gives an expected ~5ms mean landing delay, yet the observed delay is ~45ms,
i.e. about 4.5 sleep intervals per request — plausibly because the per-iteration
`reconcile_worker_events()` wakes a waker on another thread and the sleeping accept thread is
not promptly scheduled back. Either way, the measurement points at the accept loop.

**Implications.** (a) Task 2 is *correct and necessary* but *insufficient*; its benefit only
appears once Task 3 removes the sleep — expected final p50 in the sub-millisecond range, an
order of magnitude better than the current ~45ms. (b) The plan's rationale sentence that the
45ms comes from per-request `Runtime::new` + repository open appears **not** to be supported
by measurement on this machine; the numbers point at the accept loop instead. Worth
re-checking the plan's remaining assumptions (Task 4's "main-loop blocking" claims) with the
same instrumented approach.

Caveat: these are single-run measurements on one machine (200 and 100 samples respectively),
taken while the worktree was otherwise idle. The orders of magnitude are unambiguous, but the
41 vs 46ms difference is within run-to-run noise.

**2. `respond` now holds the repository `Mutex` across the whole `match`, including the
long-blocking arms.** This is exactly what the brief prescribes, and it replaces "coordinator
is serial" with "coordinator *and* every concurrent request handler is serial". No arm in
`respond` held a repository before (they all open their own via `RuntimeRepository::open`),
so this is new cross-request serialization: an in-flight `WaitAgent` with `timeout_ms: None`
(which the CLI/TUI do send) holds the lock for as long as it waits, and any concurrent
`Status` poll would block behind it. Previously those were independent connections. This is
the reason I set the report status to `DONE_WITH_CONCERNS` rather than flagging it as a
blocking defect: per the brief the task is measured by `cargo test -p yi-agent-store --lib`
(72/72 green, including 97 `runtime_ipc` tests), and the brief explicitly prescribes both the
guard placement and the "daemon's request path is already serial" reasoning. Task 3's brief
adds a poll gate but does not change this. If the plan wants the old concurrency between a
long `wait` and a status poll back, the fix is a narrow-scope variant (re-lock per arm that
needs `repository`), which is outside the brief's specified code — I did not do it unilaterally.

**3. Pre-existing unrelated test failure.** `runtime_coordinator` has one failure,
`an_autonomous_session_refuses_a_workdir_that_is_not_a_git_worktree`, which asserts the error
string contains `"not a git repository"` but this machine's git emits the localized
`"fatal: 不是 git 仓库：/nonexistent/yi-agent-test"`. I proved it pre-existing by stashing my
change and re-running it at HEAD: identical failure, same message, same line
(`runtime_coordinator.rs:3533`). The file is not touched by this task. It is environment
(LANG/locale) dependent, not caused by Task 2.

**4. Two process/worktree incidents, both fully cleaned up (disclosed for transparency).**
While measuring the baseline I ran `git stash push -- ipc.rs` *after* committing, so the push
was a no-op and the subsequent `git stash pop` applied a **foreign pre-existing stash**
(`stash@{0}: On agent-owned-worktree-design: T4-wip-ungreen`) into this worktree, creating
conflicts in 7 files (`worker.rs`, `runtime.rs`, `subagent_ipc`/`runtime_coordinator`/
`subagent_runtime_e2e` tests, `yi-agent-subagent/src/lib.rs`, `yi-agent-tools/src/worktree.rs`).
I recovered with `git reset --hard HEAD`: the 7 files are back, my commit `b3c6c6f5` is
intact, `git status --porcelain` is empty, and all three foreign stashes are preserved
untouched (`git stash list` unchanged). No `git stash pop` was retried in the worktree after
that. No process was ever killed or restarted.

---

## Verification summary

- RED confirmed: 4 compile errors, exactly `cannot find function should_reconcile` /
  `cannot find type RequestContext`.
- GREEN confirmed: `cargo test -p yi-agent-store --lib` → **72 passed, 0 failed**; 0 compile
  warnings; `cargo fmt --all --check` clean.
- Integration: all IPC-facing binaries pass (97 + 3 + 7 + 14 + 1 + 4 + 1 + 7 + 3); one
  pre-existing locale-dependent failure in `runtime_coordinator`, proven pre-existing at HEAD.
- Committed `b3c6c6f5`, no `Co-Authored-By`.
- Scratch artifacts removed; worktree byte-identical to the commit; foreign stashes preserved.
- Headline measurement: this change removes only ~0.2ms of per-request work; the ~45ms lives
  in the accept loop's 10ms sleep (Task 3's target), which I demonstrated by reducing that
  sleep to 100µs and watching p50 fall from 46.8ms to 0.54ms.
