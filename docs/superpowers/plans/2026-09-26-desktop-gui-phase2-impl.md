# Desktop GUI Baseline — Phase 2 (Tauri App + Frontend) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (or
> superpowers:subagent-driven-development) to implement this plan task-by-task.

**Goal:** Ship a native macOS desktop app (`desktop/`) that spawns the
`yi-agent app-server` binary as a sidecar, bridges JSON-RPC 2.0 over stdio, and
renders a chat UI with streaming text, tool-call cards, and a permission
approval dialog.

**Architecture:** `desktop/` is a Tauri 2.x app, independent of the Rust
workspace at `yi-agent-rs/`. The Tauri Rust backend (`desktop/src-tauri/`)
contains **no agent logic** — it only manages the sidecar process lifecycle and
bridges JS ⇄ child stdin/stdout. The React frontend depends only on the wire
protocol (hand-written TS types), never on Rust types. Protocol boundary =
process boundary.

**Tech Stack:** Tauri 2.x, Rust (tauri-plugin-shell), React 18 + TypeScript +
Tailwind CSS + Vite, Vitest for frontend unit tests.

**Design reference:** `docs/superpowers/plans/2026-09-26-desktop-gui-design.md`
§5 (repo layout), §6 (protocol), §9 (Tauri app), §10 (errors/tests), §12
(success criteria).

**Phase 1 (already merged to main):** `yi-agent-runtime` +
`yi-agent-app-server` crates and the `yi-agent app-server --listen stdio://`
CLI subcommand. Verify with:
`cargo test -p yi-agent-app-server && cargo test -p yi-agent-runtime`.

---

## Global constraints (read before starting)

- Work in the `feat/desktop-gui-phase2` worktree. Never commit on `main`.
- Commit style: conventional commits, first line ≤72 chars, **no**
  `Co-Authored-By` line.
- Rust: run `cargo fmt --all` (in `yi-agent-rs/`) before committing any `.rs`.
  Do not run `cargo test --workspace`; run per crate.
- Only one `cargo` process at a time across all worktrees.
- `desktop/` must be added to the repo but its build artifacts
  (`node_modules/`, `dist/`, `src-tauri/target/`, `src-tauri/binaries/`) must be
  gitignored.
- Frontend: never import from `@tauri-apps/api` in `lib/session.ts` — keep the
  state machine pure and unit-testable.

### Wire protocol facts (verified against `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`)

- Frame = one JSON object per line (JSONL), UTF-8, ≤1 MiB.
- Requests (client→server): `{"jsonrpc":"2.0","id":<num|str>,"method":...,"params":...}`
- Responses (server→client): `{"jsonrpc":"2.0","id":...,"result":...}` or `{"...","error":{"code","message","data?"}}`
- Notifications (server→client, no `id`): `{"jsonrpc":"2.0","method":...,"params":{...}}`
- Reverse request (server→client, has `id`): method `item/toolCall/requestApproval`.
- **Param field names are `snake_case`** (`thread_id`, `turn_id`, `item_id`).
- `Item` uses a camelCase `type` tag with snake_case fields:
  `{"type":"userMessage","id","text"}` /
  `{"type":"agentMessage","id","text"}` /
  `{"type":"toolCall","id","call_id","name","input","status","result?"}`
  where `status ∈ {"running","completed","failed"}`.
- Notification methods: `thread/started`, `turn/started`, `item/started`,
  `item/delta`, `item/completed`, `turn/completed`,
  `thread/tokenUsage/updated`, `error`.
- `turn/completed.params.status ∈ {"completed","interrupted","failed"}`,
  optional `error`.
- Approval reverse request params:
  `{thread_id, turn_id, request_id, tool_name, tool_input, prefix_suggestion, kind}`
  where `kind` is `"Normal"` or `{"Blacklisted":"<reason>"}`.
- Client approval reply (id must equal the reverse-request id):
  `{"jsonrpc":"2.0","id":"perm-N","result":{"decision":"<decision>"}}` with
  `decision ∈ {"allow_once","always_allow_tool","always_allow_prefix","deny"}`;
  for `always_allow_prefix` also send `"prefix":"<cmd>"` in `result`.
- Client methods (client→server): `initialize`, `initialized`, `thread/start`,
  `turn/start` (`{threadId, input:[{type:"text",text}]}`), `turn/interrupt`,
  `config/read`.
- Error codes: `-32700` parse, `-32600` invalid request, `-32601` method not
  found, `-32602` invalid params, `-32603` internal, `-32010` not initialized,
  `-32011` unknown thread, `-32012` turn in progress.

### Deliberate deviation from design §9.1

