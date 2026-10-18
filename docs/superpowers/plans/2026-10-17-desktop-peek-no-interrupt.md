# 桌面端「查看运行中会话不打断」实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面端「点开一个正在跑的会话」只做只读补齐、不再经 `thread/resume` 打断那一轮 turn；并修掉随之暴露的「重连后陈旧 running 状态永久阻断 resume」死锁。

**Architecture:** 两处改动。(1) `App.tsx` 的 `selectThread`：把「运行中 → 只读 `readItems`」从 `isRemoteClient()` 里提出来，成为两种客户端共有的不变量；远端专属的 `subscribe`/订阅窗口早退保持原样；冷且非运行中的会话仍走 `resume` 全量回放。(2) `ThreadStore` 新增 `forgetPushStatuses()`，在侧车 `exited` 时作废崩溃前的推送运行状态，避免 `seed()` 被陈旧 `live` 值挡住。

**Tech Stack:** TypeScript / React 19、vitest 3.2.7、@testing-library/react、tsc 6.0.3。

## Global Constraints

- 规划文件路径（相对项目根 `/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent`）：
  - `desktop/src/App.tsx`
  - `desktop/src/App.test.tsx`
  - `desktop/src/lib/threadStore.ts`
  - `desktop/src/lib/threadStore.test.ts`
- 不新增协议 RPC；不改服务端（`yi-agent-rs/`）任何文件。
- 不改 iOS 现有行为：`thread/subscribe` 仍只对远端调用。
- 所有前端命令必须在 `desktop/` 下运行，且先 `export PATH="/opt/homebrew/bin:$PATH"`。
- 用 `desktop/node_modules/.bin/{tsc,vitest}`，不要用全局同名命令。
- 改动前的基线：`desktop/src/App.tsx` 2,483 行；全量测试 **91 文件 / 1140 测试**。
- 期望终态：`tsc --noEmit` 无输出；全量测试 **91 文件 / 1144 测试**全绿。
- 缩进：`selectThread` 与 `ThreadStore` class 内均为 2 空格；新增测试与同 `describe` 内既有 `it(` 对齐。

---

### Task 1: `ThreadStore.forgetPushStatuses`（含单测）

体积小、无依赖，先做以便 Task 3 可用。

**Files:**
- Modify: `desktop/src/lib/threadStore.ts:205-212`（`drop` 方法之后、class 闭合 `}` 之前）
- Test: `desktop/src/lib/threadStore.test.ts`（在 `it("keeps an unsent draft per thread and returns it on switch-back", ...)` 之前插入）

**Interfaces:**
- Consumes: 既有 `ThreadStore` 的 `seed(threads: ThreadSummary[])`、`view(id)`、`applyNotification(n)`、私有 `statusSource`。
- Produces: `ThreadStore.prototype.forgetPushStatuses(): void`（无参数、无返回）。Task 3 在 `App.tsx` 里调用它。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/lib/threadStore.test.ts` 中，找到这一行（当前约 176 行）：

```ts
  it("keeps an unsent draft per thread and returns it on switch-back", () => {
```

在它**之前**插入：

```ts
  it("forgetPushStatuses lets a fresh listing re-seed a stale running status", () => {
    const s = new ThreadStore();
    // A turn was running: the push stream marked the status authoritative.
    s.applyNotification({
      method: "thread/status/updated",
      params: { thread_id: "a", status: "running" },
    });
    // The sidecar crashes; the new process reports the thread as idle. Without
    // forgetting the push, `seed` would refuse to overwrite the stale `running`.
    s.seed([summary("a", "idle")]);
    expect(s.view("a").status).toBe("running");

    s.forgetPushStatuses();
    s.seed([summary("a", "idle")]);
    expect(s.view("a").status).toBe("idle");
  });

```

（`summary` 是该测试文件顶部已有的辅助函数，签名 `summary(id: string, status?: ThreadSummary["status"])`。不要重复定义它。）

- [ ] **Step 2: 运行测试确认失败**

Run:
```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run src/lib/threadStore.test.ts --reporter=dot 2>&1 | tail -20
```
Expected: FAIL，报 `s.forgetPushStatuses is not a function`（TypeScript 编译期也可能先报 `Property 'forgetPushStatuses' does not exist`）。

- [ ] **Step 3: 写最小实现**

在 `desktop/src/lib/threadStore.ts` 中，找到 `drop` 方法（约 205 行）：

```ts
  /** 删除某 thread 的全部客户端状态。 */
  drop(id: string): void {
    this.views.delete(id);
    this.statusSource.delete(id);
    if (this.currentId === id) this.currentId = null;
  }
}
```

把结尾的 `  }\n}` 替换为：

```ts
  }

  /**
   * 作废所有「来自推送」的运行状态：宿主进程已死，其推送不再成立。
   *
   * 侧车退出后新进程对任何会话都不再持有内存 agent（全部退化为冷会话）。
   * `statusSource` 却会活过这次重连：`seed()` 不覆盖 `"live"` 的值，于是一条
   * 崩溃前正在跑、`listAll` 现在已报 `idle` 的会话会**一直停在 `running`**。
   * 后果不只是状态徽标骗人——它会让 `selectThread` 误以为该会话仍活着而跳过
   * `thread/resume`（本 card 正是为此不再打断运行中的会话），于是新进程里
   * 那个 agent 永远不会被重建，随后的发送撞上 `-32011 unknown thread` 且无法
   * 自愈（`sendMethodMismatchCode` 只认 `-32013/-32012`）。
   *
   * 清 `statusSource` 而非直接改 `status`：清除后 `seed()` 会按下一份
   * `listAll` 快照重新落值，真实仍活着的会话也会随之从 `running` 复位——因为
   * 新进程会把它们真正重建（resume 或首次发送），此后推送再把状态标回 `"live"`。
   */
  forgetPushStatuses(): void {
    this.statusSource.clear();
  }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run:
