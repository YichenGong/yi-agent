# Desktop Stale-Status Interject Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the desktop app recover from `-32013 no turn is running` (and its mirror `-32012`) instead of surfacing it and dropping the user's message, and stop the cached thread status from being permanently corrupted by a stale `thread/listAll` snapshot.

**Architecture:** `send()` gains a bounded two-hop self-heal loop keyed on the RPC error code, and picks its first method from the session's own `turnActive` (the same source the input button uses). `ThreadStore.seed()` stops overwriting a status that the live push stream already authoritative-ly wrote.

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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/lib/threadStore.test.ts`
Expected: FAIL — the first case reports `expected 'running' to be 'idle'`. (The second case passes already.)

- [ ] **Step 3: Write minimal implementation**

In `desktop/src/lib/threadStore.ts`, replace the body of `seed` and its doc comment:

```ts
  /**
   * Seeds status and cwd/model from a `thread/listAll` snapshot. **Does not
   * touch session content** — sessions are accumulated from notifications only.
   *
   * The snapshot is a point-in-time read that can be stale: the server writes
   * `turn/completed` *before* it persists the turn and flips the thread back to
   * `idle`, and the app re-lists the moment it sees `turn/completed`. A listing
   * issued inside that window still reports `running`. So the snapshot only
   * *seeds* a status the client has never learned; once the push stream has set
   * one, the snapshot must not roll it back.
   */
  seed(threads: ThreadSummary[]): void {
    for (const t of threads) {
      const existing = this.views.get(t.thread_id);
      const v = existing ?? this.create();
      if (!existing) this.views.set(t.thread_id, v);
      // Only seed a status we do not have yet. `v.status` is initialised to
      // "idle" at creation, so a fresh view is indistinguishable from one the
      // push stream set to "idle" — hence the `existing` check, not a
      // `v.status === "idle"` comparison.
      if (!existing) v.status = t.status ?? "idle";
      v.info = { cwd: t.cwd, model: t.model };
    }
  }
```

Note: `this.view(id)` would create-and-insert; we need to know whether it pre-existed, so read `this.views` directly.

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
 * Pick the send RPC from what the UI is showing, so the button and the wire
 * agree: while a turn is active the button says "Stop" and the message is
 * folded into it; otherwise it says "Send" and opens a new turn.
 */
function preferredSendMethod(turnActive: boolean): "turn/interject" | "turn/start" {
  return turnActive ? "turn/interject" : "turn/start";
}

/** The `-32013`/`-32012` codes both mean "the other method was the right one". */
function isMethodMismatchCode(code: unknown): code is number {
  return code === -32013 || code === -32012;
}
```

Replace `send` with:

```ts
  const send = async (text: string): Promise<boolean> => {
    const id = store.currentId;
    const c = clientRef.current;
    if (!id || !c) return false;
    const session = store.view(id).session;
    session.addUserMessage(text);
    force((v) => v + 1);
    // The status the UI renders and the status the server holds can disagree:
    // `turn/completed` reaches us before the server flips the thread back to
    // idle, and a listing read inside that window keeps us on the old value.
    // So send the method the UI implies, then let the server's own error code
    // tell us where it disagreed — each direction is tried at most once.
    const params = { threadId: id, input: [{ type: "text", text }] };
    let method = preferredSendMethod(session.turnActive);
    let lastError: unknown = null;
    for (let hop = 0; hop < 2; hop += 1) {
      try {
        await c.request(method, params);
        return true;
      } catch (e) {
        lastError = e;
        const code = (e as { code?: unknown } | null)?.code;
        if (!isMethodMismatchCode(code)) break;
        const fallback = method === "turn/interject" ? "turn/start" : "turn/interject";
        // Learn from the rejection so the very next send picks correctly.
        const status: ThreadStatus = fallback === "turn/interject" ? "running" : "idle";
        const view = store.peek(id);
        if (view) view.status = status;
        if (code === -32013) session.turnActive = false;
        else session.turnActive = true;
        method = fallback;
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

`ThreadStatus` is already imported at `desktop/src/App.tsx:11`.

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
- §2.3 (A, input/first-method alignment on session status) → Task 2 Step 3 (`preferredSendMethod(session.turnActive)`).
- §2.4 tests 1-4 → Task 2 Step 1; tests 5-7 → Task 1 Step 1 (7 is covered by the pre-existing "seeds status and cwd/model from a listing snapshot" case, which must stay green).
- §3 docs row → Task 3.

**Type consistency:** `preferredSendMethod` returns `"turn/interject" | "turn/start"`; the fallback ternary mirrors it; `ThreadStatus` is the imported union; `isMethodMismatchCode` narrows to `number` and both call sites compare against the exact codes used by the server (`server.rs` `not_running` = -32013, `turn_in_progress` = -32012).

**Known risk to verify in Task 2 Step 4:** the pre-existing test drives `thread/status/updated: running` but not `turn/started`. If `turnActive` and `statuses` were both consulted before, that test may now pick `turn/start`. If so, update that test to also fire `turn/started` (the honest arrangement: a running turn always emits both) rather than reintroducing `statuses` into the decision.