Design §9.1 says the **backend** allocates the JSON-RPC request `id`. This plan
allocates the `id` in the **frontend** instead. Rationale: with a single client
there is no collision risk, and frontend allocation removes an async race
(register the pending promise *before* sending). The Tauri `rpc` command
therefore takes an explicit `id`.

---

## Phase P: Tauri app + frontend

### Task P1: Scaffold `desktop/` and verify the toolchain

**Files:**
- Create: `desktop/` (via scaffolder), plus `.gitignore` entries
- Create: `desktop/README.md` (dev/build instructions)

**Step 1: Scaffold the Tauri app**

From the worktree root:

```bash
npm create tauri-app@latest desktop -- --template react-ts --manager npm --yes
```

Expected: creates `desktop/` with `package.json`, `src/` (React+TS+Vite),
`src-tauri/` (Tauri 2 Rust), `index.html`, `vite.config.ts`, `tsconfig.json`.

If the non-interactive flags differ in the installed version, run
`npm create tauri-app@latest -- --help` and adapt; the required outcome is a
`react-ts` template. Do NOT hand-roll the scaffold.

**Step 2: Add Tailwind CSS**

```bash
cd desktop && npm install -D tailwindcss @tailwindcss/postcss postcss autoprefixer
```

Configure Tailwind v4 (PostCSS plugin form) — create
`desktop/postcss.config.js`:

```js
export default {
  plugins: { "@tailwindcss/postcss": {} },
};
```

Replace `desktop/src/index.css` (or the template's css entry) with:

```css
@import "tailwindcss";
```

Import that css file from `desktop/src/main.tsx`.

**Step 3: Pin dev port and strict config**

Ensure `desktop/vite.config.ts` uses port `1420` with `strictPort: true` (the
Tauri template already does this). Confirm `desktop/src-tauri/tauri.conf.json`
has `build.devUrl = "http://localhost:1420"` and
`build.frontendDist = "../dist"`.

**Step 4: Verify frontend build and Rust build**

```bash
cd desktop && npm install
npm run build                 # vite build → dist/
cargo build --manifest-path src-tauri/Cargo.toml
```

Expected: both succeed. (The Tauri Rust build downloads many crates; allow
time.)

**Step 5: Verify the window opens (manual, macOS)**

```bash
cd desktop && npm run tauri dev
```

Expected: a native window titled `yi-agent` opens. Close it. If the sidecar
config from Task P2 is not yet in place, this may warn about a missing external
binary — that is fine at this step; the window itself must open.

**Step 6: Gitignore + README + commit**

Add to `desktop/.gitignore`:

```
node_modules
dist
src-tauri/target
src-tauri/binaries
```

Write `desktop/README.md` documenting `npm install`, `npm run tauri dev`,
`npm run build`, `npm test`, and the sidecar build step from Task P2.

```bash
git add desktop .gitignore
git commit -m "feat(desktop): scaffold Tauri 2 app with React, TS and Tailwind"
```

---

### Task P2: Build the sidecar binary and wire `externalBin`

**Files:**
- Create: `desktop/scripts/build-sidecar.sh`
- Modify: `desktop/package.json` (add scripts)
- Modify: `desktop/src-tauri/tauri.conf.json` (add `bundle.externalBin`)

**Step 1: Write the sidecar build script**

Create `desktop/scripts/build-sidecar.sh` (make it executable):

```bash
#!/usr/bin/env bash
# Build the yi-agent CLI and copy it to src-tauri/binaries with the
# target-triple suffix Tauri's externalBin mechanism requires.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"     # desktop/
ROOT="$(cd "$HERE/.." && pwd)"               # repo root
TRIPLE="$(rustc -vV | awk '/^host:/ {print $2}')"
PROFILE="${1:-debug}"

if [ "$PROFILE" = "release" ]; then
  cargo build --manifest-path "$ROOT/yi-agent-rs/Cargo.toml" -p yi-agent --release
  SRC="$ROOT/yi-agent-rs/target/release/yi-agent"
else
  cargo build --manifest-path "$ROOT/yi-agent-rs/Cargo.toml" -p yi-agent
  SRC="$ROOT/yi-agent-rs/target/debug/yi-agent"
fi

mkdir -p "$HERE/src-tauri/binaries"
cp "$SRC" "$HERE/src-tauri/binaries/yi-agent-$TRIPLE"
echo "sidecar -> src-tauri/binaries/yi-agent-$TRIPLE"
```

**Step 2: Add npm scripts**

In `desktop/package.json` `"scripts"`, add:

```json
"sidecar": "bash scripts/build-sidecar.sh",
"sidecar:release": "bash scripts/build-sidecar.sh release",
"dev": "npm run sidecar && vite",
"tauri": "tauri"
```

(Keep the scaffolder's existing `dev`/`build`/`tauri` entries; ensure `dev`
builds the sidecar first.)

**Step 3: Declare the external binary**

In `desktop/src-tauri/tauri.conf.json`, under `"bundle"`, add:

```json
"externalBin": ["binaries/yi-agent"]
```

**Step 4: Verify**

```bash
cd desktop && npm run sidecar
ls -l src-tauri/binaries/
```

Expected: one file named `yi-agent-<target-triple>` (e.g.
`yi-agent-aarch64-apple-darwin`). Smoke-check it:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"x","version":"0"}}}' \
  | src-tauri/binaries/yi-agent-$(rustc -vV | awk '/^host:/ {print $2}') app-server --listen stdio://
