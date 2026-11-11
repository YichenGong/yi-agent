import { describe, expect, it } from "vitest";
import {
  isHeldLease,
  isReadyLease,
  leaseDecision,
  normalizeProject,
  wantedProjects,
  type AcquireView,
} from "./runtimeLease";

describe("normalizeProject", () => {
  it("drops trailing slashes so two spellings compare equal", () => {
    expect(normalizeProject("/p/a/")).toBe("/p/a");
    expect(normalizeProject("/p/a///")).toBe("/p/a");
  });

  it("keeps the root as the root", () => {
    expect(normalizeProject("/")).toBe("/");
  });
});

describe("leaseDecision", () => {
  it("acquires the project of the open thread", () => {
    const decision = leaseDecision({ activeProject: "/p/a", threads: [], owned: [] });
    expect(decision).toEqual({ acquire: ["/p/a"], release: [] });
  });

  it("does not acquire a project merely present in the index", () => {
    // Neither thread is busy and neither is open: a historical entry is not
    // demand, and acquiring it would keep a runtime alive for nobody.
    const decision = leaseDecision({
      activeProject: null,
      threads: [
        { cwd: "/p/a", status: "idle" },
        { cwd: "/p/b", status: "idle" },
      ],
      owned: [],
    });
    expect(decision).toEqual({ acquire: [], release: [] });
  });

  it("keeps a project that still has background work after the user leaves", () => {
    const decision = leaseDecision({
      activeProject: "/p/b",
      threads: [
        { cwd: "/p/a", status: "running" },
        { cwd: "/p/b", status: "idle" },
      ],
      owned: ["/p/a", "/p/b"],
    });
    expect(decision).toEqual({ acquire: [], release: [] });
  });

  it("releases a project the user left with no work left", () => {
    const decision = leaseDecision({
      activeProject: "/p/b",
      threads: [
        { cwd: "/p/a", status: "idle" },
        { cwd: "/p/b", status: "idle" },
      ],
      owned: ["/p/a", "/p/b"],
    });
    expect(decision).toEqual({ acquire: [], release: ["/p/a"] });
  });

  it("treats awaiting approval as work that must keep its lease", () => {
    // The turn is blocked on the user, not finished: releasing now would take the
    // runtime away from a conversation that is still open.
    const decision = leaseDecision({
      activeProject: null,
      threads: [{ cwd: "/p/a", status: "awaiting_approval" }],
      owned: ["/p/a"],
    });
    expect(decision).toEqual({ acquire: [], release: [] });
  });

  it("acquires a busy project even when the user is looking elsewhere", () => {
    const decision = leaseDecision({
      activeProject: "/p/b",
      threads: [{ cwd: "/p/a", status: "running" }],
      owned: ["/p/b"],
    });
    expect(decision).toEqual({ acquire: ["/p/a"], release: [] });
  });

  it("does nothing when the wanted set already matches the held set", () => {
    const decision = leaseDecision({
      activeProject: "/p/a/",
      threads: [],
      owned: ["/p/a"],
    });
    expect(decision).toEqual({ acquire: [], release: [] });
  });

  it("releases everything once the user leaves and nothing is working", () => {
    const decision = leaseDecision({
      activeProject: null,
      threads: [{ cwd: "/p/a" }],
      owned: ["/p/a", "/p/b"],
    });
    expect(decision).toEqual({ acquire: [], release: ["/p/a", "/p/b"] });
  });

  it("ignores a thread with no status instead of guessing it is busy", () => {
    const decision = leaseDecision({
      activeProject: null,
      threads: [{ cwd: "/p/a" }],
      owned: [],
    });
    expect(decision.acquire).toEqual([]);
  });
});

describe("acquire results are not leases", () => {
  it("treats only managedReady as a held lease", () => {
    // The four other states each mean "this project has no lease I may count on".
    // `starting` in particular is the one that used to be mistaken for success:
    // the command returned, so the old code recorded a lease it did not have.
    const starting: AcquireView = {
      scopeKey: "/p/a/.yi-agent/runtime",
      attemptId: "attempt-1",
      state: "starting",
      retry: "whenIdle",
    };
    expect(isHeldLease(starting)).toBe(false);
    for (const state of ["starting", "legacyChoiceRequired", "blocked", "unknown"] as const) {
      expect(isHeldLease({ ...starting, state })).toBe(false);
    }
    expect(isHeldLease({ ...starting, state: "managedReady" })).toBe(true);
  });

  it("reports a lease the shell still holds for a blocked project", () => {
    // A compatible version mismatch keeps its lease while the switch is being
    // decided. Dropping it on the way out would leak a registration the quit
    // barrier has to release.
    const heldBlocked: AcquireView = {
      scopeKey: "/p/a/.yi-agent/runtime",
      attemptId: "attempt-1",
      state: "blocked",
      retry: "userAction",
      held: true,
    };
    expect(isHeldLease(heldBlocked)).toBe(true);
    // Held is not ready: business work stays shut until the switch completes.
    expect(isReadyLease(heldBlocked)).toBe(false);
  });

  it("does not infer a lease from a refusal when the shell says held is false", () => {
    const refused: AcquireView = {
      scopeKey: "/p/a/.yi-agent/runtime",
      attemptId: "attempt-1",
      state: "blocked",
      retry: "backoff",
      held: false,
    };
    expect(isHeldLease(refused)).toBe(false);
    expect(isReadyLease(refused)).toBe(false);
  });

  it("treats an unknown held as held only when the runtime is ready", () => {
    // Older shells do not report held. A blocked answer from them proves nothing
    // about a lease either way, so it must not be counted as one; the hook
    // reconciles that case with `runtime_registered_projects` instead.
    const noHeld: AcquireView = {
      scopeKey: "/p/a/.yi-agent/runtime",
      attemptId: "attempt-1",
      state: "blocked",
      retry: "backoff",
    };
    expect(isHeldLease(noHeld)).toBe(false);
    expect(isHeldLease({ ...noHeld, state: "managedReady" })).toBe(true);
    // Only managedReady permits work, whatever held says.
    expect(isReadyLease({ ...noHeld, state: "managedReady" })).toBe(true);
    expect(isReadyLease({ ...noHeld, state: "blocked", held: true })).toBe(false);
  });
});

describe("a plugin view is demand for its project", () => {
  it("makes the project wanted before any thread exists", () => {
    // A board-only view has no thread, so nothing else would ask for a runtime.
    // Without this the plugin could never be started: the view waits for a
    // service that no lease keeps alive.
    const wanted = wantedProjects({
      activeProject: null,
      threads: [],
      pluginProjects: ["/p/a"],
    });
    expect(wanted).toEqual(["/p/a"]);
  });

  it("still ignores projects that only appear in the index", () => {
    const wanted = wantedProjects({
      activeProject: null,
      threads: [{ cwd: "/p/a", status: "idle" }],
      pluginProjects: [],
    });
    expect(wanted).toEqual([]);
  });
});
