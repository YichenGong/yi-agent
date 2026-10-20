/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor, cleanup, act, within } from "@testing-library/react";

type Mode = "normal" | "yolo";
type ThreadSeed = {
  thread_id: string;
  title: string | null;
  permission_mode: Mode;
  driver_project?: string;
  driver_ref?: string;
};

const { clients, state, dragDrop } = vi.hoisted(() => ({
  clients: [] as Array<{
    requests: { method: string; params: unknown }[];
    // The recovery hook the App injects as `RpcClient`'s 2nd constructor arg.
    // A test invokes it to prove the wiring: after a relay bridge restart the
    // App re-handshakes the *current* client (see the "relay handshake
    // recovery" suite). Its arg is the raw request fn the real client passes.
    recover?: ((raw: (m: string, a: unknown) => Promise<unknown>) => Promise<void>) | null;
  }>,
  state: {
    mode: "normal" as Mode,
    failSet: false,
    failList: false,
    // When set, `thread/listAll` succeeds for the first `failListAfter` calls
    // and throws afterwards. Used to fail only the *post-resume* lookup while
    // letting the mount-time listing succeed.
    failListAfter: null as number | null,
    // `null` → fall back to a single t1 whose mode follows `state.mode`
    // (keeps the legacy tests terse). Tests that need a second thread or
    // per-thread modes set this explicitly.
    threads: null as ThreadSeed[] | null,
    notifHandlers: [] as Array<(n: unknown) => void>,
    approvalHandlers: [] as Array<(r: unknown) => void>,
    // Status callbacks the App registers on each client; a test fires an
    // "exited" status to simulate a sidecar crash + host restart.
    statusHandlers: [] as Array<(s: unknown) => void>,
    // Method -> error code. Any method can be forced to reject, so a test can
    // force the `-32013` / `-32012` disagreement the server can report — or a
    // `thread/clear` rejection (`-32012`, the accepted post-turn window).
    rejectCode: {} as Record<string, number>,
    // Status reported by `thread/listAll` for every seeded thread.
    listStatus: "idle" as "idle" | "running" | "awaiting_approval",
    // 顶层 pinned 列表（thread_id 顺序即渲染顺序）。服务端把已置顶的会话
    // 同时留在它自己的分组内（带 pinned:true）并在这个顶层数组里重复一份。
    pinnedIds: [] as string[],
    // Paths the native file picker hands back, in order. `null` = cancelled.
    // Tests push what they need; an empty queue resolves to `null`. An entry
    // that is itself an array models a **multi-select** `open()` (the document
    // picker in Task 10), which returns every chosen path in one call.
    picks: [] as (string | string[] | null)[],
    // Per-project 看板 (Task 7): what `resident/list` reports as registered, and
    // what the plugin's `list` answers with.
    boards: [] as unknown,
    cards: [] as unknown[],
    // Workspace groups `thread/listAll` reports. `null` → a single "/w", which
    // is what the pre-board tests assume. Board tests set the real project
    // paths, since the sidebar only offers a board row for a group it lists.
    groupWorkspaces: null as string[] | null,
    // 会话分组：`thread/listAll` 把它们挂到同 workspace 的分组上。
    workspaceGroups: [] as Array<{
      workspace: string;
      id: string;
      name: string;
      description: string;
      threadIds: string[];
    }>,
    // Rejections keyed by RPC method, or by the inner `plugin/query` method
    // ("switch.write", "list", …) so a test can fail exactly one board call.
    rpcError: {} as Record<string, unknown>,
    // Same shape as the host emits around Task 7: the structured board code
    // rides in `data.code`, with only human wording in `message`.
    hostError: {} as Record<string, { code: number; message: string; data?: unknown }>,
    // Per-test RPC override: method -> a data source for its response. Lets a
    // test hand back a pending promise so an in-flight request (e.g. the startup
    // theme read) can be interleaved with a notification or user action. Takes
    // precedence over the canned responses below.
    dataSources: {} as Record<string, (params: unknown) => unknown>,
    // UA the mocked `platform.isIos()` answers from. Empty → desktop, so every
    // pre-existing test shape keeps its behavior; the iOS pairing test sets a
    // phone UA and persists a config where it needs one.
    platformUa: "",
    // `process/list` 返回的进程;默认空,需要时由用例填充。
    processes: [] as Array<Record<string, unknown>>,
    // 已补建组长的目录（`thread/lead` 被调用过）。`thread/listAll` 据此在这些
    // 分组里多报一条 `role: "lead"` 的会话——模拟服务端真的落盘了组长 meta。
    // 默认空：既有用例的分组里没有组长，与改动前逐字节一致。
    leadCwds: [] as string[],
    // 让 `thread/lead` 抛错，用于验证「单个目录补建失败不挡首屏」。
    failLead: false,
    // 每次 `thread/lead` 调用都记一笔它请求的 cwd——即便失败也记，这样「失败
    // 路径确实被走到」可被断言，而不是靠「首屏照常」这种两边都成立的弱断言。
    leadAttempts: [] as string[],
    // `flags/list` 里 `lead_agent` 的值。默认**开**：既有用例（补建组长、
    // 首屏渲染组长条目）测的都是"开关打开时的行为"，缺省成关会让它们全部
    // 变成在测另一条分支。
    leadFlagEnabled: true,
  },
  // 捕获 `@tauri-apps/api/webview` 注册的拖放 handler，供用例驱动事件。
  dragDrop: { fn: null as null | ((event: unknown) => void) },
}));

// 内容缓存模块级替换：每个用例一份内存实现。App 在首次远端渲染时才懒建，故用
// `??=` 记录「本次用例创建的那个」，用例既可读它、也可在 `beforeEach` 里清空
// 后由用例自己塞一份预置内容（见 "App mobile local cache"）。
const { contentStore } = vi.hoisted(() => ({
  contentStore: { current: null as null | import("./lib/contentStore").AsyncContentStore },
}));
vi.mock("./lib/contentStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./lib/contentStore")>();
  return {
    ...actual,
    // One instance per test: the App grabs it lazily on first remote render.
    createContentStore: () => (contentStore.current ??= new actual.MemoryContentStore()),
  };
});

// The board panel's enqueue control opens the native picker twice. Stub it at
// the module boundary the component imports lazily.
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: async () => state.picks.shift() ?? null,
}));

vi.mock("./lib/rpc", () => ({
  RpcClient: class {
    requests: { method: string; params: unknown }[] = [];
    listCalls = 0;
    // The recovery hook the App passes as the 2nd ctor arg (see the
    // "relay handshake recovery wiring" suite).
    recover: ((raw: (m: string, a: unknown) => Promise<unknown>) => Promise<void>) | null = null;
    // Callbacks this instance registered, so `dispose` can remove exactly them —
    // mirroring the real client tearing down its transport subscription.
    private registered: Array<{ list: unknown[]; cb: unknown }> = [];
    constructor(
      _transport?: unknown,
      recover?: (raw: (m: string, a: unknown) => Promise<unknown>) => Promise<void>,
    ) {
      clients.push(this);
      if (recover) this.recover = recover;
    }
    private track(list: unknown[], cb: unknown) {
      this.registered.push({ list, cb });
      list.push(cb);
      return () => {
        const i = list.indexOf(cb);
        if (i >= 0) list.splice(i, 1);
      };
    }
    onNotification(cb: (n: unknown) => void) {
      return this.track(state.notifHandlers, cb);
    }
    onApproval(cb: (r: unknown) => void) {
      return this.track(state.approvalHandlers, cb);
    }
    onStatus(cb: (s: unknown) => void) {
      return this.track(state.statusHandlers, cb);
    }
    dispose() {
      for (const { list, cb } of this.registered) {
        const i = list.indexOf(cb);
        if (i >= 0) list.splice(i, 1);
      }
      this.registered.length = 0;
    }
    async request(method: string, params: unknown) {
      this.requests.push({ method, params });
      const forced = state.rejectCode[method];
      if (forced !== undefined) throw { code: forced, message: `forced ${forced}` };
      // Per-test override hook — see `state.dataSources`. Its return value is
      // awaited by callers, so returning a pending promise defers the response.
      const dataSource = state.dataSources[method];
      if (dataSource) return dataSource(params);
      if (method === "thread/listAll") {
        this.listCalls += 1;
        if (state.failList) throw { code: -1, message: "list failed" };
        if (state.failListAfter !== null && this.listCalls > state.failListAfter) {
          throw { code: -1, message: "list failed" };
        }
        const seeds =
          state.threads ??
          [{ thread_id: "t1", title: "one", permission_mode: state.mode }];
        const workspaces = state.groupWorkspaces ?? ["/w"];
        const leadEntries = (workspace: string) =>
          state.leadCwds.includes(workspace)
            ? [
                {
                  thread_id: `lead-${workspace}`,
                  cwd: workspace,
                  model: "m",
                  created_at: 0,
                  updated_at: 0,
                  title: "组长",
                  role: "lead",
                  status: "idle",
                  pinned: false,
                },
              ]
            : [];
        return {
          groups: workspaces.map((workspace, index) => ({
            workspace,
            exists: true,
            threads:
              index === 0
                ? [
                    ...seeds.map((t) => ({
                      thread_id: t.thread_id,
                      cwd: workspace,
                      model: "m",
                      created_at: 0,
                      updated_at: 0,
                      title: t.title,
                      permission_mode: t.permission_mode,
                      status: state.listStatus,
                      pinned: state.pinnedIds.includes(t.thread_id),
                      driver_project: t.driver_project,
                      driver_ref: t.driver_ref,
                    })),
                    ...leadEntries(workspace),
                  ]
                : leadEntries(workspace),
            groups: state.workspaceGroups
              .filter((g) => g.workspace === workspace)
              .map((g) => ({
                id: g.id,
                name: g.name,
                description: g.description,
                threadIds: g.threadIds,
              })),
          })),
          // 置顶项照旧留在分组里（服务端为向后兼容不摘除），顶层这里只是重排一份。
          pinned: state.pinnedIds.map((id) => ({
            thread_id: id,
            cwd: "/w",
            model: "m",
            created_at: 0,
            updated_at: 0,
            // 与分组内该 thread 的 seed 保持一致：summary 是同一个 thread 的
            // 两种视图，title/permission_mode 不一致会让测试验证到假行为。
            title: (seeds.find((s) => s.thread_id === id)?.title ?? null),
            permission_mode: seeds.find((s) => s.thread_id === id)?.permission_mode,
            pinned: true,
          })),
        };
      }
      if (method === "workspace/list") return { workspaces: [] };
      if (method === "flags/list") {
        return {
          flags: [
            {
              name: "lead_agent",
              summary: "组长",
              enabled: state.leadFlagEnabled,
              expires_on: "2027-01-31",
              source: state.leadFlagEnabled ? "file" : "default",
            },
          ],
        };
      }
      if (method === "thread/lead") {
        const cwd = (params as { cwd?: string } | undefined)?.cwd;
        if (typeof cwd === "string") state.leadAttempts.push(cwd);
        if (state.failLead) throw { code: -1, message: "lead failed" };
        if (typeof cwd === "string" && !state.leadCwds.includes(cwd)) {
          state.leadCwds.push(cwd);
        }
        return { thread_id: `lead-${cwd ?? "default"}`, created: true };
      }
      if (method === "thread/resume") {
        const { threadId } = params as { threadId: string };
        const seeds = state.threads ?? [{ thread_id: "t1", title: "one", permission_mode: state.mode }];
        // 服务端现在的 resume 响应直接带权威 permission_mode（修复的一部分）。
        return {
          thread_id: threadId,
          cwd: "/w",
          model: "m",
          permission_mode: seeds.find((t) => t.thread_id === threadId)?.permission_mode,
        };
      }
      if (method === "thread/start") {
        const id = `new-${this.requests.length}`;
        return { thread_id: id, cwd: "/w", model: "m", permission_mode: "normal" };
      }
      if (method === "ui/settings/read") return { theme: "dark", resident_daemon_watchman_enabled: true };
      if (method === "ui/settings/write") return { ok: true };
      if (method === "thread/setPermissionMode") {
        if (state.failSet) throw { code: -32011, message: "unknown thread" };
        return {};
      }
      if (method === "thread/setReasoningEffort") {
        return { ok: true, effort: (params as { effort: string | null }).effort };
      }
      if (method === "thread/clear") return {};
      if (method === "thread/compact") return { status: "compacted" };
      if (method === "process/list") {
        return { processes: state.processes };
      }
      if (method === "process/read") {
        return {
          process_id: (params as { process_id: string }).process_id,
          name: "dev",
          stdout: "ready on 3000\n",
          stderr: "",
          next_cursor: 0,
          truncated: false,
          status: { state: "running" },
          ready: false,
        };
      }
      if (method === "process/kill") return { ok: true };
      if (method === "resident/list") return { projects: state.boards };
      if (method === "plugin/query") {
        const inner = (params as { method?: string }).method ?? "";
        const failure = state.hostError[inner] ?? state.rpcError[inner];
        if (failure !== undefined) throw failure;
        if (inner === "list") return { cards: state.cards };
        if (inner === "switch.read") return { on: true, source: "project" };
        return {};
      }
      return {};
    }
  },
}));

// 拖放订阅走动态 import 的 `@tauri-apps/api/webview`；在模块边界桩掉它，
// 并把注册的 handler 记在 `dragDrop.fn` 上。Tauri 交来的是事件对象
// （`{ payload }`），故这里回放时也要包一层——`App` 的 subscribe 只转发 payload。
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: async (handler: (event: unknown) => void) => {
      dragDrop.fn = handler;
      return () => {};
    },
  }),
}));

/** jsdom 的 rect 恒为 0，落点判定需要一个真矩形。 */
function stubRect(el: HTMLElement, r: { left: number; top: number; right: number; bottom: number }) {
  el.getBoundingClientRect = () =>
    ({ ...r, width: r.right - r.left, height: r.bottom - r.top, x: r.left, y: r.top, toJSON: () => ({}) }) as DOMRect;
}

vi.mock("./transportFactory", () => ({ transportFactory: () => ({}) }));

// iOS-vs-desktop 的唯一判定入口。只换掉 `isIos`：其余（`isRemoteClient` 等）
// 保持真身，否则会连带打断既有的远端分支用例。UA 由 `state.platformUa` 摆布：
// 空串即桌面，所以既有用例一条都不改道。
vi.mock("./lib/platform", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./lib/platform")>();
  return {
    ...actual,
    isIos: () => /\biPhone\b|\biPad\b|\biPod\b/i.test(state.platformUa),
  };
});

import App, { REPAIR_REMOVED_TEXT } from "./App";
import { MemoryContentStore } from "./lib/contentStore";
import { nextReconnectDelay } from "./lib/reconnect";

// jsdom 没有 `URL.createObjectURL`，而待发图片的 chip 会经 `AttachmentThumb` →
// `useImageData` 真的去 `image/read` 取字节、建对象 URL（hook 的缓存按 path 全局
// 共享，用一个稳定桩即可，无需每个用例重置）。没有它，带图片的用例会在这里炸。
URL.createObjectURL = vi.fn(() => "blob:test-thumb");
URL.revokeObjectURL = vi.fn();

beforeEach(() => {
  clients.length = 0;
  state.mode = "normal";
  state.failSet = false;
  state.failList = false;
  state.failListAfter = null;
  state.threads = null;
  state.notifHandlers.length = 0;
  state.approvalHandlers.length = 0;
  state.statusHandlers.length = 0;
  state.rejectCode = {};
  state.listStatus = "idle";
  state.pinnedIds = [];
  state.picks = [];
  state.boards = [];
  state.cards = [];
  state.groupWorkspaces = null;
  state.workspaceGroups = [];
  state.rpcError = {};
  state.hostError = {};
  state.dataSources = {};
  state.platformUa = "";
  state.processes = [];
  state.leadCwds = [];
  state.failLead = false;
  state.leadAttempts = [];
  state.leadFlagEnabled = true;
  dragDrop.fn = null;
  contentStore.current = null;
  Element.prototype.scrollIntoView = vi.fn();
  // 主题是全局 DOM 状态，用例间必须清掉，否则首例会污染后续。
  delete document.documentElement.dataset.theme;
  localStorage.clear();
});

afterEach(cleanup);

const modeTrigger = () => screen.getByRole("button", { name: /mode/i });

/** Open the chip menu → pick YOLO → confirm. */
function enableYolo() {
  fireEvent.click(modeTrigger());
  fireEvent.click(screen.getByRole("menuitemradio", { name: /yolo/i }));
  fireEvent.click(screen.getByRole("button", { name: /confirm/i }));
}

describe("App YOLO wiring", () => {
  it("settles on mount, derives mode from listAll, and renders the ModeChip", async () => {
    render(<App />);

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const methods = clients[0].requests.map((r) => r.method);
    expect(methods).toContain("initialize");
    expect(methods).toContain("thread/listAll");
    expect(methods).toContain("thread/resume");
    expect(modeTrigger().textContent).toContain("Normal");
  });

  it("requests thread/setPermissionMode with the right params and applies it on success", async () => {
    render(<App />);
    await waitFor(() => expect(modeTrigger().textContent).toContain("Normal"));

    enableYolo();

    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/setPermissionMode",
        params: { threadId: "t1", mode: "yolo" },
      }),
    );
    await waitFor(() => expect(modeTrigger().textContent).toContain("YOLO"));
  });

  it("does not change local mode when setPermissionMode fails, and surfaces the error", async () => {
    state.failSet = true;
    render(<App />);
    await waitFor(() => expect(modeTrigger().textContent).toContain("Normal"));

    enableYolo();

    await waitFor(() => expect(screen.getByText("unknown thread")).toBeTruthy());
    // The RPC was attempted (with the right params) but rejected…
    expect(clients[0].requests).toContainEqual({
      method: "thread/setPermissionMode",
      params: { threadId: "t1", mode: "yolo" },
    });
    // …so the chip must still advertise Normal.
    expect(modeTrigger().textContent).toContain("Normal");
    expect(modeTrigger().textContent).not.toContain("YOLO");
  });

  it("initializes the chip to YOLO when listAll reports permission_mode=yolo", async () => {
    state.mode = "yolo";
    render(<App />);
    await waitFor(() => expect(modeTrigger().textContent).toContain("YOLO"));
  });

  it("reflects the new thread's mode when switching threads", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "yolo" },
    ];
    render(<App />);
    // Auto-resumes the first thread → Normal.
    await waitFor(() => expect(modeTrigger().textContent).toContain("Normal"));

    // Clicking the sidebar row bubbles to its onSelect handler.
    fireEvent.click(screen.getByText("two"));

    await waitFor(() => expect(modeTrigger().textContent).toContain("YOLO"));
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/resume",
        params: { threadId: "t2" },
      }),
    );
  });

  it("still stops the running turn after switching to YOLO mid-turn", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // A turn is running: the button reads "Stop".
    act(() => {
      for (const cb of state.notifHandlers)
        cb({ method: "turn/started", params: { thread_id: "t1", turn_id: "u1" } });
    });
    await waitFor(() => expect(screen.getByRole("button", { name: /stop/i })).toBeTruthy());

    // Switch to YOLO while it runs.
    enableYolo();
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/setPermissionMode",
        params: { threadId: "t1", mode: "yolo" },
      }),
    );

    // Stop must still interrupt the running turn.
    fireEvent.click(screen.getByRole("button", { name: /stop/i }));
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "turn/interrupt",
        params: { threadId: "t1" },
      }),
    );
  });

  it("drops a stale Stop when a status push says the thread went idle", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    act(() => {
      for (const cb of state.notifHandlers)
        cb({ method: "turn/started", params: { thread_id: "t1", turn_id: "u1" } });
    });
    await waitFor(() => expect(screen.getByRole("button", { name: /stop/i })).toBeTruthy());

    // `turn/completed` 丢了，但状态推送到了：服务端说这条 thread 已 idle。
    // 这是比「某条通知有没有到」更强的证据，界面必须回到 Send。
    act(() => {
      for (const cb of state.notifHandlers)
        cb({ method: "thread/status/updated", params: { thread_id: "t1", status: "idle" } });
    });

    await waitFor(() =>
      expect(screen.getByRole("button", { name: /^send$/i })).toBeTruthy(),
    );
  });

  it("drops a stale Stop when the server answers that nothing was running", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // 一轮开始 → 按钮变成 Stop。
    act(() => {
      for (const cb of state.notifHandlers)
        cb({ method: "turn/started", params: { thread_id: "t1", turn_id: "u1" } });
    });
    await waitFor(() => expect(screen.getByRole("button", { name: /stop/i })).toBeTruthy());

    // `turn/completed` 在出站队列被灌满时丢了；服务端其实已经空闲。
    // 此时点 Stop，服务端回 `alreadyIdle: true` —— 界面必须就地回到 Send。
    state.dataSources["turn/interrupt"] = () => ({ alreadyIdle: true });
    fireEvent.click(screen.getByRole("button", { name: /stop/i }));

    await waitFor(() =>
      expect(screen.getByRole("button", { name: /^send$/i })).toBeTruthy(),
    );
  });

  it("does not falsely claim Normal when thread/listAll fails", async () => {
    state.failList = true;
    render(<App />);

    // The mount path throws and surfaces an error status.
    await waitFor(() => expect(screen.getByText(/error:/i)).toBeTruthy());

    const chip = modeTrigger() as HTMLButtonElement;
    expect(chip.disabled).toBe(true);
    expect(chip.textContent).not.toContain("Normal");
    expect(chip.textContent).not.toContain("YOLO");
  });

  it("takes the mode from the resume response even when the post-resume listAll lookup fails", async () => {
    // Server-side thread is YOLO; only the mount-time listing succeeds, the
    // follow-up lookup that the resume triggers fails.
    state.mode = "yolo";
    state.failListAfter = 1;
    render(<App />);

    // Auto-resume still happens, so we reach the failing post-resume lookup.
    await waitFor(() =>
      expect(
        clients[0].requests.filter((r) => r.method === "thread/listAll").length,
      ).toBeGreaterThanOrEqual(2),
    );

    // The mode is no longer hostage to the listing: the resume *response* is
    // authoritative, so the chip still shows YOLO. (Before the fix this test
    // asserted "unknown" — the very flakiness being fixed.)
    await waitFor(() => expect(modeTrigger().textContent).toContain("YOLO"));
  });

  it("synchronizes the chip when another client changes the mode (broadcast)", async () => {
    render(<App />);
    await waitFor(() => expect(modeTrigger().textContent).toContain("Normal"));

    // Another client (e.g. the desktop) switched this thread to YOLO; the
    // server broadcasts `thread/permissionModeChanged`.
    act(() => {
      for (const cb of state.notifHandlers)
        cb({ method: "thread/permissionModeChanged", params: { thread_id: "t1", mode: "yolo" } });
    });

    await waitFor(() => expect(modeTrigger().textContent).toContain("YOLO"));
  });
});

