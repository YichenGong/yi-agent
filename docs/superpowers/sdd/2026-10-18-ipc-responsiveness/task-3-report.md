# Task 3 Report: Daemon accept loop polls instead of sleeping 10ms

**Status:** DONE
**Commit:** `c826428d` perf(daemon): poll for connections instead of sleeping 10ms (parent `1dfc6fed`)
**File changed:** `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` (50 insertions, 4 deletions)

## What I implemented

The dominant cost from Task 2's measurements: the accept loop ran the listener
non-blocking and did `thread::sleep(Duration::from_millis(10))` on `WouldBlock`,
which added a blind 0-10ms wait to every request. Replaced that blind wait with a
`poll(2)` gate.

1. **`fn daemon_accept_wait(listener: &UnixListener, timeout: Duration) -> std::io::Result<bool>`**
   (private, added directly above `fn handle_client(`): builds one `pollfd` from
   `listener.as_raw_fd()` with `events: POLLIN`, calls
   `libc::poll(&mut descriptor, 1, milliseconds)`, and maps the result to
   `Ok(false)` on timeout (0), `Ok(true)` when count > 0, and
   `Err(std::io::Error::last_os_error())` on -1. The timeout is converted with
   `timeout.as_millis().min(i32::MAX as u128) as libc::c_int`, so an
   over-large `Duration` cannot wrap. `use std::os::unix::io::AsRawFd;` was
   already imported at `:6`; `libc = "0.2"` was already a dependency
   (`yi-agent-store/Cargo.toml:27`), so no dependency change was needed.

2. **`const DAEMON_ACCEPT_POLL: Duration = Duration::from_millis(10);`** next to
   the helper. The constant is now only the `thread_stop` check cadence, not a
   per-request latency floor; it is documented as such.