```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run src/lib/threadStore.test.ts --reporter=dot 2>&1 | tail -20
```
Expected: PASS，`Test Files 1 passed`，`Tests 22 passed`（原 21 + 新增 1）。

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent && git add desktop/src/lib/threadStore.ts desktop/src/lib/threadStore.test.ts && git commit -m "fix(desktop): let a fresh listing re-seed a stale pushed status"
```

---

### Task 2: `selectThread` 把「运行中 → 只读」提为共有不变量

核心修复。

**Files:**
- Modify: `desktop/src/App.tsx:893-927`（`selectThread` 内，`setProcessOutput(null);` 之后到 `if (warm.current.has(id) ...)` 之前）
- Test: `desktop/src/App.test.tsx`（`describe("App remote subscription wiring (S2)")` 内，`it("never subscribes on the desktop build", ...)` 之后）

**Interfaces:**
- Consumes: `store.peek(id)`（`ThreadView | undefined`，`ThreadView.status: ThreadStatus`）、`clientRef.current.request(method, params)`、`subWindow.current.{has,touch}`、`isRemoteClient()`。
- Produces: 无新导出。行为契约：`status ∈ {"running","awaiting_approval"}` ⇒ 发 `thread/readItems` 且不发 `thread/resume`。

- [ ] **Step 1: 写失败测试（桌面端不打断）**

在 `desktop/src/App.test.tsx` 中，找到这一处（`describe("App remote subscription wiring (S2)")` 内）：

```ts
  it("never subscribes on the desktop build", async () => {
    render(<App />); // no persisted config → desktop, even with a phone UA absent
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    expect(clients[0].requests.some((r) => r.method === "thread/subscribe")).toBe(false);
    expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(false);
  });
```

在它**之后**插入：

```ts
  it("does not interrupt a running thread on the desktop build", async () => {
    // 桌面端过去没有这层保护（它被 `isRemoteClient()` 关在远端分支里）：点开
    // 一张正在跑的看板卡会话就会 resume，而 resume 会先中断在跑的 turn。这条
    // 用例把「看一眼 = 只读」钉死为**两种客户端共有**的不变量。
    state.listStatus = "running";
    state.dataSources["thread/readItems"] = () => ({ items: [] });
    render(<App />); // no persisted config → desktop

    await waitFor(() =>
      expect(
        clients[0].requests.some(
          (r) =>
            r.method === "thread/readItems" &&
            (r.params as { threadId: string }).threadId === "t1",
        ),
      ).toBe(true),
    );
    // The defining property: looking at a running thread never resumes it.
    expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(0);
    // Desktop still never subscribes; that stays remote-only.
    expect(clients[0].requests.some((r) => r.method === "thread/subscribe")).toBe(false);
  });

  it("still resumes a cold idle thread on the desktop build", async () => {
    // 回归护栏：把「运行中才只读」这条边钉住。若有人把 running 判断写错成
    // 「一律不 resume」，冷会话就再也合不进服务端历史（resume 是它唯一的就绪
    // 途径，turn/start 对冷会话返回 -32011）。
    render(<App />); // listStatus 默认 idle
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(false);
  });
