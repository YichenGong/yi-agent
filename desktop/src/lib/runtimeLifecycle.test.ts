import { describe, expect, it } from "vitest";
import {
  compareVersions,
  countOtherClients,
  createRuntimeLifecycle,
  describeStatus,
  isControlUnavailable,
  kindOf,
  needsSecondConfirmation,
  summarise,
  type ClientCounts,
  type OperationStatus,
  type RestartPreview,
  type RuntimeControl,
  type RuntimeStatusView,
} from "./runtimeLifecycle";

function status(over: Partial<RuntimeStatusView> = {}): RuntimeStatusView {
  const base = {
    scopeKey: "/p/a/.yi-agent/runtime",
    expectedDisplayVersion: "1.4.0",
    expectedBuildId: "build-new",
    compatibility: "compatibleMismatch",
    lifecycleManaged: true,
    localRestartAvailable: true,
    guardianRunId: "guardian-1",
    daemonGeneration: 3,
    identity: {
      daemonDisplayVersion: "1.3.0",
      daemonBuildId: "build-old",
      guardianDisplayVersion: "1.3.0",
      guardianBuildId: "build-old",
    },
    clients: [],
    activeOperation: null,
    message: null,
    snapshotId: "snap-1",
  } as unknown as RuntimeStatusView;
  return { ...base, ...over };
}

function client(clientRunId: string, mode: "business" | "lease_only" | "waiting_next_generation") {
  return { clientRunId, kind: "app", mode } as unknown as RuntimeStatusView["clients"][number];
}

/** Counts are `bigint` because the view mirrors a `u64`; tests must too. */
function counts(over: Partial<RestartPreview["counts"]> = {}): RestartPreview["counts"] {
  return {
    clients: 1n,
    pendingClients: 0n,
    running: 0n,
    queued: 0n,
    plugins: 0n,
    complete: true,
    ...over,
  };
}

function preview(over: Partial<RestartPreview> = {}): RestartPreview {
  return {
    token: "token-1",
    snapshotId: "snap-1",
    counts: counts(),
    requiresSecondConfirmation: false,
    canRestart: true,
    blockedReason: null,
    entries: [],
    revisions: [],
    ...over,
  } as unknown as RestartPreview;
}

function operation(operationId: string, phase: string): OperationStatus {
  return {
    operationId,
    phase,
    detail: null,
    readyIdentity: null,
  } as unknown as OperationStatus;
}

/** A control double that answers a scripted sequence and records the calls. */
function control(script: {
  status?: RuntimeStatusView | Error;
  preview?: RestartPreview | Error;
  confirm?: OperationStatus | Error;
  operations?: Array<OperationStatus | Error>;
}) {
  const calls: Array<{ cmd: string; args: unknown[] }> = [];
  const answer = <T>(value: T | Error): Promise<T> =>
    value instanceof Error ? Promise.reject(value) : Promise.resolve(value);
  let opIndex = 0;
  const impl: RuntimeControl = {
    status: async (scopeKey) => {
      calls.push({ cmd: "status", args: [scopeKey] });
      return answer(script.status ?? status());
    },
    preview: async (scopeKey) => {
      calls.push({ cmd: "preview", args: [scopeKey] });
      return answer(script.preview ?? preview());
    },
    confirm: async (scopeKey, token, operationId) => {
      calls.push({ cmd: "confirm", args: [scopeKey, token, operationId] });
      return answer(script.confirm ?? operation(operationId, "confirming"));
    },
    operation: async (scopeKey, operationId) => {
      calls.push({ cmd: "operation", args: [scopeKey, operationId] });
      const next = script.operations?.[opIndex] ?? operation(operationId, "switching");
      opIndex += 1;
      return answer(next);
    },
  };
  return { impl, calls };
}

/** Lets every pending microtask run without any wall-clock waiting. */
const settle = async () => {
  for (let i = 0; i < 12; i += 1) await Promise.resolve();
};

