import { describe, expect, it } from "vitest";
import { findLeadThread } from "./leadThread";
import type { ThreadSummary } from "./protocol";

function thread(over: Partial<ThreadSummary>): ThreadSummary {
  return {
    thread_id: "t1",
    cwd: "/p",
    model: "m",
    created_at: 1,
    updated_at: 1,
    title: null,
    ...over,
  };
}

describe("findLeadThread", () => {
  it("returns the lead thread when one is present", () => {
    const lead = thread({ thread_id: "lead-1", role: "lead" });
    expect(findLeadThread([thread({ thread_id: "a" }), lead])?.thread_id).toBe("lead-1");
  });

  it("returns null when no thread carries the lead role", () => {
    expect(findLeadThread([thread({})])).toBeNull();
  });

  it("returns null on an empty list", () => {
    expect(findLeadThread([])).toBeNull();
  });

  it("ignores an unknown role value (旧服务端/未来取值)", () => {
    expect(findLeadThread([thread({ role: "auditor" as never })])).toBeNull();
  });

  it("returns the earliest created lead when more than one exists", () => {
    const older = thread({ thread_id: "lead-old", role: "lead", created_at: 10 });
    const newer = thread({ thread_id: "lead-new", role: "lead", created_at: 20 });
    expect(findLeadThread([newer, older])?.thread_id).toBe("lead-old");
  });
});