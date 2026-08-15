# Subagent Complete Coverage and Real-Test Configuration Design

## Goal

Complete automated coverage for the manual subagent acceptance cases across deterministic runtime, Git, sandbox, CLI, TUI/slash, and ignored real-LLM layers; add an independently configured `Real LLM Tests` Web tab that saves and validates explicit regression credentials without exposing secrets.

## Scope

This increment includes all previously identified manual acceptance boundaries:

1. spawn, wait, and text-result return;
2. coding delivery, review, and direct-parent acceptance;
3. reject and rework review paths;
4. cancel preview/confirm atomicity;
5. direct-child capacity and release after terminal completion;
6. child sandbox and path-isolation enforcement;
7. daemon restart persistence and recovery observability;
8. CLI, TUI, and slash-command task observation and human intervention;
9. ignored real-LLM subagent end-to-end smoke coverage; and
10. saved, validated real-test provider configuration in the existing Web UI.

Default tests use mocks, temporary SQLite databases, local Unix sockets, and temporary Git repositories. They do not contact an LLM provider. Only explicitly ignored tests call a real provider.

## Configuration contract

### Dedicated environment variables

The real regression configuration uses only these dedicated variables:

```text
YI_AGENT_REAL_LLM_PROVIDER
YI_AGENT_REAL_LLM_API_URL
YI_AGENT_REAL_LLM_MODEL
YI_AGENT_REAL_LLM_API_KEY
```

`provider` accepts exactly `anthropic` or `openai`. `api_url` must be a valid absolute `http` or `https` URL. `model` and `api_key` must be non-empty.

### Resolution behavior

The test-only resolver returns one of three outcomes:

1. **Explicit configuration** — if `YI_AGENT_REAL_LLM_PROVIDER` is set, all four dedicated variables are required. Missing values fail the ignored test with one error that names every missing variable. It must not skip.
2. **Fallback configuration** — if no dedicated provider is set, select Anthropic when `ANTHROPIC_API_KEY` exists; otherwise select OpenAI when `OPENAI_API_KEY` exists. The selected provider uses the normal provider defaults or normal runtime configuration for URL/model where the existing harness already supports it.
3. **No configuration** — if neither dedicated configuration nor a fallback provider key is present, print `SKIPPED: no real LLM API key configured` and return successfully without a network request.

Dedicated values always win over ordinary runtime values. The resolver and all test diagnostics must never print the API key.

## Web UI: `Real LLM Tests` tab

The existing Web configuration UI gets a peer tab named `Real LLM Tests`; it is not a new page and it does not execute tests.

### Fields

| Label | Stored key | Behavior |
|---|---|---|
| Provider | `YI_AGENT_REAL_LLM_PROVIDER` | Required dropdown: `anthropic` or `openai`. |
| API URL | `YI_AGENT_REAL_LLM_API_URL` | Required absolute HTTP(S) URL. |
| Model | `YI_AGENT_REAL_LLM_MODEL` | Required non-blank text. |
| API Key | `YI_AGENT_REAL_LLM_API_KEY` | Password input; full value is write-only. |

The tab saves user-level configuration in the existing `.yi-agent/.env` / secret-storage mechanism. It must not overwrite normal agent provider/model/key variables.

### API and secret behavior

Use separate real-test-config endpoints and DTOs rather than reusing ordinary agent config objects:

- read returns provider, URL, model, `api_key_configured`, and optionally a fixed masked suffix;
- save accepts a non-empty replacement key but an empty key retains the existing stored key;
- a separate clear-key request removes the key after a confirmation signal;
- validate checks shape and presence only: provider, URL scheme, model, key presence, and storage readability. It never sends an LLM request.

The full key must never be included in HTML, JSON responses, logs, errors, repository configuration, test output, or Git-tracked files.

## Deterministic test coverage

### Runtime E2E and control races

Extend `crates/yi-agent-store/tests/subagent_runtime_e2e.rs` using injected deterministic worker/workspace factories.

- Same cancel confirmation token, confirmed concurrently through two clients: exactly one `TaskCancelled`, one typed rejection, exactly one terminal cancellation event, and identical durable state after restart.
- Competing accept and reject review confirmations for one delivery: exactly one persisted review row and one successful response; only an accepted winner may advance parent Git state.
- Rework lifecycle: current attempt closes once, one successor attempt exists, feedback appears in successor `WorkerStart.initial_user_messages`, and the old delivery cannot be confirmed again.
- Existing delivery/reject/restart coverage remains and asserts that recovery does not start another worker.

### IPC reconnect

Extend `crates/yi-agent-store/tests/subagent_ipc_protocol.rs` with a public socket subscription scenario. Read one event, retain its ID, close the subscriber, cause multiple events, and reconnect with that cursor. Replay IDs must be strictly increasing, exclude the saved ID, contain no duplicates, and continue with later live events.

### Real Git delivery evidence

Extend `crates/yi-agent-tools/tests/subagent_worktree.rs` with actual Git repository tests:

- a committed child delivery accepted into its direct parent changes parent HEAD;
- child delivery commit is present in parent history and delivered file content is visible on parent branch;
- a repeated acceptance does not create another merge or change parent HEAD;
- rejection/rework pre-accept paths leave parent HEAD and parent file content unchanged.

### Sandbox through subagent construction

Add a focused runtime/tool integration test that builds the child worker tool registry through the same `DaemonWorkerFactory` / sandbox injection used by production. It asserts:

