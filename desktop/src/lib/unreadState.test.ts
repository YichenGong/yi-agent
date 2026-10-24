/**
 * 跨端未读状态同步（设计见
 * docs/superpowers/specs/2026-10-25-unread-read-state-sync-design.md）。
 *
 * 未读不再是一个「收到 turn/completed 就置真」的本地布尔位，而是由两个**服务端
 * 权威水位**派生：
 *
 *     unread = last_completed_at > last_read_at
 *
 * 这样「任一端读过，所有端都不再亮」才成立；本文件钉住派生的每一处入口。
 */
import { describe, it, expect } from "vitest";
import { ThreadStore } from "./threadStore";
import type { Notification, ThreadSummary } from "./protocol";

const summary = (
  id: string,
  watermarks?: { read?: number; completed?: number },
): ThreadSummary => ({
  thread_id: id,
  cwd: "/w",
  model: "m",
  created_at: 0,
  updated_at: 0,
  title: null,
  last_read_at: watermarks?.read ?? 0,
  last_completed_at: watermarks?.completed ?? 0,
});

const done = (
  id: string,
  completedAt: number,
  status: "completed" | "failed" | "interrupted" = "completed",
): Notification => ({
  method: "turn/completed",
  params: { thread_id: id, turn_id: "u1", status, completed_at: completedAt },
});

const readState = (id: string, read: number, completed: number): Notification => ({
  method: "thread/readStateChanged",
  params: { thread_id: id, last_read_at: read, last_completed_at: completed },
});

describe("ThreadStore read watermarks", () => {
  it("derives unread from the watermarks seeded by thread/listAll", () => {
    const s = new ThreadStore();
    // 一个在**本站不在线**时跑完的会话：完成时间晚于已读水位。
    // 这是重连后唯一能补回蓝点的来源——推送当时没人在听。
    s.seed([summary("a", { read: 100, completed: 200 })]);
    expect(s.view("a").unread).toBe(true);

    // 已读追平完成时间：不未读。
    s.seed([summary("b", { read: 200, completed: 200 })]);
    expect(s.view("b").unread).toBe(false);

    // 旧服务端/meta 缺字段：0/0，不能凭空冒出蓝点。
    s.seed([summary("c")]);
    expect(s.view("c").unread).toBe(false);
  });

  it("clears the unread dot when another client marks the thread read", () => {
    const s = new ThreadStore();
    s.seed([summary("a", { read: 100, completed: 200 })]);
    expect(s.view("a").unread).toBe(true);

    // 另一端（手机）点开了它：服务端广播新水位，本端必须就地清点。
    s.applyNotification(readState("a", 200, 200));
    expect(s.view("a").unread).toBe(false);
  });

  it("keeps the dot when a completion lands after another client's read", () => {
    const s = new ThreadStore();
    s.seed([summary("a", { read: 200, completed: 200 })]);
    expect(s.view("a").unread).toBe(false);

    // 另一端读过之后，本会话又跑完一轮：这是**新**的未读，必须再亮。
    s.applyNotification(readState("a", 200, 300));
    expect(s.view("a").unread).toBe(true);
  });

  it("does not let a stale listing roll back a newer read state", () => {
    const s = new ThreadStore();
    s.applyNotification(readState("a", 500, 400));
    expect(s.view("a").unread).toBe(false);

    // 一次**旧的** listAll 快照回来（连接建立时的点读，水位更小）：
    // 按 max 合入，绝不把已读打回未读。
    s.seed([summary("a", { read: 100, completed: 400 })]);
    expect(s.view("a").unread).toBe(false);
  });

  it("marks a background thread unread on turn/completed without waiting for a listing", () => {
    const s = new ThreadStore();
    s.select("a");
    s.applyNotification(done("b", 900));
    expect(s.view("b").unread).toBe(true);
  });

  it("does not mark the thread the user is looking at", () => {
    const s = new ThreadStore();
    s.select("a");
    s.applyNotification(done("a", 900));
    expect(s.view("a").unread).toBe(false);
  });

  it("clears unread immediately when the thread is selected", () => {
    const s = new ThreadStore();
    s.applyNotification(done("b", 900));
    expect(s.view("b").unread).toBe(true);
    s.select("b");
    expect(s.view("b").unread).toBe(false);
  });

  it("falls back to the local clock when an old server omits completed_at", () => {
    const s = new ThreadStore();
    s.select("a");
    // 旧服务端：没有 completed_at 字段。
    s.applyNotification({
      method: "turn/completed",
      params: { thread_id: "b", turn_id: "u1", status: "completed" },
    } as unknown as Notification);
    expect(s.view("b").unread).toBe(true);
  });
});
