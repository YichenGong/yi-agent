# 手机端重连/重配对路径 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让手机端（iOS 远程客户端）在连不上桌面时始终有一条重新扫码/重新填表的出路，并把三处"卡住且无声"的故障改为可见、可自愈。

**Architecture:** 全部改动在 `desktop/src/`，Rust 零改动。手动入口挂在无条件渲染的状态栏；重配界面**复用** `PairingScreen`（新增可选 `onCancel`，非破坏性——提交成功才覆盖配置）；自动识别直连 4401 并进入重配；`RpcClient` 接住中继那条无 `id` 的错误帧（按帧结构判定），合成 `exited` 交给既有的退避重连回环。不新增第二套状态机。

**Tech Stack:** React 18 + TypeScript、Vitest + @testing-library/react（jsdom）、`localStorage` 持久化。

**Spec:** `docs/superpowers/specs/2026-10-17-mobile-repair-path-design.md`

## Global Constraints

- 只在 `desktop/`（前端）改动；`yi-agent-rs/` 与 `desktop/src-tauri/` 零改动。
- 桌面端行为逐字节不变；**iOS 首启配对路径逐字节不变**（QR 配对的硬不变量）。
- 重配是**非破坏性**的：绝不自动 `clearRemoteConfig`；提交成功才覆盖配置。
- 中继那条错误帧的判定按**帧结构**（`error` 有、`id` 无、`method` 无），**不按错误码**。
- 提交前 `cd desktop && npx tsc --noEmit && npx vitest run` 全绿；conventional commits；**不写 `Co-Authored-By`**。
- 每个任务在 worktree 里做，改完 `git merge --no-ff` 回 `main`。

---

### Task 1: `RpcClient` 接住无 `id` 的错误帧并合成 `exited`

**Files:**
- Modify: `desktop/src/lib/rpc.ts`（`onStatus` 附近约 124 行；`onMessage` 约 200-225 行）
- Test: `desktop/src/lib/rpc.test.ts`

**Interfaces:**
- Consumes: 既有 `Transport.onStatus(cb: (status: { state: string; code?: number | null }) => void): () => void`。
- Produces: `RpcClient.onStatus` 语义扩展——除透传 transport 状态外，还扇出自身合成的 `{ state: "exited", code: -32000 }`。**签名不变**，调用方无需改动。

**背景（写给实现者）：** 中继不解析 JSON-RPC 语义（`yi-agent-rs/crates/yi-agent-relay/src/lib.rs` 模块注释），"电脑没开"时回一帧
`{"jsonrpc":"2.0","error":{"code":-32000,"message":"no computer connected for this session"}}`
——**没有 `id`、没有 `method`**。当前 `onMessage` 要求 `id !== undefined` 才算响应、`method !== undefined` 才算通知，这帧两边都不匹配 → 被静默丢弃 → `initialize` 永久悬挂。app-server 的业务错误码段是 `-32001`~`-32025`/`-32600`+，**不含 `-32000`**，所以按 `id` 缺失判定不会误伤任何正常响应。

- [ ] **Step 1: 写失败测试**

追加到 `desktop/src/lib/rpc.test.ts` 的 `describe("RpcClient")` 内：

