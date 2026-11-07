import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ConnectionError } from "./connectionError";
import type { AgentCancelPreviewResult } from "./protocol";
import {
  type TraceActionSnapshot,
  TraceActions,
} from "./subagentTraceActions";
import type { TraceConnection, TraceTarget } from "./subagentTraceSubscription";
import { deferred, ScriptedTraceClient } from "../test/subagentTraceFixtures";

const MESSAGE = "agent/message";
const PREVIEW = "agent/cancel/preview";
const CANCEL = "agent/cancel";

const target: TraceTarget = {
  slot: "local",
  serverBindingGeneration: 1,
  threadId: "thread-a",
  taskId: "task-a",
};

const otherTarget: TraceTarget = { ...target, taskId: "task-b" };

let client: ScriptedTraceClient;
let actions: TraceActions;
let connection: TraceConnection;

function makeConnection(
  overrides: Partial<TraceConnection> = {},
  scripted: ScriptedTraceClient = client,
): TraceConnection {
  return {
    slot: "local",
    serverBindingGeneration: 1,
    connectionGeneration: 1,
    client: scripted,
    ...overrides,
  };
}

/** A task row the controller would push down from the rail. */
function taskRow(state = "running", finished = false) {
  return {
    taskId: target.taskId,
    objective: null,
    state,
    lastStep: null,
    finished,
    parentTaskId: null,
  };
}

function read(overrides: Partial<TraceTarget> = {}): TraceActionSnapshot {
  return actions.read({ ...target, ...overrides });
}

beforeEach(() => {
  client = new ScriptedTraceClient();
  actions = new TraceActions();
  connection = makeConnection();
  actions.setVisible(target);
  actions.updateTask(target, taskRow());
});

afterEach(() => {
  actions.clear();
  vi.useRealTimers();
});

