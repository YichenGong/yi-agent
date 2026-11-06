import { describe, expect, it } from "vitest";
import {
  createMigration,
  describeWorkload,
  toAffectedProject,
  toOperationView,
  toPreviewView,
  migrationBlockers,
  type MigrationControl,
  type MigrationOperationView,
  type MigrationPreviewView,
} from "./runtimeMigration";

const scope = "/p/a/.yi-agent/runtime";

function preview(over: Partial<MigrationPreviewView> = {}): MigrationPreviewView {
  return {
    token: "tok-1",
    evidenceDigest: "digest-1",
    scopeKey: scope,
    affected: [],
    blockers: [],
    expiresAtUnix: 0,
    ...over,
  };
}

function operation(operationId: string, phase = "stopping"): MigrationOperationView {
  return { operationId, phase, detail: null };
}

/** A control surface that records every call and can be made to fail. */
function control(over: {
  preview?: () => Promise<MigrationPreviewView>;
  confirm?: (operationId: string, temporaryUse: boolean) => Promise<MigrationOperationView>;
  operation?: (operationId: string) => Promise<MigrationOperationView>;
} = {}) {
  const calls: Array<{ cmd: string; args: unknown }> = [];
  const impl: MigrationControl = {
    preview: async () => {
      calls.push({ cmd: "preview", args: { scope } });
      return over.preview ? over.preview() : preview();
    },
    confirm: async (_scopeKey, _token, operationId, temporaryUse) => {
      calls.push({ cmd: "confirm", args: { operationId, temporaryUse } });
      return over.confirm ? over.confirm(operationId, temporaryUse) : operation(operationId);
    },
    operation: async (_scopeKey, operationId) => {
      calls.push({ cmd: "operation", args: { operationId } });
      return over.operation ? over.operation(operationId) : operation(operationId);
    },
  };
  return { impl, calls };
}

/** Deterministic timers: the test decides when a poll fires. */
function timers() {
  const pending: Array<() => void> = [];
  return {
    setTimeoutFn: (fn: () => void) => {
      pending.push(fn);
      return pending.length - 1;
    },
    clearTimeoutFn: () => {},
    runAll: () => {
      const all = pending.splice(0);
      for (const fn of all) fn();
    },
    size: () => pending.length,
  };
}

describe("what the dialog may say about a workload", () => {
  it("says unverified instead of idle", () => {
    expect(describeWorkload("unverified")).toContain("could not be verified");
    expect(describeWorkload("unverified")).not.toContain("no work");
  });

  it("does not let an unverified project read as safe to migrate", () => {
    const blockers = migrationBlockers(
      preview({ affected: [{ scopeKey: "/p/b", workload: "unverified" }] }),
    );
    expect(blockers.length).toBe(1);
    expect(blockers[0]).toContain("could not be verified");
  });

  it("lists the other projects an operation would affect", () => {
    const affected = [
      { scopeKey: "/p/a", workload: "running" as const },
      { scopeKey: "/p/b", workload: "verifiedIdle" as const },
    ];
    expect(migrationBlockers(preview({ affected }))).toEqual([]);
    expect(describeWorkload("running")).toContain("interrupted");
  });
});

describe("the migration state machine", () => {
  it("sends no confirmation when the user cancels", async () => {
    const { impl, calls } = control();
    const migration = createMigration(scope, { control: impl, ...timers() });
    await migration.prepare();
    expect(migration.getState().phase).toBe("awaiting-confirmation");

    migration.cancel();
    expect(migration.getState().phase).toBe("idle");
    expect(migration.getState().preview).toBeNull();
    expect(calls.map((call) => call.cmd)).toEqual(["preview"]);
    migration.dispose();
  });

  it("chooses the operation id before the click and confirms exactly once", async () => {
    const timersForTest = timers();
    const { impl, calls } = control();
    const migration = createMigration(scope, {
      control: impl,
      newOperationId: () => "op-fixed",
      ...timersForTest,
    });
    await migration.prepare();
    await migration.confirm();
    expect(calls.filter((call) => call.cmd === "confirm")).toHaveLength(1);
    expect(migration.getState().operationId).toBe("op-fixed");
    migration.dispose();
  });

  it("keeps the id and polls the same operation when the answer is lost", async () => {
    const timersForTest = timers();
    const { impl, calls } = control({
      confirm: async () => {
        throw new Error("the answer was lost");
      },
    });
    const migration = createMigration(scope, {
      control: impl,
      newOperationId: () => "op-1",
      ...timersForTest,
    });
    await migration.prepare();
    await migration.confirm();

    expect(migration.getState().phase).toBe("unknown");
    expect(migration.getState().operationId).toBe("op-1");

    timersForTest.runAll();
    await Promise.resolve();
    await Promise.resolve();

    const confirms = calls.filter((call) => call.cmd === "confirm");
    const polls = calls.filter((call) => call.cmd === "operation");
    expect(confirms).toHaveLength(1);
    expect(polls.length).toBeGreaterThan(0);
    expect(
      polls.every((call) => (call.args as { operationId: string }).operationId === "op-1"),
    ).toBe(true);
    migration.dispose();
  });

  it("refuses to confirm a preview with blockers", async () => {
    const { impl, calls } = control({
      preview: async () =>
        preview({ affected: [{ scopeKey: "/p/b", workload: "unverified" }] }),
    });
    const migration = createMigration(scope, { control: impl, ...timers() });
    await migration.prepare();
    await migration.confirm();
    expect(calls.filter((call) => call.cmd === "confirm")).toHaveLength(0);
    expect(migration.getState().error).toContain("blockers");
    migration.dispose();
  });

  it("passes explicit temporary use through, scoped to this confirmation", async () => {
    const { impl, calls } = control();
    const migration = createMigration(scope, { control: impl, ...timers() });
    await migration.prepare();
    await migration.confirm(true);
    const confirm = calls.find((call) => call.cmd === "confirm");
    expect((confirm?.args as { temporaryUse: boolean }).temporaryUse).toBe(true);
    migration.dispose();
  });
});

describe("wire translation", () => {
  it("keeps an unverified workload unverified, with its reason", () => {
    const affected = toAffectedProject({
      scope_key: "/p/a/.yi-agent/runtime",
      workload: { unverified: "the registration file does not record its workload" },
    });
    expect(affected.workload).toBe("unverified");
    expect(affected.detail).toContain("does not record");
    // The one sentence that must never appear for an unproven workload.
    expect(describeWorkload(affected.workload)).not.toContain("no work is running");
  });

  it("maps verified_idle to idle and running to running", () => {
    expect(toAffectedProject({ scope_key: "/a", workload: "verified_idle" }).workload).toBe(
      "verifiedIdle",
    );
    expect(toAffectedProject({ scope_key: "/a", workload: "running" }).workload).toBe("running");
  });

  it("translates a preview into the UI's field names", () => {
    const view = toPreviewView({
      token: "tok",
      evidence_digest: "digest",
      scope_key: "/p/a/.yi-agent/runtime",
      affected: [],
      blockers: ["a human has to act"],
      expires_at_unix: 1_700_000_000,
    });
    expect(view).toEqual({
      token: "tok",
      evidenceDigest: "digest",
      scopeKey: "/p/a/.yi-agent/runtime",
      affected: [],
      blockers: ["a human has to act"],
      expiresAtUnix: 1_700_000_000,
    });
  });

  it("keeps the operation id the sidecar answered with", () => {
    expect(
      toOperationView({ operation_id: "op-1", phase: "manual_action_required", detail: "x" }),
    ).toEqual({ operationId: "op-1", phase: "manual_action_required", detail: "x" });
  });
});