describe("App approval overlay scope", () => {
  it("leaves the composer usable so Stop stays clickable behind an approval", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    act(() => {
      for (const cb of state.notifHandlers)
        cb({ method: "turn/started", params: { thread_id: "t1", turn_id: "u1" } });
    });
    await waitFor(() => expect(screen.getByRole("button", { name: /stop/i })).toBeTruthy());

    // A tool call in this turn needs approval. The dialog is modal for the
    // transcript, but it must NOT own the composer: the overlay used to be
    // `absolute inset-0` inside `.app-main`, which painted over the input card
    // and its Stop button. The user then could not interrupt a turn that was
    // blocked waiting for an approval (a deadlock the user could only escape by
    // answering the prompt).
    state.approvalHandlers[0]({
      id: "perm-1",
      params: {
        thread_id: "t1",
        turn_id: "u1",
        request_id: 1,
        tool_name: "bash",
        tool_input: { command: "rm -rf /tmp/x" },
        prefix_suggestion: null,
        kind: "Normal",
      },
    });
    await waitFor(() => expect(screen.getByRole("dialog")).toBeTruthy());

    const overlay = screen.getByRole("dialog").parentElement as HTMLElement;
    // `absolute inset-0` paints over the *containing block*, so what matters is
    // which positioned ancestor the overlay resolves against. jsdom has no layout
    // engine, so walk up to the nearest positioned ancestor and assert that the
    // composer is outside it: that is the geometric guarantee, expressed in the
    // DOM, that the overlay cannot cover the input card.
    expect(overlay.className.split(/\s+/)).toContain("absolute");
    expect(overlay.className.split(/\s+/)).toContain("inset-0");
    const stop = screen.getByRole("button", { name: /stop/i });
    let scope: HTMLElement | null = overlay.parentElement;
    while (scope && !scope.className.split(/\s+/).includes("relative")) {
      scope = scope.parentElement;
    }
    expect(scope, "the overlay must have a positioned ancestor").not.toBeNull();
    expect(
      scope!.contains(stop),
      "an `inset-0` overlay inside this ancestor would paint over the composer's Stop",
    ).toBe(false);
    expect((stop as HTMLButtonElement).disabled).toBe(false);

    // And it still works: interrupting while an approval is pending.
    fireEvent.click(stop);
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "turn/interrupt",
        params: { threadId: "t1" },
      }),
    );
  });
});

describe("App approval focus", () => {
  it("does not steal focus from an in-progress thread rename", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // A turn is running on t1.
    state.notifHandlers[0]({
      method: "thread/status/updated",
      params: { thread_id: "t1", status: "running" },
    });

    // The user starts renaming t1 while it runs.
    fireEvent.doubleClick(screen.getByText("one"));
    const field = document.querySelector<HTMLInputElement>("[data-rename-input]")!;
    expect(field).not.toBeNull();
    expect(document.activeElement).toBe(field);
    fireEvent.change(field, { target: { value: "half-typed" } });

    // The running turn asks for tool approval: the dialog mounts.
    state.approvalHandlers[0]({
      id: "perm-1",
      params: {
        thread_id: "t1",
        turn_id: "u1",
        request_id: 1,
        tool_name: "bash",
        tool_input: {},
        prefix_suggestion: null,
        kind: "Normal",
      },
    });
    await waitFor(() => expect(screen.getByRole("dialog")).toBeTruthy());

    // The dialog must not yank focus out of the rename field: the field's
    // onBlur commits, so a stolen focus silently keeps a half-typed title and
    // unmounts the field.
    const stillThere = document.querySelector<HTMLInputElement>("[data-rename-input]");
    expect(stillThere, "rename field must survive the approval dialog").not.toBeNull();
    expect(stillThere!.value).toBe("half-typed");
    expect(document.activeElement).toBe(stillThere);
    expect(clients[0].requests.some((r) => r.method === "thread/rename")).toBe(false);
  });

  it("focuses Deny when the user is not editing a text field", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // No text field in play: the dialog keeps its deliberate safe default, so a
    // reflexive Enter/Space cannot approve a destructive tool call.
    (document.activeElement as HTMLElement | null)?.blur();
    state.approvalHandlers[0]({
      id: "perm-1",
      params: {
        thread_id: "t1",
        turn_id: "u1",
        request_id: 1,
        tool_name: "bash",
        tool_input: {},
        prefix_suggestion: null,
        kind: "Normal",
      },
    });

    await waitFor(() => expect(screen.getByRole("dialog")).toBeTruthy());
    expect((document.activeElement as HTMLElement).textContent).toBe("Deny");
  });
});

describe("App parallel threads", () => {
  it("does not re-resume a warm thread when switching back to it", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await waitFor(() => expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true));

    fireEvent.click(screen.getByText("two")); // 冷 thread → resume 一次
    await waitFor(() =>
      expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(2),
    );
    fireEvent.click(screen.getByText("one")); // warm → 不再 resume
    await new Promise((r) => setTimeout(r, 0));
    expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(2);
  });

  it("re-derives the chip on a warm switch (per-thread mode)", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "yolo" },
    ];
    render(<App />);
    // Auto-resumes t1 → Normal.
    await waitFor(() => expect(modeTrigger().textContent).toContain("Normal"));

    fireEvent.click(screen.getByText("two")); // 冷 → resume → YOLO
    await waitFor(() => expect(modeTrigger().textContent).toContain("YOLO"));

    // 切回已 resume 过的 t1（warm，不再 resume）——chip 必须回到 Normal,
    // 而不是停留在 t2 的 YOLO。
    fireEvent.click(screen.getByText("one"));
    await waitFor(() => expect(modeTrigger().textContent).toContain("Normal"));
    expect(modeTrigger().textContent).not.toContain("YOLO");
    expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(2);
  });

  it("uses turn/interject instead of turn/start while a turn is running", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // The server is authoritative for "a turn is running".
    state.notifHandlers[0]({
      method: "thread/status/updated",
      params: { thread_id: "t1", status: "running" },
    });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "follow-up" } });
    fireEvent.click(screen.getByRole("button", { name: /^send$/i }));

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/interject")).toBe(true),
    );
    const methods = clients[0].requests.map((r) => r.method);
    expect(methods).not.toContain("turn/start");
    expect(
      clients[0].requests.find((r) => r.method === "turn/interject")?.params,
    ).toMatchObject({ threadId: "t1", input: [{ type: "text", text: "follow-up" }] });
  });

  it("still uses turn/start when the thread is idle", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "fresh" } });
    fireEvent.click(screen.getByRole("button", { name: /^send$/i }));

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    expect(clients[0].requests.map((r) => r.method)).not.toContain("turn/interject");
  });

  it("keeps each thread's unsent draft out of the others", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    // Auto-resumes t1; wait until the composer belongs to it.
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const box = () => screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(box(), { target: { value: "for one" } });
    expect(box().value).toBe("for one");

    // Switch to t2: the box must start empty, not carry t1's draft over.
    fireEvent.click(screen.getByText("two"));
    await waitFor(() =>
      expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(2),
    );
    expect(box().value).toBe("");

    // t2 gets its own draft.
    fireEvent.change(box(), { target: { value: "for two" } });

    // Back to t1 (warm): its draft is restored; t2's stayed in t2.
    fireEvent.click(screen.getByText("one"));
    await waitFor(() => expect(box().value).toBe("for one"));

    fireEvent.click(screen.getByText("two"));
    await waitFor(() => expect(box().value).toBe("for two"));
  });

  it("does not lose a background thread's timeline when switching", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await waitFor(() => expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true));
    fireEvent.click(screen.getByText("two"));
    await waitFor(() =>
      expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(2),
    );

    // 后台 t1 流式输出（当前看的是 t2）。
    const notify = state.notifHandlers[0];
    notify({ method: "item/delta", params: { thread_id: "t1", item_id: "a1", delta: "bg" } });

    fireEvent.click(screen.getByText("one")); // 切回 t1（warm，不 resume）
    await waitFor(() => expect(screen.getByText("bg")).toBeTruthy());
  });

  it("shows a banner for a background approval and jumps on click", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await waitFor(() => expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true));
    fireEvent.click(screen.getByText("two")); // 当前 = t2
    await waitFor(() =>
      expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(2),
    );

    state.approvalHandlers[0]({
      id: "perm-1",
      params: {
        thread_id: "t1",
        turn_id: "u1",
        request_id: 1,
        tool_name: "bash",
        tool_input: {},
        prefix_suggestion: null,
        kind: "Normal",
      },
    });

    await waitFor(() => expect(screen.getByRole("button", { name: /jump/i })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: /jump/i }));
    // 跳到 t1 后显示其审批模态。
    await waitFor(() => expect(screen.getByText(/bash/)).toBeTruthy());
  });
});

/**
 * The server and the client can disagree about whether a turn is active:
 * `turn/completed` reaches the client before the server flips the thread back
 * to idle, so a send can pick the wrong method. The server answers with a
 * specific code (`-32013` for `turn/interject` with nothing running, `-32012`
 * for `turn/start` while one is), and the client must read it, switch method,
 * and still deliver the text.
 */
describe("App send recovery from a status disagreement", () => {
  const sendFromInput = async (text: string) => {
    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: text } });
    // While a turn is active the button reads "Stop"; this harness clicks it
    // either way, since Enter would take the same branch.
    fireEvent.click(screen.getByRole("button", { name: /^(send|stop)$/i }));
  };

  /**
   * Make `thread/listAll` report t1 as running, so the very first listing the
   * app does puts the cached status into the disagreed state. (Firing
   * `turn/started` instead would flip the button to "Stop" and there would be
   * no Send button left to click.)
   */
  const seedRunning = () => {
    state.listStatus = "running";
  };

  it("falls back to turn/start when turn/interject answers -32013", async () => {
    seedRunning();
    render(<App />);
    // 运行中的会话不再走 resume（那会打断在跑的 turn），改用只读 readItems 补齐。
    // 等它发出即证明 listAll 的 running 状态已落进 store。
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );
    state.rejectCode = { "turn/interject": -32013 };

    await sendFromInput("late message");

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    expect(clients[0].requests.map((r) => r.method)).toContain("turn/interject");
    // The text was delivered, not dropped on the error.
    expect(clients[0].requests.find((r) => r.method === "turn/start")?.params).toMatchObject({
      threadId: "t1",
      input: [{ type: "text", text: "late message" }],
    });
    expect(screen.queryByText(/no turn is running/i)).toBeNull();
  });

  it("falls back to turn/interject when turn/start answers -32012", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    // The client believes the thread is idle; the server already started a turn.
    state.rejectCode = { "turn/start": -32012 };

    await sendFromInput("raced message");

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/interject")).toBe(true),
    );
    expect(clients[0].requests.map((r) => r.method)).toContain("turn/start");
    expect(clients[0].requests.find((r) => r.method === "turn/interject")?.params).toMatchObject({
      threadId: "t1",
      input: [{ type: "text", text: "raced message" }],
    });
  });

  it("does not ping-pong when both methods fail, and surfaces the error", async () => {
    seedRunning();
    render(<App />);
    // 同上：运行中的会话以 readItems 补齐，不再 resume。
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );
    state.rejectCode = { "turn/interject": -32013, "turn/start": -32012 };

    await sendFromInput("doomed");

    await waitFor(() => expect(screen.getByText(/forced -32012/)).toBeTruthy());
    const methods = clients[0].requests.map((r) => r.method);
    expect(methods.filter((m) => m === "turn/interject")).toHaveLength(1);
    expect(methods.filter((m) => m === "turn/start")).toHaveLength(1);
    // Nothing was accepted, so the draft is kept for a retry rather than
    // silently dropped.
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("doomed");
  });

  it("picks the method the UI is showing after the turn completes", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    state.notifHandlers[0]({
      method: "turn/completed",
      params: { thread_id: "t1", turn_id: "u1", status: "completed" },
    });

    await sendFromInput("follow-up");

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    expect(clients[0].requests.map((r) => r.method)).not.toContain("turn/interject");
  });
});

describe("App slash commands", () => {
  it("renders /help as a grouped card without touching the agent", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/help" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByTestId("help-card");
    expect(screen.getByText("可用命令")).toBeTruthy();
    // 分组骨架：三组标题都在，命令说明也在。
    expect(screen.getByText("会话")).toBeTruthy();
    expect(screen.getByText("状态")).toBeTruthy();
    expect(screen.getByText("清空对话上下文")).toBeTruthy();
    expect(
      clients[0].requests.some((r) => r.method === "turn/start"),
    ).toBe(false);
  });

  it("renders /help <command> as that command's detail card", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/help cost" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByText("命令 /cost");
    expect(screen.getByText("显示 token 使用量")).toBeTruthy();
    expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false);
  });

  it("renders an unknown slash command as the same help card with an error line", async () => {
    // 打错一个字符不该掉回旧的纯文本形态：`/nope` 与 `/help nope` 同一张卡。
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/nope" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByTestId("help-card");
    expect(screen.getByText("未知命令: /nope")).toBeTruthy();
    expect(screen.getByText("清空对话上下文")).toBeTruthy();
    expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false);
  });

  it("renders /config as a card without touching the agent", async () => {
    state.dataSources["config/read"] = () => ({
      provider: "anthropic",
      model: "claude-sonnet-4",
      sandbox: "WorkspaceWrite",
    });
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/config" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByTestId("config-card");
    expect(screen.getByText("当前配置")).toBeTruthy();
    expect(screen.getByText("workspace-write")).toBeTruthy();
    // 读的是**当前会话**的生效配置，不是 sidecar 进程启动时的快照：
    // 进程快照的 workdir/model/provider 与用户正在用的会话无关。
    const read = clients[0].requests.find((r) => r.method === "config/read");
    expect(read?.params).toEqual({ threadId: "t1" });
    expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false);
  });

  it("shows a failed /config read inside the card", async () => {
    state.rejectCode["config/read"] = -1;
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/config" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByTestId("config-card");
    expect(screen.getByText(/读取配置失败/)).toBeTruthy();
  });

  it("sends thread/clear and empties the transcript on /clear", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // 先造一条消息,让 clear 有东西可清。
    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hello" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    await screen.findByText("hello");

    fireEvent.change(textarea, { target: { value: "/clear" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/clear")).toBe(true),
    );
    await waitFor(() => expect(screen.queryByText("hello")).toBeNull());
    await screen.findByText(/对话已清空/);
  });

  it("surfaces a rejected /clear without clearing the transcript", async () => {
    state.rejectCode = { "thread/clear": -32012 };
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hello" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    await screen.findByText("hello");

    fireEvent.change(textarea, { target: { value: "/clear" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByText(/清空失败|-32012/);
    expect(screen.getByText("hello")).toBeTruthy();
  });

  it("sends thread/compact and reports the server's verdict", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/compact" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/compact")).toBe(true),
    );
    // 默认 mock 返回 {},没有 status 字段 → 视为 unknown,提示"未知结果"。
    await screen.findByText(/压缩/);
  });
});

/**
 * 主区域看板（Task 7）。
 *
 * 侧栏条目是入口（Task 6 已有自己的用例），这里要的是：点开之后主区域给出的
 * 是「那个项目」的看板，而不是某个全局左列——后者连同折叠条一起被删掉了。
 */
async function openBoardFor(project: string) {
  // 打开小组右键菜单（与键盘路径同一条处理链）→ 点「看板」条目。
  const header = document.querySelector<HTMLElement>("[data-ws-header]")!;
  fireEvent.contextMenu(header);
  await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
  fireEvent.click(screen.getByLabelText("看板"));
  return project;
}

describe("App driven-session sidebar refresh", () => {
  it("lists a newly started running card session before its turn completes", async () => {
    state.groupWorkspaces = ["/proj"];
    state.threads = [{ thread_id: "t1", title: "discussion", permission_mode: "normal" }];
    render(<App />);

    await waitFor(() =>
      expect(clients[0].requests.some((request) => request.method === "thread/resume")).toBe(true),
    );

    // The driver creates a card thread directly in the server while its turn
    // starts; only the running status notification tells the desktop about it.
    // `thread/listAll` knows about it after that notification refreshes groups.
    state.threads.push({
      thread_id: "card-thread",
      title: "看板 · card-1",
      permission_mode: "yolo",
      driver_project: "/proj",
      driver_ref: "card-1",
    });
    state.listStatus = "running";
    act(() => {
      for (const notify of state.notifHandlers) {
        notify({
          method: "thread/status/updated",
          params: { thread_id: "card-thread", status: "running" },
        });
      }
    });

    await waitFor(() => expect(screen.getByText("看板会话 (1)")).toBeTruthy());
    fireEvent.click(screen.getByLabelText("看板会话"));
    expect(screen.getByText("看板 · card-1")).toBeTruthy();
  });
});

describe("App 主区域看板", () => {
  it("未选中看板时不渲染看板面板，也没有全局左列", async () => {
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());

    // 看板只在选中时才进主区域；没点之前一个都不该有。
    expect(screen.queryByText("Superpowers 看板")).toBeNull();
    expect(screen.queryByLabelText("收起看板")).toBeNull();
    expect(screen.queryByLabelText("Superpowers 看板开关")).toBeNull();
  });

  it("看板列表读不到时明说「无法读取」，而不是留空", async () => {
    // 摘要是侧栏那一眼的全部。读失败若留空，就和「空看板」长得一模一样——
    // 而这两件事对用户是两码事：一个要去看 daemon/插件，一个什么都不用做。
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    state.rpcError["list"] = new Error("daemon is unavailable: connection refused");
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());

    expect(await screen.findByText("无法读取")).toBeTruthy();
  });

  it("选中看板时主区域显示该项目看板，且不再有全局左列", async () => {
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");

    expect(await screen.findByText("Superpowers 看板")).toBeTruthy();
    // 收起已删除：离开看板走「点会话」，不再有「收起看板」按钮。
    expect(screen.queryByLabelText("收起看板")).toBeNull();
    expect(screen.getByRole("button", { name: "加入看板" })).toBeTruthy();
  });

  it("看板的所有读写都带上该项目的 project", async () => {
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    state.cards = [{ id: "c1", state: "queued", progress: null, detail: "/w" }];
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");
    await screen.findByText("Superpowers 看板");

    await waitFor(() => {
      const queries = clients[0].requests.filter((r) => r.method === "plugin/query");
      expect(queries.some((r) => (r.params as { method?: string })?.method === "list")).toBe(true);
      expect(queries.some((r) => (r.params as { method?: string })?.method === "switch.read")).toBe(true);
    });
    // 只认带着 project 的那些：漏掉 project 就会读到 app 自己的 cwd。
    const queries = clients[0].requests.filter((r) => r.method === "plugin/query");
    expect(queries.every((r) => (r.params as { project?: string }).project === "/proj")).toBe(true);
    expect(queries.map((r) => (r.params as { method?: string }).method)).toContain("list");
  });

  it("开关写入失败会显示错误，而不是静默", async () => {
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    // 宿主把语义码放在 data.code 里，message 只剩人话。
    state.hostError["switch.write"] = {
      code: -32603,
      message: "看板读写失败",
      data: { code: "some_other_failure" },
    };
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");

    fireEvent.click(await screen.findByLabelText("Superpowers 看板开关"));

    expect(await screen.findByText(/看板读写失败/)).toBeTruthy();
  });

  it("开关写入失败：daemon 不可达与插件没装各有各的说法", async () => {
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    state.hostError["switch.write"] = {
      code: -32603,
      message: "daemon is unavailable: connection refused",
      data: { code: "daemon_unavailable" },
    };
    const first = render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");
    fireEvent.click(await screen.findByLabelText("Superpowers 看板开关"));

    expect(await screen.findByText(/看板进程不可达/)).toBeTruthy();
    // 「连不上」绝不能被说成「插件没装」：那会让用户去装一个已经装好的插件。
    expect(screen.queryByText(/插件未安装/)).toBeNull();
    first.unmount();

    state.hostError["switch.write"] = {
      code: -32603,
      message: "plugin superpowers-kanban is not available",
      data: { code: "plugin_unavailable" },
    };
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");
    fireEvent.click(await screen.findByLabelText("Superpowers 看板开关"));

    expect(await screen.findByText(/插件未安装/)).toBeTruthy();
  });

  it("旧 board_not_created 码不再给「创建看板」按钮（创建入口只在侧栏右键菜单）", async () => {
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    state.hostError["switch.write"] = {
      code: -32603,
      message: "这个项目还没有看板",
      data: { code: "board_not_created" },
    };
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");
    fireEvent.click(await screen.findByLabelText("Superpowers 看板开关"));

    // 摘掉门控后宿主不再发这个码；即便收到，也只当一次说不清的失败，
    // 面板不再提供那条专属出路（创建入口保留在侧栏项目右键菜单）。
    await screen.findByText(/看板读写失败/);
    expect(screen.queryByRole("button", { name: "创建看板" })).toBeNull();
  });

  it("侧栏「创建」入口先装插件、再走通用 plugin/query 建盘", async () => {
    state.groupWorkspaces = ["/proj"];
    state.boards = [];
    render(<App />);
    const header = await waitFor(() => {
      const h = document.querySelector<HTMLElement>('[data-ws-header="/proj"]');
      expect(h).not.toBeNull();
      return h!;
    });
    fireEvent.contextMenu(header);

    fireEvent.click(screen.getByText("创建 Superpowers 看板"));

    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "plugin/install",
        params: { project: "/proj", plugin: "superpowers-kanban" },
      }),
    );
    expect(clients[0].requests).toContainEqual({
      method: "plugin/query",
      params: {
        plugin: "superpowers-kanban",
        project: "/proj",
        method: "lifecycle/create",
        params: { project: "/proj" },
      },
    });
    // 顺序是契约：全新项目没有清单，宿主不会拉起插件，plugin/query 无处可拨。
    const lifecycle = clients[0].requests.filter(
      (r) => r.method === "plugin/install" || r.method === "plugin/query",
    );
    expect(lifecycle.map((r) => r.method)).toEqual(["plugin/install", "plugin/query"]);
  });

  it("切回同一个看板不会重读（选中同一个不算换项目）", async () => {
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");
    await screen.findByText("Superpowers 看板");

    const reads = () =>
      clients[0].requests.filter(
        (r) =>
          r.method === "plugin/query" &&
          (r.params as { method?: string }).method === "list",
      ).length;
    const before = reads();
    fireEvent.click(screen.getByLabelText("看板"));

    // 再点一次同一个项目只是「还是这个」：重读会白白清空并重画面板。
    await new Promise((r) => setTimeout(r, 10));
    expect(reads()).toBe(before);
  });

  it("切换项目后看板问的是新项目", async () => {
    state.boards = [{ project: "/proj" }, { project: "/other" }];
    state.groupWorkspaces = ["/proj", "/other"];
    render(<App />);
    await waitFor(() => expect(screen.getAllByLabelText("看板")).toHaveLength(2));

    fireEvent.click(screen.getAllByLabelText("看板")[1]);
    await waitFor(() => {
      const queries = clients[0].requests.filter((r) => r.method === "plugin/query");
      expect(queries.some((r) => (r.params as { project?: string }).project === "/other")).toBe(true);
    });
  });

  it("看板打开时点侧栏另一条会话 → 跳到该会话页，看板消失（无需点收起）", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    render(<App />);
    // 启动自动选第一条会话（t1），再看板页。
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");
    await screen.findByText("Superpowers 看板");

    // 点侧栏另一条会话：应当直接跳过去，看板退场。
    fireEvent.click(screen.getByText("two"));

    await waitFor(() => expect(screen.queryByLabelText("Superpowers 看板")).toBeNull());
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/resume",
        params: { threadId: "t2" },
      }),
    );
  });

  it("看板打开时新建会话 → 落到会话页，看板消失", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    // 「+ New thread」在有最近目录时弹菜单，点其中一项即 onNew(path)。
    state.dataSources["workspace/list"] = () => ({
      workspaces: [{ path: "/proj", exists: true }],
    });
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");
    await screen.findByText("Superpowers 看板");

    fireEvent.click(screen.getByText("+ New thread"));
    fireEvent.click(await screen.findByRole("menuitem", { name: "proj" }));

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/start")).toBe(true),
    );
    await waitFor(() => expect(screen.queryByLabelText("Superpowers 看板")).toBeNull());
  });

  it("切到会话后点侧栏看板条目 → 看板重新出现", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");
    await screen.findByText("Superpowers 看板");

    // 切到另一条会话：离开看板。
    fireEvent.click(screen.getByText("two"));
    await waitFor(() => expect(screen.queryByLabelText("Superpowers 看板")).toBeNull());

    // 再点侧栏的看板条目：看板重新出现。收起已删除，回到看板只有这条入口。
    fireEvent.click(screen.getByLabelText("看板"));
    expect(await screen.findByText("Superpowers 看板")).toBeTruthy();
  });

  it("点卡片上的会话链接 = 切到那个会话、离开看板（spec §4.7）", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    state.cards = [{ id: "card-1", state: "queued", progress: null, detail: "/w", thread_id: "t1" }];
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");
    await screen.findByText("Superpowers 看板");

    // 卡片组件用 thread id 作为按钮文字；点它 = 去看那条会话。
    fireEvent.click(
      within(screen.getByLabelText("Superpowers 看板")).getByRole("button", { name: "t1" }),
    );

    // 看板是会话的兄弟视图：离开看板，回到对话，而不是把看板盖在会话上。
    await waitFor(() => expect(screen.queryByLabelText("Superpowers 看板")).toBeNull());
    expect(screen.queryByLabelText("收起看板")).toBeNull();
    // 会话确实切过去了。
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/resume",
        params: { threadId: "t1" },
      }),
    );
  });
});

