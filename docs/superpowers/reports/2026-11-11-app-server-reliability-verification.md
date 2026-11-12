# App-server reliability verification

Date: 2026-11-12
Branch: `fix/app-server-reliability`
Baseline: `a579d175`

## Scope

Six approved reliability fixes only: multi-connection revocation, turn-permit cleanup, bounded WS replies/writes, safe persistence/quiescence barrier, required final-material admission, and bounded management ingress. Public envelopes and Delivery subscription scopes are unchanged. Ordinary-session snapshot/probe/delta reconciliation is not implemented. No real LLM tests, app deployment, or running-instance restarts were performed.

## Regression evidence

Observed old-path failures before the corresponding implementation:

- Extended `a_revoked_device_is_disconnected_and_its_token_invalidated`: old same-device connection remained open and the close deadline elapsed. The repaired test closes both connections and invalidates the token.
- `permit_is_released_when_session_clears_the_turn`: production consumer helper left one live permit; expected zero. Matching completion now releases it; stale completion preserves it.
- `directed_reply_evicts_only_a_stalled_ws`: outer 11-second deadline elapsed on the old unbounded reply. The repaired reply returns Closed after the 10-second budget, retaining the fast client.
- `final_content_is_delivered_or_the_connection_explicitly_fails`: local final item was dropped while its connection remained registered. Required admission now disconnects explicitly on full queues.
- `a_failed_append_still_records_the_completion`: delete returned `{result:{}}` after append and file-delete failure. Repaired delete/resume refuse unsafe mutation, preserving completion watermark, final checkpoint and original session. An additional red/green extension proved storage repair allows a real persistence retry and readable final assistant content rather than permanent refusal.
- `prepare_completes_while_the_business_loop_is_blocked`: 65 business frames filled the 64-slot queue and blocked the subsequent private request. Managed ingress now rejects overflow and completes prepare independently.
- `full_management_output_closes_instead_of_stalling_ingress`: full local output blocked bootstrap ack past one second. Control output now fails explicitly instead of stalling the reader.

Additional passing boundary coverage:

- `old_connection_teardown_preserves_a_new_connection_of_the_same_device`
- `required_content_preserves_subscription_filtering`
- `persistence_barrier_refuses_closed_completion_and_retains_session`
- `persistence_barrier_timeout_retains_active_session`
- `dropping_a_writer_pause_resumes_the_original_session`
- `failed_file_delete_retains_the_original_session_for_retry`
- `saturated_business_ingress_still_delivers_private_authorize_responses`
- `management_ingress_refuses_excess_work_without_starting_backend`

## Verification runs

- Initial single-process cargo test baseline: 1636 passed, 1 failed, 7 ignored. The failure was `a_second_connection_is_accepted_alongside_the_first` (reset during handshake); immediate isolated rerun passed. Its root cause was not established. Later complete nextest runs passed this test; isolation alone does not prove the reset is fixed.
- Intermediate app-server nextest: 1639 passed, 7 skipped.
- First CI attempt stopped at lint: an unused wrapper and collapsible-if introduced by this repair. Both were fixed; app-server all-targets/all-features clippy passed.
- Intermediate complete `just ci`: 6450 tests passed, 41 skipped; additional 50 binding checks passed; final log `CI passed`. Subsequent boundary additions require a fresh final run below.
- Final implementation CI, first attempt: 6452 passed, 1 failed, 41 skipped; exit 100. `ws::tests::a_pair_code_minted_by_another_instance_redeems_over_the_ws` reset while waiting for initialize after successful token exchange. Checked registration, pump startup, device-table atomic writes and shared-loop gates; no definitive root cause was established. Without changing production logic, `cargo nextest run --all-features -p yi-agent-app-server -E 'test(a_pair_code_minted_by_another_instance_redeems_over_the_ws)' --stress-count 100` passed 100/100 iterations. The subsequent full CI rerun is recorded below. This evidence does not claim all intermittent WS resets are fixed.
- Final implementation CI rerun: complete `just ci` passed, `FINAL_CI_EXIT=0`, workspace tests 6453 passed / 41 skipped, binding tests 50 passed. Log: worktree `yi-agent-rs/target/reliability-final-ci-rerun.log`. All 22 CI gates completed, including fmt, all-targets/all-features clippy, workspace tests, build, bindings, MSRV 1.89, affected-check and dependency audit.
- Merge-result CI: not run; awaiting integration choice.

## Review notes and limits

No independent reviewer tool is available in this session. A direct diff and producer/consumer-path review was performed, followed by full CI. The review found and repaired a retry state that would otherwise permanently reject resume after storage repair, and moved model-startup failure through the same persistence finalizer.

`Finished` is execution completion, not a save receipt. A lifecycle mutation requires the additional paused-writer acknowledgement at the driver's idle boundary. The held control channel suspends old-driver prompts and commands; early return resumes that same driver, successful mutation stops it. Timeouts/closed channels never imply success. One failed final snapshot is retained in driver memory; its best-effort checkpoint provides crash recovery when writable. A retry clears the append record before checkpoint cleanup so a cleanup failure does not repeat the append. This does not claim transactional multi-file deletion or universal recovery from arbitrary filesystem corruption.

Final content overload explicitly disconnects even local stdio; clients recover through existing reconnect/history behavior. No new complete snapshot/probe recovery contract or zero-disconnect guarantee is claimed.

Known existing build notices: ts-rs serde-attribute warning, Darwin large unwind-section linker warning, and html5ever future incompatibility. Audit completed with the repository's allowed rustix unsoundness warning and chacha20 yanked warning; no new dependencies or exemptions were added.
