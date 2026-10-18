# Task 4 Report: Detach `board/*` and `plugin/*` RPC handling from the main loop

**Commit:** `73ec922a` — `perf(app-server): run board and plugin RPCs off the main loop`
**Branch:** `perf/ipc-responsiveness` (worktree `ipc-responsiveness`), built on `c826428d`.

## What I implemented

Followed the brief's steps 1-8 in order.

1. **Prerequisite visibility (brief Step 3, first half).** Widened the three private
   `type` aliases in `server.rs` (`BoardLauncher`, `WatchmanInstall`,
   `WatchmanUninstall`) to `pub(crate) type`.
2. **`BoardPluginState` (brief Step 3).** Added to `rpc/mod.rs` with the brief's
   exact field list: `hub`, `client`, `board_dir`, `resident_dir`, `plugin_command`,
   `theme_handle`, `watchman_home`, `watchman_install`, `watchman_uninstall`,
   `board_launcher`, `fatal_tx`. Zero `&mut`; all owned/`Arc`, so the whole struct is
   `Send + Sync + 'static` and movable into `tokio::spawn`.
3. **`write_or_fatal` (brief Step 3).** Added to `rpc/mod.rs`. Wraps
   `server::write_response` and, on `Err` (only stdio `local` can fail), sends the
   error on `fatal_tx` instead of propagating it with `?`.
4. **Handlers (brief Step 4).** `board::handle` → `board::handle_shared(&BoardPluginState, ..)`;
   `plugin::handle` → `plugin::handle_shared(&BoardPluginState, ..)`. All `state.hub` /
   `state.client` borrows became `&state.hub` / `&state.client`; every
   `write_response(..).await?` became `write_or_fatal(..).await` (trailing `?` dropped).
   `state.board_launcher` is called directly as `(state.board_launcher)(project)`.
   Bodies otherwise byte-identical.
5. **Dispatch (brief Step 5).** Merged the `Domain::Board` / `Domain::Plugin` arms into
   one arm that builds an `Arc<BoardPluginState>`, re-derives `domain_for(method)`,
   clones `req`/`id`, `tokio::spawn`s the handling, and `continue`s — so the main loop
   returns to `select!` immediately. `Flow::Continue` → done; `Flow::Unknown` →
   `write_or_fatal(id.clone(), err_response(id, method_not_found(&req.method)))`
   (the corrected form, using the cloned `id`; **no** `id_for_unknown` helper);
   `Err` → `fatal_tx.send(error)`.
6. **Fatal channel + `select!` branch (brief Step 6).** `let (fatal_tx, mut fatal_rx) =
   mpsc::channel::<anyhow::Error>(1);` next to `turn_tx`, and
   `Some(error) = fatal_rx.recv() => return Err(error),` placed immediately before the
   `card_ticker` branch.

**Gate untouched.** `initialize` / scope / admin checks all happen synchronously
*before* the dispatch `match`, and the detach point is after them, so no authorization
path changed. `Flow::Continue` / `Flow::Unknown` semantics are unchanged; only the
*handling* moved off-loop.

## What I tested and results

- `cargo test -p yi-agent-app-server --lib` → **757 passed; 0 failed; 0 ignored**
  (756 pre-existing + the new fatal-channel test).
- `cargo fmt --all --check` → clean (exit 0).
- `cargo build -p yi-agent-app-server` → warning output is **exactly the pre-existing
  baseline** (2 warnings: `lead_loop.rs` `WakeReason` variants, `ThreadSnapshot`
  fields). No new warnings.

### TDD evidence

**RED** — command:
`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib board_and_plugin_shared_state_is_owned_and_send`

Output:
```
error[E0425]: cannot find type `BoardPluginState` in module `crate::rpc`
  --> crates/yi-agent-app-server/src/rpc_tests.rs:13:36
   |
13 |     assert_spawnable::<crate::rpc::BoardPluginState>();
   |                                    ^^^^^^^^^^^^^^^^ not found in `crate::rpc`

error: could not compile `yi-agent-app-server` (lib test) due to 1 previous error
```

Why this failure was expected: the test asserts the *precondition* for detaching
(`BoardPluginState: Send + Sync + 'static`). Before the type exists there is nothing to
assert, so the only possible failure is exactly this "type not found" compile error —
which is what the brief predicts.

**GREEN** — command:
`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib board_and_plugin_shared_state_is_owned_and_send`

Output:
```
running 1 test
test server::tests::board_and_plugin_shared_state_is_owned_and_send ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 755 filtered out; finished in 0.00s
```

## Which tests prove `board/*` and `plugin/*` still work end to end after detaching