```ts
  it("treats an id-less error frame as a session-level failure", async () => {
    // 中继的 no-agent 错误帧：有 error、无 id、无 method。
    const { transport, emit } = fakeTransport();
    const client = new RpcClient(transport);
    const statuses: Array<{ state: string; code?: number | null }> = [];
    client.onStatus((s) => statuses.push(s));

    // 一条在途请求（就是 handshake 里的 initialize）：必须被 reject 而不是悬挂。
    const inflight = client.request("initialize", {});
    emit({
      jsonrpc: "2.0",
      error: { code: -32000, message: "no computer connected for this session" },
    });

    await expect(inflight).rejects.toMatchObject({ code: -32000 });
    expect(statuses).toEqual([{ state: "exited", code: -32000 }]);
  });

  it("reports the synthesized exit at most once", async () => {
    const { transport, emit } = fakeTransport();
    const client = new RpcClient(transport);
    const statuses: Array<{ state: string; code?: number | null }> = [];
    client.onStatus((s) => statuses.push(s));

    const frame = {
      jsonrpc: "2.0",
      error: { code: -32000, message: "no computer connected for this session" },
    };
    emit(frame);
    emit(frame);
    expect(statuses).toHaveLength(1);
  });

  it("leaves an id-carrying error response alone (no synthesized exit)", async () => {
    const { transport, emit } = fakeTransport();
    const client = new RpcClient(transport);
    const statuses: Array<{ state: string; code?: number | null }> = [];
    client.onStatus((s) => statuses.push(s));

    const p = client.request("nope", {});
    emit({ jsonrpc: "2.0", id: 1, error: { code: -32000, message: "forced" } });
    await expect(p).rejects.toMatchObject({ code: -32000 });
    // 带 id 的是普通响应拒绝：不产生合成 exited。
    expect(statuses).toEqual([]);
  });

  it("still forwards the transport's own status to subscribers", () => {
    // 既有语义不得回归：transport 的状态照常扇出，且只扇出一次。
    const { transport, emitStatus } = fakeTransport();
    const statuses: Array<{ state: string; code?: number | null }> = [];
    const client = new RpcClient(transport);
    client.onStatus((s) => statuses.push(s));
    emitStatus({ state: "exited", code: 1006 });
    expect(statuses).toEqual([{ state: "exited", code: 1006 }]);
  });
```

`fakeTransport` 顶部需补一个状态订阅集合（供上面的用例触发），并在返回对象里暴露 `emitStatus`：

```ts
  const statusListeners = new Set<(s: { state: string; code?: number | null }) => void>();
```

把它的 `onStatus: () => () => {}` 换成：

```ts
    onStatus: (cb) => {
      statusListeners.add(cb);
      return () => {
        statusListeners.delete(cb);
      };
    },
```

返回对象里加：

```ts
    emitStatus: (s: { state: string; code?: number | null }) => {
      for (const cb of [...statusListeners]) cb(s);
    },
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/lib/rpc.test.ts
```
Expected: FAIL —— 第一条 `expected [] to equal [{ state: "exited", code: -32000 }]`（无 id 帧被丢弃，`inflight` 悬挂）。

- [ ] **Step 3: 实现**

在 `rpc.ts` 的 `RpcClient` 类中，把 `onStatus` 从纯转发改为**自持订阅集合**（新增两个私有字段，加在既有 `private disposed = false;` 附近）：

```ts
  /**
   * 自身合成的状态订阅者。中继的 no-agent 错误帧没有 `id`，既不是响应也不是通知
   * （见 `onMessage`），却意味着「这条会话已经废了」——必须有一个出口把它告诉
   * App，否则 handshake 会永久悬挂。与 transport 的状态共用一个 `onStatus` 入口。
   */
  private statusHandlers = new Set<(status: { state: string; code?: number | null }) => void>();
  /** 合成 exit 只报一次：中继可能连发同类帧，重复上报只会让 App 反复重建客户端。 */
  private synthesizedExit = false;
```

`onStatus` 改为（**每个 handler 只加进本地集合**，向 transport 的订阅**只注册一次**——否则 N 个订阅者会各注册一个转发器，每条状态被投递 N 遍）：

```ts
  onStatus(handler: (status: { state: string; code?: number | null }) => void): () => void {
    this.statusHandlers.add(handler);
    // transport 的转发器只在首个订阅者时建立：逐个订阅者各建一个会把同一条
    // 状态投递 N 遍。
    this.ensureTransportStatusSubscription();
    return () => {
      this.statusHandlers.delete(handler);
    };
  }

  /** 建立（一次）transport → 本地订阅者的状态转发。 */
  private ensureTransportStatusSubscription(): void {
    if (this.unsubscribeTransportStatus) return;
    this.unsubscribeTransportStatus = this.transport.onStatus((status) => {
      if (this.disposed) return;
      for (const h of this.statusHandlers) h(status);
    });
  }
```

新增字段（与 `unsubscribeTransport` 并列）：

