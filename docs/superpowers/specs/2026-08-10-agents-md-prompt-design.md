# Project AGENTS.md Prompt Loading Design

## Goal

When the selected project root contains an `AGENTS.md`, yi-agent loads its
contents into the system prompt by default. This applies to both the
interactive TUI and `yi-agent run`, including when a custom system prompt is
provided through `--system-prompt` or `YI_AGENT_SYSTEM_PROMPT`.

## Scope

- Treat `Config::workdir` as the project root.
- Read only `<workdir>/AGENTS.md`; do not search parent directories or
  recursively discover instruction files.
- Keep `--naked` entirely bare: it does not read or inject `AGENTS.md`.
- If the file is absent, preserve current prompt behavior.
- If the file cannot be read, emit a warning and continue without project
  instructions.

## Prompt Composition

The effective system prompt is assembled in this order:

1. Built-in yi-agent instructions.
2. User-provided system prompt, when present.
3. Project instructions under the explicit heading
   `Project instructions (AGENTS.md):`.
4. Current local date.
5. Skills catalog, when skills are available.

This preserves the existing built-in and explicit user instructions while
making project-level constraints available in every normal agent invocation.
The source heading avoids presenting repository instructions as model-native
instructions.

## Architecture

Add a focused helper near the prompt-resolution code in
`crates/yi-agent/src/main.rs`. It accepts the resolved base prompt and the
configured workdir, reads `AGENTS.md` with `std::fs::read_to_string`, and
returns the original prompt unchanged when the file is absent or unreadable.
The helper logs a warning for unexpected I/O failures.

Both existing normal-mode call sites pass `config.workdir` into the prompt
resolver:

- `run_agent` for TUI sessions.
- `build_headless_setup` for `yi-agent run`.

No new configuration option or tool is required. The `naked` early-return in
`build_headless_setup` remains before prompt resolution, so it continues to
have no system prompt at all.

## Testing

Unit tests in `crates/yi-agent/src/main.rs` use `tempfile::TempDir` to prove:

- A missing `AGENTS.md` does not alter the resolved prompt.
- A root-level `AGENTS.md` is appended with the dedicated heading.
- A custom prompt still includes both its own content and `AGENTS.md`.
- Headless naked mode continues to expose `None` as its system prompt.

Run the focused crate tests with:

```sh
cargo test -p yi-agent
```

Then run formatting with:

```sh
cargo fmt --all
just fmt-check
```

## Project Tracking

Update `docs/project-management/yi-agent-tui.md` with an `[x]` entry linked to
the resolver and test command, and increment the corresponding completed count
in `docs/project-management/README.md`.