describe("App Superpowers 看板 enqueue", () => {
  beforeEach(() => {
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
  });

  const clickEnqueue = () =>
    fireEvent.click(screen.getByRole("button", { name: "加入看板" }));

  /** 选中 /proj 的看板，等主区域把面板画出来。 */
  const openBoard = async () => {
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");
    await screen.findByRole("button", { name: "加入看板" });
  };

  it("sends one enqueue with both picked paths", async () => {
    await openBoard();

    state.picks = ["/p/a.spec.md", "/p/a.plan.md"];
    clickEnqueue();

    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "plugin/query",
        params: {
          plugin: "superpowers-kanban",
          // 主区域渲染的是选中项目的看板，投递也带该项目。
          project: "/proj",
          method: "enqueue",
          params: { spec_path: "/p/a.spec.md", plan_path: "/p/a.plan.md" },
        },
      }),
    );
    expect(
      clients[0].requests.filter(
        (r) =>
          r.method === "plugin/query" &&
          (r.params as { method?: string })?.method === "enqueue",
      ),
    ).toHaveLength(1);
  });

  it("sends nothing when the picker is cancelled", async () => {
    await openBoard();

    state.picks = [null];
    clickEnqueue();

    await waitFor(() => expect(state.picks).toHaveLength(0));
    expect(
      clients[0].requests.filter(
        (r) =>
          r.method === "plugin/query" &&
          (r.params as { method?: string })?.method === "enqueue",
      ),
    ).toHaveLength(0);
  });

  it("surfaces a rejected enqueue instead of crashing", async () => {
    state.rpcError["enqueue"] = { code: -32000, message: "forced -32000" };
    await openBoard();

    state.picks = ["/p/a.spec.md", "/p/a.plan.md"];
    clickEnqueue();

    await waitFor(() => expect(screen.getByText(/forced -32000/)).toBeTruthy());
    // Still usable after the failure.
    expect(
      (screen.getByRole("button", { name: "加入看板" }) as HTMLButtonElement).disabled,
    ).toBe(false);
  });
});

describe("App session pinning", () => {
  it("renders a pinned thread in the Pinned section from thread/listAll", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    state.pinnedIds = ["t2"];
    const { container } = render(<App />);
    await waitFor(() => expect(screen.getAllByText("two").length).toBeGreaterThan(0));
    // Pinned 分区标题存在。
    expect(container.textContent).toContain("Pinned");
  });

  it("sends thread/setPinned and re-lists when the pin button is clicked", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    const { container } = render(<App />);
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    const before = clients[0].requests.filter((r) => r.method === "thread/listAll").length;
    fireEvent.click(container.querySelector('[aria-label="Pin thread"]')!);
    await waitFor(() => {
      expect(clients[0].requests.some((r) => r.method === "thread/setPinned")).toBe(true);
    });
    await waitFor(() => {
      const after = clients[0].requests.filter((r) => r.method === "thread/listAll").length;
      expect(after).toBeGreaterThan(before);
    });
  });

  it("auto-selects the first pinned thread on mount", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "yolo" },
    ];
    state.pinnedIds = ["t2"];
    render(<App />);
    // 服务端给了顺序：置顶的 t2 先于分组的 t1 → 首屏落在 t2 的 yolo 上。
    await waitFor(() => expect(modeTrigger().textContent).toContain("YOLO"));
    expect(clients[0].requests).toContainEqual({
      method: "thread/resume",
      params: { threadId: "t2" },
    });
  });

  it("reads a pinned thread's mode back from the top-level pinned list", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "yolo" },
    ];
    state.pinnedIds = ["t2"];
    render(<App />);
    // 首屏自动选中 t2；resume 后从 listAll 回读权限模式。
    await waitFor(() => expect(modeTrigger().textContent).toContain("YOLO"));

    // 切回 t1 再回 t2（warm，不再 resume）：回读路径必须能扫到顶层 pinned，
    // 否则 chip 会落空成 unknown，而不是 t2 的 YOLO。
    fireEvent.click(screen.getByText("one"));
    await waitFor(() => expect(modeTrigger().textContent).toContain("Normal"));
    // 置顶分区与分组里各有一行 t2（服务端不摘除、前端不隐藏分组的场景由侧栏
    // 负责），这里点第一行（置顶区）即可。
    fireEvent.click(screen.getAllByText("two")[0]);
    await waitFor(() => expect(modeTrigger().textContent).toContain("YOLO"));
  });

  it("renders a pinned thread in the Pinned section and hides it from its group", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    state.pinnedIds = ["t2"];
    const { container } = render(<App />);
    await waitFor(() => expect(screen.getAllByText("two").length).toBeGreaterThan(0));

    // 服务端把置顶项同时留在分组里；侧栏必须只渲染一次——在置顶分区，不在分组行。
    const pinnedRows = Array.from(container.querySelectorAll<HTMLElement>("[data-pinned-row]"));
    expect(pinnedRows.map((r) => r.textContent)).toEqual([expect.stringContaining("two")]);
    const groupRows = Array.from(container.querySelectorAll<HTMLElement>("[data-group-row]"));
    expect(groupRows.some((r) => r.textContent!.includes("two"))).toBe(false);
    expect(screen.getAllByText("two")).toHaveLength(1);
  });

  it("sends thread/reorderPinned with the new top-to-bottom order on drop", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    state.pinnedIds = ["t1", "t2"];
    const { container } = render(<App />);
    await waitFor(() => expect(screen.getAllByText("two").length).toBeGreaterThan(0));

    const rows = container.querySelectorAll<HTMLElement>("[data-pinned-row]");
    expect(rows).toHaveLength(2);
    // 把第一行拖到第二行位置。
    fireEvent.dragStart(rows[0]);
    fireEvent.dragOver(rows[1]);
    fireEvent.drop(rows[1]);

    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/reorderPinned",
        params: { threadIds: ["t2", "t1"] },
      }),
    );
  });
});

describe("App settings & theme wiring", () => {
  /** 推进到 initialize 之后（App 挂载即发 initialize）。 */
  async function settle() {
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "initialize")).toBe(true),
    );
  }

  /** 把一帧服务端通知交给 App 注册的通知回调。 */
  function notify(method: string, params: unknown) {
    act(() => {
      for (const cb of state.notifHandlers) cb({ method, params });
    });
  }

  it("applies the theme returned by ui/settings/read", async () => {
    render(<App />);
    await settle();
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe("dark"));
  });

  it("writes the theme when the settings dialog switches it", async () => {
    render(<App />);
    await settle();
    fireEvent.click(screen.getByRole("button", { name: "设置" }));
    fireEvent.click(screen.getByRole("button", { name: "浅色" }));
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "ui/settings/write",
        params: { theme: "light" },
      }),
    );
    expect(document.documentElement.dataset.theme).toBe("light");
  });

  it("follows a ui/settings/updated notification", async () => {
    render(<App />);
    await settle();
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe("dark"));
    notify("ui/settings/updated", { theme: "light" });
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe("light"));
  });

  it("reflects a thread/titleChanged in the sidebar without re-listing", async () => {
    render(<App />);
    await settle();
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    const listCallsBefore = clients[0].requests.filter((r) => r.method === "thread/listAll").length;
    // 首轮 LLM 标题生成成功后服务端推来的权威标题。
    notify("thread/titleChanged", { thread_id: "t1", title: "修复登录报错" });
    await waitFor(() => expect(screen.getByText("修复登录报错")).toBeTruthy());
    // 就地把权威值落笔，不该为此再拉一次全量 thread/listAll。
    expect(clients[0].requests.filter((r) => r.method === "thread/listAll").length).toBe(
      listCallsBefore,
    );
  });

  it("re-lists threads on a thread/groupsChanged notification", async () => {
    render(<App />);
    await settle();
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    const listCallsBefore = clients[0].requests.filter((r) => r.method === "thread/listAll").length;

    // 组表只住服务端，通知里不带内容，客户端只能整份重拉。
    state.workspaceGroups = [
      { workspace: "/w", id: "g1", name: "登录模块", description: "三条线", threadIds: ["t1"] },
    ];
    notify("thread/groupsChanged", { workspace: "/w" });
    await waitFor(() =>
      expect(clients[0].requests.filter((r) => r.method === "thread/listAll").length).toBe(
        listCallsBefore + 1,
      ),
    );
    await waitFor(() => expect(screen.getByText("登录模块")).toBeTruthy());
  });

  it("reflects a thread/tagChanged in the sidebar without re-listing", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await settle();
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    const listCallsBefore = clients[0].requests.filter((r) => r.method === "thread/listAll").length;

    notify("thread/tagChanged", { thread_id: "t1", tag: "red" });
    await waitFor(() =>
      expect(document.querySelector('[data-thread-tag="red"]')).toBeTruthy(),
    );
    // 就地把权威值落笔，不该为此再拉一次全量 thread/listAll。
    expect(clients[0].requests.filter((r) => r.method === "thread/listAll").length).toBe(
      listCallsBefore,
    );
  });

  it("reflects ui/tagNames/updated in the existing dot's label", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    render(<App />);
    await settle();
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());

    notify("thread/tagChanged", { thread_id: "t1", tag: "red" });
    await waitFor(() => expect(document.querySelector('[data-thread-tag="red"]')).toBeTruthy());

    notify("ui/tagNames/updated", { tag_names: { red: "紧急" } });
    await waitFor(() =>
      expect(
        document.querySelector('[data-thread-tag="red"]')?.getAttribute("aria-label"),
      ).toBe("紧急"),
    );
  });

  it("does not tag optimistically, and re-lists once thread/setTag resolves", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    // Hold the RPC open so we can observe the UI *before* the server answers.
    let release: () => void = () => {};
    state.dataSources["thread/setTag"] = () =>
      new Promise<void>((resolve) => {
        release = () => resolve();
      });
    const { container } = render(<App />);
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    const listCallsBefore = clients[0].requests.filter((r) => r.method === "thread/listAll").length;

    fireEvent.contextMenu(container.querySelector('[data-thread-row="t1"]')!);
    fireEvent.click(screen.getByRole("menuitemradio", { name: "红" }));

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/setTag")).toBe(true),
    );
    expect(clients[0].requests.find((r) => r.method === "thread/setTag")!.params).toEqual({
      threadId: "t1",
      tag: "red",
    });
    // 不做乐观更新：回包之前 UI 不能显示色点，也不能为此重拉列表。
    expect(document.querySelector('[data-thread-tag="red"]')).toBeNull();
    expect(clients[0].requests.filter((r) => r.method === "thread/listAll").length).toBe(
      listCallsBefore,
    );

    act(() => release());
    await waitFor(() => {
      expect(
        clients[0].requests.filter((r) => r.method === "thread/listAll").length,
      ).toBeGreaterThan(listCallsBefore);
    });
  });

  it("surfaces the error and still re-lists when thread/setTag rejects", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    state.rejectCode["thread/setTag"] = -32011;
    const { container } = render(<App />);
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    const listCallsBefore = clients[0].requests.filter((r) => r.method === "thread/listAll").length;

    fireEvent.contextMenu(container.querySelector('[data-thread-row="t1"]')!);
    fireEvent.click(screen.getByRole("menuitemradio", { name: "红" }));

    await waitFor(() => expect(screen.getByText(/forced -32011/)).toBeTruthy());
    // 失败同样重拉：本地不假装成功，以服务端为权威。
    await waitFor(() => {
      expect(
        clients[0].requests.filter((r) => r.method === "thread/listAll").length,
      ).toBeGreaterThan(listCallsBefore);
    });
    expect(document.querySelector('[data-thread-tag="red"]')).toBeNull();
  });

  it("writes only the changed key when a tag name is edited in settings", async () => {
    state.dataSources["ui/settings/read"] = () => ({ theme: "dark", tag_names: {} });
    render(<App />);
    await settle();
    fireEvent.click(screen.getByRole("button", { name: "设置" }));
    fireEvent.click(screen.getByRole("tab", { name: "标签" }));
    const input = document.querySelector<HTMLInputElement>('[data-tag-name-input="red"]')!;
    fireEvent.change(input, { target: { value: "紧急" } });
    fireEvent.blur(input);
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "ui/settings/write",
        params: { tag_names: { red: "紧急" } },
      }),
    );
    // 只提交改动的那一个键：其余 5 色不得出现在同一请求里。
    const write = clients[0].requests.find((r) => r.method === "ui/settings/write")!;
    expect(Object.keys((write.params as { tag_names: Record<string, string> }).tag_names)).toEqual([
      "red",
    ]);
  });

  it("reflects a thread/modelChanged from another client in that thread's picker", async () => {
    render(<App />);
    await settle();
    // t1 自动选中（跟随全局默认），初始生效模型来自 seed 的 "m"。
    const trigger = () => screen.getByRole("button", { name: /模型/ });
    await waitFor(() => expect(trigger().textContent).toContain("m"));
    // 另一个客户端（如手机）改了 t1 的生效模型：桌面端必须跟着更新。
    notify("thread/modelChanged", { thread_id: "t1", model: "model-from-phone" });
    await waitFor(() => expect(trigger().textContent).toContain("model-from-phone"));
  });

  /** Defer `ui/settings/read` so a test can interleave a newer choice (theme 或值守). */
  function deferRead() {
    type ReadValue = { theme?: unknown; resident_daemon_watchman_enabled?: unknown };
    let resolveRead: (v: ReadValue) => void = () => {};
    state.dataSources["ui/settings/read"] = () =>
      new Promise((resolve) => {
        resolveRead = resolve;
      });
    return async (value: ReadValue) => {
      await act(async () => resolveRead(value));
    };
  }

  it("does not let the in-flight startup read clobber a ui/settings/updated theme", async () => {
    const resolveRead = deferRead();
    render(<App />);
    await settle();
    // read 在途：先跟随一条 ui/settings/updated。
    notify("ui/settings/updated", { theme: "light" });
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe("light"));
    // 迟到的权威 read 带回旧值 dark，必须被忽略。
    await resolveRead({ theme: "dark" });
    expect(document.documentElement.dataset.theme).toBe("light");
  });

  it("does not let the in-flight startup read clobber the user's theme choice", async () => {
    const resolveRead = deferRead();
    render(<App />);
    await settle();
    // read 在途：用户先在设置里选了浅色。
    fireEvent.click(screen.getByRole("button", { name: "设置" }));
    fireEvent.click(screen.getByRole("button", { name: "浅色" }));
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe("light"));
    // 迟到的权威 read 带回旧值 dark，必须被忽略。
    await resolveRead({ theme: "dark" });
    expect(document.documentElement.dataset.theme).toBe("light");
  });

  it("reverts the theme when ui/settings/write rejects", async () => {
    state.dataSources["ui/settings/write"] = () =>
      Promise.reject({ code: -32000, message: "disk full" });
    render(<App />);
    await settle();
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe("dark"));
    fireEvent.click(screen.getByRole("button", { name: "设置" }));
    fireEvent.click(screen.getByRole("button", { name: "浅色" }));
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "ui/settings/write",
        params: { theme: "light" },
      }),
    );
    // 写失败：UI 必须回退到之前的值，不能停在服务端并未接受的浅色上。
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe("dark"));
  });

  it("reverts to the last confirmed theme, not a stale optimistic value, when overlapping writes fail", async () => {
    // 两次写入都挂起，由测试按序决定各自的结局。
    const deferred: Array<{ reject: (e: unknown) => void }> = [];
    state.dataSources["ui/settings/write"] = () =>
      new Promise((_resolve, reject) => {
        deferred.push({ reject });
      });
    render(<App />);
    await settle();
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe("dark"));

    fireEvent.click(screen.getByRole("button", { name: "设置" }));
    fireEvent.click(screen.getByRole("button", { name: "浅色" }));
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe("light"));
    fireEvent.click(screen.getByRole("button", { name: "深色" }));
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe("dark"));
    await waitFor(() => expect(deferred.length).toBe(2));

    // 两次都失败，且旧的那次先回。服务端从未接受过任何一个写入，故权威值
    // 仍是 read 回来的 dark；UI 绝不能停在第二次调用时刻的乐观值 light 上。
    await act(async () => deferred[0].reject({ code: -32000, message: "first failed" }));
    await act(async () => deferred[1].reject({ code: -32000, message: "second failed" }));

    await waitFor(() => expect(document.documentElement.dataset.theme).toBe("dark"));
  });

  it("reads resident_daemon_watchman_enabled and writes it back when the panel toggles it", async () => {
    // 值守是宿主级设置，与主题同走 ui/settings/*（而不是带 project 的
    // plugin/query）：这里只是把这趟 read 的值 stub 成 false；默认口径其实是开
    // （配置里没这个键也当开），点一下要写回 true。
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    state.dataSources["ui/settings/read"] = () => ({ theme: "dark", resident_daemon_watchman_enabled: false });
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");

    const box = (await screen.findByLabelText("后台值守（开机自启）开关")) as HTMLInputElement;
    expect(box.checked).toBe(false);
    fireEvent.click(box);
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "ui/settings/write",
        params: { resident_daemon_watchman_enabled: true },
      }),
    );
  });

  it("reads the legacy watchman field when an older host only returns that one", async () => {
    // 新旧宿主未必同批升级：旧宿主只回旧字段名。读不到新名时必须回退到旧名，
    // 否则用户关掉的值守会被 parseBoardWatchman 的缺省（开）悄悄翻回开。
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    state.dataSources["ui/settings/read"] = () => ({ theme: "dark", board_watchman_enabled: false });
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");

    const box = (await screen.findByLabelText("后台值守（开机自启）开关")) as HTMLInputElement;
    expect(box.checked).toBe(false);
  });

  it("surfaces the watchman warning ui/settings/write returns", async () => {
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    state.dataSources["ui/settings/read"] = () => ({ theme: "dark", resident_daemon_watchman_enabled: true });
    // 写入落盘了，但宿主没能装上登录项：它把原因放在 warning 里，面板必须原样
    // 摆出来，而不是静默让人以为已经生效。
    state.dataSources["ui/settings/write"] = (params: unknown) =>
      (params as { resident_daemon_watchman_enabled?: boolean }).resident_daemon_watchman_enabled === false
        ? { warning: "未能在登录项中安装值守" }
        : {};
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");

    const box = (await screen.findByLabelText("后台值守（开机自启）开关")) as HTMLInputElement;
    expect(box.checked).toBe(true);
    fireEvent.click(box);

    expect(await screen.findByText("未能在登录项中安装值守")).toBeTruthy();
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "ui/settings/write",
        params: { resident_daemon_watchman_enabled: false },
      }),
    );
  });

  it("does not let an in-flight re-handshake read clobber a fresh watchman toggle", async () => {
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    // 两趟 read 都返回 true：迟到的旧值一旦被采纳，开关会弹回 true，断言即红。
    const resolveRead = deferRead();
    render(<App />);
    await settle();
    // 首屏 read 在途，面板还没法开——先把 handshake 放过去把面板拿到屏上。
    await resolveRead({ theme: "dark", resident_daemon_watchman_enabled: true });
    await openBoardFor("/proj");
    const box = (await screen.findByLabelText("后台值守（开机自启）开关")) as HTMLInputElement;
    expect(box.checked).toBe(true);

    // 新一轮 read 在途（这正是重启后重放握手发的那趟），此时面板已在屏上。
    const resolveStale = deferRead();
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 1 });
    });
    await waitFor(() => expect(clients.length).toBe(2));
    await waitFor(() =>
      expect(clients[1].requests.some((r) => r.method === "ui/settings/read")).toBe(true),
    );

    // read 在途：用户把值守关掉（写入返回成功，界面随之变为关）。
    fireEvent.click(screen.getByLabelText("后台值守（开机自启）开关"));
    await waitFor(() =>
      expect(clients[1].requests).toContainEqual({
        method: "ui/settings/write",
        params: { resident_daemon_watchman_enabled: false },
      }),
    );
    await waitFor(() =>
      expect((screen.getByLabelText("后台值守（开机自启）开关") as HTMLInputElement).checked).toBe(
        false,
      ),
    );

    // 迟到的权威 read 带回旧值 true，必须被忽略：用户的选择才是当前选择。
    await resolveStale({ theme: "dark", resident_daemon_watchman_enabled: true });
    expect((screen.getByLabelText("后台值守（开机自启）开关") as HTMLInputElement).checked).toBe(
      false,
    );
  });

  it("re-handshakes on a fresh client after the sidecar exits and the host restarts it", async () => {
    render(<App />);
    await settle();
    await waitFor(() => expect(clients.length).toBe(1));
    expect(clients[0].requests.some((r) => r.method === "thread/listAll")).toBe(true);

    // 宿主报告 sidecar 退出(bridge 会随即自动重启它)。App 必须建一个新客户端
    // 并重放握手,而不是让用户去重启 App。
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 1 });
    });
    await waitFor(() => expect(clients.length).toBe(2));

    await waitFor(() =>
      expect(clients[1].requests.some((r) => r.method === "initialize")).toBe(true),
    );
    await waitFor(() =>
      expect(clients[1].requests.some((r) => r.method === "thread/listAll")).toBe(true),
    );
  });
});

