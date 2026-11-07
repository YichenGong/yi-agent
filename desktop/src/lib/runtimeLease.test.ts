import { describe, expect, it } from "vitest";
import {
  isHeldLease,
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
