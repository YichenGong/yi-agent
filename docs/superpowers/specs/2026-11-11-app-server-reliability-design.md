# App-server reliability repair design

Date: 2026-11-11
Baseline: a579d175
User approved the six-part compatibility-first design and explicitly accepted failing delete/resume when the old writer cannot be confirmed quiescent.

## Scope and invariants

1. Device revocation invalidates every connection authenticated as that device, not only its most recent connection. Teardown must not remove other connections. Recheck durable authorization at registration to close authentication/revocation races.
2. Turn completion releases active_turn_id and turn_permit together, only for the matching turn. Normal completion, interrupt consumption and failure rollback share the rule; stale completion cannot release a newer turn.
3. WS directed replies and socket writes are bounded by the existing 10-second slow-consumer budget. Timeout disconnects only the affected connection. Local reliable backpressure retains existing semantics.
4. Execution completion is not persistence success. Append failure is visible, retains recoverable partial material, and cannot satisfy a persisted barrier. Completion/last-read timestamps retain their existing semantics. Delete/resume fail retryably without removing/rebuilding the session when quiescence cannot be confirmed; file deletion errors are RPC failures, not successful deletes. No tombstone-based immediate delete in this increment.
5. Subscription scope and reliability are separate policies. Final items/batches must enter the existing bounded queue or explicitly disconnect that client (including local stdio) for recovery; no new driver wait is added. Existing list/global and historical backpressure stays unchanged; deltas remain nonblocking. Subscription filtering and existing envelopes do not change. This is the final-material portion of the existing session-completeness spec, not completion of its v1 snapshot/probe/delta-reconciliation work.
6. Managed stdio ingestion cannot await a full business queue. Preserve accepted business ordering with bounded buffering; reject overflow explicitly without unbounded allocation or task-per-frame fanout. Authentication remains checked at ingress, management remains private, and close stops ingress/background forwarding. Management output must not itself stall ingress behind a full local output queue.

## Alternatives

- Recommended: localized compatible lifecycle/transport fixes with regression tests, then implement the independent read-only snapshot project separately.
- Immediate-delete tombstones/generation fencing: permits faster delete but changes every write path and crash recovery; not selected.
- Unbounded queue/task fanout or broad protocol replacement: hides pressure or creates migration risk; rejected.

## Validation

Write failing tests before each implementation. Cover multiple device connections and late teardown, matching/stale turn cleanup, full output queues and timeout, append failure and retained checkpoint, missing/late completion and refusal to mutate, final-content reliability and filtering, business queue saturation followed by management frames, bounded overflow and shutdown. Use no real LLM calls. Run app-server all-features tests, affected tests, cargo fmt --all, and just ci before commit and on the merge result. Update module progress and index with verifiable criteria. Do not deploy/restart the running app or touch the pre-existing lifecycle example.

## Implemented lifecycle protocol

`TurnEvent::Finished` settles execution and releases its admission permit; it is deliberately not a persistence receipt. Lifecycle RPCs additionally send `SessionCommand::PauseWriter`. At the idle boundary the driver retries any retained final snapshot, then acknowledges with a one-shot control channel and stops processing prompts/commands. A held `PausedWriter` proves old-driver quiescence. Dropping it resumes the same driver (including when activation or file deletion fails); successful replacement/deletion explicitly tells the paused driver to stop. A five-second timeout or a missing/failed acknowledgement refuses mutation with `retryable` and `sessionRetained` error data. No timeout is interpreted as success.

One pending final snapshot is retained in driver memory and, for failed turns, in a best-effort final checkpoint. New turns and session commands cannot overwrite it before a real persistence retry succeeds. Once append succeeds its retry record is cleared before checkpoint cleanup, so cleanup retries do not duplicate the append. Completion watermarks still record execution completion independently of append success.

Managed ingress keeps the existing 64-slot ordered business queue, uses nonblocking admission and sends explicit overload responses. Ingress/control output uses nonblocking bounded admission; failure disconnects local stdio. Slow management work is capped at 32 in-flight tasks per handle. Authentication stays at ingress; private responses are intercepted before business admission.