describe("message submission", () => {
  it("settles the submitted revision without clearing a newer draft", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "first");
    const sent = actions.send(target, connection);
    actions.setDraft(target, "second");
    pending.resolve({ queued: true });
    await sent;

    expect(read().draft).toBe("second");
    expect(read().notice).toBe("消息已入队");
    expect(read().messagePending).toBe(false);
  });

  it("never treats a missing connection as a successful message", async () => {
    actions.setDraft(target, "keep me");
    await expect(actions.send(target, null)).rejects.toBeInstanceOf(ConnectionError);
    expect(read().draft).toBe("keep me");
    expect(read().error).not.toBeNull();
  });

  it("rejects a connection from another binding and keeps the draft", async () => {
    actions.setDraft(target, "keep me");
    await expect(
      actions.send(target, makeConnection({ serverBindingGeneration: 2 })),
    ).rejects.toBeInstanceOf(ConnectionError);
    expect(read().draft).toBe("keep me");
  });

  it("rejects a connection from another slot even when the ids match", async () => {
    actions.setDraft(target, "keep me");
    await expect(
      actions.send(target, makeConnection({ slot: "remote" })),
    ).rejects.toBeInstanceOf(ConnectionError);
    expect(read().draft).toBe("keep me");
  });

  it("does not send whitespace and reports why", async () => {
    actions.setDraft(target, "   \n  ");
    await actions.send(target, connection);

    expect(client.methods).toEqual([]);
    expect(read().draft).toBe("   \n  ");
    expect(read().error).toBe("消息不能为空");
    expect(read().messagePending).toBe(false);
  });

  it("issues exactly one message for a double submit", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "hello");

    const first = actions.send(target, connection);
    const second = actions.send(target, connection);
    pending.resolve({ queued: true });
    await Promise.all([first, second]);

    expect(client.methods).toEqual([MESSAGE]);
    expect(read().notice).toBe("消息已入队");
  });

  it("clears the draft when the revision did not change", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "hello");
    const sent = actions.send(target, connection);
    pending.resolve({ queued: true });
    await sent;

    expect(read().draft).toBe("");
    expect(read().notice).toBe("消息已入队");
  });

  it("keeps a draft that was edited back to the submitted text", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "hello");
    const sent = actions.send(target, connection);
    // Same text, but the user rewrote it: the revision moved, so the submitted
    // text is no longer what sits in the box.
    actions.setDraft(target, "hello!");
    actions.setDraft(target, "hello");
    pending.resolve({ queued: true });
    await sent;

    expect(read().draft).toBe("hello");
    expect(read().notice).toBe("消息已入队");
  });

  it("keeps the draft when the message fails and reports the failure", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "hello");
    const sent = actions.send(target, connection);
    pending.reject(new Error("boom"));
    await expect(sent).rejects.toThrow("boom");

    expect(read().draft).toBe("hello");
    expect(read().error).toContain("boom");
    expect(read().messagePending).toBe(false);
  });

  it("holds the lock against a late reply after the task finished", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "hello");
    const sent = actions.send(target, connection);
    actions.updateTask(target, taskRow("completed", true));
    pending.resolve({ queued: true });
    await sent;

    // A finished task cannot accept the message, so the text must survive even
    // though the revision never changed.
    expect(read().draft).toBe("hello");
    expect(read().messagePending).toBe(false);
  });

  it("keeps the draft when the message lands after the wait turned uncertain", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "performance"] });
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "hello");
    const sent = actions.send(target, connection);
    vi.advanceTimersByTime(15_000);
    expect(read().notice).toBe("仍在等待服务端确认，结果尚未返回。");

    pending.resolve({ queued: true });
    await sent;

    // Once the UI said the outcome was unknown it must not silently clear what
    // the user may still believe is unsent.
    expect(read().draft).toBe("hello");
    expect(read().notice).toBe("消息已入队");
    expect(read().messagePending).toBe(false);
  });

  it("replaces the uncertainty hint with the real failure", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "performance"] });
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "hello");
    const sent = actions.send(target, connection);
    vi.advanceTimersByTime(15_000);
    pending.reject(new Error("boom"));
    await expect(sent).rejects.toThrow("boom");

    expect(read().notice).toBeNull();
    expect(read().error).toContain("boom");
    expect(read().draft).toBe("hello");
  });

  it("does not write A's reply into B's record", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "for a");
    actions.setDraft(otherTarget, "for b");
    const sent = actions.send(target, connection);
    actions.setVisible(otherTarget);
    pending.resolve({ queued: true });
    await sent;

    expect(actions.read(target).draft).toBe("");
    expect(actions.read(otherTarget).draft).toBe("for b");
  });

  it("unwinds the draft into the record that was sent even after leaving the task", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "hello");
    const sent = actions.send(target, connection);
    actions.setVisible(otherTarget);
    pending.resolve({ queued: true });
    await sent;

    expect(actions.read(target).draft).toBe("");
    expect(actions.read(target).notice).toBe("消息已入队");
  });

  it("unlocks on the disposed client's rejection without faking success", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "hello");
    const sent = actions.send(target, connection);
    expect(read().messagePending).toBe(true);
    // RpcClient.dispose() rejects every in-flight request this way.
    pending.reject(new ConnectionError("disconnected", "rpc client disposed"));
    await expect(sent).rejects.toBeInstanceOf(ConnectionError);

    expect(read().messagePending).toBe(false);
    expect(read().draft).toBe("hello");
  });
});