3. **Accept loop rewritten to gate on the poll.** The loop body now runs:

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
       Ok((stream, _)) => { /* existing handler spawn, unchanged */ }
       Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
       Err(_) => break,
   }
   ```

   The wait result is handled with an explicit `match`, not `?` (the enclosing
   `thread::spawn` closure returns `()`), as the brief requires. The
   `thread::sleep(Duration::from_millis(10))` arm is gone; `WouldBlock` is now a
   no-op with the reason in a comment (`poll` said readable but the peer
   disconnected before `accept`).
   All existing loop behaviour (the reconcile/schedule/prune block before the
   wait, the handler spawn, the finished-handle reaping, `Err(_) => break`) is
   unchanged.

4. **Comments.** Per the bound global constraint ("English comments in
   `yi-agent-rs`") the brief's Chinese doc comments were rendered in English,
   matching the surrounding file/convention (Task 1's plugin-side code is
   Chinese, but that file lives under `plugins/`). Semantics are unchanged. Two
   adjacent comments were also updated so they do not lie about the new shape:
   the `listener.set_nonblocking(true)?;` site (kept as-is, now explaining that
   it exists so a peer that vanishes between poll and accept yields `WouldBlock`
   instead of blocking) and `stop_listener`'s wake-up comment (it wakes the loop
   by connecting, which now makes poll return instead of racing a sleep).

No protocol, `IpcRequest`/`IpcResponse` shape, frame shape, `PROTOCOL_VERSION`,
`PLUGIN_QUERY_TIMEOUT`, `CLIENT_REQUEST_TIMEOUT`, or `COMMAND_WRITE_DEADLINE`
was touched. No timing test was added, per the brief.

## What I tested and results

All commands run from `yi-agent-rs/` with cargo on `PATH`.

| Command | Result |
| --- | --- |
| `cargo build -p yi-agent-store` | ok, no warnings |
| `cargo fmt --all` | applied before commit (no residual diff) |
| `cargo clippy -p yi-agent-store --all-targets` | no warnings, no errors |
| `cargo test -p yi-agent-store --lib` | **73 passed, 0 failed** (0.51s) |
| `cargo test -p yi-agent-store --test runtime_ipc` | **97 passed, 0 failed** (2.99s) — matches the expected 97 |
| `cargo test -p yi-agent-store --test daemon_handler_thread_leak` | **1 passed, 0 failed** (0.88s) |

Note on the thread-leak test's runtime: the job expected ~65s, and the test
passed in **0.88s**. That is not flakiness, it is the headline evidence for this
change, so I measured it directly rather than assuming:

- With Task 3 reverted (`git checkout HEAD~1 -- .../ipc.rs`) and nothing else
  changed: **1 passed, finished in 63.10s** (`real 65.39`), with the harness
  printing `has been running for over 60 seconds`.
- With Task 3 applied: **1 passed, finished in 0.88s**.

The test drives 1,500 fresh connections and reaps finished handler `JoinHandle`s
each pass. Under the old blind sleep the loop could only accept roughly one
connection per 10ms tick, so 1,500 connections took ~63s; now each connection
returns from `poll` immediately, so the same 1,500 connections complete in under
a second. It is the same test, same assertions, same pass verdict; only the
wall-clock dropped. I restored the committed file afterwards and confirmed a
clean `git status`.

No hang and no exit 137 occurred; the peer worktree's cargo did not interfere.

## Self-review findings

- **Everything implemented?** Yes: helper, constant, poll gate, removed sleep
  arm, `set_nonblocking` preserved, explicit error handling, no new dependency.
- **Names accurate?** `daemon_accept_wait`, `DAEMON_ACCEPT_POLL` exactly as the
  brief's Interfaces section specifies; the helper is private.
- **Overbuilding avoided?** I added no timing test (brief says structural
  coverage) and no extra abstraction. The only additions beyond the brief's
  snippets are two comment-truthfulness fixes at adjacent dead-comment sites and
  English translation of the brief's Chinese comments, which the binding
  global constraint requires for `yi-agent-rs`.
- **Can the loop spin hot?** No. The only path that skips the blocking `poll` is
  the `Err` + `continue` arm, which requires `poll` itself to fail; a persistent
  `poll` failure would log once per iteration rather than silently spin, and the
  pre-existing `Err(_) => break` still ends the loop on real `accept` errors.
  The `WouldBlock` no-op cannot spin either, because reaching `accept` first
  requires a positive `poll` result; the loop then re-blocks in `poll` for up to
  `DAEMON_ACCEPT_POLL`.
- **Is `thread_stop` still checked on a fixed cadence?** Yes. The loop condition
  `while !thread_stop.load(Ordering::Acquire)` runs once per `poll` timeout, so
  at most `DAEMON_ACCEPT_POLL` (10ms) after a stop is set — the same worst-case
  cadence as the sleep it replaces. `stop_listener` additionally connects to the
  socket, which makes the listener readable and returns `poll` immediately, so
  the normal shutdown path does not even wait out the 10ms (this is unchanged
  behaviour; it previously broke out of the sleep early by the same mechanism).
- **Do tests verify real behaviour?** `runtime_ipc` (97) exercises real
  connections, shutdown joining, slow-subscriber resync, and
  `daemon_stop_joins_an_incomplete_client_handler_before_releasing_runtime_files`
  — i.e. the poll gate's connect path and the stop path. The lib suite covers
  the client timeout and request-context changes from Task 2. No new test was
  required or added.
- **Test output pristine?** Yes: no warnings in build, clippy, or test output
  for the three targets run.

## Concerns

1. **Two other 10ms sleeps remain in `ipc.rs`, deliberately untouched.** The
   event and trace subscription producer threads (`:1837`, `:1950`) poll the
   repository with `thread::sleep(Duration::from_millis(10))`. These are
   subscription *delivery* loops, not the accept loop, and are outside this
   task's scope; they can still add up to ~10ms of latency to streamed
   event/trace rows. Flagging for whichever task owns subscription latency, not
   fixing here.
2. **A `poll` that fails persistently logs at `error!` on every iteration.**
   That matches the brief's specified shape and the crate's existing
   "keep serving but say what broke" convention, and it cannot busy-spin
   silently; noting it only because it is the one unguarded path.
3. **`libc::poll` is not EINTR-retried explicitly.** A signal delivered to the
   daemon's accept thread would fail the call and log one retry before the loop
   continues; there is no tight loop risk because the thread is otherwise
   blocked. The plugin-side Task 1 helper has the identical shape, so this keeps
   the two consistent.
