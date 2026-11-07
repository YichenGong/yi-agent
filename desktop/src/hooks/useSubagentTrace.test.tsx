/** @vitest-environment jsdom */
import { StrictMode, type ReactNode } from "react";
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ConnectionError } from "../lib/connectionError";
import type { AgentCancelPreviewResult } from "../lib/protocol";
import type { SubagentRow } from "../lib/subagents";
import {
  createSubagentTraceController,
  useSubagentTrace,
} from "./useSubagentTrace";
import type { TraceConnection, TraceTarget } from "../lib/subagentTraceSubscription";
import { deferred, ScriptedTraceClient, traceRow } from "../test/subagentTraceFixtures";

const target: TraceTarget = {
  slot: "local",
  serverBindingGeneration: 1,
  threadId: "thread-a",
  taskId: "task-a",
};

const otherTarget: TraceTarget = { ...target, taskId: "task-b" };

const WATCH = "agent/trace/watch";
const UNWATCH = "agent/trace/unwatch";
const MESSAGE = "agent/message";
const PREVIEW = "agent/cancel/preview";
const CANCEL = "agent/cancel";

function row(state = "running", finished = false, taskId = target.taskId): SubagentRow {
  return {
    taskId,
    objective: null,
    state,
    lastStep: null,
    finished,
    parentTaskId: null,
  };
}

const flush = async () => {
  for (let i = 0; i < 6; i += 1) await Promise.resolve();
};

function snapshot(rows: number[] = []) {
  return {
    rows: rows.map((eventId) => traceRow(eventId, "assistant_text", { text: `e${eventId}` }, target.taskId)),
    highWaterId: rows.length === 0 ? 0 : rows[rows.length - 1],
  };
}

let client: ScriptedTraceClient;
let connection: TraceConnection;

function connect(overrides: Partial<TraceConnection> = {}, scripted = client): TraceConnection {
  return {
    slot: "local",
    serverBindingGeneration: 1,
    connectionGeneration: 1,
    client: scripted,
    ...overrides,
  };
}