```ts
  /** transport 的状态转发订阅；懒建（首个 `onStatus` 订阅者）且只建一次。 */
  private unsubscribeTransportStatus: (() => void) | null = null;
```

`dispose` 中追加（放在 `this.unsubscribeTransport();` 之后）：

```ts
    this.unsubscribeTransportStatus?.();
    this.unsubscribeTransportStatus = null;
    this.statusHandlers.clear();
```

`onMessage` 开头（在既有的响应/通知分支**之前**）插入新分支：

```ts
    // 中继的 no-agent 错误帧：有 `error`、无 `id`、无 `method`。
    // 按**帧结构**判定而非错误码：中继不解析 JSON-RPC 语义（relay/src/lib.rs），
    // 它发出的错误帧天然没有 `id`；而 app-server 的业务错误码段
    // （-32001~-32025、-32600+）不含 -32000，且一律带 `id`——按 id 判不会误伤。
    if (msg.id === undefined && msg.method === undefined && msg.error !== undefined) {
      // 在途请求全部失败：`initialize` 因此不再悬挂。
      const error = msg.error;
      for (const [, pending] of this.pending) pending.reject(error);
      this.pending.clear();
      this.buffered.clear();
      if (!this.synthesizedExit) {
        this.synthesizedExit = true;
        for (const h of this.statusHandlers) h({ state: "exited", code: error.code });
      }
      return;
    }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/lib/rpc.test.ts
```
Expected: PASS（含既有全部用例）。

- [ ] **Step 5: 提交**

```bash
cd /path/to/worktree && git add desktop/src/lib/rpc.ts desktop/src/lib/rpc.test.ts && git commit -m "fix(desktop): 接住中继无 id 的错误帧，不再让握手永久悬挂"
```

---

### Task 2: `handshake` 失败时排重连

**Files:**
- Modify: `desktop/src/App.tsx`（`handshake` 的 catch，约 1476-1480 行）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: 既有局部函数 `scheduleReconnect()`（定义在同 effect 内、`handshake` 之前）。
- Produces: 无新接口——纯行为修复。

**背景：** 现在 `handshake` 的 catch 只 `setStatus(\`error: …\`)`，**不排重连**；配合 Task 1（无 id 帧现在会让 `initialize` 以错误失败），若不补这条，握手失败仍会永久卡住。

- [ ] **Step 1: 写失败测试**

追加到 `desktop/src/App.test.tsx` 的 `describe("App reconnect resilience")` 内（它已有 `flush` / `waitMounted` / `beforeEach` 的假定时器）：

```ts
  it("schedules a reconnect when the handshake itself fails", async () => {
    // 首连接握手失败（如经中继时电脑未连接）：必须退避重试，而不是停在 error。
    state.rejectCode["initialize"] = -32000;
    render(<App />);
    await flush();
    expect(clients.length).toBe(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(nextReconnectDelay(1));
    });
    expect(clients.length).toBe(2);
    await flush();
    expect(clients[1].requests.some((r) => r.method === "initialize")).toBe(true);
  });
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/App.test.tsx -t "handshake itself fails"
```
Expected: FAIL —— `expected 1 to be 2`（没有第二次连接）。

- [ ] **Step 3: 实现**

`App.tsx` 的 `handshake` catch 改为：

```ts
      } catch (e) {
        const msg = formatError(e);
        setCurrentError(msg);
        setStatus(`error: ${msg}`);
        // 握手失败同样是"没连上"：必须排队重试，否则会永久停在 error。
        // （经中继时"电脑未连接"正是这条路径——见 rpc.ts 的无 id 错误帧。）
        scheduleReconnect();
      }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/App.test.tsx
```
Expected: PASS（既有 reconnect 用例不回归）。

- [ ] **Step 5: 提交**

```bash
cd /path/to/worktree && git add desktop/src/App.tsx desktop/src/App.test.tsx && git commit -m "fix(desktop): 握手失败也要排重连，不再永久停在 error"
```

---

### Task 3: `StatusBar` 增加可选「重新配对」入口

**Files:**
- Modify: `desktop/src/components/StatusBar.tsx`
- Test: `desktop/src/components/StatusBar.test.tsx`