```

（`state.listStatus` 与 `state.dataSources` 是该测试文件顶部 `vi.hoisted` 的 harness 字段，均已存在，不要新增。）

- [ ] **Step 2: 运行测试确认失败**

Run:
```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run src/App.test.tsx -t "does not interrupt a running thread on the desktop build" --reporter=dot 2>&1 | tail -25
```
Expected: FAIL —— `thread/resume` 次数不为 0（当前桌面端对 running 会话仍会 resume）。

- [ ] **Step 3: 改 `selectThread`**

在 `desktop/src/App.tsx` 中，找到 `selectThread` 内这一段（约 893-927 行）：

```ts
    // Remote-only (S2): keep this thread in the warm subscription window, and
    // catch up a cold-but-running thread with the read-only `thread/readItems`
    // instead of `thread/resume` — resume interrupts the in-flight turn, which
    // is exactly what a passive "look at it" must not do. Desktop (stdio) never
    // subscribes: its single client receives everything.
    if (isRemoteClient()) {
      const inWindow = subWindow.current.has(id);
      const win = subWindow.current.touch(id);
      const view = store.peek(id);
      const running = view?.status === "running" || view?.status === "awaiting_approval";
      const c0 = clientRef.current;
      if (c0) {
        if (!inWindow && running) {
          const after = store.peek(id)?.session.lastServerItemId ?? null;
          void (async () => {
            try {
              const r = await c0.request<{ items: Item[] }>(
                "thread/readItems",
                after ? { threadId: id, afterItemId: after } : { threadId: id },
              );
              const v = store.peek(id);
              if (v) {
                v.session.upsertItems(r.items);
                force((n) => n + 1);
              }
            } catch {
              // 补齐是附加能力:读失败就保留已有内容,不改动会话状态。
            }
          })();
        }
        void c0.request("thread/subscribe", { threadIds: win }).catch(() => {});
      }
      // Already warm, or caught up via readItems above → view-only swap.
      if (inWindow || running) return;
    }
```

整体替换为：

```ts
    // **看**一个运行中的会话绝不走 `thread/resume`：resume 会先中断在跑的
    // turn（服务端 `interrupt_and_wait_for_persist`），而「点开看一眼」是纯
    // 被动动作，不该有把别人打断的副作用。改用只读的 `thread/readItems` 补齐
    // 内容，之后靠既有的通知流继续跟新。这对桌面端同样成立——桌面端过去没有
    // 这层保护（它被 `isRemoteClient()` 关在远端分支里），于是点开一张正在跑
    // 的看板卡就会把它打断。
    const curView = store.peek(id);
    const running =
      curView?.status === "running" || curView?.status === "awaiting_approval";

    // Remote-only (S2): keep this thread in the warm subscription window.
    // Desktop (stdio) never subscribes: its single client receives everything.
    let inWindow = false;
    if (isRemoteClient()) {
      inWindow = subWindow.current.has(id);
      const win = subWindow.current.touch(id);
      void clientRef.current
        ?.request("thread/subscribe", { threadIds: win })
        .catch(() => {});
      // Already warm → view-only swap.
      if (inWindow) return;
    }

    if (running) {
      const c0 = clientRef.current;
      if (c0) {
        const after = store.peek(id)?.session.lastServerItemId ?? null;
        void (async () => {
          try {
            const r = await c0.request<{ items: Item[] }>(
              "thread/readItems",
              after ? { threadId: id, afterItemId: after } : { threadId: id },
            );
            const v = store.peek(id);
            if (v) {
              v.session.upsertItems(r.items);
              force((n) => n + 1);
            }
          } catch {
            // 补齐是附加能力:读失败就保留已有内容,不改动会话状态。
          }
        })();
      }
      return;
    }