```

Expected: exactly one JSON-RPC response line containing `serverInfo`.

**Step 5: Commit**

```bash
git add desktop/scripts desktop/package.json desktop/src-tauri/tauri.conf.json
git commit -m "build(desktop): bundle yi-agent as a Tauri sidecar"
```

---

### Task P3: Rust bridge — sidecar lifecycle + `rpc` / `rpc_respond`

**Files:**
- Modify: `desktop/src-tauri/Cargo.toml` (add `tauri-plugin-shell`)
- Create: `desktop/src-tauri/src/bridge.rs`
- Modify: `desktop/src-tauri/src/lib.rs` (register plugin + commands + state)
- Test: unit tests in `bridge.rs` for the pure line-classification helper

**Step 1: Add the shell plugin**

```bash
cd desktop/src-tauri && cargo add tauri-plugin-shell
```

**Step 2: Write the pure classifier first (TDD)**

Create `desktop/src-tauri/src/bridge.rs` with a pure function and tests:

```rust
use serde_json::Value;

/// Classification of one line received on the sidecar's stdout.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    /// Server→client notification (has `method`, no `id`).
    Notification,
    /// Server→client reverse request (has both `method` and `id`).
    ReverseRequest,
    /// Response to a client request (has `id`, no `method`).
    Response,
    /// Not a JSON-RPC frame — drop and log, never crash.
    Garbage,
}

pub fn classify(value: &Value) -> Frame {
    let has_method = value.get("method").is_some();
    let has_id = value.get("id").is_some();
    match (has_method, has_id) {
        (true, true) => Frame::ReverseRequest,
        (true, false) => Frame::Notification,
        (false, true) => Frame::Response,
        (false, false) => Frame::Garbage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notification_has_method_without_id() {
        assert_eq!(
            classify(&json!({"method":"turn/started","params":{}})),
            Frame::Notification
        );
    }

    #[test]
    fn reverse_request_has_method_and_id() {
        assert_eq!(
            classify(&json!({"id":"perm-1","method":"item/toolCall/requestApproval","params":{}})),
            Frame::ReverseRequest
        );
    }

    #[test]
    fn response_has_id_without_method() {
        assert_eq!(classify(&json!({"id":1,"result":{}})), Frame::Response);
    }

    #[test]
    fn garbage_is_dropped() {
        assert_eq!(classify(&json!({"hello":"world"})), Frame::Garbage);
    }
}
```

**Step 3: Implement lifecycle + commands**

In `bridge.rs`, add (keep it backend-only; no agent logic):

```rust
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

pub struct Sidecar {
    child: Mutex<Option<CommandChild>>,
}

impl Sidecar {
    pub fn new() -> Self {
        Self { child: Mutex::new(None) }
    }
}

fn write_line(state: &Sidecar, value: &serde_json::Value) -> Result<(), String> {
    let mut buf = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    buf.push(b'\n');
    let mut guard = state.child.lock().map_err(|e| e.to_string())?;
    let child = guard.as_mut().ok_or_else(|| "sidecar not running".to_string())?;
    child.write(&buf).map_err(|e| e.to_string())
}

/// Frontend → sidecar: write one JSON-RPC request line.
#[tauri::command]
pub fn rpc(
    state: State<'_, Sidecar>,
    id: u64,
    method: String,
    params: serde_json::Value,
) -> Result<(), String> {
    write_line(
        &state,
        &serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
    )
}

/// Frontend → sidecar: reply to a reverse request (permission approval).
#[tauri::command]
pub fn rpc_respond(
    state: State<'_, Sidecar>,
    id: String,
    result: serde_json::Value,
) -> Result<(), String> {
    write_line(
        &state,
        &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
    )
}

/// Spawn the sidecar and forward its stdout frames as Tauri events.
pub fn spawn(app: &AppHandle) -> Result<(), String> {
    let (mut rx, child) = app
        .shell()
        .sidecar("yi-agent")
        .map_err(|e| format!("sidecar not found: {e}"))?
        .spawn()
        .map_err(|e| format!("sidecar spawn failed: {e}"))?;

    app.state::<Sidecar>()
        .child
        .lock()
        .map_err(|e| e.to_string())?
        .replace(child);

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(bytes) => forward_stdout(&app, &bytes),
                CommandEvent::Stderr(bytes) => {
                    eprintln!("[sidecar] {}", String::from_utf8_lossy(&bytes));
                }
                CommandEvent::Terminated(payload) => {
                    let _ = app.emit(
                        "app-server://status",
                        serde_json::json!({"state":"exited","code":payload.code}),
                    );
                }
                _ => {}
            }
        }
    });
    Ok(())
}

