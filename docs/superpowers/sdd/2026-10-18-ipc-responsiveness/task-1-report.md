# Task 1 Report: Plugin query server uses `poll(2)` instead of a 200ms accept sleep

**Status:** DONE

**Commit:** `a15e0fbb` — `perf(kanban-ipc): poll for connections instead of sleeping 200ms`

**Worktree:** `.worktrees/ipc-responsiveness` (never touched `main`)

**Scope:** only `plugins/superpowers-kanban/` (standalone cargo workspace; separate from `yi-agent-rs/`).

---

## What was implemented

Followed the brief's eight steps verbatim.

1. **`libc` dependency** — added `libc = "0.2"` under `[dependencies]` in
   `crates/superpowers-kanban-ipc/Cargo.toml`. No `[workspace.dependencies]` entry
   was needed, as the brief states. `Cargo.lock` gained exactly one line (`"libc"` in
   the `superpowers-kanban-ipc` dependency list) — libc `0.2.189` was already in the
   lock because `superpowers-kanban-runner` uses `libc` (`libc::flock`), so no network
   fetch occurred.

2. **New private helper `wait_readable`** (`src/server.rs`, immediately above `serve_with`):
   builds a `libc::pollfd` from `listener.as_raw_fd()`, clamps the timeout to `i32::MAX`
   milliseconds, and calls `libc::poll(&mut descriptor, 1, milliseconds)`. Returns
   `Ok(false)` on `0` (timeout), `Ok(true)` when `count > 0` (readable or hangup), and
   `Err(last_os_error())` otherwise. The `unsafe` block carries the brief's `SAFETY`
   comment explaining why the single initialized `pollfd` with `nfds = 1` is sound.

3. **`ACCEPT_POLL` doc comment rewritten** (value unchanged at `200ms`) to state it is
   now the `keep_going` check cadence and accept wait ceiling, not a per-query delay.
   `CLIENT_READ_TIMEOUT` untouched.

4. **`serve_with` accept loop rewritten.** Non-blocking mode is retained with an
   updated comment (poll→accept race can still yield `WouldBlock`). The loop now:
   `if !wait_readable(&listener, ACCEPT_POLL)? { continue; }` then `match listener.accept()`.
   The `WouldBlock` arm is kept as an **empty** arm (`=> {}`, per the brief's note — do
   NOT treat it as an error), other errors still `return Err(error)`. `serve_connection`
   failure handling is unchanged. The `keep_going` cadence is therefore identical
   (still a 200ms ceiling), which is why both existing shutdown tests pass unchanged.

5. **Signature preserved:** `serve_with<D, F>(socket: &Path, dispatch: Arc<D>, keep_going: F)`
   is byte-for-byte the same; the single call site (`crates/superpowers-kanban-runner/src/main.rs:775`)
   was not modified and still compiles. The doc contract ("阻塞调用；调用方放到自己的线程里")
   is untouched. No response frame shape and no `MAX_FRAME_BYTES` handling changed.

6. **Regression test added** — `consecutive_queries_do_not_wait_for_the_accept_poll`,
   placed after `a_client_that_connects_and_leaves_does_not_stop_serving` exactly where
   the brief says, verbatim including the Chinese doc comment.

---

## TDD evidence

### RED — before implementing the loop change

Command:

```
cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-ipc consecutive_queries_do_not_wait_for_the_accept_poll
```

Output (tail):

```
running 1 test
test server::tests::consecutive_queries_do_not_wait_for_the_accept_poll ... FAILED

failures:

---- server::tests::consecutive_queries_do_not_wait_for_the_accept_poll stdout ----

thread 'server::tests::consecutive_queries_do_not_wait_for_the_accept_poll' (115571785) panicked at crates/superpowers-kanban-ipc/src/server.rs:338:9:
three back-to-back queries took 631.795167ms; the accept path is still sleeping
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

test result: FAILED. 0 passed; 1 failed; 0 ignored; 1 filtered out; finished in 0.71s
```