// 断线韧性:后端重启/网络闪断时 iOS 端不能只试一次就永远卡在 connecting。
// 每个用例都开假定时器,免得真的等 8s/10s 的退避。
describe("App reconnect resilience", () => {
  /** 断线(宿主报告 sidecar 退出)。 */
  const exit = () =>
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 1 });
    });

  /** 冲掉在途的微任务/React 更新,好让 `clients` 数组稳定下来。 */
  const flush = async () => {
    for (let i = 0; i < 3; i++) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(0);
      });
    }
  };

  /** 等首连接把 initialize 发出去(等价旧 suite 里的 settle)。 */
  const waitMounted = async () => {
    await flush();
    expect(clients[0].requests.some((r) => r.method === "initialize")).toBe(true);
  };

  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("schedules a reconnect after a disconnect (single shot is not enough)", async () => {
    render(<App />);
    await waitMounted();
    expect(clients.length).toBe(1);

    exit();
    // 退避尚未到点:不立刻重连(restartTimer 还没烧完)。
    expect(clients.length).toBe(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(nextReconnectDelay(0));
    });
    expect(clients.length).toBe(2);
    await flush();
    expect(clients[1].requests.some((r) => r.method === "initialize")).toBe(true);
  });

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

  it("does not double-apply notifications after a foreground reconnect", async () => {
    render(<App />);
    await waitMounted();
    await flush();
    expect(state.notifHandlers.length).toBe(1);

    // 切后台再回前台:App 重建客户端(新 transport + 新监听)。真实环境里旧
    // transport 在未 dispose 时会继续投递同一帧,于是逐字流被应用两次——文本
    // 重复,直到 item/completed 按 id 整条替换才"收敛"。
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await flush();
    expect(clients.length).toBe(2);
    // 旧监听必须已被注销,否则每条通知都会被应用两遍。
    expect(state.notifHandlers.length).toBe(1);

    act(() => {
      for (const cb of state.notifHandlers) {
        cb({ method: "item/delta", params: { thread_id: "t1", item_id: "a1", delta: "hello" } });
      }
    });
    await flush();
    expect(screen.getByText("hello")).toBeTruthy();
  });

  it("unlistens the dead client as soon as its transport exits", async () => {
    render(<App />);
    await waitMounted();
    await flush();
    expect(state.notifHandlers.length).toBe(1);

    // 断开(侧车退出)。在退避定时器到点之前,死客户端的监听就该已被摘掉:
    // 宿主随即拉起的新侧车会继续向同一事件名投递帧。
    exit();
    expect(state.notifHandlers.length).toBe(0);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(nextReconnectDelay(0));
    });
    await flush();
    expect(clients.length).toBe(2);
    expect(state.notifHandlers.length).toBe(1);
  });

  it("keeps retrying with increasing delays when attempts keep failing", async () => {
    // 卡住 initialize:每个新客户端都建得起来,却永远握不上手,等价于"后端
    // 还没起来"。这样每次重连都会在退避链上再排一轮,而不是止步于一次。
    state.dataSources["initialize"] = () => new Promise(() => {});
    render(<App />);
    await flush();
    expect(clients.length).toBe(1);
    expect(clients[0].requests.some((r) => r.method === "initialize")).toBe(true);

    exit(); // attempt 0 → 500ms
    // 499ms 还不到,必须仍然只有一个客户端。
    await act(async () => {
      await vi.advanceTimersByTimeAsync(nextReconnectDelay(0) - 1);
    });
    expect(clients.length).toBe(1);
    // 到点:第 2 个客户端出现,但它的握手仍卡着(initialize 未 resolve),
    // 于是它达不到 connected。
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(clients.length).toBe(2);

    // 第 2 次尝试同样失败:第 2 个客户端也报 exited。
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 1 });
    });
    // 下一次必须按 1000ms(而不是又一次 500ms)排队。
    await act(async () => {
      await vi.advanceTimersByTimeAsync(nextReconnectDelay(0));
    });
    expect(clients.length).toBe(2);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(nextReconnectDelay(1) - nextReconnectDelay(0));
    });
    expect(clients.length).toBe(3);

    // 第 3 次也一样:2000ms 后才轮到第 4 个客户端。
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 1 });
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(nextReconnectDelay(2) - 1);
    });
    expect(clients.length).toBe(3);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(clients.length).toBe(4);
  });

  it("resets the backoff after a successful handshake", async () => {
    render(<App />);
    await waitMounted(); // 第一趟握手走通:connected,attempt 归零
    expect(clients.length).toBe(1);

    // 一次断开 → 一次重连 → 再握手成功:回来后 attempt 又是 0。
    exit();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(nextReconnectDelay(0));
    });
    expect(clients.length).toBe(2);
    await flush();
    expect(clients[1].requests.some((r) => r.method === "thread/listAll")).toBe(true);
    expect(screen.getByText("connected")).toBeTruthy();

    // 成功之后的这一次断开,必须从 base(而不是被抬高过的档位)重新起算。
    exit();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(nextReconnectDelay(0) - 1);
    });
    expect(clients.length).toBe(2);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(clients.length).toBe(3);
  });

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

  it("reconnects immediately when the app returns to the foreground", async () => {
    render(<App />);
    await waitMounted();
    expect(clients.length).toBe(1);

    // 切到后台再回前台,而 socket 已死(状态 exited)。
    exit();
    expect(clients.length).toBe(1); // 退避还没到点
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "visible",
    });
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    // 前台唤醒 = 立刻重连,不等退避。
    expect(clients.length).toBe(2);
    await flush();
    expect(clients[1].requests.some((r) => r.method === "initialize")).toBe(true);
  });

  it("forces a fresh connect+handshake on foreground while the stale flag still says connected", async () => {
    render(<App />);
    await waitMounted();
    // 首连接握手成功:`connected` 已是 true(状态栏也这么说)。
    expect(clients.length).toBe(1);
    expect(screen.getByText("connected")).toBeTruthy();

    // iOS 场景:切后台期间 socket 被系统静默掐断,却没有 close/exited 事件,
    // 于是 `connected` 一直停在 true。回到前台时不能信这个陈旧标志,必须重建
    // 客户端重新握手——这正是本特性存在的理由。
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "visible",
    });
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });

    // 新客户端立刻出现(不等任何退避),并对它重放一整趟握手。
    expect(clients.length).toBe(2);
    await flush();
    expect(clients[1].requests.filter((r) => r.method === "initialize").length).toBe(1);
  });
});

describe("App iOS first-launch pairing", () => {
  const IPHONE_UA =
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";

  it("shows the pairing screen on iOS with no persisted config (never a dead-end transport)", () => {
    state.platformUa = IPHONE_UA;
    render(<App />);

    // 死路的定义就是「没有配置还去建 transport」：iOS 上那会落到 tauriTransport。
    expect(clients.length).toBe(0);
    expect(screen.getByRole("form", { name: "配对" })).toBeTruthy();
    expect(screen.getByLabelText("服务器地址")).toBeTruthy();
    expect(screen.getByLabelText("配对码")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "设置" })).toBeNull();
  });

  it("keeps the desktop app on the main view even with the same phone UA absent", () => {
    // 平台分支必须只认 iOS：桌面（UA 为空）照旧走握手，配对表单不出现。
    render(<App />);
    expect(screen.queryByRole("form", { name: "配对" })).toBeNull();
    expect(clients.length).toBe(1);
  });

  it("goes straight to the main app on iOS once a config is persisted", async () => {
    state.platformUa = IPHONE_UA;
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );
    render(<App />);

    await waitFor(() => expect(clients[0].requests.some((r) => r.method === "initialize")).toBe(true));
    expect(screen.queryByRole("form", { name: "配对" })).toBeNull();
  });

  it("pairs over the relay by redeeming on a frame, then connects", async () => {
    state.platformUa = IPHONE_UA;
    // The relay URL (?session=) cannot carry ?pair= (the relay forwards frames
    // but not the upgrade query), so the app must open a tokenless ws to the
    // relay and redeem via initialize -> pair/redeem. This fake speaks that
    // protocol and records the URL + frames so the test can assert the path.
    const opened: string[] = [];
    const frames: Array<{ id?: number; method?: string; params?: unknown }> = [];
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
        const frame = JSON.parse(raw) as { id?: number; method?: string; params?: unknown };
        frames.push(frame);
        if (frame.id === 1) {
          queueMicrotask(() =>
            this.onmessage?.({
              data: JSON.stringify({ jsonrpc: "2.0", id: 1, result: {} }),
            }),
          );
        } else if (frame.id === 2) {
          queueMicrotask(() =>
            this.onmessage?.({
              data: JSON.stringify({
                jsonrpc: "2.0",
                id: 2,
                result: { device_id: "dev-1", token: "yia_new", scope: "control" },
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
      fireEvent.change(screen.getByLabelText("服务器地址"), {
        target: { value: "wss://relay.test/ws?session=s1" },
      });
      fireEvent.change(screen.getByLabelText("配对码"), { target: { value: "ABCD-EFGH" } });
      fireEvent.click(screen.getByRole("button", { name: "配对" }));

      // The pairing socket is the relay URL **without** a ?pair= query.
      await waitFor(() => expect(opened.length).toBeGreaterThan(0));
      expect(opened[0]).toBe("wss://relay.test/ws?session=s1");
      // It redeemed on a frame, in order.
      expect(frames.map((f) => f.method)).toEqual(["initialize", "pair/redeem"]);
      expect((frames[1].params as { code: string }).code).toBe("ABCD-EFGH");

      // After persisting, App must rebuild the transport (version bump) and
      // start handshaking, not wait for a restart.
      await waitFor(() =>
        expect(clients[0].requests.some((r) => r.method === "initialize")).toBe(true),
      );
      const persisted = JSON.parse(localStorage.getItem("yi-agent.remote")!);
      expect(persisted).toEqual({ url: "wss://relay.test/ws?session=s1", token: "yia_new" });
      expect(screen.queryByRole("form", { name: "配对" })).toBeNull();
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("reports a failed redemption without persisting a config", async () => {
    state.platformUa = IPHONE_UA;
    render(<App />);
    fireEvent.change(screen.getByLabelText("服务器地址"), {
      target: { value: "wss://relay.test/ws" },
    });
    fireEvent.change(screen.getByLabelText("配对码"), { target: { value: "BAD-CODE" } });

    // 兑换必然失败（下面没有可用的 WebSocket）——重点是不能把界面留在"配对中"。
    fireEvent.click(screen.getByRole("button", { name: "配对" }));

    await waitFor(() => expect(screen.getByRole("button", { name: "配对" })).toBeTruthy());
    expect(screen.getByRole("alert")).toBeTruthy();
    expect(localStorage.getItem("yi-agent.remote")).toBeNull();
    expect(clients.length).toBe(0);
  });
});

describe("App mobile repair path", () => {
  const IPHONE_UA =
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";
  const RELAY = "wss://relay.test/ws?session=abc";

  /** 冲掉在途的微任务/React 更新。 */
  const settle = async () => {
    for (let i = 0; i < 3; i++) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(0);
      });
    }
  };

  /**
   * 已配对但连不上的手机。
   *
   * 必须悬停 initialize：否则 mock 的握手会立即成功、status 变成 connected，
   * `offline` 为 false，状态栏入口本就不该出现——用例会因错误的原因失败。
   */
  const pairedPhone = () => {
    state.platformUa = IPHONE_UA;
    localStorage.setItem("yi-agent.remote", JSON.stringify({ url: RELAY, token: "yia_tok" }));
    state.dataSources["initialize"] = () => new Promise(() => {});
  };

  // 本套用 `vi.advanceTimersByTimeAsync` 推进 React 更新，必须有假定时器。
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("shows a re-pair entry in the status bar while offline", async () => {
    pairedPhone();
    render(<App />);
    await settle();
    // 假定时器下 findBy* 的轮询不会自行推进，故 settle 后直接断言。
    expect(screen.getByRole("button", { name: "重新配对" })).toBeTruthy();
  });

  it("opens the pairing screen prefilled with the persisted url", async () => {
    pairedPhone();
    render(<App />);
    await settle();
    fireEvent.click(screen.getByRole("button", { name: "重新配对" }));

    expect(screen.getByRole("form", { name: "配对" })).toBeTruthy();
    expect((screen.getByLabelText("服务器地址") as HTMLInputElement).value).toBe(RELAY);
  });

  it("returns to the main view without clearing the persisted config", async () => {
    pairedPhone();
    render(<App />);
    await settle();
    fireEvent.click(screen.getByRole("button", { name: "重新配对" }));
    fireEvent.click(screen.getByRole("button", { name: "返回" }));

    expect(screen.queryByRole("form", { name: "配对" })).toBeNull();
    // 非破坏性：返回不清配置（默认清零发生在配对成功那一刻，不是这里）。
    expect(localStorage.getItem("yi-agent.remote")).not.toBeNull();
    expect(clients.length).toBe(1);
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
      await settle();
      expect(clients.length).toBe(1);
      fireEvent.click(screen.getByRole("button", { name: "重新配对" }));
      fireEvent.change(screen.getByLabelText("配对码"), { target: { value: "ABCD-EFGH" } });
      fireEvent.click(screen.getByRole("button", { name: "配对" }));

      // 重配成功后必须真的重建连接：inited 守卫已放开，新 client 重新握手。
      // 假定时器下 waitFor 不推进时间，故用 settle 冲掉微任务后再断言。
      await settle();
      expect(clients.length).toBe(2);
      expect(clients[1].requests.some((r) => r.method === "initialize")).toBe(true);
      expect(opened[0]).toBe(RELAY);
      expect(JSON.parse(localStorage.getItem("yi-agent.remote")!).token).toBe("yia_new");
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("opens the pairing screen automatically when the server rejects the token", async () => {
    pairedPhone();
    render(<App />);
    await settle();
    // 服务端以 4401 明确拒绝这个 token（ws.rs 的 close_unauthorized）：
    // 重试无意义，直接送上重配界面并说明原因。
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 4401 });
    });
    expect(screen.getByRole("form", { name: "配对" })).toBeTruthy();
    expect(screen.getByText(REPAIR_REMOVED_TEXT)).toBeTruthy();
  });

  it("leaves an ordinary disconnect to the backoff loop (no repair screen)", async () => {
    pairedPhone();
    render(<App />);
    await settle();
    // 普通断线（1006 等）：这是"网络抖了一下"，必须继续退避重连，不弹重配。
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 1006 });
    });
    expect(screen.queryByRole("form", { name: "配对" })).toBeNull();
  });

  it("does not re-open after the user dismisses an automatic offer", async () => {
    pairedPhone();
    render(<App />);
    await settle();
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 4401 });
    });
    expect(screen.getByRole("form", { name: "配对" })).toBeTruthy();

    // 用户明确「返回」：后台退避重连会再次撞上 4401，但不得把他弹回去——
    // 否则「返回」形同虚设（每轮重连都撤销一次用户的决定）。
    fireEvent.click(screen.getByRole("button", { name: "返回" }));
    expect(screen.queryByRole("form", { name: "配对" })).toBeNull();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(nextReconnectDelay(0) + 1);
    });
    await settle();
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 4401 });
    });
    expect(screen.queryByRole("form", { name: "配对" })).toBeNull();
  });

  it("re-arms the automatic offer after a successful connection", async () => {
    pairedPhone();
    // 先让握手真的成功一次（status -> connected），这会把自动提示重新武装。
    state.dataSources = {};
    render(<App />);
    await settle();
    // 假定时器下 waitFor 不推进时间，settle 后直接断言。
    expect(clients[0].requests.some((r) => r.method === "thread/listAll")).toBe(true);

    // 连接成功后凭据才失效（例如桌面撤销了这台设备）：此时应当自动提示。
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 4401 });
    });
    expect(screen.getByRole("form", { name: "配对" })).toBeTruthy();
  });

  it("never offers the entry on the desktop", async () => {
    render(<App />);
    await settle();
    expect(screen.queryByRole("button", { name: "重新配对" })).toBeNull();
  });
});

describe("deleting a thread that still runs subagents", () => {
  const deleteButton = () => screen.getByRole("button", { name: /delete thread/i });

  /** Reveal the sidebar row's controls, then click its Delete button. */
  async function clickDelete() {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/listAll")).toBe(true),
    );
    const row = screen.getByText("one");
    fireEvent.mouseEnter(row.closest("[data-thread-id]") ?? row);
    fireEvent.click(deleteButton());
  }

  it("asks the user first, and re-sends with force only after they agree", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    state.dataSources["thread/delete"] = () => ({
      status: "needs_confirmation",
      active_children: 2,
    });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    try {
      await clickDelete();

      // The backend refused; the UI must have asked, quoting how many are live.
      await waitFor(() => expect(confirm).toHaveBeenCalled());
      expect(String(confirm.mock.calls[0][0])).toContain("2");

      // Once agreed, the same delete is re-issued with force — nothing else.
      await waitFor(() =>
        expect(clients[0].requests).toContainEqual({
          method: "thread/delete",
          params: { threadId: "t1", force: true },
        }),
      );
    } finally {
      confirm.mockRestore();
    }
  });

  it("leaves the thread alone when the user declines", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    state.dataSources["thread/delete"] = () => ({
      status: "needs_confirmation",
      active_children: 1,
    });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    try {
      await clickDelete();

      await waitFor(() => expect(confirm).toHaveBeenCalled());
      // Declined: never force, and the thread is still listed.
      expect(clients[0].requests).not.toContainEqual({
        method: "thread/delete",
        params: { threadId: "t1", force: true },
      });
      expect(screen.getByText("one")).toBeTruthy();
    } finally {
      confirm.mockRestore();
    }
  });

  it("把 worktree 将被删除写进确认框（即便没有子代理）", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    // active_children 为 0：这一回要求确认的是 worktree，而非子代理。
    state.dataSources["thread/delete"] = () => ({
      status: "needs_confirmation",
      active_children: 0,
      worktree: {
        path: "/proj/.worktrees/kanban/card-1",
        action: "remove",
        reason: "worktree 有未提交改动，确认后将永久丢弃",
      },
    });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    try {
      await clickDelete();

      await waitFor(() => expect(confirm).toHaveBeenCalled());
      const message = String(confirm.mock.calls[0][0]);
      // 用户必须看见：删的是哪个目录、为什么、以及不可逆。
      expect(message).toContain("/proj/.worktrees/kanban/card-1");
      expect(message).toContain("永久丢弃");
      expect(message).toContain("继续？");

      // 同意后照旧以 force 重发。
      await waitFor(() =>
        expect(clients[0].requests).toContainEqual({
          method: "thread/delete",
          params: { threadId: "t1", force: true },
        }),
      );
    } finally {
      confirm.mockRestore();
    }
  });

  it("worktree 将被保留时如实告知，且不谎称删除", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    state.dataSources["thread/delete"] = () => ({
      status: "needs_confirmation",
      active_children: 0,
      worktree: {
        path: "/proj/not-a-worktree",
        action: "keep",
        reason: "cwd 不是本项目登记在册的看板 worktree，拒绝删除",
      },
    });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    try {
      await clickDelete();

      await waitFor(() => expect(confirm).toHaveBeenCalled());
      const message = String(confirm.mock.calls[0][0]);
      // 必须说"保留"，且绝不能反过来说"将一并删除"。
      expect(message).toContain("将保留");
      expect(message).not.toContain("将一并删除");
      expect(message).toContain("继续？");
    } finally {
      confirm.mockRestore();
    }
  });

  it("同意后 worktree 删除失败时弹窗告知，不让目录无声成为孤儿", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    state.dataSources["thread/delete"] = (p: any) =>
      p?.force
        ? {
            worktree: {
              action: "remove",
              removed: false,
              path: "/proj/.worktrees/kanban/card-1",
              reason: "fatal: cannot remove a locked working tree",
            },
          }
        : {
            status: "needs_confirmation",
            active_children: 0,
            worktree: {
              path: "/proj/.worktrees/kanban/card-1",
              action: "remove",
              reason: "worktree 有未提交改动，确认后将永久丢弃",
            },
          };
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    const alert = vi.spyOn(window, "alert").mockImplementation(() => {});
    try {
      await clickDelete();

      await waitFor(() => expect(alert).toHaveBeenCalled());
      const message = String(alert.mock.calls[0][0]);
      // 会话已被删除、无法在应用内重删，必须当场把残留路径与原因讲清楚。
      expect(message).toContain("/proj/.worktrees/kanban/card-1");
      expect(message).toContain("fatal: cannot remove a locked working tree");

      // 确认后照旧以 force 重发。
      expect(clients[0].requests).toContainEqual({
        method: "thread/delete",
        params: { threadId: "t1", force: true },
      });
    } finally {
      alert.mockRestore();
      confirm.mockRestore();
    }
  });

  it("无需确认的静默删除若 worktree 删除失败，也弹窗告知", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    state.dataSources["thread/delete"] = () => ({
      worktree: {
        action: "remove",
        removed: false,
        path: "/proj/.worktrees/kanban/card-1",
        reason: "boom",
      },
    });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    const alert = vi.spyOn(window, "alert").mockImplementation(() => {});
    try {
      await clickDelete();

      await waitFor(() => expect(alert).toHaveBeenCalled());
      expect(String(alert.mock.calls[0][0])).toContain("/proj/.worktrees/kanban/card-1");
      // 后端没要求确认，就不该多问一句。
      expect(confirm).not.toHaveBeenCalled();
    } finally {
      alert.mockRestore();
      confirm.mockRestore();
    }
  });

  it("worktree 正常删除时不弹多余提示", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    state.dataSources["thread/delete"] = (p: any) =>
      p?.force ? {} : { status: "needs_confirmation", active_children: 1 };
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    const alert = vi.spyOn(window, "alert").mockImplementation(() => {});
    try {
      await clickDelete();

      await waitFor(() =>
        expect(clients[0].requests).toContainEqual({
          method: "thread/delete",
          params: { threadId: "t1", force: true },
        }),
      );
      expect(alert).not.toHaveBeenCalled();
    } finally {
      alert.mockRestore();
      confirm.mockRestore();
    }
  });
});

