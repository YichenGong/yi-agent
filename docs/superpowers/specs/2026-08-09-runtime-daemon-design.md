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

## Process Ownership And Crate Boundary

`yi-agent` remains the interactive client and provider/tool bootstrap location.
`yi-agent-store` persists runtime records but must not depend on the `yi-agent`
binary crate. Introduce a runtime-facing trait in core so the daemon receives an
application-supplied worker factory rather than constructing CLI objects:

```rust
pub trait AgentWorkerFactory: Send + Sync {
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>>;
}
```

The daemon owns `RuntimeCoordinator`, `AgentSupervisor` instances, worker
handles, and IPC listeners. The binary builds one `AgentWorkerFactory` from the
selected provider, tool registry, skill loader, and permission policy, then
registers it when executing `yi-agent daemon start`. This keeps dependencies
one-way: `yi-agent` -> `yi-agent-store` -> `yi-agent-core`.

## SQLite Schema

Use SQLite WAL mode, foreign keys, and a migration table. IDs are stored as
canonical text UUIDs; payloads are versioned JSON blobs only where relational
queries are not required.

```sql
CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
CREATE TABLE sessions(id TEXT PRIMARY KEY, project_root TEXT NOT NULL, state TEXT NOT NULL,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL, config_json TEXT NOT NULL);
CREATE TABLE tasks(id TEXT PRIMARY KEY, root_session_id TEXT NOT NULL REFERENCES sessions(id),
  parent_id TEXT REFERENCES tasks(id), depth INTEGER NOT NULL, state_json TEXT NOT NULL,
  contract_version INTEGER NOT NULL, active_attempt_id TEXT NOT NULL, delivery_json TEXT NOT NULL,
  workspace_lease_id TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE attempts(id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), number INTEGER NOT NULL,
  state TEXT NOT NULL, checkpoint_json TEXT, budget_json TEXT NOT NULL, usage_json TEXT NOT NULL,
  started_at TEXT NOT NULL, ended_at TEXT, terminal_json TEXT, UNIQUE(task_id, number));
CREATE TABLE contracts(task_id TEXT NOT NULL REFERENCES tasks(id), version INTEGER NOT NULL,
  payload_json TEXT NOT NULL, digest TEXT NOT NULL, created_event_id INTEGER NOT NULL,
  PRIMARY KEY(task_id, version));
CREATE TABLE mailbox_messages(id TEXT PRIMARY KEY, recipient_task_id TEXT NOT NULL REFERENCES tasks(id),
  sender_task_id TEXT, kind TEXT NOT NULL, priority INTEGER NOT NULL, correlation_id TEXT,
  causation_id TEXT, payload_json TEXT NOT NULL, coalesced_count INTEGER NOT NULL DEFAULT 0,
  delivered_at TEXT, created_at TEXT NOT NULL);
CREATE TABLE resource_leases(id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id),
  resource_key TEXT NOT NULL, mode TEXT NOT NULL, units INTEGER NOT NULL, state TEXT NOT NULL,
  acquired_at TEXT NOT NULL, released_at TEXT);
CREATE TABLE events(id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
  task_id TEXT, attempt_id TEXT, actor_json TEXT NOT NULL, kind TEXT NOT NULL,
  payload_json TEXT NOT NULL, created_at TEXT NOT NULL);
```

Create indexes on `(tasks.root_session_id, tasks.state_json)`,
`(mailbox_messages.recipient_task_id, delivered_at, priority)`,
`(resource_leases.resource_key, state)`, and `(events.session_id, id)`. A task
transition writes `tasks`, optional `attempts`/mailbox/lease rows, and an event
in one `BEGIN IMMEDIATE` transaction. The snapshot update must roll back if the
event insert fails.

## IPC Protocol

The socket protocol is newline-delimited JSON frames with maximum 1 MiB frame
size. Each frame has `protocol_version`, request/event ID, and a tagged payload.

```json
{"protocol_version":1,"request_id":"...","command":{"type":"InspectTask","task_id":"..."}}
{"protocol_version":1,"request_id":"...","result":{"type":"TaskSnapshot","task":{}}}
{"protocol_version":1,"event_id":42,"event":{"type":"TaskTransition","task_id":"..."}}
```

Version mismatch returns `UnsupportedProtocol { supported_min, supported_max }`
and closes the connection. Command errors are typed: `DaemonNotRunning`,
`NotFound`, `InvalidState`, `AuthorityDenied`, `ConfirmationRequired`,
`Conflict`, `Validation`, `RateLimited`, and `Internal`. Error payloads never
include secrets or raw environment values.

`SubscribeEvents { after_event_id, filters }` sends a transactionally consistent
snapshot with its high-water event ID, then events with IDs greater than that
value. Per-client event buffers hold 1,024 frames; overflow emits one
`ResyncRequired` frame and closes the subscription. The client reopens from its
last acknowledged event ID or requests a fresh snapshot.

## Socket, Lock, And Credentials

Runtime files live under `~/.yi-agent/runtime/`: `runtime.sock`, `runtime.lock`,
`state.sqlite`, and `logs/`. The directory is mode 0700 and socket mode 0600.
Daemon startup acquires an exclusive lock before binding; a live process returns
`AlreadyRunning`, while a stale lock is removed only after PID liveness and
socket connection checks fail.

The daemon receives provider profiles by reference. A profile resolves secrets
from the daemon process environment or OS secure storage at worker start. IPC
clients cannot submit a raw API key, and `InspectTask` returns only a profile
name/identifier. Secret-bearing tool inputs are represented in events by a
redaction marker and content digest.

## Stop And Recovery Algorithm

```text
daemon stop:
  reject new sessions and admissions
  publish Draining event
  request worker safe checkpoints
  after grace deadline cancel remaining workers
  persist Paused/Interrupted state and release permits
  close socket and release lock

daemon startup:
  migrate database
  mark attempts recorded as Running/Waiting as RecoveryRequired
  release all process-local leases; retain workspace leases for reconciliation
  publish RuntimeRecovered event
  never schedule automatic retry or tool replay
```

`Resume` always creates a fresh attempt with a recovery controller instruction:
inspect the recorded worktree, `git status`, latest commit, required tool state,
and prior checkpoint before making changes. If workspace inspection cannot prove
a safe base, the task becomes `Blocked(RecoveryConflict)`.

## Daemon Test Matrix

| Test | Assertion |
|---|---|
| `migration_is_idempotent` | opening twice produces the same schema version |
| `transition_and_event_commit_atomically` | injected event failure leaves snapshot unchanged |
| `subscriber_replays_after_cursor` | second client receives ordered later events |
| `slow_subscriber_requires_resync` | buffer overflow closes only that subscriber |
| `second_daemon_is_rejected` | lock/socket ownership is exclusive |
| `restart_marks_live_attempt_recovery_required` | no worker is silently restarted |
| `inspect_never_returns_secret` | serialized snapshots/logs contain only redacted references |
