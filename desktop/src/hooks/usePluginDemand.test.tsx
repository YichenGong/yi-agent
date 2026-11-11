/** @vitest-environment jsdom */
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";


import { usePluginDemand, type Writer } from "./usePluginDemand";
import {
  registerPluginConsumer,
  resetPluginConsumers,
} from "../lib/pluginDemandRegistry";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

/** The remote web client has no shell, so the hook must stay silent there. */
const platform = { remote: false };
vi.mock("../lib/platform", () => ({ isRemoteClient: () => platform.remote }));

interface Call {
  kind: "acquire" | "release" | "scope";
  project?: string;
  scopeKey?: string;
  plugin?: string;
  consumer?: string;
}

/** A writer that records calls and answers a scope for every project but one. */
function recorder(unresolved: string[] = []): { writer: Writer; calls: Call[] } {
  const calls: Call[] = [];
  return {
    calls,
    writer: {
      async acquire(scopeKey, plugin, consumer) {
        calls.push({ kind: "acquire", scopeKey, plugin, consumer });
      },
      async release(scopeKey, plugin, consumer) {
        calls.push({ kind: "release", scopeKey, plugin, consumer });
      },
      async scopeFor(project) {
        calls.push({ kind: "scope", project });
        return unresolved.includes(project) ? null : `${project}/.yi-agent/runtime`;
      },
    },
  };
}

describe("usePluginDemand", () => {
  beforeEach(() => resetPluginConsumers());

  it("cancels a pending scope retry on unmount", async () => {
    vi.useFakeTimers();
    const { writer } = recorder(["/p/a"]);
    const { unmount } = renderHook(() => usePluginDemand({ writer, retryMs: 250 }));
    await act(async () => {
      registerPluginConsumer({ project: "/p/a", plugin: "board", consumer: "v1" });
    });
    expect(vi.getTimerCount()).toBeGreaterThan(0);
    unmount();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("does not schedule a retry when scope resolution finishes after unmount", async () => {
    vi.useFakeTimers();
    let resolve!: (scope: string | null) => void;
    const { writer } = recorder(["/p/a"]);
    writer.scopeFor = vi.fn(() => new Promise<string | null>((done) => { resolve = done; }));
    const { unmount } = renderHook(() => usePluginDemand({ writer, retryMs: 250 }));
    await act(async () => {
      registerPluginConsumer({ project: "/p/a", plugin: "board", consumer: "v1" });
    });
    unmount();
    await act(async () => { resolve(null); });
    expect(vi.getTimerCount()).toBe(0);
  });

  it("acquires once for the first view and names the consumer", async () => {
    const { writer, calls } = recorder();
    const { result } = renderHook(() => usePluginDemand({ writer }));

    let release = () => {};
    act(() => {
      release = registerPluginConsumer({ project: "/p/a", plugin: "board", consumer: "v1" });
    });

    expect(result.current).toEqual(["/p/a"]);
    await waitFor(() => expect(calls.filter((c) => c.kind === "acquire")).toHaveLength(1));
    expect(calls).toContainEqual({
      kind: "acquire",
      scopeKey: "/p/a/.yi-agent/runtime",
      plugin: "board",
      consumer: "v1",
    });
    act(() => release());
  });

  it("returns the project so the lease decision can include it", () => {
    // The board-only case: no thread exists, so this list is the only reason the
    // project's runtime is ever asked for.
    const { writer } = recorder();
    const { result } = renderHook(() => usePluginDemand({ writer }));
    act(() => {
      registerPluginConsumer({ project: "/p/board-only", plugin: "board", consumer: "v1" });
    });
    expect(result.current).toEqual(["/p/board-only"]);
  });

  it("registers a second view without re-acquiring for the first", async () => {
    const { writer, calls } = recorder();
    const { result } = renderHook(() => usePluginDemand({ writer }));

    act(() => {
      registerPluginConsumer({ project: "/p/a", plugin: "board", consumer: "v1" });
    });
    await waitFor(() => expect(calls.filter((c) => c.kind === "acquire")).toHaveLength(1));

    act(() => {
      registerPluginConsumer({ project: "/p/a", plugin: "board", consumer: "v2" });
    });
    await waitFor(() => expect(calls.filter((c) => c.kind === "acquire")).toHaveLength(2));
    expect(calls.filter((c) => c.kind === "acquire").map((c) => c.consumer)).toEqual([
      "v1",
      "v2",
    ]);
    expect(result.current).toEqual(["/p/a"]);
  });

  it("withdraws exactly the view that left, not the whole plugin", async () => {
    const { writer, calls } = recorder();
    renderHook(() => usePluginDemand({ writer }));

    let releaseOne = () => {};
    act(() => {
      releaseOne = registerPluginConsumer({ project: "/p/a", plugin: "board", consumer: "v1" });
      registerPluginConsumer({ project: "/p/a", plugin: "board", consumer: "v2" });
    });
    await waitFor(() => expect(calls.filter((c) => c.kind === "acquire")).toHaveLength(2));

    act(() => releaseOne());
    await waitFor(() => expect(calls.filter((c) => c.kind === "release")).toHaveLength(1));
    expect(calls.find((c) => c.kind === "release")?.consumer).toBe("v1");
  });

  it("does not send anything for a project the shell has not resolved yet", async () => {
    // Demand precedes the lease that resolves a scope. Sending an acquire with an
    // invented key would name a runtime the shell never opened.
    const { writer, calls } = recorder(["/p/a"]);
    renderHook(() => usePluginDemand({ writer, retryMs: 5 }));

    act(() => {
      registerPluginConsumer({ project: "/p/a", plugin: "board", consumer: "v1" });
    });
    await waitFor(() => expect(calls.filter((c) => c.kind === "scope").length).toBeGreaterThan(1));
    expect(calls.filter((c) => c.kind === "acquire")).toHaveLength(0);
  });

  it("retries once the shell resolves the scope", async () => {
    const calls: Call[] = [];
    let resolved = false;
    const writer: Writer = {
      async acquire(scopeKey, plugin, consumer) {
        calls.push({ kind: "acquire", scopeKey, plugin, consumer });
      },
      async release() {},
      async scopeFor(project) {
        calls.push({ kind: "scope", project });
        return resolved ? `${project}/.yi-agent/runtime` : null;
      },
    };
    renderHook(() => usePluginDemand({ writer, retryMs: 5 }));

    act(() => {
      registerPluginConsumer({ project: "/p/a", plugin: "board", consumer: "v1" });
    });
    await waitFor(() => expect(calls.filter((c) => c.kind === "scope").length).toBeGreaterThan(1));

    resolved = true;
    await waitFor(() => expect(calls.filter((c) => c.kind === "acquire")).toHaveLength(1), {
      timeout: 2000,
    });
  });

  it("is a no-op for the remote client", async () => {
    platform.remote = true;
    try {
      const { result } = renderHook(() => usePluginDemand());
      act(() => {
        registerPluginConsumer({ project: "/p/a", plugin: "board", consumer: "v1" });
      });
      // Projects are still reported, because the caller renders them; nothing is
      // sent, because there is no shell to send it to.
      expect(result.current).toEqual(["/p/a"]);
    } finally {
      platform.remote = false;
    }
  });
});
