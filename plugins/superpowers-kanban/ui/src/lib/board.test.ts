import { describe, expect, it, vi } from "vitest";
import type { PluginHostApi } from "@yi-agent/plugin-sdk";
import {
  enqueueCard,
  enqueuePrequeuedCard,
  listCards,
  promoteCard,
  readSettings,
  readSwitch,
  writeSettings,
  writeSwitch,
} from "./board";
import { METHODS } from "./methods";

/** Minimal host: records the calls and answers with whatever the test wants. */
function host(answer: (method: string, params: unknown) => unknown = () => ({})) {
  const calls: Array<{ method: string; params: unknown }> = [];
  const api = {
    request: vi.fn(async (method: string, params: unknown) => {
      calls.push({ method, params });
      return answer(method, params);
    }),
    result: vi.fn(async (_id: string) => ({ status: "unknown", requestId: _id, result: null })),
  } as unknown as PluginHostApi;
  return { api, calls };
}

/**
 * Params minus the generated request id.
 *
 * Assertions about payload shape should not break every time a write gains an
 * envelope field. The id itself is asserted separately, where it matters.
 */
function withoutRequestId(params: unknown): Record<string, unknown> {
  const copy = { ...(params as Record<string, unknown>) };
  delete copy.request_id;
  return copy;
}

/** A host whose write rejects with the given error and whose ledger answers `status`. */
function failingWriteHost(
  error: unknown,
  status: "committed" | "rejected" | "unknown",
) {
  const result = vi.fn(async (id: string) => ({ status, requestId: id, result: null }));
  const api = {
    request: vi.fn(async () => {
      throw error;
    }),
    result,
  } as unknown as PluginHostApi;
  return { api, result };
}

describe("board adapter", () => {
  it("reads cards through the declared list method", async () => {
    const { api, calls } = host(() => ({ cards: [{ id: "a", state: "queued" }] }));
    expect(await listCards(api)).toEqual([{ id: "a", state: "queued" }]);
    expect(calls).toEqual([{ method: METHODS.list, params: {} }]);
  });

  it("tolerates a missing cards field", async () => {
    const { api } = host(() => ({}));
    expect(await listCards(api)).toEqual([]);
  });

  it("reads the switch and its source", async () => {
    const { api, calls } = host(() => ({ on: true, source: "global" }));
    expect(await readSwitch(api)).toEqual({ on: true, source: "global" });
    expect(calls[0].method).toBe(METHODS.switchRead);
  });

  it("treats an unknown switch source as the default layer", async () => {
    const { api } = host(() => ({ on: false, source: "somewhere-else" }));
    expect(await readSwitch(api)).toEqual({ on: false, source: "default" });
  });

  it("writes the switch value it was given", async () => {
    const { api, calls } = host();
    await writeSwitch(api, true);
    expect(calls).toHaveLength(1);
    expect(calls[0].method).toBe(METHODS.switchWrite);
    expect(withoutRequestId(calls[0].params)).toEqual({ on: true });
  });

  it("passes both paths through the enqueue method", async () => {
    const { api, calls } = host();
    await enqueueCard(api, "/p/a.spec.md", "/p/a.plan.md");
    expect(calls).toHaveLength(1);
    expect(calls[0].method).toBe(METHODS.enqueue);
    expect(withoutRequestId(calls[0].params)).toEqual({
      spec_path: "/p/a.spec.md",
      plan_path: "/p/a.plan.md",
    });
  });

  it("sends the trimmed title for a pre-queued card", async () => {
    const { api, calls } = host();
    await enqueuePrequeuedCard(api, "  一个想法  ", null);
    expect(calls).toHaveLength(1);
    expect(calls[0].method).toBe(METHODS.enqueuePrequeued);
    expect(withoutRequestId(calls[0].params)).toEqual({ title: "一个想法" });
  });

  it("includes the discussion session when there is one", async () => {
    const { api, calls } = host();
    await enqueuePrequeuedCard(api, "想法", "thread-abc");
    expect(withoutRequestId(calls[0].params)).toEqual({
      title: "想法",
      origin_thread_id: "thread-abc",
    });
  });

  it("omits the session key rather than sending an empty string", async () => {
    // An empty string is not "no session": the backend would record a thread id
    // that does not exist and the card would link nowhere.
    const { api, calls } = host();
    await enqueuePrequeuedCard(api, "想法", "   ");
    expect(withoutRequestId(calls[0].params)).toEqual({ title: "想法" });
  });

  it("refuses a blank title without sending a request", async () => {
    const { api, calls } = host();
    await expect(enqueuePrequeuedCard(api, "   ", null)).rejects.toThrow(/non-empty/);
    expect(calls).toEqual([]);
  });

  it("passes id and both paths through the promote method", async () => {
    const { api, calls } = host();
    await promoteCard(api, "card-1", "/p/a.spec.md", "/p/a.plan.md");
    expect(calls).toHaveLength(1);
    expect(calls[0].method).toBe(METHODS.promote);
    expect(withoutRequestId(calls[0].params)).toEqual({
      id: "card-1",
      spec_path: "/p/a.spec.md",
      plan_path: "/p/a.plan.md",
    });
  });

  it("reads and writes settings through their own methods", async () => {
    const settings = { default_max_tasks: 3, interval_secs: 30, windows: [] };
    const { api, calls } = host(() => ({ settings }));
    expect(await readSettings(api)).toEqual(settings);
    await writeSettings(api, settings);
    expect(calls.map((c) => c.method)).toEqual([METHODS.settingsRead, METHODS.settingsWrite]);
    expect(withoutRequestId(calls[1].params)).toEqual({ settings });
  });

  it("fails loudly when the backend returns no settings", async () => {
    const { api } = host(() => ({}));
    await expect(readSettings(api)).rejects.toThrow(/no settings/);
  });
});

