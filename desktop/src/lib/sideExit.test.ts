import { describe, expect, it, vi } from "vitest";
import type { RpcClient } from "./rpc";
import type { SideExitParams } from "./protocol";
import { parseSideHandshake, type SideHostBinding } from "./sideConversation";
import { SideExitCoordinator, sideExitKey } from "./sideExit";

const SLOT = "local";

function binding(
  opts: {
    instanceId?: string;
    scope?: "control" | "observe" | "admin";
    connected?: boolean;
    snapshotLifecycle?: boolean;
    request?: (method: string, params: unknown) => Promise<unknown>;
  } = {},
): SideHostBinding {
  const request = vi.fn(
    opts.request ??
      (async () => ({
        hostInstanceId: opts.instanceId ?? "h",
        parentThreadId: "p",
        entryId: "e",
        state: "closed",
      })),
  );
  const client = { request } as unknown as RpcClient;
  const parsed = parseSideHandshake(
    SLOT,
    client,
    {
      capabilities: {
        side_conversation_v1: true,
        side_snapshot_lifecycle_v1: opts.snapshotLifecycle ?? true,
      },
      sideHostInstanceId: opts.instanceId ?? "h",
    },
    opts.scope ?? "control",
  );
  if (!parsed) throw new Error("handshake did not parse");
  return { ...parsed, connected: opts.connected ?? true };
}

function target(over: Partial<SideExitParams> = {}): SideExitParams {
  return { hostInstanceId: "h", parentThreadId: "p", entryId: "e", ...over };
}

describe("side exit intent", () => {
  it("blocks re-entry before a closed receipt", () => {
    const exits = new SideExitCoordinator(() => {});
    exits.record(target());
    expect(exits.blocked("h", "p")).toBe(true);
    // 别的父、别的宿主不受影响：屏障是精确三元组，不是一个全局开关。
    expect(exits.blocked("h", "another")).toBe(false);
    expect(exits.blocked("other", "p")).toBe(false);
    exits.dispose();
  });

  it("keeps the barrier across closing and failed, lifts only on closed", async () => {
    const states = ["closing", "failed", "closed"] as const;
    let i = 0;
    const b = binding({
      request: async () => ({
        hostInstanceId: "h",
        parentThreadId: "p",
        entryId: "e",
        state: states[Math.min(i++, states.length - 1)],
      }),
    });
    const exits = new SideExitCoordinator(() => {});
    exits.record(target());

    await exits.flush(b);
    expect(exits.blocked("h", "p")).toBe(true);
    expect(exits.pending()[0].phase).toBe("pending"); // closing = 意图已记录

    await exits.flush(b);
    expect(exits.blocked("h", "p")).toBe(true);
    expect(exits.pending()[0].phase).toBe("failed");
    expect(exits.pending()[0].message).not.toBeNull();

    await exits.flush(b);
    expect(exits.blocked("h", "p")).toBe(false);
    expect(exits.pending()).toHaveLength(0);
    exits.dispose();
  });

  it("does not lift the barrier on a mismatched receipt", async () => {
    // 回包描述的是另一张票（另一个 entry）：不能凭它放行这条意图。
    const b = binding({
      request: async () => ({
        hostInstanceId: "h",
        parentThreadId: "p",
        entryId: "someone-else",
        state: "closed",
      }),
    });
    const exits = new SideExitCoordinator(() => {});
    exits.record(target());
    await exits.flush(b);
    expect(exits.blocked("h", "p")).toBe(true);
    expect(exits.pending()[0].phase).toBe("failed");
    exits.dispose();
  });

  it("keeps the intent on a network failure and retries with the same ticket", async () => {
    let calls = 0;
    const b = binding({
      request: async () => {
        calls += 1;
        if (calls === 1) throw new Error("socket closed");
        return {
          hostInstanceId: "h",
          parentThreadId: "p",
          entryId: "e",
          state: "closed",
        };
      },
    });
    const exits = new SideExitCoordinator(() => {});
    exits.record(target());
    await exits.flush(b);
    expect(exits.blocked("h", "p")).toBe(true);
    await exits.flush(b);
    expect(exits.blocked("h", "p")).toBe(false);
    // 两次请求打的是同一个票据（没有重新签到新一代）。
    const params = (b.client.request as unknown as ReturnType<typeof vi.fn>).mock
      .calls[0][1];
    expect(params).toEqual(target());
    exits.dispose();
  });

  it("does not send while disconnected, and sends nothing for another host", async () => {
    const request = vi.fn(async () => ({}));
    const opening = new SideExitCoordinator(() => {});
    opening.record(target());
    await opening.flush(binding({ connected: false, request }));
    expect(request).not.toHaveBeenCalled();
    expect(opening.blocked("h", "p")).toBe(true);

    // 换了宿主实例：旧票据不该发给它。
    await opening.flush(binding({ instanceId: "h2", request }));
    expect(request).not.toHaveBeenCalled();
    opening.dispose();
  });

  it("drops the intent when the host was replaced, and says it cannot be verified", () => {
    const exits = new SideExitCoordinator(() => {});
    exits.record(target());
    exits.hostReplaced("h");
    expect(exits.blocked("h", "p")).toBe(false);
    expect(exits.lastNotice()).toContain("无法核实");
    exits.dispose();
  });

  it("does not close a shared environment from an observe device", async () => {
    const request = vi.fn(async () => ({}));
    const exits = new SideExitCoordinator(() => {});
    exits.record(target());
    await exits.flush(binding({ scope: "observe", request }));
    // 只读端本地离开：不发 exit，也不无限期占着屏障。
    expect(request).not.toHaveBeenCalled();
    expect(exits.blocked("h", "p")).toBe(false);
    exits.dispose();
  });

  it("does not send exit to a host without the lifecycle capability", async () => {
    const request = vi.fn(async () => ({}));
    const exits = new SideExitCoordinator(() => {});
    exits.record(target());
    await exits.flush(binding({ snapshotLifecycle: false, request }));
    expect(request).not.toHaveBeenCalled();
    expect(exits.blocked("h", "p")).toBe(false);
    expect(exits.lastNotice()).toContain("无法核实");
    exits.dispose();
  });

  it("is idempotent on repeated record and ignores an incomplete ticket", () => {
    const exits = new SideExitCoordinator(() => {});
    exits.record(target());
    exits.record(target());
    expect(exits.pending()).toHaveLength(1);
    exits.record(target({ entryId: "" }));
    expect(exits.pending()).toHaveLength(1);
    exits.dispose();
  });

  it("keys on the full tuple so a colon in a parent id cannot alias another ticket", () => {
    const a = target({ parentThreadId: "a:b", entryId: "c" });
    const b = target({ parentThreadId: "a", entryId: "b:c" });
    expect(sideExitKey(a)).not.toBe(sideExitKey(b));
  });

  it("never stores the question or the transcript", () => {
    const exits = new SideExitCoordinator(() => {});
    exits.record(target());
    // 账本里只有身份：任何序列化结果都不该出现正文。
    const dump = JSON.stringify(exits.pending());
    expect(dump).not.toContain("question");
    expect(dump).not.toContain("items");
    exits.dispose();
  });

  it("stops touching the client after dispose", async () => {
    const request = vi.fn(async () => ({}));
    const exits = new SideExitCoordinator(() => {});
    exits.record(target());
    exits.dispose();
    await exits.flush(binding({ request }));
    expect(request).not.toHaveBeenCalled();
  });
});
