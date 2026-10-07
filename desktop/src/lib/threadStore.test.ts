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
    expect(s.view("a").info).toEqual({ cwd: "/w", model: "m", model_ref: null });
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

  it("seeds the status of a view that exists but never heard from the push stream", () => {
    const s = new ThreadStore();
    // Selecting a thread creates its view with a default "idle" before any
    // status notification arrives; the listing is then the first real source.
    s.select("a");
    expect(s.view("a").status).toBe("idle");
    s.seed([summary("a", "running")]);
    expect(s.view("a").status).toBe("running");
  });

  it("seeds again after a thread is dropped and re-listed", () => {
    const s = new ThreadStore();
    s.seed([summary("a", "running")]);
    s.drop("a");
    s.seed([summary("a", "running")]);
    expect(s.view("a").status).toBe("running");
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

  it("keeps an unsent draft per thread and returns it on switch-back", () => {
    const s = new ThreadStore();
    s.select("a");
    s.setDraft("a", "half-typed");
    // Switching to another thread must not expose a's draft there.
    s.select("b");
    expect(s.view("b").draft).toBe("");
    // Switching back restores exactly what a was left holding.
    s.select("a");
    expect(s.view("a").draft).toBe("half-typed");
  });

  it("drops a thread's draft together with the thread", () => {
    const s = new ThreadStore();
    s.setDraft("a", "half-typed");
    s.drop("a");
    expect(s.peek("a")).toBeUndefined();
  });

  it("updates the target thread's effective model on thread/modelChanged, keyed by thread_id", () => {
    const s = new ThreadStore();
    s.seed([summary("a"), summary("b")]);
    s.applyNotification({
      method: "thread/modelChanged",
      params: { thread_id: "b", model: "model-b" },
    });
    // 只动被点名的那个 thread。
    expect(s.view("b").info?.model).toBe("model-b");
    expect(s.view("a").info?.model).toBe("m");
  });

  it("leaves model_ref untouched on thread/modelChanged (the notification carries no ref)", () => {
    const s = new ThreadStore();
    // 会话当前有覆盖：ref A，生效 model-a。
    s.seed([{ ...summary("a"), model: "model-a", model_ref: "A" }]);
    expect(s.view("a").info?.model_ref).toBe("A");
    s.applyNotification({
      method: "thread/modelChanged",
      params: { thread_id: "a", model: "model-b" },
    });
    // 生效模型跟随通知，引用由下拉写入方自己落笔，通知不越俎代庖。
    expect(s.view("a").info?.model).toBe("model-b");
    expect(s.view("a").info?.model_ref).toBe("A");
  });

  it("ignores thread/modelChanged for a thread with no info yet", () => {
    const s = new ThreadStore();
    // 只有会话内容、还没拿到身份信息：不能凭空造一个 info。
    s.applyNotification({
      method: "thread/modelChanged",
      params: { thread_id: "a", model: "model-b" },
    });
    expect(s.view("a").info).toBeNull();
  });
});