beforeEach(() => {
  client = new ScriptedTraceClient();
  connection = connect();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("registration", () => {
  it("registers a connection as not ready until the handshake says otherwise", () => {
    const controller = createSubagentTraceController();
    controller.setConnection(connection);

    expect(controller.connectionFor(client)).toBe(connection);
    controller.open(target);
    // Registration is not readiness: no watch may leave before the handshake.
    expect(client.methods).toEqual([]);
    expect(controller.getSnapshot().subscription.phase).toBe("error");

    controller.dispose();
  });

  it("opens a watch once the connection is ready", async () => {
    client.queue(WATCH, Promise.resolve(snapshot([1, 2])));
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    await flush();

    expect(client.methods).toEqual([WATCH]);
    expect(controller.getSnapshot().subscription.phase).toBe("ready");
    expect(controller.getSnapshot().selected).toEqual(target);

    controller.dispose();
  });

  it("never guesses a source for an unknown client", () => {
    const controller = createSubagentTraceController();
    controller.setConnection(connection);

    expect(controller.connectionFor(new ScriptedTraceClient())).toBeNull();
    // A slot argument does not let an unknown client borrow the current source.
    expect(controller.connectionFor(new ScriptedTraceClient(), "local")).toBeNull();
    // A known client asked for another slot is not the source either.
    expect(controller.connectionFor(client, "remote")).toBeNull();
    expect(controller.connectionFor(client, "local")).toBe(connection);

    controller.dispose();
  });

  it("ignores readiness reported for a connection that is no longer current", async () => {
    const nextClient = new ScriptedTraceClient();
    const next = connect({ connectionGeneration: 2 }, nextClient);
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnection(connection);
    controller.setConnection(next);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    await flush();

    expect(client.methods).toEqual([]);
    expect(nextClient.methods).toEqual([]);
    expect(controller.connectionFor(nextClient)).toBe(next);

    controller.dispose();
  });
});

describe("selection and drafts", () => {
  it("retains draft while closing the panel but not its selection", () => {
    const { result } = renderHook(() => useSubagentTrace());
    act(() => {
      result.current.controller.setConnection(connection);
      result.current.controller.setConnectionReady(connection, true);
      result.current.controller.open(target);
      result.current.controller.setDraft(target, "resume later");
      result.current.controller.close();
    });
    expect(result.current.view.selected).toBeNull();
    act(() => result.current.controller.open(target));
    expect(result.current.view.actions?.draft).toBe("resume later");
  });

  it("keeps history through close so a reopen is not a blank panel", async () => {
    client.queue(WATCH, Promise.resolve(snapshot([1, 2])));
    client.queue(UNWATCH, Promise.resolve({ stopped: true }));
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    await flush();
    const loaded = controller.getSnapshot().subscription.rows;
    expect(loaded).toHaveLength(2);

    controller.close();
    // The visible panel is idle, but nothing of the user's place was destroyed.
    expect(controller.getSnapshot().subscription.phase).toBe("idle");
    expect(controller.getSnapshot().selected).toBeNull();

    controller.dispose();
  });

  it("writes a draft to the target it was rendered with, not the current selection", async () => {
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);

    // A stale callback from A's render must not land in B.
    controller.setDraft(target, "for a");
    controller.open(otherTarget);
    controller.setDraft(target, "still for a");

    expect(controller.getSnapshot().actions?.draft).toBe("");
    controller.open(target);
    expect(controller.getSnapshot().actions?.draft).toBe("still for a");

    controller.dispose();
  });

  it("accepts a live row only from the source it was opened with", async () => {
    client.queue(WATCH, Promise.resolve(snapshot([1])));
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    await flush();

    controller.accept(connection, {
      threadId: target.threadId,
      taskId: target.taskId,
      row: traceRow(2, "assistant_text", { text: "live" }, target.taskId),
    });
    expect(controller.getSnapshot().subscription.rows).toHaveLength(2);

    // Same ids, another client object: a different source.
    controller.accept(connect({}, new ScriptedTraceClient()), {
      threadId: target.threadId,
      taskId: target.taskId,
      row: traceRow(3, "assistant_text", { text: "impostor" }, target.taskId),
    });
    expect(controller.getSnapshot().subscription.rows).toHaveLength(2);

    controller.dispose();
  });
});

describe("actions gating", () => {
  it("rejects an action before the handshake completes", async () => {
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.open(target);
    controller.setDraft(target, "hello");

    await expect(controller.send(target)).rejects.toBeInstanceOf(ConnectionError);
    expect(client.methods).toEqual([]);

    controller.dispose();
  });

  it("rejects an action for a target that is not the visible one", async () => {
    client.queue(WATCH, Promise.resolve(snapshot([1])));
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    await flush();
    controller.setDraft(otherTarget, "for b");

    await expect(controller.send(otherTarget)).rejects.toBeInstanceOf(ConnectionError);
    // Only the visible task's own watch left; B's draft must never be sent.
    expect(client.methods).toEqual([WATCH]);

    controller.dispose();
  });

  it("rejects every action after the connection reports it is dead", async () => {
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    controller.setDraft(target, "hello");
    controller.disconnected(connection);

    await expect(controller.send(target)).rejects.toBeInstanceOf(ConnectionError);
    await expect(controller.preview(target)).rejects.toBeInstanceOf(ConnectionError);
    await expect(controller.confirm(target)).rejects.toBeInstanceOf(ConnectionError);
    expect(client.methods).toEqual([]);

    controller.dispose();
  });

  it("sends a message through the registered client", async () => {
    client.queue(WATCH, Promise.resolve(snapshot([])));
    client.queue(MESSAGE, Promise.resolve({ queued: true }));
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    await flush();
    controller.setDraft(target, "hello");
    await controller.send(target);

    expect(client.calls).toContainEqual({
      method: MESSAGE,
      params: { threadId: target.threadId, taskId: target.taskId, message: "hello" },
    });
    expect(controller.getSnapshot().actions?.draft).toBe("");

    controller.dispose();
  });

  it("surfaces the action failure on the record instead of swallowing it", async () => {
    client.queue(WATCH, Promise.resolve(snapshot([1])));
    client.queue(MESSAGE, Promise.reject(new Error("server said no")));
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    await flush();
    controller.setDraft(target, "hello");

    await expect(controller.send(target)).rejects.toThrow("server said no");

    expect(controller.getSnapshot().actions?.error).toContain("server said no");
    expect(controller.getSnapshot().actions?.draft).toBe("hello");

    controller.dispose();
  });
});

describe("tab and terminal transitions", () => {
  it("keeps the watch but invalidates the confirmation when the tab hides", async () => {
    client.queue(WATCH, Promise.resolve(snapshot([1])));
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "tok",
      taskIds: [target.taskId],
      expiresInSecs: 60,
    }));
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    await flush();
    await controller.preview(target);
    expect(controller.getSnapshot().actions?.confirmation?.token).toBe("tok");

    controller.tabChanged(false);

    expect(controller.getSnapshot().actions?.confirmation).toBeNull();
    // The owner and its rows stay: it is a hidden tab, not a different task.
    expect(controller.getSnapshot().subscription.phase).toBe("ready");
    expect(client.methods).not.toContain(UNWATCH);

    controller.tabChanged(true);
    controller.dispose();
  });

  it("invalidates the confirmation when the task turns terminal", async () => {
    client.queue(WATCH, Promise.resolve(snapshot([1])));
    client.queue(PREVIEW, Promise.resolve({
      confirmationToken: "tok",
      taskIds: [target.taskId],
      expiresInSecs: 60,
    }));
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    await flush();
    controller.updateTask(target, row());
    await controller.preview(target);

    controller.updateTask(target, row("completed", true));

    expect(controller.getSnapshot().actions?.confirmation).toBeNull();
    controller.dispose();
  });

  it("drops a thread's records without touching another thread", async () => {
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    controller.setDraft(target, "a");
    controller.open(otherTarget);
    controller.setDraft(otherTarget, "b");

    controller.dropThread(target.slot, target.serverBindingGeneration, target.threadId);

    controller.open(target);
    expect(controller.getSnapshot().actions?.draft).toBe("");
    controller.dispose();
  });

  it("rebinding clears both modules and forgets the connections", async () => {
    client.queue(WATCH, Promise.resolve(snapshot([1])));
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    await flush();
    controller.setDraft(target, "draft");

    controller.resetBindings();

    expect(controller.getSnapshot().selected).toBeNull();
    expect(controller.getSnapshot().actions).toBeNull();
    expect(controller.connectionFor(client)).toBeNull();
    expect(controller.getSnapshot().subscription.phase).toBe("idle");

    controller.dispose();
  });
});