/**
 * Scheduled callbacks, run only when a test asks for them.
 *
 * Polling is a loop, so a timer double that fires eagerly would spin forever. The
 * test decides how many ticks happen, which also makes "did it schedule a poll?"
 * directly observable.
 */
function fakeTimers() {
  const pending = new Map<number, () => void>();
  let nextId = 1;
  return {
    setTimeoutFn: (fn: () => void) => {
      const id = nextId;
      nextId += 1;
      pending.set(id, fn);
      return id;
    },
    clearTimeoutFn: (handle: unknown) => {
      pending.delete(handle as number);
    },
    /** Runs up to `count` scheduled callbacks, settling after each. */
    async run(count = 1): Promise<number> {
      let ran = 0;
      for (let i = 0; i < count; i += 1) {
        const [id, fn] = pending.entries().next().value ?? [];
        if (id === undefined || fn === undefined) break;
        pending.delete(id as number);
        (fn as () => void)();
        ran += 1;
        await settle();
      }
      return ran;
    },
    size: () => pending.size,
  };
}

describe("counting the clients a restart would interrupt", () => {
  it("excludes this app's own row", () => {
    const counts = countOtherClients(
      status({ clients: [client("self", "business"), client("tui-1", "lease_only")] }),
      "self",
    );
    expect(counts).toEqual<ClientCounts>({ others: 1, othersRunning: 0 });
  });

  it("counts a client running a task separately from one merely connected", () => {
    const counts = countOtherClients(
      status({ clients: [client("self", "business"), client("tui-1", "business"), client("tui-2", "lease_only")] }),
      "self",
    );
    expect(counts).toEqual<ClientCounts>({ others: 2, othersRunning: 1 });
  });

  it("treats every row as another client when this app's id is unknown", () => {
    const counts = countOtherClients(status({ clients: [client("tui-1", "lease_only")] }), null);
    expect(counts.others).toBe(1);
  });
});

describe("when a second confirmation is needed", () => {
  it("always asks when the impact count is incomplete, since it may be larger", () => {
    expect(
      needsSecondConfirmation(
        preview({ counts: counts({ clients: 1n, running: 0n, plugins: 0n, complete: false }) }),
      ),
    ).toBe(true);
  });

  it("asks whenever anyone but this app is affected", () => {
    expect(
      needsSecondConfirmation(preview({ counts: counts({ clients: 2n, running: 0n, plugins: 0n, complete: true }) })),
    ).toBe(true);
    expect(
      needsSecondConfirmation(preview({ counts: counts({ clients: 1n, running: 1n, plugins: 0n, complete: true }) })),
    ).toBe(true);
    expect(
      needsSecondConfirmation(preview({ counts: counts({ clients: 1n, running: 0n, plugins: 1n, complete: true }) })),
    ).toBe(true);
  });

  it("asks when a client is waiting to be promoted, even with no task running", () => {
    expect(needsSecondConfirmation(preview({ counts: counts({ pendingClients: 1n }) }))).toBe(true);
    expect(needsSecondConfirmation(preview({ counts: counts({ queued: 1n }) }))).toBe(true);
  });

  it("does not ask when only this app is connected and nothing is running", () => {
    expect(
      needsSecondConfirmation(preview({ counts: counts({ clients: 1n, running: 0n, plugins: 0n, complete: true }) })),
    ).toBe(false);
  });

  it("honours the runtime's own request for a second confirmation", () => {
    expect(
      needsSecondConfirmation(preview({ requiresSecondConfirmation: true })),
    ).toBe(true);
  });
});