describe("writes whose fate is unknown", () => {
  it("resolves when the ledger says the write committed after the reply was lost", async () => {
    // The host may have committed it and lost the reply. Reporting failure here
    // would send the user to enqueue the same card twice.
    const { api, result } = failingWriteHost(
      { kind: "ResultUnknown", detail: "binding changed", requestId: "r-7" },
      "committed",
    );
    await expect(enqueueCard(api, "/p/a.spec.md", "/p/a.plan.md")).resolves.toBeUndefined();
    // The id looked up is the one the write carried, not the transport's.
    const sent = (api.request as unknown as { mock: { calls: unknown[][] } }).mock.calls[0];
    expect(result).toHaveBeenCalledWith((sent[1] as { request_id?: string }).request_id);
  });

  it("still rejects when the ledger cannot say what happened", async () => {
    const { api } = failingWriteHost(
      { kind: "ResultUnknown", detail: "binding changed", requestId: "r-8" },
      "unknown",
    );
    await expect(enqueueCard(api, "/p/a.spec.md", "/p/a.plan.md")).rejects.toMatchObject({
      kind: "ResultUnknown",
    });
  });

  it("asks the backend ledger by its own id, not the transport's", async () => {
    // The transport id never reaches the backend, so only the id inside the
    // params can be looked up. Sending the wrong one would make every recovery
    // attempt answer "unknown" for a write that did commit.
    const { api, result } = failingWriteHost(
      { kind: "ResultUnknown", detail: "binding changed", requestId: "transport-1" },
      "committed",
    );
    await enqueueCard(api, "/p/a.spec.md", "/p/a.plan.md");
    expect(result).toHaveBeenCalledTimes(1);
    const sent = (api.request as unknown as { mock: { calls: unknown[][] } }).mock.calls[0];
    const sentId = (sent[1] as { request_id?: string }).request_id;
    expect(typeof sentId).toBe("string");
    expect(sentId).not.toBe("");
    expect(sentId).not.toBe("transport-1");
    expect(result).toHaveBeenCalledWith(sentId);
  });

  it("falls back to board/operation when the host has no result method", async () => {
    // A host that never implemented the generic query answers NotSupported.
    // The answer is still one declared call away, so it must not be reported
    // as unknown.
    const request = vi.fn(async (method: string) => {
      if (method === METHODS.operation) return { status: "committed", request_id: "x" };
      throw { kind: "ResultUnknown", detail: "reply lost", requestId: "x" };
    });
    const api = {
      request,
      result: vi.fn(async () => {
        throw { kind: "NotSupported", detail: "no operation result", requestId: "x" };
      }),
    } as unknown as PluginHostApi;

    await expect(enqueueCard(api, "/p/a.spec.md", "/p/a.plan.md")).resolves.toBeUndefined();
    const consulted = (request as unknown as { mock: { calls: unknown[][] } }).mock.calls[1];
    expect(consulted[0]).toBe(METHODS.operation);
    expect((consulted[1] as { request_id?: string }).request_id).toBe(
      ((request as unknown as { mock: { calls: unknown[][] } }).mock.calls[0][1] as {
        request_id?: string;
      }).request_id,
    );
  });

  it("does not resend the write when neither ledger door can answer", async () => {
    const write = vi.fn(async () => {
      throw { kind: "ResultUnknown", detail: "reply lost", requestId: "x" };
    });
    const request = vi.fn(async (method: string, params: unknown) => {
      if (method === METHODS.operation) {
        return { status: "unknown", request_id: (params as { request_id?: string }).request_id };
      }
      return write();
    });
    const api = {
      request,
      result: vi.fn(async () => ({ status: "unknown", requestId: "x", result: null })),
    } as unknown as PluginHostApi;

    await expect(promoteCard(api, "c1", "/p/a.spec.md", "/p/a.plan.md")).rejects.toMatchObject({
      kind: "ResultUnknown",
    });
    // One attempt for the write, one for the lookup. A retry would be a second
    // write, which is exactly what the ledger rule forbids.
    expect(write).toHaveBeenCalledTimes(1);
  });

  it("does not query the ledger for an error that is not about an unknown write", async () => {
    // A Conflict is an answer. Looking it up would replace a precise message
    // with a vaguer "unknown".
    const { api, result } = failingWriteHost(
      { kind: "Conflict", detail: "卡片已存在", requestId: "r-9" },
      "committed",
    );
    await expect(enqueueCard(api, "/p/a.spec.md", "/p/a.plan.md")).rejects.toMatchObject({
      kind: "Conflict",
    });
    expect(result).not.toHaveBeenCalled();
  });

  it("reports unknown when the result query itself fails", async () => {
    // A failing lookup says nothing about the original write.
    const api = {
      request: vi.fn(async () => {
        throw { kind: "ResultUnknown", detail: "lost", requestId: "r-10" };
      }),
      result: vi.fn(async () => {
        throw new Error("bridge closed");
      }),
    } as unknown as PluginHostApi;
    await expect(promoteCard(api, "c1", "/p/a.spec.md", "/p/a.plan.md")).rejects.toMatchObject({
      kind: "ResultUnknown",
    });
  });
});
