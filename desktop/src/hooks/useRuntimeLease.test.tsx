/** @vitest-environment jsdom */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { act, cleanup, renderHook } from "@testing-library/react";
import { useRuntimeLease, type LeaseDeps } from "./useRuntimeLease";

const calls: Array<{ cmd: string; args: unknown }> = [];

/** Default answer to `runtime_acquire_project`: a real lease. */
const leaseAnswer = (project: string) => ({
  scopeKey: `${project}/.yi-agent/runtime`,
  attemptId: `attempt-${project}`,
  state: "managedReady",
  retry: "backoff",
});

/** Per-test override, so a test can hold an answer open or answer with a refusal. */
let invokeImpl: ((cmd: string, args: unknown) => Promise<unknown>) | null = null;

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args: unknown) => {
    calls.push({ cmd, args });
    if (invokeImpl) return invokeImpl(cmd, args);
    // The shell answers `runtime_scope_for_project` with the key it resolved. A
    // bare `undefined` would make the hook skip every release, so the double has
    // to answer the way the real command does.
    if (cmd === "runtime_scope_for_project") {
      const project = (args as { project: string }).project;
      return Promise.resolve(`${project}/.yi-agent/runtime`);
    }
    if (cmd === "runtime_acquire_project") {
      return Promise.resolve(leaseAnswer((args as { project: string }).project));
    }
    return Promise.resolve();
  },
}));

// The hook short-circuits for the remote web client; these tests are desktop
// unless a case flips `remote`. `vi.hoisted` keeps the flag visible to the
// hoisted mock factory (a plain `let` would be in its temporal dead zone).
const { remote } = vi.hoisted(() => ({ remote: { value: false } }));
vi.mock("../lib/platform", () => ({ isRemoteClient: () => remote.value }));

function deps(over: Partial<LeaseDeps> = {}): LeaseDeps {
  return {
    activeProject: "/p/a",
    threads: [],
    revision: "a",
    ...over,
  };
}

const flush = async () => {
  // The hook's work is a promise chain over several awaited commands, so a few
  // turns is not enough to observe its settled state. `act` also commits the
  // state updates the hook makes from that chain.
  await act(async () => {
    for (let i = 0; i < 50; i += 1) await Promise.resolve();
  });
};

