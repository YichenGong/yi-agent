import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AgentTraceSnapshotResult } from "./protocol";
import { TraceSubscription, TRACE_WATCH_TIMEOUT_MS, traceTargetKey } from "./subagentTraceSubscription";
import type { TraceConnection, TraceTarget } from "./subagentTraceSubscription";
import { ScriptedTraceClient, deferred, traceRow } from "../test/subagentTraceFixtures";

const WATCH = "agent/trace/watch";
const UNWATCH = "agent/trace/unwatch";

/**
 * Let every already-reachable microtask run.
 *
 * The subscription's guarantee is *ordering*, and each queued operation costs a
 * microtask hop, so tests need "everything that can run without a timer has
 * run". A microtask flush (unlike `waitFor`) also works under fake timers,
 * where advancing time would fire the 15s hint unexpectedly. One operation costs
 * several hops (chain link, settlement, bookkeeping), so the bound is generous
 * and only ever reached when something is genuinely still pending.
 */
async function flush(rounds = 80): Promise<void> {
  for (let index = 0; index < rounds; index += 1) await Promise.resolve();
}

describe("TraceSubscription", () => {
  const subscriptions: TraceSubscription[] = [];

  function makeSubscription(): TraceSubscription {
    const sub = new TraceSubscription();
    subscriptions.push(sub);
    return sub;
  }

  let client: ScriptedTraceClient;
  let target: TraceTarget;
  let connection: TraceConnection;
  let sub: TraceSubscription;

  beforeEach(() => {
    client = new ScriptedTraceClient();
    target = {
      slot: "local",
      serverBindingGeneration: 1,
      threadId: "thread-a",
      taskId: "task-a",
    };
    connection = {
      slot: target.slot,
      serverBindingGeneration: target.serverBindingGeneration,
      connectionGeneration: 1,
      client,
    };
    sub = makeSubscription();
  });

  afterEach(() => {
    // Every case must leave no pending timer, listener or queued controller
    // behind; dispose first, then restore real timers.
    for (const subscription of subscriptions.splice(0)) subscription.dispose();
    vi.useRealTimers();
  });

  function secondConnection(overrides: Partial<TraceConnection> = {}): TraceConnection {
    return { ...connection, connectionGeneration: 2, ...overrides };
  }

  /** Queue one successful watch and open, leaving it in flight. */
  async function openPending(conn: TraceConnection = connection): Promise<ReturnType<typeof deferred<AgentTraceSnapshotResult>>> {
    const pending = deferred<AgentTraceSnapshotResult>();
    (conn.client as ScriptedTraceClient).queue(WATCH, pending.promise);
    sub.open(target, conn);
    await flush();
    return pending;
  }

  describe("identity", () => {
    it("keys the target by slot, binding, thread and task only", () => {
      expect(traceTargetKey(target)).toBe('["local",1,"thread-a","task-a"]');
      // The key exists so ordinary refreshes do not remount: request and
      // connection generations are deliberately absent.
      expect(traceTargetKey({ ...target, taskId: "task-b" })).not.toBe(traceTargetKey(target));
      expect(traceTargetKey({ ...target, threadId: "thread-b" })).not.toBe(traceTargetKey(target));
      expect(traceTargetKey({ ...target, serverBindingGeneration: 2 })).not.toBe(
        traceTargetKey(target),
      );
    });

    it("keeps subscribe and getSnapshot identities stable, and the snapshot too", () => {
      const subscribe = sub.subscribe;
      const getSnapshot = sub.getSnapshot;
      expect(sub.subscribe).toBe(subscribe);
      expect(sub.getSnapshot).toBe(getSnapshot);

      const snapshot = sub.getSnapshot();
      expect(sub.getSnapshot()).toBe(snapshot);
      // Reading is not a state change: no snapshot is built per call.
      expect(sub.getSnapshot()).toBe(snapshot);
    });

    it("notifies subscribers on change and stops after unsubscribe", async () => {
      const listener = vi.fn();
      const unsubscribe = sub.subscribe(listener);
      const pending = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, pending.promise);

      sub.open(target, connection);
      expect(listener).toHaveBeenCalledTimes(1);
      expect(sub.getSnapshot().phase).toBe("loading");

      pending.resolve({ rows: [], highWaterId: 0 });
      await flush();
      expect(listener).toHaveBeenCalledTimes(2);
      expect(sub.getSnapshot().phase).toBe("ready");

      // A duplicate live event changes nothing, so it must not wake React.
      const before = listener.mock.calls.length;
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(5, "state_note", { note: "x" }) });
      expect(listener).toHaveBeenCalledTimes(before + 1);

      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(5, "state_note", { note: "x" }) });
      expect(listener).toHaveBeenCalledTimes(before + 1);

      unsubscribe();
      sub.close();
      expect(listener).toHaveBeenCalledTimes(before + 1);
    });
  });

  describe("watch and snapshot", () => {
    it("loads with a single watch carrying only the thread and task", async () => {
      const pending = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, pending.promise);

      sub.open(target, connection);
      expect(sub.getSnapshot()).toMatchObject({ phase: "loading", rows: [], error: null, disconnected: false });
      expect(sub.getSnapshot().owner).toMatchObject({
        slot: "local",
        serverBindingGeneration: 1,
        threadId: "thread-a",
        taskId: "task-a",
        connectionGeneration: 1,
      });

      await flush();
      expect(client.calls).toEqual([
        { method: WATCH, params: { threadId: "thread-a", taskId: "task-a" } },
      ]);

      pending.resolve({ rows: [traceRow(1, "assistant_text", { text: "hello" })], highWaterId: 1 });
      await flush();
      expect(sub.getSnapshot()).toMatchObject({ phase: "ready", error: null, disconnected: false });
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([1]);
    });

    it("never reads the trace: watch is the only baseline request", async () => {
      const pending = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, pending.promise);
      sub.open(target, connection);
      await flush();
      pending.resolve({ rows: [], highWaterId: 0 });
      await flush();

      sub.close();
      await flush();

      expect(client.calls.filter((call) => call.method === "agent/trace/read")).toEqual([]);
      // watch is the baseline; leaving the task cleans up the one watch it made.
      expect(client.methods).toEqual([WATCH, UNWATCH]);
    });

    it("writes the owner synchronously so a frame in the same tick is accepted", async () => {
      const pending = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, pending.promise);

      sub.open(target, connection);
      // No microtask has run yet. Without a synchronous owner a frame arriving
      // in this tick would be attributed to whatever was open before (or to
      // nothing), and the first rows of the task would be silently lost.
      expect(sub.getSnapshot().owner?.taskId).toBe("task-a");
      expect(sub.getSnapshot().phase).toBe("loading");

      const early = traceRow(7, "assistant_text", { text: "early" });
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: early });
      expect(sub.getSnapshot().rows).toEqual([]);

      pending.resolve({ rows: [traceRow(1, "assistant_text", { text: "base" })], highWaterId: 1 });
      await flush();
      // It really was buffered under this owner, above the watermark.
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([1, 7]);
    });

    it("fails without faking success when there is no connection", async () => {
      sub.open(target, null);

      const snapshot = sub.getSnapshot();
      expect(snapshot.phase).toBe("error");
      expect(snapshot.error).toBe("与电脑的连接已断开，正在自动重连，无需操作。");
      expect(snapshot.owner).toBeNull();
      await flush();
      expect(client.calls).toEqual([]);
    });

    it("publishes the shell error text and clears the hint on a failed watch", async () => {
      vi.useFakeTimers();
      const pending = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, pending.promise);
      sub.open(target, connection);
      await flush();
      vi.advanceTimersByTime(TRACE_WATCH_TIMEOUT_MS);
      expect(sub.getSnapshot().waitingForPriorRequest).toBe(true);

      pending.reject(new Error("boom"));
      await flush();

      expect(sub.getSnapshot()).toMatchObject({
        phase: "error",
        error: "boom",
        rows: [],
        waitingForPriorRequest: false,
      });
    });
  });

  describe("buffering", () => {
    it("buffers pre-ack rows and only merges those above the snapshot watermark", async () => {
      const pending = await openPending();
      const buffered = traceRow(9, "assistant_text", { text: "new" });
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: buffered });
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(4, "assistant_text", { text: "old" }) });
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: buffered });

      // Not visible before the ack: the authoritative snapshot decides.
      expect(sub.getSnapshot()).toMatchObject({ phase: "loading", rows: [] });

      pending.resolve({ rows: [traceRow(1, "assistant_text", { text: "a" })], highWaterId: 5 });
      await flush();

      expect(sub.getSnapshot().phase).toBe("ready");
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([1, 9]);
      expect(sub.getSnapshot().rows[1]).toEqual(buffered);
    });

    it("merges live rows in order, ignores duplicates and other tasks", async () => {
      const pending = await openPending();
      pending.resolve({ rows: [traceRow(1, "assistant_text", { text: "a" })], highWaterId: 1 });
      await flush();

      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(7, "assistant_text", { text: "c" }) });
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(3, "assistant_text", { text: "b" }) });
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(7, "assistant_text", { text: "c" }) });
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([1, 3, 7]);

      // A row for another task, even under the same thread, is not ours.
      sub.accept(connection, { threadId: "thread-a", taskId: "task-b", row: traceRow(8, "assistant_text", { text: "other" }, "task-b") });
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(8, "assistant_text", { text: "other" }, "task-b") });
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([1, 3, 7]);
    });

    it("drops pre-ack buffered rows when the intent is reopened", async () => {
      const first = await openPending();
      const stale = traceRow(9, "assistant_text", { text: "stale" });
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: stale });
      expect(sub.getSnapshot().rows).toEqual([]);

      client.queue(UNWATCH, Promise.resolve({ stopped: true }));
      const second = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, second.promise);
      sub.close();
      // Re-open the same target: a fresh intent with an empty buffer.
      sub.open(target, connection);

      first.resolve({ rows: [], highWaterId: 0 });
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH, WATCH]);

      second.resolve({ rows: [], highWaterId: 0 });
      await flush();
      // The row buffered under the abandoned intent must not reseed the new view.
      expect(sub.getSnapshot()).toMatchObject({ phase: "ready", rows: [] });
    });
  });

  describe("serialization", () => {
    it("does not let stale cleanup unwatch a reopened task", async () => {
      // Verbatim from the task brief: close then reopen must not have the old
      // cleanup kill the new watch.
      const first = deferred<AgentTraceSnapshotResult>();
      const stop = deferred<{ stopped: boolean }>();
      const second = deferred<AgentTraceSnapshotResult>();
      client.queue("agent/trace/watch", first.promise);
      client.queue("agent/trace/unwatch", stop.promise);
      client.queue("agent/trace/watch", second.promise);
      sub.open(target, connection);
      await Promise.resolve();
      sub.close();
      sub.open(target, connection);
      first.resolve({ rows: [], highWaterId: 0 });
      await vi.waitFor(() => expect(client.calls.map(c => c.method)).toEqual([
        "agent/trace/watch", "agent/trace/unwatch",
      ]));
      stop.resolve({ stopped: true });
      await vi.waitFor(() => expect(client.calls).toHaveLength(3));
      second.resolve({ rows: [traceRow(5, "assistant_text", { text: "current" })], highWaterId: 5 });
      await vi.waitFor(() => expect(sub.getSnapshot().phase).toBe("ready"));
      expect(sub.getSnapshot().rows.map(r => r.eventId)).toEqual([5]);
    });

    it("skips a queued watch for an intent that was replaced before it ran", async () => {
      sub.open(target, connection);

      const second = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, second.promise);
      // Same thread, so B replaces A with no unwatch in between.
      sub.open({ ...target, taskId: "task-b" }, connection);

      // Only B's response is queued, so A sending a watch would consume it and
      // be caught by the params assertion. A lost the race before its operation
      // ran, so it must issue nothing.
      await flush();
      expect(client.calls).toEqual([
        { method: WATCH, params: { threadId: "thread-a", taskId: "task-b" } },
      ]);

      second.resolve({ rows: [traceRow(2, "assistant_text", { text: "b" }, "task-b")], highWaterId: 2 });
      await flush();
      expect(sub.getSnapshot().phase).toBe("ready");
      expect(sub.getSnapshot().owner?.taskId).toBe("task-b");
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([2]);
    });

    it("awaits an already-sent stale watch and its unwatch before the next watch", async () => {
      const first = await openPending();
      expect(client.methods).toEqual([WATCH]);

      const stop = deferred<{ stopped: boolean }>();
      client.queue(UNWATCH, stop.promise);
      const second = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, second.promise);
      sub.open({ ...target, taskId: "task-b" }, connection);

      // A is in flight, so B must wait — and so must B's request.
      await flush();
      expect(client.methods).toEqual([WATCH]);

      first.resolve({ rows: [], highWaterId: 0 });
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH]);

      // The compensating unwatch must settle before B runs, not merely be queued.
      second.resolve({ rows: [traceRow(6, "assistant_text", { text: "b" }, "task-b")], highWaterId: 6 });
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH]);

      stop.resolve({ stopped: true });
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH, WATCH]);
      expect(sub.getSnapshot()).toMatchObject({ phase: "ready" });
      expect(sub.getSnapshot().owner?.taskId).toBe("task-b");
    });

    it("does not let a request settling after 30 seconds overtake the next intent", async () => {
      vi.useFakeTimers();
      const first = await openPending();
      const stop = deferred<{ stopped: boolean }>();
      client.queue(UNWATCH, stop.promise);
      const second = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, second.promise);
      sub.open({ ...target, taskId: "task-b" }, connection);

      vi.advanceTimersByTime(30_000);
      await flush();
      expect(client.methods).toEqual([WATCH]);

      first.resolve({ rows: [], highWaterId: 0 });
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH]);

      stop.resolve({ stopped: true });
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH, WATCH]);
      second.resolve({ rows: [], highWaterId: 0 });
      await flush();
      expect(sub.getSnapshot().phase).toBe("ready");
    });

    it("keeps a failed stale cleanup out of the new view and does not block it", async () => {
      const first = await openPending();
      // Reject through a deferred, not an eager `Promise.reject`: the unwatch is
      // only issued after the stale watch settles, so an already-rejected promise
      // would be reported as unhandled before anything can listen to it.
      const failedCleanup = deferred<{ stopped: boolean }>();
      client.queue(UNWATCH, failedCleanup.promise);
      const second = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, second.promise);
      sub.open({ ...target, taskId: "task-b" }, connection);

      first.resolve({ rows: [], highWaterId: 0 });
      await flush();
      // The compensation is in flight, so B is still held back.
      expect(client.methods).toEqual([WATCH, UNWATCH]);
      expect(sub.getSnapshot()).toMatchObject({ phase: "loading", error: null });

      failedCleanup.reject(new Error("cleanup failed"));
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH, WATCH]);
      expect(sub.getSnapshot()).toMatchObject({ phase: "loading", error: null });

      second.resolve({ rows: [], highWaterId: 0 });
      await flush();
      expect(sub.getSnapshot()).toMatchObject({ phase: "ready", error: null });
      // Diagnostics stay out of the visible snapshot.
      expect(sub.lastCleanupError).toBe("cleanup failed");
    });

    it("cleans up the old thread when the same task opens on another thread", async () => {
      const first = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, first.promise);
      sub.open(target, connection);
      await flush();
      first.resolve({ rows: [traceRow(1, "assistant_text", { text: "a" })], highWaterId: 1 });
      await flush();
      expect(sub.getSnapshot().phase).toBe("ready");

      client.queue(UNWATCH, Promise.resolve({ stopped: true }));
      const second = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, second.promise);
      const other = { ...target, threadId: "thread-b" };
      sub.open(other, connection);

      // A ready, visible thread switch starts clean instead of showing the old
      // thread's rows under the new one.
      expect(sub.getSnapshot()).toMatchObject({ phase: "loading", rows: [] });

      second.resolve({ rows: [], highWaterId: 0 });
      await flush();
      expect(sub.getSnapshot().phase).toBe("ready");
      expect(client.calls.map((call) => call.params)).toEqual([
        { threadId: "thread-a", taskId: "task-a" },
        { threadId: "thread-a" },
        { threadId: "thread-b", taskId: "task-a" },
      ]);

      // An event naming the old thread is not this owner's.
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(2, "assistant_text", { text: "late" }) });
      expect(sub.getSnapshot().rows).toEqual([]);
    });

    it("keeps an in-flight A clean at close after B replaced it and A resolves late", async () => {
      const first = await openPending();
      const stop = deferred<{ stopped: boolean }>();
      client.queue(UNWATCH, stop.promise);
      const second = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, second.promise);
      sub.open({ ...target, taskId: "task-b" }, connection);

      // B is queued behind A's watch, so nothing but A's watch has been issued.
      await flush();
      expect(client.methods).toEqual([WATCH]);

      // A -> B -> close: the close must not leave B's queued watch running, and
      // A's late snapshot must not resurrect anything.
      sub.close();
      expect(sub.getSnapshot()).toMatchObject({ owner: null, phase: "idle", rows: [] });

      first.resolve({ rows: [traceRow(1, "assistant_text", { text: "a" }, "task-a")], highWaterId: 1 });
      await flush();
      // A settles, compensates with an unwatch, and B is skipped as stale.
      expect(client.methods).toEqual([WATCH, UNWATCH]);
      expect(sub.getSnapshot()).toMatchObject({ phase: "idle", rows: [] });

      stop.resolve({ stopped: true });
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH]);
      expect(sub.getSnapshot()).toMatchObject({ phase: "idle", rows: [] });
    });

    it("ignores the old client's late snapshot when the same ids arrive on a new client", async () => {
      const old = await openPending();
      const nextClient = new ScriptedTraceClient();
      const next = secondConnection({ client: nextClient, connectionGeneration: 2 });
      client.queue(UNWATCH, Promise.resolve({ stopped: true }));
      const fresh = deferred<AgentTraceSnapshotResult>();
      nextClient.queue(WATCH, fresh.promise);

      sub.open(target, next);
      await flush();
      expect(nextClient.methods).toEqual([WATCH]);

      old.resolve({ rows: [traceRow(1, "assistant_text", { text: "old" })], highWaterId: 1 });
      await flush();
      // Identity is the client object, not the task ids it happens to name.
      expect(sub.getSnapshot()).toMatchObject({ phase: "loading", rows: [] });
      expect(client.methods).toEqual([WATCH, UNWATCH]);

      fresh.resolve({ rows: [traceRow(1, "assistant_text", { text: "new" })], highWaterId: 1 });
      await flush();
      expect(sub.getSnapshot().phase).toBe("ready");
      expect(sub.getSnapshot().owner?.client).toBe(nextClient);
    });

    it("does not poison the queue with one operation's error", async () => {
      const failed = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, failed.promise);
      sub.open(target, connection);
      await flush();
      failed.reject(new Error("boom"));
      await flush();
      expect(sub.getSnapshot()).toMatchObject({ phase: "error", error: "boom" });

      const retried = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, retried.promise);
      sub.retry(connection);
      expect(sub.getSnapshot().phase).toBe("loading");
      await flush();
      expect(client.methods).toEqual([WATCH, WATCH]);

      retried.resolve({ rows: [traceRow(2, "assistant_text", { text: "ok" })], highWaterId: 2 });
      await flush();
      expect(sub.getSnapshot()).toMatchObject({ phase: "ready", error: null });
    });
  });

  describe("close and retry", () => {
    it("publishes idle synchronously and cleans up on the old client's chain", async () => {
      const pending = await openPending();
      const stop = deferred<{ stopped: boolean }>();
      client.queue(UNWATCH, stop.promise);

      sub.close();
      expect(sub.getSnapshot()).toMatchObject({ owner: null, phase: "idle", rows: [], disconnected: false });
      expect(client.methods).toEqual([WATCH]);

      pending.resolve({ rows: [traceRow(1, "assistant_text", { text: "late" })], highWaterId: 1 });
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH]);
      // The late snapshot belongs to a closed view and must not resurrect it.
      expect(sub.getSnapshot()).toMatchObject({ phase: "idle", rows: [] });

      stop.resolve({ stopped: true });
      await flush();
    });

    it("clears local state without any RPC when there is no client", async () => {
      sub.open(target, null);
      expect(sub.getSnapshot().phase).toBe("error");

      sub.close();
      expect(sub.getSnapshot()).toMatchObject({ owner: null, phase: "idle", rows: [] });
      await flush();
      expect(client.calls).toEqual([]);
    });

    it("does not unwatch after close when the watch never left the queue", async () => {
      sub.open(target, connection);
      sub.close();
      await flush();
      expect(client.calls).toEqual([]);
    });

    it("retries without auto-resubscribing and drops the pre-failure buffer", async () => {
      const first = await openPending();
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(9, "assistant_text", { text: "lost" }) });
      first.reject(new Error("boom"));
      await flush();
      expect(sub.getSnapshot()).toMatchObject({ phase: "error", rows: [] });

      const second = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, second.promise);
      sub.retry(connection);
      await flush();
      expect(client.methods).toEqual([WATCH, WATCH]);

      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(6, "assistant_text", { text: "new" }) });
      second.resolve({ rows: [traceRow(4, "assistant_text", { text: "base" })], highWaterId: 4 });
      await flush();

      // The pre-failure row 9 must not resurrect: the snapshot is the new baseline.
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([4, 6]);
    });

    it("keeps a ready history through retry until the new baseline lands", async () => {
      const first = await openPending();
      first.resolve({ rows: [traceRow(1, "assistant_text", { text: "kept" })], highWaterId: 1 });
      await flush();

      const second = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, second.promise);
      sub.retry(connection);
      expect(sub.getSnapshot()).toMatchObject({ phase: "loading" });
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([1]);

      second.resolve({ rows: [traceRow(1, "assistant_text", { text: "kept" })], highWaterId: 1 });
      await flush();
      expect(sub.getSnapshot().phase).toBe("ready");
    });
  });

  describe("timeout", () => {
    it("publishes a 15 second hint without expiring the intent or its buffer", async () => {
      vi.useFakeTimers();
      const pending = await openPending();
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(9, "assistant_text", { text: "buffered" }) });

      vi.advanceTimersByTime(TRACE_WATCH_TIMEOUT_MS - 1);
      expect(sub.getSnapshot().waitingForPriorRequest).toBe(false);

      vi.advanceTimersByTime(1);
      expect(sub.getSnapshot()).toMatchObject({
        phase: "loading",
        waitingForPriorRequest: true,
        error: null,
      });

      // A late success with no newer intent is still authoritative.
      pending.resolve({ rows: [traceRow(1, "assistant_text", { text: "a" })], highWaterId: 1 });
      await flush();
      expect(sub.getSnapshot()).toMatchObject({
        phase: "ready",
        waitingForPriorRequest: false,
        error: null,
      });
      // The buffer survived the hint and was merged.
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([1, 9]);
    });

    it("does not release the serialization lock when the hint fires", async () => {
      vi.useFakeTimers();
      const first = await openPending();
      const stop = deferred<{ stopped: boolean }>();
      client.queue(UNWATCH, stop.promise);
      const second = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, second.promise);

      sub.open({ ...target, taskId: "task-b" }, connection);
      vi.advanceTimersByTime(TRACE_WATCH_TIMEOUT_MS * 2);
      await flush();
      expect(client.methods).toEqual([WATCH]);

      first.resolve({ rows: [], highWaterId: 0 });
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH]);

      stop.resolve({ stopped: true });
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH, WATCH]);
      second.resolve({ rows: [], highWaterId: 0 });
      await flush();
    });

    it("arms the hint per intent rather than against the previous intent's deadline", async () => {
      vi.useFakeTimers();
      const first = await openPending();
      vi.advanceTimersByTime(10_000);
      expect(sub.getSnapshot().waitingForPriorRequest).toBe(false);

      const stop = deferred<{ stopped: boolean }>();
      client.queue(UNWATCH, stop.promise);
      const second = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, second.promise);
      // A new intent at t=10s: if the old timer were left running (or the new
      // one not armed) the hint would appear 5s too early below.
      sub.open({ ...target, taskId: "task-b" }, connection);

      vi.advanceTimersByTime(5_000);
      expect(sub.getSnapshot().waitingForPriorRequest).toBe(false);

      vi.advanceTimersByTime(10_000);
      expect(sub.getSnapshot()).toMatchObject({ phase: "loading", waitingForPriorRequest: true });

      first.resolve({ rows: [], highWaterId: 0 });
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH]);
      stop.resolve({ stopped: true });
      await flush();
      expect(client.methods).toEqual([WATCH, UNWATCH, WATCH]);

      second.resolve({ rows: [], highWaterId: 0 });
      await flush();
      expect(sub.getSnapshot()).toMatchObject({ phase: "ready", waitingForPriorRequest: false });
    });

    it("clears the hint and the timer on close", async () => {
      vi.useFakeTimers();
      await openPending();
      vi.advanceTimersByTime(TRACE_WATCH_TIMEOUT_MS);
      expect(sub.getSnapshot().waitingForPriorRequest).toBe(true);

      sub.close();
      expect(sub.getSnapshot()).toMatchObject({ phase: "idle", waitingForPriorRequest: false });
      vi.advanceTimersByTime(TRACE_WATCH_TIMEOUT_MS * 2);
      expect(sub.getSnapshot().phase).toBe("idle");
    });
  });

  describe("disconnect", () => {
    it("keeps rows and selection but rejects every later event from that connection", async () => {
      const pending = await openPending();
      pending.resolve({ rows: [traceRow(1, "assistant_text", { text: "kept" })], highWaterId: 1 });
      await flush();

      sub.disconnected(connection);
      expect(sub.getSnapshot()).toMatchObject({ phase: "ready", disconnected: true, error: null });
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([1]);
      expect(sub.getSnapshot().owner?.taskId).toBe("task-a");

      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(2, "assistant_text", { text: "late" }) });
      const otherClient = new ScriptedTraceClient();
      sub.accept(secondConnection({ client: otherClient }), {
        threadId: "thread-a",
        taskId: "task-a",
        row: traceRow(3, "assistant_text", { text: "elsewhere" }),
      });
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([1]);
      expect(sub.getSnapshot().error).toBeNull();
    });

    it("shows an error when the connection dies while still loading", async () => {
      await openPending();
      sub.disconnected(connection);
      expect(sub.getSnapshot()).toMatchObject({ phase: "error", disconnected: true, rows: [] });
    });

    it("ignores a disconnect reported for another client, slot or binding", async () => {
      const pending = await openPending();
      pending.resolve({ rows: [traceRow(1, "assistant_text", { text: "kept" })], highWaterId: 1 });
      await flush();

      sub.disconnected(secondConnection({ client: new ScriptedTraceClient() }));
      sub.disconnected({ ...connection, slot: "remote" });
      sub.disconnected({ ...connection, serverBindingGeneration: 2 });
      expect(sub.getSnapshot()).toMatchObject({ phase: "ready", disconnected: false });

      sub.disconnected(connection);
      expect(sub.getSnapshot().disconnected).toBe(true);
    });

    it("retries on the new connection without resubscribing by itself", async () => {
      const pending = await openPending();
      pending.resolve({ rows: [traceRow(1, "assistant_text", { text: "kept" })], highWaterId: 1 });
      await flush();
      sub.disconnected(connection);
      await flush();
      // No automatic resubscription: the user must ask.
      expect(client.methods).toEqual([WATCH]);

      const nextClient = new ScriptedTraceClient();
      const fresh = deferred<AgentTraceSnapshotResult>();
      nextClient.queue(WATCH, fresh.promise);
      sub.retry(secondConnection({ client: nextClient, connectionGeneration: 2 }));

      expect(sub.getSnapshot()).toMatchObject({ phase: "loading", disconnected: false, error: null });
      expect(sub.getSnapshot().owner).toMatchObject({ connectionGeneration: 2, requestGeneration: expect.any(Number) });
      await flush();
      expect(nextClient.methods).toEqual([WATCH]);

      fresh.resolve({ rows: [traceRow(1, "assistant_text", { text: "kept" })], highWaterId: 1 });
      await flush();
      expect(sub.getSnapshot()).toMatchObject({ phase: "ready", disconnected: false });
    });

    it("shows the connection error when retry has no connection", async () => {
      const pending = await openPending();
      pending.resolve({ rows: [], highWaterId: 0 });
      await flush();
      sub.disconnected(connection);

      sub.retry(null);
      expect(sub.getSnapshot().phase).toBe("error");
      expect(sub.getSnapshot().error).toBe("与电脑的连接已断开，正在自动重连，无需操作。");
      expect(sub.getSnapshot().owner).toBeNull();
    });
  });

  describe("lost rows and disposal", () => {
    it("does not error on a dropped live row and restores it from a manual retry", async () => {
      const first = await openPending();
      first.resolve({ rows: [traceRow(1, "assistant_text", { text: "a" }), traceRow(2, "assistant_text", { text: "b" })], highWaterId: 2 });
      await flush();

      // eventId 3 never arrives: the stream may drop frames and must not treat
      // the gap as a failure.
      const dropped = traceRow(3, "assistant_text", { text: "c" });
      sub.accept(connection, { threadId: "thread-a", taskId: "task-a", row: traceRow(4, "assistant_text", { text: "d" }) });
      expect(sub.getSnapshot()).toMatchObject({ phase: "ready", error: null });
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([1, 2, 4]);

      const refresh = deferred<AgentTraceSnapshotResult>();
      client.queue(WATCH, refresh.promise);
      sub.retry(connection);
      refresh.resolve({
        rows: [
          traceRow(1, "assistant_text", { text: "a" }),
          traceRow(2, "assistant_text", { text: "b" }),
          dropped,
          traceRow(4, "assistant_text", { text: "d" }),
        ],
        highWaterId: 4,
      });
      await flush();
      expect(sub.getSnapshot().rows.map((row) => row.eventId)).toEqual([1, 2, 3, 4]);
      // The id that was never streamed is the exact row the snapshot restores.
      expect(sub.getSnapshot().rows[2]).toEqual(dropped);
      expect(sub.getSnapshot().error).toBeNull();
    });

    it("closes and clears listeners without disposing the caller's client", async () => {
      const disposeSpy = vi.fn();
      const owned = Object.assign(new ScriptedTraceClient(), { dispose: disposeSpy });
      const ownedConnection = { ...connection, client: owned };
      const pending = deferred<AgentTraceSnapshotResult>();
      owned.queue(WATCH, pending.promise);
      const listener = vi.fn();
      sub.subscribe(listener);

      sub.open(target, ownedConnection);
      pending.resolve({ rows: [], highWaterId: 0 });
      await flush();
      expect(sub.getSnapshot().phase).toBe("ready");

      sub.dispose();
      expect(sub.getSnapshot()).toMatchObject({ owner: null, phase: "idle" });
      // Draining a controller must never tear down the shared connection.
      expect(disposeSpy).not.toHaveBeenCalled();

      const calls = listener.mock.calls.length;
      sub.open(target, ownedConnection);
      sub.close();
      expect(listener.mock.calls.length).toBe(calls);

      // Subscribing after disposal is a harmless no-op, not a crash.
      expect(() => sub.subscribe(vi.fn())()).not.toThrow();
      await flush();
    });

    it("disposes pending state without an unhandled rejection", async () => {
      const pending = await openPending();
      client.queue(UNWATCH, Promise.resolve({ stopped: true }));
      sub.dispose();
      // A never-settling watch still leaves the chain intact; settle it late and
      // confirm nothing throws or publishes.
      pending.resolve({ rows: [], highWaterId: 0 });
      await flush();
      expect(sub.getSnapshot().phase).toBe("idle");
      expect(client.methods).toEqual([WATCH, UNWATCH]);
    });
  });
});