**Interfaces:**
- Produces: `StatusBar` 新增可选 prop `onRepair?: () => void`；**传入才渲染**按钮（`aria-label="重新配对"`）。不传时 DOM 与现在一致。

- [ ] **Step 1: 写失败测试**

追加到 `desktop/src/components/StatusBar.test.tsx`：

```tsx
  it("offers a re-pair entry only when the host passes one", () => {
    const { rerender } = render(<StatusBar cwd="/w" status="connecting" usage={null} />);
    expect(screen.queryByRole("button", { name: "重新配对" })).toBeNull();

    rerender(<StatusBar cwd="/w" status="connecting" usage={null} onRepair={() => {}} />);
    expect(screen.getByRole("button", { name: "重新配对" })).toBeTruthy();
  });

  it("invokes the re-pair callback when clicked", () => {
    const onRepair = vi.fn();
    render(<StatusBar cwd="/w" status="connecting" usage={null} onRepair={onRepair} />);
    fireEvent.click(screen.getByRole("button", { name: "重新配对" }));
    expect(onRepair).toHaveBeenCalledTimes(1);
  });
```

并把该文件顶部的 import 补上 `vi` 与 `fireEvent`：

```tsx
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/StatusBar.test.tsx
```
Expected: FAIL —— `Unable to find an accessible element with the role "button" and name "重新配对"`。

- [ ] **Step 3: 实现**

`StatusBar.tsx`：props 增加 `onRepair`，并在 `actions` 之前渲染按钮（放 `ml-auto` 那组里、`usage` 之后）：

```tsx
export function StatusBar({
  cwd,
  status,
  usage,
  actions,
  onRepair,
}: {
  cwd: string | null;
  status: string;
  usage: Usage | null;
  /**
   * 追加在右端的控件（用量之后）。给「重新打开某个可收起面板」这类常驻入口用：
   * 入口若跟着面板一起收起，就再也没有东西能把它打开。
   */
  actions?: ReactNode;
  /**
   * 手机端「重新配对」入口；**只在传入时渲染**。
   *
   * 由宿主按连接状态决定是否传入（远程客户端且未连接）。做成状态栏自己的语义
   * 而不是塞进通用的 `actions`：它的可见性由连接状态决定，与状态栏的职责同源。
   */
  onRepair?: () => void;
}) {
```

在 `{actions}` 之前插入：

```tsx
        {onRepair && (
          <button
            type="button"
            onClick={onRepair}
            className="rounded border border-line-strong px-2 py-0.5 text-fg-muted hover:text-fg"
          >
            重新配对
          </button>
        )}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/components/StatusBar.test.tsx && npx tsc --noEmit
```
Expected: PASS + tsc exit 0。

- [ ] **Step 5: 提交**

```bash
cd /path/to/worktree && git add desktop/src/components/StatusBar.tsx desktop/src/components/StatusBar.test.tsx && git commit -m "feat(desktop): 状态栏新增「重新配对」入口"
```

---

### Task 4: `PairingScreen` 增加可选 `onCancel`

**Files:**
- Modify: `desktop/src/components/PairingScreen.tsx`
- Test: `desktop/src/components/PairingScreen.test.tsx`

**Interfaces:**
- Produces: `PairingScreenProps` 新增可选 `onCancel?: () => void`；**传入才渲染**「返回」按钮。不传时 DOM 与现在**完全一致**（守首启不变量）。

- [ ] **Step 1: 写失败测试**

追加到 `desktop/src/components/PairingScreen.test.tsx`：

```tsx
describe("PairingScreen optional cancel", () => {
  it("renders no cancel button on the first-launch path", () => {
    // 首启不传 `onCancel`：DOM 必须与从前一致（QR 配对的硬不变量）。
    render(<PairingScreen redeem={vi.fn()} storage={storages()} />);
    expect(screen.queryByRole("button", { name: "返回" })).toBeNull();
  });

  it("renders a working cancel button when the host passes one", () => {
    const onCancel = vi.fn();
    render(<PairingScreen redeem={vi.fn()} storage={storages()} onCancel={onCancel} />);
    fireEvent.click(screen.getByRole("button", { name: "返回" }));
    expect(onCancel).toHaveBeenCalledTimes(1);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/PairingScreen.test.tsx -t "optional cancel"
```
Expected: FAIL —— 找不到「返回」按钮。