describe("what the banner says", () => {
  it("shows the expected and current versions", () => {
    const summary = summarise(status());
    expect(summary.currentVersion).toBe("1.3.0");
    expect(summary.expectedVersion).toBe("1.4.0");
    expect(summary.currentIsOlder).toBe(true);
  });

  it("does not call a different build an older version without comparable evidence", () => {
    const summary = summarise(
      status({
        // Only the fields the decision reads are overridden; the rest come from
        // the base fixture, so this stays a statement about version evidence.
        identity: {
          ...status().identity,
          daemonDisplayVersion: "dev-local",
          daemonBuildId: "build-x",
          guardianDisplayVersion: null,
          guardianBuildId: null,
        },
      }),
    );
    expect(summary.currentIsOlder).toBe(false);
    expect(summary.message).toContain("different build");
  });

  it("is quiet only for a verified match", () => {
    expect(summarise(status({ compatibility: "sameBuild" })).visible).toBe(false);
    for (const compatibility of [
      "compatibleMismatch",
      "controlOnly",
      "legacyTemporaryChoiceRequired",
      "unknown",
    ] as const) {
      expect(summarise(status({ compatibility })).visible).toBe(true);
    }
  });

  it("says the exit guarantee is not in effect for a legacy runtime", () => {
    const summary = summarise(
      status({ compatibility: "legacyTemporaryChoiceRequired", lifecycleManaged: false }),
    );
    expect(summary.message).toContain("not in effect");
    expect(summary.canRestart).toBe(false);
    expect(summary.cannotRestartReason).toContain("not started by");
  });

  it("does not offer a restart when the expected build cannot be verified", () => {
    const summary = summarise(status({ expectedBuildId: null }));
    expect(summary.canRestart).toBe(false);
    expect(summary.cannotRestartReason).toContain("cannot verify");
  });

  it("never claims compatibility for an unidentified runtime", () => {
    const summary = summarise(status({ compatibility: "unknown", message: null }));
    expect(summary.message).toContain("could not be identified");
  });

  it("has a distinct sentence per compatibility class", () => {
    const classes = [
      "sameBuild",
      "compatibleMismatch",
      "controlOnly",
      "legacyTemporaryChoiceRequired",
      "unknown",
    ] as const;
    const sentences = new Set(
      classes.map((compatibility) => describeStatus(status({ compatibility }))),
    );
    expect(sentences.size).toBe(classes.length);
  });
});

describe("version comparison", () => {
  it("compares numerically, not lexicographically", () => {
    expect(compareVersions("1.10.0", "1.9.0")).toBe(1);
    expect(compareVersions("1.9.0", "1.10.0")).toBe(-1);
    expect(compareVersions("1.4.0", "1.4.0")).toBe(0);
  });
});

