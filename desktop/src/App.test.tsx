/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor, cleanup } from "@testing-library/react";

const { clients, state } = vi.hoisted(() => ({
  clients: [] as Array<{ requests: { method: string; params: unknown }[] }>,
  state: { mode: "normal" as "normal" | "yolo", failSet: false },
}));

vi.mock("./lib/rpc", () => ({
  RpcClient: class {
    requests: { method: string; params: unknown }[] = [];
    constructor() {
      clients.push(this);
    }
    onNotification() {
      return () => {};
    }
    onApproval() {
      return () => {};
    }
    onStatus() {
      return () => {};
    }
    async request(method: string, params: unknown) {
      this.requests.push({ method, params });
      if (method === "thread/listAll")
        return {
          groups: [
            {
              workspace: "/w",
              exists: true,
              threads: [
                {
                  thread_id: "t1",
                  cwd: "/w",
                  model: "m",
                  created_at: 0,
                  updated_at: 0,
                  title: null,
                  permission_mode: state.mode,
                },
              ],
            },
          ],
        };
      if (method === "workspace/list") return { workspaces: [] };
      if (method === "thread/resume") return { thread_id: "t1", cwd: "/w", model: "m" };
      if (method === "thread/setPermissionMode") {
        if (state.failSet) throw { code: -32011, message: "unknown thread" };
        return {};
      }
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
  Element.prototype.scrollIntoView = vi.fn();
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
});
