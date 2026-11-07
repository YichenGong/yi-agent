/**
 * Shared typed fixtures for the subagent trace tests.
 *
 * Trace rows are the daemon's `IpcTraceRow` passed through verbatim: a test
 * that hand-builds a row would have to repeat the same JSON.stringify dance in
 * every file, and a fixture that silently serialised `undefined` would hide the
 * exact data-loss bug these tests exist to catch.
 */
import type { AgentTraceRow } from "../lib/protocol";

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
