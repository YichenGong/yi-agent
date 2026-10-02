# Desktop settings/theme — Rust follow-ups wave

Three deferred Rust findings from the desktop settings/theme feature. Branch
`fix/settings-followups-rs` (based on main); commits below. TDD was applied per
finding: a failing test was written and RUN first, its RED output captured, then
the minimal fix, then GREEN.

Toolchain: `rustc 1.98.1 (48a229cea 2026-09-01)`.

## Commits

| Commit | Finding |
| --- | --- |
| `c592d05` fix(app-server): keep the theme watcher alive across a Lagged broadcast | A (Important) |
| `f7b3be9` fix(app-server): warn when the theme preference is present but unusable | B (Minor) |
| `8671e81` fix(app-server): give each preferences write a unique temp file | C (Minor) |

## Finding A (Important) — theme watcher died on a lagged broadcast

### Change
`yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

- New `async fn pump_theme_notifications(rx, hub)` at **server.rs:1229** holds the
  watcher loop. `Lagged(missed)` logs a warning and `continue`s; only `Closed`
  returns (server.rs:1236-1241). The loop body (build `UiSettingsUpdated`,
  `write_notification` over the hub) is unchanged.
- `serve` now spawns it: **server.rs:1333**
  `tokio::spawn(pump_theme_notifications(theme.subscribe(), Arc::clone(&hub)))`,
  replacing the inlined `while let Ok(...)` block. The bus behaviour is
  unchanged: `write_notification` still `hub.broadcast`s, so stdio `local` and
  every ws client receive the frame.
- Tests: `mod theme_watcher_tests` at **server.rs:9568** —
  `theme_watcher_survives_a_lagged_broadcast` (9593) and
  `theme_watcher_ends_when_the_broadcast_closes` (9631).

Approach taken: **factored the loop body into a production function and drove
that function directly** (the task's suggested option). This is trustworthy
because it is the *same* function `serve` spawns — the test does not copy the
loop. Supporting facts:

- It uses the real `ThemeHandle::new` (hence the real `broadcast::channel(16)`),
  not a hand-configured channel.
- The harness deterministically produces a real `Lagged`: after spawn it
  `yield_now`s once so the watcher is subscribed, then `set()`s 216 times without
  draining, outrunning the consumer far past capacity 16.
- RED reproduced as a wall-clock `5.05s` failure, which only happens if the
  watcher had already exited on the first `Lagged` (`while let Ok`), leaving the
  subsequent `ui/settings/updated` unproduced. That is the real production
  failure mode, not a mock artifact.
- `theme_watcher_ends_when_the_broadcast_closes` is the paired guard proving the
  fix is "continue on Lagged", not "never exit".

### RED proof
`cargo test -p yi-agent-app-server --lib theme_watcher_tests` (with the real loop
body still `while let Ok`):

```
test server::theme_watcher_tests::theme_watcher_ends_when_the_broadcast_closes ... ok
test server::theme_watcher_tests::theme_watcher_survives_a_lagged_broadcast ... FAILED