- workspace-write child can write inside its assigned child workspace;
- `..` escape and absolute path outside the assigned workspace are denied;
- read-only child does not expose mutating filesystem tools or receives the documented denial;
- parent worktree remains unchanged by the child registry operations.

The test invokes actual registered tools rather than relying on model tool selection.

## CLI, TUI, and slash command coverage

Complete the currently unchecked project-management feature with deterministic tests that use a local daemon and mock factory.

- CLI parser and command path: `agents`; `agent inspect`, `events`, `mailbox`, `diff`, `cancel`, `retry`, `pause`, `resume`, and review actions; `daemon status` and `stop`.
- Commands requiring preview tokens reject a missing token and succeed with a fresh token. Reuse rejects a consumed token.
- CLI-to-daemon integration checks rendered task detail/summary/event/diff output and redacts secrets.
- TUI `/agents`, `/events`, `/mailbox`, `/diff`, `/cancel`, `/accept`, `/rework`, and `/reject` route through daemon IPC, display deterministic validation errors for missing IDs/tokens/reasons, and preserve the current session scope.
- Slash help and completion enumerate every item in the control command catalog.

Use the existing `TuiApp` event/render pattern; do not add screenshot or terminal-timing tests.

## Headless opt-in subagent runtime

`yi-agent run` gains an explicit `--subagents` flag. Without the flag, headless runs retain the current builtin-only tool registry and do not start, connect to, attach to, or stop a daemon. With the flag, headless execution uses the same local-runtime lifecycle as the TUI without requiring terminal interaction:

1. resolve the current `YI_AGENT_RUNTIME_DIR` (or normal runtime directory), start an embedded daemon with the production worker factory when none is running, or attach to the existing local daemon;
2. attach an application root with a unique idempotency key;
3. rebuild the root registry with ordinary headless tools plus `spawn_agent`, `wait_agent`, and `send_message` bound to that root's daemon capability and socket;
4. activate the root with the prompt as objective before `Agent::run`;
5. detach the root after stream drain; stop only the daemon embedded by this invocation, never an already-running daemon.

The CLI flag is an opt-in capability boundary, not a shortcut around review controls. Root and child agents receive no accept, reject, or rework tool. Those decisions remain local-user daemon IPC operations requiring `PreviewReview` then a single-use `ConfirmReview` token.

Deterministic CLI tests prove that the default headless registry lacks subagent tools, `--subagents` attaches a root and registers exactly the three delegation tools, and failure to attach returns a clear error without silently running a non-delegating agent. The real ignored subagent tests use `--subagents` and set a unique `YI_AGENT_RUNTIME_DIR` below their own `TempDir`.

## Ignored real-LLM subagent E2E

Add a focused `#[ignore]` suite, using `RealLlmTestConfig`, a local daemon IPC client, and `TempDir` isolated Git repositories. Tests use a 300-second deadline and structural assertions only. The real provider controls only the root/child agents' `spawn_agent`, `wait_agent`, and coding behavior. The test harness acts as the authorized human reviewer: it reads the child delivery from the temporary daemon and uses the existing two-step `PreviewReview` then `ConfirmReview` IPC flow. The harness never grants review controls to an LLM tool and never bypasses confirmation tokens.

1. A parent delegates README inspection; terminal child report is non-empty and no repository file changes.
2. A real child creates a uniquely marked file and committed delivery in its assigned worktree. The harness accepts that delivery through preview/confirmation; the file is visible on the parent branch and the accepted child commit is an ancestor of parent `HEAD`.
3. A real child creates a delivery containing a known incorrect marker. The harness requests rework through preview/confirmation, waits for the real successor attempt, then accepts the successor through preview/confirmation. Final parent content contains the correct marker and excludes the incorrect marker.
4. A real parent delegates two independent uniquely marked files concurrently. The harness confirms acceptance of both child deliveries. Both files and their distinct markers are visible on the parent branch without one overwriting the other; each delivery has distinct child worktree/branch and ancestry evidence.

The suite contains four independently named ignored tests, including the existing README smoke test. `just test-real-subagent` runs them serially with `--test-threads=1`; each creates an independent temporary Git repository, daemon socket, SQLite runtime state, and child worktrees, all removed with its `TempDir`. Real tests may accept either configured provider. They must not assert exact prose, print secret values, or run in normal `cargo test`/CI execution.

## Recipes, documentation, and verification

`yi-agent-rs/justfile` gains a `test-real-subagent` recipe and updates real-test recipes to preserve/read the dedicated `YI_AGENT_REAL_LLM_*` environment variables. It must retain skip behavior only for unconfigured fallback mode and propagate explicit-configuration errors.

Update `docs/project-management/subagent-runtime.md` with concrete commands and code locations. Mark CLI/TUI/slash coverage complete only after all related deterministic tests pass. Add the real-LLM subagent E2E feature with the ignored recipe as its criterion. Update `docs/project-management/README.md` so the module count matches literal completed/abandoned feature rows.

Run these commands serially before completion:

```bash
cargo test -p yi-agent-core
cargo test -p yi-agent-tools
cargo test -p yi-agent-store
cargo test -p yi-agent
cargo fmt --all -- --check
cargo clippy -p yi-agent-core -p yi-agent-tools -p yi-agent-store -p yi-agent -- -D warnings
git diff --check main...HEAD
```

Run the ignored real test recipe separately. With no dedicated provider and no fallback key it skips successfully; with dedicated provider configuration missing any required field it fails while naming all missing variables.