describe("view identity", () => {
  it("returns the same view object while nothing a reader can see changes", () => {
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    const first = controller.getSnapshot();
    // A second read must not allocate: useSyncExternalStore compares identity.
    expect(controller.getSnapshot()).toBe(first);

    controller.dispose();
  });

  it("notifies only on a visible change", () => {
    const controller = createSubagentTraceController();
    const listener = vi.fn();
    controller.subscribe(listener);
    controller.setConnection(connection);
    const calls = listener.mock.calls.length;
    controller.setConnection(connect({ connectionGeneration: 1 }));

    // A fresh object with the same identity is not a change to the view.
    expect(listener).toHaveBeenCalledTimes(calls);
    controller.dispose();
  });
});

describe("StrictMode lifecycle", () => {
  const wrapper = ({ children }: { children: ReactNode }) => <StrictMode>{children}</StrictMode>;

  it("issues one watch under StrictMode's double effect", async () => {
    client.queue(WATCH, Promise.resolve(snapshot([1])));
    const { result } = renderHook(() => useSubagentTrace(), { wrapper });
    act(() => {
      result.current.controller.setConnection(connection);
      result.current.controller.setConnectionReady(connection, true);
      result.current.controller.open(target);
    });
    await act(async () => {
      await flush();
    });

    expect(client.methods.filter((method) => method === WATCH)).toHaveLength(1);
  });

  it("does not publish a late ack after the hook is unmounted", async () => {
    const pending = deferred<ReturnType<typeof snapshot>>();
    client.queue(WATCH, pending.promise);
    const listener = vi.fn();
    const { result, unmount } = renderHook(() => useSubagentTrace(), { wrapper });
    act(() => {
      result.current.controller.setConnection(connection);
      result.current.controller.setConnectionReady(connection, true);
      result.current.controller.subscribe(listener);
      result.current.controller.open(target);
    });

    // `result.current` is the last render, which unmount already discarded; the
    // controller is the object that owns the state and must be the one asked.
    const controller = result.current.controller;
    unmount();
    await act(async () => {
      await flush();
    });
    const afterUnmount = listener.mock.calls.length;

    pending.resolve(snapshot([1]));
    await act(async () => {
      await flush();
    });

    expect(listener.mock.calls.length).toBe(afterUnmount);
    expect(controller.getSnapshot().subscription.phase).toBe("idle");
    expect(controller.getSnapshot().subscription.rows).toEqual([]);
  });

  it("clears the controller's timers on a real unmount", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    client.queue(WATCH, deferred().promise);
    const { result, unmount } = renderHook(() => useSubagentTrace(), { wrapper });
    act(() => {
      result.current.controller.setConnection(connection);
      result.current.controller.setConnectionReady(connection, true);
      result.current.controller.open(target);
    });
    expect(vi.getTimerCount()).toBeGreaterThan(0);

    unmount();
    await act(async () => {
      await flush();
    });

    expect(vi.getTimerCount()).toBe(0);
  });
});

describe("cancel flow end to end", () => {
  it("previews and confirms through the registered connection", async () => {
    client.queue(WATCH, Promise.resolve(snapshot([1])));
    client.queue(PREVIEW, Promise.resolve<AgentCancelPreviewResult>({
      confirmationToken: "tok",
      taskIds: [target.taskId],
      expiresInSecs: 60,
    }));
    client.queue(CANCEL, Promise.resolve({ cancelled: true }));
    const controller = createSubagentTraceController();
    controller.setConnection(connection);
    controller.setConnectionReady(connection, true);
    controller.open(target);
    await flush();

    await controller.preview(target);
    expect(controller.getSnapshot().actions?.confirmation?.token).toBe("tok");

    await controller.confirm(target);
    expect(controller.getSnapshot().actions?.notice).toBe("取消请求已提交");
    expect(controller.getSnapshot().actions?.confirmation).toBeNull();

    controller.dispose();
  });
});