describe("cancel preview", () => {
  it("discards a preview that arrives after the task finishes", async () => {
    const pending = deferred<AgentCancelPreviewResult>();
    client.queue(PREVIEW, pending.promise);
    const preview = actions.preview(target, connection);
    actions.updateTask(target, taskRow("completed", true));
    pending.resolve({ confirmationToken: "old", taskIds: [target.taskId], expiresInSecs: 60 });
    await preview;

    expect(read().confirmation).toBeNull();
    expect(read().previewPending).toBe(false);
  });

  it("never opens a confirmation for a task that is already finished", async () => {
    // No epoch bump can happen between the request and its reply here, so only
    // the record's own terminal flag can reject the token.
    actions.updateTask(target, taskRow("completed", true));
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "tok",
      taskIds: [target.taskId],
      expiresInSecs: 60,
    }));
    await actions.preview(target, connection);

    expect(read().confirmation).toBeNull();
    expect(read().previewPending).toBe(false);
  });

  it("sends only the thread and task, without a recursive flag", async () => {
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "tok",
      taskIds: [target.taskId],
      expiresInSecs: 60,
    }));
    await actions.preview(target, connection);

    expect(client.calls).toEqual([
      { method: PREVIEW, params: { threadId: target.threadId, taskId: target.taskId } },
    ]);
  });

  it("exposes the token, the deduped ids and the deadline only", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "performance"] });
    const startedAt = performance.now();
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "tok",
      taskIds: [target.taskId, target.taskId, otherTarget.taskId],
      expiresInSecs: 60,
    }));
    await actions.preview(target, connection);

    const confirmation = read().confirmation;
    expect(confirmation).toEqual({
      token: "tok",
      taskIds: [target.taskId, otherTarget.taskId],
      deadline: startedAt + 60_000,
    });
    expect(Object.keys(confirmation!).sort()).toEqual(["deadline", "taskIds", "token"]);
  });

  it("measures the deadline from the request start, not the response", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "performance"] });
    const startedAt = performance.now();
    const pending = deferred<AgentCancelPreviewResult>();
    client.queue(PREVIEW, pending.promise);
    const preview = actions.preview(target, connection);
    vi.advanceTimersByTime(3_000);
    pending.resolve({ confirmationToken: "tok", taskIds: [target.taskId], expiresInSecs: 10 });
    await preview;

    // 3s of the 10s TTL is already spent, so the deadline stays anchored at the
    // moment the request was issued.
    expect(performance.now()).toBe(startedAt + 3_000);
    expect(read().confirmation?.deadline).toBe(startedAt + 10_000);
  });

  it("drops the token at once when the deadline has already passed", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "performance"] });
    const pending = deferred<AgentCancelPreviewResult>();
    client.queue(PREVIEW, pending.promise);
    const preview = actions.preview(target, connection);
    vi.advanceTimersByTime(2_000);
    pending.resolve({ confirmationToken: "tok", taskIds: [target.taskId], expiresInSecs: 1 });
    await preview;

    expect(read().confirmation).toBeNull();
  });

  it("keeps no confirmation for a zero or non-finite ttl", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "performance"] });
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "zero",
      taskIds: [target.taskId],
      expiresInSecs: 0,
    }));
    await actions.preview(target, connection);
    expect(read().confirmation).toBeNull();

    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "nan",
      taskIds: [target.taskId],
      expiresInSecs: Number.NaN,
    }));
    await actions.preview(target, connection);
    expect(read().confirmation).toBeNull();
  });

  it("clears the confirmation when its own timer expires", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "performance"] });
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "tok",
      taskIds: [target.taskId],
      expiresInSecs: 2,
    }));
    await actions.preview(target, connection);
    expect(read().confirmation?.token).toBe("tok");

    vi.advanceTimersByTime(2_000);
    expect(read().confirmation).toBeNull();
  });

  it("rejects without a connection and never opens a confirmation", async () => {
    await expect(actions.preview(target, null)).rejects.toBeInstanceOf(ConnectionError);
    expect(read().confirmation).toBeNull();
    expect(read().previewPending).toBe(false);
    expect(client.methods).toEqual([]);
  });

  it("issues one preview for a double click", async () => {
    const pending = deferred<AgentCancelPreviewResult>();
    client.queue(PREVIEW, pending.promise);
    const first = actions.preview(target, connection);
    const second = actions.preview(target, connection);
    pending.resolve({ confirmationToken: "tok", taskIds: [target.taskId], expiresInSecs: 60 });
    await Promise.all([first, second]);

    expect(client.methods).toEqual([PREVIEW]);
  });

  it("drops a preview that lands after the user left the tab", async () => {
    const pending = deferred<AgentCancelPreviewResult>();
    client.queue(PREVIEW, pending.promise);
    const preview = actions.preview(target, connection);
    actions.setVisible(null);
    pending.resolve({ confirmationToken: "tok", taskIds: [target.taskId], expiresInSecs: 60 });
    await preview;

    expect(read().confirmation).toBeNull();
    expect(read().previewPending).toBe(false);
  });

  it("drops a preview that lands after re-opening the same task", async () => {
    const pending = deferred<AgentCancelPreviewResult>();
    client.queue(PREVIEW, pending.promise);
    const preview = actions.preview(target, connection);
    actions.setVisible(null);
    actions.setVisible(target);
    pending.resolve({ confirmationToken: "stale", taskIds: [target.taskId], expiresInSecs: 60 });
    await preview;

    expect(read().confirmation).toBeNull();
  });

  it("settles the preview without reopening a confirmation for a dropped task", async () => {
    const pending = deferred<AgentCancelPreviewResult>();
    client.queue(PREVIEW, pending.promise);
    const preview = actions.preview(target, connection);
    actions.dropThread(target.slot, target.serverBindingGeneration, target.threadId);
    pending.resolve({ confirmationToken: "tok", taskIds: [target.taskId], expiresInSecs: 60 });
    await preview;

    // The record was deleted with its thread; a late reply must not create it.
    expect(actions.read(target).draft).toBe("");
    expect(actions.read(target).confirmation).toBeNull();
  });

  it("reports a preview failure on its own record", async () => {
    const pending = deferred<AgentCancelPreviewResult>();
    client.queue(PREVIEW, pending.promise);
    const preview = actions.preview(target, connection);
    pending.reject(new Error("nope"));
    await expect(preview).rejects.toThrow("nope");

    expect(read().error).toContain("nope");
    expect(read().confirmation).toBeNull();
    expect(read().previewPending).toBe(false);
  });
});

