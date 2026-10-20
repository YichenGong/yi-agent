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

describe("applyNotification render signal", () => {
  it("reports no visible change for tool-output deltas", () => {
    // 调用方（App）只在 true 时 `force()`。bash 的流式输出每 4 KiB 一帧，
    // 基线 UI 不渲染它；这条返回 false 就是「别为看不见的字节重绘整棵树」。
    const s = new ThreadStore();
    s.applyNotification({
      method: "item/started",
      params: {
        thread_id: "t",
        item: {
          type: "toolCall",
          id: "i1",
          call_id: "c1",
          name: "bash",
          input: {},
          status: "running",
        },
      },
    });
    expect(
      s.applyNotification({
        method: "item/delta",
        params: { thread_id: "t", item_id: "i1", delta: "build log line" },
      }),
    ).toBe(false);
    expect(
      s.applyNotification({
        method: "item/delta",
        params: { thread_id: "t", item_id: "a1", delta: "answer" },
      }),
    ).toBe(true);
  });
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

  it("clears turnActive when an interrupt reports there was nothing running", () => {
    const s = new ThreadStore();
    s.applyNotification({
      method: "turn/started",
      params: { thread_id: "a", turn_id: "u1" },
    });
    expect(s.view("a").session.turnActive).toBe(true);
    // 服务端明确回答「本来就没在跑」：本地的 Stop 是过期状态，就地纠正。
    s.view("a").session.clearTurn();
    expect(s.view("a").session.turnActive).toBe(false);
  });

  it("drops a stale turnActive when the server says the thread is idle", () => {
    const s = new ThreadStore();
    // 一轮开始：turn/started 把按钮切成 Stop。
    s.applyNotification({
      method: "turn/started",
      params: { thread_id: "a", turn_id: "u1" },
    });
    expect(s.view("a").session.turnActive).toBe(true);
    // 队列被逐字流灌满时 turn/completed 可能被丢弃；但状态推送仍然到达：
    // 「thread 已 idle」是更权威的事实，必须能纠正卡住的 Stop。
    s.applyNotification({
      method: "thread/status/updated",
      params: { thread_id: "a", status: "idle" },
    });
    expect(s.view("a").status).toBe("idle");
    expect(s.view("a").session.turnActive).toBe(false);
  });

  it("keeps turnActive while the server still reports the thread as running", () => {
    const s = new ThreadStore();
    s.applyNotification({
      method: "turn/started",
      params: { thread_id: "a", turn_id: "u1" },
    });
    s.applyNotification({
      method: "thread/status/updated",
      params: { thread_id: "a", status: "running" },
    });
    expect(s.view("a").session.turnActive).toBe(true);
    s.applyNotification({
      method: "thread/status/updated",
      params: { thread_id: "a", status: "awaiting_approval" },
    });
    // 等审批仍是同一轮的延续：Stop 必须保持可用（它正是取消审批轮的手段）。
    expect(s.view("a").session.turnActive).toBe(true);
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

  it("seeds the permission mode from a listing snapshot, without inventing one", () => {
    const s = new ThreadStore();
    s.seed([
      { ...summary("a"), permission_mode: "yolo" },
      { ...summary("b") },
    ]);
    // Present in the snapshot → adopt the authoritative value.
    expect(s.view("a").mode).toBe("yolo");
    // Absent from the snapshot → unknown, never fabricated as "normal".
    expect(s.view("b").mode).toBeNull();
  });

  it("never lets a listing snapshot clobber a known or push-written mode", () => {
    const s = new ThreadStore();
    // The push stream is authoritative and just said YOLO.
    s.applyNotification({
      method: "thread/permissionModeChanged",
      params: { thread_id: "a", mode: "yolo" },
    });
    // A snapshot that omits the field (older server) must not erase it.
    s.seed([summary("a")]);
    expect(s.view("a").mode).toBe("yolo");
    // Nor may a snapshot for a thread it no longer lists.
    s.seed([summary("b")]);
    expect(s.view("a").mode).toBe("yolo");
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

  it("forgetPushStatuses lets a fresh listing re-seed a stale running status", () => {
    const s = new ThreadStore();
    // A turn was running: the push stream marked the status authoritative.
    s.applyNotification({
      method: "thread/status/updated",
      params: { thread_id: "a", status: "running" },
    });
    // The sidecar crashes; the new process reports the thread as idle. Without
    // forgetting the push, `seed` would refuse to overwrite the stale `running`.
    s.seed([summary("a", "idle")]);
    expect(s.view("a").status).toBe("running");

    s.forgetPushStatuses();
    s.seed([summary("a", "idle")]);
    expect(s.view("a").status).toBe("idle");
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

  it("writes the target thread's mode on thread/permissionModeChanged", () => {
    const s = new ThreadStore();
    s.seed([summary("a"), summary("b")]);
    s.applyNotification({
      method: "thread/permissionModeChanged",
      params: { thread_id: "a", mode: "yolo" },
    });
    // 只动被点名的那个。
    expect(s.view("a").mode).toBe("yolo");
    expect(s.view("b").mode).toBeNull();
  });

  it("applies thread/permissionModeChanged even for a thread with no prior view", () => {
    const s = new ThreadStore();
    // 远程客户端未订阅该会话时也可能收到（列表层恒推）：写入即建视图，不崩。
    s.applyNotification({
      method: "thread/permissionModeChanged",
      params: { thread_id: "ghost", mode: "normal" },
    });
    expect(s.view("ghost").mode).toBe("normal");
  });
});

describe("threadStore reasoning effort", () => {
  it("records thread/reasoningEffortChanged and defaults to no override", () => {
    const s = new ThreadStore();
    expect(s.view("a").reasoningEffort).toBeNull();
    s.applyNotification({
      method: "thread/reasoningEffortChanged",
      params: { thread_id: "a", effort: "high" },
    });
    expect(s.view("a").reasoningEffort).toBe("high");
    // 只动被点名的那个。
    expect(s.view("b").reasoningEffort).toBeNull();
  });

  it("clears the override when effort is omitted", () => {
    const s = new ThreadStore();
    s.applyNotification({
      method: "thread/reasoningEffortChanged",
      params: { thread_id: "a", effort: "high" },
    });
    s.applyNotification({ method: "thread/reasoningEffortChanged", params: { thread_id: "a" } });
    expect(s.view("a").reasoningEffort).toBeNull();
  });

  it("defaults syncState to synced so the desktop path is unchanged", () => {
    const s = new ThreadStore();
    expect(s.view("t1").syncState).toBe("synced");
  });
});
