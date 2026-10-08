/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor, cleanup, act, within } from "@testing-library/react";

type Mode = "normal" | "yolo";
type ThreadSeed = { thread_id: string; title: string | null; permission_mode: Mode };

const { clients, state } = vi.hoisted(() => ({
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
    // Per-project 看板 (Task 7): what `board/list` reports as registered, and
    // what the plugin's `list` answers with.
    boards: [] as unknown,
    cards: [] as unknown[],
    // Workspace groups `thread/listAll` reports. `null` → a single "/w", which
    // is what the pre-board tests assume. Board tests set the real project
    // paths, since the sidebar only offers a board row for a group it lists.
    groupWorkspaces: null as string[] | null,
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
  },
}));

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
        return {
          groups: workspaces.map((workspace, index) => ({
            workspace,
            exists: true,
            threads:
              index === 0
                ? seeds.map((t) => ({
                    thread_id: t.thread_id,
                    cwd: workspace,
                    model: "m",
                    created_at: 0,
                    updated_at: 0,
                    title: t.title,
                    permission_mode: t.permission_mode,
                    status: state.listStatus,
                    pinned: state.pinnedIds.includes(t.thread_id),
                  }))
                : [],
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
      if (method === "thread/resume") {
        const { threadId } = params as { threadId: string };
        return { thread_id: threadId, cwd: "/w", model: "m" };
      }
      if (method === "thread/start") {
        const id = `new-${this.requests.length}`;
        return { thread_id: id, cwd: "/w", model: "m" };
      }
      if (method === "ui/settings/read") return { theme: "dark", board_watchman_enabled: true };
      if (method === "ui/settings/write") return { ok: true };
      if (method === "thread/setPermissionMode") {
        if (state.failSet) throw { code: -32011, message: "unknown thread" };
        return {};
      }
      if (method === "thread/clear") return {};
      if (method === "thread/compact") return { status: "compacted" };
      if (method === "board/list") return { boards: state.boards };
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

import App from "./App";
import { nextReconnectDelay } from "./lib/reconnect";

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
  state.rpcError = {};
  state.hostError = {};
  state.dataSources = {};
  state.platformUa = "";
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

  it("stays unknown (not Normal) when the post-resume listAll lookup fails", async () => {
    // Server-side thread is YOLO, but only the mount-time listing succeeds; the
    // follow-up lookup that resumes triggers fails.
    state.mode = "yolo";
    state.failListAfter = 1;
    render(<App />);

    // Auto-resume still happens, so we reach the failing post-resume lookup.
    await waitFor(() =>
      expect(
        clients[0].requests.filter((r) => r.method === "thread/listAll").length,
      ).toBeGreaterThanOrEqual(2),
    );

    // Must not lie "Normal" — the mode is simply unknown.
    const chip = modeTrigger() as HTMLButtonElement;
    expect(chip.disabled).toBe(true);
    expect(chip.textContent).not.toContain("Normal");
    expect(chip.textContent).not.toContain("YOLO");
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
    const field = document.querySelector<HTMLInputElement>("input")!;
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
    const stillThere = document.querySelector<HTMLInputElement>("input");
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
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
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
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
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
  it("renders /help output as a notice without touching the agent", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/help" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByText(/可用命令:/);
    expect(
      clients[0].requests.some((r) => r.method === "turn/start"),
    ).toBe(false);
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
  const header = document.querySelector<HTMLElement>('div[tabindex][aria-haspopup="menu"]')!;
  fireEvent.contextMenu(header);
  await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
  fireEvent.click(screen.getByLabelText("看板"));
  return project;
}

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

  it("开关写入失败会显示错误，而不是静默（该项目尚未创建看板）", async () => {
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    // 宿主在 Task 4 之后把语义码放在 data.code 里，message 只剩人话。
    state.hostError["switch.write"] = {
      code: -32603,
      message: "这个项目还没有看板",
      data: { code: "board_not_created" },
    };
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");

    fireEvent.click(await screen.findByLabelText("Superpowers 看板开关"));

    expect(await screen.findByText(/该项目尚未创建看板/)).toBeTruthy();
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

  it("「该项目尚未创建看板」给一条出路：直接创建", async () => {
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
    await screen.findByText(/该项目尚未创建看板/);

    fireEvent.click(screen.getByRole("button", { name: "创建看板" }));

    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "board/create",
        params: { project: "/proj" },
      }),
    );
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
    type ReadValue = { theme?: unknown; board_watchman_enabled?: unknown };
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

  it("reads board_watchman_enabled and writes it back when the panel toggles it", async () => {
    // 值守是宿主级设置，与主题同走 ui/settings/*（而不是带 project 的
    // plugin/query）：这里只是把这趟 read 的值 stub 成 false；默认口径其实是开
    // （配置里没这个键也当开），点一下要写回 true。
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    state.dataSources["ui/settings/read"] = () => ({ theme: "dark", board_watchman_enabled: false });
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");

    const box = (await screen.findByLabelText("后台值守（开机自启）开关")) as HTMLInputElement;
    expect(box.checked).toBe(false);
    fireEvent.click(box);
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "ui/settings/write",
        params: { board_watchman_enabled: true },
      }),
    );
  });

  it("surfaces the watchman warning ui/settings/write returns", async () => {
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    state.dataSources["ui/settings/read"] = () => ({ theme: "dark", board_watchman_enabled: true });
    // 写入落盘了，但宿主没能装上登录项：它把原因放在 warning 里，面板必须原样
    // 摆出来，而不是静默让人以为已经生效。
    state.dataSources["ui/settings/write"] = (params: unknown) =>
      (params as { board_watchman_enabled?: boolean }).board_watchman_enabled === false
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
        params: { board_watchman_enabled: false },
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
    await resolveRead({ theme: "dark", board_watchman_enabled: true });
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
        params: { board_watchman_enabled: false },
      }),
    );
    await waitFor(() =>
      expect((screen.getByLabelText("后台值守（开机自启）开关") as HTMLInputElement).checked).toBe(
        false,
      ),
    );

    // 迟到的权威 read 带回旧值 true，必须被忽略：用户的选择才是当前选择。
    await resolveStale({ theme: "dark", board_watchman_enabled: true });
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

// 状态栏图标现在管的是**会话详情面板**（轨迹 + Git Diff）的开合；子 agent 栏的
// 收起则由栏自己的「收起」按钮负责。两条路径都必须可逆：
//  - 面板：状态栏图标开，面板页眉的「关闭」关；
//  - 子 agent 栏：栏内「收起」关，栏收起后由**面板页眉**的「展开子 agent 栏」开
//    （回归背景：入口曾只在被它关掉的那块 UI 里，收起后开关随面板一起消失，
//    于是再也打不开——状态栏图标改管面板后，这条退路必须显式补上）。
describe("App 会话详情面板与子 agent 栏的开合", () => {
  it("状态栏图标开合详情面板，页眉可关闭", async () => {
    render(<App />);
    await screen.findByLabelText("会话详情面板");

    // 面板初始不渲染：没有会话详情的 section。
    expect(screen.queryByLabelText("会话详情")).toBeNull();

    const toggle = await screen.findByLabelText("会话详情面板");
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(toggle);
    expect(await screen.findByLabelText("会话详情")).toBeTruthy();
    expect(screen.getByRole("tab", { name: "轨迹" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Git Diff" })).toBeTruthy();
    expect(screen.getByLabelText("会话详情面板").getAttribute("aria-expanded")).toBe("true");

    fireEvent.click(screen.getByRole("button", { name: "关闭详情" }));
    expect(screen.queryByLabelText("会话详情")).toBeNull();
  });

  it("子 agent 栏收起后，仍能从面板页眉把它重新展开", async () => {
    render(<App />);
    // 首个会话（t1）自动选中后，子 agent 栏随之出现。
    fireEvent.click(await screen.findByLabelText("收起子 agent"));
    expect(screen.queryByLabelText("子 agent")).toBeNull();
    expect(screen.queryByLabelText("收起子 agent")).toBeNull();

    // 栏没了，但退路还在：打开面板 → 页眉给出「展开子 agent 栏」。
    fireEvent.click(screen.getByLabelText("会话详情面板"));
    fireEvent.click(await screen.findByRole("button", { name: "展开子 agent 栏" }));
    expect(await screen.findByLabelText("子 agent")).toBeTruthy();
    expect(screen.getByLabelText("收起子 agent")).toBeTruthy();
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
        cb({ method: "ui/gitDiff/focus", params: { threadId: "t1", base: null, note: "看这里" } });
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

  it("点子 agent 卡片回到轨迹 Tab 并打开面板", async () => {
    state.dataSources["agent/children/list"] = () => ({
      children: [{ taskId: "task-1", objective: "整理日志", state: "running", lastStep: "读文件" }],
    });
    state.dataSources["thread/diff/read"] = () => ({
      base: null,
      baseKind: "none",
      mergeBase: null,
      commits: [],
      files: [],
      unifiedDiff: "",
      truncated: false,
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
    // 面板先停在 Git Diff，再点卡片：必须被拨回「轨迹」，否则下钻总是落空。
    fireEvent.click(await screen.findByLabelText("会话详情面板"));
    fireEvent.click(await screen.findByRole("tab", { name: "Git Diff" }));
    fireEvent.click(await screen.findByRole("button", { name: "查看子 agent task-1" }));

    expect(await screen.findByRole("tab", { name: "轨迹" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "轨迹" }).getAttribute("aria-selected")).toBe("true");
    expect(await screen.findByText("task-1")).toBeTruthy();
    expect(clients[0].requests).toContainEqual({
      method: "agent/trace/read",
      params: { threadId: "t1", taskId: "task-1" },
    });
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
        cb({ method: "ui/gitDiff/focus", params: { threadId: null, base: null, note: null } });
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
          params: { threadId: "t2", base: null, note: "这是 t2 的提示" },
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

describe("App 附件接线", () => {
  const paperclip = () => screen.getByRole("button", { name: "附加文件" });
  const sendButton = () => screen.getByRole("button", { name: /^send$/i });

  /** 点回形针触发原生多选，并等界面吸收结果（chips 或错误横幅）。 */
  async function pickFiles(picked: string | string[] | null) {
    state.picks = [picked];
    await act(async () => {
      fireEvent.click(paperclip());
    });
  }

  it("sends attachments as input blocks before the text block", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    await pickFiles(["/tmp/报告.pdf"]);
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

  it("surfaces a local pre-check failure instead of sending", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    // `.mov` 不在 `ATTACHMENT_EXTENSIONS` 里，本地预检必须就地拒绝。
    await pickFiles(["/tmp/演示.mov"]);

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

    await pickFiles(["/tmp/报告.pdf"]);
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

    await pickFiles(["/tmp/报告.pdf"]);
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

    await pickFiles(["/tmp/报告.pdf"]);
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

    await pickFiles(["/tmp/报告.pdf"]);
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

    await pickFiles(["/tmp/报告.pdf"]);
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

    await pickFiles(["/tmp/a.pdf", "/tmp/b.md"]);
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
