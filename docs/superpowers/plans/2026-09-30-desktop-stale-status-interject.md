# Desktop Stale-Status Interject Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the desktop app recover from `-32013 no turn is running` (and its mirror `-32012`) instead of surfacing it and dropping the user's message, and stop the cached thread status from being permanently corrupted by a stale `thread/listAll` snapshot.

**Architecture:** `send()` gains a bounded two-hop self-heal loop keyed on the RPC error code; its first method stays the cached thread status (the input's Send button is only reachable when no turn is active, so the cache is the right first guess). `ThreadStore.seed()` stops overwriting a status the live push stream already wrote.

**Tech Stack:** React 19 + TypeScript, Vite, Vitest + @testing-library/react (jsdom).

## Global Constraints

- Only `desktop/src` may change. Do not touch `yi-agent-rs/`, `desktop/src-tauri/`, or the app-server protocol.
- Error codes: `-32013` = `turn/interject` with no active turn; `-32012` = `turn/start` while a turn is active. Both live on the raw rejection object's `code` field (`RpcError`, `desktop/src/lib/protocol.ts:20`).
- `formatError()` discards `code`; read the raw object when branching on the code.
- No emoji in any response or file content.
- Follow CLAUDE.md: work in the worktree, `git commit` per task, no `Co-Authored-By` lines.
- Test command: `cd desktop && npm test`. Build command: `cd desktop && npm run build`.

---

## File Structure

- `desktop/src/lib/threadStore.ts` — owns the per-thread view. Task 1 changes `seed()` only.
- `desktop/src/App.tsx` — owns `send()`. Task 2 extracts the method-selection + request into a small retrying helper.
- `desktop/src/lib/threadStore.test.ts` — unit coverage for `seed()`.
- `desktop/src/App.test.tsx` — integration coverage for `send()`.

---

### Task 1: `seed()` must not roll back a live status

**Files:**
- Modify: `desktop/src/lib/threadStore.ts:68-79` (`seed`)
- Test: `desktop/src/lib/threadStore.test.ts`

**Interfaces:**
- Consumes: `ThreadSummary` from `desktop/src/lib/protocol.ts` (`{ thread_id, cwd, model, created_at, updated_at, title, permission_mode?, status? }`).
- Produces: no signature change; `seed(threads: ThreadSummary[]): void`.

- [ ] **Step 1: Write the failing test**

Add to `desktop/src/lib/threadStore.test.ts` inside the existing `describe("ThreadStore", ...)`:

```ts
  it("does not let a stale listing snapshot roll back a live status", () => {
    const s = new ThreadStore();
    s.seed([summary("a", "running")]);
    // The push stream is authoritative and says the turn just finished.
    s.applyNotification({
      method: "thread/status/updated",
      params: { thread_id: "a", status: "idle" },
    });
    expect(s.view("a").status).toBe("idle");

    // A listing read *before* the driver flipped to idle arrives late.
    s.seed([summary("a", "running")]);
    expect(s.view("a").status).toBe("idle");
  });

  it("still seeds the status of a thread it has not seen before", () => {
    const s = new ThreadStore();
    s.seed([summary("b", "running")]);
    expect(s.view("b").status).toBe("running");
  });

  it("seeds the status of a view that exists but never heard from the push stream", () => {
    const s = new ThreadStore();
    // Selecting a thread creates its view with a default "idle" before any
    // status notification arrives; the listing is then the first real source.
    s.select("a");
    expect(s.view("a").status).toBe("idle");
    s.seed([summary("a", "running")]);
    expect(s.view("a").status).toBe("running");
  });

  it("seeds again after a thread is dropped and re-listed", () => {
    const s = new ThreadStore();
    s.seed([summary("a", "running")]);
    s.drop("a");
    s.seed([summary("a", "running")]);
    expect(s.view("a").status).toBe("running");
  });
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/lib/threadStore.test.ts`
Expected: FAIL — the first case reports `expected 'running' to be 'idle'`. (The second case passes already.)

- [ ] **Step 3: Write minimal implementation**

In `desktop/src/lib/threadStore.ts`, add a source-tracking map next to `views`:

```ts
  /**
   * Which source last wrote each thread's `status`: `"live"` for the
   * `thread/status/updated` push stream (authoritative), `"snapshot"` for a
   * `thread/listAll` listing (a point-in-time read that can be stale), or
   * absent when no status has been learned yet. `seed` uses this to avoid
   * rolling a live status back.
   */
  private statusSource = new Map<string, "live" | "snapshot">();
```

Mark the push stream as authoritative in `applyNotification` (the
`thread/status/updated` branch, right after `v.status = n.params.status;`):

```ts
      this.statusSource.set(n.params.thread_id, "live");
```

Clear it in `drop`:

```ts
    this.statusSource.delete(id);
```

Replace the body of `seed` and its doc comment:

```ts
  /**
   * Seeds status and cwd/model from a `thread/listAll` snapshot. **Does not
   * touch session content** — sessions are accumulated from notifications only.
   *
   * The snapshot is a point-in-time read that can be stale: the server writes
   * `turn/completed` *before* it persists the turn and flips the thread back to
   * `idle`, and the app re-lists the moment it sees `turn/completed`. A listing
   * issued inside that window still reports `running`. So the snapshot only
   * *seeds* a status the client has never had one for; once the push stream has
   * written one, the snapshot must not roll it back. A default `idle` from a
   * merely-created view is not "written by the push stream" — `statusSource`
   * records which it is.
   */
  seed(threads: ThreadSummary[]): void {
    for (const t of threads) {
      const v = this.view(t.thread_id);
      if (this.statusSource.get(t.thread_id) !== "live") v.status = t.status ?? "idle";
      v.info = { cwd: t.cwd, model: t.model };
    }
  }
```

Do **not** use "does the view already exist" as the guard: `select()` creates a
view (default `idle`) before any status arrives, and that default is
indistinguishable from a live `idle`, which would permanently block seeding.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/lib/threadStore.test.ts`
Expected: PASS, all cases in the file green.

- [ ] **Step 5: Commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix/desktop-stale-status-interject
git add desktop/src/lib/threadStore.ts desktop/src/lib/threadStore.test.ts
git commit -m "fix(desktop): stop a stale listing snapshot from rolling back status"
```

---

### Task 2: `send()` self-heals from `-32013` / `-32012`

**Files:**
- Modify: `desktop/src/App.tsx:301-328` (`send`)
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: `RpcClient.request<T>(method, params)` which rejects with `RpcError` (`{ code: number; message: string }`).
- Produces: `send(text: string): Promise<boolean>` — unchanged signature; still resolves `true` only when a request was accepted.

- [ ] **Step 1: Write the failing tests**

Add to `desktop/src/App.test.tsx`. First extend the mock client in the `vi.hoisted` block and the `request` method so a test can force a rejection code for a given method.

Replace the `state` object's fields (add two) in `vi.hoisted`:

```ts
    // Method -> error code. When a request matches, reject with that code.
    rejectCode: {} as Record<string, number>,
```

Inside the mocked `request`, immediately after `this.requests.push({ method, params });`, add:

```ts
      const forced = state.rejectCode[method];
      if (forced !== undefined) throw { code: forced, message: `forced ${forced}` };
```

Reset it in `beforeEach`: add `state.rejectCode = {};`.

Then add these cases (a new `describe` block):

```ts
describe("App send recovery from stale status", () => {
  const sendFromInput = async (text: string) => {
    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: text } });
    fireEvent.click(screen.getByRole("button", { name: /^send$/i }));
  };

  it("falls back to turn/start when turn/interject answers -32013", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // The client believes a turn is running (as the input button would show).
    state.notifHandlers[0]({
      method: "turn/started",
      params: { thread_id: "t1", turn_id: "u1" },
    });
    state.rejectCode = { "turn/interject": -32013 };

    await sendFromInput("late message");

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    const methods = clients[0].requests.map((r) => r.method);
    expect(methods).toContain("turn/interject");
    // The user's text was delivered, not dropped on the error.
    expect(methods).toContain("turn/start");
    expect(screen.queryByText(/no turn is running/i)).toBeNull();
  });

  it("falls back to turn/interject when turn/start answers -32012", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // The client believes the thread is idle (turnActive false) but the server
    // already started a turn.
    state.rejectCode = { "turn/start": -32012 };

    await sendFromInput("raced message");

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/interject")).toBe(true),
    );
    expect(clients[0].requests.map((r) => r.method)).toContain("turn/start");
  });

  it("does not ping-pong when both methods fail, and surfaces the error", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    state.notifHandlers[0]({
      method: "turn/started",
      params: { thread_id: "t1", turn_id: "u1" },
    });
    state.rejectCode = { "turn/interject": -32013, "turn/start": -32012 };

    await sendFromInput("doomed");

    await waitFor(() => expect(screen.getByText(/forced -32012/)).toBeTruthy());
    const methods = clients[0].requests.map((r) => r.method);
    expect(methods.filter((m) => m === "turn/interject")).toHaveLength(1);
    expect(methods.filter((m) => m === "turn/start")).toHaveLength(1);
    // The optimistic bubble was rolled back.
    expect(screen.queryByText("doomed")).toBeNull();
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "send recovery"`
Expected: FAIL — the first case never issues `turn/start` (current code has no fallback) and shows `no turn is running`.

- [ ] **Step 3: Write minimal implementation**

In `desktop/src/App.tsx`, add a module-level helper above `export default function App()`:

```ts
/**
 * `-32013` means `turn/interject` arrived with no turn running; `-32012` means
 * `turn/start` arrived while one was. Both say "the other method was the right
 * one" — the server resolves the disagreement, so the code is read from the raw
 * rejection (`formatError` would drop it).
 */
function sendMethodMismatchCode(e: unknown): number | null {
  const code = (e as { code?: unknown } | null)?.code;
  return code === -32013 || code === -32012 ? code : null;
}
```

**Do not** add a `preferredSendMethod(turnActive)` helper. `MessageInput` renders
"Stop" and binds `onInterrupt` whenever `turnActive` is true, so the send path is
only reachable with `turnActive === false`; making that the first guess would send
`turn/start` in the very case the cache says `running`, breaking mid-turn folding
and the existing `uses turn/interject ...` test.

Replace `send` with:

```ts
  const send = async (text: string): Promise<boolean> => {
    const id = store.currentId;
    const c = clientRef.current;
    if (!id || !c) return false;
    const session = store.view(id).session;
    session.addUserMessage(text);
    force((v) => v + 1);
    // The cached status can be stale: `turn/completed` reaches us before the
    // server flips the thread back to idle, and a listing read inside that
    // window keeps the old value. Send the method the cache implies, then let
    // the server's error code say where it disagreed and switch to the other
    // one. Each direction is tried once, so two mismatches cannot ping-pong.
    const params = { threadId: id, input: [{ type: "text", text }] };
    let method: "turn/interject" | "turn/start" =
      store.peek(id)?.status === "running" ? "turn/interject" : "turn/start";
    let lastError: unknown = null;
    for (let hop = 0; hop < 2; hop += 1) {
      try {
        await c.request(method, params);
        return true;
      } catch (e) {
        lastError = e;
        const code = sendMethodMismatchCode(e);
        if (code === null) break;
        // Adopt the server's view so the next send (and the button) is right.
        method = method === "turn/interject" ? "turn/start" : "turn/interject";
        session.turnActive = method === "turn/interject";
        const view = store.peek(id);
        if (view) view.status = method === "turn/interject" ? "running" : "idle";
      }
    }
    session.lastError = formatError(lastError);
    // Roll back the optimistic bubble so a rejected send does not leave a
    // phantom user message.
    const last = session.items[session.items.length - 1];
    if (last && last.type === "userMessage" && last.text === text) session.items.pop();
    force((v) => v + 1);
    return false;
  };
```

`ThreadStatus` is already imported at `desktop/src/App.tsx:11` (still needed by
the sidebar `statuses` map even though `send` no longer names it).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: PASS, including the pre-existing `uses turn/interject instead of turn/start while a turn is running` case (that test fires `thread/status/updated: running`; verify it still selects `turn/interject` — if it does not, re-check that `turnActive` is driven by the same path).

- [ ] **Step 5: Run the whole desktop suite and build**

Run: `cd desktop && npm test && npm run build`
Expected: all tests PASS, build exit 0.

- [ ] **Step 6: Commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix/desktop-stale-status-interject
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "fix(desktop): recover a send from -32013/-32012 by switching method"
```

---

### Task 3: Sync project management docs

**Files:**
- Modify: `docs/project-management/desktop.md`

- [ ] **Step 1: Add the feature line**

Under the `## Features` section, append a line in the existing style (with real code citations and a runnable verification command):

```md
- [x] 发送方法自愈（`-32013`/`-32012` 自动改走另一方法）— `desktop/src/App.tsx`（`send` 的两跳重试）/ `desktop/src/lib/threadStore.ts`（`seed` 不回退实时状态）；验证 `cd desktop && npm test`
```

- [ ] **Step 2: Update README counts if the file tracks them**

Run: `grep -n "desktop" README.md | head`
If the module index table lists a completed/total count for desktop, increment the completed count by one. Otherwise skip.

- [ ] **Step 3: Commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix/desktop-stale-status-interject
git add docs/project-management/desktop.md README.md
git commit -m "docs: record the desktop stale-status send recovery"
```

---

## Self-Review

**Spec coverage**
- §2.1 (C, self-heal) → Task 2.
- §2.2 (B, snapshot no rollback) → Task 1.
- §2.3 (A) → resolved during implementation: the first-method source stays the cached status, documented in Task 2 Step 3 and in the spec §2.3.
- §2.4 tests 1-4 → Task 2 Step 1; tests 5-7 → Task 1 Step 1 (7 is covered by the pre-existing "seeds status and cwd/model from a listing snapshot" case, which must stay green).
- §3 docs row → Task 3.

**Type consistency:** `preferredSendMethod` returns `"turn/interject" | "turn/start"`; the fallback ternary mirrors it; `ThreadStatus` is the imported union; `isMethodMismatchCode` narrows to `number` and both call sites compare against the exact codes used by the server (`server.rs` `not_running` = -32013, `turn_in_progress` = -32012).

**Test-arrangement note (resolved):** the recovery tests must make the cache say
`running` by seeding `thread/listAll` with `status: "running"` (add a
`state.listStatus` knob, reset to `"idle"` in `beforeEach`). Firing
`turn/started` instead flips the button to "Stop" and there is no Send button
left to click. The helper may match `/^(send|stop)$/i` defensively.
