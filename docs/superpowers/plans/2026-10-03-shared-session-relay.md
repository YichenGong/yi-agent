# Shared-session relay (stdio + relay in one app-server) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the desktop GUI's `app-server` sidecar serve its stdio client *and* a relay-attached phone from **one** `serve()` loop, so both see the same live sessions.

**Architecture:** `serve_scoped` (stdio) and `serve_ws_inner` (ws) already both funnel into the same multi-client `server::serve`. We split the ws path into a reusable "attach a ws frontend to a given hub/inbound-channel/scopes" helper, then add a merged entry point that wires stdio (`local`/Admin) + a loopback ws frontend (phone/Control) + the relay client into a single `serve()`.

**Tech Stack:** Rust (tokio, axum/WebSocketUpgrade), clap CLI, Tauri sidecar spawn.

## Global Constraints

- **stdio zero-regression:** with no `--relay`, the stdio path stays byte-for-byte identical (existing 182+ app-server tests green is the gate).
- **Scopes unchanged:** stdio client = `Scope::Admin`; phone client = `Control`.
- **No new inbound port:** the local ws frontend binds `127.0.0.1:0`; the relay client authenticates with a `seed_local_device("relay-bridge")` token.
- **Single source of truth:** exactly one `serve()` loop holds `threads`/`runtimes`/`thread_roots`.
- Work only in `.worktrees/shared-session-relay` (branch `feat/shared-session-relay`); never edit `main` directly.

---

### Task 1: Extract `attach_ws_frontend` (zero behavior change)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs` (`serve_ws_inner`, add `attach_ws_frontend`)
- Test: existing `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs` test module

**Interfaces:**
- Produces:
  ```rust
  pub(crate) fn attach_ws_frontend(
      listener: tokio::net::TcpListener,
      hub: Arc<Broadcaster>,
      inbound_tx: tokio::sync::mpsc::Sender<(crate::broadcast::ClientId, anyhow::Result<String>)>,
      client_scopes: ClientScopes,
      client_initialized: ClientInitialized,
      device_clients: WsDeviceRegistry,
      pairing: Arc<PairingState>,
  ) -> tokio::task::JoinHandle<anyhow::Result<()>>;
  ```
  `serve_ws_inner` keeps its public behavior: it builds these five values, calls
  `attach_ws_frontend`, then drives `serve(...)` with the same `inbound_rx`.

- [ ] **Step 1: Read the current ws setup** in `serve_ws_inner` (ws.rs:63-170) and copy the exact block that builds `hub`, `inbound_tx/rx`, `client_scopes`, `client_initialized`, `device_clients`, and the axum `Router`/route closure.