describe("App remote subscription wiring (S2)", () => {
  /** Persist a relay binding, which is what makes `isRemoteClient()` true. */
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );

  it("subscribes to a thread when a remote client selects it", async () => {
    asRemote();
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/subscribe")).toBe(true),
    );
    const sub = clients[0].requests.find((r) => r.method === "thread/subscribe");
    // The window holds the selected thread; that is the whole warm set.
    expect((sub?.params as { threadIds: string[] }).threadIds).toContain("t1");
  });

  it("never subscribes on the desktop build", async () => {
    render(<App />); // no persisted config → desktop, even with a phone UA absent
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    expect(clients[0].requests.some((r) => r.method === "thread/subscribe")).toBe(false);
    expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(false);
  });

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

  it("hydrates the mode chip when the desktop opens a running thread", async () => {
    // 回归：本卡把「运行中→只读 readItems」提为两种客户端共有后，桌面端点开一个
    // 运行中的会话不再 resume，而 mode 过去只由 resume 回包/推送写入——于是 chip
    // 停在未知（"Mode"、禁用）。清单快照本就带 permission_mode，这里钉住它必须在
    // 这条路径上补上。卡会话跑 YOLO，桌面端点开时必须看得见 YOLO。
    state.listStatus = "running";
    state.mode = "yolo";
    state.dataSources["thread/readItems"] = () => ({ items: [] });
    render(<App />); // no persisted config → desktop

    await waitFor(() => expect(modeTrigger().textContent).toContain("YOLO"));
    // 仍然不打断：running 会话不 resume。
    expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(0);
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

  it("catches a cold running thread up with readItems instead of resume", async () => {
    asRemote();
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    // Every seeded thread reports running: a passive look must not interrupt it.
    state.listStatus = "running";
    state.dataSources["thread/readItems"] = () => ({ items: [] });
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/subscribe")).toBe(true),
    );

    fireEvent.click(screen.getByText("two")); // 冷 + running → readItems，不 resume

    await waitFor(() =>
      expect(
        clients[0].requests.some(
          (r) =>
            r.method === "thread/readItems" &&
            (r.params as { threadId: string }).threadId === "t2",
        ),
      ).toBe(true),
    );
    // The defining property: looking at a running thread never resumes it.
    expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(0);
  });
});

// 手机端布局：桌面是两列常驻，390pt 的屏幕上聊天区会被挤到看不清。手机把会话
// 侧栏改成抽屉（默认收起、选中即收），并隐藏子 agent 栏；桌面 build 完全不变。
describe("App mobile layout (phone client)", () => {
  /** 持久化 relay 绑定 —— 这正是 `isRemoteClient()` 为真的信号。 */
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );

  it("keeps the session drawer closed until the toggle opens it", async () => {
    asRemote();
    const { container } = render(<App />);
    // 首次渲染即带抽屉按钮（auto-select 尚未跑完时也已经能开抽屉）。
    await waitFor(() => expect(screen.getByLabelText("会话列表")).toBeTruthy());
    const sidebar = container.querySelector(".app-sidebar")!;
    expect(sidebar).not.toBeNull();
    expect(sidebar.className).not.toContain("sidebar-open");

    fireEvent.click(screen.getByLabelText("会话列表"));
    expect(container.querySelector(".app-sidebar")!.className).toContain("sidebar-open");

    // 点背板收起。
    fireEvent.click(container.querySelector(".app-sidebar-backdrop")!);
    expect(container.querySelector(".app-sidebar")!.className).not.toContain("sidebar-open");
  });

  it("closes the drawer once a thread is selected", async () => {
    asRemote();
    const { container } = render(<App />);
    await waitFor(() => expect(screen.getByLabelText("会话列表")).toBeTruthy());
    fireEvent.click(screen.getByLabelText("会话列表"));
    expect(container.querySelector(".app-sidebar")!.className).toContain("sidebar-open");

    fireEvent.click(await screen.findByText("one")); // 选中 t1
    expect(container.querySelector(".app-sidebar")!.className).not.toContain("sidebar-open");
  });

  it("leaves the desktop layout untouched", async () => {
    render(<App />); // 无持久化配置 → 桌面
    await waitFor(() => expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true));
    expect(screen.queryByLabelText("会话列表")).toBeNull();
  });

  it("floats the toggle over the chat column instead of a fixed rail", async () => {
    asRemote();
    render(<App />);
    const toggle = await screen.findByLabelText("会话列表");
    // 浮在主列内（.app-main），而不是一条 in-flow、通栏高的竖条列。
    expect(toggle.closest(".app-main")).not.toBeNull();
    expect(toggle.className).toContain("absolute");
    // 手机端唯一会话入口：44px 触控目标（Apple HIG）不能丢。
    expect(toggle.className).toContain("size-11");
    // 收起态：抽屉未打开时按钮必须报 aria-expanded="false"。
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
  });
});

// 包裹会话栏的 div 必须自带高度约束（flex + min-h-0）。它无条件存在，桌面端只是
// 没有手机类名；一旦退回块级容器，`aside` 会按内容取高并溢出这一行，把文档整体
// 撑高——内层 `flex-1 overflow-y-auto` 因此永不滚动，整页却能上下滑动（回归）。
describe("App sidebar wrapper constrains height", () => {
  const wrapperOf = (root: HTMLElement) =>
    root.querySelector('[aria-label="Resize sidebar"]')!.closest("aside")!.parentElement!;

  it("gives the desktop sidebar wrapper a flex height constraint", async () => {
    const { container } = render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const wrapper = wrapperOf(container);
    expect(wrapper.className).toContain("flex");
    expect(wrapper.className).toContain("min-h-0");
  });

  it("keeps the height constraint on the phone drawer wrapper too", async () => {
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );
    const { container } = render(<App />);
    await waitFor(() => expect(screen.getByLabelText("会话列表")).toBeTruthy());
    const wrapper = wrapperOf(container);
    expect(wrapper.className).toContain("app-sidebar");
    expect(wrapper.className).toContain("flex");
    expect(wrapper.className).toContain("min-h-0");
  });
});

// 中继路径的关键接缝：全 session 只有**一条**电脑侧桥接连接被所有手机共享。
// app-server 换进程后桥接重连会换成新 `ws-<uuid>`（`initialized=false`），而
// 手机的 socket 挂在中继上从未断开、不会自己重发 `initialize`，于是切 session
// 撞上 `-32010`。修法落在 `RpcClient.request`（收到 `-32010` 先重新握手再重发），
// 本套用例钉死 App 侧把「重新握手」正确注入了**当前**客户端。
describe("relay handshake recovery wiring", () => {
  /** A raw (no-auto-recovery) request fn that records what the hook replays. */
  const recorder = () => {
    const calls: Array<{ method: string; params: unknown }> = [];
    const raw = async (method: string, params: unknown) => {
      calls.push({ method, params });
      return {};
    };
    return { calls, raw };
  };

  it("injects a recovery hook that re-handshakes the current client", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "initialize")).toBe(true),
    );
    // 未注入该钩子，`RpcClient` 就无从在桥接重启后重新握手。
    expect(typeof clients[0].recover).toBe("function");

    const { calls, raw } = recorder();
    await act(async () => {
      await clients[0].recover!(raw);
    });
    expect(calls.some((c) => c.method === "initialize")).toBe(true);
  });

  it("targets the current client, not a disposed one, after a reconnect", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "initialize")).toBe(true),
    );

    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 1 });
    });
    await waitFor(() => expect(clients.length).toBe(2));
    await waitFor(() =>
      expect(clients[1].requests.some((r) => r.method === "initialize")).toBe(true),
    );

    // 钩子在调用时解析**当前**客户端：外层 lambda 必须把恢复交给 clientRef
    // 当时的那一个。第 2 个客户端调它时，不能打回已 dispose 的第 1 个。
    const { calls, raw } = recorder();
    await act(async () => {
      await clients[1].recover!(raw);
    });
    expect(calls.some((c) => c.method === "initialize")).toBe(true);
  });

  it("replays the subscription window so background threads keep streaming", async () => {
    // 经中继时新进程不记得任何订阅；只重发 initialize 会让后台会话静默停更。
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );
    render(<App />);
    // 远端首屏会选中第一个会话并订阅它。
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/subscribe")).toBe(true),
    );

    const { calls, raw } = recorder();
    await act(async () => {
      await clients[0].recover!(raw);
    });
    // 先握手，再重放订阅——顺序不能反（订阅在 initialize 前会被 -32010 拒）。
    expect(calls.map((c) => c.method)).toEqual(["initialize", "thread/subscribe"]);
    expect((calls[1].params as { threadIds: string[] }).threadIds).toContain("t1");
  });
});

// 状态栏图标管的是**右侧会话详情栏**（子 agent + Git Diff）的开合。它是与左侧
// 会话侧栏同级的独立列，取代了此前的「底部详情面板 + 独立子 agent 栏」两处：
//  - 栏：状态栏图标开，栏页眉的「关闭」关——两条路都作用于同一个 `panelOpen`；
//  - 子 agent 的卡片列表与轨迹详情都收在该栏的「子 agent」Tab 内（点卡片下钻、
//    「返回子 agent 列表」退回），不再有单独的「收起子 agent」按钮。
describe("App 会话详情栏的开合", () => {
  it("状态栏图标开合详情栏，栏页眉可关闭", async () => {
    render(<App />);
    await screen.findByLabelText("会话详情面板");

    // 栏初始不渲染：没有会话详情的 aside。
    expect(screen.queryByLabelText("会话详情")).toBeNull();

    const toggle = await screen.findByLabelText("会话详情面板");
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(toggle);
    expect(await screen.findByLabelText("会话详情")).toBeTruthy();
    expect(screen.getByRole("tab", { name: "子 agent" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Git Diff" })).toBeTruthy();
    expect(screen.getByLabelText("会话详情面板").getAttribute("aria-expanded")).toBe("true");

    fireEvent.click(screen.getByRole("button", { name: "关闭详情" }));
    expect(screen.queryByLabelText("会话详情")).toBeNull();
  });

  it("栏宽度可拖拽调整", async () => {
    render(<App />);
    fireEvent.click(await screen.findByLabelText("会话详情面板"));
    const section = (await screen.findByLabelText("会话详情")) as HTMLElement;
    const before = section.style.width;
    const handle = screen.getByRole("separator", { name: "调整会话详情栏宽度" });
    fireEvent.mouseDown(handle, { clientX: 800 });
    fireEvent.mouseMove(document, { clientX: 740 });
    fireEvent.mouseUp(document);
    expect(section.style.width).not.toBe(before);
  });

  it("手机端不渲染场景入口（那里栏与面板都是抽屉）", async () => {
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("会话列表")).toBeTruthy());
    expect(screen.queryByLabelText("会话详情面板")).toBeNull();
  });

  it("ui/gitDiff/focus 打开面板并切到 Git Diff，读出该会话的 diff", async () => {
    state.dataSources["thread/diff/read"] = () => ({
      base: "origin/main",
      baseKind: "origin-default",
      mergeBase: "abc1234",
      commits: [{ sha: "c1", short: "c1", subject: "add feature", author: "T", timestamp: 0 }],
      files: [{ path: "a.txt", status: "M", additions: 1, deletions: 1, binary: false }],
      unifiedDiff: "diff --git a/a.txt b/a.txt\n",
      truncated: false,
    });
    render(<App />);
    await screen.findByLabelText("会话详情面板");

    act(() => {
      for (const cb of state.notifHandlers) {
        cb({ method: "ui/gitDiff/focus", params: { threadId: "t1", note: "看这里" } });
      }
    });

    // 面板自己出现（不必先点图标），且停在 diff Tab。
    expect(await screen.findByLabelText("会话详情")).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Git Diff" }).getAttribute("aria-selected")).toBe("true");
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/diff/read",
        params: { threadId: "t1" },
      }),
    );
    // 推送里带来的提示语渲染在 diff 视图里。
    expect(await screen.findByText("看这里")).toBeTruthy();
  });

  it("ui/gitDiff/focus 同时取该会话的视图帧，并渲染模型给的命令输出", async () => {
    // 结构化快照给得出 commits/files（汇总行要它），但正文由**帧**说了算。
    state.dataSources["thread/diff/read"] = () => ({
      base: "origin/main",
      baseKind: "origin-default",
      mergeBase: "abc1234",
      commits: [],
      files: [{ path: "a.txt", status: "M", additions: 1, deletions: 1, binary: false }],
      unifiedDiff: "",
      truncated: false,
    });
    state.dataSources["thread/view/read"] = () => ({
      command: "git log --oneline -2",
      asKind: "log",
      exitCode: 0,
      stdout: "abc1234 fix a\ndef5678 add b\n",
      stderr: "",
      truncated: false,
      timedOut: false,
      note: null,
      base: null,
      baseKind: "",
    });
    render(<App />);
    await screen.findByLabelText("会话详情面板");

    act(() => {
      for (const cb of state.notifHandlers) {
        cb({ method: "ui/gitDiff/focus", params: { threadId: "t1", note: null } });
      }
    });

    await screen.findByLabelText("会话详情");
    // 帧被真的取了。
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/view/read",
        params: { threadId: "t1" },
      }),
    );
    // 命令条（命令 + 退出码）与 log 原语（提交列表）都画出来了。
    expect(await screen.findByText(/git log --oneline -2/)).toBeTruthy();
    expect(await screen.findByText("fix a")).toBeTruthy();
    expect(screen.getByText("add b")).toBeTruthy();
  });

  it("只有帧、没有结构化快照时，帧仍然渲染（不回落到空态）", async () => {
    // 帧路径与结构化快照是两条独立请求：这一条钉死「快照缺席不得吞掉帧」——
    // 否则 Task 8 的整条帧渲染路径在真实接线里永远走不到。
    state.dataSources["thread/diff/read"] = () => {
      throw { code: -1, message: "no structured diff" };
    };
    state.dataSources["thread/view/read"] = () => ({
      command: "git diff --stat",
      asKind: "text",
      exitCode: 0,
      stdout: " a.txt | 2 +-\n",
      stderr: "",
      truncated: false,
      timedOut: false,
      note: null,
      base: null,
      baseKind: "",
    });
    render(<App />);
    await screen.findByLabelText("会话详情面板");

    act(() => {
      for (const cb of state.notifHandlers) {
        cb({ method: "ui/gitDiff/focus", params: { threadId: "t1", note: null } });
      }
    });

    // 帧的文本原样可见，且不是「尚无 diff 数据」的空态。
    expect(await screen.findByText(/a\.txt \| 2 \+-/)).toBeTruthy();
    expect(screen.queryByText(/尚无 diff 数据/)).toBeNull();
  });

  it("单文件正文缺席时点标注补一次带 path 的 thread/diff/read（D1）", async () => {
    state.dataSources["thread/view/read"] = () => null; // 无帧：走结构化路径
    state.dataSources["thread/diff/read"] = (params) => {
      const p = params as { threadId: string; path?: string };
      if (p.path) {
        // 懒加载的 path 分支：给该文件的正文。
        return {
          unifiedDiff: "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n",
          truncated: false,
        };
      }
      // 会话级快照：文件列在 files[] 里，但正文被截断（unifiedDiff 空）。
      return {
        base: "origin/main",
        baseKind: "origin-default",
        mergeBase: "abc1234",
        commits: [],
        files: [{ path: "a.txt", status: "M", additions: 1, deletions: 1, binary: false }],
        unifiedDiff: "",
        truncated: true,
      };
    };
    render(<App />);
    await screen.findByLabelText("会话详情面板");

    act(() => {
      for (const cb of state.notifHandlers) {
        cb({ method: "ui/gitDiff/focus", params: { threadId: "t1", note: null } });
      }
    });
    await screen.findByLabelText("会话详情");

    // 正文缺席 -> 标注可点；点它触发一次带 path 的懒加载。
    fireEvent.click(await screen.findByRole("button", { name: /正文已截断/ }));
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/diff/read",
        params: { threadId: "t1", path: "a.txt" },
      }),
    );
  });

  it("点子 agent 卡片在栏内下钻到轨迹，可返回列表", async () => {
    state.dataSources["agent/children/list"] = () => ({
      children: [{ taskId: "task-1", objective: "整理日志", state: "running", lastStep: "读文件" }],
    });
    state.dataSources["agent/trace/read"] = () => ({
      rows: [
        {
          eventId: 1,
          taskId: "task-1",
          kind: "assistant_text",
          payloadJson: JSON.stringify({ type: "assistant_text", text: "开始工作" }),
        },
      ],
      highWaterId: 1,
    });
    render(<App />);
    // 卡片列表在「子 agent」Tab 内：打开栏即可见，点它下钻到轨迹。
    fireEvent.click(await screen.findByLabelText("会话详情面板"));
    fireEvent.click(await screen.findByRole("button", { name: "查看子 agent task-1" }));

    expect(await screen.findByText(/最近步骤/)).toBeTruthy();
    expect(clients[0].requests).toContainEqual({
      method: "agent/trace/read",
      params: { threadId: "t1", taskId: "task-1" },
    });

    // 返回列表：卡片重新出现，且该任务的流已停（换/退任务都要 unwatch）。
    fireEvent.click(screen.getByRole("button", { name: /返回子 agent 列表/ }));
    expect(await screen.findByRole("button", { name: "查看子 agent task-1" })).toBeTruthy();
    expect(screen.queryByText(/最近步骤/)).toBeNull();
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "agent/trace/unwatch",
        params: { threadId: "t1" },
      }),
    );
  });

  it("threadId 为 null 的 focus 落在当前会话上", async () => {
    state.dataSources["thread/diff/read"] = () => ({
      base: null,
      baseKind: "none",
      mergeBase: null,
      commits: [],
      files: [],
      unifiedDiff: "",
      truncated: false,
    });
    render(<App />);
    await screen.findByLabelText("会话详情面板");

    act(() => {
      for (const cb of state.notifHandlers) {
        cb({ method: "ui/gitDiff/focus", params: { threadId: null, note: null } });
      }
    });

    // 没有 threadId 时按 ref 回退到当前选中的 t1，而不是静默丢弃这次聚焦。
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/diff/read",
        params: { threadId: "t1" },
      }),
    );
  });

  // 推送的 diff 属于**哪条**会话，必须和它一起进画面。这两条覆盖同一个洞：
  // `ui/gitDiff/focus` 指名了另一条会话时，不能劫持用户正看着的这条——
  //  - 推送当下就切走当前会话？不，一条也不发、面板不动；
  //  - 更隐蔽的是「发出去时还是当前会话，回来时已经不是」（切走之后才到），
  //    此时快照一旦落盘，用户看到的既不是他选的那条，提示语也解释不了它。
  it("指名别的会话的 focus 不改动当前视图，也不发那次读", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    // 每条会话给得出可区分的 diff：万一真画上了，断言能指认是谁的。
    state.dataSources["thread/diff/read"] = (params) => ({
      base: null,
      baseKind: "none",
      mergeBase: null,
      commits: [],
      files: [
        {
          path: (params as { threadId: string }).threadId === "t2" ? "b.txt" : "a.txt",
          status: "M",
          additions: 1,
          deletions: 1,
          binary: false,
        },
      ],
      unifiedDiff: "",
      truncated: false,
    });
    render(<App />);
    await screen.findByLabelText("会话详情面板");
    // 启动自动选中 t1。先由用户自己把面板停在 Git Diff：这就有了「一个当前会话、
    // 一个可观察的面板状态」，投影来的外人改动必须一动不动。
    fireEvent.click(screen.getByLabelText("会话详情面板"));
    expect(await screen.findByLabelText("会话详情")).toBeTruthy();
    fireEvent.click(screen.getByRole("tab", { name: "Git Diff" }));
    expect(screen.getByRole("tab", { name: "Git Diff" }).getAttribute("aria-selected")).toBe(
      "true",
    );
    expect(
      clients[0].requests.filter(
        (r) => r.method === "thread/diff/read" && (r.params as { threadId?: string }).threadId === "t1",
      ).length,
    ).toBeGreaterThan(0);
    // 用户看的是 t1——t1 的 diff 已经画在这儿了（用它自己的文件名指认）。
    expect(await screen.findByText("a.txt")).toBeTruthy();

    act(() => {
      for (const cb of state.notifHandlers) {
        cb({
          method: "ui/gitDiff/focus",
          params: { threadId: "t2", note: "这是 t2 的提示" },
        });
      }
    });

    // 面板状态原封不动：还开着、还在 Git Diff，当前会话仍是 t1，提示语没落。
    expect(screen.getByLabelText("会话详情")).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Git Diff" }).getAttribute("aria-selected")).toBe(
      "true",
    );
    expect(screen.queryByText("这是 t2 的提示")).toBeNull();
    // 外人那条会话的 diff 一次都没读（推送当场就该被丢掉，别等写入时再拦）。
    expect(
      clients[0].requests.some(
        (r) => r.method === "thread/diff/read" && (r.params as { threadId?: string }).threadId === "t2",
      ),
    ).toBe(false);
    // 没被切会话：t2 的 resume 不该出现。
    expect(
      clients[0].requests.some(
        (r) => r.method === "thread/resume" && (r.params as { threadId?: string }).threadId === "t2",
      ),
    ).toBe(false);
  });

  it("在途读响应回来时用户已切走：不落盘、不挂提示语", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    // 两条会话的读都由测试自己放行：只有这样才能让 t1 的响应**恰好**落在
    // 用户切到 t2 之后，构造出那个「迟到响应」的窗口。
    const release: Record<string, (() => void) | null> = { t1: null, t2: null };
    state.dataSources["thread/diff/read"] = (params) => {
      const { threadId } = params as { threadId: string };
      return new Promise((resolve) => {
        release[threadId] = () =>
          resolve({
            base: null,
            baseKind: "none",
            mergeBase: null,
            commits: [],
            files: [
              {
                path: `${threadId}-diff.txt`,
                status: "M",
                additions: 1,
                deletions: 1,
                binary: false,
              },
            ],
            unifiedDiff: "",
            truncated: false,
          });
      });
    };
    render(<App />);
    await screen.findByLabelText("会话详情面板");
    // 用户打开面板并停在 Git Diff：对 t1 的读取发出去，一直悬着。
    fireEvent.click(screen.getByLabelText("会话详情面板"));
    fireEvent.click(await screen.findByRole("tab", { name: "Git Diff" }));
    await waitFor(() => expect(release.t1).toBeTruthy());

    // 其间用户切到 t2：t1 的读取还在途。
    fireEvent.click(screen.getByText("two"));
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/resume",
        params: { threadId: "t2" },
      }),
    );
    expect(screen.queryByLabelText("会话详情")).toBeNull();

    // t1 的响应此刻才回来：必须被丢弃——不许把 t1 的改动画进 t2 的视图。
    await act(async () => {
      release.t1!();
    });
    expect(screen.queryByText("t1-diff.txt")).toBeNull();

    // 重新打开面板并停在 Git Diff：读的是 t2，且在 t2 的响应回来之前，
    // 那块地方绝不能显示 t1 那份迟到内容（t2 的读这里同样挂着）。
    fireEvent.click(screen.getByLabelText("会话详情面板"));
    fireEvent.click(await screen.findByRole("tab", { name: "Git Diff" }));
    await waitFor(() => expect(release.t2).toBeTruthy());
    expect(screen.queryByText("t1-diff.txt")).toBeNull();

    // 放行 t2：画出来的该是 t2 自己的改动。
    await act(async () => {
      release.t2!();
    });
    expect(await screen.findByText("t2-diff.txt")).toBeTruthy();
    expect(screen.queryByText("t1-diff.txt")).toBeNull();
  });
});

