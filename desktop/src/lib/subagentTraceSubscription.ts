/**
 * Serial subscription to one subagent's trace.
 *
 * The daemon keeps one trace watcher per client and thread, so every
 * watch/unwatch this frontend issues competes for the same slot: an unwatch that
 * lands after the next watch tears down the very stream the user is looking at.
 * The rules that keep that from happening live here, in one place, as a plain
 * state machine — no React, and no second source of truth for the visible rows.
 *
 * Three invariants drive the implementation:
 *
 * 1. **The owner is written synchronously.** `open`/`retry`/`close` update the
 *    intent before returning, so a live event that arrives in the same tick is
 *    already judged against the new owner. Nothing waits for a React effect.
 * 2. **Watch/unwatch run one at a time per client object.** Each client gets its
 *    own promise chain. A queued operation re-checks the intent when it finally
 *    runs; an operation that has already been *sent* is awaited to settlement,
 *    and its compensating unwatch is awaited inside the same chain, before any
 *    later watch may run.
 * 3. **Only the current owner may publish.** A stale response still settles (the
 *    chain must not be poisoned), but its snapshot, error and rows are dropped.
 *
 * The 15s timer is deliberately weaker than a timeout: it publishes a hint and
 * nothing else. Invalidating the intent or releasing the chain would let the
 * late request create a watch the UI has already forgotten.
 */
import { ConnectionError } from "./connectionError";
import { formatError } from "./errorMessage";
import type { AgentTraceRow, AgentTraceSnapshotResult } from "./protocol";
import type { RpcClient } from "./rpc";
import type { Slot } from "./slots";
import { mergeTraceEvent, mergeTraceSnapshot } from "./subagentTraceModel";

/** The part of the RPC client this module needs; kept narrow so tests can script it. */
export type TraceClient = Pick<RpcClient, "request">;

/** One task in one thread on one server binding. */
export interface TraceTarget {
  slot: Slot;
  serverBindingGeneration: number;
  threadId: string;
  taskId: string;
}

/**
 * The identity the UI keys its per-task state by.
 *
 * Deliberately excludes the connection and request generations: a reconnect
 * refreshes the data in place, while switching task or rebinding the server is a
 * genuinely different view that must reset.
 */
export function traceTargetKey(target: TraceTarget): string {
  return JSON.stringify([
    target.slot,
    target.serverBindingGeneration,
    target.threadId,
    target.taskId,
  ]);
}

/** A live connection: which client, and which incarnation of it. */
export interface TraceConnection {
  slot: Slot;
  serverBindingGeneration: number;
  connectionGeneration: number;
  client: TraceClient;
}

/** The task an in-flight subscription belongs to, plus the connection it uses. */
export interface TraceOwner extends TraceTarget {
  /** Object identity of the RPC client; never serialized. */
  client: TraceClient;
  connectionGeneration: number;
  /** Per-instance intent counter; every open/retry gets a fresh value. */
  requestGeneration: number;
}

export interface TraceSubscriptionSnapshot {
  owner: TraceOwner | null;
  phase: "idle" | "loading" | "ready" | "error";
  rows: readonly AgentTraceRow[];
  error: string | null;
  disconnected: boolean;
  /** The 15s hint: the request (or the one queued ahead of it) has not settled. */
  waitingForPriorRequest: boolean;
}

export interface TraceEventParams {
  threadId: string;
  taskId: string;
  row: AgentTraceRow;
}

/**
 * How long a subscription intent may stay unsettled before the UI says so.
 *
 * The server reads the snapshot synchronously and its own IPC reply timeout is
 * 30s, so a watch — or the unwatch queued ahead of it — can legitimately take
 * longer than this. The timer therefore only publishes a hint: it never
 * invalidates the intent, never clears its buffer and never releases the
 * serialization lock.
 */
export const TRACE_WATCH_TIMEOUT_MS = 15_000;

const WATCH = "agent/trace/watch";
const UNWATCH = "agent/trace/unwatch";

/** Shown when there is no connection to subscribe with, instead of faking success. */
const NO_CONNECTION_ERROR = formatError(new ConnectionError("disconnected", "not connected"));

const IDLE: TraceSubscriptionSnapshot = {
  owner: null,
  phase: "idle",
  rows: [],
  error: null,
  disconnected: false,
  waitingForPriorRequest: false,
};