describe("cancel confirmation", () => {
  async function openConfirmation(
    ids: string[] = [target.taskId],
    ttl = 60,
  ): Promise<void> {
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "tok",
      taskIds: ids,
      expiresInSecs: ttl,
    }));
    await actions.preview(target, connection);
  }

  it("cancels once with the preview's token and publishes the notice", async () => {
    await openConfirmation();
    client.queue(CANCEL, Promise.resolve({ cancelled: true }));
    await actions.confirm(target, connection);

    expect(client.calls[client.calls.length - 1]).toEqual({
      method: CANCEL,
      params: {
        threadId: target.threadId,
        taskId: target.taskId,
        confirmationToken: "tok",
      },
    });
    expect(read().notice).toBe("取消请求已提交");
    expect(read().confirmation).toBeNull();
    expect(read().confirmPending).toBe(false);
  });

  it("refuses to confirm when the preview found no cancellable task", async () => {
    await openConfirmation([]);
    await actions.confirm(target, connection);

    expect(client.methods).toEqual([PREVIEW]);
    expect(read().confirmPending).toBe(false);
    expect(read().error).toContain("可取消");
  });

  it("refuses a connection that is not the one the token came from", async () => {
    await openConfirmation();
    const otherClient = new ScriptedTraceClient();
    const other = makeConnection({ connectionGeneration: 2 }, otherClient);
    await actions.confirm(target, other);

    expect(client.methods).toEqual([PREVIEW]);
    expect(otherClient.methods).toEqual([]);
    // The token belongs to the old connection and must not migrate.
    expect(read().confirmation).toBeNull();
    expect(read().error).toContain("重新预览");
  });

  it("refuses a token from another client object even with the same generation", async () => {
    await openConfirmation();
    const twinClient = new ScriptedTraceClient();
    const twin = makeConnection({}, twinClient);
    await actions.confirm(target, twin);

    expect(twinClient.methods).toEqual([]);
    expect(read().confirmation).toBeNull();
  });

  it("refuses an expired token and requires a new preview", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "performance"] });
    await openConfirmation([target.taskId], 5);
    vi.advanceTimersByTime(5_000);
    await actions.confirm(target, connection);

    expect(client.methods).toEqual([PREVIEW]);
    expect(read().confirmation).toBeNull();
    expect(read().error).toContain("重新预览");
  });

  it("clears the token when the cancel request fails", async () => {
    await openConfirmation();
    const pending = deferred<unknown>();
    client.queue(CANCEL, pending.promise);
    const confirmed = actions.confirm(target, connection);
    pending.reject(new Error("late"));
    await expect(confirmed).rejects.toThrow("late");

    expect(read().confirmation).toBeNull();
    expect(read().error).toContain("重新");
  });

  it("does not let dismiss undo a pending confirmation", async () => {
    await openConfirmation();
    const pending = deferred<unknown>();
    client.queue(CANCEL, pending.promise);
    const confirmed = actions.confirm(target, connection);
    actions.dismiss(target);
    expect(read().confirmation?.token).toBe("tok");
    expect(read().confirmPending).toBe(true);

    pending.resolve({ cancelled: true });
    await confirmed;
    expect(read().confirmation).toBeNull();
  });

  it("dismiss removes a settled confirmation without touching the draft", async () => {
    await openConfirmation();
    actions.setDraft(target, "typing");
    actions.dismiss(target);

    expect(read().confirmation).toBeNull();
    expect(read().draft).toBe("typing");
  });

  it("issues one cancel for a double confirm", async () => {
    await openConfirmation();
    const pending = deferred<unknown>();
    client.queue(CANCEL, pending.promise);
    const first = actions.confirm(target, connection);
    const second = actions.confirm(target, connection);
    pending.resolve({ cancelled: true });
    await Promise.all([first, second]);

    expect(client.methods).toEqual([PREVIEW, CANCEL]);
  });

  it("settles a late confirm without publishing into the new view", async () => {
    await openConfirmation();
    const pending = deferred<unknown>();
    client.queue(CANCEL, pending.promise);
    const confirmed = actions.confirm(target, connection);
    actions.setVisible(otherTarget);
    pending.resolve({ cancelled: true });
    await confirmed;

    expect(actions.read(otherTarget).notice).toBeNull();
    expect(actions.read(otherTarget).error).toBeNull();
    expect(actions.read(otherTarget).confirmation).toBeNull();
  });

  it("settles a late confirm failure without publishing into the new view", async () => {
    await openConfirmation();
    const pending = deferred<unknown>();
    client.queue(CANCEL, pending.promise);
    const confirmed = actions.confirm(target, connection);
    actions.setVisible(null);
    actions.setVisible(target);
    pending.reject(new Error("stale failure"));
    await expect(confirmed).rejects.toThrow("stale failure");

    expect(read().error).toBeNull();
    expect(read().confirmation).toBeNull();
    expect(read().confirmPending).toBe(false);
  });

  it("keeps a re-opened confirmation for the same task after a tab switch", async () => {
    await openConfirmation();
    actions.setVisible(null);
    // Leaving the tab invalidates the preview generation...
    expect(read().confirmation).toBeNull();

    actions.setVisible(target);
    actions.updateTask(target, taskRow());
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "fresh",
      taskIds: [target.taskId],
      expiresInSecs: 60,
    }));
    await actions.preview(target, connection);
    expect(read().confirmation?.token).toBe("fresh");
  });
});