**Why this failure is expected and correct:** at this point `libc` was added and the
test existed, but the accept loop still did `std::thread::sleep(ACCEPT_POLL)` on
`WouldBlock`. Each of the three back-to-back queries lands just after a sleep begins
and must therefore wait out the remainder of a ~200ms window; 3 × ~200ms ≈ 600ms, and
the observed **631.8ms** matches that forecast almost exactly. The assertion threshold
is `< ACCEPT_POLL` (200ms), so the failure is the mechanism under test (blind sleeping),
not test setup: the server did bind, serve all three queries, and return
`plugin.result` for each — only the elapsed time failed. This rules out a
"test is broken" explanation.

### GREEN — after implementing `wait_readable` + the new loop

Command:

```
cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-ipc
```

Output (tail):

```
running 28 tests
...
test server::tests::consecutive_queries_do_not_wait_for_the_accept_poll ... ok
test server::tests::a_deep_state_dir_still_binds_and_answers ... ok

test result: ok. 28 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.30s

   Doc-tests superpowers_kanban_ipc
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

Note the **whole 28-test binary finishes in 0.30s** — previously the three `serve_with`
tests alone spent most of their time sleeping in `ACCEPT_POLL`. The named tests from
the brief all pass: `consecutive_queries_do_not_wait_for_the_accept_poll`,
`the_socket_sits_where_the_manifest_says`,
`a_query_is_forwarded_with_its_params_and_answered_verbatim`,
`a_failed_dispatch_becomes_an_error_frame`,
`malformed_frames_are_refused_instead_of_panicking`, plus both shutdown tests
(`a_deep_state_dir_still_binds_and_answers`,
`a_client_that_connects_and_leaves_does_not_stop_serving`).

**Flakiness check:** ran the regression test 5 additional times in a row — `ok` each
time, 0.23–0.30s per run. No flakiness observed.

### Step 7 — full plugin workspace

Command:

```
cd plugins/superpowers-kanban && cargo test
```

Result: all four test binaries pass.

| Binary | Result |
| --- | --- |
| `superpowers-kanban-core` (lib) | 107 passed; 0 failed |
| `superpowers-kanban-ipc` (lib) | 28 passed; 0 failed |
| `superpowers-kanban-runner` (lib) | 91 passed; 0 failed |
| `superpowers-kanban` (bin) | 24 passed; 0 failed |

Plus 4 doc-test binaries, all `0 passed; 0 failed`.

---

## Files changed

| File | Change |
| --- | --- |
| `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/Cargo.toml` | +1 line: `libc = "0.2"` |
| `plugins/superpowers-kanban/Cargo.lock` | +1 line: `"libc"` under `superpowers-kanban-ipc` deps |
| `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs` | +86/−4: `AsRawFd` import, `ACCEPT_POLL` doc, `wait_readable`, new accept loop, regression test |

`cargo fmt --all` was run in `plugins/superpowers-kanban/` before committing; it reported
no changes needed beyond the current formatting.

---

## Self-review findings

Checked against the "Before Reporting Back" checklist:

- **Implemented everything?** Yes — all 8 steps. Verified by diff inspection: the helper,
  the constants block, the loop, the test, and the manifest/lock are all present.
- **Names accurate?** `wait_readable(listener: &UnixListener, timeout: Duration) -> std::io::Result<bool>`
  matches the brief's Interfaces section exactly. `consecutive_queries_do_not_wait_for_the_accept_poll`
  matches. `ACCEPT_POLL` / `CLIENT_READ_TIMEOUT` unchanged in name and value.
- **Test verifies real behavior?** Yes. It drives a real `UnixStream` against a real
  socket served by a real `serve_with` on its own thread, and asserts on wall-clock
  elapsed time for three sequential round-trips. It is a genuine regression test: it
  demonstrably failed (631.8ms) against the old implementation and passes (whole binary
  0.30s) against the new one, so it discriminates the behavior rather than restating it.
  It also asserts each reply's `type == "plugin.result"`, so a fast-but-wrong
  implementation cannot pass by answering garbage quickly.
- **Overbuilding / YAGNI?** None. No new public API, no new error type, no retry logic,
  no timeout tuning. `milliseconds` clamping uses the brief's exact expression. Did not
  "improve" the `WouldBlock` arm or the `serve_connection` error handling.
- **Test output pristine?** The `-p superpowers-kanban-ipc` run emits two warnings, both
  **pre-existing and in files this task did not touch**:
  - `unused variable: 'other'` at `crates/superpowers-kanban-ipc/src/client.rs:198`
  - `function 'contract_usize' is never used` at `crates/superpowers-kanban-ipc/src/socket.rs:128`

  My change introduces **zero** new warnings (the new code compiles clean, and
  `cargo doc -p superpowers-kanban-ipc --no-deps --document-private-items` produced no
  warnings, so the new `` [`wait_readable`] `` intra-doc link resolves correctly).
  I deliberately did **not** fix the unrelated pre-existing warnings — that is out of
  scope for this task and would muddy the diff.
- **One deviation to flag:** the brief listed the CLI import line as
  `use std::io::{BufRead, BufReader, Write};` as the *only* Rust line in the imports
  block (i.e. it showed the file's existing `use` block verbatim). I interpreted that
  code fence as "the resulting imports block" and made the single required addition,
  `use std::os::unix::io::AsRawFd;`, rather than deleting the other `use` lines. The
  diff confirms only `AsRawFd` was added; `UnixListener`, `UnixStream`, `PathBuf`, `Arc`,
  and `Duration` remain (all still used). This is the only reasonable reading — the
  helper cannot compile without `AsRawFd`, and every other import is still needed.
- **Constraints honored:** no `Co-Authored-By` line; no running process was killed,
  restarted, or probed for writes (verified `ps aux | grep cargo` was empty before every
  cargo invocation, per the concurrency warning); no response frame shape or
  `MAX_FRAME_BYTES` change; Chinese comments used throughout to match the file.

---

## Concerns

**None blocking.** Two observations, both informational:

1. **EINTR is not handled explicitly.** `libc::poll` can return `-1` with `EINTR` if a
   signal handler interrupts it, which `wait_readable` surfaces as `Err`, which
   `serve_with` turns into a `return Err(...)`, stopping the query server. I checked
   `grep -rn "signal|SIGCHLD|sigaction|ctrlc|EINTR" --include=*.rs crates/` in the plugin
   workspace and found **no signal handlers at all** in the plugin crates, so no
   registered handler can cause `EINTR` in practice. This matches the brief's prescribed
   code exactly, so I implemented it as written rather than adding a retry-on-EINTR arm
   (which would be unrequested scope). Worth noting because the runner spawns child
   processes (for coding agents) onto other threads — if a future change installs a
   `SIGCHLD` handler in this process, EINTR becomes reachable and this line would want an
   `EINTR => continue` arm.

2. **The test is timing-based, so it is machine-load sensitive.** The margin is large
   (631ms → a few ms, versus a 200ms threshold), so it is robust in practice, and it was
   stable across 6 total runs. It could in principle fail on an extremely loaded machine.
   That tradeoff is inherent to measuring "did we stop sleeping" and is what the brief
   specified.

3. **Unmeasured end-to-end effect.** The brief's rationale cites a bimodal `list` latency
   (0ms or ~271ms, measured). This task removes the plugin-side ~200ms component; the
   remaining ~71ms and any daemon-side contribution are handled by the other five tasks
   in this plan. I did not re-measure end-to-end `list` latency here, as that requires
   touching the live process (pid 43799), which the task forbids. Note also that the
   running plugin process still holds the old binary — the improvement only takes effect
   once the plugin is restarted, which I did not do.

---

## Verification summary

- RED confirmed: 631.795167ms, asserting "the accept path is still sleeping".
- GREEN confirmed: `consecutive_queries_do_not_wait_for_the_accept_poll` passes; ipc
  binary 28/28 in 0.30s; full workspace 250/250 across four binaries.
- No new compiler or rustdoc warnings; `cargo fmt --all` clean.
- Committed as `a15e0fbb` with no `Co-Authored-By` line.