export class TraceSubscription {
  private snapshot: TraceSubscriptionSnapshot = IDLE;
  private readonly listeners = new Set<() => void>();

  /** The selection, kept after a disconnect so `retry` knows what to reopen. */
  private target: TraceTarget | null = null;
  private owner: TraceOwner | null = null;

  /**
   * Live rows that arrived before the watch ack. They are not visible until the
   * authoritative snapshot lands, and only the part above its high-water mark
   * survives.
   */
  private buffer: AgentTraceRow[] = [];

  private timer: ReturnType<typeof setTimeout> | null = null;
  /** Monotonic intent counter; `requestGeneration` is a value of it. */
  private generation = 0;

  /** One serialized chain per client object, keyed by identity. */
  private readonly queues = new Map<TraceClient, Promise<void>>();
  /** Owners whose watch request actually left the queue (so cleanup has a target). */
  private readonly sent = new WeakSet<TraceOwner>();
  /** Owners whose unwatch is already queued or executed (so it is issued once). */
  private readonly cleanupQueued = new WeakSet<TraceOwner>();

  private disposed = false;
  /**
   * Last stale-cleanup failure. Diagnostics only: a cleanup that loses the race
   * must never be written into the newer view, so it is kept here instead of in
   * the snapshot.
   */
  private cleanupError: string | null = null;

