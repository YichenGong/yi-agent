# Desktop history-first / snapshot regression verification

Date: 2026-11-11. Scope: desktop restart and historical-thread selection. Formal installed App has NOT been replaced by this work.

## Root cause

`thread/resume` previously built/attached the agent and managed runtime before replaying history. Frontend awaited the final activation reply to complete transcript synchronization and startup handshake. Desktop also explicitly disabled both the list and transcript caches that mobile already used. Thus tool/runtime readiness was unnecessarily a prerequisite for reading old content.

## Contracts

- Desktop local opt-in `historyFirst` + nonempty `historyToken`. History chunks, usage and recovered partial precede a correlated `thread/historyReady`; actual configuration / activation success still comes from the real final response.
- Keep strict load/model checks and disk partial promotion after successful activation and writer lock acquisition. Never invent runtime readiness.
- Cached local list and content have separate storage from paired remote: `yi-agent.desktop.threadListCache`, `yi-agent-desktop-cache`. Existing normalization, 10-entry / 5 MiB LRU, 1 MiB target per entry and 800 ms debounce remain.
- Cache is a rendering hint, never a permission or activation hint. Server batches update immediately; completion removes ghosts. Late disk reads cannot resurrect an authoritative empty history. Sending waits for activation and is canceled by navigation/draft changes.
- Running threads remain read-only (`thread/readItems`), never interrupted by opening history. Trimmed snapshots request their missing head instead of only a cursor tail.

## Automated RED/GREEN evidence

All paths below are local verification artifacts, not portable CI dependencies.

| Regression | RED | GREEN |
| --- | --- | --- |
| Factory blocked: transcript and ready before activation | `/private/tmp/yi-history-backend-red.log` (1 fail / 2 pass) | `/private/tmp/yi-history-backend-green2.log` (22 pass) |
| Desktop immediate snapshot, incremental refresh, persistence across remount | `/private/tmp/yi-history-cache-red.log` (3 fail) | `/private/tmp/yi-history-cache-green.log` (3 pass) |
| Running trimmed cache must fetch head; no sidebar write per content frame | `/private/tmp/yi-history-cache-edge-red.log` (2 fail) | `/private/tmp/yi-history-cache-edge-green.log` (5 desktop snapshot tests pass) |
| Late cache after authoritative empty history | `/private/tmp/yi-history-cache-late-red.log` (1 fail) | `/private/tmp/yi-history-cache-late-green.log` (6 desktop snapshot tests pass) |
| Bounded activation wait / competing read cannot reset replay | `/private/tmp/yi-history-review-red.log` (2 fail) | Final full frontend suite below |
| Running live tail before cache metadata must still fetch full head | `/private/tmp/yi-history-running-late-red.log` (1 fail) | Final full frontend suite below |
| Linear trim, exact UTF-8 budget | `/private/tmp/yi-history-trim-red.log` (45149 serialized items for 300 input items) | `/private/tmp/yi-history-list-red-trim-green.log` (23 trim/content tests pass) and full suite |
| Replaced-client list must not be rendered or cached | `/private/tmp/yi-history-list-red-trim-green.log` (1 list fail) | Final full frontend suite below |

Reproduce frontend contracts:

```sh
cd desktop
npm test -- src/App.test.tsx -t 'App desktop history snapshot|App 同步状态机'
npm test -- src/lib/session.test.ts
npm test
npm run build
```

Rust verification uses `cargo fmt --all` and `just ci` in `yi-agent-rs/`, with ALL inherited `YI_AGENT_*` variables removed from isolated test/build processes. Do not rely on an earlier CI result after changing code.

## Isolated native managed-runtime evidence

Probe App: `/private/tmp/yi-desktop-history-native/yi-agent-history-probe.app`. Different bundle ID and `YI_AGENT_HOME`, synthetic history, no relay, no real LLM calls. Only the probe desktop PID is stopped between launches; formal desktop and guardian are untouched.

The native document-start probe delays the real `runtime_acquire_project` fetch by 5000 ms, without forging its result. Direct monkeypatching `window.__TAURI_INTERNALS__.invoke` is invalid because Tauri installs a nonwritable/nonconfigurable API; the effective probe intercepts the custom-protocol fetch.

Long-history stage (before adding desktop disk snapshots): 34.91 MiB synthetic history / 3251 items / 142 batches; all 3251 delivered, historyReady at 550 ms, readable at 742 ms, actual resume response at 5189 ms, PASS/noTimeout. Source: `/private/tmp/yi-desktop-history-native/probe-long.jsonl`. This proves history-before-activation, NOT restart cache first paint.

The current candidate was launched twice against real persistent WebKit storage. First launch (`cache-warm.jsonl`) verified two actual `threadContent` items, the desktop-only list key, and no remote list key. Final restart (`cache-restart-ship.jsonl`) intercepted startup RPC at 102/105 ms, showed cached content at **133 ms**, released the common startup gate at **5006 ms**, and completed reconciliation at **5044 ms** with two items and no timeout. The gate delays the real transport; it neither forges replies nor inserts fixture content into the DOM/cache. Earlier per-RPC delays compounded across serial handshake calls and were rejected as an invalid end-to-end probe; the final shared gate releases all startup RPCs together after five seconds. The harness detects any terminal PASS/FAIL event, not the last log line (a later lease log is not a failure).

Final long-history native run (`probe-ship-long.jsonl`): **34.3 MiB / 3251 items / 142 batches**, all items delivered; historyReady **384 ms**, readable tail **574 ms**, actual activation response **5157 ms**. Actual IndexedDB snapshot: **95 items / 1040275 bytes / trimmed=true**; PASS at 6992 ms. The probe checks the visible tail, not the first message (the latter may be outside the rendered viewport). It reports, rather than requires, beforeActivation because an already warm isolated runtime can activate immediately. This final run did observe readable history before activation; no guardian was stopped to manufacture a cold runtime.

Final verification of the source tree submitted with this record: frontend **194 files / 2879 tests**, `npm run build`, `cargo fmt --all`, full `just ci` (**6415 tests passed / 41 skipped**, additional **50 bindings tests**), release sidecar and native app build. Logs: `/private/tmp/yi-history-ui-ship.log`, `/private/tmp/yi-history-ui-ship-build.log`, `/private/tmp/yi-history-ci-ship-exclusive.log`, `/private/tmp/yi-history-native-build-ship.log`. Bundle finalized with `desktop/scripts/finalize-runtime-bundle.sh` (signature and final sidecar digest verified). The bundle is a local candidate at `desktop/src-tauri/target/release/bundle/macos/yi-agent.app`; `/Applications/yi-agent.app` was not replaced.

An earlier CI run concurrent with native release compilation (`yi-history-ci-ship.log`) had 6414 passes / one failure: unchanged TUI managed-runtime bring-up exceeded its 30 s budget. This is not counted as green. The subsequent **entire** CI ran without competing compilation and passed, including that test; no tests were removed and no budgets relaxed.

Additional regression evidence: handshake-before-send RED `/private/tmp/yi-history-cache-send-red.log`, GREEN snapshot suite; asynchronous cache metadata / live-tail RED `/private/tmp/yi-history-running-late-red.log`, GREEN full frontend suite. `usePluginDemand` unmount regression RED `/private/tmp/yi-plugin-unmount-red.log`; both queued retry and post-unmount async scope resolution are covered by its nine passing tests. This fixes a real teardown timer leak exposed by repeated full-suite runs, rather than suppressing unhandled errors.