describe("App 关联文件夹接线", () => {
  /** 打开某个目录分组的右键菜单。 */
  const openGroupMenu = (ws: string) => {
    fireEvent.contextMenu(document.querySelector<HTMLElement>(`[data-ws-header="${ws}"]`)!);
  };

  it("links the picked folder to the group it was invoked from", async () => {
    state.groupWorkspaces = ["/proj"];
    state.picks = ["/proj/docs"];
    render(<App />);
    await waitFor(() =>
      expect(document.querySelector('[data-ws-header="/proj"]')).not.toBeNull(),
    );

    openGroupMenu("/proj");
    fireEvent.click(screen.getByText("关联文件夹…"));

    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "workspace/link",
        params: { primary: "/proj", child: "/proj/docs" },
      }),
    );
  });

  it("does nothing when the native picker is cancelled", async () => {
    state.groupWorkspaces = ["/proj"];
    state.picks = [null];
    render(<App />);
    await waitFor(() =>
      expect(document.querySelector('[data-ws-header="/proj"]')).not.toBeNull(),
    );

    openGroupMenu("/proj");
    fireEvent.click(screen.getByText("关联文件夹…"));

    await waitFor(() => expect(state.picks).toHaveLength(0));
    expect(clients[0].requests.some((r) => r.method === "workspace/link")).toBe(false);
  });

  it("unlinks a folder from its linked-child section menu", async () => {
    state.groupWorkspaces = ["/proj", "/proj/docs"];
    state.dataSources["workspace/list"] = () => ({
      workspaces: [
        { path: "/proj", exists: true, linked_to: null },
        { path: "/proj/docs", exists: true, linked_to: "/proj" },
      ],
    });
    render(<App />);
    await waitFor(() =>
      expect(document.querySelector('[data-linked-child="/proj/docs"]')).not.toBeNull(),
    );

    fireEvent.contextMenu(
      document.querySelector<HTMLElement>('[data-linked-child="/proj/docs"]')!,
    );
    fireEvent.click(screen.getByText("取消关联"));

    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "workspace/unlink",
        params: { child: "/proj/docs" },
      }),
    );
  });

  it("surfaces a server-side link rejection instead of pretending it worked", async () => {
    // 服务端会拒掉成环 / 链式关联（-32602）。吞掉它就等于让用户以为关联成功。
    state.groupWorkspaces = ["/proj"];
    state.picks = ["/proj/outer"];
    state.dataSources["workspace/link"] = () => {
      throw { code: -32602, message: "主根本身已是附属根" };
    };
    render(<App />);
    await waitFor(() =>
      expect(document.querySelector('[data-ws-header="/proj"]')).not.toBeNull(),
    );

    openGroupMenu("/proj");
    fireEvent.click(screen.getByText("关联文件夹…"));

    await screen.findByText("主根本身已是附属根");
  });
});

describe("App 附件接线", () => {
  // 文档与图片共用一个入口（合并后只有一个「附加文件」按钮）。
  const attachButton = () => screen.getByRole("button", { name: "附加文件" });
  const sendButton = () => screen.getByRole("button", { name: /^send$/i });

  /** 点附件入口触发原生多选，并等界面吸收结果（chips 或错误横幅）。 */
  async function pickAttachments(picked: string | string[] | null) {
    state.picks = [picked];
    await act(async () => {
      fireEvent.click(attachButton());
    });
  }

  /**
   * 断言待发图片 chip 上的缩略图是**中性占位**，而不是画出来的 `<img>`。
   *
   * 待发图片的路径来自 OS 文件对话框，是 `/tmp/...` 这类**绝对**路径，而
   * `image/read` 以会话 cwd 为根（安全边界），必然读不回来——这是预期内的事实。
   * 故此处的期望就是占位：`attachment-thumb` 锚点存在但**不是** `<img>`；那个
   * 假「读到字节」的结果并不代表真实行为，不能替它渲染一张图。真正可读的路径
   * （服务端回显的 workspace 相对引用）才该画出 `<img>`，由 Task 11 覆盖。
   */
  function expectThumbPlaceholder() {
    const thumb = screen.getByTestId("attachment-thumb");
    expect(thumb.tagName).not.toBe("IMG");
  }

  it("sends attachments as input blocks before the text block", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    await pickAttachments(["/tmp/报告.pdf"]);
    // 多选回传一个数组：chip 是"确实进了待发列表"的可见证据。
    expect(screen.getByText("报告.pdf")).toBeTruthy();

    await act(async () => {
      fireEvent.change(screen.getByRole("textbox"), { target: { value: "总结一下" } });
      fireEvent.click(sendButton());
    });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    const call = clients[0].requests.find((r) => r.method === "turn/start");
    // 顺序是契约：所有附件 block 必须在文本 block **之前**。
    expect(call?.params).toEqual({
      threadId: "t1",
      input: [
        { type: "attachment", path: "/tmp/报告.pdf" },
        { type: "text", text: "总结一下" },
      ],
    });
  });

  it("sends a pending image as an image block before the text block", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // 图片走它自己的按钮（Task 10）：chip 出现即「图片确实进了待发列表」的可见
    // 证据；chip 上的缩略图（此处是占位）证明 kind 被记成了 "image"。
    //
    // 待发图片的路径来自 dialog，是 `/tmp/...` 这种**绝对**路径，`image/read` 以
    // 会话 cwd 为根（安全边界）必然拒绝它——桩客户端默认对未知方法回 `{}`，那会
    // 被伪造成一张空的「成功」图。这里照实模拟该拒绝，chip 才走到中性占位。
    state.rejectCode["image/read"] = -1;
    await pickAttachments(["/tmp/截图.png"]);
    expect(screen.getByText("截图.png")).toBeTruthy();
    expectThumbPlaceholder();

    await act(async () => {
      fireEvent.change(screen.getByRole("textbox"), { target: { value: "看看这个" } });
      fireEvent.click(sendButton());
    });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    const call = clients[0].requests.find((r) => r.method === "turn/start");
    // 图片走内容块（`{type:"image", path}`），与文档附件一样排在文本**之前**；
    // 服务端按顺序读，图片先入场，文本里只留问题本身。
    expect(call?.params).toEqual({
      threadId: "t1",
      input: [
        { type: "image", path: "/tmp/截图.png" },
        { type: "text", text: "看看这个" },
      ],
    });
  });

  it("sends images before documents before the text block, and echoes only documents", async () => {
    const { container } = render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // 同上一例：模拟 `image/read` 对待发绝对路径的拒绝（否则空桩会被伪造成图）。
    state.rejectCode["image/read"] = -1;
    // 一次选择里混选文档与图片，是合并入口的核心路径：分类由扩展名决定。
    await pickAttachments(["/tmp/报告.pdf", "/tmp/截图.png"]);
    expect(screen.getByText("截图.png")).toBeTruthy();
    expect(screen.getByText("报告.pdf")).toBeTruthy();
    expectThumbPlaceholder();

    await act(async () => {
      fireEvent.change(screen.getByRole("textbox"), { target: { value: "看看这些" } });
      fireEvent.click(sendButton());
    });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    // 顺序是契约：图片块、文档块，最后才是文本块。
    expect(clients[0].requests.find((r) => r.method === "turn/start")?.params).toEqual({
      threadId: "t1",
      input: [
        { type: "image", path: "/tmp/截图.png" },
        { type: "attachment", path: "/tmp/报告.pdf" },
        { type: "text", text: "看看这些" },
      ],
    });

    // 乐观回显只带文档：图片由服务端在回显的 item 上带 `images` 引用（本地此刻
    // 不知道 media_type/size）。文档 chip 出现在气泡里，图片名不在。
    const bubbles = Array.from(container.querySelectorAll("div.self-end")).filter((d) =>
      Array.from(d.childNodes).some(
        (n) => n.nodeType === Node.TEXT_NODE && n.textContent === "看看这些",
      ),
    );
    expect(bubbles).toHaveLength(1);
    expect(within(bubbles[0] as HTMLElement).getByText("报告.pdf")).toBeTruthy();
    expect(within(bubbles[0] as HTMLElement).queryByText("截图.png")).toBeNull();
  });

  it("surfaces a local pre-check failure instead of sending", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // `.mov` 不在 `ATTACHMENT_EXTENSIONS` 里，本地预检必须就地拒绝。
    await pickAttachments(["/tmp/演示.mov"]);

    // 拒绝原因写进会话错误（渲染成错误横幅），没有变成 chip。
    await waitFor(() => expect(screen.getByText(/不支持的文件类型/)).toBeTruthy());
    expect(screen.queryByText("演示.mov")).toBeNull();

    // 仅凭失效的附件不能发送：附件没进待发列表，文本框为空则 Send 禁用。
    expect((sendButton() as HTMLButtonElement).disabled).toBe(true);
    expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false);
  });

  it("sends with attachments alone and no text", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    await pickAttachments(["/tmp/报告.pdf"]);
    await waitFor(() => expect(screen.getByText("报告.pdf")).toBeTruthy());

    // 附件本身即消息：文本框留空，Send 也必须可用。
    expect((sendButton() as HTMLButtonElement).disabled).toBe(false);
    await act(async () => {
      fireEvent.click(sendButton());
    });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    const call = clients[0].requests.find((r) => r.method === "turn/start");
    expect(call?.params).toEqual({
      threadId: "t1",
      input: [
        { type: "attachment", path: "/tmp/报告.pdf" },
        { type: "text", text: "" },
      ],
    });
  });

  it("keeps the attachments for retry when the send fails, and rolls back the bubble", async () => {
    // 非 -32012/-32013 的失败：不会自愈重试，直接走失败路径。
    state.rejectCode = { "turn/start": -1 };
    const { container } = render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    await pickAttachments(["/tmp/报告.pdf"]);
    await waitFor(() => expect(screen.getByText("报告.pdf")).toBeTruthy());

    await act(async () => {
      fireEvent.change(screen.getByRole("textbox"), { target: { value: "总结一下" } });
      fireEvent.click(sendButton());
    });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    // 乐观气泡回滚：失败的发送不能留下一条幻觉用户消息。
    // 只看用户气泡本身——不能扫 div 的 textContent（输入框也叫"总结一下"），
    // 也不能靠文本相等（带附件的气泡含 chip 子元素，textContent 不再等于纯文本）。
    const bubbles = () =>
      Array.from(container.querySelectorAll("div.self-end")).filter((d) =>
        Array.from(d.childNodes).some(
          (n) => n.nodeType === Node.TEXT_NODE && n.textContent === "总结一下",
        ),
      );
    await waitFor(() => expect(bubbles()).toHaveLength(0));
    // 但附件**留着**：用户的文件必须能直接重试，而不是重新挑一遍。
    expect(screen.getByText("报告.pdf")).toBeTruthy();
  });

  it("never routes an attachment send through turn/interject", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    // 服务端说这一轮在跑：纯文本本来会走 turn/interject。
    state.notifHandlers[0]({
      method: "thread/status/updated",
      params: { thread_id: "t1", status: "running" },
    });

    await pickAttachments(["/tmp/报告.pdf"]);
    await waitFor(() => expect(screen.getByText("报告.pdf")).toBeTruthy());

    await act(async () => {
      fireEvent.change(screen.getByRole("textbox"), { target: { value: "总结一下" } });
      fireEvent.click(sendButton());
    });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    // turn/interject 的入参只认文本，附件会被静默丢掉；带附件必须改走 turn/start。
    expect(clients[0].requests.map((r) => r.method)).not.toContain("turn/interject");
    expect(
      clients[0].requests.find((r) => r.method === "turn/start")?.params,
    ).toEqual({
      threadId: "t1",
      input: [
        { type: "attachment", path: "/tmp/报告.pdf" },
        { type: "text", text: "总结一下" },
      ],
    });
  });

  it("clears the pending attachments only after a successful send", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    await pickAttachments(["/tmp/报告.pdf"]);
    await waitFor(() => expect(screen.getByText("报告.pdf")).toBeTruthy());

    await act(async () => {
      fireEvent.change(screen.getByRole("textbox"), { target: { value: "总结一下" } });
      fireEvent.click(sendButton());
    });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    // 发送成功：chip 行清空，不会把同一个文件重复带去下一轮。
    // 名字此时仍在 transcript 的用户气泡上（Task 11），所以断言要限定在待发 chip 行。
    await waitFor(() => expect(screen.queryByTestId("attachment-chips")).toBeNull());
  });

  it("keeps each thread's pending attachments out of the others", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    await pickAttachments(["/tmp/报告.pdf"]);
    await waitFor(() => expect(screen.getByText("报告.pdf")).toBeTruthy());

    // 切到 t2：t1 的附件不跟过来。
    fireEvent.click(screen.getByText("two"));
    await waitFor(() =>
      expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(2),
    );
    expect(screen.queryByText("报告.pdf")).toBeNull();

    // 切回 t1（warm）：附件还在原处，等着被发送。
    fireEvent.click(screen.getByText("one"));
    await waitFor(() => expect(screen.getByText("报告.pdf")).toBeTruthy());
  });

  it("removes a single chip without disturbing the others", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    await pickAttachments(["/tmp/a.pdf", "/tmp/b.md"]);
    await waitFor(() => expect(screen.getByText("a.pdf")).toBeTruthy());
    expect(screen.getByText("b.md")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "移除 a.pdf" }));

    expect(screen.queryByText("a.pdf")).toBeNull();
    expect(screen.getByText("b.md")).toBeTruthy();

    // 只有剩下的那个会进发送参数，且仍在文本 block 之前。
    await act(async () => {
      fireEvent.change(screen.getByRole("textbox"), { target: { value: "b のみ" } });
      fireEvent.click(sendButton());
    });
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    expect(
      clients[0].requests.find((r) => r.method === "turn/start")?.params,
    ).toEqual({
      threadId: "t1",
      input: [
        { type: "attachment", path: "/tmp/b.md" },
        { type: "text", text: "b のみ" },
      ],
    });
  });
});

describe("App first-run onboarding gate", () => {
  it("shows the onboarding wizard when the host reports needed", async () => {
    state.dataSources["onboarding/status"] = () => ({
      needed: true,
      dismissed: false,
      reasons: ["api_key"],
    });
    render(<App />);
    // 握手完成后向导应盖住主界面。
    expect(await screen.findByText("欢迎使用 Yi-Agent")).toBeTruthy();
  });

  it("skips the wizard when the host reports ready", async () => {
    state.dataSources["onboarding/status"] = () => ({
      needed: false,
      dismissed: false,
      reasons: [],
    });
    render(<App />);
    // 等到握手真正走完（resume 已发出），向导仍不出现。
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    expect(screen.queryByText("欢迎使用 Yi-Agent")).toBeNull();
  });

  it("closes the wizard after a successful apply (models.json only)", async () => {
    state.dataSources["onboarding/status"] = () => ({
      needed: true,
      dismissed: false,
      reasons: ["api_key"],
    });
    // 单写清单：宿主回 { ok, name }。没有「部分成功」态，故不该出现任何收尾横幅。
    state.dataSources["onboarding/apply"] = () => ({ ok: true, name: "gpt-4o" });
    // 门禁：保存按钮只有在连接测试通过后才可点。
    state.dataSources["onboarding/test"] = () => ({ ok: true, reason: null });
    render(<App />);
    // 走完向导：欢迎 → 选格式 → 填必填项 → 测试通过 → 保存。
    fireEvent.click(await screen.findByRole("button", { name: /开始/ }));
    fireEvent.click(screen.getByRole("button", { name: "openai" }));
    fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
    fireEvent.change(await screen.findByLabelText("API 密钥"), { target: { value: "sk-x" } });
    fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt-4o" } });
    fireEvent.click(screen.getByRole("button", { name: /测试连接/ }));
    await screen.findByText("连接正常");
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));

    // 向导退场，且没有遗留的「未纳入模型清单」横幅。
    await waitFor(() => expect(screen.queryByText("欢迎使用 Yi-Agent")).toBeNull());
    expect(screen.queryByText(/未纳入模型清单/)).toBeNull();
  });
});