  /** Stable across renders, so it can be handed straight to `useSyncExternalStore`. */
  readonly subscribe = (listener: () => void): (() => void) => {
    if (this.disposed) return () => {};
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  readonly getSnapshot = (): TraceSubscriptionSnapshot => this.snapshot;

  /** The last stale-unwatch failure, if any. Never part of the visible snapshot. */
  get lastCleanupError(): string | null {
    return this.cleanupError;
  }

  // ---------------------------------------------------------------------------
  // Intents
  // ---------------------------------------------------------------------------

  open(target: TraceTarget, connection: TraceConnection | null): void {
    if (this.disposed) return;
    const previous = this.owner;
    const generation = ++this.generation;
    this.target = { ...target };
    this.buffer = [];
    this.cleanupError = null;
    this.clearTimer();
    const owner = connection ? this.makeOwner(target, connection, generation) : null;
    this.release(previous, owner);
    this.owner = owner;
    this.publish({
      owner,
      phase: owner ? "loading" : "error",
      rows: [],
      error: owner ? null : NO_CONNECTION_ERROR,
      disconnected: false,
      waitingForPriorRequest: false,
    });
    if (owner) this.schedule(owner);
  }

  /**
   * User-requested retry on the same selection.
   *
   * The visible history survives until the new baseline lands, but the buffer is
   * dropped: rows captured before the failed attempt must not be replayed after
   * a snapshot that may already include them.
   */
  retry(connection: TraceConnection | null): void {
    if (this.disposed || this.target === null) return;
    const target = this.target;
    const generation = ++this.generation;
    this.buffer = [];
    this.cleanupError = null;
    this.clearTimer();
    const owner = connection ? this.makeOwner(target, connection, generation) : null;
    this.owner = owner;
    this.publish({
      owner,
      phase: owner ? "loading" : "error",
      error: owner ? null : NO_CONNECTION_ERROR,
      disconnected: false,
      waitingForPriorRequest: false,
    });
    if (owner) this.schedule(owner);
  }

  /**
   * Leave the task: publish idle at once, then clean up on the old client's own
   * chain. With no client the state is local-only and no RPC is issued.
   */
  close(): void {
    if (this.disposed) return;
    const owner = this.owner;
    ++this.generation;
    this.owner = null;
    this.target = null;
    this.buffer = [];
    this.clearTimer();
    this.publish({ ...IDLE });
    if (owner && this.sent.has(owner) && !this.cleanupQueued.has(owner)) {
      this.queueCleanup(owner);
    }
  }

  // ---------------------------------------------------------------------------
  // Events
  // ---------------------------------------------------------------------------

  accept(connection: TraceConnection, params: TraceEventParams): void {
    const owner = this.owner;
    if (this.disposed || owner === null) return;
    // A connection that has been reported dead must not feed the view any more,
    // even for frames that were already queued when it died.
    if (this.snapshot.disconnected) return;
    if (connection.client !== owner.client) return;
    if (connection.slot !== owner.slot) return;
    if (connection.serverBindingGeneration !== owner.serverBindingGeneration) return;
    // The source alone is not enough: the event must name the owned thread and
    // task, and the row itself must belong to that task. Same ids on another
    // thread or slot are different tasks.
    if (params.threadId !== owner.threadId || params.taskId !== owner.taskId) return;
    if (params.row.taskId !== owner.taskId) return;

    if (this.snapshot.phase === "loading") {
      // Before the ack the rows are unpublished: the snapshot will decide which
      // of them are newer than its high-water mark.
      if (!this.buffer.some((row) => row.eventId === params.row.eventId)) {
        this.buffer.push(params.row);
      }
      return;
    }
    if (this.snapshot.phase !== "ready") return;

    const rows = mergeTraceEvent(owner.taskId, this.snapshot.rows, params.row);
    // A duplicate returns the identical array; re-publishing would wake every
    // subscriber for a frame that changes nothing.
    if (rows === this.snapshot.rows) return;
    this.publish({ rows });
  }

  /**
   * The connection carrying the current owner has died.
   *
   * The loaded rows and the selection stay (the user was reading them), the view
   * is marked disconnected, and every later event from this connection — or any
   * other — is rejected until `retry` installs a new owner. Nothing is
   * resubscribed automatically.
   */
  disconnected(connection: TraceConnection): void {
    const owner = this.owner;
    if (this.disposed || owner === null) return;
    if (connection.client !== owner.client) return;
    if (connection.slot !== owner.slot) return;
    if (connection.serverBindingGeneration !== owner.serverBindingGeneration) return;
    if (this.snapshot.disconnected) return;

    // A fresh generation invalidates the in-flight intent without changing the
    // identity the user is looking at.
    ++this.generation;
    this.owner = { ...owner, requestGeneration: this.generation };
    // The object replaced the one that actually issued the watch; keep cleanup
    // reachable from `close()` even though the request is now invalid.
    this.sent.add(this.owner);
    this.clearTimer();
    const lostWhileLoading = this.snapshot.phase === "loading" && this.snapshot.rows.length === 0;
    this.publish({
      owner: this.owner,
      disconnected: true,
      waitingForPriorRequest: false,
      ...(lostWhileLoading ? { phase: "error" as const, error: NO_CONNECTION_ERROR } : {}),
    });
  }

  /** Tear down local state. The caller's RPC client is not ours to dispose. */
  dispose(): void {
    if (this.disposed) return;
    this.close();
    this.disposed = true;
    this.clearTimer();
    this.listeners.clear();
  }

  // ---------------------------------------------------------------------------
  // Serialization
  // ---------------------------------------------------------------------------

  /**
   * Clean up a watch the *server* may still hold, when the next owner cannot
   * simply replace it.
   *
   * A watch is per thread on the server, so re-opening another task in the same
   * thread is replaced by the next watch with no unwatch in between. A different
   * thread or a new client leaves the old watch behind, and only this frontend
   * knows it should go — so queue it on the old client's chain, ahead of the new
   * watch.
   */
  private release(previous: TraceOwner | null, next: TraceOwner | null): void {
    if (previous === null || !this.sent.has(previous)) return;
    if (this.cleanupQueued.has(previous)) return;
    if (next !== null && next.client === previous.client && next.threadId === previous.threadId) {
      return;
    }
    this.queueCleanup(previous);
  }

  private schedule(owner: TraceOwner): void {
    // The hint covers the queue wait too: it is armed when the intent exists,
    // not when its request happens to be sent.
    this.armTimer(owner);
    this.enqueue(owner.client, () => this.runIntent(owner));
  }

  /**
   * Append one operation to this client's chain.
   *
   * The chain's stored value never rejects, so one operation's failure cannot
   * poison the next one; the operation's own rejection is handled inside.
   */
  private enqueue(client: TraceClient, run: () => Promise<void>): void {
    const previous = this.queues.get(client) ?? Promise.resolve();
    const operation = previous.then(run, run);
    const settled = operation.then(
      () => undefined,
      () => undefined,
    );
    this.queues.set(client, settled);
    void settled.then(() => {
      if (this.queues.get(client) === settled) this.queues.delete(client);
    });
  }

  private async runIntent(owner: TraceOwner): Promise<void> {
    // A queued intent that lost the race never issues its watch.
    if (this.disposed || this.owner !== owner) return;

    this.sent.add(owner);
    let result: AgentTraceSnapshotResult;
    try {
      result = await owner.client.request<AgentTraceSnapshotResult>(WATCH, {
        threadId: owner.threadId,
        taskId: owner.taskId,
      });
    } catch (error) {
      // A stale failure is not this view's failure; it still settles the chain.
      if (this.owner === owner) this.fail(owner, error);
      return;
    }

    if (this.owner === owner) {
      this.acceptSnapshot(owner, result);
      return;
    }

    // The request was already in flight when the intent changed. Its watch is
    // still live on the server, so compensate *inside this operation* (the
    // success path never rejects, so the chain continues normally) — the next
    // watch must not run before this cleanup has settled.
    await this.compensate(owner);
  }

  private fail(owner: TraceOwner, error: unknown): void {
    this.buffer = [];
    this.clearTimer();
    this.publish({
      owner,
      phase: "error",
      error: formatError(error),
      waitingForPriorRequest: false,
    });
  }

  private acceptSnapshot(owner: TraceOwner, result: AgentTraceSnapshotResult): void {
    const rows = mergeTraceSnapshot(owner.taskId, result, this.buffer);
    this.buffer = [];
    this.clearTimer();
    this.publish({
      owner,
      phase: "ready",
      rows,
      error: null,
      disconnected: false,
      waitingForPriorRequest: false,
    });
  }

  // ---------------------------------------------------------------------------
  // Cleanup
  // ---------------------------------------------------------------------------

  /** Issue the compensating unwatch now, once per owner. */
  private async compensate(owner: TraceOwner): Promise<void> {
    if (this.cleanupQueued.has(owner)) return;
    this.cleanupQueued.add(owner);
    await this.sendUnwatch(owner);
  }

  /** Queue the unwatch behind whatever is already running for that client. */
  private queueCleanup(owner: TraceOwner): void {
    this.cleanupQueued.add(owner);
    this.enqueue(owner.client, () => this.sendUnwatch(owner));
  }

  private async sendUnwatch(owner: TraceOwner): Promise<void> {
    try {
      await owner.client.request(UNWATCH, { threadId: owner.threadId });
    } catch (error) {
      // The server may already have dropped this watch, or the connection may be
      // gone. Neither is the new view's problem: record it for diagnostics and
      // keep the local state as it is.
      this.cleanupError = formatError(error);
    }
  }

  // ---------------------------------------------------------------------------
  // Timer and publication
  // ---------------------------------------------------------------------------

  private armTimer(owner: TraceOwner): void {
    this.clearTimer();
    this.timer = setTimeout(() => {
      this.timer = null;
      // A hint for this intent only, and only while it is still loading. It does
      // not expire the intent, clear the buffer, or release the chain.
      if (this.owner !== owner) return;
      if (this.snapshot.phase !== "loading") return;
      this.publish({ waitingForPriorRequest: true });
    }, TRACE_WATCH_TIMEOUT_MS);
  }

  private clearTimer(): void {
    if (this.timer === null) return;
    clearTimeout(this.timer);
    this.timer = null;
  }

  private makeOwner(
    target: TraceTarget,
    connection: TraceConnection,
    requestGeneration: number,
  ): TraceOwner {
    return {
      ...target,
      client: connection.client,
      connectionGeneration: connection.connectionGeneration,
      requestGeneration,
    };
  }

  /** Publish only on a real change, so an unchanged snapshot keeps its identity. */
  private publish(patch: Partial<TraceSubscriptionSnapshot>): void {
    const next: TraceSubscriptionSnapshot = { ...this.snapshot, ...patch };
    if (
      next.owner === this.snapshot.owner &&
      next.phase === this.snapshot.phase &&
      next.rows === this.snapshot.rows &&
      next.error === this.snapshot.error &&
      next.disconnected === this.snapshot.disconnected &&
      next.waitingForPriorRequest === this.snapshot.waitingForPriorRequest
    ) {
      return;
    }
    this.snapshot = next;
    for (const listener of [...this.listeners]) listener();
  }
}