describe("task and connection lifecycle", () => {
  it("invalidates the confirmation when the task turns terminal", async () => {
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "tok",
      taskIds: [target.taskId],
      expiresInSecs: 60,
    }));
    await actions.preview(target, connection);
    expect(read().confirmation?.token).toBe("tok");

    actions.updateTask(target, taskRow("failed", true));
    expect(read().confirmation).toBeNull();
  });

  it("keeps the confirmation while the task keeps running", async () => {
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "tok",
      taskIds: [target.taskId],
      expiresInSecs: 60,
    }));
    await actions.preview(target, connection);
    actions.updateTask(target, taskRow("running", false));

    expect(read().confirmation?.token).toBe("tok");
  });

  it("keeps the draft but drops the token when the connection dies", async () => {
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "tok",
      taskIds: [target.taskId],
      expiresInSecs: 60,
    }));
    await actions.preview(target, connection);
    actions.setDraft(target, "half typed");
    actions.disconnected(connection);

    expect(read().confirmation).toBeNull();
    expect(read().draft).toBe("half typed");
  });

  it("ignores a disconnect reported for another client", async () => {
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "tok",
      taskIds: [target.taskId],
      expiresInSecs: 60,
    }));
    await actions.preview(target, connection);
    actions.disconnected(makeConnection({}, new ScriptedTraceClient()));

    expect(read().confirmation?.token).toBe("tok");
  });

  it("still waits for the underlying settle after a disconnect", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "hello");
    const sent = actions.send(target, connection);
    actions.disconnected(connection);
    expect(read().messagePending).toBe(true);

    // The guard must not open early just because the connection is marked dead.
    await actions.send(target, connection);
    expect(client.methods).toEqual([MESSAGE]);

    pending.resolve({ queued: true });
    await sent;
    expect(read().messagePending).toBe(false);
  });

  it("keeps separate drafts for tasks that only differ by thread", async () => {
    const sameTaskOtherThread: TraceTarget = { ...target, threadId: "thread-b" };
    actions.setDraft(target, "a");
    actions.setDraft(sameTaskOtherThread, "b");

    expect(actions.read(target).draft).toBe("a");
    expect(actions.read(sameTaskOtherThread).draft).toBe("b");
  });

  it("keeps separate drafts for the same ids on another slot", async () => {
    const remote: TraceTarget = { ...target, slot: "remote" };
    actions.setDraft(target, "local draft");
    actions.setDraft(remote, "remote draft");

    expect(actions.read(target).draft).toBe("local draft");
    expect(actions.read(remote).draft).toBe("remote draft");
  });

  it("drops a thread's records and ignores a later reply", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "hello");
    const sent = actions.send(target, connection);
    actions.dropThread(target.slot, target.serverBindingGeneration, target.threadId);
    pending.resolve({ queued: true });
    await sent;

    expect(actions.read(target).draft).toBe("");
    expect(actions.read(target).notice).toBeNull();
    expect(actions.read(target).messagePending).toBe(false);
  });

  it("drops a slot's records across threads", async () => {
    const sameTaskOtherThread: TraceTarget = { ...target, threadId: "thread-b" };
    actions.setDraft(target, "a");
    actions.setDraft(sameTaskOtherThread, "b");
    actions.dropSlot(target.slot);

    expect(actions.read(target).draft).toBe("");
    expect(actions.read(sameTaskOtherThread).draft).toBe("");
  });

  it("keeps a rebinding's failure out of the new record", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "old binding");
    const sent = actions.send(target, connection);

    // A rebind is a drop, not a rename: the old record is gone.
    actions.dropSlot(target.slot);
    pending.reject(new Error("old failure"));
    await expect(sent).rejects.toThrow("old failure");

    expect(actions.read(target).draft).toBe("");
    expect(actions.read(target).error).toBeNull();
  });

  it("clears every record and timer", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "performance"] });
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "tok",
      taskIds: [target.taskId],
      expiresInSecs: 60,
    }));
    await actions.preview(target, connection);
    actions.setDraft(target, "draft");

    actions.clear();

    expect(actions.read(target).draft).toBe("");
    expect(actions.read(target).confirmation).toBeNull();
    expect(vi.getTimerCount()).toBe(0);
  });
});