```

注意：新代码用 `curView` 而不是 `view`，因为 `selectThread` 后面的 resume 路径里已有 `const view = store.peek(id);`（约 952、976 行），同名会与块级作用域冲突。**不要**改动那两处。

- [ ] **Step 4: 运行测试确认通过**

Run:
```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run src/App.test.tsx --reporter=dot 2>&1 | tail -20
```
Expected: 新增 2 条通过。**同时会看到 2 条既有测试失败**（`App send recovery from a status disagreement` 下的 `falls back to turn/start when turn/interject answers -32013` 与 `does not ping-pong when both methods fail, and surfaces the error`）——它们用 `waitFor(thread/resume)` 当作「应用已加载」的锚点，而 running 会话现在正确地不再 resume。Task 4 修这两条锚点。此步先确认**失败原因正是锚点超时**（报错行是 `expect(...).toBe(true)` 里的 `thread/resume` 判断），而不是别的断言。

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent && git add desktop/src/App.tsx desktop/src/App.test.tsx && git commit -m "fix(desktop): peeking at a running thread no longer interrupts it"
```

---

### Task 3: `exited` 时作废陈旧推送状态（含集成测试）

Task 2 的必要配套：否则「重连后陈旧 running」会阻断 resume，把会话卡死。

**Files:**
- Modify: `desktop/src/App.tsx:1483`（`exited` 处理器内，`warm.current.clear();` 之后）
- Test: `desktop/src/App.test.tsx`（`describe("App reconnect resilience")` 内，`it("reconnects immediately when the app returns to the foreground", ...)` 之前）

**Interfaces:**
- Consumes: `store.forgetPushStatuses()`（Task 1 产出）；该套件既有辅助 `exit()`、`flush()`、`waitMounted()`、`nextReconnectDelay(n)`。
- Produces: 无新导出。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/App.test.tsx` 中，找到 `describe("App reconnect resilience")` 内的：

```ts
  it("reconnects immediately when the app returns to the foreground", async () => {
```

在它**之前**插入：

```ts
  it("re-resumes a thread after a crash even though its status was live-running", async () => {
    // 崩溃前该会话在跑（推送把状态标为 "live" running）。侧车重启后新进程对
    // 它没有内存 agent，listAll 报 idle。若不清掉那条推送状态，`seed` 不会覆盖
    // 它 → selectThread 误判"仍在跑" → 跳过 resume → 新进程里 agent 永不重建
    // → 之后发送撞 -32011 且无法自愈。这条用例钉住"重连后必须重新 resume"。
    render(<App />);
    await waitMounted();
    await flush();
    // 让 t1 进入 live-running（推送流），并把后续 listAll 改为报 idle。
    act(() => {
      for (const cb of state.notifHandlers) {
        cb({ method: "thread/status/updated", params: { thread_id: "t1", status: "running" } });
      }
    });
    state.dataSources["thread/readItems"] = () => ({ items: [] });

    // 重连之前：视图确实停在 running。
    exit();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(nextReconnectDelay(0));
    });
    await flush();

    // 新客户端上必须出现 thread/resume（而不是被陈旧 running 拦住）。
    await act(async () => {
      await vi.advanceTimersByTimeAsync(nextReconnectDelay(0));
    });
    await flush();
    expect(
      clients[1].requests.some((r) => r.method === "thread/resume"),
    ).toBe(true);
  });

```

（`state.notifHandlers`、`state.dataSources`、`act`、`vi` 在该文件里均已可用。）

- [ ] **Step 2: 运行测试确认失败**

Run:
```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run src/App.test.tsx -t "re-resumes a thread after a crash" --reporter=dot 2>&1 | tail -25
```
Expected: FAIL —— 新客户端上没有 `thread/resume`（陈旧 `live` running 让它跳过了）。

- [ ] **Step 3: 在 `exited` 处理器里调用**

在 `desktop/src/App.tsx` 中，找到 `exited` 分支（约 1475-1497 行）里的：

```ts
          warm.current.clear();
          // 同样的理由:新进程不记得任何订阅,窗口必须清空,否则重连后
          // 返回"看起来还 warm"的会话时不会再发 subscribe,内容通知被静默过滤。
          subWindow.current = new SubscriptionWindow();
```

替换为：

```ts
          warm.current.clear();
          // 推送来的运行状态同样不再成立：新进程对任何会话都没有内存 agent。
          // 不清它的话，一条崩溃前正在跑、而 `listAll` 现已报 `idle` 的会话会
          // 停在 `running`，`selectThread` 会因此跳过 resume，新进程里那个 agent
          // 永远不被重建，之后发送撞上 `-32011` 且无法自愈。详见
          // `ThreadStore.forgetPushStatuses`。
          store.forgetPushStatuses();
          // 同样的理由:新进程不记得任何订阅,窗口必须清空,否则重连后
          // 返回"看起来还 warm"的会话时不会再发 subscribe,内容通知被静默过滤。
          subWindow.current = new SubscriptionWindow();