- [ ] **Step 3: 实现**

`PairingScreen.tsx`：`PairingScreenProps` 增加字段（放在 `onPaired` 附近）：

```ts
  /** 凭据落盘后回调——宿主据此把 transport 重算成 ws。 */
  onPaired?: () => void;
  /**
   * 重配场景的「返回」；**只在传入时渲染**。
   *
   * 首启配对无处可去（iOS 上没有 Tauri IPC），故那时不传、不渲染——首启路径
   * 的 DOM 因此逐字节不变。已配对的手机进这里则是"再看一眼凭据"，退出该是自由的。
   */
  onCancel?: () => void;
```

解构参数处加 `onCancel,`；在提交按钮**之后**、扫码按钮之前插入：

```tsx
        {onCancel && (
          <button
            type="button"
            onClick={onCancel}
            className="mt-3 w-full rounded-md border border-line-strong px-3 py-2 text-sm text-fg-muted hover:text-fg"
          >
            返回
          </button>
        )}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/components/PairingScreen.test.tsx
```
Expected: PASS（既有 17 项不回归）。

- [ ] **Step 5: 提交**

```bash
cd /path/to/worktree && git add desktop/src/components/PairingScreen.tsx desktop/src/components/PairingScreen.test.tsx && git commit -m "feat(desktop): PairingScreen 支持可选「返回」（首启路径不变）"
```

---

### Task 5: App 接入——手动入口、重配界面、重连重建

**Files:**
- Modify: `desktop/src/App.tsx`
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: `StatusBar.onRepair`（Task 3）、`PairingScreen.onCancel`（Task 4）、既有 `defaultRedeem`。
- Produces: App 内部 state `repairing`；渲染优先级（1 首启配对 → 2 重配 → 3 主界面）。后续 Task 6 复用同一 state 做自动进入。

- [ ] **Step 1: 写失败测试**

追加到 `desktop/src/App.test.tsx`（新 `describe`，与 `App iOS first-launch pairing` 同级）：

```tsx
describe("App mobile repair path", () => {
  const IPHONE_UA =
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";
  const RELAY = "wss://relay.test/ws?session=abc";

  /** 已配对的手机：持久化一份配置，退到离线态（未 connected）。 */
  const pairedPhone = () => {
    state.platformUa = IPHONE_UA;
    localStorage.setItem("yi-agent.remote", JSON.stringify({ url: RELAY, token: "yia_tok" }));
    // 悬停 initialize，让握手停在 connecting：本套用例要的是「已配对但连不上」，
    // 若握手立即成功，status 会变成 connected、`offline` 为 false，状态栏入口
    // 就不该出现，用例会因错误的原因失败。写法与既有用例一致（见 App.test.tsx
    // 里 `state.dataSources["initialize"] = () => new Promise(() => {})`）。
    state.dataSources["initialize"] = () => new Promise(() => {});
  };

  it("shows a re-pair entry in the status bar while offline", async () => {
    pairedPhone();
    render(<App />);
    // 未握手成功 = 离线：状态栏出现「重新配对」。
    expect(await screen.findByRole("button", { name: "重新配对" })).toBeTruthy();
  });

  it("opens the pairing screen prefilled with the persisted url", async () => {
    pairedPhone();
    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: "重新配对" }));

    expect(screen.getByRole("form", { name: "配对" })).toBeTruthy();
    expect((screen.getByLabelText("服务器地址") as HTMLInputElement).value).toBe(RELAY);
  });

  it("returns to the main view without clearing the persisted config", async () => {
    pairedPhone();
    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: "重新配对" }));
    fireEvent.click(screen.getByRole("button", { name: "返回" }));

    expect(screen.queryByRole("form", { name: "配对" })).toBeNull();
    expect(localStorage.getItem("yi-agent.remote")).not.toBeNull();
  });

  it("rebuilds the connection after a successful re-pair", async () => {
    pairedPhone();
    const opened: string[] = [];
    class FakeSocket {
      onopen: (() => void) | null = null;
      onmessage: ((e: { data: unknown }) => void) | null = null;
      onclose: ((e: { code: number }) => void) | null = null;
      onerror: unknown = null;
      constructor(readonly url: string) {
        opened.push(url);
        queueMicrotask(() => this.onopen?.());
      }
      send(raw: string) {
        const frame = JSON.parse(raw) as { id?: number };
        if (frame.id === 1) {
          queueMicrotask(() =>
            this.onmessage?.({ data: JSON.stringify({ jsonrpc: "2.0", id: 1, result: {} }) }),
          );
        } else if (frame.id === 2) {
          queueMicrotask(() =>
            this.onmessage?.({
              data: JSON.stringify({
                jsonrpc: "2.0",
                id: 2,
                result: { device_id: "dev-2", token: "yia_new", scope: "control" },
              }),
            }),
          );
        }
      }
      close() {}
    }
    vi.stubGlobal("WebSocket", FakeSocket);
    try {
      render(<App />);
      await flush();
      expect(clients.length).toBe(1);
      fireEvent.click(await screen.findByRole("button", { name: "重新配对" }));
      fireEvent.change(screen.getByLabelText("配对码"), { target: { value: "ABCD-EFGH" } });
      fireEvent.click(screen.getByRole("button", { name: "配对" }));

      // 重配成功必须重建连接：新的 client 发 initialize（旧守卫 inited.current 已放开）。
      await waitFor(() => expect(clients.length).toBe(2));
      expect(clients[1].requests.some((r) => r.method === "initialize")).toBe(true);
      expect(opened[0]).toBe(RELAY);
      expect(JSON.parse(localStorage.getItem("yi-agent.remote")!).token).toBe("yia_new");
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("never offers the entry on the desktop", async () => {
    render(<App />);
    await flush();
    expect(screen.queryByRole("button", { name: "重新配对" })).toBeNull();
  });
});
```