// 远端（iOS）图片：没有共享文件系统，也不该弹 Tauri dialog——入口是 webview 自带的
// `<input type="file">`，选中的字节经 `image/upload/*` 分片送到服务端，待发附件记的是
// **服务端返回的工作区相对路径**（`image/read` 以会话 cwd 为根，故 chip 缩略图能读回）。
describe("App 远端图片上传", () => {
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );
  // 远端没有文档附件：合并后的单一入口在远端回落为图片选择器。
  const attachButton = () => screen.getByRole("button", { name: "附加文件" });
  const sendButton = () => screen.getByRole("button", { name: /^send$/i });

  /**
   * 远端（iOS）没有 Tauri dialog：附件入口会当场造一个 `<input type="file">`
   * 挂进 body、点了它，再在 change 时摘掉。这里等它出现，把选中的 File 塞进 `files`
   * 并派发 change。
   */
  async function pickImagesRemote(files: File[]) {
    await act(async () => {
      fireEvent.click(attachButton());
    });
    const input = await waitFor(() => {
      const el = document.body.querySelector('input[type="file"]');
      expect(el).not.toBeNull();
      return el as HTMLInputElement;
    });
    // 系统相册的选择器由这两个属性决定：只收图片、可多选。
    expect(input.accept).toBe("image/*");
    expect(input.multiple).toBe(true);
    await act(async () => {
      fireEvent.change(input, { target: { files } });
    });
  }

  /** 一个会说 `image/upload/*` 的假服务端（与 `imageUpload.test.ts` 同形）。 */
  function uploadServer(opts: { failBeginFor?: string } = {}) {
    /** 每次 begin→commit 记录一行，用来断言「送上去的是哪个文件的哪些字节」。 */
    const uploads: Array<{ name: string; mime: string; size: number; bytes: number[] }> = [];
    let seq = 0;
    let staging: { name: string; size: number; received: number[] } | null = null;
    state.dataSources["image/upload/begin"] = (params) => {
      const p = params as { name: string; mime: string; size: number };
      if (opts.failBeginFor && p.name.includes(opts.failBeginFor)) {
        throw { code: -32602, message: "begin refused" };
      }
      staging = { name: p.name, size: p.size, received: [] };
      uploads.push({ name: p.name, mime: p.mime, size: p.size, bytes: [] });
      return { uploadId: `u${++seq}`, chunkSize: 512 * 1024 };
    };
    state.dataSources["image/upload/chunk"] = (params) => {
      const p = params as { index: number; data: string };
      const bin = atob(p.data);
      for (let i = 0; i < bin.length; i += 1) staging!.received.push(bin.charCodeAt(i));
      return {};
    };
    state.dataSources["image/upload/commit"] = () => {
      const path = `.yi-agent/attachments/t1/ab-${staging!.name}`;
      uploads[uploads.length - 1].bytes = staging!.received;
      return { path, mediaType: "image/png", size: staging!.size };
    };
    state.dataSources["image/upload/abort"] = () => ({});
    return { uploads };
  }

  /**
   * 装好 HEIC→JPEG 转码环境（与 `imageUpload.test.ts` 的桩同形）。
   *
   * iOS 相册默认给 HEIC，转码走 `createImageBitmap` + canvas；jsdom 两样都没有，
   * 所以这里造一个只会回 JPEG 的桩，让「HEIC 也能进上传」这条路在测试里真的跑通。
   * 返回的 `restore` 必须在本用例结束时调用（否则桩会漏给同文件后续用例）。
   */
  function stubHeicTranscoder() {
    const bitmap = { width: 4, height: 3, close: vi.fn() };
    const createImageBitmap = vi.fn(async () => bitmap as unknown as ImageBitmap);
    vi.stubGlobal("createImageBitmap", createImageBitmap);
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({
      drawImage: vi.fn(),
    } as unknown as CanvasRenderingContext2D);
    const jpeg = new Blob([new Uint8Array([0xff, 0xd8, 0xff, 0xe0, 1, 2, 3])], {
      type: "image/jpeg",
    });
    const toBlob = vi.spyOn(HTMLCanvasElement.prototype, "toBlob").mockImplementation((cb: BlobCallback) => {
      cb(jpeg);
    });
    return {
      createImageBitmap,
      toBlob,
      restore: () => {
        toBlob.mockRestore();
        vi.restoreAllMocks();
        vi.unstubAllGlobals();
      },
    };
  }

  it("transcodes a picked .HEIC and uploads it as JPEG (iOS default camera format)", async () => {
    asRemote();
    const server = uploadServer();
    const t = stubHeicTranscoder();
    try {
      render(<App />);
      await waitFor(() =>
        expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
      );

      // iPhone 相机的默认格式：扩展名与 MIME 都是 HEIC。不转码的话服务端不认，
      // 但**也不能在客户端预检就被当「不支持的图片类型」拦下**——那会让这条路永
      // 远走不到转码。
      await pickImagesRemote([
        new File([new Uint8Array([0xa0, 0xa1, 0xa2, 0xa3])], "IMG_1234.HEIC", {
          type: "image/heic",
        }),
      ]);

      // chip 出现即「上传完成、进了待发列表」。chip 上的名字是**用户选中的文件名**
      // （与报告里 pending 名的既有行为一致，属展示层）；转码成 JPEG 这件事由下面
      // 的 `begin` 名字/MIME 与路径证明。
      await waitFor(() => expect(screen.getByText("IMG_1234.HEIC")).toBeTruthy());
      expect(screen.getByTitle(".yi-agent/attachments/t1/ab-IMG_1234.jpg")).toBeTruthy();
      // 服务端收到的是**转码后的** JPEG：名字换成 .jpg，MIME 换成 image/jpeg。
      expect(server.uploads).toEqual([
        { name: "IMG_1234.jpg", mime: "image/jpeg", size: 7, bytes: [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3] },
      ]);

      await act(async () => {
        fireEvent.click(sendButton());
      });
      await waitFor(() =>
        expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
      );
      expect(clients[0].requests.find((r) => r.method === "turn/start")?.params).toEqual({
        threadId: "t1",
        // 手机端的 turn/start 现在带上下文锚点（Task 12，spec §7.1）：该会话尚未
        // 对过账，故 contentEpoch 为初始 0、游标为 null。
        contextAnchor: { contentEpoch: 0, lastItemId: null },
        input: [{ type: "uploaded_image", uploadId: "u1" }, { type: "text", text: "" }],
      });
    } finally {
      t.restore();
    }
  });

  it("uploads picked Files and assembles an uploaded_image block on send", async () => {
    asRemote();
    const server = uploadServer();
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    await pickImagesRemote([new File([new Uint8Array([1, 2, 3])], "截图.png", { type: "image/png" })]);

    // chip 出现即「上传完成、进了待发列表」；名字取自选中的文件。
    expect(await screen.findByText("截图.png")).toBeTruthy();
    expect(server.uploads).toEqual([
      { name: "截图.png", mime: "image/png", size: 3, bytes: [1, 2, 3] },
    ]);
    // begin → chunk → commit，各一次（小文件一块）。
    const methods = clients[0].requests.map((r) => r.method);
    expect(methods.filter((m) => m === "image/upload/begin")).toHaveLength(1);
    expect(methods.filter((m) => m === "image/upload/chunk")).toHaveLength(1);
    expect(methods.filter((m) => m === "image/upload/commit")).toHaveLength(1);

    await act(async () => {
      fireEvent.change(screen.getByRole("textbox"), { target: { value: "看看这个" } });
      fireEvent.click(sendButton());
    });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    // 已上传的图片按**句柄**送（`{type:"uploaded_image", uploadId}`），路径只有本地
    // 渲染用（桌面端那条 `{type:"image", path}` 一个字都没变）。
    expect(clients[0].requests.find((r) => r.method === "turn/start")?.params).toEqual({
      threadId: "t1",
      // 手机端 turn/start 携带上下文锚点（Task 12，spec §7.1）。
      contextAnchor: { contentEpoch: 0, lastItemId: null },
      input: [
        { type: "uploaded_image", uploadId: "u1" },
        { type: "text", text: "看看这个" },
      ],
    });
  });

  it("keeps the successful files and reports the failed ones in place", async () => {
    asRemote();
    uploadServer({ failBeginFor: "坏的" });
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // 一次多选里混着好坏是常态：坏的只报它自己，好的照样入列。
    await pickImagesRemote([
      new File([new Uint8Array([1, 2, 3])], "好的.png", { type: "image/png" }),
      new File([new Uint8Array([4, 5, 6])], "坏的.png", { type: "image/png" }),
    ]);

    await waitFor(() => expect(screen.getByText(/begin refused/)).toBeTruthy());
    expect(screen.getByText("好的.png")).toBeTruthy();
    expect(screen.queryByText("坏的.png")).toBeNull();
  });

  it("still uses the plugin-dialog path on the desktop build", async () => {
    uploadServer();
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // 桌面（无 relay 绑定）走原生对话框，与既有行为逐字不变。
    state.picks = [["/tmp/截图.png"]];
    await act(async () => {
      fireEvent.click(attachButton());
    });

    expect(screen.getByText("截图.png")).toBeTruthy();
    expect(clients[0].requests.some((r) => r.method.startsWith("image/upload/"))).toBe(false);
  });
});

describe("App 输入框提示按平台分流", () => {
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );

  it("shows the newline hint (no Shift+Enter) on the phone client", async () => {
    asRemote();
    render(<App />);
    const textarea = await screen.findByRole("textbox");
    await waitFor(() => {
      expect(textarea.getAttribute("placeholder")).toContain("tap Send");
    });
    expect(textarea.getAttribute("placeholder")).not.toContain("Shift");
  });

  it("keeps the Shift+Enter hint on the desktop build", async () => {
    render(<App />);
    const textarea = await screen.findByRole("textbox");
    await waitFor(() => {
      expect(textarea.getAttribute("placeholder")).toContain("Shift+Enter");
    });
  });
});

// 手机端本地缓存：冷启首屏即有列表与内容，连上后由服务端覆盖。
describe("App mobile local cache", () => {
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );

  /** Defer `thread/listAll` until the returned resolver is called. */
  const stallList = () => {
    let release!: () => void;
    state.dataSources["thread/listAll"] = () =>
      new Promise((resolve) => {
        release = () =>
          resolve({
            groups: [
              {
                workspace: "/w",
                exists: true,
                threads: [
                  {
                    thread_id: "srv1", cwd: "/w", model: "m", created_at: 0, updated_at: 0,
                    title: "server-only", permission_mode: "normal", status: "idle",
                  },
                ],
              },
            ],
            pinned: [],
          });
      });
    return () => release();
  };

  const seedListCache = (threads: { thread_id: string; title: string }[], selectedId: string | null) =>
    localStorage.setItem(
      "yi-agent.threadListCache",
      JSON.stringify({
        version: 1,
        savedAt: Date.now(),
        selectedId,
        pinned: [],
        groups: [
          {
            workspace: "/w",
            exists: true,
            threads: threads.map((t) => ({
              thread_id: t.thread_id, cwd: "/w", model: "m", created_at: 0, updated_at: 0,
              title: t.title, permission_mode: "normal", status: "idle",
            })),
          },
        ],
      }),
    );

  it("paints the cached list before thread/listAll resolves", async () => {
    asRemote();
    seedListCache([{ thread_id: "t1", title: "cached-one" }], null);
    const release = stallList();
    render(<App />);
    // 缓存列表已在屏上，而服务端列表仍被扣着。
    expect(screen.getByText("cached-one")).toBeTruthy();
    // `thread/listAll` 是异步发出的（在 initialize → ui/settings/read →
    // workspace/list 之后），所以扣住它的 resolver 只有在请求真的上了线才存在：
    // 先等这条请求出现，再放行，让服务端列表覆盖缓存。
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/listAll")).toBe(true),
    );
    release();
    await waitFor(() => expect(screen.getByText("server-only")).toBeTruthy());
  });

  it("restores the cached selected thread instead of the first server thread", async () => {
    asRemote();
    seedListCache([{ thread_id: "t1", title: "cached-one" }], "t1");
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const resumed = clients[0].requests
      .filter((r) => r.method === "thread/resume")
      .map((r) => (r.params as { threadId: string }).threadId);
    expect(resumed).toEqual(["t1"]);
  });

  it("keeps a cache-restored selection over the first server thread on the first handshake", async () => {
    asRemote();
    // 服务端列表是 [t1, t2]，而缓存里选中 t2：恢复出的是 t2 而不是第一条。
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    seedListCache([{ thread_id: "t2", title: "two" }], "t2");
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const resumed = clients[0].requests
      .filter((r) => r.method === "thread/resume")
      .map((r) => (r.params as { threadId: string }).threadId);
    expect(resumed).toContain("t2");
    expect(resumed).not.toContain("t1");
  });

  it("prefills cached content, then the server replay replaces it in place", async () => {
    asRemote();
    seedListCache([{ thread_id: "t1", title: "one" }], "t1");
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "t1", savedAt: Date.now(), lastServerItemId: "A",
      items: [{ type: "agentMessage", id: "A", text: "cached text" }], trimmed: false, bytes: 11,
    });
    // The server replays a *different* item for the same thread.
    state.dataSources["thread/resume"] = () => ({ thread_id: "t1", cwd: "/w", model: "m", permission_mode: "normal" });
    render(<App />);
    await waitFor(() => expect(screen.getByText("cached text")).toBeTruthy());
    // The test double's resume carries no replay frames, so the reconcile guard
    // treats the empty set as "nothing to reconcile" and leaves the prefill in
    // place (on the real path the guard is a no-op too, since every replay carries
    // at least thread/started). The server's authoritative item then replaces the
    // cached one in place by id.
    act(() => {
      for (const h of state.notifHandlers)
        h({ method: "item/completed", params: { thread_id: "t1", item: { type: "agentMessage", id: "A", text: "server text" } } });
    });
    await waitFor(() => expect(screen.getByText("server text")).toBeTruthy());
    expect(screen.queryByText("cached text")).toBeNull();
  });

  it("drops a cached item the server no longer has after a full replay", async () => {
    asRemote();
    seedListCache([{ thread_id: "t1", title: "one" }], "t1");
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "t1", savedAt: Date.now(), lastServerItemId: "B",
      items: [
        { type: "agentMessage", id: "A", text: "keep me" },
        { type: "agentMessage", id: "B", text: "ghost" },
      ], trimmed: false, bytes: 20,
    });
    // Resume replays only A; B is gone server-side. Resume is stalled so the
    // replay can be delivered live, mirroring the production ordering: on the
    // real ws path the replay frames (thread/started + items/completed) are
    // enqueued BEFORE the resume response, so every id is collected by the time
    // `endReplay()` runs. The test double resolves `thread/resume` synchronously
    // (it cannot emit frames), so we hold it open and push the frame ourselves —
    // otherwise `endReplay()` would see an empty set, which per the production
    // guard is NOT treated as "server has no items" (see the test above, where
    // empty-replay must not wipe the prefill).
    let releaseResume!: () => void;
    state.dataSources["thread/resume"] = () =>
      new Promise((resolve) => {
        releaseResume = () =>
          resolve({ thread_id: "t1", cwd: "/w", model: "m", permission_mode: "normal" });
      });
    state.dataSources["thread/readItems"] = () => ({ items: [] });
    render(<App />);
    await waitFor(() => expect(screen.getByText("ghost")).toBeTruthy());
    // A arrives live while the resume is still in flight → collected for reconcile.
    act(() => {
      for (const h of state.notifHandlers)
        h({ method: "item/completed", params: { thread_id: "t1", item: { type: "agentMessage", id: "A", text: "keep me" } } });
    });
    await waitFor(() => expect(screen.getByText("keep me")).toBeTruthy());
    // Now the resume settles: dropStale({"A"}) removes the ghost, keeps A.
    act(() => releaseResume());
    await waitFor(() => expect(screen.queryByText("ghost")).toBeNull());
    expect(screen.getByText("keep me")).toBeTruthy();
  });

  it("never touches the list/content caches on the desktop build", async () => {
    localStorage.setItem("yi-agent.threadListCache", JSON.stringify({
      version: 1, savedAt: 1, selectedId: "t1", pinned: [],
      groups: [{ workspace: "/w", exists: true, threads: [{ thread_id: "t1", cwd: "/w", model: "m", created_at: 0, updated_at: 0, title: "cached-one", permission_mode: "normal", status: "idle" }] }],
    }));
    contentStore.current = null;
    render(<App />); // no relay binding → desktop
    // The desktop build must not have painted from the cache…
    expect(screen.queryByText("cached-one")).toBeNull();
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy()); // server list
    // …and must not have constructed a content store.
    expect(contentStore.current).toBeNull();

    // Deleting a thread is reachable on the desktop build too — it is the
    // sidebar row's onDelete. Routing the cache eviction through the gated
    // helper must keep the desktop path free of any store construction;
    // otherwise `thread/delete` builds an IndexedDB-backed store here.
    // The server answers the post-delete refresh with an empty list, so the row
    // disappearing proves the eviction (before `refreshThreads`) has run.
    state.dataSources["thread/delete"] = () => {
      state.threads = [];
      return {};
    };
    const row = screen.getByText("one");
    fireEvent.mouseEnter(row.closest("[data-thread-row]") ?? row);
    fireEvent.click(screen.getByRole("button", { name: "Delete thread" }));
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/delete")).toBe(true),
    );
    await waitFor(() => expect(screen.queryByText("one")).toBeNull());
    expect(contentStore.current).toBeNull();
  });

  it("resumes a thread the server flagged full, and drops the ghost content", async () => {
    asRemote();
    // 服务端说这个会话 epoch 变了（期间被 clear 过）。
    state.dataSources["initialize"] = () =>
      Promise.resolve({
        serverInfo: {}, protocolVersion: 1, capabilities: {},
        syncPlan: [{ threadId: "t1", action: "full", epoch: 4, reason: "epoch_changed" }],
      });
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "t1", savedAt: Date.now(), lastServerItemId: "ghost", contentEpoch: 2,
      items: [{ type: "agentMessage", id: "ghost", text: "stale content" }],
      trimmed: false, bytes: 1,
    });

    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    // 幽灵内容必须被清掉，而不是留在屏上。
    await waitFor(() => expect(screen.queryByText("stale content")).toBeNull());
  });

  it("forces a full resume for a trimmed cache even when the plan says none", async () => {
    asRemote();
    // 服务端**正确地说 none**（同世代、锚点仍在日志里）——但本地条目因超 1MB 丢过
    // 头部。照 none 跳过 resume 会让用户静默看到一个被截断的会话（spec §4.2 例外）。
    // 这条只依赖客户端自己知道的信息（自己有没有裁过），故 plan 覆盖时也必须生效。
    state.dataSources["initialize"] = () =>
      Promise.resolve({
        serverInfo: {}, protocolVersion: 1, capabilities: {},
        syncPlan: [{ threadId: "t1", action: "none", epoch: 3, reason: "match" }],
      });
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "t1", savedAt: Date.now(), lastServerItemId: "i-tail", contentEpoch: 3,
      items: [{ type: "agentMessage", id: "i-tail", text: "tail only" }],
      trimmed: true, bytes: 1,
    });

    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
  });

  it("catches up a thread the server marked none so a stale tail is filled", async () => {
    asRemote();
    // 服务端说 none：缓存**有效**，但这只表示"不必作废"，**不表示"已是最新"**。
    // 会话在两轮之间又长了几条，而客户端的锚点仍有效，故服务端不会给 full
    // （spec §4.2：落后是正常的并存形状，补齐交给常规路径）。
    state.dataSources["initialize"] = () =>
      Promise.resolve({
        serverInfo: {}, protocolVersion: 1, capabilities: {},
        syncPlan: [{ threadId: "t1", action: "none", epoch: 3, reason: "match" }],
      });
    state.dataSources["thread/readItems"] = () => ({
      items: [{ type: "agentMessage", id: "i-new", text: "fresh tail" }],
    });
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "t1", savedAt: Date.now(), lastServerItemId: "i-old", contentEpoch: 3,
      items: [{ type: "agentMessage", id: "i-old", text: "cached head" }],
      trimmed: false, bytes: 1,
    });

    render(<App />);
    // 必须真的补一次尾部（只读 readItems，不打断任何在途回合）——否则用户永远停在
    // 缓存尾部却以为内容是最新的，而 `turn/start` 又会因"锚点仍在"被放行，正是
    // 本特性要消除的"拿旧信息操作"。
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );
    await waitFor(() => expect(screen.getByText("fresh tail")).toBeTruthy());
  });

  it("re-tops-up a none thread after a reconnect (socket died, process survived)", async () => {
    asRemote();
    // 场景：socket 被系统掐断但 sidecar 未退出（经中继的瞬断 / 切回前台），
    // 于是 `exited` 不触发、`subWindow` 不清。此时若只按订阅窗口判定是否补齐，
    // warm idle 会话会**永远停在缓存尾部**——正是 C2 的残留缺口。
    state.dataSources["initialize"] = () =>
      Promise.resolve({
        serverInfo: {}, protocolVersion: 1, capabilities: {},
        syncPlan: [{ threadId: "t1", action: "none", epoch: 3, reason: "match" }],
      });
    state.dataSources["thread/readItems"] = () => ({ items: [] });
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "t1", savedAt: Date.now(), lastServerItemId: "i-old", contentEpoch: 3,
      items: [{ type: "agentMessage", id: "i-old", text: "cached head" }],
      trimmed: false, bytes: 1,
    });

    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );
    const first = clients[0].requests.filter((r) => r.method === "thread/readItems").length;

    // 前台唤醒重建客户端（没有 exited）。新连接必须为这个会话**再补一次**。
    Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
    act(() => document.dispatchEvent(new Event("visibilitychange")));
    await waitFor(() => expect(clients.length).toBeGreaterThan(1));
    const last = clients[clients.length - 1];
    await waitFor(() =>
      expect(last.requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );
    // 且**同一连接内**再切回不会重复发（每连接每会话只补一次）。
    expect(first).toBe(1);
  });

  it("does not let a late cache read re-inject content judged stale", async () => {
    asRemote();
    state.dataSources["initialize"] = () =>
      Promise.resolve({
        serverInfo: {}, protocolVersion: 1, capabilities: {},
        syncPlan: [{ threadId: "t1", action: "full", epoch: 4, reason: "epoch_changed" }],
      });
    // 缓存读故意延迟到 adoptEpoch 之后才 resolve。只延迟**第二次**读（prefill 的
    // 那次）：第一次是挂载预热，必须放行，否则 cacheMetaRef 为空、裁决无从播种
    // 缓存世代，测试就退化成「什么都没验」的空洞用例（曾实测）。读返回**快照**
    // 而非 resolve 时刻的 store：App 会在读到之前把已清空的会话落盘覆盖 ghost。
    let release!: () => void;
    const gate = new Promise<void>((r) => { release = r; });
    const real = new MemoryContentStore();
    await real.write({
      threadId: "t1", savedAt: Date.now(), lastServerItemId: "ghost", contentEpoch: 2,
      items: [{ type: "agentMessage", id: "ghost", text: "must not come back" }],
      trimmed: false, bytes: 1,
    });
    const ghostSnapshot = (await real.read("t1"))!;
    let reads = 0;
    contentStore.current = {
      read: async (id: string) => {
        if (id === "t1" && (reads += 1) > 1) await gate;
        return id === "t1" ? ghostSnapshot : real.read(id);
      },
      write: (e) => real.write(e),
      delete: (id) => real.delete(id),
      keys: () => real.keys(),
    };

    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    // 此刻裁决已判 full 并 adoptEpoch（世代 2→4）。
    release();
    // 读回是异步的：给它足够的 tick 走到回调里，否则断言会早于回调求值而空洞通过。
    await act(async () => { await new Promise((r) => setTimeout(r, 60)); });
    expect(screen.queryByText("must not come back")).toBeNull();
  });

  it("blocks sending on a running thread until its catch-up lands", async () => {
    asRemote();
    state.listStatus = "running";
    contentStore.current = new MemoryContentStore();
    // readItems 挂住：模拟补尾部在途。
    let release!: (v: unknown) => void;
    state.dataSources["thread/readItems"] = () =>
      new Promise((resolve) => { release = resolve; });

    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );

    // 同步中：提示条在（文案「正在同步…」，带省略号），且发送被拦（不发出 turn/start）。
    await waitFor(() => expect(screen.getByText(/正在同步…/)).toBeTruthy());
    const before = clients[0].requests.filter((r) => r.method === "turn/start").length;
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "hello" } });
    fireEvent.click(screen.getByRole("button", { name: /^(send|stop)$/i }));
    expect(clients[0].requests.filter((r) => r.method === "turn/start").length).toBe(before);

    // 补完 ⇒ 解禁，提示条退场，且此时真的发得出去。
    // 断言用带省略号的提示条文案，而非裸 `/正在同步/`：被拦时写入的错误文案
    // 「正在同步，请稍候」同样含该子串，会在同步结束后仍留在会话上，裸正则必然
    // 恒不匹配（计划自带的这条断言在计划自己的实现下不可能通过，已实测确认）。
    release({ items: [{ type: "agentMessage", id: "a1", text: "done" }] });
    await waitFor(() => expect(screen.queryByText(/正在同步…/)).toBeNull());
    const turnsBefore = clients[0].requests.filter(
      (r) => r.method === "turn/start" || r.method === "turn/interject",
    ).length;
    fireEvent.click(screen.getByRole("button", { name: /^(send|stop)$/i }));
    await waitFor(() =>
      expect(
        clients[0].requests.filter(
          (r) => r.method === "turn/start" || r.method === "turn/interject",
        ).length,
      ).toBe(turnsBefore + 1),
    );
  });

  it("converges out of syncing even when the catch-up fails", async () => {
    asRemote();
    state.listStatus = "running";
    contentStore.current = new MemoryContentStore();
    state.dataSources["thread/readItems"] = () => { throw new Error("boom"); };
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );
    // 失败也必须收敛，否则永久无法发送（spec §11 风险 3）。
    await waitFor(() => expect(screen.queryByText(/正在同步/)).toBeNull());
  });

  it("shows no syncing notice on the desktop path", async () => {
    // 桌面：不调 asRemote()。
    state.listStatus = "running";
    render(<App />);
    // 桌面端 running 会话走只读 readItems（不 resume、不置 syncing），等它落地即可。
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/readItems")).toBe(true),
    );
    expect(screen.queryByText(/正在同步/)).toBeNull();
    expect(screen.queryByText(/离线内容/)).toBeNull();
  });

  it("does not block sending on switching back to a warm idle thread", async () => {
    asRemote();
    contentStore.current = new MemoryContentStore();
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    // resume 已回放并置 warm；此刻会话 idle 且已在订阅窗口内 —— 切回不应置 syncing
    // 也绝不能让用户永久无法发送（设计 §11 风险 3）。
    await waitFor(() => expect(screen.queryByText(/正在同步/)).toBeNull());
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "hi" } });
    fireEvent.click(screen.getByRole("button", { name: /^(send|stop)$/i }));
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
  });

  it("sends a contextAnchor and does not resend after stale_context", async () => {
    asRemote();
    contentStore.current = new MemoryContentStore();
    // `turn/start` 报 stale_context（顶层方法用 dataSources 而非 rpcError，
    // 后者只在 plugin/query 的内层分支被读）。
    state.dataSources["turn/start"] = () => {
      throw { code: -32026, data: { code: "stale_context" } };
    };
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "hello" } });
    fireEvent.click(screen.getByRole("button", { name: /^(send|stop)$/i }));

    await waitFor(() => {
      const turns = clients[0].requests.filter((r) => r.method === "turn/start");
      expect(turns).toHaveLength(1);
      expect((turns[0].params as { contextAnchor?: unknown }).contextAnchor).toBeTruthy();
    });

    // 不自动重发：只提示 + 触发重同步（多一次 thread/resume）。
    await waitFor(() =>
      expect(
        clients[0].requests.filter((r) => r.method === "thread/resume").length,
      ).toBeGreaterThan(1),
    );
    // 发送没有成功：乐观回声（用户气泡）必须回滚，否则转录里会留下一条「已发出」
    // 的幽灵消息；草稿必须保留，用户确认后能重发（spec §7.3「草稿保留」）。
    // 只查**气泡**（用户消息气泡那个 class），不查裸文本：草稿框的值也是 "hello"。
    await waitFor(() =>
      expect(document.querySelector(".bg-blue-600.self-end")).toBeNull(),
    );
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("hello");
    expect(clients[0].requests.filter((r) => r.method === "turn/start")).toHaveLength(1);
  });

  it("does not send a contextAnchor from the desktop path", async () => {
    // 桌面：不调 asRemote()。
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "hello" } });
    fireEvent.click(screen.getByRole("button", { name: /^(send|stop)$/i }));
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(true),
    );
    const turn = clients[0].requests.find((r) => r.method === "turn/start");
    expect((turn?.params as { contextAnchor?: unknown } | undefined)?.contextAnchor).toBeUndefined();
  });

  it("declares resumeHints on initialize for a remote client", async () => {
    asRemote();
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "t1", savedAt: 5, lastServerItemId: "i1", contentEpoch: 3,
      items: [], trimmed: false, bytes: 1,
    });
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "initialize")).toBe(true),
    );
    // hints 从缓存元数据组装；缓存是异步读的，故轮询等待那一次 initialize 带上它。
    await waitFor(() => {
      const init = clients[0].requests.find((r) => r.method === "initialize");
      const hints = (init?.params as { resumeHints?: unknown[] } | undefined)?.resumeHints;
      expect(Array.isArray(hints)).toBe(true);
    });
  });

  it("does not send resumeHints from the desktop path", async () => {
    // 不调 asRemote()：桌面路径。
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "initialize")).toBe(true),
    );
    const init = clients[0].requests.find((r) => r.method === "initialize");
    expect((init?.params as { resumeHints?: unknown } | undefined)?.resumeHints).toBeUndefined();
  });

  it("drops a deleted thread's cached content and its cache entry", async () => {
    asRemote();
    // 服务端说这个会话已不存在（被另一端删除）——它因此不会出现在 listAll 里。
    state.dataSources["initialize"] = () =>
      Promise.resolve({
        serverInfo: {}, protocolVersion: 1, capabilities: {},
        syncPlan: [{ threadId: "t1", action: "drop", epoch: 0, reason: "deleted" }],
      });
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "t1", savedAt: Date.now(), lastServerItemId: "ghost", contentEpoch: 2,
      items: [{ type: "agentMessage", id: "ghost", text: "deleted transcript" }],
      trimmed: false, bytes: 1,
    });
    render(<App />);
    // 该会话是当前选中：它会被 drop、缓存被删、屏上不留内容。
    await waitFor(() => expect(screen.queryByText("deleted transcript")).toBeNull());
    await waitFor(async () => expect(await contentStore.current!.read("t1")).toBeNull());
  });
});