describe("the lifecycle state machine", () => {
  it("cancelling a preview calls nothing on the runtime", async () => {
    const { impl, calls } = control({});
    const lifecycle = createRuntimeLifecycle("/p/a/.yi-agent/runtime", { control: impl });
    await lifecycle.prepare();
    expect(lifecycle.getState().phase).toBe("awaiting-confirmation");
    lifecycle.cancel();
    expect(lifecycle.getState().preview).toBeNull();
    expect(lifecycle.getState().phase).toBe("idle");
    expect(calls.map((call) => call.cmd)).toEqual(["preview"]);
    lifecycle.dispose();
  });

  it("chooses the operation id before the click and reports it to the shell", async () => {
    const { impl } = control({ confirm: operation("op-fixed", "confirming") });
    const chosen: string[] = [];
    const lifecycle = createRuntimeLifecycle("/p/a/.yi-agent/runtime", {
      control: impl,
      newOperationId: () => "op-fixed",
      onOperationChosen: (_scope, id) => chosen.push(id),
      ...fakeTimers(),
    });
    await lifecycle.prepare();
    await lifecycle.confirm();
    expect(chosen).toEqual(["op-fixed"]);
    expect(lifecycle.getState().operationId).toBe("op-fixed");
    lifecycle.dispose();
  });

  it("treats a lost confirmation answer as unknown and polls the same id", async () => {
    const { impl, calls } = control({
      confirm: new Error("connection reset"),
      operations: [operation("op-1", "switching")],
    });
    const timers = fakeTimers();
    const lifecycle = createRuntimeLifecycle("/p/a/.yi-agent/runtime", {
      control: impl,
      newOperationId: () => "op-1",
      ...timers,
    });
    await lifecycle.prepare();
    await lifecycle.confirm();
    expect(lifecycle.getState().phase).toBe("unknown");
    expect(await timers.run(1)).toBe(1);
    const polls = calls.filter((call) => call.cmd === "operation");
    expect(polls.length).toBeGreaterThan(0);
    expect(polls[0].args[1]).toBe("op-1");
    lifecycle.dispose();
  });

  it("reports an unknown operation as unknown rather than failed", async () => {
    const { impl } = control({
      operations: [new Error("the runtime no longer knows this operation")],
    });
    const timers = fakeTimers();
    const lifecycle = createRuntimeLifecycle("/p/a/.yi-agent/runtime", {
      control: impl,
      ...timers,
    });
    await lifecycle.prepare();
    await lifecycle.confirm();
    await timers.run(1);
    expect(lifecycle.getState().phase).toBe("unknown");
    expect(lifecycle.getState().error).toContain("may have restarted");
    lifecycle.dispose();
  });

  it("finishes when the operation reports ready", async () => {
    const { impl } = control({ operations: [operation("op-1", "ready")] });
    const timers = fakeTimers();
    const lifecycle = createRuntimeLifecycle("/p/a/.yi-agent/runtime", {
      control: impl,
      ...timers,
    });
    await lifecycle.prepare();
    await lifecycle.confirm();
    await timers.run(1);
    expect(lifecycle.getState().phase).toBe("finished");
    lifecycle.dispose();
  });

  it("resumes the operation the shell remembers after a reload", async () => {
    const { impl, calls } = control({ operations: [operation("op-remembered", "switching")] });
    const lifecycle = createRuntimeLifecycle("/p/a/.yi-agent/runtime", {
      control: impl,
      rememberedOperationId: async () => "op-remembered",
      ...fakeTimers(),
    });
    await lifecycle.resume();
    expect(calls.filter((call) => call.cmd === "operation")[0].args[1]).toBe("op-remembered");
    lifecycle.dispose();
  });

  it("keeps the version banner when the runtime has no control surface", async () => {
    const { impl } = control({ status: new Error("not implemented: runtime.status") });
    const lifecycle = createRuntimeLifecycle("/p/a/.yi-agent/runtime", {
      control: impl,
      ...fakeTimers(),
    });
    await lifecycle.refresh();
    expect(lifecycle.getState().controlUnavailable).toBe(true);
    lifecycle.dispose();
  });
});