> 该 `describe` 用到 `flush` / `nextReconnectDelay` / `clients` / `state`；`flush` 是
> `App reconnect resilience` 内的局部函数，故此 `describe` 里另写一份（见 Step 3 旁注）
> 或改用 `await act(async () => { await vi.advanceTimersByTimeAsync(0); })` 重复三次。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/App.test.tsx -t "mobile repair path"
```
Expected: FAIL —— 找不到「重新配对」按钮。

- [ ] **Step 3: 实现**

在 `App.tsx` 中：

1. 新 state（放在 `pairingVersion` 附近）：

```ts
  // 手机端「重新配对」：true 时全屏呈现配对界面。**非破坏性**——不动已存配置，
  // 提交成功才覆盖；点「返回」即回原离线态。首启配对走 `needsPairing`，与此独立。
  const [repairing, setRepairing] = useState(false);
```

2. 渲染分支改为（首启优先，其次重配）：

```tsx
  if (needsPairing(isIos())) {
    return (
      <PairingScreen
        onPaired={() => setPairingVersion((v) => v + 1)}
        redeem={defaultRedeem}
      />
    );
  }

  if (repairing) {
    return (
      <PairingScreen
        // 预填当前已存地址，用户可改可重扫；失败不动配置（非破坏性）。
        initialUrl={
          storedRemoteConfig(
            (globalThis.localStorage as StorageLike | undefined) ?? emptyStorageLike,
          )?.url
        }
        onCancel={() => setRepairing(false)}
        onPaired={() => {
          // 已配对过时 `inited.current` 仍是 true，单靠版本号会被守卫挡掉；
          // 必须先放开守卫，让旧 effect 走 cleanup、新 effect 真正建连。
          inited.current = false;
          setRepairing(false);
          setPairingVersion((v) => v + 1);
        }}
        redeem={defaultRedeem}
      />
    );
  }