describe("lifecycle generation", () => {
  it("does not let a reply from before a drop satisfy a new record with the same key", async () => {
    const pending = deferred<{ queued: boolean }>();
    client.queue(MESSAGE, pending.promise);
    actions.setDraft(target, "old");
    const sent = actions.send(target, connection);
    actions.dropThread(target.slot, target.serverBindingGeneration, target.threadId);

    // A new visit to the same key creates a fresh record with a new lifetime.
    actions.setVisible(target);
    actions.updateTask(target, taskRow());
    actions.setDraft(target, "new");
    pending.resolve({ queued: true });
    await sent;

    expect(read().draft).toBe("new");
    expect(read().notice).toBeNull();
    expect(read().messagePending).toBe(false);
  });

  it("notifies subscribers only when the record really changes", () => {
    const listener = vi.fn();
    actions.subscribe(listener);
    actions.setDraft(target, "x");
    const calls = listener.mock.calls.length;
    actions.setDraft(target, "x");

    expect(listener).toHaveBeenCalledTimes(calls);
  });

  it("returns a stable snapshot for an unknown record", () => {
    const fresh = new TraceActions();
    const first = fresh.read(target);
    expect(fresh.read(target)).toBe(first);
    expect(first).toMatchObject({
      draft: "",
      draftRevision: 0,
      messagePending: false,
      previewPending: false,
      confirmPending: false,
      confirmation: null,
      error: null,
      notice: null,
    });
    fresh.clear();
  });
});