```

- [ ] **Step 4: 运行测试确认通过**

Run:
```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run src/App.test.tsx -t "re-resumes a thread after a crash" --reporter=dot 2>&1 | tail -20
```
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent && git add desktop/src/App.tsx desktop/src/App.test.tsx && git commit -m "fix(desktop): forget pushed statuses when the sidecar dies"
```

---

### Task 4: 修正两条既有测试的等待锚点并全量收尾

这两条测试把 `resume` 当作「应用已加载」的锚点，与它们真正被测的语义（发送方法自愈）无关。

**Files:**
- Modify: `desktop/src/App.test.tsx`（`describe("App send recovery from a status disagreement")` 内两处 `waitFor`）

**Interfaces:**
- Consumes: harness 的 `state.listStatus`、`state.dataSources`。
- Produces: 无。

- [ ] **Step 1: 改第一条锚点**

在 `desktop/src/App.test.tsx` 中，找到：

```ts
  it("falls back to turn/start when turn/interject answers -32013", async () => {
    seedRunning();
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    state.rejectCode = { "turn/interject": -32013 };
```

替换为：

```ts
  it("falls back to turn/start when turn/interject answers -32013", async () => {
    seedRunning();
    render(<App />);
    // 运行中的会话不再走 resume（那会打断在跑的 turn），改用只读 readItems 补齐。
    // 等它发出即证明 listAll 的 running 状态已落进 store。
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );
    state.rejectCode = { "turn/interject": -32013 };
```

- [ ] **Step 2: 改第二条锚点**

在同一 `describe` 内，找到：

```ts
  it("does not ping-pong when both methods fail, and surfaces the error", async () => {
    seedRunning();
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    state.rejectCode = { "turn/interject": -32013, "turn/start": -32012 };
```

替换为：

```ts
  it("does not ping-pong when both methods fail, and surfaces the error", async () => {
    seedRunning();
    render(<App />);
    // 同上：运行中的会话以 readItems 补齐，不再 resume。
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );
    state.rejectCode = { "turn/interject": -32013, "turn/start": -32012 };
```

注意：这两条用例里 `seedRunning()` 会把 `listAll` 报成 running，但**没有**注册
`state.dataSources["thread/readItems"]`。harness 的 `request()` 对未注册的方法返回
`{}`，于是 `r.items` 是 `undefined`，`upsertItems(undefined)` 会在 `for (const item of items)`
处抛错——但该调用点整个包在 `try { … } catch { }` 里（`selectThread` 的
`thread/readItems` 分支），异常被静默吞掉，与「补齐失败即保留已有内容」的既有语义
一致。所以**不需要**为这两条用例额外注册 `thread/readItems`，与实测通过的版本一字不差。
（如需显式化，可选加一行 `state.dataSources["thread/readItems"] = () => ({ items: [] });`
放在 `seedRunning();` 之后，行为等价。）

- [ ] **Step 3: 全量验证**

Run:
```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/tsc --noEmit 2>&1 | tail -5 && node_modules/.bin/vitest run --reporter=dot 2>&1 | tail -8
```
Expected:
- `tsc --noEmit`：**无输出**。
- `vitest run`：`Test Files 91 passed (91)`、`Tests 1144 passed (1144)`。

若用例数不是 1144，逐一核对 Task 1/2/3 的四条新增测试是否都已加入（基线 1140 + 4）。

- [ ] **Step 4: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent && git add desktop/src/App.test.tsx && git commit -m "test(desktop): anchor the send-recovery cases on readItems, not resume"
```

---

## 完成后的验证清单

- [ ] `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/tsc --noEmit` → 无输出
- [ ] `node_modules/.bin/vitest run` → 91 文件 / 1144 测试全绿
- [ ] iOS 既有用例未改语义：`catches a cold running thread up with readItems instead of resume`、`subscribes to a thread when a remote client selects it` 仍全绿
- [ ] `git log --oneline -4` 显示本卡的 4 个提交
- [ ] 提交后走 `superpowers:finishing-a-development-branch` 呈现集成选项；**不要自己合并**