These all drive the real `serve()` main loop through the `Harness` (send a frame on the
transport, read the response), so they exercise the new `tokio::spawn` path, not just the
handler bodies:

- `server::board_rpc_tests::board_create_then_remove_lists_the_project_between_the_two` —
  the strongest one: `board/create` → `board/list` → `board/remove` → `board/list`, four
  round-trips across the detached path including the registry contents and per-board
  `status`.
- `server::board_rpc_tests::board_list_of_an_empty_registry_answers_with_no_boards`
- `server::board_rpc_tests::a_board_created_through_the_rpc_is_written_to_the_injected_registry`
- `server::board_rpc_tests::creating_a_board_ensures_the_watchman_is_installed`
- `server::board_rpc_tests::creating_a_board_leaves_the_watchman_alone_when_the_switch_is_off`
- `server::board_rpc_tests::a_failing_watchman_install_does_not_fail_board_creation`
- `server::board_rpc_tests::board_create_for_a_missing_directory_reports_an_error_and_registers_nothing`
- `server::board_rpc_tests::board_create_without_a_project_is_invalid_params` /
  `board_remove_without_a_project_is_invalid_params`
- `server::plugin_query_tests::*` (9) — e.g.
  `a_query_reaches_the_daemon_with_the_plugin_and_method_untouched` and
  `the_rpc_maps_a_missing_board_to_its_own_code_in_data` prove `plugin/query` still
  forwards verbatim and still maps errors, over a real fake daemon on a unix socket.
- `server::plugins_tests::*` (7) + nested `skill_rpc_tests` (5) — `plugins/list`,
  `plugin/settings/read|write` through the detached path.
- `server::tests::unknown_method_returns_method_not_found` — still `-32601`, i.e. the
  raw fall-through is intact after the arms were merged.

Plus the fatal-path equivalence pins:

- `server::tests::write_or_fatal_reports_a_local_write_failure_on_the_fatal_channel`
  (**new**) — proves the new mechanism: a write to a dead stdio `local` client makes
  `write_or_fatal` put the error on the fatal channel rather than swallowing it.
- `server::tests::driver_reports_finished_when_writer_fails` and
  `server::tests::write_response_to_a_gone_ws_client_is_not_fatal` — the two tests the
  spec's risk table names for "the fatal semantics of a stdio write failure".

Focused run (`board_rpc_tests plugin_query_tests plugins_tests writer_fails gone_ws shared_state`)
→ `33 passed; 0 failed`.

## Files changed

| File | Change |
| --- | --- |
| `src/rpc/mod.rs` | `+BoardPluginState`, `+write_or_fatal`; removed the now-redundant duplicate `BoardLauncher` alias; removed the four `AppState` fields that only these domains read; doc updates |
| `src/rpc/board.rs` | `handle` → `handle_shared(&BoardPluginState, ..)`, `write_or_fatal` |
| `src/rpc/plugin.rs` | `handle` → `handle_shared(&BoardPluginState, ..)`, `write_or_fatal` |
| `src/server.rs` | three aliases widened to `pub(crate) type`; merged detaching arm; `fatal_tx`/`fatal_rx`; `select!` branch; `app_state!()` field-list trim |
| `src/rpc_tests.rs` | the brief's compile-time test + the new fatal-channel test |

## Self-review findings

1. **Can a detached task outlive a shutdown?** No, and it is not harmful. The detached
   task is spawned onto the process-wide `Runtime`, which is created once
   (`main.rs:305`, `rt.block_on(serve_stdio_with_relay(..))`). On the `stdio_eof` /
   inbound-EOF break, `serve()` returns but the runtime is *not* dropped — so a task
   spawned just before the break can finish. That is either a no-op (the client is gone,
   `write_or_fatal` sends on `fatal_tx` whose receiver is being dropped → the `send`
   returns `Err`, which is ignored via `let _ =`) or it completes the write before EOF.
   **Important detail:** I deliberately did *not* use the spec §4.2's `if let Err(error)`
   form without the guard; I used `if let Err(error) = write_response(...)` — but the
   spec's sketch has a real bug: it would treat *any* handler `Err` as fatal. Since both
   handlers now return `Ok` on every path and only ever produced `Err` from the removed
   `?`-on-write, the two forms are behaviourally identical today, and I kept the brief's
   `Result`-based form verbatim. The only `Err` the handler can still return is a
   write failure, which is exactly the fatal case.
2. **Is the fatal channel bounded appropriately?** Yes: capacity 1, per the spec. It is
   a fatal signal — the main loop's first action on receiving it is `return Err(error)`,
   which ends the session. A burst of detached writers each `send`-ing could in principle
   make all but one wait, but the first error immediately terminates the loop (dropping
   `fatal_rx`), so waiters are released with `Err` and their errors are discarded. No
   unbounded growth, no deadlock. Same shape as the existing `turn_rx`.