describe("useRuntimeLease", () => {
  beforeEach(() => {
    calls.length = 0;
    invokeImpl = null;
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

describe("useRuntimeLease: only real leases count", () => {
  it("does not record a lease while the shell says it is still starting", async () => {
    // `starting` means a preparation is in flight. The old code recorded the
    // project before the answer, so a blocked or slow start looked like a lease
    // and a later release acted on a scope nobody held.
    let answer: (view: unknown) => void = () => {};
    const acquire = new Promise((resolve) => {
      answer = resolve;
    });
    invokeImpl = async (cmd: string) => {
      if (cmd === "runtime_acquire_project") return acquire;
      if (cmd === "runtime_scope_for_project") return "/p/a/.yi-agent/runtime";
      return undefined;
    };

    const { rerender } = renderHook((props: LeaseDeps) => useRuntimeLease(props), {
      initialProps: deps(),
    });
    await flush();
    calls.length = 0;

    answer({ scopeKey: "/p/a/.yi-agent/runtime", attemptId: "1", state: "starting", retry: "whenIdle" });
    await flush();
    // Still nothing held: leaving now must not release a lease we never had.
    rerender(deps({ activeProject: null, revision: "none" }));
    await flush();
    expect(calls.filter((call) => call.cmd === "runtime_release_project")).toEqual([]);
  });

  it("records the lease when the shell answers managedReady", async () => {
    let answer: (view: unknown) => void = () => {};
    const acquire = new Promise((resolve) => {
      answer = resolve;
    });
    invokeImpl = async (cmd: string) => {
      if (cmd === "runtime_acquire_project") return acquire;
      if (cmd === "runtime_scope_for_project") return "/p/a/.yi-agent/runtime";
      return undefined;
    };

    const { rerender } = renderHook((props: LeaseDeps) => useRuntimeLease(props), {
      initialProps: deps(),
    });
    await flush();
    answer({ scopeKey: "/p/a/.yi-agent/runtime", attemptId: "1", state: "managedReady", retry: "backoff" });
    await flush();
    calls.length = 0;

    rerender(deps({ activeProject: null, revision: "none" }));
    await flush();
    expect(calls).toEqual([
      { cmd: "runtime_release_project", args: { scopeKey: "/p/a/.yi-agent/runtime" } },
    ]);
  });

  it("discards a late result for a project the user already left", async () => {
    // Switching A -> B while A's preparation is in flight must not resurrect A:
    // applying the late answer would record a lease the decision has since
    // released, and it would never be released again.
    let answerA: (view: unknown) => void = () => {};
    const acquireA = new Promise((resolve) => {
      answerA = resolve;
    });
    invokeImpl = async (cmd: string, args: unknown) => {
      if (cmd === "runtime_acquire_project") {
        const project = (args as { project: string }).project;
        if (project === "/p/a") return acquireA;
        return { scopeKey: "/p/b/.yi-agent/runtime", attemptId: "2", state: "managedReady", retry: "backoff" };
      }
      if (cmd === "runtime_scope_for_project") {
        const project = (args as { project: string }).project;
        return `${project}/.yi-agent/runtime`;
      }
      return undefined;
    };

    const { rerender } = renderHook((props: LeaseDeps) => useRuntimeLease(props), {
      initialProps: deps(),
    });
    await flush();

    rerender(deps({ activeProject: "/p/b", revision: "b" }));
    await flush();
    calls.length = 0;

    // A's answer arrives after the user moved on.
    answerA({ scopeKey: "/p/a/.yi-agent/runtime", attemptId: "1", state: "managedReady", retry: "backoff" });
    await flush();

    const releasedAfterLateAnswer = calls
      .filter((call) => call.cmd === "runtime_release_project")
      .map((call) => (call.args as { scopeKey: string }).scopeKey);
    // A late lease must not be left behind: the answer is "discarded" by being
    // released at once, not by being forgotten.
    expect(releasedAfterLateAnswer).toEqual(["/p/a/.yi-agent/runtime"]);

    calls.length = 0;
    rerender(deps({ activeProject: null, revision: "none" }));
    await flush();
    const released = calls
      .filter((call) => call.cmd === "runtime_release_project")
      .map((call) => (call.args as { scopeKey: string }).scopeKey);
    // And the discarded project is not resurrected by the later decision.
    expect(released).toEqual(["/p/b/.yi-agent/runtime"]);
  });
});

describe("useRuntimeLease: plugin views are demand too", () => {
  // The calls array is shared by the whole file and the hook's work is a promise
  // chain. Unmounting and draining before each case keeps a previous test's tail
  // from being read as this one's calls.
  beforeEach(async () => {
    cleanup();
    await flush();
    calls.length = 0;
  });

  it("acquires the project of a board-only plugin view", async () => {
    // No thread exists for this project. Without `pluginProjects` nothing would ask
    // for its runtime, and the board would wait forever for a service no lease keeps
    // alive.
    renderHook(() =>
      useRuntimeLease(
        deps({ activeProject: null, threads: [], pluginProjects: ["/p/board"], revision: "board" }),
      ),
    );
    await flush();

    expect(calls).toEqual([
      { cmd: "runtime_acquire_project", args: { project: "/p/board" } },
      { cmd: "runtime_scope_for_project", args: { project: "/p/board" } },
    ]);
  });

  it("does not acquire a project that is only static sidebar history", async () => {
    // A historical entry is not demand: it must not keep a daemon alive after
    // everyone left.
    renderHook(() =>
      useRuntimeLease(
        deps({
          activeProject: "/p/now",
          threads: [{ cwd: "/p/old", status: "completed" }],
          revision: "now",
        }),
      ),
    );
    await flush();

    const projects = calls
      .filter((call) => call.cmd === "runtime_acquire_project")
      .map((call) => (call.args as { project: string }).project);
    expect(projects).toEqual(["/p/now"]);
    expect(projects).not.toContain("/p/old");
  });

  it("releases a project once its last plugin view leaves", async () => {
    const { rerender } = renderHook((props: LeaseDeps) => useRuntimeLease(props), {
      initialProps: deps({ activeProject: null, pluginProjects: ["/p/board"], revision: "board" }),
    });
    await flush();
    calls.length = 0;

    rerender(deps({ activeProject: null, pluginProjects: [], revision: "none" }));
    await flush();

    expect(calls).toEqual([
      { cmd: "runtime_release_project", args: { scopeKey: "/p/board/.yi-agent/runtime" } },
    ]);
  });

  it("keeps the lease while any other reason to want the project remains", async () => {
    const { rerender } = renderHook((props: LeaseDeps) => useRuntimeLease(props), {
      initialProps: deps({ activeProject: "/p/a", pluginProjects: ["/p/a"], revision: "a" }),
    });
    await flush();
    calls.length = 0;

    // The plugin view closed, but the conversation in the same project is still
    // open: the project is still wanted.
    rerender(deps({ activeProject: "/p/a", pluginProjects: [], revision: "a" }));
    await flush();

    expect(calls).toEqual([]);
  });

  it("acquires a plugin project once even with two views of it", async () => {
    renderHook(() =>
      useRuntimeLease(
        deps({
          activeProject: null,
          pluginProjects: ["/p/board"],
          revision: "board",
        }),
      ),
    );
    await flush();
    calls.length = 0;

    // Two views are aggregated into one project upstream, so the lease sees one
    // entry; a second decision with the same set must not acquire again.
    expect(calls).toEqual([]);
  });
});

describe("useRuntimeLease: preparation failures are reported, not swallowed", () => {
  beforeEach(async () => {
    cleanup();
    await flush();
    calls.length = 0;
    invokeImpl = null;
    remote.value = false;
  });

  /** A refused preparation, as the shell reports it. */
  const blocked = (project: string, reason = "runtime refused a lease") => ({
    scopeKey: `${project}/.yi-agent/runtime`,
    attemptId: `attempt-${project}`,
    state: "blocked",
    retry: "backoff",
    reason,
  });

  it("reports a blocked preparation against its project", async () => {
    invokeImpl = async (cmd) => {
      if (cmd === "runtime_acquire_project") return blocked("/p/a");
      return undefined;
    };

    const { result } = renderHook(() => useRuntimeLease(deps()));
    await flush();

    const issue = result.current.issues.get("/p/a");
    expect(issue?.project).toBe("/p/a");
    expect(issue?.stage).toBe("acquire");
    expect(issue?.reason).toContain("refused a lease");
    expect(issue?.attemptId).toBe("attempt-/p/a");
  });

  it("does not spin: a blocked answer is not retried until the user asks", async () => {
    let acquireCount = 0;
    invokeImpl = async (cmd, args) => {
      if (cmd === "runtime_acquire_project") {
        acquireCount += 1;
        return blocked((args as { project: string }).project);
      }
      return undefined;
    };

    const { rerender } = renderHook((props: LeaseDeps) => useRuntimeLease(props), {
      initialProps: deps(),
    });
    await flush();
    expect(acquireCount).toBe(1);

    // Re-rendering with the same wanted set must not start another preparation.
    rerender(deps({ revision: "a" }));
    await flush();
    rerender(deps({ revision: "a" }));
    await flush();
    expect(acquireCount).toBe(1);
  });

  it("retries the same wanted project once when the user asks", async () => {
    let acquireCount = 0;
    invokeImpl = async (cmd, args) => {
      if (cmd === "runtime_acquire_project") {
        acquireCount += 1;
        return blocked((args as { project: string }).project);
      }
      if (cmd === "runtime_scope_for_project") {
        return `${(args as { project: string }).project}/.yi-agent/runtime`;
      }
      return undefined;
    };

    const { result } = renderHook(() => useRuntimeLease(deps()));
    await flush();
    expect(acquireCount).toBe(1);

    act(() => result.current.retryProject("/p/a"));
    await flush();
    expect(acquireCount).toBe(2);
    // One retry is one preparation; it must not loop on its own.
    await flush();
    expect(acquireCount).toBe(2);
  });

  it("drops a late failure for a project the user already left", async () => {
    let answerA: (view: unknown) => void = () => {};
    const acquireA = new Promise((resolve) => {
      answerA = resolve;
    });
    invokeImpl = async (cmd, args) => {
      if (cmd === "runtime_acquire_project") {
        const project = (args as { project: string }).project;
        if (project === "/p/a") return acquireA;
        return {
          scopeKey: `${project}/.yi-agent/runtime`,
          attemptId: `attempt-${project}`,
          state: "managedReady",
          retry: "doNotRetry",
          held: true,
        };
      }
      if (cmd === "runtime_scope_for_project") {
        return `${(args as { project: string }).project}/.yi-agent/runtime`;
      }
      return undefined;
    };

    const { result, rerender } = renderHook((props: LeaseDeps) => useRuntimeLease(props), {
      initialProps: deps(),
    });
    await flush();

    rerender(deps({ activeProject: "/p/b", revision: "b" }));
    await flush();

    answerA(blocked("/p/a", "late refusal"));
    await flush();

    // A is wanted no longer, so its failure is not the current project's problem.
    expect(result.current.issues.get("/p/a")).toBeUndefined();
    expect(result.current.issues.get("/p/b")).toBeUndefined();
  });

  it("releases a blocked project the shell still holds when the user leaves", async () => {
    invokeImpl = async (cmd, args) => {
      if (cmd === "runtime_acquire_project") {
        const project = (args as { project: string }).project;
        return {
          ...blocked(project, "a newer runtime is serving this project"),
          held: true,
          retry: "userAction",
        };
      }
      if (cmd === "runtime_scope_for_project") {
        return `${(args as { project: string }).project}/.yi-agent/runtime`;
      }
      return undefined;
    };

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

  it("keeps a blocked project's failure visible after the lease is released", async () => {
    invokeImpl = async (cmd) => {
      if (cmd === "runtime_acquire_project") {
        return {
          ...blocked("/p/a", "a newer runtime is serving this project"),
          held: true,
          retry: "userAction",
        };
      }
      if (cmd === "runtime_scope_for_project") return "/p/a/.yi-agent/runtime";
      return undefined;
    };

    const { result, rerender } = renderHook((props: LeaseDeps) => useRuntimeLease(props), {
      initialProps: deps(),
    });
    await flush();
    expect(result.current.issues.get("/p/a")?.stage).toBe("acquire");

    rerender(deps({ activeProject: null, revision: "none" }));
    await flush();
    // Leaving is not a retry: the reason must still be there for the user to act on.
    expect(result.current.issues.get("/p/a")?.reason).toContain("newer runtime");
  });

  it("makes no local preparation calls for the remote client", async () => {
    // The remote web client has no shell and no sidecar, so a local command would
    // fail for a reason that says nothing about the project. Keep it a no-op.
    remote.value = true;
    renderHook(() => useRuntimeLease(deps()));
    await flush();
    expect(calls.filter((c) => c.cmd === "runtime_acquire_project")).toEqual([]);
  });
});
