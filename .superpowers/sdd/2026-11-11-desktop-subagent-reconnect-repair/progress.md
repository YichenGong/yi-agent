# SDD ledger — plan: docs/superpowers/plans/2026-11-11-desktop-subagent-reconnect-repair.md

Base: bf9bba32. Worktree: .worktrees/desktop-subagent-fix; branch fix/desktop-subagent.

## Active implementation (do not duplicate)

- Task 1 implementer: 7d4977ff-93f0-4df5-8d6e-d1ef5198d6b7, store authorized-root reconnect and owner/reclaim boundaries; running, no delivery yet.
- Task 2 implementer: f0e273da-58fd-424a-b544-2791d6c340d5, desktop feedback/held/retry/scope and first-start preparation; running, no delivery yet.
- Task 3 implementer: 6ca243af-3cfb-4739-b04a-cc9a3ee594cc, isolated managed sidecar model-driven actual spawn/child bash/wait and bounded fixture IO; running, no delivery yet.
- App-server independent reviewer: fa2dbf12-df2d-475a-953f-f10c3f295b32, read-only review of managed failure handling; pending.
- No commits before required verification; no merge or production App replacement without confirmation (existing repair spec constraint).

## Evidence and completed checks

- Production logs read-only: new generation/client capabilities miss the attachment key while the root UUID stays stable; three attach Internal errors align with three UNIQUE tasks.id failures. Authorization must precede root-based recovery. Message capability is separate from guardian authorization capability and must not be rotated into it.
- Parent implemented app-server fail-closed for managed attach failure, preserving embedded optional delegation and history-first replay. Added managed_thread_start_refuses_a_silently_degraded_agent and managed_resume_reports_attach_failure_after_read_only_history. Both were observed RED against prior implementation and GREEN after fix.
- `just test-crate yi-agent-app-server`: 1635 passed, 7 skipped; log app-server-tests.log. PATH needs ~/.cargo/bin for nextest. This is not full CI and not GUI acceptance.

## Outstanding gates

- Review store authorization/ownership/reclaim changes and desktop changes after implementers deliver; independently verify retry after repair.
- Actual child shell stdout/exit and matching parent wait completion still unverified.
- cargo fmt --all, full just ci, desktop npm test/build and native regression still required.
- System Events UI elements enabled returned false. Isolated real App window acceptance has not run. Do not mark the complete App repair or A10 complete from prepare/attachment/stdio evidence.
- Update plan/module/bug-list/README with actual evidence only. Keep incomplete GUI acceptance unchecked.
