# Task 1 report: authorized root reconnect

## Status

DONE. Store-side reconnect repair implemented test-first; true red reproduced before
production changes; all relevant store/app-server suites green; clippy and fmt clean.

## Root cause (evidence)

`attach_application_root` authenticated the presented key, then looked the durable
attachment up by that key only (`application_root_attachment(idempotency_key)`).

A managed capability is minted by the guardian per `(generation, client_run_id, root_id)`,
so it changes when the App reconnects under a new run id or when the daemon restarts.
The root id, by contrast, is derived from the scope and thread key and is deliberately
stable. On a reconnect the lookup therefore missed the row that already owned the root,
execution fell through to `create_session_root(..., Some(authorized_root))`, and the
INSERT hit `UNIQUE constraint failed: tasks.id`.

This matches the production daemon trace (`session-20261111-072932.jsonl`: three
`UNIQUE constraint failed: tasks.id`).

## RED (before production code)

```
cargo test --all-features -p yi-agent-store --test managed_attach_authority \
  a_new_capability_for_the_same_authorized_root_reattaches_it
```

Failed with:

```
Repository(Sql(SqliteFailure(Error { code: ConstraintViolation, extended_code: 1555 },
  Some("UNIQUE constraint failed: tasks.id"))))
```

## Fix

- `repository/application_root.rs`: new `application_root_attachment_for_root(&TaskId)`,
  selecting the row that created the root (attached rows first, then oldest).
- `runtime.rs`: the lookup runs **after** `authorize_attach_key` verified the capability
  for that exact root. It is only reached for an authorized conversation root, never for
  a project attach or an unmanaged key.
- `runtime.rs`: the existing restore branch now uses `existing.idempotency_key` for every
  durable write (`reattach_application_root`, legacy capability fallback), so a reconnect
  addresses the row that created the root.
- `runtime.rs` + `lifecycle.rs`: `bind_verified_root` may move a root's owner between
  clients of one generation, and is only called when the capability was verified for
  that root. `bind_root` keeps refusing takeovers, so naming a root id is still not
  authority.
- Not changed on purpose: `AuthorizationLedger::issue` does not treat a revoked root as
  permanently unissuable. The guardian uses `RevokeRoot` to roll back a binding whose
  push to the daemon failed, so a tombstone there would make a legitimate retry fail
  forever. The test instead asserts the store-level property: a revoked root is
  unreachable and a project attach opens its own root.

## GREEN (actually run)

```
cargo test --all-features -p yi-agent-store --test managed_attach_authority   -> 24 passed
cargo test --all-features -p yi-agent-store --test runtime_ipc                -> 118 passed
cargo test --all-features -p yi-agent-store --test runtime_coordinator        -> 119 passed
cargo test --all-features -p yi-agent-store --test runtime_client_ownership   -> 21 passed
cargo test --all-features -p yi-agent-store --test session_delegation_lifecycle -> 15 passed
cargo test --all-features -p yi-agent-store --test subagent_runtime_e2e       -> 5 passed
cargo test --all-features -p yi-agent-store --test maintenance_ipc            -> 6 passed
cargo test --all-features -p yi-agent-store --lib                             -> 220 passed
cargo test --all-features -p yi-agent-lifecycle                               -> all passed
env -u YI_AGENT_RUNTIME_DIR cargo test --all-features -p yi-agent-subagent --lib -> 191 passed
cargo clippy --all-targets --all-features -p yi-agent-store -- -D warnings    -> clean
cargo fmt --all -- --check                                                    -> clean
```

New tests in `managed_attach_authority.rs`:

- `a_new_capability_for_the_same_authorized_root_reattaches_it` (same generation, new client)
- `a_restarted_daemon_reattaches_the_same_authorized_root` (new generation, same DB)
- `reattaching_an_authorized_root_keeps_its_linked_roots`
- `reattaching_an_authorized_root_restores_its_detached_attachment`
- `an_authorized_root_cannot_be_reattached_to_another_workspace` (real bridge reservation)
- `a_revoked_root_cannot_be_reattached`
- `a_known_root_id_is_not_an_attach_credential`

## Risks / notes

- The capability stored in `application_root_attachments.capability_secret` is the
  daemon-issued message capability, not the guardian attach capability. It is restored
  as-is and never rotated; rotating it would break `detach` and legacy-upgrade checks.
- `YI_AGENT_RUNTIME_DIR` is set in this machine's shell and makes two unrelated
  `yi-agent-subagent` attach tests fail; they pass with the variable unset.
- Committed in this worktree as `78269c10`. No merge, no production runtime touched;
  full `just ci` remains the parent's step.
