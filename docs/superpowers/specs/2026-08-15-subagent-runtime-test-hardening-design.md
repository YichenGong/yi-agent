# Subagent Runtime Test Hardening Design

## Goal

Add deterministic P0/P1 automated tests that prove the subagent runtime
preserves task, delivery, IPC, scheduling, and worktree safety invariants
across process boundaries and failure paths. The scope excludes the CLI/TUI
black-box matrix and real-LLM tests.

## Scope and constraints

- Work only in the existing `feat/subagent-core` linked worktree.
- Use mock workers/providers, temporary repositories, temporary SQLite stores,
  and local Unix sockets; no real LLM API or network dependency.
- Keep test inputs deterministic. Do not add a property-test/fuzzing crate in
  this increment; use bounded table-driven sequences instead.
- Run Cargo commands serially because this repository documents workspace lock
  and memory contention risks.
- Preserve the runtime's append-only event journal and its existing direct
  parent authority model; tests must not add test-only production bypasses.

## Test architecture

The test suite is split by failure domain rather than appended to existing
large files. Each file owns one boundary and only imports the existing public
runtime API plus local test helpers.

| File | Responsibility |
|---|---|
| `crates/yi-agent-store/tests/subagent_runtime_e2e.rs` | A daemon-backed lifecycle and concurrent-control suite using injected mock workers and temporary repositories. |
| `crates/yi-agent-store/tests/subagent_ipc_protocol.rs` | Raw Unix-socket protocol tests for fragmented, truncated, and invalid framed requests plus healthy-client recovery. |
| `crates/yi-agent-core/tests/subagent_invariants.rs` | Pure reducer replay and resource-coordinator long-sequence invariants. |
| `crates/yi-agent-tools/tests/subagent_worktree.rs` | Additional Git consistency and cleanup-failure scenarios using temporary repositories. |
| `crates/yi-agent/src/config.rs` | Environment-isolated compact-default regression tests so full binary tests are deterministic. |

Existing `runtime_ipc.rs` and `runtime_coordinator.rs` remain their current
focused coverage suites. Small helpers may be copied into a new test file
rather than expanding public production interfaces solely for test reuse.

## Lifecycle and persistence tests

`subagent_runtime_e2e.rs` will create a temporary Git repository, start a
local daemon with a deterministic injected worker factory, attach/activate an
application root, and spawn one child. The child factory reports either a
text-only completion or a delivery against its assigned workspace. The test
will then:

1. wait for the direct child and assert the terminal report is returned once;
2. preview and confirm a direct-parent review decision;
3. inspect the task and event journal before restart;
4. stop and restart the daemon over the same SQLite database;
5. re-read the task, report, review decision, and ordered events; and
6. assert no worker is restarted and no additional attempt or delivery is
   created during recovery.

A second test uses synchronization barriers to submit competing control
requests against the same state: two confirmations for the same cancel token,
and incompatible review confirmations derived from one delivery. It asserts
that at most one durable decision succeeds, a consumed token cannot create a
second terminal transition, and the final task/attempt snapshot agrees with
its event sequence.

## IPC protocol tests

`subagent_ipc_protocol.rs` will write encoded request envelopes to the Unix
socket in every chunk size from one byte through the full request length. Each
complete request must yield exactly one versioned response with its request
ID. It will also send a truncated JSON frame, an unknown command, an
unsupported protocol version, and a frame larger than the configured maximum.
For every malformed input, the daemon may return its typed error or close that
client connection, but it must remain alive: a newly connected healthy client
must obtain `Status`, and a subscription reconnecting from a saved cursor must
receive ordered non-duplicated events.

The suite does not test internal private framing functions directly. It tests
the public socket boundary, which protects the partial-frame regressions fixed
in `451d063`.

## Reducer and scheduler invariants

`subagent_invariants.rs` will use fixed event tables, not random generation.
For each event sequence it will retain a clone of the task before every
reduction and assert one of two outcomes:

- success produces a state satisfying public invariants (active attempt is
  current, terminal attempt is closed, and retry/rework creates exactly one
  successor); or
- failure leaves the entire task exactly unchanged.

Two separately constructed equivalent tasks will replay the same valid event
sequence with the same timestamps and must produce equal snapshots. Explicit
negative sequences prove that a terminal attempt cannot return directly to
`Running`, stale-attempt events cannot mutate a successor, and review events
cannot target a wrong delivery or non-parent actor.

The scheduler portion repeatedly admits/releases/cancels/ages requests from
multiple roots and parents against small capacities. After every operation it
will assert capacity is not exceeded, exclusive and shared leases do not
conflict, repeated release is harmless, and cancelled/expired queue entries
cannot later receive a lease. A bounded round-robin contention sequence proves
each continuously runnable root receives an admission.

## Git worktree failure tests

The existing worktree integration file will add focused temporary-repository
cases:

- a reviewed head that is no longer an ancestor/descendant valid delivery is
  rejected before merge;
- a direct parent whose head changed after inspection cannot merge a stale
  delivery unless the recorded inspection contract still matches;
- a missing worktree or manually removed child branch produces a domain error
  and retains the durable delivery evidence rather than reporting acceptance;
- concurrent attempts to accept one child delivery allow one merge only; and
- failed clean-up after an accepted merge leaves auditable state and a retryable
  clean-up path, never deletes a dirty worktree.

Tests use actual `git` commands and assertions on commit ancestry, branch
existence, and file content. They do not mock Git result parsing.

## Configuration test isolation

The two compact-default tests in `config.rs` will acquire the existing
`ENV_TEST_MUTEX` and use `isolated_config_env()` before calling `load`. This
matches adjacent configuration tests, removes cross-test environment mutation,
and preserves the documented default calculation:

```text
default context length = 200_000
default compact ratio = 80
compact threshold = 160_000
```

## Verification

Run these commands one at a time from `yi-agent-rs/`:

```bash
cargo test -p yi-agent-core
cargo test -p yi-agent-tools
cargo test -p yi-agent-store
cargo test -p yi-agent
cargo fmt --all -- --check
cargo clippy -p yi-agent-core -p yi-agent-tools -p yi-agent-store -p yi-agent -- -D warnings
git -C .. diff --check main...HEAD
```

The implementation is acceptable only when all commands exit successfully.
The project-management subagent-runtime entry will be updated in the same
change with the concrete test commands for this hardening coverage; the module
index count will be updated if and only if a previously unchecked feature is
fully evidenced by the new verification.
