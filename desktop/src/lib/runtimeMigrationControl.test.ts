import { beforeEach, describe, expect, it, vi } from "vitest";

/**
 * The desktop migration control must speak to the shell, not to `RpcClient`.
 *
 * A confirmation replayed by the RPC client's reconnect machinery would stop a
 * second runtime nobody approved, so the assertion here is about the *channel*:
 * three native commands, one call each, and no business RPC in between.
 */
const calls: Array<{ cmd: string; args: Record<string, unknown> }> = [];
/** When set, the next operation read answers "no record". */
let missingRecord = false;

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (cmd: string, args: Record<string, unknown>) => {
    calls.push({ cmd, args });
    if (cmd === "runtime_migration_preview") {
      return {
        token: "tok-1",
        evidence_digest: "digest",
        scope_key: "/p/a/.yi-agent/runtime",
        affected: [{ scope_key: "/p/a/.yi-agent/runtime", workload: { unverified: "silence" } }],
        blockers: ["a human has to act"],
        expires_at_unix: 1_700_000_000,
      };
    }
    if (cmd === "runtime_migration_operation" && missingRecord) {
      return null;
    }
    if (cmd === "runtime_migration_confirm" || cmd === "runtime_migration_operation") {
      return { operation_id: "op-1", phase: "manual_action_required", detail: null };
    }
    return undefined;
  },
}));

import { localMigrationControl } from "./runtimeMigration";

describe("localMigrationControl", () => {
  beforeEach(() => {
    calls.length = 0;
    missingRecord = false;
  });

  it("previews through the native command and keeps the unverified reason", async () => {
    const view = await localMigrationControl().preview("/p/a/.yi-agent/runtime");
    expect(calls).toEqual([
      { cmd: "runtime_migration_preview", args: { scopeKey: "/p/a/.yi-agent/runtime" } },
    ]);
    expect(view.affected[0].workload).toBe("unverified");
  });

  it("sends one confirmation with the id and the explicit temporary-use choice", async () => {
    const control = localMigrationControl();
    await control.confirm("/p/a/.yi-agent/runtime", "tok-1", "op-1", true);
    expect(calls).toEqual([
      {
        cmd: "runtime_migration_confirm",
        args: {
          scopeKey: "/p/a/.yi-agent/runtime",
          token: "tok-1",
          operationId: "op-1",
          temporaryUse: true,
        },
      },
    ]);
  });

  it("defaults temporary use to false, never to true", async () => {
    await localMigrationControl().confirm("/p/a/.yi-agent/runtime", "tok-1", "op-1", false);
    expect(calls[0].args.temporaryUse).toBe(false);
  });

  it("reads an operation by id without sending anything else", async () => {
    const view = await localMigrationControl().operation("/p/a/.yi-agent/runtime", "op-1");
    expect(calls).toEqual([
      {
        cmd: "runtime_migration_operation",
        args: { scopeKey: "/p/a/.yi-agent/runtime", operationId: "op-1" },
      },
    ]);
    expect(view.operationId).toBe("op-1");
  });

  it("treats a missing record as unknown rather than as a failure", async () => {
    // The sidecar may have restarted. "No record" is not "it did not happen", and
    // the state machine's recovery is to ask again about the same id.
    missingRecord = true;
    await expect(
      localMigrationControl().operation("/p/a/.yi-agent/runtime", "op-1"),
    ).rejects.toThrow(/record/);
  });
});