3. **Does any detached path still need the main loop (e.g. `&mut threads`)?** No. I
   re-enumerated the fields both handlers actually read after the rewrite: `board.rs`
   uses `board_launcher`, `board_dir`, `resident_dir`, `plugin_command`,
   `theme_handle`, `watchman_home`, `watchman_install`, `hub`, `client`;
   `plugin.rs` uses `board_dir`, `theme_handle`, `hub`, `client`. All are owned/`Arc` in
   `BoardPluginState`; none is `&mut`. This is also what the new `Send + Sync + 'static`
   assertion pins at compile time.
4. **Dead code introduced by the move.** Removing the domains from `AppState` orphaned
   four fields (`board_dir`, `resident_dir`, `plugin_command`, `board_launcher`). I
   verified with a baseline build that HEAD had no such warnings, so leaving them would
   have been a new-warning regression. They were read *only* by these two domains, so I
   removed them from `AppState` (the faithful consequence of the move) and updated the
   two comments that named them (the `app_state!()` field-name note and the struct doc,
   now 32 fields). `watchman_install` / `watchman_uninstall` / `watchman_home` /
   `theme_handle` stay because `ui/*` still uses them.
5. **Two deviations from the literal brief text, both forced:**
   - `write_or_fatal`'s `id` parameter is never read by its body (the response already
     carries the id), which produced a new `unused variable` warning. I named it `_id`
     to keep the brief's exact type/arity while keeping the build output pristine.
   - `watchman_uninstall` in `BoardPluginState` is never read (no board/plugin method
     uninstalls the watchman; `board/remove` only stops the daemon). It is a *declared*
     interface field, so I kept it and annotated it `#[allow(dead_code)]` with an
     explanatory comment, matching the repo's existing pattern for declared-but-unread
     fields (`BoardCard::kind/source/base`, `server.rs:1382-1390`). The brief's other two
     prerequisites (`board_launcher`, `watchman_install`) *are* used and needed no
     annotation.
   - Also dropped the duplicate `pub(crate) type BoardLauncher` in `rpc/mod.rs` (it
     became unused once the handler moved to the `pub(crate)` `server.rs` alias); the
     struct now references `crate::server::BoardLauncher` as the brief's note says.
6. **`id.clone()` insertion.** The brief's snippet writes
   `write_or_fatal(state, id, err_response(id, ..))`, which cannot compile (arg 2 moves
   `id` before arg 3 borrows it). I put the clone on the `write_or_fatal` argument —
   `write_or_fatal(state, id.clone(), err_response(id, ..))` — and kept the corrected
   `err_response(id, ..)` on the response, per the brief's "follow the correction" note.
7. **No residual references.** `grep` confirms no remaining `rpc::board::handle` /
   `rpc::plugin::handle` call sites and no `board/` or `plugin/` string arms left in the
   raw `serve()` dispatch (only the `_ =>` at `server.rs:2915`, using the same `method`
   binding from `:2586` and `id`, unchanged).
8. **Environment respected.** No process was killed or restarted; all reads outside the
   worktree were read-only; `PROTOCOL_VERSION`, request/response shapes and frame shapes
   untouched (no edits to `protocol.rs`; the new test uses existing `ok_response`).

## Concerns

- **Ordering nondeterminism (spec risk #2), now realized.** Detached `board/create` can
  interleave with other RPCs and no longer has deterministic relative ordering. The spec
  explicitly accepts this (atomic tmp+rename registry write, idempotent
  `create_with_project`, desktop does not issue these concurrently) and asks for it to be
  *recorded*, which this report does. Risk #3 (same-file read-modify-write) is likewise
  accepted: it stays within one process, just a different task.
- **No test drives a `board/*` or `plugin/*` request through the spawn path *and* then a
  failing `local` write,** i.e. the full `spawn → fatal_tx → select! → serve returns Err`
  chain is not covered end to end; it is covered in two halves (the new
  `write_or_fatal_...` test for the send side, and `driver_reports_finished_when_writer_fails`
  for the fatal-write semantics). Constructing a harness whose `local` pump dies while a
  board/plugin request is in flight is not cheaply expressible with the current fixtures,
  and the brief did not ask for one. Flagging it rather than claiming more than the tests
  show.
- `write_or_fatal`'s unused `_id` is a mild interface wart inherited from the brief; it is
  documented and warning-free, but if a follow-up wants to drop it, that is the only
  signature deviation I would revisit.
