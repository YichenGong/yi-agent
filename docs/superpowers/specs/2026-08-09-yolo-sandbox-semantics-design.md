# YOLO Sandbox Semantics Design

## Goal

Make `--yolo` match the dangerous full-bypass behavior expected from Codex:
skip permission confirmations and, unless the user explicitly chose a sandbox,
execute shell commands without the yi-agent sandbox.

## Current Behavior

`yolo` and `sandbox` are parsed independently. `--yolo` reaches the
permission checker, but the default sandbox remains `workspace-write`. On
macOS this invokes `sandbox-exec`, which denies a shell redirection such as
`command > /dev/null` because `/dev/null` is outside the workspace.

## CLI Semantics

`--ask-for-approval` is not currently exposed by yi-agent, so this change is
limited to existing flags:

| Invocation | Permission confirmation | Shell sandbox |
| --- | --- | --- |
| default | existing behavior | `workspace-write` |
| `--dangerously-skip-permissions` | skipped | `workspace-write` |
| `--yolo` | skipped | `danger-full-access` when no sandbox is specified |
| `--yolo --sandbox <mode>` | skipped | the explicit CLI mode |
| `--yolo` plus `YI_AGENT_SANDBOX=<mode>` | skipped | the environment mode |

An explicit sandbox always wins over `--yolo`, allowing a non-interactive but
isolated session. `YI_AGENT_SANDBOX` is treated as explicit because it is a
deliberate configuration choice. `--dangerously-skip-permissions` remains the
confirmation-only alias; it must not disable sandboxing.

## Implementation

Resolve `sandbox` after calculating `yolo` and distinguish the two CLI flags:
when `cli.yolo` is true and neither CLI nor environment selects a sandbox,
choose `SandboxMode::DangerFullAccess`. Otherwise retain the existing
precedence: CLI `--sandbox`, then `YI_AGENT_SANDBOX`, then the default.

No change is needed in tool registration: both TUI and headless paths already
propagate `Config::sandbox` to `SandboxPolicy`. `DangerFullAccess` already
runs `sh -c` directly, without the platform wrapper.

## Validation

Configuration tests will prove the default `--yolo` mapping, the explicit CLI
override, the environment override, and the confirmation-only alias behavior.
The existing sandbox unit test confirms that `DangerFullAccess` does not wrap
the command in `sandbox-exec`; a direct macOS regression test will execute a
`/dev/null` redirection under that policy.

The yi-agent and yi-agent-tools targeted tests, formatter check, and relevant
project-management status documentation will be updated in the same commit.