---- server::theme_watcher_tests::theme_watcher_survives_a_lagged_broadcast stdout ----
thread '...' panicked at crates/yi-agent-app-server/src/server.rs:9608:9:
the theme watcher must stay alive across a Lagged broadcast and still push ui/settings/updated; `while let Ok` returns instead

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 272 filtered out; finished in 5.05s
```

### GREEN proof
(Pump now matches on `recv()`.)

```
test server::theme_watcher_tests::theme_watcher_ends_when_the_broadcast_closes ... ok
test server::theme_watcher_tests::theme_watcher_survives_a_lagged_broadcast ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 272 filtered out; finished in 0.05s
```

### Deviation
None of substance; takes the factoring option the task described and states why
it is trustworthy above.

## Finding B (Minor) — `load` swallowed read/parse errors

### Change
`yi-agent-rs/crates/yi-agent-app-server/src/settings_store.rs`

- `load` (settings_store.rs:47) still falls back to `Dark`, but now:
  - `ErrorKind::NotFound` → silent return `Dark` (normal first run);
  - other read error → `tracing::warn!(error, path, "could not read the theme
    preference; using the default")` (settings_store.rs:57);
  - parse error → `tracing::warn!(error, path, "invalid theme preference; using
    the default")` (settings_store.rs:72).
  The two wordings are distinct, mirroring
  `yi-agent/src/tui/runtime_prefs.rs`.
- Tests: `mod warning_tests` (settings_store.rs:222) —
  `a_missing_file_is_not_logged` (256),
  `an_unreadable_file_is_logged_and_falls_back_to_dark` (268),
  `a_corrupt_file_is_logged_and_falls_back_to_dark` (291).

### RED proof
Before the change `cargo test -p yi-agent-app-server --lib warning_tests`:

```
test settings_store::warning_tests::a_missing_file_is_not_logged ... ok
test settings_store::warning_tests::a_corrupt_file_is_logged_and_falls_back_to_dark ... FAILED
test settings_store::warning_tests::an_unreadable_file_is_logged_and_falls_back_to_dark ... FAILED

---- settings_store::warning_tests::a_corrupt_file_is_logged_and_falls_back_to_dark stdout ----
thread '...' panicked at crates/yi-agent-app-server/src/settings_store.rs:217:9:
must be a warning: 
```

(The corrupt/unreadable tests failed because the captured log was empty: the old
`Err(_) => Theme::Dark` logged nothing. The missing-file test passed, as it must,
before and after.)

### GREEN proof
`cargo test -p yi-agent-app-server --lib settings_store`:

```
test settings_store::tests::parsing_is_case_insensitive ... ok
test settings_store::tests::missing_file_defaults_to_dark ... ok
test settings_store::warning_tests::a_missing_file_is_not_logged ... ok
test settings_store::tests::save_then_load_round_trips ... ok
test settings_store::warning_tests::a_corrupt_file_is_logged_and_falls_back_to_dark ... ok
test settings_store::tests::saving_theme_preserves_unrelated_keys ... ok
test settings_store::warning_tests::an_unreadable_file_is_logged_and_falls_back_to_dark ... ok
test settings_store::tests::malformed_and_unknown_values_fall_back_to_dark ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 269 filtered out; finished in 0.01s
```

### Deviation (dependency)
The workspace's existing capture pattern is `tracing_subscriber::fmt()` with a
custom `Write` writer + `tracing::subscriber::set_default(...)`, used by
`yi-agent-store/src/ipc.rs` `error_logging_tests`. `yi-agent-app-server` had
`tracing-subscriber` only as a transitive dep, so capturing required a dev-dep.
Chosen: add `tracing-subscriber = { workspace = true }` to
`yi-agent-app-server/[dev-dependencies]`, reusing the workspace's shared
dependency (no new crate version). `Cargo.lock` gained one edge line only. This
is the lightest option that captures a per-thread warning without a global
subscriber and without touching the workspace manifest.

## Finding C (Minor) — concurrent writers shared one temp filename

### Change
`yi-agent-rs/crates/yi-agent-app-server/src/settings_store.rs`

- `save` now names its temp file via
  `temp_path_for(&dir, TMP_SEQ.fetch_add(1, Relaxed))` (settings_store.rs:100),
  giving `preferences.json.<pid>.<seq>.tmp`.
- `TMP_SEQ` (settings_store.rs:106) is a process-wide `AtomicU64`; `temp_path_for`
  (settings_store.rs:114) is the single naming helper `save` calls, matching
  `thread_store::write_atomic`.
- The final name `preferences.json` and the read-modify-write + rename discipline
  are unchanged.
- Tests: `saving_leaves_no_temp_file_behind` (settings_store.rs:176) and
  `each_write_uses_a_unique_temp_name` (193).

Approach: uniqueness is asserted on the real `temp_path_for` that `save` calls
(the task allows this; it is production code, not a copy of the rule). `save`
itself cannot be made to emit two different names at a deterministic point
synchronously — the per-write counter has no observable break — so per-write
uniqueness is pinned at the naming function, and the end-to-end discipline is
pinned by the two-saves test.

### RED proof
With `temp_path_for` still returning the fixed name,
`cargo test -p yi-agent-app-server --lib settings_store`:

```
test settings_store::tests::each_write_uses_a_unique_temp_name ... FAILED
...
thread '...' panicked at crates/yi-agent-app-server/src/settings_store.rs:186:9:
assertion `left != right` failed: the temp name must differ per write or concurrent writers clobber each other
  left: "/var/folders/6j/.../.tmpFjsW4x/preferences.json.tmp"
 right: "/var/folders/6j/.../.tmpFjsW4x/preferences.json.tmp"

test result: FAILED. 6 passed; 1 failed; 0 ignored; 0 measured; 272 filtered out; finished in 0.01s
```

(`saving_leaves_no_temp_file_behind` passed before and after — it extends the
existing "no `.tmp` left" guarantee and pins that the rename still lands.)

### GREEN proof
`cargo test -p yi-agent-app-server --lib settings_store`:

```
test settings_store::tests::each_write_uses_a_unique_temp_name ... ok
test settings_store::tests::saving_leaves_no_temp_file_behind ... ok
...
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 269 filtered out; finished in 0.01s
```

## Out-of-scope findings (not edited)

- **Same fixed `.tmp` still used by the other two writers.** Per the task, these
  are outside the file scope, so left as-is:
  - `yi-agent-rs/crates/yi-agent/src/tui/runtime_prefs.rs` (`save`, the
    `preferences.json.tmp` write).
  - `yi-agent-rs/crates/yi-agent-boards/src/scaffold.rs:109`
    (`dir.join("preferences.json.tmp")`).
  `yi-agent-supervisors/src/switch.rs` does not itself write the file (it routes
  through the scaffold/runtime-prefs writers), so it carries no temp name of its
  own. Until the two writers above adopt a unique temp name, the cross-process
  clobber risk between them and `settings_store::save` is only reduced (our writer
  no longer collides with a fixed name), not eliminated. A follow-up should share
  a naming helper.
- `yi-agent-rs/crates/yi-agent-boards/src/registry.rs:104` uses the fixed
  `boards.json.tmp`, but for a different file (`boards.json`), so it is not part
  of Finding C.

## Verification summary

- RED→GREEN per finding as quoted above.
- `cargo test -p yi-agent-app-server`: `test result: ok. 279 passed; 0 failed;
  0 ignored; 0 measured; 0 filtered out`.
- `cargo test --workspace`: EXIT=0; 64 suites `ok`; 2120 passed, 0 failed.
- `cargo clippy -p yi-agent-app-server --all-targets`: the only diagnostics are
  the **5 pre-existing warnings also present on unmodified main** (4 ×
  `redundant closure` for `build_test_agent` in server.rs's test module, 1 ×
  `unnecessary_sort_by` at `thread_store.rs:1053`). My touched code adds none;
  `-- -D warnings` is already red on main for these. `thread_store.rs` was not
  edited.

## Changes by file

- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` — A
- `yi-agent-rs/crates/yi-agent-app-server/src/settings_store.rs` — B, C
- `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`, `yi-agent-rs/Cargo.lock` —
  B's test-only dev-dependency

No edits to `runtime_prefs.rs`, `switch.rs`, or any unrelated module. No merge,
main untouched.
