/**
 * Shared typed fixtures for the subagent trace tests.
 *
 * Trace rows are the daemon's `IpcTraceRow` passed through verbatim: a test
 * that hand-builds a row would have to repeat the same JSON.stringify dance in
 * every file, and a fixture that silently serialised `undefined` would hide the
 * exact data-loss bug these tests exist to catch.
 */
import type { AgentTraceRow } from "../lib/protocol";
import type { TraceClient } from "../lib/subagentTraceSubscription";

/** A promise whose settlement the test drives by hand, for in-flight states. */
export function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

/**
 * One trace row as the server would send it.
 *
 * `JSON.stringify` of a non-serialisable payload is `undefined`, which would
 * produce a row with `payloadJson === undefined` and no useful failure; fail
 * loudly at the fixture instead.
 */
export function traceRow(
  eventId: number,
  kind: string,
  payload: unknown,
  taskId = "task-a",
): AgentTraceRow {
  const payloadJson = JSON.stringify(payload);
  if (payloadJson === undefined) throw new Error("fixture payload must be JSON serializable");
  return { eventId, taskId, kind, payloadJson };
}

/**
 * An RPC client whose responses the test releases one by one.
 *
 * A subscription test needs three things a mock can't give at once: to see the
 * exact order of `watch`/`unwatch` calls, to hold a call in flight until the
 * test decides otherwise, and to prove that no unexpected call was made (the
 * read-only rule, and the serialization rule, are both "this call did not
 * happen yet"). Queueing promises by method gives all three, and a missing queue
 * entry fails loudly instead of silently resolving with `undefined`-shaped data.
 */
export class ScriptedTraceClient implements TraceClient {
  readonly calls: { method: string; params: unknown }[] = [];
  readonly responses = new Map<string, Promise<unknown>[]>();

  /** Preset the promise a call to `method` will return; calls consume them in order. */
  queue(method: string, response: Promise<unknown>): void {
    const queue = this.responses.get(method);
    if (queue) queue.push(response);
    else this.responses.set(method, [response]);
  }

  /** The methods called, in order — the shape most ordering assertions want. */
  get methods(): string[] {
    return this.calls.map((call) => call.method);
  }

  request<T>(method: string, params: unknown): Promise<T> {
    this.calls.push({ method, params });
    const response = this.responses.get(method)?.shift();
    if (response === undefined) {
      return Promise.reject(new Error("unexpected RPC: " + method));
    }
    return response as Promise<T>;
  }
}
