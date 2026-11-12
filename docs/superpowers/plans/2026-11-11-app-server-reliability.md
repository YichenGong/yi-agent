# App-server Reliability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Repair the six approved app-server reliability defects without replacing its public protocol.

**Architecture:** Keep per-client transport policy in Broadcaster/stdio/WS and per-turn lifecycle policy in ThreadSession and turn completion events. Make persistence and quiescence explicit instead of inferring them from execution completion. Preserve existing subscriber and read-watermark behavior.

**Tech Stack:** Rust, Tokio, axum WebSocket, existing nextest/cargo tests.

## Global Constraints

- Baseline a579d175; changes only in fix/app-server-reliability worktree.
- WS slow-consumer budget 10 seconds; local ordinary replies/history retain backpressure; final-material/control admission fails explicitly on overload.
- Refuse delete/resume when old writer cannot be confirmed quiescent; no immediate-delete tombstones.
- No real LLM calls, new dependencies, runtime deployment, or public snapshot protocol implementation.
- Existing session-completeness spec remains authoritative for later snapshot/probe/delta reconciliation.

## Task 1: Device connections
Files: src/server.rs, src/server/rpc_io.rs, src/rpc/pair.rs, src/ws.rs (under app-server).
- [x] Extend real WS revocation regression to two clients (red/green); add old-connection teardown preservation coverage.
- [x] Run the extended revoke test on the old path and observe the second connection staying open.
- [x] Represent all connections; revoke atomically drains the device entry; teardown removes only its own client; verify authorization after registration.
- [x] Re-run WS and server transport tests.

## Task 2: Turn resource cleanup
Files: src/session.rs, src/server/transport/mod.rs, src/server/turn/replay.rs, src/server/turn/failure.rs, src/server/tests/plugin_admission.rs.
- [x] Exercise the production completion-consumer helper with a real plugin permit; assert zero live permits after matching finish, preserving a stale event.
- [x] Run `cargo test -p yi-agent-app-server --all-features --lib plugin_admission`.
- [x] Centralize matching-turn cleanup as ThreadSession::finish_turn(&str), use in consumers and rollback.
- [x] Re-run admission/failure tests.

## Task 3: Directed reply timeout and final content
Files: src/broadcast.rs, src/ws.rs, src/protocol.rs, src/server/rpc_io.rs.
- [x] Add full-queue tests: reply times out/removes slow WS but not fast/local; final ItemCompleted is admitted or the connection explicitly fails under local/WS pressure and respects subscriptions.
- [x] Run targeted tests to observe failures.
- [x] Bound directed WS enqueue and socket sends; remove only same-channel stale registration on timeout. Add notification reliability policy independent of Delivery; route final items/batches through nonblocking broadcast_required (admit or explicitly disconnect, including local).
- [x] Re-run broadcast/protocol/transport tests.

## Task 4: Persistence and quiescence barrier
Files: src/server.rs, src/session.rs, src/server/turn/{persist,replay}.rs, src/server/driver/turn_driver.rs, src/rpc/thread/lifecycle/{resume,delete}.rs and completion producers/consumers.
- [x] Add failing append-error recovery tests, completion-stream-closed/timeout refusal tests, and delete-I/O error tests.
- [x] Run persistence/lifecycle tests and observe failures.
- [x] Propagate explicit persisted/failed execution outcome; retain checkpoint on append failure and emit visible error; keep completion watermark. Store last persistence failure so an idle session cannot bypass the barrier. Return Result from barrier; refuse mutation on timeout/closed stream/failure. Confirm old driver cannot write after successful barrier before replacing/removing it. Return deletion errors.
- [x] Re-run lifecycle, unread, lead, driver-resume and failure suites.

## Task 5: Managed ingress isolation
Files: src/server/transport/stdio.rs, src/server/transport/mod.rs, src/server/tests/management.rs.
- [x] Add failing saturated-business-queue tests with management request/response following business traffic; test overflow, ingress auth and close.
- [x] Run management tests and observe failures.
- [x] Keep bounded ordered business admission separate from private ingress; do not await business dispatch or output capacity in reader. Bound overflow responses and stop on output overload rather than accumulate tasks/frames. Retain immediate auth checks.
- [x] Re-run full management suite.

## Task 6: Integration
Files: docs/project-management/yi-agent-app-server.md and README.md.
- [x] Review diff against all six invariants; see reliability verification report for red/green evidence; snapshot/probe/delta tasks remain incomplete.
- [x] Run `cargo fmt --all`, app-server all-features tests and full `just ci` (workspace coverage supersedes the narrower affected run): 6453 passed / 41 skipped; see verification report.
- [x] Update module/index counts (95/98), commit conventional fix without co-author trailer.
- [x] Local `--no-ff` merge to main (`26fab411`); run complete `just ci` on that exact committed merge result in the clean worktree: 6456 passed / 41 skipped, 50 binding checks, exit 0. Main's pre-existing untracked example fails fmt-check and is preserved untouched; audit used the existing cached-db fallback. See verification report for limitations.

Cleanup policy: after successful integration, preserve the verification logs under main's `yi-agent-rs/target/` and remove only this worktree and branch. Documentation-only acceptance updates follow the same worktree and `--no-ff` workflow.

## Verification record

See [verification](../reports/2026-11-11-app-server-reliability-verification.md). Full CI on the implementation before the final three boundary regressions: 6450 tests passed / 41 skipped plus 50 binding checks. Final-tree CI and merge-result CI are recorded there separately, not inferred from that earlier run.