describe("App mobile offline UX", () => {
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );

  const seed = (listStatus: "idle" | "running", selectedId: string | null) =>
    localStorage.setItem("yi-agent.threadListCache", JSON.stringify({
      version: 1, savedAt: Date.now(), selectedId, pinned: [],
      groups: [{ workspace: "/w", exists: true, threads: [
        { thread_id: "t1", cwd: "/w", model: "m", created_at: 0, updated_at: 0, title: "one", permission_mode: "normal", status: listStatus },
      ] }],
    }));

  it("marks a cached running badge as possibly stale while offline", async () => {
    asRemote();
    seed("running", "t1");
    // Never resolve the list: stay in `connecting`.
    state.dataSources["thread/listAll"] = () => new Promise(() => {});
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("Thread status")).toBeTruthy());
    expect(screen.getByLabelText("Thread status").getAttribute("data-stale")).toBe("true");
  });

  it("refuses to send while offline and keeps the draft", async () => {
    asRemote();
    seed("idle", "t1");
    state.dataSources["thread/listAll"] = () => new Promise(() => {});
    render(<App />);
    const box = await screen.findByPlaceholderText(/Type a message/);
    fireEvent.change(box, { target: { value: "hello offline" } });
    fireEvent.click(screen.getByRole("button", { name: /send/i }));
    // No turn/start went out, and the text is still there.
    await waitFor(() => expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false));
    expect((box as HTMLTextAreaElement).value).toBe("hello offline");
  });
});

describe("App mobile reopen keeps the selection", () => {
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );

  const threads = () => [
    { thread_id: "t1", title: "one", permission_mode: "normal" as const },
    { thread_id: "t2", title: "two", permission_mode: "normal" as const },
  ];

  const resumedOn = (client: { requests: { method: string; params: unknown }[] }) =>
    client.requests
      .filter((r) => r.method === "thread/resume")
      .map((r) => (r.params as { threadId: string }).threadId);

  // 模拟一台「切后台后 socket 已被系统掐断」的手机：宿主先报 exited（App 据此清
  // 掉 warm，新进程的记忆确实是空的），用户仍在看 t2；随后 App 切回前台，前台
  // 唤醒立刻重建客户端并重放整趟握手。重放里 selectThread 是冷会话（warm 已清），
  // 于是真的会发 thread/resume —— 修复前它挑的是列表第一条 t1，修复后保持 t2。
  const foregroundAfterDeadSocket = () => {
    act(() => {
      for (const cb of state.statusHandlers) cb({ state: "exited", code: 1 });
    });
    Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
    act(() => document.dispatchEvent(new Event("visibilitychange")));
  };

  it("keeps the current thread across a foreground reconnect (remote)", async () => {
    asRemote();
    state.threads = threads();
    render(<App />);
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    fireEvent.click(screen.getByText("two"));
    await waitFor(() => expect(resumedOn(clients[0])).toContain("t2"));
    clients[0].requests.length = 0;
    foregroundAfterDeadSocket();
    await waitFor(() => expect(clients.length).toBeGreaterThan(1));
    await waitFor(() =>
      expect(clients[clients.length - 1].requests.some((r) => r.method === "thread/listAll")).toBe(true),
    );
    // 重连绝不能把选中的会话踢回第一条：并且保留的会话要被**真正**重新
    // resume（新进程记忆为空，冷会话），否则视图留着而实时更新静默停摆。
    const last = clients[clients.length - 1];
    await waitFor(() => expect(resumedOn(last)).toContain("t2"));
    expect(resumedOn(last)).not.toContain("t1");
    expect(
      last.requests.some(
        (r) =>
          r.method === "thread/subscribe" &&
          ((r.params as { threadIds?: string[] }).threadIds ?? []).includes("t2"),
      ),
    ).toBe(true);
  });

  it("still selects the first thread on the desktop reconnect (unchanged)", async () => {
    state.threads = threads();
    render(<App />);
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    fireEvent.click(screen.getByText("two"));
    await waitFor(() => expect(resumedOn(clients[0])).toContain("t2"));
    clients[0].requests.length = 0;
    foregroundAfterDeadSocket();
    await waitFor(() => expect(clients.length).toBeGreaterThan(1));
    await waitFor(() =>
      expect(clients[clients.length - 1].requests.some((r) => r.method === "thread/listAll")).toBe(true),
    );
    // 桌面端重放保留历史行为：每次都回到第一条会话。
    await waitFor(() =>
      expect(resumedOn(clients[clients.length - 1])).toContain("t1"),
    );
  });
});

describe("App reasoning effort wiring", () => {
  it("writes the session override and reflects the server-pushed effort", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    fireEvent.click(await screen.findByRole("button", { name: /推理档位/ }));
    fireEvent.click(screen.getByRole("menuitemradio", { name: "high" }));

    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/setReasoningEffort",
        params: { threadId: "t1", effort: "high" },
      }),
    );
    // 以回包为准更新控件。
    await waitFor(() =>
      expect(screen.getByRole("button", { name: /推理档位/ }).textContent).toContain("high"),
    );

    // 另一客户端改了 effort：服务端广播，桌面控件回显。
    act(() => {
      for (const cb of state.notifHandlers)
        cb({ method: "thread/reasoningEffortChanged", params: { thread_id: "t1", effort: "xhigh" } });
    });
    await waitFor(() =>
      expect(screen.getByRole("button", { name: /推理档位/ }).textContent).toContain("xhigh"),
    );
  });
});

describe("App reasoning rendering", () => {
  it("keeps each turn's CoT block before that turn's answer", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const notify = (n: unknown) =>
      act(() => {
        for (const cb of state.notifHandlers) cb(n);
      });

    // 真实顺序：开启项(user) → CoT 增量 → 开启项(agent) → 正文增量。
    notify({ method: "item/started", params: { thread_id: "t1", item: { type: "userMessage", id: "user-1", text: "q1" } } });
    notify({ method: "thread/reasoningDelta", params: { thread_id: "t1", text: "T1", kind: "Content" } });
    notify({ method: "item/started", params: { thread_id: "t1", item: { type: "agentMessage", id: "a1", text: "" } } });
    notify({ method: "item/delta", params: { thread_id: "t1", item_id: "a1", delta: "answer-one" } });

    // 第二轮。
    notify({ method: "item/started", params: { thread_id: "t1", item: { type: "userMessage", id: "user-2", text: "q2" } } });
    notify({ method: "thread/reasoningDelta", params: { thread_id: "t1", text: "T2", kind: "Content" } });
    notify({ method: "item/started", params: { thread_id: "t1", item: { type: "agentMessage", id: "a2", text: "" } } });
    notify({ method: "item/delta", params: { thread_id: "t1", item_id: "a2", delta: "answer-two" } });

    // 两轮的 CoT 各自成块，不是尾部一个合并块。
    const blocks = screen.getAllByTestId("reasoning-block");
    expect(blocks).toHaveLength(2);
    const q1 = screen.getByText("q1");
    const a1 = screen.getByText("answer-one");
    const q2 = screen.getByText("q2");
    const a2 = screen.getByText("answer-two");
    const before = (a: Element, b: Element) =>
      Boolean(a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING);
    // q1 | 思考[T1] | A1 | q2 | 思考[T2] | A2
    expect(before(q1, blocks[0])).toBe(true);
    expect(before(blocks[0], a1)).toBe(true);
    expect(before(a1, q2)).toBe(true);
    expect(before(q2, blocks[1])).toBe(true);
    expect(before(blocks[1], a2)).toBe(true);
  });
});

describe("App 后台进程接线", () => {
  const runningProcess = {
    process_id: "proc_1",
    name: "dev",
    pid: 4242,
    command: "npm run dev",
    cwd: "/w",
    status: { state: "running" },
    ready: false,
    on_exit: "kill",
    exit_code: null,
    elapsed_sec: 1.5,
  };

  const readCount = () =>
    clients[0].requests.filter((r) => r.method === "process/read").length;

  it("shows the process tab and lists a process reported by process/list", async () => {
    state.processes = [runningProcess];
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    fireEvent.click(await screen.findByLabelText("会话详情面板"));
    const tab = await screen.findByRole("tab", { name: /后台进程/ });
    fireEvent.click(tab);
    expect(await screen.findByText("dev")).toBeTruthy();
  });

  it("refetches the process list when process/updated arrives", async () => {
    state.processes = [runningProcess];
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const before = clients[0].requests.filter((r) => r.method === "process/list").length;

    // 通知只是"该重拉了":服务端不把权威数据放在事件里。
    act(() => {
      for (const cb of state.notifHandlers) {
        cb({
          method: "process/updated",
          params: { thread_id: "t1", process_id: "proc_1", state: "exited" },
        });
      }
    });

    await waitFor(() =>
      expect(
        clients[0].requests.filter((r) => r.method === "process/list").length,
      ).toBeGreaterThan(before),
    );
  });

  it("opens a detail that polls process/read, and stops polling once closed", async () => {
    state.processes = [runningProcess];
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    fireEvent.click(await screen.findByLabelText("会话详情面板"));
    fireEvent.click(await screen.findByRole("tab", { name: /后台进程/ }));
    fireEvent.click(await screen.findByRole("button", { name: /查看进程 dev/ }));

    await waitFor(() => expect(readCount()).toBeGreaterThan(0));
    expect(await screen.findByText(/ready on 3000/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /关闭进程详情/ }));
    const settled = readCount();
    // 轮询间隔 500ms:等 1.2s;若清理没生效,计数必然增长。
    await new Promise((r) => setTimeout(r, 1200));
    expect(readCount()).toBe(settled);
  });

  it("kills via process/kill and refetches the list", async () => {
    state.processes = [runningProcess];
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    fireEvent.click(await screen.findByLabelText("会话详情面板"));
    fireEvent.click(await screen.findByRole("tab", { name: /后台进程/ }));
    fireEvent.click(await screen.findByRole("button", { name: /查看进程 dev/ }));

    fireEvent.click(await screen.findByRole("button", { name: "终止进程" }));
    fireEvent.click(await screen.findByRole("button", { name: "确认终止" }));

    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "process/kill",
        params: { thread_id: "t1", process_id: "proc_1" },
      }),
    );
  });
});

describe("App lead entry reachability", () => {
  it("asks thread/lead for a project group that has no lead, and shows the lead", async () => {
    render(<App />);
    // 首屏 listAll 只有普通会话（mock 默认 /w 组含 t1，无 role:"lead"）。
    await waitFor(() =>
      expect(
        clients[0].requests.some(
          (r) => r.method === "thread/lead" && (r.params as { cwd?: string }).cwd === "/w",
        ),
      ).toBe(true),
    );
    // 服务端补建后重取 listAll，随之出现组长条目（mock 的 leadCwds 让它报出来）。
    await waitFor(() => expect(screen.getByText("组长")).toBeTruthy());
    // 补建过就不再重复发起：thread/lead 对 /w 只应有一次。
    expect(
      clients[0].requests.filter(
        (r) => r.method === "thread/lead" && (r.params as { cwd?: string }).cwd === "/w",
      ),
    ).toHaveLength(1);
  });

  it("still paints the first screen when thread/lead fails", async () => {
    state.failLead = true;
    render(<App />);
    // 补建路径确实被走到了：对 /w 发过 thread/lead（且失败了）。
    await waitFor(() => expect(state.leadAttempts).toContain("/w"));
    // 失败不得挡住首屏：既有会话照旧渲染，且没有凭空冒出组长条目。
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    expect(screen.queryByText("组长")).toBeNull();
  });

  it("never asks for a lead when the flag is off", async () => {
    state.leadFlagEnabled = false;
    render(<App />);
    // 等首屏稳定：既有会话已渲染，说明握手（含 flags/list）已经跑完。
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    // 关掉时对 /w 发 thread/lead 是纯浪费——服务端只会回 -32601。
    expect(state.leadAttempts).toEqual([]);
    expect(clients[0].requests.map((r) => r.method)).not.toContain("thread/lead");
  });
});

describe("拖入路径", () => {
  it("drop 落在输入卡片上时把路径写进当前会话草稿", async () => {
    render(<App />);
    // 首屏自动 resume 第一条会话后，输入框才不是 disabled。
    await waitFor(() => expect(screen.getByRole("textbox")).toBeTruthy());
    const card = screen.getByTestId("composer-card");
    stubRect(card, { left: 0, top: 0, right: 400, bottom: 120 });
    await act(async () => {
      dragDrop.fn?.({ payload: { type: "drop", paths: ["/Users/me/My Docs"], position: { x: 10, y: 10 } } });
    });
    const ta = screen.getByRole("textbox") as HTMLTextAreaElement;
    expect(ta.value).toBe('"/Users/me/My Docs"');
    // 光标必须落在插入块的末尾：App 把 `caretSeek` 交给 MessageInput，由它
    // `setSelectionRange`。含空格的路径带引号后长 19，故 19。
    expect(ta.selectionStart).toBe(19);
    expect(ta.selectionEnd).toBe(19);
  });

  it("连续两次 drop 都把光标放在新插入块的末尾", async () => {
    render(<App />);
    await waitFor(() => expect(screen.getByRole("textbox")).toBeTruthy());
    const card = screen.getByTestId("composer-card");
    stubRect(card, { left: 0, top: 0, right: 400, bottom: 120 });
    const ta = screen.getByRole("textbox") as HTMLTextAreaElement;
    // 第一次：空草稿 → 19 个字符，光标 19。
    await act(async () => {
      dragDrop.fn?.({ payload: { type: "drop", paths: ["/Users/me/My Docs"], position: { x: 10, y: 10 } } });
    });
    expect(ta.value).toBe('"/Users/me/My Docs"');
    expect(ta.selectionStart).toBe(19);
    // 第二次前把光标挪回行首（走组件自己的 onSelect 通路，与用户点一下行首同路）。
    // 这一步是这条用例能否证伪的关键：jsdom 在 `value` 被赋值时会把选区挪到**全文
    // 末尾**，所以「插在末尾」的第二次插入无论 caretSeek 在不在，选区都恰好等于
    // 插入块末尾（19 → 39），证伪不了任何东西。只有让第二次插入落在文本**中间**，
    // 「插入块末尾」(19) 与「全文末尾」(38) 才分开，而把光标放到 19 只能靠
    // caretSeek 这条 App→MessageInput 通路。
    ta.setSelectionRange(0, 0);
    fireEvent.select(ta);
    await act(async () => {
      dragDrop.fn?.({ payload: { type: "drop", paths: ["/Users/me/My Docs"], position: { x: 10, y: 10 } } });
    });
    // 行首插入不加前导空格（needsSpace 只在光标前有内容且不以空白结尾时成立）。
    expect(ta.value).toBe('"/Users/me/My Docs""/Users/me/My Docs"');
    expect(ta.selectionStart).toBe(19);
    expect(ta.selectionEnd).toBe(19);
  });

  it("drop 落在卡片之外时不改草稿", async () => {
    render(<App />);
    await waitFor(() => expect(screen.getByRole("textbox")).toBeTruthy());
    const card = screen.getByTestId("composer-card");
    stubRect(card, { left: 0, top: 0, right: 400, bottom: 120 });
    await act(async () => {
      dragDrop.fn?.({ payload: { type: "drop", paths: ["/a"], position: { x: 900, y: 900 } } });
    });
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("");
  });
});
