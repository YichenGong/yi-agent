/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor, cleanup, act } from "@testing-library/react";

type Mode = "normal" | "yolo";
type ThreadSeed = { thread_id: string; title: string | null; permission_mode: Mode };

const { clients, state } = vi.hoisted(() => ({
  clients: [] as Array<{ requests: { method: string; params: unknown }[] }>,
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
    // Tests push what they need; an empty queue resolves to `null`.
    picks: [] as (string | null)[],
    // Per-test RPC override: method -> a data source for its response. Lets a
    // test hand back a pending promise so an in-flight request (e.g. the startup
    // theme read) can be interleaved with a notification or user action. Takes
    // precedence over the canned responses below.
    dataSources: {} as Record<string, (params: unknown) => unknown>,
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
    constructor() {
      clients.push(this);
    }
    onNotification(cb: (n: unknown) => void) {
      state.notifHandlers.push(cb);
      return () => {};
    }
    onApproval(cb: (r: unknown) => void) {
      state.approvalHandlers.push(cb);
      return () => {};
    }
    onStatus() {
      return () => {};
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
        return {
          groups: [
            {
              workspace: "/w",
              exists: true,
              threads: seeds.map((t) => ({
                thread_id: t.thread_id,
                cwd: "/w",
                model: "m",
                created_at: 0,
                updated_at: 0,
                title: t.title,
                permission_mode: t.permission_mode,
                status: state.listStatus,
                pinned: state.pinnedIds.includes(t.thread_id),
              })),
            },
          ],
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
      if (method === "ui/settings/read") return { theme: "dark" };
      if (method === "ui/settings/write") return { ok: true };
      if (method === "thread/setPermissionMode") {
        if (state.failSet) throw { code: -32011, message: "unknown thread" };
        return {};
      }
      if (method === "thread/clear") return {};
      if (method === "thread/compact") return { status: "compacted" };
      return {};
    }
  },
}));

vi.mock("./tauriTransport", () => ({ tauriTransport: () => ({}) }));

import App from "./App";

beforeEach(() => {
  clients.length = 0;
  state.mode = "normal";
  state.failSet = false;
  state.failList = false;
  state.failListAfter = null;
  state.threads = null;
  state.notifHandlers.length = 0;
  state.approvalHandlers.length = 0;
  state.rejectCode = {};
  state.listStatus = "idle";
  state.pinnedIds = [];
  state.picks = [];
  state.dataSources = {};
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

describe("App Superpowers 看板 collapse", () => {
  it("folds the board panel down to a strip and unfolds it again", async () => {
    render(<App />);
    // The board is rendered regardless of the switch, so no need to wait for
    // the poll: the settings panel is part of the initial layout.
    const collapse = await screen.findByRole("button", { name: "收起看板" });

    expect(screen.getByRole("checkbox")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "展开看板" })).toBeNull();

    fireEvent.click(collapse);

    // Collapsed: the column is gone and the expand affordance has taken its place.
    expect(screen.queryByRole("button", { name: "收起看板" })).toBeNull();
    const expand = screen.getByRole("button", { name: "展开看板" });

    fireEvent.click(expand);

    // Expanded again: collapse comes back, expand goes away. Collapse and expand
    // must be a pair, or the panel becomes unreachable once folded.
    expect(screen.getByRole("button", { name: "收起看板" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "展开看板" })).toBeNull();
  });

  it("starts every launch expanded", async () => {
    render(<App />);
    expect(await screen.findByRole("button", { name: "收起看板" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "展开看板" })).toBeNull();
  });
});

describe("App Superpowers 看板 enqueue", () => {
  const clickEnqueue = () =>
    fireEvent.click(screen.getByRole("button", { name: "加入看板" }));

  it("sends one enqueue with both picked paths", async () => {
    render(<App />);
    await screen.findByRole("button", { name: "加入看板" });

    state.picks = ["/p/a.spec.md", "/p/a.plan.md"];
    clickEnqueue();

    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "plugin/query",
        params: {
          plugin: "superpowers-kanban",
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
    render(<App />);
    await screen.findByRole("button", { name: "加入看板" });

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
    state.rejectCode["plugin/query"] = -32000;
    render(<App />);
    await screen.findByRole("button", { name: "加入看板" });

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

  /** Defer `ui/settings/read` so a test can interleave a newer theme choice. */
  function deferRead() {
    let resolveRead: (v: { theme?: unknown }) => void = () => {};
    state.dataSources["ui/settings/read"] = () =>
      new Promise((resolve) => {
        resolveRead = resolve;
      });
    return async (value: { theme?: unknown }) => {
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
});
