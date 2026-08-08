# Runtime Daemon And Persistence Design

## References

- [Architecture decision index](2026-08-09-subagent-architecture-design.md)
- Existing store boundary: `yi-agent-rs/crates/yi-agent-store/src/lib.rs`
- Existing tracing lifecycle: `yi-agent-rs/crates/yi-agent/src/tracing_init.rs`

## Lifecycle

`yi-agent daemon start` explicitly creates one user-local daemon. It owns the
runtime workers after startup. `status`, `stop`, and `logs` are clients. No
automatic launch agent/service is installed. A daemon that is stopped or absent
runs no schedules and records missed triggers without catch-up by default.

## Durable Records

SQLite owns snapshots and an append-only event journal. Tables are: `sessions`,
`tasks`, `attempts`, `contracts`, `contract_amendments`, `mailbox_messages`,
`leases`, `deliveries`, `reviews`, `permission_requests`, `schedules`, and
`events`. Each transition writes its snapshot row and event in one transaction.
Credentials are references only; raw keys, secret tool inputs, and environment
values never enter tables or logs.

## IPC

Use a current-user-only Unix socket (platform equivalent elsewhere), a single
daemon lock, and versioned envelopes. Commands include `CreateSession`, task
controls, review/permission decisions, `SubscribeEvents`, and daemon status.
Subscription first returns a snapshot, then ordered events after an event ID.
Expired cursors receive a replacement snapshot. Slow subscribers are dropped
after a bounded buffer and reconnect from their last observed event ID.

## Recovery

Graceful stop stops admission, requests safe checkpoints, and persists paused
attempts. On process loss, in-flight attempts become `RecoveryRequired`, leases
are reconciled, and no provider/tool/Git call is replayed. Resume creates a new
attempt whose first required action is Git/command-state inspection.

## Required Tests

- Single-instance lock and private socket permissions.
- Atomic snapshot/event writes and event replay to a second client.
- Cursor expiry snapshot fallback and slow-subscriber behavior.
- Graceful stop checkpoint, crash recovery classification, and no auto-replay.