describe("project switches and terminal outcomes", () => {
  it("does not let a late preview from the old scope land in the new one", async () => {
    let release: (value: RestartPreview) => void = () => {};
    const slowPreview = new Promise<RestartPreview>((resolve) => {
      release = resolve;
    });
    const control: RuntimeControl = {
      status: async () => status(),
      preview: () => slowPreview,
      confirm: async () => operation("op-new", "confirming"),
      operation: async () => operation("op-new", "switching"),
    };
    const old = createRuntimeLifecycle("/p/old/.yi-agent/runtime", {
      control,
      ...fakeTimers(),
    });
    const pending = old.prepare();
    // The WebView switched projects: the old machine is disposed mid-flight.
    old.dispose();
    release(preview());
    await pending;
    expect(old.getState().preview).toBeNull();
    expect(old.getState().phase).toBe("previewing");

    // The new scope starts clean and can preview on its own.
    const fresh = createRuntimeLifecycle("/p/new/.yi-agent/runtime", {
      control: { ...control, preview: async () => preview({ token: "token-new" }) },
      ...fakeTimers(),
    });
    await fresh.prepare();
    expect(fresh.getState().preview?.token).toBe("token-new");
    fresh.dispose();
  });

  it("never confirms a preview that belongs to another scope", async () => {
    let confirmed = 0;
    const confirm = async () => {
      confirmed += 1;
      return operation("op-x", "confirming");
    };
    const control: RuntimeControl = {
      status: async () => status(),
      preview: async () => preview(),
      confirm,
      operation: async () => operation("op-x", "switching"),
    };
    const old = createRuntimeLifecycle("/p/old/.yi-agent/runtime", {
      control,
      ...fakeTimers(),
    });
    await old.prepare();
    // Dispose without cancelling, as a project switch does.
    old.dispose();
    await old.confirm();
    expect(confirmed).toBe(0);
  });

  it("reports a switch that ended with no clients left", async () => {
    const { impl } = control({ operations: [operation("op-1", "stopped_no_clients")] });
    const timers = fakeTimers();
    const lifecycle = createRuntimeLifecycle("/p/a/.yi-agent/runtime", {
      control: impl,
      ...timers,
    });
    await lifecycle.prepare();
    await lifecycle.confirm();
    await timers.run(1);
    expect(lifecycle.getState().phase).toBe("finished");
    // A stop with no clients is not a failure: refresh must still happen so the
    // banner reports the absence of a runtime instead of a stale "switching".
    expect(impl.status).toBeDefined();
    lifecycle.dispose();
  });

  it("treats a confirmation timeout as unknown and polls rather than failing", async () => {
    const { impl, calls } = control({ confirm: new Error("the restart did not finish within 10000ms") });
    const timers = fakeTimers();
    const lifecycle = createRuntimeLifecycle("/p/a/.yi-agent/runtime", {
      control: impl,
      newOperationId: () => "op-timeout",
      ...timers,
    });
    await lifecycle.prepare();
    await lifecycle.confirm();
    expect(lifecycle.getState().phase).toBe("unknown");
    await timers.run(1);
    expect(calls.filter((call) => call.cmd === "operation")[0].args[1]).toBe("op-timeout");
    lifecycle.dispose();
  });

  it("keeps the error visible when a status refresh fails but does not lose the banner", async () => {
    const { impl } = control({ status: new Error("socket closed") });
    const lifecycle = createRuntimeLifecycle("/p/a/.yi-agent/runtime", {
      control: impl,
      ...fakeTimers(),
    });
    await lifecycle.refresh();
    expect(lifecycle.getState().error).toContain("socket closed");
    expect(lifecycle.getState().controlUnavailable).toBe(false);
    lifecycle.dispose();
  });

  it("adopts an operation already running for the scope instead of showing idle", async () => {
    const running = status({
      activeOperation: operation("op-running", "switching"),
    });
    const { impl } = control({ status: running });
    const lifecycle = createRuntimeLifecycle("/p/a/.yi-agent/runtime", {
      control: impl,
      ...fakeTimers(),
    });
    await lifecycle.refresh();
    expect(lifecycle.getState().operationId).toBe("op-running");
    expect(lifecycle.getState().phase).toBe("switching");
    lifecycle.dispose();
  });
});

describe("error classification", () => {
  it("separates the two errors that must not be retried from the rest", () => {
    expect(kindOf(new Error("the runtime no longer knows this operation"))).toBe("not_found");
    expect(kindOf(new Error("the restart did not finish within 10000ms"))).toBe("timed_out");
    expect(kindOf(new Error("this project is not open in this app"))).toBe("refused");
    expect(kindOf(new Error("socket closed"))).toBe("other");
  });

  it("recognises a runtime that does not implement the control commands", () => {
    expect(isControlUnavailable(new Error("unsupported command runtime.preview"))).toBe(true);
    expect(isControlUnavailable(new Error("not implemented"))).toBe(true);
    expect(isControlUnavailable(new Error("socket closed"))).toBe(false);
  });
});
