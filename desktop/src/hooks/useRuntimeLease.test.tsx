/** @vitest-environment jsdom */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { renderHook } from "@testing-library/react";
import { useRuntimeLease, type LeaseDeps } from "./useRuntimeLease";

const calls: Array<{ cmd: string; args: unknown }> = [];

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args: unknown) => {
    calls.push({ cmd, args });
    // The shell answers `runtime_scope_for_project` with the key it resolved. A
    // bare `undefined` would make the hook skip every release, so the double has
    // to answer the way the real command does.
    if (cmd === "runtime_scope_for_project") {
      const project = (args as { project: string }).project;
      return Promise.resolve(`${project}/.yi-agent/runtime`);
    }
    return Promise.resolve();
  },
}));

// The hook short-circuits for the remote web client; these tests are desktop.
vi.mock("../lib/platform", () => ({ isRemoteClient: () => false }));

function deps(over: Partial<LeaseDeps> = {}): LeaseDeps {
  return {
    activeProject: "/p/a",
    threads: [],
    revision: "a",
    ...over,
  };
}

const flush = async () => {
  for (let i = 0; i < 5; i += 1) await Promise.resolve();
};

describe("useRuntimeLease", () => {
  beforeEach(() => {
    calls.length = 0;
  });

  it("acquires the open project through the shell command", async () => {
    renderHook(() => useRuntimeLease(deps()));
    await flush();
    expect(calls).toEqual([
      { cmd: "runtime_acquire_project", args: { project: "/p/a" } },
      // The key is asked for, not computed: one canonicalisation lives in the
      // shell, and release must speak the same one.
      { cmd: "runtime_scope_for_project", args: { project: "/p/a" } },
    ]);
  });

  it("releases a project once the user leaves and nothing is busy", async () => {
    const { rerender } = renderHook((props: LeaseDeps) => useRuntimeLease(props), {
      initialProps: deps(),
    });
    await flush();
    calls.length = 0;

    rerender(deps({ activeProject: null, revision: "none" }));
    await flush();

    expect(calls).toEqual([
      { cmd: "runtime_release_project", args: { scopeKey: "/p/a/.yi-agent/runtime" } },
    ]);
  });

  it("keeps a lease while background work continues", async () => {
    const { rerender } = renderHook((props: LeaseDeps) => useRuntimeLease(props), {
      initialProps: deps(),
    });
    await flush();
    calls.length = 0;

    rerender(
      deps({
        activeProject: "/p/b",
        threads: [{ cwd: "/p/a", status: "running" }],
        revision: "b|a",
      }),
    );
    await flush();

    expect(calls).toEqual([
      { cmd: "runtime_acquire_project", args: { project: "/p/b" } },
      { cmd: "runtime_scope_for_project", args: { project: "/p/b" } },
    ]);
  });

  it("does nothing when the wanted set is unchanged", async () => {
    const { rerender } = renderHook((props: LeaseDeps) => useRuntimeLease(props), {
      initialProps: deps(),
    });
    await flush();
    calls.length = 0;

    rerender(deps({ revision: "a" }));
    await flush();

    expect(calls).toEqual([]);
  });

  it("does not acquire the same project twice across revisions", async () => {
    const { rerender } = renderHook((props: LeaseDeps) => useRuntimeLease(props), {
      initialProps: deps(),
    });
    await flush();
    calls.length = 0;

    rerender(deps({ revision: "a2" }));
    await flush();

    expect(calls).toEqual([]);
  });
});