```

配套 import：`import { type StorageLike, storedRemoteConfig } from "./lib/remoteConfig";`，并加一个本地空 storage 常量（与 `platform.ts` 同形）：

```ts
/** 读不到全局 storage 时的空实现（隐私模式等），与 `lib/platform.ts` 同形。 */
const emptyStorageLike: StorageLike = { getItem: () => null, setItem: () => {}, removeItem: () => {} };
```

3. `StatusBar` 调用处传入 `onRepair`（用已有的 `offline` 判定，`App.tsx:200`）：

```tsx
            onRepair={offline ? () => setRepairing(true) : undefined}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/App.test.tsx && npx tsc --noEmit
```
Expected: PASS + tsc exit 0。

- [ ] **Step 5: 提交**

```bash
cd /path/to/worktree && git add desktop/src/App.tsx desktop/src/App.test.tsx && git commit -m "feat(desktop): 手机端离线时可重新配对，成功后重建连接"
```

---

### Task 6: 直连 4401 自动进入重配

**Files:**
- Modify: `desktop/src/App.tsx`（`client.onStatus` 回调内）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: Task 5 的 `setRepairing` / `repairing`。
- Produces: 无新接口。新增常量 `REPAIR_REMOVED_TEXT = "此设备已被桌面移除，请重新配对"`（供 Task 6 测试与后续文案复用）。

**背景：** 4401 只在**直连**路径出现（中继不校验手机 token），代表"服务端明确拒绝这个 token"，重试无意义。

**守卫语义（务必写对）：** `setStatus(s.state)` 会先把状态置为 `exited`，所以**不能**用 `status !== "connected"` 做守卫（它此刻恒为真，守卫失效）；而且 effect 闭包里的 `status` 是创建时的旧快照，读它同样不可靠。真正要守的是"不要重复进入重配界面"，即用 ref 判断 `repairing` 当前值。另外要**忽略自己发起的关闭**：`App.tsx` 的 `exited` 分支会 `client.dispose()`，而 `wsTransport.dispose()` 刻意不上报 `exited`（见 `wsTransport.ts` 注释），故不会自我触发；但切前后台重建客户端时旧 client 的 `dispose` 亦不上报，同样安全。

- [ ] **Step 1: 写失败测试**

追加到 `describe("App mobile repair path")`：

```tsx
  it("opens the pairing screen automatically when the server rejects the token", async () => {
    pairedPhone();
    render(<App />);
    await flush();
    // 服务端以 4401 明确拒绝这个 token（ws.rs 的 close_unauthorized）：
    // 重试无意义，直接送上重配界面并说明原因。
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 4401 });
    });
    expect(await screen.findByRole("form", { name: "配对" })).toBeTruthy();
    expect(screen.getByText(REPAIR_REMOVED_TEXT)).toBeTruthy();
  });

  it("leaves an ordinary disconnect to the backoff loop (no repair screen)", async () => {
    pairedPhone();
    render(<App />);
    await flush();
    // 普通断线（1006 等）：这是"网络抖了一下"，必须继续退避重连，不弹重配。
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 1006 });
    });
    expect(screen.queryByRole("form", { name: "配对" })).toBeNull();
  });
```

并把 `REPAIR_REMOVED_TEXT` 从 `App.tsx` 导入（该测试文件已 import App）：

```tsx
import App, { REPAIR_REMOVED_TEXT } from "./App";
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/App.test.tsx -t "server rejects the token"
```
Expected: FAIL —— 找不到 `配对` 表单。

- [ ] **Step 3: 实现**

`App.tsx`：导出常量（放组件外、文件上部）：

```ts
/** 凭据被服务端明确拒绝（直连 4401）时的文案；与「配对码无效」区分开。 */
export const REPAIR_REMOVED_TEXT = "此设备已被桌面移除，请重新配对";
```

在 `client.onStatus` 回调里，`setStatus(s.state)` 之后加入：

```ts
        setStatus(s.state);
        // 直连时服务端用 4401 明确拒绝这个 token（ws.rs 的 close_unauthorized）：
        // 重试一万次也不会有别的结局，直接把用户送上重配界面。中继路径不会出现
        // 4401（中继不校验手机 token），故该分支只在直连生效。
        //
        // 守卫只用 ref 判断"是否已在重配界面"：`setStatus` 刚把状态置为 exited，
        // 用 status 判断此刻恒真；闭包里的 status 也是旧快照。普通断线（1006 等）
        // 不在此分支，继续走既有退避重连。
        if (isRemoteClient() && s.code === 4401 && !repairingRef.current) {
          setRepairError(REPAIR_REMOVED_TEXT);
          setRepairing(true);
        }
