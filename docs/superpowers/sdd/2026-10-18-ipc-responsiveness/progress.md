# SDD ledger — plan: docs/superpowers/plans/2026-10-18-ipc-responsiveness.md

Task 1: complete (commits 33db139..a15e0fb, review clean)
Task 1: minor (deferred): wait_readable timeout has no lower floor (0ms -> poll spin) if ACCEPT_POLL is ever lowered below 1ms
Task 1: minor (deferred): poll error path is fail-stop, not EINTR-aware (unreachable today: no signal handlers in plugin crates)
Task 1: minor (deferred): non-WouldBlock accept error tears down the server (pre-existing, unchanged exposure)

Task 2: complete (b3c6c6f5 + 1dfc6fed, review found one Important bug, fixed)
Task 2: Important (FIXED in 1dfc6fed): respond held the repository guard across the whole match; each connection has its own handler thread, so a WaitAgent with timeout_ms: None would have stalled every Status poll behind the lock. Scope the borrow per arm; with_repository is now the only path, so holding past an arm is not expressible. Regression test an_arm_that_does_not_use_the_repository_is_not_blocked_by_one_that_does: fails on pre-fix code (250ms), passes now.
Task 2: measurement (implementer, 200/100 samples, one machine): the per-request cost this task removed is only ~0.2ms; 45ms lives in the accept loop 10ms sleep. Setting that sleep to 100us drops Status p50 46.8ms -> 0.54ms. The plan's Task 2 rationale (~45ms from Runtime::new + repository open) is NOT supported by measurement. Task 3 is therefore the dominant fix.
Task 2: minor (deferred): with_repository is unused outside respond+tests; kept as the documented interface entry point.
Task 2: pre-existing failure (not ours): runtime_coordinator::an_autonomous_session_refuses_a_workdir_that_is_not_a_git_worktree is locale-dependent (git emits Chinese "不是 git 仓库"); identical at HEAD a15e0fbb.

Task 3: complete (c826428d, verified independently by lead)
Task 3: evidence (lead reproduced, same machine): daemon_handler_thread_leak (1500 fresh connections) 21.97s before -> 0.83s after. Implementer measured 63.10s -> 0.88s. Absolute baseline varies with machine load; the direction and magnitude are unambiguous. This is the dominant fix for the reported latency.
Task 3: minor (deferred): event/trace subscription producer threads still use 10ms sleeps (ipc.rs:1837/1950); out of scope, they only affect streamed row latency.
Task 3: minor (deferred): libc::poll not EINTR-retried (same shape as Task 1's helper; no signal handlers in these crates).
Task 3: minor (deferred): a persistently failing poll logs error! each iteration (cannot spin hot; matched existing convention).

Task 4: complete (73ec922a, review clean, lead re-ran the suite: 757 passed, fmt clean)
Task 4: accepted risk (spec-recorded): detached board/* and plugin/* handling loses deterministic relative ordering vs other RPCs. Spec accepts it (registry write is atomic tmp+rename; create_with_project is idempotent; desktop issues neither concurrently).
Task 4: minor (deferred): no single test drives spawn -> fatal_tx -> select! -> serve returns Err end to end; covered in two halves (write_or_fatal_reports_a_local_write_failure_on_the_fatal_channel + driver_reports_finished_when_writer_fails).
Task 4: minor (deferred): write_or_fatal takes an unused `_id` (kept for the brief's declared arity).
Task 4: note: AppState lost 4 fields (board_dir/resident_dir/plugin_command/board_launcher) that only these domains read; removing them avoided new dead-code warnings.

Task 5: complete (fce63c30, review clean, lead re-ran the suite: 759 passed, fmt clean)
Task 5: accepted change: reconciliation now reads a snapshot up to ~1s old instead of a live read; a permanently dead daemon clears the entry within ~5s (PREFETCH_FAILURE_LIMIT=5) and falls back to live. Card state changes are seen at worst one tick (3s) later.
Task 5: minor (deferred): a dead daemon now logs one prefetch-failure line per project per second (new volume, same kind as Task 3's poll-error convention).
Task 5: minor (deferred): run_once kept with #[allow(dead_code)] (no production caller now; the 8 existing sites are tests + the live-read entry point).
Task 5: deviations (all justified by the implementer): used call["method"] not call["command"]["method"] (the brief's key would have made the assertion vacuous); tick param named cards_snapshot (snapshot already taken); followed the call-site snippet over the contradictory in-tick snippet.

Task 6: complete (8b51b2b3) — evidence file docs/superpowers/plans/2026-10-18-ipc-responsiveness-evidence.md; plan Task 2/3 rationales corrected; docs/project-management updated (store 5/5, app-server 47/47 + README index).
Task 6: all suites verified by lead: plugin workspace 250 passed; store --lib 73; store runtime_ipc 97; app-server --lib 759; fmt clean in both workspaces. Only failure is the pre-existing locale-dependent runtime_coordinator git-message test.
All 6 tasks complete. Branch perf/ipc-responsiveness ready to merge into main.
