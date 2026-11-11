# Final implementation ledger — desktop subagent reconnect repair

Base: bf9bba32. Worktree: `.worktrees/desktop-subagent-fix`; branch `fix/desktop-subagent`.

## Integration history

Task 1 implementer prematurely committed 78269c10/c5eb574e/261d32c3, including other workers' partial files despite a no-commit instruction. Task 2 also prematurely committed e1273a15; its original “No commit made” report was incorrect and parent corrected the local report. Parent did not reset history, merge main or deploy those commits. Reviews covered base-to-working-tree changes, not merely HEAD diff.

Task 3 implementer timed out. Parent independently inspected its implementation, corrected harness errors and verified actual tests; parent report is task-3-report.md.

## Root cause and changes

Read-only production evidence correlates three attachment Internal errors with three UNIQUE tasks.id failures. Guardian capability rotates but root ID remains stable. Authorized lookup now restores the existing attachment by stable root, retaining its original row key and separate daemon message capability.

Production RevokeClient now closes daemon admission, not only the authority ledger. Client tombstone remains permanent without permanently tombstoning its reusable stable roots; explicit RevokeRoot still revokes a root. Rebind refuses active owners or unsettled executions. Gate stamps capture original owner, owner release is scoped, closed original owner commits are rejected. Close avoids clients/gate lock inversion.

Independent reviewers found root-registration loss, sticky managed error cache and silent managed driver degradation; parent reproduced and fixed each. Desktop now reports preparation failure, separates held/ready, gates first thread/start, preserves lease release obligations after errors, handles offscreen first-create retry and does not replay messages.

## Review adjudication

Latest independent safety reviewer found no new authorization relaxation or old/new owner mix-up. Its proposed unconditional rejection/collection of owner=None was not applied: unmanaged schedules/headless tasks intentionally support unbound owners; a managed attach binds before returning. No production managed owner=None reproducer was supplied. Collecting all unbound slots on any client's release would violate isolation.

Its assertion that a cancelled Starting slot proved stopped was incorrect. Parent reproduced a parked committed start: release waited 5s then returned Ok although no stop was recorded (`cancel-stop-red.log`). Parent introduced phase-based is_unsettled for scoped stop accounting; 27 managed attach tests pass (`attach-final-green.log`). Precommit barrier regression also proves provisional cleanup then reconnect.

## Final evidence

- Latest complete `NEXTEST_TEST_THREADS=2 just ci`: all 22 gates passed, 6442 passed / 41 skipped (`ci-final.log`). Default higher-concurrency runs twice reset an unchanged WS pairing initialize test; isolated test passed, then full lower-concurrency CI passed. No filter or weakened assertion.
- Frontend 196 files / 2919 passed and build passed (`desktop-final.log`, `desktop-build-final.log`). Native 292 passed (`native-final.log`).
- Actual managed sidecar target 31 passed (`managed-all-final.log`); final CI includes subsequent strict-scope fixture correction. Parent model spawn → child real bash echo hello stdout/exit0 → parent matching task wait completed; resumed conversation delegates again.
- RED logs include old-root, managed-cache, managed-driver, revoked client, client/root ledger, gate owner, desktop exception, cancel-not-stop, offscreen retry and foreign request scope.
- Sidecar and isolated App bundle built. Exact current artifact location/state is in ignored `isolated-app-location.txt`; must refresh artifact after final source commit so embedded identity matches shipped sidecar. Production App remains untouched.

## Remaining blocker

System Events reports UI elements disabled. User was asked to authorize accessibility for the host application. Real isolated App window first-use/history-reopen acceptance is NOT complete; stdio/prepare/attachment tests do not substitute. Complete repair and GUI A10 stay unchecked. No main merge, production App replacement, formal daemon termination, config/relay changes or automatic replay of ambiguous spawn requests.