- [ ] **Step 2: Move that block into `attach_ws_frontend`** (returning the spawned router task's `JoinHandle`), leaving `serve_ws_inner` as:
  ```rust
  let hub = Arc::new(Broadcaster::new());
  let (inbound_tx, inbound_rx) = mpsc::channel(64);
  let client_scopes = /* existing init */;
  let client_initialized = /* existing init */;
  let device_clients = install_device_registry(/* existing init */);
  let frontend = attach_ws_frontend(listener, Arc::clone(&hub), inbound_tx,
      Arc::clone(&client_scopes), Arc::clone(&client_initialized),
      Arc::clone(&device_clients), Arc::clone(&pairing));
  serve(inbound_rx, hub, cfg, PERMISSION_TIMEOUT, workspaces, pairing,
        RuntimeAttachments { /* unchanged */ }, build_agent, client_scopes, client_initialized).await
  ```
  (keep the existing `tokio::spawn(serve_task)` structure if simpler; the invariant is: same five values, one `serve()`).

- [ ] **Step 3: Run the ws suite to verify no behavior change**

Run: `cargo test -p yi-agent-app-server --lib ws`
Expected: PASS (all pre-existing ws tests).

- [ ] **Step 4: Run the whole app-server lib suite (regression gate)**

Run: `cargo test -p yi-agent-app-server`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/ws.rs
git commit -m "refactor(app-server): extract attach_ws_frontend from serve_ws_inner"
```

---

### Task 2: `serve_stdio_with_relay` + fan-out test

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` (add `serve_stdio_with_relay`)
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` (re-export if needed)
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` test module

**Interfaces:**
- Consumes: `attach_ws_frontend` (Task 1); `yi_agent_relay::run_client(relay_url, app_server_ws, session, token)`; `PairingState::seed_local_device(label)`.
- Produces:
  ```rust
  pub(crate) async fn serve_stdio_with_relay<R, W, F>(
      reader: R, writer: W,
      cfg: RuntimeConfig,
      permission_timeout: Duration,
      workspaces: Arc<WorkspaceIndex>,
      pairing: Arc<PairingState>,
      attachments: RuntimeAttachments,
      build_agent: F,
      relay_url: String,
  ) -> anyhow::Result<()>
  where R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
        F: Fn(Option<yi_agent_core::Session>, &Path, crate::thread_store::ThreadMode)
              -> anyhow::Result<crate::server::BuiltAgent> + Send + Sync + 'static;
  ```

- [ ] **Step 1: Write the failing fan-out test.** Seed a stdio client over a duplex pipe and a ws client over the loopback frontend with a `seed_local_device` token; assert a notification from a stdio `thread/start` reaches the ws client.

```rust
#[tokio::test]
async fn merged_loop_fans_out_stdio_notifications_to_ws_client() {
    // 1. duplex() for stdio; spawn serve_stdio_with_relay(reader, writer, cfg, ..., relay_url);
    //    BUT for this unit test drive the ws frontend directly instead of a real relay
    //    (see Step 3 note) — assert both clients share one threads table.
    // 2. stdio: initialize + thread/start -> capture thread_id.
    // 3. ws: initialize + thread/listAll -> assert thread_id present.
    // 4. assert the ws client received a `thread/started`/status notification emitted by (2).
}
```

- [ ] **Step 2: Run it to verify it fails** (function/entry absent).

Run: `cargo test -p yi-agent-app-server merged_loop_fans_out -- --nocapture`
Expected: FAIL (unresolved `serve_stdio_with_relay`).

- [ ] **Step 3: Implement `serve_stdio_with_relay`.** Reuse the `serve_scoped` prologue for the stdio half, add:
  ```rust
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
  let addr = listener.local_addr()?;
  let token = pairing.seed_local_device("relay-bridge");
  let frontend = attach_ws_frontend(listener, Arc::clone(&hub), inbound_tx.clone(),
      Arc::clone(&client_scopes), Arc::clone(&client_initialized),
      Arc::clone(&device_clients), Arc::clone(&pairing));
  let (relay_base, session) = parse_relay_url(&relay_url)?;      // reuse relay_parts logic
  let app_server_ws = Url::parse(&format!("ws://{addr}/ws"))?;
  let relay_task = tokio::spawn(yi_agent_relay::run_client(relay_base, app_server_ws, session, token));
  // then the SAME serve(inbound_rx, hub, ... client_scopes{local→Admin}, client_initialized) as serve_scoped,
  // and after it returns: hub.unregister(&local); pump.await; frontend.abort(); relay_task.abort();
  ```
  For the **unit test**, add a `#[cfg(test)]`-visible variant that takes an already-built
  loopback `TcpListener` (or expose `attach_ws_frontend` + the loop wiring) so the test can
  connect a ws client without a real relay. Do **not** add a real relay dependency to unit tests.

- [ ] **Step 4: Run the test to verify it passes.**

Run: `cargo test -p yi-agent-app-server merged_loop_fans_out -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Add the scope test** (phone = Control is still gated):

```rust
#[tokio::test]
async fn merged_loop_control_client_is_denied_admin_rpc() {
    // ws client with Control scope calls `pair/create` -> expect -32014.
}
```

Run: `cargo test -p yi-agent-app-server merged_loop_control_client -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Regression + commit**

Run: `cargo test -p yi-agent-app-server`
Expected: PASS.

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/{server.rs,lib.rs}
git commit -m "feat(app-server): serve stdio and a relay ws frontend from one serve()"
```

---

### Task 3: CLI — `--relay` composes with stdio

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (`run_app_server` dispatch)
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs` (`AppServer` arg docs)
- Test: `yi-agent-rs/crates/yi-agent/src/config.rs` tests, `main.rs` tests

**Interfaces:**
- Consumes: `serve_stdio_with_relay` (Task 2).
- Produces: dispatch rules —
  - `--relay <url>` and `--listen` in {absent, `stdio://`} → `serve_stdio_with_relay`.
  - `--listen relay://<url>` → existing `run_relay_mode` (unchanged).

- [ ] **Step 1: Write the failing CLI tests.**

```rust
#[test]
fn app_server_relay_without_listen_is_merged() {
    let cli = Cli::parse_from(["yi-agent", "app-server", "--relay", "wss://r/connect?session=x"]);
    assert!(matches!(app_server_mode(&cli), AppServerMode::StdioWithRelay(_)));
}
#[test]
fn app_server_listen_relay_is_pure_relay() {
    let cli = Cli::parse_from(["yi-agent", "app-server", "--listen", "relay://wss://r/connect?session=x"]);
    assert!(matches!(app_server_mode(&cli), AppServerMode::PureRelay(_)));
}
```

- [ ] **Step 2: Run to verify failure.**

Run: `cargo test -p yi-agent app_server_relay_without_listen_is_merged`
Expected: FAIL (`app_server_mode` undefined).

- [ ] **Step 3: Implement `app_server_mode`** (pure fn: `&Cli -> AppServerMode{ Stdio, Ws(SocketAddr), PureRelay(String), StdioWithRelay(String) }`) and route `run_app_server` through it.

- [ ] **Step 4: Run the CLI tests.**

Run: `cargo test -p yi-agent app_server_`
Expected: PASS.

- [ ] **Step 5: Update `--relay` help text** in `config.rs` to say it composes with stdio and that `--listen relay://` is the pure-relay form.

- [ ] **Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/{main.rs,config.rs}
git commit -m "feat(cli): app-server --relay composes with stdio; --listen relay:// stays pure relay"
```

---

### Task 4: Desktop sidecar spawns with `--relay` when configured

**Files:**
- Modify: `desktop/src-tauri/src/bridge.rs` (`spawn_once`)
- Test: `desktop/src-tauri/src/bridge.rs` test module

**Interfaces:**
- Produces: `fn sidecar_args(relay: Option<&str>) -> Vec<String>` returning `["app-server", "--listen", "stdio://"]` plus `["--relay", url]` when `Some`.

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn sidecar_args_add_relay_only_when_configured() {
    assert_eq!(sidecar_args(None), ["app-server", "--listen", "stdio://"]);
    assert_eq!(sidecar_args(Some("wss://r/connect?session=x")),
               ["app-server", "--listen", "stdio://", "--relay", "wss://r/connect?session=x"]);
}
```

- [ ] **Step 2: Run to verify failure.**

Run: `cargo test --manifest-path desktop/src-tauri/Cargo.toml sidecar_args`
Expected: FAIL.

- [ ] **Step 3: Implement `sidecar_args`** and use it in `spawn_once`; read the relay URL from the desktop's stored remote/relay setting (reuse `settings_store`; `None` → today's behavior).

