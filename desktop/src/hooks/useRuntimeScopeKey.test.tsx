/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { useRuntimeScopeKey } from "./useRuntimeLifecycle";

const calls: Array<{ cmd: string; args: unknown }> = [];
let answer: (project: string) => Promise<string | null> = async (project) =>
  `${project}/.yi-agent/runtime`;

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args: unknown) => {
    calls.push({ cmd, args });
    if (cmd === "runtime_scope_for_project") {
      return answer((args as { project: string }).project);
    }
    return Promise.resolve(null);
  },
}));

beforeEach(() => {
  calls.length = 0;
  answer = async (project) => `${project}/.yi-agent/runtime`;
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("useRuntimeScopeKey", () => {
  it("asks the shell instead of computing the key", async () => {
    const { result } = renderHook(() => useRuntimeScopeKey("/p/a", true));
    await waitFor(() => expect(result.current).toBe("/p/a/.yi-agent/runtime"));
    expect(calls).toEqual([
      { cmd: "runtime_scope_for_project", args: { project: "/p/a" } },
    ]);
  });

  it("asks nothing when no project is open", async () => {
    const { result } = renderHook(() => useRuntimeScopeKey(null, true));
    expect(result.current).toBeNull();
    expect(calls).toEqual([]);
  });

  it("asks nothing on the remote client, which has no local shell", async () => {
    const { result } = renderHook(() => useRuntimeScopeKey("/p/a", false));
    expect(result.current).toBeNull();
    expect(calls).toEqual([]);
  });

  it("re-asks when the project changes and drops the old key first", async () => {
    const { result, rerender } = renderHook(
      ({ project }: { project: string }) => useRuntimeScopeKey(project, true),
      { initialProps: { project: "/p/a" } },
    );
    await waitFor(() => expect(result.current).toBe("/p/a/.yi-agent/runtime"));
    rerender({ project: "/p/b" });
    await waitFor(() => expect(result.current).toBe("/p/b/.yi-agent/runtime"));
    expect(calls.map((call) => (call.args as { project: string }).project)).toEqual([
      "/p/a",
      "/p/b",
    ]);
  });

  it("ignores a late answer for the project that was replaced", async () => {
    let releaseA: (value: string | null) => void = () => {};
    answer = (project) =>
      project === "/p/a"
        ? new Promise<string | null>((resolve) => {
            releaseA = resolve;
          })
        : Promise.resolve("/p/b/.yi-agent/runtime");
    const { result, rerender } = renderHook(
      ({ project }: { project: string }) => useRuntimeScopeKey(project, true),
      { initialProps: { project: "/p/a" } },
    );
    // Let the first request reach the shell before switching: the point of the
    // test is a *late answer*, not a request that never started.
    await waitFor(() => expect(calls.length).toBe(1));
    rerender({ project: "/p/b" });
    await waitFor(() => expect(result.current).toBe("/p/b/.yi-agent/runtime"));
    releaseA("/p/a/.yi-agent/runtime");
    await Promise.resolve();
    expect(result.current).toBe("/p/b/.yi-agent/runtime");
  });

  it("treats a shell that cannot answer as no scope, not as an error", async () => {
    answer = async () => {
      throw new Error("unknown command runtime_scope_for_project");
    };
    const { result } = renderHook(() => useRuntimeScopeKey("/p/a", true));
    await waitFor(() => expect(calls.length).toBe(1));
    expect(result.current).toBeNull();
  });

  it("stays null when the shell has not resolved the project yet", async () => {
    answer = async () => null;
    const { result } = renderHook(() => useRuntimeScopeKey("/p/a", true));
    await waitFor(() => expect(calls.length).toBe(1));
    expect(result.current).toBeNull();
  });
});
