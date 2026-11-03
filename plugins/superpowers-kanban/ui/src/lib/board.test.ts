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
    expect(calls).toEqual([{ method: METHODS.switchWrite, params: { on: true } }]);
  });

  it("passes both paths through the enqueue method", async () => {
    const { api, calls } = host();
    await enqueueCard(api, "/p/a.spec.md", "/p/a.plan.md");
    expect(calls).toEqual([
      { method: METHODS.enqueue, params: { spec_path: "/p/a.spec.md", plan_path: "/p/a.plan.md" } },
    ]);
  });

  it("sends the trimmed title for a pre-queued card", async () => {
    const { api, calls } = host();
    await enqueuePrequeuedCard(api, "  一个想法  ", null);
    expect(calls).toEqual([
      { method: METHODS.enqueuePrequeued, params: { title: "一个想法" } },
    ]);
  });

  it("includes the discussion session when there is one", async () => {
    const { api, calls } = host();
    await enqueuePrequeuedCard(api, "想法", "thread-abc");
    expect(calls[0].params).toEqual({ title: "想法", origin_thread_id: "thread-abc" });
  });

  it("omits the session key rather than sending an empty string", async () => {
    // An empty string is not "no session": the backend would record a thread id
    // that does not exist and the card would link nowhere.
    const { api, calls } = host();
    await enqueuePrequeuedCard(api, "想法", "   ");
    expect(calls[0].params).toEqual({ title: "想法" });
  });

  it("refuses a blank title without sending a request", async () => {
    const { api, calls } = host();
    await expect(enqueuePrequeuedCard(api, "   ", null)).rejects.toThrow(/non-empty/);
    expect(calls).toEqual([]);
  });

  it("passes id and both paths through the promote method", async () => {
    const { api, calls } = host();
    await promoteCard(api, "card-1", "/p/a.spec.md", "/p/a.plan.md");
    expect(calls).toEqual([
      {
        method: METHODS.promote,
        params: { id: "card-1", spec_path: "/p/a.spec.md", plan_path: "/p/a.plan.md" },
      },
    ]);
  });

  it("reads and writes settings through their own methods", async () => {
    const settings = { default_max_tasks: 3, interval_secs: 30, windows: [] };
    const { api, calls } = host(() => ({ settings }));
    expect(await readSettings(api)).toEqual(settings);
    await writeSettings(api, settings);
    expect(calls.map((c) => c.method)).toEqual([METHODS.settingsRead, METHODS.settingsWrite]);
    expect(calls[1].params).toEqual({ settings });
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
    expect(result).toHaveBeenCalledWith("r-7");
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