```

配套：新 state `const [repairError, setRepairError] = useState<string | null>(null);` 与 ref
`const repairingRef = useRef(false);`（在 effect 内读 state 会拿到陈旧闭包，故用 ref），
并保持 ref 与 state 同步：

```ts
  // effect 闭包读不到最新 state，用 ref 判断「是否已在重配界面」，避免重复进入。
  repairingRef.current = repairing;
```

`PairingScreen` 重配分支传入该文案。`PairingScreen` 目前把错误写在内部 state 里，
故改为**顶部展示**由宿主给的提示（新增可选 prop 见下）：

```tsx
  if (repairing) {
    return (
      <PairingScreen
        initialUrl={...}
        notice={repairError ?? undefined}
        onCancel={() => {
          setRepairing(false);
          setRepairError(null);
        }}
        onPaired={() => {
          inited.current = false;
          setRepairing(false);
          setRepairError(null);
          setPairingVersion((v) => v + 1);
        }}
        redeem={defaultRedeem}
      />
    );
  }
```

`PairingScreen` 增加可选 `notice?: string`，渲染在标题下方（不传时 DOM 不变）：

```tsx
        <h1 className="text-sm font-medium text-fg">连接桌面</h1>
        <p className="mt-2 text-xs text-fg-subtle">
          在桌面端「远程访问」生成配对码，把中继地址和码填到这里。
        </p>
        {notice !== undefined && (
          <p role="status" className="mt-2 text-xs text-amber-400">
            {notice}
          </p>
        )}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/App.test.tsx src/components/PairingScreen.test.tsx && npx tsc --noEmit
```
Expected: PASS + tsc exit 0。

- [ ] **Step 5: 提交**

```bash
cd /path/to/worktree && git add desktop/src/App.tsx desktop/src/components/PairingScreen.tsx desktop/src/App.test.tsx desktop/src/components/PairingScreen.test.tsx && git commit -m "feat(desktop): 凭据被拒（4401）时自动进入重新配对"
```

---

## 收尾

- 全量门禁：`cd desktop && npx tsc --noEmit && npx vitest run`（基线 1047 项，只增不改）。
- 更新 `docs/project-management/desktop.md`：在远程客户端相关条目补一行"手机端重连/重配对路径"，判据写本计划的验收命令。
- 更新 `README.md` 模块索引表计数（若 desktop 条目计数变化）。
- 每个任务各自 `git merge --no-ff` 回 `main` 后删分支；全部完成后移除 worktree。

## Self-Review 记录

- **Spec 覆盖**：§3.1→Task 3+5；§3.2→Task 4+5；§3.3→Task 6；§3.4→Task 1+2；§3.5→Task 5 Step 3（`inited.current = false`）；§5 测试策略 1-5→Task 3/4/5/1/2 各步。
- **类型一致**：`onRepair?: () => void`、`onCancel?: () => void`、`notice?: string`、`repairing`/`repairError`/`repairingRef`、`REPAIR_REMOVED_TEXT` 在各任务间同名同型。
- **注意项**：Task 6 引入的 `notice` prop 是对 Task 4 的 `PairingScreen` 接口的追加——Task 4 只加 `onCancel`，故 Task 6 必须显式再加 `notice`（已在 Task 6 Step 3 写全）。
- **对 spec 的一处偏离（有意）**：spec §3.3 写"限定 `status !== "connected"` 才自动弹"，但写计划时核实发现该守卫**不可实现**——`setStatus(s.state)` 会先把状态置为 `exited`（守卫恒真），且 effect 闭包里的 `status` 是旧快照。故计划改为：守卫只判"是否已在重配界面"（`repairingRef`），并以**关闭码是否为 4401** 区分"凭据被拒"（弹）与"普通断线"（不弹、继续退避）。语义结果与 spec 一致（正常使用时不会抢焦点——因为 4401 本身就意味着连接已断），实现方式更可靠。