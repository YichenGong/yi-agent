import { describe, it, expect } from "vitest";
import { ThreadStore } from "./threadStore";
import type { ApprovalRequest, Notification, ThreadSummary } from "./protocol";

const summary = (id: string, status?: ThreadSummary["status"]): ThreadSummary => ({
  thread_id: id,
  cwd: "/w",
  model: "m",
  created_at: 0,
  updated_at: 0,
  title: null,
  status,
});

const approval = (id: string, threadId: string): ApprovalRequest => ({
  id,
  params: {
    thread_id: threadId,
    turn_id: "u1",
    request_id: 1,
    tool_name: "bash",
    tool_input: {},
    prefix_suggestion: null,
    kind: "Normal",
  },
});

describe("ThreadStore", () => {
  it("routes notifications to the matching thread only", () => {
    const s = new ThreadStore();
    s.applyNotification({
      method: "item/started",
      params: { thread_id: "a", item: { type: "agentMessage", id: "x", text: "" } },
    });
    s.applyNotification({
      method: "item/delta",
      params: { thread_id: "b", item_id: "y", delta: "hi" },
    });
    expect(s.view("a").session.items).toHaveLength(1);
    expect(s.view("b").session.items).toHaveLength(1);
    expect(s.view("a").session.items[0]).toMatchObject({ id: "x" });
    expect(s.view("b").session.items[0]).toMatchObject({ id: "y" });
  });

  it("marks unread on turn/completed for a non-current thread, not the current one", () => {
    const s = new ThreadStore();
    s.select("a");
    const done = (t: string): Notification => ({
      method: "turn/completed",
      params: { thread_id: t, turn_id: "u1", status: "completed" },
    });
    s.applyNotification(done("a"));
    s.applyNotification(done("b"));
    expect(s.view("a").unread).toBe(false);
    expect(s.view("b").unread).toBe(true);
    expect(s.view("b").session.lastStatus).toBe("completed");
  });

  it("clears unread when the thread is selected", () => {
    const s = new ThreadStore();
    s.applyNotification({
      method: "turn/completed",
      params: { thread_id: "b", turn_id: "u1", status: "failed" },
    });
    expect(s.view("b").unread).toBe(true);
    s.select("b");
    expect(s.view("b").unread).toBe(false);
  });

  it("applies thread/status/updated and clears approval once it leaves awaiting", () => {
    const s = new ThreadStore();
    s.setApproval(approval("perm-1", "a"));
    expect(s.view("a").approval).not.toBeNull();
    s.applyNotification({
      method: "thread/status/updated",
      params: { thread_id: "a", status: "awaiting_approval" },
    });
    expect(s.view("a").status).toBe("awaiting_approval");
    expect(s.view("a").approval).not.toBeNull();
    s.applyNotification({
      method: "thread/status/updated",
      params: { thread_id: "a", status: "running" },
    });
    expect(s.view("a").status).toBe("running");
    expect(s.view("a").approval).toBeNull();
  });

  it("seeds status and cwd/model from a listing snapshot", () => {
    const s = new ThreadStore();
    s.seed([summary("a", "running"), summary("b")]);
    expect(s.view("a").status).toBe("running");
    expect(s.view("b").status).toBe("idle");
    expect(s.view("a").info).toEqual({ cwd: "/w", model: "m" });
  });

  it("does not let a stale listing snapshot roll back a live status", () => {
    const s = new ThreadStore();
    s.seed([summary("a", "running")]);
    // The push stream is authoritative and says the turn just finished.
    s.applyNotification({
      method: "thread/status/updated",
      params: { thread_id: "a", status: "idle" },
    });
    expect(s.view("a").status).toBe("idle");

    // A listing read *before* the driver flipped to idle arrives late.
    s.seed([summary("a", "running")]);
    expect(s.view("a").status).toBe("idle");
  });

  it("still seeds the status of a thread it has not seen before", () => {
    const s = new ThreadStore();
    s.seed([summary("b", "running")]);
    expect(s.view("b").status).toBe("running");
  });

  it("reports pending approvals for threads other than the current one", () => {
    const s = new ThreadStore();
    s.select("a");
    s.setApproval(approval("perm-1", "a"));
    s.setApproval(approval("perm-2", "b"));
    expect(s.pendingApprovalsElsewhere().map((r) => r.id)).toEqual(["perm-2"]);
  });

  it("routes a thread-less error to the current session", () => {
    const s = new ThreadStore();
    s.select("a");
    s.applyNotification({ method: "error", params: { message: "boom" } });
    expect(s.view("a").session.lastError).toBe("boom");
  });

  it("drop removes all state for a thread", () => {
    const s = new ThreadStore();
    s.select("a");
    s.setApproval(approval("perm-1", "a"));
    s.drop("a");
    expect(s.currentId).toBeNull();
    expect(s.peek("a")).toBeUndefined();
  });
});
