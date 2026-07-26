# Clippy Test Module Order Design

## Goal

Make the strict Clippy check pass without changing E2E helper behavior.

## Approach

Move the existing `#[cfg(test)] mod tests` block in `yi-agent-rs/crates/yi-agent/tests/common/mod.rs` to the end of the file, after `wait_for_child`, `run_command_with_timeout`, and `run_agent_with_timeout`.

## Constraints

- Do not change helper signatures or timeout behavior.
- Do not add a Clippy allowance for `items_after_test_module`.
- Preserve the existing timeout unit test unchanged.

## Verification

- `cargo fmt --all -- --check`
- `cargo test -p yi-agent --test e2e_complex --no-run`
- `cargo clippy -p yi-agent --all-targets --all-features -- -D warnings`
