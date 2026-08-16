# Subagent Runtime Project Isolation Design

## Goal

A single user-local subagent runtime daemon must create each attached application root and all of its descendants in the repository from which that client invocation originated. One project must never receive a worktree derived from another project that happened to start the daemon first.

## Root Cause

The runtime socket and database are intentionally user-local (`~/.yi-agent/runtime`). `DaemonAgentWorkerFactory` is constructed when that daemon starts and currently holds one `DaemonWorkspaceService`, initialized from that first process's `Config.workdir`. `AttachApplicationRoot` carries only an idempotency key, so a later client cannot identify its repository. The runtime consequently creates every new root through the first factory workspace service.

## Decision

Extend `AttachApplicationRoot` with a required absolute `workspace: PathBuf`. The CLI derives it from the resolved `config.workdir`, not from daemon process CWD. The IPC handler passes it to `RuntimeCoordinator::attach_application_root`.

Add an application-factory method that creates an `AgentWorkspaceService` for one caller workspace. The default implementation retains existing behavior by returning the factory's ordinary workspace service. `DaemonAgentWorkerFactory` overrides it by creating a fresh `DaemonWorkspaceService` rooted at the supplied caller workspace. The coordinator stores the selected service for each attached root session. Root preparation uses that session service; child preparation uses the same service and therefore remains in the root project. Existing/re-attached roots retain their already persisted worktree and reinstall a service derived from the caller workspace only after verifying that it resolves to the persisted root workspace's repository root.

## Data Flow

1. A client resolves its work directory during configuration load and sends it in `AttachApplicationRoot`.
2. The daemon selects a workspace service from its application worker factory for that path.
3. The coordinator binds that service to the newly attached root session before creating its root worktree.
4. Root and descendant workspace preparation select the service associated with their root session.
5. A repeat attach returns the persisted root workspace. Its requested repository must match the recorded root repository; otherwise the daemon returns a validation/conflict error rather than exposing a root from another project.

## Error Handling and Compatibility

`workspace` is mandatory for the version-1 `AttachApplicationRoot` request. Existing external clients that omit it receive normal serde request validation failure rather than silently falling back to daemon startup CWD. The CLI always sends its resolved configured workdir.

The coordinator rejects an empty/unusable workspace service response and rejects a reattachment whose selected service's repository does not match the persisted session root. It does not remove existing worktrees on such rejection.

## Testing

- Add a runtime IPC regression test with a recording per-workspace factory. Attach project A then project B to the same daemon and assert their root workspaces have distinct repository roots matching their respective requests.
- Assert child preparation for project B inherits the B workspace service, proving all descendants remain project-isolated.
- Retain idempotent attach coverage and update every request constructor to include a workspace.
- Add CLI-level unit coverage that attach requests use the loaded configuration workdir.

## Non-Goals

This change does not move the user-local daemon/socket, create one daemon per project, alter schedule workspace semantics, or modify Git worktree naming and delivery behavior.