- [ ] **Step 4: Run the test + build.**

Run: `cargo test --manifest-path desktop/src-tauri/Cargo.toml`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add desktop/src-tauri/src/bridge.rs
git commit -m "feat(desktop): spawn the sidecar with --relay when a relay is configured"
```

---

### Task 5: End-to-end with a real relay + docs

**Files:**
- Modify: `docs/relay-deploy.md`（新增「桌面 GUI 合一模式」小节 + `--relay` 语义变更说明）
- Test: manual E2E script under `docs/`

- [ ] **Step 1: Manual E2E.** Start a relay; start `yi-agent app-server --listen stdio:// --relay <relay-connect-url>`; pair the phone; assert (a) phone lists desktop threads, (b) a desktop turn's events appear live on the phone, (c) a phone-initiated turn appears on the desktop.

- [ ] **Step 2: Update `relay-deploy.md`** with the merged topology diagram and the `--relay` vs `--listen relay://` distinction.

- [ ] **Step 3: Commit**

```bash
git add docs/relay-deploy.md
git commit -m "docs: desktop GUI + relay share one app-server session"
```

---

## Self-Review

- **Spec coverage:** §4.1→Task 1, §4.2→Task 2, §4.3→Task 3, §4.4→Task 4, §5 E2E→Task 5, §6 doc→Task 5.
- **Placeholder scan:** unit tests intentionally avoid a real relay by testing the loopback frontend directly; the real relay is only exercised in Task 5 (manual E2E).
- **Type consistency:** `attach_ws_frontend` signature is used verbatim in Tasks 1-2; `AppServerMode` variants match Task 3's dispatch.