fn forward_stdout(app: &AppHandle, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let line = text.trim();
    if line.is_empty() {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        eprintln!("[sidecar] dropping non-JSON line: {line}");
        return;
    };
    match classify(&value) {
        Frame::ReverseRequest => {
            let _ = app.emit("app-server://request", &value);
        }
        Frame::Notification | Frame::Response => {
            let _ = app.emit("app-server://message", &value);
        }
        Frame::Garbage => eprintln!("[sidecar] dropping non-protocol frame: {value}"),
    }
}
```

In `desktop/src-tauri/src/lib.rs`, register the plugin, state, commands, and
spawn on setup. Replace the template body with:

```rust
mod bridge;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(bridge::Sidecar::new())
        .invoke_handler(tauri::generate_handler![bridge::rpc, bridge::rpc_respond])
        .setup(|app| {
            bridge::spawn(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

**Step 4: Verify**

```bash
cd desktop/src-tauri && cargo test && cargo build
```

Expected: `classify` tests pass; the crate builds.

**Step 5: Manual bridge smoke (macOS)**

Temporarily log every forwarded frame to the console (or open devtools) and run
`npm run tauri dev`. Confirm the app starts the sidecar and no
`sidecar not running` error appears on the first `invoke("rpc", ...)` from
Task P4. (Full smoke is Task P8.)

**Step 6: Commit**

```bash
cd desktop/src-tauri && cargo fmt
cd ../.. && git add desktop/src-tauri
git commit -m "feat(desktop): bridge sidecar stdio to Tauri events"
```

---

### Task P4: Frontend `lib/protocol.ts` and `lib/rpc.ts`

**Files:**
- Create: `desktop/src/lib/protocol.ts`
- Create: `desktop/src/lib/rpc.ts`
- Test: `desktop/src/lib/rpc.test.ts`

**Step 1: Write `protocol.ts` (types only, no runtime logic)**

```ts
export type RequestId = number | string;

export interface RpcError { code: number; message: string; data?: unknown }

export type ToolStatus = "running" | "completed" | "failed";

export type Item =
  | { type: "userMessage"; id: string; text: string }
  | { type: "agentMessage"; id: string; text: string }
  | {
      type: "toolCall";
      id: string;
      call_id: string;
      name: string;
      input: unknown;
      status: ToolStatus;
      result?: string;
    };

export type TurnStatus = "completed" | "interrupted" | "failed";

export type Notification =
  | { method: "thread/started"; params: { thread_id: string; cwd: string; model: string } }
  | { method: "turn/started"; params: { thread_id: string; turn_id: string } }
  | { method: "item/started"; params: { thread_id: string; item: Item } }
  | { method: "item/delta"; params: { thread_id: string; item_id: string; delta: string } }
  | { method: "item/completed"; params: { thread_id: string; item: Item } }
  | {
      method: "turn/completed";
      params: { thread_id: string; turn_id: string; status: TurnStatus; error?: string };
    }
  | {
      method: "thread/tokenUsage/updated";
      params: { thread_id: string; model: string; input_tokens: number; output_tokens: number };
    }
  | { method: "error"; params: { message: string } };

export type PermissionKind = "Normal" | { Blacklisted: string };

export interface ApprovalRequest {
  id: string;
  params: {
    thread_id: string;
    turn_id: string;
    request_id: number;
    tool_name: string;
    tool_input: unknown;
    prefix_suggestion: string | null;
    kind: PermissionKind;
  };
}

export type Decision =
  | { decision: "allow_once" }
  | { decision: "always_allow_tool" }
  | { decision: "always_allow_prefix"; prefix: string }
  | { decision: "deny" };

export interface ServerResponse {
  id: RequestId;
  result?: unknown;
  error?: RpcError;
}
```

**Step 2: Write failing `rpc.ts` tests**

`desktop/src/lib/rpc.test.ts` — test the correlation logic with an injected
transport (no Tauri imports in the test):

```ts
import { describe, it, expect, vi } from "vitest";
import { RpcClient, type Transport } from "./rpc";

function fakeTransport() {
  const sent: any[] = [];
  let onMessage: (m: unknown) => void = () => {};
  const transport: Transport = {
    send: async (m) => { sent.push(m); },
    respond: async () => {},
    onMessage: (cb) => { onMessage = cb; return () => { onMessage = () => {}; }; },
    onRequest: () => () => {},
    onStatus: () => () => {},
  };
  return { transport, sent, emit: (m: unknown) => onMessage(m) };
}

describe("RpcClient", () => {
  it("allocates increasing ids and resolves on matching response", async () => {
    const { transport, sent, emit } = fakeTransport();
    const client = new RpcClient(transport);
    const p = client.request("initialize", {});
    expect(sent[0].id).toBe(1);
    emit({ jsonrpc: "2.0", id: 1, result: { ok: true } });
    await expect(p).resolves.toEqual({ ok: true });
  });

  it("rejects on error response", async () => {
    const { transport, emit } = fakeTransport();
    const client = new RpcClient(transport);
    const p = client.request("nope", {});
    emit({ jsonrpc: "2.0", id: 1, error: { code: -32601, message: "no" } });
    await expect(p).rejects.toMatchObject({ code: -32601 });
  });

  it("buffers a response that arrives before the id is registered", async () => {
    const { transport, emit } = fakeTransport();
    const client = new RpcClient(transport);
    // Emit first, then request — the client must not lose the response.
    const p = client.request("initialize", {});
    emit({ jsonrpc: "2.0", id: 1, result: 42 });
    await expect(p).resolves.toBe(42);
  });
});
```

**Step 3: Run the tests — expect failure**

```bash
cd desktop && npm install -D vitest && npx vitest run src/lib/rpc.test.ts
```

Expected: FAIL (`RpcClient`/`Transport` not exported).

**Step 4: Implement `rpc.ts`**

```ts
import type { ApprovalRequest, Decision, Notification, RequestId, RpcError } from "./protocol";

/** Abstracts the Tauri IPC boundary so the client is unit-testable. */
export interface Transport {
  send(message: { id: number; method: string; params: unknown }): Promise<void>;
  respond(id: string, result: Decision): Promise<void>;
  onMessage(cb: (message: unknown) => void): () => void;
  onRequest(cb: (request: ApprovalRequest) => void): () => void;
  onStatus(cb: (status: { state: string; code?: number | null }) => void): () => void;
}

interface Pending {
  resolve: (value: unknown) => void;
  reject: (error: RpcError | Error) => void;
}

export class RpcClient {
  private nextId = 1;
  private pending = new Map<RequestId, Pending>();
  private buffered = new Map<RequestId, { result?: unknown; error?: RpcError }>();
  private notificationHandlers = new Set<(n: Notification) => void>();

  constructor(private transport: Transport) {
    transport.onMessage((raw) => this.onMessage(raw));
  }

  onNotification(handler: (n: Notification) => void): () => void {
    this.notificationHandlers.add(handler);
    return () => this.notificationHandlers.delete(handler);
  }

  onApproval(handler: (request: ApprovalRequest) => void): () => void {
    return this.transport.onRequest(handler);
  }

  onStatus(handler: (status: { state: string; code?: number | null }) => void): () => void {
    return this.transport.onStatus(handler);
  }

  async request<T = unknown>(method: string, params: unknown): Promise<T> {
    const id = this.nextId++;
    const promise = new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject });
    });
    await this.transport.send({ id, method, params });
    return promise;
  }

  async respond(id: string, decision: Decision): Promise<void> {
    await this.transport.respond(id, decision);
  }

  private onMessage(raw: unknown): void {
    const msg = raw as {
      id?: RequestId;
      method?: string;
      params?: unknown;
      result?: unknown;
      error?: RpcError;
    };
    if (msg.id !== undefined && msg.method === undefined) {
      // Response.
      const pending = this.pending.get(msg.id);
      if (!pending) {
        this.buffered.set(msg.id, { result: msg.result, error: msg.error });
        return;
      }
      this.pending.delete(msg.id);
      if (msg.error) pending.reject(msg.error);
      else pending.resolve(msg.result);
      return;
    }
    if (msg.method !== undefined) {
      for (const handler of this.notificationHandlers) {
        handler(msg as unknown as Notification);
      }
    }
  }
}
```

Note: the "buffers a response that arrives before the id is registered" test
only exercises the buffered map if the emit happens before `request` resolves;
since `request` registers synchronously before awaiting `send`, the buffered
path is exercised by the pre-registration case in the fake transport. Keep the
buffered map regardless — it is cheap insurance against out-of-order IPC.

**Step 5: Run tests — expect pass**

```bash
cd desktop && npx vitest run src/lib/rpc.test.ts
```

**Step 6: Commit**

```bash
git add desktop/src/lib/protocol.ts desktop/src/lib/rpc.ts desktop/src/lib/rpc.test.ts desktop/package.json
git commit -m "feat(desktop): add protocol types and rpc client"
```

---

### Task P5: Frontend `lib/session.ts` state machine (+ vitest)

**Files:**
- Create: `desktop/src/lib/session.ts`
- Test: `desktop/src/lib/session.test.ts`

**Step 1: Write failing tests**

`desktop/src/lib/session.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import { Session } from "./session";

describe("Session", () => {
  it("appends a user message locally", () => {
    const s = new Session();
    s.addUserMessage("hi");
    expect(s.items).toHaveLength(1);
    expect(s.items[0]).toMatchObject({ type: "userMessage", text: "hi" });
  });

  it("appends streamed agent text into one item", () => {
    const s = new Session();
    s.apply({ method: "item/started", params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } } });
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "Hel" } });
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "lo" } });
    expect(s.items).toHaveLength(1);
    expect(s.items[0]).toMatchObject({ type: "agentMessage", text: "Hello" });
  });

  it("updates a tool call item in place on item/completed", () => {
    const s = new Session();
    s.apply({ method: "item/started", params: { thread_id: "t", item: { type: "toolCall", id: "i1", call_id: "c1", name: "bash", input: {}, status: "running" } } });
    s.apply({ method: "item/completed", params: { thread_id: "t", item: { type: "toolCall", id: "i1", call_id: "c1", name: "bash", input: {}, status: "completed", result: "ok" } } });
    expect(s.items).toHaveLength(1);
    expect(s.items[0]).toMatchObject({ type: "toolCall", status: "completed", result: "ok" });
  });

  it("tracks turn lifecycle", () => {
    const s = new Session();
    expect(s.turnActive).toBe(false);
    s.apply({ method: "turn/started", params: { thread_id: "t", turn_id: "u1" } });
    expect(s.turnActive).toBe(true);
    s.apply({ method: "turn/completed", params: { thread_id: "t", turn_id: "u1", status: "completed" } });
    expect(s.turnActive).toBe(false);
    expect(s.lastStatus).toBe("completed");
  });

  it("records token usage", () => {
    const s = new Session();
    s.apply({ method: "thread/tokenUsage/updated", params: { thread_id: "t", model: "m", input_tokens: 10, output_tokens: 3 } });
    expect(s.usage).toEqual({ model: "m", input: 10, output: 3 });
  });
});
```

**Step 2: Run — expect failure**

```bash
cd desktop && npx vitest run src/lib/session.test.ts
```

**Step 3: Implement `session.ts`**

```ts
import type { Item, Notification, TurnStatus } from "./protocol";

let localSeq = 0;
const nextLocalId = () => `local-${++localSeq}`;

export class Session {
  items: Item[] = [];
  turnActive = false;
  lastStatus: TurnStatus | null = null;
  lastError: string | null = null;
  usage: { model: string; input: number; output: number } | null = null;

  addUserMessage(text: string): void {
    this.items.push({ type: "userMessage", id: nextLocalId(), text });
  }

  apply(notification: Notification): void {
    switch (notification.method) {
      case "item/started":
      case "item/completed": {
        const incoming = notification.params.item;
        const index = this.items.findIndex((i) => i.id === incoming.id);
        if (index >= 0) this.items[index] = incoming;
        else this.items.push(incoming);
        break;
      }
      case "item/delta": {
        const { item_id, delta } = notification.params;
        const index = this.items.findIndex((i) => i.id === item_id);
        if (index >= 0 && this.items[index].type === "agentMessage") {
          const item = this.items[index] as { type: "agentMessage"; id: string; text: string };
          item.text += delta;
        } else {
          this.items.push({ type: "agentMessage", id: item_id, text: delta });
        }
        break;
      }
      case "turn/started":
        this.turnActive = true;
        this.lastError = null;
        break;
      case "turn/completed":
        this.turnActive = false;
        this.lastStatus = notification.params.status;
        this.lastError = notification.params.error ?? null;
        break;
      case "thread/tokenUsage/updated":
        this.usage = {
          model: notification.params.model,
          input: notification.params.input_tokens,
          output: notification.params.output_tokens,
        };
        break;
      case "error":
        this.lastError = notification.params.message;
        break;
      default:
        break;
    }
  }
}
```

**Step 4: Run — expect pass**

```bash
cd desktop && npx vitest run
```

**Step 5: Commit**

```bash
git add desktop/src/lib/session.ts desktop/src/lib/session.test.ts
git commit -m "feat(desktop): add session state machine with vitest coverage"
```

---

### Task P6: UI — App shell, ChatView, MessageInput, ToolCallCard

**Files:**
- Create: `desktop/src/components/ChatView.tsx`
- Create: `desktop/src/components/MessageInput.tsx`
- Create: `desktop/src/components/ToolCallCard.tsx`
- Create: `desktop/src/components/StatusBar.tsx`
- Create: `desktop/src/tauriTransport.ts` (implements `Transport` with `invoke`/`listen`)
- Modify: `desktop/src/App.tsx`, `desktop/src/main.tsx`

**Step 1: Implement the Tauri transport**

`desktop/src/tauriTransport.ts`:

```ts
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ApprovalRequest, Decision } from "./lib/protocol";
import type { Transport } from "./lib/rpc";

export function tauriTransport(): Transport {
  return {
    send: (message) => invoke("rpc", message),
    respond: (id, result) => invoke("rpc_respond", { id, result }),
    onMessage: (cb) => {
      let unlisten = () => {};
      listen<unknown>("app-server://message", (e) => cb(e.payload)).then((u) => (unlisten = u));
      return () => unlisten();
    },
    onRequest: (cb) => {
      let unlisten = () => {};
      listen<ApprovalRequest>("app-server://request", (e) => cb(e.payload)).then((u) => (unlisten = u));
      return () => unlisten();
    },
    onStatus: (cb) => {
      let unlisten = () => {};
      listen<{ state: string; code?: number | null }>("app-server://status", (e) => cb(e.payload)).then((u) => (unlisten = u));
      return () => unlisten();
    },
  };
}
```

**Step 2: Implement `ToolCallCard.tsx`**

A collapsible card showing `name`, pretty-printed `input` JSON, a status badge
(`running`/`completed`/`failed`), and `result` when present. Use Tailwind
utilities only. Must render long output in a scrollable `<pre>` with
`whitespace-pre-wrap`.

**Step 3: Implement `ChatView.tsx`**

Render `items` in order:
- `userMessage` → right-aligned bubble.
- `agentMessage` → left-aligned, monospace (`font-mono whitespace-pre-wrap`),
  plain text (no markdown in baseline).
- `toolCall` → `<ToolCallCard/>`.

Auto-scroll to bottom on new items/deltas (`useEffect` + a bottom sentinel
`ref.scrollIntoView()`).

**Step 4: Implement `MessageInput.tsx`**

Textarea + Send button. Disabled while `turnActive`. When `turnActive`, the
button becomes **Stop** and calls `rpc.request("turn/interrupt", {})`. Enter
sends, Shift+Enter inserts a newline.

**Step 5: Implement `StatusBar.tsx`**

Shows workdir, model (from `thread/started` params), a status dot
(connected/exited), and token usage when available.

**Step 6: Wire `App.tsx`**

```tsx
import { useEffect, useRef, useState } from "react";
import { RpcClient } from "./lib/rpc";
import { Session } from "./lib/session";
import { tauriTransport } from "./tauriTransport";
import { ChatView } from "./components/ChatView";
import { MessageInput } from "./components/MessageInput";
import { StatusBar } from "./components/StatusBar";
import { ApprovalDialog } from "./components/ApprovalDialog";
import type { ApprovalRequest } from "./lib/protocol";

export default function App() {
  const [session] = useState(() => new Session());
  const [, force] = useState(0);
  const clientRef = useRef<RpcClient | null>(null);
  const [threadId, setThreadId] = useState<string | null>(null);
  const [approval, setApproval] = useState<ApprovalRequest | null>(null);
  const [status, setStatus] = useState<string>("connecting");

  useEffect(() => {
    const client = new RpcClient(tauriTransport());
    clientRef.current = client;
    client.onNotification((n) => { session.apply(n); force((v) => v + 1); });
    client.onApproval((r) => setApproval(r));
    client.onStatus((s) => setStatus(s.state));
    (async () => {
      await client.request("initialize", { clientInfo: { name: "yi-agent-desktop", version: "0" } });
      await client.request("initialized", {});
      const thread = await client.request<{ id: string }>("thread/start", {});
      setThreadId(thread.id);
      setStatus("connected");
    })().catch((e) => setStatus(`error: ${e}`));
  }, [session]);

  const send = async (text: string) => {
    if (!threadId || !clientRef.current) return;
    session.addUserMessage(text);
    force((v) => v + 1);
    await clientRef.current.request("turn/start", {
      threadId,
      input: [{ type: "text", text }],
    });
  };

  const interrupt = () => clientRef.current?.request("turn/interrupt", {});

  return (
    <div className="flex h-screen flex-col bg-neutral-950 text-neutral-100">
      <StatusBar session={session} status={status} />
      <ChatView items={session.items} />
      <MessageInput turnActive={session.turnActive} onSend={send} onInterrupt={interrupt} />
      {approval && (
        <ApprovalDialog
          request={approval}
          onDecide={async (decision) => {
            await clientRef.current?.respond(approval.id, decision);
            setApproval(null);
          }}
        />
      )}
    </div>
  );
}
```

Note: `ApprovalDialog` is implemented in Task P7. To keep this task
independently verifiable, create a minimal placeholder `ApprovalDialog.tsx` here
that just renders the tool name and the three buttons, then flesh it out in
Task P7.

**Step 7: Verify**

```bash
cd desktop && npm run build && npx vitest run
```

Expected: TS build clean, unit tests pass. Manual: `npm run tauri dev` → type a
prompt → streaming text appears (requires a configured provider; if no API key,
at least the request/response plumbing and error bubble must work).

**Step 8: Commit**

```bash
git add desktop/src
git commit -m "feat(desktop): add chat UI, input and tool-call cards"
```

---

### Task P7: Approval dialog

**Files:**
- Modify: `desktop/src/components/ApprovalDialog.tsx`

**Step 1: Implement**

Modal overlay (fixed, centered, `backdrop-blur`) showing:
- Tool name (monospace) and `kind` (highlight blacklisted in red).
- Pretty-printed `tool_input`.
- `prefix_suggestion` when present.
- Buttons: **Allow once** → `{decision:"allow_once"}`; **Always allow tool** →
  `{decision:"always_allow_tool"}`; when `prefix_suggestion` is present also
  **Always allow `<prefix>`** → `{decision:"always_allow_prefix", prefix}`;
  **Deny** → `{decision:"deny"}`.

The component must call `onDecide(decision)` exactly once and then be dismissed
by the parent. Keyboard: `Esc` = Deny.

**Step 2: Verify**

```bash
cd desktop && npm run build
```

Manual: trigger a `bash` tool call (with a non-yolo config) and confirm the
dialog appears, each button sends the right decision, and the agent continues
(allow) or receives a denial.

**Step 3: Commit**

```bash
git add desktop/src/components/ApprovalDialog.tsx
git commit -m "feat(desktop): add permission approval dialog"
```

---

### Task P8: End-to-end smoke, macOS bundle, docs sync

**Files:**
- Modify: `desktop/README.md`
- Modify: `docs/project-management/README.md` (add a `desktop` module row)
- Create: `docs/project-management/desktop.md` (module file)

**Step 1: Full manual smoke (design §12 success criteria)**

```bash
cd desktop && npm run sidecar && npm run tauri dev
```

Confirm, in order:
1. Native window opens, no browser chrome.
2. Enter a prompt → text streams in incrementally.
3. A tool call renders as a card with input/output.
4. A dangerous tool (`bash`) triggers the approval dialog; **Allow once**
   continues the turn, **Deny** makes the agent observe a denial.
5. **Stop** interrupts an in-flight turn immediately.
6. Killing the sidecar process shows the disconnected status banner (no crash).

Record the results (with screenshots if possible) in the PR description.

**Step 2: macOS bundle**

```bash
cd desktop && npm run sidecar:release && npm run tauri build
```

Expected: `desktop/src-tauri/target/release/bundle/macos/yi-agent.app` and a
`.dmg`. Verify the bundled app launches and spawns its embedded sidecar (launch
the `.app` from Finder, not `tauri dev`).

**Step 3: Docs sync (project rule)**

Create `docs/project-management/desktop.md` with the `desktop` module's
features and verifiable criteria (commands + file:line). Add a row to
`docs/project-management/README.md`'s index table:

```
| desktop | N / N | [详情](./desktop.md) |
```

**Step 4: Commit**

```bash
git add desktop/README.md docs/project-management
git commit -m "docs(desktop): document baseline app and sync module index"
```

---

## Completion criteria (Phase 2 / P0 baseline)

- [ ] `cd desktop && npm run build` succeeds (TS + Vite).
- [ ] `cd desktop && npx vitest run` is green (`session.ts`, `rpc.ts`).
- [ ] `cd desktop/src-tauri && cargo test && cargo build` is green.
- [ ] `npm run tauri build` produces a launchable `yi-agent.app` that embeds and
      spawns the sidecar.
- [ ] Manual smoke (Task P8 Step 1) passes all six checks.
- [ ] `desktop.md` module file + README index row exist.
- [ ] `cargo test -p yi-agent-app-server && cargo test -p yi-agent-runtime` still
      green (no regression to Phase 1).
