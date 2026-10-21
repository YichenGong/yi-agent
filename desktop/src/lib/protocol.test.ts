import { describe, it, expect } from "vitest";
import type { ThreadSummary, Workspace, WorkspaceGroup } from "./protocol";
import { wireTotalsToTotals, wireUsageToUsage } from "./protocol";

describe("workspace types", () => {
  it("shapes compile", () => {
    const w: Workspace = { path: "/tmp/a", exists: true };
    const g: WorkspaceGroup = { workspace: "/tmp/a", exists: true, threads: [] };
    expect(w.path).toBe("/tmp/a");
    expect(g.threads).toEqual([]);
  });
});

describe("thread summary types", () => {
  it("thread summary carries optional permission_mode", () => {
    const yolo: ThreadSummary = { thread_id: "t", cwd: "/a", model: "m", created_at: 0, updated_at: 0, title: null, permission_mode: "yolo" };
    const legacy: ThreadSummary = { thread_id: "t", cwd: "/a", model: "m", created_at: 0, updated_at: 0, title: null };
    expect(yolo.permission_mode).toBe("yolo");
    expect(legacy.permission_mode).toBeUndefined();
  });
});

describe("wireUsageToUsage", () => {
  it("maps the snake_case wire snapshot to the client shape", () => {
    expect(
      wireUsageToUsage({
        model: "claude-sonnet-4",
        input_tokens: 10,
        output_tokens: 2,
        cache_creation_input_tokens: 3,
        cache_read_input_tokens: 4,
      }),
    ).toEqual({ model: "claude-sonnet-4", input: 10, output: 2, cacheRead: 4, cacheWrite: 3 });
  });

  it("returns null for a missing snapshot", () => {
    expect(wireUsageToUsage(null)).toBeNull();
    expect(wireUsageToUsage(undefined)).toBeNull();
  });
});

describe("wireTotalsToTotals", () => {
  it("maps the server's cumulative counters", () => {
    expect(
      wireTotalsToTotals({
        input_tokens: 1200,
        output_tokens: 340,
        cache_read_input_tokens: 5000,
        cache_creation_input_tokens: 60,
      }),
    ).toEqual({ input: 1200, output: 340, cacheRead: 5000, cacheWrite: 60 });
  });

  it("treats a missing cache field as zero", () => {
    // 服务端恒发全部四个字段；缺字段只可能是更老的日志，按 0 处理而不是丢整组数。
    expect(wireTotalsToTotals({ input_tokens: 1, output_tokens: 2 })).toEqual({
      input: 1,
      output: 2,
      cacheRead: 0,
      cacheWrite: 0,
    });
  });

  it("returns null when the server sent no totals", () => {
    expect(wireTotalsToTotals(null)).toBeNull();
    expect(wireTotalsToTotals(undefined)).toBeNull();
  });
});
